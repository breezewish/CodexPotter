//! Startup verbosity onboarding prompt.
//!
//! # Divergence from upstream Codex TUI
//!
//! `codex-potter` prompts the user to pick a default verbosity level on startup when no
//! `[tui].verbosity` is configured yet. Upstream Codex TUI does not show this prompt. See
//! `tui/AGENTS.md`.

use std::sync::Arc;
use std::sync::Mutex;

use crossterm::event::KeyCode;
use crossterm::event::KeyEventKind;
use crossterm::event::KeyModifiers;
use ratatui::buffer::Buffer;
use ratatui::layout::Constraint;
use ratatui::layout::Layout;
use ratatui::layout::Rect;
use ratatui::prelude::Widget as _;
use ratatui::style::Stylize as _;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use tokio_stream::StreamExt;

use crate::StartupSetupStep;
use crate::bottom_pane::ListSelectionView;
use crate::bottom_pane::SideContentWidth;
use crate::bottom_pane::popup_content_width;
use crate::bottom_pane::side_by_side_layout_widths;
use crate::render::Insets;
use crate::render::RectExt as _;
use crate::render::renderable::Renderable;
use crate::tui::Tui;
use crate::tui::TuiEvent;
use crate::verbosity::Verbosity;
use crate::wrapping::RtOptions;
use crate::wrapping::word_wrap_line;

struct StartupVerbosityPromptView {
    view: ListSelectionView,
    selected_for_preview: Arc<Mutex<Verbosity>>,
    side_content_width: SideContentWidth,
    side_content_min_width: u16,
}

fn build_startup_prompt_view(
    app_event_tx: crate::app_event_sender::AppEventSender,
) -> StartupVerbosityPromptView {
    let mut params = crate::verbosity_picker::build_startup_verbosity_picker_params();
    params.footer_note = None;
    params.footer_hint = None;

    let selected_for_preview = Arc::new(Mutex::new(Verbosity::Minimal));
    let selected_for_preview_on_change = selected_for_preview.clone();
    let existing_on_selection_changed = params.on_selection_changed;
    params.on_selection_changed = Some(Box::new(move |idx: usize, tx: &_| {
        let modes = [Verbosity::Minimal, Verbosity::Simple];
        if let Some(verbosity) = modes.get(idx).copied() {
            let mut guard = match selected_for_preview_on_change.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            *guard = verbosity;
        }
        if let Some(cb) = &existing_on_selection_changed {
            cb(idx, tx);
        }
    }));

    StartupVerbosityPromptView {
        side_content_width: params.side_content_width,
        side_content_min_width: params.side_content_min_width,
        view: ListSelectionView::new(params, app_event_tx),
        selected_for_preview,
    }
}

fn desired_height(
    width: u16,
    view: &ListSelectionView,
    side_content_width: SideContentWidth,
    side_content_min_width: u16,
    setup_step: Option<StartupSetupStep>,
) -> u16 {
    let width = width.max(1);
    let note_line = Line::from(vec![
        "You can change this later via ".into(),
        "/verbosity".cyan(),
        ".".into(),
    ])
    .dim();
    let note_width = width.saturating_sub(2).max(1) as usize;
    let note_lines = word_wrap_line(
        &note_line,
        RtOptions::new(note_width)
            .initial_indent(Line::from(""))
            .subsequent_indent(Line::from("")),
    );

    let inner_width = popup_content_width(width);
    let is_narrow =
        side_by_side_layout_widths(inner_width, side_content_width, side_content_min_width)
            .is_none();
    let preview_width = width.saturating_sub(2).max(1);
    let preview_section_height = if is_narrow {
        crate::verbosity_picker::preview_required_height(preview_width).saturating_add(3)
    } else {
        0
    };
    let below_height = u16::try_from(1 + note_lines.len()).unwrap_or(u16::MAX);
    let below_height = below_height.saturating_add(preview_section_height);

    view.desired_height(width)
        .saturating_add(below_height)
        .saturating_add(u16::from(
            setup_step.filter(|step| step.should_render()).is_some(),
        ))
}

fn render_startup_prompt(
    area: Rect,
    buf: &mut Buffer,
    view: &ListSelectionView,
    selected_for_preview: &Arc<Mutex<Verbosity>>,
    side_content_width: SideContentWidth,
    side_content_min_width: u16,
    setup_step: Option<StartupSetupStep>,
) {
    ratatui::widgets::Clear.render(area, buf);

    let width = area.width.max(1);
    let setup_step = setup_step.filter(|step| step.should_render());
    let top_padding = u16::from(setup_step.is_some());

    if area.height == 0 {
        return;
    }

    if let Some(step) = setup_step {
        let label_area = Rect::new(area.x, area.y, area.width, 1).inset(Insets::tlbr(0, 2, 0, 0));
        if !label_area.is_empty() {
            Line::from(step.label()).dim().render(label_area, buf);
        }
    }

    if area.height <= top_padding {
        return;
    }
    let view_area = Rect::new(
        area.x,
        area.y.saturating_add(top_padding),
        area.width,
        area.height - top_padding,
    );
    if view_area.is_empty() {
        return;
    }

    let hint_line = Line::from(vec![
        "Press ".into(),
        crate::key_hint::plain(KeyCode::Enter).into(),
        " to confirm or ".into(),
        crate::key_hint::plain(KeyCode::Esc).into(),
        " to skip".into(),
    ]);
    let note_line = Line::from(vec![
        "You can change this later via ".into(),
        "/verbosity".cyan(),
        ".".into(),
    ])
    .dim();
    let note_width = width.saturating_sub(2).max(1) as usize;
    let note_lines = word_wrap_line(
        &note_line,
        RtOptions::new(note_width)
            .initial_indent(Line::from(""))
            .subsequent_indent(Line::from("")),
    );

    let inner_width = popup_content_width(width);
    let is_narrow =
        side_by_side_layout_widths(inner_width, side_content_width, side_content_min_width)
            .is_none();

    let available_height = view_area.height;
    let below_min_height = u16::try_from(1 + note_lines.len()).unwrap_or(u16::MAX);
    let selection_height = if available_height > below_min_height {
        view.desired_height(width)
            .min(available_height.saturating_sub(below_min_height))
    } else {
        available_height
    };
    let below_height = available_height.saturating_sub(selection_height);

    let [selection_area, below_area] = Layout::vertical([
        Constraint::Length(selection_height),
        Constraint::Length(below_height),
    ])
    .areas(view_area);

    view.render(selection_area, buf);

    if below_area.height == 0 || below_area.width <= 2 {
        return;
    }

    let below_x = below_area.x.saturating_add(2);
    let below_width = below_area.width.saturating_sub(2);
    let mut cursor_y = below_area.y;

    // Hint line.
    if cursor_y < below_area.bottom() {
        let line_area = Rect::new(below_x, cursor_y, below_width, 1);
        hint_line.dim().render(line_area, buf);
        cursor_y = cursor_y.saturating_add(1);
    }

    // Note lines.
    for line in &note_lines {
        if cursor_y >= below_area.bottom() {
            break;
        }
        let line_area = Rect::new(below_x, cursor_y, below_width, 1);
        line.clone().render(line_area, buf);
        cursor_y = cursor_y.saturating_add(1);
    }

    // Preview (narrow layout only; truncated to remaining height).
    if !is_narrow || cursor_y >= below_area.bottom() {
        return;
    }

    let remaining_height = below_area.bottom().saturating_sub(cursor_y);
    let preview_area = Rect::new(below_x, cursor_y, below_width, remaining_height);
    let selected = match selected_for_preview.lock() {
        Ok(guard) => *guard,
        Err(poisoned) => *poisoned.into_inner(),
    };
    let preview_width = width.saturating_sub(2).max(1);
    let mut preview_lines: Vec<Line<'static>> =
        vec!["".into(), Line::from("Preview".dim().italic()), "".into()];
    preview_lines.extend(crate::verbosity_picker::build_full_preview_lines(
        selected,
        preview_width,
    ));
    Paragraph::new(ratatui::text::Text::from(preview_lines)).render(preview_area, buf);
}

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
    let (app_event_tx, _app_event_rx) = tokio::sync::mpsc::unbounded_channel();
    let app_event_tx = crate::app_event_sender::AppEventSender::new(app_event_tx);
    let mut prompt_view = build_startup_prompt_view(app_event_tx);
    let selected_for_preview = prompt_view.selected_for_preview.clone();
    let side_content_width = prompt_view.side_content_width;
    let side_content_min_width = prompt_view.side_content_min_width;

    let render_view = |tui: &mut Tui, view: &ListSelectionView| -> anyhow::Result<()> {
        let width = tui.terminal.last_known_screen_size.width.max(1);
        let height = desired_height(
            width,
            view,
            side_content_width,
            side_content_min_width,
            setup_step,
        );
        tui.draw(height, |frame| {
            render_startup_prompt(
                frame.area(),
                frame.buffer_mut(),
                view,
                &selected_for_preview,
                side_content_width,
                side_content_min_width,
                setup_step,
            );
        })?;
        Ok(())
    };

    render_view(tui, &prompt_view.view)?;

    let events = tui.event_stream();
    tokio::pin!(events);

    while !prompt_view.view.is_complete() {
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
                    prompt_view.view.cancel();
                } else {
                    prompt_view.view.handle_key_event(key_event);
                }
                tui.frame_requester().schedule_frame();
            }
            TuiEvent::Paste(_) => {}
            TuiEvent::Draw => {
                render_view(tui, &prompt_view.view)?;
            }
        }
    }

    // Keep behavior consistent with other prompts: clear before returning so the next screen
    // starts cleanly.
    tui.terminal.clear()?;

    let modes = [Verbosity::Minimal, Verbosity::Simple];
    Ok(prompt_view
        .view
        .take_last_selected_index()
        .and_then(|idx| modes.get(idx).copied()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_event_sender::AppEventSender;
    use crate::test_backend::VT100Backend;
    use insta::assert_snapshot;
    use ratatui::Terminal;
    use tokio::sync::mpsc::unbounded_channel;

    #[test]
    fn startup_verbosity_prompt_initial_vt100() {
        let width: u16 = 100;
        let setup_step = Some(StartupSetupStep::new(2, 2));

        let (app_event_tx, _app_event_rx) = unbounded_channel();
        let app_event_tx = AppEventSender::new(app_event_tx);
        let prompt_view = build_startup_prompt_view(app_event_tx);

        let height = desired_height(
            width,
            &prompt_view.view,
            prompt_view.side_content_width,
            prompt_view.side_content_min_width,
            setup_step,
        );
        let backend = VT100Backend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("create terminal");

        terminal
            .draw(|frame| {
                render_startup_prompt(
                    frame.area(),
                    frame.buffer_mut(),
                    &prompt_view.view,
                    &prompt_view.selected_for_preview,
                    prompt_view.side_content_width,
                    prompt_view.side_content_min_width,
                    setup_step,
                );
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
        let setup_step = Some(StartupSetupStep::new(2, 2));

        let (app_event_tx, _app_event_rx) = unbounded_channel();
        let app_event_tx = AppEventSender::new(app_event_tx);
        let prompt_view = build_startup_prompt_view(app_event_tx);

        let height = desired_height(
            width,
            &prompt_view.view,
            prompt_view.side_content_width,
            prompt_view.side_content_min_width,
            setup_step,
        );
        let backend = VT100Backend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("create terminal");

        terminal
            .draw(|frame| {
                render_startup_prompt(
                    frame.area(),
                    frame.buffer_mut(),
                    &prompt_view.view,
                    &prompt_view.selected_for_preview,
                    prompt_view.side_content_width,
                    prompt_view.side_content_min_width,
                    setup_step,
                );
            })
            .expect("draw");

        assert_snapshot!(
            "startup_verbosity_prompt_narrow_vt100",
            terminal.backend().vt100().screen().contents()
        );
    }

    #[test]
    fn startup_verbosity_prompt_narrow_truncated_height_vt100() {
        let width: u16 = 80;
        let setup_step = Some(StartupSetupStep::new(2, 2));

        let (app_event_tx, _app_event_rx) = unbounded_channel();
        let app_event_tx = AppEventSender::new(app_event_tx);
        let prompt_view = build_startup_prompt_view(app_event_tx);

        let full_height = desired_height(
            width,
            &prompt_view.view,
            prompt_view.side_content_width,
            prompt_view.side_content_min_width,
            setup_step,
        );
        let height: u16 = 12;
        assert!(
            full_height > height,
            "expected the startup verbosity prompt to exceed the truncated height"
        );

        let backend = VT100Backend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("create terminal");

        terminal
            .draw(|frame| {
                render_startup_prompt(
                    frame.area(),
                    frame.buffer_mut(),
                    &prompt_view.view,
                    &prompt_view.selected_for_preview,
                    prompt_view.side_content_width,
                    prompt_view.side_content_min_width,
                    setup_step,
                );
            })
            .expect("draw");

        assert_snapshot!(
            "startup_verbosity_prompt_narrow_truncated_height_vt100",
            terminal.backend().vt100().screen().contents()
        );
    }
}
