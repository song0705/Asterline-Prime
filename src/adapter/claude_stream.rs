//! Claude Code streaming adapter.
//!
//! Drives `claude -p --input-format stream-json --output-format stream-json
//! --verbose --include-partial-messages --replay-user-messages` and translates
//! the NDJSON stream into [`AgentEvent`]s. Sessions are resumable
//! (`--resume <id>`); `--no-session-persistence` is never used.
//!
//! Input is NDJSON user messages on stdin, so another agent's message can be
//! written while a tool is running and reaches the model right after that
//! tool completes, within the same turn. `--replay-user-messages` makes Claude
//! echo each consumed stdin message on stdout; the runner uses those echoes to
//! know whether an injected message was consumed before the turn's `result`
//! (then stdin is closed so the process exits) or is still waiting (then
//! Claude runs another turn for it, and stdin stays open until that ends).

use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde_json::Value;

use crate::adapter::parser::{
    MAX_MESSAGE_TEXT_BYTES, append_bounded_text, bounded_text, str_field, summarize, tool_value,
};
use crate::adapter::process::{AdapterCommand, LineParser, SteerEncoder, StreamAdapter};
use crate::domain::event::{AgentEvent, AgentSessionId};
use crate::domain::team::{BackendKind, Effort, PermissionMode, TeamMember};

const TOOL_OUTPUT_MAX: usize = 32_000;

#[derive(Clone, Debug)]
pub struct ClaudeStreamAdapter {
    binary: String,
    cwd: std::path::PathBuf,
    /// Asterline's shared skills and this project's state, outside `cwd`.
    extra_dirs: Vec<std::path::PathBuf>,
    session_name: String,
    model: Option<String>,
    permission_mode: Option<PermissionMode>,
    allowed_tools: Vec<String>,
    system_prompt: Option<String>,
}

impl ClaudeStreamAdapter {
    pub fn from_member(member: &TeamMember, workspace: &Path) -> Self {
        Self {
            binary: "claude".to_string(),
            cwd: member.resolved_cwd(workspace),
            extra_dirs: crate::state_paths::agent_access_dirs(workspace),
            session_name: native_session_name(&member.display_name),
            model: member.model.clone(),
            permission_mode: member.permission_mode,
            allowed_tools: member.allowed_tools.clone(),
            system_prompt: member.system_prompt.clone(),
        }
    }

    pub fn with_binary(mut self, binary: impl Into<String>) -> Self {
        self.binary = binary.into();
        self
    }
}

impl StreamAdapter for ClaudeStreamAdapter {
    fn backend(&self) -> BackendKind {
        BackendKind::Claude
    }

    fn build_command(
        &self,
        prompt: &str,
        session: Option<&AgentSessionId>,
        effort: Option<Effort>,
    ) -> AdapterCommand {
        let mut args = vec![
            "--print".to_string(),
            "--input-format".to_string(),
            "stream-json".to_string(),
            "--replay-user-messages".to_string(),
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--verbose".to_string(),
            "--include-partial-messages".to_string(),
            "--forward-subagent-text".to_string(),
        ];
        if let Some(session) = session {
            args.push("--resume".to_string());
            args.push(session.as_str().to_string());
        }
        // Claude persists print-mode sessions unless explicitly disabled. A
        // stable, human-readable name records the session as Asterline-owned
        // instead of leaving a system-prompt fragment as its title. Passing it
        // on resume repairs sessions that predate this label too.
        args.push("--name".to_string());
        args.push(self.session_name.clone());
        if let Some(model) = &self.model {
            args.push("--model".to_string());
            args.push(model.clone());
        }
        // Recent Claude CLIs (e.g. v2.1.207) list acceptEdits/auto/plan/… but not
        // `default`; omit the flag so the CLI's own default applies.
        if let Some(mode) = self.permission_mode
            && mode != PermissionMode::Default
        {
            args.push("--permission-mode".to_string());
            args.push(mode.claude_arg().to_string());
        }
        if !self.allowed_tools.is_empty() {
            // Claude's `--allowed-tools` only pre-approves tools; `--tools`
            // actually removes every unlisted built-in tool from the session.
            args.push("--tools".to_string());
            args.push(self.allowed_tools.join(","));
        }
        // The skill and roster live under the Asterline home, not the project.
        for dir in &self.extra_dirs {
            args.push("--add-dir".to_string());
            args.push(dir.display().to_string());
        }
        if let Some(system_prompt) = &self.system_prompt {
            args.push("--append-system-prompt".to_string());
            args.push(system_prompt.clone());
        }
        if let Some(effort) = effort {
            args.push("--effort".to_string());
            args.push(effort.claude_arg().to_string());
        }
        AdapterCommand {
            program: self.binary.clone(),
            args,
            cwd: self.cwd.clone(),
            // Claude accepts stream-json user messages on stdin. Keeping user
            // text out of argv prevents prompts such as `--version` from being
            // reinterpreted as CLI options. Stdin stays open after the first
            // message so later ones can be injected; the runner closes it when
            // the turn is over.
            stdin: Some(encode_user_message(prompt).0),
            keep_stdin_open: true,
        }
    }

    fn steer_encoder(&self) -> Option<SteerEncoder> {
        Some(encode_user_message)
    }

    fn parser(&self) -> Box<dyn LineParser> {
        Box::new(ClaudeLineParser::default())
    }
}

/// One stream-json user message as a stdin line, plus the text Claude will
/// echo back (with `--replay-user-messages`) once it consumes the message.
/// Image attachments are passed as text carrying their file paths.
fn encode_user_message(prompt: &str) -> (String, String) {
    let text = crate::adapter::prompt_images::prompt_with_image_paths(prompt);
    let mut line = serde_json::json!({
        "type": "user",
        "message": { "role": "user", "content": text },
    })
    .to_string();
    line.push('\n');
    (line, text)
}

/// The text of a user message that Claude echoed back from stdin, or `None`
/// for any other user message (tool results, child-agent prompts). Tool
/// results carry `tool_result` blocks; an echo is plain text.
fn replayed_user_text(value: &Value) -> Option<String> {
    if value
        .get("parent_tool_use_id")
        .is_some_and(|id| !id.is_null())
    {
        return None;
    }
    let content = &value["message"]["content"];
    if let Some(text) = content.as_str() {
        return Some(text.to_string());
    }
    let blocks = content.as_array()?;
    if blocks
        .iter()
        .any(|block| str_field(block, "type") != Some("text"))
    {
        return None;
    }
    let text: Vec<&str> = blocks
        .iter()
        .filter_map(|block| str_field(block, "text"))
        .collect();
    (!text.is_empty()).then(|| text.join("\n"))
}

fn native_session_name(display_name: &str) -> String {
    format!("Asterline · {}", display_name.trim())
}

/// Streaming parser for the Claude stream-json envelope.
#[derive(Default)]
pub struct ClaudeLineParser {
    text_acc: String,
    message_emitted: bool,
    tool_blocks: HashMap<u64, String>,
    tool_input_started: HashSet<u64>,
    result_seen: bool,
    /// True after we announced that Claude is thinking. Keeps `thinking_tokens`
    /// from flooding the runtime while still starting the elapsed clock before
    /// the first `thinking_delta` arrives.
    thinking_clock: bool,
    /// Texts of stdin messages Claude echoed back, awaiting pickup.
    replayed: Vec<String>,
    /// A `result` was parsed since the runner last asked.
    turn_finished: bool,
}

impl ClaudeLineParser {
    fn flush_message(&mut self, out: &mut Vec<AgentEvent>) {
        let text = std::mem::take(&mut self.text_acc);
        if !text.trim().is_empty() {
            self.message_emitted = true;
            out.push(AgentEvent::MessageCompleted(text));
        }
    }

    fn note_thinking_started(&mut self, out: &mut Vec<AgentEvent>) {
        if self.thinking_clock {
            return;
        }
        self.thinking_clock = true;
        out.push(AgentEvent::Reasoning(String::new()));
    }

    fn handle_stream_event(&mut self, event: &Value, out: &mut Vec<AgentEvent>) {
        match str_field(event, "type") {
            Some("message_start") => {
                self.text_acc.clear();
                self.message_emitted = false;
            }
            Some("content_block_start") => {
                let block = &event["content_block"];
                if str_field(block, "type") == Some("thinking") {
                    self.note_thinking_started(out);
                    return;
                }
                if str_field(block, "type") == Some("tool_use") {
                    self.thinking_clock = false;
                    self.flush_message(out);
                    let id = str_field(block, "id").unwrap_or_default().to_string();
                    let name = str_field(block, "name").unwrap_or("tool").to_string();
                    let index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
                    self.tool_blocks.insert(index, id.clone());
                    out.push(AgentEvent::ToolStarted {
                        id: id.clone(),
                        summary: name.clone(),
                        name,
                    });
                    if let Some(input) = block.get("input") {
                        let input = tool_value(input, TOOL_OUTPUT_MAX);
                        if !input.is_empty() && input != "{}" {
                            self.tool_input_started.insert(index);
                            out.push(AgentEvent::ToolProgress {
                                id,
                                delta: format!("input:\n{input}\n"),
                            });
                        }
                    }
                }
            }
            Some("content_block_delta") => {
                let delta = &event["delta"];
                if str_field(delta, "type") == Some("text_delta")
                    && let Some(text) = str_field(delta, "text")
                {
                    let _ = append_bounded_text(&mut self.text_acc, text, MAX_MESSAGE_TEXT_BYTES);
                } else if str_field(delta, "type") == Some("input_json_delta")
                    && let Some(partial) = str_field(delta, "partial_json")
                    && let Some(index) = event.get("index").and_then(Value::as_u64)
                    && let Some(id) = self.tool_blocks.get(&index)
                {
                    let prefix = if self.tool_input_started.insert(index) {
                        "input:\n"
                    } else {
                        ""
                    };
                    out.push(AgentEvent::ToolProgress {
                        id: id.clone(),
                        delta: format!("{prefix}{partial}"),
                    });
                } else if str_field(delta, "type") == Some("thinking_delta")
                    && let Some(thinking) = str_field(delta, "thinking")
                    && !thinking.is_empty()
                {
                    self.note_thinking_started(out);
                    out.push(AgentEvent::Reasoning(thinking.to_string()));
                }
            }
            Some("content_block_stop") => {
                if let Some(index) = event.get("index").and_then(Value::as_u64) {
                    let input_started = self.tool_input_started.remove(&index);
                    if let Some(id) = self.tool_blocks.remove(&index)
                        && input_started
                    {
                        out.push(AgentEvent::ToolProgress {
                            id,
                            delta: "\n".to_string(),
                        });
                    }
                }
            }
            Some("message_stop") => self.flush_message(out),
            _ => {}
        }
    }

    fn handle_user_message(&self, message: &Value, out: &mut Vec<AgentEvent>) {
        let Some(content) = message.get("content").and_then(Value::as_array) else {
            return;
        };
        for block in content {
            if str_field(block, "type") == Some("tool_result") {
                let id = str_field(block, "tool_use_id")
                    .unwrap_or_default()
                    .to_string();
                let is_error = block
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let summary = tool_value(&block["content"], TOOL_OUTPUT_MAX);
                out.push(AgentEvent::ToolCompleted {
                    id,
                    ok: !is_error,
                    summary,
                });
            }
        }
    }
}

impl LineParser for ClaudeLineParser {
    fn parse_line(&mut self, line: &str) -> Vec<AgentEvent> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return Vec::new();
        }
        let value: Value = match serde_json::from_str(trimmed) {
            Ok(value) => value,
            Err(err) => {
                return vec![AgentEvent::ParseWarning(format!(
                    "claude: invalid JSON line: {err}"
                ))];
            }
        };

        let mut out = Vec::new();
        match str_field(&value, "type") {
            Some("system") => {
                if let Some(session) = str_field(&value, "session_id") {
                    out.push(AgentEvent::SessionDiscovered(AgentSessionId(
                        session.to_string(),
                    )));
                }
                // `thinking_tokens` progress rows arrive many times per turn
                // (observed on claude 2.1.207); logging each would flood the
                // logs drawer.
                let subtype = str_field(&value, "subtype");
                if subtype == Some("thinking_tokens") {
                    self.note_thinking_started(&mut out);
                } else if subtype != Some("init") {
                    out.push(AgentEvent::Log(format!(
                        "claude system/{}",
                        subtype.unwrap_or("event")
                    )));
                }
            }
            Some("stream_event") => self.handle_stream_event(&value["event"], &mut out),
            Some("user") => {
                // With --replay-user-messages Claude echoes each consumed
                // stdin message as a user line. Those are bookkeeping for the
                // runner, never chat content, so they yield no events.
                if let Some(text) = replayed_user_text(&value) {
                    self.replayed.push(text);
                } else {
                    self.handle_user_message(&value["message"], &mut out);
                }
            }
            Some("assistant") => {
                // Main-agent content is already represented by `stream_event`
                // partials. `--forward-subagent-text` marks forwarded child
                // blocks so their text/thinking can be surfaced without
                // duplicating the main answer. The main agent's own messages
                // carry `"parent_tool_use_id": null`; only a non-empty string
                // marks a child.
                let is_child = |parent: &Value| parent.as_str().is_some_and(|id| !id.is_empty());
                if is_child(&value["parent_tool_use_id"])
                    || is_child(&value["message"]["parent_tool_use_id"])
                {
                    let content = value["message"]["content"].as_array();
                    for block in content.into_iter().flatten() {
                        match str_field(block, "type") {
                            Some("thinking") => {
                                // Current Claude builds send only a signature, so
                                // the text is often empty: show nothing then.
                                if let Some(thinking) = str_field(block, "thinking")
                                    .filter(|thinking| !thinking.trim().is_empty())
                                {
                                    out.push(AgentEvent::Reasoning(format!(
                                        "[subagent] {thinking}"
                                    )));
                                }
                            }
                            Some("text") => {
                                if let Some(text) = str_field(block, "text") {
                                    out.push(AgentEvent::Log(format!("claude subagent: {text}")));
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            Some("result") => {
                self.result_seen = true;
                self.turn_finished = true;
                if let Some(session) = str_field(&value, "session_id") {
                    out.push(AgentEvent::SessionDiscovered(AgentSessionId(
                        session.to_string(),
                    )));
                }
                self.flush_message(&mut out);
                // Stream-json normally delivers the terminal text through
                // stream_event. Keep the result-body fallback for CLI builds
                // that omit those partials, without duplicating a flushed
                // final assistant message.
                if !self.message_emitted
                    && let Some(text) = str_field(&value, "result").filter(|text| !text.is_empty())
                {
                    out.push(AgentEvent::MessageCompleted(bounded_text(
                        text,
                        MAX_MESSAGE_TEXT_BYTES,
                    )));
                    self.message_emitted = true;
                }
                if value.get("is_error").and_then(Value::as_bool) == Some(true) {
                    out.push(AgentEvent::Fatal(result_error_message(&value)));
                }
            }
            Some(other) => out.push(AgentEvent::Log(format!("claude event: {other}"))),
            None => out.push(AgentEvent::ParseWarning(format!(
                "claude: event without type: {}",
                summarize(trimmed, 120)
            ))),
        }
        out
    }

    fn finish(&mut self) -> Vec<AgentEvent> {
        let mut out = Vec::new();
        self.flush_message(&mut out);
        out
    }

    fn take_replayed_user_messages(&mut self) -> Vec<String> {
        std::mem::take(&mut self.replayed)
    }

    fn take_turn_finished(&mut self) -> bool {
        std::mem::take(&mut self.turn_finished)
    }

    fn finish_after_exit(&mut self, ok: bool) -> Vec<AgentEvent> {
        if ok && !self.result_seen {
            vec![AgentEvent::Fatal(
                "claude exited without a terminal result event".to_string(),
            )]
        } else {
            Vec::new()
        }
    }
}

/// The message of an `is_error` result: its text, else the `errors` list some
/// CLI builds send with an empty result, else the result's subtype.
fn result_error_message(value: &Value) -> String {
    if let Some(text) = str_field(value, "result").filter(|text| !text.trim().is_empty()) {
        return text.to_string();
    }
    let errors: Vec<&str> = value
        .get("errors")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|text| !text.trim().is_empty())
        .collect();
    if !errors.is_empty() {
        return errors.join("; ");
    }
    match str_field(value, "subtype").filter(|text| !text.is_empty()) {
        Some(subtype) => format!("claude reported an error ({subtype})"),
        None => "claude reported an error".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::team::TeamMember;

    /// The text content of the first stream-json user line on stdin.
    fn stdin_text(command: &AdapterCommand) -> String {
        let line: Value = serde_json::from_str(command.stdin.as_deref().unwrap()).unwrap();
        line["message"]["content"].as_str().unwrap().to_string()
    }

    fn parse_all(lines: &[&str]) -> Vec<AgentEvent> {
        let mut parser = ClaudeLineParser::default();
        let mut out = Vec::new();
        for line in lines {
            out.extend(parser.parse_line(line));
        }
        out.extend(parser.finish());
        out
    }

    #[test]
    fn default_claude_member_passes_accept_edits() {
        let member = crate::domain::config::default_member(BackendKind::Claude);
        let adapter = ClaudeStreamAdapter::from_member(&member, Path::new("/tmp/ws"));
        let command = adapter.build_command("hello", None, None);
        assert!(
            command
                .args
                .windows(2)
                .any(|w| w == ["--permission-mode", "acceptEdits"])
        );
    }

    #[test]
    fn command_uses_stream_json_and_resume() {
        let mut member = TeamMember::new("reviewer", "Reviewer", BackendKind::Claude, "review");
        member.permission_mode = Some(PermissionMode::Plan);
        member.allowed_tools = vec!["Read".to_string(), "Bash".to_string()];
        let adapter = ClaudeStreamAdapter::from_member(&member, Path::new("/tmp/ws"));

        let command = adapter.build_command(
            "hello",
            Some(&AgentSessionId("sess-1".to_string())),
            Some(Effort::High),
        );
        assert_eq!(command.program, "claude");
        assert!(command.args.contains(&"--output-format".to_string()));
        assert!(command.args.contains(&"stream-json".to_string()));
        assert!(
            command
                .args
                .contains(&"--include-partial-messages".to_string())
        );
        assert!(
            command
                .args
                .contains(&"--forward-subagent-text".to_string())
        );
        assert!(command.args.windows(2).any(|w| w == ["--resume", "sess-1"]));
        assert!(
            command
                .args
                .windows(2)
                .any(|w| w == ["--name", "Asterline · Reviewer"])
        );
        assert!(command.args.windows(2).any(|w| w == ["--effort", "high"]));
        // The shared skills and this project's state sit outside the workspace.
        for dir in crate::state_paths::agent_access_dirs(Path::new("/tmp/ws")) {
            assert!(
                command
                    .args
                    .windows(2)
                    .any(|w| w[0] == "--add-dir" && w[1] == dir.display().to_string()),
                "missing --add-dir {}: {:?}",
                dir.display(),
                command.args
            );
        }
        assert!(
            command
                .args
                .windows(2)
                .any(|w| w == ["--permission-mode", "plan"])
        );
        assert!(
            command
                .args
                .windows(2)
                .any(|w| w == ["--tools", "Read,Bash"])
        );
        assert!(!command.args.iter().any(|arg| arg == "--allowed-tools"));
        // The first message is a stream-json user line; stdin stays open so
        // later messages can be injected, and Claude echoes consumed ones.
        assert!(
            command
                .args
                .windows(2)
                .any(|w| w == ["--input-format", "stream-json"])
        );
        assert!(command.args.contains(&"--replay-user-messages".to_string()));
        assert!(command.keep_stdin_open);
        let first: Value = serde_json::from_str(command.stdin.as_deref().unwrap()).unwrap();
        assert_eq!(
            first,
            serde_json::json!({
                "type": "user",
                "message": { "role": "user", "content": "hello" },
            })
        );
        assert!(command.stdin.as_deref().unwrap().ends_with('\n'));
        assert!(!command.args.iter().any(|arg| arg == "hello"));
        let img_dir =
            std::env::temp_dir().join(format!("asterline-claude-img-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&img_dir);
        let img = img_dir.join("shot.png");
        std::fs::write(&img, [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n']).unwrap();
        let with_image = adapter.build_command(
            &format!("hello\n[asterline-image]: {}", img.display()),
            None,
            None,
        );
        let expected = format!("hello\n(attached image: {})", img.display());
        assert_eq!(stdin_text(&with_image), expected);
        let _ = std::fs::remove_dir_all(&img_dir);
        // The product path never disables session persistence.
        assert!(!command.args.iter().any(|a| a == "--no-session-persistence"));
    }

    #[test]
    fn command_omits_permission_mode_default() {
        let mut member = TeamMember::new("reviewer", "Reviewer", BackendKind::Claude, "review");
        member.permission_mode = Some(PermissionMode::Default);
        let adapter = ClaudeStreamAdapter::from_member(&member, Path::new("/tmp/ws"));
        let command = adapter.build_command("hello", None, None);
        assert!(!command.args.iter().any(|a| a == "--permission-mode"));
        assert!(!command.args.iter().any(|a| a == "default"));
    }

    #[test]
    fn option_shaped_prompt_is_sent_on_stdin() {
        let member = TeamMember::new("reviewer", "Reviewer", BackendKind::Claude, "review");
        let adapter = ClaudeStreamAdapter::from_member(&member, Path::new("/tmp/ws"));
        let command = adapter.build_command("--version", None, None);

        assert_eq!(stdin_text(&command), "--version");
        assert!(!command.args.iter().any(|arg| arg == "--version"));
    }

    #[test]
    fn adapter_offers_a_steer_encoder_that_matches_the_first_message_format() {
        let member = TeamMember::new("reviewer", "Reviewer", BackendKind::Claude, "review");
        let adapter = ClaudeStreamAdapter::from_member(&member, Path::new("/tmp/ws"));
        let encode = adapter.steer_encoder().expect("claude can be steered");
        let (line, echoed) = encode("also check the tests");
        assert!(line.ends_with('\n'));
        assert_eq!(line.matches('\n').count(), 1);
        let parsed: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed["type"], "user");
        assert_eq!(parsed["message"]["role"], "user");
        assert_eq!(parsed["message"]["content"], "also check the tests");
        assert_eq!(echoed, "also check the tests");
    }

    #[test]
    fn replayed_user_messages_are_not_chat_events_but_are_reported_to_the_runner() {
        let mut parser = ClaudeLineParser::default();
        // String content, as Claude echoes a stdin message.
        let events = parser.parse_line(
            r#"{"type":"user","message":{"role":"user","content":"first"},"parent_tool_use_id":null,"isReplay":true}"#,
        );
        assert!(events.is_empty(), "{events:?}");
        // Text-block content.
        let events = parser.parse_line(
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"second"}]}}"#,
        );
        assert!(events.is_empty(), "{events:?}");
        assert_eq!(
            parser.take_replayed_user_messages(),
            vec!["first".to_string(), "second".to_string()]
        );
        assert!(parser.take_replayed_user_messages().is_empty());
    }

    #[test]
    fn tool_results_and_child_prompts_are_not_replays() {
        let mut parser = ClaudeLineParser::default();
        let events = parser.parse_line(
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}"#,
        );
        assert!(matches!(
            events.as_slice(),
            [AgentEvent::ToolCompleted { ok: true, .. }]
        ));
        let events = parser.parse_line(
            r#"{"type":"user","message":{"role":"user","content":"child task"},"parent_tool_use_id":"toolu_1"}"#,
        );
        assert!(events.is_empty());
        assert!(parser.take_replayed_user_messages().is_empty());
    }

    #[test]
    fn a_result_marks_the_turn_finished_once() {
        let mut parser = ClaudeLineParser::default();
        assert!(!parser.take_turn_finished());
        parser.parse_line(
            r#"{"type":"result","subtype":"success","is_error":false,"result":"done"}"#,
        );
        assert!(parser.take_turn_finished());
        assert!(!parser.take_turn_finished());
    }

    #[test]
    fn captures_session_from_init() {
        let events = parse_all(&[
            r#"{"type":"system","subtype":"init","session_id":"sess-abc","model":"claude"}"#,
        ]);
        assert_eq!(
            events,
            vec![AgentEvent::SessionDiscovered(AgentSessionId(
                "sess-abc".to_string()
            ))]
        );
    }

    #[test]
    fn thinking_tokens_progress_rows_are_not_logged() {
        let events = parse_all(&[
            r#"{"type":"system","subtype":"thinking_tokens","estimated_tokens":15,"session_id":"sess-abc"}"#,
            r#"{"type":"system","subtype":"thinking_tokens","estimated_tokens":40,"session_id":"sess-abc"}"#,
        ]);
        // Start the thinking clock once; do not log every progress row.
        assert!(
            events.contains(&AgentEvent::SessionDiscovered(AgentSessionId(
                "sess-abc".to_string()
            )))
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, AgentEvent::Reasoning(text) if text.is_empty()))
                .count(),
            1
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, AgentEvent::Log(_))),
            "{events:?}"
        );
        let events = parse_all(&[r#"{"type":"system","subtype":"compact_boundary"}"#]);
        assert!(
            matches!(events.as_slice(), [AgentEvent::Log(msg)] if msg.contains("compact_boundary"))
        );
    }

    #[test]
    fn buffers_final_text_until_the_assistant_message_is_complete() {
        let events = parse_all(&[
            r#"{"type":"stream_event","event":{"type":"message_start","message":{}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello "}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"world"}}}"#,
            r#"{"type":"stream_event","event":{"type":"message_stop"}}"#,
        ]);
        assert_eq!(
            events,
            vec![AgentEvent::MessageCompleted("Hello world".to_string())]
        );
    }

    #[test]
    fn tool_call_progress_emits_explanatory_text_ahead_of_the_tool() {
        let events = parse_all(&[
            r#"{"type":"stream_event","event":{"type":"message_start","message":{}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"I will inspect it."}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"Read","input":{}}}}"#,
            r#"{"type":"stream_event","event":{"type":"message_stop"}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"contents","is_error":false}]}}"#,
            r#"{"type":"stream_event","event":{"type":"message_start","message":{}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Done."}}}"#,
            r#"{"type":"stream_event","event":{"type":"message_stop"}}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"Done."}"#,
        ]);

        let intermediate_msg = events
            .iter()
            .position(
                |event| matches!(event, AgentEvent::MessageCompleted(text) if text.contains("inspect")),
            )
            .expect("intermediate assistant message before tool");
        let tool_started = events
            .iter()
            .position(
                |event| matches!(event, AgentEvent::ToolStarted { id, .. } if id == "toolu_1"),
            )
            .expect("tool start");
        let tool_completed = events
            .iter()
            .position(
                |event| matches!(event, AgentEvent::ToolCompleted { id, .. } if id == "toolu_1"),
            )
            .expect("tool result");
        let final_message = events
            .iter()
            .position(
                |event| matches!(event, AgentEvent::MessageCompleted(text) if text == "Done."),
            )
            .expect("final assistant message");
        assert!(
            intermediate_msg < tool_started
                && tool_started < tool_completed
                && tool_completed < final_message
        );
    }

    #[test]
    fn tool_use_start_and_result() {
        let events = parse_all(&[
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"Bash","input":{}}}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"done","is_error":false}]}}"#,
        ]);
        assert_eq!(
            events,
            vec![
                AgentEvent::ToolStarted {
                    id: "toolu_1".to_string(),
                    name: "Bash".to_string(),
                    summary: "Bash".to_string(),
                },
                AgentEvent::ToolCompleted {
                    id: "toolu_1".to_string(),
                    ok: true,
                    summary: "done".to_string(),
                },
            ]
        );
    }

    #[test]
    fn thinking_delta_is_forwarded_as_reasoning() {
        let events = parse_all(&[
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"checking invariants"}}}"#,
        ]);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, AgentEvent::Reasoning(text) if text.is_empty()))
                .count(),
            1,
            "thinking block start must open the clock once: {events:?}"
        );
        assert!(events.contains(&AgentEvent::Reasoning("checking invariants".to_string())));
    }

    #[test]
    fn forwarded_subagent_thinking_is_not_dropped() {
        let events = parse_all(&[
            r#"{"type":"assistant","parent_tool_use_id":"toolu_parent","message":{"content":[{"type":"thinking","thinking":"child analysis"},{"type":"text","text":"child result"}]}}"#,
        ]);
        assert!(events.contains(&AgentEvent::Reasoning(
            "[subagent] child analysis".to_string()
        )));
        assert!(events.iter().any(
            |event| matches!(event, AgentEvent::Log(message) if message.contains("child result"))
        ));
    }

    #[test]
    fn main_agent_assistant_messages_with_a_null_parent_are_not_subagent_output() {
        let events = parse_all(&[
            r#"{"type":"assistant","parent_tool_use_id":null,"message":{"content":[{"type":"thinking","thinking":"","signature":"sig"},{"type":"text","text":"main answer"}]}}"#,
            r#"{"type":"assistant","message":{"parent_tool_use_id":null,"content":[{"type":"thinking","thinking":"main thoughts"}]}}"#,
        ]);
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, AgentEvent::Reasoning(_))),
            "{events:?}"
        );
        assert!(
            !events.iter().any(
                |event| matches!(event, AgentEvent::Log(message) if message.contains("subagent"))
            ),
            "{events:?}"
        );
    }

    #[test]
    fn child_messages_need_a_real_parent_and_non_empty_thinking() {
        let events = parse_all(&[
            r#"{"type":"assistant","parent_tool_use_id":"toolu_x","message":{"content":[{"type":"thinking","thinking":"child thoughts"}]}}"#,
            r#"{"type":"assistant","message":{"parent_tool_use_id":"toolu_y","content":[{"type":"thinking","thinking":"nested parent"}]}}"#,
            r#"{"type":"assistant","parent_tool_use_id":"toolu_z","message":{"content":[{"type":"thinking","thinking":"  \n","signature":"sig"}]}}"#,
        ]);
        let reasoning: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::Reasoning(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            reasoning,
            ["[subagent] child thoughts", "[subagent] nested parent"]
        );
    }

    #[test]
    fn tool_input_and_multiline_result_are_preserved() {
        let events = parse_all(&[
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"Bash","input":{}}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"command\":\"printf hi\""}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"}"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop","index":1}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":[{"type":"text","text":"line one\n  line two"}],"is_error":false}]}}"#,
        ]);

        assert!(events.contains(&AgentEvent::ToolProgress {
            id: "toolu_1".to_string(),
            delta: "input:\n{\"command\":\"printf hi\"".to_string(),
        }));
        assert!(events.contains(&AgentEvent::ToolCompleted {
            id: "toolu_1".to_string(),
            ok: true,
            summary: "line one\n  line two".to_string(),
        }));
    }

    #[test]
    fn result_captures_session_and_flushes_open_message() {
        let events = parse_all(&[
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"partial"}}}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"partial","session_id":"sess-z"}"#,
        ]);
        assert!(
            events.contains(&AgentEvent::SessionDiscovered(AgentSessionId(
                "sess-z".to_string()
            )))
        );
        assert!(events.contains(&AgentEvent::MessageCompleted("partial".to_string())));
    }

    #[test]
    fn invalid_json_is_a_parse_warning() {
        let events = parse_all(&[r#"{"type":"system""#]);
        assert!(matches!(events.as_slice(), [AgentEvent::ParseWarning(_)]));
    }

    #[test]
    fn error_result_is_fatal() {
        let events = parse_all(&[
            r#"{"type":"result","is_error":true,"result":"rate limited","session_id":"s"}"#,
        ]);
        assert!(events.contains(&AgentEvent::Fatal("rate limited".to_string())));
    }

    #[test]
    fn error_result_without_text_still_names_the_cause() {
        let events = parse_all(&[
            r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":"","errors":["model 'x' not found"]}"#,
        ]);
        assert!(events.contains(&AgentEvent::Fatal("model 'x' not found".to_string())));
        let events =
            parse_all(&[r#"{"type":"result","subtype":"error_max_turns","is_error":true}"#]);
        assert!(events.contains(&AgentEvent::Fatal(
            "claude reported an error (error_max_turns)".to_string()
        )));
    }

    #[test]
    fn successful_exit_requires_a_terminal_result_event() {
        let mut parser = ClaudeLineParser::default();
        assert!(matches!(
            parser.finish_after_exit(true).as_slice(),
            [AgentEvent::Fatal(message)] if message.contains("terminal result")
        ));

        parser.parse_line(r#"{"type":"result","is_error":false}"#);
        assert!(parser.finish_after_exit(true).is_empty());
    }

    // --- end to end through ProcessRunner with a fake `claude` script -------

    #[cfg(unix)]
    mod fake_process {
        use super::*;
        use crate::adapter::{MemberRunner, ProcessRunner, RunRequest, steer_channel};
        use std::os::unix::fs::PermissionsExt;
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;
        use std::sync::mpsc;

        const RESULT: &str = r#"{"type":"result","subtype":"success","is_error":false,"result":"done","session_id":"s1"}"#;
        const REPLAY_FIRST: &str =
            r#"{"type":"user","message":{"role":"user","content":"hello"},"isReplay":true}"#;
        const REPLAY_STEER: &str =
            r#"{"type":"user","message":{"role":"user","content":"steer me"},"isReplay":true}"#;

        struct Fixture {
            dir: std::path::PathBuf,
        }

        impl Fixture {
            fn new(name: &str, script: &str) -> Self {
                let dir = std::env::temp_dir()
                    .join(format!("asterline-claude-{name}-{}", std::process::id()));
                let _ = std::fs::remove_dir_all(&dir);
                std::fs::create_dir_all(&dir).unwrap();
                let path = dir.join("fake-claude");
                std::fs::write(&path, script).unwrap();
                let mut permissions = std::fs::metadata(&path).unwrap().permissions();
                permissions.set_mode(0o755);
                std::fs::set_permissions(&path, permissions).unwrap();
                Self { dir }
            }

            /// Run one turn with `steer_messages` already waiting in the
            /// run's steer channel; return every event the runner emitted.
            fn run(&self, steer_messages: &[&str]) -> Vec<AgentEvent> {
                let member = TeamMember::new("rev", "Rev", BackendKind::Claude, "review");
                let adapter = ClaudeStreamAdapter::from_member(&member, &self.dir)
                    .with_binary(self.dir.join("fake-claude").display().to_string());
                let runner = ProcessRunner::new(adapter);
                assert!(runner.supports_steer());
                let (tx, rx) = steer_channel();
                for message in steer_messages {
                    tx.send((*message).to_string()).unwrap();
                }
                let (events, received) = mpsc::sync_channel(256);
                runner.run(
                    RunRequest {
                        prompt: "hello".to_string(),
                        session: None,
                        cancel: Arc::new(AtomicBool::new(false)),
                        effort: None,
                        steer: Some(rx),
                    },
                    events,
                );
                // The run is over: further steers are refused.
                assert!(tx.send("late".to_string()).is_err());
                received.try_iter().collect()
            }
        }

        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.dir);
            }
        }

        fn count(events: &[AgentEvent], wanted: &AgentEvent) -> usize {
            events.iter().filter(|event| *event == wanted).count()
        }

        #[test]
        fn message_consumed_before_the_result_is_delivered_and_stdin_then_closes() {
            let script = format!(
                r#"#!/bin/bash
read -r first
printf '%s\n' '{REPLAY_FIRST}'
read -r second
printf '%s\n' '{REPLAY_STEER}'
printf '%s\n' '{RESULT}'
# Claude waits for more input after `result`; the runner must close stdin.
cat > /dev/null
"#
            );
            let fixture = Fixture::new("delivered", &script);
            let events = fixture.run(&["steer me"]);
            assert_eq!(count(&events, &AgentEvent::SteerDelivered), 1, "{events:?}");
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, AgentEvent::SteerRejected { .. })),
                "{events:?}"
            );
            assert!(
                events.contains(&AgentEvent::Exited {
                    code: Some(0),
                    ok: true
                }),
                "{events:?}"
            );
            // Echoed user messages never become chat content.
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, AgentEvent::MessageCompleted(text) if text.contains("steer me"))),
                "{events:?}"
            );
        }

        #[test]
        fn message_not_yet_consumed_at_the_result_keeps_stdin_open_for_the_next_turn() {
            let marker = "stdin-state";
            let script = format!(
                r#"#!/bin/bash
read -r first
printf '%s\n' '{REPLAY_FIRST}'
read -r second
# The first turn ends before the injected message was consumed.
printf '%s\n' '{RESULT}'
read -t 0.7 _ ; echo "$?" > {marker}
printf '%s\n' '{REPLAY_STEER}'
printf '%s\n' '{RESULT}'
cat > /dev/null
"#
            );
            let fixture = Fixture::new("pending", &script);
            let events = fixture.run(&["steer me"]);
            assert_eq!(count(&events, &AgentEvent::SteerDelivered), 1, "{events:?}");
            assert!(
                events.contains(&AgentEvent::Exited {
                    code: Some(0),
                    ok: true
                }),
                "{events:?}"
            );
            // `read -t` timing out (status above 128) means stdin was still
            // open after the first result; EOF would have given status 1.
            let status: i32 = std::fs::read_to_string(fixture.dir.join(marker))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            assert!(
                status > 128,
                "stdin closed too early (read status {status})"
            );
        }

        #[test]
        fn message_the_process_never_consumed_is_handed_back() {
            let script = format!(
                r#"#!/bin/bash
read -r first
printf '%s\n' '{REPLAY_FIRST}'
read -r second
printf '%s\n' '{RESULT}'
exit 0
"#
            );
            let fixture = Fixture::new("rejected", &script);
            let events = fixture.run(&["steer me"]);
            assert_eq!(
                count(
                    &events,
                    &AgentEvent::SteerRejected {
                        prompt: "steer me".to_string()
                    }
                ),
                1,
                "{events:?}"
            );
            assert_eq!(count(&events, &AgentEvent::SteerDelivered), 0);
            // The rejection precedes the exit so the runtime sees it first.
            let rejected = events
                .iter()
                .position(|event| matches!(event, AgentEvent::SteerRejected { .. }))
                .unwrap();
            let exited = events
                .iter()
                .position(|event| matches!(event, AgentEvent::Exited { .. }))
                .unwrap();
            assert!(rejected < exited);
        }

        #[test]
        fn run_without_steered_messages_still_closes_stdin_after_the_result() {
            let script = format!(
                r#"#!/bin/bash
read -r first
printf '%s\n' '{REPLAY_FIRST}'
printf '%s\n' '{RESULT}'
cat > /dev/null
"#
            );
            let fixture = Fixture::new("plain", &script);
            let events = fixture.run(&[]);
            assert!(
                events.contains(&AgentEvent::Exited {
                    code: Some(0),
                    ok: true
                }),
                "{events:?}"
            );
            assert_eq!(count(&events, &AgentEvent::SteerDelivered), 0);
        }
    }
}
