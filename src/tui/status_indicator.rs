//! Shared live-status text for the chat view. Inspired by Codex's status row:
//! keep elapsed time and the interrupt hint in a stable place while the rest of
//! the UI changes around it.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::domain::event::MemberStatus;

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub(crate) fn spinner() -> &'static str {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    spinner_at_millis(millis)
}

/// The pulsing running icon of the Agents list. It advances on the spinner's
/// clock, one icon every `PULSE_SPINNER_TICKS` spinner ticks.
const PULSE: [&str; 4] = ["◇", "◈", "◆", "◈"];
const PULSE_SPINNER_TICKS: u128 = 3;

pub(crate) fn pulse() -> &'static str {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    pulse_at_millis(millis)
}

fn pulse_at_millis(millis: u128) -> &'static str {
    PULSE[((millis / 80 / PULSE_SPINNER_TICKS) % PULSE.len() as u128) as usize]
}

fn spinner_at_millis(millis: u128) -> &'static str {
    let idx = ((millis / 80) % SPINNER.len() as u128) as usize;
    SPINNER[idx]
}

// Mirrors Codex's compact elapsed style: 8s, 1m 04s, 1h 02m 03s.
pub(crate) fn fmt_elapsed_compact(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!(
            "{}h {:02}m {:02}s",
            secs / 3600,
            (secs % 3600) / 60,
            secs % 60
        )
    }
}

/// The live row for a member that is working, e.g. `⠹ Working · 1s`. The
/// model and the interrupt keys are shown elsewhere (header and composer
/// hints), so the row only says what is happening and for how long.
pub(crate) fn member_activity_text(
    status: MemberStatus,
    reasoning: Option<&str>,
    show_reasoning: bool,
    elapsed_secs: Option<u64>,
    spin: &str,
) -> String {
    let elapsed = elapsed_secs.map(fmt_elapsed_compact);
    if let Some(reasoning) = reasoning.filter(|text| !text.is_empty()) {
        let timed = with_elapsed("Thinking", elapsed.as_deref());
        if !show_reasoning {
            return format!("{spin} {timed} · Ctrl+T expand");
        }
        return format!("{spin} {timed}: {reasoning}");
    }

    match status {
        MemberStatus::Running => {
            let elapsed = elapsed.unwrap_or_else(|| "0s".to_string());
            format!("{spin} Working · {elapsed}")
        }
        MemberStatus::Queued => format!("{spin} Queued"),
        MemberStatus::Waiting => format!("{spin} Waiting"),
        MemberStatus::NeedsApproval => format!("{spin} Waiting for approval"),
        MemberStatus::Failed => format!("{spin} Failed"),
        MemberStatus::Idle => format!("{spin} Idle"),
    }
}

fn with_elapsed(label: &str, elapsed: Option<&str>) -> String {
    match elapsed {
        Some(elapsed) => format!("{label} · {elapsed}"),
        None => label.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_elapsed_compact_scales_units_like_codex() {
        assert_eq!(fmt_elapsed_compact(8), "8s");
        assert_eq!(fmt_elapsed_compact(64), "1m 04s");
        assert_eq!(fmt_elapsed_compact(3723), "1h 02m 03s");
    }

    #[test]
    fn activity_text_is_the_state_and_the_time() {
        assert_eq!(
            member_activity_text(MemberStatus::Running, None, true, Some(64), "⠋"),
            "⠋ Working · 1m 04s"
        );
        assert_eq!(
            member_activity_text(MemberStatus::Running, None, true, Some(1), "⠹"),
            "⠹ Working · 1s"
        );
        assert_eq!(
            member_activity_text(MemberStatus::Running, None, true, None, "⠋"),
            "⠋ Working · 0s"
        );
        assert_eq!(
            member_activity_text(
                MemberStatus::Running,
                Some("reading files"),
                true,
                Some(64),
                "⠋"
            ),
            "⠋ Thinking · 1m 04s: reading files"
        );
        assert_eq!(
            member_activity_text(
                MemberStatus::Running,
                Some("reading files"),
                false,
                Some(1),
                "⠹"
            ),
            "⠹ Thinking · 1s · Ctrl+T expand"
        );
    }

    #[test]
    fn the_row_repeats_neither_the_model_nor_the_interrupt_keys() {
        let text = member_activity_text(MemberStatus::Running, None, true, Some(5), "⠋");
        assert!(!text.contains("model"), "{text}");
        assert!(!text.contains("interrupt"), "{text}");
    }

    #[test]
    fn spinner_advances_every_eighty_millis() {
        assert_eq!(spinner_at_millis(0), "⠋");
        assert_eq!(spinner_at_millis(80), "⠙");
        assert_eq!(spinner_at_millis(800), "⠋");
    }

    #[test]
    fn pulse_cycles_through_four_icons_on_the_spinner_clock() {
        assert_eq!(pulse_at_millis(0), "◇");
        assert_eq!(pulse_at_millis(240), "◈");
        assert_eq!(pulse_at_millis(480), "◆");
        assert_eq!(pulse_at_millis(720), "◈");
        assert_eq!(pulse_at_millis(960), "◇");
    }
}
