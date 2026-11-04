# Asterline command and keyboard reference

This is the complete reference for Asterline's startup options, composer
syntax, slash commands, drawers, and keyboard controls. For installation and
the product overview, return to the [English README](../README.en.md). A
[Chinese version](commands.zh-CN.md) is also available. The default product
page is the [Chinese README](../README.md).

## Quick start

```text
inspect the repository and explain the architecture
/mode brainstorm
How could we reduce indexing latency?
/runs
/new
/resume
```

Type `/` to list Asterline commands and `@` to list team members. Use `↑` and
`↓` to select a suggestion, `Tab` or `Enter` to insert it, and `Esc` to close
the suggestion list.

## Syntax used on this page

- `<value>` is required; `[value]` is optional. Do not type the angle or square
  brackets.
- `run-<id>` means a displayed run ID such as `run-2`.
- `<message>`, `<note>`, `<reason>`, `<title>`, and `[command]` consume the
  remainder of the line, so they may contain spaces.
- Commands and mode names are lowercase.
- When `[run-<id>]` is omitted, the command selects the latest run in the
  current conversation.
- Missing required arguments, invalid arguments, and unknown slash commands
  open `/help` instead of being sent to an agent.

## Starting Asterline

```text
asterline [OPTIONS]
```

Both `--option value` and `--option=value` are accepted for `--team`,
`--workspace`, and `--db`.

### `--team <PATH>`

Load a JSON team configuration from `PATH` instead of the saved one.

```bash
asterline --team ./team.json
```

### `--workspace <PATH>`

Set the working directory inherited by team members. The default is the
directory in which Asterline is started.

```bash
asterline --workspace /path/to/project
```

### `--db <PATH>`

Set the SQLite database path. By default it is
`~/.asterline/projects/<workspace key>/asterline.sqlite3` (the home is
`$ASTERLINE_HOME` when set; see [Runtime data](configuration.md#runtime-data)).

```bash
asterline --db /path/to/asterline.sqlite3
```

### `--no-restore`

Start without replaying persisted logs or opening a new conversation. This does
not delete saved conversations; use `/resume` to select one later.

```bash
asterline --no-restore
```

### `--manual-approvals`

Show the approval card above the composer for Codex tool asks. Off by
default: those asks are approved automatically. `team.json` can set the
same switch with `"approvals": { "manual": true }`.

```bash
asterline --manual-approvals
```

### `--debug`

Developer mode. It no longer changes approval behavior.

```bash
asterline --debug
```

### `--fake`

Use deterministic offline fake agents instead of installed backend CLIs. This
is intended for development, demos, and tests. A prompt containing `slow`
makes the fake agent take about five seconds, so the running state can be
inspected. A prompt containing the word `tools` makes it run eight tool calls
of mixed kinds (one of them failing) before it answers, which is a quick way to
see a long tool group. For [`/goal`](#goal), an objective containing
`fake-goal-<N>` makes the fake agent finish the goal on its N-th goal turn, and
`fake-goal-silent` makes every goal turn reply with nothing (to see the
no-progress pause).

```bash
asterline --fake
```

### `--banner`

Print a compact startup banner before entering the TUI.

```bash
asterline --banner
```

### `update`

Run an explicit update for the installation method that owns the currently
running binary:

- A Windows Setup-managed installation keeps the verified installer flow: it
  checks the latest stable Release, verifies Setup against that Release's
  `SHA256SUMS`, schedules it after Asterline exits, then exits.
- A Homebrew installation on macOS or Linux runs `brew update` followed by
  `brew upgrade song0705/asterline-prime/asterline`. Asterline first verifies that
  its own executable is inside that Formula's installed prefix.

Portable archives, direct `.deb`/`.rpm` installs, macOS packages, and source
builds are intentionally not overwritten. They print the appropriate manual
path instead of guessing where to replace files.

```bash
ast update
```

`ast --update` remains a backward-compatible alias.

### `--no-auto-update`

Skip the once-per-24-hours background update check for this launch of a Windows
Setup-managed copy. It does not disable future checks or affect an explicit
`--update`; it has no effect on platforms and copies that never self-update.

```powershell
ast --no-auto-update
```

### `-h`, `--help`

Print command-line help and exit. This is different from the in-product
`/help` command.

```bash
asterline --help
```

## Sending messages

### `@<member> <message>`

Optional: send a message to one team member without switching views. The
member name must exist in `/team`. Plain text without a prefix already goes to
the member you are viewing (press `←` on an empty composer to switch).

```text
@builder implement the parser change and run its tests
```

In the "All" view, plain text goes to the team's default member. In a
collaboration mode, plain text starts or continues that mode using its
configured participants.

### `@all <message>`

Optional: send the same message to every enabled member.

```text
@all review the proposed API and identify one risk
```

### `@<member> /<skill> [arguments]`

Invoke a discovered skill through a specific member's native CLI. Type `/`
after the member prefix to open skill completion.

```text
@builder /asterline-team inspect the active run
```

Targeted completion inserts the native invocation discovered for that member.
Codex uses `$skill`; Claude plugin skills keep their `/plugin-name:skill`
namespace. For convenience, a manually typed
`@<codex-member> /skill` is converted only when it exactly matches a
discovered Codex skill. `@member /` offers the real `/attach` action and only
that member's discovered skills. `/attach` opens the [native
session](#native-session-attach), where the backend's own interactive
slash-command menu is available. Unknown targeted slash commands are kept out
of noninteractive runners unless they exactly match a discovered skill. Slash
controls always need one member; Asterline rejects `@all /…` rather than
broadcasting it.

## Message and conversation commands

### Older spellings

These still work when typed in full, but `/help` and completion no longer list
them:

- `/ask <member|all> <message>` and `/all <message>` are the slash-command
  forms of `@member` and `@all`.
- `/agents` opens the Agents list, the same one `←` opens.
- `/focus <member>` is the older spelling of `/logs <member>`.

### `/new`

```text
/new
/new <prompt>
```

Persist the current conversation, create a new conversation, clear the visible
transcript and current run list, create fresh backend session IDs, and reset
the terminal mode to `normal`. Mode field settings from the previous chat
(reviewer, builder, limits, and other this-chat overrides) stay in effect for
the new conversation. The new chat starts with one default `main` agent on the
first installed CLI (Codex, then Claude, Grok, Agy), using that CLI's own
default model and permission preset. Members added in `/team` and `/model`
changes belong to the chat you leave; they are not carried over, and
`/resume` brings them back with that chat. Team settings such as
`max_auto_relays`, `modes`, and `approvals` stay. When Asterline was started
with `--team <PATH>`, `/new` resets to that file's members instead. New runs in that chat start again at `run-1`. If a
member, collaboration run, or verification is active, `/new` is rejected;
press `Esc` and wait for cancellation first.

`/new <prompt>` starts the new conversation and then sends the prompt to the
main member as an ordinary first message. The prompt is kept as typed (inner
spaces and line breaks included), and it is sent only after the new
conversation exists: when `/new` is rejected, nothing is sent and the old chat
is untouched.

`/clear` takes no argument and does the same full new-conversation operation
as a bare `/new` rather than merely hiding history (`/clear text` answers
`Usage: /clear`). A normal restart opens a new conversation and leaves the earlier
one available in `/resume`, and the new chat starts with the default `main`
agent as above. `/new` also removes the chat's sub-agents (see
[Sub-agents](configuration.md#sub-agents); one at a time, `Ctrl+X` in the Agents list removes a single sub-agent). The old chat stays in the Agents
list's **Inactive** section (see [Agents list](#agents-list-)). `Ctrl+N` in
that list is the same as `/new`.

### `/resume`

```text
/resume
/resume <number>
```

`/resume` opens the Agents list with the highlight on its newest **Inactive**
row, the saved conversations. Press `Enter` (or `→`, or click) on one to
restore its transcript, team configuration, roster (including members added in
`/team` and model changes made in that chat), native backend session
IDs, active mode, and conversation-scoped runs; the restored chat itself is
the confirmation. `/resume <number>` restores the conversation with that
number directly; anything else after `/resume` answers a usage message.
Asterline refuses to switch while members or verification are active; press
`Esc` first.

### `/name`

```text
/name <name>
```

Names the current chat. The name is trimmed and must not be empty (an empty one
answers `Usage: /name <name>`). It replaces the first message as the chat's
title: on the **Main** row of the Agents list while you are in it, and on its
**Inactive** row once you leave. It is saved with the conversation, so it
survives a restart and `/resume`; `/new` starts an unnamed chat. You can also
rename a chat from the Agents list with `Ctrl+R`, which can clear a name too.
A named chat is listed under **Inactive** even before you send a message to it.

### `/goal`

```text
/goal
/goal <objective>
/goal pause
/goal resume
/goal clear
```

Gives the main member a persistent goal for this chat and keeps it working
until the goal is done. `/goal <objective>` sets the goal and immediately sends
the main member a goal prompt (to the `main` member, else the first top-level
member, the same target `/new <prompt>` uses). The chat shows your objective as
an ordinary message. The objective is trimmed, must not be empty and may be at
most 4000 characters. A goal runs in `normal` mode only; in `review`, `plan`,
`brainstorm` and `team` mode the command is refused with a notice, and
switching to one of those modes pauses a running goal.

When the main member's turn ends (including a sub-agent's hand-back that belongs
to it) and the goal is still active, Asterline sends it a continuation prompt as
its next turn, over and over. The prompt is not shown as a message; the chat
gets a dim marker row such as `↻ goal · turn 4` in the main agent's window and in
All. A message you send while a goal is active is delivered normally; the loop
carries on after that turn. The goal has **no turn, token or time cap**. It
stops only when:

- the main member ends a reply with the control line
  `@@goal_complete {"summary":"one sentence"}` (the summary is optional). Only
  the goal's member can complete its goal, and only while it is active. The
  line is removed from the displayed text and Asterline posts
  `Goal complete: <summary>`;
- you run `/goal pause` (the turn in progress finishes first) or `/goal clear`;
- you interrupt the main member with `Esc`, or its turn fails: the goal is
  paused with the reason (`Paused by interrupt`, or the error text);
- the no-progress guard trips: three continuation turns in a row that produced
  no reply text and no tool calls pause the goal with
  `Paused: 3 turns in a row made no progress`. Any turn that produces something
  resets the count.

`/goal` alone shows the objective, status, turns so far and age. `/goal resume`
continues a paused goal at once (or after the current turn). `/goal <objective>`
on a chat that already has a goal replaces its objective, keeps its turn count
and age, and sends an "objective was edited" prompt; when the main member is
busy that prompt becomes its next turn. A completed goal is replaced by a new
one.

While a goal is active or paused, one dim line sits just above the composer in
the main member's window and in All (not in a sub-agent's window):
`◎ Goal · <objective> · turn 4 · 12m`, or
`◎ Goal paused · <reason> (/goal resume)`.

The goal belongs to the chat: it is saved with it, is not carried into `/new`,
and is deleted with the chat. A goal that was active when Asterline was closed
(or when you left the chat) comes back **paused** after a restart or `/resume`
(`Paused: restored — /goal resume to continue`); a goal never starts running
by itself.

### `/retry`

```text
/retry
```

Re-send the most recent user request through the currently active mode. It
does not resume a blocked run or a paused approval route; use `/continue` or
`/approve` for those cases. If the conversation has no previous user request,
nothing is sent.

### `/attach`

```text
/attach <member>
@member /attach
```

Suspend Asterline and open that member's real interactive CLI, resuming its
existing backend session when one is available. Exit using the method supported
by that native CLI (usually its own `/exit`) to return automatically to
Asterline. A fresh Claude attach receives an Asterline-generated UUID through
`claude --session-id`, so its transcript is automatically imported and bound
to that member when you return. Codex imports only an already-bound session it
can identify safely; a Claude fork is imported only when its prior transcript
proves the lineage. Ambiguous native sessions are never guessed. Grok and Agy
can resume their sessions but do not yet import attached messages.

Claude's own interactive resume picker intentionally hides sessions created by
`claude -p` or the Agent SDK. To attach one of those Asterline sessions, open
`/team`, select the member's **session id** field, press `Enter`, choose the
session, and press `s`; then run `/attach <member>`. Asterline discovers the
transcript itself and resumes its UUID directly.

### `/import`

```text
/import <session_id>
/import <member> <session_id>
@member /import <session_id>
```

Import messages from a native Claude Code, Codex, or Grok session transcript into the
Asterline chat and bind that session to the member.

### `/export`

```text
/export
/export claude
```

Export the current Asterline conversation history into Claude Code's native session
JSONL storage (`~/.claude/projects/.../<session_id>.jsonl`), making it directly
visible and resumable via `claude --resume` (`claude -r`) in the project directory.

### `/exit`

```text
/exit
```

Exit Asterline immediately. Its normal shutdown path cancels active backend
work and restores the terminal. This is an Asterline command only; while
attached to a native backend CLI, that CLI's own `/exit` returns to Asterline.

### `/approve`

```text
/approve
```

Approve the selected pending request (oldest if you have not switched).
The usual path is the card above the composer: `y` or Enter to agree, `n`
to deny. If no request is pending, Asterline reports that there is nothing
to approve.

### `/reject`

```text
/reject
```

Reject the oldest pending Asterline approval request. If no request is
pending, Asterline reports that there is nothing to reject.

## Team and diagnostics commands

### `/team`

```text
/team
```

Open the live Team editor. Opening it refreshes installed Codex, Claude, Grok,
and Agy executables. Model catalogs are loaded asynchronously once at `ast`
startup, so the editor stays responsive and reports the actual detected model.
Focus the member's `model` field and press `t` to re-fetch it at any time; the
result is shared by matching backend/workspace members.
Missing CLIs remain visible for diagnosis but cannot be selected.

The roster you edit here belongs to the current chat: the next new chat (a
launch, `/new`, or `/clear`) starts again from one default `main` agent, and
`/resume` brings this chat's roster back.

The editor changes the roster, names, backend, model, effort, native session ID,
and approval behavior. The model field shows what the member effectively runs:
its pinned model and effort, else the ones the CLI's own configuration selects,
else the catalog default. Changes stay in a draft until `s` applies and saves
them. See [Team editor keys](#team-editor).

### `/model`

```text
/model [text]
```

Open the inline model selector above the composer. It changes the model of the
agent you are viewing (in the "All" view, the default target's agent), the way
a coding CLI's own `/model` changes the active session. The chat stays visible
above it. Text after the command pre-fills the search box, which is a single
`Search: …` line.

The selector lists the models of every installed CLI, grouped under a dim
heading each (`codex`, `claude`, `grok`, `agy`). Headings are not selectable,
so `↑` / `↓` skip them. `current` follows the effort level (two columns after
it; after the name column for a model without levels) on the target's current
model, and nothing is shown on other rows. A model is current when it is the target's pinned
`model`; for an agent with no pinned model, it is the model the CLI's own
configuration selects, else the catalog's default. A backend whose catalog is
still loading, or whose lookup failed, shows only a muted status row under its
heading and nothing to select. A search with no results shows `No matching
models`, and Enter does nothing.

Claude Code has no model-listing command, so Asterline always offers its
aliases (`opus`, `sonnet`, `haiku`, `fable`) plus every full id that has an
entry in your `modelSettings`; a non-empty `availableModels` in Claude's
settings restricts the list to those models (and the selected one). AGY lists
one model per reasoning tier (`…-low`, `…-medium`, `…-high`); the selector
merges these siblings into a single row, and `←`/`→` pick the variant.

A model that offers several effort levels shows one square per level, filled up
to the level in force, between `←` `→` arrows (on the highlighted row) and the
level name. Each model starts on the target's own effort (for its current
model), else the effort the CLI's own configuration sets (Codex
`model_reasoning_effort`, Claude `effortLevel` / `modelSettings`), else the
model's default level, else `medium`, else its lowest level. A model with fewer
than two levels shows nothing and `←`/`→` do nothing for it. When the list
scrolls, a `(3/12)` row shows the position among the models (headings are not
counted).

| Key                | Action                                         |
| ------------------ | ---------------------------------------------- |
| Type / `Backspace` | Search backend, model ID, and model name       |
| `↑` / `↓`          | Select a model (the mouse wheel does the same) |
| `←` / `→`          | Change the highlighted model's effort level    |
| `Enter`            | Apply to the target agent and close            |
| `Esc`              | Close without changing anything                |

Applying always pins the chosen model id in the agent's `model`. An `effort` is
saved only if you changed it with `←`/`→` on that row; the agent's current
model keeps the effort it already had, and any other model leaves effort to the
CLI. For AGY's merged rows the variant id carries the level, so no `effort` is
written. Choosing a model from another CLI also switches the agent to that
CLI's default permission preset and starts a fresh native session. The change is
saved to the project's `team.json`, and a `Model: …` row records it in the chat. It
is refused (with a notice) while the agent is working; press `Esc` to stop it
first.

**How the conversation carries over.** A model change on the same CLI keeps the
native session, so the context stays as it is and nothing is added. A different
CLI starts a native session that has seen nothing, so Asterline treats its
stored chat as the source of truth: the next message to that agent gets a short
recap in front of it, made of the user messages, replies and forwarded messages
in that agent's own conversation (tool calls and file changes are one line each,
without tool output). The recap goes to the model only; it is not shown in the
chat and not saved as a chat message. It is capped at about 40,000 characters
and keeps the newest part; when something had to be dropped, the `Model: …` row
gets ` · earlier messages not carried over`. Each native session of each agent
remembers how far into the chat it has seen, saved with the session id (it
survives a restart and `/resume`; `/new` clears it). Switching back to a CLI
whose old session is still remembered resumes that session and recaps only what
it missed.

### Agents list (`←`)

Press `←` on an empty composer to open the Agents list. It takes over the whole
screen (no transcript, composer, or chat footer) until you pick an agent or
close it, and `/agents` opens the same list. The main chat shows one member's
conversation at a time: that member's replies, reasoning, tools, and diffs, the
prompts you sent to it, and the relays it sends or receives. Prompts you sent
to other members, relays between other members, and notices about another
member (a `Model: …` switch, a refused spawn) are not shown in that window, and
`/find` skips them too. General notices (mode changes, `/new`, team updates)
show in every view. Choose **All messages** to see every member's activity in
order instead.

From top to bottom the screen shows:

- A dim counts line, `Agents · 2 running, 1 idle, 3 inactive`, counting
  top-level agents and saved chats.
- A search prompt (`> Search agents`). Typing, `Backspace`, and paste edit the
  search instead of the chat composer. It matches the name (for the main row,
  its title and `main`), backend, and model (substring first, then letters in order, so `rvw` finds `Reviewer`; every
  word must match). While a search is typed the list is flat and ranked by
  relevance, without sections; `No agents match your search.` appears when
  nothing matches.
- A bold legend (`Agent  Model  Activity`), then **All messages** first, then
  the **Running (n)**, **Idle (n)**, and **Inactive (n)** sections. A top-level agent is in
  *Running* while it or any of its sub-agents is working, queued, waiting, or
  needs approval; otherwise in *Idle*. A sub-agent stays under its parent,
  indented four columns inside the name column so the other columns stay
  aligned. *Inactive* lists the saved conversations (not live members), newest
  first, and appears only when there are any. Its rows have a dim `•`, the
  chat's first message on one line as the title, the main member's backend and
  model, and the time since the chat last changed (`45s`, `12m`, `3h`, `2d`).
  If you named the chat (`/name`, or `Ctrl+R` below), the name is the title
  instead of the first message. They show no preview, and a search matches
  their title and model too. `Enter`
  or `→` on one restores that conversation and closes the list; `Ctrl+N`
  starts a new conversation. Asterline reloads the saved chats every time the
  list opens.
- Each row has an icon (a pulsing `◇◈◆◈` while the agent is active, a yellow
  `!` while it waits for your approval, `•` otherwise, in the error colour for
  failed),
  the name, the backend with the model and effort it effectively runs, and its
  activity. The main member's row is titled by the chat, not by its name: the
  name you gave it (`/name`), else the first prompt sent to it, with whitespace
  collapsed to single spaces, else the workspace folder's name before any
  prompt. Sub-agents and members added in
  `/team` keep their configured names. The name column is at most 28 columns
  wide and longer text ends with `…`, so a long first prompt does not squeeze
  the other columns. Activity is:
  `working 12s` (or `queued`, `waiting`, `needs approval`) while active,
  `failed` after a failure, otherwise the time since its last reply this
  session (`3m`; blank until it has replied).
- The selected row is a full-width highlighted band. Only the selected row
  shows a preview of that agent's last message, wrapped under it in dim text
  (at most three lines, ending with `…` when cut). When the list is taller than
  the screen, the selection stays near the middle and dim `...` rows mark what
  is clipped above or below.
- A hint line on the last row.

Select a sub-agent to see its whole conversation and keep chatting with it
directly; its replies to you are not forwarded to its parent. The list opens
with the agent you are viewing selected.

- `↑` / `↓` move the selection; `Home` / `End` jump to the first or last row;
  `PageUp` / `PageDown` move by a page; the mouse wheel moves one row.
- `Ctrl+N` starts a new conversation (like `/new`) and closes the list.
- `Ctrl+R` (empty search; on an **Inactive** row or the **Main** row) renames
  that chat. The search line becomes `> Rename: <name>`, prefilled with the
  current name; `Enter` saves it (an empty name clears it) and `Esc` cancels.
  The hint line reads `Enter save   Esc cancel` meanwhile.
- `Ctrl+X` (empty search, **Inactive** row only) deletes a saved chat with all
  its messages, runs, and approvals. The first press changes the hint line to
  `Press Ctrl+X again to delete`; a second `Ctrl+X` on the same row deletes it.
  Any other key, or moving the selection, cancels. The current chat and live
  agents cannot be deleted. The hint line lists `Ctrl+R rename` and `Ctrl+X
  delete` only on rows where they apply.
- `Ctrl+X` (empty search) on a **sub-agent** row removes that one sub-agent from
  the chat; the same two-press confirmation applies, and the hint reads `Press
  Ctrl+X again to remove`, or `Press Ctrl+X again to stop and remove` when it is
  working, queued, waiting, or needs approval (its work is interrupted first and
  its queued messages and pending approvals are dropped). The hint line lists
  `Ctrl+X remove` on sub-agent rows. The parent is told in a system message, and
  the sub-agent's past messages stay in the chat. The key does nothing on top-level
  members and the **All** row, and a sub-agent that fills a seat of the running
  review, plan, or brainstorm run is refused until you cancel the run with `Esc`.
  A removed review or brainstorm seat (`reviewer`, `ideator-N`) is opened again by
  the next run. If you were viewing the removed sub-agent, the view returns to its
  parent.
- `Enter` or `→` switches the main chat to the selected row and closes the list;
  a mouse click on a row does the same.
- `Esc` clears a typed search first and closes the list when the search is
  empty. `←` closes the list only while the search is empty.

The first member (or the team's default target) is shown when Asterline
starts. Plain text goes to the agent you are viewing. In the "All" view, plain
text goes to the team's default member.

### `/logs`

```text
/logs [member]
```

Open persisted runtime logs, including backend stderr and adapter/runtime
warnings, oldest first like the transcript, opened on the newest entry. Only
warnings and errors carry a level label. Give a member to filter the drawer to its stdout, stderr, tool
activity, and adapter warnings.

```text
/logs builder
```

### `/diff`

```text
/diff
```

Open a live working-tree view containing `git diff` output plus untracked file
information. It does not stage, revert, or otherwise modify files. When the
tree is clean, or the workspace is not a git repository, a one-line notice
appears in the transcript instead of an empty panel.

### `/find`

```text
/find [text]
```

Search the current transcript case-insensitively. The footer shows the current
match and total, such as `find: "timeout" (2/5)`. With an empty composer and no
drawer open, press `n` for the next match or `p` for the previous match.
`/find` with no text, or `Esc`, clears the search. A match inside a folded
tool group (see [Tool calls](#tool-calls)) opens that group.

```text
/find timeout
```

### `/help`

```text
/help
```

Open the command palette (titled with the Asterline version), the commands worth knowing in three groups: Common, Approvals & sessions, and Runs. The `/` suggestion list uses the same order, with the best match for what you have typed first.
Unknown slash commands and commands with invalid or missing required arguments
also open this palette.

## Collaboration modes

### `/mode`

```text
/mode
/mode <normal|review|plan|brainstorm|team>
```

`/mode` with no argument opens a panel just above the composer, like the
`/model` selector, as tall as its content. The first layer lists every mode with its resolved
binding line and lands on the current conversation mode. Enter is the only
key that opens the highlighted mode's fields. Esc or q closes without
applying unsaved edits.

Each field shows the value currently in effect and where it came from:
`default`, `team.json`, or `this chat`. Edits stay pending until you apply
them. `s` selects that mode and applies its pending overrides to this
conversation. `w` writes the current mode's conversation overrides into
`team.json` as that team's defaults. `r`
clears the selected field's this-chat override so it falls back. Closing
without `s` or `w` discards pending edits.

`/mode <name>` stays the keyboard fast path: it switches immediately and
does not open the panel. A `Mode: …` line in the transcript describes the new
mode, or says plainly why this roster cannot run it yet (only an explicit
binding that cannot work, such as a reviewer bound to the builder); outside `normal` the footer hint line starts with
`review mode ·`. After selecting `review`, `plan`, `brainstorm`, or
`team`, enter the task as the next plain message — do **not** add an
`@member` prefix. That message starts the selected mode with its configured
participants. `@member <message>` deliberately remains a one-to-one
instruction and bypasses the collaboration run. `/new` starts the new
conversation in `normal` but keeps the previous chat's mode field settings
(reviewer, builder, limits, …) so you do not have to reconfigure them in the
same project. `/resume` restores the selected conversation's mode and
overrides.

#### `/mode normal`

Use ordinary direct-message dispatch. Plain text goes to the agent you are
viewing; `@member` and `@all` still send elsewhere.

#### `/mode review`

Start a builder/reviewer loop. The builder works, the reviewer emits a
structured `@@review` verdict, and revision continues until approval or
`max_iterations`. If the limit is reached, the run becomes blocked. The
reviewer is asked to inspect the working tree rather than trusting the
builder's report. An optional `reviewer_hint` is appended to that prompt.
When no other top-level member can review, the run first opens a `reviewer`
[sub-agent](configuration.md#sub-agents) under `main` with `main`'s model and
reuses it next time. A reviewer reply
without a structured verdict is nudged once, then treated as
`request_changes` (a notice says so) and costs an iteration.

```text
/mode review
Refactor the parser without changing its public behavior
```

#### `/mode plan`

Start a leader-driven planning run. The leader creates a checklist (steps do
not need owners), then the complete checklist is sent to the Builder, which
defaults to the leader itself unless `modes.plan.builder` names someone else. The Reviewer is optional. When configured, it audits only
the plan — completeness, ordering, risks, acceptance criteria, testability —
never code or diffs; requested changes return to the Leader for a revised
checklist. `modes.plan.auto_execute` defaults to `true`; set it to `false` to
pause at an explicit `/approve` before the Builder is sent the finalized plan.
Builder completion then runs verification when configured.

```text
/mode plan
Migrate the cache format and validate backward compatibility
```

#### `/mode brainstorm`

Start a structured multi-participant brainstorm. A complete run performs
judgment-free `seed`, `build`, and `stretch` generation waves, then collects
private ranked ballots, calculates the ranking, and synthesizes the selected
ideas while preserving dissent and evidence. Idea cards and ballots follow the
bundled Asterline brainstorm skill so they can be extracted reliably. With
only `main`, the run opens `ideator-1`, `ideator-2`, … sub-agents (with
`main`'s model) up to `modes.brainstorm.participant_count`, default 3.

```text
/mode brainstorm
How could we make graph retrieval robust without node text?
```

#### `/mode team`

Start a coordinator-driven team run. The coordinator creates and owns the
checklist, dispatches work to teammates, and integrates results. Repair
loops continue until the configured iteration limit is reached. Team mode
defaults to the current roster; turn on `allow_add_members` in the Mode
panel if the coordinator may add teammates (they join immediately, with
no `/approve` step).

```text
/mode team
Implement the feature, review it, update docs, and run the test suite
```

Mode roles, participants, and iteration limits are defined in `team.json`.
Reviewers communicate a one-line verdict such as:

```text
@@review {"verdict":"approve","summary":"LGTM"}
```

## Run commands

Runs belong to the current conversation. `/new` starts with no runs, and
the next run in that chat is `run-1`. `/resume` restores only the selected
conversation's runs.

### `/runs`

```text
/runs
```

Open the Runs drawer to inspect run ID, mode, phase, status, contributions,
verification, checklist, timeline, and suggested next action. See [Runs drawer
keys](#runs-drawer). Before the first run exists, a one-line notice replaces the
drawer.

### `/continue`

```text
/continue [run-<id>] [note]
```

Resume a blocked or failed mode/team run, optionally giving the coordinator or
mode engine a note. Without a run ID, the latest run in this conversation is
selected. An already active run cannot be continued, and legacy runs without
persisted mode state cannot be reconstructed.

Resume re-enters the phase the run was blocked in: review runs re-dispatch the
builder (or reviewer, if a review was pending), plan runs return to the leader
or re-dispatch unfinished owned steps, brainstorm runs re-run the current
generation wave (or voting/synthesis), and verification restarts when the run
was mid-verify. All roster members recorded in the run must still exist.

```text
/continue run-4 retry with the newly installed dependency
/continue use the simpler fallback
```

### `/note`

```text
/note [run-<id>] <text>
```

Append a checkpoint to the run timeline without waking or dispatching an
agent.

```text
/note run-4 API contract confirmed with the reviewer
```

### `/block`

```text
/block [run-<id>] <reason>
```

Mark the selected run blocked and record the reason. A run currently being
verified must be aborted before it can be blocked manually.

```text
/block run-4 waiting for the schema decision
```

### `/step`

Manage the checklist of the latest or explicitly selected run. Step numbers
start at `1`.

#### Add a step

```text
/step add [run-<id>] [@owner] <title>
```

Add a checklist item, optionally assigning it immediately.

```text
/step add run-4 @builder implement the migration
```

#### Change step status

```text
/step todo [run-<id>] <n> [note]
/step doing [run-<id>] <n> [note]
/step done [run-<id>] <n> [note]
/step block [run-<id>] <n> [note]
```

Set a step to todo, in progress, complete, or blocked. `/step blocked` is an
alias of `/step block`.

```text
/step doing run-4 2 started after API approval
/step done run-4 2 tests pass
```

#### Rename a step

```text
/step rename [run-<id>] <n> <title>
```

Replace a step's title. `/step edit` is an alias.

#### Remove a step

```text
/step remove [run-<id>] <n>
```

Delete a checklist item. `/step delete` and `/step drop` are aliases.

#### Assign or unassign a step

```text
/step assign [run-<id>] <n> <member>
/step unassign [run-<id>] <n>
```

Set or clear the owner. `member` may be written with or without `@`.
`/step owner` is an alias of `assign`; `/step clear-owner` and
`/step clear_owner` are aliases of `unassign`.

## Global keyboard controls

While a member is working, `Enter` queues the next message instead of
starting a second run. `Esc` interrupts the live member and then sends the
queued message. If that queued text has not started yet and you want to
change it, `Shift+←` pulls it back into the composer. The current
conversation can be scrolled all the way to its first message.

| Key                        | Action                                                        |
| -------------------------- | ------------------------------------------------------------- |
| `Enter`                    | Send or accept the active selection                           |
| `Shift+Enter`              | Insert a newline                                              |
| `Alt+Enter`                | Newline fallback for terminals without distinct Shift+Enter   |
| `↑` / `↓`                  | Move in composer, history, or active selection                |
| `Tab`                      | Accept completion                                             |
| `Ctrl+R`                   | Reverse-search prompt history (in the Agents list: rename)    |
| `Ctrl+X` (Agents list)     | Press twice to delete a saved chat or remove a sub-agent      |
| `n` / `p`                  | Next/previous `/find` match when composer is empty            |
| `PageUp` / `PageDown`      | Scroll chat or the open drawer by a page                      |
| `←` (empty composer)       | Open the Agents list (members, sub-agents, saved chats)       |
| `Shift+←`                  | Pull the last queued, not-yet-started message back to edit    |
| Mouse drag                 | Select and copy chat, composer, status-bar, or drawer text    |
| Click a tool group title   | Fold or unfold that tool group                                |
| Mouse wheel                | Scroll chat or the open drawer                                |
| `Esc`                      | Close overlay/find, or stop work and send the queued message  |
| `Ctrl+T`                   | Expand or collapse thinking                                   |
| `Ctrl+G`                   | Expand or collapse file-change diffs                          |
| `Ctrl+O`                   | Show full tool output and open every tool group (toggle)      |
| `Ctrl+L`                   | Open logs                                                     |
| `Ctrl+P`                   | Open command palette                                          |
| `Ctrl+N`                   | New chat (inside the Agents list)                             |
| `Ctrl+A` / `Ctrl+E`        | Move to line start/end                                        |
| `Ctrl+U` / `Cmd+Backspace` | Clear the current composer line (other lines stay)            |
| `Ctrl+W`                   | Delete previous word                                          |
| `Ctrl+C`                   | Copy chat/composer selection; else cancel, clear, or arm quit |
| `Ctrl+V` / `Cmd+V`         | Attach a clipboard image to the next send (not Ctrl+C)        |

Screenshots and copied images are copied under the process temp directory
(`std::env::temp_dir()/asterline-pasted/<pid>/`: `%TEMP%` on Windows, `$TMPDIR`
on macOS, `/tmp` on Linux). Asterline deletes them itself — unused attachments
when you backspace or clear the composer, this process's directory on exit,
and leftover directories from dead processes on the next start. They are sent
natively: Codex `localImage`, Grok ACP image blocks, Claude/Agy as a readable
attached-image path. Up to four images per message. Pasting a PNG/JPEG/GIF/WebP
file path copies that file into the temp dir instead of inserting the path as
text.

Prompt history behaves like a shell: `↑` and `↓` preserve the current draft
while browsing older submissions. During `Ctrl+R`, type to refine the match,
press `Ctrl+R` again for an older match, `Enter` to accept, or `Esc` to cancel.

### Tool calls

Consecutive tool calls by one agent are drawn as one group under a `⚒ tools`
title, one row per call. A group of up to 3 calls is always listed in full. A
longer group is folded into a single line by default:

```text
  ▸ ⚒ tools · 12 calls · 3 read, 5 edit, 4 shell · 2 failed
```

The line counts calls by kind (the first four kinds in order of appearance,
then `…`) and, when any call failed, how many (shown in the error colour). While
the group's newest call is still running, that one row stays under the line so
progress remains visible.

Click the title line to unfold that group (`▾ ⚒ tools`) and click it again to
fold it. A click toggles only the group it lands on; dragging across the line
still selects text. `Ctrl+O` also shows the complete output of each call and
opens every folded group while it is on; turning it off returns each group to
its own folded or unfolded state. `/new` and `/resume` start with all groups
folded. `⇄ agents` groups are not folded.

## Team editor

The Team editor has member-selection and field-selection levels.

| Key       | Member selection    | Field selection                         |
| --------- | ------------------- | --------------------------------------- |
| `↑` / `↓` | Select a member     | Select a field or model                 |
| `←` / `→` | —                   | Select effort in the model list         |
| `Enter`   | Open member fields  | Edit or open Agent/model/session picker |
| `Esc`     | Close Team          | Return to member selection              |
| `a` / `d` | Add/delete a member | —                                       |
| `t`       | —                   | Retry failed Model catalog              |
| `y`       | Copy the session ID | Copy the field value                    |
| `s`       | Apply and save      | Apply and save                          |
| `e`       | —                   | Manually enter model or session ID      |

The keys for the current level are listed once, in the last row of the editor.
Text fields open in a focused input box. Press `Enter` to commit or `Esc` to
cancel. Model pickers use `↑`/`↓` for model, `←`/`→` for that model's effort,
and `Enter` to apply both. When discovery returns models, its actual default
is selected directly; the generic `default` entry appears only for an empty
catalog. Browsing never alters an existing effort override. If a different
model does not advertise that override, `Enter` leaves the current setting in
place until you explicitly choose an advertised effort with `←`/`→`.

On `session id`, `Enter` opens Asterline's native-session table. It reads local
Codex, Claude, or Grok history, shows title/project/update time/native ID, and
filters sessions to the member's effective working directory. Type to filter,
move with `↑`/`↓` or `PageUp`/`PageDown`, and press `Enter` to stage the ID;
press `s` to save it. Use `e` for manual entry. Agy currently requires manual
entry because Asterline has no verified local Agy history format.

## Runs drawer

| Key                   | Action                                                    |
| --------------------- | --------------------------------------------------------- |
| `←` / `→`             | Select older/newer run                                    |
| `↑` / `↓`             | Select checklist step; select run when no step exists     |
| `x`                   | Toggle compact/detail view                                |
| `Enter`               | Stage selected step status or run next action in composer |
| `Tab`                 | Stage editable dispatch to selected step owner            |
| `PageUp` / `PageDown` | Scroll details                                            |
| `Esc`                 | Close drawer                                              |

`Enter` and `Tab` stage text in the composer; they do not execute it
immediately. Changing runs clears the selected step. To cancel active work,
close the drawer with `Esc`, then press `Esc` again.

## Other drawers

The mode panel sits above the composer, and the Agents list replaces the whole
screen; the other drawers
(`/help`, `/team`, `/logs`, `/runs`, `/diff`) are full-width overlays
just tall enough for their content (title, content, hint), with one column of
padding inside the border. All drawers support `PageUp`/`PageDown` or the mouse wheel for
scrolling and `Esc` to close. Text in chat, status bars, and drawers can be selected with a
mouse drag and copied using the terminal's normal copy shortcut.

## Native session attach

Use `/attach <member>` or `@member /attach`. Asterline
suspends its TUI and opens that member's native interactive CLI. Use that CLI's
supported exit method—normally `/exit`; EOF works only when the backend accepts
it—to return to Asterline.

Fresh Claude attach sessions are created with an Asterline-generated UUID, then
automatically imported and bound on return. Codex messages are imported only
for an already-bound session that Asterline can identify safely; a Claude fork
is imported only when its prior transcript proves the lineage. Ambiguous native
sessions are never guessed. Grok and Agy resume their native sessions but
attached messages are not currently imported.
