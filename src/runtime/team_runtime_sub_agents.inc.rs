// Sub-agents: members a top-level member opens with `@@spawn`. They live in
// one conversation, take work from their parent, and hand their final reply
// back on their own. Included into `team_runtime.rs` so the private runtime
// fields stay reachable.

/// `TeamMessage::kind` of a sub-agent's automatic result.
const HANDBACK_KIND: &str = "handback";

/// What a finished run needs to decide whether its result goes back to the
/// member that assigned it.
#[derive(Default)]
struct SubAgentOutcome {
    origin: Option<MemberId>,
    messaged_origin: bool,
    last_reply: String,
}

impl SubAgentOutcome {
    fn new(running: &RunningState) -> Self {
        Self {
            origin: running.origin.clone(),
            messaged_origin: running.messaged_origin,
            last_reply: running.last_reply.clone(),
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
        let mut candidate = match sub_agent_member(&parent, &request) {
            Ok(candidate) => candidate,
            Err(reason) => {
                self.refuse_spawn(from, turn, &request.name, &reason, step);
                return;
            }
        };

        let existing = self
            .config
            .find(candidate.id.as_str())
            .or_else(|| self.config.find(&candidate.display_name));
        if let Some(existing) = existing {
            if existing.id == candidate.id && existing.parent.as_ref() == Some(from) {
                // Reuse: the sub-agent keeps its model and session; only the
                // new task goes to it.
                self.deliver_task(from, turn, &existing.id.clone(), request.task, step);
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
        if renamed {
            // The parent addresses the child by name later, so it must learn
            // the real one.
            let note = format!(
                "Sub-agent opened as \"{id}\" (name sub-agents after their job, not their model)."
            );
            self.send_system_note(from, turn, note, step);
        }
        self.deliver_task(from, turn, &id, request.task, step);
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

    /// Tell the user why a spawn failed, and tell `from` once, as a system
    /// message that the relay guard counts like any other automatic relay.
    fn refuse_spawn(
        &mut self,
        from: &MemberId,
        turn: TurnId,
        name: &str,
        reason: &str,
        step: &mut RuntimeStep,
    ) {
        step.events.push(RuntimeEvent::MemberNotice {
            member: from.clone(),
            text: format!(
                "{} could not spawn {name}: {reason}",
                self.member_display(from)
            ),
        });
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
            format!("{display} could not finish the task: its run failed before it gave a final reply.")
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
