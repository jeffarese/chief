# Local Projects: implementation plan

A Rust TUI that recreates Claude Projects locally: you talk to one coordinator, it starts and steers
background Claude Code threads, and a board on the right parks each thread by status.

## What the spike proved (Claude Code 2.1.310)

| Question | Answer |
| --- | --- |
| Background threads | `claude --bg --name … --model … --effort … --permission-mode auto --settings <hooks> --append-system-prompt …` works; prints a short id |
| Isolation | A background thread moves itself into `.claude/worktrees/<name>` on branch `worktree-<name>` before editing |
| States | `claude agents --json --all` gives `state` (working, blocked, done, …) and `status` (busy, waiting, idle, null when the process is gone) |
| Hooks in threads | SessionStart, UserPromptSubmit, Pre/PostToolUse, Notification, Stop all fire. Stop carries `last_assistant_message` (the full report). Notification carries `notification_type` (`permission_prompt`, `idle_prompt`) |
| Coordinator → thread | A coordinator session uses `ListAgents` + `SendMessage(notify_when_idle)`. An idle thread wakes, works, and the coordinator gets an idle notice as a new turn |
| Idle notice | Truncates the reply, so full reports must come from the Stop hook |
| Permission prompts | Thread shows `blocked/waiting` plus a `permission_prompt` notification; `claude attach <id>` shows the real prompt |
| Detach | Ctrl+Z (or ←, `/exit`) detaches; the thread keeps running |
| Revive | `claude respawn <id>` restarts a stopped thread under the same id with its saved options and history |
| Traps | Untrusted folders are refused. `claude --bg --resume <id>` with any extra argument starts a copy that shares the worktree, so never use it |

## Architecture (no daemon)

Claude Code's own supervisor runs every session. SQLite is the shared state. Each piece is a
subcommand of one binary, `chief`:

- `chief hook <event>`: hook command for every session. Reads the payload, writes the event and the
  derived thread state into SQLite. For the coordinator's UserPromptSubmit it returns
  `additionalContext` with a fresh board snapshot and the memory index.
- `chief mcp --project <p>`: stdio MCP server that Claude Code starts for the coordinator. Tools:
  `start_thread`, `list_threads`, `read_report`, `wake_thread`, `resolve_thread`.
  The coordinator still talks to threads with the built-in `SendMessage`.
- `chief board`: ratatui board, grouped by Waiting on you, Working, Ready for review, Idle,
  Resolved and Failed, with a thread detail view and a settings screen.
- `chief up <p>`: opens a Rex session. The left pane runs `claude attach <coordinator>`, the right
  pane runs the board. Enter on a thread swaps the left pane to that thread; Esc returns.
  Multiplexer access goes through a trait so tmux can be added later.

Per-project files in `~/.local/share/chief/<project>/`: `project.toml` (goal, repos, coordinator and
thread model and effort), `instructions.md`, `memory/MEMORY.md`, `chief.db`, generated `hooks.json`
and `mcp.json`. The coordinator runs in this folder with worktree isolation off and no shell; it
can read the repositories, but deny rules block its edits there and in `generated/`, so it edits
only memory and instructions. Threads run in the chosen repository and get their own worktree.

Thread names are unique (`<project>-<slug>-<4 hex>`) so `SendMessage` never hits two sessions.

### Message guard (enforced, not prompted)

`ListAgents` lists every Claude session on the machine, and other sessions accept messages by
default, so a routing mistake could push work into an unrelated project. Every project session
(coordinator and threads) therefore gets a `PreToolUse` hook, `chief hook guard`, that:

- denies `ListAgents` (the coordinator uses `list_threads`, which only returns this project's threads);
- allows `SendMessage` only when `to` names a live thread registered to this project (threads may
  message only their coordinator), checked against the registry by exact name and session id;
- logs every denied attempt to the board.

Before starting a thread, `chief` also checks `claude agents --json` so no other live session already
uses the new name. Milestone 2b includes a negative test: a coordinator that tries to message a
session outside the project must be blocked.

## Milestones and effort

| # | Work | Who |
| --- | --- | --- |
| 0 | Install the Rust toolchain, scaffold the Cargo workspace, write ARCHITECTURE.md with every contract: SQLite schema, hook-to-state mapping, MCP tool schemas, settings schema, CLI wrapper | Me (xhigh) |
| 1 | Core: settings, SQLite registry and migrations, `claude` CLI wrapper, `chief hook`, state derivation as a pure function tested against the spike's real payloads | Agent (high) |
| 2 | `chief mcp`, `chief init`, coordinator start-up and prompt v1, UserPromptSubmit context injection, message guard | Agent (high) |
| 2b | End-to-end check with cheap Haiku sessions: start, wake, permission, revive, report, and the guard blocking a message to a session outside the project | Me (xhigh) |
| 3 | Board TUI and settings screen, live refresh, PR status through `gh` | Agent (medium) |
| 4 | `chief up` with Rex layout and pane swapping | Agent (medium) |
| 5 | Coordinator prompt tuning on a real repo, cross-check review of the codebase, README | Me (xhigh) + review agent (xhigh) |

I review and test each milestone before the next starts. Local git only; nothing is pushed.

## Risks

- Relies on recent Claude Code features; `chief` checks for 2.1.310 or later at start.
- Each thread is a full session on your plan; default threads to a smaller model in `project.toml`.
- A thread's process is ended after about an hour unattached once finished; `wake_thread` respawns it.

## Milestone 2b results (live, Claude Code 2.1.310, Haiku sessions, 2026-10-26)

| Check | Result |
| --- | --- |
| Start: coordinator calls start_thread, subscribes, thread works in its own worktree, report stored, idle notice back | Pass |
| Context: every coordinator turn carries `<chief-board>` and `<chief-memory>` | Pass |
| Follow-up routed to the existing thread by SendMessage, same branch | Pass |
| Permission prompt: board shows Waiting on you (permission); approving returns it to Working | Pass |
| Respawn: stopped thread shows asleep; coordinator calls wake_thread, then messages it | Pass |
| Guard: coordinator → outside session denied and recorded; thread ListAgents and thread → outside session denied | Pass |
| Thread killed mid-turn leaves Working (shows Idle, asleep) | Pass after fix |
| Hook error log | Empty over 125 events |

Fixed during the run: trust inherits from trusted parents but stops at a git root (chief refused
valid folders); "working" now means a busy process (a finished thread stayed in Working because
`claude agents` kept `state: working`); threads get their own ListAgents refusal.

For prompt tuning (M5): the coordinator offers to remove worktrees (it can't; cleanup is the
user's); a coordinator brief said "the branch this thread was created with", which confused the
thread; the coordinator correctly refused to relay a request that would route around its own guard.

## Milestone 4–5 live results (real Rex, real Claude Code 2.1.310, Haiku sessions, 2026-10-26)

| Check | Result |
| --- | --- |
| `chief up` opens the Rex layout: coordinator via real `claude attach` on the left, board on the right | Pass |
| Enter on a thread switches the left pane to that thread; Esc returns to the coordinator; the board marker follows | Pass |
| Enter on a stopped thread respawns it, then attaches | Pass |
| Guard with the chief binary missing: the coordinator's SendMessage is blocked with chief's message; state hooks fail open | Pass |
| Board cleanup: `x` on a resolved thread with an unpushed commit stops and lists the commit; the session keeps running; typing the name discards and removes worktree, branch and session | Pass |
| `chief down` / `--stop-threads` | Pass |

Fixed during the run: the coordinator never subscribed to its threads (told "with no message", then
`""`, Haiku emitted `"message": ,`; the literal now omits the key, verified with a fresh
coordinator); `chief … | head` panicked on a closed pipe.

## Review fixes, final live run (merged build 852e998, real Rex and Haiku sessions, 2026-10-26)

| Check | Result |
| --- | --- |
| Coordinator asked to Write a repo file and Edit `project.toml` itself | Both denied by the generated deny rules ("File is in a directory that is denied by your permission settings"); nothing changed |
| Coordinator SendMessage to an outside session, by name and as `name [ref]` | Both denied and recorded; the ref denial says to use the bare name |
| A real thread's job state | `~/.claude/jobs/<short>/state.json` has `worktreePath` and `worktreeBranch` matching `git worktree list` |
| Board `x` on a resolved thread with an unpushed commit | Modal lists the commit; a wrong name does nothing; the right name removes worktree, branch and session; main untouched |
| `chief down --stop-threads` while the pane shows a thread, then `chief up` | Pane back on the coordinator, the stopped thread stays stopped, the stored Rex session id is cleared and replaced |
| `claude --bg`'s transient daemon | Detaches into its own process group (so proc's group kill on timeout can't reach it) and exits by itself once idle |

## Rex optional: live run (real Rex, real tmux 3.7b, Haiku sessions, 2026-10-26)

| Check | Result |
| --- | --- |
| `chief up` with Rex running: Enter / Esc on the board switch the left pane by signal | Pass; the previous screen is fully cleared |
| `chief up` inside tmux with Rex unavailable: window `chief-<slug>` opens next to the current one, pane left, board right; Enter / Esc switch | Pass after a fix (`new-window -t` takes a window, not `$TMUX_PANE`) |
| `chief down` from a terminal outside tmux closes the stored tmux window | Pass |
| No Rex, not in tmux: `chief up` prints the two commands; run in two unrelated windows, Enter / Esc still switch | Pass |

## Account per project: live run (project on agp account cpb, commands from a default-account shell, 2026-10-26)

| Check | Result |
| --- | --- |
| `chief init` from a cpb shell records `claude_config_dir = "~/.claude-cpb"` | Pass |
| `chief up` from a shell without `CLAUDE_CONFIG_DIR`: says it uses the project's account; coordinator listed by cpb, not by the default account | Pass |
| `chief pane` / `chief board` started by hand without `CLAUDE_CONFIG_DIR`: pane attaches through cpb; a thread starts on cpb; Enter / Esc switch | Pass |
| Thread branch taken from cpb's job state (`worktreeBranch`) | Pass |
| `chief status` from the default account: nothing marked removed; the attach hint carries `CLAUDE_CONFIG_DIR` | Pass |

## Threads across agp accounts: live run (coordinator on the default account, threads on cpe and laura, Haiku, 2026-10-26)

| Check | Result |
| --- | --- |
| `chief init`: offers trust per pool account; yes runs `agp exec <id> -- claude`, no lists the fix command; each account's `.claude.json` updated only where accepted | Pass |
| `start_thread` without `account`: best-scoring allowed account (cpe); limited carlos never considered | Pass |
| `start_thread` with `account: laura`: thread on laura | Pass |
| Idle notice by `uds:` address wakes the idle coordinator, which posts the update | Pass (prompt fixed: it no longer warns the user it may not hear back) |
| Follow-up from the coordinator to a thread on cpe by address; thread's reply to the `from=` address reaches the coordinator | Pass (prompt fixed: the coordinator now asks for a reply instead of dropping the request) |
| Stop the laura thread, follow-up: `wake_thread`, new address (new pid), message and idle notice delivered | Pass |
| Guard with real listings: by-name to another account's thread denied with its address; exact address allowed; an outside socket denied; thread → coordinator by name denied with the `from=` hint; denials counted on the board | Pass |
| Board tags (PB3, PB4), detail and `chief status` show the account; Enter attaches through the thread's account | Pass |
| Settings: "Account trust" check; an untrusted pair's Enter opens Claude in that account on the board's terminal, then re-checks | Pass (summary reworded: `N untrusted · checked <labels>`) |
| `chief clean --discard --yes` removes each thread in its own account; worktrees gone | Pass |

## Subagents: live run (coordinator on the default account, thread on cpe, Haiku, 2026-10-26)

| Check | Result |
| --- | --- |
| A thread starts a named subagent; the subagent reports to `team-lead`; the thread sends it a follow-up by name and gets the answer | Pass, 0 guard denials |
| The coordinator starts its own subagent and hears its report | Pass, 0 guard denials |
| Each subagent recorded under its own session (`teammates`) | Pass |
| Guard: an Agent named like the coordinator or a project thread denied at spawn; a free name allowed; a thread messaging the coordinator's subagent, the coordinator messaging a thread's subagent, a lead messaging `team-lead` all denied; the thread's own subagent allowed by name and by agent id | Pass (the thread's catch-all denial reworded: it now names subagents, and the reply address for a thread in another account) |
| `claude respawn` of the thread: SessionStart `resume` forgets its subagents; the old name is denied | Pass |
