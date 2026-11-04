//! Where Asterline keeps its own files.
//!
//! Nothing is written into the user's project folder. Everything lives under
//! one home directory, `~/.asterline` (override with `ASTERLINE_HOME`):
//!
//! ```text
//! ~/.asterline/
//!   skills/asterline-team/SKILL.md        shared by every project
//!   skills/asterline-brainstorm/SKILL.md
//!   skills/models.md                      models @@spawn accepts, kept current
//!   models.json                           model list cache, loaded first at launch
//!   projects/<key>/                       one directory per workspace
//!     asterline.sqlite3 (+ .lock, -wal, -shm)
//!     team.json  roster.md  agy/
//! ```
//!
//! `<key>` is the workspace's canonical absolute path with every character
//! that is not an ASCII letter, digit, `-` or `_` replaced by `-` (the same
//! scheme Claude Code uses for `~/.claude/projects`). A path with any
//! non-ASCII character also gets a short hash of the full path appended, so
//! two different non-ASCII folders cannot share a key.

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::{env, fs, io};

use sha2::{Digest, Sha256};

use crate::fs_safety;

/// Environment variable that relocates the whole Asterline home.
pub const HOME_ENV: &str = "ASTERLINE_HOME";

/// Keys longer than this are cut and given a hash, to stay under the usual
/// 255-byte file name limit.
const MAX_KEY_LEN: usize = 200;

pub const DB_FILE: &str = "asterline.sqlite3";
pub const TEAM_FILE: &str = "team.json";
pub const ROSTER_FILE: &str = "roster.md";
pub const MODELS_FILE: &str = "models.md";
pub const MODELS_CACHE_FILE: &str = "models.json";

/// The Asterline home directory. Not created here.
///
/// In unit tests, when `ASTERLINE_HOME` is unset, a per-process directory under
/// the system temp directory is used so a test run can never touch the real
/// `~/.asterline`.
pub fn asterline_home() -> PathBuf {
    let override_home = env::var_os(HOME_ENV);
    #[cfg(test)]
    if override_home.as_ref().is_none_or(|value| value.is_empty()) {
        return env::temp_dir().join(format!("asterline-test-home-{}", std::process::id()));
    }
    home_from_values(override_home, crate::domain::config::user_home_dir())
}

fn home_from_values(override_home: Option<OsString>, user_home: Option<PathBuf>) -> PathBuf {
    if let Some(path) = override_home.filter(|value| !value.is_empty()) {
        return PathBuf::from(path);
    }
    match user_home {
        Some(home) => home.join(".asterline"),
        // No home directory at all: stay out of the working directory.
        None => env::temp_dir().join("asterline"),
    }
}

/// `<home>/skills`, where the shared Asterline skills are installed.
pub fn skills_dir() -> PathBuf {
    asterline_home().join("skills")
}

/// `<home>/skills/models.md`: the models each installed backend offers,
/// rewritten whenever Asterline learns a new list. It sits in the skills
/// directory because every backend is already allowed to read that.
pub fn models_path() -> PathBuf {
    skills_dir().join(MODELS_FILE)
}

/// Remove the `<home>/models.md` that earlier versions wrote (only that exact
/// regular file; a symlink or directory of that name is left alone).
pub fn remove_legacy_models_file() {
    let legacy = asterline_home().join(MODELS_FILE);
    if fs::symlink_metadata(&legacy).is_ok_and(|meta| meta.file_type().is_file()) {
        let _ = fs::remove_file(legacy);
    }
}

/// `<home>/models.json`: the model list as of the last discovery, read first
/// at launch so the model pickers need not wait for the CLIs.
pub fn models_cache_path() -> PathBuf {
    asterline_home().join(MODELS_CACHE_FILE)
}

/// `<home>/projects/<key>` for `workspace`. Not created here.
pub fn project_dir(workspace: &Path) -> PathBuf {
    asterline_home()
        .join("projects")
        .join(project_key(workspace))
}

pub fn db_path(workspace: &Path) -> PathBuf {
    project_dir(workspace).join(DB_FILE)
}

pub fn team_config_path(workspace: &Path) -> PathBuf {
    project_dir(workspace).join(TEAM_FILE)
}

pub fn roster_path(workspace: &Path) -> PathBuf {
    project_dir(workspace).join(ROSTER_FILE)
}

pub fn agy_log_dir(workspace: &Path) -> PathBuf {
    project_dir(workspace).join("agy")
}

/// Directories outside the workspace that a backend CLI must be allowed to
/// read: the shared skills and this project's state (the roster). Passed to
/// the CLIs as `--add-dir`.
pub fn agent_access_dirs(workspace: &Path) -> Vec<PathBuf> {
    vec![skills_dir(), project_dir(workspace)]
}

/// Create `<home>/projects/<key>` (mode 0700) and return it. Refuses symlinked
/// or non-directory components below the home.
pub fn ensure_project_dir(workspace: &Path) -> io::Result<PathBuf> {
    let home = ensure_home()?;
    fs_safety::ensure_workspace_directory(&home, &["projects", &project_key(workspace)], true)
}

/// Create `<home>/skills` (mode 0700) and return it.
pub fn ensure_skills_root() -> io::Result<PathBuf> {
    let home = ensure_home()?;
    fs_safety::ensure_workspace_directory(&home, &["skills"], true)
}

/// Replace the file at `path` (a direct child of the home or of its skills
/// directory, such as [`models_path`]) with `contents`, atomically: a reader sees the old or the
/// new file, never half of one. Returns whether the file changed; identical
/// contents are left untouched.
pub fn write_state_file(path: &Path, label: &str, contents: &str) -> io::Result<bool> {
    if let Ok(existing) = fs_safety::read_regular_to_string(path, label)
        && existing == contents
    {
        return Ok(false);
    }
    match path.parent() {
        Some(parent) if parent == asterline_home() => {
            ensure_home()?;
        }
        Some(parent) if parent == skills_dir() => {
            ensure_skills_root()?;
        }
        Some(parent) => fs::create_dir_all(parent)?,
        None => {}
    }
    fs_safety::write_regular_file_atomic(path, label, contents)?;
    Ok(true)
}

/// Create the home directory with mode 0700 when it does not exist yet. An
/// existing directory (possibly a symlink the user set up) is left as it is.
fn ensure_home() -> io::Result<PathBuf> {
    let home = asterline_home();
    if !home.is_dir() {
        fs::create_dir_all(&home)?;
        fs_safety::restrict_new_directory(&home)?;
    }
    Ok(home)
}

/// The canonical absolute form of `workspace`. A path that does not exist (yet)
/// is made absolute and tidied lexically instead.
pub fn canonical_workspace(workspace: &Path) -> PathBuf {
    if let Ok(real) = fs::canonicalize(workspace) {
        return strip_verbatim_prefix(real);
    }
    let absolute = if workspace.is_absolute() {
        workspace.to_path_buf()
    } else {
        env::current_dir()
            .map(|cwd| cwd.join(workspace))
            .unwrap_or_else(|_| workspace.to_path_buf())
    };
    let mut tidy = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                tidy.pop();
            }
            other => tidy.push(other.as_os_str()),
        }
    }
    tidy
}

/// Windows `canonicalize` returns `\\?\C:\...`; drop the verbatim prefix.
fn strip_verbatim_prefix(path: PathBuf) -> PathBuf {
    match path.to_str().and_then(|text| text.strip_prefix(r"\\?\")) {
        Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
        _ => path,
    }
}

/// The directory name for `workspace` under `<home>/projects`.
pub fn project_key(workspace: &Path) -> String {
    let canonical = canonical_workspace(workspace);
    let text = canonical.to_string_lossy();
    let mut key = String::with_capacity(text.len());
    let mut non_ascii = false;
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            key.push(ch);
        } else {
            non_ascii |= !ch.is_ascii();
            key.push('-');
        }
    }
    if non_ascii || key.len() > MAX_KEY_LEN {
        key.truncate(MAX_KEY_LEN.min(key.len()));
        key.push('-');
        key.push_str(&short_hash(canonical.as_os_str().as_encoded_bytes()));
    }
    key
}

/// First 8 hex digits of the SHA-256 of `bytes`: stable across runs and builds.
fn short_hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .take(4)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn ascii_paths_are_munged_like_claude_code() {
        assert_eq!(
            project_key(Path::new("/home/pys/Pj/my_test/RM")),
            "-home-pys-Pj-my_test-RM"
        );
        assert_eq!(project_key(Path::new("/tmp/a b.c/d-e")), "-tmp-a-b-c-d-e");
    }

    #[cfg(windows)]
    #[test]
    fn windows_paths_keep_the_drive_in_the_key() {
        assert_eq!(
            project_key(Path::new(r"C:\home\pys\Pj\my_test\RM")),
            "C--home-pys-Pj-my_test-RM"
        );
        assert_eq!(
            project_key(Path::new("C:/tmp/a b.c/d-e")),
            "C--tmp-a-b-c-d-e"
        );
    }

    #[test]
    fn non_ascii_paths_get_a_stable_hash_suffix() {
        let (path, other, prefix) = if cfg!(windows) {
            (
                r"C:\home\pys\项目\甲",
                r"C:\home\pys\项目\乙",
                "C--home-pys-",
            )
        } else {
            ("/home/pys/项目/甲", "/home/pys/项目/乙", "-home-pys-")
        };
        let first = project_key(Path::new(path));
        let second = project_key(Path::new(other));
        assert_ne!(first, second, "different folders must not share a key");
        assert_eq!(first, project_key(Path::new(path)));
        assert!(first.starts_with(prefix), "{first}");
        let (_, hash) = first.rsplit_once('-').unwrap();
        assert_eq!(hash.len(), 8);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
        // The exact value is part of the on-disk layout: it must never change.
        assert_eq!(short_hash(path.as_bytes()), hash);
        assert_eq!(short_hash(b"abc"), "ba7816bf");
    }

    #[test]
    fn ascii_keys_carry_no_hash_and_long_keys_are_bounded() {
        let (root, expected) = if cfg!(windows) {
            ("C:/tmp", "C--tmp-plain")
        } else {
            ("/tmp", "-tmp-plain")
        };
        assert_eq!(project_key(Path::new(&format!("{root}/plain"))), expected);
        let long = format!("{root}/{}", "x".repeat(400));
        let key = project_key(Path::new(&long));
        assert!(key.len() <= MAX_KEY_LEN + 9, "{}", key.len());
        assert_ne!(key, project_key(Path::new(&format!("{long}y"))));
    }

    #[test]
    fn a_relative_or_untidy_path_resolves_to_the_same_key() {
        let cwd = env::current_dir().unwrap();
        let absolute = cwd.join("no-such-folder-for-key-test");
        assert_eq!(
            project_key(Path::new("no-such-folder-for-key-test")),
            project_key(&absolute)
        );
        assert_eq!(
            project_key(&cwd.join("x/../no-such-folder-for-key-test/.")),
            project_key(&absolute)
        );
    }

    #[test]
    fn a_symlinked_workspace_shares_the_key_of_its_target() {
        #[cfg(unix)]
        {
            let root = env::temp_dir().join(format!("asterline-key-link-{}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(root.join("real")).unwrap();
            std::os::unix::fs::symlink(root.join("real"), root.join("link")).unwrap();
            assert_eq!(
                project_key(&root.join("link")),
                project_key(&root.join("real"))
            );
            fs::remove_dir_all(root).ok();
        }
    }

    #[test]
    fn the_home_override_wins_and_an_empty_value_is_ignored() {
        let user = Some(PathBuf::from("/home/someone"));
        assert_eq!(
            home_from_values(Some(OsString::from("/custom/ahome")), user.clone()),
            PathBuf::from("/custom/ahome")
        );
        assert_eq!(
            home_from_values(None, user.clone()),
            PathBuf::from("/home/someone/.asterline")
        );
        assert_eq!(
            home_from_values(Some(OsString::new()), user),
            PathBuf::from("/home/someone/.asterline")
        );
    }

    #[test]
    fn project_files_live_under_the_home_projects_directory() {
        let workspace = Path::new("/tmp/state-paths-layout");
        let dir = project_dir(workspace);
        assert_eq!(dir.parent().unwrap(), asterline_home().join("projects"));
        assert_eq!(db_path(workspace), dir.join("asterline.sqlite3"));
        assert_eq!(team_config_path(workspace), dir.join("team.json"));
        assert_eq!(roster_path(workspace), dir.join("roster.md"));
        assert_eq!(agy_log_dir(workspace), dir.join("agy"));
        assert_eq!(models_path(), skills_dir().join("models.md"));
        assert_eq!(models_path().parent().unwrap(), skills_dir());
        assert!(!dir.starts_with(workspace));
    }

    #[test]
    fn model_list_is_written_into_the_skills_dir_and_the_old_file_is_removed() {
        let home = ensure_home().unwrap();
        let legacy = home.join("models.md");
        fs::write(&legacy, "old").unwrap();
        assert!(write_state_file(&models_path(), "model list", "new").unwrap());
        assert_eq!(fs::read_to_string(models_path()).unwrap(), "new");
        remove_legacy_models_file();
        assert!(!legacy.exists());
        assert!(models_path().is_file());
    }

    #[cfg(unix)]
    #[test]
    fn project_directories_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let workspace = env::temp_dir().join(format!("asterline-private-{}", std::process::id()));
        let dir = ensure_project_dir(&workspace).unwrap();
        for path in [dir.clone(), dir.parent().unwrap().to_path_buf()] {
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o700,
                "{}",
                path.display()
            );
        }
        fs::remove_dir_all(dir).ok();
    }
}
