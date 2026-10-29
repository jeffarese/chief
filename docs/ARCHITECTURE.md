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
  (checked on this machine with two account folders). So each chief project belongs to one
  account and every call for it runs there (§3).
- Verified live with agp (agent-prism, the user's account switcher) on 2.1.310: an agp account
  is a Claude Code config folder. `agp exec <id> -- <cmd>` runs `<cmd>` with that account's
  `CLAUDE_CONFIG_DIR` (unset for the default account, `~/.claude`), and `agp exec <id> --
  claude --bg …` started a background session in that account. `claude agents` lists only the
  calling account's sessions.
- Across accounts, `SendMessage` by name fails both ways ("No agent named '<x>' is reachable.").
  By socket address, `uds:/tmp/cc-socks/<pid>.sock`, it works both ways: `/tmp/cc-socks` is
  shared by every account, and the receiver sees `from="uds:/tmp/cc-socks/<pid>.sock"
  from-name="<name>"`. `notify_when_idle` by socket address works across accounts too: the
  subscription's reply warns that "whether it supports idle notices is unknown", but the notice
  ("uds:/tmp/cc-socks/<pid>.sock … is idle now") arrives at the subscriber's next turn boundary
  (idle notices are delivered between turns; a 30 s wait inside one turn saw nothing).
- `agp status --json` → `views[]`: `profile_id`, `label`, `in_pool`, `limited` (null or an
  object), `score` (a float; null outside the pool), `usage.five_hour.utilization` and
  `usage.seven_day.utilization` (percent; `seven_day` may be null), `projection`;
  `identity.tool` is `claude` for Claude accounts. `agp pick --json` ignores `--accounts` and
  `--exclude` (it always returned the same account), so chief ranks accounts itself (§7).
  Sample in `tests/fixtures/agp/status.json`.
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
- Subagents, verified live on 2.1.310 in a `claude --bg` session with a hook logging every
  payload (samples in `tests/fixtures/hooks/teammate-events.jsonl`):
  - In a background session the Agent tool spawns an in-process "teammate": PostToolUse for
    `Agent` has `tool_input.name` (`"helper"`) and `tool_response` `{status: "teammate_spawned",
    agentId: "ahelper-886b99f57d4badec", name, teammate_id, …}`.
  - `SubagentStart` fires at spawn, before that PostToolUse, with the lead's `session_id`,
    `agent_id` (`"ahelper-886b99f57d4badec"`) and `agent_type` (`"helper"`: the name, because one
    was given; for an unnamed agent it is presumably the agent type, so `agent_type` is never an
    address). It fires again each time a teammate is resumed. `SubagentStop` fires after every
    teammate turn while the teammate stays registered (the Stop hook's `background_tasks` lists
    it as a running `teammate`), so SubagentStop does not mean gone.
  - A teammate's own tool calls fire PreToolUse/PostToolUse with the lead's `session_id` plus
    `agent_id` and `agent_type`, set by Claude Code, not the model. Teammates report with
    `SendMessage(to: "team-lead")`; the SendMessage tool doc also names `"main"` ("the main
    conversation, background subagents only").
  - Lead → teammate: `to: "helper"` and `to: "ahelper-886b99f57d4badec"` both reach it.
  - In-process names win: with a teammate named `spike-other` and a live outside session also
    named `spike-other`, `to: "spike-other"` reached the teammate and the outside session got
    nothing. Same after `TaskStop` of the teammate ("Teammate "spike-other" was not running;
    resumed it as an in-process teammate with 27 prior messages…"). The tool doc says the bare
    in-process name always wins, and when two in-process agents share a name the latest wins.
  - After `claude stop` + `claude respawn` (SessionStart `source: "resume"`, a new process) the
    teammates are gone: `to: "helper"` → "No agent named 'helper' is reachable."; `to:
    "spike-other"` now resolved to the outside session ("'spike-other' needs a one-time confirm
    before this send. Re-send with the ref…"). So a teammate is addressable only within the
    process that spawned it, and a stale name can point outside.
- Background work, verified live on 2.1.310 (samples in
  `tests/fixtures/hooks/subagent-wait-events.jsonl` and `async-wait-events.jsonl`):
  - The Agent tool in a background session starts one of two kinds of subagent, depending on the
    machine's Claude Code configuration (same version: one Mac got teammates, another async
    agents). An in-process **teammate** (`tool_response.status: "teammate_spawned"`, above)
    stays registered between turns. An **async agent** (`status: "async_launched"`, `isAsync:
    true`) runs once and ends with its `SubagentHandback` tool call, which reaches the lead as
    UserPromptSubmit `<agent-message from="<name>"> [Subagent hand-back] …`, then SubagentStop,
    then the lead's turn and Stop.
  - A lead that ends its turn while a **teammate** works is `status: idle` in `claude agents`, and
    a `notify_when_idle` subscriber gets its idle notice right then. Subscribing again to a
    session that is already idle notifies at once, quoting the same turn, so a subscriber can't
    wait out the subagent that way.
  - A lead that ends its turn while an **async agent** or its own background command runs stays
    `status: busy` until the work is done and the lead's last turn ends: no early idle notice
    (polled every 2 s; and a real chief coordinator subscribed to such a thread heard nothing
    until its real finish).
  - `SubagentStart` fires at spawn and again each time a message wakes an idle subagent;
    `SubagentStop` fires at the end of each of its turns, and on `TaskStop` (the lead's next Stop
    then lists no teammate).
  - When a subagent ends a turn, its lead receives an `idle_notification` teammate message and
    runs a turn: no UserPromptSubmit fires, but a Stop does. So a lead always stops again after
    its subagents' last SubagentStop. A subagent's `SendMessage(to: "team-lead")` can wake the
    lead before the subagent's own SubagentStop.
  - Stop and SubagentStop carry `background_tasks`: `[{id, type: "teammate" | "subagent" |
    "shell", status: "running", description, command?}]`. A teammate stays listed while idle; an
    async agent is listed as `subagent` (its `id` the agent id) until its hand-back; a subagent's
    background commands are listed in its lead's.
  - In auto permission mode, a PreToolUse can carry an `agent_id` that never had a
    SubagentStart (seen repeating a subagent's own Bash call), presumably an agent auto mode
    runs.
  - A lead's own background command wakes it when it ends (UserPromptSubmit with a
    `<task-notification>`). A subagent's does **not** wake the subagent: the notification is
    queued on the lead's transcript and never delivered, so the subagent's work stops there
    (seen with a 60 s sleep and with a real `cargo test`).
  - A foreground `sleep N` is refused ("Blocked: standalone sleep").
  - A Stop hook printing `{"decision":"block","reason":"…"}` keeps a background session's turn
    going with the reason as its next instruction; the Stop that turn ends with has
    `stop_hook_active: true`.
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
  `worktree-add-hello-txt`, not after the thread; use the worktree and branch its job state
  records.
- A thread killed mid-turn stays `state: working` with no status and no pid in `claude agents`.
- A `permissions.deny` rule `Edit(//<absolute canonical path>/**)` in `--settings` (a leading `//`
  means an absolute path) blocks both the Edit and the Write tool for every file under that folder
  in a background session ("File is in a directory that is denied by your permission settings."),
  and other folders stay writable. `Write(…)` and `NotebookEdit(…)` deny rules are useless: Claude
  Code prints "Deny rule Write(//…/**) never matches · use Edit(//…/**)" at startup (verified live
  on 2.1.310 with `--settings` and `--permission-mode auto`).
- Also verified live on 2.1.310 (background, `--permission-mode auto`): with no deny rule a
  session can write a project's `.claude/settings.json`, and `Edit(//<abs>/.claude/**)` blocks
  it. A mid-path glob works: `Edit(//<abs>/projects/*/project.toml)` blocks
  `projects/other/project.toml` while `projects/other/memory/note.md` stays writable. A file glob
  works: `Edit(//<abs>/settings*.json)` blocks `settings.json` and `settings.local.json` while
  `other.json` stays writable. So glob characters in a path are read as globs.

Minimum Claude Code version: 2.1.310. `chief` checks `claude --version` on `init` and `up`.

## 2. Processes (no daemon)

Claude Code's supervisor owns every session. SQLite (WAL) is the shared state. One binary:

| Command | Started by | Job |
| --- | --- | --- |
| `chief` | you | The launcher (§10a): pick a project or create one in a guided setup, then open it |
| `chief new` | you | The launcher, straight to the new-project wizard |
| `chief init <name> [--goal …] [--repo PATH…] [--no-trust-prompt]` | you | Create project files, check trust, register the project |
| `chief trust <slug>` | you | Mark every folder the project's sessions need as trusted (§3 "Trust") |
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
… on a clash) and leaves nothing behind on failure. The project belongs to the account
`CLAUDE_CONFIG_DIR` names when `init` runs (§3); `init` says so for a non-default one. It then
handles trust (§3 "Trust", `trust::check`): it marks the project folder trusted without asking
(`Trusted <dir>.`), since chief made it. With agp (§7 "Thread account") it checks every repo in
every account of the pool too (`placement::pool_trust`: the accounts `[threads] accounts` allows
that agp has `in_pool`, limited ones included) and says "Threads can also run in these agp
accounts: <labels>.". It lists every repo not trusted yet, in the project's account (`  <repo>`)
and in each pool account but the project's own (`  <repo> (in <label>)`). When stdin and stdout
are terminals (and without `--no-trust-prompt`) it asks once, "Trust them now, as accepting
Claude's trust prompt would? [Y/n]", and marks them all, one `Trusted …` line each; otherwise it
prints `To trust them later: chief trust <slug>`. An account agp can't name a folder for is
listed as not checked; a failing `agp status` is one line, and `init` still succeeds. It ends with
`Next: chief up <slug> (or run chief and pick it)`.

`chief trust <slug>` marks every folder `trust::check` lists for the project (its own folder, then
the repos in each account), one `Trusted …` or `Couldn't trust …` line each; running it is the
yes. With nothing left it prints `Every folder of <slug> is trusted already.`; any failure makes it
exit non-zero after the rest.

`chief up` (`project::up`, which prints what `project::up_with` reports: lines on stdout,
warnings on stderr; the launcher shows the same reports in its window) loads project.toml, checks
the version in the project's account (§3), rewrites
`generated/*`, runs the coordinator step (§7), then opens the layout (§9): with Rex when it is
installed and its server runs (`rex list` succeeds), storing the session id in
`ui_state.rex_session`; else, when it runs inside tmux (`$TMUX` set), in a tmux window, storing it
in `ui_state.tmux_window`; else it prints why and the two commands to run side by side in
terminals or splits of the user's own, `chief pane --project <slug>` and `chief board --project
<slug>`. When opening the chosen layout fails it prints why and the same two commands.

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

`chief status [<slug>]` polls `claude agents` once per account the projects shown use (their own
accounts and their threads', §3; stored rows if it fails), syncs each project with those
snapshots (§5), and prints
each project's coordinator and threads grouped in board order. A thread's line joins with ` · `
its name, `"<title>"`, waiting reason, `asleep`, `branch <b>`, `PR <url>`, `account <label>` (the
agp account it runs in, §7 "Thread account") (each when there is
one), then `cleaned up` or `cleanup blocked: <reason>` when §12 applies, and always ends with when
it was last active (`5m ago`, `just now`).

## 3. Files

`<root>` = `$CHIEF_HOME`, else `$XDG_DATA_HOME/chief`, else `~/.local/share/chief`, made absolute.
An empty variable counts as unset (made absolute, it would be the current folder).

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
`CHIEF_CLAUDE_BIN`, `CHIEF_GH_BIN`, `CHIEF_REX_BIN`, `CHIEF_TMUX_BIN`, `CHIEF_AGP_BIN`,
`CHIEF_CLAUDE_CONFIG` and `CHIEF_NO_NOTIFY` are written when set at generation time: as `K='v'`
prefixes on every hook command and in the MCP server's `env`. `CLAUDE_CONFIG_DIR` is written from
the project's account (below), never from the generating process: set to `claude_config_dir` for
a non-default account, absent for the default one. Paths among them are made absolute (a bare
`CHIEF_CLAUDE_BIN`, `CHIEF_GH_BIN`, `CHIEF_REX_BIN`, `CHIEF_TMUX_BIN` or `CHIEF_AGP_BIN` without a
slash is kept for PATH lookup). Rex
blocks start from the login shell's environment and tmux panes from the tmux server's, so the
layout's commands carry the same variables (§9).

A project belongs to one Claude Code account (§1), for good: the `CLAUDE_CONFIG_DIR` in effect at
`chief init`, made absolute and recorded as `claude_config_dir` in project.toml (no key for the
default account; a project.toml without the key, from before, is the default account). Every
Claude Code call for the project (`agents`, `attach`, `rm`, `respawn`, `--bg`, `stop`, the trust
prompt) runs with `CLAUDE_CONFIG_DIR` set to that folder, or removed for the default account
(`Claude::command`); job state is read from that folder's `jobs/` and trust from its
`.claude.json` (`claude::claude_dir`, `claude::claude_config_path`), and the generated env above
carries it. This holds for every caller: the CLI, the board, the pane, the MCP server and the
hooks (which read just `claude_config_dir` from project.toml, `ProjectConfig::read_account`, and
fall back to the account their generated command set when the file can't say). A command about
one project (`up`, `down`, `clean`, `gc <slug>`) run under another `CLAUDE_CONFIG_DIR` than the
project's uses the project's and says so once on stderr (`note: <slug> belongs to …; chief uses
that account, not …`). Commands across projects (`chief status`, `chief gc` without a slug) use
each project's own account: `status` takes one `claude agents` snapshot per account and syncs each
project with its account's; a project whose project.toml can't say shows its stored rows with a
warning. Commands a user types to reach a session (`claude attach <id>`, `claude` for trust) are
printed with the `CLAUDE_CONFIG_DIR=<dir> ` prefix for a non-default account. The board's settings
screen shows the account read-only. `CHIEF_CLAUDE_DIR` and `CHIEF_CLAUDE_CONFIG` are test overrides
for the default account's folder and config file (§11).

**Threads in other accounts.** The coordinator always runs in the project's account; a thread
may run in another, which `start_thread` picks from agp's pool when it starts (§7) and which
never changes after. The row stores it (`account_id`, `account_label`, `account_dir`, §4); a row
without `account_id` runs in the project's account (`SessionRow::account`). Every Claude Code
call about a thread runs in the thread's account (`Claude::for_row`): its spawn, `attach` in the
pane, `respawn` (wake, the pane), `stop` (`chief down --stop-threads`), `rm` (cleanup), its job
state (worktree, branch, PR polling) and its trust check. Syncing takes one `claude agents` per
account the project uses (the project's, plus the account of every thread not cleaned up;
`accounts::in_use`), in parallel, and applies each listing only to the rows of its account
(§5); a listing that fails leaves its account's rows as they were and is reported with the
account's folder. This holds for the board, `list_threads`, the coordinator's context, `chief
status`, `chief gc` and `chief down`; a command about one thread (wake, cleanup, the pane)
lists only that thread's account.

**Trust.** Background sessions refuse folders their account doesn't trust (§1). chief marks
a folder trusted the way accepting Claude's trust dialog records it (`claude::mark_trusted`):
`projects[<canonical path>].hasTrustDialogAccepted = true` in the account's `.claude.json`
(`claude_config_path`), keeping every other key, value and the key order (serde_json's
`preserve_order`). It writes a temporary file beside the target (through a symlink, to the
symlink's target: a dangling link is followed with `read_link`, relative to the link's folder, so
the target is created and the link stays a link), created with `create_new` and mode 0600 under a
name of its own (pid and a per-process counter: board workers and the wizard can mark at once),
gives it the target's permissions (0600 for a new file), renames it over, then reads the trust
back; Claude Code rewrites this file itself, so a write that lost that race is tried
once more before it fails. A file that doesn't parse is never replaced. chief marks only after a
yes: its own project folder needs none (chief made it, and the coordinator runs there), so `init`,
the wizard and `up` (`ensure_coordinator`, before a first spawn, for projects made before this)
mark it themselves; the user's repos only after the one question in `init`, the wizard's
checkbox, `chief trust`, or Enter on an untrusted pair in the board's settings. `trust::check`
lists what a project needs: the project folder, each repo in the project's account, and each repo
in every agp pool account but the project's own (§7 "Thread account").

- `*-settings.json`: `{"hooks": {...}}` in Claude Code's format, one command per event:
  SessionStart, UserPromptSubmit, PreToolUse (matcher `*`), PreToolUse (matcher
  `SendMessage|ListAgents|Agent`, `chief hook guard`), PostToolUse (matcher `*`), Notification, Stop,
  SubagentStart, SubagentStop, SessionEnd. Each runs `chief hook <event> --project <slug> --role <role>`. The
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
  and `"deny"`: `Edit(//<repo path>/**)` for every repo, `Edit(//<root>/projects/*/generated/**)`,
  `Edit(//<root>/projects/*/project.toml)`, `Edit(//<dir>/.claude/**)` (its cwd's project
  settings), `Edit(//<account dir>/settings*.json)` (the account's user settings: its
  `claude_config_dir`, or the real `~/.claude` for the default account, never a test override)
  and `Edit(//<root>/chief.db*)` (the database and its -wal/-shm), canonical paths, built in one
  place (`generated::coordinator_deny_rules`, syntax in `generated::edit_deny_rule` and
  `edit_deny_files`). Per §1 that blocks Edit and Write there: a routing slip can't edit a real
  checkout; the coordinator can't edit the files that hold its own or its threads' guard, nor the
  project.toml they are rewritten from, in this project or any other; it can't add hooks or
  permissions through a project or user settings file; and state stays chief's. Memory and
  `instructions.md` stay editable (another project's too). Since §1 shows paths in rules are read
  as globs, `ProjectConfig::validate` (init, settings save, every load) refuses a repo path
  holding `( ) * ? [ ] { } \` and says to rename or move the folder.
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
claude_config_dir = "/Users/me/.claude-carlos"  # the account, absolute; set by init, never
                                   # changed; no key = the default account (~/.claude)

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
accounts = ["auto"]                # agp accounts new threads may run in (§7 "Thread account"):
                                   # "auto" (the default) = every account in agp's pool, or agp
                                   # profile ids; [] = always the project's account
max_usage = 80                     # optional: a pool account at or over this % (5-hour or
                                   # weekly) takes no new thread

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
  cleaning_at     INTEGER,                -- cleanup claim: when a process claimed it, §12 (migration 7; not in SessionRow)
  account_id      TEXT,                   -- agp account id of a thread's own account, §3 (migration 10); NULL = the project's account
  account_label   TEXT,                   -- that account's agp label, e.g. 'PB2', for display (migration 10)
  account_dir     TEXT,                   -- that account's config folder; NULL with an account_id = the default account (migration 10)
  subagents_busy  INTEGER NOT NULL DEFAULT 0, -- its subagents mid-turn, §5 "Background work" (migration 12)
  background_running INTEGER NOT NULL DEFAULT 0, -- background commands its last Stop listed running (migration 12)
  stopped_early_at INTEGER                -- a thread's last Stop while background work ran; its final report is due (migration 12)
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
  pane_showing TEXT,                      -- session `chief pane` is attached to now; NULL between attaches or after the user left (migration 5)
  rex_session  TEXT,                      -- id of the Rex session `chief up` created, §9 (migration 8)
  pane_pid     INTEGER,                   -- the running `chief pane`, which the board signals to switch, §9 (migration 9)
  pane_started TEXT,                      -- that process's start time, proving the pid is still it (migration 9)
  tmux_window  TEXT                       -- '<window id> <server pid> <socket>' of the tmux window `chief up` opened, §9 (migration 9)
);

CREATE TABLE teammates (                  -- a session's subagents in its current process, §5, §6 "Subagents" (migration 11)
  session_name TEXT NOT NULL,             -- the lead: the sessions.name its hook matched (below)
  session_id   TEXT NOT NULL,             -- the lead's session id that hook carried; a record counts only for it
  agent_id     TEXT NOT NULL,             -- Claude Code's agent id, e.g. 'ahelper-886b99f57d4badec'
  name         TEXT,                      -- the Agent call's `name`; NULL when it had none, or none is known yet
  recorded_at  INTEGER NOT NULL,          -- unix ms of the latest hook that recorded it
  busy         INTEGER NOT NULL DEFAULT 0, -- mid-turn: SubagentStart to SubagentStop, §5 "Background work" (migration 12)
  PRIMARY KEY (session_name, agent_id)
);
```

Every connection: `journal_mode=WAL`, `busy_timeout=5000`, `foreign_keys=ON`.

### Matching a hook to a session

1. `session_id` matches `sessions.session_id` → that row, if it belongs to the hook's project.
2. Else `session_title` matches `sessions.name` within the project and that row has no
   `session_id` yet → that row; store `session_id`. (A row with another `session_id` is not
   matched: the hook comes from a copy of the session.)
3. Else record the event with `session_name = NULL` and stop.

Matching, the event row and the session update run in one `BEGIN IMMEDIATE` transaction.

## 5. Thread state and board groups

`src/state.rs` holds a pure function; everything else stores its result in `group_name` and
`waiting_reason`.

Hook updates (per matched session):

| Hook | Effect |
| --- | --- |
| SessionStart | `last_activity_at`; unless `source` is `compact`, `background_running = 0` and `stopped_early_at = NULL` (a new process runs none of the old one's work) |
| UserPromptSubmit, PreToolUse | `pending_permission = 0`; provisional group `working` |
| PostToolUse | `pending_permission = 0`; update `cwd`; provisional `working` |
| Notification `permission_prompt` | `pending_permission = 1` |
| Notification `idle_prompt` | nothing beyond `last_hook` |
| Stop | `pending_permission = 0`; store `last_assistant_message` in `last_report` and `reports` (skipped when empty); update `cwd` (shown only; a thread's branch comes from its job state on sync, below); `background_running` and the final report (below) |

Every hook above also sets `last_hook`, `updated_at` and `cc_fresh = 0`, re-derives the group, and
(except Notification `idle_prompt`) sets `last_activity_at`. Other events (SubagentStart,
SubagentStop, SessionEnd, other notification types) are only recorded in `events`, apart from
`teammates` and the background work (below): when one changes `subagents_busy` or
`background_running`, it sets `updated_at` and re-derives the group. Coordinator Stops store
reports too.

Subagents (§1, §6 "Subagents"): in the same transaction, a matched session's hook keeps its
`teammates` rows, under the payload's `session_id` (a subagent's own hooks carry its lead's).
SubagentStart records its `agent_id` alone, and keeps a name already recorded (it fires again on
every resume); `agent_type` is never taken for a name. PostToolUse for `Agent` records
`tool_response.agentId` (else `agent_id`) with `tool_input.name`; the name is left out when it is
missing or empty, or when `tool_response.name` differs from it (Claude Code registered another,
which could reach someone else; the agent id still works). A name goes to the latest agent
recorded with it (the others keep their ids, without a name), as Claude Code delivers it. A
SessionStart whose `source` is anything but `compact` (startup, resume, clear, or none) deletes
the session's rows: a new process has none of its old teammates (§1). A compaction keeps them,
and SubagentStop doesn't remove one (it fires after every turn of a teammate that stays); it
only ends its turn (below). Deleting a session row (a failed spawn) deletes its teammates too.

When `[ui] notify` is true, a thread's `permission_prompt` and a thread's Stop send a macOS
notification (`osascript`, spawned without waiting; skipped if missing or `CHIEF_NO_NOTIFY=1`).

Derivation, first match wins (inputs: the row plus the latest `claude agents` snapshot if fresh):

1. `resolved = 1` → **resolved**
2. `cc_state = failed` → **failed**
3. `pending_permission = 1` → **waiting**, reason `permission`; else `cc_status = waiting` →
   **waiting**, reason `question`; both unless the process is gone (below)
4. `cc_state = blocked` → **waiting**, reason `question`
5. `cc_status = busy`, or background work (below) → **working**, unless the process is gone
   (below)

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
before running the command): a snapshot is one account's (§3), and it is applied only to the
project's sessions that run in that account (`SessionRow::account`); it says nothing about the
others, which keep their state and watermark. Match each such session by `session_id`, else by
`short_id` = row `id`. A row matched by short id under another session id (after `/clear`) takes the new
`session_id`, unless another registered row already has it; a row without a `session_id` stores
the listed one. Matched rows get `cc_state`, `cc_status`, `pid` and `cc_fresh = 1`; unmatched rows
get `pid = NULL` and `cc_fresh = 0` and keep their last `cc_state`. A thread's `branch` is its job
state's `worktreeBranch` (§1) when that names a `worktreePath`, NULL when the job state names no
worktree (the checkout's branch isn't the thread's), and kept when there is no job state to read;
never anything read at the hooks' `cwd`, which follows the thread's shell into the checkout or
another thread's worktree. Rows with neither id (not spawned yet) are left alone. Then re-derive and apply auto-resolve. Each row keeps a watermark,
`synced_at` = taken-at of the newest snapshot applied to it: a snapshot older than that is ignored
for the row (polls from the board, hooks and the CLI run in separate processes and may commit out
of order). A poll that changes nothing else moves only the watermark (`updated_at` stays).

Removed outside chief: a stopped session stays listed in `claude agents --all`, and only
`claude rm` takes one off. So a thread row that has a `session_id`, isn't cleaned or already
marked removed, and is missing from a non-stale snapshot (neither its `session_id` nor its
`short_id` listed) was removed, but only when the snapshot can know that: it was taken after the
row was created and after its ids were last set (`ids_at`), and it lists at least one other
registered session of the same project (normally its coordinator). A snapshot that lists none of
them comes from another account's supervisor (another project of this chief home may live in it,
§3), or from one that lists nothing for a moment; a wrong verdict would lock the thread out (§12). The coordinator is normally listed, so this only delays the
verdict. A thread in another account than the project's is judged only by its own account's
snapshot, where the coordinator isn't: there, the verdict needs another of the project's
threads listed in that account, and without one there is none. The row gets `removed_at` = taken-at and, if it wasn't resolved, `resolved = 1` with
`resolved_reason = 'removed'`. When its worktree is gone too (job state names none, §12, or the
folder no longer exists) it also gets `cleaned_at` = taken-at and `cleanup_blocked = NULL`;
otherwise `cleanup_blocked = 'session removed outside chief; worktree remains at <path>'`, and a
later sync that finds that folder gone sets `cleaned_at`. The coordinator row is never marked.
A row with `removed_at` has no session left (`claude respawn` would fail), so it can't be woken,
reopened, messaged or shown in the pane (§6, §7, §9, §10), cleaned or not; the refusal says the
session was removed outside chief and to start a new thread.
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

### Background work

A session's subagents and background commands run inside its process after its own turn ends
(§1), and the session's own turn says nothing about them. While a teammate works, Claude Code
even calls the session idle, so its coordinator gets an idle notice for a thread that isn't done.
chief counts them from the hooks, in the same transaction as the rest (§4 `sessions.subagents_busy`,
`background_running`, `stopped_early_at`; `teammates.busy`):

- **Subagents mid-turn.** SubagentStart marks its teammate row `busy` (recording the row first,
  as above); SubagentStop clears it. A Stop of the session's own (no `agent_id`) whose
  `background_tasks` lists no task but `shell`s clears every row: no teammate or async agent is
  left (a TaskStop, say).
  `subagents_busy` counts the busy rows under the payload's `session_id`.
- **Background commands.** `background_running` is the number of `background_tasks` entries
  of type `shell` and status `running` at the session's last Stop. A command of the session's own
  wakes it when it ends (§1), so the Stop after that counts again.
- **Subagents don't run background commands.** Claude Code doesn't wake a subagent when its
  background command ends (§1): its work would stop half done, and its lead would look done. So
  the PreToolUse hook (not the guard: it fails open) denies a Bash call with
  `tool_input.run_in_background: true` whose `agent_id` is one of the session's recorded
  subagents (an agent auto mode runs isn't, §1), telling it to run the command in
  the foreground with a long enough timeout, or leave a longer one to its lead. Denials are not
  recorded in `guard_denials`, whose count is the message guard's.
- Background work is `subagents_busy > 0` or `background_running > 0`, unless the process is
  gone. It makes rule 5 match, and the board, the coordinator's snapshot (§8), `list_threads` and
  `read_report` (§7) show it, e.g. `1 subagent, 2 background commands`.
- **No finish while it runs.** A thread's Stop with background work sends no notification. A
  thread's Stop with a busy subagent while `background_tasks` lists a `teammate` also sets
  `stopped_early_at`: its coordinator was told it was idle (§1). Async agents and the thread's own
  commands keep it busy, so its idle notice comes at its real finish and nothing more is needed.
  The thread's next Stop without background work clears `stopped_early_at` and is the **final
  report**: the hook prints
  `{"decision":"block","reason":"…"}`, telling the thread that everything it waited on finished
  and to send its coordinator the final report with SendMessage (what it did, the branch and
  commit, the PR, test results, what it needs), at the address `thread_send` allows (§6): the
  coordinator's name, or its socket address for a thread in another account. That Stop's
  notification is held back; the Stop the block's turn ends with (`stop_hook_active: true`)
  doesn't block again and notifies. No block when the coordinator has no live session alone
  with its name (it reads the board when it next runs), or when listing its account fails
  (logged).
- A SessionStart other than `compact` clears `background_running` and `stopped_early_at`, and
  deletes the teammates (above).

A subagent can still stall on some other kind of background work that doesn't wake it (§1 only
verified Bash). Its lead would then look done at its next Stop. The thread prompt (§8) tells
threads to have subagents run commands in the foreground.

## 6. Message guard

`chief hook guard` runs on PreToolUse with matcher `SendMessage|ListAgents|Agent` for every chief
session. A SendMessage between a session and its own subagents is decided first, by "Subagents"
below; the rows here decide everything else.

| Caller | Tool | Rule |
| --- | --- | --- |
| any | `Agent` | deny a `name` that another session answers to ("Subagents" below); allow every other call |
| any | `ListAgents` | deny. Coordinator: "Use the chief list_threads tool; it lists only this project's threads." Thread: "ListAgents is blocked in chief threads: you can message only your coordinator and your own subagents." |
| coordinator | `SendMessage` | allow only if `to` is exactly the name of a thread registered to this project, or exactly the socket address of one (a reply, form below), and that thread is not removed outside chief, not cleaned, not resolved, and reachable (below). Removed outside chief (`removed_at`, §5) → deny: "<name>'s session was removed outside chief, so it can't be woken, reopened or messaged; start a new thread (its reports are still readable with read_report)". Cleaned → deny: "<name> was cleaned up; start a new thread (its reports are still readable with read_report)". Resolved → deny: "Thread is resolved; reopen it first." Not live → deny: "Thread is asleep: call wake_thread first." Renamed or ambiguous → deny, saying which. Everything else → deny ("… is not a thread of this project", "… is not the address of a live thread of this project") |
| thread | `SendMessage` | allow only if `to` is exactly `chief-<slug>` or exactly the coordinator's socket address, and the coordinator is reachable. Coordinator not running (or never started) → deny: "Your coordinator chief-<slug> isn't running, so the message can't be delivered. Just finish your turn: your final message is recorded as your report when you stop anyway, and the coordinator reads it there." Renamed or ambiguous → deny with the same advice. Everything else → deny ("Threads may message only their coordinator, by its exact name chief-<slug>, and their own subagents, by the name or agent id the Agent tool gave."; for a thread in another account, "by replying to the exact `from="uds:…"` address of its message" in place of the name): siblings, other projects, subagents chief has no record of, and any session the local `claude agents` doesn't list |

An allow-list by byte-exact equality. `to` is never trimmed, case-folded or parsed, and a trailing
` [ref]` is not removed: Claude Code's refs (`[2c7859]`, a short hex id) pick one session, so a
ref could deliver a message that the bare name would allow to another session. A denied `to` that
contains `[` gets "Use the bare name, without a `[ref]`: chief matches names exactly, and a ref can
pick another session." before the reason. Lookalikes (another case, U+FEFF, U+200B, a tab before
the ref) and sessions the local `claude agents` of the project's account (§3) doesn't list (other
machines, cloud sessions, other accounts, Remote Control, other sessions' teammates and
subagents) match nothing and are denied. A session's own subagents are the one exception
("Subagents" below).

Reachable (the identity check, for a thread under its name and for the coordinator under
`chief-<slug>`): the snapshot entry for the row's `session_id` (its `short_id` while the
`session_id` is unresolved; never an entry found by name) has a pid and carries exactly that name,
and no other snapshot entry, in any state, carries that name. Otherwise it is asleep (no entry, or
no pid), renamed (the entry carries another name, so the name may reach someone else), or
ambiguous (another entry carries the name too). A coordinator with no row counts as not running.

Threads in another account than the project's (§3). The coordinator's account (the project's)
can't reach them by name (§1: the name reaches nothing there, or a session of that account), so
for the coordinator such a thread is reachable only at its socket address, and the identity check
above runs in a fresh `claude agents` of the thread's own account (polled in parallel with the
project's, bounded the same; only the accounts of unresolved, uncleaned, unremoved threads the
`to` could be for: the named one, or all of them for a `uds:` address). Its name → deny: "<name>
runs in another Claude account (<label>), where names don't reach: message it at its address
uds:… (list_threads shows it)", or the asleep/resolved/… reason when it isn't reachable. An
account whose listing fails → deny for its threads, saying chief couldn't list it (a `uds:`
denial names the accounts it couldn't check); the project's account failing is a guard failure
(fail closed). A thread in another account calling (matched by the payload's `session_id`) can't
reach its coordinator by name: `chief-<slug>` → deny: "You run in another Claude account (<label>)
than your coordinator, and chief-<slug>'s name doesn't reach across accounts. To answer a message
from it, send your reply to the exact `from="uds:…"` address in that message's header. Otherwise
just finish your turn: …"; the coordinator's exact socket address is allowed as for any thread,
checked in the coordinator's (the project's) account's listing.

Socket addresses are accepted only in the exact form Claude Code gives a session's socket (§1, the
`from` of every cross-session message): `uds:/tmp/cc-socks/<pid>.sock`, the pid in decimal with no
sign or leading zeros, nothing before or after. The guard builds that string from the reachable
session's pid and compares bytes, so another folder, a bare `uds:<pid>.sock`, `0201` or a ref
after it is denied. If Claude Code ever moves its sockets, replies by address are denied (fail
closed) and the sessions fall back to names.

`to` is read from `tool_input.to`; a SendMessage whose `tool_input` isn't an object, or has no
string `to` or a blank one, is denied. Other tools are allowed. `claude agents` is only run for a
SendMessage with a `to` and for an Agent call with a `name`.

### Subagents

In a background session the Agent tool spawns in-process teammates that the session and they
reach by name or agent id, and that report to it as `team-lead` or `main` (§1). The hooks record
each session's (§5, `teammates`); the guard reads the caller's: the rows recorded for the session
whose `session_id` is the payload's, under that same session id. A call comes from one of its
subagents when the payload has a non-empty `agent_id`, which Claude Code sets, not the model.

- From a subagent: `to` exactly `team-lead` or `main` → allow.
- From the session or one of its subagents: `to` exactly the agent id of one of the caller's
  recorded subagents → allow (no session answers to an agent id). `to` exactly a recorded name →
  allow, unless another session answers to that name: the coordinator (`chief-<slug>`), any
  thread of the project (any state), or a session in the guard's fresh `claude agents` of the
  project's account and of the caller's own (a thread in another account, §3) → deny: "<to> is
  also the name of another Claude session, so a message to <to> could reach it instead of your
  subagent. Message the subagent by its agent id <id>." A listing that couldn't be taken → deny,
  saying so, with the same advice (the project's account failing is a guard failure, as always).
  In-process names win only while the subagent lives in this process (§1), so a name another
  session has could reach that session.
- A name or id that isn't plain (letters, digits, `-`, `_`, `.`), and the names `team-lead` and
  `main`, are never taken as a subagent's address: Claude Code could read `uds:…`, a ` [ref]` or
  the lead's own names as something else. Deny, saying to use the agent id.
- Anything else follows the caller's role (the table above): a subagent's message goes out under
  its session's address, so a thread's subagents may message its coordinator and the
  coordinator's its threads. `team-lead` and `main` from the session itself are denied there.
- Never another session's subagents: only the caller's own records are read. A `[ref]` matches no
  record and stays denied, with the bare-name advice.

At spawn, an `Agent` call whose `tool_input.name` exactly equals the coordinator's name, the name
of any thread of the project (any state, from the database), or a session in the fresh listings
of the project's account and the caller's own is denied: "A Claude session is already named
<name>, and a subagent named <name> would get the messages meant for it. Pick another name for the
subagent." In-process names win (§1), so that subagent would silently take the other session's
messages. An Agent call without a `name`, with an empty one, or with a free one is allowed. When a
listing can't be taken, only the database's names are checked and the call is otherwise allowed
(a broken listing shouldn't stop sessions from using subagents), and the hook error log says so.
The deadline and every other guard failure (bad payload, database, panic) deny an Agent call too,
as any call. A denial's `target` is the name it asked for.

Residual risk: the records are forgotten by the SessionStart hook of the next process (§5). If
that hook fails to run after a respawn, the old records stay, and a stale name could reach a
remote or cloud session of that name, which no local listing shows; the listing check covers the
local ones, a `[ref]` is denied, and a stale agent id reaches nothing. A thread started after one
of the coordinator's subagents took its name can't be messaged by that name until the
coordinator's process starts again (its `uds:` address still works).

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

Every deny writes a `guard_denials` row (caller matched by `session_id`, `target` = raw `to`, or
an Agent call's `name`) and
returns the PreToolUse deny JSON above; an allow prints nothing. If the guard itself fails (bad
payload, DB or `claude agents` error, or a panic), it denies with the error as the reason (fail
closed), logs the error, and records the denial if the DB opens.

## 7. Coordinator MCP tools (`chief mcp --project <slug>`)

All tools return JSON text content. Errors are tool errors with a plain message.

| Tool | Input | Does |
| --- | --- | --- |
| `start_thread` | `title` (string), `task` (string), `repo?` (repos[].name or absolute path), `model?`, `effort?`, `account?` (an agp id or label from the pool, only when the user asks) | Picks the thread's account (below), which also checks trust of the repo path there, picks a unique name, rewrites `generated/*`, inserts the row, spawns the thread in that account, resolves its ids. Returns `{name, short_id, repo, model, effort, account, address, next}` (`account` the agp label, null for the project's account), plus `account_note` when the choice skipped accounts or fell back, and `warning` when it adopted a session whose spawn reported a failure |
| `list_threads` | `include_resolved?` (bool, default false) | Refreshes from `claude agents` in every account the project uses (stored rows plus a `warning` naming what failed), returns `{threads: [...]}` in board order: `name, title, group, waiting_reason, asleep, running, branch, pr_url, last_report_preview (≤300 chars), last_activity` ("5m ago"), `cleaned`, `account`, `address`; `running` is its background work (§5), e.g. `"1 subagent"`, or null. Resolved and cleaned threads are left out unless `include_resolved` |
| `read_report` | `name`, `history?` (int, default 1, max 20) | `{name, title, group, reports: [{at, text}]}`, newest first, full text; with background work (§5) also `running` and a `note` that the thread isn't finished and reports when it is |
| `wake_thread` | `name` | Refreshes from its own account; if no live pid: `claude respawn <short_id>` there, poll its account's `claude agents` up to 20 s for a pid. Returns `{name, alive, respawned}`, plus `address` once alive and, after a respawn that gave a thread in another account a new address, `next` (subscribe again there). A cleaned thread is a tool error: "<name> was cleaned up; start a new thread (its reports are still readable with read_report)"; so is one still removed outside chief after the refresh, with §6's removed message |
| `resolve_thread` | `name`, `reopen?` (bool) | Sets or clears `resolved` (`coordinator`). Returns `{name, resolved, group}`. Reopening a cleaned thread, or one removed outside chief, is the same tool error as `wake_thread`'s (`registry::set_resolved` refuses it for every caller) |

There is no cleanup tool: removing a worktree and branch is the user's call (§12).

Names may carry a trailing ` [ref]`; it is ignored. Only this project's threads are accepted.

`start_thread` details: `repo` falls back to `threads.default_repo` (error if neither); an absolute
path must be a folder and maps back to a repos[] name when it is one. The account is chosen
before anything starts (below); falling back to the project's account checks trust there and an
untrusted repo returns the `NotTrusted` message. The name must not be in the registry nor among
the `claude agents` names of the thread's account and the project's (a few suffixes are tried; if
either `claude agents` fails, nothing starts). A task that starts with `-` is sent as `Task: <task>` so the CLI can't read it as
a flag. If the spawn fails, the session may have started anyway (a timeout, an output without
the `backgrounded` line), and a deleted row would leave it working unseen and unreachable through
the guard. So unless the error is `NotTrusted`, chief first looks for it in `claude agents` (polled
up to the 5 s resolve timeout, each call bounded): a background session (it has an `id`) named
exactly the new name, whose `cwd` is the folder the spawn ran in (compared canonically), that the
pre-spawn snapshot didn't list is adopted (its short id, session id and pid are stored) and the
call succeeds with a `warning` that quotes the failure. A session with that name in another
folder is someone else's (another chief home's, or the user's own). When listings
succeed without it, the row is deleted and the error returned. When every listing fails, chief
can't tell: the row is kept without a short id (hooks attach it by title if the session did start)
and the error says so. Failing to resolve the session id is not an error (hooks match by title,
sync by short id). `next` is
a literal SendMessage input with no `message` key, `{"to": "<address>", "notify_when_idle": true}`
(verified: omitting `message` is a valid pure subscription): the coordinator subscribes right after
starting a thread, which is how it hears the thread finished. (M5 live run: told "with no message",
and again told to pass `""`, Haiku emitted `"message": ,`, which is invalid JSON.) For a thread in
another account `next` adds that names don't reach it there and the address changes when it is
woken; when its process isn't listed yet, `next` says to get the address from `list_threads`.

A thread's `address` (`tools::address`) is where the coordinator's `SendMessage` reaches it:
its name when it runs in the project's account; otherwise, since names don't reach across
accounts (§1), `uds:/tmp/cc-socks/<pid>.sock` with the pid of its own session in a fresh listing
of its account (matched by session id, else short id). The pid changes with every respawn, so the
address is computed from the listing each time and never stored; it is null when that listing
failed or shows no process, and for a cleaned or removed thread.

The coordinator sends work to existing threads itself with `SendMessage` (`to` = the thread's
address, `notify_when_idle: true`); the guard enforces scope.

### Thread account

Without agp (`agp` not on PATH or `CHIEF_AGP_BIN` missing), or with `[threads] accounts = []`,
every thread runs in the project's account, as before. Otherwise `start_thread` reads
`agp status --json` (bounded, 15 s) and takes the pool: the Claude accounts agp has `in_pool`
that `accounts` allows (`"auto"`: all of them; else the listed profile ids). The coordinator's
own account needn't be in it. No pool account allowed: the project's account, silently. Of the
pool, the usable accounts are those not `limited`, with 5-hour and weekly utilization below
`max_usage` when it is set, best agp `score` first (`agp pick` ignores `--accounts`/`--exclude`,
§1, so chief never asks it). For each in turn chief asks agp for its folder (`agp exec <id> --
sh -c 'printf %s "${CLAUDE_CONFIG_DIR-}"'`; empty = the default account; every `agp exec` runs
with chief's own `CLAUDE_CONFIG_DIR` removed, since agp leaves it unset for the default account
and the MCP server inherits the project's) and checks the repo's
trust in that account's `.claude.json`; the first that trusts it gets the thread, and the ones
skipped (untrusted, with the `cd <repo> && agp exec <id> -- claude` that fixes it; agp failing)
are listed in `account_note`. When none is usable or none trusts the repo, or `agp status`
fails, the thread runs in the project's account and `account_note` says why. `account?` pins
one: it must be in the pool (by id, else label) and trust the repo, or the call fails; limited
or over `max_usage` only adds a note (the thread waits for the reset). The account is chosen
once: a thread whose account later hits a limit waits there and is never moved. `chief init`
(§2) and the board's settings screen (§10) check every pool account's trust of every repo ahead
of time and offer the fix for each pair, so `start_thread` rarely has to skip one.

The tool logic lives in `src/tools.rs` as plain functions over a context (connection, config,
`Claude` in the project's account, `Agp`, project dir, trust file, clock, timeouts; the account
choice is `src/placement.rs`); `src/mcp.rs` adapts them to `rmcp` and runs
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
whose message says to run `chief trust <project>`, or `claude` in that folder once and accept the
trust prompt.

Every external command chief waits on (`claude`, `git`, `gh`, `rex`) runs through
`proc::output_with_timeout`, and its timeout is a real bound: the child leads its own process
group, and the call must see the child exit *and* both pipes close before the deadline. Otherwise
the whole group gets SIGKILL and the call fails as a timeout, without waiting for readers still
stuck on a pipe (a grandchild such as `sh -c 'sleep 60 & exit 0'` would hold it open). The pipes
are awaited before the child is reaped, and the group is signalled only while the child is still
unreaped: until then its pid, the group's id, can't be reused, so the kill can't reach another
process group. The child itself is killed too (still unreaped), so a leader that moved to another
group can't hold the call past its deadline. `git`, `gh` and `claude` run without the variables
that tie git to one repository (`proc::without_repo_env`, the list `git rev-parse
--local-env-vars` prints: `GIT_DIR`, `GIT_WORK_TREE`, `GIT_INDEX_FILE`, `GIT_COMMON_DIR`,
`GIT_OBJECT_DIRECTORY`, …): a git hook exports them, and chief run from one would otherwise ask
about, or reset, the hook's repository instead of the folder it names.

Trust (`claude::is_trusted_in`) walks up the canonical path (macOS `/tmp` is `/private/tmp`),
stopping at the first folder that holds `.git`, as Claude Code does on the real path. The path as
given is walked too, but only while each of its ancestors is the same folder as the canonical
ancestor at that depth, so it never goes past where the canonical walk stops: with
`/Users/me/work` a link to `/mono/projects` and `.git` in `/mono`, a trusted `/Users/me` doesn't
count. A path that doesn't exist is walked as given. Any trusted folder on the way counts. It reads the config file of the account the session runs in (§3,
`Claude::trust_config`): `<account folder>/.claude.json`, or for the default account
`$CHIEF_CLAUDE_CONFIG` (tests) else `~/.claude.json`.

### Coordinator spawn (`chief up`)

```
cwd  = <dir>   (must be trusted; chief marks it, §3 "Trust")
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
(§3) blocks Edit and Write in every repo, in every project's `generated/` and `project.toml`, in
its own `.claude/`, in the account's `settings*.json` and on the database, so in its own folder it
can edit only `memory/` and `instructions.md` (settings are the user's: the board's `s` or the
file itself). `SendMessage` goes through the guard; its chief MCP tools run without prompts.

No prompt: it waits for you. Started when it has no row or no `short_id` (the project folder must be
trusted, and chief marks it first when it isn't; no other live session may already use the name).
A failed spawn is handled as a thread's is (above): a session that started anyway is adopted, with a
warning (stderr from the command line); a row it
inserted is removed only when listings show no such session, and kept when chief can't tell. A
kept row (no `short_id`) is adopted by the next `up` when `claude agents` lists its session (by
`session_id` if a hook stored one, else a background session with exactly the name whose `cwd`
is the project folder, compared canonically), instead of refusing the name as taken. A live
session with the name in another folder (another chief home's coordinator, or the user's own) is
never adopted: the name counts as taken. If it exists but `claude agents` shows no pid, `claude respawn` it and wait up to 10 s for
a pid. Otherwise it is left alone.

## 8. Prompts (files in `src/prompts/`, included with `include_str!`)

- `coordinator.md`: role (route, never do the work), its tools (§7: read the repos, edit only
  memory and instructions, no shell), routing rules (quick question → answer;
  new work → `start_thread`, with `account` only when the user names one; follow-up →
  `SendMessage` to the matching thread with `notify_when_idle`, waking it first if asleep, at its
  exact address with no `[ref]` (its name, or the `uds:…` address of a thread in another account,
  which changes when it is woken: subscribe again then); several tasks →
  several threads), say which thread got what, keep `max_parallel`, save preferences and decisions
  to `memory/` and update `memory/MEMORY.md`, read full reports with `read_report` when an idle
  notice arrives; when `read_report` says the thread still runs subagents or background
  commands, the notice isn't a finish: say nothing yet and don't subscribe again (the notice
  would come straight back, §1), as the thread reports when they finish (§5 "Background
  work"). Removing worktrees and branches is the user's call (board `x`, `chief clean`,
  `chief gc`), so it never offers to; it names no branch in a thread's brief unless the user did
  (threads create their own). One line says it can message its own subagents by the name or agent
  id the Agent tool gave them, and that they report to it (§6 "Subagents").
- `thread.md`: you are one thread of project <name>; read `<dir>/memory/MEMORY.md` first; project
  instructions follow; work on your own branch; of other sessions message only the coordinator,
  by its exact name (the report needs no message), or, for a thread spawned in another account
  than the coordinator's (§3), reply only to the `from="uds:…"` address of the coordinator's
  message; you can message your own subagents by the name or agent id the Agent tool gave them,
  and they report to you (§6 "Subagents"); when it ends a turn to wait for subagents or a
  background command, say what it waits for, and send the final report if chief asks for it;
  tell subagents to run tests and builds in the foreground, as chief denies their background
  commands, and run anything over 10 minutes itself (§5 "Background work"); end every turn with a short report (what changed,
  branch, PR URL if any, what you need); if something is missing, say exactly what and stop.

Placeholders (`{{name}}` replaced verbatim): coordinator `name`, `goal_line` (` Goal: …` or empty),
`project_dir`, `max_parallel`; thread `name`, `coordinator_name`, `project_dir`, `messaging` (the
by-name line, or the by-address line for a thread in another account, chosen at spawn). Both get
`instructions.md` appended under `## Project instructions` when it isn't empty.

The coordinator's UserPromptSubmit hook runs `claude agents` (≤5 s; stored rows if it fails, logged),
syncs, and returns as `additionalContext`:

```
<chief-board as-of="HH:MM">
Waiting on you:
- <name> "<title>" · waiting: permission · 1 subagent running · asleep · branch <b> · PR <url>
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

0. Inside a Rex block (`REX_SESSION` and `REX_BLOCK`, which Rex sets in each block's
   environment) on a terminal, when `rex list --json` succeeds: the user's own block becomes the
   conversation. `layout::open_beside` splits a block labelled `chief-<slug> board` right of it
   at 32 % (`--focus=false`) in the user's session, or reuses one that still runs there (an
   ended one is closed and replaced); `up_with` returns `Where::Here`, and `chief up` (or the
   launcher, §10a) then runs `chief pane` in this same terminal with `OnQuit::Board`: when the
   pane ends, by `q` or a hangup, it closes that board block and the user's block is back at its
   shell. No session is created, nothing is stored in `ui_state`, and no app has to switch or
   focus anything, so this works with several Rex apps connected. A failure here prints why and
   falls back to 3, not to a new session.
1. Rex, when `rex list --json` succeeds (installed, server running): the Rex session below.
2. tmux, when `$TMUX` is set (chief runs inside tmux): a tmux window, below.
3. Neither: it prints why and the two commands, `chief pane --project <slug>` and
   `chief board --project <slug>`, to run side by side (two windows, iTerm or Ghostty splits,
   anything). `chief down` has nothing to close then. From the launcher (§10a), Enter runs the
   pane in the launcher's own terminal instead, after printing the board's command.

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
`chief pane --project <slug> --layout` (the hidden flag: quitting the pane closes the layout, see
below), block `board` split right of it at 32 % running
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

Then focus `main` and `show` the session. Focus is best-effort: Rex refuses it as a client action
when several apps are connected, and failing there would lose the id of a session just made, so
the next `up` would make another. When Rex's `show` returns false, `up` prints
`rex -C <app> do session.select session_id=<id>` and says to pick the session in the app.

`chief pane` loop (`src/pane.rs`). On start it catches its signals (below), then records its pid
and start time in `ui_state.pane_pid` / `pane_started` (`proc::start_time`: `proc_pidinfo` on
macOS, `/proc/<pid>/stat` on Linux); on exit it clears them, only while they are still its own.

0. Read `project.toml`. When it can't be read, doesn't parse or doesn't validate, print
   `chief · project.toml can't be used:`, the error indented below it, and
   `Fix project.toml; retrying every 5 s · q quits`, then read it again after 5 s, reading keys.
   Nothing is attached, rewritten or respawned meanwhile: step 2 respawns from the file read
   here, never from stale generated files. Every `claude` call of this pass runs in the account
   it names (§3), whatever the block's shell has in `CLAUDE_CONFIG_DIR`.
1. Target: `ui_state.pane_target`; NULL, or a session that is unknown, of another project, cleaned,
   removed outside chief (`removed_at`, worktree left or not) or never started (no short id),
   means the coordinator, and a stale target is
   cleared (only while it still holds that name, so a newer board write survives). No coordinator
   with a short id yet: print `chief · no coordinator yet: run chief up <slug>` and look again
   every 500 ms.
2. Print `chief · <name> …` on a cleared screen. Sync `claude agents`; with no pid, `claude
   respawn <short_id>` and poll for a pid up to 20 s (`chief · waking <name> …`). A failing
   `claude agents` skips this step; a failing respawn is shown and retried after 500 ms.
3. Write `pane_showing = <name>`, then read the target again; if it changed, clear
   `pane_showing` and start over (see board switching below).
4. Drain queued keys (`q` quits), restore the terminal mode the pane started in but with ISIG off,
   run `claude attach <short_id>` as a child on the same TTY, in the target's own account (§3:
   a thread in another account is attached, polled and respawned there), wait (polling `waitpid(WNOHANG)`
   every 50 ms, so a signal flag set at any moment is seen), take raw mode back, clear
   `pane_showing`.
5. An attach that failed, exited non-zero, or ended within 1 s prints why below its own output
   (`… · retrying · q quits`) and waits 500 ms, reading keys. Then back to 0. An attach ended for a
   switch goes straight back to 0.
6. Any other attach ended because the user left Claude by hand (its Ctrl+Z, or Ctrl+C out of
   its agent view). Attaching again at once would put them straight back in with no way out, so
   the pane prints `chief · you left the coordinator (<name>). It keeps running in the
   background.` (or `thread <name>`), then `⏎ back to it` (`, or pick a thread on the board` in
   the layout) and `q close chief's layout (sessions keep running)` (`q quit …` outside it), and
   waits: ⏎ or a switch request (SIGUSR1, below) goes back to 0; `q`, Ctrl+C or a hangup ends the
   loop. Keys in the first 300 ms are dropped: a third Ctrl+C meant for Claude isn't an answer.

`q` while chief has the terminal ends the loop; so does the block being closed or the session
killed. With `--layout` a `q` (not a hangup) then closes the layout as `chief down` does
(`project::close_layout`: the Rex session, or the tmux window, or just chief's two blocks when
the user added others), after the pane cleared its rows and restored the terminal, since closing
hangs up the pane's own terminal; no session stops. Then the pty hangs up and the kernel sends SIGHUP to the session leader, chief, not to
`claude attach`. chief catches SIGHUP (and SIGTERM) with a handler that only sets a flag, without
`SA_RESTART`, so the default action can't end it before cleanup: the wait sees the flag, passes
SIGHUP on to attach, gives it 2 s to exit, then SIGKILLs it, and reaps it either way; the loop
then stops. On exit `pane_showing` is cleared (a stale one would keep §12 cleanup away from that
thread).

Ctrl+Z: the user can detach by hand with Claude's own Ctrl+Z inside `claude attach`; the loop then
waits as in step 6. While chief itself has the terminal, the terminal is in raw mode
(ISIG off), so Ctrl+Z, Ctrl+C and other keys are plain bytes chief reads and drops (only `q`
counts). Attach starts with ISIG still off, so a Ctrl+Z that arrives before Claude's UI is up
stays queued as input; Claude reads it once it takes the terminal and detaches. In case a SIGTSTP
gets through anyway, chief ignores it and spawns attach with SIGTSTP reset to default (an ignored
signal would survive exec); it waits with `WUNTRACED` and sends SIGCONT to a stopped attach, so the
loop can't hang. On exit chief restores the terminal mode it started with. Also, Rex runs the
command as the leader of its own session, an orphaned process group, so the kernel discards
terminal stop signals there anyway (verified in M4: with ISIG on, `^Z` echoes and nothing stops).

Board switching works in any terminal, with or without a multiplexer: write `pane_target`, then
`pane::request_switch(conn, slug)`. When `pane_showing` equals the target (NULL target = the
coordinator's name), or no pane is recorded, do nothing. A NULL `pane_showing` is signalled too:
the pane may be waiting after the user left (step 6), where the signal attaches the target, even
the one just left; between attaches the flag stays set and the next wait drops it when the target
is the one being attached.
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
  url,state,isDraft,reviewDecision,createdAt,isCrossRepository,headRefOid --limit 10` in
  `repo_path` and,
  with `cleanup = "merged"`, cleans up merged threads. Its one-time `hint`
  (no `gh`, not logged in, no GitHub remote) stays in the status line; blocked and error counts
  prefix it (`PR 1 blocked, 2 errors · …`). At start the board deletes `events` older than 30 days
  (`db::prune_events`).
- Look (`src/tui.rs`, shared with the launcher): rounded panels with a bold title, a one-column
  margin inside when the panel has 34 columns to spare, one accent colour (cyan) for focus, keys
  and marks, the selection in reverse video across the width. The last line is always the key
  bar: the keys that act on the selection, most useful first, each `key what` with the key in
  the accent colour, whole hints only, `? help` kept at the end when the rest doesn't fit. Above
  it, when there is one, the status line: a running worker, else the last action's result
  (cleared by the next key), else refresh errors, else the PR hint.
- Layout, designed for a 40–70 column pane and usable down to 30: a panel titled with the project
  name holds the goal (one dim line), then the coordinator's row, then the groups in §5 order
  (`<label> · <count>`, Waiting in bold red), empty groups hidden, "No threads yet" when there
  are none, Resolved collapsed by default (`Resolved · N · R shows`). When the list doesn't fit,
  its last line says what is below (`↓ N more`, plus `· Resolved N · R shows` for a collapsed
  header under the fold; `↑ more above` at the bottom); while the selection is on the first page
  the list starts at the top. The coordinator's row is `<target mark>◉ Coordinator  <state>`
  with the guard denial count when > 0 (`asleep · 2 guard denials`). A thread's row is
  `<target mark><glyph> <title or name>  <tags>`, cut to the width with an ellipsis and never
  wrapped; the title gives way first so tags stay readable. Tags:
  waiting reason, background work unless resolved (`1 subagent, 2 background commands`, §5),
  `asleep`, PR state (`PR draft|open|approved|changes|merged|closed`), `cleanup blocked`, `cleaned up`, then the agp label of the account it runs in (`PB2`; none for a
  thread in the project's account without agp, §7 "Thread account"). Detail panel for the
  selection, titled with the thread's title (name, group and reason or resolved reason,
  `· <background work> running`, asleep, branch, PR URL, model · effort, `account <label>`, last
  activity, cleanup state, the last report wrapped), or "Coordinator" (its state, model ·
  effort, last activity, and what to do next: with no threads, "Tell the coordinator what you want
  done, in the main pane…"): below the list when the body has ≥ 16 lines (the list takes what it
  needs up to 3/5), otherwise Tab swaps the list for it.
  Colours: a fixed palette that reads on dark and light terminals (Waiting bold red with `!`,
  Working green `●`, Review blue `◆`, Landing magenta `▲`, Idle default `○`, Failed red `✗`,
  Resolved dim `✓`). `▸` marks what the main pane shows: a thread's row, or the coordinator's.
- Keys: ↑/↓ or j/k move over the coordinator's row and the threads (headers skipped; g/G
  first/last). The first threads to show take the selection from a coordinator selected only
  because there were none; one the user selected stays selected across refreshes. Enter shows the
  selection in the main pane (a thread: refused for a cleaned one or one removed outside chief);
  Esc or c shows the coordinator from anywhere; on the coordinator's row the thread keys (w, r,
  o, x) say they act on a thread; w wake (`tools::wake_thread`, refused for cleaned or removed); r resolve
  or reopen (`registry::set_resolved`, reason `user`; reopening a cleaned or removed thread is
  refused with its message); o open the PR URL
  (`open` on macOS, `xdg-open` elsewhere, output discarded); x clean up, only for a resolved,
  uncleaned thread: `cleanup::clean(Safe)`. When blocked, a fresh `cleanup::inspect` decides the
  modal: if it lists the loss, "Discard unsaved work?" shows why plus `cleanup::render_loss` and
  runs `clean(Discard(<that inspection>))` only after the thread's name is typed exactly (Enter
  with a wrong name clears the input; Esc cancels); if it lists nothing or fails (`claude rm`
  refused over work chief can't see), "Cleanup blocked" says `Cleanup blocked: <why>.` and why no
  discard is offered, and any key closes it; R show or hide resolved; Tab detail on
  short terminals; s settings; ? help (the keys, then what each mark and glyph means; any key or
  click closes); q or Ctrl+C quit the board only. Wake and cleanup run one at a time on a worker
  thread ("waking …" in the status line).
- Mouse: a click selects a row (the coordinator or a thread), a click on the selected row is Enter,
  a click on the Resolved header is R, the wheel moves the selection. Each frame records which
  screen row holds what (`App::hits`) for this. In settings a click puts the cursor on a field and
  the wheel moves it.
- Showing a thread: write `ui_state.pane_target` (NULL for the coordinator), then call
  `pane::request_switch`, which signals the running `chief pane` unless it already shows the
  target or no pane runs (§9).
- Settings screen (`s`), a panel titled `Settings · <name>` (`· unsaved` while the draft differs),
  edits a draft of `project.toml` in four sections: Project (goal, the repos, add repo, default
  repo, the two files, notify), Coordinator (model, effort), Threads (model, effort, permission
  mode, max_parallel, cleanup) and Accounts (the account, thread accounts, max usage, account
  trust and its untrusted pairs), then Save. Under the form one line says what the field under the
  cursor does (`Field::help`), unless a message replaces it; Ctrl+S saves from any field. The
  fields: goal, coordinator model and effort, thread
  model, effort, permission mode, max_parallel, thread accounts (`[threads] accounts`, ids split
  on commas or spaces, empty = `[]`, shown "(none: the project's account)"), max usage (a percent
  1–100, empty = none), default repo, cleanup (merged/manual),
  `ui.notify`, and the repos (add by path, named after its folder with `-2`, `-3` … on a clash, as
  in `init`; `d` removes one, moving the default repo to the first remaining). It also shows the
  project's Claude Code account (`claude_config_dir`, or `default (~/.claude)`) read-only: Enter
  or ←/→ there only says it can't be changed (§3). "Account trust" checks, on a worker, every
  repo of the draft in every pool account (`placement::pool_trust`, as `init` does, the project's
  own account included when it is in the pool): its value says `checking…`, `trusted in <labels>`,
  `N untrusted · checked <labels>`, "no agp pool: threads use the project's account", or why the check
  failed (accounts it couldn't check go in the message). Each untrusted pair gets a line
  (`untrusted  <label> · <repo>`); on it the message says what Enter does, and Enter marks that
  repo trusted in that account (§3 "Trust") on a worker, then checks again (`Msg::Trusted`). Efforts, modes,
  default repo, cleanup and notify cycle with ←/→ (Enter too); models, goal, max_parallel, thread
  accounts and max usage are free text (Enter edits, Enter applies, Esc cancels). Save validates (`ProjectConfig::validate`,
  errors shown in the form), writes with `save`, regenerates `generated/*`, and says "Saved. New
  threads use these settings; the coordinator's model and effort apply on its next start." Esc
  with unsaved changes asks for a second Esc. Two entries open `instructions.md` and
  `memory/MEMORY.md` in `$EDITOR` (run through `sh -c`, so arguments work; fallback `vi`): the
  board leaves the alternate screen and raw mode, runs the editor on the same TTY, and restores.
- Terminal: `tui::init`, which is `ratatui::init` (alternate screen, raw mode, a panic hook that
  restores the terminal) plus mouse reporting, which its own panic hook turns off too; restored on
  quit and on error; the editor gets the terminal through `tui::suspended` (mouse, alternate screen
  and raw mode off, then back). Input is polled every 100 ms, results are drained between frames,
  and resizes take effect on the next frame.
- A thread with `cleanup_blocked` shows a "cleanup blocked" tag (the reason in the detail); a
  cleaned thread shows "cleaned up" in Resolved. Wake and reopen are refused for a cleaned thread
  and for one removed outside chief.

## 10a. Launcher

`chief` with no command (`src/launcher/`): `app.rs` holds the state and turns keys and clicks into
actions, `ui.rs` draws it, `wizard.rs` is the new-project wizard, `mod.rs` runs the loop and the
worker. It needs a terminal on stdin and stdout; otherwise it fails with "chief needs a terminal;
`chief --help` lists the commands for scripts". The look is the board's (§10 "Look"): one card,
at most 76 columns wide, centred, a line below the top, as tall as its content, the key bar right
under it.

- Project list (`Your projects`): each registered project's name in bold with a summary at the
  right, from project.toml and its stored rows only, so it opens at once (`1 waiting · 2 working`,
  in red while one waits; `no threads yet`; `all resolved`; `can't read project.toml: …`), and its
  goal dim below. Then `+ New project`. ↑/↓, j/k, g/G and the wheel move; Enter or a click on the
  selection opens the selected project, or the wizard on `+ New project`; n starts the wizard from
  anywhere; ? explains what chief does and the keys; q or Esc quits. With no projects the
  launcher opens on the wizard, and Esc from its first step quits. `chief new` opens on the
  wizard too.
- Wizard, four steps, `New project · step N of 4` with `✓ About  ● Code  ○ Models  ○ Review`
  below the title. Esc (or Shift+Tab) goes back a step and keeps what was typed; on the first step
  it cancels.
  1. About: Name (required; its slug shows under it; Enter refuses an empty name, one with no
     letter or digit, and a slug a project has, `project::slug_taken`) and Goal (optional). Tab or
     ↑/↓ switch fields; Enter on the name moves to the goal, Enter on the goal goes on.
  2. Code: the repository folders threads work in, starting with the git repository the launcher
     runs in (the folder holding `.git` above its cwd), if any. Type a path (`~` works; Tab
     completes the last folder name, or the prefix the matches share) and Enter adds it,
     canonical; a missing folder or one listed already says so. ↑ selects listed folders, d
     removes one. Enter with nothing typed goes on; none is fine.
  3. Models: coordinator model and effort, thread model and effort (←/→ cycle `opus`, `sonnet`,
     `haiku` and §3's efforts), defaults from `ProjectConfig::new`, a line on what each does, and
     a note that the rest is on the board (s).
  4. Review: name and slug, goal, code, models; when the project's account (this process's
     `CLAUDE_CONFIG_DIR`, as for `init`) doesn't trust some repos, lists them with
     `[x] Trust them, as accepting Claude's trust prompt would` (Space toggles). Enter creates.
- Creating (`launcher::create`, on a worker): `project::create_with` (`init`'s validation and
  files, with the wizard's models and efforts), then `trust::check`: the project folder is
  marked, and the repos in every account when the box was ticked (else a warning names
  `chief trust <slug>`); then `project::up_with`. Opening a listed project runs `up_with` alone.
  The "Starting <name>" card shows each report as it comes (`✓` done, `!` warning) under a
  spinner, then where the conversation is: Rex (`Open in Rex session chief-<slug>: the
  conversation with the coordinator on the left, its threads on the right.`, or to switch to it in
  the app when Rex couldn't), tmux likewise, in a Rex block (§9 step 0) the launcher leaves at
  once and runs `chief pane` in its own terminal beside the new board, or neither: Enter leaves the launcher, prints
  `chief board --project <slug>` to run elsewhere, and runs `chief pane` in this terminal (§9).
  A failure shows in red; before the project existed (`Msg::Created`), Enter goes back to the
  wizard as it was, after it back to the list. The list is re-read once a project is made or
  opened. q quits at any time; the worker's session keeps starting.

## 11. Testing

- Unit tests: §5 derivation (table-driven), §6 guard (table-driven), hook payload parsing against
  `tests/fixtures/hooks/spike-events.jsonl` and, for subagents, the raw payloads in
  `tests/fixtures/hooks/teammate-events.jsonl` (§1), `claude agents` parsing against
  `tests/fixtures/agents/agents-sample.json`, config round trip, migrations on a fresh DB.
- `CHIEF_CLAUDE_BIN` overrides the `claude` binary; integration tests use a fake script
  (`tests/fake-claude.sh`) that records its arguments and prints canned output. `CHIEF_GH_BIN`
  overrides `gh` the same way (the same script, linked as `gh`), and `CHIEF_REX_BIN` overrides
  `rex` (linked as `rex`; the test home's default fails like a Rex with no server, so no test
  reaches the real one). `CHIEF_TMUX_BIN` overrides `tmux` (linked as `tmux`; reached only when a
  test sets `TMUX`). `CHIEF_AGP_BIN` overrides `agp` (linked as `agp`, with canned
  `status.stdout` and per-account `exec.<id>.stdout`: the fake serves `<key>.<second argument>`
  files first); the test home's link is missing until a test installs it, so agp counts as not
  installed and no test reaches the real one. Layout logic is also tested against an in-memory `Mux`, and the tmux backend
  against a real tmux server on a private socket (`-L chief-test-…`, no config) that the test kills.
  The switch is tested with `chief pane` on a pseudo-terminal and a fake `attach` that sleeps. `claude rm` outputs from the live
  M2b probe are in `tests/fixtures/rm/`. Cleanup tests use real temporary git repositories (a bare
  remote, a clone, worktrees under `.claude/worktrees/`); git is the only real tool tests run. `CHIEF_HOME` points
  tests at a temp dir, `CHIEF_CLAUDE_CONFIG` replaces the default account's `~/.claude.json`,
  `CHIEF_CLAUDE_DIR` replaces the default account's folder `~/.claude` (job state; never written
  into generated files); a project with its own account (`claude_config_dir`, §3) uses that folder
  for both, so tests give such projects a temp account folder. `CLAUDE_CONFIG_DIR` is removed from
  the tests' environment unless a test sets it, and `CHIEF_NO_NOTIFY=1` turns off notifications.
  The fake records each call's `CLAUDE_CONFIG_DIR` (`calls/<n>.env`) and serves
  `$CLAUDE_CONFIG_DIR/fake-<key>.stdout[.<k>]` before its own canned output, so each account has
  its own sessions. It numbers calls with exclusive creates, since chief lists several accounts
  at once (§3).
- The launcher's wizard and screens are tested without a terminal (a `Probe` answers its slug and
  trust checks); its create step runs end to end against the fake in `tests/launcher.rs`, a test
  binary with one test, since that step reads chief's environment variables in-process.
- `scripts/e2e.sh` runs the live loop with cheap sessions (milestone 2b).

## 12. Cleanup (`src/cleanup.rs`, `src/pr.rs`)

Resolving a thread keeps its session, worktree and branch. Cleanup removes them with
`claude rm <short_id>` (§1), which deletes the worktree and the local branch and keeps any remote
branch. The row stays, with its reports: `cleaned_at` is set and `cleanup_blocked` cleared.

The worktree is the one `claude rm` itself deletes: the session's job state,
`<claude dir>/jobs/<short_id>/state.json` → `worktreePath`, where `<claude dir>` is the
thread's account folder (§3: its own account's, else the project's `claude_config_dir`, or for
the default account `$CHIEF_CLAUDE_DIR` in tests, else `~/.claude`); its refresh (step 1) and
its `claude rm` run in that account too. It counts only as a `<repo>/.claude/worktrees/<name>` folder
(the innermost such match, so a repository that itself lies under one still works), and while it exists
`git -C <wt> rev-parse --show-toplevel` must be that folder (a leftover plain folder would make git
answer for the checkout around it). Never the hooks' `cwd`, which follows the thread's shell
anywhere, another thread's worktree included, and never a name derived from the thread's. No state
file or no `worktreePath` means the thread has no worktree of its own: nothing to lose, and nothing
is reset or cleaned. A state file that exists but can't be read or doesn't parse (the supervisor
may be rewriting it), or a short id that isn't a plain name, is an error, never "no worktree"
(`claude::read_job_state`): `inspect` fails, PR polling skips the thread with an error line, and
cleanup blocks (below). Only the sync's display of a thread's branch and its record of a removal
(§5) read it leniently (`claude::job_state`).

`inspect(row)` (all git calls through `proc::output_with_timeout`, 10 s, without the repository
variables of §7): whether the worktree exists, its branch (the job state's `worktreeBranch`, the
one `claude rm` deletes, whatever HEAD is on now; `git branch --show-current` only when the job
state names none), `git status --porcelain --untracked-files=normal` lines (ignored files aren't
listed, as Claude Code ignores them), and unpushed commits as Claude Code means them:
`git rev-list --count HEAD refs/heads/<branch> --not --remotes`, listed with
`git log --format='%h %s' -n20 …`, and the head they end at (`git rev-parse HEAD`). So commits on
the job's branch count even when HEAD is on another branch, and `render_loss` names that branch.
A branch that doesn't exist (`rev-parse --verify --quiet` exits 1) is left out; any other failure
fails the inspection. When the worktree is gone, commits are checked in `repo_path` if the branch
still exists there, and the head is the branch's sha.

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
   remains at <path>` (the folder is the user's to delete), or, when the job state can't be read,
   `session removed outside chief; chief can't tell whether its worktree remains: <error>`.
2. Refuse (error, nothing run) unless the thread is resolved and, derived as if it weren't
   resolved, neither working nor waiting, and isn't the session `chief pane` shows
   (`ui_state.pane_showing`: the user may be typing into it, and hooks fire only on submit):
   `claude rm` stops the session even when it refuses.
3. Never spawned (no `short_id`): mark cleaned without `claude rm`.
4. Either mode: when the job state of another session registered in this chief home (read in
   that session's own account folder: its own account's, else its project's) names the same worktree, store `<wt> is also the worktree of <other>; chief won't remove or reset it` in
   `cleanup_blocked` and stop without calling `claude rm` (it would delete the worktree and its
   branch under that session). Another session's job state that can't be read may name it, so it
   blocks the same way: `<wt> is possibly also the worktree of <other>, whose job state can't be
   read (<error>); chief won't remove or reset it`. `chief gc --dry-run` reports either as
   blocked, and neither the board nor `chief clean` offers a discard for it (both reasons contain
   `cleanup::SHARED_WORKTREE`; they say to finish and clean up the other thread first).
   `Safe`: if `inspect` finds uncommitted changes or unpushed commits, or fails (an unreadable
   job state included), store the reason in `cleanup_blocked` (`2 unpushed commits`,
   `uncommitted changes: 3 files`, both joined with `, `; `could not inspect the worktree: …`)
   and stop without calling `claude rm`. `Discard` fails on an inspection that fails.
   `Discard(confirmed)` carries the inspection the user was shown and confirmed. It is refused
   (stored in `cleanup_blocked`, nothing run) when a fresh `inspect` differs from it ("the
   worktree changed after you confirmed …"); otherwise in a dirty worktree run
   `git -C <wt> reset --hard -q` and `git -C <wt> clean -fdq` (never `-x`: ignored files stay),
   in step 5.
5. Steps 1 and 2 again, with a fresh `claude agents`, right before anything is reset or
   stopped (the thread may have got a message, or the pane attached to it, meanwhile); then the
   `Discard` reset from step 4.
6. `claude rm <short_id>` (120 s: it deletes the worktree before it answers). `Discard` retries an unpushed refusal once with
   `--discard-unpushed <token>` only when the confirmed inspection itself listed unpushed commits
   and the token's head sha (`<sha>@<worktree-id>`) equals the head it saw (`git rev-parse HEAD`
   in the worktree, else the branch's sha). Otherwise `claude rm` counts work nobody was shown,
   and no destructive flag is ever passed. Removed → cleaned. Unpushed → `N unpushed commits`;
   dirty → `uncommitted changes`; anything else → `claude rm: <output>`, all stored in
   `cleanup_blocked`. A row that is cleaned by then (a sync found it removed) keeps its
   `cleaned_at` and never gets a `cleanup_blocked`.

`claude rm` output (`claude::parse_rm`, after dropping CSI and OSC escape sequences, so an OSC 8
link around an id doesn't hide it; `parse_bg_short_id` does the same): a `removed <id>` line is removed (exit status alone isn't:
a stale token printed usage and exited 0); `kept <id> — …` with a `--discard-unpushed <hex>@<hex>`
line is unpushed (its `unpushed commit` summary lines kept); `kept` with "uncommitted changes" is
dirty; anything else is `Other` with the text.

PR status (`pr.rs`): `Gh::poll(repo_path, branch, since)` runs the §10 `gh pr list` (10 s) and
takes the thread's PR: the newest listed one that isn't cross-repository (a fork's branch can share
the name), whose `createdAt` is no earlier than the thread row's `created_at` minus 5 minutes of
clock skew, and whose `headRefOid` is on the thread's branch: `git cat-file -e <oid>^{commit}`
and `git merge-base --is-ancestor <oid> refs/heads/<branch>` in `repo_path` (the head or an
ancestor of it). A branch name says nothing about whose PR it is: a new thread can reuse the
branch name of an old one whose PR merged long ago, or of a cleaned one whose remote branch stays
and whose PR merges later; that PR must not resolve and clean the new thread. Its head is on the
new thread's branch only if that branch was built on it. A PR without a readable `createdAt`, or
without a `headRefOid`, is ignored, and so is one whose head the repository doesn't have (GitHub
updated the branch, say). When the check can't run (no local branch, git fails) the poll is an
error: no PR is attributed and the stored one stays. A `repo_path` that isn't a folder is an error
too (spawning gh there would fail like a missing `gh`). A missing `gh`,
a `gh` that isn't logged in, or a repository without a GitHub remote is no PR plus a hint returned
once per `Gh`; other failures are errors. `poll_project` polls, on that branch, threads whose job
state names their own worktree and a `worktreeBranch` (`cleanup::branch_of`; never the hooks'
`cwd` or `sessions.branch`; a job state that can't be read skips the thread with an error line)
and that aren't cleaned: every unresolved one, and resolved ones whose PR isn't merged or
closed and that were active in the last 7 days. It stores results with `registry::set_pr`, which
auto-resolves merged ones. `refresh_project` (the board, every 30 s) adds, with
`cleanup = "merged"`, `clean(Safe)` for every resolved, uncleaned thread with a merged PR that is
neither working nor waiting nor showing in the pane and has no `cleanup_blocked`: a blocked
thread waits for the user, a
working one is tried again on the next refresh. `cleanup = "manual"` never cleans on its own.

`chief gc [<slug>] [--dry-run]` per project, in that project's account (§3; a project whose
project.toml can't say is skipped with a warning): `poll_project`, sync `claude agents`, then
`clean(Safe)` on every resolved, uncleaned thread (dry: what it would do, from the same checks in
the same order, writing nothing: working, waiting or showing in the pane is skipped, a thread
removed outside chief is judged as step 1 would, then `inspect` and step 4; only another process's
claim isn't checked). It prints
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

A cleaned thread, or one removed outside chief, can't be woken, reopened or messaged (§6, §7); a
cleaned one is hidden from `list_threads` unless `include_resolved`. The coordinator has no cleanup tool; its prompt says cleanup is the
user's call (§8).
