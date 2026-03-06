//! Startup verbosity onboarding prompt.
//!
//! # Divergence from upstream Codex TUI
//!
//! `codex-potter` prompts the user to pick a default verbosity level on startup when no
//! `[tui].verbosity` is configured yet. Upstream Codex TUI does not show this prompt. See
//! `tui/AGENTS.md`.

use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyEventKind;
use crossterm::event::KeyModifiers;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::prelude::Widget as _;
use ratatui::style::Stylize as _;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Clear;
use ratatui::widgets::WidgetRef;
use tokio_stream::StreamExt;

use crate::StartupSetupStep;
use crate::render::Insets;
use crate::render::renderable::ColumnRenderable;
use crate::render::renderable::Renderable;
use crate::render::renderable::RenderableExt as _;
use crate::selection_list::selection_option_row;
use crate::tui::FrameRequester;
use crate::tui::Tui;
use crate::tui::TuiEvent;
use crate::verbosity::Verbosity;

/// Prompt the user to pick a verbosity level for interim transcript items.
///
/// Returns:
/// - `Ok(Some(verbosity))` when the user selected an option
/// - `Ok(None)` when the prompt was cancelled (Esc / Ctrl+C)
///
/// When `setup_step` is provided, the prompt may render a `Setup X/Y` marker so users understand
/// how many onboarding prompts remain.
pub async fn run_startup_verbosity_prompt_with_tui(
    tui: &mut Tui,
    setup_step: Option<StartupSetupStep>,
) -> anyhow::Result<Option<Verbosity>> {
    let mut screen = VerbosityPromptScreen::new(tui.frame_requester(), setup_step);
    tui.draw(u16::MAX, |frame| {
        frame.render_widget_ref(&screen, frame.area());
    })?;

    let events = tui.event_stream();
    tokio::pin!(events);

    while !screen.is_done() {
        let Some(event) = events.next().await else {
            break;
        };
        match event {
            TuiEvent::Key(key_event) => screen.handle_key(key_event),
            TuiEvent::Paste(_) => {}
            TuiEvent::Draw => {
                tui.draw(u16::MAX, |frame| {
                    frame.render_widget_ref(&screen, frame.area());
                })?;
            }
        }
    }

    // Keep behavior consistent with other prompts: clear before returning so the next screen
    // starts cleanly.
    tui.terminal.clear()?;

    Ok(match screen.outcome() {
        Some(VerbosityPromptOutcome::Selected(selection)) => Some(selection.verbosity()),
        Some(VerbosityPromptOutcome::Cancelled) | None => None,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum VerbositySelection {
    Minimal,
    Simple,
}

impl VerbositySelection {
    fn verbosity(self) -> Verbosity {
        match self {
            VerbositySelection::Minimal => Verbosity::Minimal,
            VerbositySelection::Simple => Verbosity::Simple,
        }
    }

    fn next(self) -> Self {
        match self {
            VerbositySelection::Minimal => VerbositySelection::Simple,
            VerbositySelection::Simple => VerbositySelection::Minimal,
        }
    }

    fn prev(self) -> Self {
        match self {
            VerbositySelection::Minimal => VerbositySelection::Simple,
            VerbositySelection::Simple => VerbositySelection::Minimal,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum VerbosityPromptOutcome {
    Selected(VerbositySelection),
    Cancelled,
}

struct VerbosityPromptScreen {
    request_frame: FrameRequester,
    setup_step: Option<StartupSetupStep>,
    highlighted: VerbositySelection,
    outcome: Option<VerbosityPromptOutcome>,
}

impl VerbosityPromptScreen {
    fn new(request_frame: FrameRequester, setup_step: Option<StartupSetupStep>) -> Self {
        Self {
            request_frame,
            setup_step,
            highlighted: VerbositySelection::Minimal,
            outcome: None,
        }
    }

    fn handle_key(&mut self, key_event: KeyEvent) {
        if key_event.kind == KeyEventKind::Release {
            return;
        }
        if key_event.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key_event.code, KeyCode::Char('c') | KeyCode::Char('d'))
        {
            self.cancel();
            return;
        }

        match key_event.code {
            KeyCode::Up | KeyCode::Char('k') => self.set_highlight(self.highlighted.prev()),
            KeyCode::Down | KeyCode::Char('j') => self.set_highlight(self.highlighted.next()),
            KeyCode::Char('1') => self.select(VerbositySelection::Minimal),
            KeyCode::Char('2') => self.select(VerbositySelection::Simple),
            KeyCode::Enter => self.select(self.highlighted),
            KeyCode::Esc => self.cancel(),
            _ => {}
        }
    }

    fn set_highlight(&mut self, highlight: VerbositySelection) {
        if self.highlighted != highlight {
            self.highlighted = highlight;
            self.request_frame.schedule_frame();
        }
    }

    fn select(&mut self, selection: VerbositySelection) {
        self.highlighted = selection;
        self.outcome = Some(VerbosityPromptOutcome::Selected(selection));
        self.request_frame.schedule_frame();
    }

    fn cancel(&mut self) {
        self.outcome = Some(VerbosityPromptOutcome::Cancelled);
        self.request_frame.schedule_frame();
    }

    fn is_done(&self) -> bool {
        self.outcome.is_some()
    }

    fn outcome(&self) -> Option<VerbosityPromptOutcome> {
        self.outcome
    }
}

impl WidgetRef for &VerbosityPromptScreen {
    fn render_ref(&self, area: Rect, buf: &mut Buffer) {
        Clear.render(area, buf);

        let mut column = ColumnRenderable::new();
        if let Some(step) = self.setup_step.filter(|step| step.should_render()) {
            column.push(
                Line::from(step.label())
                    .dim()
                    .inset(Insets::tlbr(0, 2, 0, 0)),
            );
        } else {
            column.push("");
        }

        column.push(
            Line::from("Select a verbosity mode for interim transcript items:")
                .inset(Insets::tlbr(0, 2, 0, 0)),
        );
        column.push("");
        column.push(
            Line::from("You can change this later via /verbosity.")
                .dim()
                .inset(Insets::tlbr(0, 2, 0, 0)),
        );
        column.push(
            Line::from("Minimal is the default and keeps output compact.")
                .dim()
                .inset(Insets::tlbr(0, 2, 0, 0)),
        );
        column.push("");

        column.push(selection_option_row(
            0,
            format!("Minimal (default) — {}", Verbosity::Minimal.description()),
            self.highlighted == VerbositySelection::Minimal,
        ));
        column.push(selection_option_row(
            1,
            format!("Simple — {}", Verbosity::Simple.description()),
            self.highlighted == VerbositySelection::Simple,
        ));
        column.push("");
        column.push(
            Line::from(vec![
                Span::from("Press ").dim(),
                crate::key_hint::plain(KeyCode::Enter).into(),
                Span::from(" to continue").dim(),
            ])
            .inset(Insets::tlbr(0, 2, 0, 0)),
        );

        column.render(area, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_backend::VT100Backend;
    use insta::assert_snapshot;
    use ratatui::Terminal;

    #[test]
    fn startup_verbosity_prompt_initial_vt100() {
        let backend = VT100Backend::new(80, 14);
        let mut terminal = Terminal::new(backend).expect("create terminal");

        let screen = VerbosityPromptScreen::new(
            FrameRequester::test_dummy(),
            Some(StartupSetupStep::new(2, 2)),
        );

        terminal
            .draw(|frame| {
                WidgetRef::render_ref(&&screen, frame.area(), frame.buffer_mut());
            })
            .expect("draw");

        assert_snapshot!(
            "startup_verbosity_prompt_initial_vt100",
            terminal.backend().vt100().screen().contents()
        );
    }
}
