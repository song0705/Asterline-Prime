//! Loading and synthesizing [`TeamConfig`]: read a config file, detect which
//! backends are installed, and build a default in-memory roster.

use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;
use std::{env, fs, io};

use crate::adapter::models::{model_family_words, run_with_timeout};
use crate::domain::event::SpawnRequest;
use crate::domain::model_directory::ModelDirectory;
use crate::domain::team::{
    BackendKind, DefaultTarget, Effort, MAX_SUB_AGENTS, MemberId, PermissionMode, SandboxPolicy,
    TeamConfig, TeamMember, normalize_member_id,
};
use crate::fs_safety;
use crate::state_paths;

const TEAM_PROTOCOL_BEGIN: &str = "<!-- ASTERLINE_TEAM_PROTOCOL_BEGIN -->";
const TEAM_PROTOCOL_END: &str = "<!-- ASTERLINE_TEAM_PROTOCOL_END -->";
pub const ASTERLINE_TEAM_SKILL_NAME: &str = "asterline-team";
/// Bump when the embedded skill protocol gains breaking agent-facing changes.
pub const ASTERLINE_TEAM_SKILL_VERSION: u32 = 27;
const ASTERLINE_TEAM_SKILL: &str = include_str!("../../assets/skills/asterline-team/SKILL.md");
pub const ASTERLINE_BRAINSTORM_SKILL_NAME: &str = "asterline-brainstorm";
const ASTERLINE_BRAINSTORM_SKILL: &str =
    include_str!("../../assets/skills/asterline-brainstorm/SKILL.md");
const MANAGED_SKILL_MARKER: &str =
    "<!-- managed-by: asterline (auto-upgraded; local edits will be overwritten) -->";
const MIN_AGY_VERSION: (u64, u64, u64) = (1, 1, 12);
// Backend discovery runs alongside other startup and test subprocesses. Keep
// the probe bounded, but leave enough headroom that a loaded machine does not
// incorrectly hide an installed Agy binary.
const BACKEND_CAPABILITY_TIMEOUT: Duration = Duration::from_secs(5);

/// The shared team skill: `<asterline home>/skills/asterline-team/SKILL.md`.
pub fn team_skill_path() -> PathBuf {
    skill_file(&state_paths::skills_dir(), ASTERLINE_TEAM_SKILL_NAME)
}

/// The shared brainstorm skill:
/// `<asterline home>/skills/asterline-brainstorm/SKILL.md`.
pub fn brainstorm_skill_path() -> PathBuf {
    skill_file(&state_paths::skills_dir(), ASTERLINE_BRAINSTORM_SKILL_NAME)
}

/// Live roster and member status for `workspace`, rewritten whenever the team
/// or a status changes.
pub fn roster_path(workspace: &Path) -> PathBuf {
    state_paths::roster_path(workspace)
}

fn skill_file(skills_dir: &Path, name: &str) -> PathBuf {
    skills_dir.join(name).join("SKILL.md")
}

/// Ensure the shared team skill is present and, when Asterline manages it,
/// upgraded to the embedded protocol version. User-rewritten copies are left alone.
pub fn ensure_team_skill() -> io::Result<()> {
    ensure_team_skill_in(&state_paths::ensure_skills_root()?)
}

fn ensure_team_skill_in(skills_dir: &Path) -> io::Result<()> {
    let path = managed_skill_path(skills_dir, ASTERLINE_TEAM_SKILL_NAME)?;
    match fs::symlink_metadata(&path) {
        Ok(_) => {
            let existing = fs_safety::read_regular_to_string(&path, "team skill")?;
            if is_managed_skill(&existing)
                && skill_version(&existing) < ASTERLINE_TEAM_SKILL_VERSION
            {
                fs_safety::write_regular_file_atomic(&path, "team skill", ASTERLINE_TEAM_SKILL)?;
            }
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs_safety::write_regular_file_atomic(&path, "team skill", ASTERLINE_TEAM_SKILL)
        }
        Err(error) => Err(error),
    }
}

/// Install the default brainstorm protocol once. Existing deployment-local
/// copies are always preserved so teams can customize the method and card text.
pub fn ensure_brainstorm_skill() -> io::Result<()> {
    ensure_brainstorm_skill_in(&state_paths::ensure_skills_root()?)
}

fn ensure_brainstorm_skill_in(skills_dir: &Path) -> io::Result<()> {
    let path = managed_skill_path(skills_dir, ASTERLINE_BRAINSTORM_SKILL_NAME)?;
    match fs::symlink_metadata(&path) {
        Ok(_) => fs_safety::read_regular_to_string(&path, "brainstorm skill").map(|_| ()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs_safety::write_regular_file_atomic(
                &path,
                "brainstorm skill",
                ASTERLINE_BRAINSTORM_SKILL,
            )
        }
        Err(error) => Err(error),
    }
}

/// Load the brainstorm protocol, falling back to the embedded default for
/// tests and installs that have not been initialized yet.
pub fn brainstorm_skill_text() -> String {
    brainstorm_skill_text_in(&state_paths::skills_dir())
}

fn brainstorm_skill_text_in(skills_dir: &Path) -> String {
    fs_safety::read_regular_to_string(
        &skill_file(skills_dir, ASTERLINE_BRAINSTORM_SKILL_NAME),
        "brainstorm skill",
    )
    .unwrap_or_else(|_| ASTERLINE_BRAINSTORM_SKILL.to_string())
}

/// The embedded team skill, for migration tests.
#[cfg(test)]
pub(crate) fn embedded_team_skill() -> &'static str {
    ASTERLINE_TEAM_SKILL
}

/// The embedded brainstorm skill, for migration to tell a customized copy from
/// the stock one.
pub(crate) fn embedded_brainstorm_skill() -> &'static str {
    ASTERLINE_BRAINSTORM_SKILL
}

fn managed_skill_path(skills_dir: &Path, skill_name: &str) -> io::Result<PathBuf> {
    let directory = fs_safety::ensure_workspace_directory(skills_dir, &[skill_name], true)?;
    Ok(directory.join("SKILL.md"))
}

/// Frontmatter `version:` value; missing or invalid values are treated as v1.
fn skill_version(text: &str) -> u32 {
    let mut in_frontmatter = false;
    for line in text.lines() {
        let line = line.trim();
        if line == "---" {
            if in_frontmatter {
                break;
            }
            in_frontmatter = true;
            continue;
        }
        if in_frontmatter && let Some(rest) = line.strip_prefix("version:") {
            return rest.trim().parse().unwrap_or(1);
        }
    }
    1
}

/// True only for files that explicitly opt into Asterline-managed upgrades.
pub(crate) fn is_managed_skill(text: &str) -> bool {
    text.contains(MANAGED_SKILL_MARKER)
}

pub fn team_skill_hint() -> String {
    format!(
        "The Asterline team skill is available at {}. Read this skill when you need its team controls. \
         If a tool or plan is waiting for the user to approve in Asterline, wait — do not retry or assume it ran.",
        team_skill_path().display()
    )
}

/// Read and validate a team config from a JSON file.
pub fn load_team_config(path: &Path) -> io::Result<TeamConfig> {
    let text = fs_safety::read_regular_to_string(path, "team config")?;
    let config: TeamConfig =
        serde_json::from_str(&text).map_err(|err| invalid_config(path, err.to_string()))?;
    config
        .validate()
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err.to_string()))?;
    Ok(config)
}

fn invalid_config(path: &Path, err: String) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("invalid team config {}: {err}", path.display()),
    )
}

/// Which backend CLIs are available on the current `PATH`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DetectedBackends {
    pub codex: bool,
    pub claude: bool,
    pub grok: bool,
    pub agy: bool,
}

impl DetectedBackends {
    pub fn any(self) -> bool {
        self.codex || self.claude || self.grok || self.agy
    }

    /// The default main backend: Codex, then Claude, Grok, Agy.
    pub fn first(self) -> Option<BackendKind> {
        [
            BackendKind::Codex,
            BackendKind::Claude,
            BackendKind::Grok,
            BackendKind::Agy,
        ]
        .into_iter()
        .find(|backend| self.contains(*backend))
    }

    pub fn contains(self, backend: BackendKind) -> bool {
        match backend {
            BackendKind::Codex => self.codex,
            BackendKind::Claude => self.claude,
            BackendKind::Grok => self.grok,
            BackendKind::Agy => self.agy,
        }
    }
}

/// Detect supported backend CLIs on the current `PATH`.
pub fn detect_backends() -> DetectedBackends {
    let paths = env::var_os("PATH");
    let dirs: Vec<PathBuf> = paths
        .as_ref()
        .map(|value| env::split_paths(value).collect())
        .unwrap_or_default();
    DetectedBackends {
        codex: binary_in_dirs(&dirs, "codex"),
        claude: binary_in_dirs(&dirs, "claude"),
        grok: binary_in_dirs(&dirs, "grok"),
        agy: supported_agy_in_dirs(&dirs, env::var_os("PATHEXT").as_deref(), cfg!(windows)),
    }
}

fn binary_in_dirs(dirs: &[PathBuf], name: &str) -> bool {
    resolve_binary_in_dirs(dirs, name, env::var_os("PATHEXT").as_deref(), cfg!(windows)).is_some()
}

fn supported_agy_in_dirs(dirs: &[PathBuf], path_ext: Option<&OsStr>, windows: bool) -> bool {
    resolve_binary_in_dirs(dirs, "agy", path_ext, windows)
        .is_some_and(|binary| check_agy_version_path(&binary).is_ok())
}

pub(crate) fn check_agy_version(binary: &str) -> Result<(), String> {
    let resolved = resolve_binary_on_path(binary).unwrap_or_else(|| PathBuf::from(binary));
    check_agy_version_path(&resolved)
}

fn check_agy_version_path(binary: &Path) -> Result<(), String> {
    let program = binary.to_string_lossy();
    let output = run_with_timeout(
        &program,
        &["--version"],
        Path::new("."),
        BACKEND_CAPABILITY_TIMEOUT,
    )
    .map_err(|error| format!("Agy 1.1.12 or newer is required; {error}"))?;
    validate_agy_version(&String::from_utf8_lossy(&output.stdout))
}

pub(crate) fn validate_agy_version(raw: &str) -> Result<(), String> {
    let version = parse_agy_version(raw).ok_or_else(|| {
        format!(
            "Agy 1.1.12 or newer is required; could not parse version from `{}`",
            compact_version_output(raw, 80)
        )
    })?;
    let numeric = (version.major, version.minor, version.patch);
    if numeric < MIN_AGY_VERSION || (numeric == MIN_AGY_VERSION && version.prerelease) {
        return Err(format!(
            "Agy 1.1.12 or newer is required for structured streaming and reliable headless --mode enforcement; found {}",
            version.display
        ));
    }
    Ok(())
}

struct AgyVersion {
    major: u64,
    minor: u64,
    patch: u64,
    prerelease: bool,
    display: String,
}

fn parse_agy_version(raw: &str) -> Option<AgyVersion> {
    raw.split_whitespace().find_map(parse_agy_version_token)
}

fn parse_agy_version_token(token: &str) -> Option<AgyVersion> {
    let display = token.to_string();
    let token = token.trim_start_matches('v');
    let token = token.split_once('+').map_or(token, |(version, _)| version);
    let (core, prerelease) = token
        .split_once('-')
        .map_or((token, false), |(version, _)| (version, true));
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some(AgyVersion {
        major,
        minor,
        patch,
        prerelease,
        display,
    })
}

fn compact_version_output(raw: &str, max_chars: usize) -> String {
    raw.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(max_chars)
        .collect()
}

/// Resolve a runnable program exactly as Asterline will launch it. On Windows,
/// bare names honor `PATHEXT`; returning the concrete path is important because
/// Rust only appends `.exe` when `Command` is given an extensionless name.
pub(crate) fn resolve_binary_on_path(name: &str) -> Option<PathBuf> {
    let requested = Path::new(name);
    let has_path = requested.is_absolute()
        || requested.components().count() != 1
        || requested
            .components()
            .any(|component| !matches!(component, Component::Normal(_)));
    let path_ext = env::var_os("PATHEXT");
    if has_path {
        return resolve_binary_candidate(requested, path_ext.as_deref(), cfg!(windows));
    }
    let paths = env::var_os("PATH")?;
    let dirs = env::split_paths(&paths).collect::<Vec<_>>();
    resolve_binary_in_dirs(&dirs, name, path_ext.as_deref(), cfg!(windows))
}

fn resolve_binary_in_dirs(
    dirs: &[PathBuf],
    name: &str,
    path_ext: Option<&OsStr>,
    windows: bool,
) -> Option<PathBuf> {
    dirs.iter()
        .find_map(|dir| resolve_binary_candidate(&dir.join(name), path_ext, windows))
}

fn resolve_binary_candidate(
    requested: &Path,
    path_ext: Option<&OsStr>,
    windows: bool,
) -> Option<PathBuf> {
    executable_candidates(requested, path_ext, windows)
        .into_iter()
        .find(|candidate| candidate_is_executable(candidate, windows))
}

fn executable_candidates(
    requested: &Path,
    path_ext: Option<&OsStr>,
    windows: bool,
) -> Vec<PathBuf> {
    if !windows || requested.extension().is_some() {
        return vec![requested.to_path_buf()];
    }
    windows_executable_extensions(path_ext)
        .into_iter()
        .map(|extension| requested.with_extension(extension))
        .collect()
}

fn windows_executable_extensions(path_ext: Option<&OsStr>) -> Vec<OsString> {
    let raw = path_ext
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| OsStr::new(".COM;.EXE;.BAT;.CMD"));
    let mut extensions = Vec::new();
    for extension in raw.to_string_lossy().split(';') {
        let extension = extension.trim();
        if extension.len() <= 1
            || !extension.starts_with('.')
            || extension.contains('/')
            || extension.contains('\\')
        {
            continue;
        }
        let extension = OsString::from(&extension[1..]);
        if !extensions.iter().any(|known: &OsString| {
            known
                .to_string_lossy()
                .eq_ignore_ascii_case(&extension.to_string_lossy())
        }) {
            extensions.push(extension);
        }
    }
    extensions
}

fn candidate_is_executable(path: &Path, windows: bool) -> bool {
    if windows {
        path.is_file()
    } else {
        is_executable(path)
    }
}

/// User profile used for backend history and global skills. Native Windows
/// shells expose `USERPROFILE`; Unix shells expose `HOME`. Each platform also
/// accepts the other variable as a compatibility fallback.
pub(crate) fn user_home_dir() -> Option<PathBuf> {
    user_home_dir_from_values(
        env::var_os("HOME"),
        env::var_os("USERPROFILE"),
        cfg!(windows),
    )
}

fn user_home_dir_from_values(
    home: Option<OsString>,
    user_profile: Option<OsString>,
    windows: bool,
) -> Option<PathBuf> {
    let usable = |value: Option<OsString>| value.filter(|value| !value.is_empty());
    let selected = if windows {
        usable(user_profile).or_else(|| usable(home))
    } else {
        usable(home).or_else(|| usable(user_profile))
    }?;
    Some(PathBuf::from(selected))
}

pub(crate) fn codex_home_dir() -> Option<PathBuf> {
    codex_home_dir_from_values(env::var_os("CODEX_HOME"), user_home_dir())
}

fn codex_home_dir_from_values(
    codex_home: Option<OsString>,
    user_home: Option<PathBuf>,
) -> Option<PathBuf> {
    codex_home
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| user_home.map(|home| home.join(".codex")))
}

pub(crate) fn paths_equivalent(left: &Path, right: &Path) -> bool {
    let left = fs::canonicalize(left).unwrap_or_else(|_| left.to_path_buf());
    let right = fs::canonicalize(right).unwrap_or_else(|_| right.to_path_buf());
    paths_equivalent_for_platform(&left, &right, cfg!(windows))
}

fn paths_equivalent_for_platform(left: &Path, right: &Path, windows: bool) -> bool {
    if windows {
        windows_path_key(left) == windows_path_key(right)
    } else {
        left == right
    }
}

fn windows_path_key(path: &Path) -> String {
    let mut value = path.as_os_str().to_string_lossy().replace('/', "\\");
    if let Some(rest) = value.strip_prefix("\\\\?\\UNC\\") {
        value = format!("\\\\{rest}");
    } else if let Some(rest) = value.strip_prefix("\\\\?\\") {
        value = rest.to_string();
    }
    while value.ends_with('\\') && value.len() > 3 {
        value.pop();
    }
    value.to_lowercase()
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// Id of the single member a fresh workspace starts with.
pub const MAIN_MEMBER_ID: &str = "main";

/// Build the starting team when no config exists: one general-purpose `main`
/// member on the first detected backend (Codex, Claude, Grok, then Agy) with
/// that backend's safe permission defaults and no model override, so the CLI
/// keeps using its own configured default. `None` when no backend is installed.
pub fn default_team(
    workspace: impl Into<PathBuf>,
    detected: DetectedBackends,
) -> Option<TeamConfig> {
    let backend = detected.first()?;
    let mut config = TeamConfig::new("default", workspace).with_member(main_member(backend));
    config.default_target = Some(DefaultTarget::Member(MemberId::new(MAIN_MEMBER_ID)));
    Some(config)
}

/// The `main` member on `backend`, with that backend's default permissions.
pub fn main_member(backend: BackendKind) -> TeamMember {
    let mut member = TeamMember::new(MAIN_MEMBER_ID, "Main", backend, "");
    apply_backend_defaults(&mut member);
    member
}

/// Build the sub-agent an `@@spawn` asks for. Omitted backend, model, and effort
/// follow `parent`. A different backend gets that backend's default permission
/// preset; the same backend keeps the parent's. Errors are the reason shown to
/// the parent when the request is refused.
pub fn sub_agent_member(parent: &TeamMember, request: &SpawnRequest) -> Result<TeamMember, String> {
    if request.name.chars().any(char::is_control) {
        return Err("the name must not contain control characters".to_string());
    }
    let id = normalize_member_id(&request.name, "");
    if id.is_empty() {
        return Err(format!(
            "the name {:?} has no letters or digits to build an id from",
            request.name
        ));
    }
    if id.eq_ignore_ascii_case("all") {
        return Err("the name \"all\" is reserved for broadcast routing".to_string());
    }
    let backend = match request.backend.as_deref() {
        Some(raw) => BackendKind::try_from(raw.to_ascii_lowercase().as_str())
            .map_err(|_| format!("unknown backend {raw:?} (use claude, codex, grok, or agy)"))?,
        None => parent.backend,
    };
    let same_backend = backend == parent.backend;
    let effort = match request.effort.as_deref() {
        Some(raw) => {
            let effort = Effort::parse(raw).ok_or_else(|| {
                format!("unknown effort {raw:?} (use low, medium, high, xhigh, max, or ultra)")
            })?;
            if !effort.supported_by(backend) {
                return Err(format!(
                    "{backend} does not support {} effort",
                    effort.as_str()
                ));
            }
            Some(effort)
        }
        None => parent.effort.filter(|effort| effort.supported_by(backend)),
    };
    let model = match &request.model {
        Some(model) => Some(model.clone()),
        // A model name belongs to one backend; do not carry it across.
        None if same_backend => parent.model.clone(),
        None => None,
    };
    // The parent's job text is optional; it only colors protocol text.
    let role = request.role.clone().unwrap_or_default();
    let mut member = TeamMember::new(id, upper_first(request.name.trim()), backend, role);
    member.model = model;
    member.effort = effort;
    member.cwd = parent.cwd.clone();
    if same_backend {
        member.sandbox = parent.sandbox;
        member.permission_mode = parent.permission_mode;
        member.approvals_reviewer = parent.approvals_reviewer;
        member.allowed_tools = parent.allowed_tools.clone();
    } else {
        apply_backend_defaults(&mut member);
    }
    member.parent = Some(parent.id.clone());
    Ok(member)
}

/// Name given to a sub-agent whose requested name is only a model or backend.
pub const DEFAULT_SUB_AGENT_NAME: &str = "helper";

/// `text` with the first letter of each `-`/`_`-separated segment uppercased
/// and the rest as given: `test-writer` becomes `Test-Writer`.
pub(crate) fn upper_first(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut segment_start = true;
    for ch in text.chars() {
        if segment_start {
            out.extend(ch.to_uppercase());
        } else {
            out.push(ch);
        }
        segment_start = matches!(ch, '-' | '_');
    }
    out
}

/// Lowercase alphanumeric words of `text`, split on `-`, `.`, `_`, `/`, `:`,
/// spaces, and any other separator.
fn name_words(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|ch: char| !ch.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
}

/// A purely numeric word (`6`, `4`, `20251001`) or a short version tag (`v2`).
fn is_version_word(word: &str) -> bool {
    let digits = word.strip_prefix('v').unwrap_or(word);
    !digits.is_empty() && digits.chars().all(|ch| ch.is_ascii_digit())
}

/// Whether the member id `id` says nothing but which model or CLI the
/// sub-agent runs on: every non-version word is a backend name, a model family
/// word, or a word of one of `models` (`sol` for `gpt-6.1-sol`). A name that
/// adds a job (`opus-reviewer`) is not model-style.
pub fn is_model_style_name(id: &str, models: &[&str]) -> bool {
    let model_words: Vec<String> = models.iter().flat_map(|model| name_words(model)).collect();
    let mut words = name_words(id)
        .filter(|word| !is_version_word(word))
        .peekable();
    words.peek().is_some()
        && words.all(|word| {
            BackendKind::try_from(word.as_str()).is_ok()
                || model_family_words().any(|family| family == word)
                || model_words.contains(&word)
        })
}

/// If `child`'s id is model-style (see [`is_model_style_name`]) given the
/// model the request asked for and the one the child ended up with, rename it
/// to the first free `helper`, `helper-2`, `helper-3`, ... (`taken` says
/// whether a name already belongs to a member). Returns whether it renamed.
pub fn replace_model_style_name(
    child: &mut TeamMember,
    requested_model: Option<&str>,
    taken: impl Fn(&str) -> bool,
) -> bool {
    let models: Vec<&str> = requested_model
        .into_iter()
        .chain(child.model.as_deref())
        .collect();
    if !is_model_style_name(child.id.as_str(), &models) {
        return false;
    }
    let name = std::iter::once(DEFAULT_SUB_AGENT_NAME.to_string())
        .chain((2..).map(|n| format!("{DEFAULT_SUB_AGENT_NAME}-{n}")))
        .find(|name| !taken(name))
        .expect("an unbounded sequence has a free name");
    child.display_name = upper_first(&name);
    child.id = MemberId::new(name);
    true
}

/// Reset a member's sandbox and permission settings to its backend's defaults.
pub fn apply_backend_defaults(member: &mut TeamMember) {
    let defaults = default_member(member.backend);
    member.sandbox = defaults.sandbox;
    member.permission_mode = defaults.permission_mode;
    member.approvals_reviewer = defaults.approvals_reviewer;
}

/// The canonical permission preset for a backend. Custom rosters (roles,
/// sandboxes, prompts) come via a config file.
pub fn default_member(backend: BackendKind) -> TeamMember {
    match backend {
        BackendKind::Codex => {
            let mut m = TeamMember::new("builder", "Builder", BackendKind::Codex, "");
            m.apply_codex_permissions_preset(
                crate::domain::team::CodexPermissionsPreset::AskForApproval,
            );
            m
        }
        BackendKind::Claude => {
            let mut m = TeamMember::new("reviewer", "Reviewer", BackendKind::Claude, "");
            m.permission_mode = Some(PermissionMode::AcceptEdits);
            m
        }
        BackendKind::Grok => {
            let mut m = TeamMember::new("grok", "Grok", BackendKind::Grok, "");
            m.sandbox = SandboxPolicy::WorkspaceWrite;
            m.permission_mode = Some(PermissionMode::Auto);
            m
        }
        BackendKind::Agy => {
            let mut m = TeamMember::new("researcher", "Researcher", BackendKind::Agy, "");
            m.sandbox = SandboxPolicy::WorkspaceWrite;
            m.permission_mode = Some(PermissionMode::AcceptEdits);
            m
        }
    }
}

/// Prepend a compact Asterline team hint to each member's system prompt.
/// Detailed protocol lives in the shared skill under `<asterline home>/skills`.
pub fn inject_team_protocol(team: &mut TeamConfig) {
    inject_team_protocol_with(team, &ModelDirectory::default());
}

/// [`inject_team_protocol`], with the models the installed backends offer
/// listed for the members that may open sub-agents.
pub fn inject_team_protocol_with(team: &mut TeamConfig, models: &ModelDirectory) {
    let model_block = models.protocol_block();
    let roster = roster_path(&team.workspace);
    let protocols: Vec<String> = team
        .members
        .iter()
        .map(|me| match &me.parent {
            Some(parent) => build_sub_agent_protocol(me.id.as_str(), parent.as_str(), &roster),
            None => {
                let teammates: Vec<String> = team
                    .members
                    .iter()
                    .filter(|other| other.id != me.id && other.parent.is_none())
                    .map(|other| other.listing_label())
                    .collect();
                let sub_agents: Vec<String> = team
                    .sub_agents_of(&me.id)
                    .map(|child| child.listing_label())
                    .collect();
                build_protocol(
                    me.id.as_str(),
                    &teammates,
                    &sub_agents,
                    model_block.as_deref(),
                    &roster,
                )
            }
        })
        .collect();

    for (member, protocol) in team.members.iter_mut().zip(protocols) {
        let wrapped = format!("{TEAM_PROTOCOL_BEGIN}\n{protocol}\n{TEAM_PROTOCOL_END}");
        let existing = member.system_prompt.take().map(|prompt| {
            let stripped = strip_team_protocol(&prompt);
            stripped.trim().to_string()
        });
        member.system_prompt = match existing.filter(|prompt| !prompt.is_empty()) {
            Some(existing) => Some(format!("{wrapped}\n\n{existing}")),
            None => Some(wrapped),
        };
    }
}

/// Remove Asterline's injected protocol from system prompts before persisting a
/// user-editable team config.
pub fn strip_team_protocols(mut team: TeamConfig) -> TeamConfig {
    for member in &mut team.members {
        if let Some(prompt) = member.system_prompt.take() {
            let stripped = strip_team_protocol(&prompt);
            member.system_prompt = if stripped.trim().is_empty() {
                None
            } else {
                Some(stripped.trim().to_string())
            };
        }
    }
    team
}

pub fn strip_team_protocol(prompt: &str) -> String {
    let Some(begin) = prompt.find(TEAM_PROTOCOL_BEGIN) else {
        return prompt.to_string();
    };
    let after_begin = begin + TEAM_PROTOCOL_BEGIN.len();
    let Some(relative_end) = prompt[after_begin..].find(TEAM_PROTOCOL_END) else {
        return prompt.to_string();
    };
    let end = after_begin + relative_end + TEAM_PROTOCOL_END.len();
    let mut out = String::new();
    out.push_str(prompt[..begin].trim_end());
    if !out.is_empty() && !prompt[end..].trim_start().is_empty() {
        out.push_str("\n\n");
    }
    out.push_str(prompt[end..].trim_start());
    out
}

fn build_protocol(
    me: &str,
    teammates: &[String],
    sub_agents: &[String],
    model_block: Option<&str>,
    roster: &Path,
) -> String {
    let mut protocol = format!(
        "You are \"{me}\", a member of an Asterline multi-agent team.\n\
         {}\n{}\n",
        team_skill_hint(),
        roster_hint(roster)
    );
    if teammates.is_empty() {
        protocol.push_str("You are the only member.\n");
    } else {
        protocol.push_str(&format!("Teammates: {}.\n", teammates.join(", ")));
    }
    if !sub_agents.is_empty() {
        protocol.push_str(&format!("Your sub-agents: {}.\n", sub_agents.join(", ")));
    }
    protocol.push_str(&format!(
        "Sub-agents: when work can run in parallel, or a second model's view or a review is worth it, \
         open one with a control line (up to {MAX_SUB_AGENTS} per chat):\n\
         @@spawn {{\"name\":\"reviewer\",\"task\":\"what to do\",\"backend\":\"claude\",\"model\":\"opus\",\"effort\":\"high\"}}\n\
         backend, model, and effort are optional and default to yours. Name it after its job (reviewer, \
         test-writer, researcher; helper if none), never a model or CLI, since the list shows the model. \
         Its final reply comes back to you automatically as a message from it. To give a sub-agent more \
         work, send @@spawn again with the same name. To close a sub-agent you no longer need, send \
         @@close {{\"name\":\"reviewer\"}}.\n"
    ));
    if let Some(block) = model_block {
        protocol.push_str(block);
    }
    protocol.push_str("All other text you write is shown to the user.");
    protocol
}

/// Where an agent finds the live roster; the shared skill points here.
fn roster_hint(roster: &Path) -> String {
    format!(
        "The live team roster (default target, and every member's id, name, backend, and status) is at {}.",
        roster.display()
    )
}

fn build_sub_agent_protocol(me: &str, parent: &str, roster: &Path) -> String {
    format!(
        "You are \"{me}\", a sub-agent opened by \"{parent}\" in an Asterline multi-agent team.\n\
         {}\n{}\n\
         When {parent} gives you a task, your final reply is handed back to {parent} automatically; \
         you do not need to send it yourself. You cannot open sub-agents of your own.\n\
         All other text you write is shown to the user.",
        team_skill_hint(),
        roster_hint(roster)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upper_first_capitalises_each_hyphen_or_underscore_segment() {
        assert_eq!(upper_first("test-writer"), "Test-Writer");
        assert_eq!(upper_first("helper-2"), "Helper-2");
        assert_eq!(upper_first("code_review-bot"), "Code_Review-Bot");
        assert_eq!(upper_first("reviewer"), "Reviewer");
        assert_eq!(upper_first("code review!"), "Code review!");
        assert_eq!(upper_first("-x"), "-X");
        assert_eq!(upper_first(""), "");
    }

    fn detected(codex: bool, claude: bool, grok: bool, agy: bool) -> DetectedBackends {
        DetectedBackends {
            codex,
            claude,
            grok,
            agy,
        }
    }

    #[test]
    fn default_team_is_one_main_member_on_the_first_detected_backend() {
        let config = default_team("/tmp/ws", detected(true, true, true, true)).expect("team");

        assert!(config.validate().is_ok());
        assert_eq!(config.members.len(), 1);
        let main = &config.members[0];
        assert_eq!(main.id, MemberId::new("main"));
        assert_eq!(main.display_name, "Main");
        assert_eq!(main.role, "");
        assert_eq!(main.backend, BackendKind::Codex);
        assert_eq!(main.model, None);
        assert_eq!(main.effort, None);
        assert_eq!(config.default_member_ids(), vec![MemberId::new("main")]);
    }

    #[test]
    fn default_backend_priority_is_codex_claude_grok_agy() {
        for (flags, expected) in [
            ((true, true, true, true), BackendKind::Codex),
            ((false, true, true, true), BackendKind::Claude),
            ((false, false, true, true), BackendKind::Grok),
            ((false, false, false, true), BackendKind::Agy),
        ] {
            let config =
                default_team("/tmp/ws", detected(flags.0, flags.1, flags.2, flags.3)).unwrap();
            assert_eq!(config.members[0].backend, expected);
        }
    }

    #[test]
    fn default_main_member_uses_each_backends_permission_preset() {
        let codex = main_member(BackendKind::Codex);
        assert_eq!(codex.sandbox, SandboxPolicy::WorkspaceWrite);
        assert_eq!(codex.permission_mode, Some(PermissionMode::Auto));
        assert_eq!(
            codex.codex_permissions_preset(),
            Some(crate::domain::team::CodexPermissionsPreset::AskForApproval)
        );

        let claude = main_member(BackendKind::Claude);
        assert_eq!(claude.permission_mode, Some(PermissionMode::AcceptEdits));

        let grok = main_member(BackendKind::Grok);
        assert_eq!(grok.sandbox, SandboxPolicy::WorkspaceWrite);
        assert_eq!(grok.permission_mode, Some(PermissionMode::Auto));

        let agy = main_member(BackendKind::Agy);
        assert_eq!(agy.sandbox, SandboxPolicy::WorkspaceWrite);
        assert_eq!(agy.permission_mode, Some(PermissionMode::AcceptEdits));
    }

    #[test]
    fn apply_backend_defaults_resets_permissions_for_the_new_backend() {
        let mut member = main_member(BackendKind::Claude);
        member.backend = BackendKind::Grok;
        apply_backend_defaults(&mut member);
        assert_eq!(member.permission_mode, Some(PermissionMode::Auto));
        assert_eq!(member.sandbox, SandboxPolicy::WorkspaceWrite);
    }

    #[test]
    fn no_backends_yields_no_default_team() {
        assert!(default_team("/tmp/ws", detected(false, false, false, false)).is_none());
    }

    #[test]
    fn binary_in_dirs_finds_existing_file() {
        let dir = std::env::temp_dir().join(format!("asterline-cfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bin = if cfg!(windows) {
            dir.join("faux-backend.EXE")
        } else {
            dir.join("faux-backend")
        };
        std::fs::write(&bin, b"#!/bin/sh\n").unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mut permissions = std::fs::metadata(&bin).unwrap().permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&bin, permissions).unwrap();
        }

        let dirs = vec![dir.clone()];
        assert!(binary_in_dirs(&dirs, "faux-backend"));
        assert!(!binary_in_dirs(&dirs, "nope-backend"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn agy_capability_detection_rejects_versions_before_1_1_12() {
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().join(format!(
            "asterline-cfg-agy-capability-{}",
            std::process::id()
        ));
        let old_dir = root.join("old");
        let supported_dir = root.join("supported");
        let _ = std::fs::remove_dir_all(&root);
        for (dir, version) in [(&old_dir, "1.1.11"), (&supported_dir, "1.1.12")] {
            std::fs::create_dir_all(dir).unwrap();
            let binary = dir.join("agy");
            std::fs::write(&binary, format!("#!/bin/sh\nprintf '{version}\\r\\n'\n")).unwrap();
            let mut permissions = std::fs::metadata(&binary).unwrap().permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&binary, permissions).unwrap();
        }

        assert!(!supported_agy_in_dirs(&[old_dir], None, false));
        assert!(supported_agy_in_dirs(&[supported_dir], None, false));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn windows_pathext_resolves_script_shims_without_global_env_mutation() {
        let dir =
            std::env::temp_dir().join(format!("asterline-cfg-pathext-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let shim = dir.join("faux-backend.CMD");
        std::fs::write(&shim, b"@echo off\r\n").unwrap();

        let resolved = resolve_binary_in_dirs(
            std::slice::from_ref(&dir),
            "faux-backend",
            Some(OsStr::new(".EXE;.CMD;.cmd")),
            true,
        );

        assert_eq!(resolved, Some(shim));
        assert_eq!(
            windows_executable_extensions(Some(OsStr::new(".EXE;.CMD;.cmd"))),
            vec![OsString::from("EXE"), OsString::from("CMD")]
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn home_selection_covers_windows_userprofile_and_shell_fallbacks() {
        assert_eq!(
            user_home_dir_from_values(
                Some(OsString::from("/shell/home")),
                Some(OsString::from(r"C:\Users\Ada")),
                true,
            ),
            Some(PathBuf::from(r"C:\Users\Ada"))
        );
        assert_eq!(
            user_home_dir_from_values(
                Some(OsString::from("/shell/home")),
                Some(OsString::from(r"C:\Users\Ada")),
                false,
            ),
            Some(PathBuf::from("/shell/home"))
        );
        assert_eq!(
            user_home_dir_from_values(None, Some(OsString::from(r"C:\Users\Ada")), false),
            Some(PathBuf::from(r"C:\Users\Ada"))
        );
        assert_eq!(
            user_home_dir_from_values(Some(OsString::new()), Some(OsString::new()), true),
            None
        );
    }

    #[test]
    fn codex_home_prefers_explicit_override_without_requiring_user_home() {
        assert_eq!(
            codex_home_dir_from_values(Some(OsString::from("/custom/codex")), None),
            Some(PathBuf::from("/custom/codex"))
        );
        assert_eq!(
            codex_home_dir_from_values(Some(OsString::new()), Some(PathBuf::from("/shell/home")),),
            Some(PathBuf::from("/shell/home/.codex"))
        );
    }

    #[test]
    fn windows_project_paths_ignore_separator_case_and_verbatim_prefix() {
        assert!(paths_equivalent_for_platform(
            Path::new(r"C:\Work\Repo\"),
            Path::new("c:/work/repo"),
            true,
        ));
        assert!(paths_equivalent_for_platform(
            Path::new(r"\\?\C:\Work\Repo"),
            Path::new(r"c:\work\repo"),
            true,
        ));
        assert!(!paths_equivalent_for_platform(
            Path::new(r"C:\Work\Repo"),
            Path::new(r"C:\Work\Other"),
            true,
        ));
    }

    #[cfg(unix)]
    #[test]
    fn binary_in_dirs_rejects_non_executable_file() {
        let dir = std::env::temp_dir().join(format!(
            "asterline-cfg-non-executable-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("faux-backend"), b"#!/bin/sh\n").unwrap();

        assert!(!binary_in_dirs(std::slice::from_ref(&dir), "faux-backend"));

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn default_members_carry_no_hidden_role() {
        for backend in [
            BackendKind::Codex,
            BackendKind::Claude,
            BackendKind::Grok,
            BackendKind::Agy,
        ] {
            assert_eq!(default_member(backend).role, "");
        }
        assert_eq!(default_member(BackendKind::Grok).backend, BackendKind::Grok);
        assert_eq!(default_member(BackendKind::Agy).backend, BackendKind::Agy);
    }

    #[test]
    fn default_member_uses_permissive_backend_controls() {
        let codex = default_member(BackendKind::Codex);
        assert_eq!(codex.sandbox, SandboxPolicy::WorkspaceWrite);
        assert_eq!(codex.permission_mode, Some(PermissionMode::Auto));
        assert_eq!(
            codex.codex_permissions_preset(),
            Some(crate::domain::team::CodexPermissionsPreset::AskForApproval)
        );

        let claude = default_member(BackendKind::Claude);
        assert_eq!(claude.permission_mode, Some(PermissionMode::AcceptEdits));

        let grok = default_member(BackendKind::Grok);
        assert_eq!(grok.sandbox, SandboxPolicy::WorkspaceWrite);
        assert_eq!(grok.permission_mode, Some(PermissionMode::Auto));

        let agy = default_member(BackendKind::Agy);
        assert_eq!(agy.sandbox, SandboxPolicy::WorkspaceWrite);
        assert_eq!(agy.permission_mode, Some(PermissionMode::AcceptEdits));
    }

    #[test]
    fn team_protocol_is_injected_and_stripped_for_persistence() {
        let mut member = TeamMember::new("builder", "Builder", BackendKind::Codex, "impl");
        member.system_prompt = Some("custom prompt".to_string());
        let mut config = TeamConfig::new("t", "/tmp/ws")
            .with_member(member)
            .with_member(TeamMember::new(
                "reviewer",
                "Reviewer",
                BackendKind::Claude,
                "review",
            ));

        inject_team_protocol(&mut config);
        let prompt = config.members[0].system_prompt.as_ref().unwrap();
        assert!(prompt.contains("Asterline team skill"));
        assert!(prompt.contains(&team_skill_path().display().to_string()));
        assert!(team_skill_path().is_absolute());
        assert!(
            prompt.contains(&roster_path(Path::new("/tmp/ws")).display().to_string()),
            "{prompt}"
        );
        assert!(!prompt.contains(".agents/skills"));
        assert!(!prompt.contains("$asterline-team"));
        assert!(!prompt.contains("@@team_message"));
        assert!(!prompt.contains("@@team_member"));
        assert!(prompt.contains("reviewer"));
        assert!(!prompt.contains("do not message"));
        assert!(prompt.contains("custom prompt"));

        let stripped = strip_team_protocols(config);
        assert_eq!(
            stripped.members[0].system_prompt.as_deref(),
            Some("custom prompt")
        );
        assert_eq!(stripped.members[1].system_prompt, None);
    }

    #[test]
    fn protocol_lists_teammates_with_a_role_only_when_one_is_set() {
        let mut config = TeamConfig::new("t", "/tmp/ws")
            .with_member(TeamMember::new("main", "Main", BackendKind::Codex, ""))
            .with_member(TeamMember::new("qa", "QA", BackendKind::Claude, ""))
            .with_member(TeamMember::new("ops", "Ops", BackendKind::Agy, "deploys"));

        inject_team_protocol(&mut config);
        let prompt = config.members[0].system_prompt.as_ref().unwrap();
        assert!(prompt.contains("Teammates: qa, ops [deploys]"), "{prompt}");
        assert!(!prompt.contains("qa ["));
    }

    #[test]
    fn ensure_team_skill_writes_repo_skill_when_missing() {
        let dir = std::env::temp_dir().join(format!("asterline-skill-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        ensure_team_skill_in(&dir).unwrap();

        let path = skill_file(&dir, ASTERLINE_TEAM_SKILL_NAME);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("name: asterline-team"));
        assert!(text.contains("@@team_message"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ensure_brainstorm_skill_installs_default_and_preserves_custom_copy() {
        let dir =
            std::env::temp_dir().join(format!("asterline-brainstorm-skill-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        ensure_brainstorm_skill_in(&dir).unwrap();

        let path = skill_file(&dir, ASTERLINE_BRAINSTORM_SKILL_NAME);
        let installed = std::fs::read_to_string(&path).unwrap();
        assert!(installed.contains("name: asterline-brainstorm"));
        assert!(installed.contains("@@brainstorm_card"));
        assert!(installed.contains("@@brainstorm_vote"));

        let custom = installed.replace(
            "Use relevance, novelty,",
            "Use deployment-specific value, novelty,",
        );
        std::fs::write(&path, &custom).unwrap();
        ensure_brainstorm_skill_in(&dir).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), custom);
        assert_eq!(brainstorm_skill_text_in(&dir), custom);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ensure_team_skill_upgrades_managed_v4_file() {
        let dir =
            std::env::temp_dir().join(format!("asterline-skill-upgrade-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = skill_file(&dir, ASTERLINE_TEAM_SKILL_NAME);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            format!(
                "---\nname: asterline-team\nmetadata:\n  version: 4\ndescription: old\n---\n{MANAGED_SKILL_MARKER}\n\n# Old protocol\n@@team_message\n"
            ),
        )
        .unwrap();

        ensure_team_skill_in(&dir).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("version: 27"));
        assert!(text.contains("@@review"));
        assert!(text.contains("Do not send `@@team_message` merely because teammates are listed"));
        assert!(text.contains(MANAGED_SKILL_MARKER));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ensure_team_skill_leaves_user_rewritten_file_alone() {
        let dir =
            std::env::temp_dir().join(format!("asterline-skill-custom-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = skill_file(&dir, ASTERLINE_TEAM_SKILL_NAME);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let custom = "# My custom team notes\nDo not overwrite me.\n";
        std::fs::write(&path, custom).unwrap();

        ensure_team_skill_in(&dir).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text, custom);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ensure_team_skill_preserves_valid_custom_skill_without_managed_marker() {
        let dir = std::env::temp_dir().join(format!(
            "asterline-skill-custom-frontmatter-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let path = skill_file(&dir, ASTERLINE_TEAM_SKILL_NAME);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let custom = "---\nname: asterline-team\nmetadata:\n  version: 1\n---\n# Custom protocol\n";
        std::fs::write(&path, custom).unwrap();

        ensure_team_skill_in(&dir).unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), custom);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn managed_skills_reject_symlinks_and_never_read_their_targets() {
        use std::os::unix::fs::symlink;

        let dir =
            std::env::temp_dir().join(format!("asterline-skill-symlink-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let victim = dir.join("outside.txt");
        std::fs::create_dir_all(&dir).unwrap();
        let managed_old = format!("---\nversion: 1\n---\n{MANAGED_SKILL_MARKER}\n");
        std::fs::write(&victim, &managed_old).unwrap();

        let team_path = skill_file(&dir, ASTERLINE_TEAM_SKILL_NAME);
        std::fs::create_dir_all(team_path.parent().unwrap()).unwrap();
        symlink(&victim, &team_path).unwrap();
        assert!(ensure_team_skill_in(&dir).is_err());
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), managed_old);

        let brainstorm_path = skill_file(&dir, ASTERLINE_BRAINSTORM_SKILL_NAME);
        std::fs::create_dir_all(brainstorm_path.parent().unwrap()).unwrap();
        std::fs::write(&victim, "PRIVATE_LOCAL_CONTENT").unwrap();
        symlink(&victim, &brainstorm_path).unwrap();
        assert!(ensure_brainstorm_skill_in(&dir).is_err());
        assert!(!brainstorm_skill_text_in(&dir).contains("PRIVATE_LOCAL_CONTENT"));

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn embedded_team_skill_is_protocol_v26() {
        assert_eq!(skill_version(ASTERLINE_TEAM_SKILL), 27);
        assert!(
            ASTERLINE_TEAM_SKILL
                .lines()
                .any(|line| line.trim() == "version: 27")
        );
        assert!(ASTERLINE_TEAM_SKILL.contains(MANAGED_SKILL_MARKER));
        assert!(ASTERLINE_TEAM_SKILL.contains("@@review"));
        assert!(ASTERLINE_TEAM_SKILL.contains("Brainstorm Generation and Voting"));
        assert!(ASTERLINE_TEAM_SKILL.contains("$asterline-brainstorm"));
        assert!(ASTERLINE_TEAM_SKILL.contains("@@brainstorm_card"));
        assert!(ASTERLINE_TEAM_SKILL.contains("@@brainstorm_vote"));
        assert!(
            ASTERLINE_TEAM_SKILL
                .contains("Do not send `@@team_message` merely because teammates are listed")
        );
        assert!(
            ASTERLINE_TEAM_SKILL.contains("task involves search, research, review, or planning")
        );
        assert!(ASTERLINE_TEAM_SKILL.contains("`session_id`"));
        assert!(ASTERLINE_TEAM_SKILL.contains("`/mode plan`"));
        assert!(ASTERLINE_TEAM_SKILL.contains("@@run_step"));
        assert!(ASTERLINE_TEAM_SKILL.contains("Every Received Message Must Be Answered"));
        assert!(ASTERLINE_TEAM_SKILL.contains(r#""kind":"reply""#));
        assert!(
            ASTERLINE_TEAM_SKILL
                .contains("Writing the plan, review, or patch \"for the user\" is not delivery")
        );
        assert!(ASTERLINE_TEAM_SKILL.contains("`roster.md`"));
        assert!(!ASTERLINE_TEAM_SKILL.contains(".asterline/"));
        assert_eq!(ASTERLINE_TEAM_SKILL_VERSION, 27);
        assert!(ASTERLINE_TEAM_SKILL.contains("@@spawn"));
        assert!(ASTERLINE_TEAM_SKILL.contains("@@close"));
        assert!(ASTERLINE_TEAM_SKILL.contains("exact `model` ids"));
        assert!(ASTERLINE_TEAM_SKILL.contains("[hand-back from"));
        assert!(ASTERLINE_TEAM_SKILL.contains("at most 8 sub-agents"));
        assert!(ASTERLINE_TEAM_SKILL.contains("write defaults"));
        assert!(ASTERLINE_TEAM_SKILL.contains("Waiting For User Approval"));
    }

    #[test]
    fn load_team_config_round_trips_via_file() {
        let dir = std::env::temp_dir().join(format!("asterline-cfg-load-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("team.json");
        let config = default_team(&dir, detected(true, true, false, false)).unwrap();
        std::fs::write(&path, serde_json::to_string_pretty(&config).unwrap()).unwrap();

        let loaded = load_team_config(&path).expect("config loads");
        assert_eq!(loaded, config);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_team_config_derives_missing_member_ids() {
        let dir =
            std::env::temp_dir().join(format!("asterline-cfg-derived-id-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("team.json");
        std::fs::write(
            &path,
            r#"{
              "name": "manual",
              "workspace": "/tmp/ws",
              "default_target": { "member": "lead-engineer" },
              "members": [{
                "display_name": "Lead Engineer",
                "backend": "codex",
                "role": "implementation"
              }]
            }"#,
        )
        .unwrap();

        let config = load_team_config(&path).expect("config derives id from display_name");
        assert_eq!(config.members[0].id, MemberId::new("lead-engineer"));
        assert_eq!(
            config.default_member_ids(),
            vec![MemberId::new("lead-engineer")]
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_team_config_rejects_legacy_roundtable_modes_key() {
        let dir = std::env::temp_dir().join(format!(
            "asterline-cfg-legacy-roundtable-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("team.json");
        std::fs::write(
            &path,
            r#"{
              "name": "legacy",
              "workspace": "/tmp/ws",
              "members": [{
                "id": "builder",
                "display_name": "Builder",
                "backend": "codex",
                "role": "impl"
              }],
              "modes": { "roundtable": {} }
            }"#,
        )
        .unwrap();

        let err = load_team_config(&path).expect_err("legacy roundtable key must fail");
        let msg = err.to_string();
        assert!(
            msg.contains("invalid team config") && msg.contains("roundtable"),
            "startup error should name the file and unknown field: {msg}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    fn spawn(name: &str) -> SpawnRequest {
        SpawnRequest {
            name: name.to_string(),
            task: "task".to_string(),
            backend: None,
            model: None,
            effort: None,
            role: None,
        }
    }

    #[test]
    fn sub_agent_copies_the_parent_when_nothing_is_overridden() {
        let mut parent = main_member(BackendKind::Claude);
        parent.model = Some("opus".to_string());
        parent.effort = Some(Effort::High);
        parent.permission_mode = Some(PermissionMode::BypassPermissions);

        let child = sub_agent_member(&parent, &spawn("Reviewer")).unwrap();

        assert_eq!(child.id, MemberId::new("reviewer"));
        assert_eq!(child.display_name, "Reviewer");
        assert_eq!(child.role, "");
        assert_eq!(child.parent, Some(parent.id.clone()));
        assert_eq!(child.backend, BackendKind::Claude);
        assert_eq!(child.model.as_deref(), Some("opus"));
        assert_eq!(child.effort, Some(Effort::High));
        assert_eq!(
            child.permission_mode,
            Some(PermissionMode::BypassPermissions)
        );
    }

    #[test]
    fn sub_agent_on_another_backend_takes_that_backends_defaults() {
        let mut parent = main_member(BackendKind::Claude);
        parent.model = Some("opus".to_string());
        parent.effort = Some(Effort::Max);
        let mut request = spawn("coder");
        request.backend = Some("Codex".to_string());

        let child = sub_agent_member(&parent, &request).unwrap();

        assert_eq!(child.backend, BackendKind::Codex);
        assert_eq!(child.model, None);
        assert_eq!(child.effort, Some(Effort::Max));
        assert_eq!(
            child.codex_permissions_preset(),
            Some(crate::domain::team::CodexPermissionsPreset::AskForApproval)
        );
    }

    #[test]
    fn sub_agent_display_name_is_capitalised_and_the_id_is_not() {
        let parent = main_member(BackendKind::Claude);
        for (name, id, display) in [
            ("reviewer", "reviewer", "Reviewer"),
            ("test-writer", "test-writer", "Test-Writer"),
            ("helper-2", "helper-2", "Helper-2"),
            ("  code review ", "code-review", "Code review"),
            ("QA", "qa", "QA"),
            ("code_review-bot", "code_review-bot", "Code_Review-Bot"),
        ] {
            let child = sub_agent_member(&parent, &spawn(name)).unwrap();
            assert_eq!(child.id, MemberId::new(id), "{name}");
            assert_eq!(child.display_name, display, "{name}");
        }
    }

    fn renamed(name: &str, model: Option<&str>, taken: &[&str]) -> Option<(String, String)> {
        let mut parent = main_member(BackendKind::Claude);
        parent.model = None;
        let mut request = spawn(name);
        request.model = model.map(str::to_string);
        let mut child = sub_agent_member(&parent, &request).unwrap();
        replace_model_style_name(&mut child, model, |candidate| taken.contains(&candidate))
            .then(|| (child.id.to_string(), child.display_name))
    }

    #[test]
    fn model_style_names_become_helper() {
        let helper = Some(("helper".to_string(), "Helper".to_string()));
        assert_eq!(renamed("sol", Some("gpt-6.1-sol"), &[]), helper);
        assert_eq!(renamed("gpt-6-1-sol", Some("gpt-6.1-sol"), &[]), helper);
        assert_eq!(renamed("Sol", Some("gpt/6.1_sol:high"), &[]), helper);
        assert_eq!(renamed("opus", Some("opus"), &[]), helper);
        assert_eq!(renamed("opus", None, &[]), helper);
        assert_eq!(renamed("claude-opus", None, &[]), helper);
        assert_eq!(renamed("claude-opus-4-5", None, &[]), helper);
        assert_eq!(renamed("codex", None, &[]), helper);
        assert_eq!(renamed("agy", None, &[]), helper);
        assert_eq!(renamed("gemini", None, &[]), helper);
        assert_eq!(renamed("gpt", None, &[]), helper);
    }

    #[test]
    fn job_names_are_kept() {
        for (name, model) in [
            ("opus-reviewer", Some("opus")),
            ("reviewer", Some("gpt-6.1-sol")),
            ("researcher", Some("opus")),
            ("test-writer", None),
            ("helper", None),
            // `sol` is only a model word when the model says so.
            ("sol", None),
            ("4", Some("gpt-4")),
        ] {
            assert_eq!(renamed(name, model, &[]), None, "{name}");
        }
    }

    #[test]
    fn replacement_name_skips_taken_helpers() {
        assert_eq!(
            renamed("sol", Some("gpt-6.1-sol"), &["helper"]),
            Some(("helper-2".to_string(), "Helper-2".to_string()))
        );
        assert_eq!(
            renamed("codex", None, &["helper", "helper-2"]),
            Some(("helper-3".to_string(), "Helper-3".to_string()))
        );
    }

    #[test]
    fn model_style_check_uses_the_childs_effective_model() {
        let mut parent = main_member(BackendKind::Claude);
        parent.model = Some("gpt-6.1-sol".to_string());
        let mut child = sub_agent_member(&parent, &spawn("sol")).unwrap();
        assert!(replace_model_style_name(&mut child, None, |_| false));
        assert_eq!(child.id, MemberId::new("helper"));
    }

    #[test]
    fn protocol_tells_agents_to_name_sub_agents_after_their_job() {
        let protocol = build_protocol("main", &[], &[], None, Path::new("/tmp/ws/roster.md"));
        assert!(protocol.contains("Name it after its job"));
        assert!(protocol.contains("never a model or CLI"));
        assert!(protocol.contains(r#"{"name":"reviewer""#));
        assert!(ASTERLINE_TEAM_SKILL.contains("Name a sub-agent after its job"));
    }

    #[test]
    fn sub_agent_requests_with_bad_values_are_refused_with_a_reason() {
        let parent = main_member(BackendKind::Claude);
        for (mutate, reason) in [
            (
                Box::new(|r: &mut SpawnRequest| r.name = "!!!".to_string())
                    as Box<dyn Fn(&mut SpawnRequest)>,
                "no letters or digits",
            ),
            (Box::new(|r| r.name = "all".to_string()), "reserved"),
            (
                Box::new(|r| r.backend = Some("gpt".to_string())),
                "unknown backend",
            ),
            (
                Box::new(|r| r.effort = Some("huge".to_string())),
                "unknown effort",
            ),
            (
                Box::new(|r| r.effort = Some("ultra".to_string())),
                "does not support",
            ),
        ] {
            let mut request = spawn("helper");
            mutate(&mut request);
            let error = sub_agent_member(&parent, &request).unwrap_err();
            assert!(error.contains(reason), "{error}");
        }
    }
}
