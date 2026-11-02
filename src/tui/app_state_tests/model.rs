use super::focus::{ready_with, summary, two_members};
use super::*;
use crate::adapter::{DiscoveredCatalog, DiscoveredModel};
use crate::tui::model_picker::PickerRowView;

fn model(id: &str, efforts: &[Effort], default: bool) -> DiscoveredModel {
    let mut model = DiscoveredModel::simple(id);
    model.supported_efforts = efforts.to_vec();
    model.is_default = default;
    model
}

fn model_with_default_effort(
    id: &str,
    efforts: &[Effort],
    default_effort: Effort,
) -> DiscoveredModel {
    let mut model = model(id, efforts, false);
    model.default_effort = Some(default_effort);
    model
}

fn all_detected() -> DetectedBackends {
    DetectedBackends {
        codex: true,
        claude: true,
        grok: false,
        agy: true,
    }
}

/// Builder (Codex) and Reviewer (Claude), with catalogs for Codex and Claude.
fn model_state() -> AppState {
    let mut state = AppState::new(Vec::new());
    state.apply(ready_with(
        Some(DefaultTarget::Member(MemberId::new("builder"))),
        two_members(),
    ));
    state.detected_backends = Some(all_detected());
    let cwd = PathBuf::from("/tmp/ws");
    state.model_catalog.seed_catalog(
        BackendKind::Codex,
        &cwd,
        DiscoveredCatalog {
            models: vec![
                model(
                    "gpt-5.6",
                    &[Effort::Low, Effort::Medium, Effort::High],
                    true,
                ),
                model("gpt-mini", &[], false),
            ],
            native_permission: None,
            ..Default::default()
        },
    );
    state.model_catalog.seed_catalog(
        BackendKind::Claude,
        &cwd,
        DiscoveredCatalog {
            models: vec![model("opus", &[Effort::High, Effort::Max], false)],
            native_permission: None,
            ..Default::default()
        },
    );
    state.model_catalog.seed_loading(BackendKind::Agy, &cwd);
    state
}

fn names(state: &AppState) -> Vec<String> {
    state
        .model_picker_view()
        .expect("picker open")
        .rows
        .into_iter()
        .filter_map(|row| match row {
            PickerRowView::Heading(_) => None,
            PickerRowView::Entry { name, .. } => Some(name),
            PickerRowView::Note(text) => Some(text.trim().to_string()),
        })
        .collect()
}

/// Group headings in display order.
fn headings(state: &AppState) -> Vec<String> {
    state
        .model_picker_view()
        .expect("picker open")
        .rows
        .into_iter()
        .filter_map(|row| match row {
            PickerRowView::Heading(text) => Some(text),
            _ => None,
        })
        .collect()
}

fn selected_name(state: &AppState) -> String {
    state.selected_model_entry().expect("selection").name
}

fn command_members(command: UiCommand) -> Vec<TeamMember> {
    match command {
        UiCommand::ReplaceTeam { members, .. } => members,
        other => panic!("expected ReplaceTeam, got {other:?}"),
    }
}

#[test]
fn selector_targets_the_viewed_agent_or_the_default_one_in_the_all_view() {
    let mut state = model_state();
    state.set_focus(Some(MemberId::new("reviewer")));
    state.open_model_picker("");
    assert_eq!(state.model_picker_view().unwrap().target, "Reviewer");

    state.close_model_picker();
    state.set_focus(None);
    state.open_model_picker("");
    assert_eq!(state.model_picker_view().unwrap().target, "Builder");
    assert!(state.model_picker_open());
}

#[test]
fn rows_group_by_backend_and_the_catalog_default_is_current_when_nothing_is_pinned() {
    let mut state = model_state();
    state.open_model_picker("");

    // Only real catalog models, grouped by backend under a heading each; Agy
    // is still loading, so it contributes a status row and nothing selectable.
    assert_eq!(
        names(&state),
        ["gpt-5.6", "gpt-mini", "opus", "loading models…"]
    );
    assert_eq!(headings(&state), ["codex", "claude", "agy"]);
    let view = state.model_picker_view().unwrap();
    let trailing: Vec<&str> = view
        .rows
        .iter()
        .filter_map(|row| match row {
            PickerRowView::Entry { trailing, .. } => Some(trailing.as_str()),
            _ => None,
        })
        .collect();
    // Nothing is pinned and the CLI names no model, so the catalog's default
    // is what runs; the right column shows only `current`, never a backend.
    assert_eq!(trailing, ["current", "", ""]);
    assert_eq!(selected_name(&state), "gpt-5.6");
    assert_eq!(state.model_picker_entries().len(), 3);
}

#[test]
fn arrow_keys_skip_group_headings_and_the_counter_counts_models_only() {
    let mut state = model_state();
    state.open_model_picker("");
    let view = state.model_picker_view().unwrap();
    // codex heading, gpt-5.6, gpt-mini, claude heading, opus, agy heading, note.
    assert_eq!(view.rows.len(), 7);
    assert_eq!(view.total_models, 3);
    assert_eq!(view.selected_position, 1);
    assert_eq!(view.selected_row, Some(1));

    state.select_next_model();
    assert_eq!(selected_name(&state), "gpt-mini");
    // The claude heading sits between gpt-mini and opus; ↓ lands on opus.
    state.select_next_model();
    assert_eq!(selected_name(&state), "opus");
    let view = state.model_picker_view().unwrap();
    assert_eq!(view.selected_row, Some(4));
    assert_eq!(view.selected_position, 3);
    // Nothing selectable below the last model.
    state.select_next_model();
    assert_eq!(selected_name(&state), "opus");

    state.select_previous_model();
    assert_eq!(selected_name(&state), "gpt-mini");
    assert_eq!(state.model_picker_view().unwrap().selected_row, Some(2));
}

#[test]
fn a_pinned_model_is_current_and_stays_in_its_group() {
    let mut state = AppState::new(Vec::new());
    let mut builder = summary("builder", "Builder", BackendKind::Codex, MemberStatus::Idle);
    builder.model = Some("gpt-mini".to_string());
    state.apply(ready_with(None, vec![builder]));
    state.seed_installed_catalog(
        BackendKind::Codex,
        DiscoveredCatalog {
            models: vec![
                model("gpt-5.6", &[Effort::Low], true),
                model("gpt-mini", &[], false),
            ],
            ..Default::default()
        },
    );
    state.open_model_picker("");

    assert_eq!(names(&state), ["gpt-5.6", "gpt-mini"]);
    assert_eq!(headings(&state), ["codex"]);
    let view = state.model_picker_view().unwrap();
    let trailing = |index: usize| match &view.rows[index] {
        PickerRowView::Entry { trailing, .. } => trailing.clone(),
        _ => panic!("model row"),
    };
    // Row 0 is the heading.
    assert_eq!(trailing(1), "");
    assert_eq!(trailing(2), "current");
    assert_eq!(selected_name(&state), "gpt-mini");
}
#[test]
fn a_failed_backend_shows_only_its_error_and_nothing_selectable() {
    let mut state = model_state();
    state
        .model_catalog
        .seed_failed(BackendKind::Agy, Path::new("/tmp/ws"), "agy not logged in");
    state.open_model_picker("agy");

    assert_eq!(names(&state), ["model discovery failed: agy not logged in"]);
    assert_eq!(headings(&state), ["agy"]);
    assert!(state.selected_model_entry().is_none());
    // Nothing to apply: Enter does nothing.
    assert!(state.selected_model_command().is_none());
    assert!(state.model_picker_open());
}

#[test]
fn an_empty_filter_result_shows_no_rows_and_enter_does_nothing() {
    let mut state = model_state();
    state.open_model_picker("zzz-nothing");

    let view = state.model_picker_view().unwrap();
    assert!(view.rows.is_empty());
    assert!(state.selected_model_entry().is_none());
    let chat_before = state.chat().len();
    assert!(state.selected_model_command().is_none());
    assert_eq!(state.chat().len(), chat_before, "no notice either");
}
#[test]
fn search_filters_backend_id_and_name_case_insensitively() {
    let mut state = model_state();
    state.open_model_picker("");

    for ch in "GPT-M".chars() {
        state.model_filter_push(ch);
    }
    assert_eq!(names(&state), ["gpt-mini"]);
    assert_eq!(selected_name(&state), "gpt-mini");

    state.model_filter_clear();
    for ch in "claude".chars() {
        state.model_filter_push(ch);
    }
    assert_eq!(names(&state), ["opus"]);

    state.model_filter_pop();
    assert_eq!(state.model_picker_view().unwrap().filter, "claud");

    state.model_filter_clear();
    state.model_filter_paste("o\np");
    assert_eq!(state.model_picker_view().unwrap().filter, "o p");
}
#[test]
fn open_with_text_prefills_the_search() {
    let mut state = model_state();
    state.open_model_picker("  opus ");
    assert_eq!(state.model_picker_view().unwrap().filter, "opus");
    assert_eq!(names(&state), ["opus"]);
}

#[test]
fn selection_is_kept_by_identity_when_a_catalog_arrives() {
    let mut state = model_state();
    state.model_catalog = Default::default();
    let cwd = PathBuf::from("/tmp/ws");
    state.model_catalog.seed_loading(BackendKind::Codex, &cwd);
    state.model_catalog.seed_catalog(
        BackendKind::Claude,
        &cwd,
        DiscoveredCatalog {
            models: vec![model("opus", &[], false)],
            ..Default::default()
        },
    );
    state.model_catalog.seed_loading(BackendKind::Agy, &cwd);
    state.open_model_picker("");
    assert_eq!(selected_name(&state), "opus");
    let before = state.model_picker_position();

    state.model_catalog.seed_catalog(
        BackendKind::Codex,
        &cwd,
        DiscoveredCatalog {
            models: vec![model("gpt-5.6", &[], true), model("gpt-mini", &[], false)],
            ..Default::default()
        },
    );

    assert_eq!(selected_name(&state), "opus");
    assert_ne!(state.model_picker_position(), before);
}
#[test]
fn effort_starts_on_a_concrete_level_and_cycles_within_the_models_levels() {
    let mut state = model_state();
    state.open_model_picker("gpt-5.6");
    let effort = |state: &AppState| {
        let entry = state.selected_model_entry().unwrap();
        state.model_picker_effort(&entry)
    };
    // No member effort and no model default: medium.
    assert_eq!(effort(&state), Some(Effort::Medium));

    state.cycle_model_effort(true);
    assert_eq!(effort(&state), Some(Effort::High));
    state.cycle_model_effort(true);
    assert_eq!(effort(&state), Some(Effort::Low));
    state.cycle_model_effort(false);
    assert_eq!(effort(&state), Some(Effort::High));
    state.cycle_model_effort(false);
    state.cycle_model_effort(false);
    assert_eq!(effort(&state), Some(Effort::Low));
}

#[test]
fn models_without_effort_levels_have_nothing_to_cycle() {
    let mut state = model_state();
    state.open_model_picker("gpt-mini");
    let entry = state.selected_model_entry().unwrap();
    assert!(entry.levels.is_empty());
    assert_eq!(state.model_picker_effort(&entry), None);

    state.cycle_model_effort(true);
    state.cycle_model_effort(false);

    assert_eq!(state.model_picker_effort(&entry), None);
    assert!(state.model_picker.as_ref().unwrap().efforts.is_empty());
    let view = state.model_picker_view().unwrap();
    // Row 0 is the backend heading.
    let PickerRowView::Entry { levels, level, .. } = &view.rows[1] else {
        panic!("model row");
    };
    assert!(levels.is_empty() && level.is_none());
}

#[test]
fn agy_models_have_no_effort_levels() {
    let mut state = model_state();
    state.seed_installed_models(BackendKind::Agy, vec!["gemini-high".to_string()]);
    state.open_model_picker("gemini");
    let entry = state.selected_model_entry().unwrap();
    assert_eq!(entry.key.backend, BackendKind::Agy);
    state.cycle_model_effort(true);
    assert_eq!(state.model_picker_effort(&entry), None);
}
#[test]
fn seeded_level_prefers_the_members_effort_then_the_model_default_then_medium() {
    let levels = [Effort::Low, Effort::Medium, Effort::High];
    let mut state = model_state();
    let cwd = PathBuf::from("/tmp/ws");
    state.model_catalog.seed_catalog(
        BackendKind::Codex,
        &cwd,
        DiscoveredCatalog {
            models: vec![
                model("gpt-5.6", &levels, true),
                model_with_default_effort("with-default", &levels, Effort::High),
                model("high-only", &[Effort::High, Effort::Max], false),
            ],
            native_permission: None,
            ..Default::default()
        },
    );
    let level = |state: &mut AppState, filter: &str| {
        state.model_filter_clear();
        for ch in filter.chars() {
            state.model_filter_push(ch);
        }
        let entry = state.selected_model_entry().unwrap();
        state.model_picker_effort(&entry)
    };
    state.open_model_picker("");

    assert_eq!(level(&mut state, "with-default"), Some(Effort::High));
    assert_eq!(level(&mut state, "high-only"), Some(Effort::High));
    assert_eq!(level(&mut state, "gpt-5.6"), Some(Effort::Medium));

    // The member's own effort wins on its current model, if supported.
    let mut builder = summary("builder", "Builder", BackendKind::Codex, MemberStatus::Idle);
    builder.model = Some("gpt-5.6".to_string());
    builder.effort = Some(Effort::Low);
    state.apply(ready_with(None, vec![builder]));
    state.open_model_picker("");
    assert_eq!(level(&mut state, "gpt-5.6"), Some(Effort::Low));
    // ...but not on a model that is not the current one.
    assert_eq!(level(&mut state, "with-default"), Some(Effort::High));
}

#[test]
fn applying_pins_the_model_and_stores_only_an_edited_effort() {
    let mut state = model_state();
    state.open_model_picker("gpt-5.6");
    state.cycle_model_effort(true);

    let (command, notice) = state.selected_model_command().expect("command");

    let members = command_members(command);
    assert_eq!(members.len(), 2);
    let builder = &members[0];
    assert_eq!(builder.id, MemberId::new("builder"));
    assert_eq!(builder.display_name, "Builder");
    assert_eq!(builder.role, "role");
    assert_eq!(builder.backend, BackendKind::Codex);
    assert_eq!(builder.model.as_deref(), Some("gpt-5.6"));
    assert_eq!(builder.effort, Some(Effort::High));
    assert_eq!(notice, "Model: Builder → codex · gpt-5.6 · high");
    // The other member is passed through untouched.
    assert_eq!(members[1].model, None);
}
#[test]
fn applying_without_editing_writes_the_effort_the_row_displays() {
    let mut state = model_state();
    // `gpt-5.6` has no configured effort, so the row seeds on `medium`.
    state.open_model_picker("gpt-5.6");
    let entry = state.selected_model_entry().unwrap();
    assert_eq!(state.model_picker_effort(&entry), Some(Effort::Medium));

    let (command, notice) = state.selected_model_command().expect("command");

    let builder = &command_members(command)[0];
    assert_eq!(builder.model.as_deref(), Some("gpt-5.6"));
    assert_eq!(builder.effort, Some(Effort::Medium));
    assert_eq!(notice, "Model: Builder → codex · gpt-5.6 · medium");
}

#[test]
fn a_level_seeded_from_the_clis_config_is_saved_not_left_to_the_models_default() {
    // The user's case: config.toml says `high`, the model's own default is
    // `low`, and the member has no effort yet.
    let mut state = AppState::new(Vec::new());
    let builder = summary("builder", "Builder", BackendKind::Codex, MemberStatus::Idle);
    state.apply(ready_with(None, vec![builder]));
    state.seed_installed_catalog(BackendKind::Codex, native_catalog());
    state.open_model_picker("gpt-sol");
    let entry = state.selected_model_entry().unwrap();
    assert_eq!(state.model_picker_effort(&entry), Some(Effort::High));

    let (command, notice) = state.selected_model_command().expect("command");

    assert_eq!(command_members(command)[0].effort, Some(Effort::High));
    assert_eq!(notice, "Model: Builder → codex · gpt-sol · high");
}

#[test]
fn models_without_effort_levels_are_saved_with_no_effort() {
    let mut state = model_state();
    state.open_model_picker("gpt-mini");

    let (command, notice) = state.selected_model_command().expect("command");

    let builder = &command_members(command)[0];
    assert_eq!(builder.model.as_deref(), Some("gpt-mini"));
    assert_eq!(builder.effort, None);
    assert_eq!(notice, "Model: Builder → codex · gpt-mini");
}

#[test]
fn applying_the_members_current_model_keeps_its_existing_effort() {
    let mut state = AppState::new(Vec::new());
    let mut builder = summary("builder", "Builder", BackendKind::Codex, MemberStatus::Idle);
    builder.model = Some("gpt-5.6".to_string());
    builder.effort = Some(Effort::High);
    state.apply(ready_with(None, vec![builder]));
    state.seed_installed_catalog(
        BackendKind::Codex,
        DiscoveredCatalog {
            models: vec![model("gpt-5.6", &[Effort::Low, Effort::High], true)],
            ..Default::default()
        },
    );
    state.open_model_picker("");

    let (command, notice) = state.selected_model_command().expect("command");

    let builder = &command_members(command)[0];
    assert_eq!(builder.model.as_deref(), Some("gpt-5.6"));
    assert_eq!(builder.effort, Some(Effort::High));
    assert_eq!(notice, "Model: Builder → codex · gpt-5.6 · high");
}
#[test]
fn choosing_another_model_saves_the_level_it_shows_then_the_edited_one() {
    let mut state = AppState::new(Vec::new());
    let mut builder = summary("builder", "Builder", BackendKind::Codex, MemberStatus::Idle);
    builder.model = Some("gpt-5.6".to_string());
    builder.effort = Some(Effort::High);
    state.apply(ready_with(None, vec![builder]));
    state.seed_installed_catalog(
        BackendKind::Codex,
        DiscoveredCatalog {
            models: vec![
                model("gpt-5.6", &[Effort::Low, Effort::High], true),
                model("gpt-other", &[Effort::Low, Effort::High], false),
            ],
            native_permission: None,
            ..Default::default()
        },
    );
    state.open_model_picker("gpt-other");

    // Not the current model, so the member's `high` does not carry over; the
    // row shows its own seed (`low`, the lowest) and that is what is saved.
    let (command, notice) = state.selected_model_command().expect("command");
    let builder = &command_members(command)[0];
    assert_eq!(builder.model.as_deref(), Some("gpt-other"));
    assert_eq!(builder.effort, Some(Effort::Low));
    assert_eq!(notice, "Model: Builder → codex · gpt-other · low");

    // Editing it stores the chosen level.
    state.cycle_model_effort(true);
    let (command, notice) = state.selected_model_command().expect("command");
    assert_eq!(command_members(command)[0].effort, Some(Effort::High));
    assert_eq!(notice, "Model: Builder → codex · gpt-other · high");
}

#[test]
fn backend_change_takes_that_clis_defaults_and_a_fresh_session() {
    let mut state = AppState::new(Vec::new());
    let mut builder = summary("builder", "Builder", BackendKind::Codex, MemberStatus::Idle);
    builder.session = Some("thread-1".to_string());
    builder.model = Some("old-model".to_string());
    builder.effort = Some(Effort::High);
    state.apply(ready_with(None, vec![builder]));
    state.seed_installed_catalog(
        BackendKind::Claude,
        DiscoveredCatalog {
            models: vec![model("opus", &[Effort::High, Effort::Max], false)],
            ..Default::default()
        },
    );
    state.open_model_picker("claude");
    assert_eq!(selected_name(&state), "opus");

    let (command, notice) = state.selected_model_command().expect("command");

    let members = command_members(command);
    let builder = &members[0];
    assert_eq!(builder.backend, BackendKind::Claude);
    assert_eq!(builder.model.as_deref(), Some("opus"));
    // The old CLI's `high` does not follow the member; the row's own seed
    // (the lowest level the model offers) is what is saved.
    assert_eq!(builder.effort, Some(Effort::High));
    assert_eq!(builder.session_id, None);
    assert_eq!(builder.permission_mode, Some(PermissionMode::AcceptEdits));
    assert_eq!(builder.id, MemberId::new("builder"));
    assert_eq!(builder.display_name, "Builder");
    assert_eq!(builder.role, "role");
    assert_eq!(notice, "Model: Builder → claude · opus · high");
}

#[test]
fn same_backend_apply_keeps_the_live_session() {
    let mut state = AppState::new(Vec::new());
    let mut builder = summary("builder", "Builder", BackendKind::Codex, MemberStatus::Idle);
    builder.session = Some("thread-1".to_string());
    state.apply(ready_with(None, vec![builder]));
    state.seed_installed_models(BackendKind::Codex, vec!["gpt-5.6".to_string()]);
    state.open_model_picker("gpt");

    let (command, _) = state.selected_model_command().expect("command");

    assert_eq!(
        command_members(command)[0].session_id.as_deref(),
        Some("thread-1")
    );
}

#[test]
fn switching_is_refused_while_the_target_is_working() {
    let mut state = model_state();
    state.apply(RuntimeEvent::MemberStatus {
        member: MemberId::new("builder"),
        status: MemberStatus::Running,
    });
    state.open_model_picker("gpt-5.6");

    assert!(state.selected_model_command().is_none());

    assert!(state.model_picker_open());
    assert_eq!(
        state.model_picker_view().unwrap().notice.as_deref(),
        Some("Stop the running work before switching models (Esc).")
    );
    assert!(state.chat().iter().any(|item| matches!(
        item,
        ChatItem::Notice { text, .. } if text == "Stop the running work before switching models (Esc)."
    )));

    // Another member being busy does not block this one.
    let mut state = model_state();
    state.apply(RuntimeEvent::MemberStatus {
        member: MemberId::new("reviewer"),
        status: MemberStatus::Running,
    });
    state.open_model_picker("gpt-5.6");
    assert!(state.selected_model_command().is_some());
}

#[test]
fn welcome_summary_resolves_the_effective_model_and_effort() {
    // No catalog yet: just the backend.
    let mut state = AppState::new(Vec::new());
    let builder = summary("builder", "Builder", BackendKind::Codex, MemberStatus::Idle);
    state.apply(ready_with(
        Some(DefaultTarget::Member(MemberId::new("builder"))),
        vec![builder.clone()],
    ));
    assert_eq!(state.welcome_model_summary().as_deref(), Some("codex"));

    // Catalog default and its default effort.
    let mut sol = model_with_default_effort("gpt-sol", &[Effort::Low, Effort::High], Effort::Low);
    sol.name = "GPT-Sol".to_string();
    sol.is_default = true;
    state.seed_installed_catalog(
        BackendKind::Codex,
        DiscoveredCatalog {
            models: vec![sol],
            ..Default::default()
        },
    );
    assert_eq!(
        state.welcome_model_summary().as_deref(),
        Some("GPT-Sol · low")
    );

    // The CLI's own configuration outranks the catalog default.
    let mut sol = model_with_default_effort("gpt-sol", &[Effort::Low, Effort::High], Effort::Low);
    sol.name = "GPT-Sol".to_string();
    sol.is_default = true;
    state.seed_installed_catalog(
        BackendKind::Codex,
        DiscoveredCatalog {
            models: vec![sol],
            native_model: Some("gpt-sol".to_string()),
            native_efforts: crate::adapter::models::NativeEfforts {
                global: Some(Effort::High),
                by_model: Vec::new(),
            },
            ..Default::default()
        },
    );
    assert_eq!(
        state.welcome_model_summary().as_deref(),
        Some("GPT-Sol · high")
    );

    // Pinned values outrank both.
    let mut pinned = builder;
    pinned.model = Some("gpt-5.6".to_string());
    pinned.effort = Some(Effort::Medium);
    state.apply(ready_with(
        Some(DefaultTarget::Member(MemberId::new("builder"))),
        vec![pinned],
    ));
    assert_eq!(
        state.welcome_model_summary().as_deref(),
        Some("gpt-5.6 · medium")
    );
}

#[test]
fn member_model_label_matches_the_welcome_format_and_falls_back_to_the_backend() {
    let mut state = AppState::new(Vec::new());
    let builder = summary("builder", "Builder", BackendKind::Codex, MemberStatus::Idle);
    state.apply(ready_with(
        Some(DefaultTarget::Member(MemberId::new("builder"))),
        vec![builder],
    ));
    let member = state.members()[0].clone();
    // No catalog yet: just the backend.
    assert_eq!(state.member_model_label(&member), "codex");

    let mut sol = model_with_default_effort("gpt-sol", &[Effort::Low, Effort::High], Effort::High);
    sol.name = "GPT-Sol".to_string();
    sol.is_default = true;
    state.seed_installed_catalog(
        BackendKind::Codex,
        DiscoveredCatalog {
            models: vec![sol],
            ..Default::default()
        },
    );
    assert_eq!(state.member_model_label(&member), "GPT-Sol · high");
    assert_eq!(
        state.welcome_model_summary().as_deref(),
        Some(state.member_model_label(&member).as_str())
    );
}

#[test]
fn model_labels_keep_resolving_while_the_team_editor_holds_the_catalog() {
    let mut state = AppState::new(Vec::new());
    let builder = summary("builder", "Builder", BackendKind::Codex, MemberStatus::Idle);
    state.apply(ready_with(
        Some(DefaultTarget::Member(MemberId::new("builder"))),
        vec![builder],
    ));
    let mut sol = model_with_default_effort("gpt-sol", &[Effort::Low, Effort::High], Effort::High);
    sol.name = "GPT-Sol".to_string();
    sol.is_default = true;
    state.seed_installed_catalog(
        BackendKind::Codex,
        DiscoveredCatalog {
            models: vec![sol],
            ..Default::default()
        },
    );
    let member = state.members()[0].clone();
    let id = member.id.clone();
    let before = state.member_model_label(&member);
    assert_eq!(before, "GPT-Sol · high");

    // Opening `/team` moves the catalog into the editor; labels must not change.
    state.toggle_drawer(Drawer::Team);
    assert!(state.team_editor().is_some());
    assert_eq!(state.member_model_label(&member), before);
    assert_eq!(state.member_model_label_by_id(&id), Some(before.clone()));
    assert_eq!(state.welcome_model_summary(), Some(before.clone()));
    assert_eq!(
        state.member_model_summary(&member).as_deref(),
        Some("GPT-Sol · high")
    );

    // And again once the editor closes and returns the catalog.
    state.close_drawer();
    assert!(state.team_editor().is_none());
    assert_eq!(state.member_model_label(&member), before);
}

#[test]
fn team_updated_confirmation_is_dropped_only_after_a_model_switch() {
    let mut state = model_state();
    let notices = |state: &AppState| {
        state
            .chat()
            .iter()
            .filter_map(|item| match item {
                ChatItem::Notice { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
    };

    state.apply(RuntimeEvent::Notice("team updated: 2 member(s)".into()));
    assert_eq!(notices(&state), ["team updated: 2 member(s)"]);

    state.apply(RuntimeEvent::Notice(
        "Model: Builder → codex · default · default effort".into(),
    ));
    state.expect_model_switch_confirmation();
    state.apply(RuntimeEvent::Notice("team updated: 2 member(s)".into()));
    assert_eq!(notices(&state).len(), 2, "{:?}", notices(&state));

    // A rejection ends the wait, so a later Team-editor update still shows.
    state.expect_model_switch_confirmation();
    state.apply(RuntimeEvent::Notice(
        "cannot update builder while it is active".into(),
    ));
    state.apply(RuntimeEvent::Notice("team updated: 2 member(s)".into()));
    assert_eq!(notices(&state).len(), 4, "{:?}", notices(&state));
}

#[test]
fn a_partial_carry_over_is_added_to_the_model_row_not_shown_as_a_second_row() {
    let mut state = model_state();
    let notices = |state: &AppState| {
        state
            .chat()
            .iter()
            .filter_map(|item| match item {
                ChatItem::Notice { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    state.apply(RuntimeEvent::Notice(
        "Model: Builder → claude · opus".into(),
    ));
    state.expect_model_switch_confirmation();
    state.apply(RuntimeEvent::Notice(
        "team updated: 2 member(s) · earlier messages not carried over".into(),
    ));
    assert_eq!(
        notices(&state),
        ["Model: Builder → claude · opus · earlier messages not carried over"]
    );

    // Only a `/model` change waits for the confirmation; the Team editor's
    // own update shows as written.
    state.apply(RuntimeEvent::Notice(
        "team updated: 2 member(s) · earlier messages not carried over".into(),
    ));
    assert_eq!(notices(&state).len(), 2);
    assert_eq!(
        notices(&state)[0],
        "Model: Builder → claude · opus · earlier messages not carried over"
    );
}

#[test]
fn polling_reports_a_catalog_landing_only_while_models_are_on_screen() {
    let mut state = AppState::new(Vec::new());
    state.apply(ready_with(None, two_members()));
    state.detected_backends = Some(all_detected());
    let cwd = PathBuf::from("/tmp/ws");
    let pending = state.model_catalog.seed_pending(BackendKind::Codex, &cwd);
    state.open_model_picker("");
    assert!(names(&state).contains(&"loading models…".to_string()));
    assert_eq!(headings(&state)[0], "codex");
    assert!(!state.poll_team_editor_catalog(), "nothing has landed yet");

    pending
        .send(Ok(DiscoveredCatalog {
            models: vec![model("gpt-5.6", &[], true)],
            ..Default::default()
        }))
        .unwrap();

    assert!(state.poll_team_editor_catalog(), "the list must be redrawn");
    assert!(names(&state).contains(&"gpt-5.6".to_string()));
    assert!(!state.poll_team_editor_catalog(), "reported once");

    // With the selector closed, the empty chat's welcome page still shows the
    // model, so it is redrawn when a catalog lands...
    state.close_model_picker();
    let pending = state.model_catalog.seed_pending(BackendKind::Claude, &cwd);
    pending.send(Ok(DiscoveredCatalog::default())).unwrap();
    assert!(state.poll_team_editor_catalog());

    // ...but once the chat has content there is nothing to redraw for.
    state.apply(RuntimeEvent::Notice("hello".to_string()));
    let pending = state.model_catalog.seed_pending(BackendKind::Grok, &cwd);
    pending.send(Ok(DiscoveredCatalog::default())).unwrap();
    assert!(!state.poll_team_editor_catalog());
}

fn native_catalog() -> DiscoveredCatalog {
    use crate::adapter::models::NativeEfforts;
    let mut sol = model(
        "gpt-sol",
        &[Effort::Low, Effort::Medium, Effort::High],
        false,
    );
    sol.default_effort = Some(Effort::Low);
    DiscoveredCatalog {
        models: vec![model("gpt-5.6", &[Effort::Low, Effort::High], true), sol],
        native_model: Some("gpt-sol".to_string()),
        native_efforts: NativeEfforts {
            global: Some(Effort::High),
            by_model: Vec::new(),
        },
        ..Default::default()
    }
}

#[test]
fn effort_seed_prefers_the_members_own_then_the_clis_configured_effort() {
    let mut state = AppState::new(Vec::new());
    let builder = summary("builder", "Builder", BackendKind::Codex, MemberStatus::Idle);
    state.apply(ready_with(None, vec![builder.clone()]));
    state.seed_installed_catalog(BackendKind::Codex, native_catalog());
    let level = |state: &mut AppState, filter: &str| {
        state.open_model_picker(filter);
        let entry = state.selected_model_entry().unwrap();
        state.model_picker_effort(&entry)
    };

    // The CLI's configured `high` beats the model's own default of `low`.
    assert_eq!(level(&mut state, "gpt-sol"), Some(Effort::High));
    // It also applies to other models that support it.
    assert_eq!(level(&mut state, "gpt-5.6"), Some(Effort::High));

    // The member's explicit effort wins on its current model.
    let mut explicit = builder.clone();
    explicit.effort = Some(Effort::Medium);
    state.apply(ready_with(None, vec![explicit]));
    assert_eq!(level(&mut state, "gpt-sol"), Some(Effort::Medium));
    // ...but not on another model.
    assert_eq!(level(&mut state, "gpt-5.6"), Some(Effort::High));

    // A configured effort the model lacks falls back to the model default.
    let mut catalog = native_catalog();
    catalog.native_efforts.global = Some(Effort::Max);
    state.apply(ready_with(None, vec![builder]));
    state.seed_installed_catalog(BackendKind::Codex, catalog);
    assert_eq!(level(&mut state, "gpt-sol"), Some(Effort::Low));
}

#[test]
fn the_clis_configured_model_is_current_but_the_default_flag_is_not() {
    let mut state = AppState::new(Vec::new());
    state.apply(ready_with(
        None,
        vec![summary(
            "builder",
            "Builder",
            BackendKind::Codex,
            MemberStatus::Idle,
        )],
    ));
    state.seed_installed_catalog(BackendKind::Codex, native_catalog());
    state.open_model_picker("");

    // `gpt-sol` is what Codex is configured to run, so it is current even
    // though the catalog flags `gpt-5.6` as its default: the CLI's own
    // setting outranks the catalog default.
    assert_eq!(names(&state), ["gpt-5.6", "gpt-sol"]);
    let view = state.model_picker_view().unwrap();
    let trailing: Vec<String> = view
        .rows
        .iter()
        .filter_map(|row| match row {
            PickerRowView::Entry { trailing, .. } => Some(trailing.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(trailing, ["", "current"]);

    // Applying it pins the model and the `high` the CLI is configured with,
    // which the row displays.
    let (command, notice) = state.selected_model_command().expect("command");
    let builder = &command_members(command)[0];
    assert_eq!(builder.model.as_deref(), Some("gpt-sol"));
    assert_eq!(builder.effort, Some(Effort::High));
    assert_eq!(notice, "Model: Builder → codex · gpt-sol · high");

    // A configured model the catalog does not list marks nothing current.
    let mut catalog = native_catalog();
    catalog.native_model = Some("not-in-catalog".to_string());
    state.seed_installed_catalog(BackendKind::Codex, catalog);
    state.open_model_picker("");
    assert_eq!(names(&state), ["gpt-5.6", "gpt-sol"]);
    assert_eq!(selected_name(&state), "gpt-5.6");
}

fn agy_variant(id: &str, name: &str, effort: Effort) -> DiscoveredModel {
    let mut model = DiscoveredModel::simple(id);
    model.name = name.to_string();
    model.supported_efforts = vec![effort];
    model.default_effort = Some(effort);
    model
}

/// What `agy models` prints, in its order.
fn agy_catalog() -> DiscoveredCatalog {
    let mut sonnet = DiscoveredModel::simple("claude-sonnet-4-6");
    sonnet.name = "Claude Sonnet 4.6 (Thinking)".to_string();
    let mut opus = DiscoveredModel::simple("claude-opus-4-6-thinking");
    opus.name = "Claude Opus 4.6 (Thinking)".to_string();
    DiscoveredCatalog {
        models: vec![
            agy_variant(
                "gemini-3.8-flash-high",
                "Gemini 3.8 Flash (High)",
                Effort::High,
            ),
            agy_variant(
                "gemini-3.8-flash-medium",
                "Gemini 3.8 Flash (Medium)",
                Effort::Medium,
            ),
            agy_variant(
                "gemini-3.8-flash-low",
                "Gemini 3.8 Flash (Low)",
                Effort::Low,
            ),
            sonnet,
            opus,
            agy_variant(
                "gpt-oss-120b-medium",
                "GPT-OSS 120B (Medium)",
                Effort::Medium,
            ),
        ],
        ..Default::default()
    }
}

fn agy_state(pinned: Option<&str>, catalog: DiscoveredCatalog) -> AppState {
    let mut state = AppState::new(Vec::new());
    let mut builder = summary("builder", "Builder", BackendKind::Agy, MemberStatus::Idle);
    builder.model = pinned.map(str::to_string);
    state.apply(ready_with(None, vec![builder]));
    state.seed_installed_catalog(BackendKind::Agy, catalog);
    state
}

fn level_of(state: &AppState) -> Option<Effort> {
    let entry = state.selected_model_entry().unwrap();
    state.model_picker_effort(&entry)
}

#[test]
fn agy_effort_variants_merge_into_one_row_and_other_models_stay_alone() {
    let mut state = agy_state(None, agy_catalog());
    state.open_model_picker("");

    assert_eq!(
        names(&state),
        [
            "Gemini 3.8 Flash",
            "Claude Sonnet 4.6 (Thinking)",
            "Claude Opus 4.6 (Thinking)",
            "GPT-OSS 120B (Medium)",
        ]
    );
    let entries = state.model_picker_entries();
    let merged = &entries[0];
    assert_eq!(merged.key.model, "gemini-3.8-flash");
    assert_eq!(merged.levels, [Effort::Low, Effort::Medium, Effort::High]);
    assert_eq!(
        merged.variants,
        [
            (Effort::Low, "gemini-3.8-flash-low".to_string()),
            (Effort::Medium, "gemini-3.8-flash-medium".to_string()),
            (Effort::High, "gemini-3.8-flash-high".to_string()),
        ]
    );
    // The lone `-medium` model is not grouped and, with one level, shows none.
    let lone = &entries[3];
    assert_eq!(lone.key.model, "gpt-oss-120b-medium");
    assert!(lone.variants.is_empty() && lone.levels.is_empty());
    // `…-thinking` is not an effort suffix.
    assert_eq!(entries[2].key.model, "claude-opus-4-6-thinking");
    assert!(entries[2].levels.is_empty());
}

#[test]
fn a_merged_row_starts_on_medium_and_cycles_through_variant_ids() {
    let mut state = agy_state(None, agy_catalog());
    state.open_model_picker("gemini");
    let variant = |state: &AppState| {
        let entry = state.selected_model_entry().unwrap();
        entry.model_id(state.model_picker_effort(&entry))
    };

    assert_eq!(level_of(&state), Some(Effort::Medium));
    assert_eq!(variant(&state), "gemini-3.8-flash-medium");
    state.cycle_model_effort(true);
    assert_eq!(level_of(&state), Some(Effort::High));
    assert_eq!(variant(&state), "gemini-3.8-flash-high");
    state.cycle_model_effort(true);
    assert_eq!(level_of(&state), Some(Effort::Low));
    assert_eq!(variant(&state), "gemini-3.8-flash-low");
    state.cycle_model_effort(false);
    assert_eq!(level_of(&state), Some(Effort::High));
}

#[test]
fn applying_a_merged_row_pins_the_selected_variant_and_writes_no_effort() {
    let mut state = agy_state(None, agy_catalog());
    state.open_model_picker("gemini");
    state.cycle_model_effort(true);

    let (command, notice) = state.selected_model_command().expect("command");

    let builder = &command_members(command)[0];
    assert_eq!(builder.model.as_deref(), Some("gemini-3.8-flash-high"));
    assert_eq!(builder.effort, None);
    assert_eq!(notice, "Model: Builder → agy · gemini-3.8-flash-high");

    // Even a member that had an effort does not get one alongside a variant.
    let mut member = summary("builder", "Builder", BackendKind::Agy, MemberStatus::Idle);
    member.model = Some("gemini-3.8-flash-low".to_string());
    member.effort = Some(Effort::High);
    state.apply(ready_with(None, vec![member]));
    state.open_model_picker("gemini");
    let (command, _) = state.selected_model_command().expect("command");
    let builder = &command_members(command)[0];
    assert_eq!(builder.model.as_deref(), Some("gemini-3.8-flash-low"));
    assert_eq!(builder.effort, None);
}

#[test]
fn a_pinned_variant_makes_the_merged_row_current_on_its_level() {
    let mut state = agy_state(Some("gemini-3.8-flash-low"), agy_catalog());
    state.open_model_picker("");

    assert_eq!(selected_name(&state), "Gemini 3.8 Flash");
    assert_eq!(level_of(&state), Some(Effort::Low));
    let view = state.model_picker_view().unwrap();
    let PickerRowView::Entry { trailing, .. } = &view.rows[1] else {
        panic!("model row");
    };
    assert_eq!(trailing, "current");
}

#[test]
fn the_configured_variant_seeds_the_level_only_for_an_unpinned_member() {
    let mut catalog = agy_catalog();
    catalog.native_model = Some("gemini-3.8-flash-high".to_string());
    let mut state = agy_state(None, catalog.clone());
    state.open_model_picker("");
    assert_eq!(names(&state)[0], "Gemini 3.8 Flash");
    assert_eq!(level_of(&state), Some(Effort::High));
    let view = state.model_picker_view().unwrap();
    let PickerRowView::Entry { trailing, .. } = &view.rows[1] else {
        panic!("model row");
    };
    assert_eq!(trailing, "current");

    // A pinned member is not moved by the CLI's configuration.
    let mut state = agy_state(Some("claude-sonnet-4-6"), catalog);
    state.open_model_picker("gemini");
    assert_eq!(level_of(&state), Some(Effort::Medium));
    let view = state.model_picker_view().unwrap();
    let PickerRowView::Entry { trailing, .. } = &view.rows[1] else {
        panic!("model row");
    };
    assert_eq!(trailing, "");
}

#[test]
fn a_merged_rows_identity_survives_level_changes() {
    let mut state = agy_state(None, agy_catalog());
    state.open_model_picker("");
    state.select_next_model();
    state.select_previous_model();
    let before = (state.model_picker_position(), selected_name(&state));

    state.cycle_model_effort(true);
    state.cycle_model_effort(true);

    assert_eq!(
        (state.model_picker_position(), selected_name(&state)),
        before
    );
}

#[test]
fn only_exactly_suffixed_siblings_merge_and_single_levels_show_none() {
    // Codex-style models report their own levels: `-high`/`-low` ids that
    // also support other levels are not variants of one model.
    let mut state = AppState::new(Vec::new());
    state.apply(ready_with(
        None,
        vec![summary(
            "builder",
            "Builder",
            BackendKind::Codex,
            MemberStatus::Idle,
        )],
    ));
    state.seed_installed_catalog(
        BackendKind::Codex,
        DiscoveredCatalog {
            models: vec![
                model("m-low", &[Effort::Low, Effort::High], false),
                model("m-high", &[Effort::Low, Effort::High], false),
                model("solo", &[Effort::High], false),
            ],
            ..Default::default()
        },
    );
    state.open_model_picker("");

    assert_eq!(names(&state), ["m-low", "m-high", "solo"]);
    let entries = state.model_picker_entries();
    assert!(entries.iter().all(|entry| entry.variants.is_empty()));
    assert_eq!(entries[0].levels, [Effort::Low, Effort::High]);
    // One level is no choice.
    assert!(entries[2].levels.is_empty());
}

#[test]
fn claude_models_seed_the_configured_effort_for_aliases_and_full_ids() {
    use crate::adapter::models::NativeEfforts;
    let claude_model = |id: &str| {
        let mut model = DiscoveredModel::simple(id);
        model.supported_efforts = vec![
            Effort::Low,
            Effort::Medium,
            Effort::High,
            Effort::Xhigh,
            Effort::Max,
        ];
        model
    };
    let mut state = AppState::new(Vec::new());
    state.apply(ready_with(
        None,
        vec![summary(
            "builder",
            "Builder",
            BackendKind::Claude,
            MemberStatus::Idle,
        )],
    ));
    state.seed_installed_catalog(
        BackendKind::Claude,
        DiscoveredCatalog {
            models: vec![
                claude_model("opus"),
                claude_model("sonnet"),
                claude_model("claude-opus-5-5"),
            ],
            native_model: Some("opus".to_string()),
            native_efforts: NativeEfforts {
                global: None,
                by_model: vec![
                    ("claude-opus-5-5".to_string(), Effort::High),
                    ("claude-sonnet-5-5".to_string(), Effort::Low),
                ],
            },
            ..Default::default()
        },
    );
    let level = |state: &mut AppState, filter: &str| {
        state.open_model_picker(filter);
        let entry = state.selected_model_entry().unwrap();
        assert_eq!(entry.levels.len(), 5);
        state.model_picker_effort(&entry)
    };

    assert_eq!(level(&mut state, "opus"), Some(Effort::High));
    assert_eq!(level(&mut state, "claude-opus-5-5"), Some(Effort::High));
    assert_eq!(level(&mut state, "sonnet"), Some(Effort::Low));
    // The configured model leads the list as current.
    state.open_model_picker("");
    assert_eq!(names(&state)[0], "opus");
}

#[test]
fn model_change_on_a_viewed_sub_agent_edits_only_that_sub_agent() {
    use super::focus::child_summary;
    let mut state = AppState::new(Vec::new());
    state.apply(ready_with(
        Some(DefaultTarget::Member(MemberId::new("builder"))),
        vec![
            summary("builder", "Builder", BackendKind::Codex, MemberStatus::Idle),
            child_summary("scout", "Scout", "builder", MemberStatus::Idle),
        ],
    ));
    state.detected_backends = Some(all_detected());
    state.seed_installed_catalog(
        BackendKind::Claude,
        DiscoveredCatalog {
            models: vec![model("opus", &[], true)],
            ..Default::default()
        },
    );
    state.set_focus(Some(MemberId::new("scout")));
    state.open_model_picker("opus");

    let (command, notice) = state.selected_model_command().expect("command");

    let members = command_members(command);
    assert_eq!(members.len(), 2);
    assert_eq!(members[0].model, None);
    assert_eq!(members[0].parent, None);
    assert_eq!(members[1].model.as_deref(), Some("opus"));
    assert_eq!(members[1].parent, Some(MemberId::new("builder")));
    assert_eq!(notice, "Model: Scout → claude · opus");
}

#[test]
fn team_editor_lists_top_level_members_only() {
    use super::focus::child_summary;
    let mut state = AppState::new(Vec::new());
    state.apply(ready_with(
        None,
        vec![
            summary("builder", "Builder", BackendKind::Codex, MemberStatus::Idle),
            child_summary("scout", "Scout", "builder", MemberStatus::Idle),
            summary(
                "reviewer",
                "Reviewer",
                BackendKind::Claude,
                MemberStatus::Idle,
            ),
        ],
    ));

    state.toggle_drawer(Drawer::Team);

    let editor = state.team_editor().expect("editor");
    let ids: Vec<&str> = editor.members().iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["builder", "reviewer"]);
}

#[test]
fn the_runtime_is_given_the_discovered_models_once_and_again_when_they_change() {
    let mut state = model_state();

    let first = state.pending_model_directory().expect("catalogs are ready");
    assert_eq!(first.installed, Some(all_detected()));
    let codex: Vec<&str> = first
        .models(BackendKind::Codex)
        .expect("codex catalog")
        .iter()
        .map(|model| model.id.as_str())
        .collect();
    assert_eq!(codex, ["gpt-5.6", "gpt-mini"]);
    assert!(first.models(BackendKind::Grok).is_none());

    state.mark_model_directory_sent(first);
    assert!(state.pending_model_directory().is_none(), "nothing changed");

    // A catalog that is being reloaded keeps what the runtime already has.
    let cwd = PathBuf::from("/tmp/ws");
    state.model_catalog.seed_loading(BackendKind::Codex, &cwd);
    assert!(state.pending_model_directory().is_none());

    // A newly landed catalog is sent.
    state.model_catalog.seed_catalog(
        BackendKind::Grok,
        &cwd,
        DiscoveredCatalog {
            models: vec![model("grok-4.5", &[], true)],
            ..Default::default()
        },
    );
    let next = state.pending_model_directory().expect("grok landed");
    assert!(next.models(BackendKind::Grok).is_some());
    assert!(next.models(BackendKind::Codex).is_some());
}
