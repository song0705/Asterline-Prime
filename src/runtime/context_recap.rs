//! Carry a conversation across a model or CLI switch.
//!
//! Each backend CLI keeps its own native session. When a prompt goes to a
//! session that has not seen part of the member's conversation (a different
//! CLI, or one that missed turns while another model answered), the runtime
//! prepends a compact recap of that unseen part. The recap is plain text for
//! the model only: it is never shown in the transcript or stored as a chat
//! message.

use crate::domain::event::ChatItem;
use crate::domain::team::{MemberId, TeamConfig};

/// Sender shown for notes Asterline itself delivers to a member.
pub const SYSTEM_SENDER: &str = "asterline";

/// The recap, header and footer included, never exceeds this many characters.
pub const RECAP_MAX_CHARS: usize = 40_000;

/// One message longer than this is cut, so a single pasted log cannot push the
/// rest of the conversation out of the recap.
const MESSAGE_MAX_CHARS: usize = 6_000;
const WORK_LINE_MAX_CHARS: usize = 110;
const WORK_LINES_PER_GROUP: usize = 16;
const FILES_PER_CHANGE: usize = 12;

/// First line of every recap.
pub const RECAP_HEADER: &str =
    "Earlier in this conversation (handled by another model). Continue from here:";
const RECAP_INTRO: &str = "The workspace is shared, so read files for details. Lines starting with \"-\" are short summaries of work done, not full output.";
/// Last line of every recap; the new message follows it.
pub const RECAP_FOOTER: &str = "--- End of earlier conversation. The new message follows. ---";

/// The text shown after a switch when part of the history was dropped.
pub const NOT_CARRIED_OVER: &str = "earlier messages not carried over";

/// A recap ready to put in front of a prompt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Recap {
    /// Header, conversation lines and footer.
    pub text: String,
    /// Whether the oldest part was dropped to stay within [`RECAP_MAX_CHARS`].
    pub truncated: bool,
}

impl Recap {
    /// The recap followed by the actual message.
    pub fn prepend_to(&self, prompt: &str) -> String {
        format!("{}\n\n{}", self.text, prompt)
    }
}

/// The message a prompt carries, without a recap in front of it.
pub fn without_recap(prompt: &str) -> &str {
    if prompt.starts_with(RECAP_HEADER)
        && let Some((_, message)) = prompt.split_once(RECAP_FOOTER)
    {
        return message.trim_start_matches('\n');
    }
    prompt
}

/// Whether `item` is part of `member`'s own conversation: what the user said
/// to it, what it replied and did, and what was sent to or from it.
pub fn member_sees(item: &ChatItem, member: &MemberId) -> bool {
    match item {
        ChatItem::User { targets, .. } => targets.contains(member),
        ChatItem::Agent { member: from, .. }
        | ChatItem::Tool { member: from, .. }
        | ChatItem::Diff { member: from, .. } => from == member,
        ChatItem::Route { from, to, .. } => {
            from == member || to.iter().any(|target| target == member.as_str())
        }
        _ => false,
    }
}

enum Block {
    Message(String),
    /// Consecutive work lines; `hidden` more were left out past the limit.
    Work {
        lines: Vec<String>,
        hidden: usize,
    },
}

impl Block {
    fn render(&self) -> String {
        match self {
            Block::Message(text) => text.clone(),
            Block::Work { lines, hidden } => {
                let mut text = lines.join("\n");
                if *hidden > 0 {
                    text.push_str(&format!("\n- …and {hidden} more"));
                }
                text
            }
        }
    }
}

/// Summarize `items` (the part of `member`'s conversation its session has not
/// seen, oldest first) as a recap. `None` when nothing in it concerns the
/// member.
pub fn build_recap(items: &[ChatItem], member: &MemberId, config: &TeamConfig) -> Option<Recap> {
    let blocks = blocks_for(items, member, config);
    if blocks.is_empty() {
        return None;
    }

    // Keep the newest blocks that fit; the dropped older part becomes one line.
    let fixed = RECAP_HEADER.chars().count()
        + RECAP_INTRO.chars().count()
        + RECAP_FOOTER.chars().count()
        + OMITTED_LINE_RESERVE
        + 8;
    let mut budget = RECAP_MAX_CHARS.saturating_sub(fixed);
    let mut keep_from = blocks.len();
    for (index, block) in blocks.iter().enumerate().rev() {
        let cost = block.render().chars().count() + 2;
        if cost > budget {
            break;
        }
        budget -= cost;
        keep_from = index;
    }
    let dropped = &blocks[..keep_from];
    let omitted_messages = dropped
        .iter()
        .filter(|block| matches!(block, Block::Message(_)))
        .count();

    let mut text = String::new();
    text.push_str(RECAP_HEADER);
    text.push('\n');
    text.push_str(RECAP_INTRO);
    text.push_str("\n\n");
    if !dropped.is_empty() {
        text.push_str(&omitted_line(omitted_messages));
        text.push_str("\n\n");
    }
    for block in &blocks[keep_from..] {
        text.push_str(&block.render());
        text.push_str("\n\n");
    }
    text.push_str(RECAP_FOOTER);
    Some(Recap {
        text,
        truncated: !dropped.is_empty(),
    })
}

/// Room kept for the "omitted" line, whatever the count.
const OMITTED_LINE_RESERVE: usize = 80;

fn omitted_line(messages: usize) -> String {
    match messages {
        0 => "(Earlier work summaries were omitted.)".to_string(),
        1 => "(1 earlier message was omitted.)".to_string(),
        n => format!("({n} earlier messages were omitted.)"),
    }
}

fn blocks_for(items: &[ChatItem], me: &MemberId, config: &TeamConfig) -> Vec<Block> {
    let name = |id: &str| -> String {
        if id == SYSTEM_SENDER {
            return "Asterline".to_string();
        }
        config
            .find(id)
            .map(|member| member.display_name.clone())
            .unwrap_or_else(|| id.to_string())
    };
    let parent_of =
        |id: &str| -> Option<MemberId> { config.find(id).and_then(|member| member.parent.clone()) };
    let my_name = name(me.as_str());

    let mut blocks: Vec<Block> = Vec::new();
    for item in items.iter().filter(|item| member_sees(item, me)) {
        match item {
            ChatItem::User { body, .. } => {
                blocks.push(Block::Message(labelled("User", body)));
            }
            ChatItem::Agent { text, .. } => {
                blocks.push(Block::Message(labelled(
                    &format!("Assistant ({my_name})"),
                    text,
                )));
            }
            ChatItem::Route { from, to, body } => {
                let label = if from == me {
                    let names = to.iter().map(|id| name(id)).collect::<Vec<_>>().join(", ");
                    let all_children =
                        !to.is_empty() && to.iter().all(|id| parent_of(id).as_ref() == Some(me));
                    if all_children {
                        format!("You gave a task to sub-agent {names}")
                    } else {
                        format!("You sent to {names}")
                    }
                } else if from.as_str() == SYSTEM_SENDER {
                    "Note from Asterline".to_string()
                } else if parent_of(me.as_str()).as_ref() == Some(from) {
                    format!("Task from {} (your parent agent)", name(from.as_str()))
                } else if parent_of(from.as_str()).as_ref() == Some(me) {
                    format!("Sub-agent {} handed back", name(from.as_str()))
                } else {
                    format!("Message from {}", name(from.as_str()))
                };
                blocks.push(Block::Message(labelled(&label, body)));
            }
            ChatItem::Tool {
                name: tool,
                summary,
                ok,
                ..
            } => {
                let outcome = match ok {
                    Some(true) => " (ok)",
                    Some(false) => " (failed)",
                    None => "",
                };
                let what = one_line(summary, WORK_LINE_MAX_CHARS);
                let line = if what.is_empty() {
                    format!("- ran {tool}{outcome}")
                } else {
                    format!("- ran {tool}: {what}{outcome}")
                };
                push_work(&mut blocks, line);
            }
            ChatItem::Diff { files, ok, .. } => {
                if files.is_empty() {
                    continue;
                }
                let mut listed: Vec<String> = files
                    .iter()
                    .take(FILES_PER_CHANGE)
                    .map(|file| format!("{} ({})", file.path, file.kind))
                    .collect();
                if files.len() > FILES_PER_CHANGE {
                    listed.push(format!("+{} more", files.len() - FILES_PER_CHANGE));
                }
                let outcome = if *ok { "" } else { " (failed)" };
                push_work(
                    &mut blocks,
                    format!("- changed files{outcome}: {}", listed.join(", ")),
                );
            }
            _ => {}
        }
    }
    blocks
}

/// Add a work line to the group of consecutive work lines at the end, or start
/// a new group. A group past its line limit only counts what it leaves out.
fn push_work(blocks: &mut Vec<Block>, line: String) {
    if let Some(Block::Work { lines, hidden }) = blocks.last_mut() {
        if lines.len() < WORK_LINES_PER_GROUP {
            lines.push(line);
        } else {
            *hidden += 1;
        }
        return;
    }
    blocks.push(Block::Work {
        lines: vec![line],
        hidden: 0,
    });
}

fn labelled(label: &str, text: &str) -> String {
    format!("{label}: {}", clip(text.trim(), MESSAGE_MAX_CHARS))
}

/// The first non-empty line, whitespace collapsed, cut to `max` characters.
fn one_line(text: &str, max: usize) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    let collapsed = line.split_whitespace().collect::<Vec<_>>().join(" ");
    clip(&collapsed, max)
}

fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut clipped: String = text.chars().take(max.saturating_sub(1)).collect();
    clipped.push('…');
    clipped
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::event::FileChangeItem;
    use crate::domain::team::{BackendKind, TeamMember};

    fn config() -> TeamConfig {
        let mut config = TeamConfig::new("t", "/tmp");
        config.members.push(TeamMember::new(
            "main",
            "Main",
            BackendKind::Claude,
            "implementation",
        ));
        config.members.push(TeamMember::new(
            "reviewer",
            "Reviewer",
            BackendKind::Codex,
            "review",
        ));
        let mut child = TeamMember::new("scout", "Scout", BackendKind::Codex, "scout");
        child.parent = Some(MemberId::new("main"));
        config.members.push(child);
        config
    }

    fn user(body: &str, to: &[&str]) -> ChatItem {
        ChatItem::User {
            body: body.to_string(),
            targets: to.iter().map(|id| MemberId::new(*id)).collect(),
            interrupted: Vec::new(),
        }
    }

    fn agent(id: &str, text: &str) -> ChatItem {
        ChatItem::Agent {
            member: MemberId::new(id),
            display_name: id.to_string(),
            backend: BackendKind::Codex,
            text: text.to_string(),
        }
    }

    fn tool(id: &str, name: &str, summary: &str, ok: Option<bool>) -> ChatItem {
        ChatItem::Tool {
            member: MemberId::new(id),
            name: name.to_string(),
            summary: summary.to_string(),
            detail: "FULL OUTPUT THAT MUST NOT APPEAR".to_string(),
            ok,
        }
    }

    fn route(from: &str, to: &[&str], body: &str) -> ChatItem {
        ChatItem::Route {
            from: MemberId::new(from),
            to: to.iter().map(|id| id.to_string()).collect(),
            body: body.to_string(),
        }
    }

    #[test]
    fn nothing_unseen_yields_no_recap() {
        assert_eq!(build_recap(&[], &MemberId::new("main"), &config()), None);
        // Rows that are another member's business are not this member's recap.
        let items = [
            user("hi reviewer", &["reviewer"]),
            agent("reviewer", "hello"),
        ];
        assert_eq!(build_recap(&items, &MemberId::new("main"), &config()), None);
    }

    #[test]
    fn recap_lists_messages_and_work_in_order_without_tool_output() {
        let items = [
            user("add a parser", &["main"]),
            tool("main", "bash", "cargo test --all\nsecond line", Some(true)),
            tool("main", "bash", "cargo clippy", Some(false)),
            ChatItem::Diff {
                member: MemberId::new("main"),
                files: vec![
                    FileChangeItem::new("src/parser.rs", "add"),
                    FileChangeItem::new("src/lib.rs", "modify"),
                ],
                ok: true,
            },
            agent("main", "Parser added."),
            user("thanks", &["main"]),
        ];
        let recap = build_recap(&items, &MemberId::new("main"), &config()).unwrap();
        assert!(!recap.truncated);
        let text = &recap.text;
        assert!(text.starts_with(RECAP_HEADER), "{text}");
        assert!(text.ends_with(RECAP_FOOTER), "{text}");
        assert!(!text.contains("FULL OUTPUT"), "{text}");
        assert!(!text.contains("second line"), "{text}");
        let positions: Vec<usize> = [
            "User: add a parser",
            "- ran bash: cargo test --all (ok)",
            "- ran bash: cargo clippy (failed)",
            "- changed files: src/parser.rs (add), src/lib.rs (modify)",
            "Assistant (Main): Parser added.",
            "User: thanks",
        ]
        .iter()
        .map(|needle| {
            text.find(needle)
                .unwrap_or_else(|| panic!("{needle}\n{text}"))
        })
        .collect();
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]), "{text}");
    }

    #[test]
    fn routes_and_sub_agent_traffic_are_labelled() {
        let items = [
            route("reviewer", &["main"], "please fix the nit"),
            route("main", &["scout"], "look at the logs"),
            route("scout", &["main"], "found nothing"),
            route("main", &["reviewer"], "done"),
            route(SYSTEM_SENDER, &["main"], "spawn refused"),
        ];
        let recap = build_recap(&items, &MemberId::new("main"), &config()).unwrap();
        let text = &recap.text;
        for needle in [
            "Message from Reviewer: please fix the nit",
            "You gave a task to sub-agent Scout: look at the logs",
            "Sub-agent Scout handed back: found nothing",
            "You sent to Reviewer: done",
            "Note from Asterline: spawn refused",
        ] {
            assert!(text.contains(needle), "{needle}\n{text}");
        }

        let child = build_recap(&items, &MemberId::new("scout"), &config()).unwrap();
        assert!(
            child
                .text
                .contains("Task from Main (your parent agent): look at the logs"),
            "{}",
            child.text
        );
    }

    #[test]
    fn long_runs_of_tools_collapse_to_a_counted_tail() {
        let mut items = vec![user("go", &["main"])];
        for n in 0..40 {
            items.push(tool("main", "bash", &format!("cmd {n}"), Some(true)));
        }
        let recap = build_recap(&items, &MemberId::new("main"), &config()).unwrap();
        assert!(recap.text.contains("- ran bash: cmd 0 (ok)"));
        assert!(recap.text.contains("- ran bash: cmd 15 (ok)"));
        assert!(!recap.text.contains("cmd 16"), "{}", recap.text);
        assert!(recap.text.contains("- …and 24 more"), "{}", recap.text);
    }

    #[test]
    fn recap_keeps_the_newest_part_within_the_bound() {
        let filler = "x".repeat(5_000);
        let items: Vec<ChatItem> = (0..30)
            .map(|n| user(&format!("message {n:02} {filler}"), &["main"]))
            .collect();
        let recap = build_recap(&items, &MemberId::new("main"), &config()).unwrap();
        assert!(recap.truncated);
        assert!(recap.text.chars().count() <= RECAP_MAX_CHARS);
        assert!(recap.text.contains("message 29"));
        assert!(!recap.text.contains("message 00"));
        let omitted = recap
            .text
            .lines()
            .find(|line| line.contains("earlier messages were omitted"))
            .unwrap_or_else(|| panic!("{}", recap.text));
        let count: usize = omitted
            .trim_start_matches('(')
            .split_whitespace()
            .next()
            .unwrap()
            .parse()
            .unwrap();
        let kept = recap.text.matches("User: message").count();
        assert_eq!(count + kept, 30, "{omitted}");
        // Exactly one omitted line, placed right after the header.
        assert_eq!(recap.text.matches("were omitted").count(), 1);
    }

    #[test]
    fn a_single_huge_message_is_cut_not_dropped() {
        let items = [user(&"y".repeat(100_000), &["main"])];
        let recap = build_recap(&items, &MemberId::new("main"), &config()).unwrap();
        assert!(!recap.truncated);
        assert!(recap.text.chars().count() < RECAP_MAX_CHARS);
        assert!(recap.text.contains('…'));
    }

    #[test]
    fn prepending_puts_the_message_last() {
        let items = [user("earlier", &["main"])];
        let recap = build_recap(&items, &MemberId::new("main"), &config()).unwrap();
        let prompt = recap.prepend_to("what now?");
        assert!(prompt.starts_with(RECAP_HEADER));
        assert!(prompt.ends_with("\n\nwhat now?"));
    }
}
