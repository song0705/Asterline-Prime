//! Per-group tool expansion: clicks open one group, Ctrl+O opens all of them,
//! new and resumed chats start folded, and `/find` opens the group it lands in.

use super::*;

fn call(member: &str, summary: &str) -> ChatItem {
    ChatItem::Tool {
        member: MemberId::new(member),
        name: "Read".to_string(),
        summary: summary.to_string(),
        detail: String::new(),
        ok: Some(true),
    }
}

fn reply(text: &str) -> ChatItem {
    ChatItem::Agent {
        member: MemberId::new("builder"),
        display_name: "Builder".to_string(),
        backend: BackendKind::Codex,
        text: text.to_string(),
    }
}

/// Two five-call groups (first items at 0 and 6) split by a reply.
fn two_groups() -> AppState {
    let mut chat: Vec<ChatItem> = (0..5)
        .map(|n| call("builder", &format!("a{n}.rs")))
        .collect();
    chat.push(reply("between the groups"));
    chat.extend((0..5).map(|n| call("builder", &format!("b{n}.rs"))));
    AppState::new(chat)
}

#[test]
fn groups_start_folded_and_toggle_independently() {
    let mut state = two_groups();
    assert!(!state.tool_group_expanded(0));
    assert!(!state.tool_group_expanded(6));

    state.toggle_tool_group(6);
    assert!(!state.tool_group_expanded(0));
    assert!(state.tool_group_expanded(6));

    state.toggle_tool_group(6);
    assert!(!state.tool_group_expanded(6));
}

#[test]
fn each_toggle_changes_the_paint_revision() {
    let mut state = two_groups();
    let before = state.tool_groups_revision();
    state.toggle_tool_group(0);
    let opened = state.tool_groups_revision();
    state.toggle_tool_group(0);
    assert_ne!(before, opened);
    assert_ne!(opened, state.tool_groups_revision());
}

#[test]
fn ctrl_o_opens_every_group_and_turning_it_off_restores_group_state() {
    let mut state = two_groups();
    state.toggle_tool_group(0);

    state.toggle_tools_expansion();
    assert!(state.tool_group_expanded(0));
    assert!(state.tool_group_expanded(6));

    state.toggle_tools_expansion();
    assert!(state.tool_group_expanded(0));
    assert!(!state.tool_group_expanded(6));
}

#[test]
fn a_new_chat_forgets_which_groups_were_open() {
    let mut state = two_groups();
    state.toggle_tool_group(0);
    state.toggle_tool_group(6);
    let before = state.tool_groups_revision();

    state.apply(RuntimeEvent::SessionReset);

    assert!(!state.tool_group_expanded(0));
    assert!(!state.tool_group_expanded(6));
    assert_ne!(state.tool_groups_revision(), before);
}

#[test]
fn a_resumed_chat_forgets_which_groups_were_open() {
    let mut state = two_groups();
    state.toggle_tool_group(0);

    state.apply(RuntimeEvent::ConversationResumed {
        conversation: 2,
        chat: vec![call("builder", "old.rs")],
    });

    assert!(!state.tool_group_expanded(0));
}

#[test]
fn find_opens_the_folded_group_that_holds_the_match() {
    let mut state = two_groups();
    state.set_find("a2.rs");

    assert_eq!(state.find().map(|(_, _, total)| total), Some(1));
    assert!(state.tool_group_expanded(0), "the matching group opens");
    assert!(!state.tool_group_expanded(6), "other groups stay folded");
}

#[test]
fn find_leaves_short_groups_and_other_items_alone() {
    let mut state = AppState::new(vec![
        call("builder", "x.rs"),
        call("builder", "y.rs"),
        reply("needle"),
    ]);
    state.set_find("x.rs");
    assert!(!state.tool_group_expanded(0));
    state.set_find("needle");
    assert!(!state.tool_group_expanded(0));
}

#[test]
fn find_scroll_counts_a_folded_group_below_as_a_single_row() {
    // The match sits above a 40-call group. Folded, that group is one title
    // row plus its separator; it must not push the match off screen.
    let mut chat = vec![reply("needle here")];
    chat.extend((0..40).map(|n| call("builder", &format!("f{n}.rs"))));
    let mut state = AppState::new(chat);

    state.set_find("needle");
    let folded_scroll = state.scroll();

    state.toggle_tool_group(1);
    state.set_find("needle");
    assert!(
        folded_scroll < state.scroll(),
        "folded {folded_scroll} vs open {}",
        state.scroll()
    );
    assert_eq!(folded_scroll, 2);
}
