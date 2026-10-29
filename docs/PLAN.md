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

## Background work: live run (mbp-m5max, coordinator Opus low, threads Sonnet medium, 2026-10-26)

| Check | Result |
| --- | --- |
| A thread's subagent told to run `./slow-test.sh` (45 s) with `run_in_background` | Denied by the PreToolUse hook; the subagent reran it in the foreground (timeout 600000) and handed back "3 passed" |
| What the Agent tool starts on this Mac | An async agent (`async_launched`, ends with `SubagentHandback`), not the teammate the first Mac got |
| Coordinator subscribed while the subagent worked | No early idle notice: async agents keep the thread busy. One update after the real finish, no early "done" |
| Board count while the async agent worked | First run failed: the early Stop listed the agent as `subagent`, not `teammate`, and chief cleared it. Fixed (82830c5: any task but `shell` counts); re-run held `1 subagent` in Working from the early Stop to the hand-back |
| Spike, polled every 2 s: an async agent, and a lead's own background command | Both keep the lead `busy` until done, so the final-report request now fires only after a busy teammate made the thread look idle (not seen live here: this Mac makes no teammates) |
| Auto mode's own agent (a PreToolUse and a SubagentStop under unknown agent ids) | Counted as nothing, and never denied: the denial now needs a recorded subagent |

## Threads that wait on subagents or background commands (built)

Problem (side-agent note after the subagents run): a thread that hands work to a subagent ends its
turn while it waits. Claude Code then reports it idle, the coordinator's idle notice fires, and
the board shows it idle, so the coordinator can report half-finished work as done.

### Verified live (Claude Code 2.1.310, Sonnet sessions with a payload-logging hook, 2026-10-26)

Hook payloads: `tests/fixtures/hooks/subagent-wait-events.jsonl` (three leads: a follow-up to an
idle subagent with a Stop-hook block, a `TaskStop`, and background commands).

- A lead that ends its turn while its subagent works is `status: idle` in `claude agents`, and its
  `notify_when_idle` subscriber gets "is idle now" right away.
- Subscribing again to a session that is already idle notifies at once (at the subscriber's next
  turn boundary), quoting the same turn. Re-subscribing can't wait for the next turn: a
  coordinator that re-subscribes on every early notice would loop.
- `SubagentStart` fires at spawn and again each time a message wakes an idle subagent;
  `SubagentStop` fires at the end of every subagent turn, and on `TaskStop` (then
  `background_tasks` lists no teammate). So a subagent is busy from `SubagentStart` to
  `SubagentStop`.
- When a subagent ends a turn, its lead gets `<teammate-message teammate_id="…">` with
  `{"type":"idle_notification",…,"result":"<its last reply>"}` and runs a turn. No
  UserPromptSubmit fires for it, but its Stop does. So the lead always Stops again after its
  subagents' last `SubagentStop`. A subagent's `SendMessage(to: "team-lead")` can wake the lead
  before that subagent's own `SubagentStop`.
- The lead's Stop payload has `background_tasks`: `[{id, type: "teammate" | "shell", status:
  "running", description, command?}]`. Teammates stay listed as `running` while idle; a
  subagent's background shells are listed too.
- A lead's own background command (`run_in_background`) wakes it when it finishes:
  UserPromptSubmit with `<task-notification>…<status>completed</status>`, then a Stop.
- **A subagent's background command does not wake the subagent** (seen twice: `sleep 60`, and a
  real `cargo test`). Its `<task-notification>` is enqueued on the lead's transcript and never
  dequeued; the subagent stays idle and the lead gets no turn. This is Claude Code's behaviour.
- A foreground `sleep N` is refused by Bash ("Blocked: standalone sleep"); models then run it in
  the background.
- A Stop hook printing `{"decision":"block","reason":"…"}` works in a background session: the lead
  went on, sent the `SendMessage` the reason asked for (delivered as a cross-session message), and
  its next Stop had `stop_hook_active: true`.

### Design

1. **Busy subagents.** Migration 12: `teammates.busy`, `sessions.subagents_busy`,
   `sessions.background_running`, `sessions.stopped_early_at`. In the hook transaction:
   `SubagentStart` sets busy (after recording the teammate), `SubagentStop` clears it. A lead's
   own Stop whose `background_tasks` lists no `teammate` clears them all. `subagents_busy` is
   recounted after each. `background_running` = `background_tasks` entries of type `shell` with
   status `running` at the lead's last Stop; set at every Stop, cleared by SessionStart (not
   compact). Subagent hooks still don't change the lead's `last_hook` handling.
2. **State (§5 rule 5).** Working when the process isn't gone and (`cc_status` busy, or the
   provisional working hook, or `subagents_busy > 0`, or `background_running > 0`). Waiting rules
   still come first.
3. **The final report.** A thread's Stop with work still running sets `stopped_early_at`. Its
   Stop with nothing running clears it, and, unless `stop_hook_active`, prints
   `{"decision":"block","reason":…}` asking it to send its final report to the coordinator at the
   coordinator's address: its name, or `uds:/tmp/cc-socks/<pid>.sock` for a thread in another
   account (from a project-account `claude agents`, ≤5 s; no block if the coordinator isn't
   live or the listing fails). The guard already allows both. Stop notifications (`[ui] notify`)
   skip early Stops and blocked Stops.
4. **Coordinator.** Snapshot line and `list_threads` show `N subagents working` /
   `N background commands`. Prompt: an idle notice from a thread the board shows working isn't
   a finish: say nothing, don't subscribe again (the notice would come straight back); its final
   report arrives as a message.
5. **No background commands in subagents.** A second side-agent note found the gap in a
   prompt-only rule: a subagent stalled on its background command is between turns, so nothing
   counts it, and as soon as anything wakes its lead (the lead's own command ending, a message),
   the lead's Stop finds nothing running and asks for the final report on half-done work. When
   nothing wakes the lead, no hook fires at all, so detecting the stall at the lead's next Stop
   can't cover the common case either. So the regular PreToolUse hook (matcher `*`, fails open,
   unlike the guard) denies a Bash call from a subagent with `run_in_background: true`: run it in
   the foreground with a long enough timeout, or leave anything over 10 minutes to the lead.
   Thread prompt: say what it waits for when it ends a turn early (chief asks for the final
   report once everything is done), and tell subagents to run tests and builds in the
   foreground.
6. **Board.** Tags `N subagents` / `N bg commands` on Working rows; detail line.
7. **Contract.** ARCHITECTURE §1 (the facts above), §5 (hook table, rule 5, the final report),
   §7 `list_threads`, §8 prompts, the board section; `CLAUDE.md`: hooks print only the guard
   decision, the coordinator's context and the Stop block.
8. **Tests.** Derivation rows; hook replay of the fixture (busy counts, early stop, block once,
   no block with `stop_hook_active`, TaskStop clears); context and tag rendering; migration
   count 12. Then a live run with chief: a thread on another account whose subagent runs a
   foreground test command, checking the board, the coordinator's early notice and the final
   report.

## Onboarding and TUI (built, 2026-10-27)

Problem (user feedback): the TUI didn't follow TUI conventions, and starting was confusing.
`chief init <name> --goal … --repo …` meant a trust round trip per folder (open `claude`, accept,
`/exit`), then `chief up <slug>`, and nothing on screen said the left pane was the conversation or
how to get back to it. Claude Projects gets this right: you set the goal, pick a few settings, and
you are in the conversation.

1. **`chief` alone is a launcher** (ARCHITECTURE §10a): a list of projects with what needs you,
   and `+ New project`. With no projects it opens on a four-step wizard (About, Code, Models,
   Review), which creates the project, trusts what you agreed to and opens the layout, showing
   each step. The conversation is the coordinator, idle and waiting for you (the user chose no
   first turn). Without Rex or tmux the launcher runs the pane in its own terminal.
2. **Trust without the round trip** (§3 "Trust"): chief writes the key Claude's dialog writes,
   only after a yes (one question in `init`, a checkbox in the wizard, `chief trust <slug>`,
   Enter in the board's settings). It trusts its own project folder without asking.
3. **Board** (§10): framed panels, the coordinator as a selectable first row, a key bar that
   always fits and names what works on the selection, `? help` with a legend of the marks, the
   mouse, a coordinator detail that says what to do next, settings in sections with a line on each
   field and Ctrl+S.

Not verified live yet: that a background session accepts a folder chief marked (the key and
value match what M2b read from Claude's own dialog, but no live session has started in a
chief-marked folder). Check with `chief new` on a fresh repo: the coordinator must start, and so
must the first thread.

## Built-in layout (built, 2026-10-29)

Problem (user request): without Rex or tmux, `chief up` printed two commands to run side by side.
chief now draws the layout itself in the terminal it runs in (ARCHITECTURE §9 "Built-in layout"):
`claude attach` on a pseudo-terminal of its own, emulated by libghostty and drawn into ratatui's
buffer on the left, the board on the right, Ctrl+\ or a click to switch sides.

1. **Emulator** (`src/embed/`): libghostty-vt from libghostty-rs at a pinned commit, not
   crates.io's 0.2.2. That release pins a Ghostty that needs exactly Zig 0.15.2, which can't link
   native binaries against the macOS 26 SDK (its `libSystem.tbd` lists only arm64e). The pinned
   commit builds with Zig 0.17. A spike checked the queries Claude Code's terminal layer sends
   (DA1, DSR, OSC 10/11, XTVERSION, DECRQM 2026, kitty keyboard, size), colours, wide characters
   and vim on the alternate screen through the real pty path. libghostty's
   `key::Encoder::encode_to_vec` fails on any key whose first try runs out of room (it reserves
   from the length, not the capacity), so chief encodes into a stack buffer.
2. **Layout** (`src/builtin.rs`): the pane loop's steps on the embedded terminal, the wake on a
   worker, the same pane record and signals as `chief pane`, so the board's switch and
   `chief down` work unchanged. `chief up --layout builtin` forces it.
3. **Optional**: the `builtin-layout` cargo feature, on by default; `--no-default-features`
   builds without Zig and keeps the printed commands.

Tested on a pseudo-terminal with the fake `claude` (`tests/builtin_cli.rs`). Not verified live
yet: a real `claude attach` in the embedded terminal (its prompt, Shift+Enter, the agent view's
mouse and paste). Check with `chief up <slug> --layout builtin` on a live project.

## Day-to-day management (built, 2026-10-29)

Problem (review of the UX): getting to what needs you took looking, nothing could stop a thread
that was off track, cleaning up meant `x` one thread at a time, and the launcher's list only
grew.

1. **Board** (ARCHITECTURE §10): `n` shows the next thread waiting on you; `S` stops a running
   thread after a confirm (`claude stop`, the main pane moved off it first, since a pane showing
   it would respawn it); `X` runs `chief gc` for the project. The key bar offers `S` on a running
   thread and `w` only on an asleep one.
2. **Coordinator**: `stop_thread` (§7), which the prompt keeps for when the user asks or a thread
   is off track and a message didn't help.
3. **Archive** (§2, §10a): `chief archive <slug>` is `down --stop-threads`, then the project
   leaves the launcher's list and `chief status`; it isn't archived when down didn't finish
   cleanly. `chief unarchive`, or `chief up`, brings it back. In the launcher, `a` archives (after
   a confirm) or unarchives, and `A` shows the archived projects.
4. **Built-in layout** (§9): the terminal's title says how many threads wait on you, and the
   conversation is marked while it shows history.

Tested with the fake `claude` only. Not verified live yet: `claude stop` on a thread mid-turn
(what its board row and the coordinator see afterwards), and the title in Ghostty and iTerm2.
