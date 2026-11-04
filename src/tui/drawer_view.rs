//! Drawer overlays: logs, team roster/editor, command palette, diff, and
//! member logs. The `/runs` drawer body lives in `run_view`.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};

use crate::domain::event::{LogEntry, LogLevel, MemberStatus};
use crate::domain::team::MemberId;
use crate::tui::app_state::AppState;
use crate::tui::drawers::Drawer;
use crate::tui::markdown;
use crate::tui::runs_view::drawer_runs;
use crate::tui::team_builder::Field;
use crate::tui::team_builder::{backend_picker_lines, model_picker_lines, session_status_label};
use crate::tui::theme;
use crate::tui::theme::pad_width;

/// Drawers shown inline, above the composer (like the `/model` picker),
/// instead of as an overlay on the chat. (The Agents list is neither: it
/// takes over the whole frame, see `agents_view`.)
pub(crate) fn is_inline(drawer: &Drawer) -> bool {
    matches!(drawer, Drawer::Mode)
}

/// Title line and body rows of an inline drawer, every row clipped to `width`.
fn inline_parts(
    state: &AppState,
    drawer: &Drawer,
    width: usize,
) -> (Line<'static>, Vec<Line<'static>>) {
    let clip = |line: Line<'static>| Line::from(clip_spans(line.spans, width));
    // The editor leads with its own title line.
    let mut lines = match drawer {
        Drawer::Mode => drawer_mode(state),
        _ => Vec::new(),
    }
    .into_iter();
    let title = lines.next().unwrap_or_default();
    (clip(title), lines.map(clip).collect())
}

/// Rows an inline drawer needs (title, content, hint), at most `max_rows`:
/// as tall as its content, never a mostly empty box.
pub(crate) fn inline_height(state: &AppState, drawer: &Drawer, width: u16, max_rows: u16) -> u16 {
    let (_, body) = inline_parts(state, drawer, width as usize);
    ((body.len() + 2) as u16).min(max_rows.max(3))
}

/// Paint an inline drawer into `area`: a title, the content (scrolled to keep
/// the selection in view), and the key hint.
pub(crate) fn render_inline_drawer(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &AppState,
    drawer: &Drawer,
) {
    if area.height < 3 || area.width == 0 {
        return;
    }
    let width = area.width as usize;
    let (title, body) = inline_parts(state, drawer, width);
    frame.render_widget(
        Paragraph::new(title),
        Rect::new(area.x, area.y, area.width, 1),
    );
    let rows = area.height as usize - 2;
    state.note_drawer_viewport(rows);
    let max_scroll = body.len().saturating_sub(rows);
    let offset = if matches!(drawer, Drawer::Mode) {
        // Follow the editor's cursor row instead of a manual scroll.
        body.iter()
            .position(line_has_cursor)
            .map_or(0, |row| (row + 1).saturating_sub(rows))
    } else {
        state.drawer_scroll()
    }
    .min(max_scroll);
    frame.render_widget(
        Paragraph::new(body).scroll((offset as u16, 0)),
        Rect::new(area.x, area.y + 1, area.width, rows as u16),
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(" {}", drawer_hint(state, drawer, width)),
            theme::muted(),
        ))),
        Rect::new(area.x, area.y + area.height - 1, area.width, 1),
    );
}

/// Whether a drawer row starts with a selection marker.
fn line_has_cursor(line: &Line<'static>) -> bool {
    line.spans.first().is_some_and(|span| {
        let text = span.content.trim_start();
        text.starts_with('▶') || text.starts_with('›')
    })
}

/// The body lines of an overlay drawer for a content area `width` columns wide.
fn drawer_lines(state: &AppState, drawer: &Drawer, width: usize) -> Vec<Line<'static>> {
    match drawer {
        Drawer::Logs => drawer_logs(state, width),
        Drawer::Team => drawer_team(state, width),
        // The Agents list is full-screen; see `agents_view`.
        Drawer::Agents => Vec::new(),
        Drawer::Mode => drawer_mode(state),
        Drawer::Runs => drawer_runs(state, width),
        Drawer::Palette => drawer_palette(),
        Drawer::Diff => drawer_diff(state),
        Drawer::MemberLogs(member_id) => drawer_member_logs(state, member_id, width),
    }
}

/// Rows a line takes once wrapped to `width` columns.
fn visual_rows(lines: &[Line<'_>], width: usize) -> usize {
    let width = width.max(1);
    lines
        .iter()
        .map(|line| {
            let w = line_width(line);
            if w == 0 { 1 } else { w.div_ceil(width).max(1) }
        })
        .sum()
}

/// Columns of padding between an overlay's border and its text.
const OVERLAY_PADDING: u16 = 1;

/// The content area of an overlay drawn in `popup`: inside the border, one
/// column of padding on each side, minus the hint row.
fn overlay_content(popup: Rect) -> (Rect, Option<Rect>) {
    let inner = Block::default().borders(Borders::ALL).inner(popup);
    let padded = Rect::new(
        inner.x + OVERLAY_PADDING.min(inner.width),
        inner.y,
        inner.width.saturating_sub(2 * OVERLAY_PADDING),
        inner.height,
    );
    if padded.height > 1 {
        (
            Rect::new(padded.x, padded.y, padded.width, padded.height - 1),
            Some(Rect::new(
                inner.x,
                inner.y + inner.height - 1,
                inner.width,
                1,
            )),
        )
    } else {
        (padded, None)
    }
}

/// The rectangle an overlay occupies inside `area` (the chat column): full
/// width, as tall as its content (title, content, hint), anchored to the
/// bottom so it sits against the composer. The team editor, whose pickers
/// change its height, fills the column.
pub(crate) fn overlay_rect(state: &AppState, drawer: &Drawer, area: Rect) -> Rect {
    if matches!(drawer, Drawer::Team) {
        return area;
    }
    // Width of the text once the border and padding are taken off.
    let text_width = area.width.saturating_sub(2 + 2 * OVERLAY_PADDING) as usize;
    let lines = drawer_lines(state, drawer, text_width);
    // Two border rows and the hint row around the content.
    let wanted = (visual_rows(&lines, text_width) + 3).min(u16::MAX as usize) as u16;
    let height = wanted.clamp(4, area.height.max(1)).min(area.height);
    Rect::new(area.x, area.y + area.height - height, area.width, height)
}

/// Paint an overlay drawer into the bottom of `area` (the chat column). Every
/// cell it covers is cleared first, so no chat text shows through it.
pub(crate) fn render_drawer(frame: &mut Frame<'_>, area: Rect, state: &AppState, drawer: &Drawer) {
    let popup = overlay_rect(state, drawer, area);
    frame.render_widget(Clear, popup);
    let title = match drawer {
        Drawer::MemberLogs(member) => format!("Logs: {member}"),
        Drawer::Palette => format!("Commands · Asterline {}", env!("CARGO_PKG_VERSION")),
        _ => drawer.title().to_string(),
    };
    let block = Block::default()
        .title(Span::styled(format!(" {title} "), theme::accent_bold()))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme::muted());
    frame.render_widget(block, popup);

    // Text sits one column in from the border; the bottom row is the hint.
    let (content, hint_row) = overlay_content(popup);

    let lines = drawer_lines(state, drawer, content.width as usize);
    // Clamp the scroll offset so content can't be pushed entirely off-screen.
    // Account for line wrapping: the Paragraph widget wraps long lines, so the
    // visual line count can exceed `lines.len()`.
    let visual_count = visual_rows(&lines, content.width as usize);
    let max_scroll = visual_count.saturating_sub(content.height as usize);
    state.note_drawer_viewport(content.height as usize);
    state.note_drawer_max_scroll(max_scroll);
    let offset = state.drawer_scroll().min(max_scroll) as u16;
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((offset, 0)),
        content,
    );

    if let Some(hint_row) = hint_row {
        let hint = drawer_hint(state, drawer, hint_row.width.saturating_sub(1) as usize);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(format!(" {hint}"), theme::muted()))),
            hint_row,
        );
    }

    if matches!(drawer, Drawer::Team)
        && let Some(edit) = state.team_editor().and_then(|editor| editor.editing())
    {
        crate::tui::team_builder::render_edit_box(frame, content, edit);
    }
    if matches!(drawer, Drawer::Mode)
        && let Some(edit) = state.mode_editor().and_then(|editor| editor.editing())
    {
        crate::tui::team_builder::render_edit_box(frame, content, edit);
    }
}

/// The first hint that fits in `width` columns (the shortest, clipped, when
/// none does), so a hint never wraps or runs off the row.
fn fit_hint(variants: &[&str], width: usize) -> String {
    variants
        .iter()
        .find(|hint| theme::display_width(hint) <= width)
        .map_or_else(
            || theme::clip_width(variants.last().copied().unwrap_or(""), width),
            |hint| hint.to_string(),
        )
}

/// The single key-hint line of a drawer.
fn drawer_hint(state: &AppState, drawer: &Drawer, width: usize) -> String {
    let team = state.team_editor();
    let mode = state.mode_editor();
    let variants: &[&str] = match drawer {
        Drawer::Runs if state.runs_detail() => &[
            "x compact · ↑↓ step · ←→ run · Enter status · Tab dispatch · Pg scroll · Esc close",
            "x compact · ↑↓ step · ←→ run · Enter status · Esc close",
            "x compact · ↑↓ step · Esc close",
        ],
        Drawer::Runs => &[
            "x details · ↑↓ step · ←→ run · Enter status · Tab dispatch · Pg scroll · Esc close",
            "x details · ↑↓ step · ←→ run · Enter status · Esc close",
            "x details · ↑↓ step · Esc close",
        ],
        Drawer::Agents => &[crate::tui::agents_view::HINTS],
        Drawer::Team if team.is_some_and(|editor| editor.editing().is_some()) => &[
            "Enter save · Esc cancel · ←/→ move · Ctrl+U/W/K edit",
            "Enter save · Esc cancel",
        ],
        Drawer::Team if team.is_some_and(|editor| editor.backend_picker().is_some()) => {
            &["↑↓ choose Agent · Enter apply · Esc cancel"]
        }
        Drawer::Team if team.is_some_and(|editor| editor.model_picker().is_some()) => &[
            "↑↓ model · ←→ effort · type filter · Enter choose both · Esc cancel",
            "↑↓ model · ←→ effort · Enter choose · Esc cancel",
        ],
        Drawer::Team if team.is_some_and(|editor| editor.session_picker().is_some()) => &[
            "↑↓ choose session · type filter · Enter apply · Esc cancel",
            "↑↓ session · Enter apply · Esc cancel",
        ],
        Drawer::Team if team.is_some_and(|editor| editor.field_mode()) => &[
            "↑↓ field · Enter edit/choose · y copy · t reload models · e manual entry · s apply · Esc members",
            "↑↓ field · Enter edit · y copy · e manual · s apply · Esc back",
            "↑↓ field · Enter edit · s apply · Esc back",
        ],
        Drawer::Team => &[
            "↑↓ member · Enter fields · y copy session · a add · d delete · s apply · Esc close",
            "↑↓ member · Enter fields · y copy · a add · d del · s apply · Esc close",
            "↑↓ member · Enter fields · y copy · a add · d del · s apply · Esc",
            "↑↓ member · Enter fields · a add · d del · s apply · Esc",
            "↑↓ · Enter · a add · d del · s apply · Esc",
        ],
        Drawer::Mode if mode.is_some_and(|editor| editor.editing().is_some()) => &[
            "Enter save · Esc cancel · ←/→ move · Ctrl+U/W/K edit",
            "Enter save · Esc cancel",
        ],
        Drawer::Mode if mode.is_some_and(|editor| editor.member_picker().is_some()) => {
            if mode
                .and_then(|editor| editor.member_picker())
                .is_some_and(|picker| picker.multi())
            {
                &[
                    "↑↓ member · Space toggle · Enter confirm · Esc cancel",
                    "↑↓ member · Space · Enter · Esc cancel",
                ]
            } else {
                &["↑↓ member · Enter choose · Esc cancel"]
            }
        }
        Drawer::Mode if mode.is_some_and(|editor| editor.field_mode()) => &[
            "↑↓ field · Enter edit · ←/→ step · r revert · s select + apply · w team.json · Esc list",
            "↑↓ field · Enter edit · ←/→ step · s apply · Esc list",
            "↑↓ field · Enter edit · s apply · Esc list",
        ],
        Drawer::Mode => &["↑↓ select · Enter fields · Esc close"],
        _ => &["↑↓ scroll · Esc close"],
    };
    fit_hint(variants, width)
}

/// Display width of all spans in a line (for scroll clamping with wrapping).
fn line_width(line: &Line<'_>) -> usize {
    line.spans
        .iter()
        .map(|span| theme::display_width(span.content.as_ref()))
        .sum()
}

/// Log rows, oldest first, wrapped to `width`. Only warnings and errors carry
/// a level label; continuation lines align under the message text.
fn log_lines<'a>(entries: impl Iterator<Item = &'a LogEntry>, width: usize) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for entry in entries {
        let mut prefix = Vec::new();
        if matches!(entry.level, LogLevel::Warn | LogLevel::Error) {
            prefix.push(Span::styled(
                format!("{:<5} ", entry.level.as_str()),
                Style::default().fg(theme::log_color(entry.level)),
            ));
        }
        prefix.push(Span::styled(
            format!("{} ", entry.source),
            theme::accent_bold(),
        ));
        let prefix_width: usize = prefix
            .iter()
            .map(|span| theme::display_width(span.content.as_ref()))
            .sum();
        let room = width.saturating_sub(prefix_width).max(1);
        let message = markdown::wrap(&entry.message, room);
        let mut message = message.into_iter();
        let mut first = prefix;
        first.push(Span::styled(
            message.next().unwrap_or_default(),
            theme::text(),
        ));
        out.push(Line::from(first));
        for line in message {
            out.push(Line::from(vec![
                Span::raw(" ".repeat(prefix_width)),
                Span::styled(line, theme::text()),
            ]));
        }
    }
    out
}

/// The most recent entries shown, oldest first like the transcript.
const LOG_ROWS: usize = 200;

fn drawer_logs(state: &AppState, width: usize) -> Vec<Line<'static>> {
    let logs = state.logs();
    if logs.is_empty() {
        return vec![Line::styled("no logs yet", theme::muted())];
    }
    log_lines(logs.iter().skip(logs.len().saturating_sub(LOG_ROWS)), width)
}

/// Cut a row's spans to `width` display columns, ending with `…` when clipped.
fn clip_spans(spans: Vec<Span<'static>>, width: usize) -> Vec<Span<'static>> {
    let total: usize = spans
        .iter()
        .map(|span| theme::display_width(&span.content))
        .sum();
    if total <= width {
        return spans;
    }
    let budget = width.saturating_sub(1);
    let mut used = 0;
    let mut out = Vec::new();
    for span in spans {
        let span_width = theme::display_width(&span.content);
        if used + span_width <= budget {
            used += span_width;
            out.push(span);
        } else {
            let kept = theme::slice_display_cols(&span.content, 0, budget - used);
            out.push(Span::styled(format!("{kept}…"), span.style));
            break;
        }
    }
    out
}

fn drawer_member_logs(state: &AppState, member_id: &MemberId, width: usize) -> Vec<Line<'static>> {
    let member_str = member_id.as_str();
    let filtered: Vec<&LogEntry> = state
        .logs()
        .iter()
        .filter(|entry| entry.source == member_str)
        .collect();

    if filtered.is_empty() {
        return vec![Line::styled(
            format!("no logs yet for {member_id}"),
            theme::muted(),
        )];
    }
    let skip = filtered.len().saturating_sub(LOG_ROWS);
    log_lines(filtered.into_iter().skip(skip), width)
}

/// Build a table header line and its matching rule from column widths.
fn table_header(cells: &[&str], widths: &[usize], tail: &str) -> (Line<'static>, Line<'static>) {
    let mut text = String::new();
    let mut rule = String::new();
    for (cell, width) in cells.iter().zip(widths) {
        text.push_str(&pad_width(cell, *width));
        text.push_str("│ ");
        rule.push_str(&"─".repeat(*width));
        rule.push_str("┼─");
    }
    text.push_str(tail);
    rule.push_str(&"─".repeat(theme::display_width(tail).max(4)));
    (
        Line::from(Span::styled(text, theme::accent_bold())),
        Line::from(Span::styled(rule, theme::muted())),
    )
}

fn active_mode_run_banner(state: &AppState) -> Option<String> {
    state.runs().iter().find_map(|run| {
        let mode = run.mode.as_ref()?;
        if !matches!(
            run.status,
            crate::domain::event::RunStatus::Running | crate::domain::event::RunStatus::Verifying
        ) {
            return None;
        }
        Some(format!(
            "{} {} in progress — changes apply to the next run",
            mode.mode, run.id
        ))
    })
}

fn drawer_mode(state: &AppState) -> Vec<Line<'static>> {
    let Some(editor) = state.mode_editor() else {
        return vec![Line::styled("mode panel unavailable", theme::muted())];
    };
    editor.lines(active_mode_run_banner(state).as_deref())
}

fn drawer_team(state: &AppState, width: usize) -> Vec<Line<'static>> {
    if let Some(editor) = state.team_editor() {
        return drawer_team_editor(state, editor, width);
    }

    let mut lines = Vec::new();
    lines.push(Line::from(vec![
        Span::styled(" Team", theme::accent_bold()),
        Span::styled(
            format!("  {} member(s)", state.members().len()),
            theme::muted(),
        ),
    ]));
    lines.push(Line::raw(""));

    for member in state.members() {
        let color =
            theme::backend_color_shaded(member.backend, state.member_color_index(&member.id));
        let status_label = theme::status_label(member.status);
        // Header line: ● Name  backend  ·  status
        lines.push(Line::from(vec![
            Span::styled("● ", theme::bold(color)),
            Span::styled(member.display_name.clone(), theme::bold(color)),
            Span::styled(format!("   {}", member.backend.as_str()), theme::muted()),
            Span::styled(format!("   {status_label}"), {
                Style::default().fg(theme::status_color(member.status))
            }),
        ]));
        // Detail line 1: session · model · effort
        let session_status = session_status_label(
            member.session_policy,
            member.session.as_deref(),
            member.status == MemberStatus::Running,
        );
        let session = member.session.clone().unwrap_or_else(|| "—".to_string());
        let model = state
            .member_model_summary(member)
            .unwrap_or_else(|| "default".to_string());
        lines.push(Line::from(vec![
            Span::styled("  session ", theme::muted()),
            Span::styled(session_status, theme::text()),
            Span::styled(" · ", theme::muted()),
            Span::styled(session, theme::text()),
            Span::styled("  ·  model ", theme::muted()),
            Span::styled(model, theme::text()),
        ]));
        // Detail line 2: cwd
        lines.push(Line::from(vec![
            Span::styled("  cwd ", theme::muted()),
            Span::styled(member.cwd.clone(), theme::text()),
        ]));
        lines.push(Line::raw(""));
    }

    if !state.pending_approvals().is_empty() {
        lines.push(Line::styled(" Pending approvals", theme::warning_bold()));
        for approval in state.pending_approvals() {
            lines.push(Line::from(vec![
                Span::styled(format!("  {} ", approval.id), theme::warning_bold()),
                Span::styled(format!("{} (", approval.action), theme::warning()),
                Span::styled(
                    approval.body.clone(),
                    theme::text().add_modifier(ratatui::style::Modifier::ITALIC),
                ),
                Span::styled(")", theme::warning()),
            ]));
        }
    }
    lines
}

// The roster is an identity overview. Configuration and live state belong in
// the selected-member detail below it; repeating them here made the drawer
// wide without helping selection.
const EDITOR_COLUMNS: [usize; 1] = [17];

fn drawer_team_editor(
    state: &AppState,
    editor: &crate::tui::team_editor::TeamEditor,
    width: usize,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let dirty = if editor.dirty() { "modified" } else { "saved" };
    lines.push(Line::from(vec![
        Span::styled(" Team editor ", theme::accent_bold()),
        Span::styled(
            format!("({dirty})"),
            if editor.dirty() {
                theme::warning()
            } else {
                theme::muted()
            },
        ),
    ]));
    lines.push(Line::styled(
        format!(" Agent tools: {}", editor.agent_availability_label()),
        theme::muted(),
    ));
    if let Some(notice) = editor.notice() {
        lines.push(Line::styled(format!(" {notice}"), theme::warning()));
    }
    lines.push(Line::raw(""));
    let (header, rule) = table_header(&["   Member"], &EDITOR_COLUMNS, "Backend");
    lines.push(header);
    lines.push(rule);

    for (idx, member) in editor.members().iter().enumerate() {
        let selected = idx == editor.selected();
        let shade = editor.members()[..idx]
            .iter()
            .filter(|candidate| candidate.backend == member.backend)
            .count();
        let color = theme::backend_color_shaded(member.backend, shade);
        let row_style = if selected {
            theme::bold(theme::emphasis_color())
        } else {
            Style::default().fg(color)
        };
        let sep = Span::styled("│ ", theme::muted());
        lines.push(Line::from(vec![
            Span::raw(" "),
            Span::styled(
                if selected { "▶ " } else { "  " },
                if selected {
                    theme::warning_bold()
                } else {
                    theme::muted()
                },
            ),
            Span::styled(
                pad_width(
                    &if member.parent.is_some() {
                        format!("↳ {}", member.display_name)
                    } else {
                        member.display_name.clone()
                    },
                    EDITOR_COLUMNS[0] - 3,
                ),
                row_style,
            ),
            sep,
            Span::styled(member.backend.as_str().to_string(), row_style),
        ]));
    }

    if editor.backend_picker().is_none()
        && editor.model_picker().is_none()
        && editor.session_picker().is_none()
        && let Some(member) = editor.selected_member()
    {
        lines.push(Line::raw(""));
        if let Some(parent) = &member.parent {
            let parent_name = editor
                .members()
                .iter()
                .find(|candidate| &candidate.id == parent)
                .map(|candidate| candidate.display_name.as_str())
                .unwrap_or(parent.as_str());
            lines.push(Line::styled(
                format!(" Sub-agent of {parent_name} · saved with this chat"),
                theme::muted(),
            ));
        }
        lines.push(Line::from(Span::styled(
            " Selected member fields",
            theme::accent_bold(),
        )));
        let label_width = Field::label_width_for_backend(member.backend);
        for (idx, field) in editor.fields().iter().enumerate() {
            let selected = editor.field_mode() && idx == editor.field_index();
            let style = if selected {
                theme::editor_field_focus()
            } else {
                theme::text()
            };
            lines.push(Line::from(Span::styled(
                format!(
                    " {} {:>label_width$}: {}",
                    if selected { "›" } else { " " },
                    field.label_for_backend(member.backend),
                    editor.field_value(member, *field)
                ),
                style,
            )));
        }
    }

    if let Some(picker) = editor.backend_picker() {
        lines.push(Line::raw(""));
        lines.extend(backend_picker_lines(
            picker,
            editor.model_catalog(),
            &editor.selected_cwd(),
            width,
        ));
    }

    if let Some(picker) = editor.model_picker() {
        lines.push(Line::raw(""));
        lines.extend(model_picker_lines(picker, width, 8));
    }

    if let Some(picker) = editor.session_picker() {
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {} sessions ", picker.backend().as_str()),
                theme::accent_bold(),
            ),
            Span::styled(
                format!("{} match(es)", picker.visible_len()),
                theme::muted(),
            ),
        ]));
        lines.push(Line::from(vec![
            Span::styled(" Search: ", theme::muted()),
            Span::styled(
                if picker.query().is_empty() {
                    "type title, project, or id…".to_string()
                } else {
                    picker.query().to_string()
                },
                if picker.query().is_empty() {
                    theme::muted_italic()
                } else {
                    theme::emphasis()
                },
            ),
        ]));
        if let Some(error) = picker.error() {
            lines.push(Line::styled(format!(" {error}"), theme::warning()));
        } else if picker.visible_len() == 0 {
            lines.push(Line::styled(" No matching sessions.", theme::muted()));
        } else {
            let available = width.saturating_sub(6).max(32);
            let session_columns = [available * 35 / 100, available * 30 / 100, 8];
            let id_width = available.saturating_sub(session_columns.iter().sum::<usize>());
            let (header, rule) = table_header(
                &["   Title", "Project", "Updated"],
                &session_columns,
                "Session ID",
            );
            lines.push(header);
            lines.push(rule);
            let (start, entries) = picker.window(8);
            for (offset, entry) in entries.iter().enumerate() {
                let selected = start + offset == picker.selected();
                let row_style = if selected {
                    theme::emphasis()
                } else {
                    theme::text()
                };
                let sep = Span::styled("│ ", theme::muted());
                lines.push(Line::from(vec![
                    Span::styled(
                        if selected { " ▶ " } else { "   " },
                        if selected {
                            theme::warning_bold()
                        } else {
                            theme::muted()
                        },
                    ),
                    Span::styled(pad_width(&entry.title, session_columns[0] - 3), row_style),
                    sep.clone(),
                    Span::styled(pad_width(&entry.project, session_columns[1]), row_style),
                    sep.clone(),
                    Span::styled(pad_width(&entry.age(), session_columns[2]), theme::muted()),
                    sep,
                    Span::styled(theme::clip_width(&entry.id, id_width.max(1)), row_style),
                ]));
            }
        }
        lines.push(Line::styled(
            " Type filter · ↑/↓ select · PgUp/PgDn · Enter bind · Esc cancel · Ctrl+U clear",
            theme::muted_italic(),
        ));
    }

    if !state.pending_approvals().is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            " Pending approvals are in the card above the composer (y agree · n deny).",
            theme::muted(),
        ));
    }
    lines
}

fn drawer_palette() -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::styled(
            " Type to talk to the agent you're viewing · ← switch agent",
            theme::muted(),
        ),
        Line::raw(""),
    ];

    for (index, (title, names)) in crate::tui::completion::COMMAND_GROUPS.iter().enumerate() {
        if index > 0 {
            lines.push(Line::raw(""));
        }
        lines.push(Line::styled(format!("  {title}"), theme::muted()));
        for name in *names {
            let Some((name, hint, takes_arg)) = crate::tui::completion::COMMANDS
                .iter()
                .find(|(command, _, _)| command == name)
            else {
                continue;
            };
            let cmd = if *takes_arg {
                format!("/{name} …")
            } else {
                format!("/{name}")
            };
            lines.push(Line::from(vec![
                Span::styled(format!("  {:<24} ", cmd), theme::accent_bold()),
                Span::styled(format!(" {hint}"), theme::text()),
            ]));
        }
    }
    lines
}

/// Render the captured working-tree diff: structural lines stay colored by role,
/// while added/removed/context code is syntax-highlighted by the file's type.
fn drawer_diff(state: &AppState) -> Vec<Line<'static>> {
    let Some(diff) = state.diff_text() else {
        return vec![Line::styled("no diff captured — run /diff", theme::muted())];
    };
    if diff.trim().is_empty() {
        return vec![Line::styled(
            "working tree clean — no changes",
            theme::success(),
        )];
    }

    let mut out = Vec::new();
    let mut ext = String::new();
    for line in diff.lines() {
        // Track the current file so code lines highlight with the right syntax.
        if let Some(path) = line
            .strip_prefix("+++ b/")
            .or_else(|| line.strip_prefix("diff --git a/"))
        {
            ext = file_extension(path);
        }

        if line.starts_with("+++") || line.starts_with("---") {
            out.push(Line::styled(
                line.to_string(),
                theme::muted().add_modifier(ratatui::style::Modifier::BOLD),
            ));
        } else if line.starts_with("@@") {
            out.push(Line::styled(line.to_string(), theme::accent()));
        } else if line.starts_with("diff ")
            || line.starts_with("index ")
            || line.starts_with("Untracked")
        {
            out.push(Line::styled(line.to_string(), theme::warning_bold()));
        } else if let Some(rest) = line.strip_prefix('+') {
            out.push(diff_code_line('+', theme::success_color(), rest, &ext));
        } else if let Some(rest) = line.strip_prefix('-') {
            out.push(diff_code_line('-', theme::error_color(), rest, &ext));
        } else {
            let rest = line.strip_prefix(' ').unwrap_or(line);
            out.push(diff_code_line(' ', theme::muted_color(), rest, &ext));
        }
    }
    out
}

/// One diff content line: a colored +/-/space gutter plus syntax-highlighted code.
fn diff_code_line(
    marker: char,
    gutter: ratatui::style::Color,
    code: &str,
    ext: &str,
) -> Line<'static> {
    let mut spans = vec![Span::styled(
        marker.to_string(),
        Style::default().fg(gutter),
    )];
    if code.is_empty() {
        return Line::from(spans);
    }
    if ext.is_empty() {
        spans.push(Span::styled(code.to_string(), Style::default().fg(gutter)));
    } else {
        spans.extend(markdown::highlight_code_line(code, ext));
    }
    Line::from(spans)
}

fn file_extension(path: &str) -> String {
    let path = path.trim();
    std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::event::{MemberStatus, MemberSummary, RuntimeEvent};
    use crate::domain::mode::{ModesConfig, PlanModeConfig, TerminalMode};
    use crate::domain::team::{
        BackendKind, DefaultTarget, MemberId, PermissionMode, SandboxPolicy, SessionPolicy,
    };

    fn ready_state() -> AppState {
        ready_state_for(BackendKind::Codex)
    }

    fn ready_state_for(backend: BackendKind) -> AppState {
        let mut state = AppState::new(Vec::new());
        state.apply(RuntimeEvent::Ready {
            modes: Default::default(),
            mode_overrides: Default::default(),
            suggested_verify: None,
            team: "t".to_string(),
            workspace: "/tmp/ws".to_string(),
            default_target: Some(DefaultTarget::Member(MemberId::new("builder"))),
            runs: Vec::new(),
            members: vec![MemberSummary {
                id: MemberId::new("builder"),
                display_name: "Builder".to_string(),
                backend,
                role: "implementation".to_string(),
                status: MemberStatus::Idle,
                session: None,
                cwd: "/tmp/ws".to_string(),
                model: None,
                effort: None,
                sandbox: SandboxPolicy::WorkspaceWrite,
                permission_mode: Some(PermissionMode::Default),
                approvals_reviewer: crate::domain::team::CodexApprovalsReviewer::User,
                session_policy: SessionPolicy::Resume,
                parent: None,
            }],
        });
        state
    }

    #[test]
    fn help_leads_with_the_viewing_hint_and_lists_commands_in_three_groups() {
        let lines = drawer_palette();
        let text: Vec<String> = lines.iter().map(line_text).collect();

        assert!(
            text[0].contains("Type to talk to the agent you're viewing"),
            "{text:?}"
        );
        assert!(text[0].contains("← switch agent"), "{text:?}");
        assert!(!text[0].contains("@member") && !text[0].contains("last target"));

        // Group headings appear in order, each before its own commands.
        let heading = |title: &str| {
            text.iter()
                .position(|line| line.trim() == title)
                .unwrap_or_else(|| panic!("missing heading {title}: {text:?}"))
        };
        let (common, approvals, runs) = (
            heading("Common"),
            heading("Approvals & sessions"),
            heading("Runs"),
        );
        assert!(common < approvals && approvals < runs);
        let command_row = |name: &str| {
            text.iter()
                .position(|line| {
                    line.trim_start()
                        .strip_prefix('/')
                        .and_then(|rest| rest.split_whitespace().next())
                        == Some(name)
                })
                .unwrap_or_else(|| panic!("missing /{name}: {text:?}"))
        };
        assert!(common < command_row("model") && command_row("exit") < approvals);
        assert!(approvals < command_row("approve") && command_row("logs") < runs);
        assert!(runs < command_row("runs") && command_row("block") > runs);
        // Headings are dim (muted), not command-coloured.
        assert_eq!(lines[common].style, theme::muted());

        let names: Vec<&str> = text
            .iter()
            .filter_map(|line| line.trim_start().strip_prefix('/'))
            .filter_map(|rest| rest.split_whitespace().next())
            .collect();
        let expected: Vec<&str> = crate::tui::completion::COMMANDS
            .iter()
            .map(|(name, _, _)| *name)
            .collect();
        assert_eq!(names, expected, "help follows the popup order");
        for hidden in ["ask", "all", "agents", "focus"] {
            assert!(!names.contains(&hidden), "/{hidden} must not be listed");
        }
    }

    #[test]
    fn team_roster_shows_resume_then_freshing_until_a_session_is_bound() {
        let mut state = ready_state();
        let text = drawer_team(&state, 100)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("session resume · —"), "{text}");

        state.apply(RuntimeEvent::MemberStatus {
            member: MemberId::new("builder"),
            status: MemberStatus::Running,
        });
        let text = drawer_team(&state, 100)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("session freshing · —"), "{text}");
    }

    #[test]
    fn team_editor_detail_field_colons_align_for_long_backend_labels() {
        let mut state = ready_state();
        state.toggle_drawer(Drawer::Team);

        let field_lines = drawer_team(&state, 100)
            .into_iter()
            .filter_map(|line| {
                let text = line
                    .spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>();
                text.contains(':').then_some(text)
            })
            .filter(|line| {
                ["name:", "backend:", "permissions:", "session id:"]
                    .iter()
                    .any(|label| line.contains(label))
            })
            .collect::<Vec<_>>();
        let colon_positions = field_lines
            .iter()
            .map(|line| {
                let byte = line.find(':').expect("field value separator");
                theme::display_width(&line[..byte])
            })
            .collect::<Vec<_>>();

        assert!(colon_positions.len() >= 4);
        assert!(colon_positions.windows(2).all(|pair| pair[0] == pair[1]));
    }

    #[test]
    fn team_editor_shows_field_focus_only_after_enter() {
        let mut state = ready_state();
        state.toggle_drawer(Drawer::Team);

        let lines = drawer_team(&state, 100);
        let marker = lines
            .iter()
            .flat_map(|line| &line.spans)
            .find(|span| span.content == "▶ ")
            .expect("selected member marker");
        let builder = lines
            .iter()
            .flat_map(|line| &line.spans)
            .find(|span| span.content.trim() == "Builder")
            .expect("selected member row");

        assert_eq!(marker.style.fg, Some(theme::warning_color()));
        assert_eq!(builder.style.bg, None);
        assert_eq!(builder.style.fg, Some(theme::emphasis_color()));

        state.handle_team_editor_key(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE,
        );
        let lines = drawer_team(&state, 100);
        let builder = lines
            .iter()
            .flat_map(|line| &line.spans)
            .find(|span| span.content.trim() == "Builder")
            .expect("selected member row while a field is focused");
        let selected_field = lines
            .iter()
            .flat_map(|line| &line.spans)
            .find(|span| span.content.starts_with(" ›") && span.content.contains("name:"))
            .expect("focused field in the details list");

        assert_eq!(builder.style.bg, None);
        assert_eq!(builder.style.fg, Some(theme::emphasis_color()));
        assert_eq!(selected_field.style.fg, Some(theme::warning_color()));
        assert!(
            !builder
                .style
                .add_modifier
                .contains(ratatui::style::Modifier::UNDERLINED)
        );
    }

    #[test]
    fn team_editor_header_rule_and_rows_share_column_boundaries() {
        let mut state = ready_state();
        state.toggle_drawer(Drawer::Team);
        let lines = drawer_team(&state, 100);
        let text = |line: &Line<'_>| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        };
        let header = lines
            .iter()
            .map(&text)
            .find(|line| line.contains("Member") && line.contains("Backend"))
            .expect("team table header");
        let rule = lines
            .iter()
            .map(&text)
            .find(|line| line.contains('┼'))
            .expect("team table rule");
        let row = lines
            .iter()
            .map(&text)
            .find(|line| line.contains("Builder") && line.contains("codex"))
            .expect("team table row");
        let positions = |line: &str, separator: char| {
            line.match_indices(separator)
                .map(|(byte, _)| theme::display_width(&line[..byte]))
                .collect::<Vec<_>>()
        };

        let expected = vec![17];
        assert_eq!(positions(&header, '│'), expected);
        assert_eq!(positions(&rule, '┼'), expected);
        assert_eq!(positions(&row, '│'), expected);
    }

    fn mode_ready_state() -> AppState {
        let mut state = AppState::new(Vec::new());
        state.apply(RuntimeEvent::Ready {
            modes: ModesConfig {
                plan: Some(PlanModeConfig {
                    builder: Some(MemberId::new("builder")),
                    ..PlanModeConfig::default()
                }),
                ..ModesConfig::default()
            },
            mode_overrides: Default::default(),
            suggested_verify: Some("cargo test".to_string()),
            team: "t".to_string(),
            workspace: "/tmp/ws".to_string(),
            default_target: Some(DefaultTarget::Member(MemberId::new("builder"))),
            runs: Vec::new(),
            members: vec![
                MemberSummary {
                    id: MemberId::new("builder"),
                    display_name: "Builder".to_string(),
                    backend: BackendKind::Codex,
                    role: "implementation".to_string(),
                    status: MemberStatus::Idle,
                    session: None,
                    cwd: "/tmp/ws".to_string(),
                    model: None,
                    effort: None,
                    sandbox: SandboxPolicy::WorkspaceWrite,
                    permission_mode: Some(PermissionMode::Default),
                    approvals_reviewer: crate::domain::team::CodexApprovalsReviewer::User,
                    session_policy: SessionPolicy::Resume,
                    parent: None,
                },
                MemberSummary {
                    id: MemberId::new("reviewer"),
                    display_name: "Reviewer".to_string(),
                    backend: BackendKind::Claude,
                    role: "review".to_string(),
                    status: MemberStatus::Idle,
                    session: None,
                    cwd: "/tmp/ws".to_string(),
                    model: None,
                    effort: None,
                    sandbox: SandboxPolicy::WorkspaceWrite,
                    permission_mode: Some(PermissionMode::Default),
                    approvals_reviewer: crate::domain::team::CodexApprovalsReviewer::User,
                    session_policy: SessionPolicy::Resume,
                    parent: None,
                },
            ],
        });
        state
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>()
    }

    #[test]
    fn mode_list_names_share_a_column() {
        let mut state = mode_ready_state();
        state.toggle_drawer(Drawer::Mode);
        let lines = drawer_mode(&state);
        let rows = TerminalMode::ALL
            .iter()
            .map(|mode| {
                lines
                    .iter()
                    .map(line_text)
                    .find(|text| {
                        !text.contains("this chat")
                            && (text.contains(&format!("▶ {}", mode.as_str()))
                                || text.contains(&format!("● {}", mode.as_str()))
                                || text.contains(&format!("  {}", mode.as_str())))
                    })
                    .unwrap_or_else(|| panic!("missing {mode} row"))
            })
            .collect::<Vec<_>>();
        let name_starts = rows
            .iter()
            .zip(TerminalMode::ALL.iter())
            .map(|(row, mode)| {
                let idx = row.find(mode.as_str()).expect("mode name");
                theme::display_width(&row[..idx])
            })
            .collect::<Vec<_>>();
        assert!(name_starts.windows(2).all(|pair| pair[0] == pair[1]));
    }

    #[test]
    fn mode_list_names_use_mode_identity_colors() {
        let mut state = mode_ready_state();
        state.toggle_drawer(Drawer::Mode);
        let lines = drawer_mode(&state);

        for mode in TerminalMode::ALL {
            let span = lines
                .iter()
                .flat_map(|line| &line.spans)
                .find(|span| span.content.trim() == mode.as_str())
                .unwrap_or_else(|| panic!("missing {mode} name"));
            assert_eq!(span.style.fg, Some(theme::mode_color(mode)));
        }
    }

    #[test]
    fn mode_editor_shows_field_focus_only_after_enter() {
        let mut state = mode_ready_state();
        state.toggle_drawer(Drawer::Mode);
        let before = drawer_mode(&state);
        assert!(
            before
                .iter()
                .all(|line| !line_text(line).contains("builder:"))
        );

        state.handle_mode_editor_key(
            crossterm::event::KeyCode::Down,
            crossterm::event::KeyModifiers::NONE,
        );
        state.handle_mode_editor_key(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE,
        );
        let lines = drawer_mode(&state);
        let focused = lines
            .iter()
            .flat_map(|line| &line.spans)
            .find(|span| span.content.contains("builder:") && span.content.contains('›'))
            .expect("focused builder field");
        assert_eq!(focused.style.fg, Some(theme::warning_color()));
        let colons = lines
            .iter()
            .map(line_text)
            .filter(|text| {
                ["builder:", "reviewer:", "max_iterations:", "reviewer_hint:"]
                    .iter()
                    .any(|label| text.contains(label))
            })
            .map(|text| {
                let byte = text.find(':').expect("field colon");
                theme::display_width(&text[..byte])
            })
            .collect::<Vec<_>>();
        assert!(colons.len() >= 4);
        assert!(colons.windows(2).all(|pair| pair[0] == pair[1]));
    }
}
