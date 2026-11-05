use super::*;

fn fail(rt: &mut TeamRuntime, member: &MemberId) {
    rt.on_agent_event(
        member,
        AgentEvent::Fatal("Connection dropped (ECONNRESET)".into()),
    );
    // Claude can report a structured API error and still exit zero.
    rt.on_agent_event(
        member,
        AgentEvent::Exited {
            code: Some(0),
            ok: true,
        },
    );
}

#[test]
fn retry_uses_failed_agent_input_and_sender_instead_of_last_user() {
    let mut rt = runtime();
    let builder = MemberId::new("builder");
    let reviewer = MemberId::new("reviewer");
    rt.on_ui_command(user("original user task"));
    complete_ok(&mut rt, &builder, "done");
    let turn = rt.store.create_turn().unwrap();
    rt.enqueue_prompt_from(
        &reviewer,
        turn,
        "research result to process".into(),
        Some(builder.clone()),
        &mut RuntimeStep::default(),
    );
    fail(&mut rt, &reviewer);
    let retry = rt.on_ui_command(UiCommand::Retry);
    assert_eq!(retry.actions.len(), 1);
    assert_eq!(retry.actions[0].member, reviewer);
    assert!(
        retry.actions[0]
            .prompt
            .contains("research result to process")
    );
    assert_eq!(
        rt.members[&reviewer].running.as_ref().unwrap().origin,
        Some(builder)
    );
    assert!(
        !retry
            .events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::UserMessage { .. }))
    );
}

#[test]
fn retry_includes_delivered_agent_messages_in_order_without_growth_on_repeated_failure() {
    for backend in [BackendKind::Codex, BackendKind::Claude] {
        let mut config = team();
        config.members[0].backend = backend;
        let mut rt =
            TeamRuntime::new(config, SqliteStore::in_memory().unwrap()).with_approvals(false);
        let builder = MemberId::new("builder");
        let reviewer = MemberId::new("reviewer");
        let started = rt.on_ui_command(user("original task"));
        let steer = started.actions[0].steer.as_ref().unwrap();
        for body in ["first research finding", "second research finding"] {
            let turn = rt.store.create_turn().unwrap();
            rt.enqueue_prompt_from(
                &builder,
                turn,
                body.into(),
                Some(reviewer.clone()),
                &mut RuntimeStep::default(),
            );
            assert_eq!(steer.try_recv().as_deref(), Some(body));
            rt.on_agent_event(&builder, AgentEvent::SteerDelivered);
        }
        fail(&mut rt, &builder);
        let retry = rt.on_ui_command(UiCommand::Retry);
        let prompt = &retry.actions[0].prompt;
        assert!(prompt.contains("original task"));
        assert!(
            prompt.find("first research finding").unwrap()
                < prompt.find("second research finding").unwrap()
        );
        let original = rt.members[&builder]
            .running
            .as_ref()
            .unwrap()
            .retry_prompt
            .clone();
        fail(&mut rt, &builder);
        rt.on_ui_command(UiCommand::Retry);
        assert_eq!(
            rt.members[&builder].running.as_ref().unwrap().retry_prompt,
            original
        );
    }
}

#[test]
fn retry_excludes_unconfirmed_agent_message_and_waits_behind_its_running_delivery() {
    let mut rt = runtime();
    let builder = MemberId::new("builder");
    let reviewer = MemberId::new("reviewer");
    rt.on_ui_command(user("failed task"));
    let turn = rt.store.create_turn().unwrap();
    rt.enqueue_prompt_from(
        &builder,
        turn,
        "unconfirmed payload".into(),
        Some(reviewer),
        &mut RuntimeStep::default(),
    );
    fail(&mut rt, &builder);
    assert!(
        rt.members[&builder]
            .running
            .as_ref()
            .unwrap()
            .retry_prompt
            .contains("unconfirmed payload")
    );
    let retry = rt.on_ui_command(UiCommand::Retry);
    assert!(retry.actions.is_empty());
    assert_eq!(rt.members[&builder].queue.len(), 1);
    assert_eq!(rt.members[&builder].queue[0].prompt, "failed task");
    let next = complete_ok(&mut rt, &builder, "delivered queued message");
    assert_eq!(next.actions.len(), 1);
    assert_eq!(
        rt.members[&builder].running.as_ref().unwrap().retry_prompt,
        "failed task"
    );
}

#[test]
fn new_chat_discards_failed_agent_input() {
    let mut rt = runtime();
    let builder = MemberId::new("builder");
    rt.on_ui_command(user("old failed request"));
    fail(&mut rt, &builder);
    rt.on_ui_command(UiCommand::NewSession { prompt: None });
    let retry = rt.on_ui_command(UiCommand::Retry);
    assert!(retry.actions.is_empty());
    assert!(
        retry
            .events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::Notice(text) if text == "nothing to retry"))
    );
}

#[test]
fn cancellation_does_not_replace_a_failed_input() {
    let mut rt = runtime();
    let builder = MemberId::new("builder");
    rt.on_ui_command(user("failed task"));
    fail(&mut rt, &builder);
    rt.on_ui_command(user("cancelled task"));
    rt.on_ui_command(UiCommand::Cancel {
        member: Some(builder.clone()),
    });
    rt.on_agent_event(
        &builder,
        AgentEvent::Exited {
            code: None,
            ok: false,
        },
    );
    rt.on_ui_command(UiCommand::Retry);
    assert_eq!(
        rt.members[&builder].running.as_ref().unwrap().retry_prompt,
        "failed task"
    );
}

#[test]
fn retry_does_not_detach_failed_workflow_from_its_state() {
    let mut rt = runtime();
    let builder = MemberId::new("builder");
    start_team(&mut rt, "tracked task");
    fail(&mut rt, &builder);
    let retry = rt.on_ui_command(UiCommand::Retry);
    assert!(retry.actions.is_empty());
    assert!(
        retry
            .events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::Notice(text) if text.contains("/continue")))
    );
}

#[test]
fn retry_of_failed_child_still_hands_its_result_back_to_parent() {
    let mut rt = runtime();
    let parent = MemberId::new("builder");
    let child = MemberId::new("researcher");
    let started = rt.on_ui_command(user("research this"));
    let parent_input = started.actions[0].steer.as_ref().unwrap();
    rt.on_agent_event(
        &parent,
        AgentEvent::MessageCompleted(
            r#"@@spawn {"name":"researcher","task":"find evidence"}"#.into(),
        ),
    );
    fail(&mut rt, &child);
    assert!(
        parent_input
            .try_recv()
            .unwrap()
            .contains("could not finish")
    );
    rt.on_agent_event(&parent, AgentEvent::SteerDelivered);
    let retry = rt.on_ui_command(UiCommand::Retry);
    assert_eq!(retry.actions[0].member, child);
    assert!(retry.actions[0].prompt.contains("find evidence"));
    complete_ok(&mut rt, &child, "verified research result");
    assert!(
        parent_input
            .try_recv()
            .unwrap()
            .contains("verified research result")
    );
}

#[test]
fn retry_of_parent_after_child_handback_preserves_the_result() {
    let mut rt = runtime();
    let parent = MemberId::new("builder");
    let child = MemberId::new("researcher");
    let started = rt.on_ui_command(user("research this"));
    let parent_input = started.actions[0].steer.as_ref().unwrap();
    rt.on_agent_event(
        &parent,
        AgentEvent::MessageCompleted(
            r#"@@spawn {"name":"researcher","task":"find evidence"}"#.into(),
        ),
    );
    complete_ok(&mut rt, &child, "verified research result");
    assert!(
        parent_input
            .try_recv()
            .unwrap()
            .contains("verified research result")
    );
    rt.on_agent_event(&parent, AgentEvent::SteerDelivered);
    fail(&mut rt, &parent);
    let retry = rt.on_ui_command(UiCommand::Retry);
    assert_eq!(retry.actions[0].member, parent);
    assert!(retry.actions[0].prompt.contains("verified research result"));
    assert!(retry.actions[0].prompt.contains("research this"));
}

#[test]
fn resume_discards_failed_input_from_the_previous_chat() {
    let mut rt = runtime();
    let original = rt.store.active_conversation();
    rt.on_ui_command(UiCommand::NewSession { prompt: None });
    rt.on_ui_command(user("failed request in another chat"));
    fail(&mut rt, &MemberId::new("builder"));
    rt.on_ui_command(UiCommand::ResumeConversation {
        conversation: original,
    });
    let retry = rt.on_ui_command(UiCommand::Retry);
    assert!(retry.actions.is_empty());
    assert!(
        retry
            .events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::Notice(text) if text == "nothing to retry"))
    );
}
