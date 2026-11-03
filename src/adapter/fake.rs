//! Deterministic fake runner for tests and offline use.
//!
//! Emits a scripted sequence of [`AgentEvent`]s without spawning a process, so
//! the runtime and TUI can be exercised without real backends or usage.

use std::sync::mpsc::SyncSender;

use crate::adapter::{MemberRunner, RunRequest};
use crate::domain::event::{AgentEvent, AgentSessionId};
use crate::domain::team::BackendKind;
use crate::runtime::context_recap::without_recap;
use crate::runtime::mode_prompts::{
    BRAINSTORM_BUILD_HINT, BRAINSTORM_PROPOSE_HINT, BRAINSTORM_STRETCH_HINT,
    BRAINSTORM_SYNTHESIS_HINT, BRAINSTORM_VOTE_HINT, HANDBACK_MARKER, PLAN_MODE_HINT,
    REVIEW_PROTOCOL_HINT, SUB_AGENT_TASK_MARKER,
};

type Responder = Box<dyn Fn(&RunRequest) -> Vec<AgentEvent> + Send + Sync>;

pub struct FakeRunner {
    backend: BackendKind,
    responder: Responder,
}

impl FakeRunner {
    pub fn new(
        backend: BackendKind,
        responder: impl Fn(&RunRequest) -> Vec<AgentEvent> + Send + Sync + 'static,
    ) -> Self {
        Self {
            backend,
            responder: Box::new(responder),
        }
    }

    /// Echoes the prompt back as a completed message and reports a stable
    /// session id so resume bookkeeping has something to persist.
    pub fn echo(backend: BackendKind) -> Self {
        Self::new(backend, move |req| {
            vec![
                AgentEvent::SessionDiscovered(AgentSessionId(format!(
                    "fake-{}-session",
                    backend.as_str()
                ))),
                AgentEvent::MessageCompleted(format!("[{backend} fake] {}", req.prompt)),
            ]
        })
    }

    /// Scripted teammate for `--fake`: recognizes mode prompts by their template
    /// markers (same constants the engine uses) and plays along; anything else echoes.
    ///
    /// Sub-agent flow: a plain message containing `spawn` makes the member open
    /// a `reviewer` sub-agent (`spawn:<name>` picks the name) on another
    /// backend; the sub-agent answers its task, and the parent acknowledges the
    /// hand-back.
    pub fn team(backend: BackendKind) -> Self {
        Self::new(backend, move |req| {
            let session = AgentEvent::SessionDiscovered(AgentSessionId(format!(
                "fake-{}-session",
                backend.as_str()
            )));
            let mut events = vec![session];
            // Goal prompts: `fake-goal-silent` replies with nothing at all.
            let Some(text) = team_response_opt(backend, &req.prompt) else {
                return events;
            };
            // Only a plain echoed message plays the tool script, never a mode
            // prompt that happens to mention tools.
            if text.starts_with(&format!("[{backend} fake] ")) && asks_for_tools(&req.prompt) {
                events.extend(tool_script());
            }
            events.push(AgentEvent::MessageCompleted(text));
            events
        })
    }

    /// Emits a fixed event sequence regardless of the prompt.
    pub fn scripted(backend: BackendKind, events: Vec<AgentEvent>) -> Self {
        Self::new(backend, move |_| events.clone())
    }
}

/// Words in a message that script the fake teammate.
const ECHO_PROMPT_TRIGGER: &str = "echo-prompt";

/// A message with the word `tools` makes the teammate run a batch of tool
/// calls before it answers, so long tool groups can be seen offline.
fn asks_for_tools(prompt: &str) -> bool {
    without_recap(prompt)
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|word| word.eq_ignore_ascii_case("tools"))
}

/// Eight finished calls of mixed kinds, one of them failing.
fn tool_script() -> Vec<AgentEvent> {
    let calls: [(&str, &str, bool); 8] = [
        ("Read", "src/main.rs", true),
        ("Read", "src/lib.rs", true),
        ("Grep", "TODO", true),
        ("Bash", "cargo build", true),
        ("Read", "Cargo.toml", true),
        ("Bash", "cargo test", false),
        ("Grep", "fn main", true),
        ("WebFetch", "https://example.com/docs", true),
    ];
    let mut events = Vec::new();
    for (n, (name, summary, ok)) in calls.into_iter().enumerate() {
        let id = format!("fake-tool-{n}");
        events.push(AgentEvent::ToolStarted {
            id: id.clone(),
            name: name.to_string(),
            summary: summary.to_string(),
        });
        events.push(AgentEvent::ToolCompleted {
            id,
            ok,
            summary: if ok {
                summary.to_string()
            } else {
                format!("error: {summary} failed")
            },
        });
    }
    events
}

/// An objective containing `fake-goal-<N>` makes the fake finish the goal on
/// its N-th goal turn; `fake-goal-silent` makes every goal turn reply with
/// nothing, to exercise the no-progress guard. Other goal objectives never
/// finish by themselves.
const FAKE_GOAL_TRIGGER: &str = "fake-goal-";

/// The reply to a goal prompt (initial, continuation or edited objective):
/// `Some(None)` when the fake should stay silent, `None` when `prompt` is no
/// goal prompt.
fn goal_response(backend: BackendKind, prompt: &str) -> Option<Option<String>> {
    let objective = prompt
        .split_once("<objective>\n")
        .and_then(|(_, rest)| rest.split_once("\n</objective>"))
        .map(|(objective, _)| objective)?;
    let turn: u32 = prompt
        .split_once("- turn: ")
        .map(|(_, rest)| {
            rest.chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
        })
        .and_then(|digits| digits.parse().ok())
        .unwrap_or(1);
    let trigger = objective
        .split_once(FAKE_GOAL_TRIGGER)
        .map(|(_, rest)| {
            rest.split(|c: char| !c.is_ascii_alphanumeric())
                .next()
                .unwrap_or("")
        })
        .unwrap_or("");
    if trigger == "silent" {
        return Some(None);
    }
    let mut text = format!("[{backend} fake goal] working on the goal, turn {turn}");
    if trigger.parse::<u32>().is_ok_and(|done_at| turn >= done_at) {
        text.push_str("\n@@goal_complete {\"summary\":\"fake goal done\"}");
    }
    Some(Some(text))
}

/// `team_response`, except a goal prompt may get no reply at all.
fn team_response_opt(backend: BackendKind, prompt: &str) -> Option<String> {
    match goal_response(backend, prompt) {
        Some(reply) => reply,
        None => Some(team_response(backend, prompt)),
    }
}

fn team_response(backend: BackendKind, prompt: &str) -> String {
    // A message containing `echo-prompt` is answered with the exact prompt the
    // model received, recap included, so what a model switch carried over can
    // be read off the screen. Only the message itself can trigger it, not an
    // earlier one quoted in a recap.
    if without_recap(prompt).contains(ECHO_PROMPT_TRIGGER) {
        return prompt.to_string();
    }
    // Sub-agent messages first: their bodies may quote the word "spawn".
    if prompt.contains(HANDBACK_MARKER) {
        let result = prompt.split_once("\n\n").map_or(prompt, |(_, body)| body);
        return format!("[{backend} fake] Got the sub-agent's result: {result}");
    }
    if prompt.contains(SUB_AGENT_TASK_MARKER) {
        let task = prompt.split_once("\n\n").map_or(prompt, |(_, body)| body);
        return format!("[{backend} fake sub-agent] Finished: {task}");
    }
    if prompt.contains(REVIEW_PROTOCOL_HINT) {
        return "Reviewed the work.\n@@review {\"verdict\":\"approve\",\"summary\":\"fake approve\"}"
            .to_string();
    }
    if prompt.contains(PLAN_MODE_HINT) {
        return plan_plan_response(prompt);
    }
    if prompt.contains("step #") {
        return step_done_response(prompt);
    }
    if prompt.contains(BRAINSTORM_SYNTHESIS_HINT) {
        return "## Ranked result\n\n1. R1-A#1 — 10 points\n2. R1-B#1 — 8 points\n\nPrimary recommendation: validate R1-A#1 with a small experiment."
            .to_string();
    }
    if prompt.contains(BRAINSTORM_VOTE_HINT) {
        return format!(
            "I ranked candidates for relevance and testability.\n\
             @@brainstorm_vote {{\"ranked\":[\"R1-A#1\",\"R1-B#1\",\"R2-A#2\",\"R2-B#2\",\"R3-A#3\"],\"summary\":\"{backend} ballot\"}}"
        );
    }
    if prompt.contains(BRAINSTORM_STRETCH_HINT) {
        return format!(
            "@@brainstorm_card {{\"title\":\"Invert {backend}\",\"proposal\":\"Remove the default assumption\",\"mechanism\":\"Reverse the usual dependency\",\"operation\":\"INVERT\",\"sources\":[\"R2-A#1\"]}}\n\
             @@brainstorm_card {{\"title\":\"No constraint {backend}\",\"proposal\":\"Imagine the main constraint disappears\",\"mechanism\":\"Explore the newly reachable design space\",\"operation\":\"REMOVE_CONSTRAINT\",\"sources\":[\"R2-A#2\"]}}\n\
             @@brainstorm_card {{\"title\":\"Ecology analogy {backend}\",\"proposal\":\"Borrow a mechanism from ecology\",\"mechanism\":\"Map ecological feedback onto the topic\",\"operation\":\"ANALOGY\",\"sources\":[\"R2-A#3\"]}}\n\
             @@brainstorm_card {{\"title\":\"Bridge {backend}\",\"proposal\":\"Combine two prior batches\",\"mechanism\":\"Join their complementary mechanisms\",\"operation\":\"BRIDGE\",\"sources\":[\"R2-A#1\",\"R2-B#1\"]}}"
        );
    }
    if prompt.contains(BRAINSTORM_BUILD_HINT) {
        return format!(
            "@@brainstorm_card {{\"title\":\"New {backend} direction\",\"proposal\":\"Try an independent direction\",\"mechanism\":\"Start from a separate assumption\",\"operation\":\"NEW\",\"sources\":[]}}\n\
             @@brainstorm_card {{\"title\":\"Build {backend} seed\",\"proposal\":\"Extend the shared seed\",\"mechanism\":\"Add one reinforcing capability\",\"operation\":\"BUILD\",\"sources\":[\"R1-A#1\"]}}\n\
             @@brainstorm_card {{\"title\":\"Combine {backend} seeds\",\"proposal\":\"Join two mechanisms\",\"mechanism\":\"Compose their strongest interactions\",\"operation\":\"COMBINE\",\"sources\":[\"R1-A#1\",\"R1-B#1\"]}}\n\
             @@brainstorm_card {{\"title\":\"Mutate {backend} audience\",\"proposal\":\"Change the target user\",\"mechanism\":\"Reframe the seed around another actor\",\"operation\":\"MUTATE\",\"sources\":[\"R1-A#2\"]}}"
        );
    }
    if prompt.contains(BRAINSTORM_PROPOSE_HINT) {
        return format!(
            "@@brainstorm_card {{\"title\":\"{backend} seed\",\"proposal\":\"A seed tailored to the backend\",\"mechanism\":\"Use its native strengths\",\"operation\":\"SEED\",\"sources\":[]}}\n\
             @@brainstorm_card {{\"title\":\"Low-tech {backend} seed\",\"proposal\":\"Try a contrasting low-tech path\",\"mechanism\":\"Replace automation with a simple workflow\",\"operation\":\"SEED\",\"sources\":[]}}\n\
             @@brainstorm_card {{\"title\":\"Service {backend} seed\",\"proposal\":\"Use a service model\",\"mechanism\":\"Deliver the capability as an ongoing service\",\"operation\":\"SEED\",\"sources\":[]}}\n\
             @@brainstorm_card {{\"title\":\"Wild {backend} inversion\",\"proposal\":\"Invert the usual relationship\",\"mechanism\":\"Make the receiver initiate the exchange\",\"operation\":\"SEED\",\"sources\":[]}}"
        );
    }
    // Keywords below belong to the new message, not to a recap in front of it.
    let message = without_recap(prompt);
    if let Some(spawn) = spawn_response(backend, message) {
        return spawn;
    }
    format!("[{backend} fake] {message}")
}

/// Reply with one `@@spawn` line when a plain message asks to `spawn`.
fn spawn_response(backend: BackendKind, prompt: &str) -> Option<String> {
    let lower = prompt.to_ascii_lowercase();
    // Bracketed prompts are relays and system notes, not the user asking.
    if !lower.contains("spawn") || lower.trim_start().starts_with('[') {
        return None;
    }
    let name = lower
        .split_whitespace()
        .find_map(|word| word.strip_prefix("spawn:"))
        .map(|name| name.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_'))
        .filter(|name| !name.is_empty())
        .unwrap_or("reviewer");
    // `spawn:sol` mimics a model that names its sub-agent after the model it
    // asked for, so the name fallback can be exercised without a real backend.
    // That model belongs to Codex, so the request names the codex backend
    // whichever backend the asking member runs on.
    let (other, model) = match name {
        "sol" => (BackendKind::Codex, ",\"model\":\"gpt-6.1-sol\""),
        _ => (
            match backend {
                BackendKind::Claude | BackendKind::Grok => BackendKind::Codex,
                BackendKind::Codex | BackendKind::Agy => BackendKind::Claude,
            },
            "",
        ),
    };
    let task = prompt.replace(['"', '\\'], "'").replace('\n', " ");
    Some(format!(
        "Opening a {name} sub-agent.\n\
         @@spawn {{\"name\":\"{name}\",\"task\":\"Look at this request and report back: {task}\",\"backend\":\"{other}\"{model},\"effort\":\"high\"}}"
    ))
}

fn plan_plan_response(prompt: &str) -> String {
    let mut lines = Vec::new();
    lines.push("Planned the work.".to_string());
    let teammates = prompt.lines().find_map(|line| {
        line.strip_prefix("Teammates: ")
            .map(|rest| rest.split(", ").collect::<Vec<_>>())
    });
    if let Some(ids) = teammates {
        if ids.is_empty() {
            lines.push(
                "@@run_step {\"action\":\"add\",\"title\":\"Fake step (no owners)\"}".to_string(),
            );
        } else {
            for id in ids {
                let id = id.trim();
                if id.is_empty() {
                    continue;
                }
                lines.push(format!(
                    "@@run_step {{\"action\":\"add\",\"owner\":\"{id}\",\"title\":\"Fake step for {id}\"}}"
                ));
            }
        }
    } else {
        lines.push(
            "@@run_step {\"action\":\"add\",\"title\":\"Fake step (no owners)\"}".to_string(),
        );
    }
    lines.join("\n")
}

fn step_done_response(prompt: &str) -> String {
    let mut numbers = Vec::new();
    let mut rest = prompt;
    while let Some(idx) = rest.find("step #") {
        rest = &rest[idx + "step #".len()..];
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(n) = digits.parse::<u32>() {
            numbers.push(n);
        }
    }
    let mut lines = Vec::new();
    lines.push("did the work".to_string());
    for n in numbers {
        lines.push(format!("@@run_step {{\"action\":\"done\",\"step\":{n}}}"));
    }
    lines.join("\n")
}

/// How long a prompt containing `slow` keeps the fake busy.
const SLOW_REPLY: std::time::Duration = std::time::Duration::from_secs(5);

impl MemberRunner for FakeRunner {
    fn backend(&self) -> BackendKind {
        self.backend
    }

    fn run(&self, req: RunRequest, events: SyncSender<AgentEvent>) {
        // A prompt that says `slow` makes the fake take about five seconds,
        // so the running state can be looked at (and interrupted) offline.
        if without_recap(&req.prompt)
            .to_ascii_lowercase()
            .contains("slow")
        {
            let started = std::time::Instant::now();
            while started.elapsed() < SLOW_REPLY
                && !req.cancel.load(std::sync::atomic::Ordering::Relaxed)
            {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
        for event in (self.responder)(&req) {
            let _ = events.send(event);
        }
        let _ = events.send(AgentEvent::Exited {
            code: Some(0),
            ok: true,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::context_recap::{RECAP_FOOTER, RECAP_HEADER};
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc;

    fn run(runner: &FakeRunner, prompt: &str) -> Vec<AgentEvent> {
        let (tx, rx) = mpsc::sync_channel(65_536);
        runner.run(
            RunRequest {
                steer: None,
                prompt: prompt.to_string(),
                session: None,
                cancel: Arc::new(AtomicBool::new(false)),
                effort: None,
            },
            tx,
        );
        rx.iter().collect()
    }

    fn completed_text(events: &[AgentEvent]) -> &str {
        events
            .iter()
            .find_map(|e| match e {
                AgentEvent::MessageCompleted(text) => Some(text.as_str()),
                _ => None,
            })
            .expect("MessageCompleted")
    }

    #[test]
    fn a_tools_message_runs_a_batch_of_mixed_calls_with_one_failure() {
        let events = run(
            &FakeRunner::team(BackendKind::Codex),
            "use the tools please",
        );
        let started = events
            .iter()
            .filter(|e| matches!(e, AgentEvent::ToolStarted { .. }))
            .count();
        let failed = events
            .iter()
            .filter(|e| matches!(e, AgentEvent::ToolCompleted { ok: false, .. }))
            .count();
        assert_eq!((started, failed), (8, 1));
        assert!(completed_text(&events).contains("use the tools please"));

        let plain = run(&FakeRunner::team(BackendKind::Codex), "toolsmith");
        assert!(
            !plain
                .iter()
                .any(|e| matches!(e, AgentEvent::ToolStarted { .. }))
        );
    }

    #[test]
    fn echo_reports_session_message_and_exit() {
        let events = run(&FakeRunner::echo(BackendKind::Codex), "build it");
        assert!(matches!(events[0], AgentEvent::SessionDiscovered(_)));
        assert!(matches!(
            &events[1],
            AgentEvent::MessageCompleted(text) if text.contains("build it")
        ));
        assert!(matches!(
            events.last().unwrap(),
            AgentEvent::Exited { ok: true, .. }
        ));
    }

    #[test]
    fn scripted_emits_fixed_sequence_then_exit() {
        let runner = FakeRunner::scripted(
            BackendKind::Claude,
            vec![AgentEvent::MessageCompleted("hi".to_string())],
        );
        let events = run(&runner, "anything");
        assert_eq!(
            events,
            vec![
                AgentEvent::MessageCompleted("hi".to_string()),
                AgentEvent::Exited {
                    code: Some(0),
                    ok: true
                },
            ]
        );
    }

    #[test]
    fn team_review_hint_approves() {
        let events = run(
            &FakeRunner::team(BackendKind::Claude),
            &format!("please review\n\n{REVIEW_PROTOCOL_HINT}"),
        );
        let text = completed_text(&events);
        assert!(text.contains("@@review"));
        assert!(text.contains("approve"));
        assert!(matches!(events[0], AgentEvent::SessionDiscovered(_)));
    }

    #[test]
    fn team_step_hash_marks_done() {
        let events = run(
            &FakeRunner::team(BackendKind::Codex),
            "You own step #2: wire the parser. Also step #5 maybe.",
        );
        let text = completed_text(&events);
        assert!(text.contains("\"action\":\"done\""));
        assert!(text.contains("\"step\":2"));
        assert!(text.contains("\"step\":5"));
        assert!(text.contains("did the work"));
    }

    #[test]
    fn team_spawn_keyword_emits_one_spawn_line() {
        let events = run(
            &FakeRunner::team(BackendKind::Codex),
            "please spawn a helper",
        );
        let text = completed_text(&events);
        let parsed = crate::router::parse_agent_output(text);
        assert_eq!(parsed.spawns.len(), 1);
        assert_eq!(parsed.spawns[0].name, "reviewer");
        assert_eq!(parsed.spawns[0].backend.as_deref(), Some("claude"));

        let named = run(&FakeRunner::team(BackendKind::Claude), "spawn:tester now");
        let parsed = crate::router::parse_agent_output(completed_text(&named));
        assert_eq!(parsed.spawns[0].name, "tester");
        assert_eq!(parsed.spawns[0].backend.as_deref(), Some("codex"));
        assert_eq!(parsed.spawns[0].model, None);
    }

    #[test]
    fn team_spawn_sol_names_the_agent_after_its_model() {
        let events = run(&FakeRunner::team(BackendKind::Claude), "spawn:sol please");
        let parsed = crate::router::parse_agent_output(completed_text(&events));
        assert_eq!(parsed.spawns[0].name, "sol");
        assert_eq!(parsed.spawns[0].model.as_deref(), Some("gpt-6.1-sol"));
        assert_eq!(parsed.spawns[0].backend.as_deref(), Some("codex"));

        // Whichever backend asks, `spawn:sol` opens a codex sub-agent.
        let from_codex = run(&FakeRunner::team(BackendKind::Codex), "spawn:sol please");
        let parsed = crate::router::parse_agent_output(completed_text(&from_codex));
        assert_eq!(parsed.spawns[0].backend.as_deref(), Some("codex"));
        assert_eq!(parsed.spawns[0].model.as_deref(), Some("gpt-6.1-sol"));
    }

    #[test]
    fn team_sub_agent_messages_never_spawn_again() {
        let task = format!("{SUB_AGENT_TASK_MARKER}Main (main)]\nhint\n\nplease spawn more");
        let events = run(&FakeRunner::team(BackendKind::Claude), &task);
        assert!(completed_text(&events).contains("Finished: please spawn more"));
        assert!(!completed_text(&events).contains("@@spawn"));

        let handback = format!("{HANDBACK_MARKER}reviewer (reviewer)]\nhint\n\nspawn done");
        let events = run(&FakeRunner::team(BackendKind::Codex), &handback);
        assert!(!completed_text(&events).contains("@@spawn"));
    }

    #[test]
    fn echo_prompt_replies_with_the_whole_prompt_but_only_for_the_message() {
        let runner = FakeRunner::team(BackendKind::Claude);
        let events = run(&runner, "echo-prompt what did we talk about");
        assert_eq!(
            completed_text(&events),
            "echo-prompt what did we talk about"
        );

        // With a recap in front, the reply keeps all of it.
        let with_recap = format!(
            "{RECAP_HEADER}\nUser: echo-prompt earlier\n\n{RECAP_FOOTER}\n\necho-prompt now"
        );
        let events = run(&runner, &with_recap);
        assert_eq!(completed_text(&events), with_recap);

        // A trigger word that only the recap quotes is not a trigger.
        let quoted =
            format!("{RECAP_HEADER}\nUser: echo-prompt earlier\n\n{RECAP_FOOTER}\n\nplain words");
        let events = run(&runner, &quoted);
        assert_eq!(completed_text(&events), "[claude fake] plain words");
    }

    #[test]
    fn team_plain_prompt_echoes() {
        let events = run(&FakeRunner::team(BackendKind::Grok), "hello");
        let text = completed_text(&events);
        assert_eq!(text, "[grok fake] hello");
    }

    fn goal_prompt(objective: &str, turn: u32) -> String {
        format!(
            "Continue working toward the active goal.\n<objective>\n{objective}\n</objective>\n\nGoal state:\n- status: active\n- turn: {turn}\n"
        )
    }

    #[test]
    fn fake_goal_n_completes_on_the_nth_goal_turn() {
        let runner = FakeRunner::team(BackendKind::Claude);
        for turn in 1..=4 {
            let events = run(&runner, &goal_prompt("fix things fake-goal-3", turn));
            let text = completed_text(&events);
            let parsed = crate::router::parse_agent_output(text);
            assert_eq!(
                parsed.goal_completes.len(),
                usize::from(turn >= 3),
                "turn {turn}"
            );
            assert!(!parsed.visible_text.is_empty(), "turn {turn}");
        }
    }

    #[test]
    fn fake_goal_silent_and_plain_goals_behave() {
        let runner = FakeRunner::team(BackendKind::Claude);
        let events = run(&runner, &goal_prompt("nothing fake-goal-silent", 2));
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::MessageCompleted(_)))
        );
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Exited { ok: true, .. })
        ));

        // An objective with no trigger never finishes by itself.
        let events = run(&runner, &goal_prompt("just keep going", 9));
        assert!(
            crate::router::parse_agent_output(completed_text(&events))
                .goal_completes
                .is_empty()
        );
    }
}
