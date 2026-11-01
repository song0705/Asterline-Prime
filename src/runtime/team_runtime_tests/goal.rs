use super::super::*;
use super::*;
use crate::domain::goal::{
    GoalAction, GoalState, GoalStatus, REASON_INTERRUPT, REASON_NO_PROGRESS, REASON_RESTORED,
};

fn main_id() -> MemberId {
    MemberId::new("main")
}

fn id(value: &str) -> MemberId {
    MemberId::new(value)
}

/// `main` (Claude) plus an unrelated top-level member, `main` is the default.
fn goal_runtime() -> TeamRuntime {
    TeamRuntime::new(goal_config(), SqliteStore::in_memory().unwrap()).with_approvals(false)
}

fn goal_config() -> TeamConfig {
    let mut config = TeamConfig::new("goal", "/tmp/ws")
        .with_member(TeamMember::new(
            "main",
            "Main",
            BackendKind::Claude,
            "general",
        ))
        .with_member(TeamMember::new(
            "other",
            "Other",
            BackendKind::Codex,
            "impl",
        ));
    config.default_target = Some(DefaultTarget::Member(main_id()));
    config
}

fn goal_command(action: GoalAction) -> UiCommand {
    UiCommand::Goal { action }
}

fn set_goal(rt: &mut TeamRuntime, objective: &str) -> RuntimeStep {
    rt.on_ui_command(goal_command(GoalAction::Set(objective.to_string())))
}

fn goal_of(rt: &TeamRuntime) -> GoalState {
    rt.goal.clone().expect("a goal is set")
}

/// The main agent's turn ends without any output.
fn exit_silently(rt: &mut TeamRuntime) -> RuntimeStep {
    rt.on_agent_event(
        &main_id(),
        AgentEvent::Exited {
            code: Some(0),
            ok: true,
        },
    )
}

fn notices(step: &RuntimeStep) -> Vec<String> {
    step.events
        .iter()
        .filter_map(|event| match event {
            RuntimeEvent::Notice(text) | RuntimeEvent::MemberNotice { text, .. } => {
                Some(text.clone())
            }
            _ => None,
        })
        .collect()
}

fn has_notice(step: &RuntimeStep, needle: &str) -> bool {
    notices(step).iter().any(|text| text.contains(needle))
}

fn markers(step: &RuntimeStep) -> Vec<u32> {
    step.events
        .iter()
        .filter_map(|event| match event {
            RuntimeEvent::GoalMarker { turn, .. } => Some(*turn),
            _ => None,
        })
        .collect()
}

fn user_bands(step: &RuntimeStep) -> Vec<String> {
    step.events
        .iter()
        .filter_map(|event| match event {
            RuntimeEvent::UserMessage { body, .. } => Some(body.clone()),
            _ => None,
        })
        .collect()
}

fn finished_turns(step: &RuntimeStep) -> usize {
    step.events
        .iter()
        .filter(|event| matches!(event, RuntimeEvent::TurnFinished { .. }))
        .count()
}

fn goal_updates(step: &RuntimeStep) -> Vec<Option<GoalState>> {
    step.events
        .iter()
        .filter_map(|event| match event {
            RuntimeEvent::GoalUpdated { goal } => Some(goal.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn setting_a_goal_shows_the_objective_and_sends_the_goal_prompt_to_main() {
    let mut rt = goal_runtime();

    let step = set_goal(&mut rt, "  make the tests pass fake-goal-3  ");

    assert_eq!(step.actions.len(), 1);
    assert_eq!(step.actions[0].member, main_id());
    let prompt = &step.actions[0].prompt;
    assert!(prompt.contains("<objective>\nmake the tests pass fake-goal-3\n</objective>"));
    assert!(prompt.contains("user-provided data"));
    assert!(prompt.contains("@@goal_complete"));
    assert!(!prompt.contains("budget"));
    // The chat shows the objective as an ordinary user message.
    assert_eq!(user_bands(&step), vec!["make the tests pass fake-goal-3"]);
    let goal = goal_of(&rt);
    assert_eq!(goal.status, GoalStatus::Active);
    assert_eq!(goal.turns, 1);
    assert_eq!(goal.member, main_id());
    assert_eq!(goal_updates(&step).last().unwrap().as_ref(), Some(&goal));
    // It is saved with the chat.
    assert_eq!(
        rt.store.goal_for(rt.store.active_conversation()).unwrap(),
        Some(goal)
    );
}

#[test]
fn the_goal_goes_to_the_first_top_level_member_when_there_is_no_main() {
    let mut config = TeamConfig::new("goal", "/tmp/ws")
        .with_member(TeamMember::new(
            "builder",
            "Builder",
            BackendKind::Codex,
            "impl",
        ))
        .with_member(TeamMember::new(
            "reviewer",
            "Reviewer",
            BackendKind::Claude,
            "review",
        ));
    config.default_target = Some(DefaultTarget::Member(id("reviewer")));
    let mut rt = TeamRuntime::new(config, SqliteStore::in_memory().unwrap());

    let step = set_goal(&mut rt, "ship it");

    assert_eq!(step.actions[0].member, id("builder"));
    assert_eq!(goal_of(&rt).member, id("builder"));
}

#[test]
fn the_loop_continues_until_the_agent_completes_the_goal() {
    let mut rt = goal_runtime();
    set_goal(&mut rt, "fix things");

    // Turn 1 ends: the loop sends turn 2 without a user message.
    let step = complete_ok(&mut rt, &main_id(), "first pass done");
    assert_eq!(step.actions.len(), 1);
    assert_eq!(step.actions[0].member, main_id());
    assert!(
        step.actions[0]
            .prompt
            .contains("Continue working toward the active goal")
    );
    assert!(
        step.actions[0]
            .prompt
            .contains("<objective>\nfix things\n</objective>")
    );
    assert!(step.actions[0].prompt.contains("- turn: 2"));
    assert!(user_bands(&step).is_empty());
    assert_eq!(markers(&step), vec![2]);
    assert_eq!(finished_turns(&step), 1);

    let step = complete_ok(&mut rt, &main_id(), "second pass done");
    assert_eq!(markers(&step), vec![3]);
    assert_eq!(step.actions.len(), 1);

    // Turn 3 declares the goal done: no further continuation.
    let step = complete_ok(
        &mut rt,
        &main_id(),
        "All verified.\n@@goal_complete {\"summary\":\"fake goal done\"}",
    );
    assert!(step.actions.is_empty(), "{:?}", step.events);
    assert!(markers(&step).is_empty());
    assert!(has_notice(&step, "Goal complete: fake goal done"));
    let goal = goal_of(&rt);
    assert_eq!(goal.status, GoalStatus::Complete);
    assert_eq!(goal.turns, 3);
    assert_eq!(goal.last_reason.as_deref(), Some("fake goal done"));
    // The envelope is not shown.
    let shown: Vec<_> = step
        .events
        .iter()
        .filter_map(|event| match event {
            RuntimeEvent::MessageCompleted { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(shown, vec!["All verified."]);
    assert_eq!(
        rt.store
            .goal_for(rt.store.active_conversation())
            .unwrap()
            .unwrap()
            .status,
        GoalStatus::Complete
    );
}

#[test]
fn goal_complete_without_a_summary_still_completes() {
    let mut rt = goal_runtime();
    set_goal(&mut rt, "fix things");

    let step = complete_ok(&mut rt, &main_id(), "@@goal_complete");

    assert!(step.actions.is_empty());
    assert!(notices(&step).contains(&"Goal complete".to_string()));
    assert_eq!(goal_of(&rt).status, GoalStatus::Complete);
}

#[test]
fn goal_complete_from_another_member_or_for_a_paused_goal_is_ignored() {
    let mut rt = goal_runtime();
    set_goal(&mut rt, "fix things");
    // `other` is a teammate, not the goal's member.
    let started = rt.on_ui_command(UiCommand::UserMessage {
        target: MessageTarget::Member(id("other")),
        body: "@other hi".to_string(),
    });
    assert_eq!(started.actions[0].member, id("other"));
    complete_ok(
        &mut rt,
        &id("other"),
        "@@goal_complete {\"summary\":\"nope\"}",
    );
    assert_eq!(goal_of(&rt).status, GoalStatus::Active);

    // A paused goal is not completed behind the user's back either.
    rt.on_ui_command(goal_command(GoalAction::Pause));
    complete_ok(&mut rt, &main_id(), "@@goal_complete");
    assert_eq!(goal_of(&rt).status, GoalStatus::Paused);
}

#[test]
fn a_user_message_mid_goal_runs_normally_and_the_loop_continues_after_it() {
    let mut rt = goal_runtime();
    set_goal(&mut rt, "fix things");

    // The user speaks while the first goal turn is running: it queues.
    let queued = rt.on_ui_command(user("how is it going?"));
    assert!(queued.actions.is_empty());

    // The goal turn ends; the user's message goes next, not a continuation.
    let step = complete_ok(&mut rt, &main_id(), "working");
    assert_eq!(step.actions.len(), 1);
    assert!(step.actions[0].prompt.contains("how is it going?"));
    assert!(!step.actions[0].prompt.contains("Continue working"));
    assert_eq!(user_bands(&step), vec!["how is it going?"]);
    assert!(markers(&step).is_empty());

    // Its turn ends; now the loop goes on.
    let step = complete_ok(&mut rt, &main_id(), "all good, carrying on");
    assert_eq!(step.actions.len(), 1);
    assert!(step.actions[0].prompt.contains("Continue working"));
    assert_eq!(markers(&step), vec![2]);
}

#[test]
fn setting_a_goal_again_replaces_the_objective_and_queues_behind_a_busy_main() {
    let mut rt = goal_runtime();
    set_goal(&mut rt, "old objective");

    let step = set_goal(&mut rt, "new objective");

    // Main is mid-turn: the edit waits as its next turn.
    assert!(step.actions.is_empty());
    assert!(has_notice(&step, "Goal objective updated"));
    let goal = goal_of(&rt);
    assert_eq!(goal.objective, "new objective");
    assert_eq!(goal.status, GoalStatus::Active);
    assert_eq!(goal.turns, 2);

    let step = complete_ok(&mut rt, &main_id(), "done with the old one");
    assert_eq!(step.actions.len(), 1);
    let prompt = &step.actions[0].prompt;
    assert!(prompt.contains("objective was edited by the user"));
    assert!(prompt.contains("<objective>\nnew objective\n</objective>"));
    assert_eq!(user_bands(&step), vec!["new objective"]);

    let step = complete_ok(&mut rt, &main_id(), "on it");
    assert!(
        step.actions[0]
            .prompt
            .contains("<objective>\nnew objective\n</objective>")
    );
    assert_eq!(goal_of(&rt).turns, 3);
}

#[test]
fn replacing_a_paused_goal_reactivates_it_and_replacing_a_complete_one_starts_over() {
    let mut rt = goal_runtime();
    set_goal(&mut rt, "first");
    rt.on_ui_command(goal_command(GoalAction::Pause));
    complete_ok(&mut rt, &main_id(), "stopping");
    assert_eq!(goal_of(&rt).status, GoalStatus::Paused);

    // Main is idle: the edited prompt starts right away.
    let step = set_goal(&mut rt, "second");
    assert_eq!(step.actions.len(), 1);
    assert!(step.actions[0].prompt.contains("objective was edited"));
    assert_eq!(goal_of(&rt).status, GoalStatus::Active);
    assert_eq!(goal_of(&rt).turns, 2);

    complete_ok(&mut rt, &main_id(), "@@goal_complete");
    assert_eq!(goal_of(&rt).status, GoalStatus::Complete);

    let step = set_goal(&mut rt, "third");
    assert!(
        step.actions[0]
            .prompt
            .contains("The user set a persistent goal")
    );
    let goal = goal_of(&rt);
    assert_eq!((goal.status, goal.turns), (GoalStatus::Active, 1));
}

#[test]
fn pause_stops_the_loop_after_the_current_turn_and_resume_restarts_it() {
    let mut rt = goal_runtime();
    set_goal(&mut rt, "fix things");

    let step = rt.on_ui_command(goal_command(GoalAction::Pause));
    assert!(has_notice(&step, "Goal paused"));
    let goal = goal_of(&rt);
    assert_eq!(goal.status, GoalStatus::Paused);

    // The running turn is not interrupted, and no continuation follows it.
    let step = complete_ok(&mut rt, &main_id(), "finished the turn");
    assert!(step.actions.is_empty());
    assert!(markers(&step).is_empty());

    // Resume while idle continues at once.
    let step = rt.on_ui_command(goal_command(GoalAction::Resume));
    assert_eq!(step.actions.len(), 1);
    assert!(step.actions[0].prompt.contains("Continue working"));
    assert_eq!(markers(&step), vec![2]);
    assert_eq!(goal_of(&rt).status, GoalStatus::Active);
    assert_eq!(goal_of(&rt).last_reason, None);

    // Resuming something already active is a notice, not a second turn.
    let step = rt.on_ui_command(goal_command(GoalAction::Resume));
    assert!(step.actions.is_empty());
    assert!(has_notice(&step, "already active"));
}

#[test]
fn resume_while_main_is_busy_waits_for_the_turn_to_end() {
    let mut rt = goal_runtime();
    set_goal(&mut rt, "fix things");
    rt.on_ui_command(goal_command(GoalAction::Pause));

    let step = rt.on_ui_command(goal_command(GoalAction::Resume));
    assert!(step.actions.is_empty());

    let step = complete_ok(&mut rt, &main_id(), "turn over");
    assert_eq!(step.actions.len(), 1);
    assert!(step.actions[0].prompt.contains("Continue working"));
}

#[test]
fn clear_removes_the_goal_and_its_row() {
    let mut rt = goal_runtime();
    set_goal(&mut rt, "fix things");
    let conversation = rt.store.active_conversation();

    let step = rt.on_ui_command(goal_command(GoalAction::Clear));

    assert!(has_notice(&step, "Goal cleared"));
    assert_eq!(goal_updates(&step).last(), Some(&None));
    assert!(rt.goal.is_none());
    assert_eq!(rt.store.goal_for(conversation).unwrap(), None);
    let step = complete_ok(&mut rt, &main_id(), "turn over");
    assert!(step.actions.is_empty());

    let step = rt.on_ui_command(goal_command(GoalAction::Clear));
    assert!(has_notice(&step, "no goal"));
}

#[test]
fn show_reports_objective_status_turns_and_age() {
    let mut rt = goal_runtime();
    let step = rt.on_ui_command(goal_command(GoalAction::Show));
    assert!(has_notice(&step, "no goal is set"));

    set_goal(&mut rt, "fix  the\nflaky test");
    rt.goal.as_mut().unwrap().created_at = unix_now() - 125;
    let step = rt.on_ui_command(goal_command(GoalAction::Show));
    let text = notices(&step).join("\n");
    assert!(text.contains("Goal (active): fix the flaky test"), "{text}");
    assert!(text.contains("1 turn"), "{text}");
    assert!(text.contains("2m"), "{text}");

    rt.on_ui_command(goal_command(GoalAction::Pause));
    let text = notices(&rt.on_ui_command(goal_command(GoalAction::Show))).join("\n");
    assert!(text.contains("paused"), "{text}");
    assert!(text.contains("/goal resume"), "{text}");
}

#[test]
fn the_objective_must_be_non_empty_and_at_most_4000_characters() {
    let mut rt = goal_runtime();

    let step = set_goal(&mut rt, "   \n ");
    assert!(step.actions.is_empty());
    assert!(has_notice(&step, "Usage: /goal <objective>"));

    let step = set_goal(&mut rt, &"x".repeat(4001));
    assert!(step.actions.is_empty());
    assert!(has_notice(&step, "too long"));
    assert!(rt.goal.is_none());

    let step = set_goal(&mut rt, &"x".repeat(4000));
    assert_eq!(step.actions.len(), 1);
}

#[test]
fn interrupting_main_pauses_the_goal() {
    let mut rt = goal_runtime();
    set_goal(&mut rt, "fix things");

    rt.on_ui_command(UiCommand::Cancel { member: None });
    let goal = goal_of(&rt);
    assert_eq!(goal.status, GoalStatus::Paused);
    assert_eq!(goal.last_reason.as_deref(), Some(REASON_INTERRUPT));

    // The cancelled process exits; the paused goal sends nothing.
    let step = rt.on_agent_event(
        &main_id(),
        AgentEvent::Exited {
            code: None,
            ok: false,
        },
    );
    assert!(step.actions.is_empty());
    assert!(markers(&step).is_empty());
    assert_eq!(goal_of(&rt).last_reason.as_deref(), Some(REASON_INTERRUPT));
    assert_eq!(
        rt.store
            .goal_for(rt.store.active_conversation())
            .unwrap()
            .unwrap()
            .status,
        GoalStatus::Paused
    );
}

#[test]
fn interrupting_another_member_leaves_the_goal_running() {
    let mut rt = goal_runtime();
    set_goal(&mut rt, "fix things");

    rt.on_ui_command(UiCommand::Cancel {
        member: Some(id("other")),
    });

    assert_eq!(goal_of(&rt).status, GoalStatus::Active);
}

#[test]
fn a_failed_main_turn_pauses_the_goal_with_the_error() {
    let mut rt = goal_runtime();
    set_goal(&mut rt, "fix things");
    rt.on_agent_event(
        &main_id(),
        AgentEvent::Fatal("backend exploded\nbadly".to_string()),
    );

    let step = rt.on_agent_event(
        &main_id(),
        AgentEvent::Exited {
            code: Some(1),
            ok: false,
        },
    );

    assert!(step.actions.is_empty());
    let goal = goal_of(&rt);
    assert_eq!(goal.status, GoalStatus::Paused);
    assert_eq!(
        goal.last_reason.as_deref(),
        Some("Paused: backend exploded badly")
    );
    // The status line carries the reason; no chat notice repeats it.
    assert!(!has_notice(&step, "Paused"));
    assert!(!has_notice(&step, "/goal resume"));
}

#[test]
fn a_nonzero_exit_without_a_message_pauses_the_goal_too() {
    let mut rt = goal_runtime();
    set_goal(&mut rt, "fix things");

    let step = rt.on_agent_event(
        &main_id(),
        AgentEvent::Exited {
            code: Some(3),
            ok: false,
        },
    );

    assert!(step.actions.is_empty());
    let goal = goal_of(&rt);
    assert_eq!(goal.status, GoalStatus::Paused);
    assert!(
        goal.last_reason
            .unwrap()
            .contains("exited without success (code 3)")
    );
}

#[test]
fn three_silent_continuations_pause_the_goal() {
    let mut rt = goal_runtime();
    set_goal(&mut rt, "fix things");

    // The first goal turn is silent, but only continuation turns count.
    let step = exit_silently(&mut rt);
    assert_eq!(markers(&step), vec![2]);
    let step = exit_silently(&mut rt);
    assert_eq!(markers(&step), vec![3]);
    assert_eq!(goal_of(&rt).no_progress_streak, 1);
    let step = exit_silently(&mut rt);
    assert_eq!(markers(&step), vec![4]);
    assert_eq!(goal_of(&rt).no_progress_streak, 2);

    let step = exit_silently(&mut rt);
    assert!(step.actions.is_empty());
    assert!(!has_notice(&step, REASON_NO_PROGRESS));
    assert!(!has_notice(&step, "/goal resume"));
    let goal = goal_of(&rt);
    assert_eq!(goal.status, GoalStatus::Paused);
    assert_eq!(goal.last_reason.as_deref(), Some(REASON_NO_PROGRESS));
    assert_eq!(goal.turns, 4);
}

#[test]
fn an_empty_reply_and_envelope_only_replies_count_as_no_progress() {
    let mut rt = goal_runtime();
    set_goal(&mut rt, "fix things");
    complete_ok(&mut rt, &main_id(), "started");

    complete_ok(&mut rt, &main_id(), "");
    assert_eq!(goal_of(&rt).no_progress_streak, 1);
    complete_ok(&mut rt, &main_id(), "   \n");
    assert_eq!(goal_of(&rt).no_progress_streak, 2);
}

#[test]
fn a_productive_turn_resets_the_no_progress_streak() {
    let mut rt = goal_runtime();
    set_goal(&mut rt, "fix things");
    exit_silently(&mut rt);
    exit_silently(&mut rt);
    exit_silently(&mut rt);
    assert_eq!(goal_of(&rt).no_progress_streak, 2);

    // A reply resets it.
    complete_ok(&mut rt, &main_id(), "made some progress");
    assert_eq!(goal_of(&rt).no_progress_streak, 0);

    // So does a turn with only a tool call and no text.
    exit_silently(&mut rt);
    exit_silently(&mut rt);
    assert_eq!(goal_of(&rt).no_progress_streak, 2);
    rt.on_agent_event(
        &main_id(),
        AgentEvent::ToolStarted {
            id: "t1".to_string(),
            name: "Bash".to_string(),
            summary: "cargo test".to_string(),
        },
    );
    rt.on_agent_event(
        &main_id(),
        AgentEvent::ToolCompleted {
            id: "t1".to_string(),
            ok: true,
            summary: "ok".to_string(),
        },
    );
    exit_silently(&mut rt);
    assert_eq!(goal_of(&rt).no_progress_streak, 0);
    assert_eq!(goal_of(&rt).status, GoalStatus::Active);

    // The guard is a streak, not a lifetime total: three more silent turns pause it.
    exit_silently(&mut rt);
    exit_silently(&mut rt);
    exit_silently(&mut rt);
    assert_eq!(goal_of(&rt).status, GoalStatus::Paused);
}

#[test]
fn there_is_no_turn_cap_on_a_productive_goal() {
    let mut rt = goal_runtime();
    set_goal(&mut rt, "fix things");
    for _ in 0..60 {
        let step = complete_ok(&mut rt, &main_id(), "still working");
        assert_eq!(step.actions.len(), 1);
    }
    assert_eq!(goal_of(&rt).status, GoalStatus::Active);
    assert_eq!(goal_of(&rt).turns, 61);
}

#[test]
fn goals_are_refused_outside_normal_mode() {
    for mode in [
        TerminalMode::Review,
        TerminalMode::Plan,
        TerminalMode::Brainstorm,
        TerminalMode::Team,
    ] {
        let mut rt = goal_runtime();
        rt.on_ui_command(UiCommand::SetMode { mode });

        let step = set_goal(&mut rt, "fix things");

        assert!(step.actions.is_empty(), "{mode}");
        assert!(has_notice(&step, "normal mode"), "{mode}");
        assert!(rt.goal.is_none(), "{mode}");
    }
}

#[test]
fn leaving_normal_mode_pauses_a_running_goal_and_resume_is_refused_there() {
    let mut rt = goal_runtime();
    set_goal(&mut rt, "fix things");

    rt.on_ui_command(UiCommand::SetMode {
        mode: TerminalMode::Review,
    });
    assert_eq!(goal_of(&rt).status, GoalStatus::Paused);
    complete_ok(&mut rt, &main_id(), "turn over");

    let step = rt.on_ui_command(goal_command(GoalAction::Resume));
    assert!(step.actions.is_empty());
    assert!(has_notice(&step, "normal mode"));
    assert_eq!(goal_of(&rt).status, GoalStatus::Paused);
}

#[test]
fn the_turn_tree_must_finish_before_the_goal_continues() {
    let mut rt = goal_runtime();
    set_goal(&mut rt, "fix things");

    // Main opens a sub-agent and ends its turn: the turn is not finished.
    let step = complete_ok(
        &mut rt,
        &main_id(),
        "Opening one.\n@@spawn {\"name\":\"checker\",\"task\":\"check it\"}",
    );
    assert!(step.actions.iter().all(|a| a.member == id("checker")));
    assert!(markers(&step).is_empty());
    assert_eq!(goal_of(&rt).turns, 1);

    // The sub-agent's result goes back to main on the same turn.
    let step = complete_ok(&mut rt, &id("checker"), "checked");
    assert_eq!(step.actions.len(), 1);
    assert_eq!(step.actions[0].member, main_id());
    assert!(markers(&step).is_empty());

    // Only when main has answered the hand-back does the loop go on.
    let step = complete_ok(&mut rt, &main_id(), "thanks, noted");
    assert_eq!(markers(&step), vec![2]);
    assert_eq!(step.actions.len(), 1);
    assert_eq!(step.actions[0].member, main_id());
}

#[test]
fn a_goal_is_saved_per_chat_and_comes_back_paused_after_a_restart() {
    let path = std::env::temp_dir().join(format!(
        "asterline-goal-restart-{}.sqlite3",
        std::process::id()
    ));
    remove_sqlite_test_files(&path);
    let saved = {
        let mut rt = TeamRuntime::new(goal_config(), SqliteStore::open(&path).unwrap());
        set_goal(&mut rt, "ship it");
        complete_ok(&mut rt, &main_id(), "round one");
        assert_eq!(goal_of(&rt).turns, 2);
        assert_eq!(goal_of(&rt).status, GoalStatus::Active);
        goal_of(&rt)
    };

    let mut rt = TeamRuntime::new(goal_config(), SqliteStore::open(&path).unwrap());

    let goal = goal_of(&rt);
    assert_eq!(goal.status, GoalStatus::Paused);
    assert_eq!(goal.last_reason.as_deref(), Some(REASON_RESTORED));
    assert_eq!(goal.objective, "ship it");
    assert_eq!(goal.turns, 2);
    assert_eq!(goal.created_at, saved.created_at);
    // The TUI is told at startup, and nothing runs by itself.
    assert!(matches!(
        rt.goal_event(),
        Some(RuntimeEvent::GoalUpdated { goal: Some(shown) }) if shown.status == GoalStatus::Paused
    ));
    assert!(rt.take_startup_events().iter().all(|event| !matches!(
        event,
        RuntimeEvent::GoalMarker { .. } | RuntimeEvent::UserMessage { .. }
    )));
    // A message from the user works; the goal waits for /goal resume.
    let step = complete_ok(&mut rt, &main_id(), "ignored: nothing is running");
    assert!(step.actions.is_empty());
    let step = rt.on_ui_command(goal_command(GoalAction::Resume));
    assert_eq!(step.actions.len(), 1);
    assert_eq!(goal_of(&rt).status, GoalStatus::Active);
    drop(rt);
    remove_sqlite_test_files(&path);
}

#[test]
fn the_goal_belongs_to_its_chat_across_new_and_resume() {
    let mut rt = goal_runtime();
    let first = rt.store.active_conversation();
    set_goal(&mut rt, "chat one goal");
    rt.on_ui_command(goal_command(GoalAction::Pause));
    complete_ok(&mut rt, &main_id(), "turn over");

    // /new: the fresh chat has no goal, the old one keeps its own.
    let step = rt.on_ui_command(UiCommand::NewSession { prompt: None });
    assert!(rt.goal.is_none());
    assert_eq!(goal_updates(&step).last(), Some(&None));
    assert_eq!(
        rt.store.goal_for(rt.store.active_conversation()).unwrap(),
        None
    );
    assert!(rt.store.goal_for(first).unwrap().is_some());
    let second = rt.store.active_conversation();
    set_goal(&mut rt, "chat two goal");
    rt.on_ui_command(goal_command(GoalAction::Pause));
    complete_ok(&mut rt, &main_id(), "turn over");

    // Pretend chat one's goal was left active (a crash): it resumes paused.
    let mut active = rt.store.goal_for(first).unwrap().unwrap();
    active.status = GoalStatus::Active;
    active.last_reason = None;
    rt.store.set_conversation(first).unwrap();
    rt.store.save_goal(&active).unwrap();
    rt.store.set_conversation(second).unwrap();

    let step = rt.on_ui_command(UiCommand::ResumeConversation {
        conversation: first,
    });

    assert_eq!(rt.store.active_conversation(), first);
    let goal = goal_of(&rt);
    assert_eq!(goal.objective, "chat one goal");
    assert_eq!(goal.status, GoalStatus::Paused);
    assert_eq!(goal.last_reason.as_deref(), Some(REASON_RESTORED));
    assert!(step.actions.is_empty());
    assert_eq!(
        goal_updates(&step)
            .last()
            .unwrap()
            .as_ref()
            .map(|g| g.objective.as_str()),
        Some("chat one goal")
    );

    // And back to chat two: its own goal, not chat one's.
    rt.on_ui_command(UiCommand::ResumeConversation {
        conversation: second,
    });
    assert_eq!(goal_of(&rt).objective, "chat two goal");
}

#[test]
fn resuming_a_chat_without_a_goal_clears_the_shown_goal() {
    let mut rt = goal_runtime();
    let first = rt.store.active_conversation();
    rt.on_ui_command(UiCommand::NewSession { prompt: None });
    set_goal(&mut rt, "chat two goal");
    rt.on_ui_command(goal_command(GoalAction::Pause));
    complete_ok(&mut rt, &main_id(), "turn over");

    let step = rt.on_ui_command(UiCommand::ResumeConversation {
        conversation: first,
    });

    assert!(rt.goal.is_none());
    assert_eq!(goal_updates(&step).last(), Some(&None));
}

#[test]
fn deleting_a_saved_chat_deletes_its_goal() {
    let mut rt = goal_runtime();
    let saved = rt.store.active_conversation();
    set_goal(&mut rt, "to be forgotten");
    rt.on_ui_command(goal_command(GoalAction::Pause));
    complete_ok(&mut rt, &main_id(), "turn over");
    rt.on_ui_command(UiCommand::NewSession { prompt: None });
    assert!(rt.store.goal_for(saved).unwrap().is_some());

    rt.on_ui_command(UiCommand::DeleteConversation {
        conversation: saved,
    });

    assert_eq!(rt.store.goal_for(saved).unwrap(), None);
}

#[test]
fn goal_markers_replay_with_the_chat() {
    let mut rt = goal_runtime();
    set_goal(&mut rt, "fix things");
    complete_ok(&mut rt, &main_id(), "round one");

    let replay = rt.store.replay_chat().unwrap();

    assert!(replay.iter().any(|item| matches!(
        item,
        ChatItem::GoalMarker { member, turn: 2 } if member == &main_id()
    )));
    // The continuation prompt itself is never stored as a user message.
    let users: Vec<_> = replay
        .iter()
        .filter_map(|item| match item {
            ChatItem::User { body, .. } => Some(body.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(users, vec!["fix things"]);
}
