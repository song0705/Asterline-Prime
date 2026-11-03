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
fn agent_message_from_a_different_turn_is_queued() {
    let mut rt = runtime();
    let (steer, turn) = busy_builder(&mut rt);
    let mut step = RuntimeStep::default();
    rt.enqueue_prompt_from(
        &builder(),
        TurnId(turn.0 + 100),
        "other conversation".into(),
        Some(reviewer()),
        &mut step,
    );
    assert_eq!(queue_len(&rt, &builder()), 1);
    assert_eq!(steer.try_recv(), None);
    assert_eq!(steered_len(&rt, &builder()), 0);
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
