//! Chrome around the conversation: message headers, the inline mode panel,
//! overlay clearing, and the team editor's single hint line.

use super::super::*;
use super::*;

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

fn state_over(chat: Vec<ChatItem>, focus: Option<MemberId>) -> AppState {
    let mut state = AppState::new(chat);
    state.apply(RuntimeEvent::Ready {
        modes: Default::default(),
        mode_overrides: Default::default(),
        suggested_verify: None,
        team: "t".to_string(),
        workspace: "/tmp/ws".to_string(),
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
            member_summary(
                "reviewer",
                "Reviewer",
                BackendKind::Claude,
                "review",
                MemberStatus::Idle,
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

fn count(text: &[String], needle: &str) -> usize {
    text.iter().filter(|line| line.contains(needle)).count()
}

/// Screen rows as plain strings, from the terminal buffer.
fn rows_of(terminal: &Terminal<TestBackend>) -> Vec<String> {
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

fn draw_rows(state: &AppState, width: u16, height: u16) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| {
            let _ = render(frame, state);
        })
        .unwrap();
    rows_of(&terminal)
}

#[test]
fn agent_replies_have_no_header_in_a_single_agent_window_and_a_dim_label_in_all() {
    let chat = vec![
        user("hello", &[builder()]),
        agent(&builder(), "Builder", BackendKind::Codex, "hi there"),
    ];

    let one = history(&state_over(chat.clone(), Some(builder())), 80);
    assert!(!one.iter().any(|line| line.contains("Builder")), "{one:?}");
    assert!(!one.iter().any(|line| line.contains('◆')), "{one:?}");
    assert!(one.iter().any(|line| line == "  hi there"), "{one:?}");

    let state = state_over(chat, None);
    let all = history(&state, 80);
    // One dim label, no glyph, one space before the dot.
    assert!(
        all.iter().any(|line| line == "  Builder · codex"),
        "{all:?}"
    );
    assert!(!all.iter().any(|line| line.contains('◆')), "{all:?}");
    let mut lines = Vec::new();
    render_chat_history(&state, 80, 0, &mut lines);
    let label = lines
        .iter()
        .find(|line| plain_text(std::slice::from_ref(*line))[0] == "  Builder · codex")
        .unwrap();
    assert!(label.spans.iter().skip(1).all(|span| {
        span.style
            .add_modifier
            .contains(ratatui::style::Modifier::DIM)
    }));
}

#[test]
fn user_messages_are_a_headerless_band_naming_the_recipient_only_in_all() {
    let chat = vec![user("hello", &[builder()])];

    let one = history(&state_over(chat.clone(), Some(builder())), 60);
    assert!(!one.iter().any(|line| line.contains("You")), "{one:?}");
    assert!(!one.iter().any(|line| line.contains('◆')), "{one:?}");
    assert!(
        !one.iter().any(|line| line.contains("to Builder")),
        "{one:?}"
    );

    let all = history(&state_over(chat, None), 60);
    assert!(!all.iter().any(|line| line.contains("You")), "{all:?}");
    // Right-aligned on the top padding row, before the text rows.
    let top = all.first().unwrap();
    assert!(top.trim_end().ends_with("to Builder"), "{all:?}");
    assert_eq!(theme::display_width(top), 60, "{all:?}");
    assert_eq!(top.trim(), "to Builder");
}

#[test]
fn user_messages_are_full_width_bands_with_padding_rows() {
    let state = state_over(vec![user("hello band", &[builder()])], Some(builder()));
    let mut lines = Vec::new();
    render_chat_history(&state, 60, 0, &mut lines);
    let bg = theme::user_band_bg();

    // blank band row, one text row, blank band row, then the entry gap.
    assert_eq!(lines.len(), 4, "{:?}", plain_text(&lines));
    let text = plain_text(&lines);
    assert_eq!(text[0].trim(), "");
    assert_eq!(text[1], format!("  hello band{}", " ".repeat(60 - 12)));
    assert_eq!(text[2].trim(), "");
    assert_eq!(text[3], "");
    for line in &lines[..3] {
        assert_eq!(
            theme::display_width(&plain_text(std::slice::from_ref(line))[0]),
            60
        );
        assert!(
            line.spans.iter().all(|span| span.style.bg == Some(bg)),
            "every cell of a band row carries the band background: {line:?}"
        );
    }
    assert!(lines[3].spans.iter().all(|span| span.style.bg.is_none()));
    // The text itself is indented two columns.
    assert_eq!(lines[1].spans[0].content.as_ref(), "  ");
}

#[test]
fn user_text_agent_text_and_tool_titles_share_one_left_edge() {
    let state = state_over(
        vec![
            user("question", &[builder()]),
            agent(&builder(), "Builder", BackendKind::Codex, "answer"),
            tool(&builder(), "ls"),
            route(&builder(), &["reviewer"], "ping"),
            ChatItem::Notice {
                member: None,
                text: "a notice".to_string(),
            },
        ],
        Some(builder()),
    );
    let text = history(&state, 80);
    let column = |needle: &str| {
        let line = text.iter().find(|line| line.contains(needle)).unwrap();
        line.chars().take_while(|ch| *ch == ' ').count()
    };
    assert_eq!(column("question"), 2, "{text:?}");
    assert_eq!(column("answer"), 2, "{text:?}");
    assert_eq!(column("⚒ tools"), 2, "{text:?}");
    assert_eq!(column("⇄ agents"), 2, "{text:?}");
    assert_eq!(column("a notice"), 2, "{text:?}");
    // The rows inside a group sit one step deeper, under their title.
    assert_eq!(column("· Shell"), 4, "{text:?}");
}

#[test]
fn entries_are_one_blank_row_apart_with_no_rules_and_none_inside_a_group() {
    let state = state_over(
        vec![
            user("first", &[builder()]),
            agent(&builder(), "Builder", BackendKind::Codex, "reply"),
            tool(&builder(), "ls"),
            tool(&builder(), "pwd"),
            user("second", &[builder()]),
            agent(&builder(), "Builder", BackendKind::Codex, "again"),
        ],
        Some(builder()),
    );
    let text = history(&state, 80);

    assert!(!text.iter().any(|line| is_separator_text(line)), "{text:?}");
    // Never two blank rows in a row (the band's own padding rows are band
    // coloured, not empty), and always one between entries.
    for pair in text.windows(2) {
        assert!(!(pair[0].is_empty() && pair[1].is_empty()), "{text:?}");
    }
    let at = |needle: &str| text.iter().position(|line| line.contains(needle)).unwrap();
    assert!(text[at("reply") + 1].is_empty(), "{text:?}");
    assert!(text[at("⚒ tools") - 1].is_empty(), "{text:?}");
    // The two tool rows follow their title directly.
    assert_eq!(at("· Shell   ls") + 1, at("· Shell   pwd"), "{text:?}");
    assert_eq!(at("⚒ tools") + 1, at("· Shell   ls"), "{text:?}");
    // The second user band follows a blank row, then its own padding row.
    let second = at("second");
    assert!(text[second - 2].is_empty(), "{text:?}");
    assert!(text[second - 1].trim().is_empty() && !text[second - 1].is_empty());
}

#[test]
fn a_reply_continues_without_a_second_label_after_agent_message_rows() {
    let chat = vec![
        user("go", &[builder()]),
        agent(&builder(), "Builder", BackendKind::Codex, "first part"),
        route(&builder(), &["reviewer"], "please look"),
        route(&reviewer(), &["builder"], "looks fine"),
        agent(&builder(), "Builder", BackendKind::Codex, "second part"),
    ];

    for focus in [Some(builder()), None] {
        let text = history(&state_over(chat.clone(), focus.clone()), 80);
        assert_eq!(
            count(&text, "Builder · codex"),
            usize::from(focus.is_none()),
            "{focus:?}: {text:?}"
        );
        let first = text.iter().position(|l| l.contains("first part")).unwrap();
        let rows = text.iter().position(|l| l.contains("⇄ agents")).unwrap();
        let second = text.iter().position(|l| l.contains("second part")).unwrap();
        assert!(first < rows && rows < second, "{text:?}");
    }
}

#[test]
fn a_reply_continues_without_a_second_label_after_tool_rows() {
    let chat = vec![
        user("go", &[builder()]),
        agent(&builder(), "Builder", BackendKind::Codex, "first part"),
        tool(&builder(), "ls"),
        agent(&builder(), "Builder", BackendKind::Codex, "second part"),
    ];
    let text = history(&state_over(chat, None), 80);
    assert_eq!(count(&text, "Builder · codex"), 1, "{text:?}");
}

#[test]
fn the_label_repeats_after_a_user_message_or_another_agents_message() {
    let after_user = vec![
        agent(&builder(), "Builder", BackendKind::Codex, "first"),
        user("and then?", &[builder()]),
        agent(&builder(), "Builder", BackendKind::Codex, "second"),
    ];
    let text = history(&state_over(after_user, None), 80);
    assert_eq!(count(&text, "Builder · codex"), 2, "{text:?}");

    let other_spoke = vec![
        user("go", &[builder(), reviewer()]),
        agent(&builder(), "Builder", BackendKind::Codex, "first"),
        route(&builder(), &["reviewer"], "ping"),
        agent(&reviewer(), "Reviewer", BackendKind::Claude, "pong"),
        route(&reviewer(), &["builder"], "ping back"),
        agent(&builder(), "Builder", BackendKind::Codex, "second"),
    ];
    let text = history(&state_over(other_spoke, None), 80);
    assert_eq!(count(&text, "Reviewer · claude"), 1, "{text:?}");
    assert!(count(&text, "Builder · codex") >= 1, "{text:?}");
}

#[test]
fn header_chips_show_the_model_name_and_effort_once_a_catalog_names_it() {
    let mut state = state_over(Vec::new(), Some(builder()));
    // No catalog yet: the backend name.
    let rows = draw_rows(&state, 80, 10);
    assert!(rows[0].contains("● Builder · codex"), "{}", rows[0]);

    let mut sol = crate::adapter::DiscoveredModel::simple("gpt-sol");
    sol.name = "GPT-Sol".to_string();
    sol.is_default = true;
    sol.supported_efforts = vec![Effort::Low, Effort::High];
    sol.default_effort = Some(Effort::High);
    state.seed_installed_catalog(
        BackendKind::Codex,
        crate::adapter::DiscoveredCatalog {
            models: vec![sol],
            ..Default::default()
        },
    );
    let rows = draw_rows(&state, 80, 10);
    assert!(
        rows[0].contains("● Builder · GPT-Sol · high"),
        "{}",
        rows[0]
    );
    assert!(!rows[0].contains("Builder · codex"), "{}", rows[0]);
    // Only the open agent is named in the header.
    assert!(!rows[0].contains("Reviewer"), "{}", rows[0]);
}

#[test]
fn the_screen_has_no_horizontal_rules_at_all() {
    let state = state_over(
        vec![
            user("go", &[builder()]),
            agent(&builder(), "Builder", BackendKind::Codex, "done"),
            tool(&builder(), "ls"),
        ],
        Some(builder()),
    );
    let rows = draw_rows(&state, 60, 20);
    // No title row, no rule under the member bar, none around the composer.
    assert!(rows.iter().all(|row| !row.contains("────")), "{rows:#?}");
    assert!(rows[0].contains("● Builder · codex"), "{rows:#?}");
    assert!(
        !rows.iter().any(|row| row.contains("Asterline 1.")),
        "{rows:#?}"
    );
}

#[test]
fn the_composer_is_a_full_width_band_with_padding_rows_and_the_2_column_indent() {
    let mut state = state_over(vec![user("go", &[builder()])], Some(builder()));
    state.insert_text("typed text");
    let rows = draw_rows(&state, 60, 12);
    let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
    terminal
        .draw(|frame| {
            let _ = render(frame, &state);
        })
        .unwrap();
    let buffer = terminal.backend().buffer();
    let input = rows
        .iter()
        .position(|row| row.contains("typed text"))
        .unwrap();
    // Text starts at column 2, after the dim prompt; one padding row above
    // and one below, so the text is not cut by the band's edge.
    assert_eq!(rows[input].find("typed text"), Some(2), "{rows:#?}");
    assert!(rows[input - 1].trim().is_empty(), "{rows:#?}");
    assert!(rows[input + 1].trim().is_empty(), "{rows:#?}");
    for y in [input - 1, input, input + 1] {
        for x in 0..60u16 {
            assert_eq!(
                buffer[(x, y as u16)].bg,
                theme::user_band_bg(),
                "col {x} row {y}"
            );
        }
    }
    // The footer follows the bottom padding row directly, off the band.
    assert!(rows[input + 2].contains("Enter send"), "{rows:#?}");
    assert_ne!(buffer[(0u16, (input + 2) as u16)].bg, theme::user_band_bg());
}

#[test]
fn the_path_sits_dim_at_the_right_end_of_the_footer() {
    let state = state_over(Vec::new(), Some(builder()));
    let rows = draw_rows(&state, 80, 12);
    let footer = rows.last().unwrap();
    assert!(
        footer.starts_with("Enter send · ← agents · /help"),
        "{footer}"
    );
    assert!(footer.trim_end().ends_with("/tmp/ws"), "{footer}");
    // A narrow footer clips the path from the left, never the hints.
    let narrow = draw_rows(&state_over(Vec::new(), Some(builder())), 40, 12);
    let footer = narrow.last().unwrap();
    assert!(footer.starts_with("Enter send"), "{footer}");
}

#[test]
fn the_mode_leads_the_footer_hints_outside_normal_mode() {
    let mut state = state_over(Vec::new(), Some(builder()));
    let rows = draw_rows(&state, 80, 10);
    assert!(!rows[0].contains("mode") && !rows.last().unwrap().contains("mode"));

    state.apply(RuntimeEvent::ModeChanged {
        mode: TerminalMode::Review,
    });
    let rows = draw_rows(&state, 80, 10);
    assert!(!rows[0].contains("mode"), "not in the header: {}", rows[0]);
    let footer = rows.last().unwrap();
    assert!(footer.starts_with("review mode · Enter send"), "{footer}");
    // And no label is left on the composer.
    assert!(!rows.iter().skip(1).any(|row| row.contains("mode:")));
}

#[test]
fn working_is_shown_in_the_live_row_only() {
    let busy = |focus| {
        let mut state = state_over(vec![user("go", &[builder()])], focus);
        state.apply(RuntimeEvent::MemberStatus {
            member: builder(),
            status: MemberStatus::Running,
        });
        state
    };
    let count =
        |rows: &[String], needle: &str| rows.iter().filter(|row| row.contains(needle)).count();

    let rows = draw_rows(&busy(Some(builder())), 100, 16);
    assert_eq!(count(&rows, "Working"), 1, "{rows:#?}");
    // The live row follows the transcript; the footer only lists keys.
    let footer = rows.last().unwrap();
    assert!(
        footer.starts_with("Esc interrupt · ← agents · /help"),
        "{footer}"
    );
    assert!(
        !footer.contains("Working") && !footer.contains("Active"),
        "{footer}"
    );
    // The bar keeps its static glyph and no spinner or status text.
    assert!(rows[0].contains("● Builder · codex"), "{}", rows[0]);
    assert!(!rows[0].contains("Working") && !rows[0].contains("running"));

    // Viewing someone idle while another agent works: nothing extra.
    let rows = draw_rows(&busy(Some(reviewer())), 100, 16);
    assert_eq!(count(&rows, "Working"), 0, "{rows:#?}");
    assert!(rows.last().unwrap().starts_with("Enter send · ← agents"));

    // The Agents list shows a still activity word, never the spinner row.
    let mut state = busy(Some(builder()));
    state.toggle_drawer(Drawer::Agents);
    let rows = draw_rows(&state, 100, 20);
    let row = rows.iter().find(|row| row.contains("Builder")).unwrap();
    assert!(row.contains("working"), "{row}");
    assert!(!row.contains("Working"), "{row}");
}

fn log(level: crate::domain::event::LogLevel, source: &str, message: &str) -> RuntimeEvent {
    RuntimeEvent::Log(crate::domain::event::LogEntry::new(level, source, message))
}

#[test]
fn overlay_drawers_are_as_tall_as_their_content_and_clear_what_they_cover() {
    use crate::domain::event::LogLevel;
    let wall = "X".repeat(98);
    let chat: Vec<ChatItem> = (0..30)
        .map(|_| agent(&builder(), "Builder", BackendKind::Codex, &wall))
        .collect();
    let mut state = state_over(chat, Some(builder()));
    state.apply(log(LogLevel::Info, "user", "one"));
    state.apply(log(LogLevel::Warn, "main", "two"));
    state.toggle_drawer(Drawer::Logs);
    let rows = draw_rows(&state, 110, 36);

    // Title + 2 entries + hint + borders: 5 rows, sitting on the composer.
    let top = rows.iter().position(|row| row.starts_with('╭')).unwrap();
    let bottom = rows.iter().position(|row| row.starts_with('╰')).unwrap();
    assert_eq!(bottom - top + 1, 5, "{rows:#?}");
    assert!(rows[bottom + 1].trim().is_empty(), "{rows:#?}");
    // Nothing of the chat shows inside the box, and the chat above is intact.
    for row in &rows[top..=bottom] {
        assert!(!row.contains('X'), "{row}");
    }
    assert!(
        rows[1..top].iter().any(|row| row.contains('X')),
        "{rows:#?}"
    );

    // Help is also content-sized, capped at the chat column.
    let mut state = state_over(Vec::new(), Some(builder()));
    state.toggle_drawer(Drawer::Palette);
    let rows = draw_rows(&state, 110, 40);
    let top = rows.iter().position(|row| row.starts_with('╭')).unwrap();
    let bottom = rows.iter().position(|row| row.starts_with('╰')).unwrap();
    assert!(top > 1 && bottom - top + 1 < 38 - 1, "{rows:#?}");
    assert!(rows[top].contains("Commands · Asterline"), "{}", rows[top]);
    assert!(
        rows[top].contains(env!("CARGO_PKG_VERSION")),
        "{}",
        rows[top]
    );
}

#[test]
fn overlay_text_never_touches_the_border() {
    use crate::domain::event::LogLevel;
    let mut state = state_over(Vec::new(), Some(builder()));
    state.apply(log(LogLevel::Info, "user", "message"));
    for drawer in [Drawer::Logs, Drawer::Palette] {
        let mut state_with = state_over(Vec::new(), Some(builder()));
        state_with.apply(log(LogLevel::Info, "user", "message"));
        state_with.toggle_drawer(drawer.clone());
        let rows = draw_rows(&state_with, 80, 30);
        for row in rows.iter().filter(|row| row.starts_with('│')) {
            let chars: Vec<char> = row.chars().collect();
            assert_eq!(chars[1], ' ', "{drawer:?}: left padding: {row}");
            assert_eq!(
                chars[chars.len() - 2],
                ' ',
                "{drawer:?}: right padding: {row}"
            );
        }
    }
    drop(state);
}

#[test]
fn logs_read_oldest_first_with_levels_only_for_warnings_and_errors() {
    use crate::domain::event::LogLevel;
    let mut state = state_over(Vec::new(), Some(builder()));
    state.apply(log(LogLevel::Info, "user", "first entry"));
    state.apply(log(LogLevel::Warn, "main", "second entry"));
    state.apply(log(LogLevel::Error, "main", "third entry"));
    state.toggle_drawer(Drawer::Logs);
    let rows = draw_rows(&state, 100, 30);
    let at = |needle: &str| rows.iter().position(|row| row.contains(needle)).unwrap();

    assert!(at("first entry") < at("second entry") && at("second entry") < at("third entry"));
    assert!(!rows.iter().any(|row| row.contains("info")), "{rows:#?}");
    assert!(rows[at("second entry")].contains("warn  main second entry"));
    assert!(rows[at("third entry")].contains("error main third entry"));
    assert!(rows[at("first entry")].contains("│ user first entry"));
}

#[test]
fn log_continuation_lines_align_with_the_message_text() {
    use crate::domain::event::LogLevel;
    let mut state = state_over(Vec::new(), Some(builder()));
    state.apply(log(
        LogLevel::Warn,
        "main",
        "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu",
    ));
    state.toggle_drawer(Drawer::Logs);
    let rows = draw_rows(&state, 40, 20);
    let first = rows
        .iter()
        .position(|row| row.contains("warn  main alpha"))
        .unwrap();
    let message_col = rows[first][..rows[first].find("alpha").unwrap()]
        .chars()
        .count();
    let next = &rows[first + 1];
    assert!(next.starts_with('│'), "{next}");
    assert_eq!(
        next.chars().skip(1).take_while(|ch| *ch == ' ').count() + 1,
        message_col,
        "{next}"
    );
}

#[test]
fn logs_open_scrolled_to_the_newest_entry() {
    use crate::domain::event::LogLevel;
    let mut state = state_over(Vec::new(), Some(builder()));
    for index in 0..60 {
        state.apply(log(LogLevel::Info, "user", &format!("entry {index:02}")));
    }
    state.toggle_drawer(Drawer::Logs);
    let rows = draw_rows(&state, 80, 20);
    assert!(rows.iter().any(|row| row.contains("entry 59")), "{rows:#?}");
    assert!(
        !rows.iter().any(|row| row.contains("entry 00")),
        "{rows:#?}"
    );

    // Scrolling up from the bottom starts from the real end, not past it.
    state.drawer_scroll_up();
    let rows = draw_rows(&state, 80, 20);
    assert!(rows.iter().any(|row| row.contains("entry 58")), "{rows:#?}");
}

#[test]
fn empty_panels_become_one_line_notices() {
    let mut state = state_over(Vec::new(), Some(builder()));
    state.toggle_drawer(Drawer::Runs);
    assert_eq!(state.drawer(), None);
    // An empty saved-chat list is no news: no panel, no notice.
    state.apply(RuntimeEvent::SavedChats {
        conversations: Vec::new(),
    });
    assert_eq!(state.drawer(), None);
    let notices: Vec<&str> = state
        .chat()
        .iter()
        .filter_map(|item| match item {
            ChatItem::Notice { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(notices[0].starts_with("no runs yet"), "{notices:?}");
}

#[test]
fn inline_drawers_leave_the_chat_visible() {
    let mut state = state_over(
        vec![agent(
            &builder(),
            "Builder",
            BackendKind::Codex,
            "CHAT-STAYS-VISIBLE",
        )],
        Some(builder()),
    );
    state.toggle_drawer(Drawer::Mode);
    let rows = draw_rows(&state, 110, 36);

    let chat = rows
        .iter()
        .position(|row| row.contains("CHAT-STAYS-VISIBLE"))
        .expect("chat visible");
    let title = rows
        .iter()
        .position(|row| row.starts_with("  Mode"))
        .expect("mode title");
    let hint = rows
        .iter()
        .position(|row| row.contains("↑↓ select · Enter fields · Esc close"))
        .expect("mode hint");
    let composer = rows.iter().rposition(|row| row.starts_with('>')).unwrap();
    assert!(chat < title && title < hint && hint < composer, "{rows:#?}");
    // As tall as its content: title, five modes, a blank, the preview, the hint.
    assert_eq!(hint - title, 8, "{rows:#?}");
    assert!(!rows.iter().any(|row| row.contains('╭')), "{rows:#?}");
    // The mode names no longer promise an @target.
    assert!(!rows.iter().any(|row| row.contains("@target")));
    assert!(rows.iter().any(|row| row.contains("you're viewing")));
}

fn team_rows(width: u16, height: u16) -> Vec<String> {
    let mut state = state_over(Vec::new(), Some(builder()));
    state.toggle_drawer(Drawer::Team);
    draw_rows(&state, width, height)
}

#[test]
fn team_editor_has_no_handle_row_target_line_or_ready_noise() {
    let rows = team_rows(110, 36);
    let text = rows.join("\n");
    assert!(text.contains("Team editor"), "{text}");
    for gone in [
        "handle:",
        "(generated)",
        "Default target",
        "t target",
        "* all",
        "Agent CLIs ready",
        "model catalogs were requested",
    ] {
        assert!(!text.contains(gone), "{gone}: {text}");
    }
    // The hints appear once, at the bottom of the box.
    assert_eq!(text.matches("Esc close").count(), 1, "{text}");
    let hint = rows
        .iter()
        .position(|row| row.contains("y copy session · a add · d delete · s apply"))
        .expect("hint");
    assert!(rows[hint + 1].starts_with('╰'), "{text}");
}

#[test]
fn team_editor_hint_fits_a_narrow_box_without_wrapping() {
    for width in [70u16, 56] {
        let rows = team_rows(width, 30);
        let hint = rows
            .iter()
            .position(|row| row.contains("Esc"))
            .unwrap_or_else(|| panic!("hint at {width}: {rows:#?}"));
        // Directly above the bottom border, inside the box, not cut off.
        assert!(rows[hint + 1].starts_with('╰'), "{rows:#?}");
        assert!(rows[hint].trim_end().ends_with('│'), "{}", rows[hint]);
        assert!(rows[hint].contains("a add"), "{}", rows[hint]);
        assert!(rows[hint].contains("s apply"), "{}", rows[hint]);
    }
}

#[test]
fn find_marker_takes_column_zero_without_moving_the_text() {
    let mut row = Line::from(vec![Span::raw("  hello")]);
    mark_find_match(&mut row);
    assert_eq!(plain_text(std::slice::from_ref(&row))[0], "» hello");

    // A band row keeps its background and width.
    let state = state_over(vec![user("needle", &[builder()])], Some(builder()));
    let mut lines = Vec::new();
    render_chat_history(&state, 40, 0, &mut lines);
    let mut top = lines[0].clone();
    mark_find_match(&mut top);
    let text = plain_text(std::slice::from_ref(&top))[0].clone();
    assert!(text.starts_with('»'), "{text:?}");
    assert_eq!(theme::display_width(&text), 40);
}

#[test]
fn the_band_reaches_every_column_of_the_drawn_screen() {
    let state = state_over(vec![user("hello band", &[builder()])], Some(builder()));
    let mut terminal = Terminal::new(TestBackend::new(60, 14)).unwrap();
    terminal
        .draw(|frame| {
            let _ = render(frame, &state);
        })
        .unwrap();
    let buffer = terminal.backend().buffer();
    let rows = rows_of(&terminal);
    let text_row = rows
        .iter()
        .position(|row| row.contains("hello band"))
        .unwrap();
    assert_eq!(rows[text_row].find("hello band"), Some(2));
    for y in [text_row - 1, text_row, text_row + 1] {
        for x in 0..60u16 {
            assert_eq!(
                buffer[(x, y as u16)].bg,
                theme::user_band_bg(),
                "col {x} row {y}"
            );
        }
    }
    // The rows around the band are not painted.
    assert_ne!(
        buffer[(0u16, (text_row + 2) as u16)].bg,
        theme::user_band_bg()
    );
}

#[test]
fn user_markdown_keeps_its_structure_on_the_band() {
    let body = "**bold** and `code` and a [link](https://example.com)\n\n- one\n- two\n\n```rust\nlet x = 1;\n```";
    let state = state_over(vec![user(body, &[builder()])], Some(builder()));
    let mut lines = Vec::new();
    render_chat_history(&state, 70, 0, &mut lines);
    let text = plain_text(&lines);
    let joined = text.join("\n");

    // Markers are consumed, structure survives.
    assert!(
        !joined.contains("**") && !joined.contains("```"),
        "{joined}"
    );
    assert!(joined.contains("bold and code and a link"), "{joined}");
    assert!(
        joined.contains("• one") || joined.contains("- one"),
        "{joined}"
    );
    assert!(joined.contains("let x = 1;"), "{joined}");
    // Every row is still a full-width band row, indented two columns.
    let bg = theme::user_band_bg();
    for (line, plain) in lines.iter().zip(&text) {
        if plain.is_empty() {
            continue;
        }
        assert_eq!(theme::display_width(plain), 70, "{plain:?}");
        assert!(
            line.spans.iter().all(|span| span.style.bg == Some(bg)),
            "{line:?}"
        );
    }
    // Bold stays bold in the band's text colour; inline code keeps its own
    // colour so it stays distinct.
    let span_of = |needle: &str| {
        lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|span| span.content.as_ref() == needle)
            .unwrap_or_else(|| panic!("no span {needle:?}"))
    };
    let bold = span_of("bold");
    assert!(
        bold.style
            .add_modifier
            .contains(ratatui::style::Modifier::BOLD)
    );
    assert_eq!(bold.style.fg, Some(theme::emphasis_color()));
    let code = span_of("code");
    assert_ne!(code.style.fg, Some(theme::emphasis_color()));
    assert_eq!(code.style.bg, Some(bg));
}

#[test]
fn text_stops_at_the_reading_width_while_the_band_spans_the_terminal() {
    let long = "word ".repeat(80);
    let state = state_over(
        vec![
            user(&long, &[builder()]),
            agent(&builder(), "Builder", BackendKind::Codex, &long),
        ],
        Some(builder()),
    );
    let mut lines = Vec::new();
    render_chat_history(&state, 160, 0, &mut lines);
    let text = plain_text(&lines);

    // The band's rows run the full 160 columns, but the text inside wraps by 120.
    let band_rows: Vec<&String> = text
        .iter()
        .filter(|row| row.starts_with("  word"))
        .collect();
    assert!(band_rows.iter().any(|row| theme::display_width(row) == 160));
    for row in text.iter().filter(|row| row.contains("word")) {
        assert!(row.trim_end().chars().count() <= 120, "{row:?}");
    }
    // The agent's reply is held to the same width.
    assert!(text.iter().any(|row| row.trim_end().chars().count() > 100));

    // On a terminal no wider than the cap nothing changes.
    let mut narrow = Vec::new();
    render_chat_history(&state, 100, 0, &mut narrow);
    for row in plain_text(&narrow)
        .iter()
        .filter(|row| row.contains("word"))
    {
        assert!(row.trim_end().chars().count() <= 100, "{row:?}");
    }
}

#[test]
fn panels_above_the_composer_are_set_off_by_one_blank_row() {
    let mut state = state_over(
        vec![agent(&builder(), "Builder", BackendKind::Codex, "CHAT")],
        Some(builder()),
    );
    state.toggle_drawer(Drawer::Mode);
    let rows = draw_rows(&state, 100, 30);
    let title = rows
        .iter()
        .position(|row| row.contains("(this chat:"))
        .unwrap();
    assert!(rows[title - 1].trim().is_empty(), "{rows:#?}");

    // The completion list sits above the composer too, footer still below.
    let mut state = state_over(Vec::new(), Some(builder()));
    state.insert_text("/ap");
    let rows = draw_rows(&state, 80, 20);
    let popup = rows
        .iter()
        .position(|row| row.starts_with("› /approve"))
        .unwrap();
    assert!(rows[popup - 1].trim().is_empty(), "{rows:#?}");
    assert!(rows.iter().position(|row| row.starts_with('>')).unwrap() > popup);
    assert!(rows.last().unwrap().contains("Enter send"), "{rows:#?}");
}

/// The column of the first non-blank character of `row`.
fn first_column(row: &str) -> usize {
    row.chars().take_while(|c| *c == ' ').count()
}

#[test]
fn inline_panel_titles_start_in_the_column_of_their_body() {
    // Mode panel: the title lines up with the mode names after the marker.
    let mut state = state_over(Vec::new(), Some(builder()));
    state.toggle_drawer(Drawer::Mode);
    let rows = draw_rows(&state, 100, 30);
    let title = rows
        .iter()
        .position(|row| row.contains("(this chat:"))
        .unwrap();
    let body = rows
        .iter()
        .position(|row| row.contains("leader Builder"))
        .unwrap();
    let names = rows[body].find("plan").unwrap();
    assert_eq!(rows[title].find("Mode"), Some(2), "{rows:#?}");
    assert_eq!(names, 2, "{rows:#?}");

    // The /model panel, too.
    let mut state = state_over(Vec::new(), Some(builder()));
    state.open_model_picker("");
    let rows = draw_rows(&state, 80, 30);
    let title = rows
        .iter()
        .position(|row| row.contains("Model for"))
        .unwrap();
    assert_eq!(first_column(&rows[title]), 2, "{rows:#?}");
}
