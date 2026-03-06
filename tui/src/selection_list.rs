//! Shared selection-list rendering helpers.
//!
//! This module provides reusable building blocks for rendering numbered selection options in
//! startup/onboarding prompts.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::prelude::Widget as _;
use ratatui::style::Style;
use ratatui::style::Stylize as _;
use ratatui::text::Line;
use ratatui::text::Text;
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use crate::render::Insets;
use crate::render::renderable::Renderable;
use crate::render::renderable::RenderableExt as _;
use crate::wrapping::RtOptions;
use crate::wrapping::word_wrap_lines;

/// Render a numbered selection option row with a consistent inset and wrapping behavior.
pub fn selection_option_row(
    index: usize,
    label: String,
    selected: bool,
) -> crate::render::renderable::RenderableItem<'static> {
    SelectionOptionRow::new(index, label, selected).inset(Insets::tlbr(0, 2, 0, 0))
}

struct SelectionOptionRow {
    prefix: String,
    label: String,
    style: Style,
}

impl SelectionOptionRow {
    fn new(index: usize, label: String, selected: bool) -> Self {
        let number = index + 1;
        let prefix = if selected {
            format!("› {number}. ")
        } else {
            format!("  {number}. ")
        };
        let style = if selected {
            Style::default().cyan()
        } else {
            Style::default()
        };
        Self {
            prefix,
            label,
            style,
        }
    }

    fn wrapped_lines(&self, width: u16) -> Vec<Line<'static>> {
        if width == 0 {
            return Vec::new();
        }

        let prefix_width = UnicodeWidthStr::width(self.prefix.as_str());
        let subsequent_indent = " ".repeat(prefix_width);
        let opts = RtOptions::new(width as usize)
            .initial_indent(Line::from(self.prefix.clone()))
            .subsequent_indent(Line::from(subsequent_indent))
            .wrap_algorithm(textwrap::WrapAlgorithm::FirstFit);

        let label = Line::from(self.label.clone()).style(self.style);
        word_wrap_lines([label], opts)
    }
}

impl Renderable for SelectionOptionRow {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        Paragraph::new(Text::from(self.wrapped_lines(area.width))).render(area, buf);
    }

    fn desired_height(&self, width: u16) -> u16 {
        self.wrapped_lines(width).len() as u16
    }
}
