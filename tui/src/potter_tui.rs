use codex_protocol::protocol::Event;
use codex_protocol::protocol::Op;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::Instant;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::mpsc::UnboundedSender;

use crate::AppExitInfo;
use crate::bottom_pane::PromptFooterContext;
use crate::tui;
use crate::tui::Tui;

/// Parameters for [`CodexPotterTui::render_turn`].
pub struct RenderTurnParams {
    pub prompt: String,
    pub pad_before_first_cell: bool,
    pub status_header_prefix: Option<String>,
    pub prompt_footer: PromptFooterContext,
    pub codex_op_tx: UnboundedSender<Op>,
    pub codex_event_rx: UnboundedReceiver<Event>,
    pub fatal_exit_rx: UnboundedReceiver<String>,
}

/// `codex-potter`-specific TUI session wrapper:
/// - Reuses the legacy composer to collect the initial prompt
/// - Reuses the single-turn runner pipeline to render each turn
/// - Attempts to restore terminal state on Drop
pub struct CodexPotterTui {
    tui: Tui,
    turns_rendered: bool,
    project_started_at: Option<Instant>,
    queued_user_prompts: VecDeque<String>,
    composer_draft: Option<crate::bottom_pane::ChatComposerDraft>,
    check_for_update_on_startup: bool,
    startup_warnings: Vec<String>,
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
        Ok(Self {
            tui: Tui::new(terminal),
            turns_rendered: false,
            project_started_at: None,
            queued_user_prompts: VecDeque::new(),
            composer_draft: None,
            check_for_update_on_startup: true,
            startup_warnings,
        })
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
        self.tui.pause_events();
        tui::flush_terminal_input_buffer();
        self.tui.resume_events();

        Ok(match result {
            crate::update_prompt::UpdatePromptOutcome::Continue => None,
            crate::update_prompt::UpdatePromptOutcome::RunUpdate(action) => Some(action),
        })
    }

    /// Show the global gitignore recommendation prompt using the existing terminal session.
    ///
    /// This avoids tearing down and re-initializing the terminal between prompts, which can race
    /// with crossterm's stdin reader and break subsequent cursor-position queries.
    pub async fn prompt_global_gitignore(
        &mut self,
        global_gitignore_path_display: String,
    ) -> anyhow::Result<crate::GlobalGitignorePromptOutcome> {
        let result = crate::global_gitignore_prompt::run_global_gitignore_prompt_with_tui(
            &mut self.tui,
            global_gitignore_path_display,
        )
        .await;

        // Drop and recreate the underlying crossterm EventStream so any buffered input from the
        // prompt can't leak into the next screen (e.g. the composer).
        self.tui.pause_events();
        tui::flush_terminal_input_buffer();
        self.tui.resume_events();

        result
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
        let show_startup_banner = !self.turns_rendered;
        let composer_draft = self.composer_draft.take();
        let startup_warnings = std::mem::take(&mut self.startup_warnings);
        crate::app_server_render::prompt_user_with_tui(
            &mut self.tui,
            show_startup_banner,
            self.check_for_update_on_startup,
            startup_warnings,
            composer_draft,
            prompt_footer,
        )
        .await
    }

    /// Set the start time for the current CodexPotter project/session.
    ///
    /// This is used by the turn renderer to display a total elapsed timer next to the round
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

        self.tui.pause_events();
        tui::flush_terminal_input_buffer();
        self.tui.resume_events();

        result
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

        self.tui.pause_events();
        tui::flush_terminal_input_buffer();
        self.tui.resume_events();

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

    /// Render a single round (render-only runner) until the control plane signals the round
    /// finished (`EventMsg::PotterRoundFinished`) or the user interrupts.
    pub async fn render_turn(&mut self, params: RenderTurnParams) -> anyhow::Result<AppExitInfo> {
        let RenderTurnParams {
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
                "internal error: CodexPotterTui::set_project_started_at must be called before render_turn"
            );
        };
        let startup_warnings = std::mem::take(&mut self.startup_warnings);
        let options = crate::app_server_render::RenderOnlyTurnOptions {
            render_user_prompt: false,
            status_header_prefix,
            pad_before_first_cell: pad_before_first_cell || self.turns_rendered,
        };
        let mut queued = std::mem::take(&mut self.queued_user_prompts);
        let mut composer_draft = self.composer_draft.take();
        let backend = crate::app_server_render::RenderOnlyBackendChannels {
            codex_op_tx,
            codex_event_rx,
            fatal_exit_rx,
        };
        let context = crate::app_server_render::RenderOnlyTurnContext {
            project_started_at,
            prompt_footer,
        };
        let state = crate::app_server_render::RenderOnlyTurnUiState {
            queued_user_messages: &mut queued,
            composer_draft: &mut composer_draft,
        };
        let result = crate::app_server_render::run_render_only_with_tui_options_and_queue(
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
        self.turns_rendered = true;
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
