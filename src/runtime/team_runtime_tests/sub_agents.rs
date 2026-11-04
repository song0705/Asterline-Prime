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

/// Rows the chat shows as sent by Asterline itself ("from asterline").
fn system_rows(step: &RuntimeStep) -> Vec<(MemberId, String)> {
    route_events(step)
        .into_iter()
        .filter(|(from, _, _)| from.as_str() == "asterline")
        .flat_map(|(_, to, body)| to.into_iter().map(move |to| (id(&to), body.clone())))
        .collect()
}

fn has_system_row(step: &RuntimeStep, to: &MemberId, needle: &str) -> bool {
    system_rows(step)
        .iter()
        .any(|(target, body)| target == to && body.contains(needle))
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
    assert!(has_system_row(
        &step,
        &main_id(),
        "\"a\" was refused: unknown backend"
    ));
    assert!(has_system_row(
        &step,
        &main_id(),
        "\"b\" was refused: unknown effort"
    ));
    // The refusal is shown once, as the row from Asterline, not also as a notice.
    assert!(!has_notice(&step, "could not spawn"));
}

#[test]
fn spawn_refuses_effort_the_backend_cannot_use() {
    let mut rt = spawn_runtime();

    let step = main_replies(&mut rt, &spawn_line("a", r#","effort":"ultra""#));

    assert!(rt.config.member(&id("a")).is_none());
    assert!(has_system_row(
        &step,
        &main_id(),
        "does not support ultra effort"
    ));
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
    assert!(has_system_row(&step, &main_id(), "not your sub-agent"));
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
    // The existing child is the same member, now on the backend asked for
    // (see the re-spawn tests); the other fields keep their values.
    let updated = rt.config.member(&id("reviewer")).unwrap();
    assert_eq!(updated.backend, BackendKind::Codex);
    assert_eq!(updated.id, first.id);
    assert_eq!(updated.parent, first.parent);
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
    assert!(has_system_row(&step, &main_id(), "not your sub-agent"));
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
    assert!(has_system_row(&step, &main_id(), "maximum of 8 sub-agents"));

    let reuse = complete_ok(&mut rt, &main_id(), &spawn_line("kid0", ""));
    assert!(!has_notice(&reuse, "could not spawn"));
}

#[test]
fn a_sub_agent_cannot_spawn() {
    let mut rt = spawn_runtime();
    main_replies(&mut rt, &spawn_line("reviewer", ""));

    let step = complete_ok(&mut rt, &id("reviewer"), &spawn_line("grandchild", ""));

    assert!(rt.config.member(&id("grandchild")).is_none());
    assert!(has_system_row(
        &step,
        &id("reviewer"),
        "cannot open sub-agents"
    ));
    assert!(has_system_row(
        &step,
        &id("reviewer"),
        "\"grandchild\" was refused"
    ));
    // Shown once, as the row from Asterline in the asking member's window.
    assert!(!has_notice(&step, "could not spawn"));
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
        assert!(
            has_system_row(&step, &main_id(), "normal or team mode"),
            "{mode:?}"
        );
    }
}

#[test]
fn spawn_in_team_mode_respects_the_roster_lock() {
    let mut locked = spawn_runtime();
    start_team(&mut locked, "ship it");
    let coordinator = locked.config.default_member_ids()[0].clone();
    let step = complete_ok(&mut locked, &coordinator, &spawn_line("reviewer", ""));
    assert_eq!(locked.config.sub_agent_count(), 0);
    assert!(has_system_row(
        &step,
        &coordinator,
        "locked to the current roster"
    ));

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
fn edited_child_settings_are_saved_with_the_chat_and_busy_edits_are_rejected() {
    let mut rt = spawn_runtime();
    main_replies(&mut rt, &spawn_line("reviewer", ""));
    let original = strip_team_protocols(rt.config.clone());
    let mut edited = original.members.clone();
    let child = edited
        .iter_mut()
        .find(|member| member.id == id("reviewer"))
        .unwrap();
    child.model = Some("sonnet".to_string());
    child.effort = Some(Effort::Low);
    child.permission_mode = Some(PermissionMode::AcceptEdits);
    child.display_name = "Researcher".to_string();
    let expected = child.clone();
    let rejected = rt.on_ui_command(UiCommand::ReplaceTeam {
        members: edited.clone(),
        default_target: original.default_target.clone(),
    });
    assert!(rejected.persist_team.is_none());
    assert_eq!(
        rt.config.member(&id("reviewer")).unwrap().model,
        original.member(&id("reviewer")).unwrap().model
    );
    complete_ok(&mut rt, &id("reviewer"), "done");
    complete_ok(&mut rt, &main_id(), "received");
    let applied = rt.on_ui_command(UiCommand::ReplaceTeam {
        members: edited,
        default_target: original.default_target,
    });
    let launch = applied.persist_team.expect("saved launch roster");
    assert!(launch.member(&id("reviewer")).is_none());
    let restored = rt.store.restore_active_team_config(&launch).unwrap();
    assert_eq!(restored.member(&id("reviewer")), Some(&expected));
    assert_eq!(
        restored.member(&main_id()).unwrap().model.as_deref(),
        Some("opus")
    );
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

// ---- model list: protocol, resolution, re-spawn, failure text ----

fn model(id: &str, name: &str, efforts: &[Effort]) -> crate::adapter::DiscoveredModel {
    let mut model = crate::adapter::DiscoveredModel::simple(id);
    model.name = name.to_string();
    model.supported_efforts = efforts.to_vec();
    model
}

const TO_MAX: [Effort; 5] = [
    Effort::Low,
    Effort::Medium,
    Effort::High,
    Effort::Xhigh,
    Effort::Max,
];
const TO_ULTRA: [Effort; 6] = [
    Effort::Low,
    Effort::Medium,
    Effort::High,
    Effort::Xhigh,
    Effort::Max,
    Effort::Ultra,
];

/// Codex and Claude catalogs, newest first, both backends installed.
fn model_directory() -> crate::domain::model_directory::ModelDirectory {
    use crate::domain::config::DetectedBackends;
    crate::domain::model_directory::ModelDirectory {
        installed: Some(DetectedBackends {
            codex: true,
            claude: true,
            grok: false,
            agy: false,
        }),
        catalogs: vec![
            (
                BackendKind::Codex,
                vec![
                    model("gpt-6.1-sol", "GPT-6.1-Sol", &TO_ULTRA),
                    model("gpt-6-astra", "GPT-6-Astra", &TO_ULTRA),
                    model("gpt-6.1-terra", "GPT-6.1-Terra", &TO_MAX),
                    model("gpt-6-sol", "GPT-6-Sol", &TO_MAX),
                    model("gpt-reserve", "gpt-reserve", &[]),
                ],
            ),
            (
                BackendKind::Claude,
                vec![
                    {
                        let mut opus = model("opus", "Opus", &TO_MAX);
                        opus.alt_ids = vec!["claude-opus-4-8".to_string()];
                        opus
                    },
                    model("sonnet", "Sonnet", &TO_MAX),
                ],
            ),
        ],
    }
}

fn runtime_with_models() -> TeamRuntime {
    let mut rt = spawn_runtime();
    rt.on_ui_command(UiCommand::SetModelDirectory(model_directory()));
    rt
}

/// The text of the system note `main` received after its last reply.
fn parent_notes(step: &RuntimeStep) -> Vec<String> {
    route_events(step)
        .into_iter()
        .filter(|(from, to, _)| from == &id("asterline") && to == &["main"])
        .map(|(_, _, body)| body)
        .collect()
}

#[test]
fn protocol_lists_the_models_once_the_catalog_arrives() {
    let mut rt = spawn_runtime();
    let before = rt
        .config
        .member(&main_id())
        .unwrap()
        .system_prompt
        .clone()
        .unwrap_or_default();
    assert!(!before.contains("Models you can use in @@spawn"));

    let step = rt.on_ui_command(UiCommand::SetModelDirectory(model_directory()));

    let prompt = rt
        .config
        .member(&main_id())
        .unwrap()
        .system_prompt
        .clone()
        .unwrap();
    assert!(prompt.contains("Models you can use in @@spawn"));
    assert!(prompt.contains("use these exact ids"));
    assert!(prompt.contains(
        "codex: gpt-6.1-sol (GPT-6.1-Sol; low…ultra), gpt-6-astra (GPT-6-Astra; low…ultra), \
         gpt-6.1-terra (GPT-6.1-Terra; low…max), gpt-6-sol (GPT-6-Sol; low…max), gpt-reserve"
    ));
    assert!(prompt.contains("claude: opus (Opus; low…max), sonnet (Sonnet; low…max)"));
    // The running member's runner is rebuilt with the new text, no restart.
    let upsert = step
        .runner_changes
        .iter()
        .find_map(|change| match change {
            RunnerChange::Upsert { member, .. } if member.id == main_id() => Some(member),
            _ => None,
        })
        .expect("main's runner is refreshed");
    assert!(
        upsert
            .system_prompt
            .as_deref()
            .unwrap()
            .contains("gpt-6-astra (GPT-6-Astra; low…ultra)")
    );
    // The same list again changes nothing; a changed one refreshes.
    let again = rt.on_ui_command(UiCommand::SetModelDirectory(model_directory()));
    assert!(again.runner_changes.is_empty());
    // Sub-agents cannot spawn, so their protocol carries no list.
    main_replies(&mut rt, &spawn_line("reviewer", ""));
    assert!(
        !rt.config
            .member(&id("reviewer"))
            .unwrap()
            .system_prompt
            .as_deref()
            .unwrap()
            .contains("Models you can use")
    );
}

#[test]
fn a_busy_member_gets_the_model_list_when_its_next_run_starts() {
    let mut rt = spawn_runtime();
    let started = rt.on_ui_command(user("go"));
    assert_eq!(started.actions[0].member, main_id());

    let step = rt.on_ui_command(UiCommand::SetModelDirectory(model_directory()));
    assert!(
        !step.runner_changes.iter().any(
            |change| matches!(change, RunnerChange::Upsert { member, .. } if member.id == main_id())
        ),
        "a running member keeps its runner"
    );
    complete_ok(&mut rt, &main_id(), "done");
    let next = rt.on_ui_command(user("again"));
    assert!(
        next.runner_changes.iter().any(
            |change| matches!(change, RunnerChange::Upsert { member, .. }
            if member.id == main_id()
                && member.system_prompt.as_deref().unwrap_or("").contains("gpt-6-astra"))
        ),
        "refreshed before the run"
    );
    assert_eq!(next.actions[0].member, main_id());
}

#[test]
fn a_loose_model_name_is_resolved_and_the_parent_is_told_which_id_ran() {
    let mut rt = runtime_with_models();

    let step = main_replies(
        &mut rt,
        &spawn_line(
            "helper",
            r#","backend":"codex","model":"gpt-astra","effort":"max""#,
        ),
    );

    let child = rt.config.member(&id("helper")).expect("opened");
    assert_eq!(child.backend, BackendKind::Codex);
    assert_eq!(child.model.as_deref(), Some("gpt-6-astra"));
    assert_eq!(child.effort, Some(Effort::Max));
    assert_eq!(
        parent_notes(&step),
        vec![
            r#"Sub-agent "helper" opened on codex · gpt-6-astra (GPT-6-Astra) · max."#.to_string()
        ]
    );
}

#[test]
fn exact_ids_alternate_ids_and_display_names_resolve() {
    let mut rt = runtime_with_models();

    let exact = main_replies(
        &mut rt,
        &spawn_line("a", r#","backend":"codex","model":"gpt-6-astra""#),
    );
    assert_eq!(
        rt.config.member(&id("a")).unwrap().model.as_deref(),
        Some("gpt-6-astra")
    );
    assert!(parent_notes(&exact).is_empty(), "an exact id needs no note");

    main_replies_again(&mut rt, &spawn_line("b", r#","model":"claude-opus-4-8""#));
    assert_eq!(
        rt.config.member(&id("b")).unwrap().model.as_deref(),
        Some("claude-opus-4-8")
    );

    let display = main_replies_again(
        &mut rt,
        &spawn_line("c", r#","backend":"codex","model":"GPT 6 Astra""#),
    );
    assert_eq!(
        rt.config.member(&id("c")).unwrap().model.as_deref(),
        Some("gpt-6-astra")
    );
    assert!(parent_notes(&display)[0].contains("gpt-6-astra (GPT-6-Astra)"));
}

/// A further `main` turn after the first one has finished.
fn main_replies_again(rt: &mut TeamRuntime, reply: &str) -> RuntimeStep {
    complete_ok(rt, &main_id(), "settle");
    main_replies(rt, reply)
}

#[test]
fn an_ambiguous_model_name_uses_the_newest_match_and_says_so() {
    let mut rt = runtime_with_models();

    let step = main_replies(
        &mut rt,
        &spawn_line("coder", r#","backend":"codex","model":"sol""#),
    );

    assert_eq!(
        rt.config.member(&id("coder")).unwrap().model.as_deref(),
        Some("gpt-6.1-sol")
    );
    let note = parent_notes(&step).join(" ");
    assert!(note.contains("gpt-6.1-sol"), "{note}");
    assert!(note.contains("matches several"), "{note}");
    assert!(note.contains("gpt-6-sol"), "{note}");
}

#[test]
fn an_unknown_model_is_refused_with_the_valid_ids() {
    let mut rt = runtime_with_models();

    let step = main_replies(
        &mut rt,
        &spawn_line("coder", r#","backend":"codex","model":"banana""#),
    );

    assert!(rt.config.member(&id("coder")).is_none());
    let note = parent_notes(&step).join(" ");
    assert!(note.contains("was refused"), "{note}");
    assert!(note.contains("gpt-6-astra (GPT-6-Astra)"), "{note}");
    assert!(note.contains("gpt-6.1-sol (GPT-6.1-Sol)"), "{note}");
}

#[test]
fn an_effort_the_model_lacks_is_refused_with_the_supported_ones() {
    let mut rt = runtime_with_models();

    let step = main_replies(
        &mut rt,
        &spawn_line(
            "coder",
            r#","backend":"codex","model":"gpt-6-sol","effort":"ultra""#,
        ),
    );

    assert!(rt.config.member(&id("coder")).is_none());
    let note = parent_notes(&step).join(" ");
    assert!(note.contains("low, medium, high, xhigh, max"), "{note}");
    // Not changed silently into something else.
    assert!(note.contains("does not support ultra"), "{note}");
}

#[test]
fn unknown_catalog_passes_the_request_through() {
    let mut rt = spawn_runtime();
    let mut directory = model_directory();
    directory
        .catalogs
        .retain(|(backend, _)| *backend == BackendKind::Codex);
    rt.on_ui_command(UiCommand::SetModelDirectory(directory));

    // Claude's catalog is unknown: whatever the agent wrote is used.
    main_replies(
        &mut rt,
        &spawn_line("coder", r#","model":"whatever-9","effort":"high""#),
    );

    let child = rt.config.member(&id("coder")).unwrap();
    assert_eq!(child.model.as_deref(), Some("whatever-9"));
    assert_eq!(child.effort, Some(Effort::High));
}

#[test]
fn a_backend_that_is_not_installed_is_refused() {
    let mut rt = spawn_runtime();
    let mut directory = model_directory();
    directory.installed = Some(crate::domain::config::DetectedBackends {
        codex: false,
        claude: true,
        grok: false,
        agy: false,
    });
    directory
        .catalogs
        .retain(|(backend, _)| *backend == BackendKind::Claude);
    rt.on_ui_command(UiCommand::SetModelDirectory(directory));

    let step = main_replies(&mut rt, &spawn_line("coder", r#","backend":"codex""#));

    assert!(rt.config.member(&id("coder")).is_none());
    assert!(
        parent_notes(&step)
            .join(" ")
            .contains("codex is not installed")
    );
}

#[test]
fn an_inherited_effort_the_chosen_model_lacks_is_dropped() {
    let mut rt = spawn_runtime();
    rt.config.members[0].effort = Some(Effort::Ultra);
    rt.config.members[0].backend = BackendKind::Codex;
    rt.config.members[0].model = Some("gpt-6.1-sol".to_string());
    rt.on_ui_command(UiCommand::SetModelDirectory(model_directory()));

    main_replies(&mut rt, &spawn_line("coder", r#","model":"gpt-6-sol""#));

    let child = rt.config.member(&id("coder")).unwrap();
    assert_eq!(child.model.as_deref(), Some("gpt-6-sol"));
    assert_eq!(child.effort, None);
}

#[test]
fn respawn_switches_the_model_then_delivers_the_task() {
    let mut rt = runtime_with_models();
    main_replies(
        &mut rt,
        &spawn_line(
            "helper",
            r#","backend":"codex","model":"gpt-6.1-sol","effort":"high""#,
        ),
    );
    complete_ok(&mut rt, &id("helper"), "first");
    complete_ok(&mut rt, &main_id(), "noted");

    rt.on_ui_command(user("again"));
    let step = complete_ok(
        &mut rt,
        &main_id(),
        r#"@@spawn {"name":"helper","task":"second round","model":"astra","backend":"codex"}"#,
    );

    let child = rt.config.member(&id("helper")).unwrap();
    assert_eq!(child.model.as_deref(), Some("gpt-6-astra"));
    assert_eq!(child.backend, BackendKind::Codex);
    // Not named in the request: kept.
    assert_eq!(child.effort, Some(Effort::High));
    assert_eq!(rt.config.sub_agent_count(), 1);
    assert!(step.runner_changes.iter().any(|change| matches!(
        change,
        RunnerChange::Upsert { member, .. } if member.id == id("helper")
            && member.model.as_deref() == Some("gpt-6-astra")
    )));
    assert!(parent_notes(&step).iter().any(|note| {
        note.contains(r#"Sub-agent "helper" switched to codex · gpt-6-astra (GPT-6-Astra) · high."#)
    }));
    let action = step
        .actions
        .iter()
        .find(|action| action.member == id("helper"))
        .expect("task delivered");
    assert!(action.prompt.contains("second round"));
}

#[test]
fn respawn_without_model_fields_leaves_the_child_alone() {
    let mut rt = runtime_with_models();
    main_replies(
        &mut rt,
        &spawn_line("helper", r#","backend":"codex","model":"gpt-6.1-sol""#),
    );
    complete_ok(&mut rt, &id("helper"), "first");
    complete_ok(&mut rt, &main_id(), "noted");
    let before = rt.config.member(&id("helper")).cloned().unwrap();

    rt.on_ui_command(user("again"));
    let step = complete_ok(
        &mut rt,
        &main_id(),
        r#"@@spawn {"name":"helper","task":"more"}"#,
    );

    assert_eq!(rt.config.member(&id("helper")), Some(&before));
    assert!(parent_notes(&step).is_empty());
}

#[test]
fn respawn_to_another_backend_starts_a_fresh_session_with_that_backends_defaults() {
    let mut rt = runtime_with_models();
    main_replies(&mut rt, &spawn_line("helper", ""));
    complete_ok(&mut rt, &id("helper"), "first");
    complete_ok(&mut rt, &main_id(), "noted");

    rt.on_ui_command(user("again"));
    complete_ok(
        &mut rt,
        &main_id(),
        r#"@@spawn {"name":"helper","task":"more","backend":"codex","model":"terra","effort":"max"}"#,
    );

    let child = rt.config.member(&id("helper")).unwrap();
    assert_eq!(child.backend, BackendKind::Codex);
    assert_eq!(child.model.as_deref(), Some("gpt-6.1-terra"));
    assert_eq!(child.effort, Some(Effort::Max));
    assert_eq!(child.session_id, None);
    assert_eq!(
        child.codex_permissions_preset(),
        Some(crate::domain::team::CodexPermissionsPreset::AskForApproval)
    );
}

#[test]
fn respawn_with_a_bad_effort_for_the_childs_own_model_is_refused() {
    let mut rt = runtime_with_models();
    main_replies(
        &mut rt,
        &spawn_line("helper", r#","backend":"codex","model":"gpt-6-sol""#),
    );
    complete_ok(&mut rt, &id("helper"), "first");
    complete_ok(&mut rt, &main_id(), "noted");

    rt.on_ui_command(user("again"));
    let step = complete_ok(
        &mut rt,
        &main_id(),
        r#"@@spawn {"name":"helper","task":"more","effort":"ultra"}"#,
    );

    assert_eq!(
        rt.config.member(&id("helper")).unwrap().effort,
        Some(Effort::High)
    );
    assert!(
        parent_notes(&step)
            .join(" ")
            .contains("does not support ultra")
    );
}

#[test]
fn respawn_of_a_busy_child_is_refused_instead_of_changing_it_mid_run() {
    let mut rt = runtime_with_models();
    main_replies(
        &mut rt,
        &spawn_line("helper", r#","backend":"codex","model":"gpt-6.1-sol""#),
    );
    // The child is still working on its first task.
    rt.on_ui_command(user("again"));
    let step = complete_ok(
        &mut rt,
        &main_id(),
        r#"@@spawn {"name":"helper","task":"more","model":"astra"}"#,
    );

    assert_eq!(
        rt.config.member(&id("helper")).unwrap().model.as_deref(),
        Some("gpt-6.1-sol")
    );
    assert!(
        parent_notes(&step).join(" ").contains("still working"),
        "{:?}",
        route_events(&step)
    );
}

#[test]
fn a_failed_child_hands_back_the_real_error() {
    let mut rt = spawn_runtime();
    main_replies(&mut rt, &spawn_line("helper", ""));
    let child = id("helper");
    let message = "The 'gpt-astra' model is not supported when using Codex with a ChatGPT account.";
    rt.on_agent_event(&child, AgentEvent::Fatal(message.to_string()));
    let failed = rt.on_agent_event(
        &child,
        AgentEvent::Exited {
            code: Some(1),
            ok: false,
        },
    );

    assert_eq!(
        route_events(&failed)[0].2,
        format!("Helper could not finish the task: {message}")
    );
}

#[test]
fn a_refused_spawn_is_shown_once_as_the_row_from_asterline() {
    let mut rt = spawn_runtime();

    let step = main_replies(&mut rt, &spawn_line("a", r#","effort":"ultra""#));

    let rows = system_rows(&step);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert!(rows[0].1.contains("was refused"), "{rows:?}");
    // No notice repeats it ("Main could not spawn a: ...").
    assert!(
        !step.events.iter().any(|event| matches!(
            event,
            RuntimeEvent::Notice(text) | RuntimeEvent::MemberNotice { text, .. }
                if text.contains("could not spawn") || text.contains("does not support ultra")
        )),
        "{:?}",
        step.events
    );
}

// ---- removing a sub-agent: the user's Ctrl+X and the parent's @@close ----

fn remove(member: &str) -> UiCommand {
    UiCommand::RemoveSubAgent { member: id(member) }
}

/// `(parent, child, display name, removed by the user)` of each removal.
fn closed_events(step: &RuntimeStep) -> Vec<(MemberId, MemberId, String, bool)> {
    step.events
        .iter()
        .filter_map(|event| match event {
            RuntimeEvent::SubAgentClosed {
                parent,
                child,
                by_user,
                departed,
            } => Some((
                parent.clone(),
                child.clone(),
                departed.display_name.clone(),
                *by_user,
            )),
            _ => None,
        })
        .collect()
}

fn ready_without(step: &RuntimeStep, gone: &str) -> bool {
    step.events.iter().any(|event| {
        matches!(
            event,
            RuntimeEvent::Ready { members, .. }
                if members.iter().all(|m| m.id != id(gone))
        )
    })
}

/// `main` with an idle `reviewer` sub-agent whose first task is done.
fn runtime_with_idle_reviewer() -> TeamRuntime {
    let mut rt = spawn_runtime();
    main_replies(&mut rt, &spawn_line("reviewer", ""));
    complete_ok(&mut rt, &id("reviewer"), "r");
    complete_ok(&mut rt, &main_id(), "done");
    rt
}

#[test]
fn the_user_removes_an_idle_sub_agent_and_the_parent_is_told() {
    let mut rt = runtime_with_idle_reviewer();
    let original = rt.store.active_conversation();
    rt.on_agent_event(
        &id("reviewer"),
        AgentEvent::SessionDiscovered(AgentSessionId("child-session".to_string())),
    );

    let step = rt.on_ui_command(remove("reviewer"));

    assert!(rt.config.member(&id("reviewer")).is_none());
    assert_eq!(rt.config.sub_agent_count(), 0);
    assert!(!rt.members.contains_key(&id("reviewer")));
    assert!(rt.sessions.get(&id("reviewer")).is_none());
    assert!(step.runner_changes.iter().any(|change| matches!(
        change,
        RunnerChange::Remove(member) if member == &id("reviewer")
    )));
    assert!(ready_without(&step, "reviewer"));
    assert_eq!(
        closed_events(&step),
        vec![(main_id(), id("reviewer"), "Reviewer".to_string(), true)]
    );
    // Its name and model settings are saved with the chat, to label its
    // earlier messages after a restart or /resume.
    let departed = rt.store.departed_members().unwrap();
    assert_eq!(departed.len(), 1);
    assert_eq!(departed[0].id, id("reviewer"));
    assert_eq!(departed[0].display_name, "Reviewer");
    // The team file never held it, and the roster file is rewritten.
    assert!(
        step.persist_team
            .as_ref()
            .unwrap()
            .members
            .iter()
            .all(|m| m.parent.is_none())
    );
    // The parent's protocol no longer lists it.
    let prompt = rt
        .config
        .member(&main_id())
        .unwrap()
        .system_prompt
        .clone()
        .unwrap();
    assert!(!prompt.contains("Your sub-agents"), "{prompt}");
    assert!(step.runner_changes.iter().any(|change| matches!(
        change,
        RunnerChange::Upsert { member, .. }
            if member.id == main_id()
                && !member.system_prompt.clone().unwrap_or_default().contains("Your sub-agents")
    )));
    // The parent gets the note as a visible row and as a prompt.
    let note = r#"Sub-agent "reviewer" was removed by the user."#;
    assert!(
        has_system_row(&step, &main_id(), note),
        "{:?}",
        system_rows(&step)
    );
    let action = step
        .actions
        .iter()
        .find(|action| action.member == main_id())
        .expect("the parent is told");
    assert!(action.prompt.contains("[system message from Asterline]"));
    assert!(action.prompt.contains(note));

    // Persisted: a restart and resuming the chat do not bring it back.
    let launch = rt.config.without_sub_agents();
    let restored = rt.store.restore_active_team_config(&launch).unwrap();
    assert!(restored.member(&id("reviewer")).is_none());
    complete_ok(&mut rt, &main_id(), "noted");
    rt.on_ui_command(UiCommand::NewSession { prompt: None });
    rt.on_ui_command(UiCommand::ResumeConversation {
        conversation: original,
    });
    assert!(rt.config.member(&id("reviewer")).is_none());
    assert!(rt.config.member(&main_id()).is_some());
}

#[test]
fn removing_a_busy_sub_agent_interrupts_it_first_and_drops_what_waited_for_it() {
    let mut rt = spawn_runtime();
    main_replies(&mut rt, &spawn_line("reviewer", ""));
    let child = id("reviewer");
    // The task is running; one more message waits behind it.
    rt.on_ui_command(UiCommand::UserMessage {
        target: MessageTarget::Member(child.clone()),
        body: "and also this".to_string(),
    });
    assert_eq!(rt.members[&child].queue.len(), 1);

    let step = rt.on_ui_command(remove("reviewer"));

    // Interrupted like Esc would, queue dropped, but it leaves only once the
    // run has actually ended.
    assert!(
        rt.members[&child]
            .running
            .as_ref()
            .unwrap()
            .cancel
            .load(Ordering::Relaxed)
    );
    assert!(rt.members[&child].queue.is_empty());
    assert!(has_notice(&step, "cancelling reviewer"));
    assert!(rt.config.member(&child).is_some());
    assert!(closed_events(&step).is_empty());

    let step = rt.on_agent_event(
        &child,
        AgentEvent::Exited {
            code: None,
            ok: false,
        },
    );

    assert!(rt.config.member(&child).is_none());
    assert!(!rt.members.contains_key(&child));
    assert!(rt.closing.is_empty());
    assert_eq!(closed_events(&step).len(), 1);
    assert!(ready_without(&step, "reviewer"));
    // Not handed back as if it had finished; the parent only hears of the removal.
    assert!(
        !route_events(&step)
            .iter()
            .any(|(from, _, _)| from == &child)
    );
    let note = r#"Sub-agent "reviewer" was removed by the user."#;
    assert!(has_system_row(&step, &main_id(), note));
    let action = step
        .actions
        .iter()
        .find(|action| action.member == main_id())
        .expect("the parent is told");
    assert!(action.prompt.contains(note));
}

#[test]
fn removing_a_sub_agent_waiting_on_an_approval_rejects_it() {
    let mut rt = spawn_runtime().with_approvals(true);
    main_replies(&mut rt, &spawn_line("reviewer", ""));
    let child = id("reviewer");
    rt.on_agent_event(
        &child,
        AgentEvent::NativeApprovalRequested {
            request_id: 7,
            action: "run a command".to_string(),
            body: String::new(),
        },
    );
    assert!(!rt.native_approvals.is_empty());

    let step = rt.on_ui_command(remove("reviewer"));

    assert!(rt.native_approvals.is_empty());
    assert!(step.events.iter().any(|event| matches!(
        event,
        RuntimeEvent::ApprovalResolved {
            decision: ApprovalDecision::Reject,
            ..
        }
    )));
    assert!(step.runner_controls.iter().any(|control| matches!(
        control,
        RunnerControl::ResolveNativeApproval {
            decision: ApprovalDecision::Reject,
            ..
        }
    )));
}

#[test]
fn removal_is_refused_for_top_level_and_unknown_members() {
    let mut rt = runtime_with_idle_reviewer();

    for name in ["main", "other", "nobody"] {
        let step = rt.on_ui_command(remove(name));
        assert!(closed_events(&step).is_empty(), "{name}");
        assert!(step.runner_changes.is_empty(), "{name}");
        assert!(
            step.events
                .iter()
                .any(|event| matches!(event, RuntimeEvent::Notice(_))),
            "{name}: {:?}",
            step.events
        );
    }
    let step = rt.on_ui_command(remove("main"));
    assert!(has_notice(&step, "Main is not a sub-agent"));
    assert!(rt.config.member(&main_id()).is_some());
    assert!(rt.config.member(&id("other")).is_some());
    assert!(rt.config.member(&id("reviewer")).is_some());
}

#[test]
fn a_second_removal_while_the_first_winds_down_is_refused() {
    let mut rt = spawn_runtime();
    main_replies(&mut rt, &spawn_line("reviewer", ""));
    rt.on_ui_command(remove("reviewer"));

    let step = rt.on_ui_command(remove("reviewer"));

    assert!(has_notice(&step, "already being removed"));
}

#[test]
fn nothing_new_reaches_a_sub_agent_that_is_being_removed() {
    let mut rt = spawn_runtime();
    main_replies(&mut rt, &spawn_line("reviewer", ""));
    let child = id("reviewer");
    rt.on_ui_command(remove("reviewer"));

    rt.on_ui_command(UiCommand::UserMessage {
        target: MessageTarget::Member(child.clone()),
        body: "too late".to_string(),
    });
    assert!(rt.members[&child].queue.is_empty());

    let step = rt.on_agent_event(
        &child,
        AgentEvent::Exited {
            code: None,
            ok: false,
        },
    );
    assert!(rt.config.member(&child).is_none());
    assert!(!step.actions.iter().any(|action| action.member == child));
}

#[test]
fn close_from_the_parent_removes_the_sub_agent_and_confirms() {
    for line in [
        r#"@@close {"name":"reviewer"}"#,
        r#"@@close {"id":"reviewer"}"#,
        r#"@@close {"name":"Reviewer"}"#,
    ] {
        let mut rt = runtime_with_idle_reviewer();
        rt.on_ui_command(user("clean up"));

        let step = complete_ok(&mut rt, &main_id(), &format!("Done with it.\n{line}"));

        assert!(rt.config.member(&id("reviewer")).is_none(), "{line}");
        assert_eq!(
            closed_events(&step),
            vec![(main_id(), id("reviewer"), "Reviewer".to_string(), false)],
            "{line}"
        );
        assert!(ready_without(&step, "reviewer"), "{line}");
        assert!(step.runner_changes.iter().any(|change| matches!(
            change,
            RunnerChange::Remove(member) if member == &id("reviewer")
        )));
        let note = r#"Sub-agent "reviewer" closed."#;
        assert!(has_system_row(&step, &main_id(), note), "{line}");
        let action = step
            .actions
            .iter()
            .find(|action| action.member == main_id())
            .expect("the parent gets the confirmation");
        assert!(action.prompt.contains(note), "{line}");
    }
}

#[test]
fn close_of_a_busy_sub_agent_stops_it_and_confirms_once_it_has_ended() {
    let mut rt = spawn_runtime();
    main_replies(&mut rt, &spawn_line("reviewer", ""));
    let child = id("reviewer");
    rt.on_ui_command(user("more"));
    let step = complete_ok(&mut rt, &main_id(), r#"@@close {"name":"reviewer"}"#);
    assert!(closed_events(&step).is_empty());
    assert!(
        rt.members[&child]
            .running
            .as_ref()
            .unwrap()
            .cancel
            .load(Ordering::Relaxed)
    );

    let step = rt.on_agent_event(
        &child,
        AgentEvent::Exited {
            code: None,
            ok: false,
        },
    );

    assert!(rt.config.member(&child).is_none());
    assert!(has_system_row(
        &step,
        &main_id(),
        r#"Sub-agent "reviewer" closed."#
    ));
}

#[test]
fn close_is_refused_for_unknown_names_and_members_that_are_not_the_senders() {
    let mut rt = runtime_with_idle_reviewer();
    // `other` has a sub-agent of its own.
    let mut helper = TeamMember::new("helper", "Helper", BackendKind::Codex, "sub-agent");
    helper.parent = Some(id("other"));
    let mut members = rt.config.members.clone();
    members.push(helper);
    rt.on_ui_command(UiCommand::ReplaceTeam {
        members,
        default_target: Some(DefaultTarget::Member(main_id())),
    });
    assert!(rt.config.member(&id("helper")).is_some());

    for (name, needle) in [
        ("ghost", "no sub-agent named"),
        ("other", "not your sub-agent"),
        ("main", "not your sub-agent"),
        ("helper", "not your sub-agent"),
    ] {
        rt.on_ui_command(user("go"));
        let step = complete_ok(
            &mut rt,
            &main_id(),
            &format!(r#"@@close {{"name":"{name}"}}"#),
        );
        assert!(closed_events(&step).is_empty(), "{name}");
        assert!(
            has_system_row(&step, &main_id(), "was refused")
                && has_system_row(&step, &main_id(), needle),
            "{name}: {:?}",
            system_rows(&step)
        );
    }
    assert!(rt.config.member(&id("helper")).is_some());
    assert!(rt.config.member(&id("other")).is_some());
    assert!(rt.config.member(&id("reviewer")).is_some());
}

#[test]
fn a_sub_agent_cannot_close_its_sibling_or_itself() {
    let mut rt = spawn_runtime();
    main_replies(
        &mut rt,
        &format!("{}\n{}", spawn_line("a", ""), spawn_line("b", "")),
    );

    let step = complete_ok(&mut rt, &id("a"), r#"@@close {"name":"b"}"#);
    assert!(closed_events(&step).is_empty());
    assert!(has_system_row(&step, &id("a"), "not your sub-agent"));
    assert!(rt.config.member(&id("b")).is_some());
}

#[test]
fn a_malformed_close_line_stays_visible_and_changes_nothing() {
    let mut rt = runtime_with_idle_reviewer();
    rt.on_ui_command(user("go"));

    let step = complete_ok(&mut rt, &main_id(), r#"@@close {"name":"  "}"#);

    assert!(closed_events(&step).is_empty());
    assert!(rt.config.member(&id("reviewer")).is_some());
}

#[test]
fn the_protocol_text_documents_close_for_parents_only() {
    let mut rt = spawn_runtime();
    main_replies(&mut rt, &spawn_line("reviewer", ""));
    let main_prompt = rt
        .config
        .member(&main_id())
        .unwrap()
        .system_prompt
        .clone()
        .unwrap();
    assert!(main_prompt.contains("@@close"));
    let child_prompt = rt
        .config
        .member(&id("reviewer"))
        .unwrap()
        .system_prompt
        .clone()
        .unwrap();
    assert!(!child_prompt.contains("@@close"));
}

// ---- model list file and the instructions a resumed session is given ----

fn models_test_path(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("asterline-models-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir.join("models.md")
}

fn leftover_temp_files(path: &std::path::Path) -> usize {
    std::fs::read_dir(path.parent().unwrap())
        .unwrap()
        .filter(|entry| {
            entry
                .as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".tmp-")
        })
        .count()
}

#[test]
fn the_model_list_file_follows_the_directory() {
    let mut rt = spawn_runtime();
    let path = models_test_path("follows");
    rt.models_path = path.clone();

    // Nothing is known yet: nothing is written.
    rt.on_ui_command(UiCommand::SetModelDirectory(
        crate::domain::model_directory::ModelDirectory::default(),
    ));
    assert!(!path.exists());

    let step = rt.on_ui_command(UiCommand::SetModelDirectory(model_directory()));
    assert!(
        !step
            .events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::Notice(_)))
    );
    let text = std::fs::read_to_string(&path).unwrap();
    assert_eq!(text, model_directory().models_file().unwrap());
    assert!(text.starts_with("# Models Asterline can open with @@spawn"));
    assert!(
        text.contains("`gpt-6-astra` — GPT-6-Astra; efforts: low, medium, high, xhigh, max, ultra")
    );
    assert!(text.contains(
        "\"backend\", \"model\", and \"effort\" are optional and default to the parent's"
    ));
    // Backends in a fixed order, models in catalog order.
    let codex = text.find("## codex").unwrap();
    let claude = text.find("## claude").unwrap();
    assert!(codex < claude);
    assert!(text.find("`gpt-6.1-sol`").unwrap() < text.find("`gpt-6-astra`").unwrap());
    assert!(!text.contains("## grok"), "not installed");
    assert_eq!(leftover_temp_files(&path), 0);

    // The same list again leaves the file untouched.
    std::fs::write(&path, "sentinel").unwrap();
    rt.on_ui_command(UiCommand::SetModelDirectory(model_directory()));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "sentinel");

    // A changed list rewrites it in full, again without stray files.
    let mut changed = model_directory();
    changed.catalogs[1].1.push(model("haiku", "Haiku", &TO_MAX));
    rt.on_ui_command(UiCommand::SetModelDirectory(changed.clone()));
    let text = std::fs::read_to_string(&path).unwrap();
    assert_eq!(text, changed.models_file().unwrap());
    assert!(text.contains("`haiku` — Haiku"));
    assert_eq!(leftover_temp_files(&path), 0);
    std::fs::remove_dir_all(path.parent().unwrap()).ok();
}

#[test]
fn the_protocol_points_top_level_members_to_the_model_list() {
    let mut rt = spawn_runtime();
    rt.on_ui_command(UiCommand::SetModelDirectory(model_directory()));
    let main_prompt = rt
        .config
        .member(&main_id())
        .unwrap()
        .system_prompt
        .clone()
        .unwrap();
    let line = format!(
        "The current model list is kept in {}; read it when you need a model not listed here.",
        crate::state_paths::models_path().display()
    );
    assert!(main_prompt.contains(&line), "{main_prompt}");
    assert!(
        main_prompt.contains("Do not use your CLI's own built-in sub-agent or multi-agent tool")
    );
    assert!(
        main_prompt.contains("Models you can use in @@spawn"),
        "inline list stays"
    );

    main_replies(&mut rt, &spawn_line("reviewer", ""));
    let child = rt.config.member(&id("reviewer")).unwrap();
    assert!(
        !child
            .system_prompt
            .as_deref()
            .unwrap()
            .contains("model list")
    );
}

/// A Codex `main` carrying the team instructions, as in the app.
fn codex_runtime() -> TeamRuntime {
    let mut config = TeamConfig::new("codex", "/tmp/ws").with_member(TeamMember::new(
        "main",
        "Main",
        BackendKind::Codex,
        "general",
    ));
    config.default_target = Some(DefaultTarget::Member(main_id()));
    crate::domain::config::inject_team_protocol(&mut config);
    TeamRuntime::new(config, SqliteStore::in_memory().unwrap()).with_approvals(false)
}

fn send(rt: &mut TeamRuntime, text: &str) -> String {
    let step = rt.on_ui_command(user(text));
    step.actions[0].prompt.clone()
}

fn finish(rt: &mut TeamRuntime, session: Option<&str>) {
    if let Some(session) = session {
        rt.on_agent_event(
            &main_id(),
            AgentEvent::SessionDiscovered(AgentSessionId(session.to_string())),
        );
    }
    complete_ok(rt, &main_id(), "ok");
}

fn has_note(prompt: &str) -> bool {
    prompt.starts_with(crate::runtime::context_recap::PROTOCOL_NOTE_HEADER)
}

#[test]
fn a_resumed_codex_session_is_given_the_instructions_once() {
    let mut rt = codex_runtime();
    // A thread saved by an earlier run of Asterline.
    rt.sessions
        .set_external(main_id(), AgentSessionId("old-thread".to_string()), 0);

    let first = send(&mut rt, "hello");
    assert!(has_note(&first), "{first}");
    assert!(first.starts_with(
        "[system message from Asterline]\nCurrent Asterline team instructions for this session:\nYou are \"main\""
    ));
    assert!(first.contains("@@spawn"));
    assert!(first.ends_with("hello"));
    finish(&mut rt, None);

    let second = send(&mut rt, "again");
    assert_eq!(second, "again", "already told in this app run");
}

#[test]
fn a_new_codex_session_starts_with_the_instructions_and_gets_no_note() {
    let mut rt = codex_runtime();
    let first = send(&mut rt, "hello");
    assert_eq!(first, "hello");
    finish(&mut rt, Some("new-thread"));
    assert_eq!(send(&mut rt, "again"), "again");
}

#[test]
fn changed_instructions_reach_a_codex_session_on_its_next_prompt() {
    let mut rt = codex_runtime();
    assert_eq!(send(&mut rt, "one"), "one");
    finish(&mut rt, Some("thread"));

    rt.on_ui_command(UiCommand::SetModelDirectory(model_directory()));
    let next = send(&mut rt, "two");
    assert!(has_note(&next), "{next}");
    assert!(next.contains("gpt-6-astra (GPT-6-Astra; low…ultra)"));
    finish(&mut rt, None);
    assert_eq!(send(&mut rt, "three"), "three", "told once");
}

#[test]
fn backends_that_resend_their_instructions_every_run_get_no_note() {
    let mut rt = spawn_runtime();
    rt.on_ui_command(UiCommand::SetModelDirectory(model_directory()));
    // `main` runs on Claude, which passes --append-system-prompt every time.
    rt.sessions
        .set_external(main_id(), AgentSessionId("claude-session".to_string()), 0);
    assert_eq!(send(&mut rt, "hello"), "hello");
}

#[test]
fn new_chat_with_a_fresh_roster_drops_sub_agents_and_the_other_top_level_members() {
    let roster = crate::domain::config::FreshChatRoster::default_main(BackendKind::Codex);
    let mut rt = spawn_runtime().with_fresh_chat_roster(Some(roster));
    let old = rt.store.active_conversation();
    main_replies(&mut rt, &spawn_line("kid", ""));
    complete_ok(&mut rt, &id("kid"), "r");
    complete_ok(&mut rt, &main_id(), "done");
    assert_eq!(rt.config.sub_agent_count(), 1);

    let step = rt.on_ui_command(UiCommand::NewSession { prompt: None });

    assert_eq!(rt.config.sub_agent_count(), 0);
    assert_eq!(rt.config.members.len(), 1);
    assert!(rt.config.member(&id("other")).is_none());
    assert!(!rt.members.contains_key(&id("kid")));
    assert!(rt.sessions.get(&id("kid")).is_none());
    // The new Main is the default one: Codex, no pinned model or effort.
    let main = rt.config.member(&main_id()).unwrap();
    assert_eq!(main.backend, BackendKind::Codex);
    assert_eq!(main.model, None);
    assert_eq!(main.effort, None);
    for gone in ["kid", "other", "main"] {
        assert!(step.runner_changes.iter().any(|change| matches!(
            change,
            RunnerChange::Remove(member) if member == &id(gone)
        )));
    }
    let snapshot = rt
        .store
        .conversation_snapshot(rt.store.active_conversation())
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.team.members.len(), 1);
    // The old chat keeps Main (Claude), Other and the sub-agent for /resume.
    let previous = rt.store.conversation_snapshot(old).unwrap().unwrap();
    assert_eq!(previous.team.members.len(), 3);
    let prompt = main.system_prompt.clone().unwrap();
    assert!(!prompt.contains("Your sub-agents"));
}
