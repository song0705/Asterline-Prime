//! The TUI model. Every field is driven by `RuntimeEvent`s applied through
//! [`AppState::apply`]; the renderer reads it and the key handler mutates the
//! composer / drawer / scroll. No state is inferred from matching strings.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::Instant;

use crossterm::event::{KeyCode, KeyModifiers};
use sha2::{Digest, Sha256};

use crate::adapter::parser::{append_bounded_text, bounded_text, summarize};
use crate::adapter::prompt_images::{
    MAX_IMAGE_BYTES, MAX_PROMPT_IMAGES, PromptImage, image_placeholder, mime_from_bytes,
};
use crate::domain::config::{DetectedBackends, MAIN_MEMBER_ID, detect_backends};
use crate::domain::event::{
    AgentSessionId, ApprovalId, ChatItem, ConversationSummary, LogEntry, MemberStatus, MessageId,
    MessageTarget, RunId, RunStatus, RunStepStatus, RunSummary, RuntimeEvent, UiCommand,
};
use crate::domain::goal::GoalState;
use crate::domain::mode::{ModesConfig, TerminalMode};
use crate::domain::team::{
    BackendKind, CodexApprovalsReviewer, DefaultTarget, Effort, MemberId, PermissionMode,
    SandboxPolicy, SessionPolicy, TeamMember,
};

use crate::runtime::context_recap::NOT_CARRIED_OVER;
use crate::tui::attach::AttachRequest;
use crate::tui::completion::{self, AgentSkill, Completion};

use crate::tui::composer::{Composer, MAX_COMPOSER_BYTES};
use crate::tui::drawers::Drawer;
use crate::tui::mode_editor::{ModeEditor, ModeEditorOutcome};
use crate::tui::model_picker::{
    self, ModelEntry, ModelKey, ModelPicker, PickerRow, PickerRowView, PickerView,
};
use crate::tui::skills::SkillInfo;
use crate::tui::team_builder::{CatalogView, ModelCatalog};
use crate::tui::team_editor::{TeamEditor, TeamEditorOutcome};
use crate::tui::tool_display;
use uuid::Uuid;

const MAX_LOGS: usize = 4_000;
const MAX_LOG_BYTES: usize = 2 * 1024 * 1024;
pub(crate) const MAX_CHAT_ITEMS: usize = 5_000;
pub(crate) const MAX_CHAT_ITEM_BYTES: usize = 256 * 1024;
const MAX_PROMPT_HISTORY_ITEMS: usize = 1_000;
const MAX_PROMPT_HISTORY_BYTES: usize = 1024 * 1024;
const MIN_BOUNDED_CHAT_TEXT_BYTES: usize = 64;
const MAX_ACTIVE_TOOL_ID_BYTES: usize = 4 * 1024;
const MAX_ACTIVE_TOOL_NAME_BYTES: usize = 4 * 1024;
const MAX_ACTIVE_TOOL_SUMMARY_BYTES: usize = 16 * 1024;
const MAX_REASONING_STATUS_HEADER_CHARS: usize = 160;
const ACTIVE_MESSAGE_OUTPUT_OMITTED: &str =
    "[asterline: live response preview omitted by TUI memory limit]";
const ACTIVE_TOOL_OUTPUT_OMITTED: &str =
    "[asterline: live tool output omitted by TUI memory limit]";
const ACTIVE_MESSAGE_OUTPUT_INTERRUPTED: &str =
    "[asterline: response ended before a completion event]";
const ACTIVE_TOOL_OUTPUT_INTERRUPTED: &str = "[asterline: tool ended before a completion event]";
const COMPOSER_INPUT_TRUNCATED: &str =
    "Input truncated at the 256 KiB composer limit; split it into smaller messages.";

pub(crate) fn member_status_is_active(status: MemberStatus) -> bool {
    matches!(
        status,
        MemberStatus::Running
            | MemberStatus::Queued
            | MemberStatus::Waiting
            | MemberStatus::NeedsApproval
    )
}

fn checklist_notice_run(text: &str) -> Option<&str> {
    let rest = text.strip_suffix(" checklist")?;
    let (_, run) = rest.rsplit_once(" updated ")?;
    run.starts_with("run-").then_some(run)
}

fn notices_are_redundant(existing: &str, incoming: &str) -> bool {
    existing == incoming
        || matches!(
            (checklist_notice_run(existing), checklist_notice_run(incoming)),
            (Some(left), Some(right)) if left == right
        )
}

fn reasoning_status_header(text: &str) -> Option<String> {
    if let Some((_, after_start)) = text.split_once("**")
        && let Some((header, _)) = after_start.split_once("**")
    {
        let header = summarize(header.trim(), MAX_REASONING_STATUS_HEADER_CHARS);
        if !header.is_empty() {
            return Some(header);
        }
    }
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let first_line = trimmed
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?
        .trim_start_matches(['#', '*', '-', '`', ' ', '\t']);
    let header = summarize(first_line, MAX_REASONING_STATUS_HEADER_CHARS);
    (!header.is_empty()).then_some(header)
}

fn literal_char_range(text: &str, literal: &str) -> Option<(usize, usize)> {
    let start_byte = text.find(literal)?;
    let start = text[..start_byte].chars().count();
    Some((start, start + literal.chars().count()))
}

#[derive(Clone, Debug)]
struct ActiveMessageCell {
    index: Option<usize>,
    member: MemberId,
    omitted: bool,
}

#[derive(Clone, Debug)]
struct ActiveToolCell {
    index: Option<usize>,
    name: String,
    summary: String,
    omitted: bool,
}

/// Header view of one member.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemberView {
    pub id: MemberId,
    pub display_name: String,
    pub backend: BackendKind,
    pub role: String,
    pub status: MemberStatus,
    pub session: Option<String>,
    pub cwd: String,
    pub model: Option<String>,
    pub effort: Option<Effort>,
    pub sandbox: SandboxPolicy,
    pub permission_mode: Option<PermissionMode>,
    pub approvals_reviewer: CodexApprovalsReviewer,
    pub session_policy: SessionPolicy,
    /// The member that opened this sub-agent; `None` for top-level members.
    pub parent: Option<MemberId>,
}

/// One selectable choice in the Agents drawer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AgentChoice {
    /// The interleaved view of every member.
    All,
    Member(MemberId),
    /// A saved chat (an Inactive row), by conversation id.
    Saved(i64),
}

/// The Agents list's rename editor: which chat it renames (`None` is the
/// current chat) and the text typed so far.
#[derive(Clone, Debug, Eq, PartialEq)]
struct AgentsRename {
    conversation: Option<i64>,
    text: String,
}

/// One row of the Agents list: the "All" choice, a member at a nesting
/// depth (0 for a top-level member, 1 for a sub-agent under its parent), or a
/// saved chat. Members are indexes into `members()`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentRow {
    All,
    Member {
        index: usize,
        depth: usize,
    },
    /// A saved chat: an index into `saved_chats()`.
    Saved {
        index: usize,
    },
}

/// How a focused member's window shows one chat item. `/find` uses the same
/// rule, so it only matches what is on screen.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FocusRow {
    /// Rendered exactly as in the "All" view.
    Full,
    Hidden,
}

pub(crate) fn focus_row(state: &AppState, item: &ChatItem) -> FocusRow {
    let Some(focus) = state.focus() else {
        return FocusRow::Full;
    };
    let own = |member: &MemberId| {
        if member == focus {
            FocusRow::Full
        } else {
            FocusRow::Hidden
        }
    };
    match item {
        ChatItem::Agent { member, .. }
        | ChatItem::Thinking { member, .. }
        | ChatItem::Tool { member, .. }
        | ChatItem::Diff { member, .. }
        | ChatItem::Verdict { member, .. } => own(member),
        ChatItem::Error { member, .. } => member.as_ref().map_or(FocusRow::Full, own),
        // A notice about one member belongs to that member's window; a
        // general one (mode change, `/new`, team update) shows everywhere.
        ChatItem::Notice { member, .. } => member.as_ref().map_or(FocusRow::Full, own),
        ChatItem::User { targets, .. } => {
            // Legacy rows have no recorded targets; keep them visible. A
            // prompt sent to someone else is not part of this conversation.
            if targets.is_empty() || targets.contains(focus) {
                FocusRow::Full
            } else {
                FocusRow::Hidden
            }
        }
        // An agent-to-agent row is shown to the member it leaves or reaches.
        ChatItem::Route { from, to, .. } => {
            if from == focus || route_reaches(state, focus, to) {
                FocusRow::Full
            } else {
                FocusRow::Hidden
            }
        }
        // The parent sees that it opened a sub-agent; the sub-agent's own
        // window starts at the task it received.
        ChatItem::Spawn { parent, .. } => own(parent),
        // The goal loop's turn marker belongs to the goal's member.
        ChatItem::GoalMarker { member, .. } => own(member),
    }
}

/// A route names its recipients by id or display name, or `all`.
fn route_reaches(state: &AppState, member: &MemberId, to: &[String]) -> bool {
    let display = state.member_display(member);
    to.iter().any(|name| {
        name.eq_ignore_ascii_case("all")
            || name.eq_ignore_ascii_case(member.as_str())
            || name.eq_ignore_ascii_case(&display)
    })
}

/// A pending approval awaiting a decision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingApproval {
    pub id: ApprovalId,
    pub member: Option<MemberId>,
    pub action: String,
    pub body: String,
}

/// Reverse incremental history search (Ctrl+R): a query and the index of the
/// currently-matched prompt-history entry.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct HistorySearch {
    query: String,
    match_idx: Option<usize>,
}

/// Inclusive start/end anchors for a mouse drag selection in the chat column.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChatSelection {
    pub start: (usize, usize),
    pub end: (usize, usize),
}

impl ChatSelection {
    pub fn normalized(self) -> ((usize, usize), (usize, usize)) {
        if self.start <= self.end {
            (self.start, self.end)
        } else {
            (self.end, self.start)
        }
    }

    pub fn is_empty(self) -> bool {
        self.start == self.end
    }
}

/// Transcript search (`/find`): query, matching chat indices, and current match.
#[derive(Clone, Debug, Eq, PartialEq)]
struct FindState {
    query: String,
    /// Indices into `chat` captured when the query was set.
    matches: Vec<usize>,
    /// Index into `matches` for the current jump target.
    current: usize,
}

pub struct AppState {
    team: String,
    workspace: String,
    default_target: Option<DefaultTarget>,
    active_mode: TerminalMode,
    modes: ModesConfig,
    mode_overrides: ModesConfig,
    suggested_verify: Option<String>,
    members: Vec<MemberView>,
    chat: Vec<ChatItem>,
    message_index: HashMap<MessageId, ActiveMessageCell>,
    tool_index: HashMap<(MemberId, String), ActiveToolCell>,
    logs: Vec<LogEntry>,
    log_bytes: usize,
    runs: Vec<RunSummary>,
    selected_run: Option<RunId>,
    selected_run_step: Option<u32>,
    runs_detail: bool,
    pending_approvals: Vec<PendingApproval>,
    selected_approval: usize,
    paused_routes: usize,
    composer: Composer,
    pending_images: Vec<PromptImage>,
    drawer: Option<Drawer>,
    scroll: usize,
    chat_page_rows: usize,
    /// Wrap width of the composer text as last drawn; Up/Down move by these
    /// wrapped rows. `usize::MAX` (nothing drawn yet) moves by logical line.
    composer_wrap: usize,
    popup_selected: usize,
    popup_dismissed: bool,
    should_quit: bool,
    quit_armed: bool,
    runtime_available: bool,
    thinking_expanded: bool,
    diffs_expanded: bool,
    tools_expanded: bool,
    /// Long tool groups the user opened with a click, keyed by the chat index
    /// of the group's first item. Ctrl+O (`tools_expanded`) overrides it.
    expanded_tool_groups: HashSet<usize>,
    /// Bumps on every change to `expanded_tool_groups`, for the paint cache.
    tool_groups_revision: u64,
    active_reasoning: HashMap<MemberId, String>,
    reasoning_buffers: HashMap<MemberId, String>,
    /// Legacy/imported thinking cells that still need an elapsed-time closeout.
    open_thinking: HashMap<MemberId, usize>,
    thinking_started: HashMap<MemberId, Instant>,
    last_message_target: Option<MessageTarget>,
    /// Member whose conversation fills the main window; `None` is the
    /// interleaved "All" view. Not persisted.
    focus: Option<MemberId>,
    /// True once the focus holds a deliberate value (the roster default or a
    /// user pick, including "All"), so a repeated `Ready` keeps it.
    focus_initialized: bool,
    /// Highlighted Agents row, kept by identity: rows reorder as member
    /// statuses change while the drawer is open.
    agents_choice: AgentChoice,
    /// Position `agents_choice` last had, used if that member leaves the
    /// roster.
    agents_fallback: usize,
    /// Search text typed into the Agents list; empty shows every agent in
    /// status sections.
    agents_query: String,
    /// The saved chat whose deletion Ctrl+X armed (a second Ctrl+X on the same
    /// row deletes it; any other key disarms).
    agents_delete_armed: Option<i64>,
    /// The rename editor that replaces the search line (Ctrl+R).
    agents_rename: Option<AgentsRename>,
    /// The name of the chat being viewed, if it has one.
    chat_name: Option<String>,
    /// The chat's goal (`/goal`), as the runtime last reported it.
    goal: Option<GoalState>,
    /// Screen rows of the Agents list's selectable entries, noted by the last
    /// paint so a mouse click can find its row.
    agents_click_rows: RefCell<Vec<(u16, AgentChoice)>>,
    /// When each member last finished a reply, for the Agents list's age.
    last_reply_at: HashMap<MemberId, Instant>,
    header_selected: Option<usize>,
    attach_pending: Option<MemberId>,
    attach_request: Option<AttachRequest>,
    attach_release_pending: Option<MemberId>,
    /// Shell-style prompt history (oldest→newest): prior submissions recalled
    /// with ↑/↓. Seeded from replayed user messages, appended as you submit.
    prompt_history: Vec<String>,
    /// Position in `prompt_history` while browsing, or `None` when editing the
    /// live draft.
    history_cursor: Option<usize>,
    /// The live draft saved when history browsing begins, restored on the way
    /// back past the newest entry.
    history_draft: String,
    /// When each currently-running member started, for the elapsed-time
    /// "working" indicator. Set on entering Running, cleared otherwise.
    running_since: HashMap<MemberId, Instant>,
    /// Active reverse history search (Ctrl+R), if any.
    history_search: Option<HistorySearch>,
    /// Active transcript search (`/find`), if any.
    find: Option<FindState>,
    /// Vertical scroll offset for the open drawer (logs / team / diff).
    drawer_scroll: usize,
    /// Last painted list height; the Agents list pages by it.
    drawer_viewport: Cell<usize>,
    /// Furthest the open drawer can scroll, noted by the renderer.
    drawer_max_scroll: Cell<usize>,
    /// Captured working-tree diff text for the diff drawer (`/diff`).
    diff_text: Option<String>,
    /// Editable draft shown by the `/team` drawer.
    team_editor: Option<TeamEditor>,
    /// Overlay for `/mode`.
    mode_editor: Option<ModeEditor>,
    /// Cross-drawer cache for asynchronously discovered backend models. The
    /// transient editor borrows it by ownership while open, then returns it on
    /// close so reopening `/team` does not rerun CLI discovery every time.
    model_catalog: ModelCatalog,
    /// Installed backends, known once the startup check finishes.
    detected_backends: Option<DetectedBackends>,
    /// The inline `/model` selector, while open.
    model_picker: Option<ModelPicker>,
    /// A `/model` roster change is in flight; see `Notice` handling.
    model_switch_pending: bool,
    /// Backend availability is checked asynchronously at startup so every
    /// installed CLI's workspace catalog can warm without blocking the TUI.
    model_catalog_detection: Option<Receiver<DetectedBackends>>,
    /// The active `ast` process warms all installed backends once at startup.
    /// Keeping that fixed avoids model-list subprocess churn while the Team
    /// changes.
    model_catalog_warmed: bool,
    skills: Vec<SkillInfo>,
    /// Saved (not live) chats, newest first: the Agents list's Inactive rows.
    /// Refreshed by the runtime each time the list opens.
    saved_chats: Vec<ConversationSummary>,
    /// `/resume` opened the Agents list and the saved chats may still be on
    /// their way: highlight the first one when they arrive, unless the user
    /// has moved the highlight by then.
    agents_select_inactive: bool,
    /// A command raised by a selection that has no handle to send it (an
    /// Inactive row opened by Enter or a click); the key loop sends it.
    pending_command: Option<UiCommand>,
    /// Test clock for the Agents list's relative ages (Unix seconds).
    clock_override: Option<i64>,
    /// Drag-select range in flattened chat-line coordinates, if any.
    chat_selection: Option<ChatSelection>,
    composer_dragging: bool,
    queued_prompts: HashMap<MemberId, Vec<String>>,
    /// Bumped whenever transcript text changes so the chat painter can reuse
    /// last frame's flatten instead of re-parsing Markdown on every spinner tick.
    chat_fingerprint: u64,
    /// Bumped when a `Ready` event replaces roster data used by the empty-chat
    /// welcome page.
    roster_revision: u64,
}

impl AppState {
    /// Create with replayed chat history (empty for a fresh session).
    pub fn new(mut chat: Vec<ChatItem>) -> Self {
        trim_initial_chat(&mut chat);
        // Seed prompt history from prior user messages (cross-session recall),
        // collapsing consecutive duplicates the way a shell history does.
        let mut prompt_history: Vec<String> = Vec::new();
        for item in &chat {
            if let ChatItem::User { body, .. } = item
                && prompt_history.last() != Some(body)
            {
                prompt_history.push(body.clone());
            }
        }
        trim_prompt_history(&mut prompt_history);
        Self {
            team: "Asterline".to_string(),
            workspace: String::new(),
            default_target: None,
            active_mode: TerminalMode::Normal,
            modes: ModesConfig::default(),
            mode_overrides: ModesConfig::default(),
            suggested_verify: None,
            members: Vec::new(),
            chat,
            message_index: HashMap::new(),
            tool_index: HashMap::new(),
            logs: Vec::new(),
            log_bytes: 0,
            runs: Vec::new(),
            selected_run: None,
            selected_run_step: None,
            runs_detail: false,
            pending_approvals: Vec::new(),
            selected_approval: 0,
            paused_routes: 0,
            composer: Composer::new(),
            pending_images: Vec::new(),
            drawer: None,
            scroll: 0,
            chat_page_rows: 10,
            composer_wrap: usize::MAX,
            popup_selected: 0,
            popup_dismissed: false,
            should_quit: false,
            quit_armed: false,
            runtime_available: true,
            thinking_expanded: false,
            diffs_expanded: true,
            tools_expanded: false,
            expanded_tool_groups: HashSet::new(),
            tool_groups_revision: 0,
            active_reasoning: HashMap::new(),
            reasoning_buffers: HashMap::new(),
            open_thinking: HashMap::new(),
            thinking_started: HashMap::new(),
            last_message_target: None,
            focus: None,
            focus_initialized: false,
            agents_choice: AgentChoice::All,
            agents_fallback: 0,
            agents_query: String::new(),
            agents_delete_armed: None,
            agents_rename: None,
            chat_name: None,
            goal: None,
            agents_click_rows: RefCell::new(Vec::new()),
            last_reply_at: HashMap::new(),
            header_selected: None,
            attach_pending: None,
            attach_request: None,
            attach_release_pending: None,
            prompt_history,
            history_cursor: None,
            history_draft: String::new(),
            running_since: HashMap::new(),
            history_search: None,
            find: None,
            drawer_scroll: 0,
            drawer_viewport: Cell::new(0),
            drawer_max_scroll: Cell::new(0),
            diff_text: None,
            mode_editor: None,
            team_editor: None,
            model_catalog: ModelCatalog::default(),
            detected_backends: None,
            model_picker: None,
            model_switch_pending: false,
            model_catalog_detection: None,
            model_catalog_warmed: false,
            skills: Vec::new(),
            saved_chats: Vec::new(),
            agents_select_inactive: false,
            pending_command: None,
            clock_override: None,
            chat_selection: None,
            composer_dragging: false,
            queued_prompts: HashMap::new(),
            chat_fingerprint: 0,
            roster_revision: 0,
        }
    }

    // --- applying runtime events ----------------------------------------

    /// Seed the logs drawer with persisted entries replayed on startup, so logs
    /// survive a restart the way the chat transcript does.
    pub fn seed_logs(&mut self, logs: Vec<LogEntry>) {
        self.logs.clear();
        self.log_bytes = 0;
        for entry in logs {
            self.push_log(entry);
        }
    }

    pub fn apply(&mut self, event: RuntimeEvent) {
        match event {
            RuntimeEvent::Ready {
                modes,
                mode_overrides,
                suggested_verify,
                team,
                workspace,
                default_target,
                members,
                runs,
            } => {
                // A refreshed roster invalidates any unsaved Team draft, but
                // its completed/in-flight model lookups remain reusable.
                self.stash_team_editor_catalog();
                self.team = team;
                self.skills = crate::tui::skills::discover(Path::new(&workspace));
                self.workspace = workspace;
                self.default_target = default_target;
                self.modes = modes;
                self.mode_overrides = mode_overrides;
                self.suggested_verify = suggested_verify;
                self.runs = runs;
                self.ensure_selected_run();
                self.ensure_selected_run_step();
                self.members = members
                    .into_iter()
                    .map(|m| MemberView {
                        id: m.id,
                        display_name: m.display_name,
                        backend: m.backend,
                        role: m.role,
                        status: m.status,
                        session: m.session,
                        cwd: m.cwd,
                        model: m.model,
                        effort: m.effort,
                        sandbox: m.sandbox,
                        permission_mode: m.permission_mode,
                        approvals_reviewer: m.approvals_reviewer,
                        session_policy: m.session_policy,
                        parent: m.parent,
                    })
                    .collect();
                let member_ids: std::collections::HashSet<MemberId> =
                    self.members.iter().map(|m| m.id.clone()).collect();
                self.running_since
                    .retain(|member, _| member_ids.contains(member));
                self.last_reply_at
                    .retain(|member, _| member_ids.contains(member));
                for member in &self.members {
                    if member_status_is_active(member.status) {
                        self.running_since
                            .entry(member.id.clone())
                            .or_insert_with(Instant::now);
                    } else {
                        self.running_since.remove(&member.id);
                    }
                }
                if let Some(idx) = self.header_selected
                    && idx >= self.members.len()
                {
                    self.header_selected = self.members.len().checked_sub(1);
                }
                self.sync_focus_with_roster();
                if self.drawer == Some(Drawer::Team) {
                    self.open_team_editor();
                }
                if self.drawer == Some(Drawer::Mode) {
                    self.open_mode_editor();
                }
                self.roster_revision = self.roster_revision.wrapping_add(1);
            }
            RuntimeEvent::ModesUpdated {
                defaults,
                overrides,
            } => {
                self.modes = defaults.clone();
                self.mode_overrides = overrides.clone();
                if let Some(editor) = self.mode_editor.as_mut() {
                    editor.sync_from_runtime(defaults, overrides);
                }
            }
            RuntimeEvent::ModeChanged { mode } => {
                self.active_mode = mode;
                if let Some(editor) = self.mode_editor.as_mut() {
                    editor.set_active_mode(mode);
                }
            }
            RuntimeEvent::TurnStarted { .. } => {}
            RuntimeEvent::TurnFinished { .. } => {
                if crate::tui::claude_export::is_sync_enabled() {
                    let workspace = std::path::Path::new(&self.workspace);
                    let session_id = self.members.iter().find_map(|m| m.session.as_deref());
                    if let Some(session_id) = session_id {
                        let _ = crate::tui::claude_export::export_chat_items_to_claude_jsonl(
                            workspace, session_id, &self.chat,
                        );
                    }
                }
            }
            RuntimeEvent::UserMessage { body, targets, .. } => {
                let interrupted = self
                    .members
                    .iter()
                    .filter(|member| {
                        member_status_is_active(member.status)
                            && !targets.iter().any(|target| target == &member.id)
                    })
                    .map(|member| member.id.clone())
                    .collect();
                self.push(ChatItem::User {
                    body,
                    targets,
                    interrupted,
                });
            }
            RuntimeEvent::QueueUpdated { member, prompts } => {
                if prompts.is_empty() {
                    self.queued_prompts.remove(&member);
                } else {
                    self.queued_prompts.insert(member, prompts);
                }
            }
            RuntimeEvent::QueuedPromptReturned { body, .. } => {
                if !self.composer.is_empty() {
                    self.history_draft = self.composer.text();
                }
                let _ = self.composer.set_text(&body);
                self.composer.home();
                self.reconcile_pending_images();
            }
            RuntimeEvent::MemberStatus { member, status } => {
                if !member_status_is_active(status) {
                    self.finish_incomplete_cells_for_member(&member);
                }
                self.set_status(&member, status);
            }
            RuntimeEvent::MessageStarted { msg, member, .. } => {
                self.close_thinking(&member);
                if self.message_index.contains_key(&msg) {
                    self.finish_incomplete_message(msg);
                } else if self.message_index.len() >= MAX_CHAT_ITEMS
                    && let Some(oldest_available) = self.message_index.keys().next().copied()
                {
                    self.finish_incomplete_message(oldest_available);
                }
                let active_member = member.clone();
                let (display_name, backend) = self.member_meta(&member);
                let idx = self.push(ChatItem::Agent {
                    member,
                    display_name,
                    backend,
                    text: String::new(),
                });
                let index = matches!(
                    self.chat.get(idx),
                    Some(ChatItem::Agent { member, .. }) if member == &active_member
                )
                .then_some(idx);
                self.message_index.insert(
                    msg,
                    ActiveMessageCell {
                        index,
                        member: active_member,
                        omitted: index.is_none(),
                    },
                );
            }
            RuntimeEvent::MessageDelta { msg, text } => {
                if let Some(idx) = self
                    .message_index
                    .get(&msg)
                    .filter(|cell| !cell.omitted)
                    .and_then(|cell| cell.index)
                    && let Some(ChatItem::Agent { text: body, .. }) = self.chat.get_mut(idx)
                {
                    let _ = append_bounded_text(body, &text, MAX_CHAT_ITEM_BYTES);
                    self.normalize_chat_item_at(idx);
                }
                self.trim_chat_for(0, 0);
            }
            RuntimeEvent::MessageCompleted { msg, text } => {
                if let Some(cell) = self.message_index.remove(&msg) {
                    self.close_thinking(&cell.member);
                    self.last_reply_at
                        .insert(cell.member.clone(), Instant::now());
                    let streamed = cell.index.and_then(|idx| match self.chat.get(idx) {
                        Some(ChatItem::Agent { text, member, .. })
                            if member == &cell.member && !text.is_empty() =>
                        {
                            Some(text.clone())
                        }
                        _ => None,
                    });
                    let text = if !text.is_empty() {
                        text
                    } else {
                        streamed.unwrap_or_default()
                    };
                    let completed_text =
                        active_completion_text(ACTIVE_MESSAGE_OUTPUT_OMITTED, &text, cell.omitted);
                    let mut updated_index = None;
                    if !cell.omitted
                        && let Some(idx) = cell.index
                        && let Some(ChatItem::Agent {
                            text: body, member, ..
                        }) = self.chat.get_mut(idx)
                        && member == &cell.member
                    {
                        if !completed_text.is_empty() || body.is_empty() {
                            *body = completed_text.clone();
                        }
                        updated_index = Some(idx);
                    }
                    if let Some(idx) = updated_index {
                        self.normalize_chat_item_at(idx);
                    } else {
                        let (display_name, backend) = self.member_meta(&cell.member);
                        self.push(ChatItem::Agent {
                            member: cell.member.clone(),
                            display_name,
                            backend,
                            text: completed_text,
                        });
                    }
                    self.active_reasoning.remove(&cell.member);
                }
                self.trim_chat_for(0, 0);
            }
            RuntimeEvent::Reasoning { member, text } => {
                self.update_reasoning_status(member, &text);
            }
            RuntimeEvent::ReasoningCompleted { member } => {
                self.close_thinking(&member);
            }
            RuntimeEvent::ToolStarted {
                member,
                tool_id,
                name,
                summary,
            } => {
                self.close_thinking(&member);
                let key = (member.clone(), active_tool_key(&tool_id));
                if self.tool_index.contains_key(&key) {
                    self.finish_incomplete_tool(&key);
                } else if self.tool_index.len() >= MAX_CHAT_ITEMS
                    && let Some(oldest_available) = self.tool_index.keys().next().cloned()
                {
                    // The chat hot cache and its live-cell metadata share the
                    // same item-count ceiling. Provider protocol bugs cannot
                    // create an unbounded set of never-completed tombstones.
                    self.finish_incomplete_tool(&oldest_available);
                }
                let fallback_name = bounded_text(&name, MAX_ACTIVE_TOOL_NAME_BYTES);
                let fallback_summary = bounded_text(&summary, MAX_ACTIVE_TOOL_SUMMARY_BYTES);
                let idx = self.push(ChatItem::Tool {
                    member,
                    name,
                    summary,
                    detail: String::new(),
                    ok: None,
                });
                let (index, active_name, active_summary) = match self.chat.get(idx) {
                    Some(ChatItem::Tool { name, summary, .. }) => (
                        Some(idx),
                        bounded_text(name, MAX_ACTIVE_TOOL_NAME_BYTES),
                        bounded_text(summary, MAX_ACTIVE_TOOL_SUMMARY_BYTES),
                    ),
                    _ => (None, fallback_name, fallback_summary),
                };
                self.tool_index.insert(
                    key,
                    ActiveToolCell {
                        index,
                        name: active_name,
                        summary: active_summary,
                        omitted: index.is_none(),
                    },
                );
            }
            RuntimeEvent::ToolProgress {
                member,
                tool_id,
                delta,
            } => {
                let tool_id = active_tool_key(&tool_id);
                if let Some(idx) = self
                    .tool_index
                    .get(&(member, tool_id))
                    .filter(|cell| !cell.omitted)
                    .and_then(|cell| cell.index)
                    && let Some(ChatItem::Tool { detail, .. }) = self.chat.get_mut(idx)
                {
                    let _ = append_bounded_text(detail, &delta, MAX_CHAT_ITEM_BYTES);
                    self.normalize_chat_item_at(idx);
                }
                self.trim_chat_for(0, 0);
            }
            RuntimeEvent::ToolCompleted {
                member,
                tool_id,
                ok,
                output,
            } => {
                let tool_id = active_tool_key(&tool_id);
                if let Some(cell) = self.tool_index.remove(&(member.clone(), tool_id)) {
                    let completed_detail = if cell.omitted {
                        Some(active_completion_text(
                            ACTIVE_TOOL_OUTPUT_OMITTED,
                            &output,
                            true,
                        ))
                    } else if output.is_empty() {
                        None
                    } else {
                        Some(bounded_text(&output, MAX_CHAT_ITEM_BYTES))
                    };
                    let mut updated_index = None;
                    if !cell.omitted
                        && let Some(idx) = cell.index
                        && let Some(ChatItem::Tool {
                            ok: cell_ok,
                            detail,
                            ..
                        }) = self.chat.get_mut(idx)
                    {
                        *cell_ok = Some(ok);
                        if let Some(completed_detail) = &completed_detail {
                            *detail = completed_detail.clone();
                        }
                        updated_index = Some(idx);
                    }
                    if let Some(idx) = updated_index {
                        self.normalize_chat_item_at(idx);
                    } else {
                        self.push(ChatItem::Tool {
                            member,
                            name: cell.name,
                            summary: cell.summary,
                            detail: completed_detail.unwrap_or_default(),
                            ok: Some(ok),
                        });
                    }
                } else {
                    self.push(ChatItem::Tool {
                        member,
                        name: "tool".to_string(),
                        summary: String::new(),
                        detail: bounded_text(&output, MAX_CHAT_ITEM_BYTES),
                        ok: Some(ok),
                    });
                }
                self.trim_chat_for(0, 0);
            }
            RuntimeEvent::Route { from, to, body, .. } => {
                self.push(ChatItem::Route { from, to, body });
            }
            RuntimeEvent::SubAgentSpawned {
                parent,
                child,
                summary,
            } => {
                self.push(ChatItem::Spawn {
                    parent,
                    child,
                    summary,
                });
            }
            RuntimeEvent::FileChange { member, files, ok } => {
                self.push(ChatItem::Diff { member, files, ok });
            }
            RuntimeEvent::RouteError {
                from,
                target,
                reason,
                body,
                ..
            } => {
                self.push(ChatItem::Error {
                    member: Some(from),
                    message: format!("route to {target} failed: {reason} — {body}"),
                });
            }
            RuntimeEvent::RoutePaused {
                from,
                to,
                reason,
                queued,
                ..
            } => {
                self.paused_routes = queued;
                self.push(ChatItem::Notice {
                    member: None,
                    text: format!(
                        "route paused {from} → {}: {reason} (queued {queued}; /retry to resume)",
                        to.join(", ")
                    ),
                });
            }
            RuntimeEvent::RouteQueueUpdated { queued } => {
                self.paused_routes = queued;
            }
            RuntimeEvent::SessionUpdated { member, session } => {
                if let Some(view) = self.members.iter_mut().find(|m| m.id == member) {
                    view.session = Some(session.0);
                }
            }
            RuntimeEvent::AttachGranted { member } => {
                let expected = self.attach_pending.take();
                if expected.as_ref() != Some(&member) {
                    self.push(ChatItem::Notice {
                        member: None,
                        text: format!("could not attach: unexpected grant for member {member}"),
                    });
                    self.attach_release_pending = Some(member);
                    return;
                }
                let Some((display_name, backend, session, cwd)) = self
                    .members
                    .iter()
                    .find(|view| view.id == member)
                    .map(|view| {
                        (
                            view.display_name.clone(),
                            view.backend,
                            view.session.clone(),
                            view.cwd.clone(),
                        )
                    })
                else {
                    self.push(ChatItem::Notice {
                        member: None,
                        text: format!("could not attach: runtime granted unknown member {member}"),
                    });
                    self.attach_release_pending = Some(member);
                    return;
                };
                // Claude can create a session with a caller-provided UUID.
                // Supplying it up front gives the transcript importer an exact
                // file to read after attach; it never needs to guess among
                // other Claude sessions created in the same workspace.
                let fresh_session = (backend == BackendKind::Claude && session.is_none())
                    .then(|| AgentSessionId(Uuid::new_v4().to_string()));
                self.attach_request = Some(AttachRequest {
                    member,
                    display_name,
                    backend,
                    session,
                    fresh_session,
                    cwd,
                });
            }
            RuntimeEvent::AttachDenied { member, reason } => {
                if self.attach_pending.as_ref() == Some(&member) {
                    self.attach_pending = None;
                }
                self.push(ChatItem::Notice {
                    member: Some(member.clone()),
                    text: format!("could not attach to {member}: {reason}"),
                });
            }
            RuntimeEvent::ApprovalRequested {
                id,
                member,
                action,
                body,
            } => {
                self.pending_approvals.push(PendingApproval {
                    id,
                    member,
                    action,
                    body,
                });
                if self.pending_approvals.len() == 1 {
                    self.selected_approval = 0;
                }
            }
            RuntimeEvent::ApprovalResolved { id, decision } => {
                self.pending_approvals.retain(|a| a.id != id);
                self.clamp_selected_approval();
                self.push(ChatItem::Notice {
                    member: None,
                    text: format!("approval {}", decision.as_str()),
                });
            }
            RuntimeEvent::MemberError { member, message } => {
                self.push(ChatItem::Error {
                    member: Some(member),
                    message,
                });
            }
            RuntimeEvent::RunUpdated { run } => {
                if let Some(existing) = self.runs.iter_mut().find(|r| r.id == run.id) {
                    *existing = run;
                } else {
                    self.runs.push(run);
                }
                self.ensure_selected_run();
                self.ensure_selected_run_step();
            }
            RuntimeEvent::Verdict {
                member,
                approve,
                summary,
                ..
            } => {
                self.push(ChatItem::Verdict {
                    member,
                    approve,
                    summary,
                });
            }
            RuntimeEvent::Log(entry) => {
                self.push_log(entry);
            }
            RuntimeEvent::MemberNotice { member, text } => {
                if !self.should_skip_notice(&text, Some(&member)) {
                    self.push(ChatItem::Notice {
                        text,
                        member: Some(member),
                    });
                }
            }
            RuntimeEvent::Notice(text) => {
                // `/model` records its own `Model: …` row, so the runtime's
                // generic roster confirmation for that one change is noise.
                // Any other notice (say, a rejection) ends the wait.
                let from_model_switch = std::mem::take(&mut self.model_switch_pending);
                if from_model_switch && text.starts_with("team updated:") {
                    // The runtime adds the suffix to its confirmation when
                    // the switch left part of the conversation behind; show
                    // it on the `Model: …` row the user just saw.
                    if text.ends_with(NOT_CARRIED_OVER) {
                        self.mark_last_model_notice_truncated();
                    }
                    return;
                }
                if !self.should_skip_notice(&text, None) {
                    self.push(ChatItem::Notice { text, member: None });
                }
            }
            RuntimeEvent::SessionReset => {
                // Begin a fresh chat: clear the transcript and in-flight cells,
                // but keep members, logs, and prompt history. Runs belong to
                // the previous conversation and remain reachable via /resume.
                self.active_mode = TerminalMode::Normal;
                self.mode_editor = None;
                self.goal = None;
                self.saved_chats.clear();
                self.chat_name = None;
                self.agents_delete_armed = None;
                self.agents_rename = None;
                self.chat.clear();
                self.clear_tool_group_expansion();
                self.touch_chat();
                self.message_index.clear();
                self.tool_index.clear();
                self.active_reasoning.clear();
                self.reasoning_buffers.clear();
                self.open_thinking.clear();
                self.thinking_started.clear();
                self.pending_approvals.clear();
                self.paused_routes = 0;
                self.last_message_target = None;
                self.attach_pending = None;
                self.attach_request = None;
                self.running_since.clear();
                self.find = None;
                self.runs.clear();
                self.selected_run = None;
                self.selected_run_step = None;
                self.runs_detail = false;
                self.scroll = 0;
                self.drawer = None;
                self.drawer_scroll = 0;
                self.chat_selection = None;
                self.queued_prompts.clear();
                self.stash_team_editor_catalog();
                for member in &mut self.members {
                    member.session = None;
                    member.session_policy = SessionPolicy::Fresh;
                    member.status = MemberStatus::Idle;
                }
            }
            RuntimeEvent::GoalUpdated { goal } => {
                self.goal = goal;
            }
            RuntimeEvent::GoalMarker { member, turn } => {
                self.push(ChatItem::GoalMarker { member, turn });
            }
            RuntimeEvent::ConversationRenamed { name } => {
                self.chat_name = name.filter(|name| !name.trim().is_empty());
            }
            RuntimeEvent::SavedChats { conversations } => {
                self.saved_chats = conversations;
                if self.agents_select_inactive && self.drawer == Some(Drawer::Agents) {
                    self.select_first_inactive();
                }
            }
            RuntimeEvent::ConversationResumed { chat, .. } => {
                // The chat being viewed changed, so the saved list is stale.
                self.saved_chats.clear();
                self.chat_name = None;
                self.agents_delete_armed = None;
                self.agents_rename = None;
                self.chat = chat;
                trim_initial_chat(&mut self.chat);
                self.clear_tool_group_expansion();
                self.touch_chat();
                self.message_index.clear();
                self.tool_index.clear();
                self.active_reasoning.clear();
                self.reasoning_buffers.clear();
                self.open_thinking.clear();
                self.thinking_started.clear();
                self.pending_approvals.clear();
                self.paused_routes = 0;
                self.last_message_target = None;
                self.attach_pending = None;
                self.attach_request = None;
                self.running_since.clear();
                self.find = None;
                self.scroll = 0;
                self.drawer = None;
                self.drawer_scroll = 0;
                self.chat_selection = None;
                self.queued_prompts.clear();
                self.stash_team_editor_catalog();
            }
        }
    }

    fn should_skip_notice(&self, text: &str, member: Option<&MemberId>) -> bool {
        for item in self.chat.iter().rev() {
            match item {
                ChatItem::Notice {
                    text: last,
                    member: last_member,
                } => {
                    return last_member.as_ref() == member && notices_are_redundant(last, text);
                }
                ChatItem::Agent { text, .. } if text.trim().is_empty() => {}
                _ => return false,
            }
        }
        false
    }

    fn push(&mut self, item: ChatItem) -> usize {
        let item = bound_chat_item(item);
        let incoming_bytes = chat_item_bytes(&item);
        self.trim_chat_for(1, incoming_bytes);
        // When the user has scrolled up to browse history, keep the view
        // pinned by growing the scroll offset to compensate for the new item.
        // The estimate is rough (we don't know the render width here) but
        // prevents the jarring jump-to-bottom on every new message.
        if self.scroll > 0 {
            let est_lines = estimate_item_lines(&item);
            self.scroll = self.scroll.saturating_add(est_lines);
        }
        self.chat.push(item);
        self.touch_chat();
        self.chat.len() - 1
    }

    fn push_log(&mut self, entry: LogEntry) {
        let entry = entry.bounded();
        self.log_bytes = self.log_bytes.saturating_add(entry.byte_len());
        self.logs.push(entry);

        let mut remove = 0;
        while remove < self.logs.len()
            && (self.logs.len() - remove > MAX_LOGS || self.log_bytes > MAX_LOG_BYTES)
        {
            self.log_bytes = self.log_bytes.saturating_sub(self.logs[remove].byte_len());
            remove += 1;
        }
        if remove > 0 {
            self.logs.drain(0..remove);
        }
    }

    fn normalize_chat_item_at(&mut self, index: usize) {
        let Some(slot) = self.chat.get_mut(index) else {
            return;
        };
        let item = std::mem::replace(slot, chat_truncation_notice());
        *slot = bound_chat_item(item);
        self.touch_chat();
    }

    /// A cancelled or failed backend can exit without emitting the matching
    /// message/tool completion events. Retire those live-cell indexes when the
    /// member becomes inactive so tombstones cannot leak indefinitely, while
    /// keeping an attributed, visibly incomplete final cell in the transcript.
    fn finish_incomplete_cells_for_member(&mut self, member: &MemberId) {
        let message_ids = self
            .message_index
            .iter()
            .filter_map(|(id, cell)| (&cell.member == member).then_some(*id))
            .collect::<Vec<_>>();
        for id in message_ids {
            self.finish_incomplete_message(id);
        }

        let tool_keys = self
            .tool_index
            .keys()
            .filter(|(tool_member, _)| tool_member == member)
            .cloned()
            .collect::<Vec<_>>();
        for key in tool_keys {
            self.finish_incomplete_tool(&key);
        }
        self.trim_chat_for(0, 0);
    }

    fn finish_incomplete_message(&mut self, id: MessageId) {
        let Some(cell) = self.message_index.remove(&id) else {
            return;
        };
        let updated_index = cell.index.filter(|&index| {
            if let Some(ChatItem::Agent {
                member, text: body, ..
            }) = self.chat.get_mut(index)
                && member == &cell.member
            {
                *body = interrupted_completion_text(
                    ACTIVE_MESSAGE_OUTPUT_OMITTED,
                    ACTIVE_MESSAGE_OUTPUT_INTERRUPTED,
                    body,
                    cell.omitted,
                );
                true
            } else {
                false
            }
        });
        if let Some(index) = updated_index {
            self.normalize_chat_item_at(index);
        } else {
            let (display_name, backend) = self.member_meta(&cell.member);
            self.push(ChatItem::Agent {
                member: cell.member,
                display_name,
                backend,
                text: interrupted_completion_text(
                    ACTIVE_MESSAGE_OUTPUT_OMITTED,
                    ACTIVE_MESSAGE_OUTPUT_INTERRUPTED,
                    "",
                    cell.omitted,
                ),
            });
        }
    }

    fn finish_incomplete_tool(&mut self, key: &(MemberId, String)) {
        let Some(cell) = self.tool_index.remove(key) else {
            return;
        };
        let updated_index = cell.index.filter(|&index| {
            if let Some(ChatItem::Tool {
                member: item_member,
                detail,
                ok,
                ..
            }) = self.chat.get_mut(index)
                && item_member == &key.0
            {
                *detail = interrupted_completion_text(
                    ACTIVE_TOOL_OUTPUT_OMITTED,
                    ACTIVE_TOOL_OUTPUT_INTERRUPTED,
                    detail,
                    cell.omitted,
                );
                *ok = Some(false);
                true
            } else {
                false
            }
        });
        if let Some(index) = updated_index {
            self.normalize_chat_item_at(index);
        } else {
            self.push(ChatItem::Tool {
                member: key.0.clone(),
                name: cell.name,
                summary: cell.summary,
                detail: interrupted_completion_text(
                    ACTIVE_TOOL_OUTPUT_OMITTED,
                    ACTIVE_TOOL_OUTPUT_INTERRUPTED,
                    "",
                    cell.omitted,
                ),
                ok: Some(false),
            });
        }
    }

    fn trim_chat_for(&mut self, _incoming_items: usize, _incoming_bytes: usize) {
        // Keep every item in the current conversation so the user can scroll
        // back to its first message. Only compact a live cell that itself
        // exceeded the per-item cap.
        for index in 0..self.chat.len() {
            let before = chat_item_bytes(&self.chat[index]);
            if before <= MAX_CHAT_ITEM_BYTES {
                continue;
            }
            if compact_active_chat_item(&mut self.chat[index]) {
                self.touch_chat();
                for cell in self.message_index.values_mut() {
                    if cell.index == Some(index) {
                        cell.omitted = true;
                    }
                }
                for cell in self.tool_index.values_mut() {
                    if cell.index == Some(index) {
                        cell.omitted = true;
                    }
                }
            }
        }
    }

    fn set_status(&mut self, member: &MemberId, status: MemberStatus) {
        if let Some(view) = self.members.iter_mut().find(|m| &m.id == member) {
            view.status = status;
        }
        // Every active state keeps the original elapsed timer until the runtime
        // explicitly idles. Queue depth is tracked separately from member status.
        if member_status_is_active(status) {
            self.running_since
                .entry(member.clone())
                .or_insert_with(Instant::now);
        } else {
            self.running_since.remove(member);
        }
        if status == MemberStatus::Idle
            || status == MemberStatus::Failed
            || status == MemberStatus::NeedsApproval
        {
            self.close_thinking(member);
        }
    }

    fn close_thinking(&mut self, member: &MemberId) {
        self.reasoning_buffers.remove(member);
        let started = self.thinking_started.remove(member);
        if let Some(idx) = self.open_thinking.remove(member)
            && let Some(ChatItem::Thinking { elapsed_secs, .. }) = self.chat.get_mut(idx)
        {
            *elapsed_secs = started.map(|t| t.elapsed().as_secs());
            self.touch_chat();
        }
        self.active_reasoning.remove(member);
    }

    fn update_reasoning_status(&mut self, member: MemberId, delta: &str) {
        let delta = bounded_text(delta, MAX_CHAT_ITEM_BYTES);
        self.thinking_started
            .entry(member.clone())
            .or_insert_with(Instant::now);
        let buffer = self.reasoning_buffers.entry(member.clone()).or_default();
        if !delta.is_empty() {
            if delta.starts_with(buffer.as_str()) {
                *buffer = delta;
            } else if !buffer.ends_with(&delta) {
                let _ = append_bounded_text(buffer, &delta, MAX_CHAT_ITEM_BYTES);
            }
            if let Some(header) = reasoning_status_header(buffer) {
                self.active_reasoning.insert(member.clone(), header);
            }
        }
        let current_text = buffer.clone();
        let (display_name, backend) = self.member_meta(&member);
        if backend != BackendKind::Codex {
            if let Some(&idx) = self.open_thinking.get(&member) {
                if !current_text.is_empty()
                    && let Some(ChatItem::Thinking { text, .. }) = self.chat.get_mut(idx)
                {
                    *text = current_text;
                    self.touch_chat();
                }
            } else {
                let idx = self.chat.len();
                self.chat.push(ChatItem::Thinking {
                    member: member.clone(),
                    display_name,
                    backend,
                    text: current_text,
                    elapsed_secs: None,
                });
                self.open_thinking.insert(member, idx);
                self.touch_chat();
            }
        }
    }

    pub(crate) fn member_meta(&self, member: &MemberId) -> (String, BackendKind) {
        if let Some(member) = self
            .members
            .iter()
            .find(|candidate| &candidate.id == member)
        {
            return (member.display_name.clone(), member.backend);
        }
        self.chat
            .iter()
            .rev()
            .find_map(|item| match item {
                ChatItem::Agent {
                    member: candidate,
                    display_name,
                    backend,
                    ..
                } if candidate == member => Some((display_name.clone(), *backend)),
                ChatItem::Thinking {
                    member: candidate,
                    backend,
                    ..
                } if candidate == member => Some((member.to_string(), *backend)),
                _ => None,
            })
            .unwrap_or_else(|| (member.to_string(), BackendKind::Codex))
    }

    pub fn member_color_index(&self, member: &MemberId) -> usize {
        let Some(index) = self
            .members
            .iter()
            .position(|candidate| &candidate.id == member)
        else {
            return 0;
        };
        let backend = self.members[index].backend;
        self.members[..index]
            .iter()
            .filter(|candidate| candidate.backend == backend)
            .count()
    }

    pub fn member_color(&self, member: &MemberId) -> ratatui::style::Color {
        let backend = self
            .members
            .iter()
            .find(|candidate| &candidate.id == member)
            .map(|candidate| candidate.backend)
            .unwrap_or_else(|| self.member_meta(member).1);
        crate::tui::theme::backend_color_shaded(backend, self.member_color_index(member))
    }

    pub fn member_color_bold(&self, member: &MemberId) -> ratatui::style::Style {
        crate::tui::theme::bold(self.member_color(member))
    }

    pub fn member_display(&self, member: &MemberId) -> String {
        self.members
            .iter()
            .find(|m| &m.id == member)
            .map(|m| m.display_name.clone())
            .unwrap_or_else(|| member.to_string())
    }

    pub fn has_active_message(&self, member_id: &MemberId) -> bool {
        self.message_index
            .values()
            .any(|cell| &cell.member == member_id)
    }

    pub fn omitted_active_output_count(&self) -> usize {
        self.message_index
            .values()
            .filter(|cell| cell.omitted && cell.index.is_none())
            .count()
            + self
                .tool_index
                .values()
                .filter(|cell| cell.omitted && cell.index.is_none())
                .count()
    }

    pub fn remember_user_message_target(&mut self, target: &MessageTarget) {
        self.remember_message_target(target);
    }

    pub fn last_resolvable_message_target(&self) -> Option<MessageTarget> {
        let target = self.last_message_target.clone()?;
        if self.resolve_local_targets(&target).is_empty() {
            None
        } else {
            Some(target)
        }
    }

    pub fn clear_last_message_target(&mut self) {
        self.last_message_target = None;
    }

    fn remember_message_target(&mut self, target: &MessageTarget) {
        match target {
            MessageTarget::All | MessageTarget::Member(_) => {
                self.last_message_target = Some(target.clone());
            }
            MessageTarget::Default | MessageTarget::Members(_) => {}
        }
    }

    pub fn resolve_local_targets(&self, target: &MessageTarget) -> Vec<MemberId> {
        match target {
            MessageTarget::Default => match &self.default_target {
                Some(DefaultTarget::All) => self.members.iter().map(|m| m.id.clone()).collect(),
                Some(DefaultTarget::Member(id)) => {
                    let resolved = self.resolve_local_named(std::slice::from_ref(id));
                    if resolved.is_empty() {
                        self.members
                            .first()
                            .map(|m| vec![m.id.clone()])
                            .unwrap_or_default()
                    } else {
                        resolved
                    }
                }
                None => self
                    .members
                    .first()
                    .map(|m| vec![m.id.clone()])
                    .unwrap_or_default(),
            },
            MessageTarget::All => self.members.iter().map(|m| m.id.clone()).collect(),
            MessageTarget::Member(id) => self.resolve_local_named(std::slice::from_ref(id)),
            MessageTarget::Members(ids) => self.resolve_local_named(ids),
        }
    }

    fn resolve_local_named(&self, ids: &[MemberId]) -> Vec<MemberId> {
        let mut resolved = Vec::new();
        for id in ids {
            if let Some(member) = self
                .members
                .iter()
                .find(|m| m.id == *id || m.display_name.eq_ignore_ascii_case(id.as_str()))
                && !resolved.contains(&member.id)
            {
                resolved.push(member.id.clone());
            }
        }
        resolved
    }

    fn resolve_member_id(&self, requested: &MemberId) -> Option<MemberId> {
        self.members
            .iter()
            .find(|member| {
                member.id == *requested
                    || member.display_name.eq_ignore_ascii_case(requested.as_str())
            })
            .map(|member| member.id.clone())
    }

    pub(crate) fn member_backend(&self, requested: &MemberId) -> Option<BackendKind> {
        let id = self.resolve_member_id(requested)?;
        self.members
            .iter()
            .find(|member| member.id == id)
            .map(|member| member.backend)
    }

    /// Return a target-safe prompt invocation only when the slash spelling is
    /// one Asterline actually discovered for that member's backend. This keeps
    /// unknown interactive controls out of noninteractive `codex exec`.
    pub(crate) fn targeted_skill_command(
        &self,
        requested: &MemberId,
        body: &str,
    ) -> Option<UiCommand> {
        let member = self.resolve_member_id(requested)?;
        let backend = self.member_backend(&member)?;
        let token = body.split_whitespace().next()?;
        let invocation = if backend == BackendKind::Codex && token.starts_with('/') {
            format!("${}", token.trim_start_matches('/'))
        } else {
            token.to_string()
        };
        self.skills
            .iter()
            .find(|skill| skill.backend == backend && skill.invocation == invocation)?;
        Some(
            self.normalize_known_skill_invocation(UiCommand::UserMessage {
                target: MessageTarget::Member(member.clone()),
                body: format!("@{member} {body}"),
            }),
        )
    }

    // --- accessors for the renderer -------------------------------------

    pub fn team(&self) -> &str {
        &self.team
    }
    pub fn active_mode(&self) -> TerminalMode {
        self.active_mode
    }
    pub fn modes(&self) -> &ModesConfig {
        &self.modes
    }
    pub fn mode_overrides(&self) -> &ModesConfig {
        &self.mode_overrides
    }
    pub fn suggested_verify(&self) -> Option<&str> {
        self.suggested_verify.as_deref()
    }
    pub fn workspace(&self) -> &str {
        &self.workspace
    }
    pub fn default_target(&self) -> Option<&DefaultTarget> {
        self.default_target.as_ref()
    }
    pub fn members(&self) -> &[MemberView] {
        &self.members
    }

    pub fn chat(&self) -> &[ChatItem] {
        &self.chat
    }

    pub fn logs(&self) -> &[LogEntry] {
        &self.logs
    }
    pub fn runs(&self) -> &[RunSummary] {
        &self.runs
    }
    pub fn latest_run(&self) -> Option<&RunSummary> {
        self.runs.last()
    }
    pub fn latest_run_action_command(&self) -> Option<String> {
        self.latest_run()
            .and_then(|run| run_action_command(run, &self.workspace, false))
    }
    pub fn selected_run(&self) -> Option<&RunSummary> {
        self.selected_run
            .and_then(|id| self.runs.iter().find(|run| run.id == id))
            .or_else(|| self.latest_run())
    }
    pub fn selected_run_step(&self) -> Option<u32> {
        let run = self.selected_run()?;
        self.selected_run_step
            .filter(|step| run.steps.iter().any(|candidate| candidate.number == *step))
    }
    pub fn selected_run_action_command(&self) -> Option<String> {
        self.selected_run()
            .and_then(|run| run_action_command(run, &self.workspace, true))
    }
    pub fn selected_run_stage_command(&self) -> Option<String> {
        let run = self.selected_run()?;
        self.selected_run_step()
            .and_then(|step| run_step_action_command(run, step))
            .or_else(|| run_action_command(run, &self.workspace, true))
    }
    pub fn selected_run_dispatch_command(&self) -> Option<String> {
        let run = self.selected_run()?;
        let step = self.selected_run_step()?;
        run_step_dispatch_command(run, step)
    }
    pub fn runs_detail(&self) -> bool {
        self.runs_detail
    }
    pub fn pending_approvals(&self) -> &[PendingApproval] {
        &self.pending_approvals
    }
    pub fn paused_routes(&self) -> usize {
        self.paused_routes
    }
    pub fn drawer(&self) -> Option<Drawer> {
        self.drawer.clone()
    }
    pub fn header_selected(&self) -> Option<usize> {
        self.header_selected
    }
    pub fn scroll(&self) -> usize {
        self.scroll
    }

    pub fn chat_selection(&self) -> Option<ChatSelection> {
        self.chat_selection
    }

    pub fn begin_chat_selection(&mut self, pos: (usize, usize)) {
        self.disarm_quit();
        self.composer.clear_selection();
        self.composer_dragging = false;
        self.chat_selection = Some(ChatSelection {
            start: pos,
            end: pos,
        });
    }

    pub fn update_chat_selection(&mut self, pos: (usize, usize)) {
        if let Some(selection) = self.chat_selection.as_mut() {
            selection.end = pos;
        }
    }

    pub fn clear_chat_selection(&mut self) {
        self.chat_selection = None;
        self.composer_dragging = false;
    }

    pub fn begin_composer_selection(&mut self, index: usize) {
        self.disarm_quit();
        self.chat_selection = None;
        self.composer_dragging = true;
        let index = self.image_placeholder_boundary(index, None);
        self.composer.begin_selection_at(index);
    }

    pub fn update_composer_selection(&mut self, index: usize) {
        if self.composer_dragging {
            let index = self.image_placeholder_boundary(index, None);
            self.composer.extend_selection_to(index);
        }
    }

    pub fn finish_composer_selection(&mut self) -> String {
        self.composer_dragging = false;
        self.composer.selected_text()
    }

    pub fn queued_prompt_count(&self) -> usize {
        self.queued_prompts.values().map(Vec::len).sum()
    }

    pub fn queued_prompts_for(&self, member: &MemberId) -> &[String] {
        self.queued_prompts
            .get(member)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    pub fn clamp_scroll(&mut self, max: usize) {
        if self.scroll > max {
            self.scroll = max;
        }
    }

    pub fn set_chat_page_rows(&mut self, rows: usize) {
        self.chat_page_rows = rows.max(1);
    }
    /// Record the composer's drawn wrap width so cursor movement follows the
    /// same wrapped rows as the screen.
    pub fn set_composer_wrap(&mut self, width: usize) {
        self.composer_wrap = width.max(1);
    }
    pub fn composer(&self) -> &Composer {
        &self.composer
    }
    pub fn should_quit(&self) -> bool {
        self.should_quit
    }

    pub fn runtime_available(&self) -> bool {
        self.runtime_available
    }

    pub fn mark_runtime_unavailable(&mut self) {
        if !self.runtime_available {
            return;
        }
        let active_members = self
            .message_index
            .values()
            .map(|cell| cell.member.clone())
            .chain(self.tool_index.keys().map(|(member, _)| member.clone()))
            .collect::<HashSet<_>>();
        for member in active_members {
            self.finish_incomplete_cells_for_member(&member);
        }
        self.runtime_available = false;
        self.attach_pending = None;
        self.attach_request = None;
        self.paused_routes = 0;
        self.pending_approvals.clear();
        self.active_reasoning.clear();
        self.reasoning_buffers.clear();
        self.open_thinking.clear();
        self.thinking_started.clear();
        self.running_since.clear();
        for member in &mut self.members {
            if member_status_is_active(member.status) {
                member.status = MemberStatus::Failed;
            }
        }
        self.push(ChatItem::Error {
            member: None,
            message: "runtime stopped — input is disabled; press Ctrl+C to quit".to_string(),
        });
    }

    pub fn running_count(&self) -> usize {
        self.members
            .iter()
            .filter(|m| member_status_is_active(m.status))
            .count()
    }

    /// True when the next frame must paint even without input (spinner / elapsed).
    pub fn needs_animated_frame(&self) -> bool {
        self.running_count() > 0 || self.verification_active()
    }

    fn touch_chat(&mut self) {
        self.chat_fingerprint = self.chat_fingerprint.wrapping_add(1);
    }

    pub(crate) fn roster_revision(&self) -> u64 {
        self.roster_revision
    }

    pub fn verification_active(&self) -> bool {
        self.runs
            .iter()
            .any(|run| run.status == crate::domain::event::RunStatus::Verifying)
    }

    pub fn has_cancelable_work(&self) -> bool {
        self.runtime_available
            && (self.running_count() > 0
                || self.verification_active()
                || self.paused_routes > 0
                || !self.pending_approvals.is_empty()
                || self.attach_pending.is_some()
                || self.attach_request.is_some())
    }

    pub fn first_pending_approval(&self) -> Option<ApprovalId> {
        self.pending_approvals.first().map(|a| a.id)
    }

    pub fn selected_pending_approval(&self) -> Option<&PendingApproval> {
        self.pending_approvals.get(self.selected_approval)
    }

    pub fn selected_approval_index(&self) -> usize {
        self.selected_approval
    }

    pub fn select_next_pending_approval(&mut self) {
        if self.pending_approvals.len() < 2 {
            return;
        }
        self.selected_approval = (self.selected_approval + 1) % self.pending_approvals.len();
    }

    pub fn select_prev_pending_approval(&mut self) {
        if self.pending_approvals.len() < 2 {
            return;
        }
        self.selected_approval = if self.selected_approval == 0 {
            self.pending_approvals.len() - 1
        } else {
            self.selected_approval - 1
        };
    }

    fn clamp_selected_approval(&mut self) {
        if self.pending_approvals.is_empty() {
            self.selected_approval = 0;
        } else {
            self.selected_approval = self.selected_approval.min(self.pending_approvals.len() - 1);
        }
    }

    /// Request attaching to the member at `idx`'s live backend session. The
    /// synchronous attach pauses event consumption, so it is disabled while
    /// any runtime work could still produce events.
    pub fn request_attach(&mut self, idx: usize) -> Option<MemberId> {
        let Some(member) = self.members.get(idx).map(|member| member.id.clone()) else {
            self.header_selected = None;
            return None;
        };
        self.request_attach_member(member)
    }

    /// Request a named member's existing native interactive session.
    pub(crate) fn request_attach_member_by_name(
        &mut self,
        requested: &MemberId,
    ) -> Option<MemberId> {
        let member = self.resolve_member_id(requested)?;
        self.request_attach_member(member)
    }

    fn request_attach_member(&mut self, member: MemberId) -> Option<MemberId> {
        self.disarm_quit();
        if !self.runtime_available {
            self.push(ChatItem::Notice {
                member: None,
                text: "Cannot attach because the runtime has stopped.".to_string(),
            });
            self.header_selected = None;
            return None;
        }
        if self.attach_pending.is_some() || self.attach_request.is_some() {
            self.push(ChatItem::Notice {
                member: None,
                text: "An attach request is already pending.".to_string(),
            });
            self.header_selected = None;
            return None;
        }
        if self.has_cancelable_work() {
            self.push(ChatItem::Notice { member: None,
                text: "Cannot attach while member work, verification, routing, or approval is active — press Esc to cancel it or resolve it first."
                    .to_string(),
            });
            self.header_selected = None;
            return None;
        }
        self.attach_pending = Some(member.clone());
        self.header_selected = None;
        Some(member)
    }

    pub fn attach_request_send_failed(&mut self) {
        self.attach_pending = None;
    }

    /// Cancel an attach that has not yet handed the terminal to the child CLI.
    /// The runtime reservation is released by the caller. If a grant races
    /// with this cancellation, the unexpected-grant path schedules a second,
    /// idempotent release when the event arrives.
    pub fn cancel_pending_attach(&mut self) -> Option<MemberId> {
        let member = self
            .attach_pending
            .take()
            .or_else(|| self.attach_request.take().map(|request| request.member));
        if let Some(member) = &member {
            self.push(ChatItem::Notice {
                member: Some(member.clone()),
                text: format!("cancelled attach request for {member}"),
            });
        }
        member
    }

    pub fn take_attach_release_pending(&mut self) -> Option<MemberId> {
        self.attach_release_pending.take()
    }

    pub fn take_attach_request(&mut self) -> Option<AttachRequest> {
        self.attach_request.take()
    }

    // --- composer editing (each edit resets the completion popup) --------

    fn member_ids(&self) -> Vec<String> {
        self.members.iter().map(|m| m.id.to_string()).collect()
    }

    /// The active completion popup for the current composer text, if any.
    pub fn completion(&self) -> Option<Completion> {
        if self.popup_dismissed {
            return None;
        }
        let member_backends = self
            .members
            .iter()
            .flat_map(|member| {
                [
                    (member.id.to_string(), member.backend),
                    (member.display_name.clone(), member.backend),
                ]
            })
            .collect::<HashMap<_, _>>();
        let skills = self
            .skills
            .iter()
            .map(|skill| AgentSkill {
                name: skill.name.clone(),
                description: skill.description.clone(),
                backend: skill.backend,
                invocation: skill.invocation.clone(),
            })
            .collect::<Vec<_>>();
        completion::compute_with_agent_skills(
            &self.composer.head(),
            &self.member_ids(),
            &skills,
            &member_backends,
        )
    }

    pub fn popup_selected(&self) -> usize {
        self.popup_selected
    }

    fn reset_popup(&mut self) {
        self.popup_selected = 0;
        self.popup_dismissed = false;
    }

    pub(crate) fn disarm_quit(&mut self) {
        self.quit_armed = false;
    }

    pub fn insert_char(&mut self, ch: char) {
        self.disarm_quit();
        self.header_selected = None;
        self.composer.insert(ch);
        self.reconcile_pending_images();
        self.history_cursor = None;
        self.reset_popup();
    }
    pub fn pending_images(&self) -> &[PromptImage] {
        &self.pending_images
    }

    fn image_placeholder_boundary(&self, index: usize, prefer_end: Option<bool>) -> usize {
        let text = self.composer.text();
        for number in 1..=self.pending_images.len() {
            let placeholder = image_placeholder(number);
            let Some((start, end)) = literal_char_range(&text, &placeholder) else {
                continue;
            };
            if start < index && index < end {
                return match prefer_end {
                    Some(true) => end,
                    Some(false) => start,
                    None if index - start < end - index => start,
                    None => end,
                };
            }
        }
        index
    }

    fn snap_cursor_around_image(&mut self, prefer_end: Option<bool>) {
        let index = self.image_placeholder_boundary(self.composer.cursor(), prefer_end);
        self.composer.set_cursor_index(index);
    }

    fn reconcile_pending_images(&mut self) {
        if self.pending_images.is_empty() {
            return;
        }
        let text = self.composer.text();
        let mut retained = Vec::new();
        for (index, image) in std::mem::take(&mut self.pending_images)
            .into_iter()
            .enumerate()
        {
            let placeholder = image_placeholder(index + 1);
            if text.contains(&placeholder) {
                retained.push((placeholder, image));
            } else {
                super::clipboard_image::remove_managed_paste(&image.path);
            }
        }
        for (index, (old_placeholder, _)) in retained.iter().enumerate() {
            let new_placeholder = image_placeholder(index + 1);
            if old_placeholder == &new_placeholder {
                continue;
            }
            let text = self.composer.text();
            if let Some((start, end)) = literal_char_range(&text, old_placeholder) {
                let _ = self.composer.replace_range(start, end, &new_placeholder);
            }
        }
        self.pending_images = retained.into_iter().map(|(_, image)| image).collect();
    }

    pub fn has_composer_draft(&self) -> bool {
        !self.composer.is_empty() || !self.pending_images.is_empty()
    }

    pub fn attach_pending_image(&mut self, mut image: PromptImage) -> Result<(), String> {
        if self.pending_images.len() >= MAX_PROMPT_IMAGES {
            return Err(format!("at most {MAX_PROMPT_IMAGES} images per message"));
        }
        if let Ok(canonical) = std::fs::canonicalize(&image.path) {
            image.path = canonical;
        }
        let bytes = std::fs::read(&image.path)
            .map_err(|_| format!("image is not readable: {}", image.path.display()))?;
        if bytes.len() > MAX_IMAGE_BYTES {
            return Err(format!("image is too large ({} bytes)", bytes.len()));
        }
        if mime_from_bytes(&bytes).is_none() {
            return Err("attached file is not a PNG/JPEG/GIF/WebP image".to_string());
        }
        if self
            .pending_images
            .iter()
            .any(|existing| existing.path == image.path)
        {
            return Ok(());
        }
        let next_placeholder = image_placeholder(self.pending_images.len() + 1);
        if !self.composer.can_insert_text_exact(&next_placeholder) {
            return Err(COMPOSER_INPUT_TRUNCATED.to_string());
        }
        self.disarm_quit();
        self.header_selected = None;
        if self.composer.delete_selection() {
            self.reconcile_pending_images();
        }
        let next_placeholder = image_placeholder(self.pending_images.len() + 1);
        if !self.composer.insert_text_exact(&next_placeholder) {
            return Err(COMPOSER_INPUT_TRUNCATED.to_string());
        }
        self.pending_images.push(image);
        self.history_cursor = None;
        self.reset_popup();
        Ok(())
    }

    /// Bracketed paste: a single image path is copied into the workspace
    /// paste dir (or replaced by clipboard bytes). Anything else is text.
    pub fn paste_text_or_image(&mut self, text: &str) {
        if let Some(result) = super::clipboard_image::import_pasted_text(&self.workspace, text) {
            match result.and_then(|image| self.attach_pending_image(image)) {
                Ok(()) => {}
                Err(err) => {
                    self.push(ChatItem::Notice {
                        member: None,
                        text: err,
                    });
                }
            }
            return;
        }
        self.insert_text(text);
    }

    pub fn insert_text(&mut self, text: &str) {
        self.disarm_quit();
        self.header_selected = None;
        // Bound before newline normalization so a giant paste does not create
        // another full-size temporary allocation on the UI thread.
        let mut end = text.len().min(MAX_COMPOSER_BYTES);
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        let pre_truncated = end < text.len();
        let text = text[..end].replace("\r\n", "\n").replace('\r', "\n");
        let fully_inserted = self.composer.insert_text(&text) && !pre_truncated;
        self.reconcile_pending_images();
        if !fully_inserted
            && !matches!(
                self.chat.last(),
                Some(ChatItem::Notice { text, .. }) if text == COMPOSER_INPUT_TRUNCATED
            )
        {
            self.push(ChatItem::Notice {
                member: None,
                text: COMPOSER_INPUT_TRUNCATED.to_string(),
            });
        }
        self.history_cursor = None;
        self.reset_popup();
    }
    pub fn insert_newline(&mut self) {
        self.disarm_quit();
        self.header_selected = None;
        self.composer.insert_newline();
        self.reconcile_pending_images();
        self.history_cursor = None;
        self.reset_popup();
    }
    /// Move the cursor up within a multi-line composer; returns false if it is
    /// already on the first line (so the caller recalls history instead).
    pub fn composer_up(&mut self) -> bool {
        self.disarm_quit();
        let moved = self.composer.up(self.composer_wrap);
        self.snap_cursor_around_image(None);
        moved
    }
    /// Move the cursor down within a multi-line composer; returns false if it is
    /// already on the last line.
    pub fn composer_down(&mut self) -> bool {
        self.disarm_quit();
        let moved = self.composer.down(self.composer_wrap);
        self.snap_cursor_around_image(None);
        moved
    }
    pub fn backspace(&mut self) {
        self.disarm_quit();
        self.header_selected = None;
        let cursor = self.composer.cursor();
        let text = self.composer.text();
        let image_range = (self.composer.selection_range().is_none())
            .then(|| {
                (1..=self.pending_images.len()).find_map(|number| {
                    let placeholder = image_placeholder(number);
                    literal_char_range(&text, &placeholder).filter(|(_, end)| *end == cursor)
                })
            })
            .flatten();
        if let Some((start, end)) = image_range {
            self.composer.delete_range(start, end);
        } else {
            self.composer.backspace();
        }
        self.reconcile_pending_images();
        self.history_cursor = None;
        self.reset_popup();
    }
    pub fn delete_word(&mut self) {
        self.disarm_quit();
        self.header_selected = None;
        self.composer.delete_word();
        self.reconcile_pending_images();
        self.history_cursor = None;
        self.reset_popup();
    }
    pub fn clear_line(&mut self) {
        self.disarm_quit();
        self.header_selected = None;
        self.composer.delete_line();
        self.reconcile_pending_images();
        self.history_cursor = None;
        self.reset_popup();
    }
    pub fn clear_composer(&mut self) {
        self.disarm_quit();
        self.header_selected = None;
        self.composer.clear();
        self.discard_pending_images();
        self.history_cursor = None;
        self.reset_popup();
    }
    pub fn cursor_left(&mut self) {
        self.disarm_quit();
        self.composer.left();
        self.snap_cursor_around_image(Some(false));
        self.reset_popup();
    }
    pub fn cursor_right(&mut self) {
        self.disarm_quit();
        self.composer.right();
        self.snap_cursor_around_image(Some(true));
        self.reset_popup();
    }
    pub fn cursor_home(&mut self) {
        self.disarm_quit();
        self.composer.home();
        self.reset_popup();
    }
    pub fn cursor_end(&mut self) {
        self.disarm_quit();
        self.composer.end();
        self.reset_popup();
    }
    pub fn take_composer(&mut self) -> String {
        self.disarm_quit();
        let text = self.composer.take();
        // Keep copies on disk: adapters may still be reading them.
        self.pending_images.clear();
        self.history_cursor = None;
        self.reset_popup();
        text
    }

    fn discard_pending_images(&mut self) {
        for image in self.pending_images.drain(..) {
            super::clipboard_image::remove_managed_paste(&image.path);
        }
    }

    pub fn popup_up(&mut self) {
        self.disarm_quit();
        self.popup_selected = self.popup_selected.saturating_sub(1);
    }
    pub fn popup_down(&mut self) {
        self.disarm_quit();
        if let Some(completion) = self.completion()
            && self.popup_selected + 1 < completion.items.len()
        {
            self.popup_selected += 1;
        }
    }
    pub fn dismiss_popup(&mut self) {
        self.disarm_quit();
        self.popup_dismissed = true;
    }

    /// Accept the highlighted completion. Returns true if the composer changed
    /// (false means the token already matched, so the caller should submit).
    pub fn accept_completion(&mut self) -> bool {
        let Some(completion) = self.completion() else {
            return false;
        };
        self.disarm_quit();
        // A command already typed in full (and needing no argument) has
        // nothing left to complete: let Enter submit it.
        let typed: String = self
            .composer
            .head()
            .chars()
            .skip(completion.token_start)
            .collect();
        if completion::command_is_complete(&typed) {
            return false;
        }
        let index = self.popup_selected.min(completion.items.len() - 1);
        let insert = completion.items[index].insert.clone();
        // An argument that already spells the highlighted candidate exactly
        // (`/mode review`, `/logs main`) is a complete line: Enter submits it.
        // Enter accepts a completion only for a strict prefix of the candidate.
        if completion.token_start > 0 && typed.eq_ignore_ascii_case(insert.trim_end()) {
            return false;
        }
        let before = self.composer.text();
        self.composer.replace_token(completion.token_start, &insert);
        self.reconcile_pending_images();
        self.reset_popup();
        self.composer.text() != before
    }

    // --- prompt history (shell-style ↑/↓ recall) ------------------------

    /// Record a submitted line into prompt history (skipping blanks and
    /// consecutive duplicates) and end any active browse.
    pub fn record_submission(&mut self, text: &str) {
        self.disarm_quit();
        let text = text.trim();
        if !text.is_empty() && self.prompt_history.last().map(String::as_str) != Some(text) {
            self.prompt_history
                .push(bounded_text(text, MAX_CHAT_ITEM_BYTES));
            trim_prompt_history(&mut self.prompt_history);
        }
        self.history_cursor = None;
        self.history_draft.clear();
    }

    /// Recall an older entry (↑). The first step saves the live draft and jumps
    /// to the newest entry; further steps walk backwards.
    pub fn history_prev(&mut self) {
        self.disarm_quit();
        if !self.pending_images.is_empty() {
            self.composer.home();
            return;
        }
        if self.prompt_history.is_empty() {
            return;
        }
        let target = match self.history_cursor {
            None => {
                self.history_draft = self.composer.text();
                self.prompt_history.len() - 1
            }
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.history_cursor = Some(target);
        let text = self.prompt_history[target].clone();
        self.composer.set_text(&text);
        // Suppress the completion popup while browsing recalled commands.
        self.popup_dismissed = true;
        self.popup_selected = 0;
        self.header_selected = None;
    }

    /// Recall a newer entry (↓); stepping past the newest restores the draft.
    pub fn history_next(&mut self) {
        self.disarm_quit();
        let Some(i) = self.history_cursor else {
            return;
        };
        if i + 1 < self.prompt_history.len() {
            self.history_cursor = Some(i + 1);
            let text = self.prompt_history[i + 1].clone();
            self.composer.set_text(&text);
            self.popup_dismissed = true;
        } else {
            self.history_cursor = None;
            let draft = std::mem::take(&mut self.history_draft);
            self.composer.set_text(&draft);
        }
        self.popup_selected = 0;
        self.header_selected = None;
    }

    /// Whether the composer is currently showing a recalled history entry.
    pub fn browsing_history(&self) -> bool {
        self.history_cursor.is_some()
    }

    // --- reverse history search (Ctrl+R) --------------------------------

    pub fn in_history_search(&self) -> bool {
        self.history_search.is_some()
    }

    /// The active search as `(query, matched entry)` for rendering.
    pub fn history_search(&self) -> Option<(&str, Option<&str>)> {
        self.history_search.as_ref().map(|s| {
            (
                s.query.as_str(),
                s.match_idx.map(|i| self.prompt_history[i].as_str()),
            )
        })
    }

    pub fn start_history_search(&mut self) {
        self.disarm_quit();
        let match_idx = self.search_from("", None);
        self.history_search = Some(HistorySearch {
            query: String::new(),
            match_idx,
        });
        self.header_selected = None;
        self.popup_dismissed = true;
    }

    pub fn history_search_input(&mut self, ch: char) {
        self.disarm_quit();
        if let Some(mut search) = self.history_search.take() {
            search.query.push(ch);
            search.match_idx = self.search_from(&search.query, None);
            self.history_search = Some(search);
        }
    }

    pub fn history_search_backspace(&mut self) {
        self.disarm_quit();
        if let Some(mut search) = self.history_search.take() {
            search.query.pop();
            search.match_idx = self.search_from(&search.query, None);
            self.history_search = Some(search);
        }
    }

    /// Ctrl+R again: step to the next older match.
    pub fn history_search_again(&mut self) {
        self.disarm_quit();
        if let Some(mut search) = self.history_search.take() {
            let before = search.match_idx;
            if let Some(idx) = self.search_from(&search.query, before) {
                search.match_idx = Some(idx);
            }
            self.history_search = Some(search);
        }
    }

    /// Accept the current match into the composer and leave search.
    pub fn accept_history_search(&mut self) {
        self.disarm_quit();
        if let Some(search) = self.history_search.take()
            && let Some(idx) = search.match_idx
        {
            let text = self.prompt_history[idx].clone();
            self.composer.set_text(&text);
        }
        self.history_cursor = None;
    }

    pub fn cancel_history_search(&mut self) {
        self.disarm_quit();
        self.history_search = None;
    }

    // --- transcript search (`/find`) ------------------------------------

    /// Whether `/find` is active (including zero matches).
    pub fn find_active(&self) -> bool {
        self.find.is_some()
    }

    /// Active find as `(query, current 1-based, total)`. Out-of-range match
    /// indices (chat grew/shrank since `set_find`) are clamped in the counter.
    pub fn find(&self) -> Option<(&str, usize, usize)> {
        let find = self.find.as_ref()?;
        let valid: Vec<usize> = find
            .matches
            .iter()
            .copied()
            .filter(|&idx| idx < self.chat.len())
            .collect();
        let total = valid.len();
        let current = if total == 0 {
            0
        } else {
            // Prefer the stored position among still-valid matches.
            let preferred = find
                .matches
                .get(find.current)
                .copied()
                .filter(|&idx| idx < self.chat.len());
            match preferred {
                Some(idx) => valid.iter().position(|&i| i == idx).unwrap_or(0) + 1,
                None => (find.current.min(total - 1)) + 1,
            }
        };
        Some((find.query.as_str(), current, total))
    }

    /// Index into `chat` for the current find match, if any and still in range.
    pub fn find_current_chat_index(&self) -> Option<usize> {
        let find = self.find.as_ref()?;
        let idx = *find.matches.get(find.current)?;
        (idx < self.chat.len()).then_some(idx)
    }

    /// Set or clear transcript search. Empty/whitespace query clears to `None`.
    /// Otherwise case-insensitive substring match; `current` starts at the last
    /// (newest) match and the view jumps there.
    pub fn set_find(&mut self, query: &str) {
        self.disarm_quit();
        let query = query.trim();
        if query.is_empty() {
            self.find = None;
            return;
        }
        let matches = self.find_matches(query);
        let current = matches.len().saturating_sub(1);
        self.find = Some(FindState {
            query: query.to_string(),
            matches,
            current,
        });
        if let Some(idx) = self.find_current_chat_index() {
            self.scroll_to_chat_item(idx);
        }
    }

    /// Chat indices whose text contains `query`, limited to items the current
    /// focus renders so n/p never jump to a hidden row.
    fn find_matches(&self, query: &str) -> Vec<usize> {
        let needle = query.to_lowercase();
        self.chat
            .iter()
            .enumerate()
            .filter(|(_, item)| focus_row(self, item) != FocusRow::Hidden)
            .filter(|(_, item)| {
                let text = match item {
                    ChatItem::User { body, targets, .. } => self.displayed_user_body(body, targets),
                    _ => chat_item_search_text(item),
                };
                text.to_lowercase().contains(&needle)
            })
            .map(|(i, _)| i)
            .collect()
    }

    pub fn find_next(&mut self) {
        self.disarm_quit();
        let idx = {
            let Some(find) = self.find.as_mut() else {
                return;
            };
            if find.matches.is_empty() {
                return;
            }
            find.current = (find.current + 1) % find.matches.len();
            find.matches[find.current]
        };
        if idx < self.chat.len() {
            self.scroll_to_chat_item(idx);
        }
    }

    pub fn find_prev(&mut self) {
        self.disarm_quit();
        let idx = {
            let Some(find) = self.find.as_mut() else {
                return;
            };
            if find.matches.is_empty() {
                return;
            }
            find.current = if find.current == 0 {
                find.matches.len() - 1
            } else {
                find.current - 1
            };
            find.matches[find.current]
        };
        if idx < self.chat.len() {
            self.scroll_to_chat_item(idx);
        }
    }

    pub fn clear_find(&mut self) {
        self.disarm_quit();
        self.find = None;
    }

    /// Approximate scroll so chat item `idx` sits near the bottom of a
    /// bottom-anchored view. Uses the same line estimate as scroll pinning when
    /// new items arrive (approximate: ignores wrap width and markdown layout).
    pub fn scroll_to_chat_item(&mut self, idx: usize) {
        if idx >= self.chat.len() {
            return;
        }
        self.reveal_tool_group(idx);
        let mut below = 0usize;
        // A folded group is one title row; its other calls add nothing.
        let mut run: Option<(usize, bool)> = None;
        for j in idx.saturating_add(1)..self.chat.len() {
            let item = &self.chat[j];
            if focus_row(self, item) == FocusRow::Hidden {
                continue;
            }
            if matches!(item, ChatItem::Tool { .. }) {
                let continues = j > 0
                    && matches!(self.chat[j - 1], ChatItem::Tool { .. })
                    && work_item_member(&self.chat[j - 1]) == work_item_member(item);
                let start = match run {
                    Some((known, _)) if continues => known,
                    _ => self.work_run_start(j).unwrap_or(j),
                };
                let folded = match run {
                    Some((known, folded)) if known == start => folded,
                    _ => {
                        let folded = self.work_run_tool_rows(start)
                            > tool_display::TOOL_GROUP_COLLAPSE_ABOVE
                            && !self.tool_group_expanded(start);
                        run = Some((start, folded));
                        folded
                    }
                };
                if folded {
                    if j == start {
                        below += 2;
                    }
                    continue;
                }
            }
            below += estimate_item_lines(item);
        }
        self.scroll = below;
    }

    /// Newest history entry containing `query` (case-insensitive) strictly older
    /// than `before` (or the newest overall when `before` is `None`). An empty
    /// query matches the newest available entry.
    fn search_from(&self, query: &str, before: Option<usize>) -> Option<usize> {
        if self.prompt_history.is_empty() {
            return None;
        }
        let needle = query.to_lowercase();
        let start = match before {
            Some(0) => return None,
            Some(i) => i - 1,
            None => self.prompt_history.len() - 1,
        };
        (0..=start)
            .rev()
            .find(|&i| self.prompt_history[i].to_lowercase().contains(&needle))
    }

    // --- focus ----------------------------------------------------------

    /// The member whose conversation fills the main window, or `None` for the
    /// interleaved "All" view.
    pub fn focus(&self) -> Option<&MemberId> {
        self.focus.as_ref()
    }

    /// A prompt as the chat shows it. Rows saved by older versions begin with
    /// the `@member` / `@all` routing prefix the TUI used to add; drop it when
    /// it names one of the row's own targets. Stored data is left alone.
    pub(crate) fn displayed_user_body(&self, body: &str, targets: &[MemberId]) -> String {
        let shown = crate::adapter::prompt_images::display_prompt_images(body);
        let Some(rest) = shown.trim_start().strip_prefix('@') else {
            return shown;
        };
        let Some(split) = rest.find(char::is_whitespace) else {
            return shown;
        };
        let token = &rest[..split];
        let names_target = token.eq_ignore_ascii_case("all")
            || targets.iter().any(|target| {
                token.eq_ignore_ascii_case(target.as_str())
                    || token.eq_ignore_ascii_case(&self.member_display(target))
            });
        if names_target {
            rest[split..].trim().to_string()
        } else {
            shown
        }
    }

    /// Sub-agents in the roster.
    pub fn sub_agent_count(&self) -> usize {
        self.members
            .iter()
            .filter(|member| member.parent.is_some())
            .count()
    }

    /// Name shown for the current focus: the member's display name, or "All".
    pub fn focus_display(&self) -> String {
        match &self.focus {
            Some(member) => self.member_display(member),
            None => "All".to_string(),
        }
    }

    /// Switch the main window to one member (`Some`) or back to "All"
    /// (`None`). Unknown members are ignored. The view returns to the bottom
    /// and any chat selection is dropped, since its lines no longer exist.
    pub fn set_focus(&mut self, focus: Option<MemberId>) {
        let focus = match focus {
            Some(requested) => match self.resolve_member_id(&requested) {
                Some(member) => Some(member),
                None => return,
            },
            None => None,
        };
        self.disarm_quit();
        self.assign_focus(focus);
    }

    fn assign_focus(&mut self, focus: Option<MemberId>) {
        self.focus_initialized = true;
        if self.focus != focus {
            self.focus = focus;
            self.scroll = 0;
            self.chat_selection = None;
            // The visible rows changed, so redo any active search.
            if let Some(query) = self.find.as_ref().map(|find| find.query.clone()) {
                let matches = self.find_matches(&query);
                let current = matches.len().saturating_sub(1);
                self.find = Some(FindState {
                    query,
                    matches,
                    current,
                });
            }
        }
    }

    /// Keep the focus valid after a roster refresh. The first roster picks the
    /// default target's member (else the first member). Afterwards the focus is
    /// left alone, including a deliberate "All", unless the focused member left
    /// the roster, which starts over from the default. An empty roster has no
    /// focus.
    fn sync_focus_with_roster(&mut self) {
        if self.members.is_empty() {
            self.focus = None;
            self.focus_initialized = false;
            self.agents_choice = AgentChoice::All;
            self.agents_fallback = 0;
            return;
        }
        if let Some(focus) = &self.focus
            && !self.members.iter().any(|member| &member.id == focus)
        {
            self.focus_initialized = false;
        }
        if !self.focus_initialized {
            let default = match &self.default_target {
                Some(DefaultTarget::Member(id)) => self.resolve_member_id(id),
                _ => None,
            }
            .or_else(|| self.members.first().map(|member| member.id.clone()));
            self.assign_focus(default);
        }
    }

    /// Target for plain text while a member is focused; `None` in the "All"
    /// view, where plain text goes to the team's default member(s).
    pub fn focused_message_target(&self) -> Option<MessageTarget> {
        let member = self.resolve_member_id(self.focus.as_ref()?)?;
        Some(MessageTarget::Member(member))
    }

    /// Agents list rows in display order: "All", then the members by status
    /// section (each top-level member followed by its sub-agents). A search
    /// query replaces that with a flat list ranked by relevance.
    pub fn agents_rows(&self) -> Vec<AgentRow> {
        crate::tui::agents_view::rows(self)
    }

    /// Selectable Agents list choices in display order.
    pub fn agents_choices(&self) -> Vec<AgentChoice> {
        self.agents_rows()
            .into_iter()
            .map(|row| match row {
                AgentRow::All => AgentChoice::All,
                AgentRow::Member { index, .. } => {
                    AgentChoice::Member(self.members[index].id.clone())
                }
                AgentRow::Saved { index } => AgentChoice::Saved(self.saved_chats[index].id),
            })
            .collect()
    }

    /// Saved chats other than the current one, newest first.
    pub fn saved_chats(&self) -> &[ConversationSummary] {
        &self.saved_chats
    }

    /// Open the Agents list for `/resume` with the highlight on the first
    /// saved chat. The list may still be on its way from the runtime; the
    /// highlight moves when it arrives, unless the user moved it first.
    pub fn open_agents_on_saved_chats(&mut self) {
        if self.drawer != Some(Drawer::Agents) {
            self.toggle_drawer(Drawer::Agents);
        }
        self.select_first_inactive();
        self.agents_select_inactive = true;
    }

    fn select_first_inactive(&mut self) {
        let first = self
            .agents_choices()
            .iter()
            .position(|choice| matches!(choice, AgentChoice::Saved(_)));
        if let Some(position) = first {
            self.select_agent_at(position);
        }
        self.agents_select_inactive = false;
    }

    /// The command a selection raised that only the key loop can send.
    pub fn take_pending_command(&mut self) -> Option<UiCommand> {
        self.pending_command.take()
    }

    /// Queue `command` for the key loop and close the Agents list.
    pub fn queue_command_and_close(&mut self, command: UiCommand) {
        self.pending_command = Some(command);
        self.close_drawer();
    }

    /// Now, in Unix seconds, for the Agents list's relative ages.
    pub(crate) fn clock_unix_secs(&self) -> i64 {
        self.clock_override.unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_secs() as i64)
        })
    }

    #[cfg(test)]
    pub(crate) fn set_clock_unix_secs(&mut self, now: i64) {
        self.clock_override = Some(now);
    }

    /// The Model cell of a saved chat: its main member's backend, then the
    /// model and effort the way a live member's cell reads.
    pub fn saved_chat_model(&self, chat: &ConversationSummary) -> String {
        let Some(backend) = chat.main_backend else {
            return String::new();
        };
        let mut model = backend.as_str().to_string();
        if let Some(summary) = self.display_model_catalog().effective_summary(
            backend,
            chat.main_model.as_deref(),
            chat.main_effort,
            Path::new(&self.workspace),
        ) {
            model.push_str(" · ");
            model.push_str(&summary);
        }
        model
    }

    /// Current position of the highlighted agent among `agents_choices()`.
    /// If the highlighted member left the list, the nearest valid position.
    pub fn agents_selected(&self) -> usize {
        let choices = self.agents_choices();
        choices
            .iter()
            .position(|choice| choice == &self.agents_choice)
            .unwrap_or_else(|| self.agents_fallback.min(choices.len().saturating_sub(1)))
    }

    pub fn selected_agent(&self) -> Option<AgentChoice> {
        self.agents_choices()
            .into_iter()
            .nth(self.agents_selected())
    }

    fn select_agent_at(&mut self, position: usize) {
        // Any move of the highlight settles where it is.
        self.agents_select_inactive = false;
        self.agents_delete_armed = None;
        if let Some(choice) = self.agents_choices().into_iter().nth(position) {
            self.agents_choice = choice;
            self.agents_fallback = position;
        }
    }

    pub fn select_previous_agent(&mut self) {
        self.select_agent_at(self.agents_selected().saturating_sub(1));
    }

    pub fn select_next_agent(&mut self) {
        let next = self.agents_selected() + 1;
        if next < self.agents_choices().len() {
            self.select_agent_at(next);
        }
    }

    pub fn select_first_agent(&mut self) {
        self.select_agent_at(0);
    }

    pub fn select_last_agent(&mut self) {
        self.select_agent_at(self.agents_choices().len().saturating_sub(1));
    }

    /// Move the highlight by a page (`down` picks the direction), at least
    /// one row, clamped to the list.
    pub fn select_agent_page(&mut self, down: bool) {
        let step = self.drawer_viewport.get().saturating_sub(1).max(1);
        let last = self.agents_choices().len().saturating_sub(1);
        let current = self.agents_selected();
        let target = if down {
            current.saturating_add(step).min(last)
        } else {
            current.saturating_sub(step)
        };
        self.select_agent_at(target);
    }

    /// Open the highlighted Agents row and close the list: focus an agent, or
    /// queue the resume of a saved chat. With nothing highlighted (the search
    /// matches nothing) the list stays open.
    pub fn apply_selected_agent(&mut self) -> bool {
        if self.drawer != Some(Drawer::Agents) {
            return false;
        }
        match self.selected_agent() {
            Some(AgentChoice::All) => self.set_focus(None),
            Some(AgentChoice::Member(member)) => self.set_focus(Some(member)),
            Some(AgentChoice::Saved(conversation)) => {
                self.queue_command_and_close(UiCommand::ResumeConversation { conversation });
                return true;
            }
            None => return true,
        }
        self.close_drawer();
        true
    }

    /// Highlight the row of the current focus when the list opens.
    fn preselect_focused_agent(&mut self) {
        let choices = self.agents_choices();
        let position = self
            .focus
            .as_ref()
            .and_then(|focus| {
                choices
                    .iter()
                    .position(|choice| matches!(choice, AgentChoice::Member(id) if id == focus))
            })
            .unwrap_or(0);
        self.select_agent_at(position);
    }

    pub fn agents_query(&self) -> &str {
        &self.agents_query
    }

    /// The chat's goal, whatever its status.
    pub fn goal(&self) -> Option<&GoalState> {
        self.goal.as_ref()
    }

    /// The goal to show above the composer: one that is active or paused, in
    /// the view of the member working on it and in the All view (a sub-agent's
    /// window does not show the main agent's goal).
    pub fn visible_goal(&self) -> Option<&GoalState> {
        let goal = self.goal.as_ref()?;
        if goal.status == crate::domain::goal::GoalStatus::Complete {
            return None;
        }
        match self.focus() {
            Some(focus) if focus != &goal.member => None,
            _ => Some(goal),
        }
    }

    /// The name of the chat being viewed, if it has one.
    pub fn chat_name(&self) -> Option<&str> {
        self.chat_name.as_deref()
    }

    /// Whether the selected Agents row can be renamed with Ctrl+R: an
    /// Inactive row, or the Main row (the current chat). Only without a
    /// search, as the rename editor takes the search line's place.
    pub fn agents_can_rename(&self) -> bool {
        self.agents_rename_target().is_some()
    }

    /// Whether the selected Agents row can be deleted with Ctrl+X: an
    /// Inactive row only, never the current chat or a live member.
    pub fn agents_can_delete(&self) -> bool {
        self.agents_query.trim().is_empty()
            && matches!(self.selected_agent(), Some(AgentChoice::Saved(_)))
    }

    /// Whether Ctrl+X has armed the deletion of the selected row.
    pub fn agents_delete_armed(&self) -> bool {
        match (self.agents_delete_armed, self.selected_agent()) {
            (Some(armed), Some(AgentChoice::Saved(selected))) => {
                armed == selected && self.agents_can_delete()
            }
            _ => false,
        }
    }

    /// Any key other than Ctrl+X puts the armed deletion away.
    pub fn disarm_agents_delete(&mut self) {
        self.agents_delete_armed = None;
    }

    /// Ctrl+X in the Agents list: the first press on an Inactive row arms its
    /// deletion, a second one on the same row queues it.
    pub fn press_agents_delete(&mut self) {
        let Some(AgentChoice::Saved(conversation)) = self.selected_agent() else {
            self.agents_delete_armed = None;
            return;
        };
        if !self.agents_can_delete() {
            self.agents_delete_armed = None;
        } else if self.agents_delete_armed == Some(conversation) {
            self.agents_delete_armed = None;
            self.pending_command = Some(UiCommand::DeleteConversation { conversation });
        } else {
            self.agents_delete_armed = Some(conversation);
        }
    }

    /// The chat Ctrl+R would rename (`None` inside is the current chat) and
    /// the name it has now.
    fn agents_rename_target(&self) -> Option<(Option<i64>, String)> {
        if !self.agents_query.trim().is_empty() {
            return None;
        }
        match self.selected_agent()? {
            AgentChoice::Saved(id) => {
                let name = self
                    .saved_chats
                    .iter()
                    .find(|chat| chat.id == id)
                    .and_then(|chat| chat.name.clone())
                    .unwrap_or_default();
                Some((Some(id), name))
            }
            AgentChoice::Member(id) if id.as_str() == MAIN_MEMBER_ID => {
                let is_main = self
                    .members
                    .iter()
                    .any(|member| member.id == id && member.parent.is_none());
                is_main.then(|| (None, self.chat_name.clone().unwrap_or_default()))
            }
            _ => None,
        }
    }

    /// Ctrl+R in the Agents list: open the rename editor, prefilled with the
    /// selected chat's name.
    pub fn begin_agents_rename(&mut self) {
        self.agents_delete_armed = None;
        if let Some((conversation, text)) = self.agents_rename_target() {
            self.agents_rename = Some(AgentsRename { conversation, text });
        }
    }

    /// The text of the open rename editor.
    pub fn agents_rename(&self) -> Option<&str> {
        self.agents_rename.as_ref().map(|edit| edit.text.as_str())
    }

    pub fn agents_rename_push(&mut self, ch: char) {
        if let Some(edit) = &mut self.agents_rename
            && !ch.is_control()
        {
            edit.text.push(ch);
        }
    }

    pub fn agents_rename_paste(&mut self, text: &str) {
        if let Some(edit) = &mut self.agents_rename {
            edit.text.extend(
                text.chars()
                    .map(|ch| if ch.is_control() { ' ' } else { ch }),
            );
        }
    }

    pub fn agents_rename_pop(&mut self) {
        if let Some(edit) = &mut self.agents_rename {
            edit.text.pop();
        }
    }

    /// Delete the last word of the name (and the spaces before it).
    pub fn agents_rename_delete_word(&mut self) {
        if let Some(edit) = &mut self.agents_rename {
            let kept = edit.text.trim_end().len();
            edit.text.truncate(kept);
            let start = edit
                .text
                .rfind(char::is_whitespace)
                .map_or(0, |index| index + 1);
            edit.text.truncate(start);
        }
    }

    pub fn agents_rename_clear(&mut self) {
        if let Some(edit) = &mut self.agents_rename {
            edit.text.clear();
        }
    }

    /// Enter in the rename editor: queue the new name. An empty or
    /// whitespace-only name clears the chat's name.
    pub fn commit_agents_rename(&mut self) {
        if let Some(edit) = self.agents_rename.take() {
            let name = edit.text.trim();
            self.pending_command = Some(UiCommand::RenameConversation {
                conversation: edit.conversation,
                name: (!name.is_empty()).then(|| name.to_string()),
            });
        }
    }

    /// Esc in the rename editor.
    pub fn cancel_agents_rename(&mut self) {
        self.agents_rename = None;
    }

    pub fn agents_query_push(&mut self, ch: char) {
        if !ch.is_control() {
            self.agents_query.push(ch);
        }
        self.after_agents_query_change();
    }

    /// Pasted text goes into the search as one line.
    pub fn agents_query_paste(&mut self, text: &str) {
        self.agents_query.extend(
            text.chars()
                .map(|ch| if ch.is_control() { ' ' } else { ch }),
        );
        self.after_agents_query_change();
    }

    pub fn agents_query_pop(&mut self) {
        self.agents_query.pop();
        self.after_agents_query_change();
    }

    /// Delete the last word of the search (and the spaces before it).
    pub fn agents_query_delete_word(&mut self) {
        let kept = self.agents_query.trim_end().len();
        self.agents_query.truncate(kept);
        let start = self
            .agents_query
            .rfind(char::is_whitespace)
            .map_or(0, |index| index + 1);
        self.agents_query.truncate(start);
        self.after_agents_query_change();
    }

    pub fn agents_query_clear(&mut self) {
        self.agents_query.clear();
        self.after_agents_query_change();
    }

    /// A changed search highlights its best match (the first row); an emptied
    /// one returns to the viewed agent.
    fn after_agents_query_change(&mut self) {
        self.disarm_quit();
        if self.agents_query.trim().is_empty() {
            self.preselect_focused_agent();
        } else {
            self.select_agent_at(0);
        }
    }

    /// Esc in the Agents list: clear a typed search first, then close.
    pub fn escape_agents(&mut self) {
        if self.agents_query.is_empty() {
            self.close_drawer();
        } else {
            self.agents_query_clear();
        }
    }

    /// The painter notes where each selectable row landed on screen.
    pub(crate) fn set_agents_click_rows(&self, rows: Vec<(u16, AgentChoice)>) {
        *self.agents_click_rows.borrow_mut() = rows;
    }

    /// Mouse click on screen row `y` of the Agents list: highlight that row
    /// and open it. Returns whether a row was hit.
    pub fn click_agent_row(&mut self, y: u16) -> bool {
        if self.drawer != Some(Drawer::Agents) {
            return false;
        }
        let choice = self
            .agents_click_rows
            .borrow()
            .iter()
            .find(|(row, _)| *row == y)
            .map(|(_, choice)| choice.clone());
        let Some(choice) = choice else {
            return false;
        };
        self.agents_choice = choice;
        self.apply_selected_agent()
    }

    /// Seconds since `member` last finished a reply this session, if it has.
    pub fn member_reply_age_secs(&self, member: &MemberId) -> Option<u64> {
        self.last_reply_at
            .get(member)
            .map(|at| at.elapsed().as_secs())
    }

    // --- UI actions -----------------------------------------------------

    pub fn toggle_drawer(&mut self, drawer: Drawer) {
        self.disarm_quit();
        self.drawer = if self.drawer.as_ref() == Some(&drawer) {
            self.stash_team_editor_catalog();
            self.mode_editor = None;
            None
        } else {
            // Nothing to show: say so in one line instead of opening a panel.
            if drawer == Drawer::Runs && self.runs.is_empty() {
                self.apply(RuntimeEvent::Notice(
                    "no runs yet — select /mode plan, then send a goal".to_string(),
                ));
                return;
            }
            self.stash_team_editor_catalog();
            if drawer != Drawer::Mode {
                self.mode_editor = None;
            }
            match drawer {
                Drawer::Team => self.open_team_editor(),
                Drawer::Mode => self.open_mode_editor(),
                Drawer::Runs => {
                    self.ensure_selected_run();
                }
                Drawer::Agents => {
                    self.agents_query.clear();
                    self.agents_delete_armed = None;
                    self.agents_rename = None;
                    self.preselect_focused_agent();
                }
                _ => {}
            }
            Some(drawer)
        };
        self.drawer_scroll = 0;
        // Logs read oldest first like the transcript and open on the newest.
        if matches!(self.drawer, Some(Drawer::Logs | Drawer::MemberLogs(_))) {
            self.drawer_scroll = usize::MAX;
        }
    }

    #[cfg(test)]
    pub fn set_skills(&mut self, skills: Vec<SkillInfo>) {
        self.skills = skills;
    }

    /// Keep the old convenient `@codex /skill` spelling only when it matches
    /// a discovered Codex skill. Unknown slash commands and native controls
    /// remain untouched; the runtime must not guess from a leading slash.
    pub(crate) fn normalize_known_skill_invocation(&self, command: UiCommand) -> UiCommand {
        let UiCommand::UserMessage {
            target: MessageTarget::Member(member),
            mut body,
        } = command
        else {
            return command;
        };
        let Some(view) = self.members.iter().find(|view| view.id == member) else {
            return UiCommand::UserMessage {
                target: MessageTarget::Member(member),
                body,
            };
        };
        if view.backend != BackendKind::Codex {
            return UiCommand::UserMessage {
                target: MessageTarget::Member(member),
                body,
            };
        }

        let prefix = format!("@{member}");
        let Some(after_member) = body.strip_prefix(&prefix) else {
            return UiCommand::UserMessage {
                target: MessageTarget::Member(member),
                body,
            };
        };
        let Some(after_slash) = after_member.trim_start().strip_prefix('/') else {
            return UiCommand::UserMessage {
                target: MessageTarget::Member(member),
                body,
            };
        };
        let end = after_slash
            .find(char::is_whitespace)
            .unwrap_or(after_slash.len());
        let name = &after_slash[..end];
        if name.is_empty() {
            return UiCommand::UserMessage {
                target: MessageTarget::Member(member),
                body,
            };
        }
        let invocation = format!("${name}");
        let Some(skill) = self
            .skills
            .iter()
            .find(|skill| skill.backend == BackendKind::Codex && skill.invocation == invocation)
        else {
            return UiCommand::UserMessage {
                target: MessageTarget::Member(member),
                body,
            };
        };
        body = format!("{prefix} {}{}", skill.invocation, &after_slash[end..]);
        UiCommand::UserMessage {
            target: MessageTarget::Member(member),
            body,
        }
    }

    pub fn note_drawer_viewport(&self, height: usize) {
        self.drawer_viewport.set(height);
    }

    pub fn note_drawer_max_scroll(&self, max: usize) {
        self.drawer_max_scroll.set(max);
    }

    pub fn close_drawer(&mut self) {
        self.disarm_quit();
        if self.drawer == Some(Drawer::Agents) {
            self.agents_query.clear();
            self.agents_select_inactive = false;
            self.agents_delete_armed = None;
            self.agents_rename = None;
        }
        self.drawer = None;
        self.drawer_scroll = 0;
        self.stash_team_editor_catalog();
        self.mode_editor = None;
    }

    pub fn stage_selected_run_action(&mut self) -> bool {
        if self.drawer != Some(Drawer::Runs) || !self.composer.is_empty() {
            return false;
        }
        let Some(command) = self.selected_run_stage_command() else {
            return false;
        };
        self.disarm_quit();
        self.header_selected = None;
        self.composer.set_text(&command);
        self.history_cursor = None;
        self.reset_popup();
        self.close_drawer();
        true
    }

    pub fn stage_selected_run_dispatch(&mut self) -> bool {
        if self.drawer != Some(Drawer::Runs) || !self.composer.is_empty() {
            return false;
        }
        let Some(command) = self.selected_run_dispatch_command() else {
            return false;
        };
        self.disarm_quit();
        self.header_selected = None;
        self.composer.set_text(&command);
        self.history_cursor = None;
        self.reset_popup();
        self.close_drawer();
        true
    }

    pub fn toggle_runs_detail(&mut self) -> bool {
        if self.drawer != Some(Drawer::Runs) || !self.composer.is_empty() {
            return false;
        }
        self.disarm_quit();
        self.runs_detail = !self.runs_detail;
        self.drawer_scroll = 0;
        true
    }

    pub fn select_newer_run(&mut self) {
        if self.drawer != Some(Drawer::Runs) {
            return;
        }
        self.disarm_quit();
        self.selected_run_step = None;
        self.ensure_selected_run();
        let Some(id) = self.selected_run else {
            return;
        };
        let Some(index) = self.run_index(id) else {
            return;
        };
        if index + 1 < self.runs.len() {
            self.selected_run = Some(self.runs[index + 1].id);
        }
    }

    pub fn select_older_run(&mut self) {
        if self.drawer != Some(Drawer::Runs) {
            return;
        }
        self.disarm_quit();
        self.selected_run_step = None;
        self.ensure_selected_run();
        let Some(id) = self.selected_run else {
            return;
        };
        let Some(index) = self.run_index(id) else {
            return;
        };
        if index > 0 {
            self.selected_run = Some(self.runs[index - 1].id);
        }
    }

    pub fn select_previous_run_step(&mut self) -> bool {
        if self.drawer != Some(Drawer::Runs) {
            return false;
        }
        self.disarm_quit();
        let Some(run) = self.selected_run() else {
            self.selected_run_step = None;
            return false;
        };
        if run.steps.is_empty() {
            self.selected_run_step = None;
            return false;
        }
        let next = match self.selected_run_step() {
            None => run.steps.last().map(|step| step.number),
            Some(number) => run
                .steps
                .iter()
                .position(|step| step.number == number)
                .and_then(|idx| idx.checked_sub(1))
                .and_then(|idx| run.steps.get(idx))
                .map(|step| step.number),
        }
        .or_else(|| run.steps.first().map(|step| step.number));
        self.selected_run_step = next;
        true
    }

    pub fn select_next_run_step(&mut self) -> bool {
        if self.drawer != Some(Drawer::Runs) {
            return false;
        }
        self.disarm_quit();
        let Some(run) = self.selected_run() else {
            self.selected_run_step = None;
            return false;
        };
        if run.steps.is_empty() {
            self.selected_run_step = None;
            return false;
        }
        let next = match self.selected_run_step() {
            None => run.steps.first().map(|step| step.number),
            Some(number) => run
                .steps
                .iter()
                .position(|step| step.number == number)
                .and_then(|idx| run.steps.get(idx + 1))
                .map(|step| step.number),
        }
        .or_else(|| run.steps.last().map(|step| step.number));
        self.selected_run_step = next;
        true
    }

    fn ensure_selected_run(&mut self) {
        let selected_is_valid = self
            .selected_run
            .is_some_and(|id| self.runs.iter().any(|run| run.id == id));
        if !selected_is_valid {
            self.selected_run = self.runs.last().map(|run| run.id);
            self.selected_run_step = None;
        }
    }

    fn ensure_selected_run_step(&mut self) {
        let Some(step) = self.selected_run_step else {
            return;
        };
        let step_is_valid = self
            .selected_run()
            .is_some_and(|run| run.steps.iter().any(|candidate| candidate.number == step));
        if !step_is_valid {
            self.selected_run_step = None;
        }
    }

    fn run_index(&self, id: RunId) -> Option<usize> {
        self.runs.iter().position(|run| run.id == id)
    }

    /// The drawer's vertical scroll offset (top line to show).
    pub fn drawer_scroll(&self) -> usize {
        self.drawer_scroll
    }
    pub fn drawer_scroll_up(&mut self) {
        self.drawer_scroll_by(-1);
    }
    pub fn drawer_scroll_down(&mut self) {
        self.drawer_scroll_by(1);
    }
    pub fn drawer_scroll_by(&mut self, delta: i32) {
        self.disarm_quit();
        // A drawer opened "at the bottom" holds an out-of-range offset; settle
        // it on the real end before moving.
        self.drawer_scroll = self.drawer_scroll.min(self.drawer_max_scroll.get());
        if delta >= 0 {
            self.drawer_scroll = self.drawer_scroll.saturating_add(delta as usize);
        } else {
            self.drawer_scroll = self
                .drawer_scroll
                .saturating_sub(delta.unsigned_abs() as usize);
        }
    }

    /// The captured working-tree diff shown in the diff drawer.
    pub fn diff_text(&self) -> Option<&str> {
        self.diff_text.as_deref()
    }
    pub fn set_diff(&mut self, diff: String) {
        self.diff_text = Some(diff);
    }

    pub(crate) fn team_editor(&self) -> Option<&TeamEditor> {
        self.team_editor.as_ref()
    }

    pub(crate) fn mode_editor(&self) -> Option<&ModeEditor> {
        self.mode_editor.as_ref()
    }

    pub(crate) fn handle_mode_editor_key(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> ModeEditorOutcome {
        if self.drawer != Some(Drawer::Mode) {
            return ModeEditorOutcome::Ignored;
        }
        let Some(editor) = self.mode_editor.as_mut() else {
            return ModeEditorOutcome::Ignored;
        };
        editor.handle_key(code, modifiers)
    }

    pub(crate) fn insert_mode_editor_text(&mut self, text: &str) -> bool {
        if self.drawer != Some(Drawer::Mode) {
            return false;
        }
        self.mode_editor
            .as_mut()
            .is_some_and(|editor| editor.insert_edit_text(text))
    }

    fn open_mode_editor(&mut self) {
        // Sub-agents come along so the panel can tell an existing seat from
        // one a run would open; the pickers list only top-level members.
        let members = self
            .members
            .iter()
            .map(|view| self.view_to_member(view))
            .collect();
        self.mode_editor = Some(ModeEditor::new(
            self.team.clone(),
            PathBuf::from(&self.workspace),
            self.default_target.clone(),
            members,
            self.modes.clone(),
            self.mode_overrides.clone(),
            self.active_mode,
        ));
    }

    pub(crate) fn handle_team_editor_key(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> TeamEditorOutcome {
        if self.drawer != Some(Drawer::Team) {
            return TeamEditorOutcome::Ignored;
        }
        let Some(editor) = self.team_editor.as_mut() else {
            return TeamEditorOutcome::Ignored;
        };
        editor.handle_key(code, modifiers)
    }

    pub(crate) fn insert_team_editor_text(&mut self, text: &str) -> bool {
        if self.drawer != Some(Drawer::Team) {
            return false;
        }
        self.team_editor
            .as_mut()
            .is_some_and(|editor| editor.insert_edit_text(text))
    }

    /// Returns whether a model list landed while something that shows models
    /// is on screen (the `/model` selector, the welcome page, the Agents
    /// list), so the frame needs redrawing even though nothing was typed.
    pub(crate) fn poll_team_editor_catalog(&mut self) -> bool {
        if let Some(editor) = self.team_editor.as_mut() {
            editor.poll_agent_catalog();
            false
        } else {
            // A model worker may still be completing after its picker closed.
            // Poll it here so the next `/team` or `/model` open can use the
            // ready result.
            self.model_catalog.poll()
                && (self.model_picker.is_some()
                    || self.chat.is_empty()
                    || self.drawer == Some(Drawer::Agents))
        }
    }

    /// Warm every installed backend for the startup workspace. Detection and
    /// model discovery both stay asynchronous: opening Asterline never waits
    /// for a slow CLI, while a later `/team` or `/model` reuses the same keyed cache.
    pub(crate) fn warm_model_catalog_once(&mut self) -> bool {
        if self.model_catalog_warmed || self.workspace.trim().is_empty() {
            return false;
        }
        if self.model_catalog_detection.is_none() {
            let (tx, rx) = mpsc::channel();
            thread::spawn(move || {
                let _ = tx.send(detect_backends());
            });
            self.model_catalog_detection = Some(rx);
            return false;
        }
        let detected = match self
            .model_catalog_detection
            .as_ref()
            .map(Receiver::try_recv)
        {
            Some(Ok(detected)) => detected,
            Some(Err(TryRecvError::Empty)) | None => return false,
            Some(Err(TryRecvError::Disconnected)) => DetectedBackends {
                codex: false,
                claude: false,
                grok: false,
                agy: false,
            },
        };
        self.model_catalog_detection = None;
        let workspace = PathBuf::from(&self.workspace);
        self.detected_backends = Some(detected);
        if let Some(editor) = self.team_editor.as_mut() {
            editor.preload_installed_model_catalogs(detected);
        } else {
            for backend in [
                BackendKind::Codex,
                BackendKind::Claude,
                BackendKind::Grok,
                BackendKind::Agy,
            ] {
                if detected.contains(backend) {
                    self.model_catalog.preload(backend, &workspace);
                }
            }
            self.model_catalog.freeze();
        }
        self.model_catalog_warmed = true;
        // Installed backends are now known; an open selector gains rows.
        self.model_picker.is_some() || self.chat.is_empty()
    }

    fn open_team_editor(&mut self) {
        // The editor owns the top-level roster; sub-agents belong to the chat.
        let members = self
            .members
            .iter()
            .filter(|view| view.parent.is_none())
            .map(|view| self.view_to_member(view))
            .collect();
        let model_catalog = std::mem::take(&mut self.model_catalog);
        let mut editor = TeamEditor::with_model_catalog(
            self.team.clone(),
            PathBuf::from(self.workspace.clone()),
            self.default_target.clone(),
            members,
            model_catalog,
        );
        editor.load_agent_catalog();
        self.team_editor = Some(editor);
    }

    /// The catalog model labels read. While `/team` is open the editor holds
    /// the catalog (it was moved in), so labels resolve through it instead of
    /// falling back to the bare backend name.
    fn display_model_catalog(&self) -> &ModelCatalog {
        self.team_editor
            .as_ref()
            .map_or(&self.model_catalog, TeamEditor::model_catalog)
    }

    fn stash_team_editor_catalog(&mut self) {
        if let Some(editor) = self.team_editor.take() {
            self.model_catalog = editor.into_model_catalog();
        }
    }

    // --- /model selector -------------------------------------------------

    /// The member `/model` changes: the one being viewed, else (in the "All"
    /// view) the default target's member, else the first member.
    fn model_target_index(&self) -> Option<usize> {
        if let Some(focus) = &self.focus
            && let Some(index) = self.members.iter().position(|member| member.id == *focus)
        {
            return Some(index);
        }
        if let Some(DefaultTarget::Member(id)) = &self.default_target
            && let Some(index) = self.members.iter().position(|member| member.id == *id)
        {
            return Some(index);
        }
        (!self.members.is_empty()).then_some(0)
    }

    /// `<Display name> · <effort>` for what `member` effectively runs: its
    /// pinned values, else the CLI's configured ones, else the catalog default.
    /// `None` until a model catalog says (the caller falls back to the backend).
    pub fn member_model_summary(&self, member: &MemberView) -> Option<String> {
        self.display_model_catalog().effective_summary(
            member.backend,
            member.model.as_deref(),
            member.effort,
            Path::new(&self.workspace),
        )
    }

    /// `<Display name> · <effort>` for what `member` effectively runs, as shown
    /// on the header chip and the welcome page. Until a catalog names the model
    /// it is the backend name; the effort part appears only when one is known.
    pub fn member_model_label(&self, member: &MemberView) -> String {
        let effective = self.display_model_catalog().effective_model(
            member.backend,
            member.model.as_deref(),
            member.effort,
            Path::new(&self.workspace),
        );
        let name = effective
            .name
            .or(effective.id)
            .unwrap_or_else(|| member.backend.as_str().to_string());
        match effective.effort {
            Some(effort) => format!("{name} · {}", effort.as_str()),
            None => name,
        }
    }

    /// `member_model_label` for the member with this id, `None` when no such
    /// member is on the team any more.
    pub fn member_model_label_by_id(&self, id: &MemberId) -> Option<String> {
        self.members
            .iter()
            .find(|member| &member.id == id)
            .map(|member| self.member_model_label(member))
    }

    /// The model line of the welcome page: the viewed agent (else the first)
    /// as `<Display name> · <effort>` (see `member_model_label`).
    pub fn welcome_model_summary(&self) -> Option<String> {
        let member = self
            .focus
            .as_ref()
            .and_then(|focus| self.members.iter().find(|member| member.id == *focus))
            .or_else(|| self.members.first())?;
        Some(self.member_model_label(member))
    }

    /// Test hook: pretend `backend` is installed and its models are known.
    #[cfg(test)]
    pub(crate) fn seed_installed_models(&mut self, backend: BackendKind, models: Vec<String>) {
        use crate::adapter::{DiscoveredCatalog, DiscoveredModel};
        let models = models
            .into_iter()
            .enumerate()
            .map(|(index, id)| {
                let mut model = DiscoveredModel::simple(id);
                model.is_default = index == 0;
                model
            })
            .collect();
        self.seed_installed_catalog(
            backend,
            DiscoveredCatalog {
                models,
                native_permission: None,
                ..Default::default()
            },
        );
    }

    /// Test hook: pretend `backend` is installed with this catalog.
    #[cfg(test)]
    pub(crate) fn seed_installed_catalog(
        &mut self,
        backend: BackendKind,
        catalog: crate::adapter::DiscoveredCatalog,
    ) {
        let mut detected = self.detected_backends.unwrap_or(DetectedBackends {
            codex: false,
            claude: false,
            grok: false,
            agy: false,
        });
        match backend {
            BackendKind::Codex => detected.codex = true,
            BackendKind::Claude => detected.claude = true,
            BackendKind::Grok => detected.grok = true,
            BackendKind::Agy => detected.agy = true,
        }
        self.detected_backends = Some(detected);
        self.model_catalog
            .seed_catalog(backend, Path::new(&self.workspace), catalog);
    }

    pub fn model_picker_open(&self) -> bool {
        self.model_picker.is_some()
    }

    /// Open the inline model selector for the agent being viewed, with the
    /// search box prefilled. Does nothing (but says why) with no agents.
    pub fn open_model_picker(&mut self, filter: &str) {
        let Some(index) = self.model_target_index() else {
            self.apply(RuntimeEvent::Notice(
                "no agent to change the model of".to_string(),
            ));
            return;
        };
        self.close_drawer();
        self.header_selected = None;
        self.disarm_quit();
        let target = &self.members[index];
        let (target_id, target_backend) = (target.id.clone(), target.backend);
        let workspace = PathBuf::from(&self.workspace);
        for backend in [
            BackendKind::Codex,
            BackendKind::Claude,
            BackendKind::Grok,
            BackendKind::Agy,
        ] {
            if backend == target_backend
                || self.detected_backends.is_some_and(|d| d.contains(backend))
            {
                self.model_catalog.ensure(backend, &workspace);
            }
        }
        self.model_picker = Some(ModelPicker::new(target_id, filter.trim().to_string()));
        // Start on the current model when it is listed; else the first row.
        let current = self.model_target_current_key();
        if let Some(picker) = self.model_picker.as_mut() {
            picker.selected = current;
        }
        // Pin the first row down by identity too, so catalogs that land later
        // cannot slide a different model under the highlight.
        let listed = self
            .model_picker
            .as_ref()
            .and_then(|picker| picker.selected.as_ref())
            .is_some_and(|key| self.model_picker_entries().iter().any(|e| &e.key == key));
        if !listed {
            self.select_model_at(0);
        }
    }

    /// The model id the target runs: its pinned model, else the one the CLI's
    /// configuration selects when the catalog lists it.
    fn target_model_id(&self) -> Option<String> {
        let target = self.model_target()?;
        if let Some(model) = &target.model {
            return Some(model.clone());
        }
        match self
            .model_catalog
            .view(target.backend, Path::new(&self.workspace))
        {
            CatalogView::Ready(catalog) => catalog
                .native_model
                .clone()
                .filter(|native| catalog.models.iter().any(|model| model.matches_id(native))),
            _ => None,
        }
    }

    /// The row the target agent currently sits on.
    fn model_target_current_key(&self) -> Option<ModelKey> {
        let target = self.model_target()?;
        model_picker::current_key(
            target.backend,
            target.model.as_deref(),
            &self.model_catalog,
            Path::new(&self.workspace),
        )
    }

    /// Append ` · earlier messages not carried over` to the newest `Model: …`
    /// row (once).
    fn mark_last_model_notice_truncated(&mut self) {
        let suffix = format!(" · {NOT_CARRIED_OVER}");
        let found = self.chat.iter_mut().rev().find_map(|item| match item {
            ChatItem::Notice { text, .. } if text.starts_with("Model: ") => Some(text),
            _ => None,
        });
        if let Some(text) = found
            && !text.ends_with(&suffix)
        {
            text.push_str(&suffix);
            self.touch_chat();
        }
    }

    /// The agent the open `/model` selector will change.
    pub fn model_picker_target(&self) -> Option<MemberId> {
        self.model_picker
            .as_ref()
            .map(|picker| picker.target.clone())
    }

    /// Note that a `/model` change was just sent, so the runtime's
    /// `team updated` confirmation can be dropped. Call after pushing the
    /// `Model: …` notice.
    pub fn expect_model_switch_confirmation(&mut self) {
        self.model_switch_pending = true;
    }

    pub fn close_model_picker(&mut self) {
        self.disarm_quit();
        self.model_picker = None;
    }

    fn model_picker_rows(&self) -> Vec<PickerRow> {
        let Some(picker) = &self.model_picker else {
            return Vec::new();
        };
        let Some(target) = self.model_target() else {
            return Vec::new();
        };
        let detected = self.detected_backends.unwrap_or(DetectedBackends {
            codex: false,
            claude: false,
            grok: false,
            agy: false,
        });
        model_picker::build_rows(
            detected,
            target.backend,
            &self.model_catalog,
            Path::new(&self.workspace),
            &picker.filter,
        )
    }

    fn model_picker_entries(&self) -> Vec<ModelEntry> {
        self.model_picker_rows()
            .into_iter()
            .filter_map(|row| match row {
                PickerRow::Entry(entry) => Some(entry),
                _ => None,
            })
            .collect()
    }

    /// Position of the highlighted model among the selectable rows. If it no
    /// longer matches the filter, the nearest valid position.
    fn model_picker_position(&self) -> usize {
        let Some(picker) = &self.model_picker else {
            return 0;
        };
        let entries = self.model_picker_entries();
        picker
            .selected
            .as_ref()
            .and_then(|key| entries.iter().position(|entry| &entry.key == key))
            .unwrap_or_else(|| picker.fallback.min(entries.len().saturating_sub(1)))
    }

    fn selected_model_entry(&self) -> Option<ModelEntry> {
        self.model_picker_entries()
            .into_iter()
            .nth(self.model_picker_position())
    }

    fn model_target(&self) -> Option<&MemberView> {
        let picker = self.model_picker.as_ref()?;
        self.members
            .iter()
            .find(|member| member.id == picker.target)
    }

    /// The level shown for an entry: the one the user cycled to, else its
    /// seeded level. `None` for models without effort levels.
    fn model_picker_effort(&self, entry: &ModelEntry) -> Option<Effort> {
        let picker = self.model_picker.as_ref()?;
        if let Some(effort) = picker.efforts.get(&entry.key) {
            return Some(*effort);
        }
        let is_current = self.model_target_current_key().as_ref() == Some(&entry.key);
        // A merged row of effort-suffixed siblings starts on the variant the
        // target runs; the level lives in the model id, not in `effort`.
        if !entry.variants.is_empty() {
            let running = self.target_model_id().filter(|_| is_current);
            let variant = running.and_then(|id| {
                entry
                    .variants
                    .iter()
                    .find(|(_, variant_id)| *variant_id == id)
                    .map(|(effort, _)| *effort)
            });
            return model_picker::seed_level(entry, variant, None);
        }
        let members_effort = self
            .model_target()
            .filter(|_| is_current)
            .and_then(|target| target.effort);
        let native = match self
            .model_catalog
            .view(entry.key.backend, Path::new(&self.workspace))
        {
            CatalogView::Ready(catalog) => catalog.native_efforts.for_model(&entry.key.model),
            _ => None,
        };
        model_picker::seed_level(entry, members_effort, native)
    }

    fn select_model_at(&mut self, position: usize) {
        let entry = self.model_picker_entries().into_iter().nth(position);
        if let (Some(picker), Some(entry)) = (self.model_picker.as_mut(), entry) {
            picker.selected = Some(entry.key);
            picker.fallback = position;
            picker.notice = None;
        }
    }

    pub fn select_previous_model(&mut self) {
        self.select_model_at(self.model_picker_position().saturating_sub(1));
    }

    pub fn select_next_model(&mut self) {
        let next = self.model_picker_position() + 1;
        if next < self.model_picker_entries().len() {
            self.select_model_at(next);
        }
    }

    pub fn model_filter_push(&mut self, ch: char) {
        if let Some(picker) = self.model_picker.as_mut() {
            picker.filter.push(ch);
        }
        self.after_model_filter_change();
    }

    /// Pasted text goes into the search box as one line.
    pub fn model_filter_paste(&mut self, text: &str) {
        if let Some(picker) = self.model_picker.as_mut() {
            picker.filter.extend(
                text.chars()
                    .map(|ch| if ch.is_control() { ' ' } else { ch }),
            );
        }
        self.after_model_filter_change();
    }

    pub fn model_filter_pop(&mut self) {
        if let Some(picker) = self.model_picker.as_mut() {
            picker.filter.pop();
        }
        self.after_model_filter_change();
    }

    pub fn model_filter_clear(&mut self) {
        if let Some(picker) = self.model_picker.as_mut() {
            picker.filter.clear();
        }
        self.after_model_filter_change();
    }

    /// A selection the new filter hides moves to the first match.
    fn after_model_filter_change(&mut self) {
        self.disarm_quit();
        let entries = self.model_picker_entries();
        let still_listed = self
            .model_picker
            .as_ref()
            .and_then(|picker| picker.selected.as_ref())
            .is_some_and(|key| entries.iter().any(|entry| &entry.key == key));
        if let Some(picker) = self.model_picker.as_mut() {
            picker.notice = None;
        }
        if !still_listed {
            self.select_model_at(0);
        }
    }

    /// Step the highlighted model's effort through the levels it supports.
    /// Models without levels have nothing to change.
    pub fn cycle_model_effort(&mut self, forward: bool) {
        self.disarm_quit();
        let Some(entry) = self.selected_model_entry() else {
            return;
        };
        if entry.levels.is_empty() {
            return;
        }
        let next = model_picker::cycle_level(&entry, self.model_picker_effort(&entry), forward);
        if let (Some(picker), Some(next)) = (self.model_picker.as_mut(), next) {
            picker.efforts.insert(entry.key, next);
            picker.notice = None;
        }
    }

    /// Everything the renderer needs, or `None` when the selector is closed.
    pub(crate) fn model_picker_view(&self) -> Option<PickerView> {
        let picker = self.model_picker.as_ref()?;
        let target = self.model_target()?;
        let current = self.model_target_current_key();
        let rows = self.model_picker_rows();
        let selected_key = self.selected_model_entry().map(|entry| entry.key);
        let mut views = Vec::new();
        let mut selected_row = None;
        let mut models = 0;
        let mut selected_position = 0;
        for row in &rows {
            match row {
                PickerRow::Entry(entry) => {
                    models += 1;
                    let selected = selected_key.as_ref() == Some(&entry.key);
                    let level = self.model_picker_effort(entry);
                    if selected {
                        selected_row = Some(views.len());
                        selected_position = models;
                    }
                    views.push(PickerRowView::Entry {
                        name: entry.name.clone(),
                        levels: entry.levels.clone(),
                        level,
                        trailing: if current.as_ref() == Some(&entry.key) {
                            "current".to_string()
                        } else {
                            String::new()
                        },
                        selected,
                    });
                }
                PickerRow::Heading(backend) => {
                    views.push(PickerRowView::Heading(backend.as_str().to_string()))
                }
                PickerRow::Loading(_) => {
                    views.push(PickerRowView::Note("loading models…".to_string()))
                }
                PickerRow::Failed(_, error) => views.push(PickerRowView::Note(format!(
                    "model discovery failed: {error}"
                ))),
            }
        }
        Some(PickerView {
            target: target.display_name.clone(),
            filter: picker.filter.clone(),
            rows: views,
            selected_row,
            selected_position,
            total_models: models,
            notice: picker.notice.clone(),
        })
    }

    /// The roster update that switches the targeted agent to the highlighted
    /// model, plus the notice to show once it is sent. `None` (with a notice)
    /// when it cannot be applied now.
    ///
    /// Same backend: only `model` and `effort` change. A different backend
    /// takes that CLI's default permission preset and drops the stored
    /// session, so the new CLI starts fresh. Id, name, role and prompt stay.
    pub fn selected_model_command(&mut self) -> Option<(UiCommand, String)> {
        let entry = self.selected_model_entry()?;
        let index = self.model_target_index()?;
        let target = &self.members[index];
        let busy = member_status_is_active(target.status)
            || self.has_active_message(&target.id)
            || !self.queued_prompts_for(&target.id).is_empty();
        if busy {
            let message = "Stop the running work before switching models (Esc).".to_string();
            if let Some(picker) = self.model_picker.as_mut() {
                picker.notice = Some(message.clone());
            }
            let member = target.id.clone();
            self.apply(RuntimeEvent::MemberNotice {
                member,
                text: message,
            });
            return None;
        }
        // Picking a model always pins its id, and what the row shows is what
        // is saved and sent: the level it displays (seeded or cycled to) is
        // written as the effort. A model without levels gets none. For a
        // merged row of effort variants the id itself carries the level, so
        // no effort is written.
        let level = self.model_picker_effort(&entry);
        let pinned = entry.model_id(level);
        let effort = if entry.variants.is_empty() {
            level
        } else {
            None
        };
        let mut members: Vec<TeamMember> = self
            .members
            .iter()
            .map(|view| self.view_to_member(view))
            .collect();
        let name = members[index].display_name.clone();
        let main = &mut members[index];
        if main.backend != entry.key.backend {
            main.backend = entry.key.backend;
            crate::domain::config::apply_backend_defaults(main);
            main.session_id = None;
        }
        main.model = Some(pinned.clone());
        main.effort = effort;
        let mut notice = format!("Model: {name} → {} · {pinned}", entry.key.backend.as_str());
        if let Some(effort) = effort {
            notice.push_str(&format!(" · {}", effort.as_str()));
        }
        Some((
            UiCommand::ReplaceTeam {
                members,
                default_target: self.default_target.clone(),
            },
            notice,
        ))
    }

    fn view_to_member(&self, view: &MemberView) -> TeamMember {
        let mut member = TeamMember::new(
            view.id.clone(),
            view.display_name.clone(),
            view.backend,
            view.role.clone(),
        );
        member.cwd = if view.cwd.is_empty() || view.cwd == self.workspace {
            None
        } else {
            Some(PathBuf::from(&view.cwd))
        };
        member.model = view.model.clone();
        member.sandbox = view.sandbox;
        member.permission_mode = view.permission_mode;
        member.approvals_reviewer = view.approvals_reviewer;
        member.session_policy = view.session_policy;
        member.session_id = view.session.clone();
        member.effort = view.effort;
        member.parent = view.parent.clone();
        member
    }

    pub fn select_next_member(&mut self) {
        self.disarm_quit();
        let len = self.members.len();
        if len == 0 {
            return;
        }
        let next_idx = match self.header_selected {
            None => 0,
            Some(idx) => (idx + 1) % len,
        };
        self.header_selected = Some(next_idx);
        self.reset_popup();
    }

    pub fn select_prev_member(&mut self) {
        self.disarm_quit();
        let len = self.members.len();
        if len == 0 {
            return;
        }
        let prev_idx = match self.header_selected {
            None => len - 1,
            Some(idx) => (idx + len - 1) % len,
        };
        self.header_selected = Some(prev_idx);
        self.reset_popup();
    }

    pub fn clear_header_selection(&mut self) {
        self.disarm_quit();
        self.header_selected = None;
    }

    pub fn scroll_up(&mut self) {
        self.scroll_by(self.chat_page_rows as i32);
    }

    pub fn scroll_down(&mut self) {
        self.scroll_by(-(self.chat_page_rows as i32));
    }

    pub fn scroll_by(&mut self, delta: i32) {
        self.disarm_quit();
        if delta >= 0 {
            self.scroll = self.scroll.saturating_add(delta as usize);
        } else {
            self.scroll = self.scroll.saturating_sub(delta.unsigned_abs() as usize);
        }
    }

    pub fn reset_scroll(&mut self) {
        self.disarm_quit();
        self.scroll = 0;
    }

    pub fn is_quit_armed(&self) -> bool {
        self.quit_armed
    }

    pub fn quit(&mut self) {
        self.quit_armed = false;
        self.should_quit = true;
    }

    pub fn request_quit(&mut self) {
        if self.quit_armed {
            self.quit();
        } else {
            self.quit_armed = true;
            self.push(ChatItem::Notice {
                member: None,
                text: "press Ctrl+C again to quit".to_string(),
            });
        }
    }

    pub fn thinking_expanded(&self) -> bool {
        self.thinking_expanded
    }

    pub fn diffs_expanded(&self) -> bool {
        self.diffs_expanded
    }

    pub fn tools_expanded(&self) -> bool {
        self.tools_expanded
    }

    pub fn toggle_thinking_expansion(&mut self) {
        self.disarm_quit();
        self.thinking_expanded = !self.thinking_expanded;
    }

    pub fn toggle_diffs_expansion(&mut self) {
        self.disarm_quit();
        self.diffs_expanded = !self.diffs_expanded;
    }

    pub fn toggle_tools_expansion(&mut self) {
        self.disarm_quit();
        self.tools_expanded = !self.tools_expanded;
    }

    /// Whether the tool group starting at chat index `first` shows every row.
    /// Ctrl+O opens all groups; otherwise only clicked ones are open.
    pub fn tool_group_expanded(&self, first: usize) -> bool {
        self.tools_expanded || self.expanded_tool_groups.contains(&first)
    }

    pub(crate) fn tool_groups_revision(&self) -> u64 {
        self.tool_groups_revision
    }

    /// Flip one tool group between its one-line and full forms.
    pub fn toggle_tool_group(&mut self, first: usize) {
        self.disarm_quit();
        if !self.expanded_tool_groups.remove(&first) {
            self.expanded_tool_groups.insert(first);
        }
        self.tool_groups_revision = self.tool_groups_revision.wrapping_add(1);
    }

    /// Forget every per-group choice (new chat, resumed chat).
    fn clear_tool_group_expansion(&mut self) {
        if !self.expanded_tool_groups.is_empty() {
            self.expanded_tool_groups.clear();
        }
        self.tool_groups_revision = self.tool_groups_revision.wrapping_add(1);
    }

    /// First chat index of the contiguous Tool/Diff run that contains `idx`.
    fn work_run_start(&self, idx: usize) -> Option<usize> {
        let member = work_item_member(self.chat.get(idx)?)?;
        let mut start = idx;
        while start > 0 && work_item_member(&self.chat[start - 1]) == Some(member) {
            start -= 1;
        }
        Some(start)
    }

    /// Tool calls the run starting at `start` lists as rows. Successful file
    /// edits become a diff block instead, as in the chat view.
    fn work_run_tool_rows(&self, start: usize) -> usize {
        let Some(member) = self.chat.get(start).and_then(work_item_member) else {
            return 0;
        };
        self.chat[start..]
            .iter()
            .take_while(|item| work_item_member(item) == Some(member))
            .filter(|item| match item {
                ChatItem::Tool { name, ok, .. } => {
                    *ok == Some(false) || !tool_display::is_file_change_tool_name(name)
                }
                _ => false,
            })
            .count()
    }

    /// `/find` landing inside a folded tool group opens it, so the match is
    /// on screen instead of hidden behind a summary line.
    fn reveal_tool_group(&mut self, idx: usize) {
        let Some(start) = self.work_run_start(idx) else {
            return;
        };
        if matches!(self.chat[idx], ChatItem::Tool { .. })
            && self.work_run_tool_rows(start) > tool_display::TOOL_GROUP_COLLAPSE_ABOVE
            && !self.tool_group_expanded(start)
        {
            self.toggle_tool_group(start);
        }
    }

    pub fn active_reasoning(&self) -> &HashMap<MemberId, String> {
        &self.active_reasoning
    }

    /// How long `member` has been running, for the "working" elapsed timer.
    pub fn is_thinking_live(&self, member: &MemberId) -> bool {
        self.reasoning_buffers.contains_key(member)
    }

    pub fn thinking_live_secs(&self, member: &MemberId) -> Option<u64> {
        self.thinking_started
            .get(member)
            .map(|started| started.elapsed().as_secs())
    }

    pub fn member_elapsed_secs(&self, member: &MemberId) -> Option<u64> {
        self.running_since
            .get(member)
            .map(|t| t.elapsed().as_secs())
    }
}

pub(crate) fn run_action_command(
    run: &RunSummary,
    _workspace: &str,
    include_run_id: bool,
) -> Option<String> {
    match run.status {
        RunStatus::Running | RunStatus::Verifying => None,
        RunStatus::Done => run
            .mode
            .as_ref()
            .map(|mode| format!("/mode {}", mode.mode.as_str()))
            .or_else(|| Some("/mode plan".to_string())),
        RunStatus::Failed if run.verification.is_some() => {
            let mut command = continue_command_prefix(run, include_run_id);
            command.push_str(" fix failing verification");
            Some(command)
        }
        RunStatus::Failed => Some(continue_command_prefix(run, include_run_id)),
        RunStatus::Blocked => {
            let mut command = continue_command_prefix(run, include_run_id);
            command.push_str(" blocker resolved");
            Some(command)
        }
        RunStatus::Planned => Some("/retry".to_string()),
    }
}

fn continue_command_prefix(run: &RunSummary, include_run_id: bool) -> String {
    if include_run_id {
        format!("/continue {}", run.label())
    } else {
        "/continue".to_string()
    }
}

fn run_step_action_command(run: &RunSummary, step: u32) -> Option<String> {
    let step = run
        .steps
        .iter()
        .find(|candidate| candidate.number == step)?;
    let (action, note) = match step.status {
        RunStepStatus::Todo => ("doing", None),
        RunStepStatus::Doing => ("done", None),
        RunStepStatus::Blocked => ("doing", Some("blocker resolved")),
        RunStepStatus::Done => ("todo", Some("reopen")),
    };
    let mut command = format!("/step {action} {} {}", run.label(), step.number);
    if let Some(note) = note {
        command.push(' ');
        command.push_str(note);
    }
    Some(command)
}

fn run_step_dispatch_command(run: &RunSummary, step: u32) -> Option<String> {
    let step = run
        .steps
        .iter()
        .find(|candidate| candidate.number == step)?;
    let Some(owner) = &step.owner else {
        return Some(format!("/step assign {} {} ", run.label(), step.number));
    };

    let instruction = match step.status {
        RunStepStatus::Todo => "Start",
        RunStepStatus::Doing => "Continue",
        RunStepStatus::Blocked => "Revisit blocked",
        RunStepStatus::Done => "Review completed",
    };
    Some(format!(
        "@{owner} {}",
        crate::runtime::mode_prompts::manual_step_dispatch_text(
            run.label(),
            instruction,
            step.number,
            &step.title,
        )
    ))
}

/// Searchable text for one chat item (used by `/find`).
fn chat_item_search_text(item: &ChatItem) -> String {
    match item {
        ChatItem::User { body, .. } => crate::adapter::prompt_images::display_prompt_images(body),
        ChatItem::Agent { text, .. } => text.clone(),
        ChatItem::Thinking { text, .. } => text.clone(),
        ChatItem::Tool {
            name,
            summary,
            detail,
            ..
        } => format!("{name} {summary} {detail}"),
        ChatItem::Diff { files, .. } => files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>()
            .join(" "),
        ChatItem::Route { body, .. } => body.clone(),
        ChatItem::Spawn {
            parent,
            child,
            summary,
        } => format!("spawn {parent} {child} {summary}"),
        ChatItem::Notice { text, .. } => text.clone(),
        ChatItem::Error { message, .. } => message.clone(),
        ChatItem::Verdict { summary, .. } => summary.clone(),
        ChatItem::GoalMarker { turn, .. } => format!("goal turn {turn}"),
    }
}

/// Rough estimate of how many visual lines a [`ChatItem`] will occupy, used
/// only to keep the scroll position stable when new items arrive while the
/// user is browsing history, and for approximate `/find` jumps. The exact
/// count depends on the render width and markdown layout, so this is a
/// conservative lower bound.
fn estimate_item_lines(item: &ChatItem) -> usize {
    match item {
        ChatItem::User { body, .. } => body.lines().count().max(1),
        ChatItem::Agent { text, .. } => {
            if text.is_empty() {
                1 // header only
            } else {
                text.lines().count().max(1) + 1 // +1 for header
            }
        }
        ChatItem::Thinking { .. } => 1,
        ChatItem::Tool { detail, .. } => 2 + usize::from(!detail.is_empty()),
        ChatItem::Diff { files, .. } => 1 + files.len(),
        ChatItem::Route { body, .. } => 1 + body.lines().count().max(1),
        ChatItem::Spawn { .. } => 2,
        ChatItem::Notice { text, .. } => text.lines().count().max(1),
        ChatItem::Error { message, .. } => message.lines().count().max(1),
        ChatItem::Verdict { summary, .. } => 1 + summary.lines().count(),
        ChatItem::GoalMarker { .. } => 1,
    }
}

/// The member behind a tool call or diff, the items that share one work run.
fn work_item_member(item: &ChatItem) -> Option<&MemberId> {
    match item {
        ChatItem::Tool { member, .. } | ChatItem::Diff { member, .. } => Some(member),
        _ => None,
    }
}

fn chat_truncation_notice() -> ChatItem {
    ChatItem::Notice {
        member: None,
        text: "[asterline: chat item truncated]".to_string(),
    }
}

fn bounded_chat_field(text: String, fixed_bytes: usize) -> Option<String> {
    let limit = MAX_CHAT_ITEM_BYTES.checked_sub(fixed_bytes)?;
    (limit >= MIN_BOUNDED_CHAT_TEXT_BYTES).then(|| bounded_text(&text, limit))
}

/// Bound one hot-cache item independently from the larger persisted replay
/// limits. Composite metadata that leaves no useful text budget degrades to a
/// visible notice, ensuring every variant obeys the same hard ceiling.
fn bound_chat_item(item: ChatItem) -> ChatItem {
    let bounded = match item {
        ChatItem::User {
            body,
            targets,
            interrupted,
        } => bounded_chat_field(body, 0).map(|body| ChatItem::User {
            body,
            targets,
            interrupted,
        }),
        ChatItem::Agent {
            member,
            display_name,
            backend,
            text,
        } => {
            let fixed = member.as_str().len().saturating_add(display_name.len());
            bounded_chat_field(text, fixed).map(|text| ChatItem::Agent {
                member,
                display_name,
                backend,
                text,
            })
        }
        ChatItem::Thinking {
            member,
            display_name,
            backend,
            text,
            elapsed_secs,
        } => {
            let fixed = member.as_str().len().saturating_add(display_name.len());
            bounded_chat_field(text, fixed).map(|text| ChatItem::Thinking {
                member,
                display_name,
                backend,
                text,
                elapsed_secs,
            })
        }
        ChatItem::Tool {
            member,
            name,
            summary,
            detail,
            ok,
        } => {
            let fixed = member
                .as_str()
                .len()
                .saturating_add(name.len())
                .saturating_add(summary.len());
            bounded_chat_field(detail, fixed).map(|detail| ChatItem::Tool {
                member,
                name,
                summary,
                detail,
                ok,
            })
        }
        ChatItem::Diff { member, files, ok } => (member.as_str().len().saturating_add(
            files
                .iter()
                .map(|file| {
                    file.path
                        .len()
                        .saturating_add(file.kind.len())
                        .saturating_add(
                            file.old_text
                                .as_ref()
                                .map(String::len)
                                .unwrap_or(0)
                                .saturating_add(
                                    file.new_text.as_ref().map(String::len).unwrap_or(0),
                                )
                                .saturating_add(file.patch.as_ref().map(String::len).unwrap_or(0)),
                        )
                })
                .sum::<usize>(),
        ) <= MAX_CHAT_ITEM_BYTES)
            .then_some(ChatItem::Diff { member, files, ok }),
        ChatItem::Route { from, to, body } => {
            let fixed = from
                .as_str()
                .len()
                .saturating_add(to.iter().map(String::len).sum::<usize>());
            bounded_chat_field(body, fixed).map(|body| ChatItem::Route { from, to, body })
        }
        ChatItem::Spawn {
            parent,
            child,
            summary,
        } => {
            let fixed = parent.as_str().len().saturating_add(child.as_str().len());
            bounded_chat_field(summary, fixed).map(|summary| ChatItem::Spawn {
                parent,
                child,
                summary,
            })
        }
        ChatItem::Notice { text, member } => {
            let fixed = member.as_ref().map_or(0, |member| member.as_str().len());
            bounded_chat_field(text, fixed).map(|text| ChatItem::Notice { text, member })
        }
        ChatItem::Error { member, message } => {
            let fixed = member.as_ref().map_or(0, |member| member.as_str().len());
            bounded_chat_field(message, fixed).map(|message| ChatItem::Error { member, message })
        }
        ChatItem::Verdict {
            member,
            approve,
            summary,
        } => {
            let fixed = member.as_str().len();
            bounded_chat_field(summary, fixed).map(|summary| ChatItem::Verdict {
                member,
                approve,
                summary,
            })
        }
        marker @ ChatItem::GoalMarker { .. } => Some(marker),
    };
    let bounded = bounded.unwrap_or_else(chat_truncation_notice);
    debug_assert!(chat_item_bytes(&bounded) <= MAX_CHAT_ITEM_BYTES);
    bounded
}

fn chat_item_bytes(item: &ChatItem) -> usize {
    match item {
        ChatItem::User { body, .. } => body.len(),
        ChatItem::Agent {
            member,
            display_name,
            text,
            ..
        }
        | ChatItem::Thinking {
            member,
            display_name,
            text,
            ..
        } => member.as_str().len() + display_name.len() + text.len(),
        ChatItem::Tool {
            member,
            name,
            summary,
            detail,
            ..
        } => member.as_str().len() + name.len() + summary.len() + detail.len(),
        ChatItem::Diff { member, files, .. } => {
            member.as_str().len()
                + files
                    .iter()
                    .map(|file| {
                        file.path.len()
                            + file.kind.len()
                            + file.old_text.as_ref().map(String::len).unwrap_or(0)
                            + file.new_text.as_ref().map(String::len).unwrap_or(0)
                            + file.patch.as_ref().map(String::len).unwrap_or(0)
                    })
                    .sum::<usize>()
        }
        ChatItem::Route { from, to, body } => {
            from.as_str().len() + body.len() + to.iter().map(String::len).sum::<usize>()
        }
        ChatItem::Spawn {
            parent,
            child,
            summary,
        } => parent.as_str().len() + child.as_str().len() + summary.len(),
        ChatItem::Notice { text, member } => {
            text.len() + member.as_ref().map_or(0, |member| member.as_str().len())
        }
        ChatItem::Error { member, message } => {
            member.as_ref().map_or(0, |member| member.as_str().len()) + message.len()
        }
        ChatItem::Verdict {
            member, summary, ..
        } => member.as_str().len() + summary.len(),
        ChatItem::GoalMarker { member, .. } => member.as_str().len() + 4,
    }
}

fn trim_initial_chat(chat: &mut [ChatItem]) {
    // Bound each item, but keep the conversation from its first message.
    for item in chat.iter_mut() {
        let original = std::mem::replace(item, chat_truncation_notice());
        *item = bound_chat_item(original);
    }
}

fn trim_prompt_history(history: &mut Vec<String>) {
    for entry in history.iter_mut() {
        if entry.len() > MAX_CHAT_ITEM_BYTES {
            *entry = bounded_text(entry, MAX_CHAT_ITEM_BYTES);
        }
    }
    let mut bytes = history.iter().map(String::len).sum::<usize>();
    let mut remove = 0;
    while remove < history.len()
        && (history.len().saturating_sub(remove) > MAX_PROMPT_HISTORY_ITEMS
            || bytes > MAX_PROMPT_HISTORY_BYTES)
    {
        bytes = bytes.saturating_sub(history[remove].len());
        remove += 1;
    }
    if remove > 0 {
        history.drain(..remove);
        history.shrink_to_fit();
    }
}

fn compact_active_chat_item(item: &mut ChatItem) -> bool {
    match item {
        ChatItem::Agent { text, .. } if text != ACTIVE_MESSAGE_OUTPUT_OMITTED => {
            *text = ACTIVE_MESSAGE_OUTPUT_OMITTED.to_string();
            true
        }
        ChatItem::Thinking { text, .. } if text != ACTIVE_MESSAGE_OUTPUT_OMITTED => {
            *text = ACTIVE_MESSAGE_OUTPUT_OMITTED.to_string();
            true
        }
        ChatItem::Tool { detail, .. } if detail != ACTIVE_TOOL_OUTPUT_OMITTED => {
            *detail = ACTIVE_TOOL_OUTPUT_OMITTED.to_string();
            true
        }
        _ => false,
    }
}

fn active_completion_text(marker: &str, text: &str, omitted: bool) -> String {
    if !omitted {
        return bounded_text(text, MAX_CHAT_ITEM_BYTES);
    }
    if text.is_empty() {
        marker.to_string()
    } else {
        let mut completed = format!("{marker}\n\n");
        let _ = append_bounded_text(&mut completed, text, MAX_CHAT_ITEM_BYTES);
        completed
    }
}

fn active_tool_key(id: &str) -> String {
    if id.len() <= MAX_ACTIVE_TOOL_ID_BYTES {
        return id.to_string();
    }
    format!("sha256:{:x}", Sha256::digest(id.as_bytes()))
}

fn interrupted_completion_text(
    omission_marker: &str,
    interrupted_marker: &str,
    partial: &str,
    omitted: bool,
) -> String {
    let mut completed = String::new();
    if omitted {
        completed.push_str(omission_marker);
        completed.push_str("\n\n");
    }
    completed.push_str(interrupted_marker);
    if !partial.is_empty() && partial != omission_marker {
        completed.push_str("\n\n");
        let _ = append_bounded_text(&mut completed, partial, MAX_CHAT_ITEM_BYTES);
    }
    completed
}

#[cfg(test)]
#[path = "app_state_tests/mod.rs"]
mod tests;
