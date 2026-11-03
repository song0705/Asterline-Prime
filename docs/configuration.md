# Asterline configuration and operations

[简体中文](configuration.zh-CN.md)

This page covers team files, runtime data, permissions, CLI options, agent
coordination, and troubleshooting. For the product overview, return to the
[English README](../README.en.md). For interactive controls, see the
[command reference](commands.md).

## How Asterline resolves a team

At startup, Asterline chooses a roster in this order:

1. `--team <PATH>` loads that JSON file.
2. The project's saved `team.json` (see [Runtime data](#runtime-data)) is reused
   if it exists.
3. Otherwise supported backend executables are detected on `PATH` and
   Asterline starts straight into a chat with one general-purpose `main`
   agent on the first one found (Codex, then Claude, Grok, Agy), using that
   CLI's own default model and its default permission preset. The roster is
   saved to `team.json`.
4. If no saved team and no supported executable exists, startup stops with a
   setup message.

There is no setup screen. Use `/model` to change the viewed agent's model; the
choice is saved. A hidden `--pick-team` flag ignores the saved roster and
starts over from step 3.

Steps 1 and 2 decide only the settings a project carries (`name`, `workspace`,
`max_auto_relays`, `modes`, `approvals`). The members of a **new chat** do not
come from the previous chat. Every new chat (the one a launch opens, `/new`,
`/clear`, and `Ctrl+N`) starts with one default `main` agent on the first
installed CLI (Codex, then Claude, Grok, Agy) with that CLI's own default model
and permission preset, as in step 3. Members you added in `/team` and model or
effort changes you made with `/model` belong to the chat they were made in;
`/resume` brings them back with that chat, and the new chat does not inherit
them. The exception is an explicit `--team <PATH>` file: its members are used
for the first chat and again for every `/new` in that session. If no supported
CLI is installed, a new chat keeps the current top-level members.

Use `/team` to modify the live roster. Asterline loads installed Agent CLIs
and their model catalogs once when it starts; opening `/team` reuses that
cache. The same lookup reads non-Codex CLIs' local permission defaults for
display, without copying them into `team.json`. Codex instead receives
Asterline's explicit default `approvalPolicy: "never"` at thread start/resume.
Focus a member's **model** field and press `t` to re-fetch its catalog at any
time. Press `s` to apply the changes, replace member runners, and save the
updated team.

### Platform paths and backend history

On Unix, Asterline uses `HOME` for user-level configuration; on Windows it
prefers `USERPROFILE`. Each platform accepts the other variable as a fallback.
`CODEX_HOME` overrides the default `.codex` directory for both session picking,
post-attach transcript import, and global Codex skill/plugin discovery.

Default history roots are `<Codex home>/sessions`, `<user home>/.claude/projects`,
and `<user home>/.grok/sessions`. Windows project matching treats drive-letter
case and `/` versus `\` separators as equivalent. Backend discovery uses the
platform `PATH`; on Windows it also honors `PATHEXT` and launches the resolved
`.exe`, `.cmd`, or `.bat` path. When leaving an attached CLI, use `Ctrl+D` on
Unix or `Ctrl+Z` followed by `Enter` on Windows (or type `/exit`).

### Installation-aware updates

Only copies installed by the Windows Setup executable update automatically.
Portable ZIP copies and source builds never rewrite themselves. An installed
copy checks GitHub's latest stable Release at most once every 24 hours. When a
new version exists, Asterline downloads its Setup executable, verifies it
against the same Release's `SHA256SUMS`, and starts Setup in silent mode after
the current Asterline process exits.

Run `ast update` to force a check now. On macOS and Linux, it updates only an
installation that it proves belongs to the official Homebrew Formula, using
`brew update` followed by a targeted Formula upgrade. It never overwrites a
portable archive, source build, direct macOS package, or direct `.deb`/`.rpm`
install; those use their own explicit replacement path. `ast --update` remains
an alias.

Use `ast --no-auto-update` to skip the Windows automatic check for one launch.
Network failures are ignored during background checks and never prevent
Asterline from starting; a forced Windows check reports the error to the
terminal.

## Team file

```json
{
  "name": "product-team",
  "workspace": "/path/to/project",
  "default_target": { "member": "builder" },
  "max_auto_relays": 6,
  "members": [
    {
      "display_name": "Builder",
      "backend": "codex",
      "sandbox": "workspace-write",
      "effort": "high"
    },
    {
      "display_name": "Reviewer",
      "backend": "claude",
      "permission_mode": "plan",
      "effort": "medium"
    },
    {
      "display_name": "Grok",
      "backend": "grok",
      "sandbox": "workspace-write",
      "permission_mode": "auto"
    }
  ],
  "modes": {
    "review": {
      "builder": "builder",
      "reviewer": "reviewer",
      "max_iterations": 3
    },
    "plan": {
      "leader": "builder",
      "builder": "builder",
      "reviewer": "reviewer",
      "max_iterations": 3,
      "auto_execute": true
    },
    "brainstorm": {
      "participants": ["builder", "reviewer", "grok"],
      "generation_rounds": 3,
      "ideas_per_round": 4
    },
    "team": {
      "coordinator": "builder",
      "allow_add_members": false
    }
  },
  "approvals": {
    "gate": ["git", "shell", "file"],
    "apply_to": ["user", "relay", "mode"]
  }
}
```

`id` is optional. Asterline derives a stable handle from `display_name`, so
`QA Lead` becomes `qa-lead`. Set `id` only when a custom `@handle` is required.
Explicit IDs may contain only ASCII letters, digits, `-`, and `_`. IDs and
display names must be unique for routing, and `all` is reserved for broadcast
targets (case-insensitive).

### Team fields

| Field             | Required | Meaning                                         |
| ----------------- | -------- | ----------------------------------------------- |
| `name`            | Yes      | Team name shown in Asterline                    |
| `workspace`       | Yes      | Default working directory                       |
| `members`         | Yes      | Non-empty member list                           |
| `default_target`  | No       | `{"member":"id"}`, `"all"`, or the first member |
| `max_auto_relays` | No       | Handoffs in a row with no tool use; default `6` |
| `modes`           | No       | Role bindings and budgets for collab modes      |
| `approvals`       | No       | Approval-gate categories and surfaces           |

### Collaboration modes (`modes`)

Optional bindings for `/mode review`, `/mode plan`, `/mode brainstorm`, and `/mode team`. Most
omitted seat fields are derived from member names and `default_target`
(the Review-mode builder ≈ default target or first non-reviewer; reviewer ≈
a member whose name or id contains "review"; leader ≈ name or id contains "plan" or "lead", else first
participant; participants = full roster). The Plan-mode `builder` defaults to the
leader, who plans and then carries the checklist out. The Plan `reviewer` is optional:
when omitted, a complete checklist proceeds directly to the Builder. `auto_execute` defaults
to `true`; set it to `false` to require `/approve` before the Builder receives the finalized
checklist. Defaults for budgets:
`max_iterations = 3`, `generation_rounds = 3`, `ideas_per_round = 4`,
`auto_execute = true`.
Brainstorm requires at least two distinct resolved participants; repeating an
ID or referring to the same member once by ID and once by display name is
rejected.

A roster of just `main` still runs every mode. When a seat has no suitable
distinct top-level member and nothing is bound for it, Asterline opens a
[sub-agent](#sub-agents) under `main` when the run starts, named after the seat,
with the same CLI, model, and effort as `main`, and reuses it in later runs of
the same chat (so a model you switched it to with `/model` stays):

- Review: with no distinct member for the reviewer, a `reviewer` sub-agent.
- Plan: the leader (default `main`) is also the builder, so no sub-agent is needed.
- Brainstorm: without `participants`, every top-level member takes part. A lone
  member is topped up to `participant_count` (default 3) with `ideator-1`,
  `ideator-2`, …; a larger roster is used as it is unless `participant_count`
  asks for more. An explicit `participants` list is used exactly and ignores
  `participant_count`.
- Team: `main` coordinates and may `@@spawn` as needed; nothing is opened for it.

A name already taken by another member gets the next free suffix
(`reviewer-2`). Sub-agents count against the chat's limit of 8; if a run needs
more than are left, nothing starts and a notice says so. An explicit binding
that points a seat at a member always wins (a `reviewer` equal to the builder is
still refused).

Brainstorm separates divergence from convergence. The first wave collects independent
seeds; later waves expose each participant to a rotating anonymous peer sample
for building, combining, mutating, and stretching ideas. Earlier contributions
remain in the persisted idea set. After the final generation wave, every
participant privately ranks labeled ideas. Asterline aggregates ballots with a
deterministic Borda tally and dispatches one neutral ranked synthesis.
Each generated idea is extracted from an `@@brainstorm_card` envelope and
assigned a canonical candidate ID, so ballots do not depend on model-specific
Markdown numbering.

| Field               | Mode             | Meaning                                                     |
| ------------------- | ---------------- | ----------------------------------------------------------- |
| `builder`           | review/plan      | Review: implements. Plan: executor (default: the leader).   |
| `reviewer`          | review/plan      | Emits verdicts; optional Plan-only checklist audit.         |
| `leader`            | plan             | Member who writes and revises the checklist                 |
| `participants`      | brainstorm       | Roster for all generation waves (default: all members)      |
| `participant_count` | brainstorm       | Size when `participants` is unset (2–9; lone member: 3)     |
| `generation_rounds` | brainstorm       | Seed/build/stretch wave budget (default 3, minimum 2)       |
| `ideas_per_round`   | brainstorm       | Exact cards per member/wave (default 4); extras discarded.  |
| `coordinator`       | team             | Member who coordinates the whole-team run                   |
| `allow_add_members` | team             | Free add via `@@team_member` (default locked to the roster) |
| `max_iterations`    | review/plan/team | Loop budget before blocking (default 3)                     |
| `auto_execute`      | plan             | Auto-dispatch final plan (default); false needs `/approve`  |
| `reviewer_hint`     | review           | Optional extra text appended to the reviewer prompt         |

Each mode has its own configuration shape; unrelated fields are rejected by
Serde instead of being silently accepted and ignored.

The `/mode` panel can change these knobs without editing the file by hand:
`s` selects the current mode and applies its pending overrides to this
conversation, while `w` writes the current mode's conversation overrides into
`team.json`.

### Approvals (`approvals`)

Older `team.json` files may still contain an `approvals` section (`gate`,
`keywords`, `apply_to`). Those fields are kept for compatibility and no longer
hold user messages, relays, or mode dispatches. Set `"approvals": { "manual":
true }` (or pass `--manual-approvals`) to show the composer card for Codex
tool asks; the default is to approve them automatically. Plan
`auto_execute: false` is a separate confirmation. `@@team_member` is not an
approval. Set `ASTERLINE_NO_BELL=1` to disable terminal BEL/OSC 9 notifications
on approval, paused route, blocked run, and member error events.

See [approvals and tool-level control](approvals.md) for how tool callbacks
relate to backend-native sandbox and permission enforcement.

### Member fields

| Field             | Required                    | Meaning                                                 |
| ----------------- | --------------------------- | ------------------------------------------------------- |
| `display_name`    | Yes unless `id` supplies it | Visible member name                                     |
| `backend`         | Yes                         | `codex`, `claude`, `grok`, or `agy`                     |
| `role`            | No (legacy)                 | Optional job text; not shown in the UI                  |
| `id`              | No                          | Stable handle used by `@member` and routing             |
| `cwd`             | No                          | Advanced per-member working-directory override          |
| `model`           | No                          | Omitted delegates to the backend CLI                    |
| `effort`          | No                          | Chosen inside the selected model picker when supported. |
| `system_prompt`   | No                          | Additional member instructions                          |
| `sandbox`         | No                          | Codex; mapped to Grok/Agy and ignored by Claude         |
| `permission_mode` | No                          | Backend control native in `/team`                       |
| `allowed_tools`   | No                          | Backend-specific tool allowlist                         |
| `session_policy`  | No                          | `resume` (default) or `fresh`                           |
| `session_id`      | No                          | Native CLI session/conversation ID to resume            |

The `display_name` is a member's only label. `role` is a legacy optional field:
older files that carry it keep working (Review and Plan seat defaults still read
it), it is written back only when non-empty, and `/team` neither shows nor edits
it. Prefer a descriptive name such as `Reviewer` or `Planner`.

On startup, a `resume` member with a bound session is scanned for native
transcript rows written while Asterline was closed (Grok CLI, Codex, or
Claude). Only unseen messages are imported into the current chat.

In `/team`, a `resume` member displays its bound native session ID directly.
Without one, it says `select a session` to make the required picker/manual-ID
choice explicit; a `fresh` member says `not set (fresh)`. The UI never calls an
unbound session ID `default`.

`cwd` is deliberately not editable in `/team`: members created there use the
team workspace. Keep the optional `team.json` field only for an advanced
multi-repository or monorepo setup where a member must run in a different
working directory. It also determines the backend session project and the
model-catalog cache key for that member.

Both policies pin and reuse the backend session ID after the first call.
`resume` keeps an existing persisted ID when available. Switching a member to
`fresh` discards its old ID once, so the next call creates a new native CLI
conversation; that newly discovered ID is then reused for subsequent calls.
`fresh` does not create a separate conversation for every turn.

Set `session_id` to bind a team member to a specific conversation from its
native CLI history. For Codex it is the App Server `thread.id`, resumed through
`thread/resume`. Claude, Grok, and Agy use `claude --resume`, ACP
`session/load`, and `agy --conversation` respectively. In the Team editor,
use `default` to clear the explicit ID.

Asterline names its Codex threads and Claude sessions `Asterline · <member>`
so they are recognizable in native history that exposes a title. A saved roster
in the project's state directory (`team.json`) is also bound to that containing
workspace: on a project move, Asterline corrects a stale serialized workspace
before launching members, so new native transcripts remain associated with the
project you opened. Native session pickers filter by working directory. Claude
Code intentionally omits sessions created by `claude -p` or the Agent SDK from
its interactive picker. Asterline's Team editor does not apply that filter:
select the member's **session id** field and press `Enter` to choose one of
those transcripts, then press `s` to bind it. Asterline resumes the selection
directly with `claude --resume <id>`; `/attach <member>` uses the same direct-ID
path.

Permission modes, sandbox mappings, and allowed-tool behavior depend on the
backend. Do not assume a field has the same effect across all four CLIs.

## Backend setting support

This table describes what the current Asterline adapters actually pass to each
CLI. It is intentionally narrower than the union of fields accepted by the
Team editor.

| Setting                | Codex                                                              | Claude                                      | Grok ACP                                                        | Agy                                                                          |
| ---------------------- | ------------------------------------------------------------------ | ------------------------------------------- | --------------------------------------------------------------- | ---------------------------------------------------------------------------- |
| `cwd`                  | App Server `thread/start`/`thread/resume`                          | Process cwd                                 | ACP session `cwd`                                               | Process cwd plus `--add-dir`; prompt identifies the project workspace        |
| `model`                | App Server `model`                                                 | `--model`                                   | Agent `--model`                                                 | `--model`                                                                    |
| `effort`               | App Server `effort`; picker follows model metadata                 | `--effort` (through `max`)                  | Cache-defined levels pass as Agent `--reasoning-effort`         | Model-specific effort; defined only by its listed model (not a generic menu) |
| `sandbox`              | Folded into `/approvals` presets                                   | Not passed (not shown in `/team`)           | Default `workspace`; not shown in `/team`                       | Default off; not shown in `/team`                                            |
| `permission_mode`      | Codex `/approvals` presets (`Ask for approval` default)            | `--permission-mode` (default omitted)       | `default` / `auto` / `plan` / `--always-approve`                | `--mode accept-edits`/`plan`; skip-permissions only when `off`               |
| `allowed_tools`        | Not passed                                                         | `--tools` (hard built-in-tool allowlist)    | Added to ACP session rules; not a hard protocol-level allowlist | Not passed                                                                   |
| `system_prompt`        | App Server `developerInstructions`                                 | `--append-system-prompt`                    | ACP session `rules`                                             | Prepended to the print prompt                                                |
| `session_policy`       | Resume or fresh                                                    | Resume or fresh                             | ACP `session/load` or `session/new`                             | Resume or fresh conversation                                                 |
| `session_id`           | App Server `thread/resume <thread.id>`                             | `claude --resume <id>`                      | ACP `session/load`                                              | `agy --conversation <id>`                                                    |

`/team` shows the names each installed CLI's own UI uses. Codex combines
sandbox and approval into the `/approvals` presets (`Read Only`, `Ask for
approval`, `Approve for me`, `Full Access`) and sends App Server `approvalPolicy` (`never` by default)
only for unmatched Custom leftovers, plus `sandbox` and `approvalsReviewer`.
Shared `team.json` fields stay on the existing adapter names so older rosters
still load: omitted/default,
`dontAsk`/`bypassPermissions` map to Codex `never`; `plan`/`acceptEdits` map
to `untrusted`; and `auto` maps to `on-request`. Codex command, file-change, and permission-escalation callbacks are
shown as Asterline pending approvals and return your one-time decision to the
live App Server thread.
For Claude and Grok, choose only permission modes accepted by the installed CLI version. Asterline serializes
the configured value but does not negotiate vendor-version compatibility before
launch. Agy 1.1.12 or newer is required;
older versions are excluded from backend detection and rejected before a run
because earlier releases either lack structured streaming or ignore headless
`--mode` enforcement. Recent Claude CLIs no longer list
`default` as a `--permission-mode` choice; Asterline omits the flag when the
configured mode is `default` so the CLI default applies.

## Model discovery

Model choices are resolved in each member's effective working directory:

Asterline detects all four supported CLIs at launch, then starts one
workspace-scoped lookup asynchronously for every installed backend. The
queries do not depend on the configured roster, so a Codex, Claude, Grok, or
Agy member added later can use the already-loading result. Until a lookup
completes the Team editor shows `loading…`; once the CLI reports a concrete
default, it shows that model name rather than the placeholder `default`. The
result is held for the lifetime of that `ast` process: opening and closing
`/team` never re-runs it. A new working directory is shown as `not loaded ·
press Enter` rather than silently launching another background lookup; opening
that member's **Model** field explicitly loads one shared catalog for that
backend and working directory.
If a catalog loaded successfully but its CLI did not identify a default, the
field correctly shows `CLI default`; its picker still contains the discovered
models.

Focus `model` and press `t` to re-fetch that backend/workspace catalog whether
it previously succeeded or failed. The refreshed result is shared by all
matching members. If a load is already in progress, `t` leaves that shared
request in place rather than creating a duplicate.

| Backend | Source                                                          |
| ------- | --------------------------------------------------------------- |
| Codex   | App Server `model/list`; `codex debug models` only as fallback  |
| Claude  | Local settings/env; opt-in gateway `/v1/models`; Claude cache   |
| Grok    | `grok --no-auto-update models`                                  |
| Agy     | `agy models`                                                    |

Claude's catalog is local-configuration first. Asterline reads `model`,
`availableModels`, `modelOverrides`, and the corresponding `ANTHROPIC_*`
variables from the same user/project/local settings hierarchy as Claude Code.
For a `modelOverrides` entry it displays the provider ID while preserving the
Claude-side key passed to `claude --model`.

When that local configuration opts into Claude's gateway discovery with
`CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1` and a non-Anthropic
`ANTHROPIC_BASE_URL`, Asterline uses the same endpoint, authentication, and
custom headers to read `/v1/models`; a failed refresh uses Claude's local
`gateway-models.json` cache. Without a configured model or enabled gateway
discovery, it deliberately shows no invented Claude model list: use the native
CLI default or enter an explicit local/provider model ID.

Agy's local `~/.gemini/antigravity-cli/settings.json` `model` value is often
the human-readable label rather than the CLI ID. Asterline matches it against
the ID/label pairs returned by `agy models`, then displays that configured
model as the startup default.

Startup starts every installed catalog immediately; `/model` and `/team`
reuse the catalog loaded once at `ast` startup. The Agent field lists
all four supported CLIs, includes installation status and
discovered model/effort summaries, and disables missing CLIs. Open the
member's `model` field to browse the already-loading catalog. Type to filter
by display name, model ID, or description, and use `↑`/`↓` to choose a model.
Only a picker whose selected model explicitly reports effort settings shows
`←`/`→`; `←` lowers and `→` raises the selected setting. Those settings are
applied with that model. Browsing with `↑`/`↓` never changes a saved effort
override. If the highlighted model does not advertise the existing override,
`Enter` keeps the current configuration and asks for an explicit `←`/`→`
choice instead of silently substituting a guessed default. Grok reads each listed
model's menu from its own CLI cache, and Agy reads model-qualified settings.
Neither gets a fabricated generic effort menu. Claude has no machine-readable
effort capability catalog, so Asterline does not guess from a model name. A
configured Claude alias remains exactly as configured, while a gateway model
uses the gateway's `display_name`. The picker selects the CLI-marked default
when discovery returns models or, if none is marked, the first discovered
model. It shows only actual model entries in that case. `default` is available
only when discovery returns no models. Focus `model` and press `t` to re-fetch
that catalog; the refresh is shared by matching backend/workspace members.
Press `e` on the field to enter a model ID
manually.

Reasoning effort is model-aware only when discovery returns capability
metadata. Unsupported levels are omitted and the model's reported default is
shown directly as a native default when available; it is not an override.
Agy exposes only the effort encoded by a
discovered model; Grok has no independent Team effort control.

## Streaming and resource limits

Asterline applies explicit limits at process, adapter, runtime, import, and UI
boundaries so a malformed or excessively verbose backend cannot grow memory
without bound:

- JSON protocol records are limited to 8 MiB; stderr records are limited to
  1 MiB.
- A visible assistant message is limited to 4 MiB and one tool detail to
  1 MiB. PTY output retains at most 4 MiB of unread data.
- Verification output retains at most 1 MiB, preserving useful data from both
  the beginning and end of the stream.
- The product runtime-to-TUI queue holds 2,048 events and applies backpressure.
  Abort and shutdown use a separate control channel so they remain responsive
  while stream traffic is saturated.
- Imported JSONL records are limited to 8 MiB and an imported message to
  1 MiB.

When content is shortened, Asterline inserts an explicit truncation marker
instead of presenting the shortened value as complete.

## Runtime data

Asterline writes nothing into the project folder. Its files live under the
Asterline home, `~/.asterline` (`%USERPROFILE%\.asterline` on Windows), or the
directory named by the `ASTERLINE_HOME` environment variable:

```text
~/.asterline/
├── skills/
│   ├── asterline-team/SKILL.md
│   ├── asterline-brainstorm/SKILL.md
│   └── models.md            models `@@spawn` accepts, for agents to read
├── models.json              the same model list as a cache, loaded first at launch
└── projects/<workspace key>/
    ├── team.json
    ├── roster.md
    ├── agy/                 Agy logs
    └── asterline.sqlite3
```

The workspace key is the canonical absolute workspace path with every
character that is not an ASCII letter, digit, `-`, or `_` replaced by `-`, as
Claude Code does for `~/.claude/projects`: `/home/me/app` becomes
`-home-me-app`. A path with any non-ASCII character (for example a Chinese
folder name) also gets `-` and the first 8 hex digits of the SHA-256 of the
full path appended, so different folders never share a key. Directories are
created with mode `0700`.

Claude, Grok, and Agy are started with `--add-dir` for `~/.asterline/skills`
and the project's state directory so they can read the team skill and roster.

`models.json` is the model list from the last discovery (a versioned file,
written atomically). Asterline loads it first at launch, so the model picker,
the `/team` editor, and the list given to the agents are usable before any CLI
answers; discovery then runs in the background, and the file is rewritten only
when its result differs. A missing, damaged, or old-version file is ignored.
`skills/models.md` is the same list in a form agents read: every installed backend's
exact model ids, display names, and allowed efforts. Each main agent's
instructions give its path, and Asterline rewrites it whenever the list changes.

**Moving old state.** Earlier versions kept this data in
`<workspace>/.asterline/` and generated `.agents/skills/asterline-team/` and
`.agents/skills/asterline-brainstorm/` in the project. On first launch in a
workspace, Asterline moves `<workspace>/.asterline/` to the project state
directory (the database, its `-wal`/`-shm` files, `team.json`, `roster.md`)
and removes the skill files it generated, then any of `.agents/skills/…`,
`.agents/skills`, and `.agents` that are left empty. Files with other content
are never deleted. If the new state directory already has a database, it wins:
the old folder is left untouched and a notice says so once. A migration that cannot finish (for example another Asterline is
still using the old folder) is reported and retried on the next launch.

SQLite stores conversations, tool events, teammate routes, raw backend events,
logs, approvals, session identifiers, runs, checklists, timelines, and
verification outcomes.

Protect this directory like any other development transcript. It is outside
your repositories, so there is nothing to add to `.gitignore`.

Every launch opens a new, empty conversation (an existing blank one is reused
and gets the new chat's roster); earlier conversations are reached with
`/resume`. `/new` and `/clear` both create a clean conversation in normal mode
and new backend sessions while retaining older database records. Each new
conversation starts with one default `main` agent (see
[How Asterline resolves a team](#how-asterline-resolves-a-team)). They are rejected while
members, runs, or verification are active; press `Esc` and wait for
cancellation first.
`--no-restore` skips startup replay without deleting data. `--db <PATH>` moves
the database to another location.

`/resume` opens the Agents list on its saved chats (the **Inactive** section);
`/new <prompt>` starts a clean chat and sends the prompt. Restoring a chat also restores the
roster, full member configuration, and each member's native backend session ID
that belonged to that chat, including members added in `/team` and model
changes made there; the chat you leave keeps them.

## Terminal color theme

Asterline uses separate backend identity palettes for dark and light terminal
backgrounds. By default it reads the conventional `COLORFGBG` value and falls
back to the dark palette when the terminal does not expose its background.

Set `ASTERLINE_THEME` when automatic detection does not match the terminal:

```bash
ASTERLINE_THEME=dark asterline
ASTERLINE_THEME=light asterline
```

`auto` restores detection. Backend identity is also communicated by member
names, backend labels, and continuous conversation rails, so color is not the
only cue.

## Permissions and safety

Asterline launches backend CLIs locally and inherits their credentials,
environment variables, filesystem access, and network access. It does not
provide a security boundary around a backend process.

Backend-native permission and sandbox settings still apply. Codex tool asks
are approved automatically unless `--manual-approvals` or
`approvals.manual` is on; then the card above the composer is used.

The `danger-full-access` sandbox and bypass-style permission modes should be
treated as explicit trust decisions. Never assume a member name or model name
limits what the underlying process can access.

## Agent-to-agent coordination

Asterline creates `~/.asterline/skills/asterline-team/SKILL.md` when it is
missing (and upgrades it when Asterline ships a newer version of the skill that
it manages) and tells each member that this team skill is available at that
absolute path. The full protocol stays in the file instead of being repeated in every ordinary
chat turn; this reminder deliberately does not invoke `$asterline-team`, so it
does not force Codex to expand the whole skill. Live roster identity and member
status are rewritten to the project's `roster.md` (its absolute path is in each
member's protocol text) whenever the team or a member's status changes. The
team skill tells agents to read that file; its contents are not copied into
each prompt.

It also creates `~/.asterline/skills/asterline-brainstorm/SKILL.md` when missing.
Brainstorm mode loads that file for every generation, vote, and synthesis
dispatch. Existing copies are never upgraded or overwritten, so deployments
can customize the method while retaining the `@@brainstorm_card` and
`@@brainstorm_vote` schemas.

### Teammate messages

```text
@@team_message {"to":"reviewer","body":"implementation is ready for review"}
@@team_message {"to":["builder","reviewer"],"body":"align on the API"}
@@team_message {"to":"all","body":"report status"}
```

Asterline removes valid envelopes from the visible response, renders the
handoff, persists it, and delivers the body to the target members. Automatic
handoffs are capped by `max_auto_relays` consecutive messages per sender with
no member running a tool or changing a file in between; any tool call or file
change resets the count, so long coordinated work is not interrupted but a
chat-only loop pauses. `/retry` resumes a paused route.

### Roster requests

An agent may request a missing specialty:

```text
@@team_member {"display_name":"QA","backend":"codex"}
```

Asterline validates duplicate IDs and names, starts the runner, saves the
roster, and broadcasts the updated team. Omitted `sandbox` and
`permission_mode` take that backend's write defaults (Agy: `accept-edits`,
sandbox off). Agent envelopes can add members but cannot delete them;
deletion remains a `/team` action. For short-lived helpers prefer
[`@@spawn`](#sub-agents), which keeps the roster in `team.json` unchanged.

### Sub-agents

A top-level member (in `normal` or `team` mode) can open a sub-agent for a side
task, such as an independent review or a piece of parallel work:

```text
@@spawn {"name":"reviewer","task":"Review the diff and list real bugs.","backend":"claude","model":"opus","effort":"high"}
```

`name` and `task` are required. Omitted `backend`, `model`, and `effort` follow
the member that opened it; a different `backend` gets that backend's default
permissions, the same backend keeps the parent's. The sub-agent's id is the
name lowercased (spaces become `-`), its display name is the name with the first
letter of each `-`/`_`-separated part capitalised (`reviewer` is shown as
`Reviewer`, `test-writer` as `Test-Writer`; routing by name ignores case), and the task reaches it as a message from the parent. When it finishes a turn that answered the parent, its final reply
returns to the parent automatically as a hand-back message; if the turn failed
or said nothing, the parent gets a one-line note instead. Messages you send a
sub-agent yourself (open it from the Agents list, `←`) are answered only to you.
The parent can give an existing sub-agent more work with `@@spawn` using the
same name, or with `@@team_message`.

Name a sub-agent after its job (`reviewer`, `test-writer`, `researcher`), not
its model or CLI: the Agents list already shows the model, and a model name goes
stale after `/model`. If the name is only a backend name (`codex`, `claude`,
`grok`, `agy`) or model words (`sol` for `gpt-6.1-sol`, `opus`, `gpt-6-1-sol`),
Asterline opens it as `helper` instead (`helper-2`, `helper-3`, ... when taken)
and tells the parent the real name once. A name that adds a job, such as
`opus-reviewer`, is kept, and a name that already belongs to the parent's
sub-agent still reaches that sub-agent.

Limits: 8 sub-agents per chat, and a sub-agent cannot open sub-agents. In
`/mode team` the existing `allow_add_members` lock applies; review, plan, and
brainstorm runs do not accept `@@spawn`. A refused request shows a notice and
sends the parent one short explanation; hand-backs and refusals count against
`max_auto_relays` like any other automatic relay.

Sub-agents belong to the current chat only. They are never written to
the saved `team.json` (not even after `/model` or `/team` changes), they are
saved with the chat so `/resume` and a normal restart bring them back with
their sessions, and `/new` removes them all (a new chat starts with only the
default `main`, so the top-level members of the old chat are not carried over
either). A single sub-agent can be removed
with `Ctrl+X` (twice) on its row in the Agents list, or by its parent with
`@@close {"name":"reviewer"}` (`"id"` also works); a busy one is stopped first,
its past messages stay in the chat, and the parent gets a one-line note. Both
are refused for a sub-agent that fills a seat of the running review, plan, or
brainstorm run. `/model` on a viewed sub-agent changes
only that sub-agent. The `/team` editor lists top-level members only; a
sub-agent whose parent you remove is removed with it. In `team.json`, a
`parent` member field marks a sub-agent and is written only to the chat
snapshot.

### Run checklist updates

During an active run, an agent can add, update, assign, rename, or
remove checklist steps:

```text
@@run_step {"action":"add","owner":"builder","title":"Write tests"}
@@run_step {"action":"doing","step":1,"note":"Implementing edge cases"}
@@run_step {"action":"done","step":1,"note":"Tests pass"}
@@run_step {"action":"block","step":2,"note":"Waiting for credentials"}
@@run_step {"action":"assign","step":2,"owner":"reviewer"}
```

These updates appear in `/runs` and are recorded in the run timeline.

## CLI options

| Option               | Description                                          |
| -------------------- | ---------------------------------------------------- |
| `--team <PATH>`      | Load a JSON team instead of the saved one            |
| `--workspace <PATH>` | Set the workspace; defaults to the current directory |
| `--db <PATH>`        | Set the SQLite database path                         |
| `--no-restore`       | Do not replay persisted chat on startup              |
| `--debug`            | Developer mode                                       |
| `--manual-approvals` | Show the Codex tool-approval card (off by default)   |
| `--fake`             | Use offline fake agents instead of backend CLIs      |
| `--banner`           | Print a compact startup banner before the TUI        |
| `-h`, `--help`       | Print command-line help                              |

Examples:

```bash
asterline --workspace ~/code/api
asterline --team ./team.json --db ~/.local/share/asterline/api.sqlite3
asterline --fake --no-restore
```

## Troubleshooting

### No supported backend was found

Confirm that at least one of `codex`, `claude`, `grok`, or `agy` is installed,
authenticated, and on `PATH`. Alternatively, pass a valid file with `--team`.

### The model picker has no detected models

Wait for the startup catalog-loading notice to finish, then reopen the
`model` field. Verify the selected CLI is authenticated and can list models in
the member's working directory, then focus `model` and press `t` to re-fetch.
Press `e` to enter a model name manually.

### The wrong roster opens

Edit the project's `team.json` (under `~/.asterline/projects/`), use `/model` to change the model, or use `/team`
and press `s` to apply roster changes. Deleting `team.json` makes the next
launch start from the default `main` agent again.

### Start without the previous transcript

Use `asterline --no-restore`. This skips replay but does not delete SQLite data.
Use `/new` or `/clear` for a clean conversation with new backend sessions and
a single default `main` agent.

### Test without invoking backend CLIs

Run `asterline --fake`. Fake mode exercises the runtime and TUI without calling
Codex, Claude, Grok, or Agy.

### Keyboard input is malformed after leaving an attached CLI

Install the current Asterline build first; it restores terminal keyboard state
when suspending, resuming, and exiting, and disables enhanced keyboard reporting
in VS Code and Cursor terminals. If an older build left the terminal protocol
enabled, run this once in the affected shell:

```bash
printf '\033[=0u'
```

Then start the newly installed binary in a fresh terminal session.
