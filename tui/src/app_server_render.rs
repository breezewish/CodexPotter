use std::collections::HashMap;
use std::collections::VecDeque;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use codex_protocol::models::MessagePhase;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TurnStartedEvent;
use codex_protocol::user_input::UserInput;
use ratatui::prelude::Widget;
use ratatui::style::Modifier;
use ratatui::text::Line;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::mpsc::unbounded_channel;
use tokio_stream::StreamExt;

use crate::AppExitInfo;
use crate::ExitReason;
use crate::app_event::AppEvent;
use crate::app_event_sender::AppEventSender;
use crate::bottom_pane::BottomPane;
use crate::bottom_pane::BottomPaneParams;
use crate::bottom_pane::ChatComposerDraft;
use crate::bottom_pane::InputResult;
use crate::bottom_pane::PromptFooterContext;
use crate::bottom_pane::PromptFooterOverride;
use crate::exec_cell::CommandOutput;
use crate::exec_cell::ExecCell;
use crate::exec_cell::new_active_exec_command;
use crate::external_editor_integration;
use crate::file_search::FileSearchManager;
use crate::history_cell;
use crate::history_cell::HistoryCell;
use crate::history_cell_potter::PotterStreamRecoveryRetryCell;
use crate::history_cell_potter::PotterStreamRecoveryUnrecoverableCell;
use crate::render::renderable::Renderable;
use crate::slash_command::SlashCommand;
use crate::streaming::chunking::AdaptiveChunkingPolicy;
use crate::streaming::commit_tick::CommitTickScope;
use crate::streaming::commit_tick::run_commit_tick;
use crate::streaming::controller::PlanStreamController;
use crate::streaming::controller::StreamController;
use crate::tui::Tui;
use crate::tui::TuiEvent;
use crate::verbosity::Verbosity;

fn render_runner_viewport(
    area: ratatui::layout::Rect,
    buf: &mut ratatui::buffer::Buffer,
    bottom_pane: &BottomPane,
    transient_lines: Vec<Line<'static>>,
) {
    let width = area.width;
    let pane_height = bottom_pane.desired_height(width).max(1).min(area.height);
    let pane_area = ratatui::layout::Rect::new(
        area.x,
        area.bottom().saturating_sub(pane_height),
        area.width,
        pane_height,
    );

    let transient_area_height = area.height.saturating_sub(pane_height);
    if transient_area_height > 0 && !transient_lines.is_empty() {
        let transient_area =
            ratatui::layout::Rect::new(area.x, area.y, area.width, transient_area_height);
        let overflow = transient_lines
            .len()
            .saturating_sub(usize::from(transient_area_height));
        let scroll_y = u16::try_from(overflow).unwrap_or(u16::MAX);
        ratatui::widgets::Paragraph::new(ratatui::text::Text::from(transient_lines))
            .scroll((scroll_y, 0))
            .render(transient_area, buf);
    }

    bottom_pane.render(pane_area, buf);
}

/// Placeholder text shown in the bottom-pane composer.
///
/// # Divergence (codex-potter)
///
/// Upstream Codex uses `Ask Codex to do anything`; CodexPotter uses `Assign new task to
/// CodexPotter`.
const PROMPT_PLACEHOLDER_TEXT: &str = "Assign new task to CodexPotter";

fn new_default_bottom_pane(
    tui: &Tui,
    app_event_tx: AppEventSender,
    animations_enabled: bool,
) -> BottomPane {
    BottomPane::new(BottomPaneParams {
        frame_requester: tui.frame_requester(),
        enhanced_keys_supported: tui.enhanced_keys_supported(),
        app_event_tx,
        animations_enabled,
        placeholder_text: PROMPT_PLACEHOLDER_TEXT.to_string(),
        disable_paste_burst: false,
    })
}

/// Prompt the user for a new task using the bottom-pane composer.
///
/// Returns `Ok(Some(prompt))` when the user submits a prompt. Returns `Ok(None)` when the prompt
/// is cancelled (for example, <kbd>Ctrl</kbd>+<kbd>C</kbd> on an empty composer) or when the event
/// stream ends unexpectedly.
pub async fn prompt_user_with_tui(
    tui: &mut Tui,
    show_startup_banner: bool,
    check_for_update_on_startup: bool,
    startup_warnings: Vec<String>,
    composer_draft: Option<ChatComposerDraft>,
    verbosity: &mut Verbosity,
    prompt_footer: PromptFooterContext,
) -> anyhow::Result<Option<String>> {
    let (app_event_tx_raw, mut app_event_rx) = unbounded_channel::<AppEvent>();
    let app_event_tx = AppEventSender::new(app_event_tx_raw);

    let file_search_dir = prompt_footer.working_dir.clone();
    let file_search = FileSearchManager::new(file_search_dir.clone(), app_event_tx.clone());
    let prompt_history = crate::prompt_history_store::PromptHistoryStore::new();

    let mut bottom_pane = new_default_bottom_pane(tui, app_event_tx.clone(), true);
    bottom_pane.set_prompt_footer_context(prompt_footer);
    if let Some(draft) = composer_draft {
        bottom_pane.composer_mut().restore_draft(draft);
    }
    let (history_log_id, history_entry_count) = prompt_history.metadata();
    bottom_pane
        .composer_mut()
        .set_history_metadata(history_log_id, history_entry_count);

    let mut should_pad_prompt_viewport = !show_startup_banner;
    if show_startup_banner {
        let width = tui.terminal.last_known_screen_size.width.max(1);
        let codex_model = crate::codex_config::resolve_codex_model_config(&file_search_dir)?;
        let mut model_label = match codex_model.reasoning_effort {
            Some(effort) => format!("{} {effort}", codex_model.model),
            None => codex_model.model,
        };
        if codex_model.is_fast {
            model_label.push_str(" [fast]");
        }
        let banner_lines = crate::startup_banner::build_startup_banner_lines(
            width,
            crate::CODEX_POTTER_VERSION,
            &model_label,
            &file_search_dir,
        );
        should_pad_prompt_viewport = should_pad_prompt_after_history_insert(&banner_lines);
        tui.insert_history_lines(banner_lines);

        if check_for_update_on_startup
            && let Some(latest_version) = crate::updates::get_upgrade_version()
        {
            let width = tui.terminal.last_known_screen_size.width.max(1);
            let lines = crate::history_cell::UpdateAvailableHistoryCell::new(
                latest_version,
                crate::update_action::get_update_action(),
            )
            .display_lines(width);
            if !lines.is_empty() {
                should_pad_prompt_viewport =
                    should_pad_prompt_viewport || should_pad_prompt_after_history_insert(&lines);
                tui.insert_history_lines(lines);
            }
        }
    }

    if !startup_warnings.is_empty() {
        let width = tui.terminal.last_known_screen_size.width.max(1);
        for warning in startup_warnings {
            let lines = history_cell::new_warning_event(warning).display_lines(width);
            if !lines.is_empty() {
                should_pad_prompt_viewport =
                    should_pad_prompt_viewport || should_pad_prompt_after_history_insert(&lines);
                tui.insert_history_lines(lines);
            }
        }
    }

    let mut app = RenderAppState::new_prompt_screen(
        app_event_tx,
        bottom_pane,
        prompt_history,
        file_search,
        should_pad_prompt_viewport,
        *verbosity,
    );
    let _ = app.run(tui, &mut app_event_rx, None, None).await?;
    *verbosity = app.processor.verbosity;

    Ok(match app.prompt_action.take() {
        Some(PromptScreenAction::Submitted(text)) => Some(text),
        Some(PromptScreenAction::CancelledByUser) | None => None,
    })
}

/// Handle an `Op::GetHistoryEntryRequest` by serving prompt history from the local store.
///
/// # Divergence (codex-potter)
///
/// `codex-potter` persists prompt history under `~/.codexpotter/history.jsonl` and answers history
/// lookups directly in the TUI runner (rather than forwarding to an upstream core/session store).
/// See `tui/src/prompt_history_store.rs` and `tui/AGENTS.md`.
fn handle_prompt_history_entry_request(
    frame_requester: crate::tui::FrameRequester,
    bottom_pane: &mut BottomPane,
    prompt_history: &crate::prompt_history_store::PromptHistoryStore,
    log_id: u64,
    offset: usize,
) {
    let entry = prompt_history.lookup_text(log_id, offset);
    if bottom_pane
        .composer_mut()
        .on_history_entry_response(log_id, offset, entry)
    {
        frame_requester.schedule_frame();
    }
}

fn restore_runtime_theme_from_codex_config(cwd: &Path) {
    let codex_home = crate::codex_config::find_codex_home().ok();
    let configured = crate::codex_config::resolve_codex_tui_theme(cwd)
        .ok()
        .flatten();

    let fallback_name = crate::render::highlight::adaptive_default_theme_name();
    let theme = configured
        .as_deref()
        .and_then(|name| {
            crate::render::highlight::resolve_theme_by_name(name, codex_home.as_deref())
        })
        .or_else(|| {
            crate::render::highlight::resolve_theme_by_name(fallback_name, codex_home.as_deref())
        });
    if let Some(theme) = theme {
        crate::render::highlight::set_syntax_theme(theme);
    }
}

fn should_pad_prompt_after_history_insert(lines: &[Line<'_>]) -> bool {
    let Some(last) = lines.last() else {
        return false;
    };

    !last
        .spans
        .iter()
        .all(|span| span.content.as_ref().trim().is_empty())
}

/// Controls how a single Potter round is rendered into the terminal transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoundRenderOptions {
    /// When true, renders the user prompt into the transcript before sending it to the backend.
    pub render_user_prompt: bool,

    /// Optional status header prefix shown while a task is running (e.g. `Round 2/10`).
    ///
    /// This is primarily used by `codex-potter` to keep the working banner consistent when
    /// continuing an unfinished round (where `EventMsg::PotterRoundStarted` may be suppressed to
    /// avoid inserting a duplicate transcript marker).
    pub status_header_prefix: Option<String>,

    /// When true, inserts a blank line before the first emitted history cell in this round.
    ///
    /// This is useful when multiple rounds are rendered into the same terminal transcript while
    /// suppressing per-round user prompt rendering.
    pub pad_before_first_cell: bool,
}

impl Default for RoundRenderOptions {
    fn default() -> Self {
        Self {
            render_user_prompt: true,
            status_header_prefix: None,
            pad_before_first_cell: false,
        }
    }
}

/// Channels required to drive a single Potter round render loop.
pub struct RoundBackendChannels {
    /// Sends ops from the UI to the backend.
    pub codex_op_tx: UnboundedSender<Op>,
    /// Receives events streamed from the backend.
    pub codex_event_rx: UnboundedReceiver<Event>,
    /// Receives fatal errors from the control plane that should abort the round immediately.
    pub fatal_exit_rx: UnboundedReceiver<String>,
}

/// Mutable UI state that must persist across rounds.
pub struct RoundUiState<'a> {
    /// Prompts queued while a task is running (collected from the bottom composer).
    pub queued_user_messages: &'a mut VecDeque<String>,
    /// Draft composer contents to restore when returning to a prompt screen.
    pub composer_draft: &'a mut Option<crate::bottom_pane::ChatComposerDraft>,
    /// Current transcript verbosity preference.
    pub verbosity: &'a mut Verbosity,
}

/// Context that must persist across rounds within a CodexPotter project.
pub struct ProjectRenderContext {
    pub project_started_at: Instant,
    pub prompt_footer: PromptFooterContext,
}

fn text_user_input_op(text: String) -> Op {
    Op::UserInput {
        items: vec![UserInput::Text {
            text,
            text_elements: Vec::new(),
        }],
        final_output_json_schema: None,
    }
}

/// Run a single Potter round render loop and collect any queued user messages typed mid-round.
///
/// This function consumes backend events, updates the transcript, and drives the bottom composer.
/// Any prompts queued while the round is running are appended to `queued_user_messages`. The
/// current composer draft is written back to `composer_draft` so it can be restored by subsequent
/// prompt screens.
pub async fn run_round_with_tui_options_and_queue(
    tui: &mut Tui,
    prompt: String,
    options: RoundRenderOptions,
    context: ProjectRenderContext,
    backend: RoundBackendChannels,
    startup_warnings: Vec<String>,
    state: RoundUiState<'_>,
) -> anyhow::Result<AppExitInfo> {
    let ProjectRenderContext {
        project_started_at,
        prompt_footer,
    } = context;
    let RoundBackendChannels {
        codex_op_tx,
        mut codex_event_rx,
        mut fatal_exit_rx,
    } = backend;

    let (app_event_tx_raw, mut app_event_rx) = unbounded_channel::<AppEvent>();
    let app_event_tx = AppEventSender::new(app_event_tx_raw);

    for warning in startup_warnings {
        app_event_tx.send(AppEvent::InsertHistoryCell(Box::new(
            history_cell::new_warning_event(warning),
        )));
    }

    let file_search_dir = prompt_footer.working_dir.clone();
    let file_search = FileSearchManager::new(file_search_dir, app_event_tx.clone());
    let prompt_history = crate::prompt_history_store::PromptHistoryStore::new();

    let driver = AppServerEventProcessor::new(app_event_tx.clone(), *state.verbosity);
    if options.render_user_prompt {
        driver.emit_user_prompt(prompt.clone());
    }

    codex_op_tx
        .send(text_user_input_op(prompt))
        .map_err(|err| anyhow::Error::msg(err.to_string()))?;

    let mut bottom_pane = new_default_bottom_pane(tui, app_event_tx.clone(), true);
    bottom_pane.set_prompt_footer_context(prompt_footer);
    bottom_pane.set_project_started_at(Some(project_started_at));
    bottom_pane.set_status_header_prefix(options.status_header_prefix.clone());
    if let Some(draft) = state.composer_draft.take() {
        bottom_pane.composer_mut().restore_draft(draft);
    }
    let (history_log_id, history_entry_count) = prompt_history.metadata();
    bottom_pane
        .composer_mut()
        .set_history_metadata(history_log_id, history_entry_count);
    let queued_user_messages_state = std::mem::take(state.queued_user_messages);
    let mut app = RenderAppState::new(
        driver,
        app_event_tx.clone(),
        Some(codex_op_tx),
        bottom_pane,
        prompt_history,
        file_search,
        queued_user_messages_state,
    );
    app.has_emitted_history_lines = options.pad_before_first_cell;
    app.refresh_queued_user_messages();

    let result = app
        .run(
            tui,
            &mut app_event_rx,
            Some(&mut codex_event_rx),
            Some(&mut fatal_exit_rx),
        )
        .await;
    *state.queued_user_messages = app.queued_user_messages;
    *state.composer_draft = app.bottom_pane.composer_mut().take_draft();
    *state.verbosity = app.processor.verbosity;
    result
}

struct AppServerEventProcessor {
    app_event_tx: AppEventSender,
    stream: StreamController,
    plan_stream: Option<PlanStreamController>,
    adaptive_chunking: AdaptiveChunkingPolicy,
    token_usage: TokenUsage,
    context_usage: TokenUsage,
    model_context_window: Option<i64>,
    thread_id: Option<codex_protocol::ThreadId>,
    cwd: PathBuf,
    verbosity: Verbosity,
    last_rendered_width: Option<u16>,
    saw_agent_delta: bool,
    saw_plan_delta: bool,
    needs_final_message_separator: bool,
    had_work_activity: bool,
    last_separator_elapsed_secs: Option<u64>,
    current_elapsed_secs: Option<u64>,
    pending_exploring_cell: Option<ExecCell>,
    /// Divergence (codex-potter): coalesce consecutive successful non-shell `Ran` items into one
    /// history cell.
    pending_success_ran_cell: Option<ExecCell>,
    /// Divergence (codex-potter): coalesce consecutive successful patch applications into one
    /// compact `Edited ...` summary when `Verbosity::Minimal`.
    pending_compact_patch_changes: Vec<HashMap<PathBuf, codex_protocol::protocol::FileChange>>,
    pending_potter_project_succeeded: Option<PendingPotterProjectSucceeded>,
}

#[derive(Debug)]
struct PendingPotterProjectSucceeded {
    rounds: u32,
    duration: Duration,
    user_prompt_file: PathBuf,
    git_commit_start: String,
    git_commit_end: String,
}

impl AppServerEventProcessor {
    fn new(app_event_tx: AppEventSender, verbosity: Verbosity) -> Self {
        Self {
            app_event_tx,
            stream: StreamController::new(None),
            plan_stream: None,
            adaptive_chunking: AdaptiveChunkingPolicy::default(),
            token_usage: TokenUsage::default(),
            context_usage: TokenUsage::default(),
            model_context_window: None,
            thread_id: None,
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            verbosity,
            last_rendered_width: None,
            saw_agent_delta: false,
            saw_plan_delta: false,
            needs_final_message_separator: false,
            had_work_activity: false,
            last_separator_elapsed_secs: None,
            current_elapsed_secs: None,
            pending_exploring_cell: None,
            pending_success_ran_cell: None,
            pending_compact_patch_changes: Vec::new(),
            pending_potter_project_succeeded: None,
        }
    }

    fn handle_retryable_stream_error(&mut self) {
        self.flush_pending_exploring_cell();
        self.flush_pending_success_ran_cell();
        self.flush_pending_compact_patch_changes();
        if let Some(cell) = self.stream.finalize() {
            self.emit_history_cell(cell);
        }
        self.flush_plan_stream();
        self.adaptive_chunking.reset();
        self.app_event_tx.send(AppEvent::StopCommitAnimation);
        self.saw_agent_delta = false;
        self.saw_plan_delta = false;
        self.needs_final_message_separator = true;
    }

    fn emit_user_prompt(&self, prompt: String) {
        self.app_event_tx.send(AppEvent::InsertHistoryCell(Box::new(
            history_cell::new_user_prompt(prompt),
        )));
    }

    fn emit_history_cell(&mut self, cell: Box<dyn HistoryCell>) {
        self.flush_pending_compact_patch_changes();
        self.app_event_tx.send(AppEvent::InsertHistoryCell(cell));
    }

    fn on_commit_tick(&mut self) {
        self.run_commit_tick_with_scope(CommitTickScope::AnyMode);
    }

    fn run_commit_tick_with_scope(&mut self, scope: CommitTickScope) {
        let outcome = run_commit_tick(
            &mut self.adaptive_chunking,
            Some(&mut self.stream),
            self.plan_stream.as_mut(),
            scope,
            Instant::now(),
        );
        for cell in outcome.cells {
            self.emit_history_cell(cell);
        }

        if outcome.has_controller && outcome.all_idle {
            self.app_event_tx.send(AppEvent::StopCommitAnimation);
        }
    }

    fn worked_elapsed_from(&mut self, current_elapsed: u64) -> u64 {
        let baseline = match self.last_separator_elapsed_secs {
            Some(last) if current_elapsed < last => 0,
            Some(last) => last,
            None => 0,
        };
        let elapsed = current_elapsed.saturating_sub(baseline);
        self.last_separator_elapsed_secs = Some(current_elapsed);
        elapsed
    }

    fn maybe_emit_final_message_separator(&mut self) {
        // In `Verbosity::Minimal` successful patch applications are buffered and rendered as a
        // coalesced compact summary. Flush here so work activity is committed to the transcript
        // before deciding whether to insert a final-message separator.
        self.flush_pending_compact_patch_changes();
        if self.needs_final_message_separator && self.had_work_activity {
            let elapsed_seconds = self
                .current_elapsed_secs
                .map(|current| self.worked_elapsed_from(current));
            self.emit_history_cell(Box::new(history_cell::FinalMessageSeparator::new(
                elapsed_seconds,
            )));
            self.needs_final_message_separator = false;
            self.had_work_activity = false;
        } else if self.needs_final_message_separator {
            // Reset the flag even if we don't show separator (no work was done)
            self.needs_final_message_separator = false;
        }
    }

    fn handle_codex_event(&mut self, event: Event) {
        match event.msg {
            EventMsg::SessionConfigured(cfg) => {
                self.thread_id = Some(cfg.session_id);
                self.cwd = cfg.cwd;
            }
            EventMsg::PotterProjectStarted {
                user_message,
                user_prompt_file,
                ..
            } => {
                self.flush_pending_exploring_cell();
                self.flush_pending_success_ran_cell();
                if let Some(message) = user_message.filter(|message| !message.is_empty()) {
                    self.emit_user_prompt(message);
                }
                self.needs_final_message_separator = true;
                self.emit_history_cell(Box::new(
                    crate::history_cell_potter::new_potter_project_hint(user_prompt_file),
                ));
            }
            EventMsg::PotterRoundStarted { current, total } => {
                self.flush_pending_exploring_cell();
                self.flush_pending_success_ran_cell();
                self.needs_final_message_separator = true;
                self.emit_history_cell(Box::new(
                    crate::history_cell_potter::new_potter_round_started(current, total),
                ));
            }
            EventMsg::PotterProjectSucceeded {
                rounds,
                duration,
                user_prompt_file,
                git_commit_start,
                git_commit_end,
            } => {
                self.flush_pending_exploring_cell();
                self.flush_pending_success_ran_cell();
                self.pending_potter_project_succeeded = Some(PendingPotterProjectSucceeded {
                    rounds,
                    duration,
                    user_prompt_file,
                    git_commit_start,
                    git_commit_end,
                });
            }
            EventMsg::PotterRoundFinished { .. } => {
                self.flush_pending_exploring_cell();
                self.flush_pending_success_ran_cell();
                if let Some(done) = self.pending_potter_project_succeeded.take() {
                    self.emit_history_cell(Box::new(
                        crate::history_cell_potter::new_potter_project_succeeded(
                            done.rounds,
                            done.duration,
                            done.user_prompt_file,
                            done.git_commit_start,
                            done.git_commit_end,
                        ),
                    ));
                }
                self.flush_pending_compact_patch_changes();
            }
            EventMsg::TokenCount(ev) => {
                if let Some(info) = ev.info {
                    self.token_usage = info.total_token_usage;
                    self.context_usage = info.last_token_usage;
                    self.model_context_window =
                        info.model_context_window.or(self.model_context_window);
                }
            }
            EventMsg::TurnStarted(TurnStartedEvent {
                model_context_window,
                ..
            }) => {
                self.model_context_window = model_context_window;
                self.adaptive_chunking.reset();
                self.plan_stream = None;
                self.saw_agent_delta = false;
                self.saw_plan_delta = false;
            }
            EventMsg::AgentMessageDelta(ev) => {
                self.flush_pending_exploring_cell();
                self.flush_pending_success_ran_cell();
                if !self.saw_agent_delta {
                    self.maybe_emit_final_message_separator();
                }
                self.saw_agent_delta = true;
                if self.stream.push(&ev.delta) {
                    self.app_event_tx.send(AppEvent::StartCommitAnimation);
                    self.run_commit_tick_with_scope(CommitTickScope::CatchUpOnly);
                }
            }
            EventMsg::PlanDelta(ev) => {
                self.flush_pending_exploring_cell();
                self.flush_pending_success_ran_cell();
                if !self.saw_agent_delta && !self.saw_plan_delta {
                    self.maybe_emit_final_message_separator();
                }
                self.saw_plan_delta = true;

                if self.plan_stream.is_none() {
                    let width = self
                        .last_rendered_width
                        .map(|width| usize::from(width.saturating_sub(4)));
                    self.plan_stream = Some(PlanStreamController::new(width));
                }
                if let Some(controller) = self.plan_stream.as_mut()
                    && controller.push(&ev.delta)
                {
                    self.app_event_tx.send(AppEvent::StartCommitAnimation);
                    self.run_commit_tick_with_scope(CommitTickScope::CatchUpOnly);
                }
            }
            EventMsg::AgentMessage(ev) => {
                self.flush_pending_exploring_cell();
                self.flush_pending_success_ran_cell();
                if self.saw_agent_delta {
                    return;
                }
                self.maybe_emit_final_message_separator();
                let dim_commentary = self.verbosity == Verbosity::Minimal
                    && matches!(ev.phase, Some(MessagePhase::Commentary));
                self.emit_agent_message(&ev.message, dim_commentary);
            }
            EventMsg::TurnComplete(_) => {
                self.flush_pending_exploring_cell();
                self.flush_pending_success_ran_cell();
                // Flush any remaining agent markdown buffer.
                if let Some(cell) = self.stream.finalize() {
                    self.emit_history_cell(cell);
                }
                self.flush_plan_stream();
                self.app_event_tx.send(AppEvent::StopCommitAnimation);
                if let Some(done) = self.pending_potter_project_succeeded.take() {
                    self.emit_history_cell(Box::new(
                        crate::history_cell_potter::new_potter_project_succeeded(
                            done.rounds,
                            done.duration,
                            done.user_prompt_file,
                            done.git_commit_start,
                            done.git_commit_end,
                        ),
                    ));
                }
                self.flush_pending_compact_patch_changes();
            }
            EventMsg::TurnAborted(ev) => {
                self.flush_pending_exploring_cell();
                self.flush_pending_success_ran_cell();
                if let Some(cell) = self.stream.finalize() {
                    self.emit_history_cell(cell);
                }
                self.flush_plan_stream();
                self.app_event_tx.send(AppEvent::StopCommitAnimation);

                if ev.reason == codex_protocol::protocol::TurnAbortReason::Interrupted {
                    self.emit_history_cell(Box::new(history_cell::new_error_event(String::from(
                        "Conversation interrupted - tell the model what to do differently.",
                    ))));
                }
                self.flush_pending_compact_patch_changes();
            }
            EventMsg::Warning(ev) => {
                self.flush_pending_exploring_cell();
                self.flush_pending_success_ran_cell();
                self.needs_final_message_separator = true;
                self.emit_history_cell(Box::new(history_cell::new_warning_event(ev.message)));
            }
            EventMsg::ContextCompacted(_) => {
                self.flush_pending_exploring_cell();
                self.flush_pending_success_ran_cell();
                self.emit_agent_message("Context compacted", false);
            }
            EventMsg::DeprecationNotice(ev) => {
                self.flush_pending_exploring_cell();
                self.flush_pending_success_ran_cell();
                self.needs_final_message_separator = true;
                self.emit_history_cell(Box::new(history_cell::new_deprecation_notice(
                    ev.summary, ev.details,
                )));
            }
            EventMsg::PlanUpdate(ev) => {
                self.flush_pending_exploring_cell();
                self.flush_pending_success_ran_cell();
                self.needs_final_message_separator = true;
                self.emit_history_cell(Box::new(history_cell::new_plan_update(ev)));
            }
            EventMsg::WebSearchEnd(ev) => {
                self.flush_pending_exploring_cell();
                self.flush_pending_success_ran_cell();
                self.needs_final_message_separator = true;
                self.emit_history_cell(Box::new(history_cell::new_web_search_call(ev.query)));
                self.had_work_activity = true;
            }
            EventMsg::ViewImageToolCall(ev) => {
                self.flush_pending_exploring_cell();
                self.flush_pending_success_ran_cell();
                self.needs_final_message_separator = true;
                self.emit_history_cell(Box::new(history_cell::new_view_image_tool_call(
                    ev.path, &self.cwd,
                )));
            }
            EventMsg::ExecCommandEnd(ev) => {
                // Align with upstream Codex TUI behavior: flush any newline-gated agent output
                // before rendering the tool result, so transcript ordering matches the semantic
                // "agent explains -> tool runs -> agent continues" flow.
                if let Some(cell) = self.stream.finalize() {
                    self.emit_history_cell(cell);
                }
                self.flush_plan_stream();

                let aggregated_output = if !ev.aggregated_output.is_empty() {
                    ev.aggregated_output
                } else {
                    format!("{}{}", ev.stdout, ev.stderr)
                };

                let mut cell = new_active_exec_command(
                    ev.call_id.clone(),
                    ev.command,
                    ev.parsed_cmd,
                    ev.source,
                    ev.interaction_input,
                    false,
                );
                cell.complete_call(
                    &ev.call_id,
                    CommandOutput {
                        exit_code: ev.exit_code,
                        aggregated_output,
                        formatted_output: ev.formatted_output,
                    },
                    ev.duration,
                );

                if cell.is_exploring_cell() {
                    self.flush_pending_success_ran_cell();
                    if let Some(pending) = self.pending_exploring_cell.as_mut() {
                        pending.calls.extend(cell.calls);
                    } else {
                        self.pending_exploring_cell = Some(cell);
                    }
                } else if Self::can_coalesce_success_ran_cell(&cell) {
                    self.flush_pending_exploring_cell();
                    if let Some(pending) = self.pending_success_ran_cell.as_mut() {
                        pending.calls.extend(cell.calls);
                    } else {
                        self.pending_success_ran_cell = Some(cell);
                    }
                } else {
                    self.flush_pending_exploring_cell();
                    self.flush_pending_success_ran_cell();
                    self.needs_final_message_separator = true;
                    if self.verbosity != Verbosity::Minimal {
                        self.emit_history_cell(Box::new(cell));
                    }
                }
                self.had_work_activity = true;
            }
            EventMsg::PatchApplyEnd(ev) => {
                self.flush_pending_exploring_cell();
                self.flush_pending_success_ran_cell();
                if ev.success && self.verbosity == Verbosity::Minimal {
                    self.pending_compact_patch_changes.push(ev.changes);
                } else {
                    self.needs_final_message_separator = true;
                    if ev.success {
                        self.emit_history_cell(Box::new(history_cell::new_patch_event(
                            ev.changes,
                            &self.cwd,
                            self.verbosity,
                        )));
                    } else {
                        self.emit_history_cell(Box::new(history_cell::new_patch_apply_failure(
                            ev.stderr,
                        )));
                    }
                }
                self.had_work_activity = true;
            }
            EventMsg::CollabAgentSpawnBegin(_) => {}
            EventMsg::CollabAgentSpawnEnd(ev) => {
                self.on_collab_event(crate::multi_agents::spawn_end(ev))
            }
            EventMsg::CollabAgentInteractionBegin(_) => {}
            EventMsg::CollabAgentInteractionEnd(ev) => {
                self.on_collab_event(crate::multi_agents::interaction_end(ev));
            }
            EventMsg::CollabWaitingBegin(ev) => {
                self.on_collab_event(crate::multi_agents::waiting_begin(ev))
            }
            EventMsg::CollabWaitingEnd(ev) => {
                self.on_collab_event(crate::multi_agents::waiting_end(ev))
            }
            EventMsg::CollabCloseBegin(_) => {}
            EventMsg::CollabCloseEnd(ev) => {
                self.on_collab_event(crate::multi_agents::close_end(ev))
            }
            EventMsg::CollabResumeBegin(ev) => {
                self.on_collab_event(crate::multi_agents::resume_begin(ev))
            }
            EventMsg::CollabResumeEnd(ev) => {
                self.on_collab_event(crate::multi_agents::resume_end(ev))
            }
            EventMsg::Error(ev) => {
                self.flush_pending_exploring_cell();
                self.flush_pending_success_ran_cell();
                if let Some(cell) = self.stream.finalize() {
                    self.emit_history_cell(cell);
                }
                self.flush_plan_stream();
                self.app_event_tx.send(AppEvent::StopCommitAnimation);
                self.saw_agent_delta = false;
                self.saw_plan_delta = false;
                self.needs_final_message_separator = true;
                self.emit_history_cell(Box::new(history_cell::new_error_event(ev.message)));
            }
            _ => {}
        }
    }

    fn on_collab_event(&mut self, cell: history_cell::PlainHistoryCell) {
        // Align with upstream behavior: flush any newline-gated agent output before inserting the
        // collab transcript cell, so ordering matches the semantic "agent explains -> collab
        // action -> agent continues" flow.
        if let Some(cell) = self.stream.finalize() {
            self.emit_history_cell(cell);
        }
        self.flush_plan_stream();
        self.flush_pending_exploring_cell();
        self.flush_pending_success_ran_cell();
        self.needs_final_message_separator = true;
        self.emit_history_cell(Box::new(cell));
        self.had_work_activity = true;
    }

    fn flush_pending_exploring_cell(&mut self) {
        let Some(cell) = self.pending_exploring_cell.take() else {
            return;
        };
        self.needs_final_message_separator = true;
        if self.verbosity == Verbosity::Minimal {
            return;
        }
        self.emit_history_cell(Box::new(cell));
    }

    fn flush_pending_compact_patch_changes(&mut self) {
        if self.pending_compact_patch_changes.is_empty() {
            return;
        }

        let change_sets = std::mem::take(&mut self.pending_compact_patch_changes);
        self.needs_final_message_separator = true;
        self.app_event_tx.send(AppEvent::InsertHistoryCell(Box::new(
            history_cell::new_coalesced_compact_patch_event(change_sets, &self.cwd),
        )));
    }

    fn flush_pending_success_ran_cell(&mut self) {
        let Some(cell) = self.pending_success_ran_cell.take() else {
            return;
        };
        self.needs_final_message_separator = true;
        if self.verbosity == Verbosity::Minimal {
            return;
        }
        self.emit_history_cell(Box::new(cell));
    }

    fn flush_plan_stream(&mut self) {
        let Some(mut controller) = self.plan_stream.take() else {
            return;
        };
        if let Some(cell) = controller.finalize() {
            self.emit_history_cell(cell);
        }
    }

    fn can_coalesce_success_ran_cell(cell: &ExecCell) -> bool {
        let [call] = cell.calls.as_slice() else {
            return false;
        };

        call.output
            .as_ref()
            .is_some_and(|output| output.exit_code == 0)
            && !call.is_user_shell_command()
            && !call.is_unified_exec_interaction()
    }

    fn emit_agent_message(&mut self, message: &str, dim: bool) {
        let mut lines: Vec<Line<'static>> = Vec::new();
        crate::markdown::append_markdown(message, None, &mut lines);
        if lines.is_empty() {
            return;
        }
        if dim {
            for line in lines.iter_mut() {
                line.style = line.style.add_modifier(Modifier::DIM);
                for span in line.spans.iter_mut() {
                    span.style = span.style.add_modifier(Modifier::DIM);
                }
            }
        }
        self.emit_history_cell(Box::new(history_cell::AgentMessageCell::new(lines, true)));
    }
}

struct ReasoningStatusTracker {
    buffer: String,
}

impl ReasoningStatusTracker {
    fn new() -> Self {
        Self {
            buffer: String::new(),
        }
    }

    fn reset(&mut self) {
        self.buffer.clear();
    }

    fn on_section_break(&mut self) {
        self.reset();
    }

    fn on_final(&mut self) {
        self.reset();
    }

    fn on_delta(&mut self, delta: &str) -> Option<String> {
        self.buffer.push_str(delta);
        extract_first_bold(&self.buffer)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PromptScreenAction {
    Submitted(String),
    CancelledByUser,
}

struct RenderAppState {
    prompt_action: Option<PromptScreenAction>,
    should_pad_prompt_viewport: bool,
    processor: AppServerEventProcessor,
    app_event_tx: AppEventSender,
    codex_op_tx: Option<UnboundedSender<Op>>,
    bottom_pane: BottomPane,
    prompt_history: crate::prompt_history_store::PromptHistoryStore,
    file_search: FileSearchManager,
    queued_user_messages: VecDeque<String>,
    reasoning_status: ReasoningStatusTracker,
    stream_error_status_header: Option<String>,
    potter_stream_recovery_retry_cell: Option<PotterStreamRecoveryRetryCell>,
    commit_anim_running: Arc<AtomicBool>,
    has_emitted_history_lines: bool,
    exit_after_next_draw: bool,
    exit_reason: ExitReason,
}

impl RenderAppState {
    fn new(
        processor: AppServerEventProcessor,
        app_event_tx: AppEventSender,
        codex_op_tx: Option<UnboundedSender<Op>>,
        bottom_pane: BottomPane,
        prompt_history: crate::prompt_history_store::PromptHistoryStore,
        file_search: FileSearchManager,
        queued_user_messages: VecDeque<String>,
    ) -> Self {
        Self {
            prompt_action: None,
            should_pad_prompt_viewport: false,
            processor,
            app_event_tx,
            codex_op_tx,
            bottom_pane,
            prompt_history,
            file_search,
            queued_user_messages,
            reasoning_status: ReasoningStatusTracker::new(),
            stream_error_status_header: None,
            potter_stream_recovery_retry_cell: None,
            commit_anim_running: Arc::new(AtomicBool::new(false)),
            has_emitted_history_lines: false,
            exit_after_next_draw: false,
            exit_reason: ExitReason::UserRequested,
        }
    }

    fn new_prompt_screen(
        app_event_tx: AppEventSender,
        bottom_pane: BottomPane,
        prompt_history: crate::prompt_history_store::PromptHistoryStore,
        file_search: FileSearchManager,
        should_pad_prompt_viewport: bool,
        verbosity: Verbosity,
    ) -> Self {
        let processor = AppServerEventProcessor::new(app_event_tx.clone(), verbosity);
        let mut app = Self::new(
            processor,
            app_event_tx,
            None,
            bottom_pane,
            prompt_history,
            file_search,
            VecDeque::new(),
        );
        app.should_pad_prompt_viewport = should_pad_prompt_viewport;
        app
    }

    fn build_transient_lines(&self, width: u16) -> Vec<Line<'static>> {
        if self.codex_op_tx.is_none() {
            return if self.should_pad_prompt_viewport {
                vec![Line::from("")]
            } else {
                Vec::new()
            };
        }

        let mut transient_lines: Vec<Line<'static>> = Vec::new();

        if self.processor.verbosity == Verbosity::Simple {
            if let Some(cell) = self.processor.pending_success_ran_cell.as_ref() {
                transient_lines.push(Line::from(""));
                transient_lines.extend(cell.display_lines(width));
            }

            if let Some(cell) = self.processor.pending_exploring_cell.as_ref() {
                // Keep a blank line between the transcript (which may include a background-colored
                // user prompt cell) and the live explored block.
                transient_lines.push(Line::from(""));
                transient_lines.extend(cell.display_lines(width));
            }
        }

        if let Some(cell) = self.potter_stream_recovery_retry_cell.as_ref() {
            transient_lines.push(Line::from(""));
            transient_lines.extend(cell.display_lines(width));
        }

        // When the bottom pane shrinks (e.g., after a turn completes and the status indicator is
        // removed), the prompt background can end up directly adjacent to the last transcript
        // line. Keep a blank line between the transcript and the bottom pane for readability.
        //
        // While a task is running, the status indicator already renders with padding that
        // separates it from the transcript; avoid adding redundant whitespace in that case.
        if transient_lines.is_empty()
            && self.has_emitted_history_lines
            && self.bottom_pane.status_widget().is_none()
        {
            transient_lines.push(Line::from(""));
        }

        transient_lines
    }

    async fn run(
        &mut self,
        tui: &mut Tui,
        app_event_rx: &mut UnboundedReceiver<AppEvent>,
        codex_event_rx: Option<&mut UnboundedReceiver<Event>>,
        fatal_exit_rx: Option<&mut UnboundedReceiver<String>>,
    ) -> anyhow::Result<AppExitInfo> {
        let has_backend = self.codex_op_tx.is_some();
        anyhow::ensure!(
            has_backend == codex_event_rx.is_some() && has_backend == fatal_exit_rx.is_some(),
            "internal error: backend channels must be either all present or all absent",
        );

        let mut tui_events = tui.event_stream();
        self.bottom_pane.set_task_running(has_backend);
        tui.frame_requester().schedule_frame();

        let mut codex_event_rx = codex_event_rx;
        let mut fatal_exit_rx = fatal_exit_rx;

        loop {
            tokio::select! {
                maybe_event = tui_events.next() => {
                    let Some(event) = maybe_event else {
                        break;
                    };
                        match event {
                        TuiEvent::Draw => {
                            if self.bottom_pane.composer_mut().flush_paste_burst_if_due() {
                                // A paste just flushed; request an immediate redraw and skip this frame.
                                tui.frame_requester().schedule_frame();
                                continue;
                            }
                            if self.bottom_pane.composer().is_in_paste_burst() {
                                // While capturing a burst, schedule a follow-up tick and skip this frame
                                // to avoid redundant renders.
                                tui.frame_requester().schedule_frame_in(crate::bottom_pane::ChatComposer::recommended_paste_flush_delay());
                                continue;
                            }
                            // Drain any queued events before drawing so the rendered frame reflects
                            // the latest history inserts. This also avoids a race where a scheduled
                            // Draw event wins the select! before the final InsertHistoryCell events
                            // are processed, which would otherwise cause this runner to exit with
                            // missing output.
                            while let Ok(app_event) = app_event_rx.try_recv() {
                                self.handle_app_event(tui, app_event)?;
                            }
                            if let Some(rx) = codex_event_rx.as_mut() {
                                while let Ok(event) = rx.try_recv() {
                                    self.handle_app_event(tui, AppEvent::CodexEvent(event))?;
                                }
                            }
                            if let Some(rx) = fatal_exit_rx.as_mut() {
                                while let Ok(message) = rx.try_recv() {
                                    self.handle_app_event(
                                        tui,
                                        AppEvent::FatalExitRequest(message),
                                    )?;
                                }
                            }

                            // Drain any new app events produced by the codex events we just
                            // processed above before rendering the next frame.
                            while let Ok(app_event) = app_event_rx.try_recv() {
                                self.handle_app_event(tui, app_event)?;
                            }
                            self.draw(tui)?;
                            if self.exit_after_next_draw {
                                break;
                            }
                        }
                            TuiEvent::Key(key_event) => {
                                if external_editor_integration::is_ctrl_g(&key_event) {
                                    if key_event.kind == crossterm::event::KeyEventKind::Press {
                                        self.handle_external_editor(tui).await?;
                                    }
                                    continue;
                                }
                                let width = tui.terminal.last_known_screen_size.width.max(1);
                                self.handle_key_event(key_event, tui.frame_requester(), width);
                                if self.prompt_action.is_some() {
                                    if matches!(
                                        self.prompt_action,
                                        Some(PromptScreenAction::CancelledByUser)
                                    ) {
                                        // Clear the inline viewport so the shell prompt is clean on exit.
                                        tui.terminal.clear()?;
                                    }
                                    break;
                                }
                            }
                        TuiEvent::Paste(pasted) => {
                            // Many terminals convert newlines to \r when pasting (e.g., iTerm2),
                            // but tui-textarea expects \n. Normalize CR to LF.
                            let pasted = pasted.replace("\r", "\n");
                            if self.bottom_pane.composer_mut().handle_paste(pasted) {
                                tui.frame_requester().schedule_frame();
                            }
                        }
                    }
                }
                maybe_app_event = app_event_rx.recv() => {
                    let Some(app_event) = maybe_app_event else {
                        break;
                    };
                    self.handle_app_event(tui, app_event)?;
                }
                maybe_codex_event = async {
                    if let Some(rx) = codex_event_rx.as_mut() {
                        rx.recv().await
                    } else {
                        None
                    }
                }, if has_backend => {
                    match maybe_codex_event {
                        Some(event) => {
                            self.handle_app_event(tui, AppEvent::CodexEvent(event))?;
                        }
                        None => {
                            if !self.exit_after_next_draw {
                                self.processor.flush_pending_exploring_cell();
                                self.processor.flush_pending_success_ran_cell();
                                self.processor.flush_pending_compact_patch_changes();
                                self.exit_reason =
                                    ExitReason::Fatal("Backend disconnected".to_string());
                                self.exit_after_next_draw = true;
                                tui.frame_requester().schedule_frame();
                            }
                        }
                    }
                }
                maybe_fatal = async {
                    if let Some(rx) = fatal_exit_rx.as_mut() {
                        rx.recv().await
                    } else {
                        None
                    }
                }, if has_backend => {
                    let Some(message) = maybe_fatal else {
                        continue;
                    };
                    self.handle_app_event(tui, AppEvent::FatalExitRequest(message))?;
                }
            }
        }

        self.commit_anim_running.store(false, Ordering::Release);
        Ok(AppExitInfo {
            token_usage: self.processor.token_usage.clone(),
            thread_id: self.processor.thread_id,
            exit_reason: self.exit_reason.clone(),
        })
    }

    async fn handle_external_editor(&mut self, tui: &mut Tui) -> anyhow::Result<()> {
        self.bottom_pane
            .set_prompt_footer_override(Some(PromptFooterOverride::ExternalEditorHint));
        self.draw(tui)?;

        match external_editor_integration::run_external_editor(tui, self.bottom_pane.composer())
            .await
        {
            Ok(Some(new_text)) => {
                self.bottom_pane
                    .composer_mut()
                    .apply_external_edit(new_text);
            }
            Ok(None) => {
                self.handle_app_event(
                    tui,
                    AppEvent::InsertHistoryCell(Box::new(history_cell::new_error_event(
                        external_editor_integration::MISSING_EDITOR_ERROR.to_string(),
                    ))),
                )?;
            }
            Err(err) => {
                self.handle_app_event(
                    tui,
                    AppEvent::InsertHistoryCell(Box::new(history_cell::new_error_event(format!(
                        "Failed to open editor: {err}",
                    )))),
                )?;
            }
        }

        self.bottom_pane.set_prompt_footer_override(None);
        tui.frame_requester().schedule_frame();
        Ok(())
    }

    fn handle_key_event(
        &mut self,
        key_event: crossterm::event::KeyEvent,
        frame_requester: crate::tui::FrameRequester,
        terminal_width: u16,
    ) {
        if key_event.kind == crossterm::event::KeyEventKind::Release {
            return;
        }

        let is_press = key_event.kind == crossterm::event::KeyEventKind::Press;

        // Restore the last queued message into the composer for quick edits.
        if key_event.modifiers == crossterm::event::KeyModifiers::ALT
            && matches!(key_event.code, crossterm::event::KeyCode::Up)
            && !self.queued_user_messages.is_empty()
        {
            if !is_press {
                return;
            }
            if let Some(message) = self.queued_user_messages.pop_back() {
                self.bottom_pane.composer_mut().set_text_content(message);
                self.refresh_queued_user_messages();
                frame_requester.schedule_frame();
            }
            return;
        }

        if key_event
            .modifiers
            .contains(crossterm::event::KeyModifiers::CONTROL)
            && matches!(key_event.code, crossterm::event::KeyCode::Char('c'))
        {
            if !is_press {
                return;
            }
            if self.bottom_pane.composer().selection_popup_visible() {
                let (_result, needs_redraw) =
                    self.bottom_pane.composer_mut().handle_key_event(key_event);
                if needs_redraw {
                    frame_requester.schedule_frame();
                }
                return;
            }
            if self.bottom_pane.composer().is_empty() {
                if self.codex_op_tx.is_none() {
                    self.prompt_action = Some(PromptScreenAction::CancelledByUser);
                } else {
                    // Preserve any live output (for example pending "Explored" / "Ran" cells)
                    // in the transcript before clearing the inline viewport on exit.
                    self.processor.flush_pending_exploring_cell();
                    self.processor.flush_pending_success_ran_cell();
                    self.processor.flush_pending_compact_patch_changes();

                    self.app_event_tx.send(AppEvent::CodexOp(Op::Interrupt));

                    // Treat Ctrl+C as an explicit user cancellation, even if the turn just
                    // finished, so callers can stop multi-round loops reliably.
                    if !matches!(self.exit_reason, ExitReason::Fatal(_)) {
                        self.exit_reason = ExitReason::UserRequested;
                    }
                    self.exit_after_next_draw = true;
                }
            } else {
                self.bottom_pane.composer_mut().clear_for_ctrl_c();
            }
            frame_requester.schedule_frame();
            return;
        }

        if matches!(key_event.code, crossterm::event::KeyCode::Esc)
            && key_event.modifiers == crossterm::event::KeyModifiers::NONE
            && self.codex_op_tx.is_some()
            && self.bottom_pane.is_task_running()
            && !self.bottom_pane.composer().popup_active()
        {
            if !is_press {
                return;
            }

            self.app_event_tx.send(AppEvent::CodexOp(Op::Interrupt));
            frame_requester.schedule_frame();
            return;
        }

        let (result, needs_redraw) = self.bottom_pane.composer_mut().handle_key_event(key_event);
        if needs_redraw {
            frame_requester.schedule_frame();
        }
        if self.bottom_pane.composer().is_in_paste_burst() {
            frame_requester.schedule_frame_in(
                crate::bottom_pane::ChatComposer::recommended_paste_flush_delay(),
            );
        }

        match result {
            InputResult::Submitted(text) | InputResult::Queued(text) => {
                let history_text = self
                    .bottom_pane
                    .composer()
                    .encode_prompt_history_text(&text);
                self.prompt_history.record_submission(&history_text);
                if self.codex_op_tx.is_none() {
                    self.prompt_action = Some(PromptScreenAction::Submitted(text));
                } else {
                    self.queued_user_messages.push_back(text);
                    self.refresh_queued_user_messages();
                    frame_requester.schedule_frame();
                }
            }
            InputResult::Command(cmd) => match cmd {
                SlashCommand::Mention => {
                    self.bottom_pane.composer_mut().insert_str("@");
                    frame_requester.schedule_frame();
                }
                SlashCommand::Exit => {
                    if self.codex_op_tx.is_none() {
                        self.prompt_action = Some(PromptScreenAction::CancelledByUser);
                    } else {
                        // Preserve any live output (for example pending "Explored" / "Ran"
                        // cells) in the transcript before clearing the inline viewport on
                        // exit.
                        self.processor.flush_pending_exploring_cell();
                        self.processor.flush_pending_success_ran_cell();
                        self.processor.flush_pending_compact_patch_changes();

                        self.app_event_tx.send(AppEvent::CodexOp(Op::Interrupt));

                        // Treat /exit as an explicit user cancellation, even if the turn just
                        // finished, so callers can stop multi-round loops reliably.
                        if !matches!(self.exit_reason, ExitReason::Fatal(_)) {
                            self.exit_reason = ExitReason::UserRequested;
                        }
                        self.exit_after_next_draw = true;
                        frame_requester.schedule_frame();
                    }
                }
                SlashCommand::Theme => {
                    if !cmd.available_during_task() && self.bottom_pane.is_task_running() {
                        let message = format!(
                            "'/{}' is disabled while a task is in progress.",
                            cmd.command()
                        );
                        self.app_event_tx.send(AppEvent::InsertHistoryCell(Box::new(
                            history_cell::new_error_event(message),
                        )));
                        frame_requester.schedule_frame();
                        return;
                    }

                    let codex_home = crate::codex_config::find_codex_home().ok();
                    let cwd = self.bottom_pane.prompt_working_dir();
                    let current_name = crate::codex_config::resolve_codex_tui_theme(cwd)
                        .ok()
                        .flatten();
                    let params = crate::theme_picker::build_theme_picker_params(
                        current_name.as_deref(),
                        codex_home.as_deref(),
                        Some(terminal_width),
                    );
                    self.bottom_pane.composer_mut().show_selection_view(params);
                    frame_requester.schedule_frame();
                }
                SlashCommand::Verbosity => {
                    let params = crate::verbosity_picker::build_verbosity_picker_params(
                        self.processor.verbosity,
                    );
                    self.bottom_pane.composer_mut().show_selection_view(params);
                    frame_requester.schedule_frame();
                }
            },
            InputResult::None => {}
        }
    }

    fn refresh_queued_user_messages(&mut self) {
        let messages: Vec<String> = self.queued_user_messages.iter().cloned().collect();
        self.bottom_pane.set_queued_user_messages(messages);
    }

    fn draw(&mut self, tui: &mut Tui) -> anyhow::Result<()> {
        let width = tui.terminal.last_known_screen_size.width;
        self.processor.last_rendered_width = Some(width);
        let pane_height = self.bottom_pane.desired_height(width).max(1);
        let transient_lines = self.build_transient_lines(width);
        let transient_height = u16::try_from(transient_lines.len()).unwrap_or(u16::MAX);
        let viewport_height = pane_height.saturating_add(transient_height);

        tui.draw(viewport_height, |frame| {
            let area = frame.area();
            ratatui::widgets::Clear.render(area, frame.buffer_mut());
            render_runner_viewport(area, frame.buffer_mut(), &self.bottom_pane, transient_lines);

            let pane_height = self
                .bottom_pane
                .desired_height(area.width)
                .max(1)
                .min(area.height);
            let pane_area = ratatui::layout::Rect::new(
                area.x,
                area.bottom().saturating_sub(pane_height),
                area.width,
                pane_height,
            );
            let cursor = self
                .bottom_pane
                .cursor_pos(pane_area)
                .unwrap_or((area.x, area.bottom().saturating_sub(1)));
            frame.set_cursor_position(cursor);
        })?;
        Ok(())
    }

    fn handle_app_event(&mut self, tui: &mut Tui, app_event: AppEvent) -> anyhow::Result<()> {
        match app_event {
            AppEvent::InsertHistoryCell(cell) => {
                let cell: Arc<dyn HistoryCell> = cell.into();
                let width = tui.terminal.last_known_screen_size.width;
                let mut display = cell.display_lines(width);
                if display.is_empty() {
                    return Ok(());
                }

                if self.codex_op_tx.is_none() {
                    self.should_pad_prompt_viewport = self.should_pad_prompt_viewport
                        || should_pad_prompt_after_history_insert(&display);
                } else if !cell.is_stream_continuation() {
                    if self.has_emitted_history_lines {
                        display.insert(0, Line::from(""));
                    } else {
                        self.has_emitted_history_lines = true;
                    }
                }

                tui.insert_history_lines(display);
            }
            AppEvent::SyntaxThemeSelected { name } => {
                let cwd = self.bottom_pane.prompt_working_dir();
                match crate::codex_config::find_codex_home() {
                    Ok(codex_home) => {
                        match crate::codex_config::persist_codex_tui_theme(&codex_home, &name) {
                            Ok(()) => {
                                if let Some(theme) = crate::render::highlight::resolve_theme_by_name(
                                    &name,
                                    Some(&codex_home),
                                ) {
                                    crate::render::highlight::set_syntax_theme(theme);
                                }
                            }
                            Err(err) => {
                                restore_runtime_theme_from_codex_config(cwd);
                                self.handle_app_event(
                                    tui,
                                    AppEvent::InsertHistoryCell(Box::new(
                                        history_cell::new_error_event(format!(
                                            "Failed to save theme: {err}"
                                        )),
                                    )),
                                )?;
                            }
                        }
                    }
                    Err(err) => {
                        restore_runtime_theme_from_codex_config(cwd);
                        self.handle_app_event(
                            tui,
                            AppEvent::InsertHistoryCell(Box::new(history_cell::new_error_event(
                                format!("Failed to find CODEX_HOME: {err}"),
                            ))),
                        )?;
                    }
                }
                tui.frame_requester().schedule_frame();
            }
            AppEvent::VerbositySelected { verbosity } => {
                match crate::potter_config::persist_potter_tui_verbosity(verbosity) {
                    Ok(()) => {
                        self.processor.verbosity = verbosity;
                    }
                    Err(err) => {
                        self.handle_app_event(
                            tui,
                            AppEvent::InsertHistoryCell(Box::new(history_cell::new_error_event(
                                format!("Failed to save verbosity: {err}"),
                            ))),
                        )?;
                    }
                }
                tui.frame_requester().schedule_frame();
            }
            AppEvent::StartCommitAnimation => {
                anyhow::ensure!(
                    self.codex_op_tx.is_some(),
                    "internal error: StartCommitAnimation requires backend channels",
                );
                if self
                    .commit_anim_running
                    .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
                {
                    let tx = self.app_event_tx.clone();
                    let running = self.commit_anim_running.clone();
                    thread::spawn(move || {
                        while running.load(Ordering::Relaxed) {
                            thread::sleep(Duration::from_millis(50));
                            tx.send(AppEvent::CommitTick);
                        }
                    });
                }
            }
            AppEvent::StopCommitAnimation => {
                anyhow::ensure!(
                    self.codex_op_tx.is_some(),
                    "internal error: StopCommitAnimation requires backend channels",
                );
                self.commit_anim_running.store(false, Ordering::Release);
            }
            AppEvent::CommitTick => {
                anyhow::ensure!(
                    self.codex_op_tx.is_some(),
                    "internal error: CommitTick requires backend channels",
                );
                self.processor.on_commit_tick();
            }
            AppEvent::CodexEvent(event) => {
                anyhow::ensure!(
                    self.codex_op_tx.is_some(),
                    "internal error: CodexEvent requires backend channels",
                );
                self.handle_codex_event(tui.frame_requester(), event)?;
            }
            AppEvent::CodexOp(op) => match op {
                Op::GetHistoryEntryRequest { offset, log_id } => {
                    handle_prompt_history_entry_request(
                        tui.frame_requester(),
                        &mut self.bottom_pane,
                        &self.prompt_history,
                        log_id,
                        offset,
                    );
                }
                _ => {
                    let Some(tx) = self.codex_op_tx.as_ref() else {
                        anyhow::bail!("internal error: unexpected {op:?} without backend channels");
                    };
                    let _ = tx.send(op);
                }
            },
            AppEvent::StartFileSearch(query) => {
                self.file_search.on_user_query(query);
            }
            AppEvent::FileSearchResult { query, matches } => {
                self.bottom_pane
                    .composer_mut()
                    .on_file_search_result(query, matches);
                tui.frame_requester().schedule_frame();
            }
            AppEvent::FatalExitRequest(message) => {
                anyhow::ensure!(
                    self.codex_op_tx.is_some(),
                    "internal error: FatalExitRequest requires backend channels",
                );
                self.processor.flush_pending_exploring_cell();
                self.processor.flush_pending_success_ran_cell();
                self.processor.flush_pending_compact_patch_changes();
                self.exit_reason = ExitReason::Fatal(message);
                self.bottom_pane.set_task_running(false);
                self.exit_after_next_draw = true;
                tui.frame_requester().schedule_frame();
            }
        }

        Ok(())
    }

    fn handle_codex_event(
        &mut self,
        frame_requester: crate::tui::FrameRequester,
        event: Event,
    ) -> anyhow::Result<()> {
        match &event.msg {
            EventMsg::PotterStreamRecoveryUpdate {
                attempt,
                max_attempts,
                error_message,
            } => {
                self.potter_stream_recovery_retry_cell = Some(PotterStreamRecoveryRetryCell {
                    attempt: *attempt,
                    max_attempts: *max_attempts,
                    error_message: error_message.clone(),
                });

                self.processor.current_elapsed_secs = self
                    .bottom_pane
                    .status_widget()
                    .map(super::status_indicator_widget::StatusIndicatorWidget::elapsed_seconds);
                self.processor.handle_retryable_stream_error();
                frame_requester.schedule_frame();
                return Ok(());
            }
            EventMsg::PotterStreamRecoveryRecovered => {
                self.potter_stream_recovery_retry_cell = None;
                frame_requester.schedule_frame();
                return Ok(());
            }
            EventMsg::PotterStreamRecoveryGaveUp {
                error_message,
                max_attempts,
                ..
            } => {
                self.potter_stream_recovery_retry_cell = None;
                self.processor.current_elapsed_secs = self
                    .bottom_pane
                    .status_widget()
                    .map(super::status_indicator_widget::StatusIndicatorWidget::elapsed_seconds);
                self.processor.handle_retryable_stream_error();
                self.app_event_tx.send(AppEvent::InsertHistoryCell(Box::new(
                    PotterStreamRecoveryUnrecoverableCell {
                        max_attempts: *max_attempts,
                        error_message: error_message.clone(),
                    },
                )));

                frame_requester.schedule_frame();
                return Ok(());
            }
            _ => {}
        }

        if let EventMsg::StreamError(ev) = &event.msg {
            if self.stream_error_status_header.is_none() {
                self.stream_error_status_header =
                    Some(self.bottom_pane.status_header().to_string());
            }
            self.bottom_pane.update_status_header_with_details(
                ev.message.clone(),
                ev.additional_details.clone(),
            );
            return Ok(());
        }
        if let Some(header) = self.stream_error_status_header.take() {
            self.bottom_pane.update_status_header(header);
        }

        match &event.msg {
            EventMsg::PotterRoundStarted { current, total } => {
                self.bottom_pane
                    .set_status_header_prefix(Some(format!("Round {current}/{total}")));
            }
            EventMsg::TurnStarted(_) => {
                self.reasoning_status.reset();
                self.bottom_pane
                    .update_status_header(String::from("Working"));
            }
            EventMsg::AgentReasoningDelta(ev) => {
                if let Some(header) = self.reasoning_status.on_delta(&ev.delta) {
                    self.bottom_pane.update_status_header(header);
                }
                return Ok(());
            }
            EventMsg::AgentReasoningRawContentDelta(ev) => {
                if let Some(header) = self.reasoning_status.on_delta(&ev.delta) {
                    self.bottom_pane.update_status_header(header);
                }
                return Ok(());
            }
            EventMsg::AgentReasoningSectionBreak(_) => {
                self.reasoning_status.on_section_break();
                return Ok(());
            }
            EventMsg::AgentReasoning(_) => {
                self.reasoning_status.on_final();
                return Ok(());
            }
            EventMsg::AgentReasoningRawContent(ev) => {
                if let Some(header) = self.reasoning_status.on_delta(&ev.text) {
                    self.bottom_pane.update_status_header(header);
                }
                self.reasoning_status.on_final();
                return Ok(());
            }
            _ => {}
        }

        if should_filter_thinking_event(&event.msg) {
            return Ok(());
        }

        let should_exit_on_round_end = matches!(&event.msg, EventMsg::PotterRoundFinished { .. });
        let should_stop_footer = match &event.msg {
            EventMsg::PotterRoundFinished { .. } => should_exit_on_round_end,
            _ => false,
        };
        let should_update_context = matches!(
            &event.msg,
            EventMsg::TokenCount(_) | EventMsg::TurnStarted(_)
        );
        let should_redraw_after_event = matches!(&event.msg, EventMsg::ExecCommandEnd(_));

        match &event.msg {
            EventMsg::PotterRoundFinished { outcome } if should_exit_on_round_end => {
                self.exit_reason = match outcome {
                    codex_protocol::protocol::PotterRoundOutcome::Completed => {
                        ExitReason::Completed
                    }
                    codex_protocol::protocol::PotterRoundOutcome::Interrupted => {
                        ExitReason::Interrupted
                    }
                    codex_protocol::protocol::PotterRoundOutcome::UserRequested => {
                        ExitReason::UserRequested
                    }
                    codex_protocol::protocol::PotterRoundOutcome::TaskFailed { message } => {
                        ExitReason::TaskFailed(message.clone())
                    }
                    codex_protocol::protocol::PotterRoundOutcome::Fatal { message } => {
                        ExitReason::Fatal(message.clone())
                    }
                };
                self.exit_after_next_draw = true;
                frame_requester.schedule_frame();
            }
            _ => {}
        }

        self.processor.current_elapsed_secs = self
            .bottom_pane
            .status_widget()
            .map(super::status_indicator_widget::StatusIndicatorWidget::elapsed_seconds);
        self.processor.handle_codex_event(event);
        if should_update_context {
            self.update_bottom_pane_context_window();
        }
        if should_redraw_after_event {
            frame_requester.schedule_frame();
        }
        if should_stop_footer {
            self.bottom_pane.set_task_running(false);
        }

        Ok(())
    }

    fn update_bottom_pane_context_window(&mut self) {
        let Some(context_window) = self.processor.model_context_window else {
            self.bottom_pane
                .set_context_window(None, Some(self.processor.token_usage.total_tokens));
            return;
        };
        if context_window <= 0 {
            self.bottom_pane
                .set_context_window(None, Some(self.processor.token_usage.total_tokens));
            return;
        }

        let percent_left = self
            .processor
            .context_usage
            .percent_of_context_window_remaining(context_window);
        self.bottom_pane
            .set_context_window(Some(percent_left), None);
    }
}

/// Returns true when `msg` is a reasoning/thinking stream event that should not be rendered as a
/// transcript/history item.
///
/// # Divergence (codex-potter)
///
/// Reasoning messages are never rendered in the transcript; they are only used to update the live
/// status header.
fn should_filter_thinking_event(msg: &EventMsg) -> bool {
    matches!(
        msg,
        EventMsg::AgentReasoning(_)
            | EventMsg::AgentReasoningDelta(_)
            | EventMsg::AgentReasoningRawContent(_)
            | EventMsg::AgentReasoningRawContentDelta(_)
            | EventMsg::AgentReasoningSectionBreak(_)
    )
}

// Extract the first bold (Markdown) element in the form **...** from `s`.
// Returns the inner text if found; otherwise `None`.
fn extract_first_bold(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i + 1 < bytes.len() {
        if bytes[i] == b'*' && bytes[i + 1] == b'*' {
            let start = i + 2;
            let mut j = start;
            while j + 1 < bytes.len() {
                if bytes[j] == b'*' && bytes[j + 1] == b'*' {
                    let trimmed = s[start..j].trim();
                    return (!trimmed.is_empty()).then(|| trimmed.to_string());
                }
                j += 1;
            }
            return None;
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::insert_history::insert_history_lines;
    use crate::test_backend::VT100Backend;
    use codex_protocol::ThreadId;
    use codex_protocol::parse_command::ParsedCommand;
    use codex_protocol::protocol::AgentMessageDeltaEvent;
    use codex_protocol::protocol::AgentMessageEvent;
    use codex_protocol::protocol::CodexErrorInfo;
    use codex_protocol::protocol::ContextCompactedEvent;
    use codex_protocol::protocol::ErrorEvent;
    use codex_protocol::protocol::ExecCommandEndEvent;
    use codex_protocol::protocol::ExecCommandSource;
    use codex_protocol::protocol::PatchApplyBeginEvent;
    use codex_protocol::protocol::PatchApplyEndEvent;
    use codex_protocol::protocol::PlanDeltaEvent;
    use codex_protocol::protocol::SessionConfiguredEvent;
    use codex_protocol::protocol::StreamErrorEvent;
    use codex_protocol::protocol::TokenCountEvent;
    use codex_protocol::protocol::TokenUsageInfo;
    use codex_protocol::protocol::TurnAbortReason;
    use codex_protocol::protocol::TurnAbortedEvent;
    use codex_protocol::protocol::TurnCompleteEvent;
    use codex_protocol::protocol::TurnStartedEvent;
    use insta::assert_snapshot;
    use ratatui::layout::Rect;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::time::Instant;
    use tokio::sync::mpsc::UnboundedReceiver;
    use tokio::sync::mpsc::unbounded_channel;

    fn line_to_plain_string(line: &ratatui::text::Line<'_>) -> String {
        let mut out = String::new();
        for span in &line.spans {
            out.push_str(span.content.as_ref());
        }
        out
    }

    fn lines_to_plain_strings(lines: &[ratatui::text::Line<'_>]) -> Vec<String> {
        lines.iter().map(line_to_plain_string).collect()
    }

    fn drain_history_cell_strings(
        rx: &mut UnboundedReceiver<AppEvent>,
        width: u16,
    ) -> Vec<Vec<String>> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            let AppEvent::InsertHistoryCell(cell) = ev else {
                continue;
            };
            out.push(lines_to_plain_strings(&cell.display_lines(width)));
        }
        out
    }

    fn drain_render_history_events(
        rx: &mut UnboundedReceiver<AppEvent>,
        terminal: &mut crate::custom_terminal::Terminal<VT100Backend>,
        width: u16,
        has_emitted_history_lines: &mut bool,
    ) {
        while let Ok(ev) = rx.try_recv() {
            let AppEvent::InsertHistoryCell(cell) = ev else {
                continue;
            };

            let cell: Arc<dyn HistoryCell> = cell.into();
            let mut display = cell.display_lines(width);
            if display.is_empty() {
                continue;
            }

            if !cell.is_stream_continuation() {
                if *has_emitted_history_lines {
                    display.insert(0, Line::from(""));
                } else {
                    *has_emitted_history_lines = true;
                }
            }

            insert_history_lines(terminal, display).expect("insert history");
        }
    }

    fn drive_stream_to_idle(
        proc: &mut AppServerEventProcessor,
        rx: &mut UnboundedReceiver<AppEvent>,
        terminal: &mut crate::custom_terminal::Terminal<VT100Backend>,
        width: u16,
        has_emitted_history_lines: &mut bool,
    ) {
        for _ in 0..100 {
            proc.on_commit_tick();
            drain_render_history_events(rx, terminal, width, has_emitted_history_lines);
        }
    }

    fn make_round_renderer_processor(
        prompt: &str,
    ) -> (AppServerEventProcessor, UnboundedReceiver<AppEvent>) {
        let (tx_raw, rx) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);
        let proc = AppServerEventProcessor::new(app_event_tx, Verbosity::default());
        proc.emit_user_prompt(prompt.to_string());
        (proc, rx)
    }

    fn make_round_renderer_processor_without_prompt()
    -> (AppServerEventProcessor, UnboundedReceiver<AppEvent>) {
        let (tx_raw, rx) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);
        (
            AppServerEventProcessor::new(app_event_tx, Verbosity::default()),
            rx,
        )
    }

    #[tokio::test]
    async fn should_filter_thinking_events() {
        assert!(should_filter_thinking_event(
            &EventMsg::AgentReasoningDelta(codex_protocol::protocol::AgentReasoningDeltaEvent {
                delta: "thinking".to_string(),
            })
        ));
        assert!(!should_filter_thinking_event(&EventMsg::AgentMessageDelta(
            codex_protocol::protocol::AgentMessageDeltaEvent {
                delta: "output".to_string(),
            }
        )));
    }

    #[test]
    fn extract_first_bold_returns_first_markdown_bold_span() {
        assert_eq!(
            extract_first_bold("**Inspecting for code duplication**\n\nmore"),
            Some("Inspecting for code duplication".to_string())
        );
        assert_eq!(extract_first_bold("no bold here"), None);
        assert_eq!(extract_first_bold("**"), None);
        assert_eq!(extract_first_bold("**  ** trailing"), None);
        assert_eq!(
            extract_first_bold("prefix **first** then **second**"),
            Some("first".to_string())
        );
    }

    #[test]
    fn reasoning_delta_updates_status_header_from_first_bold() {
        let (tx_raw, _rx) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);
        let mut bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx,
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        bottom_pane.set_task_running(true);

        let mut tracker = ReasoningStatusTracker::new();
        assert!(
            tracker.on_delta("**Inspecting").is_none(),
            "incomplete header"
        );

        let Some(header) = tracker.on_delta(" for code duplication**") else {
            panic!("expected a header after receiving closing **");
        };
        bottom_pane.update_status_header(header);

        let status = bottom_pane.status_widget().expect("status indicator");
        assert_eq!(status.header(), "Inspecting for code duplication");
    }

    #[test]
    fn potter_stream_recovery_retry_block_persists_and_clears_on_recovered_event() {
        let width: u16 = 80;

        let (tx_raw, mut rx_app) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let processor = AppServerEventProcessor::new(app_event_tx.clone(), Verbosity::default());
        let (op_tx, mut op_rx) = unbounded_channel::<Op>();
        let mut bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx: app_event_tx.clone(),
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        bottom_pane.set_task_running(true);
        bottom_pane.update_status_header("Inspecting for code duplication".to_string());
        if let Some(status) = bottom_pane.status_indicator_mut() {
            status.pause_timer_at(Instant::now());
        }
        let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
        let mut app = RenderAppState::new(
            processor,
            app_event_tx,
            Some(op_tx),
            bottom_pane,
            crate::prompt_history_store::PromptHistoryStore::new(),
            file_search,
            VecDeque::new(),
        );

        app.handle_codex_event(
            crate::tui::FrameRequester::test_dummy(),
            Event {
                id: "err".into(),
                msg: EventMsg::PotterStreamRecoveryUpdate {
                    attempt: 1,
                    max_attempts: 10,
                    error_message:
                        "stream disconnected before completion: error sending request for url (...)"
                            .to_string(),
                },
            },
        )
        .expect("handle codex event");

        let status = app.bottom_pane.status_widget().expect("status indicator");
        pretty_assertions::assert_eq!(status.header(), "Inspecting for code duplication");
        pretty_assertions::assert_eq!(status.details(), None);

        let transient_lines = app.build_transient_lines(width);
        let transient_blob = lines_to_plain_strings(&transient_lines).join("\n");
        assert!(
            transient_blob.contains("• CodexPotter: retry 1/10"),
            "missing retry header: {transient_blob:?}"
        );
        assert!(
            transient_blob.contains(
                "└ Stream disconnected before completion: error sending request for url (...)"
            ),
            "missing retry error details: {transient_blob:?}"
        );

        let cells = drain_history_cell_strings(&mut rx_app, width);
        assert!(
            cells.is_empty(),
            "expected no history cells; got: {cells:?}"
        );

        assert!(
            op_rx.try_recv().is_err(),
            "expected no Continue op for stream recovery update"
        );

        app.handle_codex_event(
            crate::tui::FrameRequester::test_dummy(),
            Event {
                id: "turn-started".into(),
                msg: EventMsg::TurnStarted(TurnStartedEvent {
                    turn_id: "turn-1".to_string(),
                    model_context_window: None,
                }),
            },
        )
        .expect("handle retry turn start");

        let status = app.bottom_pane.status_widget().expect("status indicator");
        pretty_assertions::assert_eq!(status.header(), "Working");
        pretty_assertions::assert_eq!(status.details(), None);

        let transient_lines = app.build_transient_lines(width);
        let transient_blob = lines_to_plain_strings(&transient_lines).join("\n");
        assert!(
            transient_blob.contains("• CodexPotter: retry 1/10"),
            "expected retry block to persist until recovered: {transient_blob:?}"
        );

        app.handle_codex_event(
            crate::tui::FrameRequester::test_dummy(),
            Event {
                id: "recovered".into(),
                msg: EventMsg::PotterStreamRecoveryRecovered,
            },
        )
        .expect("handle recovered event");

        let transient_lines = app.build_transient_lines(width);
        let transient_blob = lines_to_plain_strings(&transient_lines).join("\n");
        assert!(
            !transient_blob.contains("CodexPotter: retry"),
            "expected retry block to be cleared: {transient_blob:?}"
        );

        let cells = drain_history_cell_strings(&mut rx_app, width);
        assert!(
            cells.is_empty(),
            "expected no history cells; got: {cells:?}"
        );
    }

    #[test]
    fn potter_stream_recovery_update_replaces_existing_retry_block_in_place() {
        let width: u16 = 80;

        let (tx_raw, mut rx_app) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let processor = AppServerEventProcessor::new(app_event_tx.clone(), Verbosity::default());
        let (op_tx, mut op_rx) = unbounded_channel::<Op>();
        let mut bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx: app_event_tx.clone(),
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        bottom_pane.set_task_running(true);

        let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
        let mut app = RenderAppState::new(
            processor,
            app_event_tx,
            Some(op_tx),
            bottom_pane,
            crate::prompt_history_store::PromptHistoryStore::new(),
            file_search,
            VecDeque::new(),
        );

        app.handle_codex_event(
            crate::tui::FrameRequester::test_dummy(),
            Event {
                id: "err-1".into(),
                msg: EventMsg::PotterStreamRecoveryUpdate {
                    attempt: 1,
                    max_attempts: 10,
                    error_message:
                        "stream disconnected before completion: error sending request for url (...)"
                            .to_string(),
                },
            },
        )
        .expect("handle first update");

        let transient_blob = lines_to_plain_strings(&app.build_transient_lines(width)).join("\n");
        assert!(
            transient_blob.contains("• CodexPotter: retry 1/10"),
            "missing retry 1/10: {transient_blob:?}"
        );

        app.handle_codex_event(
            crate::tui::FrameRequester::test_dummy(),
            Event {
                id: "err-3".into(),
                msg: EventMsg::PotterStreamRecoveryUpdate {
                    attempt: 3,
                    max_attempts: 10,
                    error_message:
                        "stream disconnected before completion: error sending request for url (...)"
                            .to_string(),
                },
            },
        )
        .expect("handle second update");

        let transient_blob = lines_to_plain_strings(&app.build_transient_lines(width)).join("\n");
        assert!(
            transient_blob.contains("• CodexPotter: retry 3/10"),
            "missing retry 3/10: {transient_blob:?}"
        );
        assert!(
            !transient_blob.contains("• CodexPotter: retry 1/10"),
            "expected retry block to update in place: {transient_blob:?}"
        );

        let cells = drain_history_cell_strings(&mut rx_app, width);
        assert!(
            cells.is_empty(),
            "expected no history cells; got: {cells:?}"
        );
        assert!(
            op_rx.try_recv().is_err(),
            "expected no Continue op for stream recovery update"
        );
    }

    #[test]
    fn stream_error_status_updates_and_restores_on_next_event() {
        let width: u16 = 80;

        let (tx_raw, mut rx_app) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let processor = AppServerEventProcessor::new(app_event_tx.clone(), Verbosity::default());
        let (op_tx, mut op_rx) = unbounded_channel::<Op>();
        let mut bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx: app_event_tx.clone(),
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        bottom_pane.set_task_running(true);
        bottom_pane.update_status_header("Inspecting for code duplication".to_string());
        if let Some(status) = bottom_pane.status_indicator_mut() {
            status.pause_timer_at(Instant::now());
        }
        let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
        let mut app = RenderAppState::new(
            processor,
            app_event_tx,
            Some(op_tx),
            bottom_pane,
            crate::prompt_history_store::PromptHistoryStore::new(),
            file_search,
            VecDeque::new(),
        );

        app.handle_codex_event(
            crate::tui::FrameRequester::test_dummy(),
            Event {
                id: "stream-error-1".into(),
                msg: EventMsg::StreamError(StreamErrorEvent {
                    message: "Reconnecting... 1/5".to_string(),
                    codex_error_info: Some(CodexErrorInfo::ResponseStreamDisconnected {
                        http_status_code: None,
                    }),
                    additional_details: Some(
                        "stream disconnected before completion: error sending request for url (...)"
                            .to_string(),
                    ),
                }),
            },
        )
        .expect("handle stream error event");

        let status = app.bottom_pane.status_widget().expect("status indicator");
        pretty_assertions::assert_eq!(status.header(), "Reconnecting... 1/5");
        pretty_assertions::assert_eq!(
            status.details(),
            Some("Stream disconnected before completion: error sending request for url (...)")
        );

        let cells = drain_history_cell_strings(&mut rx_app, width);
        assert!(
            cells.is_empty(),
            "expected no history cells; got: {cells:?}"
        );
        assert!(
            op_rx.try_recv().is_err(),
            "expected no Continue op for stream error"
        );

        app.handle_codex_event(
            crate::tui::FrameRequester::test_dummy(),
            Event {
                id: "stream-error-2".into(),
                msg: EventMsg::StreamError(StreamErrorEvent {
                    message: "Reconnecting... 2/5".to_string(),
                    codex_error_info: Some(CodexErrorInfo::ResponseStreamDisconnected {
                        http_status_code: None,
                    }),
                    additional_details: Some(
                        "stream disconnected before completion: error sending request for url (...)"
                            .to_string(),
                    ),
                }),
            },
        )
        .expect("handle stream error event");

        let status = app.bottom_pane.status_widget().expect("status indicator");
        pretty_assertions::assert_eq!(status.header(), "Reconnecting... 2/5");

        app.handle_codex_event(
            crate::tui::FrameRequester::test_dummy(),
            Event {
                id: "delta".into(),
                msg: EventMsg::AgentMessageDelta(AgentMessageDeltaEvent {
                    delta: "hello".to_string(),
                }),
            },
        )
        .expect("handle delta");

        let status = app.bottom_pane.status_widget().expect("status indicator");
        pretty_assertions::assert_eq!(status.header(), "Inspecting for code duplication");
        pretty_assertions::assert_eq!(status.details(), None);
    }

    #[test]
    fn non_retryable_error_inserts_error_history_cell() {
        let width: u16 = 80;

        let (tx_raw, mut rx_app) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let processor = AppServerEventProcessor::new(app_event_tx.clone(), Verbosity::default());
        let (op_tx, mut op_rx) = unbounded_channel::<Op>();
        let mut bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx: app_event_tx.clone(),
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        bottom_pane.set_task_running(true);
        let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
        let mut app = RenderAppState::new(
            processor,
            app_event_tx,
            Some(op_tx),
            bottom_pane,
            crate::prompt_history_store::PromptHistoryStore::new(),
            file_search,
            VecDeque::new(),
        );

        app.handle_codex_event(
            crate::tui::FrameRequester::test_dummy(),
            Event {
                id: "err".into(),
                msg: EventMsg::Error(ErrorEvent {
                    message: "unauthorized".to_string(),
                    codex_error_info: Some(CodexErrorInfo::Unauthorized),
                }),
            },
        )
        .expect("handle codex event");

        let cells = drain_history_cell_strings(&mut rx_app, width);
        pretty_assertions::assert_eq!(cells.len(), 1);
        let blob = cells[0].join("\n");
        assert!(
            blob.contains("■ unauthorized"),
            "unexpected error cell: {blob:?}"
        );

        assert!(
            op_rx.try_recv().is_err(),
            "expected no Continue op for non-retryable errors"
        );
    }

    #[test]
    fn stream_recovery_gave_up_inserts_unrecoverable_cell_and_exits_on_round_finished_task_failed()
    {
        let width: u16 = 80;

        let (tx_raw, mut rx_app) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let processor = AppServerEventProcessor::new(app_event_tx.clone(), Verbosity::default());
        let (op_tx, mut op_rx) = unbounded_channel::<Op>();
        let mut bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx: app_event_tx.clone(),
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        bottom_pane.set_task_running(true);
        let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
        let mut app = RenderAppState::new(
            processor,
            app_event_tx,
            Some(op_tx),
            bottom_pane,
            crate::prompt_history_store::PromptHistoryStore::new(),
            file_search,
            VecDeque::new(),
        );

        app.handle_codex_event(
            crate::tui::FrameRequester::test_dummy(),
            Event {
                id: "err".into(),
                msg: EventMsg::PotterStreamRecoveryGaveUp {
                    error_message:
                        "stream disconnected before completion: error sending request for url (...)"
                            .to_string(),
                    attempts: 10,
                    max_attempts: 10,
                },
            },
        )
        .expect("handle codex event");

        assert!(
            !app.exit_after_next_draw,
            "expected app to wait for PotterRoundFinished"
        );

        let cells = drain_history_cell_strings(&mut rx_app, width);
        pretty_assertions::assert_eq!(cells.len(), 1);
        let blob = cells[0].join("\n");
        assert!(
            blob.contains("■ CodexPotter: unrecoverable error after 10 retries"),
            "unexpected unrecoverable cell: {blob:?}"
        );
        assert!(
            blob.contains(
                "Stream disconnected before completion: error sending request for url (...)"
            ),
            "missing underlying error message: {blob:?}"
        );

        assert!(
            op_rx.try_recv().is_err(),
            "expected no Continue op after giving up"
        );

        app.handle_codex_event(
            crate::tui::FrameRequester::test_dummy(),
            Event {
                id: "round-finished".into(),
                msg: EventMsg::PotterRoundFinished {
                    outcome: codex_protocol::protocol::PotterRoundOutcome::TaskFailed {
                        message: "stream recovery gave up".to_string(),
                    },
                },
            },
        )
        .expect("handle round finished event");

        assert!(
            matches!(app.exit_reason, ExitReason::TaskFailed(_)),
            "expected TaskFailed exit reason; got: {:?}",
            app.exit_reason
        );
        assert!(app.exit_after_next_draw, "expected app to request exit");
    }

    #[test]
    fn round_renderer_context_window_percent_uses_baseline_and_last_token_usage() {
        let (tx_raw, _rx_app) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let processor = AppServerEventProcessor::new(app_event_tx.clone(), Verbosity::default());
        let (op_tx, _op_rx) = unbounded_channel::<Op>();
        let mut bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx: app_event_tx.clone(),
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        bottom_pane.set_prompt_footer_context(PromptFooterContext::new(
            PathBuf::from("project"),
            Some("main".to_string()),
        ));
        let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
        let mut app = RenderAppState::new(
            processor,
            app_event_tx,
            Some(op_tx),
            bottom_pane,
            crate::prompt_history_store::PromptHistoryStore::new(),
            file_search,
            VecDeque::new(),
        );

        app.processor.handle_codex_event(Event {
            id: "token-count".into(),
            msg: EventMsg::TokenCount(TokenCountEvent {
                info: Some(TokenUsageInfo {
                    // Simulate cumulative billing usage (should not drive the context window percent).
                    total_token_usage: TokenUsage {
                        total_tokens: 100_000,
                        ..TokenUsage::default()
                    },
                    // Simulate Codex's estimated tokens currently in the context window.
                    last_token_usage: TokenUsage {
                        total_tokens: 20_000,
                        ..TokenUsage::default()
                    },
                    model_context_window: Some(128_000),
                }),
                rate_limits: None,
            }),
        });

        app.update_bottom_pane_context_window();

        assert_eq!(app.bottom_pane.context_window_percent(), Some(93));
        assert_eq!(app.bottom_pane.context_window_used_tokens(), None);
        assert_eq!(app.processor.token_usage.total_tokens, 100_000);
    }

    #[test]
    fn round_renderer_context_window_fallback_shows_used_tokens() {
        let (tx_raw, _rx_app) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let processor = AppServerEventProcessor::new(app_event_tx.clone(), Verbosity::default());
        let (op_tx, _op_rx) = unbounded_channel::<Op>();
        let bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx: app_event_tx.clone(),
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
        let mut app = RenderAppState::new(
            processor,
            app_event_tx,
            Some(op_tx),
            bottom_pane,
            crate::prompt_history_store::PromptHistoryStore::new(),
            file_search,
            VecDeque::new(),
        );

        app.processor.token_usage = TokenUsage {
            total_tokens: 123_456,
            ..TokenUsage::default()
        };
        app.processor.model_context_window = None;

        app.update_bottom_pane_context_window();

        assert_eq!(app.bottom_pane.context_window_percent(), None);
        assert_eq!(app.bottom_pane.context_window_used_tokens(), Some(123_456));
    }

    #[test]
    fn round_renderer_composer_processes_repeat_cursor_movement() {
        use crossterm::event::KeyCode;
        use crossterm::event::KeyEvent;
        use crossterm::event::KeyEventKind;
        use crossterm::event::KeyModifiers;

        let (tx_raw, _rx_app) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let processor = AppServerEventProcessor::new(app_event_tx.clone(), Verbosity::default());
        let (op_tx, _op_rx) = unbounded_channel::<Op>();
        let bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx: app_event_tx.clone(),
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
        let mut app = RenderAppState::new(
            processor,
            app_event_tx,
            Some(op_tx),
            bottom_pane,
            crate::prompt_history_store::PromptHistoryStore::new(),
            file_search,
            VecDeque::new(),
        );

        app.bottom_pane
            .composer_mut()
            .set_text_content("hello".to_string());
        let area = Rect::new(0, 0, 80, 10);
        let before =
            crate::render::renderable::Renderable::cursor_pos(&app.bottom_pane, area).unwrap();

        let mut right_repeat = KeyEvent::new(KeyCode::Right, KeyModifiers::NONE);
        right_repeat.kind = KeyEventKind::Repeat;
        app.handle_key_event(right_repeat, crate::tui::FrameRequester::test_dummy(), 80);

        let after =
            crate::render::renderable::Renderable::cursor_pos(&app.bottom_pane, area).unwrap();
        assert!(
            after.0 > before.0,
            "expected cursor to move right on Repeat (before={before:?}, after={after:?})",
        );
    }

    #[test]
    fn round_renderer_composer_processes_repeat_ctrl_w() {
        use crossterm::event::KeyCode;
        use crossterm::event::KeyEvent;
        use crossterm::event::KeyEventKind;
        use crossterm::event::KeyModifiers;

        let (tx_raw, _rx_app) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let processor = AppServerEventProcessor::new(app_event_tx.clone(), Verbosity::default());
        let (op_tx, _op_rx) = unbounded_channel::<Op>();
        let bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx: app_event_tx.clone(),
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
        let mut app = RenderAppState::new(
            processor,
            app_event_tx,
            Some(op_tx),
            bottom_pane,
            crate::prompt_history_store::PromptHistoryStore::new(),
            file_search,
            VecDeque::new(),
        );

        app.bottom_pane.composer_mut().set_disable_paste_burst(true);
        for ch in "hello world".chars() {
            app.handle_key_event(
                KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
                crate::tui::FrameRequester::test_dummy(),
                80,
            );
        }
        assert_eq!(app.bottom_pane.composer().current_text(), "hello world");

        let mut ctrl_w_repeat = KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL);
        ctrl_w_repeat.kind = KeyEventKind::Repeat;
        app.handle_key_event(ctrl_w_repeat, crate::tui::FrameRequester::test_dummy(), 80);

        assert_eq!(app.bottom_pane.composer().current_text(), "hello ");
    }

    #[test]
    fn round_renderer_slash_mention_inserts_at_and_starts_file_search() {
        use crossterm::event::KeyCode;
        use crossterm::event::KeyEvent;
        use crossterm::event::KeyModifiers;

        let (tx_raw, mut rx_app) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let processor = AppServerEventProcessor::new(app_event_tx.clone(), Verbosity::default());
        let (op_tx, _op_rx) = unbounded_channel::<Op>();
        let bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx: app_event_tx.clone(),
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
        let mut app = RenderAppState::new(
            processor,
            app_event_tx,
            Some(op_tx),
            bottom_pane,
            crate::prompt_history_store::PromptHistoryStore::new(),
            file_search,
            VecDeque::new(),
        );

        app.bottom_pane.composer_mut().set_disable_paste_burst(true);
        for ch in "/mention".chars() {
            app.handle_key_event(
                KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
                crate::tui::FrameRequester::test_dummy(),
                80,
            );
        }
        app.handle_key_event(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            crate::tui::FrameRequester::test_dummy(),
            80,
        );

        assert_eq!(app.bottom_pane.composer().current_text(), "@");

        let mut saw_file_search = false;
        while let Ok(ev) = rx_app.try_recv() {
            if let AppEvent::StartFileSearch(query) = ev {
                assert_eq!(query, "");
                saw_file_search = true;
                break;
            }
        }
        assert!(
            saw_file_search,
            "expected StartFileSearch after inserting '@'"
        );
    }

    #[test]
    fn round_renderer_slash_exit_requests_interrupt_and_exit() {
        use crossterm::event::KeyCode;
        use crossterm::event::KeyEvent;
        use crossterm::event::KeyModifiers;

        let (tx_raw, mut rx_app) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let processor = AppServerEventProcessor::new(app_event_tx.clone(), Verbosity::default());
        let (op_tx, _op_rx) = unbounded_channel::<Op>();
        let bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx: app_event_tx.clone(),
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
        let mut app = RenderAppState::new(
            processor,
            app_event_tx,
            Some(op_tx),
            bottom_pane,
            crate::prompt_history_store::PromptHistoryStore::new(),
            file_search,
            VecDeque::new(),
        );

        app.bottom_pane.composer_mut().set_disable_paste_burst(true);
        for ch in "/exit".chars() {
            app.handle_key_event(
                KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
                crate::tui::FrameRequester::test_dummy(),
                80,
            );
        }
        app.handle_key_event(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            crate::tui::FrameRequester::test_dummy(),
            80,
        );

        assert!(app.exit_after_next_draw, "expected /exit to request exit");

        let mut saw_interrupt = false;
        while let Ok(ev) = rx_app.try_recv() {
            if let AppEvent::CodexOp(Op::Interrupt) = ev {
                saw_interrupt = true;
                break;
            }
        }
        assert!(saw_interrupt, "expected /exit to request Op::Interrupt");
    }

    #[test]
    fn round_renderer_ctrl_c_flushes_pending_minimal_patch_summary_before_exit() {
        use crossterm::event::KeyCode;
        use crossterm::event::KeyEvent;
        use crossterm::event::KeyModifiers;

        let width: u16 = 80;
        let (tx_raw, mut rx_app) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let mut processor =
            AppServerEventProcessor::new(app_event_tx.clone(), Verbosity::default());
        processor.verbosity = Verbosity::Minimal;

        let patch = diffy::create_patch("a\n", "b\n").to_string();
        let mut changes: HashMap<PathBuf, codex_protocol::protocol::FileChange> = HashMap::new();
        changes.insert(
            PathBuf::from("file.txt"),
            codex_protocol::protocol::FileChange::Update {
                unified_diff: patch,
                move_path: None,
            },
        );

        processor.handle_codex_event(Event {
            id: "patch-end".into(),
            msg: EventMsg::PatchApplyEnd(PatchApplyEndEvent {
                call_id: "patch".into(),
                turn_id: "turn-1".into(),
                stdout: String::new(),
                stderr: String::new(),
                success: true,
                changes,
            }),
        });

        let (op_tx, _op_rx) = unbounded_channel::<Op>();
        let bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx: app_event_tx.clone(),
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
        let mut app = RenderAppState::new(
            processor,
            app_event_tx,
            Some(op_tx),
            bottom_pane,
            crate::prompt_history_store::PromptHistoryStore::new(),
            file_search,
            VecDeque::new(),
        );

        app.bottom_pane.set_task_running(true);
        app.handle_key_event(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            crate::tui::FrameRequester::test_dummy(),
            width,
        );

        assert!(app.exit_after_next_draw, "expected Ctrl+C to request exit");

        let events = drain_history_cell_strings(&mut rx_app, width);
        pretty_assertions::assert_eq!(events, vec![vec!["• Edited file.txt (+1 -1)".to_string()]]);
    }

    #[test]
    fn round_renderer_esc_requests_interrupt_when_task_is_running() {
        use crossterm::event::KeyCode;
        use crossterm::event::KeyEvent;
        use crossterm::event::KeyModifiers;

        let (tx_raw, mut rx_app) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let processor = AppServerEventProcessor::new(app_event_tx.clone(), Verbosity::default());
        let (op_tx, _op_rx) = unbounded_channel::<Op>();
        let bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx: app_event_tx.clone(),
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
        let mut app = RenderAppState::new(
            processor,
            app_event_tx,
            Some(op_tx),
            bottom_pane,
            crate::prompt_history_store::PromptHistoryStore::new(),
            file_search,
            VecDeque::new(),
        );

        app.bottom_pane.set_task_running(true);
        app.handle_key_event(
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            crate::tui::FrameRequester::test_dummy(),
            80,
        );

        let mut saw_interrupt = false;
        while let Ok(ev) = rx_app.try_recv() {
            if let AppEvent::CodexOp(Op::Interrupt) = ev {
                saw_interrupt = true;
                break;
            }
        }
        assert!(saw_interrupt, "expected Esc to request Op::Interrupt");
    }

    #[test]
    fn prompt_slash_exit_cancels_without_interrupt_or_history() {
        use crossterm::event::KeyCode;
        use crossterm::event::KeyEvent;
        use crossterm::event::KeyModifiers;

        let (tx_raw, mut rx_app) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx: app_event_tx.clone(),
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
        let mut app = RenderAppState::new_prompt_screen(
            app_event_tx,
            bottom_pane,
            crate::prompt_history_store::PromptHistoryStore::new(),
            file_search,
            true,
            Verbosity::default(),
        );

        app.bottom_pane.set_task_running(false);
        app.bottom_pane.composer_mut().set_disable_paste_burst(true);
        for ch in "/exit".chars() {
            app.handle_key_event(
                KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
                crate::tui::FrameRequester::test_dummy(),
                80,
            );
        }
        app.handle_key_event(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            crate::tui::FrameRequester::test_dummy(),
            80,
        );

        assert_eq!(app.prompt_action, Some(PromptScreenAction::CancelledByUser));

        let (_log_id, entry_count) = app.prompt_history.metadata();
        assert_eq!(
            entry_count, 0,
            "expected prompt screen /exit not to be recorded in history"
        );

        let mut saw_interrupt = false;
        while let Ok(ev) = rx_app.try_recv() {
            if let AppEvent::CodexOp(Op::Interrupt) = ev {
                saw_interrupt = true;
                break;
            }
        }
        assert!(
            !saw_interrupt,
            "did not expect Op::Interrupt in prompt screen"
        );
    }

    #[test]
    fn prompt_ctrl_c_empty_cancels_without_interrupt() {
        use crossterm::event::KeyCode;
        use crossterm::event::KeyEvent;
        use crossterm::event::KeyModifiers;

        let (tx_raw, mut rx_app) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx: app_event_tx.clone(),
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
        let mut app = RenderAppState::new_prompt_screen(
            app_event_tx,
            bottom_pane,
            crate::prompt_history_store::PromptHistoryStore::new(),
            file_search,
            true,
            Verbosity::default(),
        );

        app.bottom_pane.set_task_running(false);
        app.handle_key_event(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            crate::tui::FrameRequester::test_dummy(),
            80,
        );

        assert_eq!(app.prompt_action, Some(PromptScreenAction::CancelledByUser));

        let mut saw_interrupt = false;
        while let Ok(ev) = rx_app.try_recv() {
            if let AppEvent::CodexOp(Op::Interrupt) = ev {
                saw_interrupt = true;
                break;
            }
        }
        assert!(
            !saw_interrupt,
            "did not expect Op::Interrupt in prompt screen"
        );
    }

    #[test]
    fn prompt_enter_submits_prompt_and_records_history() {
        use crossterm::event::KeyCode;
        use crossterm::event::KeyEvent;
        use crossterm::event::KeyModifiers;

        let (tx_raw, mut rx_app) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx: app_event_tx.clone(),
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
        let mut app = RenderAppState::new_prompt_screen(
            app_event_tx,
            bottom_pane,
            crate::prompt_history_store::PromptHistoryStore::new(),
            file_search,
            true,
            Verbosity::default(),
        );

        app.bottom_pane.set_task_running(false);
        app.bottom_pane.composer_mut().set_disable_paste_burst(true);
        for ch in "hello".chars() {
            app.handle_key_event(
                KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
                crate::tui::FrameRequester::test_dummy(),
                80,
            );
        }
        app.handle_key_event(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            crate::tui::FrameRequester::test_dummy(),
            80,
        );

        assert_eq!(
            app.prompt_action,
            Some(PromptScreenAction::Submitted("hello".to_string()))
        );
        assert!(
            app.queued_user_messages.is_empty(),
            "prompt screen should not queue prompts"
        );

        let (log_id, entry_count) = app.prompt_history.metadata();
        assert_eq!(entry_count, 1);
        assert_eq!(
            app.prompt_history.lookup_text(log_id, 0),
            Some("hello".to_string())
        );

        let mut saw_interrupt = false;
        while let Ok(ev) = rx_app.try_recv() {
            if let AppEvent::CodexOp(Op::Interrupt) = ev {
                saw_interrupt = true;
                break;
            }
        }
        assert!(
            !saw_interrupt,
            "did not expect Op::Interrupt in prompt screen"
        );
    }

    #[test]
    fn round_renderer_idle_prompt_is_separated_from_transcript_vt100() {
        let width: u16 = 80;

        let (tx_raw, _rx_app) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let processor = AppServerEventProcessor::new(app_event_tx.clone(), Verbosity::default());
        let (op_tx, _op_rx) = unbounded_channel::<Op>();
        let bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx: app_event_tx.clone(),
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
        let mut app = RenderAppState::new(
            processor,
            app_event_tx,
            Some(op_tx),
            bottom_pane,
            crate::prompt_history_store::PromptHistoryStore::new(),
            file_search,
            VecDeque::new(),
        );
        app.has_emitted_history_lines = true;

        let transient_lines = app.build_transient_lines(width);

        let pane_height = app.bottom_pane.desired_height(width).max(1);
        let transient_height = u16::try_from(transient_lines.len()).unwrap_or(u16::MAX);
        let viewport_height = pane_height.saturating_add(transient_height);

        let history_lines = vec![
            Line::from(
                "─ Worked for 4m 59s ──────────────────────────────────────────────────────",
            ),
            Line::from(""),
            Line::from("• ok"),
        ];
        let history_height = u16::try_from(history_lines.len()).unwrap_or(u16::MAX);
        let height = history_height.saturating_add(viewport_height).max(1);

        let backend = VT100Backend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).expect("create terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                ratatui::widgets::Clear.render(area, frame.buffer_mut());

                let history_height = history_height.min(area.height);
                let history_area = Rect::new(area.x, area.y, area.width, history_height);
                let viewport_area = Rect::new(
                    area.x,
                    area.y + history_height,
                    area.width,
                    area.height.saturating_sub(history_height),
                );

                ratatui::widgets::Paragraph::new(ratatui::text::Text::from(history_lines))
                    .render(history_area, frame.buffer_mut());
                render_runner_viewport(
                    viewport_area,
                    frame.buffer_mut(),
                    &app.bottom_pane,
                    transient_lines,
                );
            })
            .expect("draw");

        assert_snapshot!(
            "round_renderer_idle_prompt_is_separated_from_transcript_vt100",
            terminal.backend().vt100().screen().contents()
        );
    }

    #[test]
    fn round_renderer_round_banner_does_not_add_extra_padding_before_status_vt100() {
        let width: u16 = 80;

        let (tx_raw, _rx_app) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let processor = AppServerEventProcessor::new(app_event_tx.clone(), Verbosity::default());
        let (op_tx, _op_rx) = unbounded_channel::<Op>();
        let mut bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx: app_event_tx.clone(),
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        bottom_pane.set_prompt_footer_context(PromptFooterContext::new(
            PathBuf::from("project"),
            Some("main".to_string()),
        ));
        bottom_pane.set_task_running(true);
        bottom_pane.set_project_started_at(Some(Instant::now()));
        bottom_pane.set_status_header_prefix(Some("Round 1/10".to_string()));
        if let Some(status) = bottom_pane.status_indicator_mut() {
            // Ensure the elapsed timer stays at 0s for a stable snapshot.
            status.pause_timer_at(Instant::now());
        }
        let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
        let mut app = RenderAppState::new(
            processor,
            app_event_tx,
            Some(op_tx),
            bottom_pane,
            crate::prompt_history_store::PromptHistoryStore::new(),
            file_search,
            VecDeque::new(),
        );
        app.has_emitted_history_lines = true;

        let transient_lines = app.build_transient_lines(width);

        let pane_height = app.bottom_pane.desired_height(width).max(1);
        let transient_height = u16::try_from(transient_lines.len()).unwrap_or(u16::MAX);
        let viewport_height = pane_height.saturating_add(transient_height);

        let history_lines =
            crate::history_cell_potter::new_potter_round_started(1, 10).display_lines(width);
        let history_height = u16::try_from(history_lines.len()).unwrap_or(u16::MAX);
        let height = history_height.saturating_add(viewport_height).max(1);

        let backend = VT100Backend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).expect("create terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                ratatui::widgets::Clear.render(area, frame.buffer_mut());

                let history_height = history_height.min(area.height);
                let history_area = Rect::new(area.x, area.y, area.width, history_height);
                let viewport_area = Rect::new(
                    area.x,
                    area.y + history_height,
                    area.width,
                    area.height.saturating_sub(history_height),
                );

                ratatui::widgets::Paragraph::new(ratatui::text::Text::from(history_lines))
                    .render(history_area, frame.buffer_mut());
                render_runner_viewport(
                    viewport_area,
                    frame.buffer_mut(),
                    &app.bottom_pane,
                    transient_lines,
                );
            })
            .expect("draw");

        assert_snapshot!(
            "round_renderer_round_banner_padding_before_status_vt100",
            terminal.backend().vt100().screen().contents()
        );
    }

    #[test]
    fn round_renderer_round_banner_reconnecting_status_renders_details_vt100() {
        let width: u16 = 80;

        let (tx_raw, _rx_app) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let processor = AppServerEventProcessor::new(app_event_tx.clone(), Verbosity::default());
        let (op_tx, _op_rx) = unbounded_channel::<Op>();
        let mut bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx: app_event_tx.clone(),
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        bottom_pane.set_prompt_footer_context(PromptFooterContext::new(
            PathBuf::from("project"),
            Some("main".to_string()),
        ));
        bottom_pane.set_task_running(true);
        bottom_pane.set_project_started_at(Some(Instant::now()));
        bottom_pane.set_status_header_prefix(Some("Round 1/10".to_string()));
        bottom_pane.update_status_header_with_details(
            "Reconnecting... 1/5".to_string(),
            Some(
                "stream disconnected before completion: error sending request for url (https://free.xxsxx.fun/v1/responses)"
                    .to_string(),
            ),
        );
        if let Some(status) = bottom_pane.status_indicator_mut() {
            // Ensure the elapsed timer stays at 0s for a stable snapshot.
            status.pause_timer_at(Instant::now());
        }
        let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
        let mut app = RenderAppState::new(
            processor,
            app_event_tx,
            Some(op_tx),
            bottom_pane,
            crate::prompt_history_store::PromptHistoryStore::new(),
            file_search,
            VecDeque::new(),
        );
        app.has_emitted_history_lines = true;
        app.potter_stream_recovery_retry_cell =
            Some(crate::history_cell_potter::PotterStreamRecoveryRetryCell {
                attempt: 3,
                max_attempts: 10,
                error_message: "stream disconnected before completion: error sending request for url (https://free.xxsxx.fun/v1/responses)".to_string(),
            });

        let transient_lines = app.build_transient_lines(width);

        let pane_height = app.bottom_pane.desired_height(width).max(1);
        let transient_height = u16::try_from(transient_lines.len()).unwrap_or(u16::MAX);
        let viewport_height = pane_height.saturating_add(transient_height);

        let history_lines =
            crate::history_cell_potter::new_potter_round_started(1, 10).display_lines(width);
        let history_height = u16::try_from(history_lines.len()).unwrap_or(u16::MAX);
        let height = history_height.saturating_add(viewport_height).max(1);

        let backend = VT100Backend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).expect("create terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                ratatui::widgets::Clear.render(area, frame.buffer_mut());

                let history_height = history_height.min(area.height);
                let history_area = Rect::new(area.x, area.y, area.width, history_height);
                let viewport_area = Rect::new(
                    area.x,
                    area.y + history_height,
                    area.width,
                    area.height.saturating_sub(history_height),
                );

                ratatui::widgets::Paragraph::new(ratatui::text::Text::from(history_lines))
                    .render(history_area, frame.buffer_mut());
                render_runner_viewport(
                    viewport_area,
                    frame.buffer_mut(),
                    &app.bottom_pane,
                    transient_lines,
                );
            })
            .expect("draw");

        assert_snapshot!(
            "round_renderer_round_banner_reconnecting_status_vt100",
            terminal.backend().vt100().screen().contents()
        );
    }

    #[test]
    fn prompt_idle_prompt_is_separated_from_transcript_vt100() {
        let width: u16 = 80;

        let (tx_raw, _rx_app) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let mut bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx,
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        bottom_pane.set_prompt_footer_context(PromptFooterContext::new(
            PathBuf::from("project"),
            Some("main".to_string()),
        ));
        let transient_lines = vec![Line::from("")];

        let pane_height = bottom_pane.desired_height(width).max(1);
        let transient_height = u16::try_from(transient_lines.len()).unwrap_or(u16::MAX);
        let viewport_height = pane_height.saturating_add(transient_height);

        let history_lines = vec![Line::from("• ok")];
        let history_height = u16::try_from(history_lines.len()).unwrap_or(u16::MAX);
        let height = history_height.saturating_add(viewport_height).max(1);

        let backend = VT100Backend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).expect("create terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                ratatui::widgets::Clear.render(area, frame.buffer_mut());

                let history_height = history_height.min(area.height);
                let history_area = Rect::new(area.x, area.y, area.width, history_height);
                let viewport_area = Rect::new(
                    area.x,
                    area.y + history_height,
                    area.width,
                    area.height.saturating_sub(history_height),
                );

                ratatui::widgets::Paragraph::new(ratatui::text::Text::from(history_lines))
                    .render(history_area, frame.buffer_mut());
                render_runner_viewport(
                    viewport_area,
                    frame.buffer_mut(),
                    &bottom_pane,
                    transient_lines,
                );
            })
            .expect("draw");

        assert_snapshot!(
            "prompt_idle_prompt_is_separated_from_transcript_vt100",
            terminal.backend().vt100().screen().contents()
        );
    }

    #[tokio::test]
    async fn round_renderer_renders_context_compacted_event() {
        let width: u16 = 80;
        let height: u16 = 12;
        let backend = VT100Backend::new(width, height);
        let mut terminal =
            crate::custom_terminal::Terminal::with_options(backend).expect("create terminal");
        terminal.set_viewport_area(Rect::new(0, height - 1, width, 1));

        let (mut proc, mut rx) = make_round_renderer_processor("Explain this: `1 + 1`.");

        let configured = SessionConfiguredEvent {
            session_id: ThreadId::new(),
            forked_from_id: None,
            model: "test-model".to_string(),
            model_provider_id: "test-provider".to_string(),
            cwd: PathBuf::from("project"),
            reasoning_effort: None,
            history_log_id: 0,
            history_entry_count: 0,
            initial_messages: None,
            rollout_path: PathBuf::from("rollout.jsonl"),
        };

        proc.handle_codex_event(Event {
            id: "session".into(),
            msg: EventMsg::SessionConfigured(configured),
        });

        let mut has_emitted_history_lines = false;
        drain_render_history_events(
            &mut rx,
            &mut terminal,
            width,
            &mut has_emitted_history_lines,
        );

        proc.handle_codex_event(Event {
            id: "context-compacted".into(),
            msg: EventMsg::ContextCompacted(ContextCompactedEvent),
        });

        drain_render_history_events(
            &mut rx,
            &mut terminal,
            width,
            &mut has_emitted_history_lines,
        );

        assert_snapshot!(
            "round_renderer_context_compacted_vt100",
            terminal.backend().vt100().screen().contents()
        );
    }

    #[test]
    fn round_renderer_does_not_duplicate_agent_message_on_turn_complete_last_agent_message() {
        let width: u16 = 80;

        let (mut proc, mut rx) = make_round_renderer_processor_without_prompt();

        proc.handle_codex_event(Event {
            id: "agent-message".into(),
            msg: EventMsg::AgentMessage(AgentMessageEvent {
                message: "hello".to_string(),
                phase: None,
            }),
        });

        proc.handle_codex_event(Event {
            id: "turn-complete".into(),
            msg: EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: "turn-1".to_string(),
                last_agent_message: Some("hello".to_string()),
            }),
        });

        let cells = drain_history_cell_strings(&mut rx, width);
        pretty_assertions::assert_eq!(cells, vec![vec!["• hello".to_string()]]);
    }

    #[test]
    fn round_renderer_minimal_dims_commentary_agent_messages() {
        let width: u16 = 80;

        let (mut proc, mut rx) = make_round_renderer_processor_without_prompt();

        proc.handle_codex_event(Event {
            id: "agent-message".into(),
            msg: EventMsg::AgentMessage(AgentMessageEvent {
                message: "hello".to_string(),
                phase: Some(MessagePhase::Commentary),
            }),
        });

        let Ok(AppEvent::InsertHistoryCell(cell)) = rx.try_recv() else {
            panic!("expected an inserted history cell");
        };
        let lines = cell.display_lines(width);
        let Some(content) = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|span| span.content.as_ref().contains("hello"))
        else {
            panic!("expected hello span");
        };

        assert!(
            content.style.add_modifier.contains(Modifier::DIM),
            "expected commentary span to be dimmed"
        );
    }

    #[test]
    fn round_renderer_minimal_keeps_final_answer_agent_messages_normal() {
        let width: u16 = 80;

        let (mut proc, mut rx) = make_round_renderer_processor_without_prompt();

        proc.handle_codex_event(Event {
            id: "agent-message".into(),
            msg: EventMsg::AgentMessage(AgentMessageEvent {
                message: "done".to_string(),
                phase: Some(MessagePhase::FinalAnswer),
            }),
        });

        let Ok(AppEvent::InsertHistoryCell(cell)) = rx.try_recv() else {
            panic!("expected an inserted history cell");
        };
        let lines = cell.display_lines(width);
        let Some(content) = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|span| span.content.as_ref().contains("done"))
        else {
            panic!("expected done span");
        };

        assert!(
            !content.style.add_modifier.contains(Modifier::DIM),
            "expected final answer span to keep normal intensity"
        );
    }

    #[test]
    fn round_renderer_streaming_plan_delta_renders_proposed_plan_block() {
        let width: u16 = 80;

        let (mut proc, mut rx) = make_round_renderer_processor_without_prompt();

        proc.handle_codex_event(Event {
            id: "plan-1".into(),
            msg: EventMsg::PlanDelta(PlanDeltaEvent {
                delta: "- first\n".to_string(),
            }),
        });
        proc.on_commit_tick();

        let cells = drain_history_cell_strings(&mut rx, width);
        pretty_assertions::assert_eq!(
            cells,
            vec![vec![
                "• Proposed Plan".to_string(),
                " ".to_string(),
                "   ".to_string(),
                "  - first".to_string(),
            ]]
        );

        proc.handle_codex_event(Event {
            id: "plan-2".into(),
            msg: EventMsg::PlanDelta(PlanDeltaEvent {
                delta: "- second\n".to_string(),
            }),
        });
        proc.on_commit_tick();

        let cells = drain_history_cell_strings(&mut rx, width);
        pretty_assertions::assert_eq!(cells, vec![vec!["  - second".to_string()]]);

        proc.handle_codex_event(Event {
            id: "turn-complete".into(),
            msg: EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: "turn-1".to_string(),
                last_agent_message: None,
            }),
        });

        let cells = drain_history_cell_strings(&mut rx, width);
        pretty_assertions::assert_eq!(cells, vec![vec!["   ".to_string()]]);
    }

    #[tokio::test]
    async fn round_renderer_vt100_snapshots() {
        let width: u16 = 80;
        let height: u16 = 28;
        let backend = VT100Backend::new(width, height);
        let mut terminal =
            crate::custom_terminal::Terminal::with_options(backend).expect("create terminal");
        terminal.set_viewport_area(Rect::new(0, height - 1, width, 1));

        let (mut proc, mut rx) = make_round_renderer_processor("Explain this: `1 + 1`.");

        let configured = SessionConfiguredEvent {
            session_id: ThreadId::new(),
            forked_from_id: None,
            model: "test-model".to_string(),
            model_provider_id: "test-provider".to_string(),
            cwd: PathBuf::from("project"),
            reasoning_effort: None,
            history_log_id: 0,
            history_entry_count: 0,
            initial_messages: None,
            rollout_path: PathBuf::from("rollout.jsonl"),
        };

        proc.handle_codex_event(Event {
            id: "session".into(),
            msg: EventMsg::SessionConfigured(configured),
        });

        let mut has_emitted_history_lines = false;
        drain_render_history_events(
            &mut rx,
            &mut terminal,
            width,
            &mut has_emitted_history_lines,
        );

        // Stream markdown output in a few chunks to exercise incremental rendering.
        proc.handle_codex_event(Event {
            id: "delta-1".into(),
            msg: EventMsg::AgentMessageDelta(AgentMessageDeltaEvent {
                delta: "## Result\n".into(),
            }),
        });
        drive_stream_to_idle(
            &mut proc,
            &mut rx,
            &mut terminal,
            width,
            &mut has_emitted_history_lines,
        );

        proc.handle_codex_event(Event {
            id: "delta-2".into(),
            msg: EventMsg::AgentMessageDelta(AgentMessageDeltaEvent {
                delta: "- **Answer**: `2`\n".into(),
            }),
        });
        drive_stream_to_idle(
            &mut proc,
            &mut rx,
            &mut terminal,
            width,
            &mut has_emitted_history_lines,
        );

        assert_snapshot!(
            "round_renderer_streaming_partial_vt100",
            terminal.backend().vt100().screen().contents()
        );

        proc.handle_codex_event(Event {
            id: "delta-3".into(),
            msg: EventMsg::AgentMessageDelta(AgentMessageDeltaEvent {
                delta: "\n```sh\nprintf 'hello\\n'\n```\n".into(),
            }),
        });
        drive_stream_to_idle(
            &mut proc,
            &mut rx,
            &mut terminal,
            width,
            &mut has_emitted_history_lines,
        );

        // Finalize stream without a final AgentMessage, matching the streaming-only code path.
        proc.handle_codex_event(Event {
            id: "turn-complete".into(),
            msg: EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: "turn-1".to_string(),
                last_agent_message: None,
            }),
        });
        drain_render_history_events(
            &mut rx,
            &mut terminal,
            width,
            &mut has_emitted_history_lines,
        );

        // Exec output should render with truncation.
        let command = vec!["bash".into(), "-lc".into(), "printf 'line\\n'".into()];
        proc.handle_codex_event(Event {
            id: "exec-end".into(),
            msg: EventMsg::ExecCommandEnd(ExecCommandEndEvent {
                call_id: "exec-1".into(),
                process_id: None,
                turn_id: "turn-1".into(),
                command,
                cwd: PathBuf::from("project"),
                parsed_cmd: Vec::new(),
                source: ExecCommandSource::Agent,
                interaction_input: None,
                stdout: String::new(),
                stderr: String::new(),
                aggregated_output: (1..=30).map(|i| format!("line {i}\n")).collect::<String>(),
                exit_code: 0,
                duration: std::time::Duration::from_millis(1200),
                formatted_output: String::new(),
            }),
        });
        drain_render_history_events(
            &mut rx,
            &mut terminal,
            width,
            &mut has_emitted_history_lines,
        );

        // Patch apply should render a diff summary.
        let patch = diffy::create_patch("old\n", "new\n").to_string();
        let mut changes: HashMap<PathBuf, codex_protocol::protocol::FileChange> = HashMap::new();
        changes.insert(
            PathBuf::from("example.txt"),
            codex_protocol::protocol::FileChange::Update {
                unified_diff: patch,
                move_path: None,
            },
        );

        proc.handle_codex_event(Event {
            id: "patch-end".into(),
            msg: EventMsg::PatchApplyEnd(PatchApplyEndEvent {
                call_id: "patch-1".into(),
                turn_id: "turn-1".into(),
                stdout: String::new(),
                stderr: String::new(),
                success: true,
                changes,
            }),
        });

        // In `Verbosity::Minimal` successful patch applications are buffered and rendered as a
        // coalesced compact summary. Flush explicitly so the snapshot includes the patch cell.
        proc.flush_pending_compact_patch_changes();
        drain_render_history_events(
            &mut rx,
            &mut terminal,
            width,
            &mut has_emitted_history_lines,
        );

        assert_snapshot!(
            "round_renderer_end_to_end_vt100",
            terminal.backend().vt100().screen().contents()
        );
    }

    #[test]
    fn round_renderer_esc_interrupt_flow_vt100() {
        use crossterm::event::KeyCode;
        use crossterm::event::KeyEvent;
        use crossterm::event::KeyModifiers;

        let width: u16 = 80;
        let height: u16 = 12;
        let backend = VT100Backend::new(width, height);
        let mut terminal =
            crate::custom_terminal::Terminal::with_options(backend).expect("create terminal");
        terminal.set_viewport_area(Rect::new(0, height - 1, width, 1));

        let (tx_raw, mut rx) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let processor = AppServerEventProcessor::new(app_event_tx.clone(), Verbosity::default());
        let (op_tx, _op_rx) = unbounded_channel::<Op>();
        let mut bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx: app_event_tx.clone(),
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        bottom_pane.set_task_running(true);

        let file_search = FileSearchManager::new(std::env::temp_dir(), app_event_tx.clone());
        let mut app = RenderAppState::new(
            processor,
            app_event_tx,
            Some(op_tx),
            bottom_pane,
            crate::prompt_history_store::PromptHistoryStore::new(),
            file_search,
            VecDeque::new(),
        );

        app.processor.emit_user_prompt("test prompt".to_string());
        let mut has_emitted_history_lines = false;
        drain_render_history_events(
            &mut rx,
            &mut terminal,
            width,
            &mut has_emitted_history_lines,
        );

        app.handle_key_event(
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            crate::tui::FrameRequester::test_dummy(),
            width,
        );

        let mut saw_interrupt = false;
        while let Ok(ev) = rx.try_recv() {
            if let AppEvent::CodexOp(Op::Interrupt) = ev {
                saw_interrupt = true;
                break;
            }
        }
        assert!(saw_interrupt, "expected Esc to request Op::Interrupt");

        app.processor.handle_codex_event(Event {
            id: "turn-aborted".into(),
            msg: EventMsg::TurnAborted(TurnAbortedEvent {
                turn_id: Some("turn-1".to_string()),
                reason: TurnAbortReason::Interrupted,
            }),
        });
        drain_render_history_events(
            &mut rx,
            &mut terminal,
            width,
            &mut has_emitted_history_lines,
        );

        assert_snapshot!(
            "round_renderer_esc_interrupt_flow_vt100",
            terminal.backend().vt100().screen().contents()
        );
    }

    #[tokio::test]
    async fn round_renderer_inserts_worked_for_separator_before_agent_message_vt100() {
        let width: u16 = 80;
        let height: u16 = 16;
        let backend = VT100Backend::new(width, height);
        let mut terminal =
            crate::custom_terminal::Terminal::with_options(backend).expect("create terminal");
        terminal.set_viewport_area(Rect::new(0, height - 1, width, 1));

        let (mut proc, mut rx) = make_round_renderer_processor("test prompt");

        let mut has_emitted_history_lines = false;
        drain_render_history_events(
            &mut rx,
            &mut terminal,
            width,
            &mut has_emitted_history_lines,
        );

        // Simulate a successful command: it should coalesce into the "Ran" cell.
        proc.handle_codex_event(Event {
            id: "exec-end".into(),
            msg: EventMsg::ExecCommandEnd(ExecCommandEndEvent {
                call_id: "exec-1".into(),
                process_id: None,
                turn_id: "turn-1".into(),
                command: vec!["bash".into(), "-lc".into(), "true".into()],
                cwd: PathBuf::from("project"),
                parsed_cmd: Vec::new(),
                source: ExecCommandSource::Agent,
                interaction_input: None,
                stdout: String::new(),
                stderr: String::new(),
                aggregated_output: String::new(),
                exit_code: 0,
                duration: std::time::Duration::from_millis(1200),
                formatted_output: String::new(),
            }),
        });

        // No cells should be emitted yet; the Ran cell is buffered until the next non-exec output.
        drain_render_history_events(
            &mut rx,
            &mut terminal,
            width,
            &mut has_emitted_history_lines,
        );

        // Start agent output; this should flush the buffered Ran cell and insert the separator.
        proc.current_elapsed_secs = Some(0);
        proc.handle_codex_event(Event {
            id: "delta-1".into(),
            msg: EventMsg::AgentMessageDelta(AgentMessageDeltaEvent {
                delta: "ok\n".into(),
            }),
        });

        drain_render_history_events(
            &mut rx,
            &mut terminal,
            width,
            &mut has_emitted_history_lines,
        );

        drive_stream_to_idle(
            &mut proc,
            &mut rx,
            &mut terminal,
            width,
            &mut has_emitted_history_lines,
        );

        assert_snapshot!(
            "round_renderer_worked_for_separator_vt100",
            terminal.backend().vt100().screen().contents()
        );
    }

    #[tokio::test]
    async fn round_renderer_flushes_agent_stream_before_ran_vt100() {
        let width: u16 = 80;
        let height: u16 = 16;
        let backend = VT100Backend::new(width, height);
        let mut terminal =
            crate::custom_terminal::Terminal::with_options(backend).expect("create terminal");
        terminal.set_viewport_area(Rect::new(0, height - 1, width, 1));

        let (mut proc, mut rx) = make_round_renderer_processor("test prompt");
        proc.verbosity = Verbosity::Simple;
        let mut has_emitted_history_lines = false;
        drain_render_history_events(
            &mut rx,
            &mut terminal,
            width,
            &mut has_emitted_history_lines,
        );

        proc.handle_codex_event(Event {
            id: "delta-1".into(),
            msg: EventMsg::AgentMessageDelta(AgentMessageDeltaEvent {
                delta: "first message.".into(),
            }),
        });
        drain_render_history_events(
            &mut rx,
            &mut terminal,
            width,
            &mut has_emitted_history_lines,
        );

        proc.handle_codex_event(Event {
            id: "exec-end".into(),
            msg: EventMsg::ExecCommandEnd(ExecCommandEndEvent {
                call_id: "exec-1".into(),
                process_id: None,
                turn_id: "turn-1".into(),
                command: vec!["bash".into(), "-lc".into(), "echo tool".into()],
                cwd: PathBuf::from("project"),
                parsed_cmd: Vec::new(),
                source: ExecCommandSource::Agent,
                interaction_input: None,
                stdout: String::new(),
                stderr: String::new(),
                aggregated_output: String::new(),
                exit_code: 0,
                duration: Duration::from_millis(1200),
                formatted_output: String::new(),
            }),
        });
        drain_render_history_events(
            &mut rx,
            &mut terminal,
            width,
            &mut has_emitted_history_lines,
        );

        proc.handle_codex_event(Event {
            id: "delta-2".into(),
            msg: EventMsg::AgentMessageDelta(AgentMessageDeltaEvent {
                delta: "second message.".into(),
            }),
        });
        drain_render_history_events(
            &mut rx,
            &mut terminal,
            width,
            &mut has_emitted_history_lines,
        );

        proc.handle_codex_event(Event {
            id: "turn-complete".into(),
            msg: EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: "turn-1".to_string(),
                last_agent_message: None,
            }),
        });
        drain_render_history_events(
            &mut rx,
            &mut terminal,
            width,
            &mut has_emitted_history_lines,
        );

        assert_snapshot!(
            "round_renderer_flushes_agent_stream_before_ran_vt100",
            terminal.backend().vt100().screen().contents()
        );
    }

    #[tokio::test]
    async fn round_renderer_renders_potter_project_succeeded_block_vt100() {
        let width: u16 = 80;
        let height: u16 = 24;
        let backend = VT100Backend::new(width, height);
        let mut terminal =
            crate::custom_terminal::Terminal::with_options(backend).expect("create terminal");
        terminal.set_viewport_area(Rect::new(0, height - 1, width, 1));

        let (mut proc, mut rx) = make_round_renderer_processor("test prompt");
        let mut has_emitted_history_lines = false;
        drain_render_history_events(
            &mut rx,
            &mut terminal,
            width,
            &mut has_emitted_history_lines,
        );

        proc.handle_codex_event(Event {
            id: "exec-end".into(),
            msg: EventMsg::ExecCommandEnd(ExecCommandEndEvent {
                call_id: "exec-1".into(),
                process_id: None,
                turn_id: "turn-1".into(),
                command: vec!["bash".into(), "-lc".into(), "true".into()],
                cwd: PathBuf::from("project"),
                parsed_cmd: Vec::new(),
                source: ExecCommandSource::Agent,
                interaction_input: None,
                stdout: String::new(),
                stderr: String::new(),
                aggregated_output: String::new(),
                exit_code: 0,
                duration: Duration::from_millis(1200),
                formatted_output: String::new(),
            }),
        });
        drain_render_history_events(
            &mut rx,
            &mut terminal,
            width,
            &mut has_emitted_history_lines,
        );

        proc.current_elapsed_secs = Some(0);
        proc.handle_codex_event(Event {
            id: "delta-1".into(),
            msg: EventMsg::AgentMessageDelta(AgentMessageDeltaEvent {
                delta: "- Finished the project.\n".into(),
            }),
        });
        proc.handle_codex_event(Event {
            id: "potter-succeeded".into(),
            msg: EventMsg::PotterProjectSucceeded {
                rounds: 4,
                duration: Duration::from_secs(24 * 60 + 34),
                user_prompt_file: PathBuf::from(".codexpotter/projects/2026/02/01/11/MAIN.md"),
                git_commit_start: String::from("fb827a203635875b58d7e6792da84f22d723d41b"),
                git_commit_end: String::from("662d232cafebabedeadbeefdeadbeefdeadbeef"),
            },
        });
        proc.handle_codex_event(Event {
            id: "turn-complete".into(),
            msg: EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: "turn-1".to_string(),
                last_agent_message: None,
            }),
        });

        drain_render_history_events(
            &mut rx,
            &mut terminal,
            width,
            &mut has_emitted_history_lines,
        );
        drive_stream_to_idle(
            &mut proc,
            &mut rx,
            &mut terminal,
            width,
            &mut has_emitted_history_lines,
        );

        assert_snapshot!(
            "round_renderer_potter_project_succeeded_block_vt100",
            terminal.backend().vt100().screen().contents()
        );
    }

    #[test]
    fn round_renderer_live_explored_renders_in_viewport_and_merges_calls_vt100() {
        let width: u16 = 80;

        let (tx_raw, mut rx) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let mut proc = AppServerEventProcessor::new(app_event_tx, Verbosity::default());

        let base = ExecCommandEndEvent {
            call_id: "unused".into(),
            process_id: None,
            turn_id: "turn-1".into(),
            command: vec!["bash".into(), "-lc".into(), "true".into()],
            cwd: PathBuf::from("project"),
            parsed_cmd: Vec::new(),
            source: ExecCommandSource::Agent,
            interaction_input: None,
            stdout: String::new(),
            stderr: String::new(),
            aggregated_output: String::new(),
            exit_code: 0,
            duration: std::time::Duration::from_millis(1),
            formatted_output: String::new(),
        };

        // Simulate a burst of "exploring" exec results arriving over time.
        for (id, parsed_cmd) in [
            (
                "explore-1",
                vec![ParsedCommand::ListFiles {
                    cmd: "ls".into(),
                    path: None,
                }],
            ),
            (
                "explore-2",
                vec![ParsedCommand::ListFiles {
                    cmd: "ls -la".into(),
                    path: Some(".codexpotter".into()),
                }],
            ),
            (
                "explore-3",
                vec![ParsedCommand::Read {
                    cmd: "cat".into(),
                    name: "README.md".into(),
                    path: PathBuf::from("README.md"),
                }],
            ),
            (
                "explore-4",
                vec![ParsedCommand::Read {
                    cmd: "cat".into(),
                    name: "MAIN.md".into(),
                    path: PathBuf::from("MAIN.md"),
                }],
            ),
            (
                "explore-5",
                vec![ParsedCommand::Search {
                    cmd: "rg -n \"KeyCode::Tab\"".into(),
                    query: Some("KeyCode::Tab|\\\\bTab\\\\b".into()),
                    path: Some("cli".into()),
                }],
            ),
        ] {
            proc.handle_codex_event(Event {
                id: id.into(),
                msg: EventMsg::ExecCommandEnd(ExecCommandEndEvent {
                    call_id: id.into(),
                    parsed_cmd,
                    ..base.clone()
                }),
            });
        }

        // No history cell events should have been emitted yet; explored output should render
        // live in the viewport instead.
        assert!(rx.try_recv().is_err());

        let Some(explored) = proc.pending_exploring_cell.as_ref() else {
            panic!("expected a pending explored cell");
        };
        let mut exploring_lines = Vec::new();
        exploring_lines.push(Line::from(""));
        exploring_lines.extend(explored.display_lines(width));

        let prompt_lines =
            history_cell::new_user_prompt("test prompt".to_string()).display_lines(width);

        let (tx_raw, _rx) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);
        let mut bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx,
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        bottom_pane.set_prompt_footer_context(PromptFooterContext::new(
            PathBuf::from("project"),
            Some("main".to_string()),
        ));
        bottom_pane.set_task_running(true);
        if let Some(status) = bottom_pane.status_indicator_mut() {
            // Ensure the elapsed timer stays at 0s for a stable snapshot.
            status.pause_timer_at(Instant::now());
        }

        let pane_height = bottom_pane.desired_height(width).max(1);
        let exploring_height = u16::try_from(exploring_lines.len()).unwrap_or(u16::MAX);
        let viewport_height = pane_height.saturating_add(exploring_height);
        let prompt_height = u16::try_from(prompt_lines.len()).unwrap_or(u16::MAX);
        let height = prompt_height.saturating_add(viewport_height).max(1);

        let backend = VT100Backend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).expect("create terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                ratatui::widgets::Clear.render(area, frame.buffer_mut());

                let prompt_height = prompt_height.min(area.height);
                let prompt_area =
                    ratatui::layout::Rect::new(area.x, area.y, area.width, prompt_height);
                let viewport_area = ratatui::layout::Rect::new(
                    area.x,
                    area.y + prompt_height,
                    area.width,
                    area.height.saturating_sub(prompt_height),
                );

                ratatui::widgets::Paragraph::new(ratatui::text::Text::from(prompt_lines))
                    .render(prompt_area, frame.buffer_mut());
                render_runner_viewport(
                    viewport_area,
                    frame.buffer_mut(),
                    &bottom_pane,
                    exploring_lines,
                );
            })
            .expect("draw");

        assert_snapshot!(
            "round_renderer_live_explored_in_viewport_vt100",
            terminal.backend().vt100().screen().contents()
        );
    }

    #[test]
    fn round_renderer_live_explored_coalesces_reads_across_mixed_calls_vt100() {
        let width: u16 = 80;

        let (tx_raw, mut rx) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let mut proc = AppServerEventProcessor::new(app_event_tx, Verbosity::default());

        let base = ExecCommandEndEvent {
            call_id: "unused".into(),
            process_id: None,
            turn_id: "turn-1".into(),
            command: vec!["bash".into(), "-lc".into(), "true".into()],
            cwd: PathBuf::from("project"),
            parsed_cmd: Vec::new(),
            source: ExecCommandSource::Agent,
            interaction_input: None,
            stdout: String::new(),
            stderr: String::new(),
            aggregated_output: String::new(),
            exit_code: 0,
            duration: std::time::Duration::from_millis(1),
            formatted_output: String::new(),
        };

        // Simulate a "mixed" exploring call that ends in a Read, followed by another
        // Read of the same file. The renderer should coalesce consecutive Read lines
        // even when they come from different calls.
        for (id, parsed_cmd) in [
            (
                "explore-1",
                vec![
                    ParsedCommand::ListFiles {
                        cmd: "ls -la".into(),
                        path: Some(".codexpotter".into()),
                    },
                    ParsedCommand::Read {
                        cmd: "sed -n '1,240p'".into(),
                        name: "resume_design.md".into(),
                        path: PathBuf::from(".codexpotter/resume_design.md"),
                    },
                ],
            ),
            (
                "explore-2",
                vec![ParsedCommand::Read {
                    cmd: "sed -n '240,520p'".into(),
                    name: "resume_design.md".into(),
                    path: PathBuf::from(".codexpotter/resume_design.md"),
                }],
            ),
        ] {
            proc.handle_codex_event(Event {
                id: id.into(),
                msg: EventMsg::ExecCommandEnd(ExecCommandEndEvent {
                    call_id: id.into(),
                    parsed_cmd,
                    ..base.clone()
                }),
            });
        }

        assert!(rx.try_recv().is_err());

        let Some(explored) = proc.pending_exploring_cell.as_ref() else {
            panic!("expected a pending explored cell");
        };
        let mut exploring_lines = Vec::new();
        exploring_lines.push(Line::from(""));
        exploring_lines.extend(explored.display_lines(width));

        let prompt_lines =
            history_cell::new_user_prompt("test prompt".to_string()).display_lines(width);

        let (tx_raw, _rx) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);
        let mut bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx,
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        bottom_pane.set_prompt_footer_context(PromptFooterContext::new(
            PathBuf::from("project"),
            Some("main".to_string()),
        ));
        bottom_pane.set_task_running(true);
        if let Some(status) = bottom_pane.status_indicator_mut() {
            status.pause_timer_at(Instant::now());
        }

        let pane_height = bottom_pane.desired_height(width).max(1);
        let exploring_height = u16::try_from(exploring_lines.len()).unwrap_or(u16::MAX);
        let viewport_height = pane_height.saturating_add(exploring_height);
        let prompt_height = u16::try_from(prompt_lines.len()).unwrap_or(u16::MAX);
        let height = prompt_height.saturating_add(viewport_height).max(1);

        let backend = VT100Backend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).expect("create terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                ratatui::widgets::Clear.render(area, frame.buffer_mut());

                let prompt_height = prompt_height.min(area.height);
                let prompt_area =
                    ratatui::layout::Rect::new(area.x, area.y, area.width, prompt_height);
                let viewport_area = ratatui::layout::Rect::new(
                    area.x,
                    area.y + prompt_height,
                    area.width,
                    area.height.saturating_sub(prompt_height),
                );

                ratatui::widgets::Paragraph::new(ratatui::text::Text::from(prompt_lines))
                    .render(prompt_area, frame.buffer_mut());
                render_runner_viewport(
                    viewport_area,
                    frame.buffer_mut(),
                    &bottom_pane,
                    exploring_lines,
                );
            })
            .expect("draw");

        assert_snapshot!(
            "round_renderer_live_explored_coalesces_mixed_call_reads_vt100",
            terminal.backend().vt100().screen().contents()
        );
    }

    #[test]
    fn round_renderer_ctrl_c_preserves_pending_explored_output_vt100() {
        use crossterm::event::KeyCode;
        use crossterm::event::KeyEvent;
        use crossterm::event::KeyEventKind;
        use crossterm::event::KeyEventState;
        use crossterm::event::KeyModifiers;

        let width: u16 = 80;
        let height: u16 = 18;
        let backend = VT100Backend::new(width, height);
        let mut terminal =
            crate::custom_terminal::Terminal::with_options(backend).expect("create terminal");
        terminal.set_viewport_area(Rect::new(0, height - 6, width, 6));

        let (tx_raw, mut rx) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let mut proc = AppServerEventProcessor::new(app_event_tx.clone(), Verbosity::Simple);
        proc.handle_codex_event(Event {
            id: "session-start".into(),
            msg: EventMsg::PotterProjectStarted {
                user_message: None,
                working_dir: PathBuf::from("project"),
                project_dir: PathBuf::from(".codexpotter/projects/2026/01/29/18"),
                user_prompt_file: PathBuf::from(".codexpotter/projects/2026/01/29/18/MAIN.md"),
            },
        });
        proc.handle_codex_event(Event {
            id: "round-start".into(),
            msg: EventMsg::PotterRoundStarted {
                current: 1,
                total: 10,
            },
        });

        let mut has_emitted_history_lines = false;
        drain_render_history_events(
            &mut rx,
            &mut terminal,
            width,
            &mut has_emitted_history_lines,
        );

        let base = ExecCommandEndEvent {
            call_id: "unused".into(),
            process_id: None,
            turn_id: "turn-1".into(),
            command: vec!["bash".into(), "-lc".into(), "true".into()],
            cwd: PathBuf::from("project"),
            parsed_cmd: Vec::new(),
            source: ExecCommandSource::Agent,
            interaction_input: None,
            stdout: String::new(),
            stderr: String::new(),
            aggregated_output: String::new(),
            exit_code: 0,
            duration: std::time::Duration::from_millis(1),
            formatted_output: String::new(),
        };

        proc.handle_codex_event(Event {
            id: "explore-1".into(),
            msg: EventMsg::ExecCommandEnd(ExecCommandEndEvent {
                call_id: "explore-1".into(),
                command: vec!["bash".into(), "-lc".into(), "ls -la".into()],
                parsed_cmd: vec![ParsedCommand::ListFiles {
                    cmd: "ls -la".into(),
                    path: None,
                }],
                ..base.clone()
            }),
        });
        proc.handle_codex_event(Event {
            id: "explore-2".into(),
            msg: EventMsg::ExecCommandEnd(ExecCommandEndEvent {
                call_id: "explore-2".into(),
                command: vec![
                    "bash".into(),
                    "-lc".into(),
                    "rg -n README.md .codexpotter".into(),
                ],
                parsed_cmd: vec![ParsedCommand::Search {
                    cmd: "rg -n README.md .codexpotter".into(),
                    query: Some("README.md".into()),
                    path: Some(".codexpotter".into()),
                }],
                ..base
            }),
        });

        assert!(rx.try_recv().is_err());
        assert!(proc.pending_exploring_cell.is_some());

        let (codex_op_tx, _codex_op_rx) = unbounded_channel::<Op>();
        let file_search_dir = std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir());
        let file_search = FileSearchManager::new(file_search_dir, app_event_tx.clone());
        let bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx: app_event_tx.clone(),
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        let mut app = RenderAppState::new(
            proc,
            app_event_tx,
            Some(codex_op_tx),
            bottom_pane,
            crate::prompt_history_store::PromptHistoryStore::new(),
            file_search,
            VecDeque::new(),
        );

        app.handle_key_event(
            KeyEvent {
                code: KeyCode::Char('c'),
                modifiers: KeyModifiers::CONTROL,
                kind: KeyEventKind::Press,
                state: KeyEventState::NONE,
            },
            crate::tui::FrameRequester::test_dummy(),
            width,
        );

        drain_render_history_events(
            &mut rx,
            &mut terminal,
            width,
            &mut has_emitted_history_lines,
        );

        crate::terminal_cleanup::clear_inline_viewport_for_exit(&mut terminal)
            .expect("clear viewport");

        assert_snapshot!(
            "round_renderer_ctrl_c_preserves_pending_explored_output_vt100",
            terminal.backend().vt100().screen().contents()
        );
    }

    #[test]
    fn round_renderer_coalesces_success_ran_cells_snapshot() {
        let width: u16 = 80;
        let (tx_raw, mut rx) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let mut proc = AppServerEventProcessor::new(app_event_tx, Verbosity::default());

        let base = ExecCommandEndEvent {
            call_id: "unused".into(),
            process_id: None,
            turn_id: "turn-1".into(),
            command: vec!["bash".into(), "-lc".into(), "true".into()],
            cwd: PathBuf::from("project"),
            parsed_cmd: Vec::new(),
            source: ExecCommandSource::Agent,
            interaction_input: None,
            stdout: String::new(),
            stderr: String::new(),
            aggregated_output: String::new(),
            exit_code: 0,
            duration: std::time::Duration::from_millis(1),
            formatted_output: String::new(),
        };

        for (id, inner) in [
            ("ran-1", "git status --porcelain=v1"),
            ("ran-2", "git --no-pager log -5 --oneline"),
        ] {
            proc.handle_codex_event(Event {
                id: id.into(),
                msg: EventMsg::ExecCommandEnd(ExecCommandEndEvent {
                    call_id: id.into(),
                    command: vec!["bash".into(), "-lc".into(), inner.into()],
                    ..base.clone()
                }),
            });
        }

        // Coalesced Ran output should render live (not emitted as transcript history yet).
        assert!(rx.try_recv().is_err());

        let Some(cell) = proc.pending_success_ran_cell.as_ref() else {
            panic!("expected a pending Ran cell");
        };
        let lines = cell.display_lines(width);
        assert_snapshot!(
            "round_renderer_coalesced_success_ran_cells",
            lines_to_plain_strings(&lines).join("\n")
        );
    }

    #[test]
    fn round_renderer_minimal_hides_coalesced_success_ran_cells() {
        let width: u16 = 80;
        let (tx_raw, mut rx) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let mut proc = AppServerEventProcessor::new(app_event_tx, Verbosity::default());

        proc.handle_codex_event(Event {
            id: "ran-1".into(),
            msg: EventMsg::ExecCommandEnd(ExecCommandEndEvent {
                call_id: "ran-1".into(),
                process_id: None,
                turn_id: "turn-1".into(),
                command: vec!["bash".into(), "-lc".into(), "true".into()],
                cwd: PathBuf::from("project"),
                parsed_cmd: Vec::new(),
                source: ExecCommandSource::Agent,
                interaction_input: None,
                stdout: String::new(),
                stderr: String::new(),
                aggregated_output: String::new(),
                exit_code: 0,
                duration: std::time::Duration::from_millis(1),
                formatted_output: String::new(),
            }),
        });

        proc.handle_codex_event(Event {
            id: "agent-message".into(),
            msg: EventMsg::AgentMessage(AgentMessageEvent {
                message: "ok".into(),
                phase: None,
            }),
        });

        let events = drain_history_cell_strings(&mut rx, width);
        let [separator, agent_message] = events.as_slice() else {
            panic!("expected separator, then agent message");
        };
        assert!(
            separator
                .first()
                .is_some_and(|line| line.chars().all(|ch| ch == '─')),
            "expected a final message separator"
        );
        pretty_assertions::assert_eq!(agent_message, &vec!["• ok".to_string()]);
    }

    #[test]
    fn round_renderer_minimal_hides_failed_ran_cells() {
        let width: u16 = 80;
        let (tx_raw, mut rx) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let mut proc = AppServerEventProcessor::new(app_event_tx, Verbosity::default());

        proc.handle_codex_event(Event {
            id: "ran-failed".into(),
            msg: EventMsg::ExecCommandEnd(ExecCommandEndEvent {
                call_id: "ran-failed".into(),
                process_id: None,
                turn_id: "turn-1".into(),
                command: vec!["bash".into(), "-lc".into(), "false".into()],
                cwd: PathBuf::from("project"),
                parsed_cmd: Vec::new(),
                source: ExecCommandSource::Agent,
                interaction_input: None,
                stdout: String::new(),
                stderr: "nope".into(),
                aggregated_output: String::new(),
                exit_code: 1,
                duration: std::time::Duration::from_millis(1),
                formatted_output: String::new(),
            }),
        });

        proc.handle_codex_event(Event {
            id: "agent-message".into(),
            msg: EventMsg::AgentMessage(AgentMessageEvent {
                message: "ok".into(),
                phase: None,
            }),
        });

        let events = drain_history_cell_strings(&mut rx, width);
        let [separator, agent_message] = events.as_slice() else {
            panic!("expected separator, then agent message");
        };
        assert!(
            separator
                .first()
                .is_some_and(|line| line.chars().all(|ch| ch == '─')),
            "expected a final message separator"
        );
        pretty_assertions::assert_eq!(agent_message, &vec!["• ok".to_string()]);
    }

    #[test]
    fn round_renderer_minimal_coalesces_consecutive_patch_cells() {
        let width: u16 = 80;
        let (tx_raw, mut rx) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let mut proc = AppServerEventProcessor::new(app_event_tx, Verbosity::default());

        for (id, patch) in [
            ("patch-1", diffy::create_patch("a\n", "b\n").to_string()),
            ("patch-2", diffy::create_patch("b\n", "c\n").to_string()),
        ] {
            let mut changes: HashMap<PathBuf, codex_protocol::protocol::FileChange> =
                HashMap::new();
            changes.insert(
                PathBuf::from("file.txt"),
                codex_protocol::protocol::FileChange::Update {
                    unified_diff: patch,
                    move_path: None,
                },
            );

            proc.handle_codex_event(Event {
                id: format!("{id}-begin"),
                msg: EventMsg::PatchApplyBegin(PatchApplyBeginEvent {
                    call_id: id.into(),
                    turn_id: "turn-1".into(),
                    auto_approved: true,
                    changes: changes.clone(),
                }),
            });
            proc.handle_codex_event(Event {
                id: id.into(),
                msg: EventMsg::PatchApplyEnd(PatchApplyEndEvent {
                    call_id: id.into(),
                    turn_id: "turn-1".into(),
                    stdout: String::new(),
                    stderr: String::new(),
                    success: true,
                    changes,
                }),
            });
        }

        proc.handle_codex_event(Event {
            id: "agent-message".into(),
            msg: EventMsg::AgentMessage(AgentMessageEvent {
                message: "ok".into(),
                phase: None,
            }),
        });

        let events = drain_history_cell_strings(&mut rx, width);
        let [patch, separator, agent_message] = events.as_slice() else {
            panic!("expected patch, separator, then agent message");
        };
        pretty_assertions::assert_eq!(patch, &vec!["• Edited file.txt (+2 -2)".to_string()]);
        assert!(
            separator
                .first()
                .is_some_and(|line| line.chars().all(|ch| ch == '─')),
            "expected a final message separator"
        );
        pretty_assertions::assert_eq!(agent_message, &vec!["• ok".to_string()]);
    }

    #[test]
    fn round_renderer_minimal_coalesces_patch_cells_across_suppressed_events() {
        let width: u16 = 80;
        let (tx_raw, mut rx) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let mut proc = AppServerEventProcessor::new(app_event_tx, Verbosity::default());

        let mut changes_1: HashMap<PathBuf, codex_protocol::protocol::FileChange> = HashMap::new();
        changes_1.insert(
            PathBuf::from("file.txt"),
            codex_protocol::protocol::FileChange::Update {
                unified_diff: diffy::create_patch("a\n", "b\n").to_string(),
                move_path: None,
            },
        );

        proc.handle_codex_event(Event {
            id: "patch-1-begin".into(),
            msg: EventMsg::PatchApplyBegin(PatchApplyBeginEvent {
                call_id: "patch-1".into(),
                turn_id: "turn-1".into(),
                auto_approved: true,
                changes: changes_1.clone(),
            }),
        });
        proc.handle_codex_event(Event {
            id: "patch-1-end".into(),
            msg: EventMsg::PatchApplyEnd(PatchApplyEndEvent {
                call_id: "patch-1".into(),
                turn_id: "turn-1".into(),
                stdout: String::new(),
                stderr: String::new(),
                success: true,
                changes: changes_1,
            }),
        });

        proc.handle_codex_event(Event {
            id: "token-count".into(),
            msg: EventMsg::TokenCount(TokenCountEvent {
                info: None,
                rate_limits: None,
            }),
        });

        proc.handle_codex_event(Event {
            id: "thread-rolled-back".into(),
            msg: EventMsg::ThreadRolledBack(codex_protocol::protocol::ThreadRolledBackEvent {
                num_turns: 1,
            }),
        });

        proc.handle_codex_event(Event {
            id: "ran-failed".into(),
            msg: EventMsg::ExecCommandEnd(ExecCommandEndEvent {
                call_id: "ran-failed".into(),
                process_id: None,
                turn_id: "turn-1".into(),
                command: vec!["bash".into(), "-lc".into(), "false".into()],
                cwd: PathBuf::from("project"),
                parsed_cmd: Vec::new(),
                source: ExecCommandSource::Agent,
                interaction_input: None,
                stdout: String::new(),
                stderr: "nope".into(),
                aggregated_output: String::new(),
                exit_code: 1,
                duration: std::time::Duration::from_millis(1),
                formatted_output: String::new(),
            }),
        });

        let mut changes_2: HashMap<PathBuf, codex_protocol::protocol::FileChange> = HashMap::new();
        changes_2.insert(
            PathBuf::from("file.txt"),
            codex_protocol::protocol::FileChange::Update {
                unified_diff: diffy::create_patch("b\n", "c\n").to_string(),
                move_path: None,
            },
        );

        proc.handle_codex_event(Event {
            id: "patch-2-begin".into(),
            msg: EventMsg::PatchApplyBegin(PatchApplyBeginEvent {
                call_id: "patch-2".into(),
                turn_id: "turn-1".into(),
                auto_approved: true,
                changes: changes_2.clone(),
            }),
        });
        proc.handle_codex_event(Event {
            id: "patch-2-end".into(),
            msg: EventMsg::PatchApplyEnd(PatchApplyEndEvent {
                call_id: "patch-2".into(),
                turn_id: "turn-1".into(),
                stdout: String::new(),
                stderr: String::new(),
                success: true,
                changes: changes_2,
            }),
        });

        proc.handle_codex_event(Event {
            id: "agent-message".into(),
            msg: EventMsg::AgentMessage(AgentMessageEvent {
                message: "ok".into(),
                phase: None,
            }),
        });

        let events = drain_history_cell_strings(&mut rx, width);
        let [patch, separator, agent_message] = events.as_slice() else {
            panic!("expected patch, separator, then agent message");
        };
        pretty_assertions::assert_eq!(patch, &vec!["• Edited file.txt (+2 -2)".to_string()]);
        assert!(
            separator
                .first()
                .is_some_and(|line| line.chars().all(|ch| ch == '─')),
            "expected a final message separator"
        );
        pretty_assertions::assert_eq!(agent_message, &vec!["• ok".to_string()]);
    }

    #[test]
    fn round_renderer_minimal_flushes_pending_patch_summary_on_turn_complete() {
        let width: u16 = 80;
        let (tx_raw, mut rx) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let mut proc = AppServerEventProcessor::new(app_event_tx, Verbosity::Minimal);

        let patch = diffy::create_patch("a\n", "b\n").to_string();
        let mut changes: HashMap<PathBuf, codex_protocol::protocol::FileChange> = HashMap::new();
        changes.insert(
            PathBuf::from("file.txt"),
            codex_protocol::protocol::FileChange::Update {
                unified_diff: patch,
                move_path: None,
            },
        );

        proc.handle_codex_event(Event {
            id: "patch-end".into(),
            msg: EventMsg::PatchApplyEnd(PatchApplyEndEvent {
                call_id: "patch".into(),
                turn_id: "turn-1".into(),
                stdout: String::new(),
                stderr: String::new(),
                success: true,
                changes,
            }),
        });

        proc.handle_codex_event(Event {
            id: "turn-complete".into(),
            msg: EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: "turn-1".to_string(),
                last_agent_message: None,
            }),
        });

        let events = drain_history_cell_strings(&mut rx, width);
        pretty_assertions::assert_eq!(events, vec![vec!["• Edited file.txt (+1 -1)".to_string()]]);
    }

    #[test]
    fn round_renderer_minimal_coalesces_patch_cells_into_changed_file_list() {
        let width: u16 = 80;
        let (tx_raw, mut rx) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let mut proc = AppServerEventProcessor::new(app_event_tx, Verbosity::default());

        let patch_a = diffy::create_patch("old\n", "new\n").to_string();
        let mut changes_a: HashMap<PathBuf, codex_protocol::protocol::FileChange> = HashMap::new();
        changes_a.insert(
            PathBuf::from("a.txt"),
            codex_protocol::protocol::FileChange::Update {
                unified_diff: patch_a,
                move_path: None,
            },
        );

        proc.handle_codex_event(Event {
            id: "patch-a-begin".into(),
            msg: EventMsg::PatchApplyBegin(PatchApplyBeginEvent {
                call_id: "patch-a".into(),
                turn_id: "turn-1".into(),
                auto_approved: true,
                changes: changes_a.clone(),
            }),
        });
        proc.handle_codex_event(Event {
            id: "patch-a-end".into(),
            msg: EventMsg::PatchApplyEnd(PatchApplyEndEvent {
                call_id: "patch-a".into(),
                turn_id: "turn-1".into(),
                stdout: String::new(),
                stderr: String::new(),
                success: true,
                changes: changes_a,
            }),
        });

        let mut changes_b: HashMap<PathBuf, codex_protocol::protocol::FileChange> = HashMap::new();
        changes_b.insert(
            PathBuf::from("b.txt"),
            codex_protocol::protocol::FileChange::Add {
                content: "new\n".to_string(),
            },
        );

        proc.handle_codex_event(Event {
            id: "patch-b-begin".into(),
            msg: EventMsg::PatchApplyBegin(PatchApplyBeginEvent {
                call_id: "patch-b".into(),
                turn_id: "turn-1".into(),
                auto_approved: true,
                changes: changes_b.clone(),
            }),
        });
        proc.handle_codex_event(Event {
            id: "patch-b-end".into(),
            msg: EventMsg::PatchApplyEnd(PatchApplyEndEvent {
                call_id: "patch-b".into(),
                turn_id: "turn-1".into(),
                stdout: String::new(),
                stderr: String::new(),
                success: true,
                changes: changes_b,
            }),
        });

        proc.handle_codex_event(Event {
            id: "agent-message".into(),
            msg: EventMsg::AgentMessage(AgentMessageEvent {
                message: "ok".into(),
                phase: None,
            }),
        });

        let events = drain_history_cell_strings(&mut rx, width);
        let [patch, separator, agent_message] = events.as_slice() else {
            panic!("expected patch, separator, then agent message");
        };
        pretty_assertions::assert_eq!(
            patch,
            &vec![
                "• Changed 2 files (+2 -1)".to_string(),
                "  └ Edited a.txt (+1 -1)".to_string(),
                "    Added b.txt (+1 -0)".to_string(),
            ]
        );
        assert!(
            separator
                .first()
                .is_some_and(|line| line.chars().all(|ch| ch == '─')),
            "expected a final message separator"
        );
        pretty_assertions::assert_eq!(agent_message, &vec!["• ok".to_string()]);
    }

    #[tokio::test]
    async fn round_renderer_coalesces_explored_cells() {
        let (mut proc, mut rx) = make_round_renderer_processor("test prompt");
        let _ = drain_history_cell_strings(&mut rx, 80);
        proc.verbosity = Verbosity::Simple;

        let base = ExecCommandEndEvent {
            call_id: "unused".into(),
            process_id: None,
            turn_id: "turn-1".into(),
            command: vec!["bash".into(), "-lc".into(), "true".into()],
            cwd: PathBuf::from("project"),
            parsed_cmd: Vec::new(),
            source: ExecCommandSource::Agent,
            interaction_input: None,
            stdout: String::new(),
            stderr: String::new(),
            aggregated_output: String::new(),
            exit_code: 0,
            duration: std::time::Duration::from_millis(1),
            formatted_output: String::new(),
        };

        proc.handle_codex_event(Event {
            id: "explore-1".into(),
            msg: EventMsg::ExecCommandEnd(ExecCommandEndEvent {
                call_id: "explore-1".into(),
                parsed_cmd: vec![ParsedCommand::Read {
                    cmd: "cat".into(),
                    name: "AGENTS.override.md".into(),
                    path: PathBuf::from("AGENTS.override.md"),
                }],
                ..base.clone()
            }),
        });
        proc.handle_codex_event(Event {
            id: "explore-2".into(),
            msg: EventMsg::ExecCommandEnd(ExecCommandEndEvent {
                call_id: "explore-2".into(),
                parsed_cmd: vec![ParsedCommand::ListFiles {
                    cmd: "ls -la".into(),
                    path: Some(".codexpotter".into()),
                }],
                ..base.clone()
            }),
        });
        proc.handle_codex_event(Event {
            id: "explore-3".into(),
            msg: EventMsg::ExecCommandEnd(ExecCommandEndEvent {
                call_id: "explore-3".into(),
                parsed_cmd: vec![ParsedCommand::ListFiles {
                    cmd: "ls -la".into(),
                    path: Some(".codexpotter".into()),
                }],
                ..base.clone()
            }),
        });
        proc.handle_codex_event(Event {
            id: "explore-4".into(),
            msg: EventMsg::ExecCommandEnd(ExecCommandEndEvent {
                call_id: "explore-4".into(),
                parsed_cmd: vec![ParsedCommand::Read {
                    cmd: "cat".into(),
                    name: "MAIN.md".into(),
                    path: PathBuf::from("MAIN.md"),
                }],
                ..base.clone()
            }),
        });
        proc.handle_codex_event(Event {
            id: "explore-5".into(),
            msg: EventMsg::ExecCommandEnd(ExecCommandEndEvent {
                call_id: "explore-5".into(),
                parsed_cmd: vec![ParsedCommand::Read {
                    cmd: "cat".into(),
                    name: "developer_prompt.md".into(),
                    path: PathBuf::from("developer_prompt.md"),
                }],
                ..base
            }),
        });

        // Any non-exec output should flush the buffered exploring cell.
        proc.handle_codex_event(Event {
            id: "agent-message".into(),
            msg: EventMsg::AgentMessage(AgentMessageEvent {
                message: "ok".into(),
                phase: None,
            }),
        });

        let events = drain_history_cell_strings(&mut rx, 80);
        let [explored, _separator, _agent_message] = events.as_slice() else {
            panic!("expected explored cell, separator, then agent message");
        };
        let rendered = explored.join("\n") + "\n";
        assert_snapshot!("round_renderer_coalesces_explored_cells", rendered);
    }

    #[tokio::test]
    async fn round_renderer_minimal_hides_explored_cells() {
        let (mut proc, mut rx) = make_round_renderer_processor("test prompt");
        let _ = drain_history_cell_strings(&mut rx, 80);

        let base = ExecCommandEndEvent {
            call_id: "unused".into(),
            process_id: None,
            turn_id: "turn-1".into(),
            command: vec!["bash".into(), "-lc".into(), "true".into()],
            cwd: PathBuf::from("project"),
            parsed_cmd: Vec::new(),
            source: ExecCommandSource::Agent,
            interaction_input: None,
            stdout: String::new(),
            stderr: String::new(),
            aggregated_output: String::new(),
            exit_code: 0,
            duration: std::time::Duration::from_millis(1),
            formatted_output: String::new(),
        };

        proc.handle_codex_event(Event {
            id: "explore-1".into(),
            msg: EventMsg::ExecCommandEnd(ExecCommandEndEvent {
                call_id: "explore-1".into(),
                parsed_cmd: vec![ParsedCommand::Read {
                    cmd: "cat".into(),
                    name: "AGENTS.override.md".into(),
                    path: PathBuf::from("AGENTS.override.md"),
                }],
                ..base
            }),
        });

        proc.handle_codex_event(Event {
            id: "agent-message".into(),
            msg: EventMsg::AgentMessage(AgentMessageEvent {
                message: "ok".into(),
                phase: None,
            }),
        });

        let events = drain_history_cell_strings(&mut rx, 80);
        let [separator, agent_message] = events.as_slice() else {
            panic!("expected separator, then agent message");
        };
        assert!(
            separator
                .first()
                .is_some_and(|line| line.chars().all(|ch| ch == '─')),
            "expected a final message separator"
        );
        pretty_assertions::assert_eq!(agent_message, &vec!["• ok".to_string()]);
    }

    #[tokio::test]
    async fn round_renderer_flushes_explored_cells_on_turn_complete() {
        let (mut proc, mut rx) = make_round_renderer_processor("test prompt");
        let _ = drain_history_cell_strings(&mut rx, u16::MAX);
        proc.verbosity = Verbosity::Simple;

        proc.handle_codex_event(Event {
            id: "explore-1".into(),
            msg: EventMsg::ExecCommandEnd(ExecCommandEndEvent {
                call_id: "explore-1".into(),
                process_id: None,
                turn_id: "turn-1".into(),
                command: vec!["bash".into(), "-lc".into(), "true".into()],
                cwd: PathBuf::from("project"),
                parsed_cmd: vec![ParsedCommand::Read {
                    cmd: "cat".into(),
                    name: "AGENTS.override.md".into(),
                    path: PathBuf::from("AGENTS.override.md"),
                }],
                source: ExecCommandSource::Agent,
                interaction_input: None,
                stdout: String::new(),
                stderr: String::new(),
                aggregated_output: String::new(),
                exit_code: 0,
                duration: std::time::Duration::from_millis(1),
                formatted_output: String::new(),
            }),
        });
        proc.handle_codex_event(Event {
            id: "turn-complete".into(),
            msg: EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: "turn-1".to_string(),
                last_agent_message: None,
            }),
        });

        let events = drain_history_cell_strings(&mut rx, u16::MAX);
        let [explored] = events.as_slice() else {
            panic!("expected exactly one explored cell");
        };
        let rendered = explored.join("\n") + "\n";
        assert_snapshot!(
            "round_renderer_flushes_explored_cells_on_turn_complete",
            rendered
        );
    }

    #[test]
    fn round_renderer_potter_project_started_emits_user_prompt() {
        let (mut proc, mut rx) = make_round_renderer_processor_without_prompt();

        proc.handle_codex_event(Event {
            id: "potter-project-started".into(),
            msg: EventMsg::PotterProjectStarted {
                user_message: Some("test prompt".to_string()),
                working_dir: PathBuf::from("/workdir"),
                project_dir: PathBuf::from(".codexpotter/projects/2026/01/29/11"),
                user_prompt_file: PathBuf::from(".codexpotter/projects/2026/01/29/11/MAIN.md"),
            },
        });

        let events = drain_history_cell_strings(&mut rx, u16::MAX);
        let [prompt, project_hint] = events.as_slice() else {
            panic!("expected prompt cell followed by project hint cell");
        };

        let prompt_rendered = prompt.join("\n") + "\n";
        assert_snapshot!("round_renderer_potter_project_started", prompt_rendered);

        let hint_rendered = project_hint.join("\n") + "\n";
        assert_snapshot!(
            "round_renderer_potter_project_started_project_hint",
            hint_rendered
        );
    }

    #[test]
    fn round_renderer_potter_round_started_emits_iteration_marker() {
        let (mut proc, mut rx) = make_round_renderer_processor_without_prompt();

        proc.handle_codex_event(Event {
            id: "potter-round-started".into(),
            msg: EventMsg::PotterRoundStarted {
                current: 1,
                total: 15,
            },
        });

        let events = drain_history_cell_strings(&mut rx, u16::MAX);
        let [marker] = events.as_slice() else {
            panic!("expected exactly one marker cell; got: {events:?}");
        };
        let rendered = marker.join("\n") + "\n";
        assert_snapshot!("round_renderer_potter_round_started", rendered);
    }

    #[test]
    fn prompt_selection_view_hides_composer_and_prompt_footer_vt100() {
        let width: u16 = 80;

        let (tx_raw, _rx_app) = unbounded_channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);

        let mut bottom_pane = BottomPane::new(BottomPaneParams {
            frame_requester: crate::tui::FrameRequester::test_dummy(),
            enhanced_keys_supported: false,
            app_event_tx,
            animations_enabled: false,
            placeholder_text: "Assign new task to CodexPotter".to_string(),
            disable_paste_burst: false,
        });
        bottom_pane.set_prompt_footer_context(PromptFooterContext::new(
            PathBuf::from("project"),
            Some("main".to_string()),
        ));

        bottom_pane
            .composer_mut()
            .show_selection_view(crate::bottom_pane::SelectionViewParams {
                title: Some("Select Syntax Theme".to_string()),
                subtitle: Some("Move up/down to live preview themes".to_string()),
                footer_hint: Some(crate::bottom_pane::popup_consts::standard_popup_hint_line()),
                items: vec![
                    crate::bottom_pane::SelectionItem {
                        name: "base16".to_string(),
                        is_current: true,
                        dismiss_on_select: true,
                        ..Default::default()
                    },
                    crate::bottom_pane::SelectionItem {
                        name: "catppuccin-latte".to_string(),
                        dismiss_on_select: true,
                        ..Default::default()
                    },
                ],
                is_searchable: true,
                search_placeholder: Some("Type to filter themes...".to_string()),
                ..Default::default()
            });

        let height = bottom_pane.desired_height(width).max(1);
        let backend = VT100Backend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).expect("create terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                ratatui::widgets::Clear.render(area, frame.buffer_mut());
                render_runner_viewport(area, frame.buffer_mut(), &bottom_pane, Vec::new());
            })
            .expect("draw");

        assert_snapshot!(
            "prompt_selection_view_hides_composer_and_prompt_footer_vt100",
            terminal.backend().vt100().screen().contents()
        );
    }

    fn render_prompt_footer_line(override_mode: Option<PromptFooterOverride>) -> String {
        let area = Rect::new(0, 0, 80, 1);
        let mut buf = ratatui::buffer::Buffer::empty(area);
        crate::bottom_pane::render_prompt_footer_for_test(
            area,
            &mut buf,
            override_mode,
            std::path::Path::new("project"),
            Some("main"),
        );

        let mut out = String::new();
        for x in 0..area.width {
            out.push_str(buf[(x, 0)].symbol());
        }
        out.trim_end().to_string()
    }

    #[test]
    fn prompt_footer_includes_external_editor_hint() {
        assert_snapshot!(
            "prompt_footer_includes_external_editor",
            render_prompt_footer_line(None)
        );
    }

    #[test]
    fn prompt_footer_external_editor_override_replaces_footer() {
        assert_snapshot!(
            "prompt_footer_external_editor_override",
            render_prompt_footer_line(Some(PromptFooterOverride::ExternalEditorHint))
        );
    }
}
