use super::super::*;
use super::*;

fn main_id() -> MemberId {
    MemberId::new("main")
}

fn id(value: &str) -> MemberId {
    MemberId::new(value)
}

/// A lone `main` (Claude, pinned model and effort): the starting roster.
fn lone_main_runtime() -> TeamRuntime {
    let mut main = TeamMember::new("main", "Main", BackendKind::Claude, "general");
    main.model = Some("opus".to_string());
    main.effort = Some(Effort::High);
    let mut config = TeamConfig::new("solo", "/tmp/ws").with_member(main);
    config.default_target = Some(DefaultTarget::Member(main_id()));
    TeamRuntime::new(config, SqliteStore::in_memory().unwrap()).with_approvals(false)
}

fn spawned(step: &RuntimeStep) -> Vec<(MemberId, MemberId)> {
    step.events
        .iter()
        .filter_map(|event| match event {
            RuntimeEvent::SubAgentSpawned { parent, child, .. } => {
                Some((parent.clone(), child.clone()))
            }
            _ => None,
        })
        .collect()
}

fn notice_texts(step: &RuntimeStep) -> Vec<String> {
    step.events
        .iter()
        .filter_map(|event| match event {
            RuntimeEvent::Notice(text) => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn run_review(rt: &mut TeamRuntime, task: &str) -> RuntimeStep {
    let step = rt.on_ui_command(run_mode(task));
    let mut merged = step;
    let builder = complete_ok(rt, &main_id(), "implemented it");
    merged.events.extend(builder.events);
    merged.actions.extend(builder.actions);
    let verdict = complete_ok(
        rt,
        &id("reviewer"),
        "Fine.\n@@review {\"verdict\":\"approve\",\"summary\":\"good\"}",
    );
    merged.events.extend(verdict.events);
    merged.actions.extend(verdict.actions);
    merged
}

fn rebrand_reviewer_model(rt: &mut TeamRuntime, model: &str) {
    let mut members = rt.config.members.clone();
    members
        .iter_mut()
        .find(|member| member.id == id("reviewer"))
        .unwrap()
        .model = Some(model.to_string());
    let target = rt.config.default_target.clone();
    let mut step = RuntimeStep::default();
    assert!(rt.replace_team(members, target, false, &mut step));
}

#[test]
fn review_with_a_lone_main_opens_a_reviewer_and_runs_to_completion() {
    let mut rt = lone_main_runtime();

    let step = rt.on_ui_command(run_mode("add tests"));

    assert_eq!(spawned(&step), vec![(main_id(), id("reviewer"))]);
    let reviewer = rt.config.member(&id("reviewer")).expect("reviewer opened");
    let main = rt.config.member(&main_id()).unwrap();
    assert_eq!(reviewer.parent, Some(main_id()));
    assert_eq!(reviewer.display_name, "Reviewer");
    assert_eq!(reviewer.backend, main.backend);
    assert_eq!(reviewer.model, main.model);
    assert_eq!(reviewer.effort, main.effort);
    assert!(reviewer.role.contains("reviewer"), "{}", reviewer.role);
    assert!(
        step.actions.iter().any(|a| a.member == main_id()),
        "the builder gets the task"
    );
    assert!(
        !notice_texts(&step)
            .iter()
            .any(|text| text.contains("distinct members"))
    );

    let step = complete_ok(&mut rt, &main_id(), "implemented it");
    assert!(
        step.actions
            .iter()
            .any(|a| a.member == id("reviewer") && a.prompt.contains(REVIEW_PROTOCOL_HINT)),
        "the reviewer sub-agent is asked to review"
    );
    let step = complete_ok(
        &mut rt,
        &id("reviewer"),
        "Fine.\n@@review {\"verdict\":\"approve\",\"summary\":\"good\"}",
    );
    assert!(step.events.iter().any(|event| matches!(
        event,
        RuntimeEvent::RunUpdated { run } if run.status == RunStatus::Done
    )));
}

#[test]
fn a_second_review_reuses_the_reviewer_and_keeps_its_model() {
    let mut rt = lone_main_runtime();
    let first = run_review(&mut rt, "first");
    assert_eq!(spawned(&first).len(), 1);

    rebrand_reviewer_model(&mut rt, "sonnet");
    let second = rt.on_ui_command(run_mode("second"));

    assert!(spawned(&second).is_empty(), "{:?}", second.events);
    assert_eq!(rt.config.sub_agent_count(), 1);
    assert_eq!(
        rt.config.member(&id("reviewer")).unwrap().model.as_deref(),
        Some("sonnet")
    );
    assert!(second.actions.iter().any(|a| a.member == main_id()));
}

#[test]
fn plan_with_a_lone_main_has_main_as_leader_and_builder() {
    let mut rt = lone_main_runtime();

    let step = rt.on_ui_command(run_plan("make a plan"));

    assert!(spawned(&step).is_empty());
    assert!(step.actions.iter().any(|a| a.member == main_id()));
    let session = rt.mode_sessions.values().next().expect("plan session");
    assert_eq!(session.leader, main_id());
    assert_eq!(session.plan_builder.as_ref(), Some(&main_id()));
    assert_eq!(session.plan_reviewer, None);
    assert!(
        notice_texts(&step)
            .iter()
            .any(|text| text.contains("builder: main")),
        "{:?}",
        step.events
    );
}

#[test]
fn brainstorm_with_a_lone_main_opens_two_ideators_by_default() {
    let mut rt = lone_main_runtime();

    let step = rt.on_ui_command(run_brainstorm("ideas for a cache"));

    assert_eq!(
        spawned(&step),
        vec![(main_id(), id("ideator-1")), (main_id(), id("ideator-2"))]
    );
    for name in ["ideator-1", "ideator-2"] {
        let member = rt.config.member(&id(name)).unwrap();
        assert_eq!(member.parent, Some(main_id()));
        assert_eq!(member.model.as_deref(), Some("opus"));
        assert_eq!(member.role, "brainstorm participant");
    }
    let session = rt
        .mode_sessions
        .values()
        .next()
        .expect("brainstorm session");
    assert_eq!(session.participants.len(), 3);
    for name in ["main", "ideator-1", "ideator-2"] {
        assert!(
            step.actions.iter().any(|a| a.member == id(name)),
            "{name} is asked for ideas"
        );
    }
}

#[test]
fn brainstorm_participant_count_sets_how_many_ideators_open() {
    let mut rt = lone_main_runtime();
    rt.config.modes.brainstorm = Some(BrainstormModeConfig {
        participant_count: Some(4),
        ..BrainstormModeConfig::default()
    });

    let step = rt.on_ui_command(run_brainstorm("ideas"));

    assert_eq!(spawned(&step).len(), 3);
    assert_eq!(rt.config.sub_agent_count(), 3);
    assert_eq!(
        rt.mode_sessions.values().next().unwrap().participants.len(),
        4
    );
}

#[test]
fn explicit_bindings_and_full_rosters_open_nothing() {
    // Legacy three-member roster: every mode resolves as before.
    let mut rt =
        TeamRuntime::new(plan_team(), SqliteStore::in_memory().unwrap()).with_approvals(false);
    let step = rt.on_ui_command(run_mode("review it"));
    assert!(spawned(&step).is_empty());
    assert_eq!(rt.config.sub_agent_count(), 0);
    let session = rt.mode_sessions.values().next().unwrap();
    assert_eq!(
        (session.builder.as_str(), session.reviewer.as_str()),
        ("builder", "reviewer")
    );

    // An explicit participant list ignores the count.
    let mut config = plan_team();
    config.modes.brainstorm = Some(BrainstormModeConfig {
        participants: Some(vec![id("builder"), id("planner")]),
        participant_count: Some(6),
        ..BrainstormModeConfig::default()
    });
    let mut rt = TeamRuntime::new(config, SqliteStore::in_memory().unwrap()).with_approvals(false);
    let step = rt.on_ui_command(run_brainstorm("ideas"));
    assert!(spawned(&step).is_empty());
    assert_eq!(
        rt.mode_sessions.values().next().unwrap().participants.len(),
        2
    );
}

#[test]
fn a_full_chat_starts_nothing_and_says_why() {
    let mut rt = lone_main_runtime();
    let mut members = rt.config.members.clone();
    for n in 1..=7 {
        let mut child = TeamMember::new(
            format!("helper-{n}"),
            format!("Helper-{n}"),
            BackendKind::Claude,
            "sub-agent",
        );
        child.parent = Some(main_id());
        members.push(child);
    }
    let target = rt.config.default_target.clone();
    let mut step = RuntimeStep::default();
    assert!(rt.replace_team(members, target, false, &mut step));
    assert_eq!(rt.config.sub_agent_count(), 7);

    let step = rt.on_ui_command(run_brainstorm("ideas"));

    assert_eq!(
        notice_texts(&step),
        vec!["brainstorm needs 2 more sub-agents but this chat already has 7 of 8".to_string()]
    );
    assert_eq!(rt.config.sub_agent_count(), 7);
    assert!(step.actions.is_empty());
    assert!(rt.mode_sessions.is_empty());
    assert!(rt.store.latest_run().unwrap().is_none());

    // One seat still fits.
    let step = rt.on_ui_command(run_mode("review"));
    assert_eq!(spawned(&step).len(), 1);
    assert_eq!(rt.config.sub_agent_count(), 8);
}
