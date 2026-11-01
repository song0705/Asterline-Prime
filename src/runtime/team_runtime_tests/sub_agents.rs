use super::super::*;
use super::*;
use crate::domain::team::DEFAULT_MAX_AUTO_RELAYS;

fn main_id() -> MemberId {
    MemberId::new("main")
}

fn id(value: &str) -> MemberId {
    MemberId::new(value)
}

/// `main` (Claude, pinned model and effort) plus an unrelated top-level
/// member, with `main` as the default target.
fn spawn_runtime() -> TeamRuntime {
    let mut main = TeamMember::new("main", "Main", BackendKind::Claude, "general");
    main.model = Some("opus".to_string());
    main.effort = Some(Effort::High);
    main.sandbox = SandboxPolicy::WorkspaceWrite;
    main.permission_mode = Some(PermissionMode::BypassPermissions);
    let mut config = TeamConfig::new("spawn", "/tmp/ws")
        .with_member(main)
        .with_member(TeamMember::new(
            "other",
            "Other",
            BackendKind::Codex,
            "impl",
        ));
    config.default_target = Some(DefaultTarget::Member(main_id()));
    TeamRuntime::new(config, SqliteStore::in_memory().unwrap()).with_approvals(false)
}

fn spawn_line(name: &str, extra: &str) -> String {
    format!(r#"@@spawn {{"name":"{name}","task":"do {name} work"{extra}}}"#)
}

/// The user greets `main`, which answers with `reply` (controls included).
fn main_replies(rt: &mut TeamRuntime, reply: &str) -> RuntimeStep {
    let started = rt.on_ui_command(user("go"));
    assert_eq!(started.actions[0].member, main_id());
    complete_ok(rt, &main_id(), reply)
}

fn route_events(step: &RuntimeStep) -> Vec<(MemberId, Vec<String>, String)> {
    step.events
        .iter()
        .filter_map(|event| match event {
            RuntimeEvent::Route { from, to, body, .. } => {
                Some((from.clone(), to.clone(), body.clone()))
            }
            _ => None,
        })
        .collect()
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

#[test]
fn spawn_creates_a_child_that_inherits_from_the_parent_and_gets_the_task() {
    let mut rt = spawn_runtime();

    let step = main_replies(
        &mut rt,
        &format!("Opening one.\n{}", spawn_line("reviewer", "")),
    );

    let child = rt.config.member(&id("reviewer")).expect("child exists");
    assert_eq!(child.parent, Some(main_id()));
    assert_eq!(child.display_name, "Reviewer");
    assert_eq!(child.backend, BackendKind::Claude);
    assert_eq!(child.model.as_deref(), Some("opus"));
    assert_eq!(child.effort, Some(Effort::High));
    assert_eq!(child.sandbox, SandboxPolicy::WorkspaceWrite);
    assert_eq!(
        child.permission_mode,
        Some(PermissionMode::BypassPermissions)
    );
    let spawned = step
        .events
        .iter()
        .position(|event| {
            matches!(
                event,
                RuntimeEvent::SubAgentSpawned { parent, child, summary }
                    if parent == &main_id()
                        && child == &id("reviewer")
                        && summary == "claude · opus · high"
            )
        })
        .expect("spawn event");
    // The spawn is announced before the task is delivered.
    let task = step
        .events
        .iter()
        .position(|event| matches!(event, RuntimeEvent::Route { .. }))
        .expect("task route");
    assert!(spawned < task);
    assert!(!has_notice(&step, "spawned"));
    assert!(step.events.iter().any(|event| matches!(
        event,
        RuntimeEvent::Ready { members, .. }
            if members.iter().any(|m| m.id == id("reviewer") && m.parent == Some(main_id()))
    )));
    assert_eq!(
        route_events(&step),
        vec![(
            main_id(),
            vec!["reviewer".to_string()],
            "do reviewer work".to_string()
        )]
    );
    let action = step
        .actions
        .iter()
        .find(|action| action.member == id("reviewer"))
        .expect("child starts on the task");
    assert!(action.prompt.contains("[task from Main (main)]"));
    assert!(action.prompt.contains("do reviewer work"));
    assert!(action.prompt.contains("handed back"));
    assert!(step.runner_changes.iter().any(|change| matches!(
        change,
        RunnerChange::Upsert { member, .. } if member.id == id("reviewer")
            && member.system_prompt.as_deref().unwrap_or("").contains("sub-agent opened by \"main\"")
    )));
}

#[test]
fn spawn_overrides_backend_model_and_effort() {
    let mut rt = spawn_runtime();

    main_replies(
        &mut rt,
        &spawn_line(
            "coder",
            r#","backend":"codex","model":"gpt-5","effort":"xhigh","role":"implementation""#,
        ),
    );

    let child = rt.config.member(&id("coder")).unwrap();
    assert_eq!(child.backend, BackendKind::Codex);
    assert_eq!(child.model.as_deref(), Some("gpt-5"));
    assert_eq!(child.effort, Some(Effort::Xhigh));
    assert_eq!(child.role, "implementation");
    // A different backend gets its own default permission preset, not main's.
    assert_eq!(
        child.codex_permissions_preset(),
        Some(crate::domain::team::CodexPermissionsPreset::AskForApproval)
    );
}

#[test]
fn spawn_on_another_backend_does_not_carry_the_model_over() {
    let mut rt = spawn_runtime();

    main_replies(&mut rt, &spawn_line("coder", r#","backend":"codex""#));

    let child = rt.config.member(&id("coder")).unwrap();
    assert_eq!(child.model, None);
    assert_eq!(child.effort, Some(Effort::High));
}

#[test]
fn spawn_refuses_an_unknown_backend_or_effort_and_tells_the_parent() {
    let mut rt = spawn_runtime();

    let step = main_replies(
        &mut rt,
        &format!(
            "{}\n{}",
            spawn_line("a", r#","backend":"gpt""#),
            spawn_line("b", r#","effort":"huge""#)
        ),
    );

    assert!(rt.config.member(&id("a")).is_none());
    assert!(rt.config.member(&id("b")).is_none());
    assert!(has_notice(&step, "could not spawn a: unknown backend"));
    assert!(has_notice(&step, "could not spawn b: unknown effort"));
}

#[test]
fn spawn_refuses_effort_the_backend_cannot_use() {
    let mut rt = spawn_runtime();

    let step = main_replies(&mut rt, &spawn_line("a", r#","effort":"ultra""#));

    assert!(rt.config.member(&id("a")).is_none());
    assert!(has_notice(&step, "does not support ultra effort"));
}

#[test]
fn spawn_name_becomes_a_clean_member_id() {
    let mut rt = spawn_runtime();

    main_replies(&mut rt, &spawn_line("Code Review!", ""));

    let child = rt.config.member(&id("code-review")).expect("sanitized id");
    assert_eq!(child.display_name, "Code Review!");
}

/// Bodies of the system notes `main` was sent in `step`.
fn naming_notes(step: &RuntimeStep) -> Vec<String> {
    route_events(step)
        .into_iter()
        .filter(|(from, to, _)| from.as_str() == SYSTEM_SENDER && to == &[main_id().to_string()])
        .map(|(_, _, body)| body)
        .filter(|body| body.contains("Sub-agent opened as"))
        .collect()
}

const NAMING_NOTE: &str =
    "Sub-agent opened as \"helper\" (name sub-agents after their job, not their model).";

#[test]
fn spawn_named_after_its_model_is_opened_as_helper_and_the_parent_is_told_once() {
    let mut rt = spawn_runtime();

    let step = main_replies(
        &mut rt,
        &spawn_line("sol", r#","model":"gpt-6.1-sol","backend":"codex""#),
    );

    assert!(rt.config.member(&id("sol")).is_none());
    let child = rt.config.member(&id("helper")).expect("renamed child");
    assert_eq!(child.display_name, "Helper");
    assert_eq!(child.parent, Some(main_id()));
    assert_eq!(child.model.as_deref(), Some("gpt-6.1-sol"));
    assert_eq!(naming_notes(&step), vec![NAMING_NOTE.to_string()]);
    let to_main: Vec<_> = step
        .actions
        .iter()
        .filter(|action| action.member == main_id())
        .collect();
    assert_eq!(to_main.len(), 1);
    assert!(
        to_main[0]
            .prompt
            .starts_with("[system message from Asterline]")
    );
    assert!(to_main[0].prompt.contains(NAMING_NOTE));
    // The task still reaches the child under its real name.
    assert!(step.events.iter().any(|event| matches!(
        event,
        RuntimeEvent::SubAgentSpawned { child, .. } if child == &id("helper")
    )));
    assert!(
        step.actions
            .iter()
            .any(|action| action.member == id("helper") && action.prompt.contains("do sol work"))
    );
}

#[test]
fn spawn_named_after_a_model_family_or_cli_is_opened_as_helper() {
    for name in ["opus", "codex", "claude-opus"] {
        let mut rt = spawn_runtime();
        let step = main_replies(&mut rt, &spawn_line(name, ""));
        assert!(rt.config.member(&id("helper")).is_some(), "{name}");
        assert_eq!(rt.config.sub_agent_count(), 1, "{name}");
        assert_eq!(naming_notes(&step), vec![NAMING_NOTE.to_string()], "{name}");
    }
}

#[test]
fn spawn_names_that_describe_a_job_are_kept_without_a_note() {
    for name in ["opus-reviewer", "reviewer", "researcher"] {
        let mut rt = spawn_runtime();
        let step = main_replies(&mut rt, &spawn_line(name, ""));
        assert!(rt.config.member(&id(name)).is_some(), "{name}");
        assert!(naming_notes(&step).is_empty(), "{name}");
        assert!(
            step.actions.iter().all(|action| action.member != main_id()),
            "{name}"
        );
    }
}

#[test]
fn replacement_names_count_up_when_helper_is_taken() {
    let mut rt = spawn_runtime();
    main_replies(&mut rt, &spawn_line("helper", ""));
    complete_ok(&mut rt, &id("helper"), "done");
    complete_ok(&mut rt, &main_id(), "noted");
    rt.on_ui_command(user("again"));

    let step = complete_ok(&mut rt, &main_id(), &spawn_line("codex", ""));

    let second = rt.config.member(&id("helper-2")).expect("second helper");
    assert_eq!(second.display_name, "Helper-2");
    assert_eq!(
        naming_notes(&step),
        vec![
            "Sub-agent opened as \"helper-2\" (name sub-agents after their job, not their model)."
                .to_string()
        ]
    );
    assert_eq!(rt.config.sub_agent_count(), 2);
}

#[test]
fn following_up_on_a_renamed_child_reuses_it_and_sends_no_second_note() {
    let mut rt = spawn_runtime();
    main_replies(&mut rt, &spawn_line("opus", ""));
    complete_ok(&mut rt, &id("helper"), "first result");
    complete_ok(&mut rt, &main_id(), "noted");
    rt.on_ui_command(user("again"));

    // Using the name it was told reaches the same child, with no new note.
    let step = complete_ok(&mut rt, &main_id(), &spawn_line("helper", ""));

    assert_eq!(rt.config.sub_agent_count(), 1);
    assert!(naming_notes(&step).is_empty());
    assert!(
        step.actions
            .iter()
            .any(|action| action.member == id("helper") && action.prompt.contains("do helper work"))
    );
}

#[test]
fn spawn_reuse_is_matched_by_the_requested_name_before_any_replacement() {
    // A child that really is named after a model (restored from an older
    // chat) is still reached by that name.
    let mut rt = spawn_runtime();
    let mut old = TeamMember::new("opus", "opus", BackendKind::Claude, "sub-agent");
    old.parent = Some(main_id());
    let mut members = rt.config.members.clone();
    members.push(old);
    rt.on_ui_command(UiCommand::ReplaceTeam {
        members,
        default_target: Some(DefaultTarget::Member(main_id())),
    });

    let step = main_replies(&mut rt, &spawn_line("Opus", ""));

    assert_eq!(rt.config.sub_agent_count(), 1);
    assert!(rt.config.member(&id("helper")).is_none());
    assert!(naming_notes(&step).is_empty());
    assert!(
        step.actions
            .iter()
            .any(|action| action.member == id("opus") && action.prompt.contains("do Opus work"))
    );
}

#[test]
fn spawn_display_name_is_capitalised_and_routing_ignores_case() {
    let mut rt = spawn_runtime();
    main_replies(&mut rt, &spawn_line("test-writer", ""));

    let child = rt.config.member(&id("test-writer")).expect("child");
    assert_eq!(child.display_name, "Test-Writer");
    assert_eq!(
        rt.config.find("TEST-WRITER").map(|m| &m.id),
        Some(&id("test-writer"))
    );
    assert_eq!(
        rt.config.find("test-writer").map(|m| &m.id),
        Some(&id("test-writer"))
    );
}

#[test]
fn spawn_refuses_a_name_that_belongs_to_a_top_level_member() {
    let mut rt = spawn_runtime();

    let step = main_replies(&mut rt, &spawn_line("other", ""));

    assert_eq!(rt.config.sub_agent_count(), 0);
    assert!(has_notice(&step, "not your sub-agent"));
}

#[test]
fn spawn_with_an_existing_name_reuses_the_child_and_delivers_the_task() {
    let mut rt = spawn_runtime();
    main_replies(&mut rt, &spawn_line("reviewer", ""));
    complete_ok(&mut rt, &id("reviewer"), "first result");
    complete_ok(&mut rt, &main_id(), "noted");
    let first = rt.config.member(&id("reviewer")).cloned().unwrap();

    let second = rt.on_ui_command(user("again"));
    assert_eq!(second.actions[0].member, main_id());
    let step = complete_ok(
        &mut rt,
        &main_id(),
        r#"@@spawn {"name":"reviewer","task":"second round","backend":"codex"}"#,
    );

    assert_eq!(rt.config.sub_agent_count(), 1);
    // The existing child is kept as it was; the new backend is ignored.
    assert_eq!(rt.config.member(&id("reviewer")), Some(&first));
    assert!(!has_notice(&step, "spawned"));
    let action = step
        .actions
        .iter()
        .find(|action| action.member == id("reviewer"))
        .expect("task delivered");
    assert!(action.prompt.contains("second round"));
}

#[test]
fn spawn_refuses_another_parents_child() {
    let mut rt = spawn_runtime();
    let mut other_child = TeamMember::new("helper", "helper", BackendKind::Codex, "sub-agent");
    other_child.parent = Some(id("other"));
    let mut members = rt.config.members.clone();
    members.push(other_child);
    rt.on_ui_command(UiCommand::ReplaceTeam {
        members,
        default_target: Some(DefaultTarget::Member(main_id())),
    });
    assert!(rt.config.member(&id("helper")).is_some());

    let step = main_replies(&mut rt, &spawn_line("helper", ""));

    assert_eq!(rt.config.sub_agent_count(), 1);
    assert!(has_notice(&step, "not your sub-agent"));
}

#[test]
fn spawn_stops_at_the_sub_agent_limit_but_still_reuses_existing_children() {
    let mut rt = spawn_runtime();
    rt.config.max_auto_relays = 100;
    rt.relay = RelayGuard::new(100);
    let lines: Vec<String> = (0..MAX_SUB_AGENTS)
        .map(|index| spawn_line(&format!("kid{index}"), ""))
        .collect();
    main_replies(&mut rt, &lines.join("\n"));
    assert_eq!(rt.config.sub_agent_count(), MAX_SUB_AGENTS);

    let refused = rt.on_ui_command(user("more"));
    assert!(refused.actions.is_empty() || refused.actions[0].member == main_id());
    let step = complete_ok(&mut rt, &main_id(), &spawn_line("one-too-many", ""));

    assert_eq!(rt.config.sub_agent_count(), MAX_SUB_AGENTS);
    assert!(rt.config.member(&id("one-too-many")).is_none());
    assert!(has_notice(&step, "maximum of 8 sub-agents"));

    let reuse = complete_ok(&mut rt, &main_id(), &spawn_line("kid0", ""));
    assert!(!has_notice(&reuse, "could not spawn"));
}

#[test]
fn a_sub_agent_cannot_spawn() {
    let mut rt = spawn_runtime();
    main_replies(&mut rt, &spawn_line("reviewer", ""));

    let step = complete_ok(&mut rt, &id("reviewer"), &spawn_line("grandchild", ""));

    assert!(rt.config.member(&id("grandchild")).is_none());
    assert!(has_notice(&step, "Reviewer could not spawn grandchild"));
    assert!(has_notice(&step, "cannot open sub-agents"));
    // The refusal is about the member that asked, so only its window shows it.
    assert!(step.events.iter().any(|event| matches!(
        event,
        RuntimeEvent::MemberNotice { member, text }
            if member == &id("reviewer") && text.contains("could not spawn grandchild")
    )));
}

#[test]
fn spawn_is_refused_outside_normal_and_team_mode() {
    for mode in [
        TerminalMode::Review,
        TerminalMode::Plan,
        TerminalMode::Brainstorm,
    ] {
        let mut rt = spawn_runtime();
        rt.on_ui_command(UiCommand::SetMode { mode });
        rt.on_ui_command(user("go"));
        // Mode runs dispatch elsewhere; drive `main` directly.
        rt.active_mode = mode;
        let mut step = RuntimeStep::default();
        rt.handle_spawn(
            &main_id(),
            TurnId(1),
            SpawnRequest {
                name: "reviewer".to_string(),
                task: "t".to_string(),
                backend: None,
                model: None,
                effort: None,
                role: None,
            },
            &mut step,
        );
        assert_eq!(rt.config.sub_agent_count(), 0, "{mode:?}");
        assert!(has_notice(&step, "normal or team mode"), "{mode:?}");
    }
}

#[test]
fn spawn_in_team_mode_respects_the_roster_lock() {
    let mut locked = spawn_runtime();
    start_team(&mut locked, "ship it");
    let coordinator = locked.config.default_member_ids()[0].clone();
    let step = complete_ok(&mut locked, &coordinator, &spawn_line("reviewer", ""));
    assert_eq!(locked.config.sub_agent_count(), 0);
    assert!(has_notice(&step, "locked to the current roster"));

    let mut open = spawn_runtime();
    open.config.modes.team = Some(TeamModeConfig {
        allow_add_members: Some(true),
        ..TeamModeConfig::default()
    });
    start_team(&mut open, "ship it");
    let coordinator = open.config.default_member_ids()[0].clone();
    complete_ok(&mut open, &coordinator, &spawn_line("reviewer", ""));
    assert_eq!(open.config.sub_agent_count(), 1);
}

#[test]
fn refusals_are_counted_by_the_relay_guard_so_they_cannot_loop() {
    let mut rt = spawn_runtime();
    let lines: Vec<String> = (0..DEFAULT_MAX_AUTO_RELAYS + 2)
        .map(|_| spawn_line("x", r#","backend":"nope""#))
        .collect();

    let step = main_replies(&mut rt, &lines.join("\n"));

    let paused = step
        .events
        .iter()
        .filter(|event| matches!(event, RuntimeEvent::RoutePaused { .. }))
        .count();
    assert_eq!(paused, 2);
    // One short system message per allowed refusal waits in main's queue.
    let queued = rt
        .members
        .get(&main_id())
        .map_or(0, |state| state.queue.len());
    assert!(queued <= DEFAULT_MAX_AUTO_RELAYS as usize);
}

#[test]
fn refusal_message_reaches_the_parent_as_a_system_message() {
    let mut rt = spawn_runtime();
    main_replies(&mut rt, &spawn_line("reviewer", ""));
    complete_ok(&mut rt, &id("reviewer"), "ok");
    complete_ok(&mut rt, &main_id(), "noted");

    rt.on_ui_command(user("again"));
    let step = complete_ok(
        &mut rt,
        &main_id(),
        &spawn_line("bad", r#","backend":"nope""#),
    );

    let action = step
        .actions
        .iter()
        .find(|action| action.member == main_id())
        .expect("the parent is told");
    assert!(action.prompt.contains("[system message from Asterline]"));
    assert!(action.prompt.contains("was refused"));
    assert!(action.prompt.contains("unknown backend"));
    // The note is also a visible row from the pseudo-sender to the parent.
    assert!(route_events(&step).iter().any(|(from, to, body)| {
        from == &id("asterline") && to == &["main"] && body.contains("was refused")
    }));
}

#[test]
fn a_childs_reply_to_the_parents_task_is_handed_back() {
    let mut rt = spawn_runtime();
    main_replies(&mut rt, &spawn_line("reviewer", ""));

    let step = complete_ok(&mut rt, &id("reviewer"), "Found two issues.");

    assert_eq!(
        route_events(&step),
        vec![(
            id("reviewer"),
            vec!["main".to_string()],
            "Found two issues.".to_string()
        )]
    );
    let action = step
        .actions
        .iter()
        .find(|action| action.member == main_id())
        .expect("the parent runs again with the result");
    assert!(
        action
            .prompt
            .contains("[hand-back from Reviewer (reviewer)]")
    );
    assert!(action.prompt.contains("Found two issues."));
    // The turn stays open until the parent has dealt with the result.
    assert!(
        !step
            .events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::TurnFinished { .. }))
    );

    let done = complete_ok(&mut rt, &main_id(), "Summary for the user.");
    assert!(
        done.events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::TurnFinished { .. }))
    );
}

#[test]
fn the_last_reply_of_a_multi_message_run_is_what_comes_back() {
    let mut rt = spawn_runtime();
    main_replies(&mut rt, &spawn_line("reviewer", ""));
    let child = id("reviewer");

    rt.on_agent_event(&child, AgentEvent::MessageCompleted("Looking.".to_string()));
    rt.on_agent_event(
        &child,
        AgentEvent::ToolStarted {
            id: "t".to_string(),
            name: "Read".to_string(),
            summary: "x".to_string(),
        },
    );
    let step = complete_ok(&mut rt, &child, "Final verdict.");

    assert_eq!(route_events(&step)[0].2, "Final verdict.");
}

#[test]
fn a_user_message_to_a_child_is_not_forwarded_to_the_parent() {
    let mut rt = spawn_runtime();
    main_replies(&mut rt, &spawn_line("reviewer", ""));
    complete_ok(&mut rt, &id("reviewer"), "first result");
    complete_ok(&mut rt, &main_id(), "noted");

    let started = rt.on_ui_command(UiCommand::UserMessage {
        target: MessageTarget::Member(id("reviewer")),
        body: "how sure are you?".to_string(),
    });
    assert_eq!(started.actions[0].member, id("reviewer"));
    let step = complete_ok(&mut rt, &id("reviewer"), "Fairly sure.");

    assert!(route_events(&step).is_empty());
    assert!(step.actions.iter().all(|action| action.member != main_id()));
    assert!(
        step.events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::TurnFinished { .. }))
    );
}

#[test]
fn a_child_that_already_messaged_the_parent_is_not_handed_back_twice() {
    let mut rt = spawn_runtime();
    main_replies(&mut rt, &spawn_line("reviewer", ""));

    let step = complete_ok(
        &mut rt,
        &id("reviewer"),
        "Details below.\n@@team_message {\"to\":\"main\",\"kind\":\"reply\",\"body\":\"All clear.\"}",
    );

    let routes = route_events(&step);
    assert_eq!(routes.len(), 1);
    assert_eq!(routes[0].2, "All clear.");
}

#[test]
fn messaging_someone_else_does_not_count_as_answering_the_parent() {
    let mut rt = spawn_runtime();
    main_replies(&mut rt, &spawn_line("reviewer", ""));

    let step = complete_ok(
        &mut rt,
        &id("reviewer"),
        "Result.\n@@team_message {\"to\":\"other\",\"body\":\"fyi\"}",
    );

    let routes = route_events(&step);
    assert_eq!(routes.len(), 2);
    assert!(
        routes
            .iter()
            .any(|(_, to, body)| to == &["main"] && body == "Result.")
    );
}

#[test]
fn a_failed_or_silent_child_hands_back_a_short_note() {
    let mut rt = spawn_runtime();
    main_replies(&mut rt, &spawn_line("reviewer", ""));
    let child = id("reviewer");
    rt.on_agent_event(&child, AgentEvent::Fatal("boom".to_string()));
    let failed = rt.on_agent_event(
        &child,
        AgentEvent::Exited {
            code: Some(1),
            ok: false,
        },
    );
    assert!(
        route_events(&failed)[0]
            .2
            .contains("could not finish the task")
    );
    complete_ok(&mut rt, &main_id(), "ok");

    rt.on_ui_command(user("again"));
    complete_ok(&mut rt, &main_id(), &spawn_line("reviewer", ""));
    let silent = rt.on_agent_event(
        &child,
        AgentEvent::Exited {
            code: Some(0),
            ok: true,
        },
    );
    assert!(route_events(&silent)[0].2.contains("gave no text reply"));
}

#[test]
fn a_cancelled_child_is_not_handed_back() {
    let mut rt = spawn_runtime();
    main_replies(&mut rt, &spawn_line("reviewer", ""));
    let child = id("reviewer");
    rt.on_ui_command(UiCommand::Cancel {
        member: Some(child.clone()),
    });

    let step = rt.on_agent_event(
        &child,
        AgentEvent::Exited {
            code: None,
            ok: false,
        },
    );

    assert!(route_events(&step).is_empty());
}

#[test]
fn a_task_sent_with_team_message_also_comes_back_automatically() {
    let mut rt = spawn_runtime();
    main_replies(&mut rt, &spawn_line("reviewer", ""));
    complete_ok(&mut rt, &id("reviewer"), "first");
    complete_ok(&mut rt, &main_id(), "noted");

    rt.on_ui_command(user("more"));
    let step = complete_ok(
        &mut rt,
        &main_id(),
        r#"@@team_message {"to":"reviewer","body":"one more thing"}"#,
    );
    let action = step
        .actions
        .iter()
        .find(|action| action.member == id("reviewer"))
        .expect("child works");
    assert!(action.prompt.contains("[task from Main (main)]"));

    let back = complete_ok(&mut rt, &id("reviewer"), "done again");
    assert_eq!(route_events(&back)[0].2, "done again");
}

#[test]
fn hand_backs_are_counted_by_the_relay_guard() {
    let mut rt = spawn_runtime();
    main_replies(&mut rt, &spawn_line("reviewer", ""));
    // From here on no automatic relay is allowed.
    rt.relay = RelayGuard::new(0);

    let step = complete_ok(&mut rt, &id("reviewer"), "Done.");

    assert!(step.events.iter().any(|event| matches!(
        event,
        RuntimeEvent::RoutePaused { from, to, .. }
            if from == &id("reviewer") && to == &["main".to_string()]
    )));
    assert!(step.actions.iter().all(|action| action.member != main_id()));
    assert_eq!(rt.paused_routes.len(), 1);
}

#[test]
fn team_json_never_receives_sub_agents() {
    let mut rt = spawn_runtime();

    let spawned = main_replies(&mut rt, &spawn_line("reviewer", ""));
    let persisted = spawned.persist_team.expect("roster change persists");
    assert!(persisted.members.iter().all(|m| m.parent.is_none()));
    assert!(persisted.member(&id("reviewer")).is_none());
    assert!(rt.config.member(&id("reviewer")).is_some());

    // A /model change on the child keeps it out of team.json too.
    let mut members: Vec<TeamMember> = strip_team_protocols(rt.config.clone()).members;
    for member in &mut members {
        if member.id == id("reviewer") {
            member.model = Some("sonnet".to_string());
        }
    }
    complete_ok(&mut rt, &id("reviewer"), "x");
    complete_ok(&mut rt, &main_id(), "y");
    let step = rt.on_ui_command(UiCommand::ReplaceTeam {
        members,
        default_target: Some(DefaultTarget::Member(main_id())),
    });
    assert_eq!(
        rt.config.member(&id("reviewer")).unwrap().model.as_deref(),
        Some("sonnet")
    );
    assert_eq!(
        rt.config.member(&id("main")).unwrap().model.as_deref(),
        Some("opus")
    );
    let persisted = step.persist_team.expect("persisted");
    assert!(persisted.members.iter().all(|m| m.parent.is_none()));
    let json = serde_json::to_string(&persisted).unwrap();
    assert!(!json.contains("reviewer"));
    assert!(!json.contains("parent"));

    // So do mode-default saves.
    rt.on_ui_command(UiCommand::SetModeOverrides {
        overrides: ModesConfig {
            team: Some(TeamModeConfig {
                allow_add_members: Some(true),
                ..TeamModeConfig::default()
            }),
            ..ModesConfig::default()
        },
    });
    let saved = rt.on_ui_command(UiCommand::SaveModeDefaults {
        mode: TerminalMode::Team,
    });
    let persisted = saved.persist_team.expect("defaults saved");
    assert!(persisted.members.iter().all(|m| m.parent.is_none()));
}

#[test]
fn team_editor_replace_keeps_children_and_drops_those_of_a_removed_parent() {
    let mut rt = spawn_runtime();
    rt.config.max_auto_relays = 100;
    rt.relay = RelayGuard::new(100);
    main_replies(
        &mut rt,
        &format!(
            "{}\n{}",
            spawn_line("reviewer", ""),
            spawn_line("tester", "")
        ),
    );
    complete_ok(&mut rt, &id("reviewer"), "r");
    complete_ok(&mut rt, &id("tester"), "t");
    // Main handles both hand-backs, one after the other.
    complete_ok(&mut rt, &main_id(), "done");
    complete_ok(&mut rt, &main_id(), "done");
    let mut kid = TeamMember::new("pal", "pal", BackendKind::Codex, "sub-agent");
    kid.parent = Some(id("other"));
    let mut members = strip_team_protocols(rt.config.clone()).members;
    members.push(kid);
    rt.on_ui_command(UiCommand::ReplaceTeam {
        members,
        default_target: Some(DefaultTarget::Member(main_id())),
    });
    assert_eq!(rt.config.sub_agent_count(), 3);

    // The editor only knows the top-level members and edits `main`.
    let mut edited: Vec<TeamMember> = strip_team_protocols(rt.config.without_sub_agents()).members;
    edited[0].role = "lead".to_string();
    let step = rt.on_ui_command(UiCommand::ReplaceTeam {
        members: edited,
        default_target: Some(DefaultTarget::Member(main_id())),
    });
    assert_eq!(rt.config.sub_agent_count(), 3);
    assert_eq!(rt.config.member(&main_id()).unwrap().role, "lead");
    assert!(
        step.persist_team
            .unwrap()
            .members
            .iter()
            .all(|m| m.parent.is_none())
    );

    // Removing `other` removes its child with it.
    let without_other: Vec<TeamMember> = strip_team_protocols(rt.config.without_sub_agents())
        .members
        .into_iter()
        .filter(|member| member.id != id("other"))
        .collect();
    let step = rt.on_ui_command(UiCommand::ReplaceTeam {
        members: without_other,
        default_target: Some(DefaultTarget::Member(main_id())),
    });
    assert!(rt.config.member(&id("pal")).is_none());
    assert_eq!(rt.config.sub_agent_count(), 2);
    assert!(step.runner_changes.iter().any(|change| matches!(
        change,
        RunnerChange::Remove(member) if member == &id("pal")
    )));
}

#[test]
fn new_chat_removes_sub_agents_their_sessions_and_runners() {
    let mut rt = spawn_runtime();
    main_replies(&mut rt, &spawn_line("reviewer", ""));
    rt.on_agent_event(
        &id("reviewer"),
        AgentEvent::SessionDiscovered(AgentSessionId("child-session".to_string())),
    );
    complete_ok(&mut rt, &id("reviewer"), "r");
    complete_ok(&mut rt, &main_id(), "done");
    assert!(rt.sessions.get(&id("reviewer")).is_some());

    let step = rt.on_ui_command(UiCommand::NewSession { prompt: None });

    assert_eq!(rt.config.sub_agent_count(), 0);
    assert!(rt.config.member(&id("reviewer")).is_none());
    assert!(!rt.members.contains_key(&id("reviewer")));
    assert!(rt.sessions.get(&id("reviewer")).is_none());
    assert!(step.runner_changes.iter().any(|change| matches!(
        change,
        RunnerChange::Remove(member) if member == &id("reviewer")
    )));
    assert!(step.events.iter().any(|event| matches!(
        event,
        RuntimeEvent::Ready { members, .. }
            if members.len() == 2 && members.iter().all(|m| m.parent.is_none())
    )));
    let snapshot = rt
        .store
        .conversation_snapshot(rt.store.active_conversation())
        .unwrap()
        .unwrap();
    assert!(snapshot.team.members.iter().all(|m| m.parent.is_none()));
    // The parents' prompts no longer list the removed child.
    let prompt = rt
        .config
        .member(&main_id())
        .unwrap()
        .system_prompt
        .clone()
        .unwrap();
    assert!(!prompt.contains("Your sub-agents"));
}

#[test]
fn resuming_a_chat_brings_its_sub_agents_and_their_sessions_back() {
    let mut rt = spawn_runtime();
    let original = rt.store.active_conversation();
    main_replies(
        &mut rt,
        &spawn_line("reviewer", r#","backend":"codex","model":"gpt-5""#),
    );
    rt.on_agent_event(
        &id("reviewer"),
        AgentEvent::SessionDiscovered(AgentSessionId("child-session".to_string())),
    );
    complete_ok(&mut rt, &id("reviewer"), "r");
    complete_ok(&mut rt, &main_id(), "done");
    rt.on_ui_command(UiCommand::NewSession { prompt: None });
    assert_eq!(rt.config.sub_agent_count(), 0);

    let resumed = rt.on_ui_command(UiCommand::ResumeConversation {
        conversation: original,
    });

    let child = rt.config.member(&id("reviewer")).expect("child is back");
    assert_eq!(child.parent, Some(main_id()));
    assert_eq!(child.model.as_deref(), Some("gpt-5"));
    assert!(rt.members.contains_key(&id("reviewer")));
    assert_eq!(
        rt.sessions.get(&id("reviewer")),
        Some(AgentSessionId("child-session".to_string()))
    );
    assert!(resumed.runner_changes.iter().any(|change| matches!(
        change,
        RunnerChange::Upsert { member, .. } if member.id == id("reviewer")
    )));
    assert!(resumed.events.iter().any(|event| matches!(
        event,
        RuntimeEvent::Ready { members, .. }
            if members.iter().any(|m| m.id == id("reviewer") && m.parent == Some(main_id()))
    )));
    // ...and still never in team.json.
    assert!(
        resumed
            .persist_team
            .unwrap()
            .members
            .iter()
            .all(|m| m.parent.is_none())
    );

    let continued = rt.on_ui_command(UiCommand::UserMessage {
        target: MessageTarget::Member(id("reviewer")),
        body: "pick up".to_string(),
    });
    assert_eq!(
        continued.actions[0].session,
        Some(AgentSessionId("child-session".to_string()))
    );
}

#[test]
fn a_restart_restores_the_sub_agents_of_the_active_chat() {
    let mut rt = spawn_runtime();
    main_replies(&mut rt, &spawn_line("reviewer", ""));
    complete_ok(&mut rt, &id("reviewer"), "r");
    complete_ok(&mut rt, &main_id(), "done");

    // What `app::prepare` does: start from the saved top-level roster, then
    // let the active chat's snapshot supply the rest.
    let launch = rt.config.without_sub_agents();
    let restored = rt.store.restore_active_team_config(&launch).unwrap();

    assert_eq!(restored.sub_agent_count(), 1);
    assert_eq!(
        restored.member(&id("reviewer")).unwrap().parent,
        Some(main_id())
    );
    // Restored sub-agents keep the display name they were stored with.
    assert_eq!(
        restored.member(&id("reviewer")).unwrap().display_name,
        "Reviewer"
    );
}

#[test]
fn modes_ignore_sub_agents_when_choosing_roles() {
    let mut rt = spawn_runtime();
    main_replies(&mut rt, &spawn_line("reviewer", r#","role":"code review""#));

    let (roles, _) = resolve_mode_roles(&rt.config, CollabMode::Review).unwrap();

    assert_ne!(roles.reviewer, id("reviewer"));
    assert!(!roles.participants.contains(&id("reviewer")));
}

#[test]
fn parents_protocol_documents_spawn_and_children_get_theirs() {
    let mut rt = spawn_runtime();
    main_replies(&mut rt, &spawn_line("reviewer", ""));

    let main_prompt = rt
        .config
        .member(&main_id())
        .unwrap()
        .system_prompt
        .clone()
        .unwrap();
    assert!(main_prompt.contains("@@spawn"));
    assert!(main_prompt.contains("up to 8"));
    assert!(main_prompt.contains("Your sub-agents: reviewer"));
    assert!(main_prompt.contains("Teammates: other"));
    let child_prompt = rt
        .config
        .member(&id("reviewer"))
        .unwrap()
        .system_prompt
        .clone()
        .unwrap();
    assert!(child_prompt.contains("sub-agent opened by \"main\""));
    assert!(child_prompt.contains("handed back"));
    assert!(child_prompt.contains("cannot open sub-agents"));
    assert!(!child_prompt.contains("@@spawn"));
}
