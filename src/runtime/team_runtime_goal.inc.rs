// The persistent goal of the main agent (`/goal`): commands, the continuation
// loop that keeps the main agent working turn after turn, and its stop
// conditions. Included into `team_runtime.rs` so the private runtime fields
// stay reachable.
//
// The loop has no turn, token or time cap. It stops when the agent says the
// goal is complete (`@@goal_complete`), the user pauses or clears it, the main
// agent is interrupted or fails, or the no-progress guard trips.

/// What the goal loop remembers about one turn the goal's member took part in.
#[derive(Default)]
struct GoalTurn {
    /// The loop started this turn on its own (a continuation prompt).
    continuation: bool,
    /// The member replied with text or used a tool during this turn.
    produced: bool,
}

/// The longest error text kept in a pause reason.
const GOAL_REASON_MAX_CHARS: usize = 100;

impl TeamRuntime {
    /// The goal as the TUI should see it at startup; `None` when the chat has
    /// no goal.
    pub fn goal_event(&self) -> Option<RuntimeEvent> {
        self.goal.as_ref().map(|goal| RuntimeEvent::GoalUpdated {
            goal: Some(goal.clone()),
        })
    }

    /// Load the active chat's saved goal. A goal that was active when the chat
    /// was left comes back paused: a goal never runs by itself on launch or
    /// when a chat is resumed.
    fn restore_goal(&mut self) -> Result<(), rusqlite::Error> {
        let mut goal = self.store.goal_for(self.store.active_conversation())?;
        if let Some(goal) = goal.as_mut()
            && goal.status == GoalStatus::Active
        {
            goal.status = GoalStatus::Paused;
            goal.last_reason = Some(REASON_RESTORED.to_string());
            goal.no_progress_streak = 0;
            self.store.save_goal(goal)?;
        }
        self.goal = goal;
        self.goal_turns.clear();
        Ok(())
    }

    /// Save the goal and tell the TUI.
    fn sync_goal(&self, step: &mut RuntimeStep) {
        let result = match &self.goal {
            Some(goal) => self.store.save_goal(goal),
            None => self.store.clear_goal(),
        };
        if let Err(err) = result {
            self.report_store_error("save the goal", err, step);
        }
        step.events.push(RuntimeEvent::GoalUpdated {
            goal: self.goal.clone(),
        });
    }

    /// Who works toward a goal: the `main` member, else the first top-level
    /// member, else the team's default target (what `/new <prompt>` uses).
    fn goal_member(&self) -> Option<MemberId> {
        match self.main_message_target() {
            MessageTarget::Member(id) => Some(id),
            _ => self.config.default_member_ids().into_iter().next(),
        }
    }

    fn handle_goal_command(&mut self, action: GoalAction, step: &mut RuntimeStep) {
        match action {
            GoalAction::Show => self.goal_show(step),
            GoalAction::Set(raw) => self.goal_set(&raw, step),
            GoalAction::Pause => self.goal_pause(step),
            GoalAction::Resume => self.goal_resume(step),
            GoalAction::Clear => self.goal_clear(step),
        }
    }

    fn goal_notice(step: &mut RuntimeStep, text: impl Into<String>) {
        step.events.push(RuntimeEvent::Notice(text.into()));
    }

    /// Goals run in normal mode only; the other modes own the dispatch.
    fn goal_requires_normal_mode(&self, step: &mut RuntimeStep) -> bool {
        if self.active_mode == TerminalMode::Normal {
            return true;
        }
        Self::goal_notice(
            step,
            format!(
                "a goal runs only in normal mode (this chat is in {} mode); use /mode normal first",
                self.active_mode
            ),
        );
        false
    }

    fn goal_show(&self, step: &mut RuntimeStep) {
        let Some(goal) = &self.goal else {
            Self::goal_notice(step, "no goal is set; use /goal <objective> to set one");
            return;
        };
        let objective = single_line(&goal.objective, 300);
        let age = format_goal_age(goal.age_secs(unix_now()));
        let mut text = format!(
            "Goal ({}): {objective} · {} turn{} · {age}",
            goal.status.as_str(),
            goal.turns,
            if goal.turns == 1 { "" } else { "s" },
        );
        match (goal.status, &goal.last_reason) {
            (GoalStatus::Paused, Some(reason)) => {
                text.push_str(&format!(" · {reason} (/goal resume)"));
            }
            (GoalStatus::Paused, None) => text.push_str(" (/goal resume)"),
            (GoalStatus::Complete, Some(summary)) => text.push_str(&format!(" · {summary}")),
            _ => {}
        }
        Self::goal_notice(step, text);
    }

    /// `/goal <objective>`: a new goal, or a new objective for the current one.
    fn goal_set(&mut self, raw: &str, step: &mut RuntimeStep) {
        let objective = match normalize_objective(raw) {
            Ok(objective) => objective,
            Err(message) => {
                Self::goal_notice(step, message);
                return;
            }
        };
        if !self.goal_requires_normal_mode(step) {
            return;
        }
        let Some(member) = self.goal_member() else {
            Self::goal_notice(step, "no member can take a goal");
            return;
        };
        let previous = self.goal.clone();
        let edited = previous
            .as_ref()
            .is_some_and(|goal| goal.status != GoalStatus::Complete);
        let mut goal = match previous.clone().filter(|_| edited) {
            Some(mut goal) => {
                goal.objective = objective.clone();
                goal.status = GoalStatus::Active;
                goal.last_reason = None;
                goal.no_progress_streak = 0;
                goal.member = member.clone();
                goal
            }
            None => GoalState::new(objective.clone(), member.clone()),
        };
        goal.turns += 1;
        let prompt = if edited {
            objective_edited_prompt(&goal)
        } else {
            initial_prompt(&goal)
        };
        self.goal = Some(goal);
        // The user band shows the objective; the model gets the goal prompt.
        // When the member is mid-turn the prompt queues as its next turn.
        if self
            .dispatch_user_turn(MessageTarget::Member(member), objective, prompt, step)
            .is_none()
        {
            self.goal = previous;
            return;
        }
        self.sync_goal(step);
        if edited {
            Self::goal_notice(step, "Goal objective updated");
        }
    }

    fn goal_pause(&mut self, step: &mut RuntimeStep) {
        match self.goal.as_mut() {
            None => Self::goal_notice(step, "no goal is set"),
            Some(goal) if goal.status == GoalStatus::Complete => {
                Self::goal_notice(step, "the goal is already complete");
            }
            Some(goal) if goal.status == GoalStatus::Paused => {
                Self::goal_notice(step, "the goal is already paused; /goal resume continues it");
            }
            Some(goal) => {
                goal.status = GoalStatus::Paused;
                goal.last_reason = Some(REASON_USER_PAUSE.to_string());
                self.sync_goal(step);
                Self::goal_notice(
                    step,
                    "Goal paused; the current turn finishes first. /goal resume continues it",
                );
            }
        }
    }

    fn goal_resume(&mut self, step: &mut RuntimeStep) {
        match self.goal.as_ref().map(|goal| goal.status) {
            None => {
                Self::goal_notice(step, "no goal is set; use /goal <objective> to set one");
                return;
            }
            Some(GoalStatus::Active) => {
                Self::goal_notice(step, "the goal is already active");
                return;
            }
            Some(GoalStatus::Complete) => {
                Self::goal_notice(
                    step,
                    "the goal is complete; use /goal <objective> to set a new one",
                );
                return;
            }
            Some(GoalStatus::Paused) => {}
        }
        if !self.goal_requires_normal_mode(step) {
            return;
        }
        if let Some(goal) = self.goal.as_mut() {
            goal.status = GoalStatus::Active;
            goal.last_reason = None;
            goal.no_progress_streak = 0;
        }
        self.sync_goal(step);
        Self::goal_notice(step, "Goal resumed");
        self.goal_continue_if_idle(step);
    }

    fn goal_clear(&mut self, step: &mut RuntimeStep) {
        if self.goal.take().is_none() {
            Self::goal_notice(step, "no goal is set");
            return;
        }
        self.sync_goal(step);
        Self::goal_notice(step, "Goal cleared");
    }

    /// Pause an active goal and record why; the status line shows the reason, so
/// no chat notice is posted. Returns whether it was active.
    fn goal_pause_with(&mut self, reason: String, step: &mut RuntimeStep) -> bool {
        let Some(goal) = self.goal.as_mut() else {
            return false;
        };
        if goal.status != GoalStatus::Active {
            return false;
        }
        goal.status = GoalStatus::Paused;
        goal.last_reason = Some(reason);
        self.sync_goal(step);
        true
    }

    /// Esc on the goal's member (or on everyone) pauses the goal.
    fn goal_pause_for_interrupt(&mut self, member: Option<&MemberId>, step: &mut RuntimeStep) {
        let Some(goal) = &self.goal else {
            return;
        };
        if member.is_some_and(|member| member != &goal.member) {
            return;
        }
        self.goal_pause_with(REASON_INTERRUPT.to_string(), step);
    }

    /// The goal's member failed its turn: stop and say why.
    fn goal_pause_for_failure(&mut self, member: &MemberId, error: &str, step: &mut RuntimeStep) {
        if self.goal.as_ref().is_none_or(|goal| &goal.member != member) {
            return;
        }
        // The status line above the composer shows the reason; no chat notice.
        let reason = format!("Paused: {}", single_line(error, GOAL_REASON_MAX_CHARS));
        self.goal_pause_with(reason, step);
    }

    /// Another mode took over the dispatch, so the loop must not keep running.
    fn goal_pause_for_mode(&mut self, mode: TerminalMode, step: &mut RuntimeStep) {
        if mode != TerminalMode::Normal {
            self.goal_pause_with(format!("Paused: switched to {mode} mode"), step);
        }
    }

    /// Note that `member` produced something (reply text, a tool call, a file
    /// change or a control line) during `turn`, for the no-progress guard.
    fn goal_note_output(&mut self, member: &MemberId, turn: TurnId) {
        if self.goal.as_ref().is_some_and(|goal| &goal.member == member) {
            self.goal_turns.entry(turn).or_default().produced = true;
        }
    }

    /// The goal's member is about to run a turn; remember it took part.
    fn goal_note_participation(&mut self, member: &MemberId, turn: TurnId) {
        if self.goal.as_ref().is_some_and(|goal| &goal.member == member) {
            self.goal_turns.entry(turn).or_default();
        }
    }

    /// `@@goal_complete` from `member`, honoured only from the goal's member
    /// while the goal is active.
    fn goal_complete_from_agent(
        &mut self,
        member: &MemberId,
        summary: Option<String>,
        step: &mut RuntimeStep,
    ) {
        let honoured = self
            .goal
            .as_ref()
            .is_some_and(|goal| &goal.member == member && goal.status == GoalStatus::Active);
        if !honoured {
            self.log(
                member,
                LogEntry::warn(
                    member.as_str(),
                    "ignored @@goal_complete: no active goal for this member",
                ),
                step,
            );
            return;
        }
        let summary = summary.map(|summary| single_line(&summary, 300));
        if let Some(goal) = self.goal.as_mut() {
            goal.status = GoalStatus::Complete;
            goal.no_progress_streak = 0;
            goal.last_reason = summary.clone();
        }
        self.sync_goal(step);
        Self::goal_notice(
            step,
            match summary {
                Some(summary) => format!("Goal complete: {summary}"),
                None => "Goal complete".to_string(),
            },
        );
    }

    /// A turn tree finished (no member or run of it is active any more). When
    /// the goal's member took part and the goal is still active, judge the
    /// turn and send the next continuation.
    fn goal_on_turn_finished(&mut self, turn: TurnId, step: &mut RuntimeStep) {
        let Some(info) = self.goal_turns.remove(&turn) else {
            return;
        };
        let Some(goal) = self.goal.as_mut() else {
            return;
        };
        if goal.status != GoalStatus::Active {
            return;
        }
        let streak_before = goal.no_progress_streak;
        if info.produced {
            goal.no_progress_streak = 0;
        } else if info.continuation {
            goal.no_progress_streak += 1;
        }
        if goal.no_progress_streak >= CONTINUATION_NO_PROGRESS_CAP {
            self.goal_pause_with(REASON_NO_PROGRESS.to_string(), step);
            return;
        }
        if goal.no_progress_streak != streak_before {
            self.sync_goal(step);
        }
        self.goal_continue_if_idle(step);
    }

    /// Send a continuation when the goal is active and its member has nothing
    /// running or queued. Anything else already running or queued finishes
    /// first and judges the goal when its own turn ends.
    fn goal_continue_if_idle(&mut self, step: &mut RuntimeStep) {
        let Some(goal) = &self.goal else {
            return;
        };
        if goal.status != GoalStatus::Active || self.active_mode != TerminalMode::Normal {
            return;
        }
        let idle = self
            .members
            .get(&goal.member)
            .is_none_or(|state| state.running.is_none() && state.queue.is_empty());
        if idle {
            self.goal_send_continuation(step);
        }
    }

    fn goal_send_continuation(&mut self, step: &mut RuntimeStep) {
        let Some(member) = self.goal.as_ref().map(|goal| goal.member.clone()) else {
            return;
        };
        if !self.members.contains_key(&member) {
            let reason = format!("Paused: {member} is no longer on the team");
            self.goal_pause_with(reason, step);
            return;
        }
        let turn = match self.store.create_turn() {
            Ok(turn) => turn,
            Err(err) => {
                self.report_store_error("start the next goal turn", err, step);
                self.goal_pause_with("Paused: could not save the next turn".to_string(), step);
                return;
            }
        };
        let Some(goal) = self.goal.as_mut() else {
            return;
        };
        goal.turns += 1;
        let number = goal.turns;
        let prompt = continuation_prompt(goal);
        if let Err(err) = self.store.record_goal_marker(turn, &member, number) {
            self.report_store_error("save the goal marker", err, step);
        }
        self.goal_turns.insert(
            turn,
            GoalTurn {
                continuation: true,
                produced: false,
            },
        );
        step.events.push(RuntimeEvent::TurnStarted { turn });
        step.events.push(RuntimeEvent::GoalMarker {
            member: member.clone(),
            turn: number,
        });
        self.sync_goal(step);
        self.enqueue_prompt(&member, turn, prompt, step);
    }
}

/// `text` on one line, cut to `max` characters with an ellipsis.
fn single_line(text: &str, max: usize) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= max {
        return collapsed;
    }
    let mut cut: String = collapsed.chars().take(max.saturating_sub(1)).collect();
    cut.push('…');
    cut
}
