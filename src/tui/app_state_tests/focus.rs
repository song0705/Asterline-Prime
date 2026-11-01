use super::*;

pub(super) fn summary(
    id: &str,
    name: &str,
    backend: BackendKind,
    status: MemberStatus,
) -> MemberSummary {
    MemberSummary {
        id: MemberId::new(id),
        display_name: name.to_string(),
        backend,
        role: "role".to_string(),
        status,
        session: None,
        cwd: String::new(),
        model: None,
        effort: None,
        sandbox: SandboxPolicy::ReadOnly,
        permission_mode: Some(PermissionMode::Default),
        approvals_reviewer: crate::domain::team::CodexApprovalsReviewer::User,
        session_policy: SessionPolicy::Resume,
        parent: None,
    }
}

pub(super) fn ready_with(
    default: Option<DefaultTarget>,
    members: Vec<MemberSummary>,
) -> RuntimeEvent {
    RuntimeEvent::Ready {
        modes: Default::default(),
        mode_overrides: Default::default(),
        suggested_verify: None,
        team: "mixed".to_string(),
        workspace: "/tmp/ws".to_string(),
        default_target: default,
        runs: Vec::new(),
        members,
    }
}

pub(super) fn two_members() -> Vec<MemberSummary> {
    vec![
        summary("builder", "Builder", BackendKind::Codex, MemberStatus::Idle),
        summary(
            "reviewer",
            "Reviewer",
            BackendKind::Claude,
            MemberStatus::Idle,
        ),
    ]
}

#[test]
fn default_focus_follows_the_default_target_member() {
    let mut state = AppState::new(Vec::new());
    state.apply(ready_with(
        Some(DefaultTarget::Member(MemberId::new("reviewer"))),
        two_members(),
    ));

    assert_eq!(state.focus(), Some(&MemberId::new("reviewer")));
    assert_eq!(state.focus_display(), "Reviewer");
}

#[test]
fn default_focus_falls_back_to_the_first_member() {
    for default in [
        None,
        Some(DefaultTarget::All),
        Some(DefaultTarget::Member(MemberId::new("ghost"))),
    ] {
        let mut state = AppState::new(Vec::new());
        state.apply(ready_with(default, two_members()));

        assert_eq!(state.focus(), Some(&MemberId::new("builder")));
    }
}

#[test]
fn empty_roster_has_no_focus_until_members_arrive() {
    let mut state = AppState::new(Vec::new());
    state.apply(ready_with(None, Vec::new()));
    assert_eq!(state.focus(), None);

    state.apply(ready_with(None, two_members()));
    assert_eq!(state.focus(), Some(&MemberId::new("builder")));
}

#[test]
fn focus_resets_when_the_focused_member_leaves_the_roster() {
    let mut state = AppState::new(Vec::new());
    state.apply(ready_with(None, two_members()));
    state.set_focus(Some(MemberId::new("reviewer")));
    assert_eq!(state.focus(), Some(&MemberId::new("reviewer")));

    state.apply(ready_with(
        None,
        vec![summary(
            "builder",
            "Builder",
            BackendKind::Codex,
            MemberStatus::Idle,
        )],
    ));

    assert_eq!(state.focus(), Some(&MemberId::new("builder")));
}

#[test]
fn explicit_all_survives_a_repeated_ready() {
    let mut state = AppState::new(Vec::new());
    state.apply(ready_with(None, two_members()));
    state.set_focus(None);
    assert_eq!(state.focus(), None);

    state.apply(ready_with(None, two_members()));

    assert_eq!(state.focus(), None);
}

#[test]
fn explicit_member_focus_survives_a_repeated_ready() {
    let mut state = AppState::new(Vec::new());
    state.apply(ready_with(None, two_members()));
    state.set_focus(Some(MemberId::new("reviewer")));

    state.apply(ready_with(None, two_members()));

    assert_eq!(state.focus(), Some(&MemberId::new("reviewer")));
}

#[test]
fn set_focus_ignores_unknown_members_and_resets_the_view() {
    let mut state = AppState::new(Vec::new());
    state.apply(ready_with(None, two_members()));
    state.scroll_by(7);
    state.begin_chat_selection((1, 1));

    state.set_focus(Some(MemberId::new("ghost")));
    assert_eq!(state.focus(), Some(&MemberId::new("builder")));
    assert_eq!(state.scroll(), 7);

    state.set_focus(Some(MemberId::new("reviewer")));
    assert_eq!(state.scroll(), 0);
    assert_eq!(state.chat_selection(), None);
}

pub(super) fn child_summary(
    id: &str,
    name: &str,
    parent: &str,
    status: MemberStatus,
) -> MemberSummary {
    MemberSummary {
        parent: Some(MemberId::new(parent)),
        ..summary(id, name, BackendKind::Claude, status)
    }
}

fn tree_roster() -> Vec<MemberSummary> {
    vec![
        summary("main", "Main", BackendKind::Codex, MemberStatus::Idle),
        // Listed before its siblings' parent on purpose: the tree groups by
        // parent, not by roster position.
        child_summary("kid-c", "Kid C", "other", MemberStatus::Idle),
        child_summary("kid-a", "Kid A", "main", MemberStatus::Running),
        summary("other", "Other", BackendKind::Claude, MemberStatus::Failed),
        child_summary("kid-b", "Kid B", "main", MemberStatus::NeedsApproval),
    ]
}

#[test]
fn agents_list_groups_sub_agents_under_their_parent_and_preselects_the_focus() {
    let mut state = AppState::new(Vec::new());
    state.apply(ready_with(None, tree_roster()));
    state.set_focus(Some(MemberId::new("kid-b")));

    state.toggle_drawer(Drawer::Agents);

    let order: Vec<String> = state
        .agents_choices()
        .into_iter()
        .map(|choice| match choice {
            AgentChoice::All => "all".to_string(),
            AgentChoice::Member(id) => id.to_string(),
            AgentChoice::Saved(id) => format!("saved-{id}"),
        })
        .collect();
    assert_eq!(order, ["all", "main", "kid-a", "kid-b", "other", "kid-c"]);
    assert_eq!(
        state.agents_rows(),
        vec![
            AgentRow::All,
            AgentRow::Member { index: 0, depth: 0 },
            AgentRow::Member { index: 2, depth: 1 },
            AgentRow::Member { index: 4, depth: 1 },
            AgentRow::Member { index: 3, depth: 0 },
            AgentRow::Member { index: 1, depth: 1 },
        ]
    );
    assert_eq!(
        state.selected_agent(),
        Some(AgentChoice::Member(MemberId::new("kid-b")))
    );
    assert_eq!(state.sub_agent_count(), 3);
}

#[test]
fn agents_selection_is_clamped_and_applies_the_focus() {
    let mut state = AppState::new(Vec::new());
    state.apply(ready_with(None, two_members()));
    state.toggle_drawer(Drawer::Agents);
    assert_eq!(state.agents_selected(), 1);

    state.select_previous_agent();
    state.select_previous_agent();
    assert_eq!(state.selected_agent(), Some(AgentChoice::All));
    state.select_next_agent();
    state.select_next_agent();
    state.select_next_agent();
    assert_eq!(
        state.selected_agent(),
        Some(AgentChoice::Member(MemberId::new("reviewer")))
    );

    assert!(state.apply_selected_agent());
    assert_eq!(state.focus(), Some(&MemberId::new("reviewer")));
    assert_eq!(state.drawer(), None);
    assert!(!state.apply_selected_agent());
}

#[test]
fn applying_the_all_row_switches_to_the_interleaved_view() {
    let mut state = AppState::new(Vec::new());
    state.apply(ready_with(None, two_members()));
    state.toggle_drawer(Drawer::Agents);
    state.select_previous_agent();

    assert!(state.apply_selected_agent());

    assert_eq!(state.focus(), None);
}

#[test]
fn focused_message_targets_the_focused_member() {
    let mut state = AppState::new(Vec::new());
    state.apply(ready_with(None, two_members()));
    state.set_focus(Some(MemberId::new("reviewer")));

    assert_eq!(
        state.focused_message_target(),
        Some(MessageTarget::Member(MemberId::new("reviewer")))
    );

    state.set_focus(None);
    assert_eq!(state.focused_message_target(), None);
}

#[test]
fn displayed_user_body_drops_only_the_rows_own_routing_prefix() {
    let mut state = AppState::new(Vec::new());
    state.apply(ready_with(None, two_members()));
    let reviewer = vec![MemberId::new("reviewer")];

    let shown = |body: &str, targets: &[MemberId]| state.displayed_user_body(body, targets);
    assert_eq!(shown("@reviewer look at this", &reviewer), "look at this");
    assert_eq!(shown("@Reviewer look at this", &reviewer), "look at this");
    assert_eq!(shown("@all look at this", &reviewer), "look at this");
    // A prefix naming someone else is real text.
    assert_eq!(
        shown("@builder look at this", &reviewer),
        "@builder look at this"
    );
    assert_eq!(shown("@reviewer", &reviewer), "@reviewer");
    assert_eq!(shown("plain text", &reviewer), "plain text");
    assert_eq!(shown("@reviewer x", &[]), "@reviewer x");
}

#[test]
fn find_searches_the_displayed_prompt_text() {
    let mut state = AppState::new(vec![ChatItem::User {
        body: "@reviewer check the parser".to_string(),
        targets: vec![MemberId::new("reviewer")],
        interrupted: Vec::new(),
    }]);
    state.apply(ready_with(None, two_members()));
    state.set_focus(None);

    state.set_find("reviewer check");
    assert_eq!(state.find().map(|(_, _, total)| total), Some(0));
    state.set_find("check the parser");
    assert_eq!(state.find().map(|(_, _, total)| total), Some(1));
}
fn three_top_level() -> Vec<MemberSummary> {
    vec![
        summary("builder", "Builder", BackendKind::Codex, MemberStatus::Idle),
        summary(
            "reviewer",
            "Reviewer",
            BackendKind::Claude,
            MemberStatus::Idle,
        ),
        summary("planner", "Planner", BackendKind::Grok, MemberStatus::Idle),
    ]
}

/// A sub-agent of `builder` joins the roster, which pushes `reviewer` down.
fn with_builders_sub_agent() -> RuntimeEvent {
    let mut members = three_top_level();
    members.push(child_summary(
        "kid",
        "Kid",
        "builder",
        MemberStatus::Running,
    ));
    ready_with(None, members)
}

#[test]
fn agents_selection_follows_the_member_when_rows_shift() {
    let mut state = AppState::new(Vec::new());
    state.apply(ready_with(None, three_top_level()));
    state.toggle_drawer(Drawer::Agents);
    state.select_next_agent();
    assert_eq!(state.agents_selected(), 2);
    assert_eq!(
        state.selected_agent(),
        Some(AgentChoice::Member(MemberId::new("reviewer")))
    );

    // The new sub-agent appears under Builder, above Reviewer.
    state.apply(with_builders_sub_agent());

    assert_eq!(state.agents_selected(), 3);
    assert!(state.apply_selected_agent());
    assert_eq!(state.focus(), Some(&MemberId::new("reviewer")));
}

#[test]
fn agents_selection_steps_from_the_members_new_position() {
    let mut state = AppState::new(Vec::new());
    state.apply(ready_with(None, three_top_level()));
    state.toggle_drawer(Drawer::Agents);
    state.select_next_agent();
    state.apply(with_builders_sub_agent());

    state.select_previous_agent();

    assert_eq!(
        state.selected_agent(),
        Some(AgentChoice::Member(MemberId::new("kid")))
    );
}

#[test]
fn agents_selection_falls_back_when_the_member_leaves() {
    let mut state = AppState::new(Vec::new());
    state.apply(ready_with(None, two_members()));
    state.toggle_drawer(Drawer::Agents);
    state.select_next_agent();
    assert_eq!(state.agents_selected(), 2);

    state.apply(ready_with(
        None,
        vec![summary(
            "builder",
            "Builder",
            BackendKind::Codex,
            MemberStatus::Idle,
        )],
    ));

    assert_eq!(state.agents_selected(), 1);
    assert_eq!(
        state.selected_agent(),
        Some(AgentChoice::Member(MemberId::new("builder")))
    );
}

#[test]
fn find_only_matches_rows_the_focus_renders() {
    let chat = vec![
        ChatItem::Agent {
            member: MemberId::new("builder"),
            display_name: "Builder".to_string(),
            backend: BackendKind::Codex,
            text: "builder text".to_string(),
        },
        ChatItem::Agent {
            member: MemberId::new("reviewer"),
            display_name: "Reviewer".to_string(),
            backend: BackendKind::Claude,
            text: "unique-needle in review".to_string(),
        },
        ChatItem::User {
            body: "hidden-prompt-needle for reviewer".to_string(),
            targets: vec![MemberId::new("reviewer")],
            interrupted: Vec::new(),
        },
    ];
    let mut state = AppState::new(chat);
    state.apply(ready_with(None, two_members()));
    assert_eq!(state.focus(), Some(&MemberId::new("builder")));

    state.set_find("unique-needle");
    assert_eq!(state.find().map(|(_, _, total)| total), Some(0));
    assert_eq!(state.find_current_chat_index(), None);

    // A prompt sent to someone else is not on screen, so it never matches.
    state.set_find("hidden-prompt-needle");
    assert_eq!(state.find().map(|(_, _, total)| total), Some(0));

    state.set_find("unique-needle");
    state.set_focus(None);
    assert_eq!(state.find().map(|(_, _, total)| total), Some(1));
    assert_eq!(state.find_current_chat_index(), Some(1));
    state.set_find("hidden-prompt-needle");
    assert_eq!(state.find().map(|(_, _, total)| total), Some(1));
    assert_eq!(state.find_current_chat_index(), Some(2));

    state.set_focus(Some(MemberId::new("builder")));
    assert_eq!(state.find().map(|(_, _, total)| total), Some(0));
}

#[test]
fn find_scroll_ignores_hidden_rows() {
    let reviewer_reply = |text: &str| ChatItem::Agent {
        member: MemberId::new("reviewer"),
        display_name: "Reviewer".to_string(),
        backend: BackendKind::Claude,
        text: text.to_string(),
    };
    let chat = vec![
        ChatItem::Agent {
            member: MemberId::new("builder"),
            display_name: "Builder".to_string(),
            backend: BackendKind::Codex,
            text: "needle".to_string(),
        },
        reviewer_reply("one\ntwo\nthree\nfour\nfive\nsix"),
        reviewer_reply("one\ntwo\nthree\nfour\nfive\nsix"),
    ];
    let mut state = AppState::new(chat);
    state.apply(ready_with(None, two_members()));

    state.set_find("needle");
    assert_eq!(state.scroll(), 0);

    state.set_focus(None);
    state.set_find("needle");
    assert!(state.scroll() > 0);
}

#[test]
fn agents_list_orders_running_groups_before_idle_ones() {
    let mut state = AppState::new(Vec::new());
    state.apply(ready_with(
        None,
        vec![
            summary("idle-a", "Idle A", BackendKind::Codex, MemberStatus::Idle),
            summary("busy", "Busy", BackendKind::Claude, MemberStatus::Idle),
            child_summary("busy-kid", "Busy Kid", "busy", MemberStatus::Running),
            summary("idle-b", "Idle B", BackendKind::Grok, MemberStatus::Idle),
        ],
    ));
    state.toggle_drawer(Drawer::Agents);

    let rows: Vec<_> = state
        .agents_rows()
        .into_iter()
        .map(|row| match row {
            AgentRow::All => "all".to_string(),
            AgentRow::Member { index, depth } => {
                format!("{}:{depth}", state.members()[index].id)
            }
            AgentRow::Saved { index } => format!("saved:{index}"),
        })
        .collect();

    // A group is running while any of its members is: the idle parent follows
    // its running sub-agent's section, and the sub-agent stays under it.
    assert_eq!(
        rows,
        ["all", "busy:0", "busy-kid:1", "idle-a:0", "idle-b:0"]
    );
}

#[test]
fn an_agents_query_filters_ranks_and_resets_the_selection() {
    let mut state = AppState::new(Vec::new());
    state.apply(ready_with(None, two_members()));
    state.set_focus(Some(MemberId::new("reviewer")));
    state.toggle_drawer(Drawer::Agents);
    assert_eq!(
        state.selected_agent(),
        Some(AgentChoice::Member(MemberId::new("reviewer")))
    );

    // Substring of the name, case-insensitive; the list is flat and the best
    // match is highlighted.
    state.agents_query_paste("BUILD");
    assert_eq!(
        state.agents_choices(),
        vec![AgentChoice::Member(MemberId::new("builder"))]
    );
    assert_eq!(
        state.selected_agent(),
        Some(AgentChoice::Member(MemberId::new("builder")))
    );
    // Backend names match too; every word must match.
    state.agents_query_clear();
    state.agents_query_paste("claude rev");
    assert_eq!(
        state.agents_choices(),
        vec![AgentChoice::Member(MemberId::new("reviewer"))]
    );
    state.agents_query_clear();
    state.agents_query_paste("claude build");
    assert!(state.agents_choices().is_empty());
    assert_eq!(state.selected_agent(), None);
    // Nothing highlighted: Enter keeps the list open.
    assert!(state.apply_selected_agent());
    assert_eq!(state.drawer(), Some(Drawer::Agents));

    // Emptying the query returns to the viewed agent; closing resets it.
    state.agents_query_clear();
    assert_eq!(
        state.selected_agent(),
        Some(AgentChoice::Member(MemberId::new("reviewer")))
    );
    state.agents_query_push('x');
    state.close_drawer();
    state.toggle_drawer(Drawer::Agents);
    assert_eq!(state.agents_query(), "");
}

#[test]
fn deleting_a_word_from_the_agents_query_keeps_the_earlier_words() {
    let mut state = AppState::new(Vec::new());
    state.apply(ready_with(None, two_members()));
    state.toggle_drawer(Drawer::Agents);
    state.agents_query_paste("claude  rev");

    state.agents_query_delete_word();
    assert_eq!(state.agents_query(), "claude  ");
    state.agents_query_delete_word();
    assert_eq!(state.agents_query(), "");
}

#[test]
fn agents_selection_jumps_to_the_ends_and_by_pages() {
    let mut state = AppState::new(Vec::new());
    state.apply(ready_with(None, three_top_level()));
    state.toggle_drawer(Drawer::Agents);
    state.note_drawer_viewport(3);

    state.select_last_agent();
    assert_eq!(state.agents_selected(), 3);
    state.select_agent_page(false);
    assert_eq!(state.agents_selected(), 1);
    state.select_agent_page(true);
    state.select_agent_page(true);
    assert_eq!(state.agents_selected(), 3);
    state.select_first_agent();
    assert_eq!(state.agents_selected(), 0);
}

#[test]
fn member_notices_follow_their_member_and_general_ones_show_everywhere() {
    let mut state = AppState::new(Vec::new());
    state.apply(ready_with(None, two_members()));
    state.apply(RuntimeEvent::MemberNotice {
        member: MemberId::new("reviewer"),
        text: "Reviewer could not spawn scout: limit".to_string(),
    });
    state.apply(RuntimeEvent::Notice("mode: plan".to_string()));
    let notice = |state: &AppState, index: usize| focus_row(state, &state.chat()[index]);

    state.set_focus(Some(MemberId::new("builder")));
    assert_eq!(notice(&state, 0), FocusRow::Hidden);
    assert_eq!(notice(&state, 1), FocusRow::Full);
    state.set_focus(Some(MemberId::new("reviewer")));
    assert_eq!(notice(&state, 0), FocusRow::Full);
    assert_eq!(notice(&state, 1), FocusRow::Full);
    state.set_focus(None);
    assert_eq!(notice(&state, 0), FocusRow::Full);
    assert_eq!(notice(&state, 1), FocusRow::Full);
}

#[test]
fn find_skips_member_notices_of_other_agents() {
    let mut state = AppState::new(Vec::new());
    state.apply(ready_with(None, two_members()));
    state.apply(RuntimeEvent::MemberNotice {
        member: MemberId::new("reviewer"),
        text: "notice-needle".to_string(),
    });

    state.set_focus(Some(MemberId::new("builder")));
    state.set_find("notice-needle");
    assert_eq!(state.find().map(|(_, _, total)| total), Some(0));

    state.set_focus(Some(MemberId::new("reviewer")));
    state.set_find("notice-needle");
    assert_eq!(state.find().map(|(_, _, total)| total), Some(1));
}

#[test]
fn identical_notices_for_different_members_are_both_kept() {
    let mut state = AppState::new(Vec::new());
    state.apply(ready_with(None, two_members()));
    for member in ["builder", "reviewer", "reviewer"] {
        state.apply(RuntimeEvent::MemberNotice {
            member: MemberId::new(member),
            text: "same text".to_string(),
        });
    }

    // The repeat for the same member collapses; the other member's stays.
    assert_eq!(state.chat().len(), 2);
}

#[test]
fn hidden_prompts_take_no_room_when_scrolling_to_a_match() {
    let reply = |text: &str| ChatItem::Agent {
        member: MemberId::new("builder"),
        display_name: "Builder".to_string(),
        backend: BackendKind::Codex,
        text: text.to_string(),
    };
    let prompt = ChatItem::User {
        body: "p1\np2\np3\np4\np5\np6".to_string(),
        targets: vec![MemberId::new("reviewer")],
        interrupted: Vec::new(),
    };
    let mut state = AppState::new(vec![reply("needle"), prompt, reply("tail")]);
    state.apply(ready_with(None, two_members()));

    state.set_find("needle");
    let focused = state.scroll();
    state.set_focus(None);
    state.set_find("needle");

    assert!(state.scroll() > focused, "{} vs {focused}", state.scroll());
}
