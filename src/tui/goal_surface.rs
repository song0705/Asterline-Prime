//! How a goal (`/goal`) shows on screen: the dim status line above the
//! composer and the compact marker row the goal loop leaves in the chat when it
//! starts a turn on its own.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::domain::goal::{GoalState, GoalStatus, format_goal_age, unix_now};
use crate::tui::app_state::AppState;
use crate::tui::theme;

/// Left margin shared with the transcript rows.
const INDENT: &str = "  ";

/// `↻ goal · turn 4`: the row standing in for a continuation prompt.
pub(crate) fn marker_text(turn: u32) -> String {
    format!("↻ goal · turn {turn}")
}

/// The goal's one status line, cut to `width` columns, or `None` for a
/// complete goal (nothing to show once it is done).
///
/// Active: `◎ Goal · <objective> · turn N · 12m`. Paused:
/// `◎ Goal paused · <reason> (/goal resume)`. The stored reason starts with
/// "Paused", which the line already says, so that word is dropped.
pub(crate) fn status_text(goal: &GoalState, now: u64, width: usize) -> Option<String> {
    let text = match goal.status {
        GoalStatus::Complete => return None,
        GoalStatus::Active => {
            let tail = format!(
                " · turn {} · {}",
                goal.turns,
                format_goal_age(goal.age_secs(now))
            );
            let head = "◎ Goal · ";
            let room =
                width.saturating_sub(theme::display_width(head) + theme::display_width(&tail));
            let objective = theme::truncate_width(&goal.objective, room.max(1));
            format!("{head}{objective}{tail}")
        }
        GoalStatus::Paused => {
            let reason = goal
                .last_reason
                .as_deref()
                .map(|reason| {
                    let reason = reason.trim();
                    reason
                        .strip_prefix("Paused: ")
                        .or_else(|| reason.strip_prefix("Paused "))
                        .unwrap_or(reason)
                })
                .filter(|reason| !reason.is_empty());
            let hint = reason.is_none_or(|reason| !reason.contains("/goal resume"));
            let head = "◎ Goal paused";
            let tail = if hint { " (/goal resume)" } else { "" };
            match reason {
                Some(reason) => {
                    let room = width.saturating_sub(
                        theme::display_width(head) + 3 + theme::display_width(tail),
                    );
                    format!(
                        "{head} · {}{tail}",
                        theme::truncate_width(reason, room.max(1))
                    )
                }
                None => format!("{head}{tail}"),
            }
        }
    };
    Some(theme::clip_width(&text, width.max(1)))
}

/// Paint the goal line into its one-row `area`.
pub(crate) fn render(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    if area.height == 0 {
        return;
    }
    let Some(goal) = state.visible_goal() else {
        return;
    };
    let width = (area.width as usize).saturating_sub(INDENT.len() + 1);
    let Some(text) = status_text(goal, unix_now(), width) else {
        return;
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw(INDENT),
            Span::styled(text, theme::dim()),
        ])),
        Rect::new(area.x, area.y, area.width, 1),
    );
}

/// Rows the goal line takes: one while a goal is shown, else none.
pub(crate) fn height(state: &AppState) -> u16 {
    u16::from(state.visible_goal().is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::team::MemberId;

    fn goal(objective: &str, status: GoalStatus, reason: Option<&str>) -> GoalState {
        GoalState {
            objective: objective.to_string(),
            status,
            member: MemberId::new("main"),
            turns: 4,
            created_at: 1_000,
            last_reason: reason.map(str::to_string),
            no_progress_streak: 0,
        }
    }

    #[test]
    fn active_line_shows_objective_turn_and_age() {
        let line = status_text(
            &goal("fix the flaky test", GoalStatus::Active, None),
            1_000 + 725,
            80,
        );
        assert_eq!(
            line.as_deref(),
            Some("◎ Goal · fix the flaky test · turn 4 · 12m")
        );
    }

    #[test]
    fn a_long_objective_is_cut_to_fit_and_keeps_turn_and_age() {
        let long = "x".repeat(200);
        let line = status_text(&goal(&long, GoalStatus::Active, None), 1_060, 40).unwrap();
        assert_eq!(theme::display_width(&line), 40);
        assert!(line.contains('…'));
        assert!(line.ends_with(" · turn 4 · 1m"), "{line}");
    }

    #[test]
    fn paused_line_gives_the_reason_and_the_resume_hint_once() {
        let interrupted = status_text(
            &goal("g", GoalStatus::Paused, Some("Paused by interrupt")),
            2_000,
            80,
        );
        assert_eq!(
            interrupted.as_deref(),
            Some("◎ Goal paused · by interrupt (/goal resume)")
        );
        let restored = status_text(
            &goal(
                "g",
                GoalStatus::Paused,
                Some("Paused: restored — /goal resume to continue"),
            ),
            2_000,
            80,
        );
        assert_eq!(
            restored.as_deref(),
            Some("◎ Goal paused · restored — /goal resume to continue")
        );
        let bare = status_text(&goal("g", GoalStatus::Paused, None), 2_000, 80);
        assert_eq!(bare.as_deref(), Some("◎ Goal paused (/goal resume)"));
    }

    #[test]
    fn a_complete_goal_has_no_line() {
        assert_eq!(
            status_text(&goal("g", GoalStatus::Complete, Some("done")), 2_000, 80),
            None
        );
    }
}
