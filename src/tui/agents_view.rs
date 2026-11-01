//! The Agents list (← on an empty composer, `/agents`): a full-screen view that
//! replaces the chat while open, modelled on prime-agent's agents view.
//!
//! Top to bottom: a counts line, a search prompt, a column legend, the
//! "All messages" entry, then `Running` / `Idle` sections of agents (sub-agents
//! stay under their parent), an `Inactive` section of saved chats (newest
//! first; Enter resumes one), and a hint line. Only the selected agent row
//! shows a preview of the agent's last message. A typed query replaces the
//! sections with one flat list ranked by relevance.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::domain::config::MAIN_MEMBER_ID;
use crate::domain::event::{ChatItem, ConversationSummary, MemberStatus};
use crate::tui::app_state::{AgentChoice, AgentRow, AppState, MemberView, member_status_is_active};
use crate::tui::header;
use crate::tui::markdown;
use crate::tui::status_indicator::{fmt_elapsed_compact, pulse};
use crate::tui::theme;

/// Columns left of every row's content.
const MARGIN: usize = 2;
/// Columns a sub-agent is indented per nesting level, inside the name column.
const INDENT_PER_DEPTH: usize = 4;
/// Columns between the name, model and activity columns.
const COLUMN_GAP: usize = 2;
/// Widest the name column grows (indent, icon and title together), so a long
/// first message cannot squeeze the Model and Activity columns.
const NAME_COLUMN_MAX_CELLS: usize = 28;
/// Most lines of the selected agent's last message shown under its row.
const MAX_PREVIEW_LINES: usize = 3;
/// Rows above the list: counts, blank, search, blank, legend, blank.
const HEADER_ROWS: usize = 6;
const PLACEHOLDER: &str = "Search agents";
const NO_MATCH: &str = "No agents match your search.";
const HINTS_HEAD: &str = "↑/↓ navigate   Home/End first/last   Enter/→ open";
const HINTS_TAIL: &str = "Ctrl+N new   Esc close";
const HINT_RENAME: &str = "Ctrl+R rename";
const HINT_DELETE: &str = "Ctrl+X delete";
const HINT_ARMED: &str = "Press Ctrl+X again to delete";
const HINT_RENAMING: &str = "Enter save   Esc cancel";
/// The rename editor's prompt, in place of the search prompt's `> `.
const RENAME_PROMPT: &str = "> Rename: ";
/// The hint line with no row-specific keys.
pub(crate) const HINTS: &str =
    "↑/↓ navigate   Home/End first/last   Enter/→ open   Ctrl+N new   Esc close";

/// The hint line for the current state: the rename editor's, the armed
/// deletion's, or the navigation keys plus the rename and delete keys the
/// selected row allows.
fn hint_line(state: &AppState) -> String {
    if state.agents_rename().is_some() {
        return HINT_RENAMING.to_string();
    }
    if state.agents_delete_armed() {
        return HINT_ARMED.to_string();
    }
    let mut parts = vec![HINTS_HEAD];
    if state.agents_can_rename() {
        parts.push(HINT_RENAME);
    }
    if state.agents_can_delete() {
        parts.push(HINT_DELETE);
    }
    parts.push(HINTS_TAIL);
    parts.join("   ")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Section {
    Running,
    Idle,
    /// Saved chats, not live members.
    Inactive,
}

impl Section {
    fn title(self) -> &'static str {
        match self {
            Self::Running => "Running",
            Self::Idle => "Idle",
            Self::Inactive => "Inactive",
        }
    }
}

/// A top-level member and the sub-agents listed under it.
struct Group {
    root: usize,
    children: Vec<usize>,
}

/// Top-level members in roster order, each with its sub-agents. A member whose
/// parent is itself a sub-agent (or is gone) counts as top-level.
fn groups(members: &[MemberView]) -> Vec<Group> {
    let top_level = |member: &MemberView| {
        member.parent.as_ref().is_none_or(|parent| {
            !members
                .iter()
                .any(|other| other.id == *parent && other.parent.is_none())
        })
    };
    members
        .iter()
        .enumerate()
        .filter(|(_, member)| top_level(member))
        .map(|(root, member)| Group {
            root,
            children: members
                .iter()
                .enumerate()
                .filter(|(_, child)| child.parent.as_ref() == Some(&member.id))
                .map(|(child, _)| child)
                .collect(),
        })
        .collect()
}

/// A group is running while its top-level member or any sub-agent is active.
fn group_section(members: &[MemberView], group: &Group) -> Section {
    let active = |index: usize| member_status_is_active(members[index].status);
    if active(group.root) || group.children.iter().any(|&child| active(child)) {
        Section::Running
    } else {
        Section::Idle
    }
}

/// Top-level members per section, for the counts line and the headings.
fn section_counts(members: &[MemberView]) -> (usize, usize) {
    groups(members)
        .iter()
        .fold((0, 0), |(running, idle), group| {
            match group_section(members, group) {
                Section::Running => (running + 1, idle),
                Section::Idle | Section::Inactive => (running, idle + 1),
            }
        })
}

/// Rows in display order. Without a query: "All", then each section's groups,
/// then the saved chats. With one: only the matches, best first, flat.
pub(crate) fn rows(state: &AppState) -> Vec<AgentRow> {
    let members = state.members();
    let query = state.agents_query().trim();
    if query.is_empty() {
        let groups = groups(members);
        let mut rows = vec![AgentRow::All];
        for section in [Section::Running, Section::Idle] {
            for group in groups
                .iter()
                .filter(|group| group_section(members, group) == section)
            {
                rows.push(AgentRow::Member {
                    index: group.root,
                    depth: 0,
                });
                rows.extend(
                    group
                        .children
                        .iter()
                        .map(|&index| AgentRow::Member { index, depth: 1 }),
                );
            }
        }
        rows.extend((0..state.saved_chats().len()).map(|index| AgentRow::Saved { index }));
        return rows;
    }
    let mut scored: Vec<(i64, usize, AgentRow)> = Vec::new();
    if let Some(score) = match_score(query, "all messages", "all messages") {
        scored.push((score, 0, AgentRow::All));
    }
    for (index, member) in members.iter().enumerate() {
        let model = state.member_model_summary(member).unwrap_or_default();
        let title = row_title(state, member);
        let haystack = format!(
            "{} {} {} {} {}",
            title,
            member.display_name,
            member.backend.as_str(),
            model,
            member.model.as_deref().unwrap_or_default()
        );
        if let Some(score) = match_score(query, &title, &haystack) {
            scored.push((score, index + 1, AgentRow::Member { index, depth: 0 }));
        }
    }
    for (index, chat) in state.saved_chats().iter().enumerate() {
        let title = saved_title(state, chat);
        let haystack = format!(
            "{title} {} {}",
            state.saved_chat_model(chat),
            chat.main_model.as_deref().unwrap_or_default()
        );
        if let Some(score) = match_score(query, &title, &haystack) {
            scored.push((score, members.len() + 1 + index, AgentRow::Saved { index }));
        }
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.into_iter().map(|(_, _, row)| row).collect()
}

/// Relevance of `query` for one entry, or `None` when it does not match.
/// Every whitespace-separated word must match: a substring of the name scores
/// highest (a prefix best), then a substring of the other fields, then a
/// scattered in-order match of its letters.
fn match_score(query: &str, name: &str, haystack: &str) -> Option<i64> {
    let name = name.to_lowercase();
    let haystack = haystack.to_lowercase();
    let mut total = 0;
    for word in query.split_whitespace() {
        let word = word.to_lowercase();
        total += if let Some(position) = name.find(&word) {
            if position == 0 {
                1000
            } else {
                900 - position.min(100) as i64
            }
        } else if let Some(position) = haystack.find(&word) {
            600 - position.min(100) as i64
        } else {
            300 - scattered_gap(&word, &haystack)?.min(200)
        };
    }
    Some(total)
}

/// Extra characters between the first and last letter of `word` when its
/// letters occur in order in `haystack`; `None` when they do not.
fn scattered_gap(word: &str, haystack: &str) -> Option<i64> {
    let mut letters = word.chars();
    let mut wanted = letters.next()?;
    let mut first = None;
    for (position, ch) in haystack.chars().enumerate() {
        if ch != wanted {
            continue;
        }
        let first = *first.get_or_insert(position);
        match letters.next() {
            Some(next) => wanted = next,
            None => {
                let span = position - first + 1;
                return Some((span - word.chars().count()) as i64);
            }
        }
    }
    None
}

/// The section a member row belongs to: its group's.
fn row_section(members: &[MemberView], index: usize) -> Section {
    let root = members[index]
        .parent
        .as_ref()
        .and_then(|parent| {
            members
                .iter()
                .position(|other| other.id == *parent && other.parent.is_none())
        })
        .unwrap_or(index);
    let children = members
        .iter()
        .enumerate()
        .filter(|(_, child)| child.parent.as_ref() == Some(&members[root].id))
        .map(|(child, _)| child)
        .collect();
    group_section(members, &Group { root, children })
}

/// Everything one painted frame holds.
pub(crate) struct Screen {
    pub(crate) lines: Vec<Line<'static>>,
    /// Where the search caret goes (column, row).
    pub(crate) cursor: Option<(u16, u16)>,
    /// Selectable entries and the screen row each landed on.
    pub(crate) clicks: Vec<(u16, AgentChoice)>,
    /// Rows the list itself had room for.
    pub(crate) list_rows: usize,
}

/// One entry of the scrollable list body.
enum Item {
    Blank,
    Heading(String),
    Note(String),
    /// A selectable row: an index into the list's rows.
    Row(usize),
    Preview(Line<'static>),
}

/// Cells of one agent row.
struct Cells {
    depth: usize,
    icon: &'static str,
    icon_style: Style,
    name: String,
    model: String,
    activity: String,
    activity_style: Style,
}

fn icon_for(member: &MemberView) -> (&'static str, Style) {
    if member.status == MemberStatus::NeedsApproval {
        // Waiting on the user: the header's yellow `!`, not the running pulse.
        return (
            header::status_glyph(member.status),
            theme::member_dot_style(member.status),
        );
    }
    if member_status_is_active(member.status) {
        (pulse(), theme::dot_lit())
    } else if member.status == MemberStatus::Failed {
        ("•", Style::default().fg(theme::error_color()))
    } else {
        ("•", theme::dot_dark())
    }
}

/// Seconds as a compact age: `45s`, `12m`, `3h`, `2d`. Shared by live member
/// rows and saved-chat rows so both columns read the same.
fn fmt_age(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86_399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

fn activity_for(state: &AppState, member: &MemberView) -> (String, Style) {
    let timed = |word: &str| match state.member_elapsed_secs(&member.id) {
        Some(secs) => format!("{word} {}", fmt_elapsed_compact(secs)),
        None => word.to_string(),
    };
    let (mut text, style) = match member.status {
        MemberStatus::Running => (timed("working"), theme::dim()),
        MemberStatus::Queued => (timed("queued"), theme::dim()),
        MemberStatus::Waiting => (timed("waiting"), theme::dim()),
        MemberStatus::NeedsApproval => (timed("needs approval"), theme::warning()),
        MemberStatus::Failed => ("failed".to_string(), theme::error()),
        MemberStatus::Idle => (
            state
                .member_reply_age_secs(&member.id)
                .map(fmt_age)
                .unwrap_or_default(),
            theme::dim(),
        ),
    };
    let queued = state.queued_prompts_for(&member.id).len();
    if queued > 0 {
        if !text.is_empty() {
            text.push_str(" · ");
        }
        text.push_str(&format!("{queued} queued"));
    }
    (text, style)
}

/// `text` on one line, its whitespace collapsed.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The text that titles a member's row. Sub-agents and members added in /team
/// keep their configured names. The main member is titled by the chat itself,
/// as prime-agent titles a session: the name given to the chat, else the first
/// prompt sent to it with its whitespace collapsed, else the workspace
/// folder's name.
fn row_title(state: &AppState, member: &MemberView) -> String {
    if member.parent.is_some() || member.id.as_str() != MAIN_MEMBER_ID {
        return member.display_name.clone();
    }
    if let Some(name) = state
        .chat_name()
        .map(one_line)
        .filter(|name| !name.is_empty())
    {
        return name;
    }
    let first_prompt = state.chat().iter().find_map(|item| match item {
        // Rows saved by older versions record no targets; they stay visible.
        ChatItem::User { body, targets, .. }
            if targets.is_empty() || targets.contains(&member.id) =>
        {
            let shown = state.displayed_user_body(body, targets);
            let collapsed = shown.split_whitespace().collect::<Vec<_>>().join(" ");
            (!collapsed.is_empty()).then_some(collapsed)
        }
        _ => None,
    });
    first_prompt
        .or_else(|| folder_name(state.workspace()))
        .or_else(|| folder_name(&member.cwd))
        .unwrap_or_else(|| member.display_name.clone())
}

/// The last component of a folder path, if it has one.
fn folder_name(path: &str) -> Option<String> {
    std::path::Path::new(path.trim())
        .file_name()
        .map(|name| name.to_string_lossy().trim().to_string())
        .filter(|name| !name.is_empty())
}

/// A saved chat's title: its name, else its first user message on one line.
fn saved_title(state: &AppState, chat: &ConversationSummary) -> String {
    if let Some(name) = chat
        .name
        .as_deref()
        .map(one_line)
        .filter(|name| !name.is_empty())
    {
        return name;
    }
    let shown = state.displayed_user_body(&chat.preview, &[]);
    let collapsed = shown.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        format!("chat {}", chat.id)
    } else {
        collapsed
    }
}

fn saved_cells_for(state: &AppState, index: usize) -> Cells {
    let chat = &state.saved_chats()[index];
    Cells {
        depth: 0,
        icon: "•",
        icon_style: theme::dot_dark(),
        name: saved_title(state, chat),
        model: state.saved_chat_model(chat),
        activity: relative_age(&chat.updated_at, state.clock_unix_secs()),
        activity_style: theme::dim(),
    }
}

/// Seconds since the Unix epoch for a stored UTC timestamp such as
/// `2025-01-01 13:05:09` (SQLite's `CURRENT_TIMESTAMP`; a `T` separator, a
/// fraction and a trailing `Z` are accepted too).
fn parse_utc_secs(stamp: &str) -> Option<i64> {
    let stamp = stamp.trim().trim_end_matches('Z');
    let (date, time) = stamp.split_once([' ', 'T'])?;
    let mut date = date.split('-').map(|part| part.parse::<i64>().ok());
    let (year, month, day) = (date.next()??, date.next()??, date.next()??);
    let time = time.split('.').next()?;
    let mut time = time.split(':').map(|part| part.parse::<i64>().ok());
    let (hour, minute, second) = (time.next()??, time.next()??, time.next().flatten()?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    // Days since 1970-01-01 (Howard Hinnant's days-from-civil).
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let shifted_month = (month + 9) % 12;
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    Some(days * 86_400 + hour * 3600 + minute * 60 + second)
}

/// How long before `now` (Unix seconds) the UTC timestamp `stamp` was:
/// `45s`, `12m`, `3h`, `2d`. Empty when `stamp` cannot be read; a timestamp
/// ahead of `now` counts as `0s`.
pub(crate) fn relative_age(stamp: &str, now: i64) -> String {
    let Some(then) = parse_utc_secs(stamp) else {
        return String::new();
    };
    fmt_age((now - then).max(0) as u64)
}

fn cells_for(state: &AppState, index: usize, depth: usize) -> Cells {
    let member = &state.members()[index];
    let (icon, icon_style) = icon_for(member);
    let mut model = member.backend.as_str().to_string();
    if let Some(summary) = state.member_model_summary(member) {
        model.push_str(" · ");
        model.push_str(&summary);
    }
    let (activity, activity_style) = activity_for(state, member);
    Cells {
        depth,
        icon,
        icon_style,
        name: row_title(state, member),
        model,
        activity,
        activity_style,
    }
}

impl Cells {
    /// Columns the name cell wants: indent, icon, space, name.
    fn name_width(&self) -> usize {
        self.depth * INDENT_PER_DEPTH + 2 + theme::display_width(&self.name)
    }
}

/// Column widths: name, model, activity.
fn column_widths(cells: &[&Cells], width: usize) -> (usize, usize, usize) {
    let widest = |measure: &dyn Fn(&Cells) -> usize, floor: usize| {
        cells
            .iter()
            .map(|cell| measure(cell))
            .max()
            .unwrap_or(0)
            .max(floor)
    };
    let mut name =
        widest(&|cell| cell.name_width(), theme::display_width("Agent")).min(NAME_COLUMN_MAX_CELLS);
    let mut model = widest(
        &|cell| theme::display_width(&cell.model),
        theme::display_width("Model"),
    )
    .min(40);
    let mut activity = widest(
        &|cell| theme::display_width(&cell.activity),
        theme::display_width("Activity"),
    );
    let available = width.saturating_sub(MARGIN + 1);
    let mut over = (name + model + activity + 2 * COLUMN_GAP).saturating_sub(available);
    for (column, floor) in [(&mut model, 12), (&mut name, 14), (&mut activity, 8)] {
        let cut = over.min(column.saturating_sub(floor));
        *column -= cut;
        over -= cut;
    }
    (name, model, activity)
}

fn spaces(count: usize) -> Span<'static> {
    Span::raw(" ".repeat(count))
}

/// A cell clipped to `width` columns and padded to exactly that many.
fn cell(text: &str, width: usize, style: Style) -> Vec<Span<'static>> {
    let clipped = theme::clip_width(text, width.max(1));
    let pad = width.saturating_sub(theme::display_width(&clipped));
    vec![Span::styled(clipped, style), spaces(pad)]
}

fn row_line(cells: &Cells, widths: (usize, usize, usize)) -> Vec<Span<'static>> {
    let (name_width, model_width, activity_width) = widths;
    let indent = (cells.depth * INDENT_PER_DEPTH).min(name_width.saturating_sub(2));
    let mut spans = vec![
        spaces(MARGIN + indent),
        Span::styled(cells.icon, cells.icon_style),
        spaces(1),
    ];
    let room = name_width.saturating_sub(indent + 2);
    let name = theme::clip_width(&cells.name, room.max(1));
    let used = theme::display_width(&name);
    spans.push(Span::styled(name, theme::text()));
    spans.push(spaces(room.saturating_sub(used) + COLUMN_GAP));
    spans.extend(cell(&cells.model, model_width, theme::muted()));
    spans.push(spaces(COLUMN_GAP));
    spans.extend(cell(&cells.activity, activity_width, cells.activity_style));
    spans
}

/// Pad a row to the full width and paint the selection band behind it.
fn banded(mut spans: Vec<Span<'static>>, width: usize) -> Line<'static> {
    let used: usize = spans
        .iter()
        .map(|span| theme::display_width(&span.content))
        .sum();
    spans.push(spaces(width.saturating_sub(used)));
    let bg = theme::selection_bg();
    Line::from(
        spans
            .into_iter()
            .map(|span| Span::styled(span.content, span.style.bg(bg)))
            .collect::<Vec<_>>(),
    )
}

/// The selected agent's last message, wrapped under its row.
fn preview_lines(state: &AppState, index: usize, depth: usize, width: usize) -> Vec<Line<'static>> {
    let member = &state.members()[index];
    let indent = (MARGIN + depth * INDENT_PER_DEPTH + 2).min(width.saturating_sub(2));
    let text = state.chat().iter().rev().find_map(|item| match item {
        ChatItem::Agent {
            member: speaker,
            text,
            ..
        } if speaker == &member.id && !text.trim().is_empty() => Some(text),
        _ => None,
    });
    let Some(text) = text else {
        return Vec::new();
    };
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let wrap_width = width.saturating_sub(indent + 1).max(1);
    let wrapped = markdown::wrap(&collapsed, wrap_width);
    let cut = wrapped.len() > MAX_PREVIEW_LINES;
    let shown = wrapped.len().min(MAX_PREVIEW_LINES);
    wrapped
        .into_iter()
        .take(shown)
        .enumerate()
        .map(|(position, mut line)| {
            if cut && position + 1 == shown {
                line.push('…');
                line = theme::clip_width(&line, wrap_width);
            }
            Line::from(vec![spaces(indent), Span::styled(line, theme::dim())])
        })
        .collect()
}

/// Lay the whole screen out for a `width` x `height` terminal.
pub(crate) fn build(state: &AppState, width: usize, height: usize) -> Screen {
    let members = state.members();
    let rows = state.agents_rows();
    let selected = state.agents_selected();
    let searching = !state.agents_query().trim().is_empty();

    let cells: Vec<Option<Cells>> = rows
        .iter()
        .map(|row| match row {
            AgentRow::All => None,
            AgentRow::Member { index, depth } => Some(cells_for(state, *index, *depth)),
            AgentRow::Saved { index } => Some(saved_cells_for(state, *index)),
        })
        .collect();
    let widths = column_widths(&cells.iter().flatten().collect::<Vec<_>>(), width);

    // The fixed header: counts, search prompt, legend.
    let (running, idle) = section_counts(members);
    let inactive = state.saved_chats().len();
    let mut lines: Vec<Line<'static>> = vec![
        Line::from(vec![
            spaces(MARGIN),
            Span::styled(
                format!("Agents · {running} running, {idle} idle, {inactive} inactive"),
                theme::dim(),
            ),
        ]),
        Line::default(),
    ];
    let query = state.agents_query();
    let renaming = state.agents_rename();
    let prompt_head = if renaming.is_some() {
        RENAME_PROMPT
    } else {
        "> "
    };
    let head_width = theme::display_width(prompt_head);
    let mut prompt = vec![spaces(MARGIN), Span::styled(prompt_head, theme::muted())];
    let prompt_room = width.saturating_sub(MARGIN + head_width + 1).max(1);
    let typed = renaming.unwrap_or(query);
    // A long name shows its tail, where the caret is.
    let shown_query = if theme::display_width(typed) > prompt_room {
        let mut tail: Vec<char> = Vec::new();
        let mut used = 0;
        for ch in typed.chars().rev() {
            let cell = theme::display_width(&ch.to_string());
            if used + cell > prompt_room {
                break;
            }
            used += cell;
            tail.push(ch);
        }
        tail.into_iter().rev().collect()
    } else {
        typed.to_string()
    };
    let caret_col = MARGIN + head_width + theme::display_width(&shown_query);
    if renaming.is_none() && query.is_empty() {
        prompt.push(Span::styled(PLACEHOLDER, theme::dim()));
    } else {
        prompt.push(Span::styled(shown_query, theme::text()));
    }
    lines.push(Line::from(prompt));
    lines.push(Line::default());
    let mut legend = vec![spaces(MARGIN)];
    let legend_style = theme::text().add_modifier(Modifier::BOLD);
    legend.extend(cell("Agent", widths.0, legend_style));
    legend.push(spaces(COLUMN_GAP));
    legend.extend(cell("Model", widths.1, legend_style));
    legend.push(spaces(COLUMN_GAP));
    legend.push(Span::styled("Activity", legend_style));
    lines.push(Line::from(legend));
    lines.push(Line::default());
    debug_assert_eq!(lines.len(), HEADER_ROWS);

    // The scrollable body.
    let mut items: Vec<Item> = Vec::new();
    let mut selected_item = 0;
    let mut selected_preview = 0;
    let mut section: Option<Section> = None;
    for (position, row) in rows.iter().enumerate() {
        match row {
            AgentRow::All if !searching => {
                items.push(Item::Row(position));
                items.push(Item::Blank);
            }
            AgentRow::Member { index, depth: 0 } if !searching => {
                let own = row_section(members, *index);
                if section != Some(own) {
                    if section.is_some() {
                        items.push(Item::Blank);
                    }
                    let count = groups(members)
                        .iter()
                        .filter(|group| group_section(members, group) == own)
                        .count();
                    items.push(Item::Heading(format!("{} ({count})", own.title())));
                    section = Some(own);
                }
                items.push(Item::Row(position));
            }
            AgentRow::Saved { .. } if !searching => {
                if section != Some(Section::Inactive) {
                    if section.is_some() {
                        items.push(Item::Blank);
                    }
                    items.push(Item::Heading(format!(
                        "{} ({inactive})",
                        Section::Inactive.title()
                    )));
                    section = Some(Section::Inactive);
                }
                items.push(Item::Row(position));
            }
            _ => items.push(Item::Row(position)),
        }
        if position == selected {
            selected_item = items.len() - 1;
            if let AgentRow::Member { index, depth } = row {
                for line in preview_lines(state, *index, *depth, width) {
                    items.push(Item::Preview(line));
                    selected_preview += 1;
                }
            }
        }
    }
    if rows.is_empty() {
        items.push(Item::Note(NO_MATCH.to_string()));
    }

    let list_rows = height.saturating_sub(HEADER_ROWS + 1);
    let (start, end, leading, trailing) =
        window(items.len(), selected_item, selected_preview, list_rows);
    let mut clicks = Vec::new();
    let marker = || Line::from(Span::styled("  ...", theme::dim()));
    if leading {
        lines.push(marker());
    }
    for item in &items[start..end] {
        let screen_row = lines.len();
        match item {
            Item::Blank => lines.push(Line::default()),
            Item::Heading(title) => lines.push(Line::from(vec![
                spaces(MARGIN),
                Span::styled(
                    theme::clip_width(title, width.saturating_sub(MARGIN)),
                    theme::muted(),
                ),
            ])),
            Item::Note(text) => lines.push(Line::from(vec![
                spaces(MARGIN),
                Span::styled(text.clone(), theme::dim()),
            ])),
            Item::Preview(line) => lines.push(line.clone()),
            Item::Row(position) => {
                let spans = match &cells[*position] {
                    Some(cells) => row_line(cells, widths),
                    None => all_row(),
                };
                lines.push(if *position == selected {
                    banded(spans, width)
                } else {
                    Line::from(spans)
                });
                let choice = match rows[*position] {
                    AgentRow::All => AgentChoice::All,
                    AgentRow::Member { index, .. } => {
                        AgentChoice::Member(members[index].id.clone())
                    }
                    AgentRow::Saved { index } => AgentChoice::Saved(state.saved_chats()[index].id),
                };
                clicks.push((screen_row as u16, choice));
            }
        }
    }
    if trailing {
        lines.push(marker());
    }
    // The hint line is always the last screen row.
    lines.truncate(height.saturating_sub(1));
    while lines.len() + 1 < height {
        lines.push(Line::default());
    }
    if height > 0 {
        lines.push(Line::from(Span::styled(
            theme::clip_width(
                &format!("{}{}", " ".repeat(MARGIN), hint_line(state)),
                width,
            ),
            theme::muted(),
        )));
    }
    let clicks = clicks
        .into_iter()
        .filter(|(row, _)| (*row as usize) + 1 < height)
        .collect();
    Screen {
        lines,
        cursor: (height > 2).then_some((caret_col.min(width.saturating_sub(1)) as u16, 2)),
        clicks,
        list_rows,
    }
}

fn all_row() -> Vec<Span<'static>> {
    vec![
        spaces(MARGIN),
        Span::styled("All messages", theme::text().add_modifier(Modifier::BOLD)),
    ]
}

/// The slice of `len` items to show in `visible` rows: centred on the selected
/// item, shifted forward when its preview (`preview` items after it) would not
/// fit, with `...` rows folded out of the budget on any clipped side.
/// Returns `(start, end, leading_marker, trailing_marker)`.
fn window(
    len: usize,
    selected: usize,
    preview: usize,
    visible: usize,
) -> (usize, usize, bool, bool) {
    if visible == 0 || len == 0 {
        return (0, 0, false, false);
    }
    let block_end = selected + preview;
    let anchor = selected as isize - (visible / 2) as isize;
    let upper = len as isize - visible as isize;
    let start = anchor.min(upper).max(0) as usize;
    let mut leading = start > 0 && visible > 1;
    let mut trailing = start + visible < len && visible > 2;
    let mut content = visible - usize::from(leading) - usize::from(trailing);
    let (mut slice_start, mut slice_end) = (start, start);
    for _ in 0..8 {
        slice_start = if block_end >= start + content {
            (block_end + 1).saturating_sub(content).min(selected)
        } else {
            start
        };
        slice_end = (slice_start + content).min(len);
        let next_leading = slice_start > 0 && visible > 1;
        let next_trailing = slice_end < len && visible > 2;
        if (next_leading, next_trailing) == (leading, trailing) {
            break;
        }
        leading = next_leading;
        trailing = next_trailing;
        content = visible - usize::from(leading) - usize::from(trailing);
    }
    (slice_start, slice_end, leading, trailing)
}

/// Paint the Agents list over the whole frame.
pub(crate) fn render(frame: &mut Frame<'_>, state: &AppState) {
    let area = frame.area();
    if area.width == 0 || area.height == 0 {
        return;
    }
    let screen = build(state, area.width as usize, area.height as usize);
    state.note_drawer_viewport(screen.list_rows);
    state.set_agents_click_rows(screen.clicks);
    for (row, line) in screen.lines.into_iter().enumerate() {
        frame.render_widget(
            Paragraph::new(line),
            Rect::new(area.x, area.y + row as u16, area.width, 1),
        );
    }
    if let Some((column, row)) = screen.cursor {
        frame.set_cursor_position((area.x + column, area.y + row));
    }
}
