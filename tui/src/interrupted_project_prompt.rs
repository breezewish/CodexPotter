use std::path::PathBuf;

use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyEventKind;
use crossterm::event::KeyModifiers;
use ratatui::prelude::Widget;
use ratatui::text::Line;
use tokio_stream::StreamExt;

use crate::bottom_pane::ListSelectionView;
use crate::bottom_pane::SelectionItem;
use crate::bottom_pane::SelectionViewParams;
use crate::render::renderable::Renderable;
use crate::tui::Tui;
use crate::tui::TuiEvent;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterruptedProjectAction {
    StopIterate,
    ContinueIterate,
}

fn handle_prompt_key_event(view: &mut ListSelectionView, key_event: KeyEvent) {
    if key_event.kind == KeyEventKind::Release {
        return;
    }

    // This prompt is entered by pressing Esc to interrupt a running task. Users frequently press
    // Esc multiple times (or hold it briefly), which can accidentally dismiss the picker and end
    // the project immediately. Swallow Esc here so only an explicit selection (Enter/number) or
    // Ctrl+C stops the iteration.
    if key_event.modifiers == KeyModifiers::NONE && matches!(key_event.code, KeyCode::Esc) {
        return;
    }

    if key_event.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key_event.code, KeyCode::Char('c'))
    {
        if key_event.kind == KeyEventKind::Press {
            view.cancel();
        }
        return;
    }

    view.handle_key_event(key_event);
}

pub async fn prompt_interrupted_project_action(
    tui: &mut Tui,
    progress_file_rel: PathBuf,
) -> anyhow::Result<Option<InterruptedProjectAction>> {
    let items = vec![
        SelectionItem {
            name: "Stop iterate this project".to_string(),
            dismiss_on_select: true,
            ..Default::default()
        },
        SelectionItem {
            name: "I made some changes, continue iterate".to_string(),
            dismiss_on_select: true,
            ..Default::default()
        },
    ];

    let (app_event_tx, _app_event_rx) = tokio::sync::mpsc::unbounded_channel();
    let app_event_tx = crate::app_event_sender::AppEventSender::new(app_event_tx);
    let mut view = ListSelectionView::new(
        SelectionViewParams {
            title: Some("Current project is interrupted".to_string()),
            subtitle: Some(progress_file_rel.to_string_lossy().to_string()),
            footer_note: Some(Line::from(
                "Want to change the goal? Just edit this project file.",
            )),
            footer_hint: Some(Line::from(
                "Press enter to confirm, Ctrl+C to stop iterating.",
            )),
            items,
            ..Default::default()
        },
        app_event_tx,
    );

    let width = tui.terminal.last_known_screen_size.width.max(1);
    tui.draw(view.desired_height(width).saturating_add(1), |frame| {
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

    let events = tui.event_stream();
    tokio::pin!(events);

    while !view.is_complete() {
        let Some(event) = events.next().await else {
            break;
        };
        match event {
            TuiEvent::Key(key_event) => {
                handle_prompt_key_event(&mut view, key_event);
                tui.frame_requester().schedule_frame();
            }
            TuiEvent::Paste(_) => {}
            TuiEvent::Draw => {
                let width = tui.terminal.last_known_screen_size.width.max(1);
                tui.draw(view.desired_height(width).saturating_add(1), |frame| {
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
            }
        }
    }

    // Clear the inline viewport so subsequent screens start clean.
    tui.terminal.clear()?;

    let Some(idx) = view.take_last_selected_index() else {
        return Ok(None);
    };

    match idx {
        0 => Ok(Some(InterruptedProjectAction::StopIterate)),
        1 => Ok(Some(InterruptedProjectAction::ContinueIterate)),
        _ => anyhow::bail!("internal error: unexpected interrupted project selection {idx}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[test]
    fn interrupted_project_prompt_esc_does_not_cancel() {
        let view = ListSelectionView::new(
            SelectionViewParams {
                title: Some("Current project is interrupted".to_string()),
                items: vec![
                    SelectionItem {
                        name: "Stop iterate this project".to_string(),
                        dismiss_on_select: true,
                        ..Default::default()
                    },
                    SelectionItem {
                        name: "I made some changes, continue iterate".to_string(),
                        dismiss_on_select: true,
                        ..Default::default()
                    },
                ],
                ..Default::default()
            },
            crate::app_event_sender::AppEventSender::new(tokio::sync::mpsc::unbounded_channel().0),
        );

        let mut view = view;
        assert!(!view.is_complete(), "expected prompt to start incomplete");

        handle_prompt_key_event(&mut view, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(
            !view.is_complete(),
            "expected Esc not to cancel the interrupted-project prompt"
        );

        handle_prompt_key_event(
            &mut view,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        );
        assert!(
            view.is_complete(),
            "expected Ctrl+C to cancel the interrupted-project prompt"
        );
    }

    #[test]
    fn interrupted_project_prompt_renders_with_subtitle_and_footer() {
        let progress_file_rel = PathBuf::from(".codexpotter/projects/2026/03/06/4/MAIN.md");

        let items = vec![
            SelectionItem {
                name: "Stop iterate this project".to_string(),
                dismiss_on_select: true,
                ..Default::default()
            },
            SelectionItem {
                name: "I made some changes, continue iterate".to_string(),
                dismiss_on_select: true,
                ..Default::default()
            },
        ];

        let view = ListSelectionView::new(
            SelectionViewParams {
                title: Some("Current project is interrupted".to_string()),
                subtitle: Some(progress_file_rel.to_string_lossy().to_string()),
                footer_note: Some(Line::from(
                    "Want to change the goal? Just edit this project file.",
                )),
                footer_hint: Some(Line::from(
                    "Press enter to confirm, Ctrl+C to stop iterating.",
                )),
                items,
                ..Default::default()
            },
            crate::app_event_sender::AppEventSender::new(tokio::sync::mpsc::unbounded_channel().0),
        );

        let width = 64;
        let height = view.desired_height(width).saturating_add(1);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
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

        insta::assert_snapshot!(terminal.backend());
    }
}
