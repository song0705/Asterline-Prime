//! Composer completion: compute `/command` and `@member` suggestions from the
//! text before the cursor. Pure logic so it is fully unit-tested; the popup is
//! rendered and navigated by the TUI.

use std::collections::HashMap;

use crate::domain::team::BackendKind;

/// One suggestion: a label shown in the popup and the text to insert in place
/// of the current token.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompletionItem {
    pub label: String,
    pub insert: String,
}

/// An active completion: a titled list of items replacing the token that starts
/// at `token_start` (a char index into the composer head).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Completion {
    pub title: &'static str,
    pub token_start: usize,
    pub items: Vec<CompletionItem>,
}

/// A backend skill that can be invoked after an explicit `@member` target.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentSkill {
    pub name: String,
    pub description: String,
    /// The native CLI that can invoke this skill.
    pub backend: BackendKind,
    /// Exact text accepted by that backend, including any plugin namespace.
    pub invocation: String,
}

/// (name, hint, takes_argument) in display order: the groups of
/// [`COMMAND_GROUPS`], top to bottom. The `/` popup and the `/help` palette
/// both follow this order. Also feeds the `/help` palette.
///
/// Only commands worth advertising are listed. `/agents` (← opens the same
/// list), `/ask` and `/all` (the `@member` era) and `/focus` (an alias of
/// `/logs <member>`) still run when typed in full; see `commands::parse_slash`.
pub(crate) const COMMANDS: &[(&str, &str, bool)] = &[
    // Common
    (
        "model",
        "change the model of the agent you are viewing",
        true,
    ),
    (
        "new",
        "start a fresh chat, optionally with its first prompt",
        true,
    ),
    ("resume", "pick a saved chat in the agents list", true),
    ("name", "name the current chat", true),
    ("goal", "work toward a goal until it is done", true),
    ("team", "edit roster · sessions · approvals", false),
    ("mode", "open mode panel or switch dispatch mode", true),
    ("find", "search the transcript", true),
    ("diff", "show working-tree git diff", false),
    (
        "retry",
        "retry failed input or the latest user request",
        false,
    ),
    ("help", "show commands", false),
    ("exit", "exit Asterline", false),
    // Approvals & sessions
    ("approve", "approve first pending", false),
    ("reject", "reject first pending", false),
    ("attach", "open a member's native CLI session", true),
    ("import", "import native session transcript into chat", true),
    ("export", "export session to Claude Code format", true),
    (
        "logs",
        "raw logs · stderr · warnings (or one member's)",
        true,
    ),
    // Runs
    ("runs", "run status · next action", false),
    ("continue", "resume latest or selected run", true),
    ("step", "manage run checklist", true),
    ("note", "record a run checkpoint", true),
    ("block", "mark a run blocked", true),
];

/// The command groups shown in `/help`, in display order. Each group lists
/// its command names in the same order as [`COMMANDS`], which is the flat
/// concatenation of these groups.
pub(crate) const COMMAND_GROUPS: &[(&str, &[&str])] = &[
    (
        "Common",
        &[
            "model", "new", "resume", "name", "goal", "team", "mode", "find", "diff", "retry",
            "help", "exit",
        ],
    ),
    (
        "Approvals & sessions",
        &["approve", "reject", "attach", "import", "export", "logs"],
    ),
    ("Runs", &["runs", "continue", "step", "note", "block"]),
];

/// (name, hint) entries shown after `/mode `.
const MODES: &[(&str, &str)] = &[
    ("normal", "keep using direct messages until changed"),
    ("review", "keep using builder/reviewer runs until changed"),
    ("plan", "keep using leader/checklist runs until changed"),
    (
        "brainstorm",
        "keep using multi-wave idea generation until changed",
    ),
    (
        "team",
        "keep using coordinator-driven team runs until changed",
    ),
];

/// Commands that run as typed even though they accept an optional argument.
const OPTIONAL_ARGUMENT_COMMANDS: &[&str] = &["model", "logs", "new", "resume", "goal"];

/// Whether `token` is a whole slash command that needs nothing more before
/// Enter submits it: a known command with no (or only an optional) argument.
pub(crate) fn command_is_complete(token: &str) -> bool {
    let Some(name) = token.strip_prefix('/') else {
        return false;
    };
    COMMANDS.iter().any(|(command, _, takes_arg)| {
        *command == name && (!takes_arg || OPTIONAL_ARGUMENT_COMMANDS.contains(command))
    })
}

/// Compute the completion for `head` (composer text up to the cursor).
pub fn compute(head: &str, members: &[String]) -> Option<Completion> {
    compute_with_agent_skills(head, members, &[], &HashMap::new())
}

/// Compute completion, including backend-native skills after `@member /`.
pub fn compute_with_agent_skills(
    head: &str,
    members: &[String],
    skills: &[AgentSkill],
    member_backends: &HashMap<String, BackendKind>,
) -> Option<Completion> {
    let chars: Vec<char> = head.chars().collect();

    if let Some(completion) = targeted_skill_completion(&chars, skills, member_backends) {
        return Some(completion);
    }

    if head.starts_with('/') {
        return match chars.iter().position(|c| c.is_whitespace()) {
            // Still typing the command name.
            None => {
                let prefix: String = chars[1..].iter().collect();
                let lower = prefix.to_lowercase();
                // Best match first (exact, then prefix, then alias); equally
                // good matches keep the `COMMANDS` group order.
                let mut matches: Vec<(u8, &(&str, &str, bool))> = COMMANDS
                    .iter()
                    .filter_map(|command| {
                        slash_command_rank(command.0, &lower).map(|rank| (rank, command))
                    })
                    .collect();
                matches.sort_by_key(|(rank, _)| *rank);
                let items: Vec<CompletionItem> = matches
                    .into_iter()
                    .map(|(_, (name, hint, _))| CompletionItem {
                        label: format!("/{name} — {hint}"),
                        // `/mode` without an argument opens its required
                        // picker, so keep the completed command submit-ready.
                        insert: if *name == "mode" {
                            format!("/{name}")
                        } else {
                            format!("/{name} ")
                        },
                    })
                    .collect();
                non_empty("commands", 0, items)
            }
            // Command chosen; only the member-taking commands complete their
            // first argument.
            Some(space) => {
                let cmd: String = chars[1..space].iter().collect();
                if cmd == "mode" {
                    let arg: Vec<char> = chars[space + 1..].to_vec();
                    if arg.iter().any(|c| c.is_whitespace()) {
                        return None;
                    }
                    let prefix: String = arg.iter().collect();
                    let lower = prefix.to_lowercase();
                    let items = MODES
                        .iter()
                        .filter(|(name, _)| name.starts_with(&lower))
                        .map(|(name, hint)| CompletionItem {
                            label: format!("{name} — {hint}"),
                            insert: format!("{name} "),
                        })
                        .collect();
                    return non_empty("modes", space + 1, items);
                }
                if !matches!(cmd.as_str(), "ask" | "attach" | "focus" | "logs") {
                    return None;
                }
                let arg: Vec<char> = chars[space + 1..].to_vec();
                // Only while still typing the member token (no further space).
                if arg.iter().any(|c| c.is_whitespace()) {
                    return None;
                }
                let prefix: String = arg.iter().collect();
                let mut candidates: Vec<String> = (cmd == "ask")
                    .then(|| "all".to_string())
                    .into_iter()
                    .collect();
                candidates.extend(members.iter().cloned());
                member_completion(
                    &prefix,
                    space + 1,
                    &candidates,
                    "ask a member",
                    |m| m.to_string(),
                    |m| format!("{m} "),
                )
            }
        };
    }

    // `@member` mention anywhere: complete the last whitespace-delimited token
    // if it starts with '@'.
    let word_start = chars
        .iter()
        .rposition(|c| c.is_whitespace())
        .map(|i| i + 1)
        .unwrap_or(0);
    if chars.get(word_start) == Some(&'@') {
        let prefix: String = chars[word_start + 1..].iter().collect();
        let mut candidates = vec!["all".to_string()];
        candidates.extend(members.iter().cloned());
        return member_completion(
            &prefix,
            word_start,
            &candidates,
            "mention a member",
            |m| format!("@{m}"),
            |m| format!("@{m} "),
        );
    }

    None
}

/// How well `query` matches the command `name`: 0 for the whole name, 1 for a
/// prefix, 2 for an alias prefix (`/clear` is `/new`); `None` for no match.
fn slash_command_rank(name: &str, query: &str) -> Option<u8> {
    if name == query {
        Some(0)
    } else if name.starts_with(query) {
        Some(1)
    } else if name == "new" && "clear".starts_with(query) {
        Some(2)
    } else {
        None
    }
}

fn targeted_skill_completion(
    chars: &[char],
    skills: &[AgentSkill],
    member_backends: &HashMap<String, BackendKind>,
) -> Option<Completion> {
    if chars.first() != Some(&'@') {
        return None;
    }
    let member_end = chars.iter().position(|c| c.is_whitespace())?;
    let member: String = chars[1..member_end].iter().collect();
    // Composer routing accepts both a stable member ID and the member's
    // display name, case-insensitively. Completion must mirror that path so
    // `@Claude /` exposes the same actions that `@claude-primary /` does.
    let target_backend = member_backends.iter().find_map(|(candidate, backend)| {
        candidate.eq_ignore_ascii_case(&member).then_some(*backend)
    })?;
    let command_start = chars[member_end..]
        .iter()
        .position(|c| !c.is_whitespace())?
        + member_end;
    if chars.get(command_start) != Some(&'/')
        || chars[command_start + 1..].iter().any(|c| c.is_whitespace())
    {
        return None;
    }

    let prefix: String = chars[command_start + 1..].iter().collect();
    let lower = prefix.to_lowercase();
    let mut items = Vec::new();
    if "attach".starts_with(&lower) {
        items.push(CompletionItem {
            label: "/attach — open this member's native CLI session".to_string(),
            insert: "/attach ".to_string(),
        });
    }
    items.extend(
        skills
            .iter()
            .filter(|skill| skill.backend == target_backend)
            .filter(|skill| {
                skill.name.to_lowercase().starts_with(&lower)
                    || skill
                        .invocation
                        .trim_start_matches(['/', '$'])
                        .to_lowercase()
                        .starts_with(&lower)
            })
            .map(|skill| CompletionItem {
                label: format!("{} — {}", skill.invocation, skill.description),
                insert: format!("{} ", skill.invocation),
            }),
    );
    non_empty("member actions & skills", command_start, items)
}

fn member_completion(
    prefix: &str,
    token_start: usize,
    members: &[String],
    title: &'static str,
    label: impl Fn(&str) -> String,
    insert: impl Fn(&str) -> String,
) -> Option<Completion> {
    let lower = prefix.to_lowercase();
    let items: Vec<CompletionItem> = members
        .iter()
        .filter(|m| m.to_lowercase().starts_with(&lower))
        .map(|m| CompletionItem {
            label: label(m),
            insert: insert(m),
        })
        .collect();
    non_empty(title, token_start, items)
}

fn non_empty(
    title: &'static str,
    token_start: usize,
    items: Vec<CompletionItem>,
) -> Option<Completion> {
    if items.is_empty() {
        None
    } else {
        Some(Completion {
            title,
            token_start,
            items,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn members() -> Vec<String> {
        vec!["builder".to_string(), "reviewer".to_string()]
    }

    fn inserts(head: &str) -> Vec<String> {
        compute(head, &members())
            .map(|c| c.items.into_iter().map(|i| i.insert).collect())
            .unwrap_or_default()
    }

    #[test]
    fn slash_lists_all_commands() {
        let c = compute("/", &members()).unwrap();
        assert_eq!(c.token_start, 0);
        assert!(c.items.iter().any(|i| i.insert == "/team "));
        assert!(c.items.iter().any(|i| i.insert == "/mode"));
        assert!(c.items.iter().any(|i| i.insert == "/exit "));
        assert!(c.items.iter().any(|i| i.insert == "/attach "));
        // Hidden aliases still run when typed in full, but are not advertised.
        for hidden in ["/ask ", "/all ", "/agents ", "/focus "] {
            assert!(!c.items.iter().any(|i| i.insert == hidden), "{hidden}");
        }
        assert!(!c.items.iter().any(|i| i.insert == "/plan "));
        assert!(!c.items.iter().any(|i| i.insert == "/review "));
        assert!(c.items.iter().any(|i| i.insert == "/continue "));
        assert!(c.items.iter().any(|i| i.insert == "/note "));
        assert!(c.items.iter().any(|i| i.insert == "/block "));
        assert!(c.items.iter().any(|i| i.insert == "/step "));
        assert!(!c.items.iter().any(|i| i.insert == "/skills"));
        assert!(!c.items.iter().any(|i| i.insert == "/abort"));
    }

    #[test]
    fn goal_is_listed_after_name_and_runs_without_an_argument() {
        let c = compute("/go", &[]).expect("completion");
        assert!(c.items.iter().any(|i| i.insert == "/goal "));
        assert!(command_is_complete("/goal"));
        let common = COMMAND_GROUPS[0].1;
        let name = common.iter().position(|c| *c == "name").unwrap();
        assert_eq!(common[name + 1], "goal");
    }

    const EXPECTED_ORDER: [&str; 23] = [
        "model", "new", "resume", "name", "goal", "team", "mode", "find", "diff", "retry", "help",
        "exit", "approve", "reject", "attach", "import", "export", "logs", "runs", "continue",
        "step", "note", "block",
    ];

    #[test]
    fn palette_commands_are_grouped_in_display_order_and_unique() {
        let names: Vec<&str> = COMMANDS.iter().map(|(name, _, _)| *name).collect();
        assert_eq!(names, EXPECTED_ORDER);
        let mut unique = names.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), names.len());
        // The help groups are exactly the flat list, cut into three.
        let grouped: Vec<&str> = COMMAND_GROUPS
            .iter()
            .flat_map(|(_, names)| names.iter().copied())
            .collect();
        assert_eq!(grouped, names);
        assert_eq!(
            COMMAND_GROUPS
                .iter()
                .map(|(title, _)| *title)
                .collect::<Vec<_>>(),
            vec!["Common", "Approvals & sessions", "Runs"]
        );
    }

    #[test]
    fn bare_slash_lists_commands_in_group_order() {
        let c = compute("/", &members()).unwrap();
        let names: Vec<String> = c
            .items
            .iter()
            .map(|i| i.insert.trim().trim_start_matches('/').to_string())
            .collect();
        assert_eq!(names, EXPECTED_ORDER);
    }

    #[test]
    fn logs_completes_a_member_without_offering_all() {
        let c = compute("/logs rev", &members()).expect("logs member completion");
        assert_eq!(
            c.items
                .iter()
                .map(|i| i.insert.as_str())
                .collect::<Vec<_>>(),
            vec!["reviewer "]
        );
        assert!(compute("/logs a", &members()).is_none());
        assert!(command_is_complete("/logs"));
    }

    #[test]
    fn command_references_cover_every_palette_command() {
        let english = include_str!("../../docs/commands.md");
        let chinese = include_str!("../../docs/commands.zh-CN.md");
        for (name, _, _) in COMMANDS {
            let heading = format!("### `/{name}`");
            assert!(
                english.contains(&heading),
                "English command reference is missing {heading}"
            );
            assert!(
                chinese.contains(&heading),
                "Chinese command reference is missing {heading}"
            );
        }
    }

    #[test]
    fn slash_prefix_filters() {
        assert!(inserts("/as").is_empty());
        assert_eq!(
            inserts("/mo"),
            vec!["/model ".to_string(), "/mode".to_string()]
        );
        // The whole command name beats longer names it prefixes.
        assert_eq!(
            inserts("/mode"),
            vec!["/mode".to_string(), "/model ".to_string()]
        );
        assert!(inserts("/pl").is_empty());
        assert_eq!(inserts("/con"), vec!["/continue ".to_string()]);
        assert_eq!(inserts("/no"), vec!["/note ".to_string()]);
        assert_eq!(inserts("/blo"), vec!["/block ".to_string()]);
        assert_eq!(inserts("/ste"), vec!["/step ".to_string()]);
        assert_eq!(
            inserts("/a"),
            vec!["/approve ".to_string(), "/attach ".to_string(),]
        );
        let re = inserts("/re");
        assert_eq!(
            re,
            vec![
                "/resume ".to_string(),
                "/retry ".to_string(),
                "/reject ".to_string()
            ]
        );
        assert!(inserts("/find").contains(&"/find ".to_string()));
        assert_eq!(inserts("/cl"), vec!["/new ".to_string()]);
        assert_eq!(inserts("/clear"), vec!["/new ".to_string()]);
        assert!(inserts("/lead").is_empty());
        assert!(inserts("/round").is_empty());
    }

    #[test]
    fn mode_command_opens_second_level_mode_completion() {
        let modes = compute("/mode ", &members()).unwrap();
        assert_eq!(modes.title, "modes");
        assert_eq!(modes.token_start, 6);
        assert_eq!(
            modes
                .items
                .iter()
                .map(|item| item.insert.as_str())
                .collect::<Vec<_>>(),
            vec!["normal ", "review ", "plan ", "brainstorm ", "team "]
        );
        assert_eq!(inserts("/mode br"), vec!["brainstorm ".to_string()]);
        assert!(compute("/mode review fix it", &members()).is_none());
    }

    #[test]
    fn ask_completes_member_argument() {
        let c = compute("/ask rev", &members()).unwrap();
        assert_eq!(c.title, "ask a member");
        assert_eq!(c.token_start, 5);
        assert_eq!(
            c.items,
            vec![CompletionItem {
                label: "reviewer".to_string(),
                insert: "reviewer ".to_string()
            }]
        );
    }

    #[test]
    fn ask_with_no_prefix_lists_all_members() {
        let c = compute("/ask ", &members()).unwrap();
        assert_eq!(c.items.len(), 3);
        assert_eq!(c.items[0].insert, "all ");
    }

    #[test]
    fn ask_after_member_chosen_has_no_popup() {
        assert!(compute("/ask reviewer hello", &members()).is_none());
    }

    #[test]
    fn other_commands_do_not_complete_args() {
        assert!(compute("/all hello", &members()).is_none());
        assert!(compute("/team ", &members()).is_none());

        let attach = compute("/attach rev", &members()).expect("attach member completion");
        assert_eq!(
            attach
                .items
                .iter()
                .map(|item| item.insert.as_str())
                .collect::<Vec<_>>(),
            vec!["reviewer "]
        );
        assert!(compute("/attach all", &members()).is_none());
    }

    #[test]
    fn at_mention_completes_member() {
        let c = compute("@rev", &members()).unwrap();
        assert_eq!(c.token_start, 0);
        assert_eq!(
            c.items[0],
            CompletionItem {
                label: "@reviewer".to_string(),
                insert: "@reviewer ".to_string()
            }
        );
    }

    #[test]
    fn at_mention_completes_all() {
        let c = compute("@a", &members()).unwrap();
        assert_eq!(c.token_start, 0);
        assert_eq!(
            c.items[0],
            CompletionItem {
                label: "@all".to_string(),
                insert: "@all ".to_string()
            }
        );
    }

    #[test]
    fn at_mention_mid_text() {
        let c = compute("ping @bu", &members()).unwrap();
        assert_eq!(c.token_start, 5);
        assert_eq!(
            c.items[0],
            CompletionItem {
                label: "@builder".to_string(),
                insert: "@builder ".to_string()
            }
        );
    }

    #[test]
    fn plain_text_has_no_completion() {
        assert!(compute("hello world", &members()).is_none());
        assert!(compute("", &members()).is_none());
    }

    #[test]
    fn unknown_slash_prefix_has_no_items() {
        assert!(compute("/zzz", &members()).is_none());
    }

    #[test]
    fn targeted_slash_completes_agent_skills() {
        let skills = vec![
            AgentSkill {
                name: "review-patch".to_string(),
                description: "Review a patch carefully".to_string(),
                backend: BackendKind::Codex,
                invocation: "$review-patch".to_string(),
            },
            AgentSkill {
                name: "write-tests".to_string(),
                description: "Add regression tests".to_string(),
                backend: BackendKind::Codex,
                invocation: "$write-tests".to_string(),
            },
        ];

        let completion = compute_with_agent_skills(
            "@builder /rev",
            &members(),
            &skills,
            &HashMap::from([("builder".to_string(), BackendKind::Codex)]),
        )
        .unwrap();
        assert_eq!(completion.title, "member actions & skills");
        assert_eq!(completion.token_start, 9);
        assert_eq!(
            completion
                .items
                .iter()
                .map(|item| item.insert.as_str())
                .collect::<Vec<_>>(),
            vec!["$review-patch "]
        );
    }

    #[test]
    fn targeted_completion_offers_native_attach_and_skills_only() {
        let completion = compute_with_agent_skills(
            "@builder /",
            &members(),
            &[],
            &HashMap::from([("builder".to_string(), BackendKind::Codex)]),
        )
        .expect("member controls");

        assert_eq!(completion.title, "member actions & skills");
        assert!(completion.items.iter().any(|item| {
            item.insert == "/attach " && item.label.contains("native CLI session")
        }));
        for command in [
            "/model",
            "/fast",
            "/permissions",
            "/compact",
            "/status",
            "/review",
        ] {
            assert!(
                !completion.items.iter().any(|item| item.insert == command),
                "native control {command} must not be presented as a noninteractive action"
            );
        }
    }

    #[test]
    fn agent_skill_completion_requires_an_explicit_known_target() {
        let skills = vec![AgentSkill {
            name: "review-patch".to_string(),
            description: "Review a patch carefully".to_string(),
            backend: BackendKind::Codex,
            invocation: "$review-patch".to_string(),
        }];

        assert!(compute_with_agent_skills("/rev", &members(), &skills, &HashMap::new()).is_none());
        assert!(
            compute_with_agent_skills("@unknown /rev", &members(), &skills, &HashMap::new())
                .is_none()
        );
        assert!(
            compute_with_agent_skills("@builder /rev now", &members(), &skills, &HashMap::new())
                .is_none()
        );
    }

    #[test]
    fn targeted_skill_completion_only_shows_the_target_backend() {
        let members = vec![
            "claude".to_string(),
            "codex".to_string(),
            "grok".to_string(),
            "agy".to_string(),
        ];
        let backends = HashMap::from([
            ("claude".to_string(), BackendKind::Claude),
            ("codex".to_string(), BackendKind::Codex),
            ("grok".to_string(), BackendKind::Grok),
            ("agy".to_string(), BackendKind::Agy),
        ]);
        let skills = vec![
            AgentSkill {
                name: "wake".to_string(),
                description: "Claude only".to_string(),
                backend: BackendKind::Claude,
                invocation: "/wake".to_string(),
            },
            AgentSkill {
                name: "review".to_string(),
                description: "Codex only".to_string(),
                backend: BackendKind::Codex,
                invocation: "$review".to_string(),
            },
            AgentSkill {
                name: "inspect".to_string(),
                description: "Grok only".to_string(),
                backend: BackendKind::Grok,
                invocation: "/inspect".to_string(),
            },
            AgentSkill {
                name: "plan".to_string(),
                description: "Agy only".to_string(),
                backend: BackendKind::Agy,
                invocation: "/plan".to_string(),
            },
        ];

        let claude = compute_with_agent_skills("@claude /", &members, &skills, &backends)
            .expect("Claude skills");
        assert_eq!(
            claude
                .items
                .iter()
                .map(|item| item.insert.as_str())
                .collect::<Vec<_>>(),
            vec!["/attach ", "/wake "]
        );

        let codex = compute_with_agent_skills("@codex /", &members, &skills, &backends)
            .expect("Codex skills");
        let codex_inserts = codex
            .items
            .iter()
            .map(|item| item.insert.as_str())
            .collect::<Vec<_>>();
        assert!(codex_inserts.contains(&"/attach "));
        assert!(!codex_inserts.contains(&"/model"));
        assert!(codex_inserts.contains(&"$review "));
        assert!(!codex_inserts.contains(&"/fast"));
        assert!(!codex_inserts.contains(&"/review"));
        assert!(!codex_inserts.contains(&"/wake "));
        assert!(!codex_inserts.contains(&"/inspect "));
        assert!(!codex_inserts.contains(&"/plan "));

        for (member, expected) in [("grok", "/inspect "), ("agy", "/plan ")] {
            let completion =
                compute_with_agent_skills(&format!("@{member} /"), &members, &skills, &backends)
                    .unwrap();
            assert_eq!(
                completion
                    .items
                    .iter()
                    .map(|item| item.insert.as_str())
                    .collect::<Vec<_>>(),
                vec!["/attach ", expected]
            );
        }
    }
}
