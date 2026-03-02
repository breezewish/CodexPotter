use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use itertools::Itertools as _;
use ratatui::buffer::Buffer;
use ratatui::layout::Constraint;
use ratatui::layout::Layout;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;
use unicode_width::UnicodeWidthStr;

use crate::app_event_sender::AppEventSender;
use crate::key_hint::KeyBinding;
use crate::render::renderable::ColumnRenderable;
use crate::render::renderable::Renderable;

use super::popup_consts::MAX_POPUP_ROWS;
use super::scroll_state::ScrollState;
use super::selection_popup_common::GenericDisplayRow;
use super::selection_popup_common::measure_rows_height;
use super::selection_popup_common::render_menu_surface;
use super::selection_popup_common::render_rows;
use super::selection_popup_common::wrap_styled_line;

/// Minimum list width (in content columns) required before the side-by-side
/// layout is activated. Keeps the list usable even when sharing horizontal
/// space with the side content panel.
const MIN_LIST_WIDTH_FOR_SIDE: u16 = 40;

/// Horizontal gap (in columns) between the list area and the side content
/// panel when side-by-side layout is active.
const SIDE_CONTENT_GAP: u16 = 2;

/// Shared menu-surface horizontal inset (2 cells per side) used by selection popups.
const MENU_SURFACE_HORIZONTAL_INSET: u16 = 4;

/// Controls how the side content panel is sized relative to the popup width.
///
/// When the computed side width falls below `side_content_min_width` or the
/// remaining list area would be narrower than [`MIN_LIST_WIDTH_FOR_SIDE`], the
/// side-by-side layout is abandoned and the stacked fallback is used instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SideContentWidth {
    /// Fixed number of columns. `Fixed(0)` disables side content entirely.
    Fixed(u16),
    /// Exact 50/50 split of the content area (minus the inter-column gap).
    Half,
}

impl Default for SideContentWidth {
    fn default() -> Self {
        Self::Fixed(0)
    }
}

/// Returns the popup content width after subtracting the shared menu-surface
/// horizontal inset (2 columns on each side).
pub fn popup_content_width(total_width: u16) -> u16 {
    total_width.saturating_sub(MENU_SURFACE_HORIZONTAL_INSET)
}

/// Returns side-by-side layout widths as `(list_width, side_width)` when the
/// layout can fit. Returns `None` when the side panel is disabled/too narrow or
/// when the remaining list width would become unusably small.
pub fn side_by_side_layout_widths(
    content_width: u16,
    side_content_width: SideContentWidth,
    side_content_min_width: u16,
) -> Option<(u16, u16)> {
    let side_width = match side_content_width {
        SideContentWidth::Fixed(0) => return None,
        SideContentWidth::Fixed(width) => width,
        SideContentWidth::Half => content_width.saturating_sub(SIDE_CONTENT_GAP) / 2,
    };
    if side_width < side_content_min_width {
        return None;
    }
    let list_width = content_width.saturating_sub(SIDE_CONTENT_GAP + side_width);
    (list_width >= MIN_LIST_WIDTH_FOR_SIDE).then_some((list_width, side_width))
}

pub type SelectionAction = Box<dyn Fn(&AppEventSender) + Send + Sync>;
pub type OnSelectionChangedCallback = Option<Box<dyn Fn(usize, &AppEventSender) + Send + Sync>>;
pub type OnCancelCallback = Option<Box<dyn Fn(&AppEventSender) + Send + Sync>>;

#[derive(Default)]
pub struct SelectionItem {
    pub name: String,
    pub display_shortcut: Option<KeyBinding>,
    pub description: Option<String>,
    pub selected_description: Option<String>,
    pub is_current: bool,
    pub is_default: bool,
    pub is_disabled: bool,
    pub actions: Vec<SelectionAction>,
    pub dismiss_on_select: bool,
    pub search_value: Option<String>,
    pub disabled_reason: Option<String>,
}

pub struct SelectionViewParams {
    pub title: Option<String>,
    pub subtitle: Option<String>,
    pub footer_note: Option<Line<'static>>,
    pub footer_hint: Option<Line<'static>>,
    pub items: Vec<SelectionItem>,
    pub is_searchable: bool,
    pub search_placeholder: Option<String>,
    pub header: Box<dyn Renderable>,
    pub initial_selected_idx: Option<usize>,

    /// Rich content rendered beside (wide terminals) or below (narrow terminals)
    /// the list items, inside the bordered menu surface. Used by the theme picker
    /// to show a syntax-highlighted preview.
    pub side_content: Box<dyn Renderable>,

    /// Width mode for side content when side-by-side layout is active.
    pub side_content_width: SideContentWidth,

    /// Minimum side panel width required before side-by-side layout activates.
    pub side_content_min_width: u16,

    /// Optional fallback content rendered when side-by-side does not fit.
    /// When absent, `side_content` is reused.
    pub stacked_side_content: Option<Box<dyn Renderable>>,

    /// Keep side-content background colors after rendering in side-by-side mode.
    /// Disabled by default so existing popups preserve their reset-background look.
    pub preserve_side_content_bg: bool,

    /// Called when the highlighted item changes (navigation, filter, number-key).
    /// Receives the *actual* item index, not the filtered/visible index.
    pub on_selection_changed: OnSelectionChangedCallback,

    /// Called when the picker is dismissed via Esc/Ctrl+C without selecting.
    pub on_cancel: OnCancelCallback,
}

impl Default for SelectionViewParams {
    fn default() -> Self {
        Self {
            title: None,
            subtitle: None,
            footer_note: None,
            footer_hint: None,
            items: Vec::new(),
            is_searchable: false,
            search_placeholder: None,
            header: Box::new(()),
            initial_selected_idx: None,
            side_content: Box::new(()),
            side_content_width: SideContentWidth::default(),
            side_content_min_width: 0,
            stacked_side_content: None,
            preserve_side_content_bg: false,
            on_selection_changed: None,
            on_cancel: None,
        }
    }
}

pub struct ListSelectionView {
    footer_note: Option<Line<'static>>,
    footer_hint: Option<Line<'static>>,
    items: Vec<SelectionItem>,
    state: ScrollState,
    complete: bool,
    app_event_tx: AppEventSender,
    is_searchable: bool,
    search_query: String,
    search_placeholder: Option<String>,
    filtered_indices: Vec<usize>,
    last_selected_actual_idx: Option<usize>,
    header: Box<dyn Renderable>,
    initial_selected_idx: Option<usize>,
    side_content: Box<dyn Renderable>,
    side_content_width: SideContentWidth,
    side_content_min_width: u16,
    stacked_side_content: Option<Box<dyn Renderable>>,
    preserve_side_content_bg: bool,
    on_selection_changed: OnSelectionChangedCallback,
    on_cancel: OnCancelCallback,
}

impl ListSelectionView {
    pub fn new(params: SelectionViewParams, app_event_tx: AppEventSender) -> Self {
        let mut header = params.header;
        if params.title.is_some() || params.subtitle.is_some() {
            let title = params.title.map(|title| Line::from(title.bold()));
            let subtitle = params.subtitle.map(|subtitle| Line::from(subtitle.dim()));
            header = Box::new(ColumnRenderable::with([
                header,
                Box::new(title),
                Box::new(subtitle),
            ]));
        }
        let mut s = Self {
            footer_note: params.footer_note,
            footer_hint: params.footer_hint,
            items: params.items,
            state: ScrollState::new(),
            complete: false,
            app_event_tx,
            is_searchable: params.is_searchable,
            search_query: String::new(),
            search_placeholder: if params.is_searchable {
                params.search_placeholder
            } else {
                None
            },
            filtered_indices: Vec::new(),
            last_selected_actual_idx: None,
            header,
            initial_selected_idx: params.initial_selected_idx,
            side_content: params.side_content,
            side_content_width: params.side_content_width,
            side_content_min_width: params.side_content_min_width,
            stacked_side_content: params.stacked_side_content,
            preserve_side_content_bg: params.preserve_side_content_bg,
            on_selection_changed: params.on_selection_changed,
            on_cancel: params.on_cancel,
        };
        s.apply_filter();
        s
    }

    pub fn handle_key_event(&mut self, key_event: KeyEvent) {
        match key_event {
            // Some terminals (or configurations) send Control key chords as
            // C0 control characters without reporting the CONTROL modifier.
            // Handle fallbacks for Ctrl-P/N here so navigation works everywhere.
            KeyEvent {
                code: KeyCode::Up, ..
            }
            | KeyEvent {
                code: KeyCode::Char('p'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }
            | KeyEvent {
                code: KeyCode::Char('\u{0010}'),
                modifiers: KeyModifiers::NONE,
                ..
            } /* ^P */ => self.move_up(),
            KeyEvent {
                code: KeyCode::Char('k'),
                modifiers: KeyModifiers::NONE,
                ..
            } if !self.is_searchable => self.move_up(),
            KeyEvent {
                code: KeyCode::Down,
                ..
            }
            | KeyEvent {
                code: KeyCode::Char('n'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }
            | KeyEvent {
                code: KeyCode::Char('\u{000e}'),
                modifiers: KeyModifiers::NONE,
                ..
            } /* ^N */ => self.move_down(),
            KeyEvent {
                code: KeyCode::Char('j'),
                modifiers: KeyModifiers::NONE,
                ..
            } if !self.is_searchable => self.move_down(),
            KeyEvent {
                code: KeyCode::Backspace,
                ..
            } if self.is_searchable => {
                self.search_query.pop();
                self.apply_filter();
            }
            KeyEvent {
                code: KeyCode::Esc, ..
            } => self.cancel(),
            KeyEvent {
                code: KeyCode::Char('c'),
                modifiers,
                ..
            } if modifiers.contains(KeyModifiers::CONTROL) => self.cancel(),
            KeyEvent {
                code: KeyCode::Char(c),
                modifiers,
                ..
            } if self.is_searchable
                && !modifiers.contains(KeyModifiers::CONTROL)
                && !modifiers.contains(KeyModifiers::ALT) =>
            {
                self.search_query.push(c);
                self.apply_filter();
            }
            KeyEvent {
                code: KeyCode::Char(c),
                modifiers,
                ..
            } if !self.is_searchable
                && !modifiers.contains(KeyModifiers::CONTROL)
                && !modifiers.contains(KeyModifiers::ALT) =>
            {
                if let Some(idx) = c
                    .to_digit(10)
                    .map(|d| d as usize)
                    .and_then(|d| d.checked_sub(1))
                    && idx < self.items.len()
                    && self
                        .items
                        .get(idx)
                        .is_some_and(|item| item.disabled_reason.is_none() && !item.is_disabled)
                {
                    self.state.selected_idx = Some(idx);
                    self.accept();
                }
            }
            KeyEvent {
                code: KeyCode::Enter,
                modifiers: KeyModifiers::NONE,
                ..
            } => self.accept(),
            _ => {}
        }
    }

    pub fn is_complete(&self) -> bool {
        self.complete
    }

    pub fn cancel(&mut self) {
        if let Some(cb) = &self.on_cancel {
            cb(&self.app_event_tx);
        }
        self.complete = true;
    }

    pub fn take_last_selected_index(&mut self) -> Option<usize> {
        self.last_selected_actual_idx.take()
    }

    fn visible_len(&self) -> usize {
        self.filtered_indices.len()
    }

    fn max_visible_rows(len: usize) -> usize {
        MAX_POPUP_ROWS.min(len.max(1))
    }

    fn selected_actual_idx(&self) -> Option<usize> {
        self.state
            .selected_idx
            .and_then(|visible_idx| self.filtered_indices.get(visible_idx).copied())
    }

    fn apply_filter(&mut self) {
        let previously_selected = self
            .selected_actual_idx()
            .or_else(|| {
                (!self.is_searchable)
                    .then(|| self.items.iter().position(|item| item.is_current))
                    .flatten()
            })
            .or_else(|| self.initial_selected_idx.take());

        if self.is_searchable && !self.search_query.is_empty() {
            let query_lower = self.search_query.to_lowercase();
            self.filtered_indices = self
                .items
                .iter()
                .positions(|item| {
                    item.search_value
                        .as_ref()
                        .is_some_and(|v| v.to_lowercase().contains(&query_lower))
                })
                .collect();
        } else {
            self.filtered_indices = (0..self.items.len()).collect();
        }

        let len = self.filtered_indices.len();
        self.state.selected_idx = self
            .state
            .selected_idx
            .and_then(|visible_idx| {
                self.filtered_indices
                    .get(visible_idx)
                    .and_then(|idx| self.filtered_indices.iter().position(|cur| cur == idx))
            })
            .or_else(|| {
                previously_selected.and_then(|actual_idx| {
                    self.filtered_indices
                        .iter()
                        .position(|idx| *idx == actual_idx)
                })
            })
            .or_else(|| (len > 0).then_some(0));

        let visible = Self::max_visible_rows(len);
        self.state.clamp_selection(len);
        self.state.ensure_visible(len, visible);

        // Notify the callback when filtering changes the selected actual item
        // so live preview stays in sync (e.g. typing in the theme picker).
        if self.selected_actual_idx() != previously_selected {
            self.fire_selection_changed();
        }
    }

    fn build_rows(&self) -> Vec<GenericDisplayRow> {
        self.filtered_indices
            .iter()
            .enumerate()
            .filter_map(|(visible_idx, actual_idx)| {
                self.items.get(*actual_idx).map(|item| {
                    let is_selected = self.state.selected_idx == Some(visible_idx);
                    let prefix = if is_selected { '›' } else { ' ' };
                    let name = item.name.as_str();
                    let marker = if item.is_current {
                        " (current)"
                    } else if item.is_default {
                        " (default)"
                    } else {
                        ""
                    };
                    let name_with_marker = format!("{name}{marker}");
                    let n = visible_idx + 1;
                    let wrap_prefix = if self.is_searchable {
                        // The number keys don't work when search is enabled (since we let the
                        // numbers be used for the search query).
                        format!("{prefix} ")
                    } else {
                        format!("{prefix} {n}. ")
                    };
                    let wrap_prefix_width = UnicodeWidthStr::width(wrap_prefix.as_str());
                    let display_name = format!("{wrap_prefix}{name_with_marker}");
                    let description = is_selected
                        .then(|| item.selected_description.clone())
                        .flatten()
                        .or_else(|| item.description.clone());
                    let wrap_indent = description.is_none().then_some(wrap_prefix_width);
                    let is_disabled = item.is_disabled || item.disabled_reason.is_some();
                    GenericDisplayRow {
                        name: display_name,
                        display_shortcut: item.display_shortcut,
                        match_indices: None,
                        description,
                        wrap_indent,
                        is_disabled,
                        disabled_reason: item.disabled_reason.clone(),
                    }
                })
            })
            .collect()
    }

    fn move_up(&mut self) {
        let before = self.selected_actual_idx();
        let len = self.visible_len();
        self.state.move_up_wrap(len);
        let visible = Self::max_visible_rows(len);
        self.state.ensure_visible(len, visible);
        self.skip_disabled_up();
        if self.selected_actual_idx() != before {
            self.fire_selection_changed();
        }
    }

    fn move_down(&mut self) {
        let before = self.selected_actual_idx();
        let len = self.visible_len();
        self.state.move_down_wrap(len);
        let visible = Self::max_visible_rows(len);
        self.state.ensure_visible(len, visible);
        self.skip_disabled_down();
        if self.selected_actual_idx() != before {
            self.fire_selection_changed();
        }
    }

    fn fire_selection_changed(&self) {
        if let Some(cb) = &self.on_selection_changed
            && let Some(actual_idx) = self.selected_actual_idx()
        {
            cb(actual_idx, &self.app_event_tx);
        }
    }

    fn accept(&mut self) {
        let selected_item = self
            .state
            .selected_idx
            .and_then(|idx| self.filtered_indices.get(idx))
            .and_then(|actual_idx| self.items.get(*actual_idx));
        if let Some(item) = selected_item
            && item.disabled_reason.is_none()
            && !item.is_disabled
        {
            if let Some(idx) = self.state.selected_idx
                && let Some(actual_idx) = self.filtered_indices.get(idx)
            {
                self.last_selected_actual_idx = Some(*actual_idx);
            }
            for act in &item.actions {
                act(&self.app_event_tx);
            }
            if item.dismiss_on_select {
                self.complete = true;
            }
        } else if selected_item.is_none() {
            if let Some(cb) = &self.on_cancel {
                cb(&self.app_event_tx);
            }
            self.complete = true;
        }
    }

    fn rows_width(total_width: u16) -> u16 {
        total_width.saturating_sub(2)
    }

    fn clear_to_terminal_bg(buf: &mut Buffer, area: Rect) {
        let buf_area = buf.area();
        let min_x = area.x.max(buf_area.x);
        let min_y = area.y.max(buf_area.y);
        let max_x = area
            .x
            .saturating_add(area.width)
            .min(buf_area.x.saturating_add(buf_area.width));
        let max_y = area
            .y
            .saturating_add(area.height)
            .min(buf_area.y.saturating_add(buf_area.height));
        for y in min_y..max_y {
            for x in min_x..max_x {
                buf[(x, y)]
                    .set_symbol(" ")
                    .set_style(ratatui::style::Style::reset());
            }
        }
    }

    fn force_bg_to_terminal_bg(buf: &mut Buffer, area: Rect) {
        let buf_area = buf.area();
        let min_x = area.x.max(buf_area.x);
        let min_y = area.y.max(buf_area.y);
        let max_x = area
            .x
            .saturating_add(area.width)
            .min(buf_area.x.saturating_add(buf_area.width));
        let max_y = area
            .y
            .saturating_add(area.height)
            .min(buf_area.y.saturating_add(buf_area.height));
        for y in min_y..max_y {
            for x in min_x..max_x {
                buf[(x, y)].set_bg(ratatui::style::Color::Reset);
            }
        }
    }

    fn stacked_side_content(&self) -> &dyn Renderable {
        self.stacked_side_content
            .as_deref()
            .unwrap_or_else(|| self.side_content.as_ref())
    }

    fn side_layout_width(&self, content_width: u16) -> Option<u16> {
        side_by_side_layout_widths(
            content_width,
            self.side_content_width,
            self.side_content_min_width,
        )
        .map(|(_, side_width)| side_width)
    }

    fn skip_disabled_down(&mut self) {
        let len = self.visible_len();
        for _ in 0..len {
            if let Some(idx) = self.state.selected_idx
                && let Some(actual_idx) = self.filtered_indices.get(idx)
                && self
                    .items
                    .get(*actual_idx)
                    .is_some_and(|item| item.disabled_reason.is_some() || item.is_disabled)
            {
                self.state.move_down_wrap(len);
            } else {
                break;
            }
        }
    }

    fn skip_disabled_up(&mut self) {
        let len = self.visible_len();
        for _ in 0..len {
            if let Some(idx) = self.state.selected_idx
                && let Some(actual_idx) = self.filtered_indices.get(idx)
                && self
                    .items
                    .get(*actual_idx)
                    .is_some_and(|item| item.disabled_reason.is_some() || item.is_disabled)
            {
                self.state.move_up_wrap(len);
            } else {
                break;
            }
        }
    }
}

impl Renderable for ListSelectionView {
    fn desired_height(&self, width: u16) -> u16 {
        let inner_width = popup_content_width(width);
        let side_w = self.side_layout_width(inner_width);

        let full_rows_width = Self::rows_width(width);
        let effective_rows_width = if let Some(sw) = side_w {
            full_rows_width.saturating_sub(SIDE_CONTENT_GAP + sw)
        } else {
            full_rows_width
        };

        // Measure wrapped height for up to MAX_POPUP_ROWS items.
        let rows = self.build_rows();
        let rows_height = measure_rows_height(
            &rows,
            &self.state,
            MAX_POPUP_ROWS,
            effective_rows_width.saturating_add(1),
        );

        let mut height = self.header.desired_height(inner_width);
        height = height.saturating_add(rows_height + 3);
        if self.is_searchable {
            height = height.saturating_add(1);
        }

        if side_w.is_none() {
            let side_h = self.stacked_side_content().desired_height(inner_width);
            if side_h > 0 {
                height = height.saturating_add(1 + side_h);
            }
        }

        if let Some(note) = &self.footer_note {
            let note_width = width.saturating_sub(2);
            let note_lines = wrap_styled_line(note, note_width);
            height = height.saturating_add(note_lines.len() as u16);
        }
        if self.footer_hint.is_some() {
            height = height.saturating_add(1);
        }
        height
    }

    fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 || area.width == 0 {
            return;
        }

        let note_width = area.width.saturating_sub(2);
        let note_lines = self
            .footer_note
            .as_ref()
            .map(|note| wrap_styled_line(note, note_width));
        let note_height = note_lines.as_ref().map_or(0, |lines| lines.len() as u16);
        let footer_rows = note_height + u16::from(self.footer_hint.is_some());
        let [content_area, footer_area] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(footer_rows)]).areas(area);

        let outer_content_area = content_area;
        // Paint the shared menu surface and then layout inside the returned inset.
        let content_area = render_menu_surface(outer_content_area, buf);

        let inner_width = popup_content_width(outer_content_area.width);
        let side_w = self.side_layout_width(inner_width);

        // When side-by-side is active, shrink the list to make room.
        let rows = self.build_rows();
        let full_rows_width = Self::rows_width(outer_content_area.width);
        let effective_rows_width = if let Some(sw) = side_w {
            full_rows_width.saturating_sub(SIDE_CONTENT_GAP + sw)
        } else {
            full_rows_width
        };
        let header_height = self.header.desired_height(inner_width);
        let rows_height = measure_rows_height(
            &rows,
            &self.state,
            MAX_POPUP_ROWS,
            effective_rows_width.saturating_add(1),
        );

        let stacked_side_h = if side_w.is_none() {
            self.stacked_side_content().desired_height(inner_width)
        } else {
            0
        };
        let stacked_gap = if stacked_side_h > 0 { 1 } else { 0 };

        let [header_area, _, search_area, list_area, _, stacked_side_area] = Layout::vertical([
            Constraint::Max(header_height),
            Constraint::Max(1),
            Constraint::Length(if self.is_searchable { 1 } else { 0 }),
            Constraint::Length(rows_height),
            Constraint::Length(stacked_gap),
            Constraint::Length(stacked_side_h),
        ])
        .areas(content_area);

        if header_area.height < header_height {
            let [header_area, elision_area] =
                Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(header_area);
            self.header.render(header_area, buf);
            Paragraph::new(vec![
                Line::from(format!("[… {header_height} lines] ctrl + a view all")).dim(),
            ])
            .render(elision_area, buf);
        } else {
            self.header.render(header_area, buf);
        }

        if self.is_searchable {
            Line::from(self.search_query.clone()).render(search_area, buf);
            let query_span: Span<'static> = if self.search_query.is_empty() {
                self.search_placeholder
                    .as_ref()
                    .map(|placeholder| placeholder.clone().dim())
                    .unwrap_or_else(|| "".into())
            } else {
                self.search_query.clone().into()
            };
            Line::from(query_span).render(search_area, buf);
        }

        if list_area.height > 0 {
            let render_area = Rect {
                x: list_area.x.saturating_sub(2),
                y: list_area.y,
                width: effective_rows_width.max(1),
                height: list_area.height,
            };
            render_rows(
                render_area,
                buf,
                &rows,
                &self.state,
                render_area.height as usize,
                "no matches",
            );
        }

        if let Some(sw) = side_w {
            let side_x = content_area.x + content_area.width - sw;
            let side_area = Rect::new(side_x, content_area.y, sw, content_area.height);

            let clear_x = side_x.saturating_sub(SIDE_CONTENT_GAP);
            let clear_w = outer_content_area
                .x
                .saturating_add(outer_content_area.width)
                .saturating_sub(clear_x);
            Self::clear_to_terminal_bg(
                buf,
                Rect::new(
                    clear_x,
                    outer_content_area.y,
                    clear_w,
                    outer_content_area.height,
                ),
            );
            self.side_content.render(side_area, buf);
            if !self.preserve_side_content_bg {
                Self::force_bg_to_terminal_bg(
                    buf,
                    Rect::new(
                        clear_x,
                        outer_content_area.y,
                        clear_w,
                        outer_content_area.height,
                    ),
                );
            }
        } else if stacked_side_area.height > 0 {
            let clear_height = (outer_content_area.y + outer_content_area.height)
                .saturating_sub(stacked_side_area.y);
            let clear_area = Rect::new(
                outer_content_area.x,
                stacked_side_area.y,
                outer_content_area.width,
                clear_height,
            );
            Self::clear_to_terminal_bg(buf, clear_area);
            self.stacked_side_content().render(stacked_side_area, buf);
        }

        if footer_area.height > 0 {
            let [note_area, hint_area] = Layout::vertical([
                Constraint::Length(note_height),
                Constraint::Length(if self.footer_hint.is_some() { 1 } else { 0 }),
            ])
            .areas(footer_area);

            if let Some(lines) = note_lines {
                let note_area = Rect {
                    x: note_area.x + 2,
                    y: note_area.y,
                    width: note_area.width.saturating_sub(2),
                    height: note_area.height,
                };
                for (idx, line) in lines.iter().enumerate() {
                    if idx as u16 >= note_area.height {
                        break;
                    }
                    let line_area = Rect {
                        x: note_area.x,
                        y: note_area.y + idx as u16,
                        width: note_area.width,
                        height: 1,
                    };
                    line.clone().render(line_area, buf);
                }
            }

            if let Some(hint) = &self.footer_hint {
                let hint_area = Rect {
                    x: hint_area.x + 2,
                    y: hint_area.y,
                    width: hint_area.width.saturating_sub(2),
                    height: hint_area.height,
                };
                hint.clone().dim().render(hint_area, buf);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_event_sender::AppEventSender;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use tokio::sync::mpsc::unbounded_channel;

    #[test]
    fn action_picker_popup_snapshot() {
        let (app_event_tx, _app_event_rx) = unbounded_channel();
        let app_event_tx = AppEventSender::new(app_event_tx);
        let view = ListSelectionView::new(
            SelectionViewParams {
                title: Some("Select Action".to_string()),
                footer_hint: Some(Line::from("Press enter to run, or esc to exit.")),
                items: vec![SelectionItem {
                    name: "Iterate 10 more rounds".to_string(),
                    dismiss_on_select: true,
                    ..Default::default()
                }],
                ..Default::default()
            },
            app_event_tx,
        );

        let width = 54;
        let height = view.desired_height(width);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        terminal
            .draw(|f| view.render(f.area(), f.buffer_mut()))
            .expect("draw");
        insta::assert_snapshot!(terminal.backend());
    }
}
