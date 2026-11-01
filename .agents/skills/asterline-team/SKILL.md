---
name: asterline-team
description: Use when acting as an Asterline team member who actually needs to message teammates, open a sub-agent for a side task, coordinate explicitly collaborative work, update run steps, or request that Asterline add a teammate to the live roster.
metadata:
  version: 23
---
<!-- managed-by: asterline (auto-upgraded; local edits will be overwritten) -->

# Asterline Team Protocol

Asterline reads special control lines from your final output. Put each control line on its own line with valid single-line JSON. Parsed control lines are removed from the visible chat.

## Roster And Messaging Policy

Read `.asterline/roster.md` for the current default target and every member's id, role, backend, and status. Asterline rewrites that file when the team or a status changes.

Do not send `@@team_message` merely because teammates are listed, another member has a relevant role, or the task involves search, research, review, or planning. Message only when the user asks for collaboration, the active run requires a handoff, or you are blocked on a specific teammate. If you can finish the request yourself, do not emit `@@team_message`.

## Every Received Message Must Be Answered

Two kinds of message need no `@@team_message` back. A message that opens with `[task from ...]` is a task from the member that opened you as a sub-agent; your final reply is handed back to it automatically (see "Open A Sub-Agent"). A message that opens with `[hand-back from ...]` is a sub-agent's result for you; read it and carry on, without acknowledging it.

For every other relay from another member, remember the sender. Before ending your turn, you MUST emit exactly one `@@team_message` back to that sender. This rule applies in normal chat and every collaboration mode.

```text
@@team_message {"to":"original-sender","kind":"reply","body":"The full deliverable, decision, question, or blocker"}
```

Visible response text, checklist updates, and tool output do not count as delivering to the sender. Writing the plan, review, or patch "for the user" is not delivery. The teammate who asked cannot see your user-facing prose; they only see this `@@team_message` body.

If the sender asked you to plan, design, review, implement, or investigate, the `body` MUST contain the actual artifact: the full plan, the field list, the verdict, the findings. A pointer such as "方案已写给用户" or "see the chat" is not a reply.

Put the deliverable in `body` even when it is long. Escape newlines as `\n` so the control line stays one line of JSON.

For delegated checklist work, update every owned run step to `done` or `block`, then send the reply with results, changed files, checks run, and blockers.

To stop acknowledgement loops, a message with `"kind":"reply"` does not require another reply unless its body contains a new question, request, correction, or blocker requiring action.

## Message Teammates

When the messaging policy above permits it, send necessary work or questions to one or more teammates:

```text
@@team_message {"to":"reviewer","body":"Please review the parser changes."}
@@team_message {"to":["builder","reviewer"],"body":"Let's agree on the data model."}
@@team_message {"to":"all","body":"Status update?"}
```

`to` accepts a member id, display name, array of ids/names, or `all`.

## Open A Sub-Agent

A top-level member can open sub-agents for side work. They belong to the current chat only: they are not saved in `team.json`, `/resume` brings them back, and `/new` removes them.

```text
@@spawn {"name":"reviewer","task":"Review the diff in src/router and list real bugs only.","backend":"claude","model":"opus","effort":"high","role":"review"}
```

Name a sub-agent after its job, short: `reviewer`, `test-writer`, `researcher`. Never use a model, model family, or CLI name (`sol`, `opus`, `gpt-6-1`, `codex`): the Agents list already shows each sub-agent's model, and the name goes stale when the model changes. Use `helper` when there is no specific job. If you do, Asterline renames it to `helper` (`helper-2`, ... when taken) and tells you the real name once.

Required fields: `name` and `task`. Optional fields: `backend` (`codex`, `claude`, `grok`, or `agy`), `model`, `effort`, `role`. Omitted `backend`, `model`, and `effort` follow you; a different backend gets that backend's default permissions, and the same backend keeps yours.

When it is worth it:
- Independent pieces of work can run in parallel.
- A second model's perspective, or an independent review, would catch more than you alone.
- Do not open one for something you can finish yourself in a few steps.

How it works:
- The sub-agent's id is the `name` lowercased with spaces turned into `-`; its display name is the `name` with the first letter of each `-`/`_`-separated part capitalised (`test-writer` shows as `Test-Writer`). The `task` reaches it as a message from you.
- When it finishes, its final reply comes back to you automatically as a `[hand-back from ...]` message. It does not need to message you itself, and you should not poll or wait for it.
- To give an existing sub-agent more work, send `@@spawn` again with the same `name` (the other fields are ignored), or an `@@team_message`. It keeps its context.
- A chat holds at most 8 sub-agents. A sub-agent cannot open sub-agents. If Asterline refuses a request, you receive one short `[system message from Asterline]` saying why; do not retry the same request.
- In `/mode team`, `@@spawn` follows the same roster lock as `@@team_member`. Review, plan, and brainstorm runs do not accept it.
- Review and brainstorm runs on a roster too small for their seats open `reviewer` and `ideator-N` sub-agents by themselves (Asterline does this, not you); do not spawn those names yourself during such a run.
- The user can open any sub-agent from the Agents list (`←` on an empty composer) and keep chatting with it. Its replies to the user are not forwarded to you.

If you are a sub-agent (your prompt says who opened you), just do the task and give your final reply as plain text.

## Add A Teammate

Only emit this when the current prompt says the roster may grow. In
`/mode team` the default is the current roster; do not emit `@@team_member`
when the prompt says the roster is locked.

When adding is allowed and the roster lacks a needed specialty, request a new teammate:

```text
@@team_member {"display_name":"QA","backend":"codex","role":"tests"}
```

Required fields: `display_name`, `backend`, `role`.
Optional fields: `id`, `model`, `effort`, `cwd`, `sandbox`, `permission_mode`, `allowed_tools`, `session_policy`, `session_id`, `system_prompt`.

Omitted `sandbox` and `permission_mode` use Asterline's write defaults for
that backend (Agy: `accept-edits` with sandbox off). Set them only to
override. Do not send `plan` unless the user asked for a plan-first seat.

Rules:
- `backend` must be `codex`, `claude`, `grok`, or `agy`.
- `effort` may be `low`, `medium`, `high`, `xhigh`, or `max`.
- Only adding is supported; do not request deletes or overwrites.
- Asterline derives a stable lowercase id from `display_name`; set `id` only when you need a custom handle.
- Avoid ids or display names already in the roster.

## Review Verdicts

When asked to review work, you MUST end your reply with exactly one control line that reports your verdict:

```text
@@review {"verdict":"approve","summary":"Parser covers the edge cases and tests pass."}
@@review {"verdict":"request_changes","summary":"Needs fixes before merge","items":["Add a regression test for empty input","Rename helper to match module style"]}
```

- `verdict` is required and must be `approve` or `request_changes`.
- `summary` is optional free-text explaining the decision.
- `items` is optional; use it for a short bullet list of concrete changes when requesting work.

## Brainstorm Generation and Voting

For `/mode brainstorm`, follow the deployed `$asterline-brainstorm` skill and
the current phase prompt. That separate skill defines the structured
`@@brainstorm_card` schema, judgment-free generation waves, private
`@@brainstorm_vote` ballot, and final synthesis. Do not substitute a free-form
card format.

## Update Run Steps

During `/mode plan` or `/continue` work, keep the run checklist current:

```text
@@run_step {"action":"add","owner":"builder","title":"Write parser tests"}
@@run_step {"action":"doing","step":1,"note":"Implementing lexer edge cases"}
@@run_step {"action":"done","step":1,"note":"Covered lexer edge cases"}
@@run_step {"action":"block","step":2,"note":"Waiting for API credentials"}
@@run_step {"action":"assign","step":2,"owner":"reviewer"}
@@run_step {"action":"unassign","step":2}
@@run_step {"action":"rename","step":2,"title":"Document API credential setup"}
@@run_step {"action":"remove","step":3}
```

Use `add` for new checklist items. Use `todo`, `doing`, `done`, or `block` with
the 1-based step number shown in `/runs` to update an existing item. Use
`assign` with a member handle to set ownership, `unassign` to clear it,
`rename` to fix a step title, and `remove` only for duplicate or obsolete steps.

## Waiting For User Approval

By default Asterline auto-approves Codex tool asks. If the user enabled
manual approvals, a live tool (especially a Codex command, file change, or
permission escalation) pauses until they agree in the card above the
composer. A Plan run with manual execute confirmation waits the same way
before the Builder is sent the checklist.

When you are waiting:

- Do not retry the same tool, command, or handoff.
- Do not assume the action succeeded or failed.
- Do not send another `@@team_message`, `@@spawn`, or `@@team_member` just to nudge.
- Stop and wait for the next prompt. After approval, the tool result or next
  dispatch arrives normally. After rejection, treat the action as denied and
  continue another way.

Everything else you write is shown to the user.
