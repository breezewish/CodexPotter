//! Builds the `/verbosity` picker dialog for the TUI.

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use codex_protocol::protocol::ExecCommandSource;
use codex_protocol::protocol::FileChange;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;

use crate::StartupSetupStep;
use crate::app_event::AppEvent;
use crate::bottom_pane::SelectionItem;
use crate::bottom_pane::SelectionViewParams;
use crate::bottom_pane::SideContentWidth;
use crate::bottom_pane::popup_consts::standard_popup_hint_line;
use crate::exec_cell::CommandOutput;
use crate::exec_cell::ExecCell;
use crate::history_cell;
use crate::render::renderable::ColumnRenderable;
use crate::render::renderable::Renderable;
use crate::verbosity::Verbosity;

/// Minimum side-panel width for side-by-side verbosity preview.
const WIDE_PREVIEW_MIN_WIDTH: u16 = 40;

/// Left inset used for wide preview content.
const WIDE_PREVIEW_LEFT_INSET: u16 = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StackedPreviewMode {
    Hidden,
    FullWithLabel,
}

struct VerbosityPreviewWideRenderable {
    selected: Arc<Mutex<Verbosity>>,
}

struct VerbosityPreviewStackedFullRenderable {
    selected: Arc<Mutex<Verbosity>>,
}

fn dim_lines(lines: &mut [Line<'static>]) {
    for line in lines.iter_mut() {
        line.style = line.style.add_modifier(Modifier::DIM);
        for span in line.spans.iter_mut() {
            span.style = span.style.add_modifier(Modifier::DIM);
        }
    }
}

fn preview_commentary_cell(verbosity: Verbosity) -> history_cell::AgentMessageCell {
    let mut lines: Vec<Line<'static>> = Vec::new();
    crate::markdown::append_markdown(
        "I'll first align the rollout and rendering pipeline, then implement `/verbosity`.",
        None,
        &mut lines,
    );
    if verbosity == Verbosity::Minimal {
        dim_lines(&mut lines);
    }
    history_cell::AgentMessageCell::new(lines, true)
}

fn preview_final_answer_cell() -> history_cell::AgentMessageCell {
    let mut lines: Vec<Line<'static>> = Vec::new();
    crate::markdown::append_markdown("Final answer text stays fully visible.", None, &mut lines);
    history_cell::AgentMessageCell::new(lines, true)
}

fn preview_patch_changes() -> HashMap<PathBuf, FileChange> {
    let mut changes = HashMap::new();
    changes.insert(
        PathBuf::from("protocol/src/models.rs"),
        FileChange::Update {
            unified_diff: [
                "--- a/protocol/src/models.rs",
                "+++ b/protocol/src/models.rs",
                "@@ -3,1 +3,2 @@",
                "-use serde::Serialize;",
                "+use serde::Deserialize;",
                "+use serde::Serialize;",
                "",
            ]
            .join("\n"),
            move_path: None,
        },
    );
    changes.insert(
        PathBuf::from("protocol/src/protocol.rs"),
        FileChange::Update {
            unified_diff: [
                "--- a/protocol/src/protocol.rs",
                "+++ b/protocol/src/protocol.rs",
                "@@ -10,1 +10,2 @@",
                " pub struct Foo;",
                "+pub struct Bar;",
                "",
            ]
            .join("\n"),
            move_path: None,
        },
    );
    changes
}

fn preview_ran_cell() -> ExecCell {
    let call_id = String::from("preview-ran");
    let command = vec![
        "rg".to_string(),
        "-n".to_string(),
        "verbosity".to_string(),
        "-S".to_string(),
        "tui/src".to_string(),
    ];
    let mut cell = crate::exec_cell::new_active_exec_command(
        call_id.clone(),
        command,
        Vec::new(),
        ExecCommandSource::Agent,
        None,
        false,
    );
    cell.complete_call(
        &call_id,
        CommandOutput {
            exit_code: 0,
            aggregated_output: String::new(),
            formatted_output: String::new(),
        },
        Duration::from_millis(120),
    );
    cell
}

fn append_preview_cell(
    out: &mut Vec<Line<'static>>,
    cell: &dyn history_cell::HistoryCell,
    width: u16,
    with_gap: bool,
) {
    let mut lines = cell.display_lines(width);
    if lines.is_empty() {
        return;
    }
    if with_gap && !out.is_empty() {
        out.push(Line::from(""));
    }
    out.append(&mut lines);
}

fn build_full_preview_lines(verbosity: Verbosity, width: u16) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut out: Vec<Line<'static>> = Vec::new();

    let commentary = preview_commentary_cell(verbosity);
    append_preview_cell(&mut out, &commentary, width, true);

    if verbosity == Verbosity::Simple {
        let ran = preview_ran_cell();
        append_preview_cell(&mut out, &ran, width, true);
    }

    let changes = preview_patch_changes();
    let cwd = Path::new(".");
    let patch = history_cell::new_patch_event(changes, cwd, verbosity);
    append_preview_cell(&mut out, &patch, width, true);

    let final_answer = preview_final_answer_cell();
    append_preview_cell(&mut out, &final_answer, width, true);

    out
}

fn preview_required_height(width: u16) -> u16 {
    let minimal = build_full_preview_lines(Verbosity::Minimal, width);
    let simple = build_full_preview_lines(Verbosity::Simple, width);
    u16::try_from(minimal.len().max(simple.len())).unwrap_or(u16::MAX)
}

fn render_preview(area: Rect, buf: &mut Buffer, verbosity: Verbosity, left_inset: u16) {
    if area.is_empty() {
        return;
    }

    let left_pad = left_inset.min(area.width.saturating_sub(1));
    let render_area = Rect::new(
        area.x.saturating_add(left_pad),
        area.y,
        area.width.saturating_sub(left_pad),
        area.height,
    );

    let lines = build_full_preview_lines(verbosity, render_area.width);
    if lines.is_empty() {
        return;
    }

    Paragraph::new(ratatui::text::Text::from(lines)).render(render_area, buf);
}

impl Renderable for VerbosityPreviewWideRenderable {
    fn desired_height(&self, width: u16) -> u16 {
        let effective_width = width.saturating_sub(WIDE_PREVIEW_LEFT_INSET).max(1);
        preview_required_height(effective_width)
    }

    fn render(&self, area: Rect, buf: &mut Buffer) {
        let verbosity = match self.selected.lock() {
            Ok(guard) => *guard,
            Err(poisoned) => *poisoned.into_inner(),
        };
        render_preview(area, buf, verbosity, WIDE_PREVIEW_LEFT_INSET);
    }
}

impl Renderable for VerbosityPreviewStackedFullRenderable {
    fn desired_height(&self, width: u16) -> u16 {
        if width == 0 {
            return 0;
        }
        preview_required_height(width).saturating_add(1)
    }

    fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.is_empty() {
            return;
        }

        let label_area = Rect::new(area.x, area.y, area.width, 1.min(area.height));
        Line::from("Preview".dim().italic()).render(label_area, buf);
        if area.height <= 1 {
            return;
        }

        let preview_area = Rect::new(
            area.x,
            area.y.saturating_add(1),
            area.width,
            area.height - 1,
        );
        let verbosity = match self.selected.lock() {
            Ok(guard) => *guard,
            Err(poisoned) => *poisoned.into_inner(),
        };
        render_preview(preview_area, buf, verbosity, 0);
    }
}

fn startup_picker_header(setup_step: Option<StartupSetupStep>) -> Box<dyn Renderable> {
    let Some(step) = setup_step.filter(|step| step.should_render()) else {
        return Box::new(());
    };

    let mut column = ColumnRenderable::new();
    column.push(Line::from(step.label()).dim());
    column.push("");
    Box::new(column)
}

fn build_verbosity_picker_params_impl(
    current: Option<Verbosity>,
    initial: Verbosity,
    header: Box<dyn Renderable>,
    footer_note: Option<Line<'static>>,
    include_actions: bool,
    stacked_preview_mode: StackedPreviewMode,
) -> SelectionViewParams {
    let selected_mode = Arc::new(Mutex::new(initial));
    let selected_for_preview = selected_mode.clone();

    let modes = [Verbosity::Minimal, Verbosity::Simple];
    let items: Vec<SelectionItem> = modes
        .iter()
        .copied()
        .map(|verbosity| {
            let verbosity_for_action = verbosity;
            let is_current = current.is_some_and(|cur| cur == verbosity);
            let actions = if include_actions {
                vec![
                    Box::new(move |tx: &crate::app_event_sender::AppEventSender| {
                        tx.send(AppEvent::VerbositySelected {
                            verbosity: verbosity_for_action,
                        });
                    }) as _,
                ]
            } else {
                Vec::new()
            };

            SelectionItem {
                name: verbosity.label().to_string(),
                description: Some(verbosity.description().to_string()),
                is_current,
                is_default: verbosity == Verbosity::default(),
                dismiss_on_select: true,
                actions,
                ..Default::default()
            }
        })
        .collect();

    let initial_selected_idx = Some(match initial {
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
        footer_note,
        footer_hint: Some(standard_popup_hint_line()),
        items,
        header,
        initial_selected_idx,
        side_content: Box::new(VerbosityPreviewWideRenderable {
            selected: selected_mode.clone(),
        }),
        side_content_width: SideContentWidth::Half,
        side_content_min_width: WIDE_PREVIEW_MIN_WIDTH,
        fit_popup_height_to_side_content: true,
        stacked_side_content: Some(match stacked_preview_mode {
            StackedPreviewMode::Hidden => Box::new(()),
            StackedPreviewMode::FullWithLabel => Box::new(VerbosityPreviewStackedFullRenderable {
                selected: selected_mode,
            }),
        }),
        preserve_side_content_bg: true,
        on_selection_changed,
        ..Default::default()
    }
}

/// Builds [`SelectionViewParams`] for the `/verbosity` picker dialog.
pub fn build_verbosity_picker_params(current: Verbosity) -> SelectionViewParams {
    build_verbosity_picker_params_impl(
        Some(current),
        current,
        Box::new(()),
        None,
        true,
        StackedPreviewMode::Hidden,
    )
}

/// Builds [`SelectionViewParams`] for the startup verbosity onboarding prompt.
///
/// This prompt is a codex-potter-specific divergence from upstream Codex TUI.
pub fn build_startup_verbosity_picker_params(
    setup_step: Option<StartupSetupStep>,
) -> SelectionViewParams {
    build_verbosity_picker_params_impl(
        None,
        Verbosity::Minimal,
        startup_picker_header(setup_step),
        Some(Line::from("You can change this later via /verbosity.").dim()),
        false,
        StackedPreviewMode::FullWithLabel,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use insta::assert_snapshot;
    use pretty_assertions::assert_eq;
    use ratatui::style::Color;
    use unicode_width::UnicodeWidthStr;

    fn render_buffer(renderable: &dyn Renderable, width: u16, height: u16) -> Buffer {
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        renderable.render(area, &mut buf);
        buf
    }

    fn render_lines(renderable: &dyn Renderable, width: u16, height: u16) -> Vec<String> {
        let buf = render_buffer(renderable, width, height);
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
    fn verbosity_picker_hides_stacked_preview_when_narrow() {
        let params = build_verbosity_picker_params(Verbosity::default());
        assert_eq!(params.side_content_width, SideContentWidth::Half);
        assert_eq!(params.side_content_min_width, WIDE_PREVIEW_MIN_WIDTH);
        assert!(params.fit_popup_height_to_side_content);
        assert!(params.stacked_side_content.is_some());
        assert!(params.preserve_side_content_bg);

        let stacked_height = params
            .stacked_side_content
            .as_deref()
            .expect("stacked side content")
            .desired_height(80);
        assert_eq!(stacked_height, 0);
    }

    #[test]
    fn verbosity_picker_wide_preview_snapshot_minimal() {
        let selected = Arc::new(Mutex::new(Verbosity::Minimal));
        let renderable = VerbosityPreviewWideRenderable { selected };
        let width: u16 = 72;
        let height = renderable.desired_height(width);
        let lines = render_lines(&renderable, width, height).join("\n");
        assert_snapshot!("verbosity_picker_wide_preview_minimal", lines);
    }

    #[test]
    fn wide_preview_height_matches_the_tallest_mode_for_the_width() {
        let selected = Arc::new(Mutex::new(Verbosity::Minimal));
        let renderable = VerbosityPreviewWideRenderable { selected };
        let width: u16 = 72;

        let expected =
            preview_required_height(width.saturating_sub(WIDE_PREVIEW_LEFT_INSET).max(1));
        assert_eq!(renderable.desired_height(width), expected);
    }

    #[test]
    fn wide_preview_includes_added_and_removed_color_spans() {
        let selected = Arc::new(Mutex::new(Verbosity::Simple));
        let renderable = VerbosityPreviewWideRenderable { selected };
        let width: u16 = 80;
        let height = renderable.desired_height(width);
        let buf = render_buffer(&renderable, width, height);

        let mut saw_green = false;
        let mut saw_red = false;
        for y in 0..height {
            for x in 0..width {
                let cell = &buf[(x, y)];
                if cell.style().fg == Some(Color::Green) {
                    saw_green = true;
                }
                if cell.style().fg == Some(Color::Red) {
                    saw_red = true;
                }
            }
        }

        assert!(saw_green, "expected +added spans to render in green");
        assert!(saw_red, "expected -removed spans to render in red");
    }
}
