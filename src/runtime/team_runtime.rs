//! The team runtime core.
//!
//! Pure orchestration logic: [`TeamRuntime::on_ui_command`] and
//! [`TeamRuntime::on_agent_event`] take an input and return the
//! [`RuntimeEvent`]s to emit plus the [`RunAction`]s to dispatch. All threading
//! and child-process work lives in the transport layer (`agent_runner` / the
//! `run` loop), so the core is fully unit-testable without spawning anything.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::adapter::parser::{
    MAX_MESSAGE_TEXT_BYTES, MAX_TOOL_DETAIL_BYTES, append_bounded_text, bounded_text,
    format_permission_denial, is_permission_denied_tool,
};
use crate::domain::config::{
    ASTERLINE_BRAINSTORM_SKILL_NAME, MAIN_MEMBER_ID, brainstorm_skill_text,
    inject_team_protocol_with, replace_model_style_name, strip_team_protocol, strip_team_protocols,
    sub_agent_member, team_protocol_text, team_skill_hint,
};
use crate::domain::event::{
    AgentEvent, AgentSessionId, ApprovalDecision, ApprovalId, ChatItem, CloseRequest,
    DepartedMember, ImportedMessage, LogEntry, MemberStatus, MemberSummary, MessageId,
    MessageTarget, RouteTo, RunId, RunStatus, RunStepRequest, RunStepStatus, RunStepSummary,
    RunSummary, RuntimeEvent, SpawnRequest, TeamMessage, TurnId, UiCommand,
};
use crate::domain::goal::{
    CONTINUATION_NO_PROGRESS_CAP, GoalAction, GoalState, GoalStatus, REASON_INTERRUPT,
    REASON_NO_PROGRESS, REASON_RESTORED, REASON_USER_PAUSE, continuation_prompt, format_goal_age,
    initial_prompt, normalize_objective, objective_edited_prompt, unix_now,
};
use crate::domain::mode::{
    AutoSeat, BrainstormCard, CollabMode, ModeStatusSummary, ModesConfig, ReviewVerdict,
    ReviewVerdictKind, TerminalMode, apply_mode_overrides, auto_seat_parent, clear_mode_overrides,
    format_mode_binding, merge_modes, mode_binding_is_error, mode_overrides_for,
    prune_empty_mode_overrides, resolve_mode_roles, resolve_plan_auto_execute,
    resolve_plan_builder, resolve_plan_reviewer, resolve_team_allow_add_members,
    resolve_team_coordinator, resolve_team_limits, validate_mode_overrides, validate_terminal_mode,
};
use crate::domain::model_directory::{ModelDirectory, ResolvedModel, effort_refusal};
use crate::domain::team::{
    BackendKind, DefaultTarget, Effort, MAX_SUB_AGENTS, MemberId, SessionPolicy, TeamConfig,
    TeamMember, normalize_member_id,
};
use crate::fs_safety;
use crate::router::{self, RelayDecision, RelayGuard, parse_agent_output};
use crate::runtime::context_recap::{self, NOT_CARRIED_OVER, Recap, SYSTEM_SENDER};
use crate::runtime::mode_prompts::{
    HANDBACK_MARKER, SUB_AGENT_TASK_MARKER, brainstorm_build_prompt, brainstorm_propose_prompt,
    brainstorm_stretch_prompt, brainstorm_synthesis_prompt, brainstorm_vote_prompt,
    plan_iteration_prompt, plan_nudge_prompt, plan_plan_prompt, plan_progress_prompt,
    plan_review_prompt, plan_step_nudge_prompt, plan_verify_failure_prompt,
    review_iteration_prompt, review_prompt, review_task_prompt, step_dispatch_prompt,
    verdict_nudge_prompt, verify_failure_prompt,
};
use crate::runtime::session_registry::SessionRegistry;
use crate::state_paths;
use crate::store::sqlite::SqliteStore;

pub(super) const MAX_IMPORTED_ITEMS: usize = 1_000;
pub(super) const MAX_IMPORTED_ITEM_BYTES: usize = 1024 * 1024;
pub(super) const MAX_IMPORTED_TOTAL_BYTES: usize = 8 * 1024 * 1024;

/// What the core wants the transport layer to do after handling an input.
#[derive(Default)]
pub struct RuntimeStep {
    pub events: Vec<RuntimeEvent>,
    pub actions: Vec<RunAction>,
    pub verify_actions: Vec<VerifyAction>,
    pub runner_changes: Vec<RunnerChange>,
    pub runner_controls: Vec<RunnerControl>,
    pub persist_team: Option<TeamConfig>,
}

/// A runner map mutation requested after a live roster edit.
pub enum RunnerChange {
    Upsert {
        member: Box<TeamMember>,
        workspace: PathBuf,
    },
    Remove(MemberId),
}

/// A control message for a live backend runner. These are delivered by the
/// transport outside the pure runtime core, after the corresponding decision
/// has been durably recorded.
pub enum RunnerControl {
    ResolveNativeApproval {
        member: MemberId,
        request_id: u64,
        decision: ApprovalDecision,
    },
}

/// A run the transport layer should start for a member.
pub struct RunAction {
    pub member: MemberId,
    pub prompt: String,
    pub session: Option<AgentSessionId>,
    pub cancel: Arc<AtomicBool>,
    pub effort: Option<Effort>,
}

/// A verification command the transport layer should run outside the core loop.
pub struct VerifyAction {
    pub run_id: RunId,
    pub command: String,
    pub workspace: PathBuf,
    pub cancel: Arc<AtomicBool>,
}

/// Result of a completed verification command.
pub struct VerifyOutput {
    pub run_id: RunId,
    pub command: String,
    pub ok: bool,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub start_error: Option<String>,
    pub cancelled: bool,
}

struct RunningState {
    cancel: Arc<AtomicBool>,
    turn: TurnId,
    message: Option<MessageId>,
    text: String,
    reasoning: String,
    reasoning_started: Option<Instant>,
    failed: bool,
    permission_denied: bool,
    denied_reason: Option<String>,
    permission_noticed: bool,
    raw_persistence_failed: bool,
    /// The member whose message started this run; `None` for the user and for
    /// engine dispatch. A sub-agent's reply goes back to its parent only when
    /// the parent is the origin.
    origin: Option<MemberId>,
    /// The member already sent its origin a message of its own this run.
    messaged_origin: bool,
    /// The latest non-empty visible reply of this run.
    last_reply: String,
    /// What went wrong, when the backend reported a failure.
    error: Option<String>,
}

struct QueuedPrompt {
    turn: TurnId,
    prompt: String,
    origin: Option<MemberId>,
}

struct PendingUserMessage {
    targets: Vec<MemberId>,
    body: String,
    persisted: bool,
}

struct MemberState {
    status: MemberStatus,
    queue: VecDeque<QueuedPrompt>,
    running: Option<RunningState>,
    tools: HashMap<String, ActiveTool>,
    effort: Option<Effort>,
}

struct ActiveTool {
    name: String,
    summary: String,
    detail: String,
}

impl MemberState {
    fn new(effort: Option<Effort>) -> Self {
        Self {
            status: MemberStatus::Idle,
            queue: VecDeque::new(),
            running: None,
            tools: HashMap::new(),
            effort,
        }
    }
}

struct PausedRoute {
    turn: TurnId,
    from: MemberId,
    to_members: Vec<MemberId>,
    to_labels: Vec<String>,
    prompt: String,
}

struct HeldApproval {
    turn: TurnId,
    targets: Vec<MemberId>,
    /// The prompt actually enqueued on approve (relay-wrapped for relays).
    prompt: String,
    /// The mode run to block if this dispatch is rejected (set by the M3 engine).
    mode_run: Option<RunId>,
    /// Agent-originated roster mutation to apply only after explicit approval.
    member_request: Option<(MemberId, TeamMember)>,
}

/// An approval emitted by a live backend while a turn is already running.
/// Unlike [`HeldApproval`], approving it resumes the same runner instead of
/// enqueueing a new prompt.
struct NativeApproval {
    member: MemberId,
    request_id: u64,
    turn: TurnId,
}

pub struct TeamRuntime {
    config: TeamConfig,
    store: SqliteStore,
    relay: RelayGuard,
    sessions: SessionRegistry,
    members: HashMap<MemberId, MemberState>,
    relay_paused: bool,
    paused_routes: VecDeque<PausedRoute>,
    held_approvals: HashMap<ApprovalId, HeldApproval>,
    native_approvals: HashMap<ApprovalId, NativeApproval>,
    /// Direct user messages waiting for their first actual member dispatch.
    pending_user_messages: HashMap<TurnId, PendingUserMessage>,
    run_turns: HashMap<TurnId, RunId>,
    failed_runs: HashSet<RunId>,
    mode_sessions: HashMap<RunId, ModeSession>,
    /// Selection for subsequent messages in the current chat. `/new` resets it
    /// to normal; another `/mode` selection replaces it within the chat.
    active_mode: TerminalMode,
    /// Field-level conversation overlay on `config.modes`.
    session_mode_overrides: ModesConfig,
    last_user: Option<(MessageTarget, String)>,
    next_message_id: u64,
    /// When false (default), Codex tool asks are approved automatically.
    manual_approvals: bool,
    startup_notices: Vec<String>,
    startup_events: Vec<RuntimeEvent>,
    startup_reconciled: bool,
    /// The persistent goal of this chat's main agent (`/goal`).
    goal: Option<GoalState>,
    /// Turns the goal's member took part in, for the continuation loop.
    goal_turns: HashMap<TurnId, GoalTurn>,
    /// The models the installed backends offer, as the UI last reported them.
    models: ModelDirectory,
    /// Members whose runner still carries an older protocol text because they
    /// were busy when the model list changed; refreshed when their next run
    /// starts.
    protocol_stale: HashSet<MemberId>,
    /// Where the live model list is written (`<asterline home>/skills/models.md`).
    models_path: PathBuf,
    /// The team instructions each member's native session was last given in
    /// this app run (at thread start or by a note), for backends that fix
    /// them when a session starts. Empty after launch: a resumed session's
    /// instructions are unknown until the first prompt tells it.
    session_protocols: HashMap<MemberId, String>,
    /// Sub-agents being removed that still have a run winding down; each is
    /// taken off the roster when that run exits.
    closing: HashMap<MemberId, CloseOrigin>,
}

impl TeamRuntime {
    pub fn new(config: TeamConfig, store: SqliteStore) -> Self {
        let mut startup_notices = Vec::new();
        if let Err(err) = store.upsert_team(&config) {
            startup_notices.push(format!("could not save the initial team: {err}"));
        }
        // Bind to the latest conversation so records and replay agree.
        match store.current_conversation() {
            Ok(conversation) => {
                if let Err(err) = store.set_conversation(conversation) {
                    startup_notices.push(format!("could not select the current chat: {err}"));
                }
            }
            Err(err) => {
                startup_notices.push(format!("could not load the current chat: {err}"));
            }
        }
        match store.reject_pending_approvals_for_active_conversation() {
            Ok(0) => {}
            Ok(count) => startup_notices.push(format!(
                "rejected {count} approval request(s) interrupted by restart"
            )),
            Err(err) => startup_notices.push(format!(
                "could not reject approval requests interrupted by restart: {err}"
            )),
        }
        let (active_mode, session_mode_overrides) =
            match store.conversation_snapshot(store.active_conversation()) {
                Ok(Some(snapshot)) => (snapshot.mode, snapshot.mode_overrides),
                Ok(None) => (TerminalMode::Normal, ModesConfig::default()),
                Err(err) => {
                    startup_notices.push(format!("could not restore the current chat mode: {err}"));
                    (TerminalMode::Normal, ModesConfig::default())
                }
            };
        // In-flight mode runs cannot be resumed losslessly across process restarts.
        let mut startup_reconciled = true;
        match store.running_mode_runs() {
            Ok(ids) => {
                for id in ids {
                    if let Err(err) = store.block_run(id, "interrupted by restart") {
                        startup_reconciled = false;
                        startup_notices
                            .push(format!("could not block interrupted run {id}: {err}"));
                    }
                }
            }
            Err(err) => {
                startup_reconciled = false;
                startup_notices.push(format!("could not inspect interrupted runs: {err}"));
            }
        }
        let mut sessions = SessionRegistry::from_store(&store, &config.members);
        // Sessions saved before watermarks existed are taken to have seen the
        // chat as it is now; a configured session that is new to the member
        // carries its own history, so the chat so far counts as seen too.
        let latest = store.latest_message_id().unwrap_or(0);
        let mut watermarks_changed = sessions.resolve_unknown_seen(latest);
        for member in &config.members {
            if let Some(id) = &member.session_id {
                let session = AgentSessionId(id.clone());
                if sessions.get(&member.id).as_ref() == Some(&session) {
                    continue;
                }
                sessions.set_external(member.id.clone(), session.clone(), latest);
                watermarks_changed = true;
                if let Err(err) = store.upsert_session(&member.id, member.backend, &session) {
                    startup_notices.push(format!(
                        "could not save configured session for {}: {err}",
                        member.id
                    ));
                }
            }
        }
        if watermarks_changed
            && let Err(err) = store.save_session_rows(&sessions.to_stored(&config.members))
        {
            startup_notices.push(format!("could not save session watermarks: {err}"));
        }
        let members = config
            .members
            .iter()
            .map(|m| (m.id.clone(), MemberState::new(m.effort)))
            .collect();
        let relay = RelayGuard::new(config.max_auto_relays);
        let manual_approvals = config.approvals.manual;
        let runtime = Self {
            config,
            store,
            relay,
            sessions,
            members,
            relay_paused: false,
            paused_routes: VecDeque::new(),
            held_approvals: HashMap::new(),
            native_approvals: HashMap::new(),
            pending_user_messages: HashMap::new(),
            run_turns: HashMap::new(),
            failed_runs: HashSet::new(),
            mode_sessions: HashMap::new(),
            active_mode,
            session_mode_overrides,
            last_user: None,
            next_message_id: 0,
            manual_approvals,
            startup_notices,
            startup_events: Vec::new(),
            startup_reconciled,
            goal: None,
            goal_turns: HashMap::new(),
            models: ModelDirectory::default(),
            protocol_stale: HashSet::new(),
            models_path: state_paths::models_path(),
            session_protocols: HashMap::new(),
            closing: HashMap::new(),
        };
        let mut runtime = runtime;
        if let Err(err) = runtime.restore_goal() {
            runtime
                .startup_notices
                .push(format!("could not restore the chat's goal: {err}"));
        }
        if let Err(err) = runtime.persist_conversation_snapshot() {
            runtime
                .startup_notices
                .push(format!("could not save the initial chat snapshot: {err}"));
        }
        if let Err(err) = runtime.write_roster_snapshot() {
            runtime
                .startup_notices
                .push(format!("could not write the team roster: {err}"));
        }
        runtime.sync_resume_sessions();
        runtime
    }

    /// Enable the composer approval card for Codex tool asks. Off by default.
    pub fn with_approvals(self, manual: bool) -> Self {
        self.with_manual_approvals(manual)
    }

    pub fn with_manual_approvals(mut self, enabled: bool) -> Self {
        self.manual_approvals = enabled;
        self
    }

    pub fn active_mode(&self) -> TerminalMode {
        self.active_mode
    }

    fn effective_config(&self) -> TeamConfig {
        apply_mode_overrides(&self.config, &self.session_mode_overrides)
    }

    /// Conversation-local `run-N` handle. Notices and chat must use this, not
    /// the raw SQLite id, or `/new` makes the footer and transcript disagree.
    fn run_label(&self, id: RunId) -> String {
        self.store
            .run(id)
            .map(|run| run.label())
            .unwrap_or_else(|_| format!("run-{}", id.0))
    }

    fn emit_modes_updated(&self) -> RuntimeEvent {
        RuntimeEvent::ModesUpdated {
            defaults: self.config.modes.clone(),
            overrides: self.session_mode_overrides.clone(),
        }
    }

    fn sync_resume_sessions(&mut self) {
        let existing = match self.store.replay_chat() {
            Ok(chat) => chat
                .iter()
                .filter_map(chat_item_fingerprint)
                .collect::<HashSet<_>>(),
            Err(err) => {
                self.startup_notices.push(format!(
                    "could not compare native sessions with chat: {err}"
                ));
                return;
            }
        };
        let members = self.config.members.clone();
        for member in members {
            if member.session_policy != SessionPolicy::Resume {
                continue;
            }
            let Some(session) = self.sessions.get(&member.id) else {
                continue;
            };
            let native = native_session_messages(
                member.backend,
                session.as_str(),
                &member.resolved_cwd(&self.config.workspace),
            );
            let cursor = match self.store.native_import_cursor(&member.id, &session) {
                Ok(cursor) => cursor.min(native.len()),
                Err(err) => {
                    self.startup_notices.push(format!(
                        "could not read the native import cursor for {}: {err}",
                        member.id
                    ));
                    continue;
                }
            };
            let incoming = native
                .iter()
                .skip(cursor)
                .filter(|item| !existing.contains(&import_fingerprint(&item.text)))
                .cloned()
                .collect::<Vec<_>>();
            if !incoming.is_empty() {
                let mut step = RuntimeStep::default();
                self.handle_import_transcript(member.id.clone(), incoming, &mut step);
                self.startup_events.extend(step.events);
            }
            if let Err(err) =
                self.store
                    .set_native_import_cursor(&member.id, &session, native.len())
            {
                self.startup_notices.push(format!(
                    "could not save the native import cursor for {}: {err}",
                    member.id
                ));
            }
        }
    }

    fn refresh_native_import_cursor(&self, member: &MemberId) {
        let Some(cfg) = self.config.member(member) else {
            return;
        };
        if cfg.session_policy != SessionPolicy::Resume {
            return;
        }
        let Some(session) = self.sessions.get(member) else {
            return;
        };
        let count = native_session_messages(
            cfg.backend,
            session.as_str(),
            &cfg.resolved_cwd(&self.config.workspace),
        )
        .len();
        let _ = self.store.set_native_import_cursor(member, &session, count);
    }

    pub fn take_startup_events(&mut self) -> Vec<RuntimeEvent> {
        let mut events = std::mem::take(&mut self.startup_notices)
            .into_iter()
            .map(RuntimeEvent::Notice)
            .collect::<Vec<_>>();
        events.extend(std::mem::take(&mut self.startup_events));
        events
    }

    /// Snapshot for the TUI's initial `Ready` event.
    pub fn ready_event(&self) -> RuntimeEvent {
        let members = self
            .config
            .members
            .iter()
            .map(|m| MemberSummary {
                id: m.id.clone(),
                display_name: m.display_name.clone(),
                backend: m.backend,
                role: m.role.clone(),
                status: self
                    .members
                    .get(&m.id)
                    .map(|s| s.status)
                    .unwrap_or(MemberStatus::Idle),
                session: self.sessions.get(&m.id).map(|s| s.0.clone()),
                cwd: m.resolved_cwd(&self.config.workspace).display().to_string(),
                model: m.model.clone(),
                effort: self.members.get(&m.id).and_then(|s| s.effort),
                sandbox: m.sandbox,
                permission_mode: m.permission_mode,
                approvals_reviewer: m.approvals_reviewer,
                session_policy: m.session_policy,
                parent: m.parent.clone(),
            })
            .collect();
        RuntimeEvent::Ready {
            modes: self.config.modes.clone(),
            mode_overrides: self.session_mode_overrides.clone(),
            suggested_verify: None,
            team: self.config.name.clone(),
            workspace: self.config.workspace.display().to_string(),
            default_target: self.config.default_target.clone(),
            members,
            runs: self.store.recent_runs(50).unwrap_or_default(),
        }
    }

    // === command handling ===============================================

    pub fn on_ui_command(&mut self, cmd: UiCommand) -> RuntimeStep {
        let mut step = RuntimeStep::default();
        let requires_reconciled_startup = matches!(
            &cmd,
            UiCommand::UserMessage { .. }
                | UiCommand::Retry
                | UiCommand::NewSession { .. }
                | UiCommand::ResumeConversation { .. }
                | UiCommand::RequestAttach { .. }
                | UiCommand::Approve {
                    decision: ApprovalDecision::Approve,
                    ..
                }
                | UiCommand::ResolvePausedRoute { resume: true }
                | UiCommand::ContinueRun { .. }
                | UiCommand::RunMode { .. }
                | UiCommand::Goal {
                    action: GoalAction::Set(_) | GoalAction::Resume
                }
        );
        if requires_reconciled_startup && !self.startup_reconciled {
            let reason = "cannot change chats or dispatch work because interrupted runs were not reconciled; restart after fixing SQLite persistence"
                .to_string();
            match &cmd {
                UiCommand::RequestAttach { member } => {
                    step.events.push(RuntimeEvent::AttachDenied {
                        member: member.clone(),
                        reason,
                    });
                }
                _ => step.events.push(RuntimeEvent::Notice(reason)),
            }
            return step;
        }
        match cmd {
            UiCommand::SetMode { mode } => {
                let previous = self.active_mode;
                self.active_mode = mode;
                if let Err(err) = self.persist_conversation_snapshot() {
                    self.active_mode = previous;
                    step.events.push(RuntimeEvent::Notice(format!(
                        "could not save terminal mode: {err}"
                    )));
                    return step;
                }
                self.goal_pause_for_mode(mode, &mut step);
                step.events.push(RuntimeEvent::ModeChanged { mode });
                step.events
                    .push(RuntimeEvent::Notice(self.mode_switch_notice(mode)));
            }
            UiCommand::SetModeOverrides { overrides } => {
                self.handle_set_mode_overrides(overrides, &mut step);
            }
            UiCommand::SaveModeDefaults { mode } => {
                self.handle_save_mode_defaults(mode, &mut step);
            }
            UiCommand::UserMessage { target, body } => {
                self.handle_active_user_message(target, body, &mut step);
            }
            UiCommand::Cancel { member } => {
                // Pause first so the cancelled turn does not continue the goal.
                self.goal_pause_for_interrupt(member.as_ref(), &mut step);
                self.handle_cancel(member, &mut step)
            }
            UiCommand::Goal { action } => self.handle_goal_command(action, &mut step),
            UiCommand::SetModelDirectory(models) => self.handle_model_directory(models, &mut step),
            UiCommand::EditQueuedPrompt { member } => {
                self.handle_edit_queued_prompt(member, &mut step);
            }
            UiCommand::Retry => {
                if let Some((target, body)) = self.last_user.clone() {
                    self.handle_active_user_message(target, body, &mut step);
                } else {
                    step.events
                        .push(RuntimeEvent::Notice("nothing to retry".to_string()));
                }
            }
            UiCommand::Approve { id, decision } => self.handle_approval(id, decision, &mut step),
            UiCommand::SetRelayPaused(paused) => {
                self.relay_paused = paused;
                step.events.push(RuntimeEvent::Notice(if paused {
                    "automatic agent-to-agent relay paused".to_string()
                } else {
                    "automatic agent-to-agent relay resumed".to_string()
                }));
            }
            UiCommand::ResolvePausedRoute { resume } => {
                self.resolve_next_paused_route(resume, &mut step)
            }
            UiCommand::ReplaceTeam {
                members,
                default_target,
            } => {
                let members = self.with_existing_sub_agents(members);
                self.handle_replace_team(members, default_target, &mut step)
            }
            UiCommand::NewSession { prompt } => {
                // The prompt goes out only once the reset has succeeded, so it
                // can never land in the chat being left.
                if self.handle_new_session(&mut step)
                    && let Some(prompt) = prompt
                {
                    let target = self.main_message_target();
                    self.handle_active_user_message(target, prompt, &mut step);
                }
            }
            UiCommand::RequestSavedChats => self.handle_request_saved_chats(&mut step),
            UiCommand::RemoveSubAgent { member } => {
                self.handle_remove_sub_agent(&member, &mut step);
            }
            UiCommand::DeleteConversation { conversation } => {
                self.handle_delete_conversation(conversation, &mut step)
            }
            UiCommand::RenameConversation { conversation, name } => {
                self.handle_rename_conversation(conversation, name, &mut step)
            }
            UiCommand::ResumeConversation { conversation } => {
                self.handle_resume_conversation(conversation, &mut step)
            }
            // Transcript import is valid only through the transport's matching
            // AttachFinished reservation. Keep the public legacy command from
            // bypassing that trust boundary when the core is used directly.
            UiCommand::ImportTranscript { .. } => step.events.push(RuntimeEvent::Notice(
                "ignored direct transcript import without an attach reservation".to_string(),
            )),
            UiCommand::ImportSession { member, session_id } => {
                self.handle_import_session(member, session_id, &mut step)
            }
            UiCommand::ExportSession { format } => self.handle_export_session(format, &mut step),
            UiCommand::RequestAttach { member } => self.handle_request_attach(member, &mut step),
            // The transport owns the attach reservation. A stray completion
            // delivered directly to the synchronous core is intentionally a
            // no-op.
            UiCommand::AttachFinished { .. } => {}
            UiCommand::ContinueRun { run_id, note } => {
                self.handle_continue_run(run_id, note, &mut step)
            }
            UiCommand::NoteRun { run_id, note } => self.handle_note_run(run_id, note, &mut step),
            UiCommand::BlockRun { run_id, reason } => {
                self.handle_block_run(run_id, reason, &mut step)
            }
            UiCommand::AddRunStep {
                run_id,
                owner,
                title,
            } => self.handle_add_run_step(run_id, owner, title, &mut step),
            UiCommand::UpdateRunStep {
                run_id,
                step: step_number,
                status,
                note,
            } => self.handle_update_run_step(run_id, step_number, status, note, &mut step),
            UiCommand::RenameRunStep {
                run_id,
                step: step_number,
                title,
            } => self.handle_rename_run_step(run_id, step_number, title, &mut step),
            UiCommand::RemoveRunStep {
                run_id,
                step: step_number,
            } => self.handle_remove_run_step(run_id, step_number, &mut step),
            UiCommand::AssignRunStep {
                run_id,
                step: step_number,
                owner,
            } => self.handle_assign_run_step(run_id, step_number, owner, &mut step),
            UiCommand::RunMode { mode, task } => self.handle_run_mode(mode, task, &mut step),
            UiCommand::Shutdown => self.handle_cancel(None, &mut step),
        }
        step
    }

    /// Import the transcript owned by a transport-validated attach completion.
    /// The transport calls this only while the matching reservation is held.
    pub(super) fn import_attached_transcript(
        &mut self,
        member: MemberId,
        session: Option<AgentSessionId>,
        items: Vec<ImportedMessage>,
    ) -> RuntimeStep {
        let mut step = RuntimeStep::default();
        if let Some(session) = session {
            self.record_member_session(&member, session, &mut step);
        }
        self.handle_import_transcript(member, items, &mut step);
        step
    }

    /// The one-line transcript notice for a mode switch: what the mode does,
    /// or, when this roster cannot run it, why in plain words.
    fn mode_switch_notice(&self, mode: TerminalMode) -> String {
        if mode == TerminalMode::Normal {
            return "Mode: normal — plain text goes to the agent you're viewing".to_string();
        }
        let config = self.effective_config();
        let binding = format_mode_binding(&config, mode);
        if mode_binding_is_error(&config, mode) {
            format!("Mode: {mode} — can't run with this roster yet: {binding}")
        } else {
            format!("Mode: {mode} — next plain text starts a run ({binding})")
        }
    }

    fn handle_set_mode_overrides(&mut self, overrides: ModesConfig, step: &mut RuntimeStep) {
        let mut overrides = overrides;
        prune_empty_mode_overrides(&mut overrides);
        if let Err(err) = validate_mode_overrides(&self.config, &overrides) {
            step.events.push(RuntimeEvent::Notice(err));
            return;
        }
        let previous = self.session_mode_overrides.clone();
        self.session_mode_overrides = overrides;
        if let Err(err) = self.persist_conversation_snapshot() {
            self.session_mode_overrides = previous;
            step.events.push(RuntimeEvent::Notice(format!(
                "could not save mode settings: {err}"
            )));
            return;
        }
        step.events.push(self.emit_modes_updated());
    }

    fn handle_save_mode_defaults(&mut self, mode: TerminalMode, step: &mut RuntimeStep) {
        if matches!(mode, TerminalMode::Normal) {
            step.events.push(RuntimeEvent::Notice(
                "normal has no team.json defaults to save".to_string(),
            ));
            return;
        }
        let extracted = mode_overrides_for(&self.session_mode_overrides, mode);
        if extracted.is_default() {
            step.events.push(RuntimeEvent::Notice(format!(
                "no this-chat overrides to save for {mode}"
            )));
            return;
        }
        let mut next_modes = merge_modes(&self.config.modes, &extracted);
        prune_empty_mode_overrides(&mut next_modes);
        let mut check = self.config.clone();
        check.modes = next_modes.clone();
        if let Err(err) = validate_terminal_mode(&check, mode) {
            step.events.push(RuntimeEvent::Notice(err));
            return;
        }

        let previous_modes = self.config.modes.clone();
        let previous_overrides = self.session_mode_overrides.clone();
        self.config.modes = next_modes;
        clear_mode_overrides(&mut self.session_mode_overrides, mode);
        prune_empty_mode_overrides(&mut self.session_mode_overrides);
        if let Err(err) = self.persist_conversation_snapshot() {
            self.config.modes = previous_modes;
            self.session_mode_overrides = previous_overrides;
            step.events.push(RuntimeEvent::Notice(format!(
                "could not save mode defaults: {err}"
            )));
            return;
        }
        step.persist_team = Some(strip_team_protocols(self.config.without_sub_agents()));
        step.events.push(self.emit_modes_updated());
        step.events.push(RuntimeEvent::Notice(format!(
            "saved {mode} defaults to team.json"
        )));
    }

    fn handle_active_user_message(
        &mut self,
        target: MessageTarget,
        body: String,
        step: &mut RuntimeStep,
    ) {
        self.last_user = Some((target.clone(), body.clone()));
        // Plain text starts the selected mode. An explicit @member / @all is
        // always a direct chat, even after a team run has finished — team
        // runs do not use ModeSession, so "run is active" is the wrong gate.
        if !matches!(self.active_mode, TerminalMode::Normal)
            && !matches!(target, MessageTarget::Default)
        {
            self.handle_user_message(target, body, step);
            return;
        }
        let task = strip_routing_prefix(&body);
        match self.active_mode {
            TerminalMode::Normal => {
                self.handle_user_message(target, body, step);
            }
            TerminalMode::Team => self.handle_run_team(task, step),
            mode => self.handle_run_mode(
                mode.collab_mode().expect("collaboration terminal mode"),
                task,
                step,
            ),
        }
    }

    fn handle_user_message(
        &mut self,
        target: MessageTarget,
        body: String,
        step: &mut RuntimeStep,
    ) -> Option<TurnId> {
        self.last_user = Some((target.clone(), body.clone()));
        self.dispatch_user_turn(target, body.clone(), body, step)
    }

    /// Start a user turn: `body` is what the chat shows as the user's message
    /// and `prompt` is what the members receive (they differ for a goal).
    fn dispatch_user_turn(
        &mut self,
        target: MessageTarget,
        body: String,
        prompt: String,
        step: &mut RuntimeStep,
    ) -> Option<TurnId> {
        let (targets, unknown) = self.resolve_message_target(&target);
        for name in unknown {
            step.events
                .push(RuntimeEvent::Notice(format!("unknown member: {name}")));
        }
        if targets.is_empty() {
            step.events.push(RuntimeEvent::Notice(
                "no matching member for message".to_string(),
            ));
            return None;
        }

        let turn = match self.store.create_turn() {
            Ok(turn) => turn,
            Err(err) => {
                step.events
                    .push(RuntimeEvent::Notice(format!("store error: {err}")));
                return None;
            }
        };
        let starts_immediately = targets.iter().any(|member| {
            self.members
                .get(member)
                .is_some_and(|state| state.running.is_none())
        });
        let defer_persistence = !starts_immediately;
        if !defer_persistence && let Err(err) = self.store.record_user(turn, &targets, &body) {
            self.report_store_error("save the user message", err, step);
            return None;
        }
        step.events.push(RuntimeEvent::TurnStarted { turn });

        let targets_str: Vec<String> = targets.iter().map(|t| t.to_string()).collect();
        if let Some(first_target) = targets.first() {
            self.log(
                first_target,
                LogEntry::info("user", format!("→ {}: {}", targets_str.join(", "), body)),
                step,
            );
        }

        self.pending_user_messages.insert(
            turn,
            PendingUserMessage {
                targets: targets.clone(),
                body: body.clone(),
                persisted: !defer_persistence,
            },
        );
        for member in targets {
            self.enqueue_prompt(&member, turn, prompt.clone(), step);
        }
        Some(turn)
    }

    fn resolve_message_target(&self, target: &MessageTarget) -> (Vec<MemberId>, Vec<String>) {
        match target {
            MessageTarget::Default => (self.config.default_member_ids(), Vec::new()),
            MessageTarget::All => (self.config.all_member_ids(), Vec::new()),
            MessageTarget::Member(id) => self.resolve_named(std::slice::from_ref(id)),
            MessageTarget::Members(ids) => self.resolve_named(ids),
        }
    }

    fn resolve_named(&self, ids: &[MemberId]) -> (Vec<MemberId>, Vec<String>) {
        let mut resolved = Vec::new();
        let mut unknown = Vec::new();
        for id in ids {
            match self.config.find(id.as_str()) {
                Some(member) if !resolved.contains(&member.id) => resolved.push(member.id.clone()),
                Some(_) => {}
                None => unknown.push(id.to_string()),
            }
        }
        (resolved, unknown)
    }

    fn handle_cancel(&mut self, member: Option<MemberId>, step: &mut RuntimeStep) {
        let mut cancelled_approval_turns = Vec::new();
        let mut cancelled_route_turns = HashSet::new();
        let native_approval_ids: Vec<ApprovalId> = self
            .native_approvals
            .iter()
            .filter(|(_, held)| member.as_ref().is_none_or(|target| target == &held.member))
            .map(|(id, _)| *id)
            .collect();
        for id in native_approval_ids {
            let Some(held) = self.native_approvals.remove(&id) else {
                continue;
            };
            if let Err(err) = self.store.resolve_approval(id, ApprovalDecision::Reject) {
                self.report_store_error("cancel a native approval", err, step);
            }
            step.events.push(RuntimeEvent::ApprovalResolved {
                id,
                decision: ApprovalDecision::Reject,
            });
            step.runner_controls
                .push(RunnerControl::ResolveNativeApproval {
                    member: held.member,
                    request_id: held.request_id,
                    decision: ApprovalDecision::Reject,
                });
        }
        let targets: Vec<MemberId> = match member {
            Some(m) => vec![m],
            None => {
                self.block_all_mode_sessions("aborted by user", step);
                let team_runs: HashSet<RunId> = self
                    .run_turns
                    .values()
                    .copied()
                    .filter(|run_id| {
                        self.store.run(*run_id).is_ok_and(|run| {
                            run.mode
                                .as_ref()
                                .is_some_and(|mode| mode.mode == CollabMode::Team)
                        })
                    })
                    .collect();
                for run_id in team_runs {
                    self.block_mode_run(run_id, "aborted by user", step);
                }
                cancelled_route_turns.extend(self.paused_routes.drain(..).map(|route| route.turn));
                step.events
                    .push(RuntimeEvent::RouteQueueUpdated { queued: 0 });
                for (id, held) in std::mem::take(&mut self.held_approvals) {
                    cancelled_approval_turns.push(held.turn);
                    if let Err(err) = self.store.resolve_approval(id, ApprovalDecision::Reject) {
                        self.report_store_error("cancel a pending approval", err, step);
                    }
                    step.events.push(RuntimeEvent::ApprovalResolved {
                        id,
                        decision: ApprovalDecision::Reject,
                    });
                }
                self.members.keys().cloned().collect()
            }
        };
        let mut roster_changed = false;
        for member in targets {
            let start_queued = if let Some(state) = self.members.get_mut(&member) {
                // Keep queued prompts. Esc interrupts the live run so the
                // next queued message can start as soon as the child exits.
                if let Some(running) = &state.running {
                    running.cancel.store(true, Ordering::Relaxed);
                    let queued = state.queue.len();
                    step.events.push(RuntimeEvent::MemberNotice {
                        member: member.clone(),
                        text: if queued == 0 {
                            format!("cancelling {member}")
                        } else {
                            format!(
                                "cancelling {member} · {queued} queued message(s) will send next"
                            )
                        },
                    });
                    None
                } else if !state.queue.is_empty() {
                    state.queue.pop_front()
                } else if state.status != MemberStatus::Idle {
                    state.status = MemberStatus::Idle;
                    step.events.push(RuntimeEvent::MemberStatus {
                        member: member.clone(),
                        status: MemberStatus::Idle,
                    });
                    roster_changed = true;
                    None
                } else {
                    None
                }
            } else {
                None
            };
            if let Some(queued) = start_queued {
                self.emit_queue_updated(&member, step);
                self.start_run(&member, queued.turn, queued.prompt, queued.origin, step);
            }
        }
        if roster_changed {
            self.note_roster_write(step);
        }
        for turn in cancelled_approval_turns {
            cancelled_route_turns.insert(turn);
        }
        for turn in cancelled_route_turns {
            if !step.events.iter().any(
                |event| matches!(event, RuntimeEvent::TurnFinished { turn: done } if *done == turn),
            ) {
                self.check_turn_complete(turn, step);
            }
        }
    }

    /// Where `/new <prompt>` sends its prompt: the main member (else the first
    /// top-level member), or the team's default target when neither exists.
    fn main_message_target(&self) -> MessageTarget {
        self.config
            .members
            .iter()
            .find(|member| member.id.as_str() == MAIN_MEMBER_ID)
            .or_else(|| self.config.members.iter().find(|m| m.parent.is_none()))
            .map_or(MessageTarget::Default, |member| {
                MessageTarget::Member(member.id.clone())
            })
    }

    /// Start a fresh chat. Returns whether it did; a refusal has already
    /// pushed its notice.
    fn handle_new_session(&mut self, step: &mut RuntimeStep) -> bool {
        if self.has_active_work() {
            step.events.push(RuntimeEvent::Notice(
                "cannot start a new chat while members or runs are active; press Esc to cancel work first"
                    .to_string(),
            ));
            return false;
        }
        if let Err(err) = self.persist_conversation_snapshot() {
            step.events.push(RuntimeEvent::Notice(format!(
                "could not save the current chat before /new: {err}"
            )));
            return false;
        }
        // A fresh chat: a new conversation (so the transcript starts clean and
        // restart shows only this chat) plus new backend sessions for everyone.
        // Sub-agents belong to the chat being left; the new one starts with
        // the top-level members only.
        let mut snapshot_team = strip_team_protocols(self.config.without_sub_agents());
        for member in &mut snapshot_team.members {
            member.session_id = None;
            member.session_policy = SessionPolicy::Fresh;
        }
        match self.store.create_fresh_conversation(
            &snapshot_team,
            TerminalMode::Normal,
            &self.session_mode_overrides,
        ) {
            Ok(_) => {}
            Err(err) => {
                step.events.push(RuntimeEvent::Notice(format!(
                    "could not start a new chat: {err}"
                )));
                return false;
            }
        }
        let previous_mode = self.active_mode;
        self.active_mode = TerminalMode::Normal;
        self.sessions = SessionRegistry::new();
        let removed_sub_agents: Vec<MemberId> = self
            .config
            .members
            .iter()
            .filter(|member| member.parent.is_some())
            .map(|member| member.id.clone())
            .collect();
        self.config.members.retain(|member| member.parent.is_none());
        for id in &removed_sub_agents {
            self.members.remove(id);
            step.runner_changes.push(RunnerChange::Remove(id.clone()));
        }
        for member in &mut self.config.members {
            member.session_id = None;
            member.session_policy = SessionPolicy::Fresh;
        }
        if !removed_sub_agents.is_empty() {
            // Parents' protocol text lists their sub-agents; refresh it.
            inject_team_protocol_with(&mut self.config, &self.models);
            if let Err(err) = self.store.upsert_team(&self.config) {
                self.report_store_error("update the saved roster", err, step);
            }
            for member in self.config.members.clone() {
                step.runner_changes.push(RunnerChange::Upsert {
                    member: Box::new(member),
                    workspace: self.config.workspace.clone(),
                });
            }
        }
        for state in self.members.values_mut() {
            state.status = MemberStatus::Idle;
            state.running = None;
            state.tools.clear();
        }
        // Drop any in-flight turn state from the previous chat.
        self.paused_routes.clear();
        self.held_approvals.clear();
        self.last_user = None;
        // The goal belongs to the chat being left; the fresh chat has none.
        self.goal_turns.clear();
        if self.goal.take().is_some() {
            step.events.push(RuntimeEvent::GoalUpdated { goal: None });
        }
        if previous_mode != TerminalMode::Normal {
            step.events.push(RuntimeEvent::ModeChanged {
                mode: TerminalMode::Normal,
            });
        }
        step.events.push(RuntimeEvent::SessionReset);
        if !removed_sub_agents.is_empty() {
            step.events.push(self.ready_event());
        }
        step.events.push(self.emit_modes_updated());
        self.note_roster_write(step);
        true
    }

    fn handle_request_saved_chats(&self, step: &mut RuntimeStep) {
        match self.store.resumable_conversations() {
            Ok(conversations) => step.events.push(RuntimeEvent::SavedChats { conversations }),
            Err(err) => step.events.push(RuntimeEvent::Notice(format!(
                "could not list saved chats: {err}"
            ))),
        }
    }

    /// The event telling the TUI the current chat's name, when it has one
    /// (an unnamed chat is the default, so nothing is sent for it).
    pub fn chat_name_event(&self) -> Option<RuntimeEvent> {
        let name = self
            .store
            .conversation_name(self.store.active_conversation())
            .ok()
            .flatten()?;
        Some(RuntimeEvent::ConversationRenamed { name: Some(name) })
    }

    fn handle_delete_conversation(&mut self, conversation: i64, step: &mut RuntimeStep) {
        if conversation == self.store.active_conversation() {
            step.events.push(RuntimeEvent::Notice(
                "cannot delete the current chat".to_string(),
            ));
            return;
        }
        match self.store.delete_conversation(conversation) {
            Ok(true) => {}
            Ok(false) => step.events.push(RuntimeEvent::Notice(format!(
                "saved chat {conversation} is no longer available"
            ))),
            Err(err) => {
                self.report_store_error("delete the saved chat", err, step);
                return;
            }
        }
        self.handle_request_saved_chats(step);
    }

    fn handle_rename_conversation(
        &mut self,
        conversation: Option<i64>,
        name: Option<String>,
        step: &mut RuntimeStep,
    ) {
        let current = self.store.active_conversation();
        let target = conversation.unwrap_or(current);
        let name = name
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty());
        match self.store.set_conversation_name(target, name.as_deref()) {
            Ok(true) => {}
            Ok(false) => {
                step.events.push(RuntimeEvent::Notice(format!(
                    "saved chat {target} is no longer available"
                )));
                return;
            }
            Err(err) => {
                self.report_store_error("rename the chat", err, step);
                return;
            }
        }
        if target == current {
            step.events
                .push(RuntimeEvent::ConversationRenamed { name: name.clone() });
            if conversation.is_none() {
                step.events.push(RuntimeEvent::Notice(match &name {
                    Some(name) => format!("named this chat \"{name}\""),
                    None => "cleared this chat's name".to_string(),
                }));
            }
        }
        self.handle_request_saved_chats(step);
    }

    fn handle_resume_conversation(&mut self, conversation: i64, step: &mut RuntimeStep) {
        if self.has_active_work() {
            step.events.push(RuntimeEvent::Notice(
                "cannot resume another chat while members or runs are active; press Esc to cancel work first"
                    .to_string(),
            ));
            return;
        }

        if let Err(err) = self.persist_conversation_snapshot() {
            step.events.push(RuntimeEvent::Notice(format!(
                "could not save the current chat before /resume: {err}"
            )));
            return;
        }
        let snapshot = match self.store.conversation_snapshot(conversation) {
            Ok(Some(snapshot)) => snapshot,
            Ok(None) => {
                step.events.push(RuntimeEvent::Notice(format!(
                    "saved chat {conversation} is no longer available"
                )));
                return;
            }
            Err(err) => {
                step.events.push(RuntimeEvent::Notice(format!(
                    "could not restore saved chat {conversation}: {err}"
                )));
                return;
            }
        };
        let mode = snapshot.mode;
        let mode_overrides = snapshot.mode_overrides;
        let raw_config = strip_team_protocols(snapshot.team);
        if let Err(err) = raw_config.validate() {
            step.events.push(RuntimeEvent::Notice(format!(
                "saved chat {conversation} has an invalid team: {err}"
            )));
            return;
        }
        let chat = match self.store.replay_chat_for(conversation) {
            Ok(chat) => chat,
            Err(err) => {
                step.events.push(RuntimeEvent::Notice(format!(
                    "could not replay saved chat {conversation}: {err}"
                )));
                return;
            }
        };
        let restored_registry =
            SessionRegistry::from_stored_for(&snapshot.sessions, &raw_config.members);
        let restored_sessions = restored_registry.to_stored(&raw_config.members);
        let mut operational_config = raw_config.clone();
        inject_team_protocol_with(&mut operational_config, &self.models);
        let rejected = match self.store.activate_runtime_team_state(
            conversation,
            &operational_config,
            &raw_config,
            &restored_sessions,
            mode,
            &mode_overrides,
        ) {
            Ok(rejected) => rejected,
            Err(err) => {
                self.report_store_error("restore the selected chat atomically", err, step);
                return;
            }
        };
        if rejected > 0 {
            step.events.push(RuntimeEvent::Notice(format!(
                "rejected {rejected} approval request(s) interrupted before this chat was resumed"
            )));
        }

        let old_ids: Vec<MemberId> = self.members.keys().cloned().collect();
        for id in old_ids {
            step.runner_changes.push(RunnerChange::Remove(id));
        }

        self.config = operational_config;
        self.members = self
            .config
            .members
            .iter()
            .map(|member| (member.id.clone(), MemberState::new(member.effort)))
            .collect();
        self.sessions = restored_registry;
        // A chat saved before watermarks existed: its sessions saw all of it.
        if self
            .sessions
            .resolve_unknown_seen(self.store.latest_message_id().unwrap_or(0))
        {
            self.persist_session_rows_or_notice(step);
        }
        self.relay = RelayGuard::new(self.config.max_auto_relays);
        self.paused_routes.clear();
        self.held_approvals.clear();
        self.run_turns.clear();
        self.failed_runs.clear();
        self.mode_sessions.clear();
        self.active_mode = mode;
        self.session_mode_overrides = mode_overrides;
        self.last_user = None;
        if let Err(err) = self.restore_goal() {
            self.goal = None;
            self.report_store_error("restore the chat's goal", err, step);
        }
        for member in self.config.members.clone() {
            step.runner_changes.push(RunnerChange::Upsert {
                member: Box::new(member),
                workspace: self.config.workspace.clone(),
            });
        }
        step.persist_team = Some(raw_config.without_sub_agents());
        let departed = self
            .store
            .departed_members_for(conversation)
            .unwrap_or_default();
        step.events.push(RuntimeEvent::ConversationResumed {
            conversation,
            chat,
            departed,
        });
        step.events.extend(self.chat_name_event());
        step.events.push(self.ready_event());
        step.events.push(RuntimeEvent::ModeChanged { mode });
        step.events.push(RuntimeEvent::GoalUpdated {
            goal: self.goal.clone(),
        });
        self.note_roster_write(step);
    }

    fn persist_conversation_snapshot(&self) -> crate::store::sqlite::Result<()> {
        self.persist_conversation_snapshot_for(&self.config)
    }

    fn persist_conversation_snapshot_for(
        &self,
        config: &TeamConfig,
    ) -> crate::store::sqlite::Result<()> {
        if self.store.active_conversation() <= 0 {
            return Ok(());
        }
        let team = strip_team_protocols(config.clone());
        let sessions = self.sessions.to_stored(&config.members);
        self.store.save_conversation_snapshot(
            &team,
            &sessions,
            self.active_mode,
            &self.session_mode_overrides,
        )
    }

    fn persist_snapshot_or_notice(&self, context: &str, step: &mut RuntimeStep) {
        if let Err(err) = self.persist_conversation_snapshot() {
            self.report_store_error(context, err, step);
        }
    }

    /// Save the session ids with their watermarks and refresh the chat snapshot.
    fn persist_session_rows_or_notice(&self, step: &mut RuntimeStep) {
        if let Err(err) = self
            .store
            .save_session_rows(&self.sessions.to_stored(&self.config.members))
        {
            self.report_store_error("save the session watermarks", err, step);
        }
        self.persist_snapshot_or_notice("save the chat session", step);
    }

    /// Note that the member's native session has now seen the chat up to its
    /// newest message: it took part in everything the member's conversation
    /// holds. The watermark only moves forward and is saved with the session.
    fn advance_seen(&mut self, member: &MemberId, step: &mut RuntimeStep) {
        if self.sessions.get(member).is_none() {
            return;
        }
        let latest = match self.store.latest_message_id() {
            Ok(latest) => latest,
            Err(err) => {
                self.report_store_error("read the chat position", err, step);
                return;
            }
        };
        if self.sessions.seen(member) >= latest {
            return;
        }
        self.sessions.mark_seen(member, latest);
        if let Err(err) = self.store.set_session_seen(member, latest) {
            self.report_store_error("save the session watermark", err, step);
        }
        self.persist_snapshot_or_notice("save the chat session", step);
    }

    /// A recap of the part of `member`'s conversation after `seen`, left out
    /// the rows of `skip_turn` (the message being sent now). `None` when
    /// there is nothing to carry over.
    fn recap_since(
        &self,
        member: &MemberId,
        seen: i64,
        skip_turn: Option<TurnId>,
        config: &TeamConfig,
    ) -> Result<Option<Recap>, rusqlite::Error> {
        let items: Vec<ChatItem> = self
            .store
            .history_after(seen, skip_turn)?
            .into_iter()
            .map(|entry| entry.item)
            .collect();
        Ok(context_recap::build_recap(&items, member, config))
    }

    /// `prompt`, with a recap of what the member's current native session has
    /// not seen in front of it. The recap is for the model only; it is not
    /// stored and not shown.
    fn prompt_with_recap(
        &self,
        member: &MemberId,
        turn: TurnId,
        prompt: String,
        step: &mut RuntimeStep,
    ) -> String {
        let seen = self.sessions.seen(member);
        match self.recap_since(member, seen, Some(turn), &self.config) {
            Ok(Some(recap)) => recap.prepend_to(&prompt),
            Ok(None) => prompt,
            Err(err) => {
                self.report_store_error("read the earlier conversation", err, step);
                prompt
            }
        }
    }

    /// For a backend that fixes its instructions when a native session starts
    /// (Codex), `prompt` with the current team instructions in front of it
    /// when it resumes a session that has not been given them in this app run
    /// (the first prompt after launch) or was given different ones (the model
    /// list or roster changed since). A new session starts with the current
    /// instructions and needs no note.
    fn prompt_with_protocol_note(
        &mut self,
        member: &MemberId,
        resuming: bool,
        prompt: String,
    ) -> String {
        if !backend_fixes_protocol_at_start(self.member_backend(member)) {
            self.session_protocols.remove(member);
            return prompt;
        }
        let Some(protocol) = self
            .config
            .member(member)
            .and_then(|entry| entry.system_prompt.as_deref())
            .and_then(team_protocol_text)
            .map(str::to_string)
        else {
            return prompt;
        };
        let known = self.session_protocols.get(member) == Some(&protocol);
        self.session_protocols
            .insert(member.clone(), protocol.clone());
        if resuming && !known {
            context_recap::with_protocol_note(&protocol, &prompt)
        } else {
            prompt
        }
    }

    fn report_store_error(&self, context: &str, err: rusqlite::Error, step: &mut RuntimeStep) {
        step.events
            .push(RuntimeEvent::Notice(format!("could not {context}: {err}")));
    }

    fn has_active_work(&self) -> bool {
        self.members
            .values()
            .any(|state| state.running.is_some() || !state.queue.is_empty())
            || !self.paused_routes.is_empty()
            || !self.held_approvals.is_empty()
            || !self.native_approvals.is_empty()
            || !self.run_turns.is_empty()
            || !self.mode_sessions.is_empty()
    }

    fn handle_request_attach(&self, member: MemberId, step: &mut RuntimeStep) {
        if self.config.member(&member).is_none() {
            let reason = format!("cannot attach: unknown member {member}");
            step.events
                .push(RuntimeEvent::AttachDenied { member, reason });
            return;
        }
        if self.has_active_work() {
            step.events.push(RuntimeEvent::AttachDenied {
                member,
                reason: "cannot attach while member work, verification, routing, approval, or a run is active; press Esc to cancel it or resolve it first"
                    .to_string(),
            });
            return;
        }
        step.events.push(RuntimeEvent::AttachGranted { member });
    }

    /// Take the model list the UI discovered, and give every member that may
    /// open sub-agents the new list in its protocol text. Idle members get a
    /// fresh runner now; busy ones when their next run starts.
    fn handle_model_directory(&mut self, models: ModelDirectory, step: &mut RuntimeStep) {
        if self.models == models {
            return;
        }
        self.models = models;
        self.write_models_file(step);
        let mut next = self.config.clone();
        inject_team_protocol_with(&mut next, &self.models);
        let changed: Vec<MemberId> = next
            .members
            .iter()
            .filter(|member| {
                self.config
                    .member(&member.id)
                    .is_none_or(|old| old.system_prompt != member.system_prompt)
            })
            .map(|member| member.id.clone())
            .collect();
        if changed.is_empty() {
            return;
        }
        self.config = next;
        if let Err(err) = self.store.upsert_team(&self.config) {
            self.report_store_error("save the refreshed team protocol", err, step);
        }
        for id in changed {
            let busy = self
                .members
                .get(&id)
                .is_some_and(|state| state.running.is_some() || !state.queue.is_empty());
            if busy {
                self.protocol_stale.insert(id);
            } else {
                self.push_runner_upsert(&id, step);
            }
        }
    }

    /// Rewrite the model list file for the current directory. Nothing is
    /// written while no catalog is known, and an unchanged list is left alone.
    fn write_models_file(&self, step: &mut RuntimeStep) {
        let Some(text) = self.models.models_file() else {
            return;
        };
        match state_paths::write_state_file(&self.models_path, "model list", &text) {
            Ok(_) => state_paths::remove_legacy_models_file(),
            Err(err) => step.events.push(RuntimeEvent::Notice(format!(
                "could not update the model list: {err}"
            ))),
        }
    }

    /// Rebuild `member`'s runner from the current roster entry.
    fn push_runner_upsert(&mut self, member: &MemberId, step: &mut RuntimeStep) {
        self.protocol_stale.remove(member);
        if let Some(entry) = self.config.member(member).cloned() {
            step.runner_changes.push(RunnerChange::Upsert {
                member: Box::new(entry),
                workspace: self.config.workspace.clone(),
            });
        }
    }

    fn handle_replace_team(
        &mut self,
        members: Vec<TeamMember>,
        default_target: Option<DefaultTarget>,
        step: &mut RuntimeStep,
    ) {
        self.replace_team(members, default_target, true, step);
    }

    /// Carry the conversation's sub-agents across a roster edit that only
    /// knows the top-level members (the `/team` editor). A sub-agent the edit
    /// does mention (a `/model` change) is taken as given.
    fn with_existing_sub_agents(&self, mut members: Vec<TeamMember>) -> Vec<TeamMember> {
        let mentioned: HashSet<MemberId> = members.iter().map(|member| member.id.clone()).collect();
        let previous = strip_team_protocols(self.config.clone());
        members.extend(
            previous
                .members
                .into_iter()
                .filter(|member| member.parent.is_some() && !mentioned.contains(&member.id)),
        );
        members
    }

    /// Replace the live roster with `members` (top-level members and
    /// sub-agents). A sub-agent whose parent is not a top-level member of the
    /// new roster is dropped with it. Returns whether the roster was applied.
    fn replace_team(
        &mut self,
        members: Vec<TeamMember>,
        default_target: Option<DefaultTarget>,
        announce: bool,
        step: &mut RuntimeStep,
    ) -> bool {
        let mut raw_config = self.config.clone();
        raw_config.members = self.merge_member_config(members);
        let top_level: HashSet<MemberId> = raw_config
            .members
            .iter()
            .filter(|member| member.parent.is_none())
            .map(|member| member.id.clone())
            .collect();
        raw_config.members.retain(|member| {
            member
                .parent
                .as_ref()
                .is_none_or(|parent| top_level.contains(parent))
        });
        raw_config.default_target = default_target.or_else(|| {
            raw_config
                .members
                .first()
                .map(|member| DefaultTarget::Member(member.id.clone()))
        });
        raw_config = strip_team_protocols(raw_config);

        if let Err(err) = raw_config.validate() {
            step.events
                .push(RuntimeEvent::Notice(format!("team update rejected: {err}")));
            return false;
        }

        let previous_raw = strip_team_protocols(self.config.clone());
        let old_ids: HashSet<MemberId> = self.members.keys().cloned().collect();
        let new_ids: HashSet<MemberId> = raw_config.members.iter().map(|m| m.id.clone()).collect();
        for removed in old_ids.difference(&new_ids) {
            if let Some(state) = self.members.get(removed)
                && (state.status != MemberStatus::Idle
                    || state.running.is_some()
                    || !state.queue.is_empty())
            {
                step.events.push(RuntimeEvent::Notice(format!(
                    "cannot remove {removed} while it is active"
                )));
                return false;
            }
        }

        for member in &raw_config.members {
            let changed = previous_raw
                .member(&member.id)
                .is_some_and(|old| old != member);
            let active = self.members.get(&member.id).is_some_and(|state| {
                state.status != MemberStatus::Idle
                    || state.running.is_some()
                    || !state.queue.is_empty()
            });
            if changed && active {
                step.events.push(RuntimeEvent::Notice(format!(
                    "cannot update {} while it is active",
                    member.id
                )));
                return false;
            }
        }

        let old_members: HashMap<MemberId, TeamMember> = self
            .config
            .members
            .iter()
            .cloned()
            .map(|member| (member.id.clone(), member))
            .collect();
        // The sessions after this edit. A CLI switch parks the old CLI's
        // session (it stays resumable if the member switches back) and starts
        // the new CLI with nothing seen; same-CLI model changes keep the
        // session and its watermark, so they carry the conversation as is.
        let latest_message = self.store.latest_message_id().unwrap_or(0);
        let mut next_registry = self.sessions.clone();
        for member in &raw_config.members {
            let Some(old) = old_members.get(&member.id) else {
                continue;
            };
            if old.backend != member.backend {
                next_registry.switch_backend(&member.id, old.backend, member.backend);
            } else if member.session_id.is_none()
                && ((old.session_policy != SessionPolicy::Fresh
                    && member.session_policy == SessionPolicy::Fresh)
                    || old.session_id.is_some())
            {
                next_registry.clear(&member.id);
            }
        }
        for member in &raw_config.members {
            // A session named in the roster carries its own history.
            if let Some(id) = &member.session_id {
                let session = AgentSessionId(id.clone());
                if next_registry.get(&member.id).as_ref() != Some(&session) {
                    next_registry.set_external(member.id.clone(), session, latest_message);
                }
            }
        }

        let removed_ids: Vec<MemberId> = old_ids.difference(&new_ids).cloned().collect();
        let removed_set: HashSet<MemberId> = removed_ids.iter().cloned().collect();
        let approvals_to_reject: Vec<(ApprovalId, TurnId, Option<RunId>)> = self
            .held_approvals
            .iter()
            .filter(|(_, held)| {
                held.targets.iter().any(|id| removed_set.contains(id))
                    || held.member_request.as_ref().is_some_and(|(from, member)| {
                        removed_set.contains(from) || removed_set.contains(&member.id)
                    })
            })
            .map(|(id, held)| (*id, held.turn, held.mode_run))
            .collect();
        let native_to_reject: Vec<ApprovalId> = self
            .native_approvals
            .iter()
            .filter(|(_, held)| removed_set.contains(&held.member))
            .map(|(id, _)| *id)
            .collect();
        let approval_ids: Vec<ApprovalId> = approvals_to_reject
            .iter()
            .map(|(id, _, _)| *id)
            .chain(native_to_reject.iter().copied())
            .collect();
        for removed in &removed_ids {
            next_registry.drop_member(removed);
        }
        // A CLI switch whose recap will have to drop the oldest messages is
        // told to the user with the confirmation below.
        let partial_recap = announce
            && raw_config.members.iter().any(|member| {
                old_members
                    .get(&member.id)
                    .is_some_and(|old| old.backend != member.backend)
                    && self
                        .recap_since(
                            &member.id,
                            next_registry.seen(&member.id),
                            None,
                            &raw_config,
                        )
                        .ok()
                        .flatten()
                        .is_some_and(|recap| recap.truncated)
            });
        let next_sessions = next_registry.to_stored(&raw_config.members);
        let mut operational_config = raw_config.clone();
        inject_team_protocol_with(&mut operational_config, &self.models);
        if let Err(err) = self.store.replace_runtime_team_state(
            &operational_config,
            &raw_config,
            &next_sessions,
            self.active_mode,
            &self.session_mode_overrides,
            &approval_ids,
        ) {
            self.report_store_error("save the updated team atomically", err, step);
            return false;
        }

        let mut cleanup_turns: HashSet<TurnId> = approvals_to_reject
            .iter()
            .map(|(_, turn, _)| *turn)
            .collect();
        let mut affected_runs: HashSet<RunId> = approvals_to_reject
            .iter()
            .filter_map(|(_, _, run_id)| *run_id)
            .collect();
        for (id, _, _) in approvals_to_reject {
            self.held_approvals.remove(&id);
            step.events.push(RuntimeEvent::ApprovalResolved {
                id,
                decision: ApprovalDecision::Reject,
            });
        }
        for id in native_to_reject {
            let Some(held) = self.native_approvals.remove(&id) else {
                continue;
            };
            cleanup_turns.insert(held.turn);
            step.events.push(RuntimeEvent::ApprovalResolved {
                id,
                decision: ApprovalDecision::Reject,
            });
            step.runner_controls
                .push(RunnerControl::ResolveNativeApproval {
                    member: held.member,
                    request_id: held.request_id,
                    decision: ApprovalDecision::Reject,
                });
        }

        self.paused_routes.retain(|route| {
            let keep = !removed_set.contains(&route.from)
                && !route.to_members.iter().any(|id| removed_set.contains(id));
            if !keep {
                cleanup_turns.insert(route.turn);
                if let Some(run_id) = self.run_turns.get(&route.turn) {
                    affected_runs.insert(*run_id);
                }
            }
            keep
        });
        for run_id in affected_runs {
            self.block_mode_run(run_id, "member removed while dispatch was pending", step);
        }

        for id in &removed_ids {
            self.members.remove(id);
            step.runner_changes.push(RunnerChange::Remove(id.clone()));
        }

        self.sessions = next_registry;

        for member in &raw_config.members {
            self.members
                .entry(member.id.clone())
                .or_insert_with(|| MemberState::new(member.effort))
                .effort = member.effort;
        }

        self.config = operational_config;
        self.manual_approvals |= self.config.approvals.manual;
        for member in self.config.members.clone() {
            step.runner_changes.push(RunnerChange::Upsert {
                member: Box::new(member),
                workspace: self.config.workspace.clone(),
            });
        }
        for turn in cleanup_turns {
            self.check_turn_complete(turn, step);
        }
        step.persist_team = Some(raw_config.without_sub_agents());
        step.events.push(self.ready_event());
        if announce {
            let mut text = format!("team updated: {} member(s)", self.config.members.len());
            if partial_recap {
                text.push_str(&format!(" · {NOT_CARRIED_OVER}"));
            }
            step.events.push(RuntimeEvent::Notice(text));
        }
        self.note_roster_write(step);
        true
    }

    fn merge_member_config(&self, members: Vec<TeamMember>) -> Vec<TeamMember> {
        let previous: HashMap<MemberId, TeamMember> = self
            .config
            .members
            .iter()
            .cloned()
            .map(|member| (member.id.clone(), member))
            .collect();
        members
            .into_iter()
            .map(|mut member| {
                if let Some(old) = previous.get(&member.id) {
                    if member.system_prompt.is_none()
                        && let Some(prompt) = &old.system_prompt
                    {
                        let prompt = strip_team_protocol(prompt);
                        if !prompt.trim().is_empty() {
                            member.system_prompt = Some(prompt.trim().to_string());
                        }
                    }
                    if member.allowed_tools.is_empty() && !old.allowed_tools.is_empty() {
                        member.allowed_tools = old.allowed_tools.clone();
                    }
                }
                member
            })
            .collect()
    }

    /// Persist and surface messages exchanged in a member's native session
    /// (imported after an interactive attach), as one synthetic turn.
    fn handle_import_transcript(
        &mut self,
        member: MemberId,
        items: Vec<ImportedMessage>,
        step: &mut RuntimeStep,
    ) {
        let Some(id) = self.config.find(member.as_str()).map(|m| m.id.clone()) else {
            step.events
                .push(RuntimeEvent::Notice(format!("unknown member: {member}")));
            return;
        };
        let items = coalesce_imported_assistant_messages(items);
        let original_count = items.len();
        let mut retained_bytes = 0_usize;
        let mut truncated = false;
        let mut oversized_items = 0_usize;
        let mut retained = Vec::with_capacity(original_count.min(MAX_IMPORTED_ITEMS));
        for (index, item) in items.into_iter().enumerate() {
            if index == MAX_IMPORTED_ITEMS {
                truncated = true;
                break;
            }
            if item.text.len() > MAX_IMPORTED_ITEM_BYTES {
                truncated = true;
                oversized_items = oversized_items.saturating_add(1);
                continue;
            }
            let remaining = MAX_IMPORTED_TOTAL_BYTES.saturating_sub(retained_bytes);
            if remaining == 0 || item.text.len() > remaining {
                truncated = true;
                break;
            }
            retained_bytes = retained_bytes.saturating_add(item.text.len());
            retained.push(item);
            if retained_bytes == MAX_IMPORTED_TOTAL_BYTES {
                truncated |= retained.len() < original_count;
                break;
            }
        }
        if retained.is_empty() {
            if truncated {
                step.events.push(RuntimeEvent::Notice(format!(
                    "attached transcript from {id} exceeded import limits and contained no retainable messages (skipped {oversized_items} oversized message(s))"
                )));
            }
            return;
        }
        let turn = match self.store.create_turn() {
            Ok(turn) => turn,
            Err(err) => {
                step.events
                    .push(RuntimeEvent::Notice(format!("store error: {err}")));
                return;
            }
        };
        let display = self.member_display(&id);
        let backend = self.member_backend(&id);
        let count = retained.len();
        step.events.push(RuntimeEvent::TurnStarted { turn });
        for item in retained {
            if item.from_user {
                if let Err(err) =
                    self.store
                        .record_user(turn, std::slice::from_ref(&id), &item.text)
                {
                    self.report_store_error("save an imported user message", err, step);
                    continue;
                }
                step.events.push(RuntimeEvent::UserMessage {
                    turn,
                    targets: vec![id.clone()],
                    body: item.text,
                });
            } else {
                if let Err(err) = self
                    .store
                    .record_agent(turn, &id, &display, backend, &item.text)
                {
                    self.report_store_error("save an imported agent message", err, step);
                    continue;
                }
                let msg = self.next_msg();
                step.events.push(RuntimeEvent::MessageStarted {
                    msg,
                    turn,
                    member: id.clone(),
                });
                step.events.push(RuntimeEvent::MessageCompleted {
                    msg,
                    text: item.text,
                });
            }
        }
        step.events.push(RuntimeEvent::Notice(format!(
            "imported {count} message(s) from {id}'s session"
        )));
        if truncated || count < original_count {
            step.events.push(RuntimeEvent::Notice(format!(
                "attached transcript from {id} was truncated to {count} message(s) and {retained_bytes} bytes; skipped {oversized_items} oversized message(s)"
            )));
        }
        step.events.push(RuntimeEvent::TurnFinished { turn });
        // The imported messages come from the member's own native session.
        self.advance_seen(&id, step);
    }

    fn handle_import_session(
        &mut self,
        target_member: Option<MemberId>,
        session_id: String,
        step: &mut RuntimeStep,
    ) {
        let member_id = match target_member {
            Some(m) => m,
            None => match &self.config.default_target {
                Some(DefaultTarget::Member(id)) => id.clone(),
                _ => match self.config.members.first() {
                    Some(m) => m.id.clone(),
                    None => {
                        step.events.push(RuntimeEvent::Notice(
                            "team has no members to import into".to_string(),
                        ));
                        return;
                    }
                },
            },
        };
        let (backend, cwd) = match self.config.member(&member_id) {
            Some(member) => (member.backend, member.resolved_cwd(&self.config.workspace)),
            None => {
                step.events
                    .push(RuntimeEvent::Notice(format!("unknown member: {member_id}")));
                return;
            }
        };
        let native = native_session_messages(backend, &session_id, &cwd);
        if native.is_empty() {
            step.events.push(RuntimeEvent::Notice(format!(
                "no messages found in {backend} session '{session_id}'"
            )));
            return;
        }
        let count = native.len();
        let agent_session = AgentSessionId(session_id.clone());
        // An imported session carries its own history, so the chat so far
        // counts as seen by it.
        let latest = self.store.latest_message_id().unwrap_or(0);
        self.sessions
            .set_external(member_id.clone(), agent_session.clone(), latest);
        let _ = self
            .store
            .upsert_session(&member_id, backend, &agent_session);
        let _ = self.store.set_session_seen(&member_id, latest);
        let _ = self
            .store
            .set_native_import_cursor(&member_id, &agent_session, count);
        // Refresh the chat snapshot so /resume restores the attached session.
        self.persist_snapshot_or_notice("save the chat session", step);

        self.handle_import_transcript(member_id.clone(), native, step);
        step.events.push(RuntimeEvent::SessionUpdated {
            member: member_id.clone(),
            session: agent_session,
        });
        step.events.push(RuntimeEvent::Notice(format!(
            "successfully imported {count} message(s) from {backend} session '{session_id}' into @{member_id}"
        )));
    }

    fn handle_export_session(&mut self, _format: Option<String>, step: &mut RuntimeStep) {
        let session_id = self
            .config
            .members
            .iter()
            .find_map(|m| self.sessions.get(&m.id).map(|s| s.0))
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

        let chat_items = match self.store.replay_chat() {
            Ok(items) => items,
            Err(err) => {
                step.events.push(RuntimeEvent::Notice(format!(
                    "failed to load conversation for export: {err}"
                )));
                return;
            }
        };

        match crate::tui::claude_export::export_chat_items_to_claude_jsonl(
            &self.config.workspace,
            &session_id,
            &chat_items,
        ) {
            Ok(path) => {
                step.events.push(RuntimeEvent::Notice(format!(
                    "successfully exported session to Claude format: {}",
                    path.display()
                )));
            }
            Err(err) => {
                step.events.push(RuntimeEvent::Notice(format!(
                    "failed to export Claude session: {err}"
                )));
            }
        }
    }

    fn handle_approval(
        &mut self,
        id: ApprovalId,
        decision: ApprovalDecision,
        step: &mut RuntimeStep,
    ) {
        if let Some(held) = self.native_approvals.remove(&id) {
            match self.store.resolve_approval(id, decision) {
                Ok(true) => step
                    .events
                    .push(RuntimeEvent::ApprovalResolved { id, decision }),
                Ok(false) => {
                    step.events
                        .push(RuntimeEvent::Notice(format!("no pending approval {id}")));
                    return;
                }
                Err(err) => {
                    // Keep the in-memory request alive if its durable row
                    // could not be updated. The runner remains paused, so the
                    // user can safely retry rather than silently continuing.
                    self.native_approvals.insert(id, held);
                    self.report_store_error("resolve a native approval", err, step);
                    return;
                }
            }
            step.runner_controls
                .push(RunnerControl::ResolveNativeApproval {
                    member: held.member,
                    request_id: held.request_id,
                    decision,
                });
            return;
        }
        if decision == ApprovalDecision::Approve
            && let Some(run_id) = self.held_approvals.get(&id).and_then(|held| held.mode_run)
        {
            match self.store.active_run(run_id) {
                Ok(run) if matches!(run.status, RunStatus::Running | RunStatus::Verifying) => {}
                Ok(_) | Err(rusqlite::Error::QueryReturnedNoRows) => {
                    match self.store.resolve_approval(id, ApprovalDecision::Reject) {
                        Ok(true) => step.events.push(RuntimeEvent::ApprovalResolved {
                            id,
                            decision: ApprovalDecision::Reject,
                        }),
                        Ok(false) => {
                            step.events
                                .push(RuntimeEvent::Notice(format!("no pending approval {id}")));
                            return;
                        }
                        Err(err) => {
                            self.report_store_error("reject a stale mode approval", err, step);
                            return;
                        }
                    }
                    let held = self
                        .held_approvals
                        .remove(&id)
                        .expect("approval was inspected above");
                    let mut affected_turns = self.reject_sibling_mode_approvals(run_id, step);
                    affected_turns.insert(held.turn);
                    self.failed_runs.insert(run_id);
                    self.mode_sessions.remove(&run_id);
                    step.events.push(RuntimeEvent::Notice(format!(
                        "approval {id} rejected because run {run_id} is no longer active"
                    )));
                    for turn in affected_turns {
                        self.check_turn_complete(turn, step);
                    }
                    return;
                }
                Err(err) => {
                    self.report_store_error("confirm an approval run is active", err, step);
                    return;
                }
            }
        }
        match self.store.resolve_approval(id, decision) {
            Ok(true) => step
                .events
                .push(RuntimeEvent::ApprovalResolved { id, decision }),
            Ok(false) => {
                step.events
                    .push(RuntimeEvent::Notice(format!("no pending approval {id}")));
                return;
            }
            Err(err) => {
                self.report_store_error("resolve an approval", err, step);
                return;
            }
        }
        let Some(held) = self.held_approvals.remove(&id) else {
            return;
        };
        if let Some((from, member)) = held.member_request {
            match decision {
                ApprovalDecision::Approve => {
                    self.add_team_member_from_agent(&from, member, step);
                }
                ApprovalDecision::Reject => {
                    step.events.push(RuntimeEvent::Notice(
                        "teammate addition rejected".to_string(),
                    ));
                }
            }
            self.check_turn_complete(held.turn, step);
            return;
        }
        match decision {
            ApprovalDecision::Approve => {
                if let Some(run_id) = held.mode_run
                    && self
                        .mode_sessions
                        .get(&run_id)
                        .is_some_and(|session| session.phase == ModePhase::AwaitingExecution)
                    && !self.mode_plan_confirm_execution(run_id, step)
                {
                    self.check_turn_complete(held.turn, step);
                    return;
                }
                for member in held.targets {
                    self.enqueue_prompt(&member, held.turn, held.prompt.clone(), step);
                }
            }
            ApprovalDecision::Reject => {
                step.events
                    .push(RuntimeEvent::Notice("request rejected".to_string()));
                if let Some(run_id) = held.mode_run {
                    self.block_mode_run(run_id, "dispatch rejected by user", step);
                    for turn in self.reject_sibling_mode_approvals(run_id, step) {
                        self.check_turn_complete(turn, step);
                    }
                }
                self.check_turn_complete(held.turn, step);
            }
        }
    }

    fn reject_sibling_mode_approvals(
        &mut self,
        run_id: RunId,
        step: &mut RuntimeStep,
    ) -> HashSet<TurnId> {
        let siblings: Vec<(ApprovalId, TurnId)> = self
            .held_approvals
            .iter()
            .filter(|(_, held)| held.mode_run == Some(run_id))
            .map(|(id, held)| (*id, held.turn))
            .collect();
        let ids: Vec<ApprovalId> = siblings.iter().map(|(id, _)| *id).collect();
        if let Err(err) = self.store.reject_pending_approvals(&ids) {
            self.report_store_error("reject sibling mode approvals", err, step);
            return HashSet::new();
        }
        let mut turns = HashSet::new();
        for (id, turn) in siblings {
            self.held_approvals.remove(&id);
            turns.insert(turn);
            step.events.push(RuntimeEvent::ApprovalResolved {
                id,
                decision: ApprovalDecision::Reject,
            });
        }
        turns
    }

    fn resolve_next_paused_route(&mut self, resume: bool, step: &mut RuntimeStep) {
        let Some(route) = self.paused_routes.pop_front() else {
            step.events
                .push(RuntimeEvent::RouteQueueUpdated { queued: 0 });
            step.events
                .push(RuntimeEvent::Notice("no paused routes".to_string()));
            return;
        };
        step.events.push(RuntimeEvent::RouteQueueUpdated {
            queued: self.paused_routes.len(),
        });
        if resume {
            step.events.push(RuntimeEvent::Notice(format!(
                "resumed route {} -> {}",
                route.from,
                route.to_labels.join(", ")
            )));
            for member in route.to_members {
                self.enqueue_prompt_from(
                    &member,
                    route.turn,
                    route.prompt.clone(),
                    Some(route.from.clone()),
                    step,
                );
            }
        } else {
            step.events.push(RuntimeEvent::Notice(format!(
                "dropped route {} -> {}",
                route.from,
                route.to_labels.join(", ")
            )));
            self.check_turn_complete(route.turn, step);
        }
    }

    // === agent event handling ===========================================

    pub fn on_agent_event(&mut self, member: &MemberId, event: AgentEvent) -> RuntimeStep {
        let mut step = RuntimeStep::default();
        // Ignore stream events for a member that is not currently running,
        // except the terminal Exited which we always honor.
        let running = self
            .members
            .get(member)
            .map(|s| s.running.is_some())
            .unwrap_or(false);
        if !running && !matches!(event, AgentEvent::Exited { .. }) {
            return step;
        }

        match event {
            AgentEvent::MessageStarted => self.start_message(member, &mut step),
            AgentEvent::TextDelta(text) => {
                self.ensure_message(member, &mut step);
                if let Some(msg) = self.message_id(member)
                    && let Some(text) = self.append_text(member, &text)
                {
                    step.events.push(RuntimeEvent::MessageDelta { msg, text });
                }
            }
            AgentEvent::Reasoning(text) => {
                if self.message_id(member).is_some() {
                    let pending = self
                        .members
                        .get(member)
                        .and_then(|state| state.running.as_ref())
                        .map(|running| running.text.clone())
                        .unwrap_or_default();
                    self.complete_message(member, pending, &mut step);
                }
                if let Some(text) = self.append_reasoning(member, &text) {
                    step.events.push(RuntimeEvent::Reasoning {
                        member: member.clone(),
                        text,
                    });
                }
            }
            AgentEvent::ReasoningSectionBreak | AgentEvent::ReasoningCompleted => {
                self.commit_open_thinking(member, &mut step);
            }
            AgentEvent::MessageCompleted(text) => self.complete_message(member, text, &mut step),
            AgentEvent::ToolStarted { id, name, summary } => {
                if let Some(turn) = self.running_turn(member) {
                    self.goal_note_output(member, turn);
                }
                self.commit_open_thinking(member, &mut step);
                self.commit_open_message_before_tools(member, &mut step);
                if let Some(state) = self.members.get_mut(member) {
                    state.tools.insert(
                        id.clone(),
                        ActiveTool {
                            name: name.clone(),
                            summary: summary.clone(),
                            detail: String::new(),
                        },
                    );
                }
                step.events.push(RuntimeEvent::ToolStarted {
                    member: member.clone(),
                    tool_id: id,
                    name,
                    summary,
                });
            }
            AgentEvent::ToolProgress { id, delta } => {
                let retained_delta = if let Some(tool) = self
                    .members
                    .get_mut(member)
                    .and_then(|state| state.tools.get_mut(&id))
                {
                    append_bounded_text(&mut tool.detail, &delta, MAX_TOOL_DETAIL_BYTES)
                } else {
                    Some(bounded_text(&delta, MAX_TOOL_DETAIL_BYTES))
                };
                if let Some(delta) = retained_delta {
                    step.events.push(RuntimeEvent::ToolProgress {
                        member: member.clone(),
                        tool_id: id,
                        delta,
                    });
                }
            }
            AgentEvent::ToolCompleted { id, ok, summary } => {
                let tool = self
                    .members
                    .get_mut(member)
                    .and_then(|state| state.tools.remove(&id));
                let (name, input, mut output) = match tool {
                    Some(tool) => (tool.name, tool.summary, tool.detail),
                    None => ("tool".to_string(), String::new(), String::new()),
                };
                if !summary.is_empty()
                    && summary.trim() != input.trim()
                    && output.trim_end() != summary.trim()
                {
                    let separator = if !output.is_empty() && !output.ends_with('\n') {
                        "\n"
                    } else {
                        ""
                    };
                    let _ = append_bounded_text(
                        &mut output,
                        &format!("{separator}{summary}"),
                        MAX_TOOL_DETAIL_BYTES,
                    );
                }
                if let Some(turn) = self.running_turn(member)
                    && let Err(err) =
                        self.store
                            .record_tool(turn, member, &name, &input, &output, Some(ok))
                {
                    self.report_store_error("save a tool result", err, &mut step);
                }
                if !ok && is_permission_denied_tool(&output) {
                    self.record_permission_denial(member, &output, &mut step);
                }
                step.events.push(RuntimeEvent::ToolCompleted {
                    member: member.clone(),
                    tool_id: id,
                    ok,
                    output,
                });
            }
            AgentEvent::FileChange { files, ok } => {
                if let Some(turn) = self.running_turn(member) {
                    self.goal_note_output(member, turn);
                }
                if let Some(turn) = self.running_turn(member)
                    && let Err(err) = self.store.record_diff(turn, member, &files, ok)
                {
                    self.report_store_error("save a file change", err, &mut step);
                }
                step.events.push(RuntimeEvent::FileChange {
                    member: member.clone(),
                    files,
                    ok,
                });
            }
            AgentEvent::SessionDiscovered(session) => {
                self.record_member_session(member, session, &mut step);
            }
            AgentEvent::NativeApprovalRequested {
                request_id,
                action,
                body,
            } => {
                let Some(turn) = self.running_turn(member) else {
                    step.runner_controls
                        .push(RunnerControl::ResolveNativeApproval {
                            member: member.clone(),
                            request_id,
                            decision: ApprovalDecision::Reject,
                        });
                    return step;
                };
                if !self.manual_approvals {
                    self.log(
                        member,
                        LogEntry::info(member.as_str(), format!("auto-approved {action}")),
                        &mut step,
                    );
                    step.runner_controls
                        .push(RunnerControl::ResolveNativeApproval {
                            member: member.clone(),
                            request_id,
                            decision: ApprovalDecision::Approve,
                        });
                    return step;
                }
                match self
                    .store
                    .insert_approval(Some(turn), Some(member), &action, &body)
                {
                    Ok(id) => {
                        self.native_approvals.insert(
                            id,
                            NativeApproval {
                                member: member.clone(),
                                request_id,
                                turn,
                            },
                        );
                        step.events.push(RuntimeEvent::ApprovalRequested {
                            id,
                            member: Some(member.clone()),
                            action,
                            body,
                        });
                    }
                    Err(err) => {
                        self.report_store_error("save a native approval request", err, &mut step);
                        step.runner_controls
                            .push(RunnerControl::ResolveNativeApproval {
                                member: member.clone(),
                                request_id,
                                decision: ApprovalDecision::Reject,
                            });
                    }
                }
            }
            AgentEvent::Raw(line) => {
                let persistence_disabled = self
                    .members
                    .get(member)
                    .and_then(|state| state.running.as_ref())
                    .is_some_and(|running| running.raw_persistence_failed);
                if persistence_disabled {
                    return step;
                }
                if let Err(err) = self.store.record_stream_event(member, &line) {
                    if let Some(running) = self
                        .members
                        .get_mut(member)
                        .and_then(|state| state.running.as_mut())
                    {
                        running.raw_persistence_failed = true;
                    }
                    self.report_store_error("save a raw stream event", err, &mut step);
                }
            }
            AgentEvent::Stderr(line) => {
                self.log(member, LogEntry::warn(member.as_str(), line), &mut step)
            }
            AgentEvent::Log(message) => {
                if message.contains("agy quota exhausted") {
                    step.events.push(RuntimeEvent::Notice(message.clone()));
                }
                self.log(member, LogEntry::info(member.as_str(), message), &mut step)
            }
            AgentEvent::ParseWarning(message) => {
                self.log(member, LogEntry::warn(member.as_str(), message), &mut step)
            }
            AgentEvent::Fatal(message) => {
                let permission_denied = is_permission_denied_tool(&message);
                let (turn, cancelled, already_denied) = self
                    .members
                    .get_mut(member)
                    .and_then(|state| state.running.as_mut())
                    .map(|running| {
                        let cancelled = running.cancel.load(Ordering::Relaxed);
                        if permission_denied {
                            running.permission_denied = true;
                        } else if !cancelled && !running.permission_denied {
                            running.failed = true;
                        }
                        (running.turn, cancelled, running.permission_denied)
                    })
                    .map(|(turn, cancelled, denied)| (Some(turn), Some(cancelled), denied))
                    .unwrap_or((None, None, permission_denied));
                if cancelled == Some(true) {
                    self.log(
                        member,
                        LogEntry::info(
                            member.as_str(),
                            format!("backend stopped during cancellation: {message}"),
                        ),
                        &mut step,
                    );
                } else if permission_denied || already_denied {
                    self.record_permission_denial(member, &message, &mut step);
                } else {
                    if let Some(running) = self
                        .members
                        .get_mut(member)
                        .and_then(|state| state.running.as_mut())
                    {
                        running.error.get_or_insert_with(|| message.clone());
                    }
                    if let Some(turn) = turn
                        && let Err(err) =
                            self.store.record_error(Some(turn), Some(member), &message)
                    {
                        self.report_store_error("save a member error", err, &mut step);
                    }
                    step.events.push(RuntimeEvent::MemberError {
                        member: member.clone(),
                        message,
                    });
                }
            }
            AgentEvent::Exited { code, ok } => {
                self.finalize_run(member, code, ok, &mut step);
                self.finish_pending_close(member, &mut step);
            }
        }
        step
    }

    /// Persist and surface a backend session only when it changed. Both stream
    /// events and a transcript-proven native attach use this path.
    fn record_member_session(
        &mut self,
        member: &MemberId,
        session: AgentSessionId,
        step: &mut RuntimeStep,
    ) {
        if self.sessions.get(member).as_ref() == Some(&session) {
            return;
        }
        let backend = self.member_backend(member);
        self.sessions.set(member.clone(), session.clone());
        if let Err(err) = self.store.upsert_session(member, backend, &session) {
            self.report_store_error("save a member session", err, step);
        }
        self.persist_snapshot_or_notice("save the chat session", step);
        step.events.push(RuntimeEvent::SessionUpdated {
            member: member.clone(),
            session,
        });
    }

    fn log(&self, _member: &MemberId, entry: LogEntry, step: &mut RuntimeStep) {
        let entry = entry.bounded();
        if let Err(err) = self.store.record_log(&entry) {
            step.events.push(RuntimeEvent::Log(LogEntry::error(
                "store",
                format!("could not save a log entry: {err}"),
            )));
        }
        step.events.push(RuntimeEvent::Log(entry));
    }

    fn start_message(&mut self, member: &MemberId, step: &mut RuntimeStep) {
        self.commit_open_thinking(member, step);
        let msg = self.next_msg();
        if let Some(turn) = self.running_turn(member)
            && let Some(state) = self.members.get_mut(member)
            && let Some(running) = &mut state.running
        {
            running.message = Some(msg);
            running.text.clear();
            step.events.push(RuntimeEvent::MessageStarted {
                msg,
                turn,
                member: member.clone(),
            });
        }
    }

    fn ensure_message(&mut self, member: &MemberId, step: &mut RuntimeStep) {
        if self.message_id(member).is_none() {
            self.start_message(member, step);
        }
    }

    /// Clear the current thinking progress before the next tool or answer.
    /// Thinking is transient status, not transcript content.
    fn commit_open_thinking(&mut self, member: &MemberId, step: &mut RuntimeStep) {
        if self.running_turn(member).is_none() {
            return;
        }
        let had_reasoning = self
            .members
            .get(member)
            .and_then(|state| state.running.as_ref())
            .is_some_and(|running| !running.reasoning.trim().is_empty());
        if let Some(state) = self.members.get_mut(member)
            && let Some(running) = &mut state.running
        {
            running.reasoning.clear();
            running.reasoning_started = None;
        }
        if !had_reasoning {
            return;
        }
        step.events.push(RuntimeEvent::ReasoningCompleted {
            member: member.clone(),
        });
    }

    /// Close any in-progress assistant cell before tools land after it.
    /// Otherwise MessageCompleted writes the final reply into the earlier
    /// cell and the UI shows the essay above the tools that produced it.
    fn commit_open_message_before_tools(&mut self, member: &MemberId, step: &mut RuntimeStep) {
        if self.message_id(member).is_none() {
            return;
        }
        let text = self
            .members
            .get(member)
            .and_then(|state| state.running.as_ref())
            .map(|running| running.text.clone())
            .unwrap_or_default();
        self.complete_message(member, text, step);
    }

    fn complete_message(&mut self, member: &MemberId, text: String, step: &mut RuntimeStep) {
        self.ensure_message(member, step);
        let Some(msg) = self.message_id(member) else {
            return;
        };
        let Some(turn) = self.running_turn(member) else {
            return;
        };

        let supplied_text = bounded_text(&text, MAX_MESSAGE_TEXT_BYTES);
        // Deltas are the only final text supplied by some otherwise-valid
        // streaming transports. Never replace a visible streamed answer with
        // an empty terminal event just because that transport omits its
        // canonical text field.
        let text = if supplied_text.is_empty() {
            self.members
                .get(member)
                .and_then(|state| state.running.as_ref())
                .map(|running| running.text.clone())
                .unwrap_or(supplied_text)
        } else {
            supplied_text
        };
        let mut parsed = parse_agent_output(&text);
        for warning in &parsed.warnings {
            self.log(
                member,
                LogEntry::warn(member.as_str(), warning.clone()),
                step,
            );
        }

        let has_controls = !parsed.messages.is_empty()
            || !parsed.members.is_empty()
            || !parsed.spawns.is_empty()
            || !parsed.closes.is_empty()
            || !parsed.run_steps.is_empty()
            || !parsed.reviews.is_empty()
            || !parsed.brainstorm_votes.is_empty()
            || !parsed.brainstorm_cards.is_empty()
            || !parsed.goal_completes.is_empty();
        if has_controls {
            let raw_persistence_failed = self
                .members
                .get(member)
                .and_then(|state| state.running.as_ref())
                .is_some_and(|running| running.raw_persistence_failed);
            if raw_persistence_failed {
                step.events.push(RuntimeEvent::MemberNotice {
                    member: member.clone(),
                    text: format!(
                        "ignored controls from {member} because their raw source could not be saved"
                    ),
                });
                self.fail_completed_message(member, msg, step);
                return;
            }
            if let Err(err) = self.store.record_agent_control_source(turn, member, &text) {
                self.report_store_error("save an agent control source", err, step);
                self.fail_completed_message(member, msg, step);
                return;
            }
        }

        if let Some(limit) = self.brainstorm_ideas_cap(member) {
            let extra = parsed.brainstorm_cards.len().saturating_sub(limit);
            if extra > 0 {
                parsed.brainstorm_cards.truncate(limit);
                step.events.push(RuntimeEvent::MemberNotice {
                    member: member.clone(),
                    text: format!(
                        "{member} emitted {extra} extra brainstorm card(s); kept {limit}"
                    ),
                });
            }
        }
        let visible_text =
            render_brainstorm_response(&parsed.visible_text, &parsed.brainstorm_cards);
        if !visible_text.is_empty() {
            let display = self.member_display(member);
            let backend = self.member_backend(member);
            if let Err(err) =
                self.store
                    .record_agent(turn, member, &display, backend, &visible_text)
            {
                self.report_store_error("save an agent message", err, step);
                // Streaming deltas are provisional until the canonical message
                // is durable. Clear them and do not execute controls from an
                // output that is absent from the audit trail.
                self.fail_completed_message(member, msg, step);
                return;
            }
        }
        step.events.push(RuntimeEvent::MessageCompleted {
            msg,
            text: visible_text.clone(),
        });

        if let Some(state) = self.members.get_mut(member)
            && let Some(running) = &mut state.running
        {
            running.message = None;
            running.text.clear();
            if !visible_text.trim().is_empty() {
                running.last_reply = visible_text.clone();
            }
        }

        // Reply text or a team action counts as progress for the goal loop.
        if !visible_text.trim().is_empty()
            || !parsed.messages.is_empty()
            || !parsed.members.is_empty()
            || !parsed.spawns.is_empty()
            || !parsed.closes.is_empty()
            || !parsed.run_steps.is_empty()
        {
            self.goal_note_output(member, turn);
        }
        self.mode_record_message(member, turn, &visible_text, &parsed, step);

        for member_request in parsed.members {
            self.request_team_member_from_agent(member, turn, member_request, step);
        }
        for spawn in parsed.spawns {
            self.handle_spawn(member, turn, spawn, step);
        }
        for close in parsed.closes {
            self.handle_close(member, turn, close, step);
        }
        let mut checklist_run = None;
        for request in parsed.run_steps {
            if let Some(run) = self.apply_run_step_from_agent(member, turn, request, step) {
                checklist_run = Some(run);
            }
        }
        if let Some(run) = checklist_run {
            step.events.push(RuntimeEvent::MemberNotice {
                member: member.clone(),
                text: format!("{member} updated {} checklist", run.label()),
            });
        }
        for tmsg in parsed.messages {
            self.route_team_message(member, turn, tmsg, step);
        }
        for done in parsed.goal_completes {
            self.goal_complete_from_agent(member, done.summary, step);
        }
    }

    fn fail_completed_message(
        &mut self,
        member: &MemberId,
        msg: MessageId,
        step: &mut RuntimeStep,
    ) {
        if let Some(state) = self.members.get_mut(member)
            && let Some(running) = &mut state.running
        {
            running.failed = true;
            running.message = None;
            running.text.clear();
        }
        step.events.push(RuntimeEvent::MessageCompleted {
            msg,
            text: String::new(),
        });
    }

    fn request_team_member_from_agent(
        &mut self,
        from: &MemberId,
        _turn: TurnId,
        member: TeamMember,
        step: &mut RuntimeStep,
    ) {
        if !self.validate_agent_team_member(from, &member, step) {
            return;
        }
        if self.active_mode == TerminalMode::Team && !self.team_allows_add_members() {
            step.events.push(RuntimeEvent::MemberNotice {
                member: from.clone(),
                text: format!(
                    "{from} could not add teammate {}: team mode is locked to the current roster",
                    member.id
                ),
            });
            return;
        }
        self.add_team_member_from_agent(from, member, step);
    }

    fn team_allows_add_members(&self) -> bool {
        resolve_team_allow_add_members(&self.effective_config())
    }

    fn validate_agent_team_member(
        &self,
        from: &MemberId,
        member: &TeamMember,
        step: &mut RuntimeStep,
    ) -> bool {
        if self.config.find(member.id.as_str()).is_some() {
            step.events.push(RuntimeEvent::MemberNotice {
                member: from.clone(),
                text: format!(
                    "{from} could not add teammate {}: member already exists",
                    member.id
                ),
            });
            return false;
        }
        if self.config.find(&member.display_name).is_some() {
            step.events.push(RuntimeEvent::MemberNotice {
                member: from.clone(),
                text: format!(
                    "{from} could not add teammate {}: display name already exists",
                    member.display_name
                ),
            });
            return false;
        }
        true
    }

    fn add_team_member_from_agent(
        &mut self,
        from: &MemberId,
        member: TeamMember,
        step: &mut RuntimeStep,
    ) {
        if !self.validate_agent_team_member(from, &member, step) {
            return;
        }

        let id = member.id.clone();
        let backend = member.backend;
        let detail = if member.role.trim().is_empty() {
            backend.to_string()
        } else {
            format!("{backend}, {}", member.role.trim())
        };
        let mut members = self.config.members.clone();
        members.push(member);
        let default_target = self.config.default_target.clone();
        self.handle_replace_team(members, default_target, step);

        if self.config.member(&id).is_some() {
            step.events.push(RuntimeEvent::MemberNotice {
                member: from.clone(),
                text: format!("{from} added teammate {id} ({detail})"),
            });
        }
    }

    fn route_team_message(
        &mut self,
        from: &MemberId,
        turn: TurnId,
        tmsg: crate::domain::event::TeamMessage,
        step: &mut RuntimeStep,
    ) {
        let resolved = router::resolve_targets(&self.config, &tmsg.to, Some(from));
        let to_labels: Vec<String> = resolved.members.iter().map(|m| m.to_string()).collect();

        if !to_labels.is_empty() {
            if let Err(err) = self.store.record_route(turn, from, &to_labels, &tmsg.body) {
                self.report_store_error("save an agent route", err, step);
                return;
            }
            step.events.push(RuntimeEvent::Route {
                turn,
                from: from.clone(),
                to: to_labels.clone(),
                body: tmsg.body.clone(),
            });

            self.log(
                from,
                LogEntry::info(
                    from.as_str(),
                    format!("→ {}: {}", to_labels.join(", "), tmsg.body),
                ),
                step,
            );
        }
        for unknown in &resolved.unknown {
            let err_msg = format!(
                "route to {unknown} failed: unknown member — message: {}",
                tmsg.body
            );
            if let Err(err) = self.store.record_error(Some(turn), Some(from), &err_msg) {
                self.report_store_error("save a route error", err, step);
            }
            step.events.push(RuntimeEvent::RouteError {
                turn,
                from: from.clone(),
                target: unknown.clone(),
                reason: "unknown member".to_string(),
                body: tmsg.body.clone(),
            });
        }
        if resolved.members.is_empty() {
            return;
        }

        self.note_origin_messaged(from, &resolved.members);

        // A parent's sub-agents are told their reply comes back on its own;
        // everyone else gets the usual relay wording.
        let from_display = self.member_display(from);
        let (children, others): (Vec<MemberId>, Vec<MemberId>) =
            resolved.members.into_iter().partition(|id| {
                self.config
                    .member(id)
                    .is_some_and(|member| member.parent.as_ref() == Some(from))
            });
        let mut dispatches: Vec<(Vec<MemberId>, String)> = Vec::new();
        if !children.is_empty() {
            dispatches.push((
                children,
                sub_agent_task_prompt(from, &from_display, &tmsg.body),
            ));
        }
        if !others.is_empty() {
            let prompt = if tmsg.kind.as_deref() == Some(HANDBACK_KIND) {
                handback_prompt(from, &from_display, &tmsg.body)
            } else {
                relay_prompt(from, &from_display, tmsg.kind.as_deref(), &tmsg.body)
            };
            dispatches.push((others, prompt));
        }

        if self.relay_paused {
            for (members, prompt) in dispatches {
                self.pause_route_to(turn, from, members, prompt, "relay paused by user", step);
            }
            return;
        }
        match self.relay.record_auto_relay(turn, from) {
            RelayDecision::Continue { .. } => {
                for (members, prompt) in dispatches {
                    for member in members {
                        self.enqueue_prompt_from(
                            &member,
                            turn,
                            prompt.clone(),
                            Some(from.clone()),
                            step,
                        );
                    }
                }
            }
            RelayDecision::Pause { count } => {
                // Ungated: resume via /retry is an explicit human decision.
                for (members, prompt) in dispatches {
                    self.pause_route_to(
                        turn,
                        from,
                        members,
                        prompt,
                        &format!("auto-relay limit reached ({count})"),
                        step,
                    );
                }
            }
        }
    }

    fn pause_route_to(
        &mut self,
        turn: TurnId,
        from: &MemberId,
        members: Vec<MemberId>,
        prompt: String,
        reason: &str,
        step: &mut RuntimeStep,
    ) {
        let labels = members.iter().map(|m| m.to_string()).collect();
        self.pause_route(turn, from, members, labels, prompt, reason, step);
    }

    fn apply_run_step_from_agent(
        &mut self,
        from: &MemberId,
        turn: TurnId,
        request: RunStepRequest,
        step: &mut RuntimeStep,
    ) -> Option<crate::domain::event::RunSummary> {
        let Some(run_id) = self.run_turns.get(&turn).copied() else {
            step.events.push(RuntimeEvent::MemberNotice {
                member: from.clone(),
                text: format!("{from} ignored run step update: no active run"),
            });
            return None;
        };

        let requested_owner = match &request {
            RunStepRequest::Add { owner, .. } | RunStepRequest::Assign { owner, .. } => {
                owner.as_ref()
            }
            _ => None,
        };
        if let Some(owner) = requested_owner
            && self.config.member(owner).is_none()
        {
            step.events.push(RuntimeEvent::MemberNotice {
                member: from.clone(),
                text: format!("{from} could not update run {run_id}: unknown step owner {owner}"),
            });
            return None;
        }

        let result = match request {
            RunStepRequest::Add { owner, title } => {
                self.store.add_run_step(run_id, owner.as_ref(), &title)
            }
            RunStepRequest::Update {
                step: step_number,
                status,
                note,
            } => self
                .store
                .update_run_step(run_id, step_number, status, note.as_deref()),
            RunStepRequest::Rename {
                step: step_number,
                title,
            } => self.store.rename_run_step(run_id, step_number, &title),
            RunStepRequest::Remove { step: step_number } => {
                self.store.remove_run_step(run_id, step_number)
            }
            RunStepRequest::Assign {
                step: step_number,
                owner,
            } => self
                .store
                .assign_run_step(run_id, step_number, owner.as_ref()),
        };

        match result {
            Ok(run) => {
                step.events
                    .push(RuntimeEvent::RunUpdated { run: run.clone() });
                Some(run)
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                step.events.push(RuntimeEvent::MemberNotice {
                    member: from.clone(),
                    text: format!("{from} could not update run {run_id}: step was not found"),
                });
                None
            }
            Err(err) => {
                step.events.push(RuntimeEvent::MemberNotice {
                    member: from.clone(),
                    text: format!("{from} could not update run {run_id}: {err}"),
                });
                None
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn pause_route(
        &mut self,
        turn: TurnId,
        from: &MemberId,
        to_members: Vec<MemberId>,
        to_labels: Vec<String>,
        prompt: String,
        reason: &str,
        step: &mut RuntimeStep,
    ) {
        self.paused_routes.push_back(PausedRoute {
            turn,
            from: from.clone(),
            to_members,
            to_labels: to_labels.clone(),
            prompt,
        });
        step.events.push(RuntimeEvent::RoutePaused {
            turn,
            from: from.clone(),
            to: to_labels,
            reason: reason.to_string(),
            queued: self.paused_routes.len(),
        });
    }

    fn finalize_run(
        &mut self,
        member: &MemberId,
        code: Option<i32>,
        ok: bool,
        step: &mut RuntimeStep,
    ) {
        self.commit_open_thinking(member, step);
        // Flush an unterminated streaming message.
        let pending_text = self.members.get(member).and_then(|s| {
            s.running
                .as_ref()
                .filter(|r| r.message.is_some())
                .map(|r| r.text.clone())
        });
        if let Some(text) = pending_text {
            self.complete_message(member, text, step);
        }

        let handback: SubAgentOutcome;
        let (
            turn,
            cancelled,
            failed,
            permission_denied,
            denied_reason,
            permission_noticed,
            run_error,
        ) = match self.members.get_mut(member).and_then(|s| s.running.take()) {
            Some(running) => {
                handback = SubAgentOutcome::new(&running);
                (
                    running.turn,
                    running.cancel.load(Ordering::Relaxed),
                    running.failed,
                    running.permission_denied,
                    running.denied_reason,
                    running.permission_noticed,
                    running.error,
                )
            }
            None => return,
        };

        // A failed turn of the goal's member stops the goal loop; the check
        // after it must not send a continuation.
        if !cancelled && (failed || (!ok && !permission_denied)) {
            let error = run_error.clone().unwrap_or_else(|| {
                format!(
                    "{} exited without success (code {})",
                    self.member_backend(member),
                    code.map(|c| c.to_string())
                        .unwrap_or_else(|| "unknown".to_string())
                )
            });
            self.goal_pause_for_failure(member, &error, step);
        }

        let terminal_state_saved = if cancelled {
            // A user-requested cancel kills the process (no exit code); that is
            // expected, not an error.
            self.mode_mark_turn_cancelled(turn);
            step.events.push(RuntimeEvent::MemberNotice {
                member: member.clone(),
                text: format!("{member} cancelled"),
            });
            true
        } else if failed {
            // A structured backend failure remains authoritative even when the
            // child process subsequently exits with status 0.
            self.mark_run_turn(turn, RunStatus::Failed, step)
        } else if !ok && permission_denied {
            // Agy/Claude print-mode auto-deny is a tool result. The member may
            // exit non-zero; Plan must keep going instead of blocking.
            if !permission_noticed {
                let reason = denied_reason.unwrap_or_else(|| "a command".to_string());
                step.events.push(RuntimeEvent::MemberNotice {
                    member: member.clone(),
                    text: format!(
                        "{member} was denied permission to run {reason}; the run continues"
                    ),
                });
            }
            true
        } else if !ok {
            let message = format!(
                "{} exited without success (code {})",
                self.member_backend(member),
                code.map(|c| c.to_string())
                    .unwrap_or_else(|| "unknown".to_string())
            );
            if let Err(err) = self.store.record_error(Some(turn), Some(member), &message) {
                self.report_store_error("save a member exit error", err, step);
            }
            step.events.push(RuntimeEvent::MemberError {
                member: member.clone(),
                message,
            });
            self.mark_run_turn(turn, RunStatus::Failed, step)
        } else {
            true
        };

        if let Some(state) = self.members.get_mut(member) {
            state.tools.clear();
            state.status = MemberStatus::Idle;
        }
        // The native session took the prompt (with any recap) and answered it.
        // A run that failed outright may not have, so its turn stays unseen.
        if cancelled || permission_denied || (ok && !failed) {
            self.advance_seen(member, step);
        }
        step.events.push(RuntimeEvent::MemberStatus {
            member: member.clone(),
            status: MemberStatus::Idle,
        });
        self.refresh_native_import_cursor(member);
        self.note_roster_write(step);

        if !terminal_state_saved {
            return;
        }

        // Hand a sub-agent's result back before the turn is judged finished,
        // so the parent's follow-up run keeps the same turn open.
        if !cancelled {
            self.hand_back_to_parent(
                member,
                turn,
                handback,
                failed || (!ok && !permission_denied),
                step,
            );
        }

        // Finalize the completed turn before starting unrelated queued work.
        // If the terminal transition could not be persisted, run_turns keeps
        // ownership and the queue remains stopped for an explicit recovery.
        let finishing_turn = !self.turn_active(turn);
        self.check_turn_complete(turn, step);
        if finishing_turn && self.run_turns.contains_key(&turn) {
            return;
        }

        // Start the next queued prompt for this member, if any.
        let next = self
            .members
            .get_mut(member)
            .and_then(|s| s.queue.pop_front());
        if let Some(queued) = next {
            self.emit_queue_updated(member, step);
            self.start_run(member, queued.turn, queued.prompt, queued.origin, step);
        }
    }

    fn record_permission_denial(
        &mut self,
        member: &MemberId,
        detail: &str,
        step: &mut RuntimeStep,
    ) {
        let reason = format_permission_denial(detail);
        let already_noticed = self
            .members
            .get(member)
            .and_then(|state| state.running.as_ref())
            .is_some_and(|running| running.permission_noticed);
        if let Some(running) = self
            .members
            .get_mut(member)
            .and_then(|state| state.running.as_mut())
        {
            running.permission_denied = true;
            if running.denied_reason.is_none() {
                running.denied_reason = Some(reason.clone());
            }
            running.permission_noticed = true;
        }
        if already_noticed {
            return;
        }
        self.log(
            member,
            LogEntry::warn(member.as_str(), format!("tool permission denied: {detail}")),
            step,
        );
        step.events.push(RuntimeEvent::MemberNotice {
            member: member.clone(),
            text: format!("{member} was denied permission to run {reason}; the run continues"),
        });
    }

    fn emit_queue_updated(&self, member: &MemberId, step: &mut RuntimeStep) {
        let prompts = self
            .members
            .get(member)
            .map(|state| {
                state
                    .queue
                    .iter()
                    .map(|queued| queued.prompt.clone())
                    .collect()
            })
            .unwrap_or_default();
        step.events.push(RuntimeEvent::QueueUpdated {
            member: member.clone(),
            prompts,
        });
    }

    fn handle_edit_queued_prompt(&mut self, member: Option<MemberId>, step: &mut RuntimeStep) {
        let member = member
            .or_else(|| {
                self.last_user.as_ref().and_then(|(target, _)| {
                    self.resolve_message_target(target)
                        .0
                        .into_iter()
                        .rev()
                        .find(|id| {
                            self.members
                                .get(id)
                                .is_some_and(|state| !state.queue.is_empty())
                        })
                })
            })
            .or_else(|| {
                self.members
                    .iter()
                    .find_map(|(id, state)| (!state.queue.is_empty()).then_some(id.clone()))
            });
        let Some(member) = member else {
            step.events.push(RuntimeEvent::Notice(
                "no queued message to edit".to_string(),
            ));
            return;
        };
        let Some(queued) = self
            .members
            .get_mut(&member)
            .and_then(|state| state.queue.pop_back())
        else {
            step.events.push(RuntimeEvent::MemberNotice {
                member: member.clone(),
                text: format!("{member} has no queued message"),
            });
            return;
        };
        self.check_turn_complete(queued.turn, step);
        if let Some(state) = self.members.get_mut(&member)
            && state.running.is_none()
            && state.queue.is_empty()
            && state.status == MemberStatus::Queued
        {
            state.status = MemberStatus::Idle;
            step.events.push(RuntimeEvent::MemberStatus {
                member: member.clone(),
                status: MemberStatus::Idle,
            });
            self.note_roster_write(step);
        }
        self.emit_queue_updated(&member, step);
        step.events.push(RuntimeEvent::QueuedPromptReturned {
            member,
            body: queued.prompt,
        });
    }

    // === queueing / dispatch ============================================

    fn enqueue_prompt(
        &mut self,
        member: &MemberId,
        turn: TurnId,
        prompt: String,
        step: &mut RuntimeStep,
    ) {
        self.enqueue_prompt_from(member, turn, prompt, None, step);
    }

    /// Enqueue `prompt` for `member`. `origin` is the member that sent it
    /// (`None` for the user and for engine dispatch).
    fn enqueue_prompt_from(
        &mut self,
        member: &MemberId,
        turn: TurnId,
        prompt: String,
        origin: Option<MemberId>,
        step: &mut RuntimeStep,
    ) {
        if self.closing.contains_key(member) {
            // On its way off the roster: nothing new is queued for it.
            self.check_turn_complete(turn, step);
            return;
        }
        let stripped_prompt = strip_routing_prefix(&prompt);
        let busy = self
            .members
            .get(member)
            .map(|s| s.running.is_some())
            .unwrap_or(false);
        if busy {
            if let Some(state) = self.members.get_mut(member) {
                state.queue.push_back(QueuedPrompt {
                    turn,
                    prompt: stripped_prompt,
                    origin,
                });
            }
            self.emit_queue_updated(member, step);
            self.note_roster_write(step);
        } else {
            self.start_run(member, turn, stripped_prompt, origin, step);
        }
    }

    fn start_run(
        &mut self,
        member: &MemberId,
        turn: TurnId,
        prompt: String,
        origin: Option<MemberId>,
        step: &mut RuntimeStep,
    ) {
        if !self.members.contains_key(member) {
            let message = format!("cannot dispatch to unknown member {member}");
            if let Err(err) = self.store.record_error(Some(turn), Some(member), &message) {
                self.report_store_error("save an unknown-member dispatch error", err, step);
            }
            step.events.push(RuntimeEvent::MemberError {
                member: member.clone(),
                message: message.clone(),
            });
            if let Some(run_id) = self.run_turns.get(&turn).copied() {
                self.block_mode_run(run_id, &message, step);
            }
            self.check_turn_complete(turn, step);
            return;
        }
        if !self.publish_pending_user_message(turn, step) {
            self.check_turn_complete(turn, step);
            return;
        }
        // Both policies pin the first session id reported by the backend.
        // `fresh` only controls whether an older id is discarded when that
        // policy is selected; it must not create a new session every turn.
        self.goal_note_participation(member, turn);
        let session = self.sessions.get(member);
        let cancel = Arc::new(AtomicBool::new(false));
        let effort = self.members.get(member).and_then(|s| s.effort);
        if let Some(state) = self.members.get_mut(member) {
            state.running = Some(RunningState {
                cancel: cancel.clone(),
                turn,
                message: None,
                text: String::new(),
                reasoning: String::new(),
                reasoning_started: None,
                failed: false,
                permission_denied: false,
                denied_reason: None,
                permission_noticed: false,
                raw_persistence_failed: false,
                origin,
                messaged_origin: false,
                last_reply: String::new(),
                error: None,
            });
            state.status = MemberStatus::Running;
            state.tools.clear();
        }
        step.events.push(RuntimeEvent::MemberStatus {
            member: member.clone(),
            status: MemberStatus::Running,
        });
        self.note_roster_write(step);
        if self.protocol_stale.contains(member) {
            self.push_runner_upsert(member, step);
        }
        let prompt = normalize_backend_command(self.member_backend(member), prompt);
        let prompt = self.prompt_with_recap(member, turn, prompt, step);
        let prompt = self.prompt_with_protocol_note(member, session.is_some(), prompt);
        step.actions.push(RunAction {
            member: member.clone(),
            prompt,
            session,
            cancel,
            effort,
        });
    }

    fn publish_pending_user_message(&mut self, turn: TurnId, step: &mut RuntimeStep) -> bool {
        let Some(pending) = self.pending_user_messages.get(&turn) else {
            return true;
        };
        if !pending.persisted
            && let Err(err) = self
                .store
                .record_user(turn, &pending.targets, &pending.body)
        {
            self.report_store_error("save the queued user message", err, step);
            return false;
        }
        let pending = self
            .pending_user_messages
            .remove(&turn)
            .expect("pending user message exists");
        step.events.push(RuntimeEvent::UserMessage {
            turn,
            targets: pending.targets,
            body: pending.body,
        });
        true
    }

    fn roster_snapshot(&self) -> String {
        let mut lines = vec![
            "# Asterline roster".to_string(),
            String::new(),
            format!("Default target: {}", self.default_target_label()),
            String::new(),
            "## Members".to_string(),
        ];
        for member in &self.config.members {
            lines.push(format!("- {}", self.team_member_card(member)));
        }
        lines.push(String::new());
        lines.join("\n")
    }

    fn write_roster_snapshot(&self) -> std::io::Result<()> {
        let directory = state_paths::ensure_project_dir(&self.config.workspace)?;
        fs_safety::write_regular_file(
            &directory.join(state_paths::ROSTER_FILE),
            "team roster",
            &self.roster_snapshot(),
        )
    }

    fn note_roster_write(&self, step: &mut RuntimeStep) {
        if let Err(err) = self.write_roster_snapshot() {
            step.events.push(RuntimeEvent::Notice(format!(
                "could not update the team roster: {err}"
            )));
        }
    }

    fn team_member_card(&self, member: &TeamMember) -> String {
        let status = self
            .members
            .get(&member.id)
            .map(|state| state.status)
            .unwrap_or(MemberStatus::Idle);
        let model = member.model.as_deref().unwrap_or("-");
        let effort = member.effort.map(Effort::as_str).unwrap_or("-");
        let permission = member.permission_native_label();
        let allowed_tools = if member.allowed_tools.is_empty() {
            "-".to_string()
        } else {
            member.allowed_tools.join(",")
        };
        let parent = member
            .parent
            .as_ref()
            .map(|parent| format!(" parent={parent}"))
            .unwrap_or_default();
        let role = if member.role.trim().is_empty() {
            String::new()
        } else {
            format!(" role={:?}", member.role.trim())
        };
        format!(
            "id={} display_name={:?} backend={}{role} status={} model={} effort={} cwd={:?} sandbox={} permission_mode={} session_policy={} allowed_tools={}{parent}",
            member.id,
            member.display_name,
            member.backend.as_str(),
            status.as_str(),
            model,
            effort,
            member
                .resolved_cwd(&self.config.workspace)
                .display()
                .to_string(),
            member.sandbox_native_label(),
            permission,
            session_policy_label(member.session_policy),
            allowed_tools,
        )
    }

    fn default_target_label(&self) -> String {
        match &self.config.default_target {
            Some(DefaultTarget::All) => "all".to_string(),
            Some(DefaultTarget::Member(id)) => id.to_string(),
            None => self
                .config
                .members
                .first()
                .map(|member| member.id.to_string())
                .unwrap_or_else(|| "-".to_string()),
        }
    }

    fn check_turn_complete(&mut self, turn: TurnId, step: &mut RuntimeStep) {
        if !self.turn_active(turn) {
            self.pending_user_messages.remove(&turn);
            self.relay.reset_turn(turn);
            let run_id = self.run_turns.get(&turn).copied();
            let completion_saved = match run_id {
                Some(run_id) if self.mode_sessions.contains_key(&run_id) => {
                    self.goal_turns.remove(&turn);
                    self.run_turns.remove(&turn);
                    step.events.push(RuntimeEvent::TurnFinished { turn });
                    self.mode_on_turn_complete(run_id, step);
                    return;
                }
                Some(run_id) if !self.failed_runs.contains(&run_id) => {
                    // Team runs may auto-verify; plain/team Done otherwise.
                    self.finish_plain_or_team_run(run_id, step)
                }
                _ => true,
            };
            if !completion_saved {
                return;
            }
            self.run_turns.remove(&turn);
            step.events.push(RuntimeEvent::TurnFinished { turn });
            self.goal_on_turn_finished(turn, step);
        }
    }

    fn mark_run_turn(&mut self, turn: TurnId, status: RunStatus, step: &mut RuntimeStep) -> bool {
        let Some(run_id) = self.run_turns.get(&turn).copied() else {
            return true;
        };
        match self.store.update_run_status(run_id, status) {
            Ok(run) => {
                if status == RunStatus::Failed {
                    self.failed_runs.insert(run_id);
                }
                step.events.push(RuntimeEvent::RunUpdated { run });
                true
            }
            Err(err) => {
                self.report_store_error("save a run status", err, step);
                false
            }
        }
    }

    fn turn_active(&self, turn: TurnId) -> bool {
        let in_members = self.members.values().any(|state| {
            state.running.as_ref().map(|r| r.turn) == Some(turn)
                || state.queue.iter().any(|q| q.turn == turn)
        });
        in_members
            || self.paused_routes.iter().any(|r| r.turn == turn)
            || self.held_approvals.values().any(|h| h.turn == turn)
            || self.native_approvals.values().any(|h| h.turn == turn)
    }

    // === small helpers ==================================================

    fn append_text(&mut self, member: &MemberId, text: &str) -> Option<String> {
        self.members
            .get_mut(member)
            .and_then(|state| state.running.as_mut())
            .and_then(|running| {
                append_bounded_text(&mut running.text, text, MAX_MESSAGE_TEXT_BYTES)
            })
    }

    fn append_reasoning(&mut self, member: &MemberId, text: &str) -> Option<String> {
        let text = bounded_text(text, MAX_MESSAGE_TEXT_BYTES);
        self.members
            .get_mut(member)
            .and_then(|state| state.running.as_mut())
            .and_then(|running| {
                if running.reasoning_started.is_none() {
                    running.reasoning_started = Some(Instant::now());
                }
                if text.is_empty() {
                    return Some(String::new());
                }
                if text.starts_with(running.reasoning.as_str()) {
                    if text == running.reasoning {
                        return None;
                    }
                    running.reasoning = text;
                    return Some(running.reasoning.clone());
                }
                if running.reasoning.ends_with(&text) {
                    return None;
                }
                append_bounded_text(&mut running.reasoning, &text, MAX_MESSAGE_TEXT_BYTES)
                    .map(|_| running.reasoning.clone())
            })
    }

    fn message_id(&self, member: &MemberId) -> Option<MessageId> {
        self.members
            .get(member)
            .and_then(|s| s.running.as_ref())
            .and_then(|r| r.message)
    }

    fn running_turn(&self, member: &MemberId) -> Option<TurnId> {
        self.members
            .get(member)
            .and_then(|s| s.running.as_ref())
            .map(|r| r.turn)
    }

    fn next_msg(&mut self) -> MessageId {
        self.next_message_id += 1;
        MessageId(self.next_message_id)
    }

    fn member_display(&self, member: &MemberId) -> String {
        self.config
            .member(member)
            .map(|m| m.display_name.clone())
            .unwrap_or_else(|| member.to_string())
    }

    fn member_backend(&self, member: &MemberId) -> BackendKind {
        self.config
            .member(member)
            .map(|m| m.backend)
            .unwrap_or(BackendKind::Codex)
    }
}

// Team-run and shared run handlers (handle_run_team … on_verify_output), split out for
// readability. Still inside this module so private fields are accessible.
include!("team_runtime_runs.inc.rs");

// Collaboration-mode engine (review, plan, brainstorm, and team).
include!("team_runtime_modes.inc.rs");

// Sub-agents (`@@spawn`) and the automatic hand-back of their results.
include!("team_runtime_sub_agents.inc.rs");

// The persistent goal of the main agent (`/goal`) and its continuation loop.
include!("team_runtime_goal.inc.rs");

fn relay_prompt(from: &MemberId, from_display: &str, kind: Option<&str>, body: &str) -> String {
    let reply_instruction = if kind == Some("reply") {
        "This message is marked as a reply. Do not send another acknowledgement unless it contains a new question, request, correction, or blocker that requires an answer."
            .to_string()
    } else {
        format!(
            "Before ending your turn, you MUST answer the sender with exactly one control line:\n\
             @@team_message {{\"to\":\"{from}\",\"kind\":\"reply\",\"body\":\"your substantive response\"}}\n\
             Visible response text and run-step updates do not replace this reply."
        )
    };
    format!(
        "[relay from {from_display} ({from})]\n{}\n{reply_instruction}\n\n{body}",
        team_skill_hint()
    )
}

fn session_policy_label(policy: SessionPolicy) -> &'static str {
    match policy {
        SessionPolicy::Resume => "resume",
        SessionPolicy::Fresh => "fresh",
    }
}

fn native_session_messages(
    backend: BackendKind,
    session_id: &str,
    cwd: &std::path::Path,
) -> Vec<ImportedMessage> {
    match backend {
        BackendKind::Grok => crate::tui::grok_import::messages_for_session(session_id),
        BackendKind::Codex => crate::tui::rollout_import::messages_for_session(session_id),
        BackendKind::Claude => {
            crate::tui::claude_import::messages_for_session(session_id, &cwd.display().to_string())
        }
        BackendKind::Agy => Vec::new(),
    }
}

/// Native transcripts omit tool calls from the Asterline chat. Their assistant
/// text therefore arrives in multiple adjacent records for one user turn.
fn coalesce_imported_assistant_messages(items: Vec<ImportedMessage>) -> Vec<ImportedMessage> {
    let mut merged: Vec<ImportedMessage> = Vec::with_capacity(items.len());
    for item in items {
        let can_merge = merged.last().is_some_and(|previous| {
            !previous.from_user
                && !item.from_user
                && previous
                    .text
                    .len()
                    .saturating_add(item.text.len())
                    .saturating_add(2)
                    <= MAX_IMPORTED_ITEM_BYTES
        });
        if !can_merge {
            merged.push(item);
            continue;
        }
        let previous = merged.last_mut().expect("checked above");
        previous.text.push_str("\n\n");
        previous.text.push_str(&item.text);
    }
    merged
}

fn chat_item_fingerprint(item: &ChatItem) -> Option<String> {
    let text = match item {
        ChatItem::User { body, .. } | ChatItem::Agent { text: body, .. } => body.as_str(),
        _ => return None,
    };
    Some(import_fingerprint(text))
}

fn import_fingerprint(text: &str) -> String {
    let extracted = extract_tagged(text, "user_query").unwrap_or(text);
    let trimmed = extracted.trim();
    let body = if let Some(rest) = trimmed.strip_prefix('@') {
        rest.split_once(char::is_whitespace)
            .map(|(_, rest)| rest)
            .unwrap_or(rest)
    } else {
        trimmed
    };
    body.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(240)
        .collect()
}

fn extract_tagged<'a>(text: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = text.find(&open)? + open.len();
    let end = text[start..].find(&close)? + start;
    let inner = text[start..end].trim();
    (!inner.is_empty()).then_some(inner)
}

fn strip_routing_prefix(prompt: &str) -> String {
    let trimmed = prompt.trim();
    if let Some(rest) = trimmed.strip_prefix('@')
        && let Some(idx) = rest.find(char::is_whitespace)
    {
        return rest[idx..].trim().to_string();
    }
    prompt.to_string()
}

/// Do not rewrite a member's text based only on its first character. Native
/// controls and skills have different grammars; the TUI inserts an exact skill
/// invocation (for example `$review` for Codex) when it knows one.
/// Whether the backend gives its instructions to a native session only when
/// the session starts, so a resumed session keeps the old text. Codex's app
/// server sends `developerInstructions` at `thread/start`, and a thread it
/// resumes (or already has open) keeps its original ones. Claude passes
/// `--append-system-prompt` on every run, Agy puts the instructions in every
/// prompt, and Grok sends its rules with every `session/load`.
fn backend_fixes_protocol_at_start(backend: BackendKind) -> bool {
    matches!(backend, BackendKind::Codex)
}

fn normalize_backend_command(_backend: BackendKind, prompt: String) -> String {
    prompt
}

fn summarize_verify_output(stdout: &[u8], stderr: &[u8]) -> String {
    let mut text = String::new();
    let stdout = String::from_utf8_lossy(stdout);
    let stderr = String::from_utf8_lossy(stderr);
    if !stdout.trim().is_empty() {
        text.push_str(stdout.trim());
    }
    if !stderr.trim().is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(stderr.trim());
    }
    let text = text.lines().rev().take(12).collect::<Vec<_>>();
    let mut summary = text.into_iter().rev().collect::<Vec<_>>().join("\n");
    if summary.chars().count() > 1200 {
        summary = summary.chars().take(1199).collect::<String>() + "…";
    }
    if summary.is_empty() {
        "verification produced no output".to_string()
    } else {
        summary
    }
}

#[cfg(test)]
#[path = "team_runtime_tests/mod.rs"]
mod tests;
