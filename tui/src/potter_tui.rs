use codex_protocol::protocol::Event;
use codex_protocol::protocol::Op;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::mpsc::UnboundedSender;

use crate::AppExitInfo;
use crate::bottom_pane::PromptFooterContext;
use crate::history_cell::HistoryCell;
use crate::tui;
use crate::tui::Tui;
use crate::verbosity::Verbosity;

/// Parameters for [`CodexPotterTui::render_round`].
pub struct RenderRoundParams {
    pub prompt: String,
    pub pad_before_first_cell: bool,
    /// Optional status header prefix shown while a task is running (e.g. `Round 2/10`).
    pub status_header_prefix: Option<String>,
    pub prompt_footer: PromptFooterContext,
    pub codex_op_tx: UnboundedSender<Op>,
    pub codex_event_rx: UnboundedReceiver<Event>,
    pub fatal_exit_rx: UnboundedReceiver<String>,
}

/// `codex-potter`-specific TUI wrapper:
/// - Reuses the legacy composer to collect the initial prompt
/// - Reuses the legacy rendering pipeline to render each round
/// - Attempts to restore terminal state on Drop
pub struct CodexPotterTui {
    tui: Tui,
    has_rendered_round: bool,
    project_started_at: Option<Instant>,
    queued_user_prompts: VecDeque<String>,
    composer_draft: Option<crate::bottom_pane::ChatComposerDraft>,
    check_for_update_on_startup: bool,
    startup_warnings: Vec<String>,
    verbosity: Verbosity,
    needs_startup_verbosity_prompt: bool,
}

impl CodexPotterTui {
    /// Initialize the TUI (enter raw mode) and clear the screen.
    pub fn new() -> anyhow::Result<Self> {
        let mut terminal = tui::init()?;
        terminal.clear()?;
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let codex_home = crate::codex_config::find_codex_home()?;
        let theme = crate::codex_config::resolve_codex_tui_theme(&cwd)?;
        let mut startup_warnings = Vec::new();
        if let Some(warning) = crate::render::highlight::set_theme_override(theme, Some(codex_home))
        {
            startup_warnings.push(warning);
        }
        let (verbosity, needs_startup_verbosity_prompt) =
            match crate::potter_config::load_potter_tui_verbosity() {
                Ok(Some(verbosity)) => (verbosity, false),
                Ok(None) => (Verbosity::default(), true),
                Err(err) => {
                    startup_warnings.push(format!("Failed to load TUI verbosity: {err}"));
                    (Verbosity::default(), false)
                }
            };
        Ok(Self {
            tui: Tui::new(terminal),
            has_rendered_round: false,
            project_started_at: None,
            queued_user_prompts: VecDeque::new(),
            composer_draft: None,
            check_for_update_on_startup: true,
            startup_warnings,
            verbosity,
            needs_startup_verbosity_prompt,
        })
    }

    fn reset_event_stream_after_prompt(&mut self) {
        self.tui.reset_event_stream();
    }

    /// Enable/disable update checks and update prompts on startup.
    ///
    /// When disabled, CodexPotter will not check for updates and will suppress the update prompt
    /// and update-available banner.
    pub fn set_check_for_update_on_startup(&mut self, enabled: bool) {
        self.check_for_update_on_startup = enabled;
    }

    /// Show the "update available" modal, if applicable.
    ///
    /// Returns `Some(action)` when the user chooses "Update now", so the caller can run the
    /// update command after restoring terminal state.
    pub async fn prompt_update_if_needed(&mut self) -> anyhow::Result<Option<crate::UpdateAction>> {
        if !self.check_for_update_on_startup {
            return Ok(None);
        }

        let result = crate::update_prompt::run_update_prompt_if_needed(&mut self.tui).await?;

        // Drop and recreate the underlying crossterm EventStream so any buffered input from the
        // prompt can't leak into the next screen (e.g. the global gitignore prompt / composer).
        self.reset_event_stream_after_prompt();

        Ok(match result {
            crate::update_prompt::UpdatePromptOutcome::Continue => None,
            crate::update_prompt::UpdatePromptOutcome::RunUpdate(action) => Some(action),
        })
    }

    /// Show the global gitignore recommendation prompt using the existing terminal session.
    ///
    /// This avoids tearing down and re-initializing the terminal between prompts, which can race
    /// with crossterm's stdin reader and break subsequent cursor-position queries.
    ///
    /// When `setup_step` is provided, the prompt may render a `Setup X/Y` marker so users
    /// understand how many onboarding prompts remain.
    pub async fn prompt_global_gitignore(
        &mut self,
        global_gitignore_path_display: String,
        setup_step: Option<crate::StartupSetupStep>,
    ) -> anyhow::Result<crate::GlobalGitignorePromptOutcome> {
        let result = crate::global_gitignore_prompt::run_global_gitignore_prompt_with_tui(
            &mut self.tui,
            global_gitignore_path_display,
            setup_step,
        )
        .await;

        // Drop and recreate the underlying crossterm EventStream so any buffered input from the
        // prompt can't leak into the next screen (e.g. the composer).
        self.reset_event_stream_after_prompt();

        result
    }

    /// Returns `true` when startup should prompt the user to pick a default verbosity level.
    ///
    /// This is used for first-run onboarding when no persisted `[tui].verbosity` is configured
    /// yet.
    pub fn should_prompt_startup_verbosity(&self) -> bool {
        self.needs_startup_verbosity_prompt
    }

    /// Prompt the user to select a default verbosity level and persist it to disk.
    ///
    /// The prompt is intended to be shown on startup before entering the main composer UI.
    ///
    /// When `setup_step` is provided, the prompt may render a `Setup X/Y` marker so users
    /// understand how many onboarding prompts remain.
    ///
    /// Cancelled prompts (Esc / Ctrl+C) leave the verbosity unchanged and do not persist.
    pub async fn prompt_startup_verbosity(
        &mut self,
        setup_step: Option<crate::StartupSetupStep>,
    ) -> anyhow::Result<()> {
        let result = crate::verbosity_prompt::run_startup_verbosity_prompt_with_tui(
            &mut self.tui,
            setup_step,
        )
        .await;

        // Drop and recreate the underlying crossterm EventStream so any buffered input from the
        // prompt can't leak into the next screen (e.g. the composer).
        self.reset_event_stream_after_prompt();

        let Some(verbosity) = result? else {
            return Ok(());
        };

        self.verbosity = verbosity;
        match crate::potter_config::persist_potter_tui_verbosity(verbosity) {
            Ok(()) => self.needs_startup_verbosity_prompt = false,
            Err(err) => self
                .startup_warnings
                .push(format!("Failed to persist TUI verbosity: {err}")),
        }

        Ok(())
    }

    /// Collect the user's initial prompt via the legacy composer.
    ///
    /// Returns:
    /// - `Ok(Some(prompt))`: submitted
    /// - `Ok(None)`: cancelled (Ctrl+C)
    pub async fn prompt_user(
        &mut self,
        prompt_footer: PromptFooterContext,
    ) -> anyhow::Result<Option<String>> {
        let show_startup_banner = !self.has_rendered_round;
        let composer_draft = self.composer_draft.take();
        let startup_warnings = std::mem::take(&mut self.startup_warnings);
        crate::app_server_render::prompt_user_with_tui(
            &mut self.tui,
            show_startup_banner,
            self.check_for_update_on_startup,
            startup_warnings,
            composer_draft,
            &mut self.verbosity,
            prompt_footer,
        )
        .await
    }

    /// Set the start time for the current CodexPotter project.
    ///
    /// This is used by the round renderer to display a total elapsed timer next to the round
    /// prefix (e.g. `Round 3/10 (4m 13s) · ...`).
    pub fn set_project_started_at(&mut self, started_at: Instant) {
        self.project_started_at = Some(started_at);
    }

    /// Prompt the user to select an action from a list.
    ///
    /// Returns:
    /// - `Ok(Some(index))`: selected the action at `index`
    /// - `Ok(None)`: cancelled (Esc/Ctrl+C)
    pub async fn prompt_action_picker(
        &mut self,
        actions: Vec<String>,
    ) -> anyhow::Result<Option<usize>> {
        let result =
            crate::action_picker_prompt::prompt_action_picker(&mut self.tui, actions).await;

        self.reset_event_stream_after_prompt();

        result
    }

    /// Prompt the user for how to resolve an interrupted CodexPotter project.
    ///
    /// Returns `None` when the prompt is cancelled (Esc/Ctrl+C).
    pub async fn prompt_interrupted_project_action(
        &mut self,
        progress_file_rel: PathBuf,
    ) -> anyhow::Result<Option<crate::InterruptedProjectAction>> {
        let result = crate::interrupted_project_prompt::prompt_interrupted_project_action(
            &mut self.tui,
            progress_file_rel,
        )
        .await;

        self.reset_event_stream_after_prompt();

        result
    }

    /// Insert a summary block for an interrupted CodexPotter project into the transcript.
    pub fn insert_interrupted_project_summary(
        &mut self,
        rounds: u32,
        duration: Duration,
        user_prompt_file: PathBuf,
        git_commit_start: String,
        git_commit_end: String,
    ) {
        let width = self.tui.terminal.last_known_screen_size.width.max(1);
        let mut lines = crate::history_cell_potter::new_potter_project_interrupted(
            rounds,
            duration,
            user_prompt_file,
            git_commit_start,
            git_commit_end,
        )
        .display_lines(width);

        if lines.is_empty() {
            return;
        }

        if self.has_rendered_round {
            lines.insert(0, ratatui::text::Line::from(""));
        }

        self.tui.insert_history_lines(lines);
    }

    /// Prompt the user to select a resumable CodexPotter project to resume.
    ///
    /// `Esc` returns [`crate::ResumePickerOutcome::StartFresh`] (do not exit the app).
    /// `Ctrl+C` returns [`crate::ResumePickerOutcome::Exit`].
    pub async fn prompt_resume_picker(
        &mut self,
        rows: Vec<crate::ResumePickerRow>,
    ) -> anyhow::Result<crate::ResumePickerOutcome> {
        let result =
            crate::resume_picker_prompt::run_resume_picker_prompt_with_tui(&mut self.tui, rows)
                .await;

        self.reset_event_stream_after_prompt();

        result
    }

    /// Clear current screen contents (used to remove composer remnants).
    pub fn clear(&mut self) -> anyhow::Result<()> {
        self.tui.terminal.clear()?;
        Ok(())
    }

    /// Pop the next prompt queued via the bottom composer while tasks were running.
    pub fn pop_queued_user_prompt(&mut self) -> Option<String> {
        self.queued_user_prompts.pop_front()
    }

    /// Take all prompts queued via the bottom composer while tasks were running.
    ///
    /// This is primarily intended for exit paths so the caller can surface any queued prompts
    /// before the process terminates and the in-memory queue is lost.
    pub fn take_queued_user_prompts(&mut self) -> VecDeque<String> {
        std::mem::take(&mut self.queued_user_prompts)
    }

    /// Render a single Potter round until the control plane signals the round
    /// finished (`EventMsg::PotterRoundFinished`) or the user interrupts.
    pub async fn render_round(&mut self, params: RenderRoundParams) -> anyhow::Result<AppExitInfo> {
        let RenderRoundParams {
            prompt,
            pad_before_first_cell,
            status_header_prefix,
            prompt_footer,
            codex_op_tx,
            codex_event_rx,
            fatal_exit_rx,
        } = params;
        let Some(project_started_at) = self.project_started_at else {
            anyhow::bail!(
                "internal error: CodexPotterTui::set_project_started_at must be called before render_round"
            );
        };
        let startup_warnings = std::mem::take(&mut self.startup_warnings);
        let options = crate::app_server_render::RoundRenderOptions {
            render_user_prompt: false,
            pad_before_first_cell: pad_before_first_cell || self.has_rendered_round,
            status_header_prefix,
        };
        let mut queued = std::mem::take(&mut self.queued_user_prompts);
        let mut composer_draft = self.composer_draft.take();
        let backend = crate::app_server_render::RoundBackendChannels {
            codex_op_tx,
            codex_event_rx,
            fatal_exit_rx,
        };
        let context = crate::app_server_render::ProjectRenderContext {
            project_started_at,
            prompt_footer,
        };
        let state = crate::app_server_render::RoundUiState {
            queued_user_messages: &mut queued,
            composer_draft: &mut composer_draft,
            verbosity: &mut self.verbosity,
        };
        let result = crate::app_server_render::run_round_with_tui_options_and_queue(
            &mut self.tui,
            prompt,
            options,
            context,
            backend,
            startup_warnings,
            state,
        )
        .await;
        self.queued_user_prompts = queued;
        self.composer_draft = composer_draft;
        self.has_rendered_round = true;
        result
    }
}

impl Drop for CodexPotterTui {
    fn drop(&mut self) {
        // Best-effort: clear any leftover inline UI so the user's shell prompt is clean.
        let _ = crate::terminal_cleanup::clear_inline_viewport_for_exit(&mut self.tui.terminal);

        // Always attempt to restore the terminal, even if the caller exits early.
        let _ = tui::restore();
    }
}
