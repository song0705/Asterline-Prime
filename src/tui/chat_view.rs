//! Renders the chat-first UI: the header block, the single scrolling
//! conversation column, the bottom composer, a footer hint line, and an
//! optional drawer overlay. Chat-block rendering lives here; the header,
//! drawers, and run presentation live in sibling modules.

use std::cell::RefCell;

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Padding, Paragraph};

use crate::domain::event::{ChatItem, FileChangeItem};
use crate::domain::team::{BackendKind, MemberId};
use crate::tui::agents_view;
use crate::tui::app_state::{AppState, FocusRow, focus_row, member_status_is_active};
use crate::tui::completion::Completion;
use crate::tui::drawer_view::{inline_height, is_inline, render_drawer, render_inline_drawer};
use crate::tui::drawers::Drawer;
use crate::tui::file_diff;
use crate::tui::goal_surface;
use crate::tui::header::{render_footer, render_header};
use crate::tui::markdown;
use crate::tui::model_picker;
use crate::tui::status_indicator;
use crate::tui::theme;
use crate::tui::theme::{clip_width, truncate_width};
use crate::tui::tool_display;

/// Snapshot of the flattened chat layout for the last frame. Used by
/// content-anchored mouse selection so anchors survive scrolling.
#[derive(Clone, Debug)]
pub struct ChatLayout {
    /// The chat `inner` rect actually rendered into.
    pub area: Rect,
    /// Flattened index of the first visible line.
    pub first_line: usize,
    /// Total flattened lines, including those above/below the viewport.
    pub total_lines: usize,
    /// Wrap width used to build the lines.
    pub width: usize,
    /// Plain text of the visible viewport (unstyled). Index 0 is `first_line`.
    pub lines: Vec<String>,
    /// Completion popup bounds when it is visible. This uses screen-space
    /// selection because popup rows do not belong to chat history.
    pub completion_area: Option<Rect>,
    /// Composer inner rect for drag-select / copy.
    pub composer_area: Option<Rect>,
    /// Text wrap width inside the composer (excludes the `> ` gutter).
    pub composer_wrap: usize,
    /// Visual rows reserved above composer text (attachment chips).
    pub composer_text_origin: u16,
    /// Clickable tool-group title rows inside the viewport, as
    /// `(flattened line index, chat index of the group's first item)`. The
    /// line index is content-anchored, like `first_line`, so a click maps
    /// correctly after the view has scrolled.
    pub tool_titles: Vec<(usize, usize)>,
}

impl ChatLayout {
    /// Chat index of the tool group whose title sits on flattened line `line`.
    pub fn tool_group_at_line(&self, line: usize) -> Option<usize> {
        self.tool_titles
            .iter()
            .find(|(at, _)| *at == line)
            .map(|(_, key)| *key)
    }

    /// Maximum scroll offset (lines up from the bottom) for this layout.
    pub fn max_scroll(&self) -> usize {
        let height = self.area.height as usize;
        self.total_lines.saturating_sub(height)
    }

    pub fn composer_contains(&self, x: u16, y: u16) -> bool {
        self.composer_area.is_some_and(|area| {
            x >= area.x
                && x < area.x.saturating_add(area.width)
                && y >= area.y
                && y < area.y.saturating_add(area.height)
        })
    }

    pub fn screen_to_composer_index(
        &self,
        composer: &crate::tui::composer::Composer,
        x: u16,
        y: u16,
    ) -> Option<usize> {
        let area = self.composer_area?;
        if !self.composer_contains(x, y) {
            return None;
        }
        let wrap = self.composer_wrap.max(1);
        let row = y
            .saturating_sub(area.y)
            .saturating_sub(self.composer_text_origin) as usize;
        let col = x.saturating_sub(area.x).saturating_sub(2) as usize;
        Some(composer.index_at_visual(wrap, row, col))
    }

    /// Map a screen cell to a content `(line_index, display_column)`, clamping
    /// into the chat area. Returns `None` when the layout is empty.
    pub fn screen_to_content(&self, x: u16, y: u16) -> Option<(usize, usize)> {
        if self.area.is_empty() || self.lines.is_empty() {
            return None;
        }
        let area = self.area;
        let row = (y.clamp(area.y, area.y + area.height.saturating_sub(1)) - area.y) as usize;
        let col = (x.clamp(area.x, area.x + area.width.saturating_sub(1)) - area.x) as usize;
        let line_idx = self
            .first_line
            .saturating_add(row)
            .min(self.total_lines.saturating_sub(1));
        let local = line_idx.saturating_sub(self.first_line);
        let line = self.lines.get(local).map(String::as_str).unwrap_or("");
        let width = theme::display_width(line);
        let col = if width == 0 { 0 } else { col.min(width - 1) };
        Some((line_idx, col))
    }

    /// True when `(x, y)` lies inside the chat content rect.
    pub fn contains(&self, x: u16, y: u16) -> bool {
        !self.area.is_empty()
            && x >= self.area.x
            && x < self.area.x.saturating_add(self.area.width)
            && y >= self.area.y
            && y < self.area.y.saturating_add(self.area.height)
    }

    /// Plain text covered by a drag selection, joined with newlines.
    pub fn selected_text(&self, selection: crate::tui::app_state::ChatSelection) -> String {
        if selection.is_empty() {
            return String::new();
        }
        let (from, to) = selection.normalized();
        let last = self.total_lines.saturating_sub(1);
        let start_line = from.0.max(self.first_line).min(last);
        let end_line = to.0.max(self.first_line).min(last);
        let mut out = String::new();
        for (offset, line) in self.lines.iter().enumerate() {
            let index = self.first_line.saturating_add(offset);
            if index < start_line || index > end_line {
                continue;
            }
            if !out.is_empty() {
                out.push('\n');
            }
            let start_col = if index == start_line { from.1 } else { 0 };
            let end_col = if index == end_line {
                to.1.saturating_add(1)
            } else {
                theme::display_width(line)
            };
            // The user band pads rows to the full width; do not copy the padding.
            out.push_str(theme::slice_display_cols(line, start_col, end_col).trim_end());
        }
        out
    }
}

/// Paint the full chat UI. Returns a layout snapshot of the conversation
/// column for content-anchored selection (ignored when a drawer is open).
pub fn render(frame: &mut Frame<'_>, state: &AppState) -> Option<ChatLayout> {
    let layout = render_screen(frame, state);
    // Remember where the links landed so a click can open them.
    crate::tui::hyperlink::record_frame(frame.buffer_mut());
    layout
}

fn render_screen(frame: &mut Frame<'_>, state: &AppState) -> Option<ChatLayout> {
    // The Agents list replaces the whole chat screen: no transcript,
    // composer or footer.
    if state.drawer() == Some(Drawer::Agents) {
        agents_view::render(frame, state);
        return None;
    }
    // The composer grows with its content up to a cap, like a real textarea.
    const MAX_COMPOSER_ROWS: u16 = 8;
    let composer_avail = frame.area().width.saturating_sub(2) as usize;
    let composer_rows =
        (state.composer().visual_line_count(composer_avail) as u16).clamp(1, MAX_COMPOSER_ROWS);
    // One band padding row above and one below the input, like a user band.
    let composer_height = composer_rows + 2;
    let completion = if state.drawer().is_none() && !state.model_picker_open() {
        state.completion()
    } else {
        None
    };
    // The key-hint footer stays one row below the composer band; the
    // completion list sits above the band instead.
    let bottom_height = 1;
    let completion_height = completion.as_ref().map_or(0, completion_popup_height);
    let queued_preview = queued_input_preview_lines(state, frame.area().width as usize);
    let queued_height = queued_preview.len() as u16;
    // The goal's status line sits just below the chat, above anything that
    // belongs to the composer.
    let goal_height = goal_surface::height(state);
    let approval_height = approval_card_height(state, frame.area().width);
    let picker = state.model_picker_view();
    // Leave the chat a few rows even on a short terminal.
    let picker_height = picker.as_ref().map_or(0, |view| {
        model_picker::picker_height(view).min(frame.area().height.saturating_sub(10))
    });
    // Panels above the composer are set off from the transcript by one blank
    // row (their first row) instead of a rule.
    const PANEL_GAP: u16 = 1;
    // The mode panel sits above the composer like the picker: as tall as
    // its content, at most about half the chat area.
    let inline_drawer = state.drawer().filter(is_inline);
    let inline_drawer_height = inline_drawer.as_ref().map_or(0, |drawer| {
        let chat_rows = frame.area().height.saturating_sub(
            1 + goal_height + queued_height + approval_height + composer_height + bottom_height,
        );
        inline_height(
            state,
            drawer,
            frame.area().width,
            (chat_rows / 2).max(5).min(chat_rows.saturating_sub(2)),
        )
    });
    let panel_height = picker_height
        .max(inline_drawer_height)
        .max(completion_height);
    let above_composer_height = if panel_height > 0 {
        panel_height + PANEL_GAP
    } else {
        0
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(goal_height),
            Constraint::Length(queued_height),
            Constraint::Length(approval_height),
            Constraint::Length(above_composer_height),
            Constraint::Length(composer_height),
            Constraint::Length(bottom_height),
        ])
        .split(frame.area());

    render_header(frame, chunks[0], state);
    let mut layout = render_chat(frame, chunks[1], state);
    goal_surface::render(frame, chunks[2], state);
    frame.render_widget(Paragraph::new(queued_preview), chunks[3]);
    render_approval_card(frame, chunks[4], state);
    render_composer(frame, chunks[6], state);
    // The panel area minus its leading blank row.
    let panel = Rect::new(
        chunks[5].x,
        chunks[5].y + PANEL_GAP.min(chunks[5].height),
        chunks[5].width,
        chunks[5].height.saturating_sub(PANEL_GAP),
    );
    if let Some(view) = &picker {
        // After the composer so the search box owns the cursor.
        model_picker::render_picker(frame, panel, view);
    }
    let composer_inner = composer_text_area(chunks[6]);
    layout.composer_area = Some(composer_inner);
    layout.composer_wrap = (composer_inner.width as usize).saturating_sub(2);
    layout.composer_text_origin = 0;
    if let Some(completion) = completion {
        render_popup(frame, panel, &completion, state.popup_selected());
        layout.completion_area = Some(panel);
    }
    render_footer(frame, chunks[7], state);

    if let Some(drawer) = state.drawer() {
        if is_inline(&drawer) {
            render_inline_drawer(frame, panel, state, &drawer);
        } else {
            // The overlay is as tall as its content (at most the chat
            // column), full width, anchored above the composer; it clears
            // every cell it covers.
            render_drawer(frame, chunks[1], state, &drawer);
        }
        // Drawers replace the chat for selection; content selection uses the
        // drawer path.
        return None;
    }
    Some(layout)
}

/// Left edge shared by every transcript row: user text, agent text, tool and
/// agent-message group titles, reasoning, notices and the Working row.
const INDENT_TEXT: &str = "  ";

/// Columns for wrapped text: the left indent and one column of right margin.
fn content_width(width: usize) -> usize {
    width.saturating_sub(INDENT_TEXT.len() + 1).max(1)
}

/// Widest a transcript row gets: long lines stay readable on wide terminals.
const MAX_READING_WIDTH: usize = 120;

const QUEUED_PREVIEW_LINE_LIMIT: usize = 3;

fn queued_input_preview_lines(state: &AppState, width: usize) -> Vec<Line<'static>> {
    if state.queued_prompt_count() == 0 || width < 4 {
        return Vec::new();
    }

    let mut lines = vec![Line::from(vec![
        Span::styled("• ", theme::muted()),
        Span::styled("Queued follow-up inputs", theme::muted()),
    ])];
    let content_width = width.saturating_sub(4).max(1);

    for member in state.members() {
        for prompt in state.queued_prompts_for(&member.id) {
            let wrapped = markdown::wrap(prompt, content_width);
            let wrapped_len = wrapped.len();
            for (index, text) in wrapped
                .into_iter()
                .take(QUEUED_PREVIEW_LINE_LIMIT)
                .enumerate()
            {
                let prefix = if index == 0 {
                    format!("  ↳ @{} ", member.display_name)
                } else {
                    "    ".to_string()
                };
                lines.push(Line::from(vec![
                    Span::styled(prefix, theme::muted()),
                    Span::styled(text, theme::muted().italic()),
                ]));
            }
            if wrapped_len > QUEUED_PREVIEW_LINE_LIMIT {
                lines.push(Line::from(Span::styled("    …", theme::muted().italic())));
            }
        }
    }

    lines.push(Line::from(Span::styled(
        "    Shift+← edit last queued message",
        theme::muted(),
    )));
    lines
}

const MAX_COMPLETION_ROWS: usize = 6;

fn completion_popup_height(completion: &Completion) -> u16 {
    completion.items.len().min(MAX_COMPLETION_ROWS) as u16
}

fn render_popup(frame: &mut Frame<'_>, area: Rect, completion: &Completion, selected: usize) {
    let count = completion.items.len();
    let shown = count.min(MAX_COMPLETION_ROWS);
    let selected = selected.min(count.saturating_sub(1));
    let start = if selected >= shown {
        selected + 1 - shown
    } else {
        0
    };
    let name_width = completion
        .items
        .iter()
        .filter_map(|item| {
            let (name, description) = completion_parts(&item.label);
            description.map(|_| theme::display_width(name))
        })
        .max()
        .unwrap_or(0)
        .min(18);
    let lines: Vec<Line> = completion
        .items
        .iter()
        .enumerate()
        .skip(start)
        .take(shown)
        .map(|(i, item)| {
            let (name, description) = completion_parts(&item.label);
            let is_selected = i == selected;
            let name_style = if is_selected {
                theme::accent()
            } else {
                theme::emphasis()
            };
            let marker_style = if is_selected {
                theme::accent()
            } else {
                Style::default()
            };
            let marker = if is_selected { "› " } else { "  " };
            let mut spans = vec![
                Span::styled(marker, marker_style),
                Span::styled(name.to_string(), name_style),
            ];
            if let Some(description) = description {
                let padding = name_width.saturating_sub(theme::display_width(name)) + 2;
                spans.push(Span::raw(" ".repeat(padding)));
                spans.push(Span::styled(
                    description.to_string(),
                    if is_selected {
                        theme::accent()
                    } else {
                        theme::muted()
                    },
                ));
            }
            Line::from(spans)
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), area);
}

fn completion_parts(label: &str) -> (&str, Option<&str>) {
    match label.split_once(" — ") {
        Some((name, description)) => (name, Some(description)),
        None => (label, None),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PaintKey {
    width: usize,
    focus: Option<MemberId>,
    roster_revision: u64,
    thinking_expanded: bool,
    diffs_expanded: bool,
    tools_expanded: bool,
    /// Bumps whenever a single tool group is toggled or the set is cleared.
    tool_groups_revision: u64,
    find_current: Option<usize>,
    /// The welcome page's model line, which fills in when a catalog lands.
    welcome_model: Option<String>,
    /// Every member's model label (speaker labels, spawn rows), which change
    /// on a `/model` switch or once a catalog names the model.
    member_labels: Vec<String>,
}

#[derive(Default)]
struct ChatPaintCache {
    key: Option<PaintKey>,
    item_revs: Vec<u64>,
    /// Finished prefix kept as one entry so streaming only re-paints the tail.
    prefix: Option<CachedChunk>,
    tail: Option<CachedChunk>,
    live_slots: Vec<LiveSlot>,
}

/// Visible styled lines, their plain text, the total flattened line count,
/// and the clickable tool-group titles `(line index, group key)`.
type PaintedChat = (Vec<Line<'static>>, Vec<String>, usize, Vec<(usize, usize)>);

#[derive(Clone, Default)]
struct CachedChunk {
    item_lo: usize,
    item_hi: usize,
    height: usize,
    lines: Option<Vec<Line<'static>>>,
    plain: Option<Vec<String>>,
    /// `(line offset in the chunk, chat index of the group's first item)` for
    /// each clickable tool-group title.
    tool_titles: Vec<(usize, usize)>,
}

#[derive(Clone, Debug)]
struct LiveSlot {
    at: usize,
    member: MemberId,
    show_header: bool,
}

thread_local! {
    static CHAT_PAINT: RefCell<ChatPaintCache> = const { RefCell::new(ChatPaintCache {
        key: None,
        item_revs: Vec::new(),
        prefix: None,
        tail: None,
        live_slots: Vec::new(),
    }) };
}

enum LiveRender<'a> {
    #[cfg(test)]
    Inline,
    Record(&'a mut Vec<LiveSlot>),
}

fn paint_key(state: &AppState, width: usize) -> PaintKey {
    PaintKey {
        width,
        focus: state.focus().cloned(),
        roster_revision: state.roster_revision(),
        thinking_expanded: state.thinking_expanded(),
        diffs_expanded: state.diffs_expanded(),
        tools_expanded: state.tools_expanded(),
        tool_groups_revision: state.tool_groups_revision(),
        find_current: state.find_current_chat_index(),
        welcome_model: state
            .chat()
            .is_empty()
            .then(|| state.welcome_model_summary())
            .flatten(),
        member_labels: state
            .members()
            .iter()
            .map(|member| state.member_model_label(member))
            .chain(
                state
                    .departed_members()
                    .iter()
                    .filter_map(|gone| state.member_model_label_by_id(&gone.id)),
            )
            .collect(),
    }
}

fn line_to_plain(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

fn render_chat(frame: &mut Frame<'_>, area: Rect, state: &AppState) -> ChatLayout {
    // No side padding: the transcript owns its left edge (a 2-column indent)
    // and the user band runs the full width.
    let inner = area;

    let width = inner.width as usize;
    let height = inner.height as usize;
    let (mut visible, plain, total, tool_titles) =
        painted_chat(state, width, state.scroll(), height);

    let max_start = total.saturating_sub(height);
    let start = max_start.saturating_sub(state.scroll());
    if let Some(selection) = state.chat_selection() {
        apply_selection_style(&mut visible, start, selection);
    }
    frame.render_widget(Paragraph::new(visible), inner);

    ChatLayout {
        area: inner,
        first_line: start,
        total_lines: total,
        width,
        lines: plain,
        completion_area: None,
        composer_area: None,
        composer_wrap: 0,
        composer_text_origin: 0,
        tool_titles,
    }
}

/// Grok Build keeps a pinned header and virtualizes the middle scrollback:
/// each finished chunk is measured once, off-screen render caches are
/// dropped, and a live tail is the only thing rebuilt while streaming.
fn painted_chat(state: &AppState, width: usize, scroll: usize, height: usize) -> PaintedChat {
    CHAT_PAINT.with(|cell| {
        let mut cache = cell.borrow_mut();
        ensure_scrollback_cached(state, width, &mut cache);
        compose_scrollback(&mut cache, state, width, scroll, height)
    })
}

fn ensure_scrollback_cached(state: &AppState, width: usize, cache: &mut ChatPaintCache) {
    let key = paint_key(state, width);
    let spinner_frame = status_indicator::spinner();
    let revs: Vec<u64> = state
        .chat()
        .iter()
        .map(|item| paint_item_revision(state, item, spinner_frame))
        .collect();
    let flags_changed = cache.key.as_ref() != Some(&key);
    let dirty = if flags_changed {
        0
    } else {
        rewind_work_run(state.chat(), first_rev_mismatch(&cache.item_revs, &revs))
    };
    if !flags_changed
        && cache.item_revs.len() == revs.len()
        && dirty == revs.len()
        && cache.tail.is_some()
    {
        cache.item_revs = revs;
        return;
    }

    if dirty == 0 {
        cache.prefix = None;
        cache.tail = None;
        cache.live_slots.clear();
    } else if cache
        .prefix
        .as_ref()
        .is_some_and(|prefix| prefix.item_hi > dirty)
    {
        cache.prefix = None;
        cache.live_slots.clear();
    }
    if cache.prefix.is_none() && dirty > 0 {
        cache.live_slots.clear();
        cache.prefix = Some(render_chunk(state, width, 0, dirty, &mut cache.live_slots));
    }

    let prefix_height = cache.prefix.as_ref().map_or(0, |prefix| prefix.height);
    cache.live_slots.retain(|slot| slot.at <= prefix_height);
    let tail_start = cache.prefix.as_ref().map_or(0, |prefix| prefix.item_hi);
    let slot_from = cache.live_slots.len();
    cache.tail = Some(render_chunk(
        state,
        width,
        tail_start,
        state.chat().len(),
        &mut cache.live_slots,
    ));
    for slot in cache.live_slots.iter_mut().skip(slot_from) {
        slot.at = slot.at.saturating_add(prefix_height);
    }
    // A cached prefix slot stays attached to a finished reply. Drop it once
    // that member has a later region in the tail, or the old block lights up
    // again the moment they start working.
    cache.live_slots.retain(|slot| {
        slot.at > prefix_height
            || !member_has_later_live_region(state.chat(), tail_start, &slot.member)
    });
    cache.item_revs = revs;
    cache.key = Some(key);
}

/// Live thinking and in-flight tools both own a spinner. Include its frame in
/// their cache revision so the virtualized tail is repainted while they run.
fn paint_item_revision(state: &AppState, item: &ChatItem, spinner_frame: &str) -> u64 {
    let revision = item_rev(item);
    match item {
        ChatItem::Thinking { member, .. } if state.is_thinking_live(member) => {
            revision ^ fnv1a64_bytes(spinner_frame.as_bytes())
        }
        ChatItem::Tool { ok: None, .. } => revision ^ fnv1a64_bytes(spinner_frame.as_bytes()),
        _ => revision,
    }
}

fn first_rev_mismatch(old: &[u64], new: &[u64]) -> usize {
    let shared = old.len().min(new.len());
    for index in 0..shared {
        if old[index] != new[index] {
            return index;
        }
    }
    shared.min(new.len())
}

fn rewind_work_run(items: &[ChatItem], dirty: usize) -> usize {
    if let Some(ChatItem::User { targets, .. }) = items.get(dirty) {
        // A newly targeted prompt starts a new live region. Repaint that
        // member's prior region as well, otherwise a cached trailing Working
        // slot remains attached to the completed answer above it.
        return targets
            .iter()
            .filter_map(|target| {
                items[..dirty]
                    .iter()
                    .rposition(|item| item_member(item).is_some_and(|member| member == target))
            })
            .min()
            .unwrap_or(dirty);
    }
    let Some(ChatItem::Tool { member, .. } | ChatItem::Diff { member, .. }) = items.get(dirty)
    else {
        return dirty;
    };
    let mut start = dirty;
    while start > 0 {
        match &items[start - 1] {
            ChatItem::Tool {
                member: prev_member,
                ..
            }
            | ChatItem::Diff {
                member: prev_member,
                ..
            } if prev_member == member => start -= 1,
            _ => break,
        }
    }
    start
}

fn render_chunk(
    state: &AppState,
    width: usize,
    start: usize,
    end: usize,
    live_slots: &mut Vec<LiveSlot>,
) -> CachedChunk {
    let mut lines = Vec::new();
    let mut tool_titles = Vec::new();
    if start == 0 && state.chat().is_empty() {
        lines.push(Line::raw(""));
        lines.extend(quick_start_lines(state));
        lines.push(Line::raw(""));
    }
    render_chat_history_range(
        state,
        width,
        start,
        end,
        &mut lines,
        LiveRender::Record(live_slots),
        &mut tool_titles,
    );
    if end == state.chat().len() && state.omitted_active_output_count() > 0 {
        let omitted = state.omitted_active_output_count();
        let text = format!(
            "… {omitted} active output cell(s) omitted by the TUI memory limit; final results will appear on completion"
        );
        for wrapped in markdown::wrap(&text, content_width(width.min(MAX_READING_WIDTH))) {
            lines.push(Line::from(Span::styled(
                format!("{INDENT_TEXT}{wrapped}"),
                theme::warning_bold(),
            )));
        }
        lines.push(Line::raw(""));
    }
    CachedChunk {
        item_lo: start,
        item_hi: end,
        height: lines.len(),
        plain: Some(lines.iter().map(line_to_plain).collect()),
        lines: Some(lines),
        tool_titles,
    }
}

fn item_rev(item: &ChatItem) -> u64 {
    let (tag, len, bytes) = match item {
        ChatItem::User { body, .. } => (1u64, body.len(), body.as_bytes()),
        ChatItem::Agent { text, .. } => (2, text.len(), text.as_bytes()),
        ChatItem::Thinking {
            text, elapsed_secs, ..
        } => (3 ^ elapsed_secs.unwrap_or(0), text.len(), text.as_bytes()),
        ChatItem::Tool { detail, name, .. } => (4, detail.len() + name.len(), detail.as_bytes()),
        ChatItem::Diff { files, .. } => {
            let len = files.iter().map(|file| file.path.len()).sum();
            (
                5,
                len,
                files
                    .first()
                    .map(|file| file.path.as_bytes())
                    .unwrap_or(&[]),
            )
        }
        ChatItem::Route { body, .. } => (6, body.len(), body.as_bytes()),
        ChatItem::Notice { text, .. } => (7, text.len(), text.as_bytes()),
        ChatItem::Spawn {
            parent,
            child,
            summary,
        } => (
            10,
            parent.as_str().len() + child.as_str().len() + summary.len(),
            child.as_str().as_bytes(),
        ),
        ChatItem::Close {
            parent,
            child,
            name,
            by_user,
        } => (
            12,
            parent.as_str().len() + child.as_str().len() + name.len() + usize::from(*by_user),
            child.as_str().as_bytes(),
        ),
        ChatItem::Error { message, .. } => (8, message.len(), message.as_bytes()),
        ChatItem::Verdict { summary, .. } => (9, summary.len(), summary.as_bytes()),
        ChatItem::GoalMarker { turn, .. } => (11u64, *turn as usize, &[][..]),
    };
    tag.wrapping_shl(56) ^ (len as u64).wrapping_shl(32) ^ fnv1a64_bytes(bytes)
}

fn fnv1a64_bytes(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn compose_scrollback(
    cache: &mut ChatPaintCache,
    state: &AppState,
    width: usize,
    scroll: usize,
    height: usize,
) -> PaintedChat {
    let prefix_h = cache.prefix.as_ref().map_or(0, |chunk| chunk.height);
    let tail_h = cache.tail.as_ref().map_or(0, |chunk| chunk.height);
    let mut live = realized_live_slots(cache, state, width);
    // A Working row in the middle of the transcript is an entry too: one blank
    // row keeps it off the entry below.
    let content_rows = prefix_h + tail_h;
    for (at, lines) in &mut live {
        if *at < content_rows && lines.last().is_some_and(|line| !line_is_blank(line)) {
            lines.push(Line::raw(""));
        }
    }
    let extra: usize = live.iter().map(|(_, lines)| lines.len()).sum();
    let total = prefix_h + tail_h + extra;
    let max_start = total.saturating_sub(height);
    let start = max_start.saturating_sub(scroll);
    let end = start.saturating_add(height);

    evict_offscreen_chunk(cache.prefix.as_mut(), 0, start, end);
    evict_offscreen_chunk(cache.tail.as_mut(), prefix_h, start, end);
    ensure_chunk_lines(state, width, cache.prefix.as_mut());
    ensure_chunk_lines(state, width, cache.tail.as_mut());

    let mut visible = Vec::with_capacity(height.min(total));
    let mut plain = Vec::with_capacity(height.min(total));
    let mut titles = Vec::new();
    let mut index = 0usize;
    let mut live_at = 0usize;
    // `title` is the chat index of the tool group this row titles, if any.
    let mut emit = |line: &Line<'static>, title: Option<usize>| {
        if index >= start && visible.len() < height {
            plain.push(line_to_plain(line));
            visible.push(line.clone());
            if let Some(key) = title {
                titles.push((index, key));
            }
        }
        index += 1;
    };
    let flush_live_at =
        |at: usize, live_at: &mut usize, emit: &mut dyn FnMut(&Line<'static>, Option<usize>)| {
            while *live_at < live.len() && live[*live_at].0 == at {
                for live_line in &live[*live_at].1 {
                    emit(live_line, None);
                }
                *live_at += 1;
            }
        };
    let emit_chunk = |chunk: Option<&CachedChunk>,
                      origin: usize,
                      live_at: &mut usize,
                      emit: &mut dyn FnMut(&Line<'static>, Option<usize>)| {
        if let Some(chunk) = chunk
            && let Some(lines) = chunk.lines.as_ref()
        {
            for (offset, line) in lines.iter().enumerate() {
                flush_live_at(origin + offset, live_at, emit);
                let title = chunk
                    .tool_titles
                    .iter()
                    .find(|(at, _)| *at == offset)
                    .map(|(_, key)| *key);
                emit(line, title);
            }
        } else if let Some(chunk) = chunk {
            for offset in 0..chunk.height {
                flush_live_at(origin + offset, live_at, emit);
                emit(&Line::raw(""), None);
            }
        }
    };
    emit_chunk(cache.prefix.as_ref(), 0, &mut live_at, &mut emit);
    emit_chunk(cache.tail.as_ref(), prefix_h, &mut live_at, &mut emit);
    while live_at < live.len() {
        for live_line in &live[live_at].1 {
            emit(live_line, None);
        }
        live_at += 1;
    }
    (visible, plain, total, titles)
}

fn evict_offscreen_chunk(
    chunk: Option<&mut CachedChunk>,
    origin: usize,
    view_start: usize,
    view_end: usize,
) {
    let Some(chunk) = chunk else {
        return;
    };
    let chunk_end = origin.saturating_add(chunk.height);
    if chunk_end <= view_start || origin >= view_end {
        chunk.lines = None;
        chunk.plain = None;
    }
}

fn ensure_chunk_lines(state: &AppState, width: usize, chunk: Option<&mut CachedChunk>) {
    let Some(chunk) = chunk else {
        return;
    };
    if chunk.lines.is_some() {
        return;
    }
    let mut unused = Vec::new();
    *chunk = render_chunk(state, width, chunk.item_lo, chunk.item_hi, &mut unused);
}

fn realized_live_slots(
    cache: &ChatPaintCache,
    state: &AppState,
    width: usize,
) -> Vec<(usize, Vec<Line<'static>>)> {
    let mut chosen = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for slot in cache.live_slots.iter().rev() {
        if seen.insert(slot.member.clone()) {
            chosen.push(slot);
        }
    }
    chosen.reverse();

    let mut out = Vec::new();
    for slot in chosen {
        let mut lines = Vec::new();
        if render_live_member_activity(state, width, &slot.member, slot.show_header, &mut lines) {
            if slot.show_header {
                lines.push(Line::raw(""));
            }
            out.push((slot.at, lines));
        }
    }
    out
}

#[cfg(test)]
fn render_chat_history(state: &AppState, width: usize, start: usize, out: &mut Vec<Line<'static>>) {
    render_chat_history_range(
        state,
        width,
        start,
        state.chat().len(),
        out,
        LiveRender::Inline,
        &mut Vec::new(),
    );
}

fn apply_selection_style(
    visible: &mut [Line<'_>],
    first_line: usize,
    selection: crate::tui::app_state::ChatSelection,
) {
    let (from, to) = selection.normalized();
    for (offset, line) in visible.iter_mut().enumerate() {
        let index = first_line + offset;
        if index < from.0 || index > to.0 {
            continue;
        }
        let start_col = if index == from.0 { from.1 } else { 0 };
        let end_col = if index == to.0 {
            to.1.saturating_add(1)
        } else {
            usize::MAX
        };
        *line = restyle_column_range(line, start_col, end_col);
    }
}

fn restyle_column_range(line: &Line<'_>, start_col: usize, end_col: usize) -> Line<'static> {
    let end_col = end_col.max(start_col);
    let mut out = Vec::new();
    let mut col = 0;
    for span in &line.spans {
        let mut unselected = String::new();
        let mut selected = String::new();
        for ch in span.content.chars() {
            let width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if col >= start_col && col < end_col {
                if !unselected.is_empty() {
                    out.push(Span::styled(std::mem::take(&mut unselected), span.style));
                }
                selected.push(ch);
            } else {
                if !selected.is_empty() {
                    out.push(Span::styled(
                        std::mem::take(&mut selected),
                        theme::chat_selection(),
                    ));
                }
                unselected.push(ch);
            }
            col += width;
        }
        if !unselected.is_empty() {
            out.push(Span::styled(unselected, span.style));
        }
        if !selected.is_empty() {
            out.push(Span::styled(selected, theme::chat_selection()));
        }
    }
    Line::from(out)
}

/// Display order that keeps one member's work together only when a later
/// prompt was sent while that member was already working. Sequential turns
/// stay chronological.
fn grouped_chat_indices(items: &[ChatItem], start: usize, end: usize) -> Vec<usize> {
    let start = start.min(items.len());
    let end = end.min(items.len()).max(start);
    let mut used = vec![false; end];
    let mut out = Vec::with_capacity(end.saturating_sub(start));
    for i in start..end {
        if used[i] {
            continue;
        }
        used[i] = true;
        out.push(i);
        let Some(member) = item_member(&items[i]).cloned() else {
            continue;
        };
        for j in (i + 1)..end {
            if used[j] {
                continue;
            }
            if user_breaks_member_region(&items[j], &member)
                || relay_starts_member_region(&items[j], &member)
            {
                break;
            }
            if item_member(&items[j]) == Some(&member) {
                used[j] = true;
                out.push(j);
            }
        }
    }
    out
}

/// An incoming member relay is a fresh unit of work for its recipient. Do not
/// pull that recipient's later tools back under their earlier reply just
/// because another member's route sat between the two in the event timeline.
fn relay_starts_member_region(item: &ChatItem, member: &MemberId) -> bool {
    matches!(item, ChatItem::Route { to, .. } if to.iter().any(|target| target == member.as_str()))
}

fn user_breaks_member_region(item: &ChatItem, member: &MemberId) -> bool {
    match item {
        ChatItem::User {
            targets,
            interrupted,
            ..
        } => {
            if targets.iter().any(|target| target == member) {
                return true;
            }
            // A prompt sent while this member was already working should not
            // split their later output into the other member's region.
            !interrupted.iter().any(|busy| busy == member)
        }
        // The goal loop starts a turn for its member without a user message;
        // the marker row opens a new region just like a prompt would.
        ChatItem::GoalMarker { member: marked, .. } => marked == member,
        _ => false,
    }
}

/// The nearest earlier row that is actually drawn, looking back through this
/// paint chunk and then the items before it. With `skip_agent_messages`,
/// `⇄ agents` rows are passed over.
fn previous_rendered<'a>(
    state: &AppState,
    items: &'a [ChatItem],
    order: &[usize],
    pos: usize,
    start: usize,
    skip_agent_messages: bool,
) -> Option<&'a ChatItem> {
    let counts = |item: &&ChatItem| {
        item_renders(item, state) && !(skip_agent_messages && is_agent_message(item))
    };
    order[..pos]
        .iter()
        .rev()
        .map(|&p| &items[p])
        .find(counts)
        .or_else(|| {
            (start > 0)
                .then(|| items[..start].iter().rev().find(counts))
                .flatten()
        })
}

fn render_chat_history_range(
    state: &AppState,
    width: usize,
    start: usize,
    end: usize,
    out: &mut Vec<Line<'static>>,
    mut live: LiveRender<'_>,
    titles: &mut Vec<(usize, usize)>,
) {
    // Text never wraps wider than the reading width; only the user band
    // keeps the full terminal width for its background.
    let full_width = width;
    let width = width.min(MAX_READING_WIDTH);
    let items = state.chat();
    let start = start.min(items.len());
    let end = end.min(items.len()).max(start);
    // A focused member's window drops everyone else's work from the display
    // order, so hidden items never act as a neighbour or previous speaker.
    let order: Vec<usize> = grouped_chat_indices(items, start, end)
        .into_iter()
        .filter(|&i| focus_row(state, &items[i]) != FocusRow::Hidden)
        .collect();
    let mut rendered_live = std::collections::HashSet::new();
    let mut skip_until = 0usize;
    for (pos, &i) in order.iter().enumerate() {
        if pos < skip_until {
            continue;
        }
        let item = &items[i];
        if matches!(
            item,
            ChatItem::Thinking {
                backend: BackendKind::Codex,
                ..
            }
        ) {
            continue;
        }
        let before = out.len();
        let previous = previous_rendered(state, items, &order, pos, start, false);
        // Agent-to-agent rows (`⇄ agents`) belong to the block they sit in,
        // so they never decide who spoke last.
        let speaker_before = previous_rendered(state, items, &order, pos, start, true);
        let after_agent_messages = previous.is_some_and(is_agent_message);
        // A new user message or another agent's reply starts a new block. The
        // same agent continuing after its own tools or agent-message rows
        // stays under its header; two consecutive replies keep their titles.
        let show_sender_header = match item {
            ChatItem::User { .. } => true,
            ChatItem::Agent { member, .. } => match speaker_before {
                Some(ChatItem::Agent { member: prev, .. }) if prev == member => {
                    !after_agent_messages
                }
                Some(prev) if item_sender(prev) == Some(ChatSender::Agent(member.clone())) => false,
                _ => true,
            },
            _ => item_sender(item) != speaker_before.and_then(item_sender),
        };
        let run_end = consecutive_work_run_end(items, &order, pos);
        let is_find_current = order[pos..=run_end]
            .iter()
            .any(|&idx| state.find_current_chat_index() == Some(idx));
        if matches!(item, ChatItem::Tool { .. } | ChatItem::Diff { .. }) {
            let run: Vec<&ChatItem> = order[pos..=run_end].iter().map(|&j| &items[j]).collect();
            let anchor = ToolGroupAnchor {
                first_index: i,
                titles,
            };
            render_work_run(&run, anchor, width, state, out, show_sender_header);
            skip_until = run_end + 1;
        } else if is_agent_message(item) {
            let run: Vec<&ChatItem> = order[pos..=run_end].iter().map(|&j| &items[j]).collect();
            render_agent_messages(&run, width, state, out);
            skip_until = run_end + 1;
        } else {
            render_item_wide(item, width, full_width, state, out, show_sender_header);
        }
        if is_find_current && let Some(line) = out.get_mut(before) {
            // Marker in column 0, over the first blank column of the row, so
            // the text keeps its place.
            mark_find_match(line);
        }
        // Exactly one blank row between consecutive entries (a user band, a
        // reply, a tool group, ...); nothing inside an entry.
        if out.len() > before {
            out.push(Line::raw(""));
        }

        let item = &items[order[run_end]];
        let next = order.get(run_end + 1).map(|&j| &items[j]);
        let member_block_ends = item_member(item)
            .is_some_and(|member| next.and_then(item_member).is_none_or(|next| next != member));
        if member_block_ends
            && let Some(member) = item_member(item)
            && live_activity_belongs_after(items, &order, run_end, member)
        {
            // Thinking is already a live indicator. Mark the region claimed
            // so the trailing fallback does not reprint Working at the bottom.
            match &mut live {
                #[cfg(test)]
                LiveRender::Inline => {
                    if matches!(item, ChatItem::Thinking { .. })
                        || render_live_member_activity(state, width, member, false, out)
                    {
                        rendered_live.insert(member.clone());
                    }
                }
                LiveRender::Record(slots) => {
                    if matches!(item, ChatItem::Thinking { .. }) {
                        rendered_live.insert(member.clone());
                    } else {
                        slots.push(LiveSlot {
                            at: out.len(),
                            member: member.clone(),
                            show_header: false,
                        });
                        rendered_live.insert(member.clone());
                    }
                }
            }
        }
    }
    if end == items.len() {
        for member in state.members() {
            if rendered_live.contains(&member.id) {
                continue;
            }
            if state.focus().is_some_and(|focus| focus != &member.id) {
                continue;
            }
            // Prefix already recorded this member's live row. A tail-only
            // fallback would reprint Working under a fresh header.
            if prefix_owns_member_live_region(items, start, &member.id) {
                continue;
            }
            match &mut live {
                #[cfg(test)]
                LiveRender::Inline => {
                    if member_status_is_active(member.status)
                        && render_live_member_activity(state, width, &member.id, true, out)
                    {
                        out.push(Line::raw(""));
                    }
                }
                LiveRender::Record(slots) => {
                    slots.push(LiveSlot {
                        at: out.len(),
                        member: member.id.clone(),
                        show_header: true,
                    });
                }
            }
        }
    }
}

/// Live thinking belongs on this member's latest region, not on an earlier
/// finished reply. A later prompt only closes the region when it would also
/// break grouping — talking to someone else while this member is still
/// working must leave their thinking in their own block.
///
/// Scan the full timeline, not just this paint chunk. A cached prefix does
/// not see the member's next turn in `order`, and would otherwise keep the
/// Working row on the completed reply.
fn live_activity_belongs_after(
    items: &[ChatItem],
    order: &[usize],
    pos: usize,
    member: &MemberId,
) -> bool {
    let Some(&item_index) = order.get(pos) else {
        return false;
    };
    !member_has_later_live_region(items, item_index + 1, member)
}

fn item_opens_new_member_region(item: &ChatItem, member: &MemberId) -> bool {
    user_breaks_member_region(item, member) || relay_starts_member_region(item, member)
}

fn member_has_later_live_region(items: &[ChatItem], after: usize, member: &MemberId) -> bool {
    items.iter().skip(after).any(|later| {
        item_member(later) == Some(member) || item_opens_new_member_region(later, member)
    })
}

fn prefix_owns_member_live_region(items: &[ChatItem], start: usize, member: &MemberId) -> bool {
    start > 0
        && items[..start]
            .iter()
            .any(|item| item_member(item) == Some(member))
        && !member_has_later_live_region(items, start, member)
}

fn render_live_member_activity(
    state: &AppState,
    width: usize,
    member_id: &MemberId,
    show_placeholder_header: bool,
    out: &mut Vec<Line<'static>>,
) -> bool {
    if state.focus().is_some_and(|focus| focus != member_id) {
        return false;
    }
    let width = width.min(MAX_READING_WIDTH);
    let Some(member) = state.members().iter().find(|m| &m.id == member_id) else {
        return false;
    };
    if !member_status_is_active(member.status) {
        return false;
    }
    // A streaming answer is itself the live view; do not add a second row.
    if state.has_active_message(&member.id) {
        return false;
    }
    if show_placeholder_header {
        out.extend(agent_header_line(
            state,
            &member.id,
            &member.display_name,
            member.backend,
            state.member_color(&member.id),
        ));
    }
    let reasoning = state.active_reasoning().get(member_id).map(String::as_str);
    let line_text = status_indicator::member_activity_text(
        member.status,
        reasoning,
        reasoning.is_some(),
        state.member_elapsed_secs(&member.id),
        status_indicator::spinner(),
    );
    for wrapped in markdown::wrap(&line_text, width.saturating_sub(2).max(1)) {
        out.push(Line::from(vec![
            Span::raw(" "),
            Span::raw(" "),
            Span::styled(wrapped, theme::muted_italic()),
        ]));
    }
    true
}

fn consecutive_work_run_end(items: &[ChatItem], order: &[usize], pos: usize) -> usize {
    if order.get(pos).is_some_and(|&i| is_agent_message(&items[i])) {
        let mut end = pos;
        while order
            .get(end + 1)
            .is_some_and(|&j| is_agent_message(&items[j]))
        {
            end += 1;
        }
        return end;
    }
    let member = match order.get(pos).map(|&i| &items[i]) {
        Some(ChatItem::Tool { member, .. } | ChatItem::Diff { member, .. }) => member,
        _ => return pos,
    };
    let mut end = pos;
    while let Some(&j) = order.get(end + 1) {
        match &items[j] {
            ChatItem::Tool { member: next, .. } | ChatItem::Diff { member: next, .. }
                if next == member =>
            {
                end += 1;
            }
            _ => break,
        }
    }
    end
}

/// Where a tool group sits in the transcript: the chat index of the first
/// item of its run (the key for per-group expansion) and the list that learns
/// which output row is the clickable title.
struct ToolGroupAnchor<'a> {
    first_index: usize,
    titles: &'a mut Vec<(usize, usize)>,
}

fn render_work_run(
    run: &[&ChatItem],
    anchor: ToolGroupAnchor<'_>,
    width: usize,
    state: &AppState,
    out: &mut Vec<Line<'static>>,
    show_sender_header: bool,
) {
    let all_tools: Vec<&ChatItem> = run
        .iter()
        .copied()
        .filter(|item| matches!(item, ChatItem::Tool { .. }))
        .collect();
    let diffs: Vec<&ChatItem> = run
        .iter()
        .copied()
        .filter(|item| matches!(item, ChatItem::Diff { .. }))
        .collect();
    let mut diff_member = None;
    let mut files = Vec::new();
    let mut diff_ok = true;
    if let Some((member, merged, ok)) = merge_diff_run(&diffs) {
        diff_member = Some(member);
        files = merged;
        diff_ok = ok;
    }
    let edit_tools: Vec<&ChatItem> = all_tools
        .iter()
        .copied()
        .filter(|item| {
            is_file_change_tool(item)
                && !matches!(
                    item,
                    ChatItem::Tool {
                        ok: Some(false),
                        ..
                    }
                )
        })
        .collect();
    for tool in &edit_tools {
        let ChatItem::Tool {
            name,
            summary,
            detail,
            ok,
            ..
        } = tool
        else {
            continue;
        };
        if let Some(file) = tool_display::file_change_from_edit_tool(name, summary, detail) {
            merge_file_change(&mut files, file);
        }
        diff_ok &= *ok != Some(false);
    }
    let tools: Vec<&ChatItem> = all_tools
        .iter()
        .copied()
        .filter(|item| {
            !is_file_change_tool(item)
                || matches!(
                    item,
                    ChatItem::Tool {
                        ok: Some(false),
                        ..
                    }
                )
        })
        .collect();
    if !tools.is_empty() {
        render_tool_group(&tools, Some(anchor), width, state, out, show_sender_header);
    }
    let member = diff_member.or_else(|| {
        edit_tools.iter().find_map(|item| match item {
            ChatItem::Tool { member, .. } => Some(member),
            _ => None,
        })
    });
    if let Some(member) = member.filter(|_| !files.is_empty()) {
        if tools.is_empty() && show_sender_header {
            let (display_name, backend) = state.member_meta(member);
            out.extend(agent_header_line(
                state,
                member,
                &display_name,
                backend,
                state.member_color(member),
            ));
        }
        for (index, files) in split_files_by_edit(&edit_tools, files)
            .into_iter()
            .enumerate()
        {
            if index > 0 {
                out.push(Line::from(vec![Span::raw(" "), Span::raw("")]));
            }
            render_file_changes(state, &files, diff_ok, width, out);
        }
    }
}

fn is_file_change_tool(item: &ChatItem) -> bool {
    matches!(item, ChatItem::Tool { name, .. } if tool_display::is_file_change_tool_name(name))
}

fn split_files_by_edit(
    tools: &[&ChatItem],
    files: Vec<FileChangeItem>,
) -> Vec<Vec<FileChangeItem>> {
    let mut assigned = vec![false; files.len()];
    let mut groups = Vec::new();
    for tool in tools {
        let ChatItem::Tool {
            name,
            summary,
            detail,
            ..
        } = tool
        else {
            continue;
        };
        if tool_display::tool_kind(name) != "Edit" {
            continue;
        }
        let target = tool_display::tool_target(name, summary, detail);
        if target.is_empty() {
            continue;
        }
        let edited = files
            .iter()
            .enumerate()
            .filter_map(|(index, file)| {
                if !assigned[index] && paths_match(&target, &file.path) {
                    assigned[index] = true;
                    Some(file.clone())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        if !edited.is_empty() {
            groups.push(edited);
        }
    }
    let unassigned = files
        .into_iter()
        .enumerate()
        .filter_map(|(index, file)| (!assigned[index]).then_some(file))
        .collect::<Vec<_>>();
    if !unassigned.is_empty() {
        groups.push(unassigned);
    }
    groups
}

fn paths_match(edit_target: &str, file_path: &str) -> bool {
    let edit_target = edit_target.trim();
    edit_target == file_path
        || file_path
            .strip_suffix(edit_target)
            .is_some_and(|prefix| prefix.ends_with('/'))
        || edit_target
            .strip_suffix(file_path)
            .is_some_and(|prefix| prefix.ends_with('/'))
}

fn merge_diff_run<'a>(diffs: &[&'a ChatItem]) -> Option<(&'a MemberId, Vec<FileChangeItem>, bool)> {
    let mut member = None;
    let mut files: Vec<FileChangeItem> = Vec::new();
    let mut ok = true;
    for item in diffs {
        let ChatItem::Diff {
            member: next,
            files: next_files,
            ok: next_ok,
        } = item
        else {
            continue;
        };
        member = Some(next);
        ok &= *next_ok;
        for file in next_files {
            if let Some(existing) = files.iter_mut().find(|seen| seen.path == file.path) {
                *existing = file.clone();
            } else {
                files.push(file.clone());
            }
        }
    }
    Some((member?, files, ok))
}

fn merge_file_change(files: &mut Vec<FileChangeItem>, incoming: FileChangeItem) {
    if let Some(existing) = files
        .iter_mut()
        .find(|existing| paths_match(&incoming.path, &existing.path))
    {
        if file_change_detail_len(&incoming) > file_change_detail_len(existing) {
            *existing = incoming;
        }
    } else {
        files.push(incoming);
    }
}

fn file_change_detail_len(file: &FileChangeItem) -> usize {
    file.old_text.as_ref().map_or(0, String::len)
        + file.new_text.as_ref().map_or(0, String::len)
        + file.patch.as_ref().map_or(0, String::len)
}

/// Distinct tool kinds named in a collapsed title before `…`.
const COLLAPSED_KINDS_SHOWN: usize = 4;

/// `3 read, 5 edit, 4 shell` in order of first appearance, capped with `…`.
fn tool_kind_counts(tools: &[&ChatItem]) -> String {
    let mut counts: Vec<(String, usize)> = Vec::new();
    for item in tools {
        let ChatItem::Tool { name, .. } = item else {
            continue;
        };
        let kind = tool_display::tool_kind(name).to_lowercase();
        match counts.iter_mut().find(|(known, _)| *known == kind) {
            Some((_, count)) => *count += 1,
            None => counts.push((kind, 1)),
        }
    }
    let more = counts.len() > COLLAPSED_KINDS_SHOWN;
    let mut parts: Vec<String> = counts
        .into_iter()
        .take(COLLAPSED_KINDS_SHOWN)
        .map(|(kind, count)| format!("{count} {kind}"))
        .collect();
    if more {
        parts.push("…".to_string());
    }
    parts.join(", ")
}

fn render_tool_group(
    tools: &[&ChatItem],
    anchor: Option<ToolGroupAnchor<'_>>,
    width: usize,
    state: &AppState,
    out: &mut Vec<Line<'static>>,
    show_sender_header: bool,
) {
    let Some(ChatItem::Tool { member, .. }) = tools.first().copied() else {
        return;
    };
    if show_sender_header {
        let (display_name, backend) = state.member_meta(member);
        out.extend(agent_header_line(
            state,
            member,
            &display_name,
            backend,
            state.member_color(member),
        ));
    }
    let failed = tools
        .iter()
        .filter(|item| {
            matches!(
                item,
                ChatItem::Tool {
                    ok: Some(false),
                    ..
                }
            )
        })
        .count();
    let all_failed = failed == tools.len();
    let (marker, title_style) = if all_failed {
        ("✕", theme::error_bold())
    } else {
        ("⚒", theme::accent_bold())
    };
    // Only a long run folds, and only when we know which group it is so a
    // click can find it again.
    let foldable = tools.len() > tool_display::TOOL_GROUP_COLLAPSE_ABOVE;
    let key = anchor.as_ref().filter(|_| foldable).map(|a| a.first_index);
    let collapsed = key.is_some_and(|key| !state.tool_group_expanded(key));
    let fold = match (key, collapsed) {
        (None, _) => "",
        (Some(_), true) => "▸ ",
        (Some(_), false) => "▾ ",
    };
    if let (Some(anchor), Some(key)) = (anchor, key) {
        anchor.titles.push((out.len(), key));
    }
    if !collapsed {
        out.push(Line::from(vec![Span::styled(
            format!("  {fold}{marker} tools"),
            title_style,
        )]));
        for item in tools {
            let ChatItem::Tool {
                name,
                summary,
                detail,
                ok,
                ..
            } = item
            else {
                continue;
            };
            render_tool_row(name, summary, detail, *ok, width, state, out);
        }
        return;
    }
    let failed_suffix = if failed > 0 {
        format!(" · {failed} failed")
    } else {
        String::new()
    };
    let room = width
        .saturating_sub(theme::display_width(&failed_suffix))
        .max(10);
    let head = format!(
        "  {fold}{marker} tools · {} calls · {}",
        tools.len(),
        tool_kind_counts(tools)
    );
    let mut spans = vec![Span::styled(clip_width(&head, room), title_style)];
    if !failed_suffix.is_empty() {
        spans.push(Span::styled(failed_suffix, theme::error_bold()));
    }
    out.push(Line::from(spans));
    // Keep progress visible: the newest call, while it is still running.
    if let Some(ChatItem::Tool {
        name,
        summary,
        detail,
        ok: None,
        ..
    }) = tools.last().copied()
    {
        render_tool_row(name, summary, detail, None, width, state, out);
    }
}

#[allow(clippy::too_many_arguments)] // The row maps directly from a tool event and render context.
fn render_tool_row(
    name: &str,
    summary: &str,
    detail: &str,
    ok: Option<bool>,
    width: usize,
    state: &AppState,
    out: &mut Vec<Line<'static>>,
) {
    let (prefix, color, text_style) = match ok {
        None => (
            status_indicator::spinner(),
            theme::warning_color(),
            theme::emphasis(),
        ),
        Some(true) => ("·", theme::success_color(), theme::text()),
        Some(false) => ("✕", theme::error_color(), theme::error()),
    };
    let kind = tool_display::tool_kind(name);
    let target = tool_display::tool_target(name, summary, detail);
    let kind_col = theme::pad_width(&kind, 6);
    let rest_width = width.saturating_sub(8).max(10);
    let mut spans = vec![
        Span::styled(format!("    {prefix} "), Style::default().fg(color)),
        Span::styled(kind_col, text_style),
    ];
    if !target.is_empty() {
        let shown = truncate_width(&target, rest_width.saturating_sub(8).max(4));
        spans.push(Span::raw("  "));
        spans.push(Span::styled(shown, theme::muted()));
    }
    out.push(Line::from(spans));
    if detail.trim().is_empty() {
        return;
    }
    let detail_style = theme::muted();
    let detail_width = width.saturating_sub(10).max(1);
    let expanded = state.tools_expanded();
    let failure = ok == Some(false);
    let (lines, clipped) = if failure {
        let body = tool_display::tool_error_body(detail);
        if body.is_empty() {
            (Vec::new(), false)
        } else if expanded {
            (markdown::wrap(&body, detail_width), false)
        } else {
            let (preview, clipped) = tool_display::compact_tool_error(&body);
            let wrapped = markdown::wrap(&preview, detail_width);
            let clipped = clipped || wrapped.len() > 6;
            (wrapped.into_iter().take(6).collect::<Vec<_>>(), clipped)
        }
    } else if expanded {
        let wrapped = markdown::wrap(&tool_display::tool_body(detail), detail_width);
        (wrapped, false)
    } else {
        (Vec::new(), false)
    };
    for (idx, line) in lines.into_iter().enumerate() {
        let line_style = if failure && is_tool_error_line(&line) {
            theme::error()
        } else {
            detail_style
        };
        out.push(Line::from(vec![
            Span::raw(if idx == 0 { "      ↳ " } else { "        " }),
            Span::styled(line, line_style),
        ]));
    }
    if clipped && !expanded {
        out.push(Line::from(vec![Span::styled(
            "        … Ctrl+O expand tool output",
            theme::muted_italic(),
        )]));
    }
}

fn is_agent_message(item: &ChatItem) -> bool {
    matches!(
        item,
        ChatItem::Route { .. } | ChatItem::Spawn { .. } | ChatItem::Close { .. }
    )
}

/// Agent-to-agent traffic, drawn like a tool group: a `⇄ agents` title and one
/// `·` row per message. Rows read relative to the viewed agent (`to` / `from`
/// / `spawn`); the interleaved view names both ends.
fn render_agent_messages(
    run: &[&ChatItem],
    width: usize,
    state: &AppState,
    out: &mut Vec<Line<'static>>,
) {
    out.push(Line::from(vec![Span::styled(
        "  ⇄ agents",
        theme::accent_bold(),
    )]));
    for item in run {
        render_agent_message_row(item, width, state, out);
    }
}

fn agent_name_span(state: &AppState, member: &MemberId) -> Span<'static> {
    Span::styled(
        state.member_display(member),
        state.member_color_bold(member),
    )
}

/// Recipients written as ids or display names, each in its member's colour.
fn push_recipient_names(state: &AppState, names: &[String], spans: &mut Vec<Span<'static>>) {
    for (index, name) in names.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(", ", theme::muted()));
        }
        let shown = state
            .members()
            .iter()
            .find(|member| {
                member.id.as_str().eq_ignore_ascii_case(name)
                    || member.display_name.eq_ignore_ascii_case(name)
            })
            .map(|member| member.display_name.clone())
            .or_else(|| {
                // A removed sub-agent is still named by its display name.
                state
                    .departed_members()
                    .iter()
                    .find(|gone| {
                        gone.id.as_str().eq_ignore_ascii_case(name)
                            || gone.display_name.eq_ignore_ascii_case(name)
                    })
                    .map(|gone| gone.display_name.clone())
            })
            .unwrap_or_else(|| name.clone());
        spans.push(Span::styled(
            shown,
            theme::bold(named_member_color(state, name)),
        ));
    }
}

fn render_agent_message_row(
    item: &ChatItem,
    width: usize,
    state: &AppState,
    out: &mut Vec<Line<'static>>,
) {
    let mut spans = vec![Span::styled(
        "    · ",
        Style::default().fg(theme::success_color()),
    )];
    let kind =
        |label: &str| Span::styled(format!("{} ", theme::pad_width(label, 6)), theme::text());
    let (preview, detail) = match item {
        ChatItem::Route { from, to, body } => {
            match state.focus() {
                Some(focus) if focus == from => {
                    spans.push(kind("to"));
                    push_recipient_names(state, to, &mut spans);
                }
                Some(_) => {
                    spans.push(kind("from"));
                    spans.push(agent_name_span(state, from));
                }
                None => {
                    spans.push(agent_name_span(state, from));
                    spans.push(Span::styled(" → ", theme::muted()));
                    push_recipient_names(state, to, &mut spans);
                }
            }
            (first_body_line(body).to_string(), Some(body.as_str()))
        }
        ChatItem::Spawn {
            parent,
            child,
            summary,
        } => {
            spans.push(kind("spawn"));
            match state.focus() {
                Some(focus) if focus == parent => spans.push(agent_name_span(state, child)),
                Some(_) => spans.push(agent_name_span(state, parent)),
                None => {
                    spans.push(agent_name_span(state, parent));
                    spans.push(Span::styled(" → ", theme::muted()));
                    spans.push(agent_name_span(state, child));
                }
            }
            // The live model label wins over the summary stored at spawn time,
            // which names the raw model id; a removed child keeps the summary.
            let label = state
                .member_model_label_by_id(child)
                .unwrap_or_else(|| summary.clone());
            (label, None)
        }
        ChatItem::Close {
            parent,
            child: _,
            name,
            by_user,
        } => {
            // The sub-agent is off the roster, so the row carries its name.
            let removed = || Span::styled(name.clone(), theme::muted());
            if *by_user {
                // No `parent → child` arrow: it would read as the parent
                // having closed it.
                spans.push(kind("remove"));
                spans.push(removed());
                ("by you".to_string(), None)
            } else {
                spans.push(kind("close"));
                match state.focus() {
                    Some(focus) if focus == parent => spans.push(removed()),
                    Some(_) => spans.push(agent_name_span(state, parent)),
                    None => {
                        spans.push(agent_name_span(state, parent));
                        spans.push(Span::styled(" → ", theme::muted()));
                        spans.push(removed());
                    }
                }
                (String::new(), None)
            }
        }
        _ => return,
    };
    let used: usize = spans
        .iter()
        .map(|span| theme::display_width(&span.content))
        .sum();
    if !preview.is_empty() {
        spans.push(Span::raw("  "));
        let room = width.saturating_sub(used + 2).max(4);
        spans.push(Span::styled(truncate_width(&preview, room), theme::muted()));
    }
    out.push(Line::from(spans));
    let Some(body) = detail.filter(|body| state.tools_expanded() && !body.trim().is_empty()) else {
        return;
    };
    let detail_width = width.saturating_sub(10).max(1);
    for (idx, line) in markdown::wrap(body, detail_width).into_iter().enumerate() {
        out.push(Line::from(vec![
            Span::raw(if idx == 0 { "      ↳ " } else { "        " }),
            Span::styled(line, theme::muted()),
        ]));
    }
}

fn is_tool_error_line(line: &str) -> bool {
    let line = line.trim_start().to_ascii_lowercase();
    line.starts_with("error:")
        || line.contains("test result: failed")
        || line.contains("panicked at")
        || line.starts_with("failures:")
        || line.contains("assertion")
        || line.starts_with("failure:")
}

fn render_file_changes(
    state: &AppState,
    files: &[FileChangeItem],
    ok: bool,
    width: usize,
    out: &mut Vec<Line<'static>>,
) {
    let (marker, title_style) = if ok {
        ("✎", theme::accent_bold())
    } else {
        ("✕", theme::error_bold())
    };
    if !state.diffs_expanded() {
        let count = files.len();
        let summary = if count == 1 {
            files
                .first()
                .map(|file| truncate_width(&file.path, width.saturating_sub(36).max(8)))
                .unwrap_or_else(|| "1 file".to_string())
        } else {
            format!("{count} files")
        };
        out.push(Line::from(vec![
            Span::styled(format!("  {marker} file changes"), title_style),
            Span::styled(
                format!(" · {summary} · Ctrl+G expand"),
                theme::muted_italic(),
            ),
        ]));
        return;
    }
    out.push(Line::from(vec![
        Span::styled(format!("  {marker} file changes"), title_style),
        Span::styled(" · Ctrl+G collapse", theme::muted_italic()),
    ]));
    for (index, file) in files.iter().enumerate() {
        if index > 0 {
            out.push(Line::from(vec![Span::raw(" "), Span::raw("")]));
        }
        let (sign, style) = match file.kind.as_str() {
            "add" => ("+", theme::diff_add_text()),
            "delete" => ("-", theme::diff_delete_text()),
            _ => ("Edit", theme::text()),
        };
        let shown = truncate_width(&file.path, width.saturating_sub(11).max(10));
        out.push(Line::from(vec![
            Span::styled(format!("    {sign} "), style),
            Span::styled(shown, theme::warning()),
        ]));
        let hunks = file
            .patch
            .as_deref()
            .map(file_diff::unified_hunks)
            .filter(|hunks| !hunks.is_empty())
            .unwrap_or_else(
                || match (file.old_text.as_deref(), file.new_text.as_deref()) {
                    (None, None) => Vec::new(),
                    (None, Some(new_text)) => file_diff::line_hunks("", new_text),
                    (Some(old_text), None) => file_diff::line_hunks(old_text, ""),
                    (Some(old_text), Some(new_text)) => file_diff::line_hunks(old_text, new_text),
                },
            );
        let max_hunks = 80;
        let clipped = hunks.len() > max_hunks;
        let line_number_width = hunks
            .iter()
            .filter_map(|line| line.number)
            .map(|number| number.to_string().len())
            .max()
            .unwrap_or(1);
        for line in hunks.into_iter().take(max_hunks) {
            let (mark, number_style, content_style) = match line.kind {
                '+' => ("+", theme::diff_add_text(), theme::diff_add()),
                '-' => ("-", theme::diff_delete_text(), theme::diff_delete()),
                _ => (" ", theme::muted(), theme::muted()),
            };
            let number = line
                .number
                .map(|number| format!("{number:>line_number_width$}"))
                .unwrap_or_else(|| " ".repeat(line_number_width));
            let shown = clip_width(
                &line.text,
                width.saturating_sub(line_number_width + 8).max(8),
            );
            out.push(Line::from(vec![
                Span::styled(format!("    {number} "), number_style),
                Span::styled(
                    pad_diff_content(&format!("{mark}{shown}"), width, line_number_width),
                    content_style,
                ),
            ]));
        }
        if clipped {
            out.push(Line::from(vec![Span::styled(
                "      …",
                theme::muted_italic(),
            )]));
        }
    }
}

fn pad_diff_content(text: &str, width: usize, line_number_width: usize) -> String {
    let target = width.saturating_sub(line_number_width + 6);
    let used = theme::display_width(text);
    if used >= target {
        return text.to_string();
    }
    format!("{text}{}", " ".repeat(target - used))
}

fn thinking_label(live: bool, stored: Option<u64>, live_secs: Option<u64>, lines: usize) -> String {
    let lines = format!("{lines} lines");
    if live {
        match live_secs {
            Some(secs) => format!(
                "thinking · {} · {lines}",
                status_indicator::fmt_elapsed_compact(secs)
            ),
            None => format!("thinking · {lines}"),
        }
    } else if let Some(secs) = stored {
        if secs == 0 {
            format!("thought · {lines}")
        } else {
            format!(
                "thinking for {} · {lines}",
                status_indicator::fmt_elapsed_compact(secs)
            )
        }
    } else {
        format!("thought · {lines}")
    }
}

fn item_member(item: &ChatItem) -> Option<&MemberId> {
    match item {
        ChatItem::Agent { member, .. }
        | ChatItem::Thinking { member, .. }
        | ChatItem::Tool { member, .. }
        | ChatItem::Diff { member, .. } => Some(member),
        ChatItem::Route { from, .. } => Some(from),
        ChatItem::Spawn { parent, .. } | ChatItem::Close { parent, .. } => Some(parent),
        ChatItem::Error { member, .. } => member.as_ref(),
        ChatItem::Verdict { member, .. } => Some(member),
        ChatItem::User { .. } | ChatItem::Notice { .. } | ChatItem::GoalMarker { .. } => None,
    }
}

/// First non-blank line of a message body.
fn first_body_line(body: &str) -> &str {
    body.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
}

/// Color for a route recipient written as an id or display name.
fn named_member_color(state: &AppState, name: &str) -> Color {
    state
        .members()
        .iter()
        .find(|member| {
            member.id.as_str().eq_ignore_ascii_case(name)
                || member.display_name.eq_ignore_ascii_case(name)
        })
        .map_or_else(theme::muted_color, |member| state.member_color(&member.id))
}

/// Empty closed agent cells stay in the timeline after a tool split but
/// must not count as a previous speaker, or the next tool loses its header.
fn item_renders(item: &ChatItem, state: &AppState) -> bool {
    // Rows hidden from the focused window never lead the next row.
    if focus_row(state, item) != FocusRow::Full {
        return false;
    }
    match item {
        ChatItem::Agent { text, member, .. } => {
            !text.is_empty() || state.has_active_message(member)
        }
        ChatItem::Thinking { text, .. } => !text.is_empty(),
        _ => true,
    }
}

/// Put the `/find` marker `»` in column 0 of `line`, replacing a leading blank
/// column (every transcript row starts with the indent) instead of shifting
/// the row.
fn mark_find_match(line: &mut Line<'static>) {
    let marker = Span::styled("»", theme::selection());
    match line.spans.first_mut() {
        Some(first) if first.content.starts_with(' ') => {
            let rest = first.content[1..].to_string();
            first.content = rest.into();
            line.spans.insert(0, marker);
        }
        _ => line.spans.insert(0, marker),
    }
}

fn line_is_blank(line: &Line<'_>) -> bool {
    line.spans.iter().all(|span| span.content.trim().is_empty())
}

fn quick_start_lines(state: &AppState) -> Vec<Line<'static>> {
    let mut lines = Vec::new();

    if state.members().is_empty() {
        lines.push(Line::styled("  Team is loading...", theme::muted()));
        return lines;
    }

    if let Some(model) = state.welcome_model_summary() {
        lines.push(Line::from(vec![
            Span::styled("  Model: ", theme::muted()),
            Span::styled(model, theme::text()),
        ]));
    }
    lines.push(Line::raw(""));

    let talk_to = match state.focus() {
        Some(_) => format!("talk to {}", state.focus_display()),
        None => "talk to the default agent".to_string(),
    };
    let examples = [
        ("<message>".to_string(), talk_to),
        ("←".to_string(), "switch agent".to_string()),
        ("/model".to_string(), "switch model".to_string()),
        ("/help".to_string(), "all commands".to_string()),
    ];
    for (i, (cmd, desc)) in examples.iter().enumerate() {
        lines.push(Line::from(vec![
            Span::styled(if i == 0 { "  Try:  " } else { "        " }, theme::muted()),
            Span::styled(format!("{cmd:<12}"), theme::accent_bold()),
            Span::styled(desc.clone(), theme::muted()),
        ]));
    }
    lines
}

/// The speaker label of an agent block: a dim `Main · GPT-6.1-Sol · high`
/// line (the member's model label, as on the header chip), only in the All
/// view where several agents interleave. A member that no longer exists
/// (replayed history of a removed sub-agent) falls back to its backend name.
/// One agent's window has no label at all; the whole window is that agent.
fn agent_header_line(
    state: &AppState,
    member: &MemberId,
    display_name: &str,
    backend: crate::domain::team::BackendKind,
    color: Color,
) -> Option<Line<'static>> {
    state.focus().is_none().then(|| {
        Line::from(vec![
            Span::raw(INDENT_TEXT),
            Span::styled(
                display_name.to_string(),
                Style::default().fg(color).add_modifier(Modifier::DIM),
            ),
            Span::styled(
                format!(
                    " · {}",
                    state
                        .member_model_label_by_id(member)
                        .unwrap_or_else(|| backend.as_str().to_string())
                ),
                theme::muted().add_modifier(Modifier::DIM),
            ),
        ])
    })
}

/// A user message: a full-width band of subtle background with a blank band
/// row above and below and the text indented like every other row. No name
/// header; the All view alone shows who it went to, dim and right-aligned on
/// the top padding row.
fn render_user_band(
    state: &AppState,
    body: &str,
    targets: &[MemberId],
    width: usize,
    full_width: usize,
    out: &mut Vec<Line<'static>>,
) {
    let full_width = full_width.max(width);
    let bg = theme::user_band_bg();
    let band = Style::default().bg(bg);
    // Rows are always padded out to the full terminal width.
    let pad_row = |mut spans: Vec<Span<'static>>| {
        let used: usize = spans
            .iter()
            .map(|span| theme::display_width(span.content.as_ref()))
            .sum();
        spans.push(Span::styled(
            " ".repeat(full_width.saturating_sub(used)),
            band,
        ));
        Line::from(spans)
    };

    // Top padding row, carrying the recipient in the All view (right-aligned
    // at the text's right edge).
    let mut top = Vec::new();
    if state.focus().is_none() && !targets.is_empty() {
        let mut names = Vec::new();
        push_target_names(state, targets, &mut names);
        let label = format!(
            "to {}",
            names
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        );
        let label_width = theme::display_width(&label);
        // Two columns of margin on the right, like the text's left indent.
        if label_width + 2 * INDENT_TEXT.len() <= width {
            let pad = width - label_width - INDENT_TEXT.len();
            top = vec![
                Span::styled(" ".repeat(pad), band),
                Span::styled(
                    label,
                    theme::muted().add_modifier(Modifier::DIM).patch(band),
                ),
            ];
        }
    }
    out.push(pad_row(top));

    // The body keeps its markdown structure and wrapping; plain text takes the
    // band's text colour, while spans that already carry their own colour
    // (inline code, links, headings, code blocks) keep it so they stay
    // visibly distinct.
    let body = state.displayed_user_body(body, targets);
    let wrap_width = width.saturating_sub(2 * INDENT_TEXT.len()).max(1);
    let rendered = markdown::render(&body, wrap_width);
    if rendered.is_empty() {
        out.push(pad_row(vec![Span::styled(INDENT_TEXT, band)]));
    }
    for line in rendered {
        let mut spans = vec![Span::styled(INDENT_TEXT, band)];
        spans.extend(line.spans.into_iter().map(|span| {
            let mut style = span.style.bg(bg);
            if style.fg.is_none() {
                style = style.fg(theme::emphasis_color());
            }
            Span::styled(span.content, style)
        }));
        out.push(pad_row(spans));
    }
    out.push(pad_row(Vec::new()));
}

/// Append the recipients of a prompt: `all` when it reached the whole roster,
/// else each member's name in their color.
fn push_target_names(state: &AppState, targets: &[MemberId], spans: &mut Vec<Span<'static>>) {
    let roster = state.members();
    if roster.len() > 1
        && targets.len() == roster.len()
        && roster
            .iter()
            .all(|member| targets.iter().any(|target| target == &member.id))
    {
        spans.push(Span::styled("all", theme::emphasis()));
        return;
    }
    for (index, target) in targets.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(", ", theme::muted()));
        }
        let (name, _) = state.member_meta(target);
        spans.push(Span::styled(name, state.member_color_bold(target)));
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ChatSender {
    User,
    Agent(MemberId),
}

fn item_sender(item: &ChatItem) -> Option<ChatSender> {
    match item {
        ChatItem::User { .. } => Some(ChatSender::User),
        ChatItem::Agent { member, .. } => Some(ChatSender::Agent(member.clone())),
        // Tools, diffs, relays, and member-attributed failures belong to the
        // same visible speaker block as the response they lead to. Treating
        // them as anonymous made a tool appear before its member title.
        _ => item_member(item).cloned().map(ChatSender::Agent),
    }
}

#[cfg(test)]
fn render_item(
    item: &ChatItem,
    width: usize,
    state: &AppState,
    out: &mut Vec<Line<'static>>,
    show_sender_header: bool,
) {
    render_item_wide(item, width, width, state, out, show_sender_header);
}

/// `width` is the (capped) text width; `full_width` is the terminal width the
/// user band's background spans.
fn render_item_wide(
    item: &ChatItem,
    width: usize,
    full_width: usize,
    state: &AppState,
    out: &mut Vec<Line<'static>>,
    show_sender_header: bool,
) {
    if show_sender_header
        && !matches!(item, ChatItem::User { .. } | ChatItem::Agent { .. })
        && let Some(member) = item_member(item)
    {
        let (display_name, backend) = state.member_meta(member);
        out.extend(agent_header_line(
            state,
            member,
            &display_name,
            backend,
            state.member_color(member),
        ));
    }
    match item {
        ChatItem::User { body, targets, .. } => {
            render_user_band(state, body, targets, width, full_width, out);
        }
        ChatItem::Agent {
            member,
            display_name,
            backend,
            text,
            ..
        } => {
            if text.is_empty() && !state.has_active_message(member) {
                return;
            }
            if show_sender_header {
                out.extend(agent_header_line(
                    state,
                    member,
                    display_name,
                    *backend,
                    state.member_color(member),
                ));
            }
            for line in markdown::render(text, content_width(width)) {
                let mut spans = vec![Span::raw(INDENT_TEXT)];
                spans.extend(line.spans);
                out.push(Line::from(spans));
            }
        }
        ChatItem::Thinking {
            member,
            text,
            elapsed_secs,
            ..
        } => {
            let live = elapsed_secs.is_none() && state.is_thinking_live(member);
            if text.is_empty() && !live {
                return;
            }
            let lines = text
                .lines()
                .filter(|line| !line.trim().is_empty())
                .count()
                .max(1);
            let label =
                thinking_label(live, *elapsed_secs, state.thinking_live_secs(member), lines);
            let label = if live {
                format!("{} {label}", status_indicator::spinner())
            } else {
                format!("✻ {label}")
            };
            if !state.thinking_expanded() {
                out.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(format!("{label} · Ctrl+T expand"), theme::muted_italic()),
                ]));
                return;
            }
            out.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(format!("{label} · Ctrl+T collapse"), theme::muted_italic()),
            ]));
            for line in markdown::wrap(text.trim(), width.saturating_sub(5).max(1)) {
                out.push(Line::from(vec![
                    Span::raw("    "),
                    Span::styled(line, theme::muted_italic()),
                ]));
            }
        }
        ChatItem::Tool { .. } => {
            render_tool_group(&[item], None, width, state, out, false);
        }
        ChatItem::Diff { files, ok, .. } => {
            render_file_changes(state, files, *ok, width, out);
        }
        ChatItem::Route { .. } | ChatItem::Spawn { .. } | ChatItem::Close { .. } => {
            render_agent_messages(&[item], width, state, out);
        }
        ChatItem::Notice { text, .. } => {
            // Wrapped lines hang under the text, not under the bullet.
            let room = width.saturating_sub(4 + 1).max(1);
            for (index, line) in markdown::wrap(text, room).into_iter().enumerate() {
                let lead = if index == 0 { "  • " } else { "    " };
                out.push(Line::from(Span::styled(
                    format!("{lead}{line}"),
                    theme::notice(),
                )));
            }
        }
        ChatItem::GoalMarker { turn, .. } => {
            // The goal loop's own prompt is not a user message: one dim row.
            out.push(Line::from(Span::styled(
                format!("  {}", goal_surface::marker_text(*turn)),
                theme::dim(),
            )));
        }
        ChatItem::Error { member, message } => {
            if let Some(member) = member {
                let text = format!("✕ {member}: {message}");
                for line in markdown::wrap(&text, width.saturating_sub(3).max(1)) {
                    out.push(Line::from(vec![Span::styled(
                        format!("  {line}"),
                        theme::error(),
                    )]));
                }
            } else {
                push_wrapped(&format!("  ✕ {message}"), width, "", theme::error(), out);
            }
        }
        ChatItem::Verdict {
            approve, summary, ..
        } => {
            if *approve {
                out.push(Line::from(Span::styled(
                    "  ✓ review approved",
                    theme::success_bold(),
                )));
            } else {
                out.push(Line::from(Span::styled(
                    "  ✗ changes requested",
                    theme::warning_bold(),
                )));
            }
            let summary = summary.trim();
            if !summary.is_empty() {
                push_wrapped(summary, width, "    ", theme::text(), out);
            }
        }
    }
}

fn push_wrapped(
    text: &str,
    width: usize,
    indent: &str,
    style: Style,
    out: &mut Vec<Line<'static>>,
) {
    let wrap_width = width.saturating_sub(indent.len()).max(1);
    for line in markdown::wrap(text, wrap_width) {
        out.push(Line::from(Span::styled(format!("{indent}{line}"), style)));
    }
}

fn approval_card_height(state: &AppState, width: u16) -> u16 {
    let Some(pending) = state.selected_pending_approval() else {
        return 0;
    };
    let inner = width.saturating_sub(4).max(8) as usize;
    let body_rows = markdown::wrap(&pending.body, inner).len().clamp(1, 4);
    (4 + body_rows) as u16
}

fn render_approval_card(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    if area.height == 0 {
        return;
    }
    let Some(pending) = state.selected_pending_approval() else {
        return;
    };
    let count = state.pending_approvals().len();
    let index = state.selected_approval_index() + 1;
    let title = if count > 1 {
        format!(" Approval {index}/{count} ")
    } else {
        " Approval ".to_string()
    };
    let block = Block::default()
        .title(Span::styled(title, theme::warning_bold()))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::warning_color()))
        .padding(Padding::horizontal(1));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let wrap = inner.width as usize;
    let who = pending
        .member
        .as_ref()
        .map(|member| format!("{member} · "))
        .unwrap_or_default();
    let mut lines = vec![Line::from(vec![
        Span::styled(who, theme::emphasis()),
        Span::styled(pending.action.clone(), theme::warning_bold()),
        Span::styled(" is waiting", theme::muted()),
    ])];
    for (idx, line) in markdown::wrap(&pending.body, wrap)
        .into_iter()
        .take(4)
        .enumerate()
    {
        let style = if idx == 0 {
            theme::text()
        } else {
            theme::muted()
        };
        lines.push(Line::from(Span::styled(line, style)));
    }
    let hint = if count > 1 {
        "y agree   n deny   ← → switch"
    } else {
        "y agree   n deny"
    };
    if (lines.len() as u16) < inner.height {
        lines.push(Line::from(Span::styled(hint, theme::muted())));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Where the composer text goes: between the band's top and bottom padding
/// rows, full width.
fn composer_text_area(area: Rect) -> Rect {
    Rect::new(
        area.x,
        area.y
            .saturating_add(1)
            .min(area.y.saturating_add(area.height)),
        area.width,
        area.height.saturating_sub(2),
    )
}

/// The composer: the same subtle full-width band as a user message, one
/// padding row above and below the input, text on the transcript's 2-column indent. A
/// dim `>` prompt marks the first row.
fn render_composer(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let band = Style::default().bg(theme::user_band_bg());
    frame.render_widget(Block::default().style(band), area);
    let prompt_color = if state.paused_routes() > 0 {
        theme::warning_color()
    } else {
        theme::muted_color()
    };

    let inner = composer_text_area(area);
    let rows = inner.height as usize;
    if rows == 0 {
        return;
    }
    let avail = (inner.width as usize).saturating_sub(2); // "> " / "  " gutter

    let mut out_lines: Vec<Line> = Vec::new();
    let mut cursor_screen: Option<(u16, u16)> = None;
    let text_origin = 0usize;

    // Visual lines with wrapping so long input is fully visible (no horizontal
    // clipping). The cursor maps directly to a screen cell.
    let wrapped = state.composer().visual_layout(avail);
    let (cursor_row, cursor_col) = (wrapped.cursor_row, wrapped.cursor_col);
    let selection = state.composer().selection_range();
    let text_rows = rows.saturating_sub(text_origin);

    // Vertical scroll so the cursor's visual line stays visible.
    let top = if cursor_row >= text_rows {
        cursor_row - text_rows + 1
    } else {
        0
    };

    for (offset, row) in (top..top + text_rows).enumerate() {
        let Some(visual_row) = wrapped.rows.get(row) else {
            break;
        };
        let line = visual_row.text.as_str();
        let prefix = if row == 0 { "> " } else { "  " };
        let start_char = visual_row.start;
        let cursor_width = if row == cursor_row { cursor_col } else { 0 };
        let mut spans = vec![Span::styled(
            prefix.to_string(),
            Style::default().fg(prompt_color).bg(theme::user_band_bg()),
        )];
        spans.extend(style_composer_line(line, start_char, selection));
        out_lines.push(Line::from(spans));
        if row == cursor_row {
            cursor_screen = Some((
                inner.x + 2 + cursor_width as u16,
                inner.y + (text_origin + offset) as u16,
            ));
        }
    }
    frame.render_widget(Paragraph::new(out_lines).style(band), inner);

    if state.drawer().is_none()
        && !state.model_picker_open()
        && let Some((col, row)) = cursor_screen
    {
        frame.set_cursor_position((col, row));
    }
}

fn style_composer_line(
    line: &str,
    start_char: usize,
    selection: Option<(usize, usize)>,
) -> Vec<Span<'static>> {
    let Some((sel_start, sel_end)) = selection else {
        return vec![Span::raw(line.to_string())];
    };
    let mut spans = Vec::new();
    let mut index = start_char;
    let mut plain = String::new();
    let mut selected = String::new();
    for grapheme in unicode_segmentation::UnicodeSegmentation::graphemes(line, true) {
        let next = index + grapheme.chars().count();
        if index >= sel_start && index < sel_end {
            if !plain.is_empty() {
                spans.push(Span::raw(std::mem::take(&mut plain)));
            }
            selected.push_str(grapheme);
        } else {
            if !selected.is_empty() {
                spans.push(Span::styled(
                    std::mem::take(&mut selected),
                    theme::chat_selection(),
                ));
            }
            plain.push_str(grapheme);
        }
        index = next;
    }
    if !plain.is_empty() {
        spans.push(Span::raw(plain));
    }
    if !selected.is_empty() {
        spans.push(Span::styled(selected, theme::chat_selection()));
    }
    if spans.is_empty() {
        spans.push(Span::raw(String::new()));
    }
    spans
}

#[cfg(test)]
#[path = "chat_view_tests/mod.rs"]
mod tests;
