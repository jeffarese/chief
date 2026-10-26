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
- Background sessions refuse untrusted folders ("Workspace not trusted…"). Trust lives in
  `~/.claude.json` → `projects[<path>].hasTrustDialogAccepted`. A trusted folder covers the folders
  under it, even one whose own entry is false, but not past a git repository's root: with `/`
  trusted, a plain folder worked and a fresh git repo was refused (verified live in M2b).
- Before editing, a background thread moves into `<repo>/.claude/worktrees/<name>` on branch
  `worktree-<name>`. `worktree.bgIsolation: "none"` in its settings turns that off. The
  supervisor records it in `<claude dir>/jobs/<short_id>/state.json` → `worktreePath` (with
  `worktreeBranch`); a session that never moved has no `worktreePath`. That file, not a hook's
  `cwd`, is the source of truth for a thread's worktree (§12).
- `CLAUDE_CONFIG_DIR` gives Claude Code a separate config folder (one per account): its own
  `.claude.json` (trust), `jobs/` and supervisor, so `claude agents`, `attach` and `rm` only see that
  folder's sessions. Unset, the folder is `~/.claude` and the config file `~/.claude.json`
  (checked on this machine with two account folders).
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
- `claude rm <id>` (verified live in M2b) stops the session **even when it then refuses**, and
  refuses (exit 1, "kept <id> — …") while the worktree has unpushed commits or uncommitted changes
  (untracked files count; git-ignored files don't). An unpushed refusal prints
  `claude rm <id> --discard-unpushed <head sha>@<worktree-id>`; a dirty refusal offers no flag in
  its message, though usage lists `--force-remove-worktree <worktree-id>` (untested). On success it
  deletes the worktree and the local branch, keeps any remote branch, and the session disappears
  from `claude agents --all` (a stopped session stays listed as `stopped`).
- Threads name their own worktree (`EnterWorktree`), e.g. `.claude/worktrees/add-hello-txt` on
  `worktree-add-hello-txt`, not after the thread; use the cwd and branch hooks report.
- A thread killed mid-turn stays `state: working` with no status and no pid in `claude agents`.
- A `permissions.deny` rule `Edit(//<absolute canonical path>/**)` in `--settings` (a leading `//`
  means an absolute path) blocks both the Edit and the Write tool for every file under that folder
  in a background session ("File is in a directory that is denied by your permission settings."),
  and other folders stay writable. `Write(…)` and `NotebookEdit(…)` deny rules are useless: Claude
  Code prints "Deny rule Write(//…/**) never matches · use Edit(//…/**)" at startup (verified live
  on 2.1.310 with `--settings` and `--permission-mode auto`).

Minimum Claude Code version: 2.1.310. `chief` checks `claude --version` on `init` and `up`.

## 2. Processes (no daemon)

Claude Code's supervisor owns every session. SQLite (WAL) is the shared state. One binary:

| Command | Started by | Job |
| --- | --- | --- |
| `chief init <name> [--goal …] [--repo PATH…] [--no-trust-prompt]` | you | Create project files, check trust, register the project |
| `chief up <slug>` | you | Start or revive the coordinator, open the layout (Rex, tmux, or say how) |
| `chief down <slug> [--stop-threads]` | you | Close the layout; threads keep running unless asked |
| `chief board --project <slug>` | `chief up` | The board TUI (right pane) |
| `chief pane --project <slug>` | `chief up` | Left pane loop: attaches to the current target session |
| `chief hook <event> --project <slug> --role <coordinator\|thread>` | Claude Code hooks | Record events, derive state, inject context |
| `chief hook guard --project <slug> --role …` | PreToolUse hook | Message guard |
| `chief mcp --project <slug>` | Claude Code (coordinator's MCP config) | Coordinator tools over stdio |
| `chief status [<slug>]` | you | Print the board as text (debugging, scripting) |
| `chief gc [<slug>] [--dry-run]` | you | Refresh PRs, then clean up every resolved thread that loses nothing (§12) |
| `chief clean <slug> <thread> [--discard] [--yes]` | you | Clean up one resolved thread; `--discard` throws away unsaved work after confirming (§12) |

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

`chief up` checks the version, rewrites `generated/*`, runs the coordinator step (§7), then opens
the layout (§9): with Rex when it is installed and its server runs (`rex list` succeeds), storing
the session id in `ui_state.rex_session`; else, when it runs inside tmux (`$TMUX` set), in a tmux
window, storing it in `ui_state.tmux_window`; else it prints why and the two commands to run side
by side in terminals or splits of the user's own, `chief pane --project <slug>` and `chief board
--project <slug>`. When opening the chosen layout fails it prints why and the same two commands.

`chief down <slug> [--stop-threads]` closes the layout `chief up` opened, and only that: the Rex
session and the tmux window it stored, each when stored (§9 `close`: kills the session or window
when it holds only chief's blocks, else closes just `main` and `board`), or nothing when none is
stored (`No layout of chief's is open.`, as for a layout the user made). It clears
`ui_state.pane_showing` and `ui_state.pane_target` (the next `chief up` shows the coordinator; a
pane back on the thread it last showed would respawn it right after `--stop-threads` stopped it),
syncs `claude agents` (stored rows if it fails), and runs
`claude stop` on the coordinator when it has a process; with `--stop-threads` also on every
uncleaned thread with a process. Without it, it says how many threads keep running. It never runs
`claude rm`. Each step prints what it did; any failure makes it exit non-zero after the rest ran.

`chief status [<slug>]` polls `claude agents` once (stored rows if it fails), syncs (§5), and prints
each project's coordinator and threads grouped in board order. A thread's line joins with ` · `
its name, `"<title>"`, waiting reason, `asleep`, `branch <b>`, `PR <url>` (each when there is
one), then `cleaned up` or `cleanup blocked: <reason>` when §12 applies, and always ends with when
it was last active (`5m ago`, `just now`).

## 3. Files

`<root>` = `$CHIEF_HOME`, else `$XDG_DATA_HOME/chief`, else `~/.local/share/chief`, made absolute.

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

`generated/*` is rewritten from `project.toml` by `init`, `up` and whenever settings change, and
right before every spawn and respawn (`start_thread`, `wake_thread`, the pane's respawn, `chief up`):
Claude Code reads hooks and permissions when a session starts, so an edit to these files (one that
drops the guard, say) never reaches a later start. A rewrite that fails stops the spawn or
respawn. Commands inside them use the canonical absolute path of the `chief` binary
(`std::env::current_exe()`), so install with `cargo install --path .`: `chief up` prints a
one-line warning when that path is inside a cargo `target/` folder (an ancestor named `target`
beside a `Cargo.toml`), since `cargo clean` would disable chief's hooks and guard. Every word of a
hook command is shell-quoted.

Sessions are started by Claude Code's supervisor and don't inherit chief's environment, so
`CHIEF_HOME` is always written into the files, set to the resolved `<root>` (a root that came from
`$XDG_DATA_HOME`, which the supervisor doesn't have, or from the default), and the variables
`CHIEF_CLAUDE_BIN`, `CHIEF_GH_BIN`, `CHIEF_REX_BIN`, `CHIEF_TMUX_BIN`, `CHIEF_CLAUDE_CONFIG`,
`CHIEF_NO_NOTIFY` and `CLAUDE_CONFIG_DIR` (§1: every chief process must talk to the same account's
supervisor) are written when set at generation time: as `K='v'` prefixes on every hook command and
in the MCP server's `env`. Paths among them are made absolute (a bare `CHIEF_CLAUDE_BIN`,
`CHIEF_GH_BIN`, `CHIEF_REX_BIN` or `CHIEF_TMUX_BIN` without a slash is kept for PATH lookup). Rex
blocks start from the login shell's environment and tmux panes from the tmux server's, so the
layout's commands carry the same variables (§9).

- `*-settings.json`: `{"hooks": {...}}` in Claude Code's format, one command per event:
  SessionStart, UserPromptSubmit, PreToolUse (matcher `*`), PreToolUse (matcher
  `SendMessage|ListAgents`, `chief hook guard`), PostToolUse (matcher `*`), Notification, Stop,
  SubagentStop, SessionEnd. Each runs `chief hook <event> --project <slug> --role <role>`. The
  state and context hooks fail open (a missing binary is a non-blocking hook error). So do bad
  arguments: clap's exit 2 would block (every tool for the `*` PreToolUse, the prompt for
  UserPromptSubmit, the end of the turn for Stop), so a `chief hook` whose arguments clap rejects
  logs `bad arguments: …` to the hook error log and exits 0 with empty stdout, unless one of its
  words (not the `--project` or `--role` value) is `guard`: then it exits 2 with the reason on
  stderr. Other subcommands keep clap's usual exit 2. The guard's command fails closed:
  `<chief> hook guard --project <slug> --role <role> || { echo "chief's message guard could not
  run (<chief path>); message blocked. Run chief up again." >&2; exit 2; }` (the message is one
  shell-quoted word), so a missing or broken binary (exit 127 or any non-zero exit) blocks the
  call with exit 2, which Claude Code treats as a block and shows to the model. The guard's entry
  also sets `"timeout": 30` (seconds; the time budget is in §6). The coordinator's file adds `"worktree": {"bgIsolation": "none"}` and `"permissions"`:
  `"allow": ["mcp__chief"]` (nobody watches its background session for prompts on its own tools)
  and `"deny"`: `Edit(//<repo path>/**)` for every repo, `Edit(//<dir>/generated/**)`,
  `Edit(//<dir>/project.toml)` and `Edit(//<root>/chief.db*)` (the database and its -wal/-shm),
  canonical paths, built in one place (`generated::coordinator_deny_rules`, syntax in
  `generated::edit_deny_rule` and `edit_deny_files`). Per §1 that blocks Edit and Write there: a
  routing slip can't edit a real checkout, the coordinator can't edit the files that hold its own
  and its threads' guard, and settings and state stay the user's and chief's. Memory and
  `instructions.md` stay editable.
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
cleanup = "merged"                 # merged | manual (default merged), see §12

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
  resolved_reason TEXT,                   -- 'user' | 'coordinator' | 'merged' | 'inactive' | 'removed'
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
  gone_at         INTEGER,                -- when a snapshot no older than the last hook showed no process (migration 3)
  cleaned_at      INTEGER,                -- when the session, worktree and local branch were removed, §12 (migration 4)
  cleanup_blocked TEXT,                   -- why the last cleanup kept it, e.g. '2 unpushed commits' (migration 4)
  synced_at       INTEGER,                -- taken-at of the newest snapshot applied; older ones are ignored, §5 (migration 6)
  ids_at          INTEGER,                -- when short_id or session_id last changed; db::update_session stamps it (migration 6)
  removed_at      INTEGER,                -- when a sync found the session removed outside chief, §5 (migration 6)
  cleaning_at     INTEGER                 -- cleanup claim: when a process claimed it, §12 (migration 7; not in SessionRow)
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
  pane_target  TEXT,                      -- session name the left pane should show
  pane_showing TEXT,                      -- session `chief pane` is attached to now; NULL between attaches (migration 5)
  rex_session  TEXT,                      -- id of the Rex session `chief up` created, §9 (migration 8)
  pane_pid     INTEGER,                   -- the running `chief pane`, which the board signals to switch, §9 (migration 9)
  pane_started TEXT,                      -- that process's start time, proving the pid is still it (migration 9)
  tmux_window  TEXT                       -- '<window id> <server pid> <socket>' of the tmux window `chief up` opened, §9 (migration 9)
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
3. `pending_permission = 1` → **waiting**, reason `permission`; else `cc_status = waiting` →
   **waiting**, reason `question`; both unless the process is gone (below)
4. `cc_state = blocked` → **waiting**, reason `question`
5. `cc_status = busy` → **working**, unless the process is gone (below)

`cc_state` is Claude Code's judgment of the task (working, blocked, done), not of the process: in M2b
a thread that had finished its turn stayed `working` / `idle` indefinitely (its job record never
reached a terminal state). So only `cc_status` (busy, waiting, idle) says whether something runs;
`cc_state` still drives rules 2 and 4. A live permission prompt showed as `blocked` / `waiting` in
the spike, which is why a `waiting` status counts even before the Notification hook lands.
6. `pr_state = OPEN` and `pr_review = APPROVED` → **landing**
7. `pr_state = OPEN` and not draft → **review**
8. anything else → **idle** (the board marks it "asleep" when `pid` is NULL)

Without a `claude agents` row (not listed yet, or no poll since the hook: `cc_fresh = 0`), use the
provisional group from the last hook: UserPromptSubmit/PreToolUse/PostToolUse → working; pending
permission → waiting; Stop or Notification `idle_prompt` → idle. Concretely, rules 2–5 read
`cc_state` and `cc_status` as absent and rule 5 matches when the last hook is one of the three
working hooks; a
provisional idle still falls through rules 6 and 7, so a stopped thread with an open PR shows in
review.

Unknown `cc_state` values count as idle. Auto-resolve (threads only, and only if the thread was
never resolved before: reopening keeps `resolved_reason`, so a reopened thread is not resolved
again automatically; reopening also sets `last_activity_at`): `pr_state = MERGED` → resolved
(`merged`); no activity for 7 days and not working or waiting → resolved (`inactive`).

Sync (`src/registry.rs`, run with each `claude agents` snapshot and the time it was taken, read just
before running the command): match each project session by `session_id`, else by `short_id` = row
`id`. A row matched by short id under another session id (after `/clear`) takes the new
`session_id`, unless another registered row already has it; a row without a `session_id` stores
the listed one. Matched rows get `cc_state`, `cc_status`, `pid` and `cc_fresh = 1`; unmatched rows
get `pid = NULL` and `cc_fresh = 0` and keep their last `cc_state`. Rows with neither id (not
spawned yet) are left alone. Then re-derive and apply auto-resolve. Each row keeps a watermark,
`synced_at` = taken-at of the newest snapshot applied to it: a snapshot older than that is ignored
for the row (polls from the board, hooks and the CLI run in separate processes and may commit out
of order). A poll that changes nothing else moves only the watermark (`updated_at` stays).

Removed outside chief: a stopped session stays listed in `claude agents --all`, and only
`claude rm` takes one off. So a thread row that has a `session_id`, isn't cleaned or already
marked removed, and is missing from a non-stale snapshot (neither its `session_id` nor its
`short_id` listed) was removed, but only when the snapshot can know that: it was taken after the
row was created and after its ids were last set (`ids_at`), and it lists at least one other
session registered in this chief home (any project). A snapshot that lists none of them comes from
another Claude config or supervisor, or from one that lists nothing for a moment; a wrong verdict
would lock the thread out (§12). The coordinator is normally listed, so this only delays the
verdict. The row gets `removed_at` = taken-at and, if it wasn't resolved, `resolved = 1` with
`resolved_reason = 'removed'`. When its worktree is gone too (job state names none, §12, or the
folder no longer exists) it also gets `cleaned_at` = taken-at and `cleanup_blocked = NULL`;
otherwise `cleanup_blocked = 'session removed outside chief; worktree remains at <path>'`, and a
later sync that finds that folder gone sets `cleaned_at`. The coordinator row is never marked.
If a later snapshot lists the session again, the sync undoes only what it did: `removed_at`
cleared, `cleaned_at` cleared when it equals `removed_at`, `resolved`/`resolved_reason` cleared when
the reason is `removed`, and the `session removed outside chief…` `cleanup_blocked` cleared.

A snapshot taken before a row's last hook (`last_hook_at` > taken-at) is stale for that row: it keeps
`cc_fresh = 0` and its `cc_state`/`cc_status`, so the hook's provisional group stands. Ids are still
learned, a matched row still takes the snapshot's `pid`, and an unmatched row keeps its `pid` (the
newer hook proves the process ran). Every hook in the table above sets `last_hook_at`.

A thread whose process died mid-turn drops out of `claude agents` (or stays listed without a pid),
and its last hook would keep it provisionally working forever. So a non-stale snapshot that shows
no process sets `gone_at` = taken-at (only when the verdict changes, so repeated polls don't rewrite
the row), and the row counts as **process gone** while `pid` is NULL and `gone_at` ≥
`last_hook_at`. A process-gone row never matches rule 5, neither from `cc_state`/`cc_status` nor
from the provisional working hooks. A permission prompt dies with its process and a respawn doesn't
bring it back, so the sync that sets `gone_at` also clears `pending_permission`, and rule 3 doesn't
match a process-gone row either. Rules 1, 2, 4 and 6–8 still apply, so it shows as waiting (a
blocked question), review/landing (open PR), resolved, failed, or idle/asleep. A newer hook (e.g. a
respawn starting a turn) makes it count as running again until the next non-stale poll.

Board group order: Waiting on you, Working, Ready for review, Landing, Idle, Failed, Resolved.

## 6. Message guard

`chief hook guard` runs on PreToolUse with matcher `SendMessage|ListAgents` for every chief session.

| Caller | Tool | Rule |
| --- | --- | --- |
| any | `ListAgents` | deny. Coordinator: "Use the chief list_threads tool; it lists only this project's threads." Thread: "ListAgents is blocked in chief threads: you can message only your coordinator." |
| coordinator | `SendMessage` | allow only if `to` is exactly the name of a thread registered to this project, or exactly the socket address of one (a reply, form below), and that thread is not cleaned, not resolved, and reachable (below). Cleaned → deny: "<name> was cleaned up; start a new thread (its reports are still readable with read_report)". Resolved → deny: "Thread is resolved; reopen it first." Not live → deny: "Thread is asleep: call wake_thread first." Renamed or ambiguous → deny, saying which. Everything else → deny ("… is not a thread of this project", "… is not the address of a live thread of this project") |
| thread | `SendMessage` | allow only if `to` is exactly `chief-<slug>` or exactly the coordinator's socket address, and the coordinator is reachable. Coordinator not running (or never started) → deny: "Your coordinator chief-<slug> isn't running, so the message can't be delivered. Just finish your turn: your final message is recorded as your report when you stop anyway, and the coordinator reads it there." Renamed or ambiguous → deny with the same advice. Everything else → deny ("Threads may message only their coordinator, by its exact name chief-<slug>."): siblings, other projects, the thread's own subagents, and any session the local `claude agents` doesn't list |

An allow-list by byte-exact equality. `to` is never trimmed, case-folded or parsed, and a trailing
` [ref]` is not removed: Claude Code's refs (`[2c7859]`, a short hex id) pick one session, so a
ref could deliver a message that the bare name would allow to another session. A denied `to` that
contains `[` gets "Use the bare name, without a `[ref]`: chief matches names exactly, and a ref can
pick another session." before the reason. Lookalikes (another case, U+FEFF, U+200B, a tab before
the ref) and sessions the local `claude agents` doesn't list (other machines, cloud sessions,
Remote Control, teammates, subagents) match nothing and are denied.

Reachable (the identity check, for a thread under its name and for the coordinator under
`chief-<slug>`): the snapshot entry for the row's `session_id` (its `short_id` while the
`session_id` is unresolved; never an entry found by name) has a pid and carries exactly that name,
and no other snapshot entry, in any state, carries that name. Otherwise it is asleep (no entry, or
no pid), renamed (the entry carries another name, so the name may reach someone else), or
ambiguous (another entry carries the name too). A coordinator with no row counts as not running.

Socket addresses are accepted only in the exact form Claude Code gives a session's socket (§1, the
`from` of every cross-session message): `uds:/tmp/cc-socks/<pid>.sock`, the pid in decimal with no
sign or leading zeros, nothing before or after. The guard builds that string from the reachable
session's pid and compares bytes, so another folder, a bare `uds:<pid>.sock`, `0201` or a ref
after it is denied. If Claude Code ever moves its sockets, replies by address are denied (fail
closed) and the sessions fall back to names.

`to` is read from `tool_input.to`; a SendMessage whose `tool_input` isn't an object, or has no
string `to` or a blank one, is denied. Other tools are allowed. `claude agents` is only run for a
SendMessage with a `to`.

The guard process itself exits 0 (deny JSON or empty stdout), except on arguments it can't read,
where it exits 2 with the reason on stderr (§3); its generated command turns any other exit,
including a chief binary that can't start, into exit 2 (§3), which blocks the call.

Time budget. When Claude Code's hook timeout fires it cancels the command and the call proceeds
(the `|| exit 2` never runs), so the guard must answer first. Its entry sets `"timeout": 30`
explicitly (§3). Inside, `claude agents` is bounded at 10 s (a real bound, §7), and a watchdog
denies at 20 s whatever the guard is still doing: "chief's message guard didn't decide within 20s,
so the message is blocked. Try again; if it keeps happening, check <hook error log>." That covers
SQLite busy waits, which are bounded only per statement (5 s, §4; the guard opens the database,
which runs the migration check, reads the rows, and inserts at most one denial). The watchdog's
denial is logged to the hook error log, not to `guard_denials`: the database may be what is stuck.
`CHIEF_GUARD_DEADLINE_MS` shortens the deadline for tests (it can only deny sooner).

Every deny writes a `guard_denials` row (caller matched by `session_id`, `target` = raw `to`) and
returns the PreToolUse deny JSON above; an allow prints nothing. If the guard itself fails (bad
payload, DB or `claude agents` error, or a panic), it denies with the error as the reason (fail
closed), logs the error, and records the denial if the DB opens.

## 7. Coordinator MCP tools (`chief mcp --project <slug>`)

All tools return JSON text content. Errors are tool errors with a plain message.

| Tool | Input | Does |
| --- | --- | --- |
| `start_thread` | `title` (string), `task` (string), `repo?` (repos[].name or absolute path), `model?`, `effort?` | Checks trust of the repo path, picks a unique name, rewrites `generated/*`, inserts the row, spawns the thread, resolves its ids. Returns `{name, short_id, repo, model, effort, next}`, plus `warning` when it adopted a session whose spawn reported a failure |
| `list_threads` | `include_resolved?` (bool, default false) | Refreshes from `claude agents` (stored rows plus a `warning` if that fails), returns `{threads: [...]}` in board order: `name, title, group, waiting_reason, asleep, branch, pr_url, last_report_preview (≤300 chars), last_activity` ("5m ago"), `cleaned`. Resolved and cleaned threads are left out unless `include_resolved` |
| `read_report` | `name`, `history?` (int, default 1, max 20) | `{name, title, group, reports: [{at, text}]}`, newest first, full text |
| `wake_thread` | `name` | If no live pid: `claude respawn <short_id>`, poll `claude agents` up to 20 s for a pid. Returns `{name, alive, respawned}`. A cleaned thread is a tool error: "<name> was cleaned up; start a new thread (its reports are still readable with read_report)" |
| `resolve_thread` | `name`, `reopen?` (bool) | Sets or clears `resolved` (`coordinator`). Returns `{name, resolved, group}`. Reopening a cleaned thread is the same tool error as `wake_thread`'s (`registry::set_resolved` refuses it for every caller) |

There is no cleanup tool: removing a worktree and branch is the user's call (§12).

Names may carry a trailing ` [ref]`; it is ignored. Only this project's threads are accepted.

`start_thread` details: `repo` falls back to `threads.default_repo` (error if neither); an absolute
path must be a folder and maps back to a repos[] name when it is one. Trust is checked before
anything else and an untrusted repo returns the `NotTrusted` message. The name must not be in the
registry nor among `claude agents` names (a few suffixes are tried; if `claude agents` fails,
nothing starts). A task that starts with `-` is sent as `Task: <task>` so the CLI can't read it as
a flag. If the spawn fails, the session may have started anyway (a timeout, an output without
the `backgrounded` line), and a deleted row would leave it working unseen and unreachable through
the guard. So unless the error is `NotTrusted`, chief first looks for it in `claude agents` (polled
up to the 5 s resolve timeout, each call bounded): a background session (it has an `id`) named
exactly the new name that the pre-spawn snapshot didn't list is adopted (its short id, session id
and pid are stored) and the call succeeds with a `warning` that quotes the failure. When listings
succeed without it, the row is deleted and the error returned. When every listing fails, chief
can't tell: the row is kept without a short id (hooks attach it by title if the session did start)
and the error says so. Failing to resolve the session id is not an error (hooks match by title,
sync by short id). `next` is
a literal SendMessage input with no `message` key, `{"to": "<name>", "notify_when_idle": true}`
(verified: omitting `message` is a valid pure subscription): the coordinator subscribes right after
starting a thread, which is how it hears the thread finished. (M5 live run: told "with no message",
and again told to pass `""`, Haiku emitted `"message": ,`, which is invalid JSON.)

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

Every external command chief waits on (`claude`, `git`, `gh`, `rex`) runs through
`proc::output_with_timeout`, and its timeout is a real bound: the child leads its own process
group, and the call must see the child exit *and* both pipes close before the deadline. Otherwise
the whole group gets SIGKILL and the call fails as a timeout, without waiting for readers still
stuck on a pipe (a grandchild such as `sh -c 'sleep 60 & exit 0'` would hold it open).

Trust (`claude::is_trusted`) walks up both the path as given and its canonical form (macOS `/tmp`
is `/private/tmp`), stopping at the first folder that holds `.git`: any trusted folder on the way
counts. It reads `$CHIEF_CLAUDE_CONFIG` when set, else `$CLAUDE_CONFIG_DIR/.claude.json` when that
is set (§1), else `~/.claude.json`.

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

Its tools: everything but Bash (disallowed) and ListAgents (the guard, §6). It reads the
repositories (Read, Grep, Glob; they are `--add-dir`s) but can't edit them: `permissions.deny`
blocks Edit and Write in every repo and in `generated/` (§3), so it can edit only its own folder
outside `generated/` (memory, `instructions.md`, `project.toml`). `SendMessage` goes through the
guard; its chief MCP tools run without prompts.

No prompt: it waits for you. Started when it has no row or no `short_id` (the project folder must be
trusted, and no other live session may already use the name). A failed spawn is handled as a
thread's is (above): a session that started anyway is adopted, with a warning on stderr; a row it
inserted is removed only when listings show no such session, and kept when chief can't tell. A
kept row (no `short_id`) is adopted by the next `up` when `claude agents` lists its session (by
`session_id` if a hook stored one, else a background session with exactly the name), instead of
refusing the name as taken. If it exists but `claude agents` shows no pid, `claude respawn` it and wait up to 10 s for
a pid. Otherwise it is left alone.

## 8. Prompts (files in `src/prompts/`, included with `include_str!`)

- `coordinator.md`: role (route, never do the work), its tools (§7: read the repos, edit only
  memory and instructions, no shell), routing rules (quick question → answer;
  new work → `start_thread`; follow-up → `SendMessage` to the matching thread with
  `notify_when_idle`, waking it first if asleep, by its exact name with no `[ref]`; several tasks →
  several threads), say which thread got what, keep `max_parallel`, save preferences and decisions
  to `memory/` and update `memory/MEMORY.md`, read full reports with `read_report` when an idle
  notice arrives. Removing worktrees and branches is the user's call (board `x`, `chief clean`,
  `chief gc`), so it never offers to; it names no branch in a thread's brief unless the user did
  (threads create their own).
- `thread.md`: you are one thread of project <name>; read `<dir>/memory/MEMORY.md` first; project
  instructions follow; work on your own branch; message only the coordinator, by its exact name
  (the report needs no message); end every turn with a short report (what changed, branch, PR URL
  if any, what you need); if something is missing, say exactly what and stop.

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

## 9. Layout (`chief up`: Rex, tmux, or the user's own)

The layout is two panes side by side: `chief pane` on the left, `chief board` on the right.
Nothing in it depends on a multiplexer: the board switches the pane by signalling it (below), so
any two terminals or splits work. `chief up` opens it for the user with the first that applies:

1. Rex, when `rex list --json` succeeds (installed, server running): the Rex session below.
2. tmux, when `$TMUX` is set (chief runs inside tmux): a tmux window, below.
3. Neither: it prints why and the two commands, `chief pane --project <slug>` and
   `chief board --project <slug>`, to run side by side (two windows, iTerm or Ghostty splits,
   anything). `chief down` has nothing to close then.

`src/mux/mod.rs` defines a backend-neutral trait (block ids are unique across sessions):

```rust
pub trait Mux {
    fn has_session(&self, s: &SessionRef) -> Result<bool>;                // by id, never by label; Err = not installed / not running
    fn create_session(&self, label: &str, first: &BlockSpec) -> Result<(SessionRef, BlockRef)>;
    fn run_block(&self, s: &SessionRef, spec: &BlockSpec) -> Result<BlockRef>; // split, ratio, label, cwd, cmd, focus
    fn blocks(&self, s: &SessionRef) -> Result<Vec<BlockInfo>>;            // id + label
    fn is_running(&self, b: &BlockRef) -> Result<bool>;                    // a kept-open block outlives its program
    fn close_block(&self, b: &BlockRef) -> Result<()>;
    fn focus(&self, b: &BlockRef) -> Result<()>;
    fn show(&self, s: &SessionRef) -> Result<bool>;                        // false: can't bring it into view
    fn kill_session(&self, s: &SessionRef) -> Result<()>;
    fn find_block(&self, s: &SessionRef, label: &str) -> Result<Option<BlockRef>>; // provided, via blocks
}
```

`src/mux/rex.rs` implements it with the `rex` CLI (`CHIEF_REX_BIN` overrides the binary), always
with `--autostart=false` (no server means no layout, never a server chief started) and a 15 s
timeout: `rex list --json`; `rex new <label> --cwd <dir> --keep-open --json -- <cmd>` then
`rex block rename -s <s> <block> <label>` (`new` labels its first block "shell"); `rex run -s <s>
--label <l> --focus=<bool> [-b <anchor> --split <dir> --ratio <n>] --cwd <dir> --keep-open --json
-- <cmd>`; `rex block ls -s <s> --json`; `rex block call -b <b> process` (running while it lists a
`child`); `rex block close <b>`; `rex focus <b>`; `rex kill <s>`; and
`rex do session.select session_id=<s>` for `show` (a client action: Rex picks the app when exactly
one is connected, and fails otherwise). chief only touches the session `chief up` created, by the
id it stored in `ui_state.rex_session` (migration 8): a session that merely carries the label
`chief-<slug>` may be the user's, and is never reused or killed.

`src/mux/tmux.rs` implements it with the `tmux` CLI (`CHIEF_TMUX_BIN` overrides the binary). A
"session" is one tmux window and a block one of its panes. Every command names the server with
`-S <socket>` (and runs without `TMUX`/`TMUX_PANE`, so nothing else picks the target), with a 15 s
timeout. `up` takes the socket from `$TMUX` (`<socket>,<pid>,<session>`); the stored id is
`<window id> <server pid> <socket>`, so `down` finds the server from any terminal, and a window id
from an earlier server (ids restart with each server) is never taken for chief's: `has_session`
runs `list-windows -a -F '#{pid} #{window_id}'` and needs that exact pair, on the same socket (no
server there: false, not an error). `create_session`: `new-window -n chief-<slug> [-a -t
$TMUX_PANE] [-d] -P -F '#{pane_id} #{window_id} #{pid}' -c <dir> -- <cmd>` (the window opens right
after the user's, in the current session). `run_block`: `split-window -h|-v [-b] -l <ratio>% -t
<anchor> [-d] -P -F '#{pane_id}' -c <dir> -- <cmd>` (`-b` for left or above; without an anchor it
splits the window). Each new pane gets `set-option -p @chief-block <label>` (a label programs
can't change, unlike the pane title) and `remain-on-exit on` (kept open, like Rex's
`--keep-open`). `blocks`: `list-panes -t <window> -F '#{pane_id} #{@chief-block}'`;
`is_running`: `display-message -p -t <pane> '#{pane_dead}'` is `0`; `close_block`: `kill-pane`;
`focus`: `select-pane`; `show`: `select-window` (true when it succeeds); `kill_session`:
`kill-window`.

`src/layout.rs` builds the layout over any `Mux`: session `chief-<slug>`, block `main` running
`chief pane --project <slug>`, block `board` split right of it at 32 % running
`chief board --project <slug>`, both with the project folder as cwd. Each command is the canonical
chief path, behind `env K=V…` with the §3 passthrough variables when any is set. `open`:

1. No stored session, or the stored id is gone: create `chief-<slug>` with `main`, add `board`
   with `--focus=false`. `up` stores the new id.
2. Both blocks there and running: reuse.
3. One running: close the other if it is there but ended, and run it again beside the running one
   (`board` right of `main` at 32 %, or `main` left of `board` at 68 %).
4. Neither running: run `main` in a new window of the same session first, then close the dead
   `main`/`board` blocks (closing the last block first could end the session), then add `board`
   beside the new `main`. The session may hold the user's blocks, so `open` never kills it.

`close` (for `chief down`): nothing stored or the id gone → nothing to close. A session holding
only `main` and `board` is killed and the stored id cleared; one that holds other blocks too (the
user added them) keeps running with those, and only `main` and `board` are closed.

Then focus `main` and `show` the session. When Rex's `show` returns false, `up` prints
`rex -C <app> do session.select session_id=<id>` and says to pick the session in the app.

`chief pane` loop (`src/pane.rs`). On start it catches its signals (below), then records its pid
and start time in `ui_state.pane_pid` / `pane_started` (`proc::start_time`: `proc_pidinfo` on
macOS, `/proc/<pid>/stat` on Linux); on exit it clears them, only while they are still its own.

1. Target: `ui_state.pane_target`; NULL, or a session that is unknown, of another project, cleaned
   (removed counts) or never started (no short id), means the coordinator, and a stale target is
   cleared (only while it still holds that name, so a newer board write survives). No coordinator
   with a short id yet: print `chief · no coordinator yet: run chief up <slug>` and look again
   every 500 ms.
2. Print `chief · <name> …` on a cleared screen. Sync `claude agents`; with no pid, `claude
   respawn <short_id>` and poll for a pid up to 20 s (`chief · waking <name> …`). A failing
   `claude agents` skips this step; a failing respawn is shown and retried after 500 ms.
3. Write `pane_showing = <name>`, then read the target again; if it changed, clear
   `pane_showing` and start over (see board switching below).
4. Drain queued keys (`q` quits), restore the terminal mode the pane started in but with ISIG off,
   run `claude attach <short_id>` as a child on the same TTY, wait (polling `waitpid(WNOHANG)`
   every 50 ms, so a signal flag set at any moment is seen), take raw mode back, clear
   `pane_showing`.
5. An attach that failed, exited non-zero, or ended within 1 s prints why below its own output
   (`… · retrying · q quits`) and waits 500 ms, reading keys. Then back to 1. An attach ended for a
   switch goes straight back to 1.

`q` while chief has the terminal ends the loop; so does the block being closed or the session
killed. Then the pty hangs up and the kernel sends SIGHUP to the session leader, chief, not to
`claude attach`. chief catches SIGHUP (and SIGTERM) with a handler that only sets a flag, without
`SA_RESTART`, so the default action can't end it before cleanup: the wait sees the flag, passes
SIGHUP on to attach, gives it 2 s to exit, then SIGKILLs it, and reaps it either way; the loop
then stops. On exit `pane_showing` is cleared (a stale one would keep §12 cleanup away from that
thread).

Ctrl+Z: the user can detach by hand with Claude's own Ctrl+Z inside `claude attach`; the loop then
attaches to the current target. While chief itself has the terminal, the terminal is in raw mode
(ISIG off), so Ctrl+Z, Ctrl+C and other keys are plain bytes chief reads and drops (only `q`
counts). Attach starts with ISIG still off, so a Ctrl+Z that arrives before Claude's UI is up
stays queued as input; Claude reads it once it takes the terminal and detaches. In case a SIGTSTP
gets through anyway, chief ignores it and spawns attach with SIGTSTP reset to default (an ignored
signal would survive exec); it waits with `WUNTRACED` and sends SIGCONT to a stopped attach, so the
loop can't hang. On exit chief restores the terminal mode it started with. Also, Rex runs the
command as the leader of its own session, an orphaned process group, so the kernel discards
terminal stop signals there anyway (verified in M4: with ISIG on, `^Z` echoes and nothing stops).

Board switching works in any terminal, with or without a multiplexer: write `pane_target`, then
`pane::request_switch(conn, slug)`. When `pane_showing` is NULL (between attaches, or no pane) or
equals the target (NULL target = the coordinator's name), or no pane is recorded, do nothing.
Otherwise check that the recorded pid's start time still matches `pane_started`; if not, the pane
is gone and the pid may be another process's by now, so clear the record (only while it holds that
pid) and send nothing. Else send it SIGUSR1. The pane catches SIGUSR1 like SIGHUP (a handler that
only sets a flag, no `SA_RESTART`), before it records its pid, since the default action would end
it. When the wait sees the flag it clears it and reads the target again: a target it already shows
(the board saw the pane just before it moved on) is ignored. Otherwise it sends SIGTERM to `claude
attach`, gives it 2 s to exit, then SIGKILLs it, and reaps it. SIGTERM ends the attach client and
leaves the background session running (verified on Claude Code 2.1.310: still listed, idle, with
its pid). A killed attach leaves its screen behind, so the pane then restores the terminal mode it
started with and writes a reset: leave the alternate screen (`ESC[?1049l`), show the cursor
(`ESC[?25h`), turn off mouse reporting (`ESC[?1000l ESC[?1002l ESC[?1003l ESC[?1006l`), bracketed
paste (`ESC[?2004l`) and focus events (`ESC[?1004l`), reset attributes (`ESC[0m`), clear
(`ESC[2J ESC[H`); then it takes raw mode back, clears `pane_showing` and attaches to the new
target. The pane writes `pane_showing` before its last read of the target and the board writes the
target before reading `pane_showing`, so a switch that sends nothing is always seen by the pane's
next read; a SIGUSR1 that lands before attach starts leaves the flag set, and the first look of the
wait ends that attach at once. One pane per project receives switches: the latest one started.

## 10. Board

`chief board --project <slug>` (`src/board/`): `app.rs` holds the state and turns keys into actions
(`App::on_key(KeyEvent) -> Vec<Action>`), `ui.rs` draws it, `refresh.rs` polls in the background,
`jobs.rs` runs actions on worker threads, `settings.rs` is the settings form. Logic and rendering
are tested without a terminal (`TestBackend`).

- Refresh: two background threads, each with its own SQLite connection, send results to the UI over
  a channel; the UI never waits on them. Every 2 s: take `taken_at`, run
  `claude agents --json --all`, sync (§5), read the project's rows, guard denial count and
  `pane_target`. A failing `claude agents` still sends the stored rows, with "refresh failed: …" in
  the status line; a failing database read keeps the last rows. After each action the worker asks
  for an immediate re-read (database only). Every 30 s (first at start): `pr::refresh_project` with
  one `Gh` kept for the board's lifetime and `project.toml` re-read each time (§12), which runs
  `gh pr list --head <branch> --state all --json
  url,state,isDraft,reviewDecision,createdAt,isCrossRepository --limit 10` in `repo_path` and,
  with `cleanup = "merged"`, cleans up merged threads. Its one-time `hint`
  (no `gh`, not logged in, no GitHub remote) stays in the status line; blocked and error counts
  prefix it (`PR 1 blocked, 2 errors · …`). At start the board deletes `events` older than 30 days
  (`db::prune_events`).
- Layout, designed for a 40–70 column pane and usable down to 30: header (project name, goal,
  `coordinator: <state>` with the guard denial count when > 0); the groups in §5 order with counts,
  empty groups hidden, "No threads yet" when there are none, Resolved collapsed by default
  (`Resolved N · R shows`). When the list doesn't fit, its last line says what is below
  (`↓ N more`, plus `· Resolved N · R shows` for a collapsed header under the fold; `↑ more above` at
  the bottom). A row is `<target mark><glyph> <title or name>  <tags>`, cut to the
  width with an ellipsis and never wrapped; the title gives way first so tags stay readable. Tags:
  waiting reason, `asleep`, PR state (`PR draft|open|approved|changes|merged|closed`),
  `cleanup blocked`, `cleaned up`. Detail panel for the selection (title, name, group and reason or
  resolved reason, asleep, branch, PR URL, model · effort, last activity, cleanup state, the last
  report wrapped): below the list when the body has ≥ 16 lines (the list takes what it needs up to
  3/5), otherwise Tab swaps the list for it. One status line: a running worker, else the last
  action's result (cleared by the next key), else refresh errors, else the PR hint, else key help.
  Colours: a fixed palette that reads on dark and light terminals (Waiting bold red with `!`,
  Working green `●`, Review blue `◆`, Landing magenta `▲`, Idle default `○`, Failed red `✗`,
  Resolved dim `✓`). `▸` marks what the main pane shows: a thread row, or the coordinator line.
- Keys: ↑/↓ or j/k move over threads (headers skipped; g/G first/last); Enter shows the selected
  thread in the main pane (refused for a cleaned one); Esc or c shows the coordinator; w wake
  (`tools::wake_thread`, refused for cleaned); r resolve or reopen (`registry::set_resolved`,
  reason `user`; reopening a cleaned thread is refused with its message); o open the PR URL
  (`open` on macOS, `xdg-open` elsewhere, output discarded); x clean up, only for a resolved,
  uncleaned thread: `cleanup::clean(Safe)`. When blocked, a fresh `cleanup::inspect` decides the
  modal: if it lists the loss, "Discard unsaved work?" shows why plus `cleanup::render_loss` and
  runs `clean(Discard(<that inspection>))` only after the thread's name is typed exactly (Enter
  with a wrong name clears the input; Esc cancels); if it lists nothing or fails (`claude rm`
  refused over work chief can't see), "Cleanup blocked" says `Cleanup blocked: <why>.` and why no
  discard is offered, and any key closes it; R show or hide resolved; Tab detail on
  short terminals; s settings; ? help (any key closes); q or Ctrl+C quit the board only. Wake and
  cleanup run one at a time on a worker thread ("waking …" in the status line).
- Showing a thread: write `ui_state.pane_target` (NULL for the coordinator), then call
  `pane::request_switch`, which signals the running `chief pane` unless it already shows the
  target or no pane runs (§9).
- Settings screen (`s`) edits a draft of `project.toml`: goal, coordinator model and effort, thread
  model, effort, permission mode, max_parallel, default repo, cleanup (merged/manual),
  `ui.notify`, and the repos (add by path, named after its folder with `-2`, `-3` … on a clash, as
  in `init`; `d` removes one, moving the default repo to the first remaining). Efforts, modes,
  default repo, cleanup and notify cycle with ←/→ (Enter too); models, goal and max_parallel are
  free text (Enter edits, Enter applies, Esc cancels). Save validates (`ProjectConfig::validate`,
  errors shown in the form), writes with `save`, regenerates `generated/*`, and says "Saved. New
  threads use these settings; the coordinator's model and effort apply on its next start." Esc
  with unsaved changes asks for a second Esc. Two entries open `instructions.md` and
  `memory/MEMORY.md` in `$EDITOR` (run through `sh -c`, so arguments work; fallback `vi`): the
  board leaves the alternate screen and raw mode, runs the editor on the same TTY, and restores.
- Terminal: `ratatui::init` (alternate screen, raw mode, a panic hook that restores the terminal);
  restored on quit and on error; input is polled every 100 ms, results are drained between frames,
  and resizes take effect on the next frame.
- A thread with `cleanup_blocked` shows a "cleanup blocked" tag (the reason in the detail); a
  cleaned thread shows "cleaned up" in Resolved. Wake and reopen are refused for a cleaned thread.

## 11. Testing

- Unit tests: §5 derivation (table-driven), §6 guard (table-driven), hook payload parsing against
  `tests/fixtures/hooks/spike-events.jsonl`, `claude agents` parsing against
  `tests/fixtures/agents/agents-sample.json`, config round trip, migrations on a fresh DB.
- `CHIEF_CLAUDE_BIN` overrides the `claude` binary; integration tests use a fake script
  (`tests/fake-claude.sh`) that records its arguments and prints canned output. `CHIEF_GH_BIN`
  overrides `gh` the same way (the same script, linked as `gh`), and `CHIEF_REX_BIN` overrides
  `rex` (linked as `rex`; the test home's default fails like a Rex with no server, so no test
  reaches the real one). `CHIEF_TMUX_BIN` overrides `tmux` (linked as `tmux`; reached only when a
  test sets `TMUX`). Layout logic is also tested against an in-memory `Mux`, and the tmux backend
  against a real tmux server on a private socket (`-L chief-test-…`, no config) that the test kills.
  The switch is tested with `chief pane` on a pseudo-terminal and a fake `attach` that sleeps. `claude rm` outputs from the live
  M2b probe are in `tests/fixtures/rm/`. Cleanup tests use real temporary git repositories (a bare
  remote, a clone, worktrees under `.claude/worktrees/`); git is the only real tool tests run. `CHIEF_HOME` points
  tests at a temp dir, `CHIEF_CLAUDE_CONFIG` replaces `~/.claude.json`, `CHIEF_CLAUDE_DIR` replaces
  Claude Code's config folder (job state; checked before `CLAUDE_CONFIG_DIR`, never written into
  generated files), `CLAUDE_CONFIG_DIR` is removed from the tests' environment, and
  `CHIEF_NO_NOTIFY=1` turns off notifications.
- `scripts/e2e.sh` runs the live loop with cheap sessions (milestone 2b).

## 12. Cleanup (`src/cleanup.rs`, `src/pr.rs`)

Resolving a thread keeps its session, worktree and branch. Cleanup removes them with
`claude rm <short_id>` (§1), which deletes the worktree and the local branch and keeps any remote
branch. The row stays, with its reports: `cleaned_at` is set and `cleanup_blocked` cleared.

The worktree is the one `claude rm` itself deletes: the session's job state,
`<claude dir>/jobs/<short_id>/state.json` → `worktreePath`, where `<claude dir>` is
`$CHIEF_CLAUDE_DIR` (tests), else `$CLAUDE_CONFIG_DIR` (as Claude Code reads it), else
`~/.claude`. It counts only as a `<repo>/.claude/worktrees/<name>` folder, and while it exists
`git -C <wt> rev-parse --show-toplevel` must be that folder (a leftover plain folder would make git
answer for the checkout around it). Never the hooks' `cwd`, which follows the thread's shell
anywhere, another thread's worktree included, and never a name derived from the thread's. No state
file or no `worktreePath` means the thread has no worktree of its own: nothing to lose, and nothing
is reset or cleaned.

`inspect(row)` (all git calls through `proc::output_with_timeout`, 10 s): whether the worktree
exists, its branch (`git branch --show-current`, else `sessions.branch`),
`git status --porcelain --untracked-files=normal` lines (ignored files aren't listed, as Claude
Code ignores them), and unpushed commits as Claude Code means them:
`git rev-list --count HEAD refs/heads/<branch> --not --remotes`, listed with
`git log --format='%h %s' -n20 …`, and the head they end at (`git rev-parse HEAD`). When the
worktree is gone, commits are checked in `repo_path` if the branch still exists there, and the
head is the branch's sha.

`clean(ctx, name, mode)`:

0. Claim the thread (`db::claim_cleanup`, one `BEGIN IMMEDIATE` transaction on
   `sessions.cleaning_at`, migration 7): already cleaned → `AlreadyCleaned`; claimed by another
   process less than 10 minutes ago → `InProgress`, nothing run (the board's refresh skips it,
   `chief gc` says `skipped  another chief process is cleaning it up`, the board's `x` and
   `chief clean` say so too); an older claim was left by a process that died and is taken over.
   Every path below releases the claim (`db::release_cleanup`, only if it is still this one's).
1. Refresh from `claude agents` and sync (§5); if that finds the session removed with its
   worktree, it is already clean. Removed outside chief with the worktree still there
   (`removed_at` set, not cleaned): no `claude rm` (it has no session to remove); cleaned when the
   job state's worktree is gone, otherwise blocked with `session removed outside chief; worktree
   remains at <path>` (the folder is the user's to delete).
2. Refuse (error, nothing run) unless the thread is resolved and, derived as if it weren't
   resolved, neither working nor waiting, and isn't the session `chief pane` shows
   (`ui_state.pane_showing`: the user may be typing into it, and hooks fire only on submit):
   `claude rm` stops the session even when it refuses.
3. Never spawned (no `short_id`): mark cleaned without `claude rm`.
4. `Safe`: if `inspect` finds uncommitted changes or unpushed commits, or fails, store the reason
   in `cleanup_blocked` (`2 unpushed commits`, `uncommitted changes: 3 files`, both joined with
   `, `) and stop without calling `claude rm`.
   `Discard(confirmed)` carries the inspection the user was shown and confirmed. It is refused
   (stored in `cleanup_blocked`, nothing run) when a fresh `inspect` differs from it ("the
   worktree changed after you confirmed …") or when the job state of another session registered
   in this chief home names the same worktree; otherwise in a dirty worktree run
   `git -C <wt> reset --hard -q` and `git -C <wt> clean -fdq` (never `-x`: ignored files stay),
   in step 5.
5. Steps 1 and 2 again, with a fresh `claude agents`, right before anything is reset or
   stopped (the thread may have got a message, or the pane attached to it, meanwhile); then the
   `Discard` reset from step 4.
6. `claude rm <short_id>`. `Discard` retries an unpushed refusal once with
   `--discard-unpushed <token>` only when the confirmed inspection itself listed unpushed commits
   and the token's head sha (`<sha>@<worktree-id>`) equals the head it saw (`git rev-parse HEAD`
   in the worktree, else the branch's sha). Otherwise `claude rm` counts work nobody was shown,
   and no destructive flag is ever passed. Removed → cleaned. Unpushed → `N unpushed commits`;
   dirty → `uncommitted changes`; anything else → `claude rm: <output>`, all stored in
   `cleanup_blocked`. A row that is cleaned by then (a sync found it removed) keeps its
   `cleaned_at` and never gets a `cleanup_blocked`.

`claude rm` output (`claude::parse_rm`): a `removed <id>` line is removed (exit status alone isn't:
a stale token printed usage and exited 0); `kept <id> — …` with a `--discard-unpushed <hex>@<hex>`
line is unpushed (its `unpushed commit` summary lines kept); `kept` with "uncommitted changes" is
dirty; anything else is `Other` with the text.

PR status (`pr.rs`): `Gh::poll(repo_path, branch, since)` runs the §10 `gh pr list` (10 s) and
takes the thread's PR: the newest listed one that isn't cross-repository (a fork's branch can share
the name) and whose `createdAt` is no earlier than the thread row's `created_at` minus 5 minutes of
clock skew. Worktree branches are named after threads, so a new thread can reuse the branch of an
old one whose PR merged long ago; that PR must not resolve and clean the new thread. A PR without a
readable `createdAt` is ignored. A missing `gh`,
a `gh` that isn't logged in, or a repository without a GitHub remote is no PR plus a hint returned
once per `Gh`; other failures are errors. `poll_project` polls threads that work in a worktree,
have a branch and aren't cleaned: every unresolved one, and resolved ones whose PR isn't merged or
closed and that were active in the last 7 days. It stores results with `registry::set_pr`, which
auto-resolves merged ones. `refresh_project` (the board, every 30 s) adds, with
`cleanup = "merged"`, `clean(Safe)` for every resolved, uncleaned thread with a merged PR that is
neither working nor waiting nor showing in the pane and has no `cleanup_blocked`: a blocked
thread waits for the user, a
working one is tried again on the next refresh. `cleanup = "manual"` never cleans on its own.

`chief gc [<slug>] [--dry-run]` per project: `poll_project`, sync `claude agents`, then
`clean(Safe)` on every resolved, uncleaned thread (dry: what it would do, from `inspect`). It prints
`cleaned` / `would clean` / `blocked <reason>` / `skipped <why>` (working or waiting, or an error)
per thread, or `nothing to clean`; warnings go to stderr. `--dry-run` still stores PR status, as
the board would.

`chief clean <slug> <thread>` runs `clean(Safe)` and fails with the reason when blocked, plus the
`--discard` command when `inspect` lists the loss, or else that chief can't list that work and
won't discard it. With `--discard` it checks step 2 first, prints what would be deleted and lost
(`git status --short` lines, the unpushed commits), and, when something would be lost, asks
`Type the thread name to confirm:` on a terminal; without one it refuses unless `--yes`. It then
runs `clean(Discard(<that inspection>))`, so "Nothing unsaved will be lost" never discards a
commit `claude rm` reports.

A cleaned thread can't be woken, reopened or messaged (§6, §7) and is hidden from `list_threads`
unless `include_resolved`. The coordinator has no cleanup tool; its prompt says cleanup is the
user's call (§8).
