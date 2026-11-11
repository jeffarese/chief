# chief: architecture and contracts

chief recreates Claude Projects locally. You talk to one coordinator Claude session; it starts and
steers background Claude Code sessions ("threads"); a board parks each thread by status. A project
can also repeat work on a schedule (routines, §13) and show itself in the browser (the dashboard,
§14); one optional background process, `chief serve`, does both.

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
  `usage.seven_day.utilization` (percent; `seven_day` may be null), `projection`, `token`
  (`"Missing"` when agp has no login for the account, else `{"Valid": …}` or `{"Expired": …}`:
  an expired token is refreshed by the account's next run);
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
    "shell", status: "running", description, command?}]`. A teammate stays listed while idle,
    under a task id (`t5yzdiksp`), not its agent id: only its `description` matches the Agent
    call's. An async agent is listed as `subagent` (its `id` the agent id) until its hand-back; a
    subagent's background commands are listed in its lead's.
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
- A command hook with `"asyncRewake": true` runs in the background: the event goes on without it
  (a Stop ends the turn at once), and when it exits 2 Claude Code wakes the idle session with a
  user message `<task-notification><summary>{rewakeSummary}</summary></task-notification>` plus a
  system-reminder `{rewakeMessage} {its stderr, else stdout}`, queued to run next; any other exit
  wakes nothing. `rewakeSummary` and `rewakeMessage` are optional strings in the hook's entry
  (defaults "Stop hook feedback" and `Stop hook blocking error from command "<event>":`). The
  hook's parent is the session's process, and its `timeout` (seconds) is how long it may run.
  Verified live on 2.1.316 in a `claude --bg` session, on SessionStart and Stop; older versions
  are untested.

- Usage limits and moving a session between accounts, verified live on 2.1.316 (§5 "Limits and
  moves"):
  - A turn stopped by a usage limit leaves the session's process alive and idle, and its job state
    `state: "blocked"`, `needs: "rate limited — wait and retry · <detail>"`, `detail: "You've hit
    your 5-hour limit · resets 6:10am (Europe/Madrid)"`. It doesn't retry by itself. `claude
    agents` shows only `state: blocked, status: idle`, the same as a question.
  - `claude --bg --resume <session id> <flags> "<prompt>"`, run in another account (whose
    supervisor has no job for that session, and which sees the transcript through a shared
    `projects/`) with the old process stopped, resumes the same session: the same session id and
    short id, back inside its worktree, with its whole conversation. The new job has
    `worktreePath: null` and `cwd` = the folder the command ran in; the job in the first account
    still names the worktree.
  - In an account that already has a job for the session, the same command with any flag starts a
    copy under a new session id ("keeps its own saved options, so the flags you passed started a
    copy"), and so does one while its process runs ("already running … started a copy"). Without
    flags, on a stopped job, it wakes the session with its saved options and runs the prompt.
  - `claude rm` in the new account removes only its job: it names no worktree. In the first
    account it deletes the worktree and branch, as always. Neither deletes the transcript.
  - agp watches background sessions it starts (`agp [opts] -- --bg …`) or is given (`agp bg
    watch`), and moves one to another account when its own hits a limit, in that way. `agp bg
    --json` lists them: `session_id`, `short_id`, `profile`, `label`, `config_dir` (null: the
    default account), `state` (`watching`, `limited`, `waiting`, `moving`, `ended`, `dormant`),
    `limit`, `moves[]` (`at` in unix seconds, `from`/`to` with labels and folders, `reason`:
    "5-hour limit", "weekly limit", "model limit", "limit", "manual move"). It writes `moving`
    before it stops a session and keeps it until the session runs again. An older agp has no
    `bg`, and reads `agp bg …` as `claude` arguments: chief asks `agp --help` for a line starting
    `agp bg` first.

Minimum Claude Code version: 2.1.316 (the coordinator's watcher, §5 "Telling the coordinator",
needs `asyncRewake`). `chief` checks `claude --version` on `init` and `up`.

## 2. Processes (one optional daemon)

Claude Code's supervisor owns every session. SQLite (WAL) is the shared state. Nothing below
needs a process of chief's to stay up, with one exception that is opt-in: `chief serve` (§14),
which fires routines (§13) and serves the dashboard. Hooks, the board, the pane and the
coordinator's tools work the same with it stopped. One binary:

| Command | Started by | Job |
| --- | --- | --- |
| `chief` | you | The launcher (§10a): pick a project or create one in a guided setup, then open it |
| `chief new` | you | The launcher, straight to the new-project wizard |
| `chief init <name> [--goal …] [--repo PATH…] [--no-trust-prompt] [--no-agp]` | you | Create project files, check trust, register the project |
| `chief trust <slug>` | you | Mark every folder the project's sessions need as trusted (§3 "Trust") |
| `chief up <slug> [--layout builtin]` | you | Unarchive the project if it is archived, start or revive the coordinator, open the layout (Rex, tmux, chief's own, or say how) |
| `chief down <slug> [--stop-threads]` | you | End the pane, close the layout; threads keep running unless asked |
| `chief archive <slug>` | you | `chief down <slug> --stop-threads`, then put the project away (hidden from the launcher's list and `chief status`); nothing is deleted |
| `chief unarchive <slug>` | you | Bring an archived project back; starts nothing |
| `chief board --project <slug>` | `chief up` | The board TUI (right pane) |
| `chief pane --project <slug>` | `chief up` | Left pane loop: attaches to the current target session; in Rex it runs as `claude` (§9) |
| `chief hook <event> --project <slug> --role <coordinator\|thread>` | Claude Code hooks | Record events, derive state, inject context |
| `chief hook guard --project <slug> --role …` | PreToolUse hook | Message guard |
| `chief hook watch --project <slug> --role coordinator` | the coordinator's SessionStart and Stop hooks, in the background (`asyncRewake`) | Wake the coordinator when a thread is stuck on the user (§5 "Telling the coordinator") |
| `chief mcp --project <slug>` | Claude Code (coordinator's MCP config) | Coordinator tools over stdio |
| `chief mcp --project <slug> --routine <name>` | Claude Code (a routine run's MCP config) | That routine's run tools over stdio: `record_value`, `report_outcome` (§13) |
| `chief status [<slug>]` | you | Print the board as text (debugging, scripting) |
| `chief gc [<slug>] [--dry-run]` | you | Refresh PRs, then clean up every resolved thread that loses nothing (§12) |
| `chief clean <slug> <thread> [--discard] [--yes]` | you | Clean up one resolved thread; `--discard` throws away unsaved work after confirming (§12) |
| `chief routine list\|add\|show\|run\|approve\|pause\|resume\|rm <slug> …` | you | A project's routines (§13). `add` takes `--prompt`, `--command` (with `--timeout`) or both; `show` prints one with how it stands, what it found, its command's 10 latest runs and what the last printed; `run` waits for a command and prints its output; `approve` is the user's yes to a command the coordinator wrote (§13 "Approval") |
| `chief routine value <series> <number> [--unit U] [--note N]`, `chief routine outcome <nothing\|update\|attention> <headline> [--details D]` | a routine's command | What the command's run tracks and came to (§13 "A command"); they read `CHIEF_PROJECT`, `CHIEF_ROUTINE` and `CHIEF_RUN` |
| `chief routine exec <slug> <run>` (hidden) | chief, detached (`command::spawn`) | Runs one run of a routine's command to its end and records it (§13 "A command") |
| `chief serve [--bind ADDR] [--port N]` | you, `chief serve start`, launchd | The background server, in the foreground: fires due routines and serves the dashboard (§14) |
| `chief serve start\|stop\|status\|install\|uninstall` | you; `chief up` and `chief dashboard` run `start` | Start it detached, end it, say what it serves, keep it running across logins (§14) |
| `chief dashboard <slug> [--off] [--no-open]` | you | Turn the project's dashboard on (or off), start the server, print the sign-in link (§14) |

Hook commands must be fast, must never block a session, and must exit 0. Failures go to
`<root>/logs/hook-errors.log`. The only non-empty stdout is the guard decision, the coordinator's
UserPromptSubmit context, the notice at its SessionStart to read a brief that went stale (§8
"A brief that went stale"), the PreToolUse denial of a coordinator's look outside the project or past its
turn's count (§6 "The coordinator's own looking"), the two outputs of §5 "Background work" (the PreToolUse denial of a
subagent's background command and the Stop decision that asks a thread for its final report), and
the Stop decision that asks a routine's run for its outcome (§13 "What a run leaves"). The
one exception is `chief hook watch`: it runs in the background, so it may take as long as it
needs and blocks nothing, and it exits 2 with its notice on stderr to wake the coordinator. The
`claude agents` call inside the guard is bounded at 10 s; inside the coordinator's context, the
final report's Stop and a Stop chief can't match yet (§4 "Matching a hook to a session") at 5 s.

`chief init` validates first (Claude Code version, name → slug with ≥1 letter or digit, slug not
taken, every `--repo` an existing folder, stored canonical and named after its folder with `-2`, `-3`
… on a clash) and leaves nothing behind on failure. The project belongs to the account
`CLAUDE_CONFIG_DIR` names when `init` runs (§3); `init` says so for a non-default one. agp is
optional, and a project uses it for its threads when it is installed (the `[threads]` defaults,
§3). `--no-agp` creates the project with `accounts = []` and `runner = "claude"`
(`ThreadSettings::without_agp`), so its threads stay in its account, started by plain `claude`,
and agp is never asked; `init` says "Its threads stay in this account, started by claude: agp
isn't used.". It then
handles trust (§3 "Trust", `trust::check`): it marks the project folder trusted without asking
(`Trusted <dir>.`), since chief made it. With agp (§7 "Thread account") it checks every repo in
every account of the pool too (`placement::pool_trust`: the accounts `[threads] accounts` allows
that agp has `in_pool`, limited ones included) and says "Threads can also run in these agp
accounts: <labels>.". It lists every repo not trusted yet, in the project's account (`  <repo>`)
and in each pool account but the project's own (`  <repo> (in <label>)`). When stdin and stdout
are terminals (and without `--no-trust-prompt`) it asks once, "Trust them now, as accepting
Claude's trust prompt would? [Y/n]", and marks them all, one `Trusted …` line each; otherwise it
prints `To trust them later: chief trust <slug>`. An account agp can't name a folder for is
listed as not checked; a failing `agp status` is one line, and `init` still succeeds. So is a
trust check that fails once the project exists (an unreadable `.claude.json`: `chief couldn't
check which folders Claude Code trusts: …`, then `To trust them later: chief trust <slug>`):
failing then would leave a project a second `init` finds taken. It ends with
`Next: chief up <slug> (or run chief and pick it)`.

`chief trust <slug>` marks every folder `trust::check` lists for the project (its own folder, then
the repos in each account), one `Trusted …` or `Couldn't trust …` line each; running it is the
yes. With nothing left it prints `Every folder of <slug> is trusted already.`; any failure makes it
exit non-zero after the rest.

`chief up` (`project::up`, which prints what `project::up_with` reports: lines on stdout,
warnings on stderr; the launcher shows the same reports in its window) loads project.toml, checks
the version in the project's account (§3), rewrites
`generated/*`, clears `projects.archived_at` when it is set (`Unarchived <name>.`: opening an
archived project is wanting it back), runs the coordinator step (§7), then opens the layout (§9): with Rex when it is
installed and its server runs (`rex list` succeeds), storing the session id in
`ui_state.rex_session`; else, when it runs inside tmux (`$TMUX` set), in a tmux window, storing it
in `ui_state.tmux_window`; else, on a terminal, it draws both sides itself, the built-in layout
(§9); else (no terminal) it prints why and
the two commands to run side by side in terminals or splits of the user's own, `chief pane
--project <slug>` and `chief board --project <slug>`. When opening the chosen layout fails it
falls back the same way. `--layout builtin` skips Rex and tmux and opens the built-in layout; it
fails before anything starts without a terminal or the feature.

After the coordinator step, `up` starts `chief serve` when the project has an enabled routine or
its dashboard on and none runs (`serve::ensure_for`: `Started chief serve at <url> (routines…).`);
a server that can't start is a warning, and `up` goes on. `chief down` never stops it: it serves
every project of the chief home.

`chief down <slug> [--stop-threads]` (`project::down`, which prints what `project::down_with`
reports: lines on stdout, warnings and failures on stderr) first ends the running pane, wherever
it runs: one `down` didn't open (in the user's own Rex block, §9 case 0, or run by hand) would otherwise attach again
and respawn the coordinator, or its target, seconds after `down` stopped it. When `pane_pid`'s
start time matches `pane_started` (`pane::live_pane`) it sends SIGTERM, which ends the pane as a
hangup does (it reaps its attach, clears its record, and closes the board it opened beside the
user's block, but not chief's layout), and waits up to 3 s for the record to clear or the process
to go (`Ended chief pane (pid <pid>).`); a pane still running then is a failure (`chief pane
(pid <pid>) is still running after 3 s.`). A record whose process is gone is cleared, silently.
Then it closes the layout `chief up` opened, and only that: the Rex session and the tmux window it
stored, each when stored (§9 `close`: kills the session or window when it holds only chief's
blocks, else closes just `board` and `main`), or nothing when none is stored (`No layout of
chief's is open.`, as for a layout the user made). It clears `ui_state.pane_showing` (unless the
pane still runs) and `ui_state.pane_target` (the next `chief up` shows the coordinator; a
pane back on the thread it last showed would respawn it right after `--stop-threads` stopped it),
syncs `claude agents` (stored rows if it fails), and runs
`claude stop` on the coordinator when it has a process; with `--stop-threads` also on every
uncleaned thread with a process. Without it, it says how many threads keep running. It never runs
`claude rm`. Each step prints what it did; any failure makes it exit non-zero after the rest ran.

`chief archive <slug>` (`project::archive`, printing what `project::archive_with` reports, which
the launcher shows in its window) puts a finished project away, reversibly. An archived project
already says `<name> is archived already.` and exits 0. Otherwise it runs `down_with(slug,
stop_threads: true)`, and only when that finished cleanly stores `projects.archived_at` and says
`Archived <name>. Nothing was deleted: chief unarchive <slug> or chief up <slug> brings it back.`
When down didn't finish cleanly (a stop or a close failed, the account unreadable) the project
isn't archived, since something of it may still run and archived would hide it: it exits non-zero
with `<name> isn't archived: … Run chief archive <slug> again once that is fixed.` Conversations,
worktrees, branches and files all stay. `chief unarchive <slug>` clears `archived_at` and starts
nothing (`Unarchived <name>. chief up <slug> opens it.`, or `<name> isn't archived.`). Archiving
changes only what is listed: `chief gc` without a slug still goes through archived projects (their
resolved threads still hold worktrees), and every command that takes a slug works on them.

`chief status [<slug>]` polls `claude agents` once per account the projects shown use (their own
accounts and their threads', §3; stored rows if it fails), syncs each project with those
snapshots (§5), and prints
each project's coordinator and threads grouped in board order. A thread's line joins with ` · `
its name, `"<title>"`, waiting reason, `asleep`, `branch <b>`, `PR <url>`, `account <label>` (the
agp account it runs in, §7 "Thread account"), `routine <name>` (a routine's run, §13) (each when there is
one), then `cleaned up` or `cleanup blocked: <reason>` when §12 applies, and always ends with when
it was last active (`5m ago`, `just now`). Under the threads of a project with routines,
`Routines (N)` and a line each (`routines::render_status`): name, `"<title>"`, kind, schedule in
words, how it stands (§13 "State": `Failing: exit 22: …`, `OK: passed in 0.3s`, `Needs
approval: …`), and `next <when>`, `paused` or `never due`. Without a slug it leaves archived projects out and ends
with `N archived project(s) not shown; chief status <slug> shows one.` when there are any; with
the slug of an archived one, its first line is `<slug> (archived; chief unarchive <slug> brings it
back)`.

## 3. Files

`<root>` = `$CHIEF_HOME`, else `$XDG_DATA_HOME/chief`, else `~/.local/share/chief`, made absolute.
An empty variable counts as unset (made absolute, it would be the current folder).

```
<root>/chief.db
<root>/logs/hook-errors.log
<root>/logs/serve.log   what a detached `chief serve` says (§14)
<root>/logs/routine-runs.log   what the processes that run routines' commands say of themselves (§13)
<root>/serve.toml       optional: where `chief serve` listens (`bind`, `port`)
<root>/serve.lock       held by the running `chief serve`
<root>/serve.json       the running server's pid, address and start time
<root>/serve-token      the dashboard's secret, mode 0600
<root>/projects/<slug>/
  project.toml          settings (schema below)
  instructions.md       standing brief sent to the coordinator and every new thread
  memory/MEMORY.md      memory index; other memory files beside it
  generated/coordinator-settings.json
  generated/coordinator-prompt.md    the brief the coordinator's session was created with (§8 "A brief that went stale")
  generated/coordinator-brief.md     its brief as it reads now, written when that one went stale, for it to read
  generated/coordinator-brief.read   what it last read of it, while that is still in its context
  generated/coordinator-started      when the coordinator's process last started (unix ms), noted by its SessionStart hook
  generated/thread-settings.json
  generated/run-settings.json        a routine run's: a thread's hooks, and `mcp__chief` allowed (§13)
  generated/mcp.json
  generated/run-mcp-<routine>.json   the run tools of one routine, written before each run
  routines/<routine>/NOTES.md        what that routine's runs remember; other files they keep beside it
  routines/<routine>/last-run.log    everything its command's latest run printed (§13 "A command")
  workspace/                         where threads work when the project has no repositories
```

**Workspace.** A project needn't be about code. With no `[[repos]]` and no repo named,
`start_thread` and a routine's run work in `<dir>/workspace` (`tools::resolve_place`, made on
first use, repo name `workspace`): a plain folder, so no worktree and no branch. The project
folder's trust (below) covers it. A project with repositories never uses it.

`generated/*` is rewritten from `project.toml` by `init`, `up` and whenever settings change, and
right before every spawn and respawn (`start_thread`, `wake_thread`, the pane's respawn, `chief up`):
Claude Code reads hooks and permissions when a session starts, so an edit to these files (one that
drops the guard, say) never reaches a later start. A rewrite that fails stops the spawn or
respawn. Each file, and project.toml, is written through a temp file of that write's own in the
same folder (`<name>.<pid>-<n>.tmp`, `config::replace_file`), renamed over it and removed when
the write fails: a session starting mid-write never reads half a file, and writers at once (`chief
up`, the pane's respawn, `start_thread` in the MCP server) never share a temp file. Commands
inside them use the canonical absolute path of the `chief` binary (`std::env::current_exe()`), so install with `cargo install --path .`: `chief up` prints a
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
pane, `respawn` (wake, the pane), `stop` (`chief down --stop-threads`, `stop_thread` and the board's S), `rm` (cleanup), its job
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
  also sets `"timeout": 30` (seconds; the time budget is in §6). The coordinator's SessionStart
  and Stop entries have a second command, its watcher (§5 "Telling the coordinator"):
  `{"command": "<chief> hook watch --project <slug> --role coordinator", "asyncRewake": true,
  "timeout": 86400, "rewakeSummary": "chief: a thread is waiting on you", "rewakeMessage":
  "Message from chief (not a hook error):"}` (§1). Every file has `"permissions": {"deny":
  ["RemoteTrigger", "Skill(schedule)"]}` (`generated::CLOUD_SCHEDULING_DENY`): Claude Code's
  `RemoteTrigger` tool creates scheduled tasks that run in Anthropic's cloud, and `/schedule`
  is the skill that drives it (Claude Code's docs; not tried live). A project's repeating work
  is a routine, which runs on this machine (§13), so no chief session may schedule it
  elsewhere. Session-scoped loops (`CronCreate`, `/loop`) are local and stay. The coordinator's file adds `"worktree": {"bgIsolation": "none"}` and `"permissions"`:
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
  holding `( ) * ? [ ] { } \` and says to rename or move the folder. It refuses a relative repo
  path too: the deny rule (resolved where the files are generated) and `--add-dir` (resolved
  from the coordinator's cwd) would name different folders.
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
runner = "agp"                     # agp (default) | claude: who starts threads (§7 "Thread
                                   # spawn"); with agp, agp watches each one for usage limits
on_limit = "switch"                # switch (default) | wait | stay: what agp does when a thread's
                                   # account hits a limit (§5 "Limits and moves"); with
                                   # accounts = [] a switch is a wait

[[repos]]
name = "payments-api"
path = "/Users/me/workspace/payments-api"

[ui]
notify = true                      # macOS notifications on permission prompts and finished threads

[dashboard]                        # left out of the file while it is off
enabled = true                     # `chief serve` shows the project in the browser (§14)

[push]                             # left out of the file while no command is set
command = "curl -s -H \"Title: $CHIEF_TITLE\" -d \"$CHIEF_BODY\" ntfy.sh/my-topic"
                                   # run through `sh -c` to reach the user (§13 "Reaching the user")
on = "attention"                   # attention (default) | updates: also a run's news that can wait
```

Routines are not in project.toml: they live in the database (§4, §13), since the coordinator,
the CLI and the dashboard all write them and `chief serve` stamps each run.

## 4. SQLite schema (`chief.db`, migrations in `src/db.rs`, `PRAGMA user_version`)

```sql
CREATE TABLE projects (
  slug        TEXT PRIMARY KEY,
  dir         TEXT NOT NULL,
  created_at  INTEGER NOT NULL,           -- unix ms
  archived_at INTEGER                     -- when `chief archive` put it away, §2; NULL = live (migration 14)
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
  resolved_reason TEXT,                   -- 'user' | 'coordinator' | 'merged' | 'inactive' | 'removed' | 'routine' (a finished run put away by its routine's next run, §13)
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
  stopped_early_at INTEGER,               -- a thread's last Stop while background work ran; its final report is due (migration 12)
  reason_before_removed TEXT,             -- the resolved_reason a reopened thread kept when a sync marked it removed; put back if the session is listed again, §5 (migration 13)
  legacy_cleaned  INTEGER NOT NULL DEFAULT 0, -- 1 = cleaned before migration 13, possibly by a removed verdict from before migration 6, §5 (migration 13)
  blocked_since   INTEGER,                -- when it got stuck on the user (rule 3, §5); NULL while it isn't (migration 17)
  blocked_told_at INTEGER,                -- when the coordinator's watcher last told of it, §5 "Telling the coordinator" (migration 17)
  runner          TEXT,                   -- 'agp' once agp watches the session for limits (it started it, or `agp bg watch` took it); NULL = plain claude, §5 "Limits and moves" (migration 18)
  prior_accounts  TEXT,                   -- JSON [{id, label, dir}]: the accounts agp moved it out of, oldest first; the first holds the job that owns its worktree, §12 (migration 18)
  moved_at        INTEGER,                -- when agp last moved it (unix ms) (migration 18)
  move_note       TEXT,                   -- 'moved from PB2 to PB4 after the 5-hour limit' (migration 18)
  moved_told_at   INTEGER,                -- when the coordinator's watcher told of that move (migration 18)
  limit_note      TEXT,                   -- the limit its job state says stopped it ('You've hit your 5-hour limit · resets …'); NULL when none (migration 18)
  limited_since   INTEGER,                -- when a sync first saw that limit (migration 18)
  limit_told_at   INTEGER,                -- when the coordinator's watcher told of it (migration 18)
  agp_state       TEXT,                   -- agp's state for it (`agp bg --json`): watching, limited, waiting, moving, ended, dormant (migration 18)
  routine         TEXT,                   -- routines.name of the routine this thread is a run of, §13; NULL for any other thread (migration 19)
  routine_told_at INTEGER                 -- when the coordinator's watcher told of this run's finish, §13 (migration 19)
);
CREATE INDEX sessions_by_routine ON sessions(project_slug, routine);   -- (migration 19)

CREATE TABLE routines (                   -- §13 (migration 19)
  id           INTEGER PRIMARY KEY,
  project_slug TEXT NOT NULL REFERENCES projects(slug),
  name         TEXT NOT NULL,             -- its handle, [a-z0-9-], at most 32 chars, never changed
  title        TEXT NOT NULL,             -- each run's thread carries it
  prompt       TEXT NOT NULL,             -- what every run does
  schedule     TEXT NOT NULL,             -- as written: five cron fields, an @shorthand, or `every 30m` (`src/cron.rs`)
  repo         TEXT,                      -- a repos[].name; NULL = the project's default repo
  model        TEXT,                      -- NULL = [threads] model
  effort       TEXT,                      -- NULL = [threads] effort
  enabled      INTEGER NOT NULL DEFAULT 1, -- 0 = paused
  notify       INTEGER NOT NULL DEFAULT 1, -- 1 = the coordinator is told when a run finishes
  created_at   INTEGER NOT NULL,
  updated_at   INTEGER NOT NULL,
  next_run_at  INTEGER,                   -- when it next fires (unix ms); NULL while paused, or when no day matches
  last_run_at  INTEGER,                   -- when its latest run started
  last_thread  TEXT,                      -- that run's sessions.name
  last_note    TEXT,                      -- why the last occurrence started no thread, or what the start warned of
  command      TEXT,                      -- the shell line it runs instead of a session, or before one, §13 "A command" (migration 21)
  approved_command TEXT,                  -- the words of `command` the user approved; it runs only while they are the same (migration 21)
  timeout_secs INTEGER,                   -- how long the command may run; NULL = 300 (migration 21)
  UNIQUE (project_slug, name)
);

CREATE TABLE routine_runs (               -- one run of a routine's command, §13 "A command" (migration 21)
  id           INTEGER PRIMARY KEY,
  project_slug TEXT NOT NULL,
  routine      TEXT NOT NULL,             -- routines.name; kept when the routine is deleted
  fired_by     TEXT NOT NULL CHECK (fired_by IN ('schedule','manual')),
  started_at   INTEGER NOT NULL,
  finished_at  INTEGER,                   -- NULL while it runs
  pid          INTEGER,                   -- the chief process that runs it
  status       TEXT NOT NULL CHECK (status IN ('running','ok','failed','timeout','lost')),
  exit_code    INTEGER,                   -- NULL while it runs, for a timeout, and for one that couldn't start; 128 + the signal for one a signal ended
  output       TEXT,                      -- the last 16 KB of what it printed, stdout and stderr together
  outcome      TEXT CHECK (outcome IN ('nothing','update','attention')),  -- what the command said with `chief routine outcome`
  headline     TEXT,
  details      TEXT,
  thread       TEXT                       -- the session started for its news (a check)
);
CREATE INDEX routine_runs_by_routine ON routine_runs(project_slug, routine, id);

CREATE TABLE routine_outcomes (           -- what a run said it came to, §13 (migration 20)
  id           INTEGER PRIMARY KEY,
  project_slug TEXT NOT NULL,
  routine      TEXT NOT NULL,             -- routines.name; kept when the routine is deleted
  thread       TEXT,                      -- the run's sessions.name; NULL for what a command's run found
  ts           INTEGER NOT NULL,
  outcome      TEXT NOT NULL CHECK (outcome IN ('nothing','update','attention')),
  headline     TEXT NOT NULL,             -- one line for the user, at most 200 chars
  details      TEXT,
  run          INTEGER,                   -- routine_runs.id, for what a command's run found (migration 21)
  told_at      INTEGER                    -- when the coordinator was told of a command's finding; set at once for one nobody is to hear of (migration 21)
);
CREATE INDEX routine_outcomes_by_routine ON routine_outcomes(project_slug, routine, id);
CREATE INDEX routine_outcomes_by_thread ON routine_outcomes(thread, id);

CREATE TABLE routine_points (             -- the values a routine tracks over time, §13 (migration 20)
  id           INTEGER PRIMARY KEY,
  project_slug TEXT NOT NULL,
  routine      TEXT NOT NULL,
  series       TEXT NOT NULL,             -- what the number is; one line per series
  ts           INTEGER NOT NULL,
  value        REAL NOT NULL,
  unit         TEXT,
  note         TEXT
);
CREATE INDEX routine_points_by_series ON routine_points(project_slug, routine, series, id);

CREATE TABLE events (
  id           INTEGER PRIMARY KEY,
  session_name TEXT,                      -- NULL if the hook could not match a session
  session_id   TEXT,
  project_slug TEXT NOT NULL,
  hook         TEXT NOT NULL,
  ts           INTEGER NOT NULL,
  summary      TEXT,                      -- short human line, at most 120 chars; never the whole payload (below)
  agent_id     TEXT                       -- the payload's `agent_id`: the subagent that made the call; NULL for the session itself (migration 16)
);
CREATE INDEX events_by_session ON events(session_name, id);   -- a thread's latest events, for the board's detail, §10 (migration 15)
CREATE INDEX events_by_agent ON events(session_name, agent_id, id); -- a subagent's latest call, for the detail's tree, §10 (migration 16)

CREATE TABLE reports (
  id           INTEGER PRIMARY KEY,
  session_name TEXT NOT NULL,
  ts           INTEGER NOT NULL,
  text         TEXT NOT NULL
);
CREATE INDEX reports_by_session ON reports(session_name, id); -- a thread's reports, for the board's report view, §10 (migration 15)

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
  tmux_window  TEXT,                      -- '<window id> <server pid> <socket>' of the tmux window `chief up` opened, §9 (migration 9)
  watcher_pid  INTEGER                    -- the newest coordinator watcher; older ones end, §5 "Telling the coordinator" (migration 17)
);

CREATE TABLE teammates (                  -- a session's subagents in its current process, §5, §6 "Subagents" (migration 11)
  session_name TEXT NOT NULL,             -- the lead: the sessions.name its hook matched (below)
  session_id   TEXT NOT NULL,             -- the lead's session id that hook carried; a record counts only for it
  agent_id     TEXT NOT NULL,             -- Claude Code's agent id, e.g. 'ahelper-886b99f57d4badec'
  name         TEXT,                      -- the Agent call's `name`; NULL when it had none, or none is known yet
  description  TEXT,                      -- the Agent call's `description`, for the board's detail (migration 16)
  recorded_at  INTEGER NOT NULL,          -- unix ms of the latest hook that recorded it
  busy         INTEGER NOT NULL DEFAULT 0, -- mid-turn: SubagentStart to SubagentStop, §5 "Background work" (migration 12)
  PRIMARY KEY (session_name, agent_id)
);
```

Every connection: `journal_mode=WAL`, a busy handler that retries a locked statement for 5 s of
real time (`db::BUSY_WAIT`), `foreign_keys=ON`. Not SQLite's `busy_timeout`: it adds up the sleeps
it asks for, not the time that passes, and with the bundled SQLite on macOS a hook held up by a
locked database waited about 12 s for its 5000 ms. Every hook opens the
database, so opening reads `user_version` first and a database at the current version takes no
write lock. One behind it is migrated in a `BEGIN IMMEDIATE` transaction that reads the version
again, so processes opening it at once migrate it once. Every other write transaction is
`db::immediate`'s `BEGIN IMMEDIATE`, which rolls back on an error, a failed `COMMIT` and a panic
unwinding through it alike: a long-lived connection left in an open transaction (the board's
refresher starts again after a panic, §10) would keep the write lock every hook waits on.

### Matching a hook to a session

1. `session_id` matches `sessions.session_id` → that row, if it belongs to the hook's project.
2. Else `session_title` matches `sessions.name` within the project and that row has no
   `session_id` yet → that row; store `session_id`. (A row with another `session_id` is not
   matched: the hook comes from a copy of the session.)
3. Else record the event with `session_name = NULL` and stop.

Matching, the event row and the session update run in one `BEGIN IMMEDIATE` transaction.

After `/clear` a session runs under a new `session_id`, which no row has until a sync finds it
under the row's `short_id` (§5); its title matches a row that has the old id, so rule 2 doesn't
apply either. A Stop in that window would lose its report (Stop carries no title). So before that
transaction, a Stop whose `session_id` no row has syncs the project first, as a board poll does
(`claude agents` in each account it uses, 5 s), when the project has a session the sync could
match (a `short_id`, not cleaned, not removed). Then it matches as above. A failed listing is
logged and the Stop is recorded unmatched. Any other hook in that window is recorded unmatched
until a sync learns the id; a Stop that matches never waits on `claude agents`.

## 5. Thread state and board groups

`src/state.rs` holds a pure function; everything else stores its result in `group_name` and
`waiting_reason`.

Hook updates (per matched session):

| Hook | Effect |
| --- | --- |
| SessionStart | `last_activity_at`; unless `source` is `compact`, `background_running = 0` and `stopped_early_at = NULL` (a new process runs none of the old one's work) |
| UserPromptSubmit, PreToolUse | `pending_permission = 0` (not from a subagent, below); provisional group `working` |
| PostToolUse | `pending_permission = 0` and update `cwd` (neither from a subagent); provisional `working` |
| Notification `permission_prompt` | `pending_permission = 1`, whoever's prompt it is (below) |
| Notification `idle_prompt` | nothing beyond `last_hook` |
| Stop | `pending_permission = 0`; store `last_assistant_message` in `last_report` and `reports` (skipped when empty); update `cwd` (shown only; a thread's branch comes from its job state on sync, below); `background_running` and the final report (below) |

Every hook above also sets `last_hook`, `updated_at` and `cc_fresh = 0`, re-derives the group, and
(except Notification `idle_prompt`) sets `last_activity_at`.

A subagent's own tool calls carry its lead's `session_id` (§1) and match the lead's row; the
payload's non-empty `agent_id` tells them apart. They set `last_hook`, `last_hook_at` and the rest
above (the process works), but never clear `pending_permission` or update `cwd`: they answer no
prompt of the lead's, and the subagent's cwd isn't the lead's. Rule 3 comes before rule 5, so a lead
waiting on a permission stays **waiting** while its subagents work. A Notification carries no
`agent_id`, so a subagent's own permission prompt marks its lead's row too; it clears at the lead's
next own UserPromptSubmit, PreToolUse or PostToolUse, at its Stop, when a poll sees it answered, or
when the process is gone (below), not when the subagent goes on. Other events (SubagentStart,
SubagentStop, SessionEnd, other notification types) are only recorded in `events`, apart from
`teammates` and the background work (below): when one changes `subagents_busy` or
`background_running`, it sets `updated_at` and re-derives the group. Coordinator Stops store
reports too.

An event's `summary`, whitespace collapsed and cut to 120 chars: for PreToolUse and PostToolUse
`<tool>: <what it acts on>`, the first of the tool input's `command`, `file_path`,
`notebook_path`, `pattern`, `url`, `query`, `description`, `to`, `skill`, `name`, `task_id`,
`path` that is a non-empty string (AskUserQuestion: its first question), a path inside the
payload's `cwd` given from there, or the tool name alone; for UserPromptSubmit the prompt; for a
Notification `<notification_type>: <message>`; for Stop the last assistant message; for
SessionStart the source, SessionEnd the reason, SubagentStart and SubagentStop the agent id.

Subagents (§1, §6 "Subagents"): in the same transaction, a matched session's hook keeps its
`teammates` rows, under the payload's `session_id` (a subagent's own hooks carry its lead's).
SubagentStart records its `agent_id` alone, and keeps a name already recorded (it fires again on
every resume); `agent_type` is never taken for a name. PostToolUse for `Agent` records
`tool_response.agentId` (else `agent_id`) with `tool_input.name` and `tool_input.description`; the name is left out when it is
missing or empty, or when `tool_response.name` differs from it (Claude Code registered another,
which could reach someone else; the agent id still works). A name goes to the latest agent
recorded with it (the others keep their ids, without a name), as Claude Code delivers it. A
SessionStart whose `source` is anything but `compact` (startup, resume, clear, or none) deletes
the session's rows: a new process has none of its old teammates (§1). A compaction keeps them,
and SubagentStop doesn't remove one (it fires after every turn of a teammate that stays); it
only ends its turn (below). Deleting a session row (a failed spawn) deletes its teammates too.

When `[ui] notify` is true, a thread's `permission_prompt` and a thread's Stop send a macOS
notification (`osascript`, spawned without waiting; skipped if missing or `CHIEF_NO_NOTIFY=1`).
Not the Stop of a routine's run: a run says what it came to itself, and only what needs the user
is sent (§13 "Reaching the user"), so an hourly routine doesn't notify hourly. A
`permission_prompt` is also pushed with the project's `[push]` command, when one is set.

Derivation, first match wins (inputs: the row plus the latest `claude agents` snapshot if fresh):

1. `resolved = 1` → **resolved**
2. `cc_state = failed` → **failed**
3. `pending_permission = 1` → **waiting**, reason `permission`; else `cc_status = waiting` →
   **waiting**, reason `question`; both unless the process is gone (below)
3a. `limit_note` set and no hook since `limited_since` (`registry::limited`) → **limited**: a
   usage limit stopped its turn (§5 "Limits and moves"). Claude Code blocks it too, but it waits
   on the clock or another account, not on the user, so it comes before rule 4.
4. `cc_state = blocked` → **waiting**, reason `question`
5. `cc_status = busy`, or background work (below) → **working**, unless the process is gone
   (below)

A thread that matches rule 3 (not resolved, not failed) is **stuck on the user**: its turn can't go
on until they answer. `state::blocked` says so, and every re-derive keeps `blocked_since`: the
time it got stuck, kept while it stays stuck, NULL once it isn't (migration 17 sets it to
`updated_at` for rows stuck then). Rule 4, a turn that ended on a question, isn't stuck: the
coordinator's idle notice carries it.

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
another thread's worktree. For a thread agp moved, whose job in its current account names no
worktree, the job that does is in an account it ran in before (`prior_accounts`, newest first,
`claude::prior_job_state`). Its `limit_note` comes from its job state in the account it runs in now
(an older account's job stays limited): `job_limit` (state `blocked`, `needs` starting "rate
limited", the text its `detail`), with `limited_since` = taken-at when first seen, or seen again
after a later hook (a turn that hit it again); both NULL once the job state says nothing of a
limit. Rows with neither id (not spawned yet) are left alone. Then re-derive and apply auto-resolve. Each row keeps a watermark,
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
`resolved_reason = 'removed'`; a reopened thread's kept reason moves to `reason_before_removed`.
When its worktree is gone too (job state names none, §12, or the
folder no longer exists) it also gets `cleaned_at` = taken-at and `cleanup_blocked = NULL`;
otherwise `cleanup_blocked = 'session removed outside chief; worktree remains at <path>'`, and a
later sync that finds that folder gone sets `cleaned_at`. The coordinator row is never marked.
A row with `removed_at` has no session left (`claude respawn` would fail), so it can't be woken,
reopened, messaged or shown in the pane (§6, §7, §9, §10), cleaned or not; the refusal says the
session was removed outside chief and to start a new thread.
If a later snapshot lists the session again, the sync undoes only what the verdict did:
`removed_at` cleared; `cleaned_at` cleared when it is no earlier than `removed_at` (set with the
verdict, or once its worktree was found gone; no `claude rm` runs on a removed row, §12); when the
reason is `removed`, `resolved` cleared and `resolved_reason` set back to `reason_before_removed`,
so a reopened thread stays reopened and isn't resolved again automatically (NULL for a thread
never resolved); `reason_before_removed` cleared; and the `session removed outside chief…`
`cleanup_blocked` cleared.

Before migration 6 a removed verdict on a resolved row set only `cleaned_at` (clearing
`cleanup_blocked`), and migration 6's backfill gave such a row no `removed_at`, so it kept
`cleaned_at` while its session lived on. It looks like a row `claude rm` removed, so migration 13
sets `legacy_cleaned = 1` on every row cleaned until then (`cleaned_at` set, no `removed_at`).
chief's own cleanup sets `cleaned_at` only after `claude rm` removed the session (§12), which then
disappears from `claude agents --all` (§1). So a sync whose snapshot lists a `legacy_cleaned` row's
session (by `session_id` or `short_id`) and was taken after its `cleaned_at` clears `cleaned_at`
and `legacy_cleaned`; the row keeps its resolution. A snapshot taken earlier may have run while
`claude rm` did, and proves nothing. Rows cleaned since are never flagged: their `cleaned_at` comes
from `claude rm`, from a removed verdict that set `removed_at`, or from a thread that never
started.

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
match a process-gone row either.

Claude Code fires no hook when the user answers a permission prompt: the next one comes when the
approved call ends, which can take minutes. Its status changes at once, so a sync clears
`pending_permission` when a non-stale snapshot finds a row `busy` or `idle` whose stored state is
fresh (`cc_fresh = 1`, no hook since the poll before) and says `waiting` with `pending_permission =
1`: both polls came after the prompt's Notification (which set `cc_fresh = 0`), and the earlier
one saw the session wait on it. The board then shows it working within one poll (2 s, §10). A
prompt whose status was never seen `waiting` (a subagent's, while its lead runs) keeps waiting until
the session's own next hook. Rules 1, 2, 4 and 6–8 still apply, so it shows as waiting (a
blocked question), review/landing (open PR), resolved, failed, or idle/asleep. A newer hook (e.g. a
respawn starting a turn) makes it count as running again until the next non-stale poll.

Board group order: Waiting on you, Working, Limited, Ready for review, Landing, Idle, Failed,
Resolved.

### Background work

A session's subagents and background commands run inside its process after its own turn ends
(§1), and the session's own turn says nothing about them. While a teammate works, Claude Code
even calls the session idle, so its coordinator gets an idle notice for a thread that isn't done.
chief counts them from the hooks, in the same transaction as the rest (§4 `sessions.subagents_busy`,
`background_running`, `stopped_early_at`; `teammates.busy`):

- **Subagents mid-turn.** SubagentStart marks its teammate row `busy` (recording the row first,
  as above); SubagentStop clears it. A SubagentStop chief missed (its hook gave up on a locked database,
  say) is caught at the session's own next Stop (no `agent_id`), whose `background_tasks` lists
  every subagent the session still has (§1): an async agent as a `subagent` under its agent id
  until its hand-back, a teammate as a `teammate` under a task id, idle or not. When that list
  has no `teammate` (nor a type chief doesn't know), every busy row whose agent isn't listed as a
  running `subagent` is cleared: it can't be mid-turn (with nothing listed but `shell`s, every
  row, as after a TaskStop). A listed teammate doesn't say which agent works, so then nothing is
  cleared: a teammate whose SubagentStop was missed stays busy until its next turn ends, a Stop
  lists no teammate, a new process starts (below), or the process is gone (background work
  doesn't count then). A Stop without `background_tasks` clears nothing.
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

### Limits and moves (`src/moves.rs`)

A thread whose account hits a usage limit stops mid-turn and sits blocked (§1); it doesn't go on by
itself. So threads run through agp (`[threads] runner = "agp"`, the default), which watches each
one (§7 "Thread spawn"). When its account hits a limit, agp acts per `on_limit`: `switch` stops it
and resumes the same conversation, in the same worktree and under the same ids, in the pool
account with the most room that shares `projects/` and trusts its folder (or waits for the reset
when none can take it); `wait` wakes it on the same account at the reset; `stay` does nothing. Its
first prompt there says it moved and that its background tasks and subagents stopped. A thread
whose account hits a limit shows **limited** (rule 3a) until agp moves it, it is woken after the
reset, or a sync finds its job going on.

chief follows a move from `agp bg --json` (`moves::refresh`, which asks only while the project
has a live thread with `runner = 'agp'`, and only an agp whose help lists `agp bg`). It runs first
in every refresh (`accounts::refresh`, and `refresh_row` for an agp thread), so the listings that
follow are of the right accounts, and in the coordinator's watcher (below). For each thread agp
lists (`follow`), it stores `agp_state`; once a move is done (never while agp says `moving`: the
new account may not list it yet) it points the row at the account it runs in now (no account of
its own when that is the project's, §3), appends the accounts before to `prior_accounts`, takes the
short id agp gives, and sets `moved_at` (from agp), `move_note` ("moved from PB2 to PB4 after the
5-hour limit"; "on request" for a manual move), `pid = NULL`, `cc_fresh = 0`, `ids_at` = now (a
listing of the new account from before the move can't judge it removed) and clears the limit. Each
move is recorded as a `Moved` event.

A respawn while agp moves a thread would start it again where agp just stopped it, next to the
copy agp resumes. So before chief revives a session agp watches (`wake_thread`, the pane, `chief
up`, `move_thread`) it settles (`moves::settle`): while `agp bg --json` says `moving` it waits (1 s
polls, up to 90 s, then an error), then follows the move. A thread that runs again after the move
isn't respawned at all. agp itself checks for a live process right before it resumes, and gives
up the move when one appeared. After `generated::rewrite` and before `claude respawn`, chief runs
`agp bg watch` for the session (`moves::rearm`): agp writes again the settings it merged with
chief's (the respawned session reads them) and brings back a watcher that ended. Its failure is
only logged.

The coordinator is watched with `on_limit = wait`, always (`moves::coordinator_run`): its threads
reach it by name, which only its own account carries (§1), so it is woken after the reset, never
moved. Its row gets `runner = 'agp'` but never moves (`follow` applies to threads only).

`move_thread` (§7) moves one now through `agp bg move`. Cleanup of a moved thread runs `claude rm`
in every account it ran in (§12).

### Telling the coordinator (`src/waiting.rs`)

A thread stuck on the user (above) can't report: its turn hasn't ended, so its coordinator gets no
idle notice, and the coordinator, idle itself, reads no board until the user writes to it. So the
board's Waiting on you was the only sign, and the user, watching the coordinator, saw nothing. The
coordinator's own hooks fix that with a watcher, `chief hook watch`, that Claude Code runs in the
background (`asyncRewake`, §1, §3) at each SessionStart and Stop of the coordinator:

- It stores its pid in `ui_state.watcher_pid`; the newest watcher wins, and an older one that finds
  another pid there ends (exit 0). So one watches per project, the one the latest turn started.
- Every second it reads the project's threads. A thread is **due** when it isn't resolved, cleaned
  or removed, its row is stuck now (`state::blocked`, so a `blocked_since` that outlived its block
  never raises a false alarm), has been stuck for at least 10 s (`blocked_since`; a prompt
  answered at once, in the pane, needs no word), and wasn't told since it got stuck (`blocked_told_at` NULL or older than
  `blocked_since`). In one write transaction, looking again, it sets `blocked_told_at` on every due
  thread, then prints a notice to stderr and exits 2, which wakes the coordinator with it. A thread
  is told once per time it gets stuck.
- Two more kinds are due the same way, and one notice tells every due thread of every kind,
  marking each in that transaction. **Moved**: agp moved it since the coordinator was told
  (`moved_at` set, `moved_told_at` NULL or older); the coordinator's subscription named the old
  process, so it must subscribe again at the new address. **Limited**: rule 3a for at least 2
  minutes (`limited_since`; agp, when it watches the thread, has moved it well within that) and not
  told since (`limit_told_at`).
- A fourth kind is due the same way. **Routine run finished** (§13, `routine_due`): a thread with
  a `routine` that reported (`last_report_at`) since the coordinator was last told
  (`routine_told_at`), isn't working, waiting or limited now, has no `stopped_early_at`, whose
  routine tells (`notify`), and whose run didn't say `nothing` (§13 "What a run leaves": a run
  that found nothing new is told to nobody; one that gave no outcome at all is told, since
  something may be wrong). Nobody subscribed to a thread `chief serve` started, so no idle notice
  carries its report. A run its routine's next run put away (`resolved_reason = 'routine'`)
  still counts; one a person or the coordinator resolved was seen. `<chief-routine>`: `- <name>
  "<title>" · routine <routine> · finished at HH:MM · <outcome>: <headline>` (for a run without
  an outcome, `· gave no outcome` and a `report:` preview line, ≤200 chars), then: tell the user
  in a line each, attention first; look closer with `read_routine` or `read_report` for those and
  for a run without an outcome; resolve a run that leaves nothing to do. What a routine's
  command found (§13 "News") has no thread, so it is due by itself (`routines::untold`: a
  finding with a `run` and no `told_at`) and rides in the same section: `- routine <routine>
  "<title>" · command · at HH:MM · <outcome>: <headline>`, the latest 10 spelled out under `-
  (N earlier findings of commands: read_routine lists them)`, all marked told; the text adds
  that such a line has no thread and that `read_routine` shows the run and what it printed.
- The notice is a section per kind, separated by a blank line. `<chief-waiting>`, a line per
  thread, `</chief-waiting>`, and what to do: `- <name> "<title>" · <permission|question> since
  HH:MM · wants to run <command>` (or `wants to use <call>`, or `asks <question>`, read from its
  events as the board's detail does, §10). `<chief-moved>`: `- <name> "<title>" · moved from PB2
  to PB4 after the 5-hour limit at HH:MM`, then: call `list_threads` for each one's address and
  subscribe again. `<chief-limited>`: `- <name> "<title>" · limited since HH:MM · <the limit> ·
  <what happens next>` (agp found no account with room and resumes it at the reset; agp is still
  moving it; agp isn't watching it, or it doesn't run through agp, so it waits until messaged),
  then: tell the user in a line each (the limit, when it resets, what happens next), offer
  `move_thread`, and message a thread agp doesn't resume after the reset. Each section says
  what to do with it, since the coordinator's brief (§8) only says to do what a notice says;
  the waiting one adds that a waiting thread can't hear a message, the routine one that
  runs that found nothing aren't listed and that a run isn't subscribed to.
- It ends (exit 0, nothing told) when its parent, the coordinator's process, is gone, or a minute
  before its hook `timeout` (a day). A coordinator idle longer than that hears of the next stuck
  thread at its next SessionStart or Stop.
- Each look reads the database only. A prompt is seen at once (its Notification), a question (rule
  3's `cc_status = waiting`) at the next board poll. No hook reports a limit or a move, so every
  10 s, while the project has a live thread, it syncs them itself (`Watch::sync`): agp's moves
  (`moves::refresh`), then every live thread's limit from its job state alone
  (`registry::refresh_limits`: files, no `claude agents`), writing only rows that changed. Its
  errors go to the hook error log.

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

Lookalikes count as taken. Wherever the guard asks whether another session has a name (the
identity check's "no other entry carries that name" below, a subagent's name or agent id, and an
Agent call's `name`, "Subagents" below), it compares the names' lookalike forms
(`guard::lookalike`: format characters such as U+200B–U+200F, U+2060–U+2064, U+FEFF and the soft
hyphen, and variation selectors, removed; then trimmed and lowercased). If Claude Code ever
matched names loosely, a lookalike could reach the other session, so this only ever adds a
denial; what the guard allows is still matched byte for byte.

Reachable (the identity check, for a thread under its name and for the coordinator under
`chief-<slug>`): the snapshot entry for the row's `session_id` (its `short_id` while the
`session_id` is unresolved; never an entry found by name) has a pid and carries exactly that name,
and no other snapshot entry, in any state, carries that name or a lookalike of it. Otherwise it is
asleep (no entry, or no pid), renamed (the entry carries another name, so the name may reach
someone else), or ambiguous (another entry carries the name or a lookalike too). A coordinator
with no row counts as not running.

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

A thread caller whose `session_id` matches no row (its new id after `/clear`, before a hook or a
sync stores it) is placed by the payload's `transcript_path`, which Claude Code keeps in the
session's account folder (`<folder>/projects/…`; for the default account `~/.claude`, or
`$CHIEF_CLAUDE_DIR` in tests): `..` components never match, and folders are also compared
canonically. When no thread of the project that isn't cleaned up runs in another account
(resolved and removed ones count: their sessions may still run), the caller runs in the
project's account, as before. Otherwise a transcript under the project's account folder places
it there; one under the folder of a thread's account places it in that account (the denial
above); anything else, or no transcript, leaves it unknown, and `chief-<slug>` → deny: "chief
doesn't know your session yet, so it can't tell whether you run in your coordinator's Claude
account, and chief-<slug>'s name doesn't reach across accounts. To answer a message from it, …";
the coordinator's exact socket address is allowed as for any thread. A transcript only ever
moves a caller chief doesn't know from the project's account to another or to unknown: a
caller matched by `session_id` keeps its row's account.

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
  recorded subagents → allow, unless another session answers to it (below; no session should,
  but one named like the id could get the message) → deny: "<to> is your subagent's agent id,
  but another Claude session has that name too, …; message not sent." A missing listing doesn't
  deny an agent id. `to` exactly a recorded name → allow, unless another session answers to that
  name (or a lookalike of it, above): the coordinator (`chief-<slug>`), any thread of the project
  (any state), or a session in the guard's fresh `claude agents` of the project's account and of
  the caller's own (a thread in another account, §3) → deny: "<to> is also the name of another
  Claude session, so a message to <to> could reach it instead of your subagent. Message the
  subagent by its agent id <id>." A listing that couldn't be taken → deny, saying so, with the
  same advice (the project's account failing is a guard failure, as always). In-process names
  win only while the subagent lives in this process (§1), so a name another session has could
  reach that session.
- A name or id that isn't plain (letters, digits, `-`, `_`, `.`), and the names `team-lead` and
  `main`, are never taken as a subagent's address: Claude Code could read `uds:…`, a ` [ref]` or
  the lead's own names as something else. Deny, saying to use the agent id.
- Anything else follows the caller's role (the table above): a subagent's message goes out under
  its session's address, so a thread's subagents may message its coordinator and the
  coordinator's its threads. `team-lead` and `main` from the session itself are denied there.
- Never another session's subagents: only the caller's own records are read. A `[ref]` matches no
  record and stays denied, with the bare-name advice.

At spawn, an `Agent` call whose `tool_input.name` equals (or is a lookalike of, above) the
coordinator's name, the name of any thread of the project (any state, from the database), or a
session in the fresh listings of the project's account and the caller's own is denied: "A Claude session is already named
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
a hanging `claude agents` together with the rest, and SQLite busy waits, which are bounded only per statement (5 s, §4; the guard opens the database,
which runs the migration check, reads the rows, and inserts at most one denial). The watchdog's
denial is logged to the hook error log, not to `guard_denials`: the database may be what is stuck.
`CHIEF_GUARD_DEADLINE_MS` shortens the deadline for tests (it can only deny sooner).

Every deny writes a `guard_denials` row (caller matched by `session_id`, `target` = raw `to`, or
an Agent call's `name`) and
returns the PreToolUse deny JSON above; an allow prints nothing. If the guard itself fails (bad
payload, DB or `claude agents` error, or a panic), it denies with the error as the reason (fail
closed), logs the error, and records the denial if the DB opens, with what the payload gives: the
tool, the target and the caller (by `session_id`, when its row can be read); a payload that
doesn't parse, or has no `tool_name`, is recorded as tool `unknown` with no target.

### The coordinator's own looking (`src/lookups.rs`)

The coordinator routes work and answers quick questions; what takes finding out is a thread's
(§8). It has Read, Grep and Glob, and web tools, and nothing in Claude Code keeps a "quick
look" quick, so its PreToolUse hook does (`hook::dispatch`, for `--role coordinator`; its
subagents' calls are its own). Unlike the message guard this is a rule of the house, not a
safety check: it fails open (an error is logged and the call goes on), and it records no
denial.

- **Where.** A Read, Grep or Glob is weighed by where it looks (`lookups::classify`): Read's
  `file_path`; Grep's and Glob's `path`, else the session's folder; for a Glob whose pattern
  is an absolute path, the folders before its first wildcard. `~` is the home folder, a
  relative path starts at the payload's `cwd`, `..` is resolved, and a path that exists is
  taken where it really is, so a link inside a repository that leads out is out. Inside the
  coordinator's own folder (`<dir>`): a Read is free, always. Inside the project's other
  folders (`lookups::roots`: its repositories, and every folder one of its sessions works in,
  a thread's worktree included), or what Claude Code keeps for this session and sends it to
  read (`lookups::claude_files`, from the payload's `transcript_path`: the folder beside its
  own transcript, where a long tool result goes; its folder's auto memory; the account's
  `skills/` and `plugins/`): allowed, and counted. Anywhere else: denied, with the path
  and what to do instead (start a thread that names the path, changes nothing and reports
  what it finds, with a small model and low effort; tell the user it is looking). Other
  sessions' transcripts are anywhere else.
- **How much.** A turn gets `lookups::PER_TURN` (6) lookups: every Grep, Glob, WebSearch and
  WebFetch, and every Read outside its own folder. The count is read from the session's
  recorded PreToolUse events (`lookups::made`, by their summaries, §4) since its last
  UserPromptSubmit or Stop; the call being weighed is among them, and so is one that was
  denied. One past the count is denied: answer with what you have, or start a thread for it.
  The next turn starts at none.
- Threads are not limited: looking is their work.

## 7. Coordinator MCP tools (`chief mcp --project <slug>`)

All tools return JSON text content. Errors are tool errors with a plain message.

| Tool | Input | Does |
| --- | --- | --- |
| `start_thread` | `title` (string), `task` (string), `repo?` (repos[].name or absolute path), `model?`, `effort?`, `account?` (an agp id or label from the pool, only when the user asks) | Picks the thread's account (below), which also checks trust of the repo path there, picks a unique name, rewrites `generated/*`, inserts the row, spawns the thread in that account, resolves its ids. Returns `{name, short_id, repo, model, effort, account, runner, address, next}` (`account` the agp label, null for the project's account; `runner` `agp` when agp watches it for limits, else `claude`), plus `account_note` when the choice skipped accounts or fell back, and `warning` when it kept a session whose spawn reported a failure (adopted from a listing, or already attached by its hooks: `short_id` null then) or couldn't store the started session's ids |
| `list_threads` | `include_resolved?` (bool, default false) | Refreshes from `claude agents` in every account the project uses (stored rows plus a `warning` naming what failed), returns `{threads: [...]}` in board order: `name, title, group, waiting_reason, asleep, running, branch, pr_url, last_report_preview (≤300 chars), last_activity` ("5m ago"), `cleaned`, `account`, `address`, and when set `limit` (the limit it waits on, group `limited`) and `moved` (its `move_note`); `running` is its background work (§5), e.g. `"1 subagent"`, or null. Resolved and cleaned threads are left out unless `include_resolved` |
| `read_report` | `name`, `history?` (int, default 1, max 20) | `{name, title, group, reports: [{at, text}]}`, newest first, full text; with background work (§5) also `running` and a `note` that the thread isn't finished and reports when it is |
| `wake_thread` | `name` | Refreshes from its own account; if no live pid, settles a thread agp watches (§5 "Limits and moves": waits while agp moves it, then follows the move) and, when it moved and runs already, returns it without a respawn; else `agp bg watch` for an agp thread, then `claude respawn <short_id>` in its account, poll its account's `claude agents` up to 20 s for a pid (each poll bounded by the time left, so a hanging listing ends the wait at 20 s; database busy waits are bounded only per statement, §4). Returns `{name, alive, respawned}`, plus `address` once alive and, after a respawn that gave a thread in another account a new address, `next` (subscribe again there); a thread that moved also gets `moved` (its note), `account` and `next`. A cleaned thread is a tool error: "<name> was cleaned up; start a new thread (its reports are still readable with read_report)"; so is one still removed outside chief after the refresh, with §6's removed message |
| `resolve_thread` | `name`, `reopen?` (bool) | Sets or clears `resolved` (`coordinator`). Returns `{name, resolved, group}`. Reopening a cleaned thread, or one removed outside chief, is the same tool error as `wake_thread`'s (`registry::set_resolved` refuses it for every caller) |
| `move_thread` | `name`, `account?` (an agp id or label from the pool) | Needs an agp with `bg`. Settles the thread (§5 "Limits and moves"); a named account must be in the pool, trust its repo (placement's pinned rules, §7 "Thread account") and not be the one it runs in. Then `agp bg move <session id> [--to <id>] --json` (bounded, 120 s): agp stops it and resumes it there (or in the other pool account with the most room). chief follows the move from `agp bg --json`, else from the move's answer, asks agp to watch it (`runner = "agp"`), refreshes its new account and returns `{name, alive, address, moved, account, next}` (subscribe again). Its description says agp moves limited threads itself: only when the user asks, or for a limited thread agp didn't move (§8) |
| `stop_thread` | `name` | Refreshes from its own account; with a live pid: when `ui_state.pane_target` is this thread, clears it and calls `pane::request_switch` (a pane showing it would respawn it), then `claude stop <short_id>` in its account. Returns `{name, stopped: true}`, or `{name, stopped: false, note}` for one with no process. The turn it was in ends without a report; its conversation, worktree and branch stay, and `wake_thread` brings it back. A cleaned thread or one removed outside chief is the same tool error as `wake_thread`'s. Its description says to use it only when the user asks or a thread is off track and a message didn't help (§8) |
| `list_routines` | none | `{routines: [...]}`, each a routine's row (§4) plus `schedule_text` ("Weekdays at 09:00"), `next_run` and `last_run` (local `YYYY-MM-DD HH:MM`, null when none), `kind` (`session`, `command` or `check`), `needs_approval`, `state` (§13 "State": `key`, `label`, `line`, `at`, `needs_user`), `last_exec` (its command's latest run: `id`, `status`, `exit_code`, `fired_by`, `started_at`, `started`, `finished_at`, `duration_ms`, `line`, `outcome`, `headline`, `details`, `thread`; null before any), `last_finding` (what its last run said: `outcome`, `headline`, `details`, `at`, `when`, `thread`, `run`; null before any) and `series` (up to 4 of what it tracks, most recently recorded first: `name`, `unit`, `latest`, `previous`, `at`, and its 30 latest `points`); and `scheduler`, a line saying `chief serve` isn't running and how the user starts it, whenever it isn't (on every routine tool's answer but `delete_routine`'s and `run_routine`'s) |
| `create_routine` | `title`, `schedule`, `prompt?`, `command?` (one of the two at least), `timeout_secs?`, `repo?`, `model?`, `effort?`, `notify?` (default true), `enabled?` (default true), `name?` (default: made from the title) | Validates (§13 "A routine") and stores it with its next run. Returns the routine as `list_routines` shows it, with `notes` (its notes file) and, for one with a command, `approval`: the command doesn't run until the user approves it, which the coordinator can't, and where they do (§13 "Approval") |
| `update_routine` | `name`, and any of `title`, `prompt`, `command` (an empty string removes either; one must remain), `timeout_secs` (0 = the default), `schedule`, `repo`, `model`, `effort` (an empty string goes back to the project's default), `notify`, `enabled` | Changes only what is given; `enabled: false` pauses, `true` resumes from now. A changed command waits for approval again (`approval`). Returns the routine |
| `delete_routine` | `name` | Removes the routine; its runs' threads and reports stay. `{name, deleted: true, note}`. Its description keeps it for when the user asks |
| `run_routine` | `name` | A run now, outside the schedule, which doesn't move (§13 "A run"). A session: `{routine, started: true, thread, next}`. A command, which runs detached: `{routine, started: true, run, note, next}`. `{routine, started: false, note}` while its last run still works or its command waits for approval |
| `read_routine` | `name` | One routine in full, without a thread: what `list_routines` shows, plus `notes` (its `NOTES.md`, the first 8 KB, with `notes_file` and `notes_cut` when it goes on), `findings` (its 10 latest: `outcome`, `headline`, `details`, `at`, `when`, `thread`, `run`), `series` with each one's 60 latest `points` (`[at, value]`), `runs` (its command's 5 latest, as `last_exec`, each with `output` and `output_cut`: the latest run's whole 16 KB, 2 KB of the earlier ones) and, with a command, `log_file` |

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
succeed without it, chief reads the row first: a `session_id` there means a hook of the session
already attached it (only a running session's hooks store one, §4), so it started, and the row is
kept and the call succeeds with `short_id` null and a `warning` that quotes the failure (sync
then matches it by session id). Otherwise the row is deleted and the error returned; a row that
can't be read is kept, and the error says so. The read and the delete are one transaction
(`db::delete_unattached_session`), so a hook that attaches the row meanwhile keeps it. When every listing
fails, chief can't tell: the row is kept without a short id (hooks attach it by title if the
session did start) and the error says so. Failing to resolve the session id is not an error
(hooks match by title, sync by short id). Once the spawn returned an id (or a session was
adopted), the session runs, so nothing after it fails the call, which the coordinator could
retry into a duplicate: a database error while storing the ids (`database is locked`, say) is
logged and the call succeeds with a `warning` that says not to start it again; the row is kept
for hooks to attach by name, and the reply is built from what chief knows. A call the client
cancels (MCP `notifications/cancelled`, or the server shutting down) starts nothing: rmcp cancels
the request's token but lets the handler run on the blocking pool, so `start_thread` checks the
token right before the spawn, deletes its row and returns "start_thread was cancelled …". After
the spawn a cancellation changes nothing: the thread runs and its row stays. The other tools
don't check it: none of them starts anything. `next` is
a literal SendMessage input with no `message` key, `{"to": "<address>", "notify_when_idle": true}`
(verified: omitting `message` is a valid pure subscription): the coordinator subscribes right after
starting a thread, which is how it hears the thread finished. (M5 live run: told "with no message",
and again told to pass `""`, Haiku emitted `"message": ,`, which is invalid JSON.) For a thread in
another account `next` adds that names don't reach it there, that the address changes when it is
woken, that idle notices do work at such an address (Claude Code says it doesn't know whether
they are supported) and that the thread can only reply to a message from there, not send one
first; when its process isn't listed yet, `next` says to get the address from `list_threads`.

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
pool, the usable accounts are those agp has a login for (`token` isn't `"Missing"`, §1: a
thread there would only ask to log in), not `limited` (an object, or `true`; `null` and `false`
are not limits), with 5-hour and weekly utilization below
`max_usage` when it is set, best agp `score` first (`agp pick` ignores `--accounts`/`--exclude`,
§1, so chief never asks it). For each in turn chief asks agp for its folder (`agp exec <id> --
sh -c 'printf %s "${CLAUDE_CONFIG_DIR-}"'`; empty = the default account; every `agp exec` runs
with chief's own `CLAUDE_CONFIG_DIR` removed, since agp leaves it unset for the default account
and the MCP server inherits the project's) and checks the repo's
trust in that account's `.claude.json`; the first that trusts it gets the thread, and the ones
skipped (untrusted, with the `cd <repo> && agp exec <id> -- claude` that fixes it; agp failing)
are listed in `account_note`. When none is usable or none trusts the repo, or `agp status`
fails, the thread runs in the project's account and `account_note` says why. `account?` pins
one: it must be in the pool (by id, else label), have a login ("can't run it in <label>: agp
has no login for it (run `agp add <id>` once)") and trust the repo, or the call fails; limited
or over `max_usage` only adds a note (the thread waits for the reset). chief chooses the
account once, at the start; after that only agp moves a thread, at a limit or on `move_thread`
(§5 "Limits and moves"). `chief init`
(§2) and the board's settings screen (§10) check every pool account's trust of every repo ahead
of time and offer the fix for each pair, so `start_thread` rarely has to skip one.

The tool logic lives in `src/tools.rs` (the routine tools in `src/routines.rs`) as plain functions
over a context (connection, config,
`Claude` in the project's account, `Agp`, project dir, trust file, clock, timeouts; the account
choice is `src/placement.rs`). `tools::with_project` builds that context for a project as it is
now, for `chief mcp`, `chief serve` and `chief routine`; `src/mcp.rs` adapts the tools to `rmcp` and runs
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

With `[threads] runner = "agp"` (the default), an agp whose `agp --help` lists `agp bg` (asked
once per binary and process, stdin closed, 5 s), and a pool account chosen (§7 "Thread account"),
agp starts it instead: `agp --as <id> --on-limit <on_limit> [--accounts a,b] [--max-usage n] --
--bg …` with the same `claude` arguments and cwd, and without `CLAUDE_CONFIG_DIR` (agp sets the
account's). agp passes claude's output through, starts its watcher detached (its own session,
stdio on /dev/null, so the timeout below isn't held up) and exits with claude's code; it merges
its own settings into `--settings` (an overlay, the shared memory folder) and writes them again on
every `agp bg watch`. A thread in the project's account (no pool account chosen) is started by
`claude` and then handed to agp with `agp bg watch <short id> --on-limit …` (best effort). Either
way the row gets `runner = 'agp'` once agp has it, and its brief (§8) says it may move. With
`accounts = []` there is nowhere to switch to, so `switch` is passed as `wait`. Without such an
agp, or `runner = "claude"`, threads start as before and nothing watches them for limits.

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
inserted is removed only when listings show no such session and no hook stored a session id in
it (in one transaction, as for a thread), and kept when chief can't tell or a hook did. A
kept row (no `short_id`) is adopted by the next `up` when `claude agents` lists its session (by
`session_id` if a hook stored one, else a background session with exactly the name whose `cwd`
is the project folder, compared canonically), instead of refusing the name as taken. A live
session with the name in another folder (another chief home's coordinator, or the user's own) is
never adopted: the name counts as taken. If it exists but `claude agents` shows no pid, `claude respawn` it and wait up to 10 s for
a pid. Otherwise it is left alone.

A coordinator whose session was removed outside chief (`claude rm`, or from `claude agents`) has
nothing to respawn. When the respawn fails and a listing of its account that succeeds has neither
its `session_id` nor its `short_id`, chief clears the row's session (ids, pid, Claude's state,
runner, limit) and starts a new coordinator under the same name, with a warning: the conversation
is gone, while the row's reports and events, the threads and `memory/` stay. Any other respawn
failure is an error, as before.

With `[threads] runner = "agp"` and an agp that has `bg`, a coordinator just started, respawned,
or found running without `runner = 'agp'` is handed to agp with `agp bg watch <short id>
--on-limit wait` (best effort, §5 "Limits and moves"): after a limit it is woken on its own account
at the reset, never moved.

## 8. Prompts (files in `src/prompts/`, included with `include_str!`)

- `coordinator.md`: the routing rules and little else, under 7,000 characters. The
  coordinator runs at low effort and skims a long brief (at 14,000 characters it did), so the
  brief holds what every turn needs, and what only some turns need is said where it is used:
  in a tool's description and its parameters' (Claude Code loads the chief tools' on demand,
  through its tool search, seen live), in a tool's answer (`next`, `approval`, `scheduler`),
  in a refusal (the guard's, §6; the lookup limit's), and in chief's own notices (§5 "Telling
  the coordinator"), each of which says what to do with it. The brief has:
  - Its role: it talks with the user and routes, tracks and reports; it never does the work,
    so the user can always reach it.
  - "Route every message": a quick question, from the board, the reports, memory or one or
    two reads → answer; a question that takes finding out → a lookup thread, never itself and
    never a subagent of its own, since the user can't talk to it while it looks and what it
    reads fills the conversation: the task says what to find and where, that it changes
    nothing and that its report is the answer, with `haiku` or `sonnet` at `low`; it answers
    from the report, saves what is worth keeping to memory and resolves the thread without
    asking; something said earlier that memory doesn't have → it answers what it can and
    starts a lookup thread over Claude Code's transcripts in the same turn, rather than
    saying it doesn't have the conversation or asking the user to repeat it, and the thread
    saves what it finds to memory; new work → a thread, one per unrelated task; a follow-up
    → the matching thread; work to repeat or something to watch → a routine; how the user
    wants the project run → follow it and save it to memory. Then: say which thread got
    what, keep `max_parallel`.
  - "Threads": `task` is a thread's whole brief, and `next` in `start_thread`'s answer is
    how it hears the thread finished; a follow-up goes with `SendMessage` to the thread's
    exact `address` with `notify_when_idle`, waking it first if asleep; one line says it can
    message its own subagents by the name or agent id the Agent tool gave them, and that
    they report to it (§6 "Subagents"); at an idle notice it reads the whole report with
    `read_report`, and when that says the thread still runs subagents or background commands
    the notice isn't a finish: it says nothing yet and doesn't subscribe again (the notice
    would come straight back, §1), as the thread reports when they finish (§5 "Background
    work"); it resolves a thread when the user agrees or its pull request merged, a lookup
    thread once it passed on what it found; removing worktrees and branches is the user's
    call (board `x`, `chief clean`, `chief gc`), so it never offers to; a `<chief-…>`
    message from chief says what happened and what to do, and it does that.
  - "Routines": work chief repeats on a schedule on this machine (`create_routine`); a
    prompt (judgement each run), a command (whatever a script can decide: no session, as
    often as every minute) or both (a check that gets a session only for news), chosen by
    what the work needs; a command it writes or changes waits for the user's approval, so
    it shows them the command and where to approve; `read_routine` answers a question
    about something followed without starting anything; Claude Code's own scheduling (the
    `schedule` skill, remote triggers) is never used, being cloud and blocked (§3); the
    routine tools' descriptions say the rest, to be read before writing one.
  - "Looking and memory": a quick look in its own folder and the repositories, which chief
    keeps quick (§6 "The coordinator's own looking"), no shell; memory in `memory/` with
    `MEMORY.md` as the index, saved when the user asks and on its own; it edits only
    `memory/` and `instructions.md`.
  - "Style": brief, what happened first.

  What left the brief, and where it is said now. `start_thread`'s description: threads
  create their own branch, so the task names one only when the user did; its `account`
  parameter: only when the user names one. `next` (§7): the literal subscription, and for
  a thread in another account that names don't reach it, that its address changes when it
  is woken, that idle notices do work at a `uds:…` address whatever Claude Code says, and
  that it can only reply to a message, not send one first. `move_thread`'s and
  `stop_thread`'s descriptions: only when the user asks, or for a limited thread agp didn't
  move, or one off track that a message didn't bring back. The notices (§5): what to do
  about threads waiting on the user, moved or limited, and routine runs that finished.
  `create_routine`'s description and parameters: what the user's words for a routine sound
  like, to prefer a check to a prompt that would find nothing most times, to show a command
  and where it is approved and not run it before, to say the schedule in words and when it
  first runs; how to write a prompt that stands on its own (a run sees neither the
  conversation nor earlier runs and can't ask anyone; chief tells every run how runs work,
  so the prompt says the task, what counts as news and when the user wants interrupting);
  how a command speaks (`chief routine value`, `chief routine outcome`,
  `CHIEF_ROUTINE_DIR`, only what changed is told); cheap runs (`haiku` or `sonnet` at `low`
  for a lookup, no more often than the thing changes). `read_routine`'s description: it may
  edit a routine's notes. `scheduler` in a routine tool's answer: `chief serve` isn't
  running, tell the user.
- `thread.md`: you are one thread of project <name>; read `<dir>/memory/MEMORY.md` first; project
  instructions follow; work on your own branch, except for a task that only asks to find
  something out, which changes nothing and whose report is the answer; of other sessions message only the coordinator,
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
by-name line, the by-address line for a thread in another account, or, for a thread agp may move
(`runner` agp with `on_limit = "switch"`), a line that holds in either account: it needn't message
the coordinator to report, and it answers a message by replying to the exact `from` address in its
header (`chief-<slug>` or `uds:…`); chosen at spawn, which a move keeps). Both get
`instructions.md` appended under `## Project instructions` when it isn't empty.

The coordinator's UserPromptSubmit hook runs `claude agents` (≤5 s; stored rows if it fails, logged),
syncs, and returns as `additionalContext`:

```
<chief-board as-of="HH:MM">
Waiting on you:
- <name> "<title>" · routine <routine> · waiting: permission · 1 subagent running · limited: <limit> · asleep · branch <b> · PR <url>
  report: <≤200-char one-line preview>
…groups in board order; resolved threads left out; "No open threads." when empty
</chief-board>
<chief-memory>
<MEMORY.md, or (empty)>
</chief-memory>
```

The whole context stays under 8 KB: previews shrink first (200, 100, 40, none), then memory is cut
with a marker, then the board itself. The memory marker (36 bytes) takes the place of `(empty)`
(7 bytes), so it is used only when the cut context fits; a board that leaves less room than that
is cut instead, and memory is left out.

### A brief that went stale

`claude respawn` starts a session again with the options of its first start (§1), the
coordinator's `--append-system-prompt` among them. So a coordinator keeps the brief it was
created with for as long as its conversation lives: a new chief, a new goal or new
instructions never reach it that way. Its tools change only when its process restarts,
since its MCP server starts with it (`src/brief.rs`).

- When chief creates a coordinator's session it keeps the brief it gave it in
  `<dir>/generated/coordinator-prompt.md` (`generated::save_coordinator_prompt`; a respawn
  doesn't touch it, and the coordinator can't edit `generated/`).
- A hook can't carry the brief: Claude Code cuts what a hook adds to a session's context at
  about 10,000 characters and leaves a preview with a file path, and a brief with the
  project's instructions is longer (seen live: the coordinator opened that file one time and
  not the next). So chief writes the brief as it reads now to
  `<dir>/generated/coordinator-brief.md` and says to read it (`brief::notice`): `<chief-brief>`,
  that the brief in its system prompt is out of date, to read that file in full before
  anything else and before answering the user, and that it replaces the one it started with.
  Under 1,000 characters.
- The notice is due while the brief chief would give now is neither the one the session was
  created with nor the one it last read. It is said at the coordinator's SessionStart
  (`hook::keep_current`) and before the board in every UserPromptSubmit context, until the
  coordinator reads the file: the PostToolUse of that Read (its own, not a subagent's) keeps
  a copy of what it read in `generated/coordinator-brief.read` (`brief::note_read`). What
  empties its context forgets the reading (`brief::forget_read`): a SessionStart that isn't a
  `resume`, and a SessionEnd whose reason is `clear`. A session created with the current
  brief is never told.
- **Old tools.** A coordinator's `chief mcp` is the chief that was installed when its process
  started, and `/clear` keeps the process. Its process's start is its last SessionStart with
  source `startup` or `resume`. Its own hook notes each in `generated/coordinator-started`
  (`brief::note_start`), whatever row the event matched: after a `/clear` a restart's
  SessionStart can carry a session id no row has yet and be recorded unmatched (§4; seen
  live, where a restarted coordinator went on being told its tools were old). For a process
  that started under a chief that noted nothing, its events say (§4). When the chief
  binary's modification time is later than the later of the two (`brief::stale_tools`),
  every UserPromptSubmit context starts with `<chief-update>`: that its tools are the old
  version, and to tell the user once that `chief down <slug> && chief up <slug>` restarts
  it with the new ones and keeps the conversation.

## 9. Layout (`chief up`: Rex, tmux, chief's own, or the user's own)

The layout is two panes side by side: `chief pane` on the left, `chief board` on the right.
Nothing in it depends on a multiplexer: the board switches the pane by signalling it (below), so
any two terminals or splits work. `chief up` opens it for the user with the first that applies
(`--layout builtin`: 3 straight away):

0. Inside a Rex block (`REX_SESSION` and `REX_BLOCK`, which Rex sets in each block's
   environment) on a terminal, when `rex list --json` succeeds: the user's own block becomes the
   conversation. `layout::open_beside` splits a block labelled `chief-<slug> board` right of it
   at 32 % (`--focus=false`) in the user's session, or reuses one that still runs there (an
   ended one is closed and replaced); `up_with` returns `Where::Here`, and `chief up` (or the
   launcher, §10a) then runs `chief pane` in this same terminal with `OnQuit::Board`: when the
   pane ends, by `q` or a hangup, it closes that board block and the user's block is back at its
   shell. No session is created, nothing is stored in `ui_state`, and no app has to switch or
   focus anything, so this works with several Rex apps connected. A failure here prints why and
   falls back to 3 (else 4), not to a new session.
1. Rex, when `rex list --json` succeeds (installed, server running): the Rex session below.
2. tmux, when `$TMUX` is set (chief runs inside tmux): a tmux window, below.
3. Neither, on a terminal: the built-in layout (below), in this terminal: `up_with` returns
   `Where::Builtin` and `chief up` (or the launcher, §10a) runs it. `chief down` has nothing to
   close then, but ends it as it ends any pane.
4. Without a terminal: it prints why and the two commands, `chief pane --project <slug>` and
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
-- <cmd>`, then, beside an anchor, `rex api call session.resize_pane '{"session_id", "direction"
("horizontal" or "vertical"), "before_block_id", "after_block_id", "ratio"}'` with the share of
the block left of or above the border (Rex 0.1 records every split `rex run` makes at half,
whatever `--ratio` says; a failed resize is ignored, the block is there at half); `rex block ls -s <s> --json`; `rex block call -b <b> process` (running while it lists a
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
   with `--focus=false`. `up` stores the new id. Any error once the session exists (Rex's
   rename of its first block, tmux's mark of its pane, the `board` block, `show`, storing the
   id) kills it, best effort, before the error is reported (`mux::kill_on_error`): a session
   whose id is never stored is nobody's, the next `up` would make another, and a `q` in its
   pane would close the stored layout.
2. Both blocks there and running: reuse.
3. One running: close the other if it is there but ended, and run it again beside the running one
   (`board` right of `main` at 32 %, or `main` left of `board` at 68 %).
4. Neither running: run `main` in a new window of the same session first, then close the dead
   `main`/`board` blocks (closing the last block first could end the session), then add `board`
   beside the new `main`. The session may hold the user's blocks, so `open` never kills it.

`close` (for `chief down`): nothing stored or the id gone → nothing to close. A session holding
only `main` and `board` is killed and the stored id cleared; one that holds other blocks too (the
user added them) keeps running with those, and only `main` and `board` are closed, `board`
first: `main` runs the pane, which may be the one closing the layout (its `q`), and closing its
block hangs it up.

Then focus `main` and `show` the session. Focus is best-effort: Rex refuses it as a client action
when several apps are connected, and failing there would lose the id of a session just made, so
the next `up` would make another. When Rex's `show` returns false, `up` prints
`rex -C <app> do session.select session_id=<id>` and says to pick the session in the app.

`chief pane` loop (`src/pane.rs`). In a Rex block (`REX_BLOCK` set, stdin a terminal) it first
runs the pane as a new process, `chief pane --project <slug>` with `--layout`, or `--board <block
id>` for `OnQuit::Board` (a hidden flag), under argv0 `claude`, unless it already runs so: Rex
picks a block's agent icon from its foreground process's argv0, and settles it once per process.
Verified live: a block whose process was `claude` from the start got the Claude icon; one that
renamed itself in place (exec, same pid and executable) didn't, nor did one sending
`app=claude-code` reports. The pane's own `claude attach` runs on chief's pty, out of Rex's sight.
As a shell runs a command, the new process gets a process group of its own and becomes the
terminal's foreground (both it, before exec, and this process set it, SIGTTOU ignored, so it
never reads the terminal from the background); this process waits for it, passes SIGTERM, SIGHUP
and SIGINT on, takes the terminal back for its own group and exits with its code. Every caller
runs the pane last, so nothing after it is skipped; when it can't start, the pane runs in this
process, without the icon.
While it relays an attach there, it also reports the shown session's state as Claude Code would
(`rex_status`, OSC 7501 `state=…:app=claude-code[:kind=…]:title=<base64>[:msg=<base64>]`): from
the session's row, read again every second and sent when it changes; working → `working`,
waiting → `blocked` with `kind=permission|question`, review, landing and resolved → `done`,
failed → `error`, idle → `idle`, unless its last message (`last_report_at`) came after the pane
attached it: a turn that ended on screen is `done`, as Claude Code reports one, or `blocked` with
`kind=question` when that message ends with a question mark (Claude Code's own simplest rule); the
title is the thread's title (`Coordinator` for the
coordinator), the message its background work (`3 subagents running`). The relay writes a report
only between Claude's escape sequences and UTF-8 characters (`rex_status::Boundary`); when the
attach ends, an ST and `state=clear` end the record. Outside Rex the pane neither renames itself
nor reports. On start it then catches its signals (below), then records its pid
and start time in `ui_state.pane_pid` / `pane_started` (`proc::start_time`: `proc_pidinfo` on
macOS, `/proc/<pid>/stat` on Linux); on exit it clears them, with `pane_showing`, only while they
are still its own. It writes and clears `pane_showing` only while `pane_pid` is its own pid (or
NULL, for a pane that couldn't record itself: `db::set_pane_showing_as`), so an older pane still
running neither overwrites nor clears what the newest one shows; §12 would otherwise take that
thread for one nobody shows.

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
4. Drain queued keys (`q` or Ctrl+Q quits), then relay `claude attach <short_id>` (below) in the
   target's own account (§3: a thread in another account is attached, polled and respawned
   there) until it ends, then clear `pane_showing`.
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
   Below that, `Next time, Ctrl+Q quits at once, even inside Claude.`

Relay (`src/relay.rs`). attach runs on a pseudo-terminal of its own (`embed::Pty::open`: the
child leads its own session with the slave as its controlling terminal, keeps the pane's `TERM`
and `TERM_PROGRAM`, and has SIGTSTP/SIGHUP/SIGINT/SIGTERM at their defaults; both sides of the
pty are close-on-exec from the moment they are open, so no other child chief starts can keep
the terminal open after attach), of the pane
terminal's size, resized on SIGWINCH. One thread `poll`s stdin and the pty's master
(non-blocking) in slices of at most 50 ms, looking at the signal flags between them, and copies
bytes both ways untouched: Claude draws on the user's terminal as if it had it (its scrollback,
selection and rendering are the terminal's), its queries reach that terminal and the answers come
back as input. Keys attach hasn't taken yet are queued and written as the pty takes them, so a
long paste into a busy Claude never deadlocks the relay. There is no emulator: chief reads the keys
only as far as Ctrl+Q, in each encoding a terminal sends it in: 0x11; the kitty keyboard
protocol's `CSI 113;<mods>[:<event>]…u` with Ctrl and no other modifier but Caps Lock or Num
Lock (a press quits, a repeat or release is dropped), once Claude turns the protocol on; and
modifyOtherKeys' `CSI 27;5;113~`. Inside a bracketed paste (`CSI 200~` … `CSI 201~`) nothing is
a key. The start of an escape sequence cut off at the end of a read waits up to 10 ms for its
rest, then goes on as it is (a lone Esc is a key). Claude Code doesn't bind Ctrl+Q; its own way
out of an attach is Ctrl+C twice, then twice more in its agent view, and lands in step 6.
The relay ends when attach closes its terminal (then it gets 500 ms to be reaped, or is ended and
counts as failed), when attach exits with its terminal still held open, or when chief ends it:
for a switch, for Ctrl+Q or the board's quit request (the loop then stops as for `q`) or for a
hangup (SIGHUP or SIGTERM,
POLLHUP on stdin, a read of 0 or a failed write). chief ends it with SIGTERM to its process
group, 2 s, then SIGKILL, and reaps it (`Pty::end`), reading and dropping its output meanwhile:
on macOS a process can't finish exiting, even killed, while output it wrote sits unread on its
terminal, so an attach killed mid-draw would leave the pane waiting forever. SIGTERM leaves its
session running. chief keeps the pane's terminal in raw mode the whole time.

`q` or Ctrl+Q while chief has the terminal ends the loop, as Ctrl+Q does during a relay; so do
the board's Ctrl+Q (below), the block being closed and the session killed. With `--layout` a `q` or Ctrl+Q (not a hangup) then closes the layout as `chief down` does
(`project::close_layout`: the Rex session, or the tmux window, or just chief's two blocks when
the user added others), after the pane cleared its rows and restored the terminal, since closing
hangs up the pane's own terminal; no session stops. It writes the lines `close_layout` reports
ignoring errors: that terminal may have hung up already. Then the pty hangs up and the kernel
sends SIGHUP to the session leader, chief; `claude attach` leads a session of its own. chief
catches SIGHUP (and SIGTERM) with a handler that only sets a flag, without `SA_RESTART`, so the
default action can't end it before cleanup: the relay sees the flag and ends attach as above; the
loop then stops. On exit `pane_showing` is cleared (a stale one would keep §12 cleanup away from that
thread).

Keys: while chief has the terminal it reads them itself, with `poll` on stdin in slices of at most
200 ms and `read`, looking at the signal flags between slices (a signal also interrupts a
slice). Only `q`, Enter (CR or LF), Ctrl+C (0x03) and Ctrl+Q (0x11) count; the bytes of a read from an ESC on
are an escape sequence (an arrow key, a mouse report, a paste), never a key. POLLHUP, POLLERR or
POLLNVAL, a read of 0 bytes, or a read error other than EINTR or EAGAIN is a hangup, as SIGHUP
is. Not crossterm's event reader: on a hung-up terminal it retries a read that returns 0 or EIO
forever, so its poll never returns, the flags go unread, and a pane waiting for a key ran at full
CPU until SIGKILL (seen live after its tmux server was killed; SIGTERM didn't end it).

Ctrl+Z: the user can detach by hand with Claude's own Ctrl+Z inside `claude attach`; the loop then
waits as in step 6. The pane's terminal is in raw mode (ISIG off) for as long as the pane runs,
so Ctrl+Z, Ctrl+C and other keys are plain bytes: relayed to attach, whose own terminal decides
what they do, or read and dropped by chief between attaches (only `q`, Ctrl+Q and the step 6
answers count). In case a SIGTSTP gets through anyway, chief ignores it; attach starts with it at
its default (an ignored signal would survive exec). On exit chief restores the terminal mode it
started with.

Board switching works in any terminal, with or without a multiplexer: write `pane_target`, then
`pane::request_switch(conn, slug)`. When `pane_showing` equals the target (NULL target = the
coordinator's name), or no pane is recorded, do nothing. A NULL `pane_showing` is signalled too:
the pane may be waiting after the user left (step 6), where the signal attaches the target, even
the one just left; between attaches the flag stays set and the next wait drops it when the target
is the one being attached.
Otherwise check that the recorded pid's start time still matches `pane_started`
(`pane::live_pane`); if not, the pane is gone and the pid may be another process's by now, so
clear the record and its `pane_showing` (only while it holds that pid) and send nothing. Else
send it SIGUSR1. The pane catches SIGUSR1 like SIGHUP (a handler that only sets a flag, no
`SA_RESTART`), before it records its pid, since the default action would end it. When the relay
sees the flag it clears it and reads the target again: a target it already shows (the board saw
the pane just before it moved on) is ignored. Otherwise it ends `claude attach` as above. SIGTERM
ends the attach client and leaves the background session running (verified on Claude Code
2.1.310: still listed, idle, with its pid). A killed attach leaves the screen modes it turned on
in the pane's terminal behind (and so does one ended for Ctrl+Q), so the pane then writes a reset: end synchronized output first (`ESC[?2026l`: until then the
terminal holds back the rest), leave the alternate screen (`ESC[?1049l`), show the cursor
(`ESC[?25h`), turn off mouse reporting (`ESC[?1000l ESC[?1002l ESC[?1003l ESC[?1006l`), bracketed
paste (`ESC[?2004l`) and focus events (`ESC[?1004l`), pop the kitty keyboard flags (`ESC[<u`),
reset modifyOtherKeys (`ESC[>4m`), reset attributes (`ESC[0m`), clear (`ESC[2J ESC[H`); then it
clears `pane_showing` and attaches to the new target. The pane writes `pane_showing` before its last read of the target and the board writes the
target before reading `pane_showing`, so a switch that sends nothing is always seen by the pane's
next read; a SIGUSR1 that lands before attach starts leaves the flag set, and the relay's first
look ends that attach at once. One pane per project receives switches and owns `pane_showing`: the
latest one started.

The board's Ctrl+Q quits chief, the pane too (`pane::request_quit`): to a live pane
(`pane::live_pane`) it sends SIGUSR2, caught as SIGUSR1 is (a flag, no `SA_RESTART`, before the
pane records its pid), then waits up to 3 s for that process to end before the board quits.
The pane takes the flag as its own Ctrl+Q: the relay ends attach (`Ended::Quit`), a wait for keys
ends, and the pane closes what `OnQuit` names, the layout or the board's block, so the board
quitting first would close its block under the pane. The built-in layout catches SIGUSR2 as well
and quits on it.

### Built-in layout (`src/builtin.rs`, `src/embed/`)

One chief process draws both sides in the terminal it runs in, for a terminal with neither Rex
nor tmux: the conversation on the left, the board on the right. It is the pane loop above and the
board, sharing a screen instead of a multiplexer.

- Conversation. The pane loop's steps 0 to 6, with the same constants, messages and
  `left_lines` (as for `OnQuit::Layout`), but `claude attach <short_id>` runs on a pseudo-terminal
  of its own (`embed::Pty::spawn`: as for the relay above, but with `TERM=xterm-256color`,
  `COLORTERM=truecolor` and no `TERM_PROGRAM`, since chief's emulator reads its output, a
  reader thread, and a writer thread that the keys, the paste and the emulator's replies are
  queued for, in order: a child that stops reading its input would otherwise stop the UI thread
  in a blocking write, Ctrl+Q included) instead of being relayed. `alacritty_terminal`
  (Alacritty's terminal emulator, pure Rust, from crates.io) emulates it (`embed::Screen`): it
  parses attach's output and answers
  most of its queries (DA1 as a VT102, DA2, DSR, the size reports, DECRQM, the kitty keyboard
  query; OSC 10/11 and the palette chief answers from a dark default and xterm's colours). A
  second parser over the same bytes answers what alacritty_terminal never sees (XTVERSION
  `chief`, DA3) in order with the rest, follows X10 mouse mode (9), and drops a kitty keyboard
  push past 1024 deep (alacritty_terminal panics at 4096). Alacritty keeps its input encoders in
  the app, so chief encodes the keys, mouse, paste and focus reports itself, in whatever modes
  attach asked for: xterm's keys (application cursor keys, Shift+Enter as modifyOtherKeys'
  `CSI 27;2;13~`), the kitty keyboard protocol's flags, SGR, legacy and UTF-8 mouse, bracketed
  paste with ESC neutralised. The UI thread owns the emulator, and a reader thread only moves the
  pty's bytes. The screen is copied into ratatui's buffer each frame, palette colours as indices
  so the outer terminal's theme applies; during a synchronized update (mode 2026) vte's processor
  holds the update's bytes, so the last complete frame stays, for at most 500 ms. chief's own lines (`chief · <name> …`,
  the retry and "you left" lines) are written on a fresh emulated screen, or below attach's own
  output when it failed. attach has ended when its terminal closes (the reader's end of output),
  or, as in the relay, when it exited with the terminal still held open by something it started
  (on Linux: macOS revokes a session's terminal when its leader, attach, exits, so the reader sees
  the end): each tick checks whether it exited, and 500 ms after that it counts as ended, and
  what holds the terminal is ended with its process group (`Pty::end`). Each pass of the
  loop has a generation number, so output of an attach
  that ended or was ended, or the result of a wake for an earlier target, is dropped. The wake step
  (`pane::wake`: `claude agents`, then `claude respawn` and its wait) runs on a worker with its own
  database connection, so the screen stays live. An attach being ended (switch, quit) gets
  SIGTERM, 2 s, then SIGKILL, to its whole process group, on a thread that chief joins before it
  exits.
- Pane record. It catches SIGHUP, SIGTERM and SIGUSR1 as `chief pane` does, records itself in
  `pane_pid`/`pane_started` and writes `pane_showing` through `set_pane_showing_as`, so the
  board's switch (its own, or one in another terminal) and `chief down` reach it exactly as they
  reach `chief pane`: the board beside it writes `pane_target` and signals its own process.
- Board. `board::Board` (the app, the refresher and the Show worker, as in `chief board`) drawn
  with `ui::draw_in` in the right part of the frame; `App::embedded` adds `^\ conversation` to
  its key bar, which keeps it however narrow the board is. Its Edit action pauses the input thread and the layout's terminal modes around the
  editor, so neither takes the editor's keys.
- Screen. 100 columns or more: the board gets 32 % (at least 36 columns), a dim `│` divider, the
  conversation the rest. Narrower: the focused side fills the screen; the conversation keeps its
  size while the board is over it, so switching sides doesn't make Claude redraw. The side without
  the keyboard is drawn dim, and the cursor shows only while the conversation has the keys and
  attach runs.
- Bar. The conversation's last row (when it has two or more) is chief's, and Claude gets the rows
  above: dim `chief · coordinator` or `chief · thread <name>` on the left while that target is
  being woken or attached (else `chief`), and on the right, in the key bar's style, `^Q quit`
  and `^\ board`; before them, while threads wait on the user, `! <n> waiting` in the board's
  Waiting style. As many as fit, in that order, the waiting count first. A click on a hint does
  what its key does, and one on the waiting count what the board's `n` does (it shows the next
  thread waiting, or says the one shown is the only one) and gives the conversation the keys;
  other clicks on the bar go nowhere.
- Keys. Ctrl+Q (which Claude Code doesn't bind) quits from either side, attached or not. Ctrl+\
  (which Claude Code doesn't bind either; legacy terminals report it as Ctrl+4) switches
  sides, and a click focuses the side clicked. On the conversation side every other key goes to
  attach while it runs; while chief's own lines offer `q` (the retry lines, the "you left"
  wait), `q` quits (also Ctrl+C in the "you left" wait, whose ⏎ goes back and whose first 300 ms
  of keys are dropped, as in step 6); while a target wakes, keys go nowhere, `q` included. On
  the board side keys are the board's, and its `q` quits. Whatever the board shows (⏎, `n`, Esc
  or `c`, a click on the selection) gives the conversation the keys, so the answer to a waiting
  thread is the next thing typed and a narrow screen shows it at once; a key typed before its
  attach runs is that answer's start, not a `q` for chief, hence the rule above. The mouse goes
  to the side under it: to attach as its mouse reporting asks, else the wheel scrolls the
  conversation's history, marked `↑ history · a key
  returns` at its top right (and no cursor) until a key, a paste or the wheel brings back the live
  screen; paste goes to attach. Focus
  reports follow the side with the keys. chief turns on bracketed paste, focus reports and, where
  the terminal answers the query, the kitty keyboard protocol's disambiguated codes (so Shift+Enter
  reaches Claude as itself), and turns them off on exit.
- Input. crossterm reads keys on a thread of its own so the loop can wait on keys, pty output and
  workers at once (one channel), drawing at most every 8 ms during a burst and every 100 ms
  otherwise. crossterm's reader spins on a hung-up terminal, so the loop itself ends on SIGHUP or
  SIGTERM, or when `poll` reports POLLHUP or POLLNVAL on stdin; process exit then stops that
  thread.
- Title. The terminal's title is `chief · <name>`, with ` · <n> waiting` while threads wait on
  the user (set when it changes, and again after the board's editor), so a terminal tab in the
  background says when it needs them. The terminal's own title is pushed on start and popped on
  exit (XTWINOPS 22 and 23, which terminals without them ignore).
- Quitting ends the attach (sessions keep running), joins the enders, clears `pane_showing` and
  the pane record (while they are still its own), turns the terminal modes off and restores the
  terminal.

## 10. Board

`chief board --project <slug>` (`src/board/`): `app.rs` holds the state and turns keys into actions
(`App::on_key(KeyEvent) -> Vec<Action>`), `ui.rs` draws it, `refresh.rs` polls in the background,
`jobs.rs` runs actions on worker threads, `settings.rs` is the settings form, and `mod.rs`'s
`Board` holds the app with its workers for `chief board` and the built-in layout (§9), which
draws it in part of its frame (`ui::draw_in`). Logic and rendering are tested without a terminal
(`TestBackend`).

- Refresh: two background threads, each with its own SQLite connection, send results to the UI over
  a channel; the UI never waits on them. Every 2 s: take `taken_at`, run
  `claude agents --json --all`, sync (§5), read the project's rows, guard denial count and
  `pane_target`. A failing `claude agents` still sends the stored rows, with "refresh failed: …" in
  the status line; a failing database read keeps the last rows. After each action the worker asks
  for an immediate re-read (database only). With the rows, each working or waiting thread's 12
  latest events (`db::session_events`, by `events_by_session`), and its subagents mid-turn with
  each one's latest PreToolUse (`db::busy_subagents`, by `events_by_agent`), for the detail. Every 30 s
  (first at start): `pr::refresh_project` with one `Gh` kept for the board's lifetime and
  `project.toml` re-read each time (§12), which runs
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
  bar: the keys that act on the selection, most useful first (`n next waiting` while a thread
  waits; on a thread `S stop` when it has a process, else `w wake`; on a resolved, uncleaned
  one `x clean up` and `X clean up all`; `o PR` with a PR, then `v report` with a report), each
  `key what` with the key in
  the accent colour, whole hints only, `^Q quit` and `? help` kept at the end when the rest doesn't
  fit (in the built-in layout `^\ conversation` instead of `^Q quit`, which the conversation's
  bar names, §9). Above
  it, when there is one, the status line: a running worker, else the last action's result
  (cleared by the next key), else refresh errors, else the PR hint. Widths are counted per
  grapheme, as ratatui draws them (an emoji with a variation selector or a joiner is one). In
  messages, the home folder shows as `~` only where a path starts (the start of the text, or
  after whitespace, a quote or `(`), never inside a longer path such as `/private/tmp/…`.
- Layout, designed for a 40–70 column pane and usable down to 30: a panel titled with the project
  name holds the goal (one dim line), then the coordinator's row, then the groups in §5 order
  (`<label> · <count>`, Waiting in bold red), empty groups hidden, "No threads yet" when there
  are none, Resolved collapsed by default (`Resolved · N · R shows`). When the list doesn't fit,
  its last line says what is below (`↓ N more`, plus `· Resolved N · R shows` for a collapsed
  header under the fold; `↑ more above` at the bottom); while the selection is on the first page
  the list starts at the top, and scrolling up shows a group's header with its first row when
  both fit. The selection is always in view. The coordinator's row is `<target mark>◉ Coordinator  <state>`
  with the guard denial count when > 0 (`asleep · 2 guard denials`). A thread's row is
  `<target mark><glyph> <title or name>  <tags>`, cut to the width with an ellipsis and never
  wrapped; the title gives way first so tags stay readable. Tags:
  waiting reason, background work unless resolved (`1 subagent, 2 background commands`, §5),
  `asleep`, PR state (`PR draft|open|approved|changes|merged|closed`), `cleanup blocked`, `cleaned up`, `routine <name>` for a routine's run (§13), then the agp label of the account it runs in (`PB2`; none for a
  thread in the project's account without agp, §7 "Thread account"). Detail panel for the
  selection, titled with the thread's title (name, group and reason or resolved reason,
  `· <background work> running`, asleep; under it a tree of its subagents mid-turn, one line
  each, `├ <name, else description, else agent id, up to 18 columns>  <latest call> · 12s` (or
  `starting` before its first call); then, from a working or waiting thread's latest events
  (a working thread's own, without its subagents' calls; a waiting one's all, as a permission
  prompt may be for a subagent's call), what it does or asks: `wants to run <command>` (or `wants to use <tool: what>`) for the call a
  permission prompt is for, the PreToolUse before the prompt; `asks <question>` while its latest
  call is an unanswered AskUserQuestion; `now <call> · 12s` while its latest call runs, else
  `last <call> · 3s ago`; its task on one line; branch, PR URL, model · effort, `account
  <label>`, `moved <move_note> <ago>` for a thread agp moved, `limit <limit_note>` while limited
  (with ` · agp resumes it at the reset` when agp waits, ` · agp is moving it` while it does), last
  activity, cleanup state; up to three calls before that one under `earlier`, each
  with its age; the last report wrapped, and when the text runs past the panel its last line
  says `↓ more · v reads it all`), or "Coordinator" (its state, model ·
  effort, last activity, and what to do next: with no threads, "Tell the coordinator what you want
  done, in the main pane…"): below the list when the body has ≥ 16 lines (the list takes what it
  needs up to 3/5), otherwise Tab swaps the list for it.
  Colours: a fixed palette that reads on dark and light terminals (Waiting bold red with `!`,
  Working green `●`, Limited yellow `◷`, Review blue `◆`, Landing magenta `▲`, Idle default `○`, Failed red `✗`,
  Resolved dim `✓`). `▸` marks what the main pane shows: a thread's row, or the coordinator's.
- Keys: ↑/↓ or j/k move over the coordinator's row and the threads (headers skipped; g/G
  first/last). The first threads to show take the selection from a coordinator selected only
  because there were none; one the user selected stays selected across refreshes. Enter shows the
  selection in the main pane (a thread: refused for a cleaned one or one removed outside chief);
  Esc or c shows the coordinator from anywhere; on the coordinator's row the thread keys (w, r,
  o, x, S) say they act on a thread; n shows the next thread in Waiting after the one the main
  pane shows (wrapping) and selects it, or says nothing waits, or that the only waiting one is
  shown already; w wake (`tools::wake_thread`, refused for cleaned or removed); S stop, for a
  thread with a process (refused for cleaned, removed or asleep): a small modal `Stop <title>?`
  says what stopping keeps, y runs `tools::stop_thread` on a worker ("stopping …") and moves the
  board's own `pane_target` off it at once, any other key cancels (`<name> keeps running.`);
  X clean up every resolved, uncleaned thread: `cleanup::gc` for the project on a worker, then one
  line (`Cleaned up 2 threads; 1 thread would lose work or couldn't be checked (x on one says
  why).`, plus gc's first warning); r resolve
  or reopen (`registry::set_resolved`, reason `user`; reopening a cleaned or removed thread is
  refused with its message; the row flips on the board at once, so a second r before the
  refresh undoes the first rather than sending it again); o open the PR URL
  (`open` on macOS, `xdg-open` elsewhere, output discarded); x clean up, only for a resolved,
  uncleaned thread: `cleanup::clean(Safe)`. When blocked, a fresh `cleanup::inspect` decides the
  modal: if it lists the loss, "Discard unsaved work?" shows why plus `cleanup::render_loss` and
  runs `clean(Discard(<that inspection>))` only after the thread's name is typed exactly (Enter
  with a wrong name clears the input; Esc cancels); if it lists nothing or fails (`claude rm`
  refused over work chief can't see), "Cleanup blocked" says `Cleanup blocked: <why>.` and why no
  discard is offered, and any key closes it. A modal that comes while settings is open waits
  until settings closes (the form's message says so), so it never throws a draft away; v the
  report view (below); R show or hide resolved; Tab detail on
  short terminals; s settings; ? help (the keys, then what each mark and glyph means; any key or
  click closes); q or Ctrl+C quit the board only; Ctrl+Q quits chief, the main pane too, which
  closes the layout (§9, `pane::request_quit`). Wake and cleanup run one at a time on a worker
  thread ("waking …" in the status line). Shows run in order on one worker of their own: picks
  that queue up while one runs are skipped for the newest, so the main pane ends on the last
  pick.
- Report view (`v` on a thread; `src/board/report.rs`): the board's whole area for one thread's
  reports, newest first, one at a time: a panel titled `<title> · report <i> of <n>` (just the
  title for one), the task, then `report · HH:MM · <ago>` and the report, wrapped. It opens at
  once on the row's `last_report`; a worker reads the thread's 50 latest reports
  (`db::recent_reports`, by `reports_by_session`) and only that thread's read replaces them.
  ↑/↓ or j/k scroll a line, PgUp/PgDn, b and space a page (one line kept), g/G or Home/End the
  top and bottom, the wheel three lines; the scroll never passes the end. ← or h shows the
  report before, → or l the one after, each from its top. Esc, q or v go back to the board.
  Its key bar: `↑↓ space scroll` when the text doesn't fit, `← older`, `→ newer` where there is
  one, `Esc back`.
- Mouse: a click selects a row (the coordinator or a thread), a click on the selected row is Enter,
  a click on the Resolved header is R, the wheel moves the selection. Each frame records which
  screen row holds what (`App::hits`) for this. In settings a click puts the cursor on a field and
  the wheel moves it, except while a field is being typed into: then the mouse does nothing.
- Showing a thread: write `ui_state.pane_target` (NULL for the coordinator), then call
  `pane::request_switch`, which signals the running `chief pane` unless it already shows the
  target or no pane runs (§9).
- Settings screen (`s`), a panel titled `Settings · <name>` (`· unsaved` while the draft differs),
  edits a draft of `project.toml`, read again each time the screen opens so a save keeps edits
  made by hand while the board ran (the board's name and goal follow them; if it can't be read,
  the form says so and starts from the board's copy), in four sections: Project (goal, the repos, add repo, default
  repo, the two files, notify), Coordinator (model, effort), Threads (model, effort, permission
  mode, max_parallel, cleanup) and Accounts (the account, thread accounts, max usage, threads run
  through, on a usage limit, account trust and its untrusted pairs), then Save. Under the form one line says what the field under the
  cursor does (`Field::help`), unless a message replaces it; Ctrl+S saves from any field (in a
  text field being edited, it applies the text first; a bad value stays open with why). The
  fields: goal, coordinator model and effort, thread
  model, effort, permission mode, max_parallel, thread accounts (`[threads] accounts`, ids split
  on commas or spaces, empty = `[]`, shown "(none: the project's account)"), max usage (a percent
  1–100, empty = none), threads run through (`[threads] runner`, agp/claude), on a usage limit
  (`[threads] on_limit`, switch/wait/stay), default repo, cleanup (merged/manual),
  `ui.notify`, and the repos (add by path, named after its folder with `-2`, `-3` … on a clash, as
  in `init`; `d` removes one, moving the default repo to the first remaining). It also shows the
  project's Claude Code account (`claude_config_dir`, or `default (~/.claude)`) read-only: Enter
  or ←/→ there only says it can't be changed (§3). "Account trust" checks, on a worker, every
  repo of the draft in every pool account (`placement::pool_trust`, as `init` does, the project's
  own account included when it is in the pool): its value says `checking…`, `trusted in <labels>`,
  `N untrusted · checked <labels>`, "no agp pool: threads use the project's account", or why the check
  failed (accounts it couldn't check go in the message). Each untrusted pair gets a line
  (`untrusted  <label> · <repo>`); on it the message says what Enter does, and Enter marks that
  repo trusted in that account (§3 "Trust") on a worker, then checks again (`Msg::Trusted`).
  While a check runs, the last check's pairs stay listed and the cursor stays where it was;
  Enter on a pair or on "Account trust" then says to wait. Efforts, modes, runner, on_limit,
  default repo, cleanup and notify cycle with ←/→ (Enter too); models, goal, max_parallel, thread
  accounts and max usage are free text (Enter edits, Enter applies, Esc cancels; the edit
  belongs to the field it was opened on, which Enter writes whatever the cursor is on). Save validates (`ProjectConfig::validate`,
  errors shown in the form), writes with `save`, regenerates `generated/*`, and says "Saved. New
  threads use these settings; the coordinator's model and effort apply on its next start." When
  `generated/*` can't be rewritten after project.toml was written, the save still counts:
  "Saved, but generated/* wasn't rewritten: <why>…" (red). `Msg::Saved` carries the draft that
  was sent, so edits made while the save ran stay in the draft, unsaved. Esc
  with unsaved changes asks for a second Esc. Two entries open `instructions.md` and
  `memory/MEMORY.md` in `$EDITOR` (run through `sh -c`, so arguments work; fallback `vi`): the
  board leaves the alternate screen and raw mode, runs the editor on the same TTY, and restores;
  what the editor run reports shows in the form's message.
- Terminal: `chief board` needs a terminal on stdin and stdout; otherwise it fails with "chief
  board needs a terminal". `tui::init` sets up the alternate screen, raw mode and mouse reporting
  itself, not with `ratatui::init`, whose panic hook restores the terminal for a panic on any
  thread: chief's hook restores it only for a panic on the thread that called `init`. A worker's
  panic is not printed while the UI runs (it would land mid-screen); the worker runs under
  `tui::catch`, which turns it into the worker's failure message (`Waking <name> crashed: <panic>
  at <file:line>`, a save's `Not saved: …`, a trust check's failure), so `busy` and `checking…`
  always end. A refresh thread that panics says `refresh crashed: …; trying again` in the status
  line and starts its loop again after its interval, on the same connection (a transaction the
  panic interrupted was rolled back, §4). The terminal is restored on quit and on
  error; the editor gets it through `tui::suspended` (mouse, alternate screen and raw mode off,
  then back). Input is polled every 100 ms, results are drained between frames, and resizes take
  effect on the next frame.
- A thread with `cleanup_blocked` shows a "cleanup blocked" tag (the reason in the detail); a
  cleaned thread shows "cleaned up" in Resolved. Wake and reopen are refused for a cleaned thread
  and for one removed outside chief.
- **Routines** (§13). The refresh's database read also brings the project's routines
  (`Snapshot.routines`, a `RoutineRow` each: the routine, its `routines::state`, its schedule in
  words, its command's latest run with the last 5 lines it printed, its latest finding). They
  are a section under the thread groups, after Resolved even when collapsed, hidden when the
  project has none:
  - Header `Routines · N`, bold; `Routines · N · 1 needs you` (`k need you`) in Waiting's bold
    red when some do (`State::needs_user`).
  - A row is ` <glyph> <title>  <tags>`, cut like a thread's, its title dim when paused, the
    target mark's column blank (a routine is never what the main pane shows). The glyph is its
    state: `!` bold red (`approval`, `attention`), `✗` bold red (`failing`), `●` green
    (`running`), `✓` green (`ok`), `◆` blue (`update`), `○` (`nothing`; dim for `new`). Tags,
    joined with ` · `: the state's word when it matters (`needs approval`, `failing`, `needs
    you` in bold red, `running` in green, `update` in blue; none for ok, nothing new and no
    run yet), `paused`, the kind when it isn't a plain session (`command`, `check`), and while
    enabled when it next runs (`in 12m`, `due now`, `never due`).
  - Routines come after the threads in the selection (keys, wheel, clicks; `↓ N more` counts
    them). A selected routine stays selected while it exists; `n` goes to a thread.
  - Detail, titled with its title: `<name> · <kind> · <schedule in words> · next HH:MM (in
    12m)` (or `due now`, `never due`, `paused`); the state's word and line in the glyph's
    colour, with ` · for 3h` while failing, else ` · 5m ago`; `$ <command>` on one line (200
    chars) with `(at most Ns)`; while it waits, `Waits for your approval: a shows it whole, y
    approves.`; `last run <ago> · <the run's line>` (` · started <thread>` for a check's
    session) and the last 5 lines it printed (for a session routine, `last run <ago> ·
    <thread>`); `outcome <verdict>: <headline> · <ago>` when it says more than the state line;
    `note <last_note>`; `prompt …` on one line; `repo`, `model · effort` when set. What a
    command printed is shown through `ui::printable`: escape sequences and control characters
    out, the last redraw of a line that used `\r`.
  - Keys on a routine, in the key bar in this order: `a` approve (offered while its command
    waits, instead of `e`), `e` run now, `p` pause or resume. Thread keys on a routine, and
    these on anything else, say what they act on; Enter on a routine shows nothing and says
    where its runs are.
  - `a` asks first (`Screen::Approve`, a red card titled `Approve the command of <title>?`):
    that chief will run it with `sh -c`, as the user and with nobody watching, on its schedule
    (in words) and whenever it is run, until its words change; then the whole command; `y
    approves · any other key cancels`. A command too long to show whole can't be approved
    there (`chief routine show` reads it). `y` sends the words that were shown, and the worker
    approves only those (`routines::approve`, §13 "Approval").
  - `e` runs on a worker as `wake` does (`tools::refresh`, then `routines::run(Manual)`):
    `Started <thread>.`, `<name>: its command runs; the detail shows how it ends.`, or `Nothing
    started: <why>.`; refused while its command waits for approval. `p` flips the row at once
    and updates the routine as the user (`Paused <name>: …`, `Resumed <name>: next run …`).

## 10a. Launcher

`chief` with no command (`src/launcher/`): `app.rs` holds the state and turns keys and clicks into
actions, `ui.rs` draws it, `wizard.rs` is the new-project wizard, `mod.rs` runs the loop and the
worker (which runs `up_with`, the create step and `archive_with`, so the launcher shows their
reports instead of printing into its screen). It needs a terminal on stdin and stdout;
otherwise it fails with "chief needs a terminal; `chief --help` lists the commands for scripts". The look is the board's (§10 "Look"): one card,
at most 76 columns wide, centred, a line below the top, as tall as its content (wrapped lines
counted as the drawing wraps them), the key bar right under it. On a short terminal a card's
last lines (what Enter does, the trust box, the result or failure) stay whole at its bottom;
the lines above are cut from the top, under `↑ more above`, so the newest notes and the folders
next to the trust box are the ones that show. The terminal and worker panics are handled as on
the board (§10 "Terminal"): a worker that panics ends the card with `Starting crashed: …`
(`Archiving crashed: …` on the Archiving card).

- Project list (`Your projects`): each registered project's name in bold with a summary at the
  right, from project.toml and its stored rows only, so it opens at once (`1 waiting · 2 working`,
  in red while one waits; `no threads yet`; `all resolved`; `can't read project.toml: …`), and its
  goal dim below. Archived projects (`projects.archived_at`, §2) come after the live ones,
  folded into one line like the board's Resolved header, `Archived · N · A shows`; A (or a click
  on that line) shows them under `Archived · N`, dim, each marked `archived` where the summary
  goes, and hides them again. With every project archived the list says `Every project is
  archived.` Then `+ New project`, which stays selected when A toggles. ↑/↓, j/k, g/G and the
  wheel move over what is shown; Enter or a click on the selection opens the selected project
  (an archived one too: `up_with` unarchives it, and says so in its reports), or the wizard on
  `+ New project`; n starts the wizard from anywhere; ? explains what chief does and the keys; q
  or Esc quits. With no projects the launcher opens on the wizard, and Esc from its first step
  quits. `chief new` opens on the wizard too.
- Archiving: a on a live project asks first, in a card of its own (`Archive <name>? Its
  coordinator and threads stop; their conversations, worktrees and branches stay.`); y archives,
  any other key or a click cancels. `project::archive_with` then runs on the worker, and the
  "Archiving <name>" card shows its reports as the Starting card does (below), under a spinner,
  then the outcome: under Archived now, or the error in red when it isn't archived. Enter or Esc
  goes back to the list, which is re-read either way (sessions may have stopped). a on an
  archived project unarchives it at once (`Unarchived <name>. ⏎ opens it.` in green below the
  list) and starts nothing. The key bar names a's action for the selection (`a archive` or `a
  unarchive`) and, when any project is archived, A's (`A show archived`, `A hide archived`).
- Wizard, four steps, `New project · step N of 4` with `✓ About  ● Code  ○ Models  ○ Review`
  below the title. Esc (or Shift+Tab) goes back a step and keeps what was typed; on the first step
  it cancels.
  1. About: Name (required; its slug shows under it; Enter refuses an empty name, one with no
     letter or digit, and a slug a project has, `project::slug_taken`) and Goal (optional). Tab or
     ↑/↓ switch fields; Enter on the name moves to the goal, Enter on the goal goes on.
  2. Code: the repository folders threads work in, starting with the git repository the launcher
     runs in (the folder holding `.git` above its cwd), if any. Type a path (`~` works; Tab
     completes the last folder name, or the prefix the matches share, `~` alone to `~/`; when
     nothing matches it says so, and when several share nothing longer it says how many; ^W
     drops the last folder name, the one before a trailing `/` too; ^U clears) and Enter adds it,
     canonical; a missing folder or one listed already says so. ↑ selects listed folders, d
     removes one. Enter with nothing typed goes on; none is fine.
  3. Models: coordinator model and effort, thread model and effort (←/→ cycle `fable`, `opus`,
     `sonnet`, `haiku` and §3's efforts), defaults from `ProjectConfig::new`, a line on what each does, and
     a note that the rest is on the board (s). With agp installed (below) a fifth row, `Threads
     run through`, ←/→ between `agp` (the default: the `[threads]` defaults) and `claude`
     (`init --no-agp`, §2); its line names the pool accounts threads would also run in
     (`Besides this account: PB2, PB3.`), or says that agp watches each thread when it has no
     other account, or that nothing watches threads for usage limits with `claude`.
  4. Review: name and slug, goal, code, models, and with agp installed an `agp` row (`threads
     also run in PB2, PB3`, `watches threads for usage limits`, or `threads stay in this
     account, without agp`); when some repos aren't trusted yet, in the project's account (this
     process's `CLAUDE_CONFIG_DIR`, as for `init`) or, with `agp` chosen, in a pool account
     (`<repo> (in <label>)`), lists them with `[x] Trust them, as accepting Claude's trust
     prompt would` (Space toggles). Enter creates.
- agp in the wizard: the launcher asks agp once, on a thread of its own when it starts
  (`launcher::agp_pool`: `placement::pool_accounts` for the `[threads]` defaults, which is `agp
  status --json` and an `agp exec` per pool account, without the account the project gets), so
  a slow agp never holds the screen. The answer (`Msg::Agp`) reaches the open wizard and every
  one opened after (`Launcher::agp_found`). Until it comes, and without agp or when agp fails,
  the wizard shows nothing about agp and the project gets the `[threads]` defaults.
- Creating (`launcher::create`, on a worker): `project::create_with` (`init`'s validation and
  files, with the wizard's models and efforts, and `without_agp` when `claude` was chosen),
  then `trust::check`: the project folder is marked, and the repos in every account when the
  box was ticked. Else a warning names `chief trust <slug>`: "<n> folders aren't trusted, so
  threads there won't start: …" when the project's account lacks some, and "<n> folders aren't
  trusted in agp's other accounts, so threads there skip those accounts: …" when only pool
  accounts do; then `project::up_with`. Opening a listed project runs `up_with` alone.
  The "Starting <name>" card shows each report as it comes (`✓` done, `!` warning) under a
  spinner, then where the conversation is: Rex (`Open in Rex session chief-<slug>: the
  conversation with the coordinator on the left, its threads on the right.`, or to switch to it in
  the app when Rex couldn't), tmux likewise, in a Rex block (§9 step 0) the launcher leaves at
  once and runs `chief pane` in its own terminal beside the new board, with neither it leaves at
  once and runs the built-in layout in its own terminal (§9 step 3), and in a chief built without
  that layout Enter leaves the launcher, prints `chief board --project <slug>` to run elsewhere,
  and runs `chief pane` in this terminal (§9).
  A failure shows in red; before the project existed (`Msg::Created`), Enter goes back to the
  wizard as it was, after it back to the list. The list is re-read once a project is made or
  opened. q quits at any time; the worker's session keeps starting.

## 11. Testing

- Unit tests: §5 derivation (table-driven), §6 guard (table-driven), hook payload parsing against
  `tests/fixtures/hooks/spike-events.jsonl` and, for subagents, the raw payloads in
  `tests/fixtures/hooks/teammate-events.jsonl` (§1), `claude agents` parsing against
  `tests/fixtures/agents/agents-sample.json`, config round trip, migrations on a fresh DB and
  upgrades from older versions (their backfills).
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
  The switch is tested with `chief pane` on a pseudo-terminal and a fake `attach` that sleeps on
  the pty chief relays (`tests/pane_cli.rs`: its echo shows which keys reached it; Ctrl+Q in each
  encoding, a resize through the fake's `attach.sh`, hangups), and so is the built-in layout
  (`tests/builtin_cli.rs`: both sides drawn, a switch, the board's `q`, Ctrl+Q, a hangup); its emulator and pty are unit-tested (`embed::`) with no Claude at all;
  a hangup by closing the pty's master side, which the test opens close-on-exec so neither the
  pane nor its attach holds it. `claude rm` outputs from the live M2b probe are in
  `tests/fixtures/rm/`. Cleanup tests use real temporary git repositories (a bare
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
- Routines: the schedule (`cron::`, table-driven: next runs, refusals, wording), the watcher's
  routine kind (`waiting::`), and `tests/routines.rs` against the fake (validation, a due run
  starting a tagged thread, the claim, the skip while the last run works, older runs put away, a
  failed start noted and not retried).
- The board's routines: `board::` (the section and its tags, selection into it, the detail of a
  failing command and of one waiting for approval, the approval card, thread and routine keys
  refusing each other, tiny terminals) and `tests/board.rs` (the refresh reading them, their
  actions on workers). No test runs a command through the board's worker: it would start the
  test binary as `chief routine exec`.
- Commands (§13): `command::` (a run's line, what is news and what repeats, the tail),
  `tests/routines.rs` running real `sh` commands in-process with `run_here` (no session and no
  finding for a pass, the kinds and their gaps, a failure told once and its recovery once with
  what is pushed and what the coordinator is due, approval of the coordinator's words, a check
  starting a session only for news and falling back to a finding, the time limit, one run at a
  time and a lost run), and `tests/serve_cli.rs` with the binary (the server running a due
  command detached, the command speaking through `chief routine value` and `outcome`, the
  dashboard's state, Run now and Approve, `chief routine run` and `show`).
- The coordinator's own looking and its brief: `lookups::` (what is free, counted and outside,
  a link that leads out, the count per turn) and `tests/hook_cli.rs` through the hook command
  (memory read freely, a handful of lookups then a denial, a new turn starting over, nothing
  outside the project, threads unlimited; a stale brief's notice at SessionStart and with
  every prompt until the brief is read, again after `/clear` and a compaction, none for a
  current one; the old-tools line; a thread's start silent) and `brief::`.
- A run's side: `tests/routines.rs` (the notes folder and brief, values joining one series, the
  outcome and what it pushes, the Stop hook's request, who is woken, the workspace of a project
  without repositories), `tests/mcp_cli.rs` (`chief mcp --routine` serves the two run tools over
  stdio and nothing of the coordinator's) and `push::` (the command gets the message in its
  environment).
- `chief serve` runs end to end in `tests/serve_cli.rs`: the binary on port 0 with the fake, driven
  over raw sockets (sign-in, the origin rule, the API and its actions, the board stream, the
  terminal socket carrying a fake `attach`, a due routine firing, one server per home, stop).
  `CHIEF_SERVE_TICK_MS` shortens the scheduler's tick for it. The server check reads the chief
  home a tool's project folder lives in (`serve::home_of`), never `paths::root()`, so in-process
  tool tests don't touch the developer's own home.
- `scripts/e2e.sh` runs the live loop with cheap sessions (milestone 2b).

## 12. Cleanup (`src/cleanup.rs`, `src/pr.rs`)

Resolving a thread keeps its session, worktree and branch. Cleanup removes them with
`claude rm <short_id>` (§1), which deletes the worktree and the local branch and keeps any remote
branch. The row stays, with its reports: `cleaned_at` is set and `cleanup_blocked` cleared.

The worktree is the one `claude rm` itself deletes: the session's job state,
`<claude dir>/jobs/<short_id>/state.json` → `worktreePath`, where `<claude dir>` is the
thread's account folder (§3: its own account's, else the project's `claude_config_dir`, or for
the default account `$CHIEF_CLAUDE_DIR` in tests, else `~/.claude`); its refresh (step 1) and
its `claude rm` run in that account too. A thread agp moved (§5 "Limits and moves") runs under a
job that resumed in the worktree and names none; the job that does is in an account it ran in
before (`prior_accounts`, newest first: `claude::prior_job_state`), and `cleanup::own_job` returns
that one, with its account. It counts only as a `<repo>/.claude/worktrees/<name>` folder
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
   `claude rm` stops the session even when it refuses. A `pane_showing` whose recorded pane is
   gone (`pane_pid`'s start time no longer matches `pane_started`: a pane killed with SIGKILL
   never clears it) shows nothing, and that record is cleared (`pane::showing`); otherwise it
   would keep the thread from cleanup for good. One with no pane recorded (a pane that couldn't
   record itself) counts.
3. Never spawned (no `short_id`): mark cleaned without `claude rm`.
4. Either mode: when the job state of another session registered in this chief home (read in
   that session's own account folder: its own account's, else its project's; for a moved one, its
   earlier accounts' too) names the same worktree, store `<wt> is also the worktree of <other>; chief won't remove or reset it` in
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
6. A thread agp moved has a job in every account it ran in (`cleanup::gather`). The row moves
   back to the account whose job names the worktree (the current one when none does; no account
   of its own when that is the project's), `prior_accounts` cleared, in one transaction first; then
   `claude rm <short_id>` runs in each other account that still has a job for it (those name no
   worktree, so only the job goes, and its process stops). A refusal there stops cleanup with
   `claude rm of its old job in <label>: …` in `cleanup_blocked`; the row already names the job
   that stays, so a later cleanup goes on from there.
   Then `claude rm <short_id>` in the row's account (120 s: it deletes the worktree before it answers). `Discard` retries an unpushed refusal once with
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
neither working nor waiting nor showing in the pane (`pane::showing`, as in step 2) and has no
`cleanup_blocked`: a blocked
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

## 13. Routines (`src/routines.rs`, `src/command.rs`, `src/cron.rs`)

A routine is work a project repeats on a schedule, run by chief with nobody watching. It is one
of three things (`Routine::kind`), by what it has:

- a **session** (a `prompt`): each run is a fresh thread. For work that needs judgement;
- a **command** (a `command`): a shell line chief runs itself, with no Claude session ("A
  command", below). A check, a script, a cron job, at the cost of a process;
- a **check** (both): the command runs first and a session starts only when the command has
  news ("News"), so watching something costs nothing while all is well.

The coordinator makes and manages them with its tools (§7), so "every weekday at 9, triage new
issues" and "check staging every minute" are said in the conversation; `chief routine` (§2), the
board (§10) and the dashboard (§14) do the same. They are rows (§4 `routines`), written through
the same functions by all four.

### A routine

- `name`: its handle, `[a-z0-9-]`, at most 32 characters, unique in the project and never
  changed. Made from the title when none is given (`-2`, `-3` … on a clash); an explicit name
  that is taken is refused.
- `title`, never empty, and a `prompt`, a `command`, or both. The prompt is a session's whole
  brief; it is empty for a routine that is only a command. The command is one `sh -c` line (or
  several), at most 16 KB: a longer script belongs in a file the command runs. `timeout_secs`
  is how long it may run: 1 to 86400, default 300 (`command::DEFAULT_TIMEOUT_SECS`).
- `schedule`, in the machine's local time, one of (`cron::Schedule::parse`):
  - five cron fields, `minute hour day-of-month month day-of-week`: `*`, numbers, ranges
    (`1-5`), lists (`0,30`), steps (`*/15`, `5/20`, `1-10/2`), month and weekday names (`jan`,
    `mon`), 7 as Sunday. When both day fields are restricted, a day that fits either matches,
    as in cron;
  - `@hourly`, `@daily` (`@midnight`), `@weekly`, `@monthly`, `@weekdays` (weekdays at 09:00);
  - `every <n>m|h|d` (also `minutes`, `hours`, `days`): that long after the last run.
  What a run costs decides how often it may fire (`Schedule::check_gap`): a routine that is
  only a session is refused a schedule that would fire less than 5 minutes apart
  (`cron::MIN_GAP_MINUTES`), since each run is a whole session; one with a command may fire
  every minute (`MIN_GAP_COMMAND_MINUTES`), cron's own grain, since its sessions start only for
  news. A time the clocks skip doesn't run that day; one they repeat runs at its first pass. `describe` words a schedule for people ("Weekdays at 09:00", "Every 30
  minutes"), and shows one it has no words for as `cron <fields>`.
- `repo`: a repository of the project by name (never a path: trust and the coordinator's deny
  rules are set up for the project's repositories), else the project's default repo; a routine
  with neither is refused. A command runs in that repository's own folder, not a worktree.
  `model` and `effort`: overrides of `[threads]`, for its sessions.
- `enabled`: a paused routine keeps its row and fires nothing (`next_run_at` NULL). Resuming
  counts from that moment; nothing missed while paused is made up.
- `notify` (default true): whether the coordinator is told when a run finishes (below).

`next_run_at` is set when the routine is created or resumed and whenever its schedule changes
(an edit to anything else leaves it, so a new prompt doesn't push an interval routine's next run
away).

### Firing

Routines fire only while `chief serve` runs (§14): every 15 s (`serve::TICK`) it looks, in every
project that isn't archived, for enabled routines whose `next_run_at` has passed, refreshes that
project from `claude agents` when a due routine depends on what its sessions do
(`routines::due_needs_rows`: a session to start, or a check whose last session the rows show
at work; never for commands alone, so a command a minute doesn't cost a listing a minute), and
runs each (`serve::Scheduler::tick`, `routines::run_due`). Without the server nothing fires; every listing
(`list_routines`, `chief routine list`, a new routine's answer) says so and how to start it, and
`chief up` starts it for a project that has an enabled routine (§2).

A routine that came due while the server was down fires once when it next looks, however many
occurrences were missed, and its next run is counted from then.

### A run (`routines::run`)

1. **Claim.** For a scheduled run, in one write transaction: when the routine is enabled and
   due, `next_run_at` moves to the next time after now. Two processes looking at once start one
   run, and a run that then fails or is skipped has still spent its occurrence: a broken
   routine doesn't start a session every tick. A manual run (`run_routine`, `chief routine run`,
   the board's `r`, the dashboard's Run now) claims nothing, leaves the schedule alone, and
   works on a paused routine too.
2. **Approved.** A routine whose command waits for the user's yes ("Approval") starts nothing,
   by schedule or by hand: `last_note` says `skipped at HH:MM: its command waits for the user's
   approval`.
3. **One at a time.** When a thread of this routine is still working, waiting on the user or
   limited (not resolved, cleaned or removed), nothing starts: `last_note` says `skipped at
   HH:MM: its last run (<thread>) is still <group>`. Likewise while its command still runs
   (`command::under_way`): `skipped at HH:MM: its command still runs (since HH:MM)`.

A routine with a command then runs it ("A command"). A session, or the session a check's news
starts (`routines::start_session`), goes on:

4. **Put away.** Its earlier runs that sit idle are resolved (`resolved_reason = 'routine'`), so
   one finished run per routine stays on the board. A run with an open pull request (review,
   landing), a failed one, and anything of another routine stay.
5. **Start.** `tools::start_thread_as` with the routine's title, repo (the project's workspace
   when it has no repositories, §3), model and effort and no account (placement picks, §7
   "Thread account"): a thread like any other, with the same guard, hooks, agp watching and
   cleanup, plus what makes it a run (`tools::RunOf`): its row carries `routine = <name>` from
   the start; `--add-dir` its routine's folder; `--settings generated/run-settings.json` (a
   thread's hooks, and `permissions.allow: ["mcp__chief"]`, since nobody watches a run for a
   prompt on the tools it must call); `--mcp-config generated/run-mcp-<name>.json`, written just
   before (`chief mcp --project <slug> --routine <name>` as its `chief` server). Its task is the
   prompt under chief's own brief (`routines::brief`, below), and for a check, between the two,
   why it started ("News"). The routine then gets
   `last_run_at`, `last_thread` and `last_note` (a start's `warning`, else NULL). A start that
   fails is the error, with `last_note` = `failed at HH:MM: <why>`.

`max_parallel` is not counted against routine runs: it is a rule the coordinator keeps for the
threads it starts.

### What a run leaves

A session's runs are separate sessions: fresh context every time, which is what keeps a routine
that runs for months as cheap on its hundredth run as on its first. What carries over is what a run
leaves, and chief's brief tells every run to leave three things, so a routine's own prompt says
only the task:

- **Notes.** `<dir>/routines/<name>/NOTES.md` (`routines::ensure_folder`, made with the routine
  and before each run) is the routine's memory. The brief: read it first; before finishing,
  rewrite it for the next run (the current state, what was checked and when, what to watch),
  under about 150 lines, replacing what is stale rather than appending; anything else worth
  keeping goes in that folder. The user and the coordinator may edit it too (the coordinator's
  deny rules don't cover it): the next run reads what is there. The folder outlives the routine.
- **Values.** `record_value {series, value, unit?, note?}` stores one point of one series
  (`routine_points`): a price, a rate, a balance. One series per thing, the same name every run;
  whitespace is collapsed and a name that differs only in case from one in use takes that one,
  so a line doesn't split. Returns `{series, recorded, previous}`. An empty or over-long (60
  chars) series and a value that isn't a finite number are refused.
- **An outcome.** `report_outcome {outcome, headline, details?}` (`routine_outcomes`, tied to
  the routine's newest thread, `routines::current_run`): `nothing` (nothing changed that the
  user would want to hear), `update` (news that can wait) or `attention` (they should look or
  decide now), with a headline of one line, at most 200 chars, written for a phone. A run's
  later word replaces its earlier one. Returns `{recorded, pushed, next}`.

The brief also says nobody is watching (don't wait for an answer; say what is missing in the
outcome and stop) and to be economical.

A run that ends without an outcome is asked for one: the thread's Stop hook
(`hook::outcome_block`) prints `{"decision":"block","reason":…}` naming the notes file and
`report_outcome`, when the thread is a run, has no outcome recorded, no subagents or background
commands still running (that Stop isn't the end of the work), and the Stop isn't itself the end
of a blocked turn (`stop_hook_active`): once per chain of Stops, after the final-report request
(§5), which comes first. A failure to decide is logged and blocks nothing.

### A command (`src/command.rs`)

A command's run is a `routine_runs` row (§4) and a process, never a session:

- **Who runs it.** `routines::run` writes the row (`command::begin`, `running`) and starts
  `chief routine exec <slug> <run>` detached (`command::spawn`: its own session, its words in
  `<root>/logs/routine-runs.log`), so the scheduler's next look, a tool's answer and a page's
  request never wait for a command, and a run records itself even when what started it ends
  first. `chief routine run` runs it in its own process instead and waits (`run_here`), since
  whoever typed it watches. Either way `command::execute` does the rest.
- **How.** `sh -c <command>` in the routine's repository (the project's workspace when it has
  none, §3), stdin closed, stdout and stderr together into `<dir>/routines/<name>/last-run.log`
  (the latest run's, whole). It leads its own process group. Its environment is the server's,
  plus `CHIEF_HOME`, `CHIEF_PROJECT`, `CHIEF_ROUTINE`, `CHIEF_ROUTINE_DIR` (the routine's
  folder, where it keeps what it likes between runs), `CHIEF_RUN` (the run's id), the folder of
  this chief binary first on `PATH`, `CLAUDE_CONFIG_DIR` as the project's account (§3), and no
  `GIT_*` of another repository.
- **How it ends.** Exit status 0 is `ok`; any other status, a signal (kept as 128 + the
  signal), or a command that couldn't start is `failed`; past `timeout_secs` its group gets
  SIGTERM, 2 s, then SIGKILL, and it is `timeout`. The row keeps the last 16 KB of what it
  printed. A run whose process is gone without a word (the machine restarted) is `lost` the
  next time the routine is looked at; it says nothing about the command. The 200 latest runs
  of a routine are kept.
- **Where it runs.** On this machine, as every routine does: `chief serve` fires it and the
  process is its child. Nothing of a routine runs in Anthropic's cloud, and chief sessions
  are denied Claude Code's own cloud scheduling (§3, `CLOUD_SCHEDULING_DENY`).
- **What it says.** Nothing is required: the exit status is the verdict. A command that knows
  more says it with two commands it finds on its `PATH`, which are a session's two run tools:
  `chief routine value <series> <number> [--unit U] [--note N]` (`record_value`) and `chief
  routine outcome nothing|update|attention <headline> [--details D]`, which is kept on the run's
  row until the run ends (a later word replaces an earlier one). Outside a run both say that
  they are for a routine's command.

### Approval

The coordinator can't run commands (§3: no Bash), and a routine's command runs with the user's
rights and nobody watching. So a command runs only in the words the user approved
(`routines.approved_command`, `Routine::needs_approval`):

- What the user writes is approved as they write it (`Author::User`): `chief routine add`, the
  dashboard's form, whose every save carries the command.
- `chief routine` run by a Claude session isn't the user's hand (Claude Code sets `CLAUDECODE`
  for what its Bash tool runs): a thread has Bash, and the coordinator could ask one to approve
  for it. There `add --command` writes as the coordinator does, and `approve` is refused with
  where the user approves. A session that unsets the variable gets past this: it keeps a
  helpful session from approving for the user, it is not a sandbox.
- What the coordinator writes or changes (`Author::Coordinator`: `create_routine`,
  `update_routine`) waits. The tool's answer carries `approval`, which tells it that it can't
  approve, to show the user the command, and where they say yes: `a` on the board (§10),
  Approve in the dashboard (§14), `chief routine approve <slug> <name>` (`routines::approve`).
  The board and the dashboard send the words they showed, and a command that reads otherwise
  by then is refused in the same transaction: the yes is to what was read.
  A change of anything but the command leaves the approval; setting the command back to the
  approved words needs none.
- Until then nothing of the routine runs ("A run", step 2), and it stands as `approval`
  ("State") wherever it is shown. `command::execute` checks again before it starts anything.

### News

A command runs often and repeats itself, so what a run came to reaches someone only when the
run before it didn't say the same (`command::news_of`; the run before is the latest that
ended, `lost` ones aside):

| The run | Is news when | As |
| --- | --- | --- |
| said an outcome (`chief routine outcome`) | it isn't `nothing`, and the run before didn't say the same outcome and headline | its outcome, headline and details |
| failed or timed out without saying one | the run before didn't also fail without saying one | `attention`, `Failed: exit 22: <the last line it printed>` (or `timed out after 5m 00s: …`, `didn't start: …`), details: the end of its output |
| passed without saying one | the run before failed | `update`, `Passes again after N failed runs` (a recovery) |

So a check that fails every minute through the night is one line in the morning, and its
recovery another. News goes one of two ways (`command::deliver`):

- **A command alone:** it is a finding (`routine_outcomes`, with the run's id and no thread)
  and reaches people as any finding does ("Who hears of a run").
- **A check:** it starts the routine's session ("A run", steps 4 and 5), which says the outcome
  itself. Its brief gets, before the prompt, why it started: the command, how it ended and what
  it said, and the last 4 KB of what it printed, with the path of the whole log. The run's row
  names the thread. A recovery is never handed to a session; and when no session can take the
  news (its last one still works, or the start fails), it is a finding after all, its details
  ending with why no session looked into it.

### State

How a routine stands is one word and one line, computed in one place (`routines::state`) for
`chief routine`, the board and the dashboard:

| `key` | Word | When | Line |
| --- | --- | --- | --- |
| `approval` | Needs approval | its command waits for the user | |
| `running` | Running | its command runs now | `since HH:MM` |
| `failing` | Failing | its command's last run that ended failed | what a check's session found since the failures began, else the run's line (`exit 22: <the last line it printed>`); `at` is when the failures began |
| `attention`, `update` | Needs you, Update | its command's last run passed and said so; or, for a session, its latest finding | the headline |
| `ok` | OK | its command's last run passed | `passed in 0.3s` |
| `nothing` | Nothing new | a session's latest finding | the headline |
| `new` | No run yet | nothing ran | |

`approval`, `failing` and `attention` need the user (`State::needs_user`).

### Who hears of a run

A run's outcome decides it, so a routine costs a coordinator's turn only when there is news:

| Outcome | Coordinator | User |
| --- | --- | --- |
| `nothing` | not told | nothing; the routine's page shows the finding |
| `update` | told with `<chief-routine>` on its next turn (its watcher, §5); never woken for it | pushed only with `[push] on = "updates"` |
| `attention` | told, and woken when it sleeps | pushed, and a desktop notification |
| none (it crashed, or forgot despite being asked) | told on its next turn, with its report's first words | nothing |

A command's run reaches this table only when it is news ("News"), with the outcome it said or
the one chief gave it: a first failure is `attention`, a recovery `update`.

- A command's finding has no thread to carry it, so it is told by itself: `routine_outcomes.
  told_at` is empty until the coordinator's watcher tells it (`routines::untold`, §5), and it
  wakes a sleeping coordinator as a thread's `attention` does. One that says `nothing`, or
  whose routine tells nobody, is stored as told.
- A routine with `notify = false` tells the coordinator of no run; its runs still push.
- **Waking.** The watcher lives in the coordinator's process, which Claude Code ends after
  about an hour idle (§1). So `chief serve` wakes it for a run that needs the user
  (`Scheduler::tick`, `routines::unheard`, `wake_coordinator`): each look, a project with a run
  (or a command's finding) that is due to be told, said `attention`, and reported at least 30 s
  ago (`WAKE_AFTER_MS`: a turn the Stop hook kept going has moved on, and a coordinator that is
  awake has heard) gets
  its coordinator refreshed and, when it has no process, respawned (`project::respawn`, 5 s for
  a pid). Its SessionStart hook starts the watcher (§3), which finds the run due and wakes the
  coordinator with the notice. Each report gets one wake (kept in the server's memory, so a
  restart may wake once more). A coordinator that was never started isn't started for this.
- A run stuck on the user, moved or limited is told by the watcher's other kinds, like any
  thread.

A deleted routine's runs keep their tag, their threads, their findings and their values.

### Reaching the user (`src/push.rs`)

The coordinator's words wait in its conversation; these reach the user when they aren't
looking, and never wait on anything (each is spawned and left):

- **The desktop notification** (`push::desktop`, macOS `osascript`, `[ui] notify`): a thread's
  permission prompt, a plain thread's finished turn (§5), and a run's `attention`, a command's
  included.
- **The push command** (`[push] command`, `push::send`): the user's own one-liner, run through
  `sh -c` with the message in its environment, never in its command line, so nothing in a
  headline is read as shell: `CHIEF_TITLE` (`<project name> · <routine title>`, or the
  notification's title), `CHIEF_BODY` (the headline), `CHIEF_KIND` (`attention`, `update`,
  `permission`), `CHIEF_PROJECT` (the slug) and `CHIEF_URL` (the dashboard's page for it, when a
  server runs and the project's dashboard is on; else empty). It runs for a run's `attention`,
  for a thread's permission prompt, and with `on = "updates"` for a run's `update`. chief ships
  no service of its own: ntfy, Telegram, mail and the rest are a `curl` away.

## 14. `chief serve` and the dashboard (`src/serve/`)

`chief serve` is the one optional long-lived process of a chief home. It does two things: it
fires routines (§13 "Firing", `schedule.rs`) and it serves the dashboard, a web page for each
project that turned it on (`http.rs`, `view.rs`, `term.rs`, and the page itself in
`src/serve/web/`, compiled into the binary: no build step, nothing fetched at run time).

### The process

- **One per chief home.** It holds an exclusive `flock` on `<root>/serve.lock` for as long as it
  runs; the kernel releases it however the process ends. "Is it running" is "is the lock held"
  (`serve::running`), never a pid file a crash left behind. A second `chief serve` fails with
  `chief serve already runs for this chief home at <url>`.
- **Where it listens.** `--bind` and `--port`, else `<root>/serve.toml` (`bind`, `port`), else
  `127.0.0.1:4747`. Once it listens it writes `<root>/serve.json` (`pid`, `bind`, `port`,
  `started_at`), which it removes when it ends; port 0 takes a free port, recorded there. On an
  address that isn't loopback it logs that other machines can reach it, over plain HTTP.
- **Accounts.** It runs in no account: every call for a project is made in that project's
  account (`tools::with_project`, §3), whatever `CLAUDE_CONFIG_DIR` the server started with.
- `chief serve start` runs it detached (its own session, stdin closed, output appended to
  `<root>/logs/serve.log`, `CHIEF_HOME` set to the resolved root) and returns once it listens
  (10 s, else an error naming the log); already running is fine. `chief up` runs it for a
  project that needs it and `chief dashboard` always (§2).
- `chief serve stop` sends SIGTERM and waits up to 5 s for the lock to be free. The server ends
  on SIGTERM or SIGINT without waiting for open streams and terminals; the `claude attach` of an
  open terminal ends with it (its pseudo-terminal closes), its session keeps running.
- `chief serve status` says whether it runs and where, then each live project with a routine or
  its dashboard on.
- `chief serve install` (macOS) writes a launchd agent, `~/Library/LaunchAgents/
  dev.chief.serve.plist`, and loads it: `chief serve` at login, restarted when it ends, with
  this shell's `PATH` (launchd's has neither `claude` nor `agp`), `CHIEF_HOME` and the variables
  the generated files carry (§3; never `CLAUDE_CONFIG_DIR`). It stops a running server first, so
  launchd's doesn't fail on the lock, and refuses a chief binary inside a cargo `target/`.
  `uninstall` unloads and removes it. Elsewhere `install` says to use the system's own service
  manager.

### Who may use the dashboard

The dashboard starts and steers sessions that run commands on this machine, so:

- **A token.** `<root>/serve-token` (mode 0600, made on first use, 244 random bits) is the one
  secret. `chief dashboard <slug>` prints `http://<host>:<port>/login?token=<token>&next=/p/<slug>`
  and opens it. `GET /login` with the right token sets the cookie `chief_token` (`HttpOnly`,
  `SameSite=Strict`, a year) and redirects to `next`, only ever a path on this server. Every
  other path but `/assets/*` needs that cookie or `Authorization: Bearer <token>`; without it an
  API call is 401 and a page is a note on how to get the link. Tokens are compared in constant
  time.
- **Its own origin.** A request that proves itself with the cookie and changes something (any
  method but GET and HEAD), or opens the terminal socket, must carry an `Origin` that names the
  host it was sent to, else 403: another site's page can make a browser send its cookie here,
  but not under this origin. A client that writes the Bearer header itself is exempt.
- **Opt-in per project.** Only projects with `[dashboard] enabled = true` that aren't archived
  are served (`http::in_project`); any other slug is 404 with how to turn it on. Slugs are
  checked against `[a-z0-9-]` before they name a folder.
- **Localhost by default.** Reaching it from another machine is a choice made in `serve.toml`
  (`bind = "0.0.0.0"`, or a tailnet address); the token still gates it, but travels in clear
  over plain HTTP, so that is for a network the user trusts, or behind a TLS proxy.
- Responses carry `X-Frame-Options: DENY`, `X-Content-Type-Options: nosniff`, `Referrer-Policy:
  no-referrer` and a `Content-Security-Policy` that allows only the server's own scripts.

### API (`http.rs` routes to `view.rs`)

JSON; a failure is `{"error": "<plain message>"}` with 400 (what the tools refuse), 401, 403 or
404. Every handler runs on the blocking pool with `tools::with_project`.

| Route | Does |
| --- | --- |
| `GET /`, `GET /p/<slug>` | The page |
| `GET /api/projects` | `{projects: [{slug, name, goal, waiting, working, open, routines}], version}`: the projects served, from stored rows |
| `GET /api/p/<slug>/board` | The board (below), from stored rows |
| `GET /api/p/<slug>/stream` | The board as server-sent events: one `board` event with the board's JSON whenever it changed |
| `GET /api/p/<slug>/sessions/<name>` | One session of the project in full: its board entry plus `task`, `reports` (newest first, 50), `events` (40) and `subagents` mid-turn |
| `POST /api/p/<slug>/threads/<name>/<action>` | `wake` and `stop` (`tools::wake_thread`, `stop_thread`), `resolve` and `reopen` (`registry::set_resolved`, reason `user`), `clean` (`cleanup::clean(Safe)`). `{ok, message}`. A blocked cleanup is an error that names `x` on the board and `chief clean --discard`: discarding unsaved work needs the loss shown and the name typed (§12), which the dashboard doesn't do. Never the coordinator |
| `POST /api/p/<slug>/routines`, `PATCH`/`DELETE …/routines/<name>`, `POST …/routines/<name>/run` | `routines::create`, `update`, `delete`, `run(Manual)` (§13), as the user (`Author::User`): whoever holds the token is, so a command saved here is approved |
| `POST /api/p/<slug>/routines/<name>/approve` | `routines::approve`: the user's yes to its command (§13 "Approval"). `{command}`, the words the page showed, is optional; a command that reads otherwise now is a 400. The routine |
| `GET /api/p/<slug>/routines/<name>` | The routine in full (`routines::read`): as `read_routine`, with all its notes (256 KB), 40 findings, 400 points a series and its command's 30 latest runs |
| `GET /api/p/<slug>/files` | `{instructions, memory}`: `instructions.md` and `memory/MEMORY.md`, to read |
| `GET /api/p/<slug>/term?target=<thread>&cols=&rows=` | The terminal socket (below); no `target` is the coordinator |

The board (`view::board`): `project` (slug, name, goal, repos, default repo, thread model and
effort, max_parallel), `groups` (§5 order, key and label), `coordinator` (null before it first
started) and `threads` in board order, `routines` (as `list_routines`), `guard_denials`, and
`error` when a listing failed (the stored rows are shown). A session: `name`, `title`, `group`,
`waiting_reason`, `asleep`, `started`, `running` (background work), `branch`, `pr` (`url`,
`state`, `draft`, `review`), `account`, `routine`, `repo`, `model`, `effort`,
`last_activity_at`, `last_report_at`, `report_preview`, `resolved_reason`, `cleaned`, `removed`,
`cleanup_blocked`, `limit`, `moved`, for a routine's run its `finding` (what it said it came to); and for a working or waiting thread what it asks (`asks`:
`label`, `text`, from `waiting::asks`) or does (`doing`: its own latest call), as the terminal
board's detail (§10).

**Live.** While anyone watches a project's stream, one refresher per project (`http::refresher`)
does what the terminal board's refresh does (§10): every 2 s it syncs with `claude agents` in
every account the project uses and publishes the board when its JSON changed; every 30 s it
refreshes pull requests (and cleans up merged threads when `cleanup = "merged"`, §12). An action
makes it read at once. It ends 10 s after its last viewer left, so a project nobody looks at
costs nothing.

### The conversation (`term.rs`)

Claude Code has no command that sends a prompt into a running background session (§1), so the
dashboard shows the session itself: `claude attach` on a pseudo-terminal of its own
(`embed::Pty`), its bytes carried over a WebSocket to a terminal emulator in the page
(xterm.js). What `chief pane` does for a terminal (§9), for a browser tab: the real prompt,
permission prompts, questions and slash commands all work, for the coordinator and any thread.

- Before attaching, the session is brought up as the pane does: the coordinator through
  `generated::rewrite` and `project::ensure_coordinator` (started on first use, respawned when
  asleep, as `chief up`); a thread through `pane::wake`. A cleaned thread and one removed outside
  chief are refused with §6's messages.
- Frames: binary is terminal data, either way. Text is JSON: from the page `{"type": "resize",
  "cols", "rows"}`; to the page `{"type": "status", "text"}` while the session is brought up,
  `{"type": "ready", "name"}` once attach runs, `{"type": "exit"}` when attach ended, `{"type":
  "error", "text"}`.
- Closing the tab, leaving the Conversation tab or selecting another session closes the socket,
  which ends that `claude attach` (SIGTERM, a grace period, SIGKILL). The session keeps running.
- The dashboard's terminals are independent of `ui_state.pane_target`: they don't move the
  terminal pane, and each browser tab has its own attach.

### The page (`src/serve/web/`)

`index.html`, `app.css`, `app.js` and the vendored xterm.js 5.5.0 with its fit addon
(`vendor/`, MIT, licence beside them), served from `/assets/`. One page per project:

- A top bar with the project (a menu when several are served) and its goal, and whether the
  stream is live.
- The board on the left: a red strip when threads wait on the user (it opens the first),
  Overview when the project has routines, the
  coordinator, the threads by group as on the terminal board (same glyphs and colours, the same
  tags as chips, what a working thread does and a waiting one asks, Resolved folded), then the
  routines, each with a glyph for how it stands (`!` when it needs the user, `$` for one with a
  command, `↻` for a session, `‖` paused), its state's line (else its schedule in words), its
  next run, and chips for `approve`, `failing` and `running`; and `+ New`.
- The selection on the right. A session: its state, the actions that apply (Open PR, Wake or
  Stop, Resolve; Reopen and Clean up when resolved; Stop, Clean up and deleting a routine ask
  once more), what it waits on as a callout, and tabs: Conversation (the terminal), Reports (its
  task and every report, as Markdown), and Activity (its latest calls and subagents) for a
  thread or Project (settings in brief, instructions and memory, to read) for the coordinator. A
  routine has two tabs. Status: for a session, what its last run found (outcome, headline,
  details, a link to the run's report); for one with a command, how it stands (§13 "State": the
  word, the line, since when it fails), the command, what its last run printed and the session
  that looked into it; then a line chart per series it tracks, the findings (for a command,
  the times it had news), its command's runs (when, how each ended, by hand or not, each
  opening to what it printed) with the path of the whole log, and its notes. Settings: its
  form (title; what runs: a Claude session, a command, or a command then a session, which
  shows the prompt, the command and its time limit as they apply; a schedule builder that
  writes the cron line; repository, model, effort, whether the coordinator is told), Run now,
  Pause or Resume, Delete, and its sessions. A routine whose command waits for approval says
  so above both tabs, with the command in full and Approve (§13 "Approval").
- **Overview**, where a project with routines lands: what needs the user (threads waiting, and
  routines whose state does: a command to approve, a failing command, a run that said
  `attention`), then a tile per routine with its state as an icon and a word, its line, the
  latest value of what it tracks with a sparkline, whether it is a command, its schedule, next
  run and last run.
- Charts (drawn as SVG, no library): one series a chart, never two scales on one; a 2px line in
  one validated series colour per theme, round-number gridlines, a crosshair that reads the
  nearest run's value and time, and every value as a table under the chart. A change since the
  run before is shown in neutral ink: chief doesn't know whether up is good.
- The selection lives in the URL's hash, so reload and back work. Below 860 px the board is the
  home and a selection opens over it. Light and dark follow the system; the terminal is always
  a dark screen.
- Everything shown from a session (titles, reports, calls) is inserted as text or through the
  page's own Markdown renderer, which escapes first and produces only its own tags.
