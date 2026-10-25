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
| `chief init <name> [--goal …] [--repo PATH…]` | you | Create project files, check trust, register the project |
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
UserPromptSubmit context.

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
absolute path of the `chief` binary (`std::env::current_exe()`), so install with `cargo install --path .`.

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
  pid             INTEGER,                -- NULL = process gone (asleep)
  last_hook       TEXT,                   -- last hook event name
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
  last_activity_at INTEGER NOT NULL
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

1. `session_id` matches `sessions.session_id` → that row.
2. Else `session_title` matches `sessions.name` within the project → that row; store `session_id`.
3. Else record the event with `session_name = NULL` and stop.

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
| Stop | `pending_permission = 0`; store `last_assistant_message` in `last_report` and `reports`; update `cwd` and `branch` (`git -C <cwd> branch --show-current`, 2 s timeout) |

Derivation, first match wins (inputs: the row plus the latest `claude agents` snapshot if fresh):

1. `resolved = 1` → **resolved**
2. `cc_state = failed` → **failed**
3. `pending_permission = 1` → **waiting**, reason `permission`
4. `cc_state = blocked` → **waiting**, reason `question`
5. `cc_state = working` or `cc_status = busy` → **working**
6. `pr_state = OPEN` and `pr_review = APPROVED` → **landing**
7. `pr_state = OPEN` and not draft → **review**
8. anything else → **idle** (the board marks it "asleep" when `pid` is NULL)

Without a `claude agents` row (not listed yet, or no poll since the hook), use the provisional group
from the last hook: UserPromptSubmit/PreToolUse/PostToolUse → working; pending permission → waiting;
Stop or Notification `idle_prompt` → idle.

Unknown `cc_state` values count as idle. Auto-resolve: `pr_state = MERGED` → resolved (`merged`);
no activity for 7 days and not working or waiting → resolved (`inactive`).

Board group order: Waiting on you, Working, Ready for review, Landing, Idle, Failed, Resolved.

## 6. Message guard

`chief hook guard` runs on PreToolUse with matcher `SendMessage|ListAgents` for every chief session.

| Caller | Tool | Rule |
| --- | --- | --- |
| any | `ListAgents` | deny: "Use the chief list_threads tool; it lists only this project's threads." |
| coordinator | `SendMessage` | allow only if `to` (with any trailing ` [ref]` removed) is the name of a thread in this project, not resolved, and alive (pid in a fresh `claude agents` snapshot). Asleep → deny: "Thread is asleep: call wake_thread first." Resolved → deny: "Thread is resolved; reopen it first." Also allow `to = uds:…/<pid>.sock` when `<pid>` is a live thread of this project (a reply). Everything else → deny |
| thread | `SendMessage` | deny if `to` starts with `uds:` and the pid is not this project's coordinator; deny if `to` (ref removed) equals the name of any live session in `claude agents` other than this project's coordinator; allow otherwise (the thread's own subagents) |

Every deny writes a `guard_denials` row and returns the PreToolUse deny JSON above. If the guard
itself fails (DB or `claude agents` error), deny with the error as the reason (fail closed).

## 7. Coordinator MCP tools (`chief mcp --project <slug>`)

All tools return JSON text content. Errors are tool errors with a plain message.

| Tool | Input | Does |
| --- | --- | --- |
| `start_thread` | `title` (string), `task` (string), `repo?` (repos[].name or absolute path), `model?`, `effort?` | Checks trust of the repo path, picks a unique name, inserts the row, spawns the thread, resolves its ids. Returns `{name, short_id, repo, model, effort}` |
| `list_threads` | `include_resolved?` (bool, default false) | Refreshes from `claude agents`, returns rows: `name, title, group, waiting_reason, asleep, branch, pr_url, last_report_preview (≤300 chars), last_activity` |
| `read_report` | `name`, `history?` (int, default 1) | Last `history` reports, full text |
| `wake_thread` | `name` | If no live pid: `claude respawn <short_id>`, wait up to 20 s for a pid. Returns `{name, alive}` |
| `resolve_thread` | `name`, `reopen?` (bool) | Sets or clears `resolved` (`coordinator`) |

The coordinator sends work to existing threads itself with `SendMessage` (`to` = name,
`notify_when_idle: true`); the guard enforces scope.

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
`sessionId` from `claude agents --json --all` (retry up to 5 s).

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

No prompt: it waits for you. If the coordinator exists but has no pid, `claude respawn` it.

## 8. Prompts (files in `src/prompts/`, included with `include_str!`)

- `coordinator.md`: role (route, never do the work), routing rules (quick question → answer;
  new work → `start_thread`; follow-up → `SendMessage` to the matching thread with
  `notify_when_idle`, waking it first if asleep; several tasks → several threads), say which thread
  got what, keep `max_parallel`, save preferences and decisions to `memory/` and update
  `memory/MEMORY.md`, read full reports with `read_report` when an idle notice arrives.
- `thread.md`: you are one thread of project <name>; read `<dir>/memory/MEMORY.md` first; project
  instructions follow; work on your own branch; end every turn with a short report (what changed,
  branch, PR URL if any, what you need); if something is missing, say exactly what and stop.

Both get `instructions.md` appended. The coordinator's UserPromptSubmit hook adds a board snapshot
(the `list_threads` rows, compact) and the current `MEMORY.md` as `additionalContext`.

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
- `CHIEF_CLAUDE_BIN` overrides the `claude` binary; integration tests use a fake script that records
  its arguments and prints canned output. `CHIEF_HOME` points tests at a temp dir.
- `scripts/e2e.sh` runs the live loop with cheap sessions (milestone 2b).
