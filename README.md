# chief

Claude Projects, locally. You talk to one coordinator Claude session; it starts background Claude
Code sessions ("threads") that each own one task on their own git branch, routes your follow-ups
to the right thread, and reports back. A board beside the conversation parks every thread by
status, and you can open any thread's own conversation from it.

![A new project in chief: you ask the coordinator for three things, it starts a thread for each,
the board shows the one waiting on you, you answer it there, a pull request lands in Ready for
review, and the board shows a thread's subagents and what each one runs](docs/demo.gif)

<sub>A scripted run (<a href="docs/demo.mp4">MP4</a>): the launcher, board and pane are chief itself,
the Claude sessions behind them a replay. <code>scripts/demo/demo.sh</code> records it again.</sub>

## Requirements

- macOS or Linux, Rust (stable), and Claude Code **2.1.310 or later** (`claude --version`).
- Any terminal. Rex or tmux are optional: they let `chief up` open the two-pane layout for you
  (see [The layout](#the-layout)).
- `gh`, signed in, for pull request status on the board (optional).

## Install

```sh
cargo install --path .
```

Install it rather than running it from `target/`: the hooks Claude Code calls use chief's absolute
path, and `cargo clean` would switch them off (chief warns about this).

## Quick start

```sh
cd ~/workspace/payments-api
chief
```

The first time, `chief` walks you through a new project in four short steps: a name and a goal,
the repositories its threads work in (the one you ran it from is listed already), the models,
and a review. Background sessions refuse folders Claude Code doesn't trust, so the review lists
any repo that isn't trusted yet and offers to trust it, as accepting Claude's trust prompt would.
Enter creates the project in `~/.local/share/chief/projects/<slug>/`, starts its coordinator and
opens the layout: the conversation on the left, the board on the right. Next time, `chief` lists
your projects (with what needs you) and Enter opens one.

The same from a script: `chief init "Payments latency" --goal "…" --repo ~/workspace/payments-api`,
then `chief up payments-latency`.

Then just talk: ask questions, hand over
tasks, send follow-ups. The coordinator
answers quick questions itself, starts a thread per piece of work, sends follow-ups to the thread
that owns the work (waking it first if it's asleep), and posts a short update when a thread
finishes. A thread that ends its turn while its subagents or background commands still run isn't
finished: the board keeps it in Working and shows what runs (`1 subagent`), and the coordinator
hears from it once they finish.

`chief down <slug>` closes the layout `up` opened and stops the coordinator; threads keep running
unless you add `--stop-threads`.

A finished project can be put away: `chief archive <slug>` (or `a` in `chief`'s list) stops its
coordinator and threads and hides it from the list and `chief status`. Its conversations,
worktrees, branches and files stay; `chief unarchive <slug>`, or opening it again, brings it
back.

## The layout

Two panes side by side: `chief pane --project <slug>` on the left shows the coordinator or the
thread you picked, `chief board --project <slug>` on the right. `chief up` opens them with the
first that applies:

- **Inside a Rex block**: right there. The board opens beside the block you ran `chief` in, and
  that block shows the conversation; `q` closes the board and gives you your shell back.
- **Rex**, when it is installed and its server is running: a Rex session `chief-<slug>`.
- **tmux**, when you run `chief up` inside tmux: a new window `chief-<slug>` in your current
  session, the pane on the left and the board on the right.
- **Anything else**: chief draws both itself, in the terminal you ran it in: the conversation on
  the left, the board on the right. **Ctrl+\\** switches sides (or click one); the side without
  the keyboard is dimmed, and below 100 columns the focused side fills the screen. Showing
  something from the board (Enter, `n`, Esc) gives the conversation the keyboard, so you can
  answer at once. The conversation's last row is chief's: what it shows, `! 2 waiting` while
  threads wait on you (click it for the next one), and `^Q quit` and `^\ board`, which you can
  click. Ctrl+Q, or `q` on the board, closes it; sessions keep running. `chief up <slug> --layout
  builtin` opens it even where Rex or tmux would. Without a terminal, chief prints the two
  commands instead, to run side by side in two windows or splits of your terminal (iTerm,
  Ghostty, …).

Switching works the same in all of them: Enter on the board tells the pane process to show that
thread, whatever terminal it runs in. `chief down` closes only a layout `chief up` opened, and
ends the pane wherever it runs.

## The board

The coordinator is the first row, then the threads by group, in order: **Waiting on you** (a
permission prompt or a question), **Working**, **Ready for review** (open pull request), **Landing**
(approved pull request), **Idle** (marked *asleep* when its process has ended), **Failed**,
**Resolved** (hidden until you press `R`). `▸` marks what the left pane shows. The panel below
the list tells about the selection: for a working or waiting thread, what it runs or asks right
now (`wants to run npm publish`, `asks Which limit should each key get?`) and its calls before,
then its task and its last report, which `v` shows in full. The bottom line always lists the
keys that work on the selection, and `?` explains every mark. The mouse works too: click to
select, click again to show, scroll to move.

| Key | Does |
| --- | --- |
| ↑ ↓ / j k, g G | Move; first (the coordinator) / last |
| Enter | Show the selection in the left pane (answer a thread's prompt, talk to it directly) |
| Esc / c | Back to the coordinator |
| n | Show the next thread waiting on you (again for the one after) |
| w | Wake an asleep thread |
| S | Stop a running thread (asks first; `w` wakes it again) |
| r | Resolve or reopen |
| o | Open the pull request |
| v | Read the thread's reports in full: ↑ ↓ scroll, ← → older and newer, Esc back |
| x | Clean up a resolved thread (see below) |
| X | Clean up every resolved thread that loses nothing (`chief gc` for this project) |
| R | Show or hide resolved threads |
| s | Settings |
| ? | Help |
| q | Quit the board (the layout stays) |
| Ctrl+Q | Quit chief: the conversation and the board |

**Ctrl+Q** in the conversation or on the board quits chief at once, whatever Claude is doing, and
closes the layout (in a Rex block, the board beside it). The board's key bar shows it. Leaving Claude in the left pane by hand (Ctrl+Z, or
Ctrl+C out of its agent view) brings you to chief's own screen instead: ⏎ goes back, a pick on the
board shows that instead, and `q` or Ctrl+Q closes the layout. The coordinator and threads keep
running in the background either way; `chief` or `chief up <slug>` opens the layout again.

## Settings

`s` on the board edits `project.toml`, in sections, with a line on what each field does (Ctrl+S
saves): the goal, the coordinator's model and effort (default Opus,
low: it routes, it doesn't do the work), the threads' model, effort and permission mode (default
Sonnet, high, `auto`), how many threads to run at once, the default repo, the repos, cleanup, and
notifications. It also opens `instructions.md` (a standing brief for the coordinator and every new
thread) and `memory/MEMORY.md` (the project memory index) in `$EDITOR`. New threads pick up changes
at once; the coordinator's model and effort apply on its next start.

## Accounts

A project belongs to the Claude Code account you ran `chief init` from (its `CLAUDE_CONFIG_DIR`,
recorded as `claude_config_dir` in `project.toml`). The coordinator always runs there, and every
chief command for the project uses that account, whatever terminal you type it in.

With agp (agent-prism) installed, threads can spread over your other accounts: each new thread
starts in the account of agp's pool with the most room (`agp status`), skipping limited ones.
`[threads] accounts` in `project.toml` narrows the pool (`["auto"]`, the default, is the whole
pool; `[]` keeps threads on the project's account) and `max_usage` skips accounts at or over a
5-hour or weekly percentage. Ask the coordinator for a specific account and it pins the thread
there. A thread stays in the account it started in; the board tags it with the account's label.

Each account trusts folders separately, so `chief init`, the new-project wizard and the settings
screen ("Account trust") check every repo in every pool account and offer to trust it there. A
thread never starts in an account that doesn't trust its repo. Without agp, threads use the
project's account.

## Cleanup

Each thread works in its own worktree, `<repo>/.claude/worktrees/…`, on its own branch. chief
removes them through `claude rm`, and never loses work without asking:

- With `cleanup = "merged"` (the default), a thread whose pull request merged is cleaned up
  automatically once it's idle.
- `x` on the board, or `chief clean <slug> <thread>`, cleans up one resolved thread. If that would
  lose uncommitted changes or commits that exist on no remote, chief stops, lists exactly what
  would be lost, and only goes ahead after you type the thread's name (or pass `--discard`).
- `chief gc [<slug>]` cleans every resolved thread that loses nothing and lists the rest.

The coordinator has no cleanup tool: deleting work is your call. A cleaned thread's reports stay
readable.

## Safety

- **Message guard.** Claude Code lets a session message any other session on your machine. chief
  blocks that: the coordinator may message only its own project's live threads, and the only other
  session a thread may message is its coordinator. `ListAgents` is blocked for both. Each target is
  checked against the project's registered threads and a fresh `claude agents` listing matched by
  session id; anything chief can't verify is blocked, including when chief itself can't run.
  Blocked attempts are counted on the board. Each session may also use its own subagents: it
  messages them by the name or agent id the Agent tool gave them, they report back to it, and a
  subagent can't take the name of another session (it would get that session's messages).
- **Subagents run commands in the foreground.** Claude Code doesn't wake a subagent when its
  background command ends, so its work would stop half done. chief denies a subagent's background
  commands and tells it to run them in the foreground (up to 10 minutes); longer ones are left to
  the thread itself.
- **Trust.** chief marks a folder trusted in an account's `.claude.json` only after you say yes
  (the wizard's checkbox, `init`'s question, `chief trust <slug>`, Enter in settings), writing
  exactly what accepting Claude's trust prompt writes and keeping the rest of the file. The one
  folder it trusts without asking is the project folder it created itself.
- Threads run with the permission mode you choose (default `auto`); permission prompts show as
  Waiting on you, and you answer them by opening the thread.

## Commands

| Command | Does |
| --- | --- |
| `chief` | Pick a project, or create one in a guided setup, and open it |
| `chief new` | Straight to the guided setup |
| `chief init <name> [--goal …] [--repo PATH…]` | Create a project from a script |
| `chief trust <slug>` | Trust every folder the project's sessions need |
| `chief up <slug>` | Start or revive the coordinator and open the layout |
| `chief down <slug> [--stop-threads]` | Close the layout and stop the coordinator |
| `chief archive <slug>` | Stop the coordinator and every thread and put the project away; nothing is deleted |
| `chief unarchive <slug>` | Bring an archived project back (`chief up` does too) |
| `chief pane --project <slug>` | The left pane on its own, for a layout of your own |
| `chief board --project <slug>` | The board on its own |
| `chief status [<slug>]` | The board as text |
| `chief clean <slug> <thread> [--discard] [--yes]` | Clean up one resolved thread |
| `chief gc [<slug>] [--dry-run]` | Clean up every resolved thread that loses nothing |

`chief hook` and `chief mcp` are called by Claude Code's hooks and the coordinator's MCP config.

## How it works

No daemon: Claude Code's background supervisor owns every session, and a SQLite database
(`~/.local/share/chief/chief.db`) is the shared state.

- The coordinator is a background Claude Code session with chief's MCP tools (`start_thread`,
  `list_threads`, `read_report`, `wake_thread`, `resolve_thread`) and no shell. It reaches threads
  with Claude Code's built-in `SendMessage` and hears back through idle notices.
- Threads are ordinary background sessions (`claude --bg`), revived with `claude respawn` when their
  process has ended.
- Claude Code hooks call `chief hook` on every session start, prompt, tool call, notification and
  stop. They record state and reports, and on each coordinator turn add a compact board snapshot
  and the memory index to its context.
- The board polls `claude agents` every 2 s and pull request status every 30 s.

`docs/ARCHITECTURE.md` is the full contract, including what was verified against Claude Code and
how; `docs/PLAN.md` has the build plan and the live test results.

## Development

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

Tests never start real Claude sessions: they use fake `claude`, `gh`, `rex` and `tmux` binaries
(`CHIEF_CLAUDE_BIN`, `CHIEF_GH_BIN`, `CHIEF_REX_BIN`, `CHIEF_TMUX_BIN`) and a temporary
`CHIEF_HOME`. The tmux backend is also tested against a real tmux server on a private socket,
which the test kills; your own tmux server is never touched.

`scripts/demo/demo.sh` records the demo above (`docs/demo.gif`, `docs/demo.mp4`) with
[VHS](https://github.com/charmbracelet/vhs): the release build in its own tmux server, against a
fake `claude` and `gh` (`scripts/demo/bin/`) under a temporary `HOME`. It needs `vhs`, `tmux`, `jq`
and `python3`, and starts no real Claude session.
