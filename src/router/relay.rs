//! Structured relay guard.
//!
//! Stops runaway agent-to-agent chatter without limiting real work. The limit
//! is the number of *consecutive* automatic relays per sender with no member
//! doing any work in between: [`RelayGuard::note_progress`] (called when a tool
//! runs or a file changes) clears every sender's count for that turn. Counting
//! is keyed by `(turn, sender)`, so each member gets an independent budget, and
//! a new user turn starts fresh.

use std::collections::HashMap;

use crate::domain::event::TurnId;
use crate::domain::team::{DEFAULT_MAX_AUTO_RELAYS, MemberId};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RelayDecision {
    Continue { count: u32 },
    Pause { count: u32 },
}

#[derive(Clone, Debug)]
pub struct RelayGuard {
    max_auto_relays: u32,
    counts: HashMap<(TurnId, MemberId), u32>,
}

impl RelayGuard {
    pub fn new(max_auto_relays: u32) -> Self {
        Self {
            max_auto_relays,
            counts: HashMap::new(),
        }
    }

    pub fn with_default_limit() -> Self {
        Self::new(DEFAULT_MAX_AUTO_RELAYS)
    }

    /// Record one automatic relay initiated by `sender` during `turn`.
    /// Returns [`RelayDecision::Pause`] once the sender exceeds the limit.
    pub fn record_auto_relay(&mut self, turn: TurnId, sender: &MemberId) -> RelayDecision {
        let count = self.counts.entry((turn, sender.clone())).or_insert(0);
        *count = count.saturating_add(1);

        if *count > self.max_auto_relays {
            RelayDecision::Pause { count: *count }
        } else {
            RelayDecision::Continue { count: *count }
        }
    }

    /// Real work happened in `turn` (a tool call or a file change): forget every
    /// sender's relay count for it, so only relays with no work in between add up.
    pub fn note_progress(&mut self, turn: TurnId) {
        self.counts.retain(|(t, _), _| *t != turn);
    }

    /// Forget all counters for a finished turn.
    pub fn reset_turn(&mut self, turn: TurnId) {
        self.counts.retain(|(t, _), _| *t != turn);
    }
}

impl Default for RelayGuard {
    fn default() -> Self {
        Self::with_default_limit()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(id: &str) -> MemberId {
        MemberId::new(id)
    }

    #[test]
    fn continues_until_limit_then_pauses() {
        let mut guard = RelayGuard::new(3);
        let turn = TurnId(1);
        let builder = member("builder");

        for expected in 1..=3 {
            assert_eq!(
                guard.record_auto_relay(turn, &builder),
                RelayDecision::Continue { count: expected }
            );
        }
        assert_eq!(
            guard.record_auto_relay(turn, &builder),
            RelayDecision::Pause { count: 4 }
        );
    }

    #[test]
    fn counts_are_independent_per_member_and_turn() {
        let mut guard = RelayGuard::new(1);
        let turn1 = TurnId(1);
        let turn2 = TurnId(2);

        assert_eq!(
            guard.record_auto_relay(turn1, &member("a")),
            RelayDecision::Continue { count: 1 }
        );
        // Different member, same turn: independent budget.
        assert_eq!(
            guard.record_auto_relay(turn1, &member("b")),
            RelayDecision::Continue { count: 1 }
        );
        // Same member, different turn: independent budget.
        assert_eq!(
            guard.record_auto_relay(turn2, &member("a")),
            RelayDecision::Continue { count: 1 }
        );
        // Same member + turn again: now over the limit.
        assert_eq!(
            guard.record_auto_relay(turn1, &member("a")),
            RelayDecision::Pause { count: 2 }
        );
    }

    #[test]
    fn note_progress_clears_counts_for_that_turn_only() {
        let mut guard = RelayGuard::new(2);
        let turn1 = TurnId(1);
        let turn2 = TurnId(2);
        for _ in 0..2 {
            guard.record_auto_relay(turn1, &member("a"));
            guard.record_auto_relay(turn1, &member("b"));
            guard.record_auto_relay(turn2, &member("a"));
        }

        guard.note_progress(turn1);

        // turn1: every sender starts over, so each may relay up to the limit again.
        for expected in 1..=2 {
            assert_eq!(
                guard.record_auto_relay(turn1, &member("a")),
                RelayDecision::Continue { count: expected }
            );
            assert_eq!(
                guard.record_auto_relay(turn1, &member("b")),
                RelayDecision::Continue { count: expected }
            );
        }
        // turn2 was not touched: still at the limit, so the next relay pauses.
        assert_eq!(
            guard.record_auto_relay(turn2, &member("a")),
            RelayDecision::Pause { count: 3 }
        );
    }

    #[test]
    fn progress_between_relays_never_pauses() {
        let mut guard = RelayGuard::new(2);
        let turn = TurnId(1);
        for _ in 0..50 {
            guard.note_progress(turn);
            assert_eq!(
                guard.record_auto_relay(turn, &member("a")),
                RelayDecision::Continue { count: 1 }
            );
        }
    }

    #[test]
    fn reset_turn_clears_only_that_turn() {
        let mut guard = RelayGuard::new(1);
        let turn1 = TurnId(1);
        let turn2 = TurnId(2);
        guard.record_auto_relay(turn1, &member("a"));
        guard.record_auto_relay(turn2, &member("a"));

        guard.reset_turn(turn1);

        // turn1 reset -> back to 1; turn2 untouched -> now 2 -> pause.
        assert_eq!(
            guard.record_auto_relay(turn1, &member("a")),
            RelayDecision::Continue { count: 1 }
        );
        assert_eq!(
            guard.record_auto_relay(turn2, &member("a")),
            RelayDecision::Pause { count: 2 }
        );
    }
}
