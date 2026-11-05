//! Delay single-click actions so a second click can select the whole row.

use std::time::{Duration, Instant};

use super::app_state::FoldTarget;

pub(super) const DOUBLE_CLICK_DELAY: Duration = Duration::from_millis(300);

#[derive(Debug)]
pub(super) enum ClickAction {
    Fold(FoldTarget),
    Link(String),
}

#[derive(Default)]
pub(super) struct ChatClicks {
    last: Option<(Instant, (usize, usize))>,
    pressed: bool,
    pending: Vec<(Instant, ClickAction)>,
}

impl ChatClicks {
    pub fn press(&mut self, pos: (usize, usize), now: Instant) -> bool {
        self.pressed = true;
        let Some((at, previous)) = self.last.take() else {
            return false;
        };
        if now.saturating_duration_since(at) <= DOUBLE_CLICK_DELAY
            && previous.0 == pos.0
            && previous.1.abs_diff(pos.1) <= 1
        {
            self.pending.retain(|(queued, _)| *queued != at);
            return true;
        }
        false
    }

    pub fn release(&mut self, pos: (usize, usize), action: Option<ClickAction>, now: Instant) {
        self.pressed = false;
        self.last = Some((now, pos));
        if let Some(action) = action {
            self.pending.push((now, action));
        }
    }

    pub fn cancel_double_click(&mut self) {
        self.last = None;
    }

    pub fn finish_press(&mut self) {
        self.pressed = false;
    }

    pub fn take_due(&mut self, now: Instant) -> Vec<ClickAction> {
        if self.pressed {
            return Vec::new();
        }
        if self
            .last
            .is_some_and(|(at, _)| now.saturating_duration_since(at) >= DOUBLE_CLICK_DELAY)
        {
            self.last = None;
        }
        let (due, waiting): (Vec<_>, Vec<_>) = std::mem::take(&mut self.pending)
            .into_iter()
            .partition(|(at, _)| now.saturating_duration_since(*at) >= DOUBLE_CLICK_DELAY);
        self.pending = waiting;
        due.into_iter().map(|(_, action)| action).collect()
    }

    pub fn reset(&mut self) {
        self.pressed = false;
        self.last = None;
        self.pending.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn separate_quick_clicks_are_not_dropped_and_wait_until_drag_finishes() {
        let now = Instant::now();
        let mut clicks = ChatClicks::default();
        assert!(!clicks.press((0, 2), now));
        clicks.release(
            (0, 2),
            Some(ClickAction::Fold(FoldTarget::Thinking(0))),
            now,
        );
        assert!(!clicks.press((5, 2), now + Duration::from_millis(20)));
        assert!(clicks.take_due(now + DOUBLE_CLICK_DELAY).is_empty());
        clicks.release(
            (5, 2),
            Some(ClickAction::Fold(FoldTarget::Thinking(1))),
            now + Duration::from_millis(30),
        );
        let actions = clicks.take_due(now + Duration::from_secs(1));
        assert!(matches!(
            actions.as_slice(),
            [
                ClickAction::Fold(FoldTarget::Thinking(0)),
                ClickAction::Fold(FoldTarget::Thinking(1))
            ]
        ));
    }
}
