//! Application bootstrap: parse CLI args, resolve a team (config file or a
//! default roster from detected backends), open the store, spawn the runtime,
//! and run the chat-first TUI. Exiting shuts the runtime down gracefully.

use std::collections::HashMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;
use std::thread::JoinHandle;

use crate::adapter::{FakeRunner, MemberRunner, runner_for};
use crate::domain::TerminalMode;
use crate::domain::config::{
    DetectedBackends, FreshChatRoster, default_team, detect_backends, ensure_brainstorm_skill,
    ensure_team_skill, first_installed_backend, inject_team_protocol, load_team_config,
    strip_team_protocols,
};
use crate::domain::event::{ChatItem, RuntimeEvent};
use crate::domain::mode::ModesConfig;
use crate::domain::team::{BackendKind, SessionPolicy, TeamConfig};
use crate::fs_safety;
use crate::legacy_migration;
use crate::runtime::{self, Runners, RuntimeHandle};
use crate::state_paths;
use crate::store::sqlite::SqliteStore;
use crate::tui;
use crate::tui::app_state::AppState;

/// Entry point invoked from `main`.
pub fn run() -> io::Result<()> {
    run_with_args(std::env::args().skip(1), std::env::current_dir()?)
}

pub fn run_with_args<I, S>(args: I, cwd: impl AsRef<Path>) -> io::Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let config = AppConfig::parse(args)?;
    if config.show_help {
        println!("{}", AppConfig::help());
        return Ok(());
    }
    if config.update {
        println!(
            "{}",
            crate::managed_update::run().map_err(io::Error::other)?
        );
        return Ok(());
    }

    let prepared = match prepare(&config, cwd.as_ref())? {
        Some(prepared) => prepared,
        None => {
            eprintln!(
                "Asterline: no team config and no supported backend CLI was found on PATH.\n\
                 Install a backend CLI, or pass --team <config.json>."
            );
            return Ok(());
        }
    };

    let Prepared {
        handle,
        join,
        events,
        state,
        instance_lock: _instance_lock,
    } = prepared;

    if config.banner {
        print_startup_banner();
    }

    // Keep an independent shutdown handle so terminal-initialization errors
    // inside `tui::run` cannot leave the runtime waiting forever. The TUI also
    // sends Shutdown on its normal cleanup path; a duplicate send is harmless.
    let shutdown_handle = handle.clone();
    let tui_result = tui::run(handle, events, state);
    shutdown_handle.shutdown();
    let runtime_result = join_runtime(join);
    tui_result.and(runtime_result)
}

fn join_runtime(join: JoinHandle<()>) -> io::Result<()> {
    join.join()
        .map_err(|_| io::Error::other("Asterline runtime thread panicked"))
}

fn print_startup_banner() {
    println!("\x1b[1;36mAsterline\x1b[0m · Multi-Agent Coding Console");
}

/// Everything needed to run the TUI, wired but not yet started.
struct Prepared {
    handle: RuntimeHandle,
    join: JoinHandle<()>,
    events: mpsc::Receiver<RuntimeEvent>,
    state: AppState,
    instance_lock: InstanceLock,
}

/// An OS-backed exclusive lock for one SQLite store. The marker file remains
/// after exit, but the lock itself is released automatically with the handle.
struct InstanceLock {
    _file: std::fs::File,
}

impl InstanceLock {
    fn acquire(db_path: &Path) -> io::Result<Self> {
        let mut lock_name = db_path.as_os_str().to_os_string();
        lock_name.push(".lock");
        let lock_path = PathBuf::from(lock_name);
        fs_safety::ensure_private_regular_file(&lock_path, "instance lock")?;
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.share_mode(0);
        }
        let mut file = options.open(&lock_path).map_err(|err| {
            io::Error::new(
                err.kind(),
                format!(
                    "could not lock {} (another Asterline instance may be using this store): \
                     {err}",
                    db_path.display()
                ),
            )
        })?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                let err = io::Error::last_os_error();
                return Err(io::Error::new(
                    err.kind(),
                    format!(
                        "another Asterline instance is already using {}: {err}",
                        db_path.display()
                    ),
                ));
            }
        }
        file.set_len(0)?;
        writeln!(file, "pid={}", std::process::id())?;
        Ok(Self { _file: file })
    }
}

/// Build the team, store, runners, and runtime. Returns `None` if no team can
/// be resolved (no config and no detected backends).
fn prepare(config: &AppConfig, cwd: &Path) -> io::Result<Option<Prepared>> {
    prepare_with(config, cwd, &first_installed_backend)
}

/// [`prepare`] with the lookup of the first installed CLI injected, so tests
/// do not depend on what is on `PATH`.
fn prepare_with(
    config: &AppConfig,
    cwd: &Path,
    first_backend: &dyn Fn() -> Option<BackendKind>,
) -> io::Result<Option<Prepared>> {
    let requested_workspace = config
        .workspace
        .clone()
        .unwrap_or_else(|| cwd.to_path_buf());

    // Asterline keeps its files under its own home, never in the project. A
    // workspace that does not exist yet is created, as it always was.
    ensure_workspace_exists(&requested_workspace)?;
    let mut startup_notices = legacy_migration::migrate_workspace(&requested_workspace);
    let requested_state_dir = state_paths::ensure_project_dir(&requested_workspace)?;
    let saved_team = requested_state_dir.join(state_paths::TEAM_FILE);
    // Only an implicitly reused saved roster participates in conversation
    // snapshot restoration. An explicit --team file or a freshly created
    // default team is a launch-time choice and must not be overwritten by an
    // older DB.
    let restore_saved_roster = config.team_path.is_none()
        && !config.pick_team
        && fs_safety::regular_file_exists(&saved_team, "saved team config")?;
    let mut team = match &config.team_path {
        Some(path) => load_team_config(path)?,
        // Reuse the saved roster (including any `/model` change). The hidden
        // `--pick-team` flag ignores it and starts over from the default.
        None if restore_saved_roster => load_team_config(&saved_team)?,
        None => match default_startup_team(&saved_team, &requested_workspace, detect_backends())? {
            Some(team) => team,
            None => return Ok(None),
        },
    };
    // A CLI workspace is an explicit launch-time override. A roster loaded
    // from this directory's `team.json` belongs to that directory as well: if
    // a project was moved, its serialized `workspace` value is stale, and
    // continuing to use it would make native Codex/Claude transcripts belong
    // to the old project. An explicitly supplied `--team` file retains its
    // declared workspace instead.
    let adopt_saved_workspace = restore_saved_roster && config.workspace.is_none();
    if let Some(workspace) = &config.workspace {
        team.workspace = workspace.clone();
    } else if adopt_saved_workspace && team.workspace != requested_workspace {
        team.workspace = requested_workspace.clone();
        runtime::save_team_config(&saved_team, &team)?;
    }
    let workspace = team.workspace.clone();
    if state_paths::project_key(&workspace) != state_paths::project_key(&requested_workspace) {
        ensure_workspace_exists(&workspace)?;
        startup_notices.extend(legacy_migration::migrate_workspace(&workspace));
    }
    ensure_team_skill()?;
    ensure_brainstorm_skill()?;

    let db_path = match &config.db_path {
        Some(path) => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            path.clone()
        }
        None => state_paths::ensure_project_dir(&workspace)?.join(state_paths::DB_FILE),
    };
    fs_safety::ensure_private_regular_file(&db_path, "SQLite database")?;
    let instance_lock = InstanceLock::acquire(&db_path)?;
    let store = SqliteStore::open(&db_path).map_err(|err| io::Error::other(err.to_string()))?;

    if !config.no_restore && restore_saved_roster {
        team = store
            .restore_active_team_config(&team)
            .map_err(|err| io::Error::other(err.to_string()))?;
    }
    // What every new chat starts with: an explicit `--team` file's members,
    // otherwise one default Main on the first installed CLI. Computed once
    // here; the launch chat and `/new` both apply it. Without any installed
    // CLI there is nothing to build and new chats keep the current members.
    let fresh_roster = match &config.team_path {
        Some(_) => Some(FreshChatRoster::from_team(&team)),
        None => first_backend().map(FreshChatRoster::default_main),
    };
    if !config.no_restore {
        // Every launch opens a new, empty chat; earlier chats are reached
        // through `/resume`.
        team = start_fresh_chat(&store, team, fresh_roster.as_ref(), restore_saved_roster)?;
        if restore_saved_roster && fresh_roster.is_some() {
            // team.json mirrors the live roster: the new chat's, not the
            // previous chat's.
            runtime::save_team_config(&saved_team, &strip_team_protocols(team.clone()))?;
        }
    }
    inject_team_protocol(&mut team);

    let runners = build_runners(&team, config.fake);
    // The chat starts empty: nothing from the previous chat is replayed. Logs
    // are global rather than per chat, so the recent tail still fills the logs
    // drawer.
    let mut chat = Vec::new();
    let logs = if config.no_restore {
        Vec::new()
    } else {
        match store.recent_logs(4000) {
            Ok(logs) => logs,
            Err(err) => {
                chat.push(ChatItem::Notice {
                    member: None,
                    text: format!("could not replay logs: {err}"),
                });
                Vec::new()
            }
        }
    };
    chat.extend(
        startup_notices
            .into_iter()
            .map(|text| ChatItem::Notice { member: None, text }),
    );
    let mut state = AppState::new(chat);
    state.seed_logs(logs);

    // Bound the runtime-to-TUI stream so a fast or malformed backend cannot
    // turn a slow terminal renderer into an unbounded in-memory queue.
    let (events_tx, events_rx) = mpsc::sync_channel(2_048);
    #[cfg(windows)]
    if !config.no_auto_update {
        crate::update::spawn_auto_update(events_tx.clone());
    }
    let team_save_path = config.team_path.clone().unwrap_or(saved_team);
    let manual_approvals = config.manual_approvals || team.approvals.manual;
    let (handle, join) = runtime::spawn_bounded(
        team,
        store,
        runners,
        events_tx,
        manual_approvals,
        config.fake,
        Some(team_save_path),
        fresh_roster,
    );

    Ok(Some(Prepared {
        handle,
        join,
        events: events_rx,
        state,
        instance_lock,
    }))
}

/// Open a new chat unless the active one is still blank.
///
/// The previous chat keeps its own snapshot (roster, sessions, mode), so
/// `/resume` can bring it back. The new chat gets `fresh_roster` through the
/// same [`FreshChatRoster::apply_to`] that `/new` uses (the previous chat's
/// top-level members when there is none), in the previous chat's mode
/// overrides. When the roster came from the saved team or an earlier chat
/// (`clear_sessions`), its backend sessions belong to the previous chat and
/// are dropped; an explicit `--team` file keeps the sessions it configures. A
/// blank active chat is reused so repeated launches do not pile up empty
/// rows; it takes the new roster and its snapshot is rewritten to match.
/// Returns the roster to start the runtime with.
fn start_fresh_chat(
    store: &SqliteStore,
    team: TeamConfig,
    fresh_roster: Option<&FreshChatRoster>,
    clear_sessions: bool,
) -> io::Result<TeamConfig> {
    let to_io = |err: rusqlite::Error| io::Error::other(err.to_string());
    let previous = store.current_conversation().map_err(to_io)?;
    let build = |team: &TeamConfig| {
        let base = match fresh_roster {
            Some(roster) => roster.apply_to(team),
            None => team.clone(),
        };
        let mut fresh = strip_team_protocols(base.without_sub_agents());
        if clear_sessions {
            for member in &mut fresh.members {
                member.session_id = None;
                member.session_policy = SessionPolicy::Fresh;
            }
        }
        fresh
    };
    if !store.conversation_has_content(previous).map_err(to_io)? {
        if fresh_roster.is_none() {
            return Ok(team);
        }
        // The blank chat is reused with the new roster. Its snapshot is what a
        // later `/resume` and the next launch read, so it must say the same.
        let fresh = build(&team);
        let (mode, mode_overrides) = store
            .conversation_snapshot(previous)
            .map_err(to_io)?
            .map(|snapshot| (snapshot.mode, snapshot.mode_overrides))
            .unwrap_or_default();
        store
            .save_conversation_snapshot_for(previous, &fresh, &[], mode, &mode_overrides)
            .map_err(to_io)?;
        return Ok(fresh);
    }
    let mode_overrides = match store.conversation_snapshot(previous).map_err(to_io)? {
        Some(snapshot) => snapshot.mode_overrides,
        None => {
            // A chat from before snapshots existed would otherwise vanish from
            // `/resume` once it stops being the active one.
            store
                .save_conversation_snapshot_for(
                    previous,
                    &strip_team_protocols(team.clone()),
                    &[],
                    TerminalMode::Normal,
                    &ModesConfig::default(),
                )
                .map_err(to_io)?;
            ModesConfig::default()
        }
    };
    let fresh = build(&team);
    store
        .create_fresh_conversation(&fresh, TerminalMode::Normal, &mode_overrides)
        .map_err(to_io)?;
    Ok(fresh)
}

fn ensure_workspace_exists(workspace: &Path) -> io::Result<()> {
    if !workspace.is_dir() {
        std::fs::create_dir_all(workspace)?;
    }
    if workspace.is_dir() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("workspace is not a directory: {}", workspace.display()),
        ))
    }
}

/// The team for a workspace with no saved or explicit config. There is no
/// setup screen: one `main` member on the first detected backend, with that
/// CLI's own default model. Saved so later launches and `/model` build on it.
fn default_startup_team(
    saved_team: &Path,
    workspace: &Path,
    detected: DetectedBackends,
) -> io::Result<Option<TeamConfig>> {
    let Some(team) = default_team(workspace, detected) else {
        return Ok(None);
    };
    runtime::save_team_config(saved_team, &team)?;
    Ok(Some(team))
}

fn build_runners(team: &TeamConfig, fake: bool) -> Runners {
    let mut runners: Runners = HashMap::new();
    for member in &team.members {
        let runner: Arc<dyn MemberRunner> = if fake {
            Arc::new(FakeRunner::team(member.backend))
        } else {
            Arc::from(runner_for(member, &team.workspace))
        };
        runners.insert(member.id.clone(), runner);
    }
    runners
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AppConfig {
    team_path: Option<PathBuf>,
    workspace: Option<PathBuf>,
    db_path: Option<PathBuf>,
    no_restore: bool,
    debug: bool,
    manual_approvals: bool,
    fake: bool,
    pick_team: bool,
    banner: bool,
    no_auto_update: bool,
    update: bool,
    show_help: bool,
}

impl AppConfig {
    pub fn parse<I, S>(args: I) -> io::Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut config = AppConfig::default();
        let args: Vec<String> = args.into_iter().map(|a| a.as_ref().to_string()).collect();
        if args.first().is_some_and(|argument| argument == "update") {
            if args.len() != 1 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "update does not accept options or arguments",
                ));
            }
            config.update = true;
            return Ok(config);
        }
        let mut index = 0;
        while index < args.len() {
            let arg = args[index].as_str();
            match arg {
                "--team" => {
                    config.team_path = Some(Self::value(&args, &mut index, "--team")?.into())
                }
                "--workspace" => {
                    config.workspace = Some(Self::value(&args, &mut index, "--workspace")?.into())
                }
                "--db" => config.db_path = Some(Self::value(&args, &mut index, "--db")?.into()),
                "--no-restore" => config.no_restore = true,
                "--debug" => config.debug = true,
                "--manual-approvals" => config.manual_approvals = true,
                "--fake" => config.fake = true,
                "--pick-team" => config.pick_team = true,
                "--banner" => config.banner = true,
                "--no-auto-update" => config.no_auto_update = true,
                "--update" => config.update = true,
                "-h" | "--help" => config.show_help = true,
                _ if arg.starts_with("--team=") => {
                    config.team_path = Some(arg["--team=".len()..].into())
                }
                _ if arg.starts_with("--workspace=") => {
                    config.workspace = Some(arg["--workspace=".len()..].into())
                }
                _ if arg.starts_with("--db=") => config.db_path = Some(arg["--db=".len()..].into()),
                unknown => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("unknown argument: {unknown}"),
                    ));
                }
            }
            index += 1;
        }
        Ok(config)
    }

    fn value(args: &[String], index: &mut usize, flag: &str) -> io::Result<String> {
        *index += 1;
        args.get(*index).cloned().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{flag} requires a value"),
            )
        })
    }

    pub fn help() -> &'static str {
        "Asterline — a chat-first multi-agent coding console.\n\
         \n\
         Usage: asterline [OPTIONS]\n\
         \x20\x20\x20\x20\x20\x20 asterline update\n\
         \n\
         Options:\n\
         \x20 --team <PATH>       Load a team config (JSON) instead of the saved one.\n\
         \x20 --workspace <PATH>  Working directory for members. Default: current directory.\n\
         \x20 --db <PATH>         SQLite path. Default: <asterline home>/projects/<workspace key>/asterline.sqlite3.\n\
         \x20 --no-restore        Do not replay persisted chat history on startup.\n\
         \x20 --debug             Developer mode.\n\
         \x20 --manual-approvals  Show the composer card for Codex tool asks (off by default).\n\
         \x20 --fake              Use offline fake agents instead of real CLIs.\n\
         \x20 --banner            Print a compact startup banner before the TUI.\n\
         \x20 update              Update via Windows Setup or an owning Homebrew Formula.\n\
         \x20 --update            Backward-compatible alias for `update`.\n\
         \x20 --no-auto-update    Skip the Windows installer update check.\n\
         \x20 -h, --help          Show this help.\n\
         \n\
         With no --team, Asterline starts a chat with one `main` agent on the first\n\
         detected backend (Codex, Claude, Grok, Agy) using its default model. Switch\n\
         models in the session with /model; the choice is saved in\n\
         <asterline home>/projects/<workspace key>/team.json. The asterline home is\n\
         ~/.asterline, or $ASTERLINE_HOME when set; the workspace key is its absolute\n\
         path with every other character turned into `-`. Nothing is written into the\n\
         workspace itself."
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::event::{MessageTarget, UiCommand};
    use std::time::Duration;

    /// Launch with no CLI found on `PATH`, so a new chat keeps the current
    /// members and the result does not depend on this machine.
    fn prepare(config: &AppConfig, cwd: &Path) -> io::Result<Option<Prepared>> {
        prepare_with(config, cwd, &|| None)
    }

    #[test]
    fn parses_flags() {
        let config = AppConfig::parse([
            "--team",
            "/tmp/t.json",
            "--workspace",
            "/tmp/ws",
            "--no-restore",
            "--fake",
            "--banner",
            "--no-auto-update",
        ])
        .unwrap();
        assert_eq!(config.team_path, Some(PathBuf::from("/tmp/t.json")));
        assert_eq!(config.workspace, Some(PathBuf::from("/tmp/ws")));
        assert!(config.no_restore);
        assert!(config.fake);
        assert!(config.banner);
        assert!(config.no_auto_update);
    }

    #[test]
    fn parses_equals_form_and_help() {
        let config = AppConfig::parse(["--db=/tmp/x.sqlite3", "--help"]).unwrap();
        assert_eq!(config.db_path, Some(PathBuf::from("/tmp/x.sqlite3")));
        assert!(config.show_help);
        assert!(!config.banner);
        assert!(!config.update);
    }

    #[test]
    fn help_mentions_compact_banner_flag() {
        assert!(AppConfig::help().contains("--banner"));
        assert!(AppConfig::help().contains("compact startup banner"));
        assert!(AppConfig::help().contains("--update"));
        assert!(AppConfig::help().contains("asterline update"));
        assert!(AppConfig::help().contains("--no-auto-update"));
        assert!(AppConfig::help().contains("--manual-approvals"));
    }

    #[test]
    fn help_no_longer_advertises_the_team_builder() {
        let help = AppConfig::help();
        assert!(!help.contains("--pick-team"));
        assert!(!help.contains("team builder"));
        assert!(help.contains("/model"));
        // The flag is still accepted as a hidden reset.
        assert!(AppConfig::parse(["--pick-team"]).unwrap().pick_team);
    }

    #[test]
    fn startup_without_a_saved_team_creates_and_saves_one_main_member() {
        let dir =
            std::env::temp_dir().join(format!("asterline-app-default-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let saved = dir.join("team.json");
        let detected = DetectedBackends {
            codex: false,
            claude: true,
            grok: true,
            agy: false,
        };

        let team = default_startup_team(&saved, &dir, detected)
            .unwrap()
            .expect("team");

        assert_eq!(team.members.len(), 1);
        let main = &team.members[0];
        assert_eq!(main.id.as_str(), "main");
        assert_eq!(main.backend, crate::domain::team::BackendKind::Claude);
        assert_eq!(main.model, None);
        assert_eq!(main.effort, None);
        assert_eq!(
            main.permission_mode,
            Some(crate::domain::team::PermissionMode::BypassPermissions)
        );
        let reloaded = load_team_config(&saved).expect("saved team loads");
        assert_eq!(reloaded.members.len(), 1);
        assert_eq!(reloaded.members[0].id, main.id);
        assert_eq!(reloaded.default_member_ids(), vec![main.id.clone()]);

        let none = DetectedBackends {
            codex: false,
            claude: false,
            grok: false,
            agy: false,
        };
        assert!(
            default_startup_team(&dir.join("other.json"), &dir, none)
                .unwrap()
                .is_none()
        );
        assert!(!dir.join("other.json").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn parses_manual_approvals_flag() {
        let config = AppConfig::parse(["--manual-approvals"]).unwrap();
        assert!(config.manual_approvals);
        assert!(!AppConfig::parse(["--fake"]).unwrap().manual_approvals);
    }

    #[test]
    fn parses_manual_update_flag() {
        let config = AppConfig::parse(["--update"]).unwrap();
        assert!(config.update);
        assert!(!config.no_auto_update);
    }

    #[test]
    fn parses_update_subcommand_without_starting_the_tui() {
        let config = AppConfig::parse(["update"]).unwrap();
        assert!(config.update);
    }

    #[test]
    fn update_subcommand_rejects_extra_arguments() {
        assert!(AppConfig::parse(["update", "--fake"]).is_err());
    }

    #[test]
    fn unknown_arg_rejected() {
        assert!(AppConfig::parse(["--nope"]).is_err());
    }

    #[test]
    fn missing_value_rejected() {
        assert!(AppConfig::parse(["--team"]).is_err());
    }

    #[test]
    fn runtime_thread_panic_is_reported_as_an_io_error() {
        let join = std::thread::spawn(|| panic!("runtime failure"));
        let result = join_runtime(join);

        assert_eq!(
            result.expect_err("panic must be visible").to_string(),
            "Asterline runtime thread panicked"
        );
    }

    #[test]
    fn store_instance_lock_is_exclusive_and_released_on_drop() {
        let dir =
            std::env::temp_dir().join(format!("asterline-instance-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("state.sqlite3");

        let first = InstanceLock::acquire(&db).unwrap();
        let error = InstanceLock::acquire(&db)
            .err()
            .expect("a second instance must be rejected");
        assert!(error.to_string().contains("another Asterline instance"));

        drop(first);
        InstanceLock::acquire(&db).expect("lock must be released when the owner exits");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn instance_lock_rejects_a_symlink_without_touching_its_target() {
        use std::os::unix::fs::symlink;

        let dir = std::env::temp_dir().join(format!(
            "asterline-instance-lock-symlink-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("state.sqlite3");
        let victim = dir.join("victim.txt");
        std::fs::write(&victim, "do not truncate").unwrap();
        symlink(&victim, format!("{}.lock", db.display())).unwrap();

        assert!(InstanceLock::acquire(&db).is_err());
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "do not truncate");

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn inject_protocol_lists_teammates() {
        use crate::domain::team::{BackendKind, MemberId, TeamMember};
        let mut team = TeamConfig::new("t", "/tmp/ws")
            .with_member(TeamMember::new(
                "builder",
                "Builder",
                BackendKind::Codex,
                "implementation",
            ))
            .with_member(TeamMember::new(
                "reviewer",
                "Reviewer",
                BackendKind::Claude,
                "review",
            ));
        inject_team_protocol(&mut team);
        let builder = team.member(&MemberId::new("builder")).unwrap();
        let prompt = builder.system_prompt.as_ref().unwrap();
        assert!(prompt.contains("Asterline team skill"));
        assert!(!prompt.contains("$asterline-team"));
        assert!(!prompt.contains(".asterline/roster.md"));
        assert!(!prompt.contains(".agents/skills"));
        assert!(prompt.contains("reviewer"));
    }

    #[test]
    fn prepare_with_fake_backend_runs_a_turn() {
        let dir = std::env::temp_dir().join(format!("asterline-app-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Force a single-codex default team via a written config so the test is
        // independent of what is installed on PATH.
        let team = crate::domain::config::default_team(
            &dir,
            crate::domain::config::DetectedBackends {
                codex: true,
                claude: false,
                grok: false,
                agy: false,
            },
        )
        .unwrap();
        let team_path = dir.join("team.json");
        std::fs::write(&team_path, serde_json::to_string(&team).unwrap()).unwrap();

        let config = AppConfig::parse([
            "--team",
            team_path.to_str().unwrap(),
            "--db",
            dir.join("db.sqlite3").to_str().unwrap(),
            "--fake",
        ])
        .unwrap();

        let prepared = prepare(&config, &dir).unwrap().expect("prepared");
        assert!(crate::domain::config::team_skill_path().is_file());
        assert!(!dir.join(".agents").exists());
        assert!(!dir.join(".asterline").exists());
        let Prepared {
            handle,
            join,
            events,
            ..
        } = prepared;

        // Drain the Ready event.
        let ready = events.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(ready, RuntimeEvent::Ready { .. }));

        handle.send(UiCommand::UserMessage {
            target: MessageTarget::Default,
            body: "hello".to_string(),
        });

        let mut saw_completed = false;
        while let Ok(event) = events.recv_timeout(Duration::from_secs(2)) {
            if let RuntimeEvent::MessageCompleted { text, .. } = &event
                && text.contains("hello")
            {
                saw_completed = true;
            }
            if matches!(event, RuntimeEvent::TurnFinished { .. }) {
                break;
            }
        }
        assert!(saw_completed);

        handle.send(UiCommand::Shutdown);
        let _ = join.join();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn launch_does_not_carry_the_previous_chats_model_choices_over() {
        let dir = std::env::temp_dir().join(format!(
            "asterline-app-restore-effort-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut launch = crate::domain::config::default_team(
            &dir,
            crate::domain::config::DetectedBackends {
                codex: true,
                claude: false,
                grok: false,
                agy: false,
            },
        )
        .unwrap();
        launch.members[0].effort = Some(crate::domain::team::Effort::Low);
        let team_path = state_paths::team_config_path(&dir);
        std::fs::create_dir_all(team_path.parent().unwrap()).unwrap();
        runtime::save_team_config(&team_path, &launch).unwrap();

        let db_path = dir.join("db.sqlite3");
        let store = SqliteStore::open(&db_path).unwrap();
        store.current_conversation().unwrap();
        let mut saved = launch.clone();
        saved.members[0].effort = Some(crate::domain::team::Effort::High);
        store
            .save_conversation_snapshot(
                &saved,
                &[],
                crate::domain::TerminalMode::Normal,
                &crate::domain::mode::ModesConfig::default(),
            )
            .unwrap();
        let turn = store.create_turn().unwrap();
        store
            .record_user(
                turn,
                std::slice::from_ref(&launch.members[0].id),
                "restore this chat after reopening",
            )
            .unwrap();
        drop(store);

        let config = AppConfig::parse([
            "--workspace",
            dir.to_str().unwrap(),
            "--db",
            db_path.to_str().unwrap(),
            "--fake",
        ])
        .unwrap();
        let prepared = prepare_with(&config, &dir, &|| Some(BackendKind::Codex))
            .unwrap()
            .expect("prepared");
        // A launch opens a new, empty chat: the old message is not replayed.
        assert!(!prepared.state.chat().iter().any(|item| matches!(
            item,
            ChatItem::User { body, .. } if body == "restore this chat after reopening"
        )));
        let ready = prepared
            .events
            .recv_timeout(Duration::from_secs(2))
            .expect("ready");
        assert!(matches!(
            ready,
            RuntimeEvent::Ready { members, .. }
                if members.len() == 1
                    && members[0].id.as_str() == "main"
                    && members[0].effort.is_none()
        ));
        // team.json mirrors the new chat's roster.
        let saved_team = load_team_config(&team_path).unwrap();
        assert_eq!(saved_team.members.len(), 1);
        assert_eq!(saved_team.members[0].effort, None);

        prepared.handle.send(UiCommand::Shutdown);
        prepared.join.join().unwrap();

        // The old chat is still reachable through `/resume`.
        let store = SqliteStore::open(&db_path).unwrap();
        let resumable = store.resumable_conversations().unwrap();
        assert_eq!(resumable.len(), 1);
        assert_eq!(resumable[0].preview, "restore this chat after reopening");
        assert_eq!(
            resumable[0].main_effort,
            Some(crate::domain::team::Effort::High)
        );
        drop(store);
        std::fs::remove_dir_all(&dir).ok();
    }

    fn launch_prepared(dir: &Path, db_path: &Path) -> Prepared {
        let config = AppConfig::parse([
            "--workspace",
            dir.to_str().unwrap(),
            "--db",
            db_path.to_str().unwrap(),
            "--fake",
        ])
        .unwrap();
        prepare(&config, dir).unwrap().expect("prepared")
    }

    fn codex_only_team(dir: &Path) -> TeamConfig {
        crate::domain::config::default_team(
            dir,
            crate::domain::config::DetectedBackends {
                codex: true,
                claude: false,
                grok: false,
                agy: false,
            },
        )
        .unwrap()
    }

    #[test]
    fn launching_twice_without_messages_reuses_the_blank_chat() {
        let dir = std::env::temp_dir().join(format!(
            "asterline-app-blank-relaunch-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("db.sqlite3");
        let team_path = state_paths::team_config_path(&dir);
        std::fs::create_dir_all(team_path.parent().unwrap()).unwrap();
        runtime::save_team_config(&team_path, &codex_only_team(&dir)).unwrap();

        let mut active = Vec::new();
        for _ in 0..2 {
            let prepared = launch_prepared(&dir, &db_path);
            assert!(prepared.state.chat().is_empty());
            prepared.handle.send(UiCommand::Shutdown);
            prepared.join.join().unwrap();
            let store = SqliteStore::open(&db_path).unwrap();
            active.push(store.current_conversation().unwrap());
        }
        assert_eq!(active[0], active[1]);
        let count: i64 = rusqlite::Connection::open(&db_path)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM conversations", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fresh_chat_drops_sub_agents_and_sessions_of_the_previous_chat() {
        let dir =
            std::env::temp_dir().join(format!("asterline-app-fresh-roster-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let launch = codex_only_team(&dir);
        let team_path = state_paths::team_config_path(&dir);
        std::fs::create_dir_all(team_path.parent().unwrap()).unwrap();
        runtime::save_team_config(&team_path, &launch).unwrap();

        let db_path = dir.join("db.sqlite3");
        let store = SqliteStore::open(&db_path).unwrap();
        let old = store.current_conversation().unwrap();
        let mut saved = launch.clone();
        saved.members[0].session_id = Some("old-native-session".to_string());
        saved.members[0].session_policy = crate::domain::team::SessionPolicy::Resume;
        let mut child =
            crate::domain::team::TeamMember::new("kid", "Kid", saved.members[0].backend, "helper");
        child.parent = Some(saved.members[0].id.clone());
        saved.members.push(child);
        store
            .save_conversation_snapshot(
                &saved,
                &[],
                crate::domain::TerminalMode::Normal,
                &crate::domain::mode::ModesConfig::default(),
            )
            .unwrap();
        let turn = store.create_turn().unwrap();
        store
            .record_user(turn, std::slice::from_ref(&launch.members[0].id), "hello")
            .unwrap();
        drop(store);

        let prepared = launch_prepared(&dir, &db_path);
        let ready = prepared
            .events
            .recv_timeout(Duration::from_secs(2))
            .expect("ready");
        match ready {
            RuntimeEvent::Ready { members, .. } => {
                assert!(members.iter().all(|member| member.id.as_str() != "kid"));
            }
            other => panic!("expected Ready, got {other:?}"),
        }
        prepared.handle.send(UiCommand::Shutdown);
        prepared.join.join().unwrap();

        let store = SqliteStore::open(&db_path).unwrap();
        let fresh = store.current_conversation().unwrap();
        assert_ne!(fresh, old);
        let snapshot = store.conversation_snapshot(fresh).unwrap().unwrap();
        assert!(snapshot.team.members.iter().all(|m| m.parent.is_none()));
        assert!(snapshot.team.members.iter().all(|m| m.session_id.is_none()
            && m.session_policy == crate::domain::team::SessionPolicy::Fresh));
        assert!(snapshot.sessions.is_empty());
        // The old chat keeps its own roster for `/resume`.
        let previous = store.conversation_snapshot(old).unwrap().unwrap();
        assert!(previous.team.members.iter().any(|m| m.parent.is_some()));
        drop(store);
        std::fs::remove_dir_all(&dir).ok();
    }

    fn builder_and_reviewer_team(dir: &Path) -> TeamConfig {
        use crate::domain::team::{BackendKind, DefaultTarget, MemberId, TeamMember};
        let mut team = TeamConfig::new("old-team", dir)
            .with_member(TeamMember::new(
                "builder",
                "Builder",
                BackendKind::Codex,
                "impl",
            ))
            .with_member(TeamMember::new(
                "reviewer",
                "Reviewer",
                BackendKind::Claude,
                "review",
            ));
        team.default_target = Some(DefaultTarget::Member(MemberId::new("builder")));
        team.max_auto_relays = 9;
        team
    }

    fn member_ids(team: &TeamConfig) -> Vec<&str> {
        team.members.iter().map(|m| m.id.as_str()).collect()
    }

    #[test]
    fn new_chat_starts_with_the_default_main_instead_of_the_saved_roster() {
        use crate::domain::team::{BackendKind, DefaultTarget, MemberId};
        let dir =
            std::env::temp_dir().join(format!("asterline-app-fresh-main-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let saved = builder_and_reviewer_team(&dir);
        let store = SqliteStore::in_memory().unwrap();
        let old = store.current_conversation().unwrap();
        store
            .save_conversation_snapshot(
                &saved,
                &[],
                crate::domain::TerminalMode::Normal,
                &ModesConfig::default(),
            )
            .unwrap();
        let turn = store.create_turn().unwrap();
        store
            .record_user(turn, std::slice::from_ref(&saved.members[0].id), "hello")
            .unwrap();

        let roster = FreshChatRoster::default_main(BackendKind::Claude);
        let fresh = start_fresh_chat(&store, saved, Some(&roster), true).unwrap();

        assert_eq!(member_ids(&fresh), ["main"]);
        assert_eq!(fresh.members[0].backend, BackendKind::Claude);
        assert_eq!(
            fresh.default_target,
            Some(DefaultTarget::Member(MemberId::new("main")))
        );
        assert_eq!(fresh.name, "old-team");
        assert_eq!(fresh.max_auto_relays, 9);
        let current = store.current_conversation().unwrap();
        assert_ne!(current, old);
        let snapshot = store.conversation_snapshot(current).unwrap().unwrap();
        assert_eq!(member_ids(&snapshot.team), ["main"]);
        // The old chat still has its own roster for `/resume`.
        let previous = store.conversation_snapshot(old).unwrap().unwrap();
        assert_eq!(member_ids(&previous.team), ["builder", "reviewer"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_reused_blank_chat_takes_the_default_main_and_keeps_its_mode_overrides() {
        use crate::domain::team::BackendKind;
        let dir =
            std::env::temp_dir().join(format!("asterline-app-blank-main-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let saved = builder_and_reviewer_team(&dir);
        let db_path = dir.join("db.sqlite3");
        let store = SqliteStore::open(&db_path).unwrap();
        let blank = store.current_conversation().unwrap();
        let overrides = ModesConfig {
            review: Some(crate::domain::mode::ReviewModeConfig {
                max_iterations: Some(3),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(!overrides.is_default());
        store
            .save_conversation_snapshot(
                &saved,
                &[],
                crate::domain::TerminalMode::Normal,
                &overrides,
            )
            .unwrap();

        let roster = FreshChatRoster::default_main(BackendKind::Codex);
        let fresh = start_fresh_chat(&store, saved, Some(&roster), true).unwrap();

        assert_eq!(member_ids(&fresh), ["main"]);
        assert_eq!(fresh.max_auto_relays, 9);
        // The same chat is reused, with its snapshot rewritten to match.
        assert_eq!(store.current_conversation().unwrap(), blank);
        let count: i64 = rusqlite::Connection::open(&db_path)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM conversations", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
        let snapshot = store.conversation_snapshot(blank).unwrap().unwrap();
        assert_eq!(member_ids(&snapshot.team), ["main"]);
        assert_eq!(snapshot.mode_overrides, overrides);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn without_a_fresh_roster_a_new_chat_keeps_the_top_level_members() {
        let dir =
            std::env::temp_dir().join(format!("asterline-app-no-roster-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let saved = builder_and_reviewer_team(&dir);
        let store = SqliteStore::in_memory().unwrap();
        store.current_conversation().unwrap();
        let turn = store.create_turn().unwrap();
        store
            .record_user(turn, std::slice::from_ref(&saved.members[0].id), "hello")
            .unwrap();

        let fresh = start_fresh_chat(&store, saved, None, true).unwrap();

        assert_eq!(member_ids(&fresh), ["builder", "reviewer"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn launch_with_an_old_saved_roster_shows_only_the_default_main() {
        let dir =
            std::env::temp_dir().join(format!("asterline-app-old-roster-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let old = builder_and_reviewer_team(&dir);
        let team_path = state_paths::team_config_path(&dir);
        std::fs::create_dir_all(team_path.parent().unwrap()).unwrap();
        runtime::save_team_config(&team_path, &old).unwrap();
        let db_path = dir.join("db.sqlite3");
        let store = SqliteStore::open(&db_path).unwrap();
        let old_chat = store.current_conversation().unwrap();
        store
            .save_conversation_snapshot(
                &old,
                &[],
                crate::domain::TerminalMode::Normal,
                &ModesConfig::default(),
            )
            .unwrap();
        let turn = store.create_turn().unwrap();
        store
            .record_user(turn, std::slice::from_ref(&old.members[0].id), "hello")
            .unwrap();
        drop(store);

        let config = AppConfig::parse([
            "--workspace",
            dir.to_str().unwrap(),
            "--db",
            db_path.to_str().unwrap(),
            "--fake",
        ])
        .unwrap();
        let prepared = prepare_with(&config, &dir, &|| Some(BackendKind::Claude))
            .unwrap()
            .expect("prepared");
        let ready = prepared
            .events
            .recv_timeout(Duration::from_secs(2))
            .expect("ready");
        match ready {
            RuntimeEvent::Ready { members, .. } => {
                assert_eq!(members.len(), 1);
                assert_eq!(members[0].id.as_str(), "main");
                assert_eq!(members[0].backend, BackendKind::Claude);
            }
            other => panic!("expected Ready, got {other:?}"),
        }
        prepared.handle.send(UiCommand::Shutdown);
        prepared.join.join().unwrap();

        let saved = load_team_config(&team_path).unwrap();
        assert_eq!(member_ids(&saved), ["main"]);
        assert_eq!(saved.max_auto_relays, 9);
        let store = SqliteStore::open(&db_path).unwrap();
        let previous = store.conversation_snapshot(old_chat).unwrap().unwrap();
        assert_eq!(member_ids(&previous.team), ["builder", "reviewer"]);
        drop(store);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn explicit_team_file_still_launches_with_its_own_roster() {
        let dir = std::env::temp_dir().join(format!(
            "asterline-app-explicit-roster-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let explicit = builder_and_reviewer_team(&dir);
        let team_path = dir.join("explicit-team.json");
        runtime::save_team_config(&team_path, &explicit).unwrap();
        let db_path = dir.join("db.sqlite3");
        // The previous chat has content, so launch opens a new chat.
        let store = SqliteStore::open(&db_path).unwrap();
        store.current_conversation().unwrap();
        let turn = store.create_turn().unwrap();
        store
            .record_user(turn, std::slice::from_ref(&explicit.members[0].id), "hello")
            .unwrap();
        drop(store);

        let config = AppConfig::parse([
            "--team",
            team_path.to_str().unwrap(),
            "--db",
            db_path.to_str().unwrap(),
            "--fake",
        ])
        .unwrap();
        let prepared = prepare(&config, &dir).unwrap().expect("prepared");
        let ready = prepared
            .events
            .recv_timeout(Duration::from_secs(2))
            .expect("ready");
        match ready {
            RuntimeEvent::Ready { members, .. } => {
                let ids: Vec<&str> = members.iter().map(|m| m.id.as_str()).collect();
                assert_eq!(ids, ["builder", "reviewer"]);
            }
            other => panic!("expected Ready, got {other:?}"),
        }
        prepared.handle.send(UiCommand::Shutdown);
        prepared.join.join().unwrap();

        let store = SqliteStore::open(&db_path).unwrap();
        let snapshot = store
            .conversation_snapshot(store.current_conversation().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(member_ids(&snapshot.team), ["builder", "reviewer"]);
        drop(store);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn explicit_team_is_not_overwritten_by_active_conversation_snapshot() {
        let dir = std::env::temp_dir().join(format!(
            "asterline-app-explicit-team-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut explicit = crate::domain::config::default_team(
            &dir,
            crate::domain::config::DetectedBackends {
                codex: true,
                claude: false,
                grok: false,
                agy: false,
            },
        )
        .unwrap();
        explicit.members[0].effort = Some(crate::domain::team::Effort::Low);
        let team_path = dir.join("explicit-team.json");
        runtime::save_team_config(&team_path, &explicit).unwrap();

        let db_path = dir.join("db.sqlite3");
        let store = SqliteStore::open(&db_path).unwrap();
        store.current_conversation().unwrap();
        let mut stale = explicit.clone();
        stale.members[0].effort = Some(crate::domain::team::Effort::High);
        stale.members[0].display_name = "Stale snapshot member".to_string();
        store
            .save_conversation_snapshot(
                &stale,
                &[],
                crate::domain::TerminalMode::Normal,
                &crate::domain::mode::ModesConfig::default(),
            )
            .unwrap();
        drop(store);

        let config = AppConfig::parse([
            "--team",
            team_path.to_str().unwrap(),
            "--db",
            db_path.to_str().unwrap(),
            "--fake",
        ])
        .unwrap();
        let prepared = prepare(&config, &dir).unwrap().expect("prepared");
        let ready = prepared
            .events
            .recv_timeout(Duration::from_secs(2))
            .expect("ready");
        assert!(matches!(
            ready,
            RuntimeEvent::Ready { members, .. }
                if members.first().is_some_and(|member|
                    member.effort == Some(crate::domain::team::Effort::Low)
                        && member.display_name != "Stale snapshot member")
        ));

        prepared.handle.send(UiCommand::Shutdown);
        prepared.join.join().unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn team_workspace_is_canonical_for_the_default_database() {
        let root = std::env::temp_dir().join(format!(
            "asterline-app-team-workspace-{}",
            std::process::id()
        ));
        let invocation = root.join("invocation");
        let team_workspace = root.join("team-workspace");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&invocation).unwrap();
        std::fs::create_dir_all(&team_workspace).unwrap();

        let team = crate::domain::config::default_team(
            &team_workspace,
            crate::domain::config::DetectedBackends {
                codex: true,
                claude: false,
                grok: false,
                agy: false,
            },
        )
        .unwrap();
        let team_path = root.join("team.json");
        std::fs::write(&team_path, serde_json::to_string(&team).unwrap()).unwrap();
        let config = AppConfig::parse([
            "--team",
            team_path.to_str().unwrap(),
            "--fake",
            "--no-restore",
        ])
        .unwrap();

        let prepared = prepare(&config, &invocation).unwrap().expect("prepared");
        assert!(state_paths::db_path(&team_workspace).is_file());
        assert!(!state_paths::db_path(&invocation).exists());
        assert_eq!(std::fs::read_dir(&team_workspace).unwrap().count(), 0);
        assert_eq!(std::fs::read_dir(&invocation).unwrap().count(), 0);
        let ready = prepared
            .events
            .recv_timeout(Duration::from_secs(2))
            .expect("ready");
        assert!(matches!(
            ready,
            RuntimeEvent::Ready { workspace, .. }
                if workspace == team_workspace.display().to_string()
        ));

        prepared.handle.send(UiCommand::Shutdown);
        prepared.join.join().unwrap();
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn saved_roster_adopts_the_workspace_that_contains_it_after_a_project_move() {
        let root = std::env::temp_dir().join(format!(
            "asterline-app-moved-workspace-{}",
            std::process::id()
        ));
        let previous_workspace = root.join("previous-location");
        let moved_workspace = root.join("moved-location");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&previous_workspace).unwrap();
        // Resolve the same canonical path when saving and loading the roster
        // (macOS /var is a symlink; Windows canonical paths include a drive).
        std::fs::create_dir_all(&moved_workspace).unwrap();
        std::fs::create_dir_all(state_paths::project_dir(&moved_workspace)).unwrap();

        let team = crate::domain::config::default_team(
            &previous_workspace,
            crate::domain::config::DetectedBackends {
                codex: true,
                claude: false,
                grok: false,
                agy: false,
            },
        )
        .unwrap();
        let saved_team = state_paths::team_config_path(&moved_workspace);
        runtime::save_team_config(&saved_team, &team).unwrap();

        let config = AppConfig::parse(["--fake", "--no-restore"]).unwrap();
        let prepared = prepare(&config, &moved_workspace)
            .unwrap()
            .expect("prepared");
        let ready = prepared
            .events
            .recv_timeout(Duration::from_secs(2))
            .expect("ready");
        assert!(matches!(
            ready,
            RuntimeEvent::Ready { workspace, .. }
                if workspace == moved_workspace.display().to_string()
        ));
        let saved: TeamConfig = load_team_config(&saved_team).unwrap();
        assert_eq!(saved.workspace, moved_workspace);
        assert!(state_paths::db_path(&moved_workspace).is_file());
        assert!(!state_paths::db_path(&previous_workspace).exists());

        prepared.handle.send(UiCommand::Shutdown);
        prepared.join.join().unwrap();
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn cli_workspace_overrides_the_team_file_workspace() {
        let root = std::env::temp_dir().join(format!(
            "asterline-app-cli-workspace-{}",
            std::process::id()
        ));
        let declared_workspace = root.join("declared-workspace");
        let cli_workspace = root.join("cli-workspace");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&declared_workspace).unwrap();
        std::fs::create_dir_all(&cli_workspace).unwrap();

        let team = crate::domain::config::default_team(
            &declared_workspace,
            crate::domain::config::DetectedBackends {
                codex: true,
                claude: false,
                grok: false,
                agy: false,
            },
        )
        .unwrap();
        let team_path = root.join("team.json");
        std::fs::write(&team_path, serde_json::to_string(&team).unwrap()).unwrap();
        let config = AppConfig::parse([
            "--team",
            team_path.to_str().unwrap(),
            "--workspace",
            cli_workspace.to_str().unwrap(),
            "--fake",
            "--no-restore",
        ])
        .unwrap();

        let prepared = prepare(&config, &root).unwrap().expect("prepared");
        assert!(state_paths::db_path(&cli_workspace).is_file());
        assert!(!state_paths::db_path(&declared_workspace).exists());
        assert_eq!(std::fs::read_dir(&cli_workspace).unwrap().count(), 0);
        assert_eq!(std::fs::read_dir(&declared_workspace).unwrap().count(), 0);
        let ready = prepared
            .events
            .recv_timeout(Duration::from_secs(2))
            .expect("ready");
        assert!(matches!(
            ready,
            RuntimeEvent::Ready { workspace, .. }
                if workspace == cli_workspace.display().to_string()
        ));

        prepared.handle.send(UiCommand::Shutdown);
        prepared.join.join().unwrap();
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn startup_moves_old_in_project_state_and_the_chat_is_still_there() {
        let root =
            std::env::temp_dir().join(format!("asterline-app-legacy-{}", std::process::id()));
        let workspace = root.join("项目 with spaces");
        let _ = std::fs::remove_dir_all(&root);
        let old = workspace.join(".asterline");
        std::fs::create_dir_all(&old).unwrap();
        let team = crate::domain::config::default_team(
            &workspace,
            crate::domain::config::DetectedBackends {
                codex: true,
                claude: false,
                grok: false,
                agy: false,
            },
        )
        .unwrap();
        runtime::save_team_config(&old.join("team.json"), &team).unwrap();
        {
            let store = SqliteStore::open(old.join("asterline.sqlite3")).unwrap();
            store.current_conversation().unwrap();
            let turn = store.create_turn().unwrap();
            store
                .record_user(
                    turn,
                    std::slice::from_ref(&team.members[0].id),
                    "chat from the old layout",
                )
                .unwrap();
        }
        // Generated skill files from the old layout are cleaned up as well.
        let skill_dir = workspace.join(".agents/skills/asterline-team");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: asterline-team\n---\n<!-- managed-by: asterline (auto-upgraded; local edits will be overwritten) -->\n",
        )
        .unwrap();

        let config =
            AppConfig::parse(["--workspace", workspace.to_str().unwrap(), "--fake"]).unwrap();
        let prepared = prepare(&config, &root).unwrap().expect("prepared");

        // The migrated chat is no longer replayed on launch; `/resume` lists it.
        assert!(!prepared.state.chat().iter().any(|item| matches!(
            item,
            ChatItem::User { body, .. } if body == "chat from the old layout"
        )));
        assert!(
            prepared.state.chat().iter().any(|item| matches!(
                item,
                ChatItem::Notice { text, .. } if text.contains("Moved Asterline state")
            )),
            "the move is announced"
        );
        assert!(state_paths::db_path(&workspace).is_file());
        assert!(state_paths::team_config_path(&workspace).is_file());
        // The project folder is left with nothing of Asterline's.
        assert_eq!(std::fs::read_dir(&workspace).unwrap().count(), 0);

        prepared.handle.send(UiCommand::Shutdown);
        prepared.join.join().unwrap();
        let store = SqliteStore::open(state_paths::db_path(&workspace)).unwrap();
        assert!(
            store
                .resumable_conversations()
                .unwrap()
                .iter()
                .any(|chat| chat.preview == "chat from the old layout")
        );
        drop(store);
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(state_paths::project_dir(&workspace)).ok();
    }
}
