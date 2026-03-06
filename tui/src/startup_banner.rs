//! Startup banner rendering.
//!
//! # Divergence from upstream Codex TUI
//!
//! `codex-potter` renders a customized startup banner (ASCII art + directory/model line) to match
//! the codex-potter CLI experience. See `tui/AGENTS.md`.

use std::path::Path;

use ratatui::prelude::*;
use ratatui::text::Line;
use ratatui::text::Span;
use unicode_width::UnicodeWidthChar;
use unicode_width::UnicodeWidthStr;

use crate::text_formatting::format_directory_for_display;
use crate::ui_colors::orange_color;
use crate::ui_colors::secondary_color;

const POTTER_ASCII_ART: &[&str] = &[
    "                 __                                 __    __                   ",
    "                /\\ \\                               /\\ \\__/\\ \\__                ",
    "  ___    ___    \\_\\ \\     __   __  _  _____     ___\\ \\ ,_\\ \\ ,_\\    __   _ __  ",
    " /'___\\ / __`\\  /'_` \\  /'__`\\/\\ \\/'\\/\\ '__`\\  / __`\\ \\ \\/\\ \\ \\/  /'__`\\/\\`'__\\",
    "/\\ \\__//\\ \\L\\ \\/\\ \\L\\ \\/\\  __/\\/>  </\\ \\ \\L\\ \\/\\ \\L\\ \\ \\ \\_\\ \\ \\_/\\  __/\\ \\ \\/ ",
    "\\ \\____\\ \\____/\\ \\___,_\\ \\____\\/\\_/\\_\\\\ \\ ,__/\\ \\____/\\ \\__\\\\ \\__\\ \\____\\\\ \\_\\ ",
    " \\/____/\\/___/  \\/__,_ /\\/____/\\//\\/_/ \\ \\ \\/  \\/___/  \\/__/ \\/__/\\/____/ \\/_/ ",
    "                                        \\ \\_\\                                  ",
    "                                         \\/_/                                  ",
];

const ASCII_INDENT: &str = "  ";
const FAST_LABEL_SUFFIX: &str = "[fast]";
const FAST_LABEL_WITH_SEPARATOR: &str = " [fast]";
// Bold split positions (0-based, within each ASCII art line after trimming trailing spaces).
const ASCII_BOLD_SPLIT_COLS: [usize; 9] = [52, 51, 38, 37, 37, 38, 38, 40, 41];

fn take_prefix_by_width(text: &str, max_width: usize) -> &str {
    if max_width == 0 {
        return "";
    }
    if UnicodeWidthStr::width(text) <= max_width {
        return text;
    }

    let mut used_width = 0usize;
    let mut end = 0usize;
    for (idx, ch) in text.char_indices() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used_width + ch_width > max_width {
            break;
        }
        used_width += ch_width;
        end = idx + ch.len_utf8();
    }
    &text[..end]
}

fn truncate_model_label_for_banner(text: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(text) <= max_width {
        return text.to_string();
    }

    if let Some(base) = text.strip_suffix(FAST_LABEL_WITH_SEPARATOR)
        && UnicodeWidthStr::width(FAST_LABEL_SUFFIX) <= max_width
    {
        let available_base_width =
            max_width.saturating_sub(UnicodeWidthStr::width(FAST_LABEL_WITH_SEPARATOR));
        let truncated_base = take_prefix_by_width(base, available_base_width);
        if truncated_base.is_empty() {
            return FAST_LABEL_SUFFIX.to_string();
        }
        return format!("{truncated_base}{FAST_LABEL_WITH_SEPARATOR}");
    }

    take_prefix_by_width(text, max_width).to_string()
}

/// Build the startup banner as plain `Line`s, sized to fit within `width`.
pub fn build_startup_banner_lines(
    width: u16,
    version: &str,
    model_label: &str,
    directory: &Path,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let indent_width = UnicodeWidthStr::width(ASCII_INDENT);
    let max_width = usize::from(width);
    let banner_width = POTTER_ASCII_ART
        .iter()
        .map(|line| indent_width + UnicodeWidthStr::width(line.trim_end()))
        .max()
        .unwrap_or(indent_width)
        .min(max_width);

    for (idx, line) in POTTER_ASCII_ART.iter().enumerate() {
        let trimmed = line.trim_end();
        let split_at = ASCII_BOLD_SPLIT_COLS[idx].min(trimmed.len());

        let version_label = if idx == POTTER_ASCII_ART.len().saturating_sub(1) {
            let label = format!("v{version}");
            Some(take_prefix_by_width(label.as_str(), banner_width).to_string())
        } else {
            None
        };
        let version_width = version_label
            .as_deref()
            .map(UnicodeWidthStr::width)
            .unwrap_or(0);
        let gap_reserve = if version_width > 0 && banner_width > version_width {
            1
        } else {
            0
        };
        let max_prefix_width = banner_width.saturating_sub(version_width + gap_reserve);
        let indent_visible_width = indent_width.min(max_prefix_width);
        let remaining = max_prefix_width.saturating_sub(indent_visible_width);
        let visible = take_prefix_by_width(trimmed, remaining);

        let visible_split_at = split_at.min(visible.len());
        let (left, right) = visible.split_at(visible_split_at);

        let dim_style = Style::default().dim();
        let bold_secondary_style = Style::default().fg(secondary_color()).bold();
        let mut spans: Vec<Span<'static>> = vec![
            Span::styled(ASCII_INDENT[..indent_visible_width].to_string(), dim_style),
            Span::styled(left.to_string(), dim_style),
            Span::styled(right.to_string(), bold_secondary_style),
        ];

        if let Some(version_label) = version_label {
            let prefix_width = indent_visible_width + UnicodeWidthStr::width(visible);
            let gap = banner_width.saturating_sub(prefix_width + version_width);
            spans.push(Span::from(" ".repeat(gap)));
            spans.push(Span::from(version_label).dim());
        }

        lines.push(Line::from(spans));
    }

    lines.push(Line::from(""));

    let dir_label = "directory: ";
    let dir_prefix_width = UnicodeWidthStr::width(ASCII_INDENT) + UnicodeWidthStr::width(dir_label);
    let available_tail_width = usize::from(width).saturating_sub(dir_prefix_width);
    let min_dir_width = usize::from(available_tail_width > 0);

    let mut model_display = model_label.to_string();
    let mut model_gap_width = if model_label.is_empty() { 0 } else { 2 };
    if model_gap_width + UnicodeWidthStr::width(model_display.as_str()) + min_dir_width
        > available_tail_width
    {
        let max_model_width = available_tail_width.saturating_sub(model_gap_width + min_dir_width);
        model_display = truncate_model_label_for_banner(model_label, max_model_width);
        if model_display.is_empty() {
            model_gap_width = 0;
        }
    }

    let model_label_width = UnicodeWidthStr::width(model_display.as_str());
    let dir_max_width = available_tail_width.saturating_sub(model_gap_width + model_label_width);
    let dir_display = format_directory_for_display(directory, Some(dir_max_width));

    let mut directory_spans: Vec<Span<'static>> = vec![
        Span::from(ASCII_INDENT),
        Span::from(dir_label).dim(),
        Span::from(dir_display),
    ];
    if !model_display.is_empty() {
        directory_spans.push(Span::from("  "));
        directory_spans.push(Span::styled(
            model_display,
            Style::default().fg(orange_color()).bold(),
        ));
    }
    lines.push(Line::from(directory_spans));
    lines.push(Line::from(""));

    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use insta::assert_snapshot;

    fn to_plain_text(lines: &[Line<'static>]) -> String {
        let mut out = String::new();
        for (idx, line) in lines.iter().enumerate() {
            if idx > 0 {
                out.push('\n');
            }
            for span in &line.spans {
                out.push_str(span.content.as_ref());
            }
        }
        out.push('\n');
        out
    }

    #[test]
    fn startup_banner_snapshot() {
        let dir = Path::new("/Users/example/repo");
        let lines = build_startup_banner_lines(120, "0.0.1", "gpt-5.2 xhigh", dir);
        assert_snapshot!("startup_banner_snapshot", to_plain_text(&lines));
    }

    #[test]
    fn startup_banner_fast_snapshot() {
        let dir = Path::new("/Users/example/repo");
        let lines = build_startup_banner_lines(120, "0.0.1", "gpt-5.2 xhigh [fast]", dir);
        assert_snapshot!("startup_banner_fast_snapshot", to_plain_text(&lines));
    }

    #[test]
    fn startup_banner_truncates_ascii_art_without_wrapping_and_keeps_version_visible() {
        let dir = Path::new("/Users/example/repo");
        let width: u16 = 30;
        let lines = build_startup_banner_lines(width, "0.0.1", "", dir);

        for (idx, line) in lines.iter().enumerate() {
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            assert!(
                UnicodeWidthStr::width(text.as_str()) <= usize::from(width),
                "line {idx} must fit within {width} cols: {text:?}",
            );
        }

        let version_line = &lines[POTTER_ASCII_ART.len() - 1];
        let text: String = version_line
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(
            text.ends_with("v0.0.1"),
            "version line must end with version label: {text:?}",
        );
    }

    #[test]
    fn startup_banner_truncates_directory_line_and_keeps_fast_suffix_visible() {
        let dir = Path::new("/Users/example/repo");
        let width: u16 = 30;
        let lines = build_startup_banner_lines(width, "0.0.1", "gpt-5.2 xhigh [fast]", dir);

        let directory_line = &lines[POTTER_ASCII_ART.len() + 1];
        let text: String = directory_line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(
            UnicodeWidthStr::width(text.as_str()) <= usize::from(width),
            "directory line must fit within {width} cols: {text:?}",
        );
        assert!(
            text.ends_with("gpt-5.2 [fast]"),
            "expected fast suffix to remain visible after truncation: {text:?}",
        );
    }
}
