// Sub-agents: members a top-level member opens with `@@spawn`. They live in
// one conversation, take work from their parent, and hand their final reply
// back on their own. Included into `team_runtime.rs` so the private runtime
// fields stay reachable.

/// `TeamMessage::kind` of a sub-agent's automatic result.
const HANDBACK_KIND: &str = "handback";

/// Who asked for a sub-agent to be removed, which decides who is told and how.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CloseOrigin {
    /// The user, from the Agents list.
    User,
    /// The sub-agent's parent, with `@@close` in its reply to this turn.
    Parent(TurnId),
}

/// What a finished run needs to decide whether its result goes back to the
/// member that assigned it.
#[derive(Default)]
struct SubAgentOutcome {
    origin: Option<MemberId>,
    messaged_origin: bool,
    last_reply: String,
    /// The backend's own message when the run failed.
    error: Option<String>,
}

impl SubAgentOutcome {
    fn new(running: &RunningState) -> Self {
        Self {
            origin: running.origin.clone(),
            messaged_origin: running.messaged_origin,
            last_reply: running.last_reply.clone(),
            error: running.error.clone(),
        }
    }
}

impl TeamRuntime {
    /// Execute one `@@spawn` from `from`: open the sub-agent (or reuse the one
    /// of that name) and hand it the task as a message from `from`.
    fn handle_spawn(
        &mut self,
        from: &MemberId,
        turn: TurnId,
        request: SpawnRequest,
        step: &mut RuntimeStep,
    ) {
        let Some(parent) = self.config.member(from).cloned() else {
            return;
        };
        if parent.parent.is_some() {
            self.refuse_spawn(
                from,
                turn,
                &request.name,
                "a sub-agent cannot open sub-agents of its own",
                step,
            );
            return;
        }
        let mode_allows = matches!(self.active_mode, TerminalMode::Normal | TerminalMode::Team);
        if !mode_allows {
            self.refuse_spawn(
                from,
                turn,
                &request.name,
                "sub-agents can only be opened in normal or team mode",
                step,
            );
            return;
        }
        let (request, resolution) = match self.resolve_spawn_request(&parent, request.clone()) {
            Ok(resolved) => resolved,
            Err(reason) => {
                self.refuse_spawn(from, turn, &request.name, &reason, step);
                return;
            }
        };
        let mut candidate = match sub_agent_member(&parent, &request) {
            Ok(candidate) => candidate,
            Err(reason) => {
                self.refuse_spawn(from, turn, &request.name, &reason, step);
                return;
            }
        };
        self.fit_inherited_effort(&mut candidate, &request);

        let existing = self
            .config
            .find(candidate.id.as_str())
            .or_else(|| self.config.find(&candidate.display_name));
        if let Some(existing) = existing {
            if existing.id == candidate.id && existing.parent.as_ref() == Some(from) {
                // Reuse: the sub-agent keeps its context. Only what the
                // request names explicitly (and differently) is changed.
                let existing = existing.clone();
                self.respawn(from, turn, existing, &candidate, &request, &resolution, step);
            } else {
                self.refuse_spawn(
                    from,
                    turn,
                    &request.name,
                    &format!(
                        "{} already names a member that is not your sub-agent",
                        existing.id
                    ),
                    step,
                );
            }
            return;
        }
        if self.config.sub_agent_count() >= MAX_SUB_AGENTS {
            self.refuse_spawn(
                from,
                turn,
                &request.name,
                &format!("this chat already has the maximum of {MAX_SUB_AGENTS} sub-agents"),
                step,
            );
            return;
        }
        if self.active_mode == TerminalMode::Team && !self.team_allows_add_members() {
            self.refuse_spawn(
                from,
                turn,
                &request.name,
                "team mode is locked to the current roster (modes.team.allow_add_members is off)",
                step,
            );
            return;
        }

        // Only a brand-new sub-agent is renamed: reuse above has already
        // matched an existing child by the name that was asked for.
        let renamed = replace_model_style_name(&mut candidate, request.model.as_deref(), |name| {
            self.config.find(name).is_some()
        });
        let id = candidate.id.clone();
        if !self.open_sub_agent(from, candidate, step) {
            self.refuse_spawn(
                from,
                turn,
                &request.name,
                "the roster rejected the new member",
                step,
            );
            return;
        }
        let mut notes = Vec::new();
        if renamed {
            // The parent addresses the child by name later, so it must learn
            // the real one.
            notes.push(format!(
                "Sub-agent opened as \"{id}\" (name sub-agents after their job, not their model)."
            ));
        }
        if resolution.changed {
            let summary = self
                .config
                .member(&id)
                .map(|child| named_summary(child, &resolution))
                .unwrap_or_default();
            notes.push(format!("Sub-agent \"{id}\" opened on {summary}."));
        }
        notes.extend(resolution.remark.clone());
        if !notes.is_empty() {
            self.send_system_note(from, turn, notes.join(" "), step);
        }
        self.deliver_task(from, turn, &id, request.task, step);
    }

    /// `@@close` from `from`: remove one of its sub-agents.
    fn handle_close(
        &mut self,
        from: &MemberId,
        turn: TurnId,
        request: CloseRequest,
        step: &mut RuntimeStep,
    ) {
        match self.closable_sub_agent(&request.name, Some(from)) {
            Ok(child) => self.begin_close(&child.id, CloseOrigin::Parent(turn), step),
            Err(reason) => self.refuse_close(from, turn, &request.name, &reason, step),
        }
    }

    /// The user asked to remove `member` (Agents list). A refusal is a notice.
    fn handle_remove_sub_agent(&mut self, member: &MemberId, step: &mut RuntimeStep) {
        match self.closable_sub_agent(member.as_str(), None) {
            Ok(child) => self.begin_close(&child.id, CloseOrigin::User, step),
            Err(reason) => step.events.push(RuntimeEvent::Notice(reason)),
        }
    }

    /// The sub-agent `name` stands for, if `requester` (a parent, or `None`
    /// for the user) may remove it now; else the reason it may not.
    fn closable_sub_agent(
        &self,
        name: &str,
        requester: Option<&MemberId>,
    ) -> Result<TeamMember, String> {
        let Some(member) = self.config.find(name).cloned() else {
            return Err(match requester {
                Some(_) => format!("there is no sub-agent named \"{name}\""),
                None => format!("there is no member named \"{name}\""),
            });
        };
        let display = member.display_name.clone();
        match (&member.parent, requester) {
            (None, Some(_)) => return Err(format!("{display} is not your sub-agent")),
            (None, None) => {
                return Err(format!(
                    "{display} is not a sub-agent, so it cannot be removed"
                ));
            }
            (Some(parent), Some(from)) if parent != from => {
                return Err(format!("{display} is not your sub-agent"));
            }
            _ => {}
        }
        if let Some(mode) = self.active_seat_mode(&member.id) {
            return Err(match requester {
                Some(_) => format!(
                    "{display} is in use by the running {mode}; close it after the run ends"
                ),
                None => format!(
                    "{display} is in use by the running {mode}; press Esc to cancel the run first"
                ),
            });
        }
        if self.closing.contains_key(&member.id) {
            return Err(format!("{display} is already being removed"));
        }
        Ok(member)
    }

    /// The mode whose running run has `member` in one of its seats.
    fn active_seat_mode(&self, member: &MemberId) -> Option<CollabMode> {
        self.mode_sessions
            .values()
            .find(|session| {
                [&session.builder, &session.reviewer, &session.leader]
                    .into_iter()
                    .chain(session.plan_builder.as_ref())
                    .chain(session.plan_reviewer.as_ref())
                    .chain(session.participants.iter())
                    .any(|seat| seat == member)
            })
            .map(|session| session.mode)
    }

    /// Stop whatever `child` is doing, then take it off the roster. A run
    /// that is still winding down finishes the removal when it exits.
    fn begin_close(&mut self, child: &MemberId, origin: CloseOrigin, step: &mut RuntimeStep) {
        // Same as Esc on this one member, plus: nothing queued for it survives
        // and nothing waits on an approval for it.
        self.goal_pause_for_interrupt(Some(child), step);
        let dropped: Vec<TurnId> = self
            .members
            .get_mut(child)
            .map(|state| state.queue.drain(..).map(|queued| queued.turn).collect())
            .unwrap_or_default();
        if !dropped.is_empty() {
            self.emit_queue_updated(child, step);
        }
        let held: Vec<ApprovalId> = self
            .held_approvals
            .iter()
            .filter(|(_, held)| held.targets.contains(child))
            .map(|(id, _)| *id)
            .collect();
        for id in held {
            self.handle_approval(id, ApprovalDecision::Reject, step);
        }
        self.handle_cancel(Some(child.clone()), step);
        for turn in dropped {
            self.check_turn_complete(turn, step);
        }
        let running = self
            .members
            .get(child)
            .is_some_and(|state| state.running.is_some());
        if running {
            self.closing.insert(child.clone(), origin);
            return;
        }
        self.finish_close(child, origin, false, step);
    }

    /// A member's run just ended: if it was being removed, remove it now.
    fn finish_pending_close(&mut self, member: &MemberId, step: &mut RuntimeStep) {
        let Some(origin) = self.closing.get(member).copied() else {
            return;
        };
        let still_running = self
            .members
            .get(member)
            .is_some_and(|state| state.running.is_some());
        if still_running {
            return;
        }
        self.closing.remove(member);
        if self.config.member(member).is_some() {
            self.finish_close(member, origin, true, step);
        }
    }

    /// Take `child` off the roster, tell the parent, and say so in the chat.
    /// `deferred` is set when the child had a run to stop first, so the
    /// parent's turn that asked may be over. False when the roster refused.
    fn finish_close(
        &mut self,
        child: &MemberId,
        origin: CloseOrigin,
        deferred: bool,
        step: &mut RuntimeStep,
    ) -> bool {
        let Some(member) = self.config.member(child).cloned() else {
            return false;
        };
        let Some(parent) = member.parent.clone() else {
            return false;
        };
        let members: Vec<TeamMember> = self
            .config
            .members
            .iter()
            .filter(|other| other.id != *child)
            .cloned()
            .collect();
        let default_target = match &self.config.default_target {
            Some(DefaultTarget::Member(id)) if id == child => None,
            other => other.clone(),
        };
        if !self.replace_team(members, default_target, false, step) {
            if let CloseOrigin::Parent(turn) = origin
                && !deferred
            {
                self.refuse_close(&parent, turn, child.as_str(), "the roster rejected the change", step);
            }
            return false;
        }
        self.protocol_stale.remove(child);
        let departed = DepartedMember {
            id: child.clone(),
            display_name: member.display_name.clone(),
            backend: member.backend,
            model: member.model.clone(),
            effort: member.effort,
        };
        // Saved with the chat, so its earlier messages keep their labels after
        // a restart or a /resume.
        if let Err(err) = self.store.record_departed_member(&departed) {
            self.report_store_error("remember the removed sub-agent", err, step);
        }
        step.events.push(RuntimeEvent::SubAgentClosed {
            parent: parent.clone(),
            child: child.clone(),
            by_user: matches!(origin, CloseOrigin::User),
            departed,
        });
        if self.config.member(&parent).is_none() {
            return true;
        }
        let turn = match origin {
            CloseOrigin::Parent(turn) if !deferred => Some(turn),
            _ => self.start_note_turn(step),
        };
        if let Some(turn) = turn {
            let note = match origin {
                CloseOrigin::User => format!("Sub-agent \"{child}\" was removed by the user."),
                CloseOrigin::Parent(_) => format!("Sub-agent \"{child}\" closed."),
            };
            self.send_system_note(&parent, turn, note, step);
        }
        true
    }

    /// A new turn of its own, for a note nobody's current turn owns.
    fn start_note_turn(&mut self, step: &mut RuntimeStep) -> Option<TurnId> {
        match self.store.create_turn() {
            Ok(turn) => {
                step.events.push(RuntimeEvent::TurnStarted { turn });
                Some(turn)
            }
            Err(err) => {
                step.events
                    .push(RuntimeEvent::Notice(format!("store error: {err}")));
                None
            }
        }
    }

    /// Tell `from` why a `@@close` failed, once, like a refused spawn.
    fn refuse_close(
        &mut self,
        from: &MemberId,
        turn: TurnId,
        name: &str,
        reason: &str,
        step: &mut RuntimeStep,
    ) {
        let note = format!("Your @@close for \"{name}\" was refused: {reason}. Nothing was changed.");
        self.send_system_note(from, turn, note, step);
    }

    /// Check the backend, model, and effort an `@@spawn` names against the
    /// installed backends and their model lists. The returned request names the
    /// exact model id; the error is the reason to refuse. A backend whose list
    /// is not known passes through unchanged.
    fn resolve_spawn_request(
        &self,
        parent: &TeamMember,
        mut request: SpawnRequest,
    ) -> Result<(SpawnRequest, SpawnResolution), String> {
        let mut resolution = SpawnResolution::default();
        // A request for a sub-agent that already exists is about that
        // sub-agent: what it does not name follows the child, not the parent.
        let child = self
            .config
            .find(&normalize_member_id(&request.name, ""))
            .filter(|child| child.parent.as_ref() == Some(&parent.id));
        let default_backend = child.map_or(parent.backend, |child| child.backend);
        if request.backend.is_none() && child.is_some() {
            request.backend = Some(default_backend.as_str().to_string());
        }
        // A bad backend or effort word is reported by `sub_agent_member`.
        let backend = match request.backend.as_deref() {
            Some(raw) => match BackendKind::try_from(raw.to_ascii_lowercase().as_str()) {
                Ok(backend) => backend,
                Err(_) => return Ok((request, resolution)),
            },
            None => parent.backend,
        };
        if request.backend.is_some() && backend != default_backend && self.models.lacks(backend) {
            return Err(format!(
                "{backend} is not installed, so it cannot run a sub-agent"
            ));
        }
        request.model = request
            .model
            .take()
            .map(|model| model.trim().to_string())
            .filter(|model| !model.is_empty());
        let mut chosen = None;
        if let Some(asked) = request.model.clone() {
            if let Some(result) = self.models.resolve(backend, &asked) {
                let resolved = result?;
                resolution.changed = resolved.id != asked;
                resolution.remark = resolved.remark.clone();
                request.model = Some(resolved.id.clone());
                chosen = Some(resolved);
            }
        } else {
            // No model named: the one the sub-agent will run.
            let inherited = match child {
                Some(child) => child.model.as_deref(),
                None => (backend == parent.backend)
                    .then_some(parent.model.as_deref())
                    .flatten(),
            };
            chosen = self.models.effective(backend, inherited);
        }
        if let (Some(raw), Some(model)) = (request.effort.as_deref(), chosen.as_ref())
            && let Some(effort) = Effort::parse(raw)
            && let Some(reason) = effort_refusal(model, effort)
        {
            return Err(reason);
        }
        resolution.model = chosen;
        Ok((request, resolution))
    }

    /// A sub-agent keeps an effort inherited from its parent only when the
    /// model it runs offers it.
    fn fit_inherited_effort(&self, child: &mut TeamMember, request: &SpawnRequest) {
        if request.effort.is_some() {
            return;
        }
        self.fit_effort(child);
    }

    fn fit_effort(&self, child: &mut TeamMember) {
        let Some(effort) = child.effort else {
            return;
        };
        if let Some(model) = self
            .models
            .effective(child.backend, child.model.as_deref())
            && effort_refusal(&model, effort).is_some()
        {
            child.effort = None;
        }
    }

    /// `@@spawn` for a sub-agent that already exists: switch it to the backend,
    /// model, or effort the request names (when they differ), then hand it the
    /// task. Fields the request leaves out keep the sub-agent's values.
    #[allow(clippy::too_many_arguments)]
    fn respawn(
        &mut self,
        from: &MemberId,
        turn: TurnId,
        existing: TeamMember,
        candidate: &TeamMember,
        request: &SpawnRequest,
        resolution: &SpawnResolution,
        step: &mut RuntimeStep,
    ) {
        let child = existing.id.clone();
        let mut updated = existing.clone();
        let backend_changes = request.backend.is_some() && candidate.backend != existing.backend;
        if backend_changes {
            // Same as `/model` onto another CLI: that CLI's permission preset,
            // and a fresh native session.
            updated.backend = candidate.backend;
            crate::domain::config::apply_backend_defaults(&mut updated);
            updated.session_id = None;
            updated.model = request.model.as_ref().and(candidate.model.clone());
            updated.effort = existing
                .effort
                .filter(|effort| effort.supported_by(updated.backend));
        } else if request.model.is_some() {
            updated.model = candidate.model.clone();
        }
        if request.effort.is_some() {
            updated.effort = candidate.effort;
            // The check in `resolve_spawn_request` looked at the model a new
            // sub-agent would get; this one keeps its own.
            if let (Some(effort), Some(model)) = (
                updated.effort,
                self.models
                    .effective(updated.backend, updated.model.as_deref()),
            ) && let Some(reason) = effort_refusal(&model, effort)
            {
                self.refuse_spawn(from, turn, &request.name, &reason, step);
                return;
            }
        } else {
            self.fit_effort(&mut updated);
        }
        if updated == existing {
            self.deliver_task(from, turn, &child, request.task.clone(), step);
            return;
        }
        let busy = self
            .members
            .get(&child)
            .is_some_and(|state| state.running.is_some() || !state.queue.is_empty());
        if busy {
            self.refuse_spawn(
                from,
                turn,
                &request.name,
                &format!(
                    "{child} is still working, so its model cannot change now. Wait for its hand-back, \
                     or send @@spawn without backend, model, and effort to queue more work for it as it is"
                ),
                step,
            );
            return;
        }
        let mut members = self.config.members.clone();
        if let Some(slot) = members.iter_mut().find(|member| member.id == child) {
            *slot = updated.clone();
        }
        let default_target = self.config.default_target.clone();
        if !self.replace_team(members, default_target, false, step) {
            self.refuse_spawn(from, turn, &request.name, "the roster rejected the change", step);
            return;
        }
        let summary = named_summary(&updated, resolution);
        step.events.push(RuntimeEvent::MemberNotice {
            member: child.clone(),
            text: format!("Model: {} → {summary}", self.member_display(&child)),
        });
        let mut note = format!("Sub-agent \"{child}\" switched to {summary}.");
        if let Some(remark) = &resolution.remark {
            note.push(' ');
            note.push_str(remark);
        }
        self.send_system_note(from, turn, note, step);
        self.deliver_task(from, turn, &child, request.task.clone(), step);
    }

    /// Add `candidate` to the roster as a sub-agent of `parent` and announce
    /// it. False when the roster rejects it.
    fn open_sub_agent(
        &mut self,
        parent: &MemberId,
        candidate: TeamMember,
        step: &mut RuntimeStep,
    ) -> bool {
        let id = candidate.id.clone();
        let summary = sub_agent_summary(&candidate);
        let mut members = self.config.members.clone();
        members.push(candidate);
        let default_target = self.config.default_target.clone();
        if !self.replace_team(members, default_target, false, step) || self.config.member(&id).is_none()
        {
            return false;
        }
        step.events.push(RuntimeEvent::SubAgentSpawned {
            parent: parent.clone(),
            child: id,
            summary,
        });
        true
    }

    /// Open the sub-agents a mode run needs for seats the roster cannot fill,
    /// under the main member and with its model. Nothing is opened when they
    /// do not all fit. False means the run must not start (a notice says why).
    fn open_auto_seats(&mut self, mode: CollabMode, seats: &[AutoSeat], step: &mut RuntimeStep) -> bool {
        if seats.is_empty() {
            return true;
        }
        let in_use = self.config.sub_agent_count();
        if in_use + seats.len() > MAX_SUB_AGENTS {
            step.events.push(RuntimeEvent::Notice(format!(
                "{mode} needs {} more sub-agent{} but this chat already has {in_use} of {MAX_SUB_AGENTS}",
                seats.len(),
                if seats.len() == 1 { "" } else { "s" },
            )));
            return false;
        }
        let parent_id = auto_seat_parent(&self.config);
        let Some(parent) = self.config.member(&parent_id).cloned() else {
            return false;
        };
        let mut candidates = Vec::with_capacity(seats.len());
        for seat in seats {
            let request = SpawnRequest {
                name: seat.id.to_string(),
                task: String::new(),
                backend: None,
                model: None,
                effort: None,
                role: Some(seat.role.clone()),
            };
            match sub_agent_member(&parent, &request) {
                Ok(candidate) if candidate.id == seat.id => candidates.push(candidate),
                Ok(_) | Err(_) => {
                    step.events.push(RuntimeEvent::Notice(format!(
                        "could not open the {} sub-agent for {mode}",
                        seat.id
                    )));
                    return false;
                }
            }
        }
        for candidate in candidates {
            let name = candidate.id.to_string();
            if !self.open_sub_agent(&parent_id, candidate, step) {
                step.events.push(RuntimeEvent::Notice(format!(
                    "could not open the {name} sub-agent for {mode}"
                )));
                return false;
            }
        }
        true
    }

    /// Send `task` to a sub-agent as an ordinary message from its parent.
    fn deliver_task(
        &mut self,
        from: &MemberId,
        turn: TurnId,
        child: &MemberId,
        task: String,
        step: &mut RuntimeStep,
    ) {
        self.route_team_message(
            from,
            turn,
            TeamMessage {
                to: vec![RouteTo::Member(child.to_string())],
                kind: None,
                body: task,
            },
            step,
        );
    }

    /// Tell `from` why a spawn failed, once, as a system message that the relay
    /// guard counts like any other automatic relay. The message also shows in
    /// the chat as a row from Asterline, so no separate notice repeats it.
    fn refuse_spawn(
        &mut self,
        from: &MemberId,
        turn: TurnId,
        name: &str,
        reason: &str,
        step: &mut RuntimeStep,
    ) {
        let note = format!(
            "Your @@spawn for \"{name}\" was refused: {reason}. No sub-agent was created."
        );
        self.send_system_note(from, turn, note, step);
    }

    /// Tell `from` something once, as a system message that the relay guard
    /// counts like any other automatic relay.
    fn send_system_note(
        &mut self,
        from: &MemberId,
        turn: TurnId,
        note: String,
        step: &mut RuntimeStep,
    ) {
        // Shown in the chat like any other message the parent receives.
        let system = MemberId::new(SYSTEM_SENDER);
        let to = vec![from.to_string()];
        match self.store.record_route(turn, &system, &to, &note) {
            Ok(_) => step.events.push(RuntimeEvent::Route {
                turn,
                from: system,
                to,
                body: note.clone(),
            }),
            Err(err) => self.report_store_error("save a system note", err, step),
        }
        let prompt = format!(
            "[system message from Asterline]\n{}\n{note}",
            team_skill_hint()
        );
        if self.relay_paused {
            self.pause_route_to(turn, from, vec![from.clone()], prompt, "relay paused by user", step);
            return;
        }
        match self.relay.record_auto_relay(turn, from) {
            RelayDecision::Continue { .. } => {
                self.enqueue_prompt_from(from, turn, prompt, None, step);
            }
            RelayDecision::Pause { count } => self.pause_route_to(
                turn,
                from,
                vec![from.clone()],
                prompt,
                &format!("auto-relay limit reached ({count})"),
                step,
            ),
        }
    }

    /// A sub-agent that writes to the member who assigned its task has
    /// answered it; its closing reply is then not handed back a second time.
    fn note_origin_messaged(&mut self, from: &MemberId, recipients: &[MemberId]) {
        if let Some(running) = self
            .members
            .get_mut(from)
            .and_then(|state| state.running.as_mut())
            && running
                .origin
                .as_ref()
                .is_some_and(|origin| recipients.contains(origin))
        {
            running.messaged_origin = true;
        }
    }

    /// When a sub-agent finishes a run that answered a message from its
    /// parent, send the parent its final reply (or a short note when the run
    /// failed or said nothing). Runs started by the user are never forwarded.
    fn hand_back_to_parent(
        &mut self,
        child: &MemberId,
        turn: TurnId,
        outcome: SubAgentOutcome,
        failed: bool,
        step: &mut RuntimeStep,
    ) {
        let Some(parent) = self
            .config
            .member(child)
            .and_then(|member| member.parent.clone())
        else {
            return;
        };
        if outcome.origin.as_ref() != Some(&parent) || outcome.messaged_origin {
            return;
        }
        if self.config.member(&parent).is_none() {
            return;
        }
        let display = self.member_display(child);
        let reply = outcome.last_reply.trim();
        let body = if failed {
            match outcome.error.as_deref().map(failure_reason).filter(|r| !r.is_empty()) {
                Some(reason) => format!("{display} could not finish the task: {reason}"),
                None => format!(
                    "{display} could not finish the task: its run failed before it gave a final reply."
                ),
            }
        } else if reply.is_empty() {
            format!("{display} finished the task but gave no text reply.")
        } else {
            reply.to_string()
        };
        self.route_team_message(
            child,
            turn,
            TeamMessage {
                to: vec![RouteTo::Member(parent.to_string())],
                kind: Some(HANDBACK_KIND.to_string()),
                body,
            },
            step,
        );
    }
}

/// What checking an `@@spawn` against the model lists found.
#[derive(Default)]
struct SpawnResolution {
    /// The model the sub-agent runs, as the list describes it.
    model: Option<ResolvedModel>,
    /// The model id differs from what the agent wrote.
    changed: bool,
    /// Extra wording for the parent (several models matched).
    remark: Option<String>,
}

/// `backend · model (Display Name) · effort`; the name only when the list gave one.
fn named_summary(member: &TeamMember, resolution: &SpawnResolution) -> String {
    let mut parts = vec![member.backend.as_str().to_string()];
    if let Some(model) = &member.model {
        match resolution.model.as_ref().filter(|r| &r.id == model) {
            Some(resolved) if !resolved.name.is_empty() && resolved.name != resolved.id => {
                parts.push(format!("{model} ({})", resolved.name))
            }
            _ => parts.push(model.clone()),
        }
    }
    if let Some(effort) = member.effort {
        parts.push(effort.as_str().to_string());
    }
    parts.join(" · ")
}

/// The backend's error text, cut to something a hand-back can carry.
fn failure_reason(error: &str) -> String {
    const MAX_CHARS: usize = 400;
    let flat = error.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= MAX_CHARS {
        return flat;
    }
    let cut: String = flat.chars().take(MAX_CHARS).collect();
    format!("{cut}…")
}

/// `backend[ · model][ · effort]` for the "spawned" notice.
fn sub_agent_summary(member: &TeamMember) -> String {
    let mut parts = vec![member.backend.as_str().to_string()];
    if let Some(model) = &member.model {
        parts.push(model.clone());
    }
    if let Some(effort) = member.effort {
        parts.push(effort.as_str().to_string());
    }
    parts.join(" · ")
}

/// The wording for a task a parent gives its sub-agent.
fn sub_agent_task_prompt(from: &MemberId, from_display: &str, body: &str) -> String {
    format!(
        "{SUB_AGENT_TASK_MARKER}{from_display} ({from})]\n{}\n\
         {from_display} opened you as a sub-agent for this task. Your final reply is handed back to \
         {from_display} automatically, so do not send it with @@team_message. You cannot open sub-agents.\n\n{body}",
        team_skill_hint()
    )
}

/// The wording for a sub-agent's result arriving at its parent.
fn handback_prompt(from: &MemberId, from_display: &str, body: &str) -> String {
    format!(
        "{HANDBACK_MARKER}{from_display} ({from})]\n{}\n\
         {from_display} is your sub-agent; this is its final reply to your task. No acknowledgement is needed.\n\n{body}",
        team_skill_hint()
    )
}
