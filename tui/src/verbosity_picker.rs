//! Builds the `/verbosity` picker dialog for the TUI.

use std::sync::Arc;
use std::sync::Mutex;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;

use crate::app_event::AppEvent;
use crate::bottom_pane::SelectionItem;
use crate::bottom_pane::SelectionViewParams;
use crate::bottom_pane::SideContentWidth;
use crate::bottom_pane::popup_consts::standard_popup_hint_line;
use crate::render::renderable::Renderable;
use crate::verbosity::Verbosity;

/// Minimum side-panel width for side-by-side verbosity preview.
const WIDE_PREVIEW_MIN_WIDTH: u16 = 40;

/// Left inset used for wide preview content.
const WIDE_PREVIEW_LEFT_INSET: u16 = 2;

/// Minimum frame padding used for vertically centered wide preview.
const PREVIEW_FRAME_PADDING: u16 = 1;

struct VerbosityPreviewWideRenderable {
    selected: Arc<Mutex<Verbosity>>,
}

struct VerbosityPreviewNarrowRenderable {
    selected: Arc<Mutex<Verbosity>>,
}

fn centered_offset(available: u16, content: u16, min_frame: u16) -> u16 {
    let free = available.saturating_sub(content);
    let frame = if free >= min_frame.saturating_mul(2) {
        min_frame
    } else {
        0
    };
    frame + free.saturating_sub(frame.saturating_mul(2)) / 2
}

fn preview_lines(verbosity: Verbosity, compact: bool) -> Vec<Line<'static>> {
    match (verbosity, compact) {
        (Verbosity::Minimal, false) => vec![
            Line::from("我先对上 rollout 与渲染链路，再实现 /verbosity。".dim()),
            Line::from(""),
            vec!["• ".dim(), "Edited".bold(), " 2 files (+22 -0)".dim()].into(),
            vec!["  └ ".dim(), "protocol/src/models.rs (+19 -0)".dim()].into(),
            vec!["    ".into(), "protocol/src/protocol.rs (+3 -0)".dim()].into(),
            Line::from(""),
            Line::from("Ran/Explored are hidden in Minimal mode.".dim().italic()),
            Line::from(""),
            Line::from("最终结论文本保持正常显示。"),
        ],
        (Verbosity::Minimal, true) => vec![
            Line::from("commentary is dimmed".dim().italic()),
            vec!["• ".dim(), "Edited".bold(), " 2 files (+22 -0)".dim()].into(),
            vec!["  └ ".dim(), "protocol/src/models.rs (+19 -0)".dim()].into(),
            vec!["    ".into(), "protocol/src/protocol.rs (+3 -0)".dim()].into(),
        ],
        (Verbosity::Simple, false) => vec![
            Line::from("我先对上 rollout 与渲染链路，再实现 /verbosity。"),
            Line::from(""),
            vec!["• ".dim(), "Explored".bold()].into(),
            vec!["  └ ".dim(), "tui/src/app_server_render.rs".dim()].into(),
            Line::from(""),
            vec!["• ".dim(), "Ran".bold()].into(),
            vec!["  └ ".dim(), "rg -n \"verbosity\" -S tui/src".dim()].into(),
            Line::from(""),
            vec!["• ".dim(), "Edited".bold(), " 2 files (+22 -0)".dim()].into(),
            vec!["  └ ".dim(), "protocol/src/models.rs (+19 -0)".dim()].into(),
            vec!["     ".into(), "5".dim()].into(),
            vec!["     ".into(), "6 +use serde::Deserialize;".dim()].into(),
        ],
        (Verbosity::Simple, true) => vec![
            Line::from("commentary is normal".dim().italic()),
            vec!["• ".dim(), "Ran".bold()].into(),
            vec!["  └ ".dim(), "rg -n \"verbosity\" -S tui/src".dim()].into(),
            vec!["• ".dim(), "Edited".bold(), " 2 files (+22 -0)".dim()].into(),
        ],
    }
}

fn render_preview(
    area: Rect,
    buf: &mut Buffer,
    verbosity: Verbosity,
    compact: bool,
    left_inset: u16,
) {
    if area.is_empty() {
        return;
    }

    let mut lines = preview_lines(verbosity, compact);
    if lines.is_empty() {
        return;
    }

    let content_height = (lines.len() as u16).min(area.height);
    let top_pad = if compact {
        0
    } else {
        centered_offset(area.height, content_height, PREVIEW_FRAME_PADDING)
    };

    let left_pad = left_inset.min(area.width.saturating_sub(1));
    let render_area = Rect::new(
        area.x.saturating_add(left_pad),
        area.y,
        area.width.saturating_sub(left_pad),
        area.height,
    );

    if top_pad > 0 {
        let mut padded = Vec::with_capacity(lines.len() + top_pad as usize);
        padded.extend(std::iter::repeat_n(Line::from(""), top_pad as usize));
        padded.append(&mut lines);
        lines = padded;
    }

    Paragraph::new(ratatui::text::Text::from(lines)).render(render_area, buf);
}

impl Renderable for VerbosityPreviewWideRenderable {
    fn desired_height(&self, _width: u16) -> u16 {
        u16::MAX
    }

    fn render(&self, area: Rect, buf: &mut Buffer) {
        let verbosity = match self.selected.lock() {
            Ok(guard) => *guard,
            Err(poisoned) => *poisoned.into_inner(),
        };
        render_preview(area, buf, verbosity, false, WIDE_PREVIEW_LEFT_INSET);
    }
}

impl Renderable for VerbosityPreviewNarrowRenderable {
    fn desired_height(&self, _width: u16) -> u16 {
        4
    }

    fn render(&self, area: Rect, buf: &mut Buffer) {
        let verbosity = match self.selected.lock() {
            Ok(guard) => *guard,
            Err(poisoned) => *poisoned.into_inner(),
        };
        render_preview(area, buf, verbosity, true, 0);
    }
}

/// Builds [`SelectionViewParams`] for the `/verbosity` picker dialog.
pub fn build_verbosity_picker_params(current: Verbosity) -> SelectionViewParams {
    let selected_mode = Arc::new(Mutex::new(current));
    let selected_for_preview = selected_mode.clone();

    let modes = [Verbosity::Minimal, Verbosity::Simple];
    let items: Vec<SelectionItem> = modes
        .iter()
        .copied()
        .map(|verbosity| {
            let is_current = verbosity == current;
            let verbosity_for_action = verbosity;
            SelectionItem {
                name: verbosity.label().to_string(),
                description: Some(verbosity.description().to_string()),
                is_current,
                is_default: verbosity == Verbosity::default(),
                dismiss_on_select: true,
                actions: vec![Box::new(
                    move |tx: &crate::app_event_sender::AppEventSender| {
                        tx.send(AppEvent::VerbositySelected {
                            verbosity: verbosity_for_action,
                        });
                    },
                )],
                ..Default::default()
            }
        })
        .collect();

    let initial_selected_idx = Some(match current {
        Verbosity::Minimal => 0,
        Verbosity::Simple => 1,
    });

    let on_selection_changed = Some(Box::new(move |idx: usize, _tx: &_| {
        let Some(verbosity) = modes.get(idx).copied() else {
            return;
        };
        let mut guard = match selected_for_preview.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *guard = verbosity;
    })
        as Box<dyn Fn(usize, &crate::app_event_sender::AppEventSender) + Send + Sync>);

    SelectionViewParams {
        title: Some("Select Verbosity".to_string()),
        subtitle: Some("Choose how interim transcript items are shown".to_string()),
        footer_hint: Some(standard_popup_hint_line()),
        items,
        initial_selected_idx,
        side_content: Box::new(VerbosityPreviewWideRenderable {
            selected: selected_mode.clone(),
        }),
        side_content_width: SideContentWidth::Half,
        side_content_min_width: WIDE_PREVIEW_MIN_WIDTH,
        stacked_side_content: Some(Box::new(VerbosityPreviewNarrowRenderable {
            selected: selected_mode,
        })),
        on_selection_changed,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use insta::assert_snapshot;
    use pretty_assertions::assert_eq;
    use unicode_width::UnicodeWidthStr;

    fn render_lines(renderable: &dyn Renderable, width: u16, height: u16) -> Vec<String> {
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        renderable.render(area, &mut buf);
        (0..height)
            .map(|row| {
                let mut line = String::new();
                let mut col = 0u16;
                while col < width {
                    let symbol = buf[(col, row)].symbol();
                    if symbol.is_empty() {
                        line.push(' ');
                        col = col.saturating_add(1);
                        continue;
                    }
                    line.push_str(symbol);
                    let symbol_width = UnicodeWidthStr::width(symbol);
                    let advance = u16::try_from(symbol_width).unwrap_or(1).max(1);
                    col = col.saturating_add(advance);
                }
                line
            })
            .collect()
    }

    #[test]
    fn verbosity_picker_uses_half_width_with_stacked_fallback_preview() {
        let params = build_verbosity_picker_params(Verbosity::default());
        assert_eq!(params.side_content_width, SideContentWidth::Half);
        assert_eq!(params.side_content_min_width, WIDE_PREVIEW_MIN_WIDTH);
        assert!(params.stacked_side_content.is_some());
    }

    #[test]
    fn verbosity_picker_wide_preview_snapshot_minimal() {
        let selected = Arc::new(Mutex::new(Verbosity::Minimal));
        let renderable = VerbosityPreviewWideRenderable { selected };
        let lines = render_lines(&renderable, 64, 12).join("\n");
        assert_snapshot!("verbosity_picker_wide_preview_minimal", lines);
    }

    #[test]
    fn verbosity_picker_narrow_preview_snapshot_simple() {
        let selected = Arc::new(Mutex::new(Verbosity::Simple));
        let renderable = VerbosityPreviewNarrowRenderable { selected };
        let lines = render_lines(&renderable, 64, 4).join("\n");
        assert_snapshot!("verbosity_picker_narrow_preview_simple", lines);
    }
}
