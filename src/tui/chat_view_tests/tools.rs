//! Tool groups: short runs list every call, longer ones fold into a single
//! summary line that opens with a click or Ctrl+O.

use super::*;

fn call(name: &str, summary: &str, ok: Option<bool>) -> ChatItem {
    ChatItem::Tool {
        member: MemberId::new("builder"),
        name: name.to_string(),
        summary: summary.to_string(),
        detail: String::new(),
        ok,
    }
}

fn reads(count: usize) -> Vec<ChatItem> {
    (0..count)
        .map(|n| call("Read", &format!("src/file{n}.rs"), Some(true)))
        .collect()
}

fn rendered(state: &AppState) -> Vec<String> {
    let mut lines = Vec::new();
    render_chat_history(state, 90, 0, &mut lines);
    plain_text(&lines)
}

fn row_count(text: &[String], needle: &str) -> usize {
    text.iter().filter(|line| line.contains(needle)).count()
}

fn draw(terminal: &mut Terminal<TestBackend>, state: &AppState) -> ChatLayout {
    let mut layout = None;
    terminal
        .draw(|frame| {
            layout = render(frame, state);
        })
        .unwrap();
    layout.expect("chat layout")
}

#[test]
fn groups_of_three_or_fewer_calls_render_every_row() {
    for count in 1..=3 {
        let state = AppState::new(reads(count));
        let text = rendered(&state);
        assert!(
            text.iter().any(|line| line == "  ⚒ tools"),
            "{count}: {text:?}"
        );
        assert_eq!(row_count(&text, "· Read"), count, "{text:?}");
        assert!(!text.iter().any(|line| line.contains('▸')), "{text:?}");
    }
}

#[test]
fn four_calls_collapse_into_one_line_with_counts_by_kind() {
    let state = AppState::new(vec![
        call("Read", "a.rs", Some(true)),
        call("Bash", "cargo test", Some(true)),
        call("Read", "b.rs", Some(true)),
        call("Grep", "needle", Some(true)),
    ]);
    let text = rendered(&state);
    assert!(
        text.iter()
            .any(|line| line == "  ▸ ⚒ tools · 4 calls · 2 read, 1 shell, 1 search"),
        "{text:?}"
    );
    assert_eq!(row_count(&text, "· Read"), 0, "{text:?}");
    assert_eq!(row_count(&text, "a.rs"), 0, "{text:?}");
}

#[test]
fn collapsed_line_names_at_most_four_kinds_then_an_ellipsis() {
    let state = AppState::new(vec![
        call("Read", "a", Some(true)),
        call("Bash", "b", Some(true)),
        call("Grep", "c", Some(true)),
        call("WebFetch", "d", Some(true)),
        call("TodoWrite", "e", Some(true)),
        call("Read", "f", Some(true)),
    ]);
    let text = rendered(&state);
    assert!(
        text.iter()
            .any(|line| line == "  ▸ ⚒ tools · 6 calls · 2 read, 1 shell, 1 search, 1 fetch, …"),
        "{text:?}"
    );
}

#[test]
fn collapsed_line_counts_failed_calls_in_the_error_colour() {
    let mut items = reads(3);
    items.push(call("Bash", "cargo test", Some(false)));
    items.push(call("Bash", "cargo build", Some(false)));
    let state = AppState::new(items);
    let mut lines = Vec::new();
    render_chat_history(&state, 90, 0, &mut lines);
    let title = lines
        .iter()
        .find(|line| plain_text(std::slice::from_ref(*line))[0].contains("▸"))
        .expect("collapsed title");
    let text = plain_text(std::slice::from_ref(title))[0].clone();
    assert_eq!(text, "  ▸ ⚒ tools · 5 calls · 3 read, 2 shell · 2 failed");
    let failed = title.spans.last().expect("failed span");
    assert_eq!(failed.content, " · 2 failed");
    assert_eq!(failed.style, theme::error_bold());
    assert_eq!(title.spans[0].style, theme::accent_bold());
}

#[test]
fn a_fully_failed_collapsed_group_uses_the_failure_marker() {
    let items = (0..4)
        .map(|n| call("Bash", &format!("cmd {n}"), Some(false)))
        .collect();
    let text = rendered(&AppState::new(items));
    assert!(
        text.iter()
            .any(|line| line == "  ▸ ✕ tools · 4 calls · 4 shell · 4 failed"),
        "{text:?}"
    );
}

#[test]
fn a_running_newest_call_shows_below_the_collapsed_line() {
    let mut state = AppState::new(reads(4));
    state.apply(RuntimeEvent::ToolStarted {
        member: MemberId::new("builder"),
        tool_id: "t-run".to_string(),
        name: "Bash".to_string(),
        summary: "cargo test".to_string(),
    });
    let text = rendered(&state);
    let title = text
        .iter()
        .position(|line| line.starts_with("  ▸ ⚒ tools · 5 calls"))
        .expect("collapsed title");
    assert!(
        text[title + 1].contains("Shell") && text[title + 1].contains("cargo test"),
        "{text:?}"
    );
    assert_eq!(row_count(&text, "· Read"), 0, "{text:?}");
    assert_eq!(row_count(&text, "src/file"), 0, "{text:?}");

    // Once the call finishes the group is the single line again.
    state.apply(RuntimeEvent::ToolCompleted {
        member: MemberId::new("builder"),
        tool_id: "t-run".to_string(),
        ok: true,
        output: String::new(),
    });
    let text = rendered(&state);
    let title = text
        .iter()
        .position(|line| line.starts_with("  ▸ ⚒ tools · 5 calls"))
        .expect("collapsed title");
    assert!(text[title + 1].is_empty(), "{text:?}");
    assert_eq!(row_count(&text, "cargo test"), 0, "{text:?}");
}

#[test]
fn an_expanded_group_shows_the_down_marker_and_every_row() {
    let mut state = AppState::new(reads(5));
    state.toggle_tool_group(0);
    let text = rendered(&state);
    assert!(text.iter().any(|line| line == "  ▾ ⚒ tools"), "{text:?}");
    assert_eq!(row_count(&text, "· Read"), 5, "{text:?}");
}

#[test]
fn ctrl_o_expands_every_group_and_turning_it_off_restores_group_state() {
    let mut items = reads(4);
    items.push(ChatItem::Agent {
        member: MemberId::new("builder"),
        display_name: "Builder".to_string(),
        backend: BackendKind::Codex,
        text: "between".to_string(),
    });
    items.extend(reads(4));
    let mut state = AppState::new(items);
    state.toggle_tool_group(0);

    state.toggle_tools_expansion();
    let text = rendered(&state);
    assert_eq!(row_count(&text, "· Read"), 8, "{text:?}");
    assert_eq!(row_count(&text, "▾ ⚒ tools"), 2, "{text:?}");

    state.toggle_tools_expansion();
    let text = rendered(&state);
    assert_eq!(row_count(&text, "· Read"), 4, "{text:?}");
    assert_eq!(row_count(&text, "▾ ⚒ tools"), 1, "{text:?}");
    assert_eq!(row_count(&text, "▸ ⚒ tools · 4 calls"), 1, "{text:?}");
}

#[test]
fn the_title_row_is_registered_and_clicking_it_repaints_the_group() {
    let mut state = AppState::new(reads(6));
    let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();

    let layout = draw(&mut terminal, &state);
    let title_line = layout
        .lines
        .iter()
        .position(|line| line.contains("▸ ⚒ tools · 6 calls"))
        .expect("collapsed title");
    assert_eq!(
        layout.tool_group_at_line(layout.first_line + title_line),
        Some(0)
    );
    assert_eq!(layout.fold_titles.len(), 1);
    let view = format!("{}", terminal.backend());
    assert!(!view.contains("src/file3.rs"), "{view}");

    // The paint cache must notice the toggle even though no item changed.
    state.toggle_tool_group(0);
    let layout = draw(&mut terminal, &state);
    let view = format!("{}", terminal.backend());
    assert!(view.contains("▾ ⚒ tools"), "{view}");
    assert!(view.contains("src/file3.rs"), "{view}");
    let title_line = layout
        .lines
        .iter()
        .position(|line| line.contains("▾ ⚒ tools"))
        .expect("expanded title");
    assert_eq!(
        layout.tool_group_at_line(layout.first_line + title_line),
        Some(0)
    );

    state.toggle_tool_group(0);
    draw(&mut terminal, &state);
    let view = format!("{}", terminal.backend());
    assert!(!view.contains("src/file3.rs"), "{view}");
}

#[test]
fn title_rows_keep_their_content_line_after_scrolling() {
    // Two long folded groups separated by replies, in a short viewport.
    let mut items = Vec::new();
    for round in 0..2 {
        items.extend(reads(5));
        items.push(ChatItem::Agent {
            member: MemberId::new("builder"),
            display_name: "Builder".to_string(),
            backend: BackendKind::Codex,
            text: format!(
                "reply {round}\n\n{}",
                (0..8)
                    .map(|n| format!("paragraph {n}"))
                    .collect::<Vec<_>>()
                    .join("\n\n")
            ),
        });
    }
    let mut state = AppState::new(items);
    let mut terminal = Terminal::new(TestBackend::new(100, 14)).unwrap();

    let at_bottom = draw(&mut terminal, &state);
    let total = at_bottom.total_lines;
    assert!(total > 14, "the transcript must overflow the viewport");

    // Sweep the view from the bottom to the top. Every registered title is
    // the row that shows it, and a group keeps one content line throughout.
    let max = total;
    let mut lines_by_key = std::collections::HashMap::new();
    for back in 0..=max {
        state.reset_scroll();
        state.scroll_by(back as i32);
        let layout = draw(&mut terminal, &state);
        for &(line, key) in &layout.fold_titles {
            let row = &layout.lines[line - layout.first_line];
            assert!(row.contains("▸ ⚒ tools · 5 calls"), "scroll {back}: {row}");
            assert_eq!(
                *lines_by_key.entry(key).or_insert(line),
                line,
                "scroll {back}"
            );
            assert_eq!(layout.fold_target_at_line(line), Some(key));
        }
    }
    let mut keys: Vec<_> = lines_by_key
        .keys()
        .map(|target| match target {
            FoldTarget::Tools(first) => *first,
            other => panic!("unexpected disclosure: {other:?}"),
        })
        .collect();
    keys.sort_unstable();
    assert_eq!(keys, vec![0, 6], "both groups must be reachable by click");
}
