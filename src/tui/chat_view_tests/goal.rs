//! The goal's status line above the composer and the marker row the goal loop
//! leaves in the chat.

use super::super::*;
use super::*;
use crate::domain::goal::{GoalState, GoalStatus, unix_now};

fn main_id() -> MemberId {
    MemberId::new("main")
}

fn scout() -> MemberId {
    MemberId::new("scout")
}

fn goal(status: GoalStatus, reason: Option<&str>) -> GoalState {
    GoalState {
        objective: "make the flaky test stable".to_string(),
        status,
        member: main_id(),
        turns: 4,
        created_at: unix_now() - 725,
        last_reason: reason.map(str::to_string),
        no_progress_streak: 0,
    }
}

fn state_with_goal(goal: Option<GoalState>, focus: Option<MemberId>) -> AppState {
    state_over_chat(
        vec![ChatItem::GoalMarker {
            member: main_id(),
            turn: 4,
        }],
        goal,
        focus,
    )
}

fn state_over_chat(
    chat: Vec<ChatItem>,
    goal: Option<GoalState>,
    focus: Option<MemberId>,
) -> AppState {
    let mut state = AppState::new(chat);
    let mut child = member_summary(
        "scout",
        "Scout",
        BackendKind::Codex,
        "explore",
        MemberStatus::Idle,
    );
    child.parent = Some(main_id());
    state.apply(RuntimeEvent::Ready {
        modes: Default::default(),
        mode_overrides: Default::default(),
        suggested_verify: None,
        team: "t".to_string(),
        workspace: "/tmp/ws".to_string(),
        default_target: Some(DefaultTarget::Member(main_id())),
        runs: Vec::new(),
        members: vec![
            member_summary(
                "main",
                "Main",
                BackendKind::Claude,
                "general",
                MemberStatus::Idle,
            ),
            child,
        ],
    });
    state.apply(RuntimeEvent::GoalUpdated { goal });
    state.set_focus(focus);
    state
}

fn screen(state: &AppState) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
    terminal
        .draw(|frame| {
            let _ = render(frame, state);
        })
        .unwrap();
    let buffer = terminal.backend().buffer();
    let area = buffer.area;
    (0..area.height)
        .map(|y| {
            (0..area.width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect::<String>()
        })
        .collect()
}

fn history(state: &AppState, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    render_chat_history(state, width, 0, &mut lines);
    plain_text(&lines)
}

fn row_with(rows: &[String], needle: &str) -> Option<usize> {
    rows.iter().position(|row| row.contains(needle))
}

#[test]
fn an_active_goal_shows_one_dim_line_above_the_composer() {
    let state = state_with_goal(Some(goal(GoalStatus::Active, None)), Some(main_id()));

    let rows = screen(&state);

    let line = row_with(&rows, "◎ Goal ·").expect("the goal line");
    assert!(
        rows[line].contains("◎ Goal · make the flaky test stable · turn 4 · 12m"),
        "{}",
        rows[line]
    );
    // The composer's input band comes after it; the chat is above.
    let composer = rows
        .iter()
        .rposition(|row| row.contains("Ask") || row.contains('›') || row.contains('>'))
        .unwrap_or(rows.len() - 2);
    assert!(line < composer, "{rows:#?}");
    assert_eq!(rows.iter().filter(|row| row.contains("◎ Goal")).count(), 1);
}

#[test]
fn a_paused_goal_says_why_and_how_to_resume() {
    let state = state_with_goal(
        Some(goal(GoalStatus::Paused, Some("Paused by interrupt"))),
        None,
    );

    let rows = screen(&state);

    let line = row_with(&rows, "◎ Goal paused").expect("the paused goal line");
    assert!(
        rows[line].contains("◎ Goal paused · by interrupt (/goal resume)"),
        "{}",
        rows[line]
    );
}

#[test]
fn a_complete_or_missing_goal_shows_no_line() {
    for goal in [None, Some(goal(GoalStatus::Complete, Some("done")))] {
        let state = state_with_goal(goal, Some(main_id()));
        assert!(row_with(&screen(&state), "◎ Goal").is_none());
    }
}

#[test]
fn the_line_shows_in_the_main_and_all_views_but_not_in_a_sub_agent_view() {
    for (focus, shown) in [
        (Some(main_id()), true),
        (None, true),
        (Some(scout()), false),
    ] {
        let state = state_with_goal(Some(goal(GoalStatus::Active, None)), focus.clone());
        assert_eq!(
            row_with(&screen(&state), "◎ Goal").is_some(),
            shown,
            "{focus:?}"
        );
    }
}

#[test]
fn the_goal_line_does_not_duplicate_the_working_indicator() {
    let mut state = state_with_goal(Some(goal(GoalStatus::Active, None)), Some(main_id()));
    state.apply(RuntimeEvent::MemberStatus {
        member: main_id(),
        status: MemberStatus::Running,
    });

    let rows = screen(&state);

    let working: Vec<_> = rows
        .iter()
        .enumerate()
        .filter(|(_, row)| row.contains("Working"))
        .collect();
    assert_eq!(working.len(), 1, "{rows:#?}");
    assert_eq!(rows.iter().filter(|row| row.contains("◎ Goal")).count(), 1);
    // The goal line is below the Working row (which ends the transcript).
    assert!(
        working[0].0 < row_with(&rows, "◎ Goal").unwrap(),
        "{rows:#?}"
    );
}

#[test]
fn the_marker_row_shows_in_the_main_and_all_views_only() {
    for (focus, shown) in [
        (Some(main_id()), true),
        (None, true),
        (Some(scout()), false),
    ] {
        let state = state_with_goal(None, focus.clone());
        let lines = history(&state, 80);
        assert_eq!(
            lines.iter().any(|line| line == "  ↻ goal · turn 4"),
            shown,
            "{focus:?}: {lines:?}"
        );
    }
}

#[test]
fn marker_events_become_chat_rows_and_goal_events_replace_the_goal() {
    let mut state = state_with_goal(None, None);
    state.apply(RuntimeEvent::GoalMarker {
        member: main_id(),
        turn: 5,
    });
    assert!(matches!(
        state.chat().last(),
        Some(ChatItem::GoalMarker { turn: 5, .. })
    ));

    state.apply(RuntimeEvent::GoalUpdated {
        goal: Some(goal(GoalStatus::Active, None)),
    });
    assert_eq!(state.goal().map(|g| g.turns), Some(4));
    state.apply(RuntimeEvent::GoalUpdated { goal: None });
    assert!(state.goal().is_none());

    // `/new` forgets it as well.
    state.apply(RuntimeEvent::GoalUpdated {
        goal: Some(goal(GoalStatus::Active, None)),
    });
    state.apply(RuntimeEvent::SessionReset);
    assert!(state.goal().is_none());
}

#[test]
fn each_goal_turn_keeps_its_marker_above_its_own_reply() {
    let reply = |text: &str| ChatItem::Agent {
        member: main_id(),
        display_name: "Main".to_string(),
        backend: BackendKind::Claude,
        text: text.to_string(),
    };
    let marker = |turn| ChatItem::GoalMarker {
        member: main_id(),
        turn,
    };
    let state = state_over_chat(
        vec![
            reply("first"),
            marker(2),
            reply("second"),
            marker(3),
            reply("third"),
        ],
        None,
        Some(main_id()),
    );

    let lines = history(&state, 80);
    let positions: Vec<usize> = ["first", "turn 2", "second", "turn 3", "third"]
        .iter()
        .map(|needle| lines.iter().position(|l| l.contains(needle)).unwrap())
        .collect();

    assert!(positions.windows(2).all(|w| w[0] < w[1]), "{lines:?}");
}
