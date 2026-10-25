# chief: architecture and contracts

chief recreates Claude Projects locally. You talk to one coordinator Claude session; it starts and
steers background Claude Code sessions ("threads"); a board parks each thread by status.

This file is the contract. Code follows it; if code needs to differ, change this file in the same
commit and say why.

## 1. Facts this design rests on

Verified against Claude Code 2.1.310 in a spike (payload samples in `tests/fixtures/hooks/`):

- `claude --bg [flags] "<prompt>"` starts a background session under Claude Code's supervisor and
  prints `backgrounded · <short-id> · <name>`. `--session-id` is **ignored** with `--bg`: look the
  full id up in `claude agents --json --all` by short id (`id`) → `sessionId`.
- Background sessions refuse untrusted folders ("Workspace not trusted…"). Trust is per exact path in
  `~/.claude.json` → `projects[<path>].hasTrustDialogAccepted`; parents do not cover children.
- Before editing, a background thread moves into `<repo>/.claude/worktrees/<name>` on branch
  `worktree-<name>`. `worktree.bgIsolation: "none"` in its settings turns that off.
- `claude agents --json --all` rows: `id`, `sessionId`, `name`, `cwd`, `kind`, `startedAt`, `state`
  (seen: `working`, `blocked`, `done`; docs also list failed/stopped/idle), `status` (`busy`,
  `waiting`, `idle`, absent when the process is gone), `pid` (absent when the process is gone).
- Hooks passed with `--settings` fire in background sessions and survive `claude respawn`.
  Every payload has `session_id`, `cwd`, `transcript_path`, `hook_event_name`; SessionStart and
  UserPromptSubmit also carry `session_title` (= the `--name`). Stop carries
  `last_assistant_message` (the full final reply). Notification carries `notification_type`
  (`permission_prompt`, `idle_prompt`).
- A UserPromptSubmit hook can inject context:
  `{"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":"…"}}`.
  Cross-session messages and idle notices also arrive as UserPromptSubmit, so they get it too.
- A PreToolUse hook can deny a tool:
  `{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"…"}}`.
  Matchers are regexes on the tool name (`SendMessage|ListAgents`).
- A coordinator session reaches a thread with the built-in `SendMessage` (`to` = the thread's name,
  `notify_when_idle: true`). An idle thread wakes and starts a turn; a busy one reads it between tool
  calls. The idle notice truncates the reply, so full reports come from the Stop hook.
- Incoming cross-session messages show in the receiver's UserPromptSubmit `prompt` as
  `<cross-session-message from="uds:/tmp/cc-socks/<pid>.sock" from-name="<name>" …>`.
- `claude stop <id>` ends the process and keeps the conversation and worktree.
  `claude respawn <id>` restarts it under the same id with its saved options.
  **Never** use `claude --bg --resume <id> <anything>`: it starts a copy that shares the worktree.
- The supervisor ends a finished, unattached session's process after about an hour.
- `claude attach <short-id>` shows the real session UI. Ctrl+Z detaches and exits the attach
  process; the session keeps running.

Minimum Claude Code version: 2.1.310. `chief` checks `claude --version` on `init` and `up`.

## 2. Processes (no daemon)

Claude Code's supervisor owns every session. SQLite (WAL) is the shared state. One binary:

| Command | Started by | Job |
| --- | --- | --- |
| `chief init <name> [--goal …] [--repo PATH…] [--no-trust-prompt]` | you | Create project files, check trust, register the project |
| `chief up <slug>` | you | Start or revive the coordinator, open the Rex layout |
| `chief down <slug> [--stop-threads]` | you | Close the layout; threads keep running unless asked |
| `chief board --project <slug>` | `chief up` | The board TUI (right pane) |
| `chief pane --project <slug>` | `chief up` | Left pane loop: attaches to the current target session |
| `chief hook <event> --project <slug> --role <coordinator\|thread>` | Claude Code hooks | Record events, derive state, inject context |
| `chief hook guard --project <slug> --role …` | PreToolUse hook | Message guard |
| `chief mcp --project <slug>` | Claude Code (coordinator's MCP config) | Coordinator tools over stdio |
| `chief status [<slug>]` | you | Print the board as text (debugging, scripting) |

Hook commands must be fast, must never block a session, and must exit 0. Failures go to
`<root>/logs/hook-errors.log`. The only non-empty stdout is the guard decision and the coordinator's
UserPromptSubmit context. The `claude agents` call inside the guard is bounded at 10 s, inside the
coordinator's context at 5 s.

`chief init` validates first (Claude Code version, name → slug with ≥1 letter or digit, slug not
taken, every `--repo` an existing folder, stored canonical and named after its folder with `-2`, `-3`
… on a clash) and leaves nothing behind on failure. It then lists every folder Claude Code doesn't
trust yet: the project folder always, plus untrusted repos. When stdin and stdout are terminals
(and without `--no-trust-prompt`) it asks for each "Open Claude there now to trust it? [Y/n]" and
runs `claude` interactively there (accept the prompt, then `/exit` or Ctrl+C twice), then re-checks;
otherwise it prints `cd <folder> && claude` for each.

`chief up` checks the version, rewrites `generated/*`, then runs the coordinator step (§7). Until
the layout lands (§9) it prints the coordinator's name and `claude attach <short_id>`.

`chief status [<slug>]` polls `claude agents` once (stored rows if it fails), syncs (§5), and prints
each project's coordinator and threads grouped in board order.

## 3. Files

`<root>` = `$CHIEF_HOME`, else `$XDG_DATA_HOME/chief`, else `~/.local/share/chief`.

```
<root>/chief.db
<root>/logs/hook-errors.log
<root>/projects/<slug>/
  project.toml          settings (schema below)
  instructions.md       standing brief sent to the coordinator and every new thread
  memory/MEMORY.md      memory index; other memory files beside it
  generated/coordinator-settings.json
  generated/thread-settings.json
  generated/mcp.json
```

`generated/*` is rewritten by `init`, `up` and whenever settings change. Commands inside them use the
canonical absolute path of the `chief` binary (`std::env::current_exe()`), so install with
`cargo install --path .`. Every word of a hook command is shell-quoted.

Sessions are started by Claude Code's supervisor and don't inherit chief's environment, so the
variables `CHIEF_HOME`, `CHIEF_CLAUDE_BIN`, `CHIEF_CLAUDE_CONFIG` and `CHIEF_NO_NOTIFY`, when set at
generation time, are written into the files: as `K='v'` prefixes on every hook command and in the
MCP server's `env`. Paths among them are made absolute (a bare `CHIEF_CLAUDE_BIN` without a slash is
kept for PATH lookup).

- `*-settings.json`: `{"hooks": {...}}` in Claude Code's format, one command per event:
  SessionStart, UserPromptSubmit, PreToolUse (matcher `*`), PreToolUse (matcher
  `SendMessage|ListAgents`, `chief hook guard`), PostToolUse (matcher `*`), Notification, Stop,
  SubagentStop, SessionEnd. Each runs `chief hook <event> --project <slug> --role <role>`.
  The coordinator's file adds `"worktree": {"bgIsolation": "none"}` and
  `"permissions": {"allow": ["mcp__chief"]}` (nobody watches its background session for prompts
  on its own tools).
- `mcp.json`: `{"mcpServers": {"chief": {"type": "stdio", "command": <chief>, "args": ["mcp",
  "--project", <slug>], "env": {...}}}}`.

`instructions.md` starts as an HTML comment explaining its use; comments are stripped before it is
appended to prompts, and an otherwise empty file is not appended. `memory/MEMORY.md` starts with a
one-line header.

### project.toml

```toml
name = "Payments latency"          # display name
slug = "payments-latency"          # [a-z0-9-], unique, immutable
goal = "Hold p95 under 200 ms"     # optional

[coordinator]
model = "opus"                     # passed to --model
effort = "low"                     # low|medium|high|xhigh|max

[threads]
model = "sonnet"
effort = "high"
permission_mode = "auto"           # must not be bypassPermissions (messages would be held)
max_parallel = 3                   # soft; told to the coordinator, not enforced
default_repo = "payments-api"      # a repos[].name

[[repos]]
name = "payments-api"
path = "/Users/me/workspace/payments-api"

[ui]
notify = true                      # macOS notifications on permission prompts and finished threads
```

## 4. SQLite schema (`chief.db`, migrations in `src/db.rs`, `PRAGMA user_version`)

```sql
CREATE TABLE projects (
  slug        TEXT PRIMARY KEY,
  dir         TEXT NOT NULL,
  created_at  INTEGER NOT NULL            -- unix ms
);

CREATE TABLE sessions (                   -- coordinator and threads
  name            TEXT PRIMARY KEY,       -- unique --name, the SendMessage address
  project_slug    TEXT NOT NULL REFERENCES projects(slug),
  role            TEXT NOT NULL CHECK (role IN ('coordinator','thread')),
  short_id        TEXT,                   -- `claude --bg` id; NULL until spawned
  session_id      TEXT UNIQUE,            -- full uuid; NULL until resolved
  title           TEXT,
  task            TEXT,                   -- the first prompt
  repo_name       TEXT,
  repo_path       TEXT,
  cwd             TEXT,                   -- latest cwd seen in a hook (worktree once it moved)
  branch          TEXT,
  model           TEXT,
  effort          TEXT,
  cc_state        TEXT,                   -- last `claude agents` state
  cc_status       TEXT,                   -- last `claude agents` status
  cc_fresh        INTEGER NOT NULL DEFAULT 0, -- 1 = cc_state/cc_status come from a poll newer than the last hook
  pid             INTEGER,                -- NULL = process gone (asleep)
  last_hook       TEXT,                   -- last hook event name; Notification keeps its type: `Notification:permission_prompt`
  pending_permission INTEGER NOT NULL DEFAULT 0,
  group_name      TEXT NOT NULL DEFAULT 'working',   -- derived board group, see §5
  waiting_reason  TEXT,                   -- 'permission' | 'question' | NULL
  resolved        INTEGER NOT NULL DEFAULT 0,
  resolved_reason TEXT,                   -- 'user' | 'coordinator' | 'merged' | 'inactive'
  pr_url          TEXT,
  pr_state        TEXT,                   -- OPEN | MERGED | CLOSED
  pr_draft        INTEGER,
  pr_review       TEXT,                   -- reviewDecision from gh
  last_report     TEXT,
  last_report_at  INTEGER,
  created_at      INTEGER NOT NULL,
  updated_at      INTEGER NOT NULL,
  last_activity_at INTEGER NOT NULL,
  last_hook_at    INTEGER,                -- when last_hook ran (migration 2); see §5 sync
  gone_at         INTEGER                 -- when a snapshot no older than the last hook showed no process (migration 3)
);

CREATE TABLE events (
  id           INTEGER PRIMARY KEY,
  session_name TEXT,                      -- NULL if the hook could not match a session
  session_id   TEXT,
  project_slug TEXT NOT NULL,
  hook         TEXT NOT NULL,
  ts           INTEGER NOT NULL,
  summary      TEXT                       -- short human line; never the whole payload
);

CREATE TABLE reports (
  id           INTEGER PRIMARY KEY,
  session_name TEXT NOT NULL,
  ts           INTEGER NOT NULL,
  text         TEXT NOT NULL
);

CREATE TABLE guard_denials (
  id           INTEGER PRIMARY KEY,
  project_slug TEXT NOT NULL,
  session_name TEXT,
  tool         TEXT NOT NULL,
  target       TEXT,
  reason       TEXT NOT NULL,
  ts           INTEGER NOT NULL
);

CREATE TABLE ui_state (
  project_slug TEXT PRIMARY KEY,
  pane_target  TEXT                       -- session name the left pane should show
);
```

Every connection: `journal_mode=WAL`, `busy_timeout=5000`, `foreign_keys=ON`.

### Matching a hook to a session

1. `session_id` matches `sessions.session_id` → that row, if it belongs to the hook's project.
2. Else `session_title` matches `sessions.name` within the project and that row has no
   `session_id` yet → that row; store `session_id`. (A row with another `session_id` is not
   matched: the hook comes from a copy of the session.)
3. Else record the event with `session_name = NULL` and stop.

Matching, the event row and the session update run in one `BEGIN IMMEDIATE` transaction; the git
branch lookup runs before it so the write lock is never held for the 2 s timeout.

## 5. Thread state and board groups

`src/state.rs` holds a pure function; everything else stores its result in `group_name` and
`waiting_reason`.

Hook updates (per matched session):

| Hook | Effect |
| --- | --- |
| SessionStart | `last_activity_at` |
| UserPromptSubmit, PreToolUse | `pending_permission = 0`; provisional group `working` |
| PostToolUse | `pending_permission = 0`; update `cwd`; provisional `working` |
| Notification `permission_prompt` | `pending_permission = 1` |
| Notification `idle_prompt` | nothing beyond `last_hook` |
| Stop | `pending_permission = 0`; store `last_assistant_message` in `last_report` and `reports` (skipped when empty); update `cwd` and `branch` (`git -C <cwd> branch --show-current`, 2 s timeout, threads only, kept on a detached HEAD or error) |

Every hook above also sets `last_hook`, `updated_at` and `cc_fresh = 0`, re-derives the group, and
(except Notification `idle_prompt`) sets `last_activity_at`. Other events (SubagentStop,
SessionEnd, other notification types) are only recorded in `events`. Coordinator Stops store
reports too.

When `[ui] notify` is true, a thread's `permission_prompt` and a thread's Stop send a macOS
notification (`osascript`, spawned without waiting; skipped if missing or `CHIEF_NO_NOTIFY=1`).

Derivation, first match wins (inputs: the row plus the latest `claude agents` snapshot if fresh):

1. `resolved = 1` → **resolved**
2. `cc_state = failed` → **failed**
3. `pending_permission = 1` → **waiting**, reason `permission`
4. `cc_state = blocked` → **waiting**, reason `question`
5. `cc_state = working` or `cc_status = busy` → **working**, unless the process is gone (below)
6. `pr_state = OPEN` and `pr_review = APPROVED` → **landing**
7. `pr_state = OPEN` and not draft → **review**
8. anything else → **idle** (the board marks it "asleep" when `pid` is NULL)

Without a `claude agents` row (not listed yet, or no poll since the hook: `cc_fresh = 0`), use the
provisional group from the last hook: UserPromptSubmit/PreToolUse/PostToolUse → working; pending
permission → waiting; Stop or Notification `idle_prompt` → idle. Concretely, rules 2, 4 and 5 read
`cc_state` as absent and rule 5 matches when the last hook is one of the three working hooks; a
provisional idle still falls through rules 6 and 7, so a stopped thread with an open PR shows in
review.

Unknown `cc_state` values count as idle. Auto-resolve (threads only, and only if the thread was
never resolved before: reopening keeps `resolved_reason`, so a reopened thread is not resolved
again automatically; reopening also sets `last_activity_at`): `pr_state = MERGED` → resolved
(`merged`); no activity for 7 days and not working or waiting → resolved (`inactive`).

Sync (`src/registry.rs`, run with each `claude agents` snapshot and the time it was taken, read just
before running the command): match each project session by `session_id`, or by `short_id` = row
`id` while `session_id` is NULL (then store it). Matched rows get `cc_state`, `cc_status`, `pid` and
`cc_fresh = 1`; unmatched rows get `pid = NULL` and `cc_fresh = 0` and keep their last `cc_state`.
Rows with neither id (not spawned yet) are left alone. Then re-derive and apply auto-resolve.

A snapshot taken before a row's last hook (`last_hook_at` > taken-at) is stale for that row: it keeps
`cc_fresh = 0` and its `cc_state`/`cc_status`, so the hook's provisional group stands. Ids are still
learned, a matched row still takes the snapshot's `pid`, and an unmatched row keeps its `pid` (the
newer hook proves the process ran). Every hook in the table above sets `last_hook_at`.

A thread whose process died mid-turn drops out of `claude agents` (or stays listed without a pid),
and its last hook would keep it provisionally working forever. So a non-stale snapshot that shows
no process sets `gone_at` = taken-at (only when the verdict changes, so repeated polls don't rewrite
the row), and the row counts as **process gone** while `pid` is NULL and `gone_at` ≥
`last_hook_at`. A process-gone row never matches rule 5, neither from `cc_state`/`cc_status` nor
from the provisional working hooks; rules 1–4 and 6–8 still apply, so it shows as waiting
(pending permission), review/landing (open PR), resolved, or idle/asleep. A newer hook (e.g. a
respawn starting a turn) makes it count as running again until the next non-stale poll.

Board group order: Waiting on you, Working, Ready for review, Landing, Idle, Failed, Resolved.

## 6. Message guard

`chief hook guard` runs on PreToolUse with matcher `SendMessage|ListAgents` for every chief session.

| Caller | Tool | Rule |
| --- | --- | --- |
| any | `ListAgents` | deny: "Use the chief list_threads tool; it lists only this project's threads." |
| coordinator | `SendMessage` | allow only if `to` (with any trailing ` [ref]` removed) is the name of a thread in this project, not resolved, and alive (pid in a fresh `claude agents` snapshot). Asleep → deny: "Thread is asleep: call wake_thread first." Resolved → deny: "Thread is resolved; reopen it first." Also allow `to = uds:…/<pid>.sock` when `<pid>` is a live thread of this project (a reply). Everything else → deny |
| thread | `SendMessage` | deny if `to` starts with `uds:` and the pid is not this project's coordinator; deny if `to` (ref removed) equals the name of any live session in `claude agents` other than this project's coordinator; allow otherwise (the thread's own subagents) |

`to` is read from `tool_input.to`; a SendMessage whose `tool_input` isn't an object or has no string
`to` is denied. A thread's pid is the pid that the snapshot lists for its `session_id` (or
`short_id` while unresolved); the coordinator's pid likewise. The coordinator is also denied when
more than one live session carries the thread's name (never deliver to an ambiguous name). Other
tools are allowed. `claude agents` is only run for a SendMessage with a `to`.

Every deny writes a `guard_denials` row (caller matched by `session_id`, `target` = raw `to`) and
returns the PreToolUse deny JSON above; an allow prints nothing. If the guard itself fails (bad
payload, DB or `claude agents` error, or a panic), it denies with the error as the reason (fail
closed), logs the error, and records the denial if the DB opens.

## 7. Coordinator MCP tools (`chief mcp --project <slug>`)

All tools return JSON text content. Errors are tool errors with a plain message.

| Tool | Input | Does |
| --- | --- | --- |
| `start_thread` | `title` (string), `task` (string), `repo?` (repos[].name or absolute path), `model?`, `effort?` | Checks trust of the repo path, picks a unique name, inserts the row, spawns the thread, resolves its ids. Returns `{name, short_id, repo, model, effort, next}` |
| `list_threads` | `include_resolved?` (bool, default false) | Refreshes from `claude agents` (stored rows plus a `warning` if that fails), returns `{threads: [...]}` in board order: `name, title, group, waiting_reason, asleep, branch, pr_url, last_report_preview (≤300 chars), last_activity` ("5m ago") |
| `read_report` | `name`, `history?` (int, default 1, max 20) | `{name, title, group, reports: [{at, text}]}`, newest first, full text |
| `wake_thread` | `name` | If no live pid: `claude respawn <short_id>`, poll `claude agents` up to 20 s for a pid. Returns `{name, alive, respawned}` |
| `resolve_thread` | `name`, `reopen?` (bool) | Sets or clears `resolved` (`coordinator`). Returns `{name, resolved, group}` |

Names may carry a trailing ` [ref]`; it is ignored. Only this project's threads are accepted.

`start_thread` details: `repo` falls back to `threads.default_repo` (error if neither); an absolute
path must be a folder and maps back to a repos[] name when it is one. Trust is checked before
anything else and an untrusted repo returns the `NotTrusted` message. The name must not be in the
registry nor among `claude agents` names (a few suffixes are tried; if `claude agents` fails,
nothing starts). A task that starts with `-` is sent as `Task: <task>` so the CLI can't read it as
a flag. If the spawn fails, the inserted row is deleted and the error returned. Failing to resolve
the session id is not an error (hooks match by title, sync by short id). `next` is
`Subscribe now: SendMessage(to="<name>", notify_when_idle=true) with no message.`: the coordinator
subscribes right after starting a thread, which is how it hears the thread finished.

The coordinator sends work to existing threads itself with `SendMessage` (`to` = name,
`notify_when_idle: true`); the guard enforces scope.

The tool logic lives in `src/tools.rs` as plain functions over a context (connection, config,
`Claude`, project dir, trust file, clock, timeouts); `src/mcp.rs` adapts them to `rmcp` and runs
each call on the blocking pool with a freshly loaded `project.toml`. stdout is the protocol; logs go
to stderr.

### Thread spawn

```
cwd  = repo path
name = <slug>-<title slug, ≤24 chars>-<4 hex>   (must not appear in `claude agents --json --all`)
claude --bg --name <name> --model <m> --effort <e> --permission-mode <threads.permission_mode>
       --settings <dir>/generated/thread-settings.json
       --add-dir <dir>/memory
       --append-system-prompt <thread brief, §8>
       "<task>"
```

Parse `backgrounded · <short-id>` from stdout (error if missing, include stderr). Resolve
`sessionId` from `claude agents --json --all` (retry up to 5 s). List flags (`--add-dir`,
`--disallowedTools`) are repeated once per value, and empty optionals are left out. The command runs
with stdin closed; "Workspace not trusted" in its output becomes `ClaudeError::NotTrusted(cwd)`,
whose message says to run `claude` in that folder once and accept the trust prompt.

Trust (`claude::is_trusted`) checks both the path as given and its canonical form (macOS `/tmp` is
`/private/tmp`), reading `$CHIEF_CLAUDE_CONFIG` instead of `~/.claude.json` when set.

### Coordinator spawn (`chief up`)

```
cwd  = <dir>   (must be trusted; `chief init` tells you how)
name = chief-<slug>
claude --bg --name chief-<slug> --model <m> --effort <e> --permission-mode auto
       --settings <dir>/generated/coordinator-settings.json   (hooks + "worktree": {"bgIsolation": "none"})
       --mcp-config <dir>/generated/mcp.json
       --disallowedTools Bash
       --add-dir <each repo path>
       --append-system-prompt <coordinator prompt, §8>
```

No prompt: it waits for you. Started when it has no row or no `short_id` (the project folder must be
trusted, and no other live session may already use the name; a failed spawn removes a row it
inserted). If it exists but `claude agents` shows no pid, `claude respawn` it and wait up to 10 s for
a pid. Otherwise it is left alone.

## 8. Prompts (files in `src/prompts/`, included with `include_str!`)

- `coordinator.md`: role (route, never do the work), routing rules (quick question → answer;
  new work → `start_thread`; follow-up → `SendMessage` to the matching thread with
  `notify_when_idle`, waking it first if asleep; several tasks → several threads), say which thread
  got what, keep `max_parallel`, save preferences and decisions to `memory/` and update
  `memory/MEMORY.md`, read full reports with `read_report` when an idle notice arrives.
- `thread.md`: you are one thread of project <name>; read `<dir>/memory/MEMORY.md` first; project
  instructions follow; work on your own branch; end every turn with a short report (what changed,
  branch, PR URL if any, what you need); if something is missing, say exactly what and stop.

Placeholders (`{{name}}` replaced verbatim): coordinator `name`, `goal_line` (` Goal: …` or empty),
`project_dir`, `max_parallel`; thread `name`, `coordinator_name`, `project_dir`. Both get
`instructions.md` appended under `## Project instructions` when it isn't empty.

The coordinator's UserPromptSubmit hook runs `claude agents` (≤5 s; stored rows if it fails, logged),
syncs, and returns as `additionalContext`:

```
<chief-board as-of="HH:MM">
Waiting on you:
- <name> "<title>" · waiting: permission · asleep · branch <b> · PR <url>
  report: <≤200-char one-line preview>
…groups in board order; resolved threads left out; "No open threads." when empty
</chief-board>
<chief-memory>
<MEMORY.md, or (empty)>
</chief-memory>
```

The whole context stays under 8 KB: previews shrink first (200, 100, 40, none), then memory is cut
with a marker, then the board itself.

## 9. Layout (`chief up`, Rex first, tmux later)

`src/mux/mod.rs` defines:

```rust
pub trait Mux {
    fn ensure_session(&self, label: &str, cwd: &Path) -> Result<SessionRef>;
    fn run_block(&self, s: &SessionRef, spec: &BlockSpec) -> Result<BlockRef>; // split, ratio, label, cmd
    fn find_block(&self, s: &SessionRef, label: &str) -> Result<Option<BlockRef>>;
    fn send_key(&self, b: &BlockRef, key: &str) -> Result<()>;
    fn focus(&self, b: &BlockRef) -> Result<()>;
    fn kill_session(&self, s: &SessionRef) -> Result<()>;
}
```

`src/mux/rex.rs` implements it with the `rex` CLI (`--json` outputs). Layout: Rex session
`chief-<slug>`, block `main` running `chief pane --project <slug>`, block `board` split right at 32 %
running `chief board --project <slug>`.

`chief pane` loop: read `ui_state.pane_target` (default the coordinator); run
`claude attach <short_id>` as a child on the same TTY; when it exits (Ctrl+Z detach), read the target
again and repeat. If the target is asleep, respawn it first. If attach fails, print why and poll the
target every 500 ms.

Board switching: write `pane_target`, then `send_key(main, "ctrl+z")`.

## 10. Board

- Refresh every 2 s from SQLite and `claude agents --json --all` (filtered to the project's session
  ids, written back through §5). PR status every 30 s:
  `gh pr list --head <branch> --state all --json url,state,isDraft,reviewDecision --limit 1`
  run in `repo_path`.
- Layout: header (project, goal, coordinator state, denials count); groups in §5 order with counts;
  detail panel for the selection (title, name, group, reason, branch, PR, model, last report).
- Keys: ↑/↓ or j/k move; Enter view thread in the main pane; Esc or c view coordinator; w wake;
  r resolve or reopen; o open PR; s settings; ? help; q quit the board only.
- Settings screen edits `project.toml` (goal, models, efforts, permission mode, max_parallel,
  default repo, repos) and opens `instructions.md` or `memory/` in `$EDITOR`. Saving regenerates
  `generated/*`. Changes reach new threads; coordinator model and effort apply on its next start.

## 11. Testing

- Unit tests: §5 derivation (table-driven), §6 guard (table-driven), hook payload parsing against
  `tests/fixtures/hooks/spike-events.jsonl`, `claude agents` parsing against
  `tests/fixtures/agents/agents-sample.json`, config round trip, migrations on a fresh DB.
- `CHIEF_CLAUDE_BIN` overrides the `claude` binary; integration tests use a fake script
  (`tests/fake-claude.sh`) that records its arguments and prints canned output. `CHIEF_HOME` points
  tests at a temp dir, `CHIEF_CLAUDE_CONFIG` replaces `~/.claude.json`, `CHIEF_NO_NOTIFY=1` turns off
  notifications.
- `scripts/e2e.sh` runs the live loop with cheap sessions (milestone 2b).
