//! One-time cleanup of the files earlier Asterline versions put inside the
//! user's project folder.
//!
//! Older builds kept `<workspace>/.asterline/` (database, `team.json`,
//! `roster.md`, Agy logs) and generated `<workspace>/.agents/skills/
//! asterline-team/SKILL.md` and `.../asterline-brainstorm/SKILL.md`. State now
//! lives under the Asterline home (see [`crate::state_paths`]). This moves the
//! old state over and removes the generated skill files, touching nothing that
//! is not provably Asterline's.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::domain::config::{
    ASTERLINE_BRAINSTORM_SKILL_NAME, ASTERLINE_TEAM_SKILL_NAME, embedded_brainstorm_skill,
    is_managed_skill,
};
use crate::fs_safety;
use crate::state_paths::{self, DB_FILE};

const OLD_STATE_DIR: &str = ".asterline";
/// Written into the new project directory once the "both exist" warning has
/// been shown, so it is not repeated on every launch.
const NOTED_MARKER: &str = "legacy-state-noted";

/// Move old in-project state to the Asterline home and remove generated skill
/// files. Returns messages worth showing the user; failures are reported, never
/// fatal, because the new location works on its own.
pub fn migrate_workspace(workspace: &Path) -> Vec<String> {
    let mut notices = Vec::new();
    match migrate_state(workspace) {
        Ok(Some(notice)) => notices.push(notice),
        Ok(None) => {}
        Err(err) => notices.push(format!(
            "could not move the old {OLD_STATE_DIR} folder out of {}: {err}",
            workspace.display()
        )),
    }
    match remove_generated_skills(workspace, &state_paths::skills_dir()) {
        Ok(Some(notice)) => notices.push(notice),
        Ok(None) => {}
        Err(err) => notices.push(format!(
            "could not clean up the old Asterline skill files in {}: {err}",
            workspace.display()
        )),
    }
    notices
}

fn migrate_state(workspace: &Path) -> io::Result<Option<String>> {
    let old = workspace.join(OLD_STATE_DIR);
    match fs::symlink_metadata(&old) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            return Ok(Some(format!(
                "left {} alone: it is not a plain folder",
                old.display()
            )));
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    }
    let new = state_paths::ensure_project_dir(workspace)?;

    if fs_safety::regular_file_exists(&new.join(DB_FILE), "SQLite database")? {
        let marker = new.join(NOTED_MARKER);
        if marker.exists() {
            return Ok(None);
        }
        fs_safety::write_regular_file(&marker, "migration marker", &old.display().to_string())?;
        return Ok(Some(format!(
            "Asterline state now lives in {}. The older {} was left untouched; delete it when you no longer need it.",
            new.display(),
            old.display()
        )));
    }

    if old_state_in_use(&old) {
        return Ok(Some(format!(
            "{} is in use by another Asterline instance; its state was not moved. Close that instance and restart.",
            old.display()
        )));
    }

    drop_pasted_copies(&old);
    let mut entries: Vec<PathBuf> = fs::read_dir(&old)?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .collect();
    // The database goes last, after its -wal and -shm, so an interrupted move
    // leaves nothing that looks like a finished migration.
    entries.sort_by_key(|path| {
        let name = file_name(path);
        (name == DB_FILE, name)
    });

    let mut left_behind = Vec::new();
    for source in entries {
        let target = new.join(file_name(&source));
        if fs::symlink_metadata(&target).is_ok() {
            left_behind.push(file_name(&source));
            continue;
        }
        move_entry(&source, &target)?;
    }
    if left_behind.is_empty() {
        fs::remove_dir(&old)?;
        Ok(Some(format!(
            "Moved Asterline state from {} to {}.",
            old.display(),
            new.display()
        )))
    } else {
        Ok(Some(format!(
            "Moved Asterline state to {}; {} already existed there, so {} was kept in {}.",
            new.display(),
            left_behind.join(", "),
            if left_behind.len() == 1 {
                "its old copy"
            } else {
                "their old copies"
            },
            old.display()
        )))
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Earlier builds left `pasted-*` image copies here; they are scratch files.
fn drop_pasted_copies(old: &Path) {
    let pasted = old.join("pasted");
    let Ok(entries) = fs::read_dir(&pasted) else {
        return;
    };
    for entry in entries.flatten() {
        if file_name(&entry.path()).starts_with("pasted-") {
            let _ = fs::remove_file(entry.path());
        }
    }
    let _ = fs::remove_dir(&pasted);
}

/// Rename, or copy then delete when the two folders are on different
/// filesystems. A copied file appears under its final name only once complete.
fn move_entry(source: &Path, target: &Path) -> io::Result<()> {
    if fs::rename(source, target).is_ok() {
        return Ok(());
    }
    copy_entry(source, target)?;
    let metadata = fs::symlink_metadata(source)?;
    if metadata.is_dir() {
        fs::remove_dir_all(source)
    } else {
        fs::remove_file(source)
    }
}

fn copy_entry(source: &Path, target: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    if metadata.file_type().is_symlink() {
        #[cfg(unix)]
        {
            return std::os::unix::fs::symlink(fs::read_link(source)?, target);
        }
        #[cfg(not(unix))]
        return Err(io::Error::other(format!(
            "cannot move symlink {}",
            source.display()
        )));
    }
    if metadata.is_dir() {
        fs::create_dir(target)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            copy_entry(&entry.path(), &target.join(entry.file_name()))?;
        }
        return Ok(());
    }
    let mut partial = target.as_os_str().to_os_string();
    partial.push(".migrating");
    let partial = PathBuf::from(partial);
    let result = fs::copy(source, &partial).and_then(|_| fs::rename(&partial, target));
    if result.is_err() {
        let _ = fs::remove_file(&partial);
    }
    result
}

/// True when another process holds the old store's instance lock.
fn old_state_in_use(old: &Path) -> bool {
    let lock = old.join(format!("{DB_FILE}.lock"));
    if !lock.is_file() {
        return false;
    }
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(0);
    }
    let Ok(file) = options.open(&lock) else {
        // On Windows a held lock makes the open itself fail.
        return cfg!(windows);
    };
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) != 0 }
    }
    #[cfg(not(unix))]
    {
        let _ = file;
        false
    }
}

/// The `name:` value in a skill file's frontmatter.
fn frontmatter_name(text: &str) -> Option<&str> {
    let mut lines = text.lines();
    if lines.next()?.trim() != "---" {
        return None;
    }
    for line in lines {
        let line = line.trim();
        if line == "---" {
            return None;
        }
        if let Some(value) = line.strip_prefix("name:") {
            return Some(value.trim().trim_matches(['"', '\'']));
        }
    }
    None
}

/// A directory that is a real directory, not a symlink.
fn is_plain_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir())
}

/// Remove the skill files Asterline generated into the project, then the
/// folders that are left empty. Files with other content stay.
///
/// `shared_skills` is where the shared copies live; a customized brainstorm
/// skill is carried over there instead of being lost.
fn remove_generated_skills(workspace: &Path, shared_skills: &Path) -> io::Result<Option<String>> {
    let agents = workspace.join(".agents");
    let skills = agents.join("skills");
    if !is_plain_dir(&agents) || !is_plain_dir(&skills) {
        return Ok(None);
    }
    let mut removed_any = false;
    let mut kept = Vec::new();
    for name in [ASTERLINE_TEAM_SKILL_NAME, ASTERLINE_BRAINSTORM_SKILL_NAME] {
        let directory = skills.join(name);
        let file = directory.join("SKILL.md");
        if !is_plain_dir(&directory) || !fs_safety::regular_file_exists(&file, "skill")? {
            continue;
        }
        let Ok(text) = fs_safety::read_regular_to_string(&file, "skill") else {
            continue;
        };
        if frontmatter_name(&text) != Some(name) {
            continue;
        }
        if name == ASTERLINE_TEAM_SKILL_NAME && !is_managed_skill(&text) {
            // A hand-written team skill: not ours to delete.
            kept.push(file);
            continue;
        }
        if name == ASTERLINE_BRAINSTORM_SKILL_NAME && text != embedded_brainstorm_skill() {
            // A customized brainstorm skill. Carry it over as the shared copy
            // when there is none yet; otherwise leave it where it is.
            let shared = shared_skills.join(name).join("SKILL.md");
            if fs::symlink_metadata(&shared).is_ok() {
                kept.push(file);
                continue;
            }
            if shared_skills == state_paths::skills_dir() {
                state_paths::ensure_skills_root()?;
            }
            let target_dir = fs_safety::ensure_workspace_directory(shared_skills, &[name], true)?;
            fs_safety::write_regular_file_atomic(
                &target_dir.join("SKILL.md"),
                "brainstorm skill",
                &text,
            )?;
        }
        fs::remove_file(&file)?;
        removed_any = true;
        let _ = fs::remove_dir(&directory);
    }
    if removed_any {
        // `remove_dir` only succeeds on an empty directory.
        let _ = fs::remove_dir(&skills);
        let _ = fs::remove_dir(&agents);
    }
    Ok(if kept.is_empty() {
        None
    } else {
        Some(format!(
            "left {} in place: it was edited, and Asterline now keeps its skills under {}.",
            kept.iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", "),
            state_paths::skills_dir().display()
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::config::team_skill_path;

    fn clean_skills(workspace: &Path) -> Option<String> {
        remove_generated_skills(workspace, &workspace.join("shared-skills")).unwrap()
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("asterline-mig-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn generated_team_skill() -> String {
        crate::domain::config::embedded_team_skill().to_string()
    }

    #[test]
    fn moves_the_database_team_config_and_roster_and_the_chat_resumes() {
        use crate::domain::event::ChatItem;
        use crate::store::sqlite::SqliteStore;

        let workspace = scratch("move");
        let old = workspace.join(".asterline");
        fs::create_dir_all(old.join("agy")).unwrap();
        fs::write(old.join("team.json"), "{\"team\":1}").unwrap();
        fs::write(old.join("roster.md"), "# roster").unwrap();
        fs::write(old.join("agy/main-1.log"), "log").unwrap();
        {
            let store = SqliteStore::open(old.join(DB_FILE)).unwrap();
            store.current_conversation().unwrap();
            let turn = store.create_turn().unwrap();
            store
                .record_user(
                    turn,
                    &[crate::domain::MemberId::new("main")],
                    "remember me after the move",
                )
                .unwrap();
        }
        // A finished session may leave its lock file behind; a stale one must
        // not block the move.
        fs::write(old.join("asterline.sqlite3.lock"), "pid=1").unwrap();

        let notices = migrate_workspace(&workspace);
        let new = state_paths::project_dir(&workspace);

        assert!(!old.exists(), "{notices:?}");
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert_eq!(
            fs::read_to_string(new.join("team.json")).unwrap(),
            "{\"team\":1}"
        );
        assert_eq!(
            fs::read_to_string(new.join("roster.md")).unwrap(),
            "# roster"
        );
        assert!(new.join("agy/main-1.log").is_file());
        let store = SqliteStore::open(new.join(DB_FILE)).unwrap();
        store
            .set_conversation(store.current_conversation().unwrap())
            .unwrap();
        assert!(store.replay_chat().unwrap().iter().any(|item| matches!(
            item,
            ChatItem::User { body, .. } if body == "remember me after the move"
        )));
        // A second launch has nothing left to do.
        drop(store);
        assert!(migrate_workspace(&workspace).is_empty());

        fs::remove_dir_all(&workspace).ok();
        fs::remove_dir_all(new).ok();
    }

    #[test]
    fn the_wal_and_shm_files_move_together_with_the_database() {
        let workspace = scratch("wal");
        let old = workspace.join(".asterline");
        fs::create_dir_all(&old).unwrap();
        for name in [
            "asterline.sqlite3",
            "asterline.sqlite3-wal",
            "asterline.sqlite3-shm",
        ] {
            fs::write(old.join(name), name).unwrap();
        }

        migrate_workspace(&workspace);

        let new = state_paths::project_dir(&workspace);
        for name in [
            "asterline.sqlite3",
            "asterline.sqlite3-wal",
            "asterline.sqlite3-shm",
        ] {
            assert_eq!(fs::read_to_string(new.join(name)).unwrap(), name);
        }
        fs::remove_dir_all(&workspace).ok();
        fs::remove_dir_all(new).ok();
    }

    #[test]
    fn copy_fallback_moves_files_and_folders() {
        let root = scratch("copy");
        let source = root.join("from");
        fs::create_dir_all(source.join("sub")).unwrap();
        fs::write(source.join("sub/file.txt"), "data").unwrap();
        fs::write(source.join("top.txt"), "top").unwrap();

        copy_entry(&source, &root.join("to")).unwrap();
        fs::remove_dir_all(&source).unwrap();

        assert_eq!(
            fs::read_to_string(root.join("to/sub/file.txt")).unwrap(),
            "data"
        );
        assert_eq!(fs::read_to_string(root.join("to/top.txt")).unwrap(), "top");
        assert!(!root.join("to/top.txt.migrating").exists());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn when_both_exist_the_new_state_wins_and_the_old_folder_is_untouched() {
        let workspace = scratch("both");
        let old = workspace.join(".asterline");
        fs::create_dir_all(&old).unwrap();
        fs::write(old.join(DB_FILE), "old db").unwrap();
        fs::write(old.join("team.json"), "old team").unwrap();
        let new = state_paths::ensure_project_dir(&workspace).unwrap();
        fs::write(new.join(DB_FILE), "new db").unwrap();

        let first = migrate_workspace(&workspace);
        assert_eq!(first.len(), 1, "{first:?}");
        assert!(first[0].contains("left untouched"), "{first:?}");
        assert_eq!(fs::read_to_string(old.join(DB_FILE)).unwrap(), "old db");
        assert_eq!(
            fs::read_to_string(old.join("team.json")).unwrap(),
            "old team"
        );
        assert_eq!(fs::read_to_string(new.join(DB_FILE)).unwrap(), "new db");
        assert!(!new.join("team.json").exists());

        // The warning is shown once.
        assert!(migrate_workspace(&workspace).is_empty());
        fs::remove_dir_all(&workspace).ok();
        fs::remove_dir_all(new).ok();
    }

    #[test]
    fn an_existing_new_file_is_not_overwritten_by_the_old_one() {
        let workspace = scratch("conflict");
        let old = workspace.join(".asterline");
        fs::create_dir_all(&old).unwrap();
        fs::write(old.join(DB_FILE), "old db").unwrap();
        fs::write(old.join("team.json"), "old team").unwrap();
        let new = state_paths::ensure_project_dir(&workspace).unwrap();
        fs::write(new.join("team.json"), "new team").unwrap();

        let notices = migrate_workspace(&workspace);

        assert_eq!(fs::read_to_string(new.join(DB_FILE)).unwrap(), "old db");
        assert_eq!(
            fs::read_to_string(new.join("team.json")).unwrap(),
            "new team"
        );
        assert_eq!(
            fs::read_to_string(old.join("team.json")).unwrap(),
            "old team"
        );
        assert!(notices[0].contains("team.json"), "{notices:?}");
        fs::remove_dir_all(&workspace).ok();
        fs::remove_dir_all(new).ok();
    }

    #[cfg(unix)]
    #[test]
    fn state_held_by_another_instance_is_not_moved() {
        use std::os::fd::AsRawFd;

        let workspace = scratch("busy");
        let old = workspace.join(".asterline");
        fs::create_dir_all(&old).unwrap();
        fs::write(old.join(DB_FILE), "old db").unwrap();
        let lock = fs::File::create(old.join("asterline.sqlite3.lock")).unwrap();
        assert_eq!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );

        let notices = migrate_workspace(&workspace);

        assert!(notices[0].contains("in use"), "{notices:?}");
        assert!(old.join(DB_FILE).is_file());
        assert!(!state_paths::project_dir(&workspace).join(DB_FILE).exists());
        drop(lock);
        fs::remove_dir_all(&workspace).ok();
        fs::remove_dir_all(state_paths::project_dir(&workspace)).ok();
    }

    #[test]
    fn a_workspace_without_old_state_is_left_alone() {
        let workspace = scratch("clean");
        assert!(migrate_workspace(&workspace).is_empty());
        assert_eq!(fs::read_dir(&workspace).unwrap().count(), 0);
        fs::remove_dir_all(&workspace).ok();
    }

    #[test]
    fn generated_skills_are_removed_with_their_now_empty_folders() {
        let workspace = scratch("skills");
        let skills = workspace.join(".agents/skills");
        fs::create_dir_all(skills.join("asterline-team")).unwrap();
        fs::create_dir_all(skills.join("asterline-brainstorm")).unwrap();
        fs::write(
            skills.join("asterline-team/SKILL.md"),
            generated_team_skill(),
        )
        .unwrap();
        fs::write(
            skills.join("asterline-brainstorm/SKILL.md"),
            embedded_brainstorm_skill(),
        )
        .unwrap();

        assert_eq!(clean_skills(&workspace), None);

        assert!(!workspace.join(".agents").exists());
        fs::remove_dir_all(&workspace).ok();
    }

    #[test]
    fn user_files_next_to_generated_skills_survive() {
        let workspace = scratch("skills-user");
        let skills = workspace.join(".agents/skills");
        fs::create_dir_all(skills.join("asterline-team")).unwrap();
        fs::create_dir_all(skills.join("my-skill")).unwrap();
        fs::create_dir_all(skills.join("asterline-brainstorm/agents")).unwrap();
        fs::write(
            skills.join("asterline-team/SKILL.md"),
            generated_team_skill(),
        )
        .unwrap();
        fs::write(skills.join("asterline-team/notes.md"), "mine").unwrap();
        fs::write(
            skills.join("my-skill/SKILL.md"),
            "---\nname: my-skill\n---\n",
        )
        .unwrap();
        fs::write(
            skills.join("asterline-brainstorm/SKILL.md"),
            embedded_brainstorm_skill(),
        )
        .unwrap();
        fs::write(skills.join("asterline-brainstorm/agents/openai.yaml"), "x").unwrap();

        clean_skills(&workspace);

        assert!(!skills.join("asterline-team/SKILL.md").exists());
        assert!(!skills.join("asterline-brainstorm/SKILL.md").exists());
        assert_eq!(
            fs::read_to_string(skills.join("asterline-team/notes.md")).unwrap(),
            "mine"
        );
        assert!(
            skills
                .join("asterline-brainstorm/agents/openai.yaml")
                .is_file()
        );
        assert!(skills.join("my-skill/SKILL.md").is_file());
        fs::remove_dir_all(&workspace).ok();
    }

    #[test]
    fn files_that_only_look_similar_are_not_removed() {
        let workspace = scratch("skills-lookalike");
        let skills = workspace.join(".agents/skills");
        fs::create_dir_all(skills.join("asterline-team")).unwrap();
        fs::create_dir_all(skills.join("asterline-brainstorm")).unwrap();
        // No frontmatter name.
        fs::write(skills.join("asterline-team/SKILL.md"), "my own notes").unwrap();
        // A different name.
        fs::write(
            skills.join("asterline-brainstorm/SKILL.md"),
            "---\nname: other\n---\n",
        )
        .unwrap();

        let notices = clean_skills(&workspace);

        assert!(notices.is_none(), "{notices:?}");
        assert!(skills.join("asterline-team/SKILL.md").is_file());
        assert!(skills.join("asterline-brainstorm/SKILL.md").is_file());

        // Right name, but written by hand (no managed marker).
        let handwritten = "---\nname: asterline-team\n---\n# mine\n";
        fs::write(skills.join("asterline-team/SKILL.md"), handwritten).unwrap();
        let notices = clean_skills(&workspace);
        assert!(notices.unwrap().contains("edited"));
        assert_eq!(
            fs::read_to_string(skills.join("asterline-team/SKILL.md")).unwrap(),
            handwritten
        );
        fs::remove_dir_all(&workspace).ok();
    }

    #[test]
    fn a_customized_brainstorm_skill_becomes_the_shared_copy_when_there_is_none() {
        let workspace = scratch("skills-custom");
        let skills = workspace.join(".agents/skills/asterline-brainstorm");
        fs::create_dir_all(&skills).unwrap();
        let custom = format!("{}\nCustom house rule.\n", embedded_brainstorm_skill());
        fs::write(skills.join("SKILL.md"), &custom).unwrap();
        let shared_root = scratch("skills-custom-shared");
        let shared = shared_root.join("asterline-brainstorm/SKILL.md");

        let notice = remove_generated_skills(&workspace, &shared_root).unwrap();

        assert_eq!(notice, None);
        assert!(!workspace.join(".agents").exists());
        assert_eq!(fs::read_to_string(&shared).unwrap(), custom);

        // With a shared copy already present, a customized one stays put.
        fs::create_dir_all(&skills).unwrap();
        fs::write(skills.join("SKILL.md"), &custom).unwrap();
        let notice = remove_generated_skills(&workspace, &shared_root).unwrap();
        assert!(notice.unwrap().contains("edited"));
        assert!(skills.join("SKILL.md").is_file());
        fs::remove_dir_all(&shared_root).ok();
        fs::remove_dir_all(&workspace).ok();
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_agents_folder_is_not_followed() {
        let root = scratch("skills-link");
        let workspace = root.join("ws");
        let elsewhere = root.join("shared/.agents");
        fs::create_dir_all(elsewhere.join("skills/asterline-team")).unwrap();
        fs::create_dir_all(&workspace).unwrap();
        fs::write(
            elsewhere.join("skills/asterline-team/SKILL.md"),
            generated_team_skill(),
        )
        .unwrap();
        std::os::unix::fs::symlink(&elsewhere, workspace.join(".agents")).unwrap();

        clean_skills(&workspace);

        assert!(elsewhere.join("skills/asterline-team/SKILL.md").is_file());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn frontmatter_name_reads_only_the_frontmatter() {
        assert_eq!(frontmatter_name("---\nname: a\n---\nname: b"), Some("a"));
        assert_eq!(frontmatter_name("---\nname: \"a\"\n---\n"), Some("a"));
        assert_eq!(frontmatter_name("name: a\n"), None);
        assert_eq!(frontmatter_name("---\ndescription: x\n---\nname: a"), None);
    }

    #[test]
    fn the_shared_team_skill_path_is_under_the_asterline_home() {
        assert!(team_skill_path().starts_with(state_paths::asterline_home()));
    }
}
