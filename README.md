# chief

Claude Projects, locally. You talk to one coordinator Claude session; it starts background Claude
Code sessions ("threads") that each own one task on their own git branch, routes your follow-ups
to the right thread, and reports back. A board beside the conversation parks every thread by
status, and you can open any thread's own conversation from it.

```
┌──────────────────────────────── chief-<project> ─────────────────────────────────┐
│ main: the coordinator (or the thread you opened)│ board                          │
│                                                 │ Waiting on you 1               │
│ ❯ add rate limiting to the API, and fix the     │  ! Approve the migration       │
│   flaky checkout test                           │ Working 2                      │
│ ⏺ Started two threads: …                        │  ● Rate limiting               │
│                                                 │  ● Flaky checkout test         │
│                                                 │ Ready for review 1             │
│                                                 │  ◆ Speed up search  PR open    │
└─────────────────────────────────────────────────┴────────────────────────────────┘
```

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
chief init "Payments latency" --goal "Hold p95 under 200 ms" --repo ~/workspace/payments-api
chief up payments-latency
```

`init` creates the project in `~/.local/share/chief/projects/<slug>/` and checks that Claude Code
trusts the project folder and each repo; background sessions refuse untrusted folders. If one isn't
trusted, it offers to open Claude there so you can accept the trust prompt.

`up` starts the coordinator (or revives it) and opens the layout: the conversation on the left, the
board on the right (or tells you how to open it yourself). Then just talk: ask questions, hand over
tasks, send follow-ups. The coordinator
answers quick questions itself, starts a thread per piece of work, sends follow-ups to the thread
that owns the work (waking it first if it's asleep), and posts a short update when a thread
finishes.

`chief down <slug>` closes the layout `up` opened and stops the coordinator; threads keep running
unless you add `--stop-threads`.

## The layout

Two panes side by side: `chief pane --project <slug>` on the left shows the coordinator or the
thread you picked, `chief board --project <slug>` on the right. `chief up` opens them with the
first that applies:

- **Rex**, when it is installed and its server is running: a Rex session `chief-<slug>`.
- **tmux**, when you run `chief up` inside tmux: a new window `chief-<slug>` in your current
  session, the pane on the left and the board on the right.
- **Anything else**: `chief up` prints the two commands. Run them side by side in two terminal
  windows or in splits of your terminal (iTerm, Ghostty, …).

Switching works the same in all three: Enter on the board tells the pane process to show that
thread, whatever terminal it runs in. `chief down` closes only a layout `chief up` opened.

## The board

Groups, in order: **Waiting on you** (a permission prompt or a question), **Working**, **Ready for
review** (open pull request), **Landing** (approved pull request), **Idle** (marked *asleep* when its
process has ended), **Failed**, **Resolved** (hidden until you press `R`).

| Key | Does |
| --- | --- |
| ↑ ↓ / j k, g G | Move; first / last |
| Enter | Show the selected thread in the left pane (answer its prompt, talk to it directly) |
| Esc / c | Back to the coordinator |
| w | Wake an asleep thread |
| r | Resolve or reopen |
| o | Open the pull request |
| x | Clean up a resolved thread (see below) |
| R | Show or hide resolved threads |
| s | Settings |
| ? | Help |
| q | Quit the board (the layout stays) |

In the left pane, Claude's own Ctrl+Z detaches too; chief re-attaches to whatever the board
selected.

## Settings

`s` on the board edits `project.toml`: the goal, the coordinator's model and effort (default Opus,
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

Each account trusts folders separately, so `chief init` and the settings screen ("Account trust")
check every repo in every pool account and offer to open Claude there to accept the prompt. A
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
  Blocked attempts are counted on the board.
- **Trust.** chief never edits Claude Code's trust settings; it only reads them.
- Threads run with the permission mode you choose (default `auto`); permission prompts show as
  Waiting on you, and you answer them by opening the thread.

## Commands

| Command | Does |
| --- | --- |
| `chief init <name> [--goal …] [--repo PATH…]` | Create a project |
| `chief up <slug>` | Start or revive the coordinator and open the layout |
| `chief down <slug> [--stop-threads]` | Close the layout and stop the coordinator |
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
