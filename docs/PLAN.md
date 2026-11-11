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

- Relies on recent Claude Code features; `chief` checks for 2.1.316 or later at start (the coordinator's waiting watcher needs `asyncRewake`).
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

## Quitting in one key (built, 2026-10-29)

Problem (from use, in a Rex block): `chief pane` gave the terminal to `claude attach`, so the only
way out was Claude's: Ctrl+C twice, twice more in its agent view, then `q` on chief's "you left"
screen.

1. **Relay** (ARCHITECTURE §9): `chief pane` runs attach on a pseudo-terminal of its own and
   copies the bytes both ways untouched, so Claude still draws on the user's terminal (its
   scrollback, selection, rendering) while chief sees every key. Ctrl+Q, which Claude Code 2.1.312
   doesn't bind, quits from anywhere: as a plain byte, in the kitty keyboard protocol or under
   modifyOtherKeys, never inside a paste. Sessions keep running. No emulator, so no Zig on this
   path. A first version ran the pane on the built-in layout's emulator to put a bar under
   Claude; it cost the terminal's own scrollback and selection, and tied the Rex path to
   libghostty.
2. **Built-in layout**: the conversation's last row is chief's bar (`chief · coordinator`,
   `^Q quit`, `^\ board`, clickable), and Ctrl+Q quits from either side.
3. **No Zig**: the built-in layout's emulator is `alacritty_terminal` 0.26 (pure Rust, crates.io)
   instead of libghostty, so the `builtin-layout` feature, the pinned git dependency and Zig are
   gone. chief encodes keys, mouse and paste itself (Alacritty keeps those encoders in the app),
   and a second parser answers XTVERSION and DA3, which the crate never sees. DA1 now reports a
   VT102. alacritty_terminal panics on a kitty keyboard push 4096 deep; chief drops pushes past
   1024.
4. **Where Ctrl+Q shows** (Rex, tmux, manual): Claude's screen is Claude's, so the board's key
   bar ends with `^Q quit`, and Ctrl+Q there quits chief too (SIGUSR2 to the pane, which quits as
   for its own Ctrl+Q). The "you left" screen says Ctrl+Q would have done it at once. A one-row
   Rex block under Claude as a bar was tried and dropped: Rex gives a block at least 4 rows, plus
   about 3 of divider.
5. **Board width in Rex**: Rex 0.1 ignores `rex run --ratio` (every new split is recorded at
   half), so the board took half the window. chief now sizes the split with
   `session.resize_pane` after `rex run`: the board gets its 32 %.

Tested with the fake `claude` (`tests/pane_cli.rs`, `tests/builtin_cli.rs`) and the emulator's
unit tests (`embed::`); the resize call against Rex 0.1 by hand (a probe block given 25 % got 40
of 167 columns). Not verified live yet: Ctrl+Q reaching chief through Rex, Claude's kitty
keyboard query answered by the user's terminal through the relay, and a real `claude attach` in
the alacritty_terminal emulator (Shift+Enter, the agent view's mouse and paste).

## Threads through agp: usage limits and moves (built, 2026-11-01)

Problem (from use on mbp-m2max): threads ran on `claude --bg`, so a thread whose account hit its
5-hour limit sat blocked until someone noticed. The board showed it as Waiting on you · question,
and the coordinator, stuck in the same account, could do nothing. agp moved sessions between
accounts only while it was their parent process (its dock, or `-p --on-limit`).

1. **agp `--bg`** (agent-prism, DESIGN §11d): `agp [opts] -- --bg …` starts the session on the
   account it picks and leaves a detached watcher per session. On a limit the watcher stops the
   session and resumes the same conversation in another pool account (`switch`), or wakes it at
   the reset (`wait`). `agp bg --json` lists the sessions, `agp bg watch` takes one over,
   `agp bg move` moves one now. The resume facts behind it were checked live first (ARCHITECTURE
   §1): across accounts the same session id comes back in its worktree; in an account that has
   the job, only the no-flag form avoids a copy.
2. **chief starts threads through agp** (`[threads] runner = "agp"`, the default; `on_limit =
   "switch"`; both on the board's settings screen). A thread placed in the project's account is
   started by `claude` and handed to `agp bg watch`. The coordinator is watched with `wait`,
   never moved: threads reach it by name.
3. **Following moves** (`src/moves.rs`): every refresh reads `agp bg --json` first and points a
   moved row at its new account. It keeps the accounts before in `prior_accounts` (the first one's
   job owns the worktree) and the note "moved from PB2 to PB4 after the 5-hour limit". Every
   revive (`wake_thread`, the pane, `chief up`) waits while agp says `moving`, so nothing starts
   the session again in the account agp just stopped it in, and it hands the session back to
   `agp bg watch` before `claude respawn`.
4. **Limited** is a group of its own (rule 3a), from the job state's `rate limited` block, no
   longer a question. The coordinator's watcher reads limits and moves itself every 10 s and
   wakes the coordinator with `<chief-moved>` (subscribe again at the new address) and
   `<chief-limited>` (agp didn't move it: tell the user). `move_thread` moves one on request.
5. **Cleanup** of a moved thread removes the jobs in its later accounts first, then runs `claude
   rm` where the worktree's job is.

Tested with the fake `claude` and a fake `agp` (`tests/tools.rs`, `tests/cleanup.rs`) and unit
tests (`moves::`, `waiting::`, `agp::`, `claude::`); agp's side with its own fake-claude
integration tests. Not verified live yet: a real limit moving a real thread end to end, and the
coordinator's `<chief-moved>` turn.

## Routines and the web dashboard (built, 2026-11-09)

Problem (user request): a project could only do what someone asked for in the conversation, and
could only be seen in a terminal. Projects should also repeat work on a schedule, with the
coordinator in charge of it like the threads, and show themselves in a browser.

Decisions (asked first): one optional `chief serve` fires routines and serves the dashboard,
rather than OS scheduler entries or firing only while chief is open; a run is a fresh thread,
not a turn of the coordinator; the conversation in the browser is the real `claude attach` in an
embedded terminal, since Claude Code has no command to send a prompt into a running session;
localhost by default, with a token, and another address only by choice.

1. **Routines** (ARCHITECTURE §13; `src/routines.rs`, `src/cron.rs`, migration 19): a prompt on
   a schedule (five cron fields in local time, `@daily`-style shorthands, `every 30m`; at most
   every 5 minutes), stored in the database. A due run claims its occurrence, is skipped while
   the routine's last run still works, puts idle earlier runs away, and starts a thread like any
   other, tagged `routine <name>`. The coordinator has `list_routines`, `create_routine`,
   `update_routine`, `delete_routine` and `run_routine`; `chief routine` does the same from a
   shell.
2. **Hearing of a run**: nobody subscribes to a thread the scheduler started, so the
   coordinator's watcher tells it with `<chief-routine>` when a run finishes, and `chief serve`
   respawns a coordinator that sleeps so that its start-up watcher can (a side-agent note caught
   that an overnight run would otherwise be heard by nobody until the user next wrote).
3. **`chief serve`** (§14; `src/serve/`): one per chief home, by a lock; `start`, `stop`,
   `status`, and `install` for a launchd agent. `chief up` starts it for a project that has a
   routine or its dashboard on.
4. **Dashboard** (§14): `chief dashboard <slug>` turns it on and prints the sign-in link. The
   board live over server-sent events (the terminal board's refresh, while someone watches),
   reports as Markdown, activity, the thread actions, a routine editor with a schedule builder,
   and the conversation in xterm.js over a WebSocket. axum; the page is plain JS and CSS compiled
   into the binary.

Tested with the fake `claude` (`tests/routines.rs`, `tests/serve_cli.rs`: the server on a free
port over raw sockets, the terminal socket included) and unit tests (`cron::`, `waiting::`,
`serve::`). The page was checked in headless Chrome against a seeded home: light, dark and phone
widths, the terminal's round trip, creating and editing a routine.

Not verified live yet: a real routine run end to end (`chief serve` starting a real thread and
the coordinator's `<chief-routine>` turn); the respawned coordinator's SessionStart watcher
delivering that notice (§1 verified `asyncRewake` on SessionStart, not after a respawn by the
server); a real `claude attach` in xterm.js (its keyboard protocol queries, Shift+Enter, paste,
the agent view's mouse), and two attaches to one session at once (the terminal pane and a
browser tab); the launchd agent (`chief serve install`).

## Routines that follow things: memory, outcomes, values, push (built, 2026-11-10)

Problem (user request): routines should carry the things one tracks and automates outside code
too (a mortgage, flights), as a single source, and do it properly: efficient, with memory. As
built the day before, each run started from nothing, every finished run cost a coordinator's
turn and a notification, the result was text only, nothing reached a phone, and a project
needed a repository.

1. **Memory** (ARCHITECTURE §13 "What a run leaves"): each routine has a folder with a notes
   file every run reads first and rewrites. Runs stay fresh sessions, so a routine's hundredth
   run costs what its first did; the notes are what carries over.
2. **Outcomes**: every run ends with `report_outcome` (`nothing`, `update`, `attention`, and a
   headline); the Stop hook asks a run that forgets. The outcome decides who hears (§13 "Who
   hears of a run"): nothing tells nobody, an update waits for the coordinator's next turn,
   attention wakes it and reaches the user. The per-run desktop notification is gone.
3. **Values**: `record_value` stores what a routine tracks as series over time (migration 20).
4. **Run tools**: a run gets its own MCP server (`chief mcp --routine <name>`) and settings
   that allow it; the coordinator gets `read_routine` (notes, findings, values) to answer
   without starting anything, and guidance to keep runs cheap.
5. **Push** (`[push] command`, `src/push.rs`): the user's own command, with the message in its
   environment, for a run's attention and a thread's permission prompt.
6. **Workspace** (§3): a project without repositories works in its own folder.
7. **Dashboard**: an overview (what needs you, a tile per routine with its outcome, headline,
   latest value and trend), and a routine's status page (charts, findings, notes).

Tested with the fakes (`tests/routines.rs`, `tests/mcp_cli.rs`, `tests/serve_cli.rs`, `push::`)
and the page in headless Chrome against a seeded home project.

Not verified live yet: a real run using its tools (that a `--bg` thread started with
`--mcp-config` and the `mcp__chief` allow rule calls `record_value` and `report_outcome` without
a prompt, as the coordinator's own tools do); the Stop hook's outcome request on a real run; a
thread in a folder that isn't a git repository; a real push command. Still open from the review
below: an unattended run can't reach sites that need a login, and nothing gates a run on a cheap
check before a session starts (a script that says "nothing changed" for zero tokens).

Found on the way: `hooks_against_a_locked_database_log_and_exit_zero` fails about one run in
four on the commit before this work too. A hook can wait about 10 s on a locked database, not
the 5 s §4 says: the busy handler's clock starts again for each lock a statement waits on.

## Routines that are commands (built, 2026-11-11)

Problem (user request): every routine run was a whole Claude session, which is too much for a
check ("is staging up", "did the build pass") and rules out what a cron job does. This closes
the open item above (nothing gated a run on a cheap check).

1. **Kinds** (ARCHITECTURE §13; `src/command.rs`, migration 21): a routine is a prompt (a
   session each run, as before), a command chief runs itself with no session, or both: the
   command first, and a session only when the command has news, with its output in the brief.
   A routine with a command may fire every minute; a prompt alone stays at 5.
2. **A command's run** is a `routine_runs` row and a detached `chief routine exec`: `sh -c` in
   the routine's repository with a time limit, its output kept (the last 16 KB on the row, all
   of the latest in `last-run.log`). It speaks through its exit status, and when it knows more
   through `chief routine value` and `chief routine outcome`.
3. **News**: a command repeats itself, so only a change reaches anyone: the first failure, the
   first pass after it, an outcome it didn't say the run before.
4. **Approval**: the coordinator has no Bash, so a command it writes or changes waits for the
   user's yes (the board, the dashboard, `chief routine approve`); the user's own is approved.
5. **State**: one word and one line for how a routine stands (`routines::state`), shown the
   same by `chief routine list|show`, the board and the dashboard.
6. **Board**: a Routines section with each routine's state, a detail panel, and keys to
   approve, run and pause. **Dashboard**: the kind in the form, state on tiles and rows, the
   command's runs and output on its page, Approve.

Tested with real `sh` commands in-process (`tests/routines.rs`, `command::`) and through the
binary (`tests/serve_cli.rs`: the server running a due command detached, the command speaking
through the CLI, the dashboard's API), the board's section without a terminal (`board::`,
`tests/board.rs`), and by hand against a seeded project: the page in Chrome with the server
firing a command every minute, and the board in tmux.

Not verified live yet: a check's session started by real news; the coordinator writing a
command and the user approving it from the board; a command under launchd's environment.

## A coordinator that routes (built, 2026-11-11)

Problem (seen live): asked about an earlier conversation, the coordinator spent a turn
searching other accounts' session transcripts itself. Claude Projects' coordinator is a light
router (its docs: it "sees what threads report back, not every step they take", and even
read-only exploration is a thread's); chief's prompt said the same, and nothing kept it so.

1. **Its own looking is kept quick** (ARCHITECTURE §6, `src/lookups.rs`): its Read, Grep and
   Glob stay inside the project's folders, and a turn gets 6 lookups (searches, web calls,
   reads outside its own folder) before chief denies the next and says to start a thread.
   Its memory stays free to read.
2. **Lookup threads**: the brief routes anything that takes finding out to a thread that
   changes nothing and reports the answer, on a small model; earlier conversations are found
   that way and saved to memory. A thread with such a task makes no branch.
3. **A brief that went stale reaches the coordinator** (§8, `src/brief.rs`): `claude respawn`
   keeps the first start's prompt, so without this no prompt change ever reached a running
   project. Sending the brief through a hook didn't work live: Claude Code cuts a hook's
   context at about 10,000 characters and the brief with instructions is about 17,000, so
   the coordinator got a preview and read the rest only sometimes. chief now writes the brief
   to a file and tells the coordinator to read it, at SessionStart and with every prompt
   until it has. It also says when the coordinator's tools are older than the installed
   chief (`/clear` keeps the process) and what restarts it.
4. **No cloud scheduling**: every chief session is denied `RemoteTrigger` and `/schedule`;
   routines are the project's repeating work and run on this machine.

Tested through the hook command (`tests/hook_cli.rs`), `lookups::` and `brief::`. Seen live
on 2026-11-11: the coordinator kept to a few lookups and answered in place. Not verified live
yet: the read-this notice making a real coordinator read its brief; a lookup thread started
for an earlier conversation; the denials' wording; that `Skill(schedule)` in
`permissions.deny` stops the skill (from Claude Code's docs).

Open: the brief is long for a router (13,600 characters, about 17,000 with a project's
instructions). It is cached, so it costs little per turn, but at low effort a long brief is
skimmed. Cut it to the routing rules and move what most turns don't need (accounts and
moves, limits, routines) to where it is used: tool descriptions, or files read on demand.

## Review, 2026-11-09: what would make chief better

From a read-only pass over the docs and code. Not built; ranked by value.

1. **A finished thread nobody subscribed to is silent.** The coordinator hears of a finish only
   through its own `notify_when_idle` subscription, which dies with its process. The routine
   notice above covers runs; the same `<chief-finished>` kind for any thread whose report is
   newer than the last telling would cover a coordinator that was respawned.
2. **Questions and pull requests are seen only while a board (or now a dashboard) runs.** A
   thread blocked on `AskUserQuestion` after `chief down` tells nobody: the waiting state comes
   from a `claude agents` poll. Treat an unanswered PreToolUse `AskUserQuestion` as waiting, and
   let `chief serve` do the board's periodic sync and PR refresh when it runs.
3. **`chief doctor`.** One command that checks the Claude Code version against the versions §1's
   facts were verified on, that `claude agents --json` still carries `status` and `pid` (every
   `AgentRow` field is optional, so a rename shows everything idle), the job state's shape,
   `/tmp/cc-socks`, the generated hook paths, agp's `bg`, trust, the database version, and the
   tail of the hook error log.
4. **Hook errors are invisible**, and a Stop that loses the database lock loses its report. Show
   an error count on the board and in `chief status`; spool a failed Stop's payload, or re-read
   the transcript at the next sync.
5. **Five renderers of a thread's state** (`board/ui.rs` tags, `context.rs`, `render_status`,
   `tools::thread_view`, `rex_status`, plus the dashboard's `view::session`) already disagree on
   "asleep". One typed, serialisable view that every front end renders.
6. **An older binary runs silently on a newer database** (`db::migrate` returns Ok when the
   version is ahead): a `target/` build can migrate the real home under the installed hooks.
   Refuse or warn.
7. **`chief status` skips agp's moves** (it syncs without `moves::refresh`), so a moved thread
   can look asleep there.
8. **Notifications** are macOS-only (`osascript`), fire on every turn end, and say nothing of a
   question, a limit or a move. Add `notify-send`, de-dupe, and notify a coordinator turn the
   user didn't start.
9. **No live smoke script and no CI.** `scripts/e2e.sh` is cited but was never written; the "not
   verified live" items above and before would be its checklist. Fixtures are frozen at
   2.1.310/316: a capture script per Claude Code version would show drift.
10. **ARCHITECTURE.md is written at code level** (about 2,500 lines for 22k of code) and has
    started to contradict itself (§3 and §7 "Thread account" still say a thread's account never
    changes; §5 has agp move it). Split it into facts, invariants and schema, and move
    per-function behaviour into module docs and tests.
11. **The queue lives in the coordinator's conversation.** `max_parallel` is prompted, not
    enforced, and a compaction or restart forgets what was queued.
12. **One inbox across projects.** `chief status --waiting` / `--json`, and the dashboard's
    project list already shows the counts.
