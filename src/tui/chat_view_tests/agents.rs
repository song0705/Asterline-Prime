//! The full-screen Agents list: it replaces the chat, lists agents in status
//! sections with their sub-agents nested, paints the selection as a band, and
//! filters with a typed query.

use super::super::*;
use super::*;
use crate::domain::event::{ConversationSummary, MemberSummary, MessageId, TurnId, UiCommand};
use crate::tui::app_state::{AgentChoice, AgentRow};

fn id(name: &str) -> MemberId {
    MemberId::new(name)
}

fn agent(member: &str, name: &str, backend: BackendKind, text: &str) -> ChatItem {
    ChatItem::Agent {
        member: id(member),
        display_name: name.to_string(),
        backend,
        text: text.to_string(),
    }
}

fn child(name: &str, parent: &str, status: MemberStatus) -> MemberSummary {
    MemberSummary {
        parent: Some(id(parent)),
        ..member_summary(
            &name.to_lowercase(),
            name,
            BackendKind::Claude,
            "sub-agent",
            status,
        )
    }
}

/// Builder (running) with its sub-agent Scout (idle), then idle Reviewer and
/// Planner. Viewing `focus`.
fn roster_state(chat: Vec<ChatItem>, focus: Option<&str>) -> AppState {
    let mut state = AppState::new(chat);
    state.apply(RuntimeEvent::Ready {
        modes: Default::default(),
        mode_overrides: Default::default(),
        suggested_verify: None,
        team: "t".to_string(),
        workspace: String::new(),
        default_target: Some(DefaultTarget::Member(id("builder"))),
        runs: Vec::new(),
        members: vec![
            member_summary(
                "builder",
                "Builder",
                BackendKind::Codex,
                "impl",
                MemberStatus::Running,
            ),
            child("Scout", "builder", MemberStatus::Idle),
            member_summary(
                "reviewer",
                "Reviewer",
                BackendKind::Claude,
                "review",
                MemberStatus::Idle,
            ),
            member_summary(
                "planner",
                "Planner",
                BackendKind::Grok,
                "plan",
                MemberStatus::Idle,
            ),
        ],
    });
    state.set_focus(focus.map(id));
    state
}

fn open(state: &mut AppState) {
    state.toggle_drawer(Drawer::Agents);
}

fn type_query(state: &mut AppState, text: &str) {
    for ch in text.chars() {
        state.agents_query_push(ch);
    }
}

fn terminal_of(state: &AppState, width: u16, height: u16) -> Terminal<TestBackend> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| {
            let layout = render(frame, state);
            assert_eq!(layout.is_none(), state.drawer() == Some(Drawer::Agents));
        })
        .unwrap();
    terminal
}

fn rows_of(terminal: &Terminal<TestBackend>) -> Vec<String> {
    let buffer = terminal.backend().buffer();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect::<String>()
        })
        .collect()
}

fn draw_rows(state: &AppState, width: u16, height: u16) -> Vec<String> {
    rows_of(&terminal_of(state, width, height))
}

fn row_with<'a>(rows: &'a [String], needle: &str) -> &'a String {
    rows.iter()
        .find(|row| row.contains(needle))
        .unwrap_or_else(|| panic!("no row with {needle:?}: {rows:#?}"))
}

/// Display column of `needle` within `row`.
fn column_of(row: &str, needle: &str) -> usize {
    let byte = row
        .find(needle)
        .unwrap_or_else(|| panic!("{needle} in {row}"));
    theme::display_width(&row[..byte])
}

#[test]
fn the_list_takes_over_the_whole_frame() {
    let mut state = roster_state(
        vec![agent(
            "reviewer",
            "Reviewer",
            BackendKind::Claude,
            "CHAT-GONE",
        )],
        Some("builder"),
    );
    open(&mut state);

    let rows = draw_rows(&state, 100, 30);

    assert!(
        rows.iter().all(|row| !row.contains("CHAT-GONE")),
        "{rows:#?}"
    );
    // No composer, chat footer or member bar.
    assert!(rows.iter().all(|row| !row.starts_with('>')), "{rows:#?}");
    assert!(
        rows.iter().all(|row| !row.contains("Enter send")),
        "{rows:#?}"
    );
    assert!(
        rows.iter().all(|row| !row.contains("← agents")),
        "{rows:#?}"
    );
    assert!(rows.iter().all(|row| !row.contains('●')), "{rows:#?}");
    assert_eq!(
        rows[0].trim_end(),
        "  Agents · 1 running, 2 idle, 0 inactive"
    );
    assert!(rows[1].trim().is_empty());
    assert_eq!(rows[2].trim_end(), "  > Search agents");
    assert!(rows[3].trim().is_empty());
    let legend: Vec<&str> = rows[4].split_whitespace().collect();
    assert_eq!(legend, ["Agent", "Model", "Activity"], "{}", rows[4]);
    assert!(rows[5].trim().is_empty());
    assert!(rows[6].contains("All messages"), "{rows:#?}");
}

#[test]
fn the_search_caret_sits_after_the_typed_query() {
    let mut state = roster_state(Vec::new(), Some("builder"));
    open(&mut state);
    type_query(&mut state, "rev");
    let mut terminal = terminal_of(&state, 100, 30);

    terminal.backend_mut().assert_cursor_position((7, 2));
    assert_eq!(rows_of(&terminal)[2].trim_end(), "  > rev");
}

#[test]
fn agents_are_listed_in_counted_sections_with_all_messages_first() {
    let mut state = roster_state(Vec::new(), Some("builder"));
    open(&mut state);

    let rows = draw_rows(&state, 100, 30);

    let position = |needle: &str| {
        rows.iter()
            .position(|row| row.contains(needle))
            .unwrap_or_else(|| panic!("no {needle}: {rows:#?}"))
    };
    let all = position("All messages");
    let running = position("Running (1)");
    let idle = position("Idle (2)");
    assert!(all < running && running < idle, "{rows:#?}");
    // All messages sits in no section: a blank row follows it, and one
    // separates the sections.
    assert!(rows[all + 1].trim().is_empty(), "{rows:#?}");
    assert_eq!(running, all + 2, "{rows:#?}");
    assert!(rows[idle - 1].trim().is_empty(), "{rows:#?}");
    // The sections hold their members; the idle sub-agent stays under its
    // running parent.
    let (builder, scout) = (position("Builder"), position("Scout"));
    let (reviewer, planner) = (position("Reviewer"), position("Planner"));
    assert!(
        running < builder && builder < scout && scout < idle,
        "{rows:#?}"
    );
    assert!(idle < reviewer && reviewer < planner, "{rows:#?}");
    // Section headings are muted, rows are not.
    let terminal = terminal_of(&state, 100, 30);
    let buffer = terminal.backend().buffer();
    assert_eq!(buffer[(2u16, running as u16)].style().fg, theme::muted().fg);
}

#[test]
fn rows_show_the_pulse_model_and_activity() {
    let mut state = roster_state(
        vec![agent("reviewer", "Reviewer", BackendKind::Claude, "done")],
        Some("builder"),
    );
    open(&mut state);
    let terminal = terminal_of(&state, 110, 30);
    let rows = rows_of(&terminal);
    let buffer = terminal.backend().buffer();

    let builder = row_with(&rows, "Builder");
    let y = rows.iter().position(|row| row == builder).unwrap() as u16;
    let icon = buffer[(2u16, y)].symbol();
    assert!(["◇", "◈", "◆"].contains(&icon), "{builder}");
    assert!(
        buffer[(2u16, y)]
            .style()
            .add_modifier
            .contains(ratatui::style::Modifier::BOLD),
        "{builder}"
    );
    assert!(builder.contains("codex"), "{builder}");
    assert!(builder.contains("working"), "{builder}");

    // Idle agents get the still bullet in the dim colour.
    let planner = row_with(&rows, "Planner");
    let y = rows.iter().position(|row| row == planner).unwrap() as u16;
    assert_eq!(buffer[(2u16, y)].symbol(), "•");
    assert_eq!(buffer[(2u16, y)].style().fg, Some(theme::dim_color()));
    assert!(planner.contains("grok"), "{planner}");
}

#[test]
fn the_idle_activity_is_the_age_of_the_last_reply() {
    let mut state = roster_state(Vec::new(), Some("builder"));
    state.apply(RuntimeEvent::MessageStarted {
        msg: MessageId(1),
        turn: TurnId(1),
        member: id("reviewer"),
    });
    state.apply(RuntimeEvent::MessageCompleted {
        msg: MessageId(1),
        text: "all good".to_string(),
    });
    open(&mut state);

    let rows = draw_rows(&state, 110, 30);

    assert!(row_with(&rows, "Reviewer").contains("0s"), "{rows:#?}");
    assert!(!row_with(&rows, "Reviewer").contains("ago"), "{rows:#?}");
    // No reply yet: the cell stays blank.
    assert!(!row_with(&rows, "Planner").contains("0s"), "{rows:#?}");
}

#[test]
fn the_selected_row_is_a_full_width_band_without_a_marker() {
    let mut state = roster_state(Vec::new(), Some("reviewer"));
    open(&mut state);
    let terminal = terminal_of(&state, 100, 30);
    let rows = rows_of(&terminal);
    let buffer = terminal.backend().buffer();

    assert!(rows.iter().all(|row| !row.contains('›')), "{rows:#?}");
    let selected = rows
        .iter()
        .position(|row| row.contains("Reviewer"))
        .unwrap() as u16;
    for x in 0..100u16 {
        assert_eq!(
            buffer[(x, selected)].style().bg,
            Some(theme::selection_bg()),
            "column {x}"
        );
    }
    let other = rows.iter().position(|row| row.contains("Planner")).unwrap() as u16;
    assert_ne!(
        buffer[(50u16, other)].style().bg,
        Some(theme::selection_bg())
    );
    assert_ne!(theme::selection_bg(), theme::user_band_bg());
    // Only that one row is banded.
    let banded = (0..30u16)
        .filter(|&y| buffer[(0u16, y)].style().bg == Some(theme::selection_bg()))
        .count();
    assert_eq!(banded, 1);
}

#[test]
fn only_the_selected_row_shows_its_last_message() {
    let chat = vec![
        agent("builder", "Builder", BackendKind::Codex, "BUILDER-REPLY"),
        agent(
            "reviewer",
            "Reviewer",
            BackendKind::Claude,
            "REVIEWER-REPLY",
        ),
        agent("planner", "Planner", BackendKind::Grok, "PLANNER-REPLY"),
    ];
    let mut state = roster_state(chat, Some("reviewer"));
    open(&mut state);

    let rows = draw_rows(&state, 100, 30);

    assert!(
        rows.iter().any(|row| row.contains("REVIEWER-REPLY")),
        "{rows:#?}"
    );
    assert!(
        rows.iter().all(|row| !row.contains("BUILDER-REPLY")),
        "{rows:#?}"
    );
    assert!(
        rows.iter().all(|row| !row.contains("PLANNER-REPLY")),
        "{rows:#?}"
    );
    // The preview sits right under the row, indented to the name.
    let at = rows
        .iter()
        .position(|row| row.contains("Reviewer"))
        .unwrap();
    assert_eq!(rows[at + 1].trim_end(), "    REVIEWER-REPLY");

    state.select_next_agent();
    let rows = draw_rows(&state, 100, 30);
    assert!(
        rows.iter().any(|row| row.contains("PLANNER-REPLY")),
        "{rows:#?}"
    );
    assert!(
        rows.iter().all(|row| !row.contains("REVIEWER-REPLY")),
        "{rows:#?}"
    );
}

#[test]
fn a_long_preview_wraps_to_three_lines_and_ends_with_an_ellipsis() {
    let long = "alpha bravo charlie delta echo foxtrot golf hotel india juliet kilo lima mike november oscar papa quebec romeo sierra tango";
    let mut state = roster_state(
        vec![agent("reviewer", "Reviewer", BackendKind::Claude, long)],
        Some("reviewer"),
    );
    open(&mut state);

    let rows = draw_rows(&state, 40, 30);

    let at = rows
        .iter()
        .position(|row| row.contains("Reviewer"))
        .unwrap();
    let preview: Vec<&String> = rows[at + 1..at + 4].iter().collect();
    // Exactly three lines: the next row is already the next agent.
    assert!(rows[at + 4].contains("Planner"), "{rows:#?}");
    assert!(preview[2].trim_end().ends_with('…'), "{preview:#?}");
    assert!(!preview[0].contains('…') && !preview[1].contains('…'));
    assert!(preview.iter().all(|row| row.starts_with("    ")));
    // Agents without a reply show nothing under their row.
    state.select_next_agent();
    let rows = draw_rows(&state, 40, 30);
    assert!(
        rows.iter().all(|row| !row.contains("no replies yet")),
        "{rows:#?}"
    );
}

#[test]
fn sub_agents_are_indented_inside_the_name_column() {
    let mut state = roster_state(Vec::new(), Some("builder"));
    open(&mut state);

    let rows = draw_rows(&state, 110, 30);

    let builder = row_with(&rows, "Builder");
    let scout = row_with(&rows, "Scout");
    // The icon moves right by one nesting level (4 columns); the name follows.
    assert_eq!(column_of(builder, "Builder"), 4);
    assert_eq!(column_of(scout, "Scout"), 8);
    // Model and activity columns line up with the top-level rows.
    let reviewer = row_with(&rows, "Reviewer");
    assert_eq!(column_of(builder, "codex"), column_of(scout, "claude"));
    assert_eq!(column_of(reviewer, "claude"), column_of(scout, "claude"));
    let legend = &rows[4];
    assert_eq!(column_of(legend, "Model"), column_of(scout, "claude"));
    assert!(column_of(legend, "Activity") > column_of(legend, "Model"));
}

#[test]
fn typing_filters_to_a_flat_ranked_list_without_sections() {
    let mut state = roster_state(Vec::new(), Some("builder"));
    open(&mut state);
    type_query(&mut state, "rev");

    let rows = draw_rows(&state, 100, 30);

    assert_eq!(rows[2].trim_end(), "  > rev");
    assert!(
        rows.iter().all(|row| !row.contains("Running (")),
        "{rows:#?}"
    );
    assert!(rows.iter().all(|row| !row.contains("Idle (")), "{rows:#?}");
    assert!(
        rows.iter().all(|row| !row.contains("All messages")),
        "{rows:#?}"
    );
    assert!(rows.iter().all(|row| !row.contains("Builder")), "{rows:#?}");
    assert!(rows.iter().all(|row| !row.contains("Planner")), "{rows:#?}");
    assert!(rows[6].contains("Reviewer"), "{rows:#?}");
    // The counts line still describes the whole roster.
    assert_eq!(
        rows[0].trim_end(),
        "  Agents · 1 running, 2 idle, 0 inactive"
    );

    // Backend and fuzzy matches.
    state.agents_query_clear();
    type_query(&mut state, "grok");
    assert!(draw_rows(&state, 100, 30)[6].contains("Planner"));
    state.agents_query_clear();
    type_query(&mut state, "rvw");
    assert!(draw_rows(&state, 100, 30)[6].contains("Reviewer"));
}

#[test]
fn better_matches_rank_first() {
    let mut state = roster_state(Vec::new(), Some("builder"));
    open(&mut state);
    // "claude" matches Scout and Reviewer by backend; "r" prefixes Reviewer
    // and Planner contains it later: the prefix wins.
    type_query(&mut state, "r");

    let rows = draw_rows(&state, 100, 30);

    let order = |needle: &str| rows.iter().position(|row| row.contains(needle)).unwrap();
    assert!(order("Reviewer") < order("Builder"), "{rows:#?}");
}

#[test]
fn a_query_without_matches_says_so() {
    let mut state = roster_state(Vec::new(), Some("builder"));
    open(&mut state);
    type_query(&mut state, "zzz");

    let rows = draw_rows(&state, 100, 30);

    assert_eq!(rows[6].trim_end(), "  No agents match your search.");
    assert!(
        rows.iter().all(|row| !row.contains("Reviewer")),
        "{rows:#?}"
    );
    // The hints stay at the bottom.
    assert!(rows[29].contains("Esc close"));
}

#[test]
fn the_hints_fill_the_last_row() {
    let mut state = roster_state(Vec::new(), Some("builder"));
    open(&mut state);
    let terminal = terminal_of(&state, 100, 30);
    let rows = rows_of(&terminal);

    assert_eq!(
        rows[29].trim_end(),
        "  ↑/↓ navigate   Home/End first/last   Enter/→ open   Ctrl+N new   Esc close"
    );
    assert_eq!(
        terminal.backend().buffer()[(2u16, 29u16)].style().fg,
        theme::muted().fg
    );
    assert!(rows[..29].iter().all(|row| !row.contains("navigate")));
}

#[test]
fn a_tall_list_keeps_the_selection_near_the_middle_with_clip_markers() {
    let members: Vec<_> = (0..20)
        .map(|index| {
            member_summary(
                &format!("m{index}"),
                &format!("Member{index:02}"),
                BackendKind::Codex,
                "role",
                MemberStatus::Idle,
            )
        })
        .collect();
    let mut state = AppState::new(Vec::new());
    state.apply(RuntimeEvent::Ready {
        modes: Default::default(),
        mode_overrides: Default::default(),
        suggested_verify: None,
        team: "t".to_string(),
        workspace: String::new(),
        default_target: None,
        runs: Vec::new(),
        members,
    });
    open(&mut state);

    // At the top only the bottom is clipped.
    let rows = draw_rows(&state, 100, 20);
    assert!(
        rows.iter().any(|row| row.contains("All messages")),
        "{rows:#?}"
    );
    assert_eq!(
        rows.iter().filter(|row| row.trim() == "...").count(),
        1,
        "{rows:#?}"
    );
    assert_eq!(rows[18].trim(), "...", "{rows:#?}");

    for _ in 0..11 {
        state.select_next_agent();
    }
    let rows = draw_rows(&state, 100, 20);
    let at = rows
        .iter()
        .position(|row| row.contains("Member10"))
        .unwrap();
    // Rows 6..19 hold the list; the selection is roughly centred.
    assert!((10..=14).contains(&at), "{at} {rows:#?}");
    assert_eq!(rows[6].trim(), "...", "{rows:#?}");
    assert_eq!(rows[18].trim(), "...", "{rows:#?}");
    assert!(
        rows.iter().all(|row| !row.contains("All messages")),
        "{rows:#?}"
    );

    // At the end only the top is clipped.
    state.select_last_agent();
    let rows = draw_rows(&state, 100, 20);
    assert!(rows.iter().any(|row| row.contains("Member19")), "{rows:#?}");
    assert_eq!(
        rows.iter().filter(|row| row.trim() == "...").count(),
        1,
        "{rows:#?}"
    );
    assert_eq!(rows[6].trim(), "...", "{rows:#?}");
}

#[test]
fn clicks_find_their_rows_and_nothing_is_marked_as_viewing() {
    let mut state = roster_state(Vec::new(), None);
    open(&mut state);
    let rows = draw_rows(&state, 100, 30);
    assert!(
        rows.iter().all(|row| !row.contains("(viewing)")),
        "{rows:#?}"
    );

    // The painted rows are clickable.
    let at = rows
        .iter()
        .position(|row| row.contains("Reviewer"))
        .unwrap();
    assert!(!state.click_agent_row(0));
    assert!(state.click_agent_row(at as u16));
    assert_eq!(state.focus(), Some(&id("reviewer")));
    assert_eq!(state.drawer(), None);
}

#[test]
fn closing_the_list_brings_the_chat_back() {
    let mut state = roster_state(
        vec![agent("builder", "Builder", BackendKind::Codex, "CHAT-BACK")],
        Some("builder"),
    );
    open(&mut state);
    state.close_drawer();

    let rows = draw_rows(&state, 100, 30);

    assert!(
        rows.iter().any(|row| row.contains("CHAT-BACK")),
        "{rows:#?}"
    );
    assert!(
        rows.iter().all(|row| !row.contains("Search agents")),
        "{rows:#?}"
    );
}

#[test]
fn a_tiny_terminal_still_paints_the_hints() {
    let mut state = roster_state(Vec::new(), Some("builder"));
    open(&mut state);

    let rows = draw_rows(&state, 30, 8);

    assert!(rows[7].contains("↑/↓ navigate"), "{rows:#?}");
    let rows = draw_rows(&state, 12, 3);
    assert_eq!(rows.len(), 3);
}

fn user(body: &str, targets: &[&str]) -> ChatItem {
    ChatItem::User {
        body: body.to_string(),
        targets: targets.iter().map(|name| id(name)).collect(),
        interrupted: Vec::new(),
    }
}

/// The main member with its sub-agent Helper, and a /team member Reviewer,
/// in the workspace `/work/ttl-project`.
fn main_state(chat: Vec<ChatItem>) -> AppState {
    let mut state = AppState::new(chat);
    state.apply(RuntimeEvent::Ready {
        modes: Default::default(),
        mode_overrides: Default::default(),
        suggested_verify: None,
        team: "t".to_string(),
        workspace: "/work/ttl-project".to_string(),
        default_target: Some(DefaultTarget::Member(id("main"))),
        runs: Vec::new(),
        members: vec![
            member_summary(
                "main",
                "Main",
                BackendKind::Codex,
                "general",
                MemberStatus::Idle,
            ),
            child("Helper", "main", MemberStatus::Idle),
            member_summary(
                "reviewer",
                "Reviewer",
                BackendKind::Claude,
                "review",
                MemberStatus::Idle,
            ),
        ],
    });
    open(&mut state);
    state
}

#[test]
fn the_main_row_is_titled_by_the_first_message_sent_to_it() {
    let state = main_state(vec![
        user("review the diff", &["reviewer"]),
        user("fix the parser bug in src/router", &["main"]),
        user("then add a test", &["main"]),
    ]);

    let rows = draw_rows(&state, 110, 30);

    let main = row_with(&rows, "fix the parser bug");
    assert!(main.contains("codex"), "{main}");
    assert!(rows.iter().all(|row| !row.contains("Main")), "{rows:#?}");
    assert!(rows.iter().all(|row| !row.contains("review the diff")));
}

#[test]
fn whitespace_in_the_main_title_is_collapsed() {
    let state = main_state(vec![user("  fix\n\n the \t parser   bug  ", &["main"])]);

    let rows = draw_rows(&state, 110, 30);

    assert!(rows.iter().any(|row| row.contains("fix the parser bug")));
}

#[test]
fn without_messages_the_main_row_falls_back_to_the_workspace_folder() {
    let state = main_state(vec![user("only for the reviewer", &["reviewer"])]);

    let rows = draw_rows(&state, 110, 30);

    let main = row_with(&rows, "ttl-project");
    assert!(main.contains("codex"), "{main}");
    assert!(rows.iter().all(|row| !row.contains("only for the")));
}

#[test]
fn a_long_main_title_is_clipped_without_moving_the_other_columns() {
    let long = "fix the parser bug in src/router and also rewrite the whole thing";
    let state = main_state(vec![user(long, &["main"])]);

    let rows = draw_rows(&state, 110, 30);

    let main = row_with(&rows, "fix the parser");
    assert!(main.contains('…'), "{main}");
    assert!(!main.contains("whole thing"));
    // The name column is capped, so the other columns are not squeezed.
    assert!(column_of(main, "codex") <= 2 + 28 + 2, "{main}");
    let reviewer = row_with(&rows, "Reviewer");
    assert_eq!(column_of(main, "codex"), column_of(reviewer, "claude"));
    let legend = &rows[4];
    assert_eq!(column_of(legend, "Model"), column_of(main, "codex"));
    // A narrow terminal still keeps the columns aligned.
    let narrow = draw_rows(&state, 60, 30);
    let main = row_with(&narrow, "fix the");
    let reviewer = row_with(&narrow, "Reviewer");
    assert_eq!(column_of(main, "codex"), column_of(reviewer, "claude"));
}

#[test]
fn sub_agents_and_team_members_keep_their_names() {
    let state = main_state(vec![user("fix the parser bug", &["main"])]);

    let rows = draw_rows(&state, 110, 30);

    assert!(rows.iter().any(|row| row.contains("Helper")), "{rows:#?}");
    assert!(rows.iter().any(|row| row.contains("Reviewer")), "{rows:#?}");
}

#[test]
fn search_finds_the_main_row_by_its_id_and_by_title_words() {
    let mut state = main_state(vec![user("fix the parser bug in src/router", &["main"])]);

    type_query(&mut state, "main");
    let rows = draw_rows(&state, 110, 30);
    assert!(rows[6].contains("fix the parser bug"), "{rows:#?}");

    state.agents_query_clear();
    type_query(&mut state, "parser router");
    let rows = draw_rows(&state, 110, 30);
    assert!(rows[6].contains("fix the parser bug"), "{rows:#?}");
    assert!(rows.iter().all(|row| !row.contains("Reviewer")));
}

/// Glyph and style of the status icon two columns before `name` in its row.
fn icon_of(
    terminal: &Terminal<TestBackend>,
    name: &str,
    y_hint: &str,
) -> (String, ratatui::style::Style) {
    let rows = rows_of(terminal);
    let y = rows
        .iter()
        .position(|row| row.contains(name) && row.contains(y_hint))
        .unwrap_or_else(|| panic!("no row with {name}: {rows:#?}"));
    let x = column_of(&rows[y], name) - 2;
    let cell = &terminal.backend().buffer()[(x as u16, y as u16)];
    (cell.symbol().to_string(), cell.style())
}

#[test]
fn a_running_agent_pulses_lit_and_an_idle_one_is_a_dark_dot() {
    let mut state = roster_state(Vec::new(), Some("builder"));
    open(&mut state);
    let terminal = terminal_of(&state, 110, 30);

    let (glyph, lit) = icon_of(&terminal, "Builder", "codex");
    assert!(["◇", "◈", "◆"].contains(&glyph.as_str()), "{glyph}");
    assert_eq!(lit.fg, theme::emphasis().fg);
    assert!(lit.add_modifier.contains(ratatui::style::Modifier::BOLD));

    for name in ["Reviewer", "Planner", "Scout"] {
        let (glyph, dark) = icon_of(&terminal, name, "");
        assert_eq!(glyph, "•", "{name}");
        assert!(!dark.add_modifier.contains(ratatui::style::Modifier::BOLD));
        assert_eq!(dark.fg, theme::dim().fg, "{name}");
    }
}

#[test]
fn a_failed_agent_keeps_its_red_dot() {
    let mut state = roster_state(Vec::new(), Some("builder"));
    state.apply(RuntimeEvent::MemberStatus {
        member: id("reviewer"),
        status: MemberStatus::Failed,
    });
    open(&mut state);
    let terminal = terminal_of(&state, 110, 30);

    let (glyph, failed) = icon_of(&terminal, "Reviewer", "");
    assert_eq!(glyph, "•");
    assert_eq!(failed.fg, theme::error().fg);
}

fn saved(id: i64, updated_at: &str, title: &str, backend: BackendKind) -> ConversationSummary {
    ConversationSummary {
        id,
        created_at: updated_at.to_string(),
        updated_at: updated_at.to_string(),
        preview: title.to_string(),
        name: None,
        message_count: 2,
        member_count: 1,
        main_backend: Some(backend),
        main_model: None,
        main_effort: None,
    }
}

/// 2026-03-10 12:00:00 UTC.
const NOW: i64 = 1_773_144_000;

/// The roster with three saved chats, newest first, and a fixed clock.
fn saved_state() -> AppState {
    let mut state = roster_state(Vec::new(), Some("builder"));
    state.set_clock_unix_secs(NOW);
    state.apply(RuntimeEvent::SavedChats {
        conversations: vec![
            saved(
                9,
                "2026-03-10 11:59:15",
                "fresh   question",
                BackendKind::Claude,
            ),
            saved(
                5,
                "2026-03-10 09:00:00",
                "an older saved chat",
                BackendKind::Codex,
            ),
            saved(
                2,
                "2026-03-08 12:00:00",
                "ancient history",
                BackendKind::Grok,
            ),
        ],
    });
    state
}

#[test]
fn saved_chats_fill_a_counted_inactive_section_after_idle() {
    let mut state = saved_state();
    open(&mut state);

    let rows = draw_rows(&state, 100, 30);

    assert!(
        row_with(&rows, "Agents ·").contains("Agents · 1 running, 2 idle, 3 inactive"),
        "{rows:#?}"
    );
    let position = |needle: &str| rows.iter().position(|row| row.contains(needle)).unwrap();
    let (idle, planner, inactive) = (
        position("Idle (2)"),
        position("Planner"),
        position("Inactive (3)"),
    );
    assert!(idle < planner && planner < inactive, "{rows:#?}");
    assert!(rows[inactive - 1].trim().is_empty(), "{rows:#?}");
    // Newest first, titles collapsed to one line, models from the saved roster.
    let fresh = position("fresh question");
    assert_eq!(fresh, inactive + 1, "{rows:#?}");
    assert_eq!(position("an older saved chat"), inactive + 2);
    assert_eq!(position("ancient history"), inactive + 3);
    assert!(rows[fresh].contains("claude"), "{rows:#?}");
    assert!(rows[inactive + 3].contains("grok"), "{rows:#?}");
    // Activity is the age of the newest message.
    assert!(rows[fresh].trim_end().ends_with("45s"), "{rows:#?}");
    assert!(rows[inactive + 2].trim_end().ends_with("3h"), "{rows:#?}");
    assert!(rows[inactive + 3].trim_end().ends_with("2d"), "{rows:#?}");
}

#[test]
fn an_inactive_row_has_a_dark_dot_and_no_preview() {
    let mut state = saved_state();
    open(&mut state);
    state.select_last_agent();
    let terminal = terminal_of(&state, 110, 30);

    let (glyph, dark) = icon_of(&terminal, "ancient history", "");
    assert_eq!(glyph, "•");
    assert_eq!(dark.fg, theme::dim().fg);
    assert!(!dark.add_modifier.contains(ratatui::style::Modifier::BOLD));
    // Nothing is printed under the selected saved chat.
    let rows = rows_of(&terminal);
    let at = rows
        .iter()
        .position(|row| row.contains("ancient history"))
        .unwrap();
    assert!(rows[at + 1].trim().is_empty(), "{rows:#?}");
}

#[test]
fn without_saved_chats_there_is_no_inactive_section_but_the_count_stays() {
    let mut state = roster_state(Vec::new(), Some("builder"));
    open(&mut state);

    let rows = draw_rows(&state, 100, 30);

    assert!(
        rows.iter().all(|row| !row.contains("Inactive")),
        "{rows:#?}"
    );
    assert!(
        row_with(&rows, "Agents ·").contains("1 running, 2 idle, 0 inactive"),
        "{rows:#?}"
    );
}

#[test]
fn searching_matches_saved_titles_and_models_in_the_flat_list() {
    let mut state = saved_state();
    open(&mut state);

    type_query(&mut state, "older");
    let rows = draw_rows(&state, 100, 30);
    assert!(
        rows.iter().all(|row| !row.contains("Inactive (")),
        "{rows:#?}"
    );
    assert!(rows.iter().any(|row| row.contains("an older saved chat")));
    assert!(rows.iter().all(|row| !row.contains("ancient history")));
    assert_eq!(
        state.selected_agent(),
        Some(crate::tui::app_state::AgentChoice::Saved(5))
    );

    state.agents_query_clear();
    type_query(&mut state, "grok");
    let kinds = state.agents_rows();
    assert!(kinds.contains(&AgentRow::Saved { index: 2 }), "{kinds:?}");
    assert!(
        kinds
            .iter()
            .any(|row| matches!(row, AgentRow::Member { .. })),
        "the Grok member matches too: {kinds:?}"
    );
}

#[test]
fn enter_on_an_inactive_row_queues_its_resume_and_closes_the_list() {
    let mut state = saved_state();
    open(&mut state);
    state.select_last_agent();
    state.select_previous_agent();

    assert!(state.apply_selected_agent());

    assert_eq!(state.drawer(), None);
    assert_eq!(
        state.take_pending_command(),
        Some(UiCommand::ResumeConversation { conversation: 5 })
    );
    assert_eq!(state.take_pending_command(), None);
}

#[test]
fn clicking_an_inactive_row_resumes_it() {
    let mut state = saved_state();
    open(&mut state);
    let rows = draw_rows(&state, 100, 30);
    let at = rows
        .iter()
        .position(|row| row.contains("fresh question"))
        .unwrap();

    assert!(state.click_agent_row(at as u16));

    assert_eq!(
        state.take_pending_command(),
        Some(UiCommand::ResumeConversation { conversation: 9 })
    );
    assert_eq!(state.drawer(), None);
}

#[test]
fn resume_opens_on_the_first_inactive_row_even_when_the_list_arrives_late() {
    let mut state = roster_state(Vec::new(), Some("builder"));
    state.set_clock_unix_secs(NOW);
    state.open_agents_on_saved_chats();
    // Nothing saved yet: the highlight stays on the viewed agent.
    assert_eq!(
        state.selected_agent(),
        Some(crate::tui::app_state::AgentChoice::Member(id("builder")))
    );

    state.apply(RuntimeEvent::SavedChats {
        conversations: vec![saved(4, "2026-03-10 11:00:00", "late", BackendKind::Codex)],
    });
    assert_eq!(
        state.selected_agent(),
        Some(crate::tui::app_state::AgentChoice::Saved(4))
    );
}

#[test]
fn a_late_saved_list_does_not_steal_a_highlight_the_user_moved() {
    let mut state = roster_state(Vec::new(), Some("builder"));
    state.open_agents_on_saved_chats();
    state.select_next_agent();
    let moved = state.selected_agent();

    state.apply(RuntimeEvent::SavedChats {
        conversations: vec![saved(4, "2026-03-10 11:00:00", "late", BackendKind::Codex)],
    });

    assert_eq!(state.selected_agent(), moved);
}

#[test]
fn an_ordinary_open_keeps_the_viewed_agent_highlighted_when_chats_arrive() {
    let mut state = roster_state(Vec::new(), Some("reviewer"));
    open(&mut state);
    state.apply(RuntimeEvent::SavedChats {
        conversations: vec![saved(4, "2026-03-10 11:00:00", "late", BackendKind::Codex)],
    });
    assert_eq!(
        state.selected_agent(),
        Some(crate::tui::app_state::AgentChoice::Member(id("reviewer")))
    );
}

#[test]
fn the_selection_follows_a_saved_chat_when_the_list_is_refreshed() {
    let mut state = saved_state();
    open(&mut state);
    state.select_last_agent();
    assert_eq!(
        state.selected_agent(),
        Some(crate::tui::app_state::AgentChoice::Saved(2))
    );

    state.apply(RuntimeEvent::SavedChats {
        conversations: vec![
            saved(11, "2026-03-10 11:59:59", "newest", BackendKind::Codex),
            saved(
                2,
                "2026-03-08 12:00:00",
                "ancient history",
                BackendKind::Grok,
            ),
        ],
    });

    assert_eq!(
        state.selected_agent(),
        Some(crate::tui::app_state::AgentChoice::Saved(2))
    );
}

#[test]
fn a_new_or_resumed_chat_forgets_the_saved_list() {
    let mut state = saved_state();
    state.apply(RuntimeEvent::SessionReset);
    assert!(state.saved_chats().is_empty());

    let mut state = saved_state();
    state.apply(RuntimeEvent::ConversationResumed {
        conversation: 9,
        chat: Vec::new(),
    });
    assert!(state.saved_chats().is_empty());
}

#[test]
fn relative_ages_use_the_utc_clock_and_prime_buckets() {
    use crate::tui::agents_view::relative_age;
    let at = |stamp: &str, now: i64| relative_age(stamp, now);
    // 2025-01-01 00:00:00 UTC is 1_735_689_600.
    let base = 1_735_689_600;
    assert_eq!(at("2025-01-01 00:00:00", base), "0s");
    assert_eq!(at("2025-01-01 00:00:00", base + 45), "45s");
    assert_eq!(at("2025-01-01 00:00:00", base + 59), "59s");
    assert_eq!(at("2025-01-01 00:00:00", base + 60), "1m");
    assert_eq!(at("2025-01-01 00:00:00", base + 12 * 60 + 5), "12m");
    assert_eq!(at("2025-01-01 00:00:00", base + 3599), "59m");
    assert_eq!(at("2025-01-01 00:00:00", base + 3600), "1h");
    assert_eq!(at("2025-01-01 00:00:00", base + 3 * 3600 + 1800), "3h");
    assert_eq!(at("2025-01-01 00:00:00", base + 86_399), "23h");
    assert_eq!(at("2025-01-01 00:00:00", base + 86_400), "1d");
    assert_eq!(at("2025-01-01 00:00:00", base + 2 * 86_400 + 5), "2d");
    // Leap day, ISO spelling, and a stamp in the future.
    assert_eq!(at("2024-02-29T23:59:59.500Z", 1_709_251_199 + 7), "7s");
    assert_eq!(at("2025-01-01 00:00:10", base), "0s");
    // Unreadable stamps show nothing.
    assert_eq!(at("", base), "");
    assert_eq!(at("not a time", base), "");
    assert_eq!(at("2025-13-01 00:00:00", base), "");
}

fn last_row(state: &AppState) -> String {
    draw_rows(state, 110, 30)[29].trim_end().to_string()
}

fn named(mut chat: ConversationSummary, name: &str) -> ConversationSummary {
    chat.name = Some(name.to_string());
    chat
}

#[test]
fn an_inactive_row_shows_the_delete_and_rename_keys_in_the_hints() {
    let mut state = saved_state();
    open(&mut state);
    // A live member: neither key applies.
    assert!(!state.agents_can_rename() && !state.agents_can_delete());
    assert_eq!(
        last_row(&state),
        "  ↑/↓ navigate   Home/End first/last   Enter/→ open   Ctrl+N new   Esc close"
    );

    state.select_last_agent();
    assert_eq!(
        last_row(&state),
        "  ↑/↓ navigate   Home/End first/last   Enter/→ open   Ctrl+R rename   Ctrl+X delete   Ctrl+N new   Esc close"
    );

    // With a search typed the keys are off again.
    type_query(&mut state, "ancient");
    assert_eq!(
        state.selected_agent().map(|c| c == AgentChoice::Saved(2)),
        Some(true)
    );
    assert!(!state.agents_can_rename() && !state.agents_can_delete());
    assert_eq!(
        last_row(&state),
        "  ↑/↓ navigate   Home/End first/last   Enter/→ open   Ctrl+N new   Esc close"
    );
}

#[test]
fn the_main_row_can_be_renamed_but_not_deleted() {
    let mut state = main_state(vec![user("fix the parser bug", &["main"])]);
    state.select_first_agent();
    state.select_next_agent();
    assert_eq!(
        state.selected_agent(),
        Some(AgentChoice::Member(id("main")))
    );

    assert!(state.agents_can_rename());
    assert!(!state.agents_can_delete());
    assert_eq!(
        last_row(&state),
        "  ↑/↓ navigate   Home/End first/last   Enter/→ open   Ctrl+R rename   Ctrl+N new   Esc close"
    );
    // Not a sub-agent and not another member.
    state.select_next_agent();
    assert_eq!(
        state.selected_agent(),
        Some(AgentChoice::Member(id("helper")))
    );
    assert!(!state.agents_can_rename());
}

#[test]
fn ctrl_x_arms_and_a_second_press_on_the_same_row_deletes() {
    let mut state = saved_state();
    open(&mut state);
    state.select_last_agent();
    state.select_previous_agent();
    assert_eq!(state.selected_agent(), Some(AgentChoice::Saved(5)));

    state.press_agents_delete();
    assert!(state.agents_delete_armed());
    assert_eq!(state.take_pending_command(), None);
    assert_eq!(last_row(&state), "  Press Ctrl+X again to delete");

    state.press_agents_delete();
    assert_eq!(
        state.take_pending_command(),
        Some(UiCommand::DeleteConversation { conversation: 5 })
    );
    assert!(!state.agents_delete_armed());
    // The list stays open for the refreshed Inactive rows.
    assert_eq!(state.drawer(), Some(Drawer::Agents));

    // Once the runtime resends the list without it, the row is gone.
    state.apply(RuntimeEvent::SavedChats {
        conversations: vec![
            saved(
                9,
                "2026-03-10 11:59:15",
                "fresh   question",
                BackendKind::Claude,
            ),
            saved(
                2,
                "2026-03-08 12:00:00",
                "ancient history",
                BackendKind::Grok,
            ),
        ],
    });
    assert!(state.selected_agent().is_some());
    let rows = draw_rows(&state, 110, 30);
    assert!(rows.iter().all(|row| !row.contains("an older saved chat")));
}

#[test]
fn moving_the_selection_or_closing_disarms_the_deletion() {
    let mut state = saved_state();
    open(&mut state);
    state.select_last_agent();
    state.press_agents_delete();
    assert!(state.agents_delete_armed());

    state.select_previous_agent();
    assert!(!state.agents_delete_armed());
    // Back on the first row, the next Ctrl+X only arms again.
    state.select_next_agent();
    state.press_agents_delete();
    assert_eq!(state.take_pending_command(), None);
    assert!(state.agents_delete_armed());

    // Arming one row and landing on another by a refreshed list does not
    // carry the arm over.
    state.apply(RuntimeEvent::SavedChats {
        conversations: vec![saved(
            9,
            "2026-03-10 11:59:15",
            "only one",
            BackendKind::Claude,
        )],
    });
    state.press_agents_delete();
    assert_eq!(state.take_pending_command(), None);

    state.close_drawer();
    open(&mut state);
    assert!(!state.agents_delete_armed());
}

#[test]
fn ctrl_x_does_nothing_on_live_members_or_with_a_search_typed() {
    let mut state = saved_state();
    open(&mut state);
    state.press_agents_delete();
    state.press_agents_delete();
    assert_eq!(state.take_pending_command(), None);

    state.select_last_agent();
    type_query(&mut state, "ancient");
    state.press_agents_delete();
    state.press_agents_delete();
    assert_eq!(state.take_pending_command(), None);
}

#[test]
fn ctrl_r_renames_an_inactive_row_in_place_of_the_search_line() {
    let mut state = saved_state();
    state.apply(RuntimeEvent::SavedChats {
        conversations: vec![
            named(
                saved(9, "2026-03-10 11:59:15", "first words", BackendKind::Claude),
                "Release",
            ),
            saved(
                5,
                "2026-03-10 09:00:00",
                "an older saved chat",
                BackendKind::Codex,
            ),
        ],
    });
    open(&mut state);
    state.select_last_agent();
    state.select_previous_agent();
    assert_eq!(state.selected_agent(), Some(AgentChoice::Saved(9)));

    state.begin_agents_rename();
    // Prefilled with the current name.
    assert_eq!(state.agents_rename(), Some("Release"));
    for ch in " prep".chars() {
        state.agents_rename_push(ch);
    }
    let rows = draw_rows(&state, 110, 30);
    assert_eq!(rows[2].trim_end(), "  > Rename: Release prep");
    assert_eq!(rows[29].trim_end(), "  Enter save   Esc cancel");
    assert!(rows.iter().all(|row| !row.contains("Search agents")));

    state.commit_agents_rename();
    assert_eq!(
        state.take_pending_command(),
        Some(UiCommand::RenameConversation {
            conversation: Some(9),
            name: Some("Release prep".to_string()),
        })
    );
    assert_eq!(state.agents_rename(), None);
    assert_eq!(state.drawer(), Some(Drawer::Agents));
    assert_eq!(
        draw_rows(&state, 110, 30)[2].trim_end(),
        "  > Search agents"
    );
}

#[test]
fn an_empty_name_clears_and_escape_cancels() {
    let mut state = saved_state();
    state.apply(RuntimeEvent::SavedChats {
        conversations: vec![named(
            saved(9, "2026-03-10 11:59:15", "first words", BackendKind::Claude),
            "Release",
        )],
    });
    open(&mut state);
    state.select_last_agent();

    state.begin_agents_rename();
    state.agents_rename_clear();
    for ch in "   ".chars() {
        state.agents_rename_push(ch);
    }
    state.commit_agents_rename();
    assert_eq!(
        state.take_pending_command(),
        Some(UiCommand::RenameConversation {
            conversation: Some(9),
            name: None,
        })
    );

    state.begin_agents_rename();
    state.agents_rename_push('x');
    state.cancel_agents_rename();
    assert_eq!(state.agents_rename(), None);
    assert_eq!(state.take_pending_command(), None);

    // An unnamed chat starts the editor empty; paste stays on one line.
    let mut state = saved_state();
    open(&mut state);
    state.select_last_agent();
    state.begin_agents_rename();
    assert_eq!(state.agents_rename(), Some(""));
    state.agents_rename_paste("two\nlines");
    assert_eq!(state.agents_rename(), Some("two lines"));
    state.agents_rename_delete_word();
    assert_eq!(state.agents_rename(), Some("two "));
    state.agents_rename_pop();
    assert_eq!(state.agents_rename(), Some("two"));
}

#[test]
fn the_current_chat_is_renamed_from_its_main_row() {
    let mut state = main_state(vec![user("fix the parser bug", &["main"])]);
    state.select_first_agent();
    state.select_next_agent();
    state.begin_agents_rename();
    assert_eq!(state.agents_rename(), Some(""));
    for ch in "Parser work".chars() {
        state.agents_rename_push(ch);
    }
    state.commit_agents_rename();
    assert_eq!(
        state.take_pending_command(),
        Some(UiCommand::RenameConversation {
            conversation: None,
            name: Some("Parser work".to_string()),
        })
    );

    // The runtime confirms; the Main row now carries the name.
    state.apply(RuntimeEvent::ConversationRenamed {
        name: Some("Parser work".to_string()),
    });
    assert_eq!(state.chat_name(), Some("Parser work"));
    let rows = draw_rows(&state, 110, 30);
    let main = row_with(&rows, "Parser work");
    assert!(main.contains("codex"), "{main}");
    assert!(rows.iter().all(|row| !row.contains("fix the parser")));
    state.begin_agents_rename();
    assert_eq!(state.agents_rename(), Some("Parser work"));
    state.cancel_agents_rename();

    // Clearing falls back to the first message.
    state.apply(RuntimeEvent::ConversationRenamed { name: None });
    let rows = draw_rows(&state, 110, 30);
    assert!(rows.iter().any(|row| row.contains("fix the parser bug")));
}

#[test]
fn a_new_or_resumed_chat_drops_the_current_name() {
    for event in [
        RuntimeEvent::SessionReset,
        RuntimeEvent::ConversationResumed {
            conversation: 4,
            chat: Vec::new(),
        },
    ] {
        let mut state = main_state(Vec::new());
        state.apply(RuntimeEvent::ConversationRenamed {
            name: Some("Old name".to_string()),
        });
        assert_eq!(state.chat_name(), Some("Old name"));
        state.apply(event);
        assert_eq!(state.chat_name(), None);
    }
}

#[test]
fn a_saved_chat_is_titled_by_its_name_else_its_first_message() {
    let mut state = roster_state(Vec::new(), Some("builder"));
    state.set_clock_unix_secs(NOW);
    state.apply(RuntimeEvent::SavedChats {
        conversations: vec![
            named(
                saved(9, "2026-03-10 11:59:15", "first words", BackendKind::Claude),
                "  Release   prep ",
            ),
            // A named chat with no user message is listed under its name.
            named(
                saved(7, "2026-03-10 10:00:00", "", BackendKind::Codex),
                "Scratch pad",
            ),
            saved(
                5,
                "2026-03-10 09:00:00",
                "an older saved chat",
                BackendKind::Codex,
            ),
        ],
    });
    open(&mut state);

    let rows = draw_rows(&state, 110, 30);

    assert!(
        rows.iter().any(|row| row.contains("Release prep")),
        "{rows:#?}"
    );
    assert!(rows.iter().all(|row| !row.contains("first words")));
    assert!(rows.iter().any(|row| row.contains("Scratch pad")));
    assert!(rows.iter().any(|row| row.contains("an older saved chat")));
    // Search finds a chat by its name.
    type_query(&mut state, "scratch");
    let rows = draw_rows(&state, 110, 30);
    assert!(rows[6].contains("Scratch pad"), "{rows:#?}");
}

#[test]
fn a_member_waiting_for_approval_gets_a_yellow_bang_instead_of_the_pulse() {
    let mut state = roster_state(Vec::new(), Some("builder"));
    state.apply(RuntimeEvent::MemberStatus {
        member: id("reviewer"),
        status: MemberStatus::NeedsApproval,
    });
    open(&mut state);
    let terminal = terminal_of(&state, 110, 30);

    let (glyph, style) = icon_of(&terminal, "Reviewer", "");
    assert_eq!(glyph, "!");
    assert_eq!(style.fg, theme::warning().fg);
    assert_eq!(
        style.fg,
        theme::member_dot_style(MemberStatus::NeedsApproval).fg
    );
    // Other statuses are unchanged: the running member still pulses.
    let (glyph, lit) = icon_of(&terminal, "Builder", "codex");
    assert!(["◇", "◈", "◆"].contains(&glyph.as_str()), "{glyph}");
    assert_eq!(lit.fg, theme::emphasis().fg);
    let (glyph, _) = icon_of(&terminal, "Planner", "");
    assert_eq!(glyph, "•");
}
