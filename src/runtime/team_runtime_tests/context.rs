//! Carrying a conversation across a model or CLI switch: per-session
//! watermarks, the recap in front of the next prompt, and what the user sees.

use super::*;
use crate::runtime::context_recap::{
    NOT_CARRIED_OVER, PROTOCOL_NOTE_HEADER, RECAP_FOOTER, RECAP_HEADER, RECAP_MAX_CHARS,
    without_protocol_note,
};

/// A runtime whose members carry the team instructions, as in the app.
fn runtime() -> TeamRuntime {
    let mut rt = super::runtime();
    crate::domain::config::inject_team_protocol_with(&mut rt.config, &rt.models);
    rt
}

fn builder() -> MemberId {
    MemberId::new("builder")
}

/// The user writes to `builder`; the native session `session` answers `reply`.
/// Returns the prompt that was sent to the CLI.
fn exchange(rt: &mut TeamRuntime, text: &str, session: &str, reply: &str) -> String {
    let step = rt.on_ui_command(user(text));
    let action = &step.actions[0];
    assert_eq!(action.member, builder());
    let prompt = action.prompt.clone();
    rt.on_agent_event(
        &builder(),
        AgentEvent::SessionDiscovered(AgentSessionId(session.to_string())),
    );
    complete_ok(rt, &builder(), reply);
    prompt
}

fn roster_with(rt: &TeamRuntime, edit: impl FnOnce(&mut TeamMember)) -> Vec<TeamMember> {
    let mut members = strip_team_protocols(rt.config.clone()).members;
    edit(members.iter_mut().find(|m| m.id == builder()).unwrap());
    members
}

fn replace(rt: &mut TeamRuntime, members: Vec<TeamMember>) -> RuntimeStep {
    rt.on_ui_command(UiCommand::ReplaceTeam {
        members,
        default_target: Some(DefaultTarget::Member(builder())),
    })
}

/// What `/model` sends for a pick on another CLI: new backend, no session.
fn switch_cli(rt: &mut TeamRuntime, backend: BackendKind) -> RuntimeStep {
    let members = roster_with(rt, |member| {
        member.backend = backend;
        member.session_id = None;
        member.model = None;
    });
    replace(rt, members)
}

fn latest(rt: &TeamRuntime) -> i64 {
    rt.store.latest_message_id().unwrap()
}

fn stored(rt: &TeamRuntime, parked: bool) -> Vec<(BackendKind, String, Option<i64>)> {
    rt.store
        .stored_sessions()
        .unwrap()
        .into_iter()
        .filter(|saved| saved.member == builder() && saved.parked == parked)
        .map(|saved| (saved.backend, saved.session_id, saved.seen_through))
        .collect()
}

#[test]
fn a_cli_switch_prepends_a_recap_of_the_conversation() {
    let mut rt = runtime();
    exchange(&mut rt, "add a parser", "codex-1", "Parser added.");

    switch_cli(&mut rt, BackendKind::Claude);
    let step = rt.on_ui_command(user("what did we talk about"));
    let action = &step.actions[0];

    assert_eq!(action.session, None, "the new CLI starts a fresh session");
    let prompt = &action.prompt;
    assert!(prompt.starts_with(RECAP_HEADER), "{prompt}");
    assert!(prompt.contains("User: add a parser"), "{prompt}");
    assert!(
        prompt.contains("Assistant (Builder): Parser added."),
        "{prompt}"
    );
    assert!(
        prompt.ends_with(&format!("{RECAP_FOOTER}\n\nwhat did we talk about")),
        "{prompt}"
    );
    // The new message is the prompt itself, not part of the recap.
    assert_eq!(prompt.matches("what did we talk about").count(), 1);
}

#[test]
fn a_model_switch_on_the_same_cli_adds_no_recap() {
    let mut rt = runtime();
    exchange(&mut rt, "first", "codex-1", "one");

    let members = roster_with(&rt, |member| {
        member.model = Some("gpt-6.1-sol".to_string());
        member.effort = Some(Effort::High);
        member.session_id = Some("codex-1".to_string());
    });
    let step = replace(&mut rt, members);
    assert!(step.events.iter().any(|event| matches!(
        event,
        RuntimeEvent::Notice(text) if text == "team updated: 2 member(s)"
    )));
    let step = rt.on_ui_command(user("second"));

    assert_eq!(step.actions[0].prompt, "second");
    assert_eq!(
        step.actions[0].session,
        Some(AgentSessionId("codex-1".to_string()))
    );
}

#[test]
fn switching_back_recaps_only_what_the_old_session_missed() {
    let mut rt = runtime();
    exchange(&mut rt, "q-one", "codex-1", "a-one");
    switch_cli(&mut rt, BackendKind::Claude);
    let on_claude = exchange(&mut rt, "q-two", "claude-1", "a-two");
    assert!(on_claude.contains("q-one") && on_claude.contains("a-one"));

    switch_cli(&mut rt, BackendKind::Codex);
    let step = rt.on_ui_command(user("q-three"));
    let action = &step.actions[0];

    // The first codex session is resumed: it already knows q-one and a-one.
    assert_eq!(action.session, Some(AgentSessionId("codex-1".to_string())));
    // That thread may predate the current instructions (Claude ran in
    // between), so it gets them once in front of the recap.
    assert!(action.prompt.starts_with(PROTOCOL_NOTE_HEADER));
    let prompt = without_protocol_note(&action.prompt);
    assert!(prompt.starts_with(RECAP_HEADER), "{prompt}");
    assert!(prompt.contains("User: q-two"), "{prompt}");
    assert!(prompt.contains("Assistant (Builder): a-two"), "{prompt}");
    assert!(!prompt.contains("q-one"), "{prompt}");
    assert!(!prompt.contains("a-one"), "{prompt}");
    assert!(prompt.ends_with("q-three"));
}

#[test]
fn an_up_to_date_session_gets_the_message_alone_after_a_switch_round_trip() {
    let mut rt = runtime();
    exchange(&mut rt, "q-one", "codex-1", "a-one");
    // Away and straight back, with nothing said in between.
    switch_cli(&mut rt, BackendKind::Claude);
    switch_cli(&mut rt, BackendKind::Codex);

    let step = rt.on_ui_command(user("q-two"));

    assert_eq!(step.actions[0].prompt, "q-two");
    assert_eq!(
        step.actions[0].session,
        Some(AgentSessionId("codex-1".to_string()))
    );
}

#[test]
fn the_watermark_advances_after_a_turn_and_is_saved_with_the_session() {
    let mut rt = runtime();
    assert_eq!(rt.sessions.seen(&builder()), 0);
    exchange(&mut rt, "one", "codex-1", "reply one");
    let after_first = latest(&rt);
    assert!(after_first > 0);
    assert_eq!(rt.sessions.seen(&builder()), after_first);
    assert_eq!(
        stored(&rt, false),
        vec![(BackendKind::Codex, "codex-1".to_string(), Some(after_first))]
    );

    // A second turn in the same session moves it forward again.
    exchange(&mut rt, "two", "codex-1", "reply two");
    assert_eq!(rt.sessions.seen(&builder()), latest(&rt));
    assert!(latest(&rt) > after_first);

    // After a switch the old session is parked with the watermark it had.
    let before_switch = latest(&rt);
    switch_cli(&mut rt, BackendKind::Claude);
    assert_eq!(rt.sessions.seen(&builder()), 0);
    assert_eq!(
        stored(&rt, true),
        vec![(
            BackendKind::Codex,
            "codex-1".to_string(),
            Some(before_switch)
        )]
    );
    assert!(stored(&rt, false).is_empty());
    // The conversation snapshot carries both, so /resume gets them back.
    let snapshot = rt
        .store
        .conversation_snapshot(rt.store.active_conversation())
        .unwrap()
        .unwrap();
    assert!(snapshot.sessions.iter().any(|saved| saved.parked
        && saved.backend == BackendKind::Codex
        && saved.seen_through == Some(before_switch)));
}

#[test]
fn watermarks_survive_a_restart() {
    let path = std::env::temp_dir().join(format!(
        "asterline-watermark-restart-{}.sqlite3",
        std::process::id()
    ));
    remove_sqlite_test_files(&path);
    let mut rt = TeamRuntime::new(team(), SqliteStore::open(&path).unwrap()).with_approvals(false);
    exchange(&mut rt, "q-one", "codex-1", "a-one");
    switch_cli(&mut rt, BackendKind::Claude);
    exchange(&mut rt, "q-two", "claude-1", "a-two");
    let config = strip_team_protocols(rt.config.clone());
    let claude_seen = rt.sessions.seen(&builder());
    assert_eq!(claude_seen, latest(&rt));
    drop(rt);

    let mut rt = TeamRuntime::new(config, SqliteStore::open(&path).unwrap()).with_approvals(false);
    assert_eq!(rt.sessions.seen(&builder()), claude_seen);
    assert_eq!(
        rt.sessions.get(&builder()),
        Some(AgentSessionId("claude-1".to_string()))
    );
    // Up to date: the next message goes out as typed.
    let step = rt.on_ui_command(user("q-three"));
    assert_eq!(step.actions[0].prompt, "q-three");
    complete_ok(&mut rt, &builder(), "a-three");
    // The parked codex session came back from disk too.
    switch_cli(&mut rt, BackendKind::Codex);
    let step = rt.on_ui_command(user("q-four"));
    assert_eq!(
        step.actions[0].session,
        Some(AgentSessionId("codex-1".to_string()))
    );
    assert!(!step.actions[0].prompt.contains("q-one"));
    assert!(step.actions[0].prompt.contains("q-three"));
    drop(rt);
    remove_sqlite_test_files(&path);
}

#[test]
fn sessions_saved_before_watermarks_existed_count_as_up_to_date() {
    let path = std::env::temp_dir().join(format!(
        "asterline-watermark-legacy-{}.sqlite3",
        std::process::id()
    ));
    remove_sqlite_test_files(&path);
    let mut rt = TeamRuntime::new(team(), SqliteStore::open(&path).unwrap()).with_approvals(false);
    exchange(&mut rt, "q-one", "codex-1", "a-one");
    let config = strip_team_protocols(rt.config.clone());
    let newest = latest(&rt);
    drop(rt);
    // An older version stored the session without a watermark.
    Connection::open(&path)
        .unwrap()
        .execute("UPDATE agent_sessions SET seen_through = NULL", [])
        .unwrap();

    let mut rt = TeamRuntime::new(config, SqliteStore::open(&path).unwrap()).with_approvals(false);

    assert_eq!(rt.sessions.seen(&builder()), newest);
    assert_eq!(
        stored(&rt, false),
        vec![(BackendKind::Codex, "codex-1".to_string(), Some(newest))]
    );
    let step = rt.on_ui_command(user("q-two"));
    assert_eq!(step.actions[0].prompt, "q-two");
    drop(rt);
    remove_sqlite_test_files(&path);
}

#[test]
fn new_chat_clears_every_watermark_and_parked_session() {
    let mut rt = runtime();
    exchange(&mut rt, "q-one", "codex-1", "a-one");
    switch_cli(&mut rt, BackendKind::Claude);
    exchange(&mut rt, "q-two", "claude-1", "a-two");
    assert!(!stored(&rt, true).is_empty());

    rt.on_ui_command(UiCommand::NewSession { prompt: None });

    assert_eq!(rt.sessions.get(&builder()), None);
    assert_eq!(rt.sessions.seen(&builder()), 0);
    assert!(stored(&rt, false).is_empty());
    assert!(stored(&rt, true).is_empty());
    // The fresh chat starts clean: no recap of the old one.
    switch_cli(&mut rt, BackendKind::Codex);
    let step = rt.on_ui_command(user("hello again"));
    assert_eq!(step.actions[0].prompt, "hello again");
    assert_eq!(step.actions[0].session, None);
}

#[test]
fn resuming_a_chat_restores_its_watermarks() {
    let mut rt = runtime();
    let original = rt.store.active_conversation();
    exchange(&mut rt, "q-one", "codex-1", "a-one");
    let seen = rt.sessions.seen(&builder());
    rt.on_ui_command(UiCommand::NewSession { prompt: None });
    assert_eq!(rt.sessions.seen(&builder()), 0);

    rt.on_ui_command(UiCommand::ResumeConversation {
        conversation: original,
    });

    assert_eq!(rt.sessions.seen(&builder()), seen);
    let step = rt.on_ui_command(user("q-two"));
    assert_eq!(step.actions[0].prompt, "q-two");
    assert_eq!(
        step.actions[0].session,
        Some(AgentSessionId("codex-1".to_string()))
    );
}

#[test]
fn a_recap_is_never_stored_or_shown_in_the_transcript() {
    let mut rt = runtime();
    exchange(&mut rt, "q-one", "codex-1", "a-one");
    switch_cli(&mut rt, BackendKind::Claude);
    let step = rt.on_ui_command(user("q-two"));
    assert!(step.actions[0].prompt.starts_with(RECAP_HEADER));
    // What the TUI is told about the user's message is the message as typed.
    assert!(step.events.iter().any(|event| matches!(
        event,
        RuntimeEvent::UserMessage { body, .. } if body == "q-two"
    )));
    rt.on_agent_event(
        &builder(),
        AgentEvent::SessionDiscovered(AgentSessionId("claude-1".to_string())),
    );
    complete_ok(&mut rt, &builder(), "a-two");

    let chat = rt.store.replay_chat().unwrap();
    for item in &chat {
        let text = match item {
            ChatItem::User { body, .. } => body,
            ChatItem::Agent { text, .. } => text,
            ChatItem::Notice { text, .. } => text,
            _ => continue,
        };
        assert!(!text.contains(RECAP_HEADER), "{text}");
        assert!(!text.contains(RECAP_FOOTER), "{text}");
    }
    assert!(chat.iter().any(|item| matches!(
        item,
        ChatItem::User { body, .. } if body == "q-two"
    )));
}

#[test]
fn a_recap_only_covers_the_members_own_conversation() {
    let mut rt = runtime();
    // The user talks to the reviewer; the builder is not part of it.
    rt.on_ui_command(UiCommand::UserMessage {
        target: MessageTarget::Member(MemberId::new("reviewer")),
        body: "private to reviewer".to_string(),
    });
    complete_ok(&mut rt, &MemberId::new("reviewer"), "reviewer reply");
    exchange(&mut rt, "for builder", "codex-1", "builder reply");
    switch_cli(&mut rt, BackendKind::Claude);

    let step = rt.on_ui_command(user("go on"));

    let prompt = &step.actions[0].prompt;
    assert!(prompt.contains("for builder"), "{prompt}");
    assert!(!prompt.contains("private to reviewer"), "{prompt}");
    assert!(!prompt.contains("reviewer reply"), "{prompt}");
}

#[test]
fn an_oversized_history_keeps_the_newest_part_and_says_so_on_the_switch() {
    let mut rt = runtime();
    let filler = "x".repeat(5_000);
    for n in 0..30 {
        exchange(
            &mut rt,
            &format!("question {n:02} {filler}"),
            "codex-1",
            &format!("answer {n:02}"),
        );
    }

    let step = switch_cli(&mut rt, BackendKind::Claude);
    assert!(
        step.events.iter().any(|event| matches!(
            event,
            RuntimeEvent::Notice(text)
                if text == &format!("team updated: 2 member(s) · {NOT_CARRIED_OVER}")
        )),
        "{:?}",
        step.events
    );

    let step = rt.on_ui_command(user("summarize"));
    let prompt = &step.actions[0].prompt;
    assert!(prompt.chars().count() <= RECAP_MAX_CHARS + "summarize".len() + 2);
    assert!(prompt.contains("question 29"), "newest is kept");
    assert!(!prompt.contains("question 00"), "oldest is dropped");
    assert!(prompt.contains("earlier messages were omitted"));
    assert!(prompt.ends_with("summarize"));
}

#[test]
fn a_switch_that_carries_everything_over_has_no_suffix() {
    let mut rt = runtime();
    exchange(&mut rt, "short question", "codex-1", "short answer");

    let step = switch_cli(&mut rt, BackendKind::Claude);

    assert!(step.events.iter().any(|event| matches!(
        event,
        RuntimeEvent::Notice(text) if text == "team updated: 2 member(s)"
    )));
    assert!(!step.events.iter().any(|event| matches!(
        event,
        RuntimeEvent::Notice(text) if text.contains(NOT_CARRIED_OVER)
    )));
}

#[test]
fn a_failed_run_leaves_its_turn_unseen_so_the_next_prompt_still_recaps_it() {
    let mut rt = runtime();
    exchange(&mut rt, "q-one", "codex-1", "a-one");
    let seen = rt.sessions.seen(&builder());
    rt.on_ui_command(user("q-two"));
    rt.on_agent_event(
        &builder(),
        AgentEvent::Exited {
            code: Some(1),
            ok: false,
        },
    );

    assert_eq!(rt.sessions.seen(&builder()), seen);
}
