//! The top header (the open window's agent) and the bottom footer
//! (search prompt, alerts, running status, run hint, or key hints).

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::domain::event::MemberStatus;
use crate::domain::mode::TerminalMode;
use crate::domain::team::DefaultTarget;
use crate::tui::app_state::{AppState, MemberView, member_status_is_active};
use crate::tui::runs_view::run_footer_hint;
use crate::tui::theme;
use crate::tui::theme::{clip_width_start, display_width, truncate_width};

/// The workspace for the title row: `$HOME` abbreviated to `~`.
pub(crate) fn display_workspace(workspace: &str, home: Option<&str>) -> String {
    let Some(home) = home.map(|home| home.trim_end_matches(['/', '\\'])) else {
        return workspace.to_string();
    };
    if home.is_empty() {
        return workspace.to_string();
    }
    match workspace.strip_prefix(home) {
        Some("") => "~".to_string(),
        Some(rest) if rest.starts_with(['/', '\\']) => format!("~{rest}"),
        _ => workspace.to_string(),
    }
}

/// Columns kept clear between the key hints and the workspace path.
const FOOTER_PATH_GAP: usize = 2;

/// Header: one line naming the agent whose window is open: a status glyph,
/// its name, and its model label. The All view shows the default member. No
/// title row and no rule: the product name and version live in `/help`, the
/// path in the footer, the active mode in the footer hints.
pub(crate) fn render_header(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let mut chip = vec![Span::raw("  ")];
    match header_member(state) {
        Some(member) => push_member_chip(&mut chip, state, member),
        None => chip.push(Span::styled("starting…", theme::muted())),
    }
    frame.render_widget(Paragraph::new(Line::from(chip)), area);
}

/// The member the header names: the focused one, or in the All view the
/// default member (falling back to the first top-level member).
fn header_member(state: &AppState) -> Option<&MemberView> {
    let members = state.members();
    if let Some(focus) = state.focus()
        && let Some(member) = members.iter().find(|member| &member.id == focus)
    {
        return Some(member);
    }
    if let Some(DefaultTarget::Member(id)) = state.default_target()
        && let Some(member) = members.iter().find(|member| &member.id == id)
    {
        return Some(member);
    }
    members
        .iter()
        .find(|member| member.parent.is_none())
        .or_else(|| members.first())
}

fn push_member_chip(chips: &mut Vec<Span<'static>>, state: &AppState, member: &MemberView) {
    chips.push(Span::styled(
        format!("{} ", status_glyph(member.status)),
        theme::member_dot_style(member.status),
    ));
    chips.push(Span::styled(
        member.display_name.clone(),
        theme::backend_bold_shaded(member.backend, state.member_color_index(&member.id)),
    ));
    chips.push(Span::styled(
        format!(" · {}", state.member_model_label(member)),
        theme::muted(),
    ));
}

/// Compact status marker for a member chip (styled by `theme::member_dot_style`: lit when active, dim when idle).
pub(crate) fn status_glyph(status: MemberStatus) -> &'static str {
    match status {
        MemberStatus::Idle
        | MemberStatus::Queued
        | MemberStatus::Running
        | MemberStatus::Waiting => "●",
        MemberStatus::NeedsApproval => "!",
        MemberStatus::Failed => "✗",
    }
}

pub(crate) fn render_footer(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    // A disconnected runtime is terminal for this TUI instance. Do not leave
    // stale run/verification hints suggesting cancellation is still available.
    if !state.runtime_available() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "runtime stopped · input disabled · Ctrl+C quit",
                theme::error(),
            ))),
            area,
        );
        return;
    }

    // Reverse history search (Ctrl+R) takes over the footer while active.
    if let Some((query, matched)) = state.history_search() {
        let mut spans = vec![
            Span::styled("(reverse-search) ", theme::bold(theme::accent_color())),
            Span::styled(format!("`{query}`"), theme::bold(theme::emphasis_color())),
            Span::styled(" → ", theme::muted()),
        ];
        match matched {
            Some(text) => spans.push(Span::styled(
                truncate_width(text, area.width as usize),
                theme::text(),
            )),
            None => spans.push(Span::styled(
                "no match (Esc to cancel)",
                theme::muted_italic(),
            )),
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
        return;
    }

    // Transcript find (`/find`) takes over the footer while active.
    if let Some((query, current, total)) = state.find() {
        let mut spans = vec![Span::styled(
            format!("find: \"{query}\" ({current}/{total})"),
            theme::accent(),
        )];
        if total == 0 {
            spans.push(Span::styled(" · no matches", theme::accent()));
        } else {
            spans.push(Span::styled(" · n/p jump · Esc clear", theme::accent()));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
        return;
    }

    let mut parts = Vec::new();
    if state.paused_routes() > 0 {
        parts.push(Span::styled(
            format!("● {} paused route(s) · /retry", state.paused_routes()),
            theme::warning_bold(),
        ));
    }
    if !state.pending_approvals().is_empty() {
        if !parts.is_empty() {
            parts.push(Span::raw("   "));
        }
        parts.push(Span::styled(
            format!(
                "● {} pending approval(s) · y agree · n deny",
                state.pending_approvals().len()
            ),
            theme::warning_bold(),
        ));
    }

    // "Working" lives in one place only: the live row at the end of the viewed
    // agent's transcript. The footer just says which keys apply meanwhile.
    let viewed_busy = match state.focus() {
        Some(focus) => state
            .members()
            .iter()
            .any(|member| &member.id == focus && member_status_is_active(member.status)),
        None => state
            .members()
            .iter()
            .any(|member| member_status_is_active(member.status)),
    };
    if viewed_busy {
        if !parts.is_empty() {
            parts.push(Span::raw("   "));
        }
        parts.push(Span::styled(
            "Esc interrupt · ← agents · /help",
            theme::muted(),
        ));
        let queued = state.queued_prompt_count();
        if queued > 0 {
            parts.push(Span::raw("   "));
            parts.push(Span::styled(
                format!("{queued} queued · Esc send · Shift+← edit"),
                theme::warning_bold(),
            ));
        }
    } else if let Some((text, color)) = run_footer_hint(state) {
        if !parts.is_empty() {
            parts.push(Span::raw("   "));
        }
        parts.push(Span::styled(text, theme::bold(color)));
    } else if parts.is_empty() {
        // Idle: one short, faint key-hint line.
        parts.push(Span::styled(
            "Enter send · ← agents · /help",
            theme::muted(),
        ));
    }

    // The active collaboration mode leads the hints (normal mode shows nothing).
    if state.active_mode() != TerminalMode::Normal {
        parts.insert(
            0,
            Span::styled(format!("{} mode · ", state.active_mode()), theme::muted()),
        );
    }

    // The workspace, `~`-abbreviated and clipped from the left so the project
    // folder stays visible, dim at the right end.
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok();
    let workspace = display_workspace(state.workspace(), home.as_deref());
    let used: usize = parts
        .iter()
        .map(|span| display_width(span.content.as_ref()))
        .sum();
    let width = area.width as usize;
    let room = width
        .saturating_sub(used)
        .saturating_sub(FOOTER_PATH_GAP + 1);
    if !workspace.is_empty() && room >= 8 {
        let path = clip_width_start(&workspace, room);
        let gap = width - used - display_width(&path) - 1;
        parts.push(Span::raw(" ".repeat(gap)));
        parts.push(Span::styled(
            path,
            theme::muted().add_modifier(Modifier::DIM),
        ));
    }

    frame.render_widget(Paragraph::new(Line::from(parts)), area);
}
