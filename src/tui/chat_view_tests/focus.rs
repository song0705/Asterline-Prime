use super::super::*;
use super::*;
use crate::domain::event::{MemberSummary, TurnId};

fn builder() -> MemberId {
    MemberId::new("builder")
}

fn reviewer() -> MemberId {
    MemberId::new("reviewer")
}

fn agent(member: &MemberId, name: &str, backend: BackendKind, text: &str) -> ChatItem {
    ChatItem::Agent {
        member: member.clone(),
        display_name: name.to_string(),
        backend,
        text: text.to_string(),
    }
}

fn user(body: &str, targets: &[MemberId]) -> ChatItem {
    ChatItem::User {
        body: body.to_string(),
        targets: targets.to_vec(),
        interrupted: Vec::new(),
    }
}

fn route(from: &MemberId, to: &[&str], body: &str) -> ChatItem {
    ChatItem::Route {
        from: from.clone(),
        to: to.iter().map(|name| name.to_string()).collect(),
        body: body.to_string(),
    }
}

fn tool(member: &MemberId, summary: &str) -> ChatItem {
    ChatItem::Tool {
        member: member.clone(),
        name: "shell".to_string(),
        summary: summary.to_string(),
        detail: "ok".to_string(),
        ok: Some(true),
    }
}

/// A two-member roster over `chat`, focused on `focus` (`None` is "All").
fn focused_state(chat: Vec<ChatItem>, focus: Option<MemberId>) -> AppState {
    focused_state_with(chat, focus, MemberStatus::Idle, MemberStatus::Idle)
}

fn focused_state_with(
    chat: Vec<ChatItem>,
    focus: Option<MemberId>,
    builder_status: MemberStatus,
    reviewer_status: MemberStatus,
) -> AppState {
    let mut state = AppState::new(chat);
    state.apply(RuntimeEvent::Ready {
        modes: Default::default(),
        mode_overrides: Default::default(),
        suggested_verify: None,
        team: "t".to_string(),
        workspace: String::new(),
        default_target: Some(DefaultTarget::Member(builder())),
        runs: Vec::new(),
        members: vec![
            member_summary(
                "builder",
                "Builder",
                BackendKind::Codex,
                "impl",
                builder_status,
            ),
            member_summary(
                "reviewer",
                "Reviewer",
                BackendKind::Claude,
                "review",
                reviewer_status,
            ),
        ],
    });
    state.set_focus(focus);
    state
}

fn history(state: &AppState, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    render_chat_history(state, width, 0, &mut lines);
    plain_text(&lines)
}

fn has(text: &[String], needle: &str) -> bool {
    text.iter().any(|line| line.contains(needle))
}

fn mixed_chat() -> Vec<ChatItem> {
    vec![
        user("task for builder", &[builder()]),
        agent(&builder(), "Builder", BackendKind::Codex, "builder reply"),
        tool(&reviewer(), "reviewer tool"),
        agent(
            &reviewer(),
            "Reviewer",
            BackendKind::Claude,
            "reviewer reply",
        ),
        ChatItem::Verdict {
            member: reviewer(),
            approve: true,
            summary: "reviewer verdict".to_string(),
        },
        ChatItem::Error {
            member: Some(reviewer()),
            message: "reviewer failure".to_string(),
        },
        ChatItem::Error {
            member: None,
            message: "global failure".to_string(),
        },
        ChatItem::Notice {
            member: None,
            text: "a notice".to_string(),
        },
        ChatItem::Notice {
            member: Some(reviewer()),
            text: "reviewer-only notice".to_string(),
        },
        user("legacy prompt", &[]),
        user("ask the reviewer\nsecond line", &[reviewer()]),
    ]
}

#[test]
fn focused_window_shows_only_that_members_rows() {
    let state = focused_state(mixed_chat(), Some(builder()));

    let text = history(&state, 80);

    assert!(has(&text, "task for builder"), "{text:?}");
    assert!(has(&text, "builder reply"), "{text:?}");
    assert!(has(&text, "global failure"), "{text:?}");
    assert!(has(&text, "a notice"), "{text:?}");
    assert!(has(&text, "legacy prompt"), "{text:?}");
    for hidden in [
        "reviewer tool",
        "reviewer reply",
        "reviewer verdict",
        "reviewer failure",
        "reviewer-only notice",
        "ask the reviewer",
    ] {
        assert!(!has(&text, hidden), "{hidden} leaked: {text:?}");
    }
}

#[test]
fn prompt_to_another_member_is_not_shown_at_all() {
    let state = focused_state(mixed_chat(), Some(builder()));

    let text = history(&state, 80);

    assert!(!has(&text, "ask the reviewer"), "{text:?}");
    assert!(!has(&text, "second line"), "{text:?}");
    assert!(!has(&text, "you →"), "{text:?}");
    // Only prompts that reached Builder keep a band (a band's text row is
    // the one padded out to the full width).
    let bands = text
        .iter()
        .filter(|line| line.starts_with("  ") && line.ends_with(' ') && !line.trim().is_empty())
        .count();
    assert_eq!(bands, 2, "{text:?}");
}

#[test]
fn each_member_sees_only_the_prompts_sent_to_it() {
    let chat = vec![
        user("for builder", &[builder()]),
        user("for reviewer", &[reviewer()]),
    ];

    let builder_view = history(&focused_state(chat.clone(), Some(builder())), 80);
    // One agent's window needs no recipient: the prompt is plainly to it.
    assert!(has(&builder_view, "  for builder"), "{builder_view:?}");
    assert!(!has(&builder_view, "to Builder"), "{builder_view:?}");
    assert!(!has(&builder_view, "for reviewer"), "{builder_view:?}");
    assert!(!has(&builder_view, "you →"), "{builder_view:?}");

    let reviewer_view = history(&focused_state(chat.clone(), Some(reviewer())), 80);
    assert!(has(&reviewer_view, "  for reviewer"), "{reviewer_view:?}");
    assert!(!has(&reviewer_view, "to Reviewer"), "{reviewer_view:?}");
    assert!(!has(&reviewer_view, "for builder"), "{reviewer_view:?}");
    assert!(!has(&reviewer_view, "you →"), "{reviewer_view:?}");

    let all = history(&focused_state(chat, None), 80);
    assert!(has(&all, "for builder"), "{all:?}");
    assert!(has(&all, "for reviewer"), "{all:?}");
}

/// The user opens a sub-agent (Reviewer) under Main (Builder), then talks to
/// it directly.
fn sub_agent_chat() -> Vec<ChatItem> {
    vec![
        user("spawn please", &[builder()]),
        ChatItem::Spawn {
            parent: builder(),
            child: reviewer(),
            summary: "opened Reviewer".to_string(),
        },
        route(&builder(), &["reviewer"], "task from the parent"),
        agent(
            &reviewer(),
            "Reviewer",
            BackendKind::Claude,
            "reviewer reply",
        ),
        route(&reviewer(), &["builder"], "hand-back to the parent"),
        user("hi reviewer, private note", &[reviewer()]),
        agent(&builder(), "Builder", BackendKind::Codex, "builder reply"),
    ]
}

#[test]
fn a_sub_agent_window_has_no_row_for_the_prompt_to_its_parent() {
    let text = history(&focused_state(sub_agent_chat(), Some(reviewer())), 80);

    assert!(!has(&text, "spawn please"), "{text:?}");
    assert!(!has(&text, "you →"), "{text:?}");
    assert!(!has(&text, "· spawn"), "{text:?}");
    assert!(!has(&text, "builder reply"), "{text:?}");
    // Its own conversation stays: the task, its reply, the hand-back and the
    // user's direct message.
    assert!(has(&text, "task from the parent"), "{text:?}");
    assert!(has(&text, "reviewer reply"), "{text:?}");
    assert!(has(&text, "hand-back to the parent"), "{text:?}");
    assert!(has(&text, "hi reviewer, private note"), "{text:?}");
    assert!(has(&text, "⇄ agents"), "{text:?}");
}

#[test]
fn the_parent_window_has_no_row_for_the_direct_message_to_its_sub_agent() {
    let text = history(&focused_state(sub_agent_chat(), Some(builder())), 80);

    assert!(!has(&text, "private note"), "{text:?}");
    assert!(!has(&text, "you →"), "{text:?}");
    assert!(!has(&text, "reviewer reply"), "{text:?}");
    assert!(has(&text, "spawn please"), "{text:?}");
    // The child exists, so the row shows its model label, not the summary.
    assert!(has(&text, "· spawn  Reviewer  claude"), "{text:?}");
    assert!(!has(&text, "opened Reviewer"), "{text:?}");
    assert!(has(&text, "task from the parent"), "{text:?}");
    assert!(has(&text, "hand-back to the parent"), "{text:?}");
    assert!(has(&text, "builder reply"), "{text:?}");
}

#[test]
fn all_messages_still_shows_every_prompt() {
    let text = history(&focused_state(sub_agent_chat(), None), 80);

    assert!(has(&text, "spawn please"), "{text:?}");
    assert!(has(&text, "hi reviewer, private note"), "{text:?}");
    assert!(has(&text, "reviewer reply"), "{text:?}");
    assert!(has(&text, "builder reply"), "{text:?}");
}

#[test]
fn a_member_notice_shows_only_in_its_window_and_in_all_messages() {
    let chat = vec![
        ChatItem::Notice {
            member: Some(reviewer()),
            text: "Model: Reviewer → claude · opus".to_string(),
        },
        ChatItem::Notice {
            member: None,
            text: "Mode: plan".to_string(),
        },
        ChatItem::Notice {
            member: None,
            text: "team updated: 2 member(s)".to_string(),
        },
    ];

    let reviewer_view = history(&focused_state(chat.clone(), Some(reviewer())), 80);
    assert!(has(&reviewer_view, "Model: Reviewer"), "{reviewer_view:?}");
    assert!(has(&reviewer_view, "Mode: plan"), "{reviewer_view:?}");

    let builder_view = history(&focused_state(chat.clone(), Some(builder())), 80);
    assert!(!has(&builder_view, "Model: Reviewer"), "{builder_view:?}");
    assert!(has(&builder_view, "Mode: plan"), "{builder_view:?}");
    assert!(has(&builder_view, "team updated"), "{builder_view:?}");

    let all = history(&focused_state(chat, None), 80);
    assert!(has(&all, "Model: Reviewer"), "{all:?}");
    assert!(has(&all, "Mode: plan"), "{all:?}");
    assert!(has(&all, "team updated"), "{all:?}");
}

#[test]
fn routes_become_sender_and_receiver_summaries() {
    let chat = vec![
        route(
            &builder(),
            &["reviewer"],
            "please review the parser\nwith care",
        ),
        route(&reviewer(), &["Builder"], "found a bug"),
        route(&reviewer(), &["planner"], "unrelated relay"),
        route(&reviewer(), &["all"], "broadcast note"),
    ];

    let builder_view = history(&focused_state(chat.clone(), Some(builder())), 80);
    assert!(
        has(&builder_view, "· to     Reviewer  please review the parser"),
        "{builder_view:?}"
    );
    assert!(
        has(&builder_view, "· from   Reviewer  found a bug"),
        "{builder_view:?}"
    );
    assert!(
        has(&builder_view, "· from   Reviewer  broadcast note"),
        "{builder_view:?}"
    );
    assert!(!has(&builder_view, "unrelated relay"), "{builder_view:?}");
    assert!(!has(&builder_view, "with care"), "{builder_view:?}");

    let reviewer_view = history(&focused_state(chat, Some(reviewer())), 80);
    assert!(
        has(&reviewer_view, "· from   Builder  please review the parser"),
        "{reviewer_view:?}"
    );
    assert!(
        has(&reviewer_view, "· to     Builder  found a bug"),
        "{reviewer_view:?}"
    );
    assert!(
        has(&reviewer_view, "· to     planner  unrelated relay"),
        "{reviewer_view:?}"
    );
    // Rows are relative to the viewed agent; the interleaved arrow is not used.
    assert!(
        !has(&reviewer_view, "Builder → Reviewer"),
        "{reviewer_view:?}"
    );
    assert!(has(&reviewer_view, "⇄ agents"), "{reviewer_view:?}");
}

#[test]
fn adjacent_agent_rows_share_one_group() {
    let state = focused_state(
        vec![
            agent(&builder(), "Builder", BackendKind::Codex, "builder reply"),
            route(&builder(), &["reviewer"], "first"),
            route(&builder(), &["reviewer"], "second"),
        ],
        Some(builder()),
    );

    let text = history(&state, 80);

    let first = text.iter().position(|line| line.contains("first")).unwrap();
    assert!(text[first + 1].contains("second"), "{text:?}");
    assert!(text[first - 1].contains("⇄ agents"), "{text:?}");
    assert_eq!(
        text.iter().filter(|line| line.contains("⇄ agents")).count(),
        1,
        "{text:?}"
    );
}

#[test]
fn hidden_rows_do_not_act_as_the_previous_speaker() {
    let chat = vec![
        tool(&builder(), "cargo build"),
        agent(
            &reviewer(),
            "Reviewer",
            BackendKind::Claude,
            "reviewer reply",
        ),
        user("for reviewer", &[reviewer()]),
        agent(&builder(), "Builder", BackendKind::Codex, "builder reply"),
    ];

    let all = history(&focused_state(chat.clone(), None), 80);
    let focused = history(&focused_state(chat, Some(builder())), 80);

    let titles = |text: &[String]| {
        text.iter()
            .filter(|line| line.contains("Builder · codex"))
            .count()
    };
    assert_eq!(titles(&all), 2, "{all:?}");
    // One agent's window has no speaker labels at all.
    assert_eq!(titles(&focused), 0, "{focused:?}");
    assert!(has(&focused, "builder reply"), "{focused:?}");
}

#[test]
fn turns_are_never_separated_by_a_rule_in_either_view() {
    let chat = vec![
        user("first", &[builder()]),
        agent(&builder(), "Builder", BackendKind::Codex, "builder reply"),
        tool(&reviewer(), "reviewer tool"),
        user("second", &[builder()]),
    ];

    let all = history(&focused_state(chat.clone(), None), 80);
    let focused = history(&focused_state(chat, Some(builder())), 80);

    assert!(!all.iter().any(|line| is_separator_text(line)), "{all:?}");
    assert!(
        !focused.iter().any(|line| is_separator_text(line)),
        "{focused:?}"
    );
}

#[test]
fn all_view_keeps_the_interleaved_transcript() {
    let chat = vec![
        user("for reviewer", &[reviewer()]),
        agent(
            &reviewer(),
            "Reviewer",
            BackendKind::Claude,
            "reviewer reply",
        ),
        route(&builder(), &["reviewer"], "please review"),
        agent(&builder(), "Builder", BackendKind::Codex, "builder reply"),
    ];

    let text = history(&focused_state(chat, None), 80);

    assert!(has(&text, "to Reviewer"), "{text:?}");
    assert!(has(&text, "  for reviewer"), "{text:?}");
    assert!(has(&text, "reviewer reply"), "{text:?}");
    assert!(
        has(&text, "· Builder → Reviewer  please review"),
        "{text:?}"
    );
    assert!(has(&text, "builder reply"), "{text:?}");
    assert!(!has(&text, "→ you →"), "{text:?}");
}

#[test]
fn live_activity_is_shown_only_for_the_focused_member() {
    let chat = vec![
        user("go", &[builder(), reviewer()]),
        agent(&builder(), "Builder", BackendKind::Codex, "builder reply"),
    ];
    let running = |focus| {
        focused_state_with(
            chat.clone(),
            focus,
            MemberStatus::Running,
            MemberStatus::Running,
        )
    };

    let all = history(&running(None), 100);
    assert_eq!(
        all.iter().filter(|line| line.contains("Working ·")).count(),
        2
    );

    let builder_view = history(&running(Some(builder())), 100);
    assert_eq!(
        builder_view
            .iter()
            .filter(|line| line.contains("Working ·"))
            .count(),
        1,
        "{builder_view:?}"
    );
    assert!(!has(&builder_view, "Reviewer"), "{builder_view:?}");

    let reviewer_view = history(&running(Some(reviewer())), 100);
    assert_eq!(
        reviewer_view
            .iter()
            .filter(|line| line.contains("Working ·"))
            .count(),
        1,
        "{reviewer_view:?}"
    );
    assert!(!has(&reviewer_view, "builder reply"), "{reviewer_view:?}");
}

fn draw(terminal: &mut Terminal<TestBackend>, state: &AppState) -> Option<ChatLayout> {
    let mut layout = None;
    terminal
        .draw(|frame| {
            layout = render(frame, state);
        })
        .unwrap();
    layout
}

#[test]
fn switching_focus_repaints_without_stale_content() {
    let mut state = focused_state(
        vec![
            agent(&builder(), "Builder", BackendKind::Codex, "BUILDER-ONLY"),
            agent(
                &reviewer(),
                "Reviewer",
                BackendKind::Claude,
                "REVIEWER-ONLY",
            ),
        ],
        Some(builder()),
    );
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();

    draw(&mut terminal, &state);
    let view = format!("{}", terminal.backend());
    assert!(
        view.contains("BUILDER-ONLY") && !view.contains("REVIEWER-ONLY"),
        "{view}"
    );

    state.set_focus(Some(reviewer()));
    draw(&mut terminal, &state);
    let view = format!("{}", terminal.backend());
    assert!(
        view.contains("REVIEWER-ONLY") && !view.contains("BUILDER-ONLY"),
        "{view}"
    );

    state.set_focus(None);
    draw(&mut terminal, &state);
    let view = format!("{}", terminal.backend());
    assert!(
        view.contains("REVIEWER-ONLY") && view.contains("BUILDER-ONLY"),
        "{view}"
    );

    state.set_focus(Some(builder()));
    draw(&mut terminal, &state);
    let view = format!("{}", terminal.backend());
    assert!(
        view.contains("BUILDER-ONLY") && !view.contains("REVIEWER-ONLY"),
        "{view}"
    );
}

#[test]
fn layout_text_and_selection_reflect_the_focused_window() {
    let state = focused_state(
        vec![
            agent(&builder(), "Builder", BackendKind::Codex, "BUILDER-ONLY"),
            agent(
                &reviewer(),
                "Reviewer",
                BackendKind::Claude,
                "REVIEWER-ONLY",
            ),
        ],
        Some(builder()),
    );
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();

    let layout = draw(&mut terminal, &state).expect("chat layout");

    let joined = layout.lines.join("\n");
    assert!(joined.contains("BUILDER-ONLY"), "{joined}");
    assert!(!joined.contains("REVIEWER-ONLY"), "{joined}");
    let copied = layout.selected_text(crate::tui::app_state::ChatSelection {
        start: (0, 0),
        end: (layout.total_lines.saturating_sub(1), 99),
    });
    assert!(copied.contains("BUILDER-ONLY"), "{copied}");
    assert!(!copied.contains("REVIEWER-ONLY"), "{copied}");
}

#[test]
fn header_lists_the_members_and_leaves_the_agent_hint_to_the_footer() {
    let state = focused_state(Vec::new(), Some(reviewer()));
    let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();

    draw(&mut terminal, &state);
    let view = format!("{}", terminal.backend());

    assert!(view.contains("● Builder · codex"), "{view}");
    assert!(view.contains("● Reviewer · claude"), "{view}");
    // The hint lives in the footer only, never twice.
    assert!(
        !view.lines().next().unwrap_or_default().contains("← agents"),
        "{view}"
    );
    assert_eq!(view.matches("← agents").count(), 1, "{view}");
    assert!(
        view.lines().last().unwrap_or_default().contains("← agents")
            || view.contains("Enter send · ← agents"),
        "{view}"
    );
}

#[test]
fn route_events_from_the_runtime_are_summarized_in_a_focused_window() {
    let mut state = focused_state(Vec::new(), Some(builder()));
    state.apply(RuntimeEvent::Route {
        turn: TurnId(1),
        from: reviewer(),
        to: vec!["builder".to_string()],
        body: "fix the parser".to_string(),
    });

    let text = history(&state, 80);

    assert!(has(&text, "· from   Reviewer  fix the parser"), "{text:?}");
}

#[test]
fn legacy_routing_prefix_is_not_shown() {
    let chat = vec![
        user("@builder old prompt for builder", &[builder()]),
        user("@reviewer old prompt for reviewer", &[reviewer()]),
        user("@all old broadcast", &[builder(), reviewer()]),
    ];

    let all = history(&focused_state(chat.clone(), None), 80);
    assert!(has(&all, "old prompt for builder"), "{all:?}");
    assert!(!has(&all, "@builder"), "{all:?}");
    assert!(!has(&all, "@reviewer"), "{all:?}");
    assert!(!has(&all, "@all"), "{all:?}");

    let focused = history(&focused_state(chat, Some(builder())), 80);
    assert!(!has(&focused, "old prompt for reviewer"), "{focused:?}");
    assert!(has(&focused, "old prompt for builder"), "{focused:?}");
    assert!(has(&focused, "old broadcast"), "{focused:?}");
    assert!(!has(&focused, "@reviewer"), "{focused:?}");
    assert!(!has(&focused, "@builder"), "{focused:?}");
}

#[test]
fn welcome_page_teaches_plain_text_and_the_agent_switch() {
    let state = focused_state(Vec::new(), Some(reviewer()));
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();

    draw(&mut terminal, &state);
    let view = format!("{}", terminal.backend());

    assert!(view.contains("<message>"), "{view}");
    assert!(view.contains("talk to Reviewer"), "{view}");
    assert!(view.contains("switch agent"), "{view}");
    assert!(!view.contains("@reviewer"), "{view}");
}

#[test]
fn welcome_page_shows_the_model_and_the_model_command() {
    let state = focused_state(Vec::new(), Some(builder()));
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();

    draw(&mut terminal, &state);
    let view = format!("{}", terminal.backend());

    assert!(view.contains("Model: codex"), "{view}");
    assert!(!view.contains("Model: codex ·"), "{view}");
    assert!(view.contains("/model"), "{view}");
    assert!(view.contains("switch model"), "{view}");
    assert!(view.contains("talk to Builder"), "{view}");
}

#[test]
fn model_selector_renders_inline_above_the_composer() {
    let mut state = focused_state(
        vec![agent(
            &reviewer(),
            "Reviewer",
            BackendKind::Claude,
            "CHAT-STAYS-VISIBLE",
        )],
        Some(reviewer()),
    );
    state.seed_installed_models(
        BackendKind::Codex,
        vec!["gpt-5.6".to_string(), "gpt-mini".to_string()],
    );
    state.open_model_picker("");
    let mut terminal = Terminal::new(TestBackend::new(100, 34)).unwrap();

    draw(&mut terminal, &state);
    let view = format!("{}", terminal.backend());

    assert!(view.contains("CHAT-STAYS-VISIBLE"), "{view}");
    assert!(view.contains("Model for Reviewer"), "{view}");
    assert!(view.contains("Search: type to filter models"), "{view}");
    // Nothing is pinned, so no row is current and there are no default rows.
    assert!(view.contains("› gpt-5.6"), "{view}");
    assert!(view.contains("gpt-mini"), "{view}");
    assert!(
        !view.contains("current ·") && !view.contains("default"),
        "{view}"
    );
    // The target's own backend is still loading: a heading and a status row.
    let rows = screen_rows(&terminal);
    for expected in ["  codex", "  claude", "  loading models…"] {
        assert!(
            rows.iter().any(|row| row.trim_end() == expected),
            "{expected}: {rows:#?}"
        );
    }
    assert!(
        view.contains("↑/↓ model · ←/→ effort · Enter select · Esc close"),
        "{view}"
    );
    // Neither model reports effort levels, so no squares are drawn.
    assert!(!view.contains('■') && !view.contains('□'), "{view}");
    // The selector sits between the chat and the composer.
    let chat = view.find("CHAT-STAYS-VISIBLE").unwrap();
    let selector = view.find("Model for Reviewer").unwrap();
    let composer = view.find("Enter send").unwrap();
    assert!(chat < selector && selector < composer, "{view}");
    assert!(!view.contains("Agents"), "{view}");
}

#[test]
fn model_selector_hint_shortens_on_narrow_terminals() {
    let mut state = focused_state(Vec::new(), Some(builder()));
    state.open_model_picker("");
    let mut terminal = Terminal::new(TestBackend::new(60, 30)).unwrap();

    draw(&mut terminal, &state);
    let view = format!("{}", terminal.backend());

    assert!(view.contains("Enter select · Esc close"), "{view}");
    assert!(!view.contains("↑/↓ model"), "{view}");
}

/// The screen as plain rows, from the terminal buffer.
fn screen_rows(terminal: &Terminal<TestBackend>) -> Vec<String> {
    let buffer = terminal.backend().buffer();
    let area = buffer.area;
    (0..area.height)
        .map(|y| {
            (0..area.width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect::<String>()
        })
        .collect()
}

fn effort_catalog() -> crate::adapter::DiscoveredCatalog {
    use crate::adapter::DiscoveredModel;
    let model = |id: &str, name: &str, levels: &[Effort], default: bool| {
        let mut model = DiscoveredModel::simple(id);
        model.name = name.to_string();
        model.supported_efforts = levels.to_vec();
        model.is_default = default;
        model
    };
    crate::adapter::DiscoveredCatalog {
        models: vec![
            model(
                "gpt-5.6-sol",
                "GPT-5.6 Sol",
                &[Effort::Low, Effort::Medium, Effort::High, Effort::Xhigh],
                true,
            ),
            model(
                "o3",
                "o3",
                &[Effort::Low, Effort::Medium, Effort::High],
                false,
            ),
            model(
                "a-very-long-model-name-for-alignment",
                "A very long model name for alignment checks",
                &[Effort::Medium, Effort::High],
                false,
            ),
            model("plain", "plain", &[], false),
        ],
        native_permission: None,
        ..Default::default()
    }
}

/// The picker's model rows on screen, by the name each starts with.
fn picker_rows<'a>(rows: &'a [String], names: &[&str]) -> Vec<&'a String> {
    rows.iter()
        .filter(|row| {
            let text = row.trim_start_matches(['›', ' ']);
            names.iter().any(|name| text.starts_with(name))
                && (row.starts_with("› ") || row.starts_with("  "))
                && !row.contains("Model:")
        })
        .collect()
}

#[test]
fn effort_squares_start_in_one_column_right_after_the_name_column() {
    let mut state = focused_state(Vec::new(), Some(builder()));
    state.seed_installed_catalog(BackendKind::Codex, effort_catalog());
    state.open_model_picker("");
    let mut terminal = Terminal::new(TestBackend::new(110, 36)).unwrap();

    draw(&mut terminal, &state);
    let rows = screen_rows(&terminal);

    let model_rows = picker_rows(&rows, &["GPT-5.6 Sol", "o3", "A very long", "plain"]);
    assert_eq!(model_rows.len(), 4, "{rows:#?}");

    let first_square = |row: &str| row.chars().position(|ch| ch == '■' || ch == '□');
    let columns: Vec<Option<usize>> = model_rows.iter().map(|row| first_square(row)).collect();
    let reasoning: Vec<usize> = columns.iter().flatten().copied().collect();
    assert_eq!(reasoning.len(), 3, "{model_rows:#?}");
    assert!(
        reasoning.iter().all(|column| *column == reasoning[0]),
        "squares must share a column: {model_rows:#?}"
    );
    assert!(columns.contains(&None), "the plain model has no squares");
    // The cluster hugs the name column (marker 2 + name <= 30 + small gap +
    // arrow cell) instead of floating towards the middle of a wide terminal.
    assert!(reasoning[0] <= 2 + 30 + 2 + 2, "{model_rows:#?}");
    // Only the current model carries trailing text, two columns after its
    // effort label rather than at the far edge.
    let current: Vec<&&String> = model_rows
        .iter()
        .filter(|row| row.trim_end().ends_with("current"))
        .collect();
    assert_eq!(current.len(), 1, "{model_rows:#?}");
    assert!(current[0].contains("GPT-5.6 Sol"), "{current:?}");
    assert!(current[0].contains("medium  current"), "{}", current[0]);
    assert!(current[0].trim_end().chars().count() < 60, "{}", current[0]);
    assert!(!model_rows.iter().any(|row| row.contains("codex")));
    // The longest name is truncated with an ellipsis, not allowed to push
    // the squares sideways.
    assert!(model_rows.iter().any(|row| row.contains('…')), "{rows:#?}");
    // The cluster never collides with the trailing zone.
    for row in &model_rows {
        let squares_end = row
            .chars()
            .enumerate()
            .filter(|(_, ch)| *ch == '■' || *ch == '□')
            .map(|(index, _)| index)
            .last();
        if let Some(end) = squares_end {
            assert!(end + 4 < 110 - "current".len(), "{row}");
        }
    }
}

#[test]
fn current_marker_follows_the_name_column_on_a_row_without_levels() {
    let mut state = focused_state(Vec::new(), Some(builder()));
    state.apply(RuntimeEvent::Ready {
        modes: Default::default(),
        mode_overrides: Default::default(),
        suggested_verify: None,
        team: "t".to_string(),
        workspace: String::new(),
        default_target: Some(DefaultTarget::Member(builder())),
        runs: Vec::new(),
        members: vec![
            MemberSummary {
                model: Some("plain".to_string()),
                ..member_summary(
                    "builder",
                    "Builder",
                    BackendKind::Codex,
                    "impl",
                    MemberStatus::Idle,
                )
            },
            member_summary(
                "reviewer",
                "Reviewer",
                BackendKind::Claude,
                "review",
                MemberStatus::Idle,
            ),
        ],
    });
    state.set_focus(Some(builder()));
    state.seed_installed_catalog(BackendKind::Codex, effort_catalog());
    state.open_model_picker("");
    let mut terminal = Terminal::new(TestBackend::new(110, 36)).unwrap();

    draw(&mut terminal, &state);
    let rows = screen_rows(&terminal);

    let plain = rows
        .iter()
        .find(|row| row.trim_end().ends_with("current"))
        .expect("current row");
    assert!(plain.contains("plain"), "{plain}");
    // Name column is the widest reasoning name (30 at most): two columns past
    // it, not at the edge.
    let at = plain[..plain.find("current").unwrap()].chars().count();
    assert!(at <= 2 + 30 + 2, "{plain}");
    assert!(at > "› plain".len(), "{plain}");
}

#[test]
fn selected_row_shows_arrows_the_level_name_and_a_band() {
    let mut state = focused_state(Vec::new(), Some(builder()));
    state.seed_installed_catalog(BackendKind::Codex, effort_catalog());
    state.open_model_picker("");
    state.cycle_model_effort(true);
    let mut terminal = Terminal::new(TestBackend::new(110, 36)).unwrap();

    draw(&mut terminal, &state);
    let rows = screen_rows(&terminal);

    let selected = rows
        .iter()
        .position(|row| row.starts_with("› "))
        .expect("selected row");
    assert!(rows[selected].contains("GPT-5.6 Sol"), "{}", rows[selected]);
    assert!(rows[selected].contains('←') && rows[selected].contains('→'));
    // Medium is seeded; one step up is high: three of four squares filled.
    assert!(rows[selected].contains("■■■□"), "{}", rows[selected]);
    assert!(rows[selected].contains("high"), "{}", rows[selected]);
    // It is also the model Codex runs, so it says so, and nothing else.
    assert!(rows[selected].trim_end().ends_with("current"));
    assert!(!rows[selected].contains("codex"), "{}", rows[selected]);
    // Other rows carry no arrows.
    for (index, row) in rows.iter().enumerate() {
        if index != selected && row.contains('□') {
            assert!(!row.contains('←') && !row.contains('→'), "{row}");
            assert!(row.starts_with("  ") || !row.starts_with('›'), "{row}");
        }
    }
    // The band covers the whole selected row.
    let buffer = terminal.backend().buffer();
    let band = crate::tui::theme::chat_selection().bg;
    for x in 0..110u16 {
        assert_eq!(buffer[(x, selected as u16)].bg, band.unwrap(), "col {x}");
    }
    let other = rows.iter().position(|row| row.starts_with("  o3")).unwrap();
    assert_ne!(buffer[(0u16, other as u16)].bg, band.unwrap());
}

#[test]
fn selector_search_is_one_plain_line_and_rows_group_under_backend_headings() {
    let mut state = focused_state(Vec::new(), Some(builder()));
    state.seed_installed_catalog(BackendKind::Codex, effort_catalog());
    state.seed_installed_models(BackendKind::Claude, vec!["opus".to_string()]);
    state.open_model_picker("");
    let mut terminal = Terminal::new(TestBackend::new(110, 40)).unwrap();

    draw(&mut terminal, &state);
    let rows = screen_rows(&terminal);

    let header = rows
        .iter()
        .position(|row| row.starts_with("  Model for Builder"))
        .expect("header");
    // The very next row is the whole search field: no box around it.
    assert_eq!(
        rows[header + 1].trim_end(),
        "  Search: type to filter models"
    );
    assert!(
        !rows
            .iter()
            .any(|row| row.contains('┌') || row.contains('╭'))
    );
    // Dim one-line headings, and no backend name on the model rows.
    let codex = rows
        .iter()
        .position(|row| row.trim_end() == "  codex")
        .unwrap();
    let claude = rows
        .iter()
        .position(|row| row.trim_end() == "  claude")
        .unwrap();
    assert!(header + 1 < codex && codex < claude, "{rows:#?}");
    let buffer = terminal.backend().buffer();
    let dim = buffer[(2u16, codex as u16)].style();
    assert_eq!(dim.fg, crate::tui::theme::muted().fg);
    assert!(dim.add_modifier.contains(ratatui::style::Modifier::DIM));
    assert!(
        !rows[codex + 1..claude]
            .iter()
            .any(|row| row.trim_end().ends_with("codex")),
        "{rows:#?}"
    );
    assert!(rows[claude + 1].contains("opus"), "{rows:#?}");

    // Typing fills the same single line, and the cursor follows it.
    for ch in "gpt".chars() {
        state.model_filter_push(ch);
    }
    draw(&mut terminal, &state);
    let rows = screen_rows(&terminal);
    let search = rows
        .iter()
        .position(|row| row.starts_with("  Search: gpt"))
        .expect("typed query");
    let cursor = terminal.get_cursor_position().unwrap();
    assert_eq!(
        (cursor.x, cursor.y),
        ("  Search: gpt".len() as u16, search as u16)
    );
}

#[test]
fn windowed_selector_shows_a_scroll_position_row() {
    let mut state = focused_state(Vec::new(), Some(builder()));
    state.seed_installed_models(
        BackendKind::Codex,
        (0..14).map(|index| format!("model-{index:02}")).collect(),
    );
    state.open_model_picker("model");
    for _ in 0..4 {
        state.select_next_model();
    }
    let mut terminal = Terminal::new(TestBackend::new(100, 40)).unwrap();

    draw(&mut terminal, &state);
    let view = format!("{}", terminal.backend());

    assert!(view.contains("(5/14)"), "{view}");
    assert!(view.contains("› model-04"), "{view}");
    assert!(
        view.contains("model-04 ·") || view.contains("model-04"),
        "{view}"
    );

    // A short list is not windowed, so there is no scroll row.
    let mut short = focused_state(Vec::new(), Some(builder()));
    short.open_model_picker("");
    draw(&mut terminal, &short);
    let view = format!("{}", terminal.backend());
    assert!(!view.contains("(1/"), "{view}");
}

#[test]
fn model_selector_hides_the_composer_cursor_and_completion() {
    let mut state = focused_state(Vec::new(), Some(builder()));
    state.open_model_picker("");
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();

    draw(&mut terminal, &state);

    let cursor = terminal.get_cursor_position().unwrap();
    // The search box row is above the composer rules.
    let size = terminal.size().unwrap();
    assert!(cursor.y < size.height.saturating_sub(4), "{cursor:?}");
}

#[test]
fn backend_status_rows_are_indented_to_the_name_column_and_muted() {
    // No catalog has landed, so the target's backend shows a loading row.
    let mut state = focused_state(Vec::new(), Some(builder()));
    state.open_model_picker("");
    let mut terminal = Terminal::new(TestBackend::new(100, 36)).unwrap();

    draw(&mut terminal, &state);
    let rows = screen_rows(&terminal);

    let status = rows
        .iter()
        .position(|row| row.contains("loading models…"))
        .expect("status row");
    assert!(
        rows[status].starts_with("  loading models…"),
        "{}",
        rows[status]
    );
    // It sits under the backend's own heading.
    assert_eq!(rows[status - 1].trim_end(), "  codex", "{rows:#?}");
    let buffer = terminal.backend().buffer();
    let column = rows[status].find("loading").unwrap() as u16;
    let style = buffer[(column, status as u16)].style();
    assert_eq!(style.fg, crate::tui::theme::muted().fg);
    assert!(
        !style
            .add_modifier
            .contains(ratatui::style::Modifier::ITALIC)
    );
}

#[test]
fn level_less_models_show_their_full_names_beside_reasoning_models() {
    use crate::adapter::{DiscoveredCatalog, DiscoveredModel};
    let mut state = focused_state(Vec::new(), Some(builder()));
    let mut reasoning = DiscoveredModel::simple("reasoning");
    reasoning.name = "Short".to_string();
    reasoning.supported_efforts = vec![Effort::Low, Effort::High];
    let mut plain = DiscoveredModel::simple("plain");
    plain.name = "A level-less model with quite a long descriptive name".to_string();
    state.seed_installed_catalog(
        BackendKind::Codex,
        DiscoveredCatalog {
            models: vec![reasoning, plain],
            ..Default::default()
        },
    );
    state.open_model_picker("");
    let mut terminal = Terminal::new(TestBackend::new(110, 36)).unwrap();

    draw(&mut terminal, &state);
    let rows = screen_rows(&terminal);

    // The reasoning name column is only as wide as "Short", yet the level-less
    // name is not cut to it.
    assert!(
        rows.iter().any(|row| row
            .contains("A level-less model with quite a long descriptive name")
            && !row.contains('…')),
        "{rows:#?}"
    );
}

#[test]
fn empty_search_shows_a_muted_no_matching_models_row() {
    let mut state = focused_state(Vec::new(), Some(builder()));
    state.open_model_picker("zzz-nothing");
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();

    draw(&mut terminal, &state);
    let rows = screen_rows(&terminal);

    let row = rows
        .iter()
        .position(|row| row.starts_with("  No matching models"))
        .expect("empty row");
    let buffer = terminal.backend().buffer();
    assert_eq!(
        buffer[(2u16, row as u16)].style().fg,
        crate::tui::theme::muted().fg
    );
}

#[test]
fn a_merged_agy_row_shares_the_effort_column_with_other_rows() {
    use crate::adapter::{DiscoveredCatalog, DiscoveredModel};
    let variant = |id: &str, name: &str, effort: Effort| {
        let mut model = DiscoveredModel::simple(id);
        model.name = name.to_string();
        model.supported_efforts = vec![effort];
        model
    };
    let mut state = focused_state(Vec::new(), Some(builder()));
    state.seed_installed_catalog(BackendKind::Codex, effort_catalog());
    state.seed_installed_catalog(
        BackendKind::Agy,
        DiscoveredCatalog {
            models: vec![
                variant("gem-high", "Gem (High)", Effort::High),
                variant("gem-low", "Gem (Low)", Effort::Low),
            ],
            ..Default::default()
        },
    );
    state.open_model_picker("");
    let mut terminal = Terminal::new(TestBackend::new(110, 40)).unwrap();

    draw(&mut terminal, &state);
    let rows = screen_rows(&terminal);

    let gem = rows
        .iter()
        .find(|row| row.trim_start_matches(['›', ' ']).starts_with("Gem"))
        .expect("merged row");
    assert!(!gem.contains("(High)") && !gem.contains("(Low)"), "{gem}");
    let column = |row: &str| row.chars().position(|ch| ch == '■' || ch == '□');
    let codex = rows
        .iter()
        .find(|row| {
            row.trim_start_matches(['›', ' '])
                .starts_with("GPT-5.6 Sol")
        })
        .unwrap();
    assert_eq!(column(gem), column(codex), "{rows:#?}");
    assert!(gem.contains("■□") || gem.contains("□■"), "{gem}");
}

#[test]
fn effort_squares_stay_in_one_column_while_the_list_scrolls() {
    use crate::adapter::{DiscoveredCatalog, DiscoveredModel};
    let model = |id: &str, name: &str, levels: &[Effort]| {
        let mut model = DiscoveredModel::simple(id);
        model.name = name.to_string();
        model.supported_efforts = levels.to_vec();
        model
    };
    // The first window holds short names with two levels; the models that
    // scroll in later have long names, four levels and longer labels.
    let mut models: Vec<DiscoveredModel> = (0..8)
        .map(|index| {
            model(
                &format!("short-{index}"),
                &format!("short {index}"),
                &[Effort::Low, Effort::High],
            )
        })
        .collect();
    models.extend((0..8).map(|index| {
        model(
            &format!("long-{index}"),
            &format!("a considerably longer model name {index}"),
            &[Effort::Low, Effort::Medium, Effort::High, Effort::Xhigh],
        )
    }));
    let mut state = focused_state(Vec::new(), Some(builder()));
    state.seed_installed_catalog(
        BackendKind::Codex,
        DiscoveredCatalog {
            models,
            ..Default::default()
        },
    );
    state.open_model_picker("");
    let mut terminal = Terminal::new(TestBackend::new(110, 40)).unwrap();
    let square_column = |terminal: &Terminal<TestBackend>, needle: &str| {
        screen_rows(terminal)
            .iter()
            .find(|row| row.contains(needle))
            .and_then(|row| row.chars().position(|ch| ch == '■' || ch == '□'))
            .unwrap_or_else(|| panic!("no effort squares on a row with {needle:?}"))
    };

    draw(&mut terminal, &state);
    let at_top = square_column(&terminal, "short 1");

    for _ in 0..15 {
        state.select_next_model();
    }
    draw(&mut terminal, &state);
    let at_bottom = square_column(&terminal, "considerably");

    assert_eq!(at_top, at_bottom);
}

/// Builder with two sub-agents (`scout` running, `tester` idle) and the
/// unrelated Reviewer, with `focus` in the main window.
fn tree_state(chat: Vec<ChatItem>, focus: Option<MemberId>) -> AppState {
    let child = |id: &str, name: &str, status| MemberSummary {
        parent: Some(builder()),
        ..member_summary(id, name, BackendKind::Claude, "sub-agent", status)
    };
    let mut state = AppState::new(chat);
    state.apply(RuntimeEvent::Ready {
        modes: Default::default(),
        mode_overrides: Default::default(),
        suggested_verify: None,
        team: "t".to_string(),
        workspace: String::new(),
        default_target: Some(DefaultTarget::Member(builder())),
        runs: Vec::new(),
        members: vec![
            member_summary(
                "builder",
                "Builder",
                BackendKind::Codex,
                "impl",
                MemberStatus::Idle,
            ),
            child("scout", "Scout", MemberStatus::Running),
            member_summary(
                "reviewer",
                "Reviewer",
                BackendKind::Claude,
                "review",
                MemberStatus::Idle,
            ),
            child("tester", "Tester", MemberStatus::Failed),
        ],
    });
    state.set_focus(focus);
    state
}

#[test]
fn header_folds_sub_agents_into_a_count_and_shows_the_viewed_one() {
    let state = tree_state(Vec::new(), Some(builder()));
    let mut terminal = Terminal::new(TestBackend::new(110, 12)).unwrap();

    draw(&mut terminal, &state);
    let header = format!("{}", terminal.backend())
        .lines()
        .next()
        .unwrap_or_default()
        .to_string();

    assert!(header.contains("Builder · codex"), "{header}");
    assert!(header.contains("Reviewer · claude"), "{header}");
    // A busy hidden sub-agent only tints the count: no "running" text on the bar.
    assert!(header.contains("+2 sub-agents"), "{header}");
    assert!(!header.contains("running"), "{header}");
    assert!(!header.contains("Scout"), "{header}");
    assert!(!header.contains("Tester"), "{header}");

    // Viewing a sub-agent puts its chip right after its parent.
    let state = tree_state(Vec::new(), Some(MemberId::new("scout")));
    draw(&mut terminal, &state);
    let header = format!("{}", terminal.backend())
        .lines()
        .next()
        .unwrap_or_default()
        .to_string();
    let at = |needle: &str| header.find(needle).unwrap_or(usize::MAX);
    assert!(at("Builder") < at("Scout"), "{header}");
    assert!(at("Scout") < at("Reviewer"), "{header}");
    assert!(!header.contains("Tester"), "{header}");
    // The viewed sub-agent is on the bar, so only the other one is counted
    // (and it is not running).
    assert!(header.contains("+1 sub-agent"), "{header}");
    assert!(!header.contains("+1 sub-agents"), "{header}");
    assert!(!header.contains("running"), "{header}");
}

#[test]
fn header_omits_the_sub_agent_count_once_the_only_one_is_on_the_bar() {
    let mut state = AppState::new(Vec::new());
    let mut child = member_summary(
        "scout",
        "Scout",
        BackendKind::Claude,
        "research",
        MemberStatus::Idle,
    );
    child.parent = Some(builder());
    state.apply(RuntimeEvent::Ready {
        modes: Default::default(),
        mode_overrides: Default::default(),
        suggested_verify: None,
        team: "t".to_string(),
        workspace: String::new(),
        default_target: None,
        runs: Vec::new(),
        members: vec![
            member_summary(
                "builder",
                "Builder",
                BackendKind::Codex,
                "impl",
                MemberStatus::Idle,
            ),
            child,
        ],
    });
    let mut terminal = Terminal::new(TestBackend::new(110, 12)).unwrap();
    let header = |state: &AppState, terminal: &mut Terminal<TestBackend>| {
        draw(terminal, state);
        format!("{}", terminal.backend())
            .lines()
            .next()
            .unwrap_or_default()
            .to_string()
    };

    state.set_focus(Some(builder()));
    let hidden = header(&state, &mut terminal);
    assert!(hidden.contains("+1 sub-agent"), "{hidden}");
    assert!(!hidden.contains("Scout"), "{hidden}");

    state.set_focus(Some(MemberId::new("scout")));
    let shown = header(&state, &mut terminal);
    assert!(shown.contains("Scout · claude"), "{shown}");
    assert!(!shown.contains("sub-agent"), "{shown}");

    // No sub-agents at all: no count either.
    let state = focused_state(Vec::new(), Some(builder()));
    assert!(!header(&state, &mut terminal).contains("sub-agent"));
}

#[test]
fn header_counts_a_single_sub_agent_in_the_singular() {
    let mut state = AppState::new(Vec::new());
    state.apply(RuntimeEvent::Ready {
        modes: Default::default(),
        mode_overrides: Default::default(),
        suggested_verify: None,
        team: "t".to_string(),
        workspace: String::new(),
        default_target: None,
        runs: Vec::new(),
        members: vec![
            member_summary(
                "builder",
                "Builder",
                BackendKind::Codex,
                "impl",
                MemberStatus::Idle,
            ),
            MemberSummary {
                parent: Some(builder()),
                ..member_summary(
                    "scout",
                    "Scout",
                    BackendKind::Claude,
                    "sub-agent",
                    MemberStatus::Idle,
                )
            },
        ],
    });
    let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();

    draw(&mut terminal, &state);
    let header = format!("{}", terminal.backend())
        .lines()
        .next()
        .unwrap_or_default()
        .to_string();

    assert!(header.contains("+1 sub-agent"), "{header}");
    assert!(!header.contains("sub-agents"), "{header}");
    assert!(!header.contains("running"), "{header}");
}

#[test]
fn spawn_task_and_hand_back_are_rows_in_the_viewed_agents_group() {
    let scout = MemberId::new("scout");
    let chat = vec![
        ChatItem::Spawn {
            parent: builder(),
            child: scout.clone(),
            summary: "claude · opus · high".to_string(),
        },
        route(&builder(), &["scout"], "Task line one\nTask line two"),
        route(&scout, &["builder"], "Result line one\nResult line two"),
    ];

    let parent = history(&tree_state(chat.clone(), Some(builder())), 90);
    let spawn = parent
        .iter()
        .position(|l| l.contains("· spawn  Scout"))
        .expect("spawn");
    let task = parent
        .iter()
        .position(|l| l.contains("· to     Scout"))
        .expect("task");
    let back = parent
        .iter()
        .position(|l| l.contains("· from   Scout"))
        .expect("result");
    assert!(spawn < task && task < back, "{parent:?}");
    // Scout exists with no catalog: its label is the backend name, not the
    // raw summary stored at spawn time.
    assert!(parent[spawn].ends_with("Scout  claude"), "{parent:?}");
    assert!(!parent[spawn].contains("opus"), "{parent:?}");
    assert_eq!(
        parent.iter().filter(|l| l.contains("⇄ agents")).count(),
        1,
        "{parent:?}"
    );
    // Collapsed: first line only.
    assert!(!has(&parent, "Task line two"), "{parent:?}");
    assert!(!has(&parent, "Result line two"), "{parent:?}");

    // The child sees the same flow from its own side.
    let child = history(&tree_state(chat.clone(), Some(scout.clone())), 90);
    assert!(!has(&child, "· spawn"), "{child:?}");
    assert!(has(&child, "· from   Builder  Task line one"), "{child:?}");
    assert!(
        has(&child, "· to     Builder  Result line one"),
        "{child:?}"
    );

    // Another member sees none of it.
    let other = history(
        &tree_state(chat.clone(), Some(MemberId::new("reviewer"))),
        90,
    );
    assert!(!has(&other, "⇄ agents"), "{other:?}");

    // The interleaved view names both ends.
    let all = history(&tree_state(chat, None), 90);
    assert!(has(&all, "· spawn  Builder → Scout  claude"), "{all:?}");
    assert!(has(&all, "· Builder → Scout  Task line one"), "{all:?}");
    assert!(has(&all, "· Scout → Builder  Result line one"), "{all:?}");
}

#[test]
fn ctrl_o_shows_the_full_agent_message_under_its_row() {
    let chat = vec![route(
        &builder(),
        &["reviewer"],
        "Line one\nLine two\nLine three",
    )];
    let mut state = focused_state(chat, Some(builder()));

    let collapsed = history(&state, 80);
    assert!(
        has(&collapsed, "· to     Reviewer  Line one"),
        "{collapsed:?}"
    );
    assert!(!has(&collapsed, "Line two"), "{collapsed:?}");

    state.toggle_tools_expansion();
    let expanded = history(&state, 80);
    assert!(has(&expanded, "↳ Line one"), "{expanded:?}");
    assert!(has(&expanded, "Line two"), "{expanded:?}");
    assert!(has(&expanded, "Line three"), "{expanded:?}");
}

#[test]
fn agent_rows_are_truncated_to_the_width_and_the_rail_is_gone() {
    let long = "word ".repeat(40);
    let state = focused_state(
        vec![route(&builder(), &["reviewer"], &long)],
        Some(builder()),
    );
    let mut lines = Vec::new();
    render_chat_history(&state, 40, 0, &mut lines);

    let text = plain_text(&lines);
    let row = text.iter().find(|l| l.contains("· to")).expect("row");
    assert!(theme::display_width(row) <= 40, "{row}");
    assert!(row.ends_with('…'), "{row}");
    assert!(
        lines
            .iter()
            .all(|line| line.spans.iter().all(|s| s.style.bg.is_none()))
    );
}

#[test]
fn find_matches_agent_message_bodies() {
    let chat = vec![route(&builder(), &["reviewer"], "needle-in-a-relay")];
    let mut state = focused_state(chat, Some(builder()));

    state.set_find("needle-in-a-relay");

    assert_eq!(state.find().map(|(_, _, total)| total), Some(1));
}

/// Style of the status dot drawn two cells before `name` in the header row.
fn header_dot(terminal: &Terminal<TestBackend>, name: &str) -> (String, ratatui::style::Style) {
    let buffer = terminal.backend().buffer();
    let row: Vec<String> = (0..buffer.area.width)
        .map(|x| buffer[(x, 0u16)].symbol().to_string())
        .collect();
    let at = (0..row.len())
        .find(|&x| row[x..].concat().starts_with(name))
        .unwrap_or_else(|| panic!("{name} in {row:?}"));
    let cell = &buffer[((at - 2) as u16, 0u16)];
    (cell.symbol().to_string(), cell.style())
}

fn header_with(builder: MemberStatus, reviewer: MemberStatus) -> Terminal<TestBackend> {
    let state = focused_state_with(Vec::new(), Some(builder_id()), builder, reviewer);
    let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
    draw(&mut terminal, &state);
    terminal
}

fn builder_id() -> MemberId {
    builder()
}

#[test]
fn header_dot_is_lit_when_running_and_dark_when_idle() {
    let terminal = header_with(MemberStatus::Running, MemberStatus::Idle);

    let (glyph, lit) = header_dot(&terminal, "Builder");
    assert_eq!(glyph, "●");
    assert_eq!(lit.fg, theme::emphasis().fg);
    assert!(lit.add_modifier.contains(ratatui::style::Modifier::BOLD));

    let (glyph, dark) = header_dot(&terminal, "Reviewer");
    assert_eq!(glyph, "●", "idle keeps the same dot shape");
    assert_eq!(dark.fg, theme::dim().fg);
    assert!(!dark.add_modifier.contains(ratatui::style::Modifier::BOLD));
    assert_ne!(lit.fg, dark.fg);
}

#[test]
fn header_dot_is_lit_for_queued_and_waiting_too() {
    let terminal = header_with(MemberStatus::Queued, MemberStatus::Waiting);

    for name in ["Builder", "Reviewer"] {
        let (glyph, style) = header_dot(&terminal, name);
        assert_eq!(glyph, "●", "{name}");
        assert_eq!(style.fg, theme::dot_lit().fg, "{name}");
        assert!(style.add_modifier.contains(ratatui::style::Modifier::BOLD));
    }
}

#[test]
fn header_dot_is_yellow_for_approval_and_red_for_failed() {
    let terminal = header_with(MemberStatus::NeedsApproval, MemberStatus::Failed);

    let (glyph, approval) = header_dot(&terminal, "Builder");
    assert_eq!(glyph, "!");
    assert_eq!(approval.fg, theme::warning().fg);

    let (glyph, failed) = header_dot(&terminal, "Reviewer");
    assert_eq!(glyph, "✗");
    assert_eq!(failed.fg, theme::error().fg);
}

fn seed_models(state: &mut AppState, name: &str) {
    let mut model = crate::adapter::DiscoveredModel::simple("opus");
    model.name = name.to_string();
    model.is_default = true;
    model.supported_efforts = vec![Effort::Low, Effort::High];
    model.default_effort = Some(Effort::High);
    state.seed_installed_catalog(
        BackendKind::Claude,
        crate::adapter::DiscoveredCatalog {
            models: vec![model],
            ..Default::default()
        },
    );
}

#[test]
fn spawn_row_and_speaker_label_show_the_model_label_and_follow_the_catalog() {
    let scout = MemberId::new("scout");
    let chat = vec![
        ChatItem::Spawn {
            parent: builder(),
            child: scout.clone(),
            summary: "claude · opus · high".to_string(),
        },
        agent(&scout, "Scout", BackendKind::Claude, "scout reply"),
    ];
    let mut state = tree_state(chat, None);
    let before = history(&state, 90);
    assert!(
        has(&before, "· spawn  Builder → Scout  claude"),
        "{before:?}"
    );
    assert!(before.iter().any(|l| l == "  Scout · claude"), "{before:?}");

    // The catalog lands: both rows re-render (the paint cache is invalidated).
    seed_models(&mut state, "Opus 9");
    let after = history(&state, 90);
    assert!(
        has(&after, "· spawn  Builder → Scout  Opus 9 · high"),
        "{after:?}"
    );
    assert!(
        after.iter().any(|l| l == "  Scout · Opus 9 · high"),
        "{after:?}"
    );

    // The painted (cached) view follows a later catalog change too.
    let painted = |state: &AppState| {
        let mut terminal = Terminal::new(TestBackend::new(90, 14)).unwrap();
        terminal
            .draw(|frame| {
                let _ = render(frame, state);
            })
            .unwrap();
        format!("{}", terminal.backend())
    };
    assert!(painted(&state).contains("Scout · Opus 9 · high"));
    seed_models(&mut state, "Opus 10");
    let repainted = painted(&state);
    assert!(repainted.contains("Scout · Opus 10 · high"), "{repainted}");
    assert!(repainted.contains("Scout  Opus 10 · high"), "{repainted}");
}

#[test]
fn removed_members_keep_the_backend_name_and_the_stored_spawn_summary() {
    let gone = MemberId::new("gone");
    let chat = vec![
        ChatItem::Spawn {
            parent: builder(),
            child: gone.clone(),
            summary: "claude · opus · high".to_string(),
        },
        agent(&gone, "Gone", BackendKind::Claude, "last words"),
    ];
    let mut state = tree_state(chat, None);
    seed_models(&mut state, "Opus 9");
    let text = history(&state, 90);
    assert!(has(&text, "claude · opus · high"), "{text:?}");
    assert!(text.iter().any(|l| l == "  Gone · claude"), "{text:?}");
}
