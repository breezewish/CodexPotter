//! Startup verbosity onboarding prompt.
//!
//! # Divergence from upstream Codex TUI
//!
//! `codex-potter` prompts the user to pick a default verbosity level on startup when no
//! `[tui].verbosity` is configured yet. Upstream Codex TUI does not show this prompt. See
//! `tui/AGENTS.md`.

use crossterm::event::KeyCode;
use crossterm::event::KeyEventKind;
use crossterm::event::KeyModifiers;
use ratatui::prelude::Widget as _;
use tokio_stream::StreamExt;

use crate::StartupSetupStep;
use crate::bottom_pane::ListSelectionView;
use crate::render::renderable::Renderable;
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
    let params = crate::verbosity_picker::build_startup_verbosity_picker_params(setup_step);

    let (app_event_tx, _app_event_rx) = tokio::sync::mpsc::unbounded_channel();
    let app_event_tx = crate::app_event_sender::AppEventSender::new(app_event_tx);
    let mut view = ListSelectionView::new(params, app_event_tx);

    let render_view = |tui: &mut Tui, view: &ListSelectionView| -> anyhow::Result<()> {
        let width = tui.terminal.last_known_screen_size.width.max(1);
        let height = view.desired_height(width).saturating_add(1);
        tui.draw(height, |frame| {
            let area = frame.area();
            ratatui::widgets::Clear.render(area, frame.buffer_mut());
            let view_area = ratatui::layout::Rect::new(
                area.x,
                area.y.saturating_add(1),
                area.width,
                area.height.saturating_sub(1),
            );
            view.render(view_area, frame.buffer_mut());
        })?;
        Ok(())
    };

    render_view(tui, &view)?;

    let events = tui.event_stream();
    tokio::pin!(events);

    while !view.is_complete() {
        let Some(event) = events.next().await else {
            break;
        };
        match event {
            TuiEvent::Key(key_event) => {
                if key_event.kind == KeyEventKind::Release {
                    continue;
                }
                if key_event.modifiers.contains(KeyModifiers::CONTROL)
                    && matches!(key_event.code, KeyCode::Char('c') | KeyCode::Char('d'))
                    && key_event.kind == KeyEventKind::Press
                {
                    view.cancel();
                } else {
                    view.handle_key_event(key_event);
                }
                tui.frame_requester().schedule_frame();
            }
            TuiEvent::Paste(_) => {}
            TuiEvent::Draw => {
                render_view(tui, &view)?;
            }
        }
    }

    // Keep behavior consistent with other prompts: clear before returning so the next screen
    // starts cleanly.
    tui.terminal.clear()?;

    let modes = [Verbosity::Minimal, Verbosity::Simple];
    Ok(view
        .take_last_selected_index()
        .and_then(|idx| modes.get(idx).copied()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_event_sender::AppEventSender;
    use crate::render::renderable::Renderable;
    use crate::test_backend::VT100Backend;
    use insta::assert_snapshot;
    use ratatui::Terminal;
    use tokio::sync::mpsc::unbounded_channel;

    #[test]
    fn startup_verbosity_prompt_initial_vt100() {
        let width: u16 = 100;

        let params = crate::verbosity_picker::build_startup_verbosity_picker_params(Some(
            StartupSetupStep::new(2, 2),
        ));

        let (app_event_tx, _app_event_rx) = unbounded_channel();
        let app_event_tx = AppEventSender::new(app_event_tx);
        let view = ListSelectionView::new(params, app_event_tx);

        let height = view.desired_height(width).saturating_add(1);
        let backend = VT100Backend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("create terminal");

        terminal
            .draw(|frame| {
                let area = frame.area();
                ratatui::widgets::Clear.render(area, frame.buffer_mut());
                let view_area = ratatui::layout::Rect::new(
                    area.x,
                    area.y.saturating_add(1),
                    area.width,
                    area.height.saturating_sub(1),
                );
                view.render(view_area, frame.buffer_mut());
            })
            .expect("draw");

        assert_snapshot!(
            "startup_verbosity_prompt_initial_vt100",
            terminal.backend().vt100().screen().contents()
        );
    }

    #[test]
    fn startup_verbosity_prompt_narrow_vt100() {
        let width: u16 = 80;

        let params = crate::verbosity_picker::build_startup_verbosity_picker_params(Some(
            StartupSetupStep::new(2, 2),
        ));

        let (app_event_tx, _app_event_rx) = unbounded_channel();
        let app_event_tx = AppEventSender::new(app_event_tx);
        let view = ListSelectionView::new(params, app_event_tx);

        let height = view.desired_height(width).saturating_add(1);
        let backend = VT100Backend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("create terminal");

        terminal
            .draw(|frame| {
                let area = frame.area();
                ratatui::widgets::Clear.render(area, frame.buffer_mut());
                let view_area = ratatui::layout::Rect::new(
                    area.x,
                    area.y.saturating_add(1),
                    area.width,
                    area.height.saturating_sub(1),
                );
                view.render(view_area, frame.buffer_mut());
            })
            .expect("draw");

        assert_snapshot!(
            "startup_verbosity_prompt_narrow_vt100",
            terminal.backend().vt100().screen().contents()
        );
    }
}
