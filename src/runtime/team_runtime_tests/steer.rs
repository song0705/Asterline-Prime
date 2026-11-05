//! Delivering another agent's message into a member's live run (steering),
//! and falling back to the queue whenever that is not possible.

use super::*;
use crate::adapter::SteerReceiver;

fn builder() -> MemberId {
    MemberId::new("builder")
}

fn reviewer() -> MemberId {
    MemberId::new("reviewer")
}

/// The run `step` started for `member`.
fn started_run(step: &RuntimeStep, member: &MemberId) -> RunAction {
    let mut actions = step.actions.iter().filter(|a| &a.member == member);
    let action = actions.next().expect("a run was started");
    assert!(actions.next().is_none(), "only one run was started");
    RunAction {
        member: action.member.clone(),
        prompt: action.prompt.clone(),
        session: action.session.clone(),
        cancel: action.cancel.clone(),
        effort: action.effort,
        steer: action.steer.clone(),
    }
}

/// Start a user turn on `builder` and return its steer receiver and turn.
fn busy_builder(rt: &mut TeamRuntime) -> (SteerReceiver, TurnId) {
    let step = rt.on_ui_command(UiCommand::UserMessage {
        target: MessageTarget::Member(builder()),
        body: "first".to_string(),
    });
    let action = started_run(&step, &builder());
    let turn = rt.running_turn(&builder()).expect("builder is running");
    (action.steer.expect("codex runs can be steered"), turn)
}

fn queue_len(rt: &TeamRuntime, member: &MemberId) -> usize {
    rt.members[member].queue.len()
}

fn steered_len(rt: &TeamRuntime, member: &MemberId) -> usize {
    rt.members[member].running.as_ref().unwrap().steered.len()
}

fn exit_ok(rt: &mut TeamRuntime, member: &MemberId) -> RuntimeStep {
    rt.on_agent_event(
        member,
        AgentEvent::Exited {
            code: Some(0),
            ok: true,
        },
    )
}

#[test]
fn agent_message_to_a_busy_member_of_the_same_turn_goes_into_the_live_run() {
    let mut rt = runtime();
    let (steer, turn) = busy_builder(&mut rt);

    let mut step = RuntimeStep::default();
    rt.enqueue_prompt_from(
        &builder(),
        turn,
        "@builder please also run the linter".to_string(),
        Some(reviewer()),
        &mut step,
    );

    assert_eq!(queue_len(&rt, &builder()), 0, "the queue must not grow");
    assert!(step.actions.is_empty());
    assert!(
        !step
            .events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::QueueUpdated { .. }))
    );
    assert_eq!(
        steer.try_recv().as_deref(),
        Some("please also run the linter")
    );
    assert_eq!(steered_len(&rt, &builder()), 1);
}

#[test]
fn delivered_steer_leaves_nothing_to_run_afterwards() {
    let mut rt = runtime();
    let (steer, turn) = busy_builder(&mut rt);
    let mut step = RuntimeStep::default();
    rt.enqueue_prompt_from(
        &builder(),
        turn,
        "extra".into(),
        Some(reviewer()),
        &mut step,
    );
    assert_eq!(steer.try_recv().as_deref(), Some("extra"));

    let step = rt.on_agent_event(&builder(), AgentEvent::SteerDelivered);
    assert!(step.events.iter().any(|event| matches!(
        event,
        RuntimeEvent::Log(entry) if entry.message.contains("delivered mid-turn")
    )));
    assert_eq!(steered_len(&rt, &builder()), 0);

    let step = exit_ok(&mut rt, &builder());
    assert!(
        step.actions.is_empty(),
        "a delivered message must not run again"
    );
    assert_eq!(queue_len(&rt, &builder()), 0);
}

#[test]
fn rejected_steer_is_queued_and_runs_after_the_current_run() {
    let mut rt = runtime();
    let (steer, turn) = busy_builder(&mut rt);
    let mut step = RuntimeStep::default();
    rt.enqueue_prompt_from(
        &builder(),
        turn,
        "late thought".into(),
        Some(reviewer()),
        &mut step,
    );
    assert_eq!(steer.try_recv().as_deref(), Some("late thought"));

    let step = rt.on_agent_event(
        &builder(),
        AgentEvent::SteerRejected {
            prompt: "late thought".to_string(),
        },
    );
    assert!(step.events.iter().any(|event| matches!(
        event,
        RuntimeEvent::QueueUpdated { prompts, .. } if prompts == &["late thought".to_string()]
    )));
    assert_eq!(queue_len(&rt, &builder()), 1);
    assert_eq!(steered_len(&rt, &builder()), 0);

    let step = exit_ok(&mut rt, &builder());
    let next = started_run(&step, &builder());
    assert!(next.prompt.contains("late thought"));
    assert_eq!(queue_len(&rt, &builder()), 0);
    // It was sent by the reviewer, so that stays the new run's origin.
    assert_eq!(
        rt.members[&builder()].running.as_ref().unwrap().origin,
        Some(reviewer())
    );
}

#[test]
fn steer_the_backend_never_confirmed_is_queued_when_the_run_ends() {
    let mut rt = runtime();
    let (_steer, turn) = busy_builder(&mut rt);
    let mut step = RuntimeStep::default();
    rt.enqueue_prompt_from(
        &builder(),
        turn,
        "unconfirmed".into(),
        Some(reviewer()),
        &mut step,
    );

    // The run ends without ever reporting delivery or rejection.
    let step = exit_ok(&mut rt, &builder());
    let next = started_run(&step, &builder());
    assert!(next.prompt.contains("unconfirmed"));
}

#[test]
fn user_message_to_a_busy_member_is_still_queued() {
    let mut rt = runtime();
    let (steer, _turn) = busy_builder(&mut rt);
    let step = rt.on_ui_command(UiCommand::UserMessage {
        target: MessageTarget::Member(builder()),
        body: "second".to_string(),
    });
    assert!(step.events.iter().any(|event| matches!(
        event,
        RuntimeEvent::QueueUpdated { prompts, .. } if prompts == &["second".to_string()]
    )));
    assert_eq!(queue_len(&rt, &builder()), 1);
    assert_eq!(steer.try_recv(), None);
}

#[test]
fn engine_dispatch_without_a_sender_is_queued_even_in_the_same_turn() {
    let mut rt = runtime();
    let (steer, turn) = busy_builder(&mut rt);
    let mut step = RuntimeStep::default();
    rt.enqueue_prompt_from(&builder(), turn, "system note".into(), None, &mut step);
    assert_eq!(queue_len(&rt, &builder()), 1);
    assert_eq!(steer.try_recv(), None);
}

#[test]
fn agent_message_from_a_different_turn_joins_the_live_run_and_keeps_its_turn_active() {
    let mut rt = runtime();
    let (steer, turn) = busy_builder(&mut rt);
    let other = TurnId(turn.0 + 100);
    let mut step = RuntimeStep::default();
    rt.enqueue_prompt_from(
        &builder(),
        other,
        "late result".into(),
        Some(reviewer()),
        &mut step,
    );
    assert_eq!(queue_len(&rt, &builder()), 0);
    assert_eq!(steer.try_recv().as_deref(), Some("late result"));
    assert_eq!(steered_len(&rt, &builder()), 1);
    assert!(rt.turn_active(other));
    rt.check_turn_complete(other, &mut step);
    assert!(
        !step
            .events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::TurnFinished { turn } if *turn == other))
    );
    rt.on_agent_event(&builder(), AgentEvent::SteerDelivered);
    assert!(
        rt.turn_active(other),
        "delivery must not finish the source turn early"
    );
    let finished = exit_ok(&mut rt, &builder());
    assert!(!rt.turn_active(other));
    assert!(
        finished
            .events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::TurnFinished { turn } if *turn == other))
    );
    assert!(
        finished.actions.is_empty(),
        "accepted input must not be run again"
    );
}

#[test]
fn foreign_turn_rejected_by_the_backend_is_queued_without_losing_its_identity() {
    let mut rt = runtime();
    let (steer, turn) = busy_builder(&mut rt);
    let other = TurnId(turn.0 + 100);
    rt.enqueue_prompt_from(
        &builder(),
        other,
        "late result".into(),
        Some(reviewer()),
        &mut RuntimeStep::default(),
    );
    assert_eq!(steer.try_recv().as_deref(), Some("late result"));
    rt.on_agent_event(
        &builder(),
        AgentEvent::SteerRejected {
            prompt: "late result".into(),
        },
    );
    assert_eq!(queue_len(&rt, &builder()), 1);
    assert!(rt.turn_active(other));
    let next = exit_ok(&mut rt, &builder());
    assert!(
        started_run(&next, &builder())
            .prompt
            .contains("late result")
    );
    assert_eq!(rt.running_turn(&builder()), Some(other));
}

#[test]
fn unconfirmed_cross_turn_input_is_requeued_once_with_the_original_turn() {
    let mut rt = runtime();
    let (_steer, turn) = busy_builder(&mut rt);
    let other = TurnId(turn.0 + 100);
    rt.enqueue_prompt_from(
        &builder(),
        other,
        "late result".into(),
        Some(reviewer()),
        &mut RuntimeStep::default(),
    );
    let next = exit_ok(&mut rt, &builder());
    assert!(
        started_run(&next, &builder())
            .prompt
            .contains("late result")
    );
    assert_eq!(rt.running_turn(&builder()), Some(other));
    assert!(
        !next
            .events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::TurnFinished { turn } if *turn == other))
    );
    let done = exit_ok(&mut rt, &builder());
    assert!(done.actions.is_empty());
    assert_eq!(
        done.events
            .iter()
            .filter(|event| matches!(event, RuntimeEvent::TurnFinished { turn } if *turn == other))
            .count(),
        1
    );
}

#[test]
fn delayed_child_handback_reaches_a_parent_that_has_started_a_new_turn() {
    for backend in [BackendKind::Codex, BackendKind::Claude] {
        let mut config = team();
        config
            .members
            .iter_mut()
            .find(|member| member.id == builder())
            .unwrap()
            .backend = backend;
        let mut rt =
            TeamRuntime::new(config, SqliteStore::in_memory().unwrap()).with_approvals(false);
        busy_builder(&mut rt);
        let spawned = rt.on_agent_event(
            &builder(),
            AgentEvent::MessageCompleted(
                r#"@@spawn {"name":"researcher","task":"research this"}"#.to_string(),
            ),
        );
        let child = MemberId::new("researcher");
        let _ = started_run(&spawned, &child);
        let child_turn = rt.running_turn(&child).unwrap();
        exit_ok(&mut rt, &builder());
        let (parent_input, new_turn) = busy_builder(&mut rt);
        assert_ne!(child_turn, new_turn);
        rt.on_agent_event(
            &child,
            AgentEvent::MessageCompleted("first research result".to_string()),
        );
        let handback = exit_ok(&mut rt, &child);
        assert_eq!(queue_len(&rt, &builder()), 0);
        assert!(
            parent_input
                .try_recv()
                .unwrap()
                .contains("first research result")
        );
        assert!(!handback.events.iter().any(
            |event| matches!(event, RuntimeEvent::TurnFinished { turn } if *turn == child_turn)
        ));
        rt.on_agent_event(&builder(), AgentEvent::SteerDelivered);
        let finished = exit_ok(&mut rt, &builder());
        assert!(finished.events.iter().any(
            |event| matches!(event, RuntimeEvent::TurnFinished { turn } if *turn == child_turn)
        ));
        assert!(finished.actions.is_empty());
    }
}

#[test]
fn sub_agents_on_different_turns_can_message_each_other_mid_run() {
    let mut rt = runtime();
    busy_builder(&mut rt);
    let spawned = rt.on_agent_event(&builder(), AgentEvent::MessageCompleted(
        "@@spawn {\"name\":\"first\",\"task\":\"first task\"}\n@@spawn {\"name\":\"second\",\"task\":\"second task\"}".to_string()
    ));
    let first = MemberId::new("first");
    let second = MemberId::new("second");
    let first_input = started_run(&spawned, &first).steer.unwrap();
    let original_turn = rt.running_turn(&first).unwrap();
    rt.on_agent_event(&second, AgentEvent::MessageCompleted("done".to_string()));
    exit_ok(&mut rt, &second);
    rt.on_ui_command(UiCommand::UserMessage {
        target: MessageTarget::Member(second.clone()),
        body: "new task".to_string(),
    });
    assert_ne!(rt.running_turn(&second).unwrap(), original_turn);
    rt.on_agent_event(
        &second,
        AgentEvent::MessageCompleted(
            r#"@@team_message {"to":"first","body":"new finding"}"#.to_string(),
        ),
    );
    assert_eq!(queue_len(&rt, &first), 0);
    assert!(first_input.try_recv().unwrap().contains("new finding"));
}

#[test]
fn backend_without_mid_turn_input_keeps_queueing() {
    let mut config = TeamConfig::new("mixed", "/tmp/ws")
        .with_member(TeamMember::new(
            "researcher",
            "Researcher",
            BackendKind::Grok,
            "research",
        ))
        .with_member(TeamMember::new("agy", "Agy", BackendKind::Agy, "misc"))
        .with_member(TeamMember::new(
            "reviewer",
            "Reviewer",
            BackendKind::Claude,
            "review",
        ));
    config.default_target = Some(DefaultTarget::Member(MemberId::new("researcher")));
    let mut rt = TeamRuntime::new(config, SqliteStore::in_memory().unwrap()).with_approvals(false);
    for name in ["researcher", "agy"] {
        let member = MemberId::new(name);
        let step = rt.on_ui_command(UiCommand::UserMessage {
            target: MessageTarget::Member(member.clone()),
            body: "first".to_string(),
        });
        let action = started_run(&step, &member);
        assert!(action.steer.is_none(), "{name} has no steer channel");
        let turn = rt.running_turn(&member).unwrap();
        let mut step = RuntimeStep::default();
        rt.enqueue_prompt_from(&member, turn, "extra".into(), Some(reviewer()), &mut step);
        assert_eq!(queue_len(&rt, &member), 1, "{name} queues");
    }
}

#[test]
fn closed_steer_channel_falls_back_to_the_queue() {
    let mut rt = runtime();
    let (steer, turn) = busy_builder(&mut rt);
    // The runner shut the channel (for example, its backend cannot be steered).
    steer.close();
    let mut step = RuntimeStep::default();
    rt.enqueue_prompt_from(
        &builder(),
        turn,
        "extra".into(),
        Some(reviewer()),
        &mut step,
    );
    assert_eq!(queue_len(&rt, &builder()), 1);
    assert_eq!(steered_len(&rt, &builder()), 0);
}

#[test]
fn cancelled_run_is_not_steered() {
    let mut rt = runtime();
    let (steer, turn) = busy_builder(&mut rt);
    rt.members[&builder()]
        .running
        .as_ref()
        .unwrap()
        .cancel
        .store(true, Ordering::Relaxed);
    let mut step = RuntimeStep::default();
    rt.enqueue_prompt_from(
        &builder(),
        turn,
        "extra".into(),
        Some(reviewer()),
        &mut step,
    );
    assert_eq!(queue_len(&rt, &builder()), 1);
    assert_eq!(steer.try_recv(), None);
}

#[test]
fn routed_agent_message_steers_the_busy_recipient_and_keeps_its_origin() {
    let mut rt = runtime();
    // The user's message starts builder's turn; builder hands work to the
    // reviewer, who starts running it.
    rt.on_ui_command(UiCommand::UserMessage {
        target: MessageTarget::Member(builder()),
        body: "go".to_string(),
    });
    let first = rt.on_agent_event(
        &builder(),
        AgentEvent::MessageCompleted(
            "@@team_message {\"to\":\"reviewer\",\"body\":\"first task\"}".to_string(),
        ),
    );
    let run = started_run(&first, &reviewer());
    let steer = run.steer.expect("claude runs can be steered");
    assert_eq!(
        rt.members[&reviewer()].running.as_ref().unwrap().origin,
        Some(builder())
    );

    // While the reviewer works, builder (still in the same turn) adds to it.
    let second = rt.on_agent_event(
        &builder(),
        AgentEvent::MessageCompleted(
            "@@team_message {\"to\":\"reviewer\",\"body\":\"one more thing\"}".to_string(),
        ),
    );
    assert!(second.actions.is_empty());
    assert_eq!(queue_len(&rt, &reviewer()), 0);
    let delivered = steer.try_recv().expect("the live run got the message");
    assert!(delivered.contains("one more thing"), "{delivered}");
    // The conversation still shows the message going to the reviewer.
    assert!(second.events.iter().any(|event| matches!(
        event,
        RuntimeEvent::Route { to, body, .. }
            if to == &["reviewer".to_string()] && body == "one more thing"
    )));
    // The run's origin, and so who its reply goes back to, is unchanged.
    assert_eq!(
        rt.members[&reviewer()].running.as_ref().unwrap().origin,
        Some(builder())
    );
}
