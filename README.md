# chief

Claude Projects, locally. You talk to one coordinator Claude session; it starts background Claude
Code sessions ("threads") that each own one task on their own git branch, routes your follow-ups
to the right thread, and reports back. A board beside the conversation parks every thread by
status, and you can open any thread's own conversation from it. A project can also follow
things over time on a schedule ([routines](#routines)) and be used from a browser
([the dashboard](#the-dashboard)), and it needn't be about code: a project for the household's
money and plans works the same way.

![A new project in chief: you ask the coordinator for three things, it starts a thread for each,
the board shows the one waiting on you, you answer it there, a pull request lands in Ready for
review, and the board shows a thread's subagents and what each one runs](docs/demo.gif)

<sub>A scripted run (<a href="docs/demo.mp4">MP4</a>): the launcher, board and pane are chief itself,
the Claude sessions behind them a replay. <code>scripts/demo/demo.sh</code> records it again.</sub>

## Requirements

- macOS or Linux, Rust (stable), and Claude Code **2.1.316 or later** (`claude --version`).
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

Then just talk: ask questions, hand over tasks, send follow-ups. The coordinator answers quick
questions itself and hands anything that takes finding out to a lookup thread, so it stays free
to talk: chief keeps its own looking to the project's folders and to a handful of searches a
turn. It starts a thread per piece of work, sends follow-ups to the thread that owns the
work (waking it first if it's asleep), and posts a short update when a thread finishes. A thread that ends its turn while its subagents or background commands still run isn't
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

A project with [routines](#routines) lists them under the threads, each with how it stands:
`✓` passed, `✗` failing, `!` needs you (a command to approve, or a run that said so), `◆` has
news, `●` running. The panel below shows a selected routine's schedule and next run, its
command, and what its last run printed.

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
| a | Approve a routine's command (shows it whole and asks first) |
| e | Run a routine now |
| p | Pause or resume a routine |
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

## Routines

A routine is something the project does or watches on a schedule, run by chief with nobody
watching. It is how a project follows things over time: a fare, a mortgage rate, a nightly
build, an inbox of issues, whether staging is up. A routine is one of three things:

- **A prompt**: each run is a fresh Claude session. For work that needs judgement.
- **A command**: a shell line chief runs itself, like a cron job. No session and no tokens, as
  often as every minute. For whatever a script can decide.
- **Both**: the command is a cheap check and the prompt is what to do about it. A session
  starts only when the check fails or says it has news, with the check's output in its brief.

Ask the coordinator in plain words ("every morning, check flights to Tokyo for 10-24 April and
tell me if one drops under 700", "check every minute that staging is up, and look into it when
it isn't") and it creates one; it can list, read, change, pause, run and delete them too. Or
from a shell:

```sh
chief routine add home --title "Flights to Tokyo" --schedule "0 8 * * *" --model haiku \
  --prompt "Find the lowest return fare BCN-TYO for 10-24 April. Record it. Attention only under 700 EUR."
chief routine add api --title "Staging health" --schedule "* * * * *" \
  --command "curl -fsS https://staging.example.com/health" \
  --prompt "Staging's health check fails. Find out why from the deploy log and say what to do."
chief routine list home
chief routine run home flights-to-tokyo    # now, outside its schedule
chief routine show api staging-health      # how it stands, its last runs and output
```

- **Schedules** are in your local time: five cron fields (`0 9 * * 1-5`), `@hourly`, `@daily`,
  `@weekly`, `@monthly`, or `every 30m` / `every 2h`. A prompt alone runs at most every 5
  minutes, since every run is a whole session; a routine with a command may run every minute.
- **A command** runs with `sh -c` in the routine's repository, for at most 5 minutes unless
  you give it `--timeout`. Exit status 0 means all is well; anything else is a failure. It
  repeats itself, so you hear only what changes: the first failure, not the same one every
  minute, and the first pass after it. A command that knows more can say it: `chief routine
  value "<series>" <number>` records a number it tracks, and `chief routine outcome
  update|attention "<headline>"` says what the run came to. `$CHIEF_ROUTINE_DIR` is a folder
  of its own for what it keeps between runs, and what its last run printed is in
  `last-run.log` there.
- **Approval.** The coordinator can't run commands, so a command it writes or changes doesn't
  run until you say yes: `a` on the board, Approve in the dashboard, or `chief routine approve
  <slug> <name>`. A command you write yourself is approved as you write it. `chief routine`
  run from inside a Claude session doesn't count as you: there `add --command` waits for
  approval too, and `approve` is refused.
- **Memory.** Runs are separate sessions, so the hundredth run costs what the first did. What
  carries over is the routine's notes file (`routines/<name>/NOTES.md` in the project folder):
  every run reads it first and rewrites it before it ends. You can edit it, and so can the
  coordinator ("our mortgage is at 2.9% now").
- **Values.** A run records the numbers it tracks (a price, a rate, a balance), which join into
  a line over time on the dashboard.
- **An outcome.** Every run ends by saying what it came to, with a one-line headline: `nothing`
  (nothing new), `update` (news that can wait) or `attention` (you should look now). chief asks
  a run that forgets.
- **Who hears.** That outcome decides it, so a routine costs you and the coordinator nothing
  when nothing happened. `nothing` tells nobody. `update` is told by the coordinator when you
  next talk to it. `attention` wakes the coordinator, sends a desktop notification, and is
  pushed to your phone if you set a push command (below).
- **A run** is otherwise a thread like any other, tagged `routine <name>` on the board. A run is
  skipped while the routine's last one still works, and the next run puts the last finished one
  away (resolved), so runs don't pile up.
- **Routines fire while `chief serve` runs**, the one background process chief has. `chief up`
  starts it for a project with routines; `chief serve install` keeps it running across logins
  (macOS), so an 8am routine runs with no terminal open. A routine that came due while it was
  down runs once when it is back.
- **Everything runs on your machine.** Sessions are local Claude Code sessions and commands
  are local processes. chief's sessions can't use Claude Code's own cloud scheduling
  (`/schedule`, remote triggers): it is denied in their settings.

**Push.** To hear about what needs you away from the machine, give the project a command in
`project.toml`. chief runs it with the message in `CHIEF_TITLE`, `CHIEF_BODY`, `CHIEF_KIND`
(`attention`, `update` or `permission`) and `CHIEF_URL` (the dashboard page, when it is on):

```toml
[push]
command = 'curl -s -H "Title: $CHIEF_TITLE" -H "Click: $CHIEF_URL" -d "$CHIEF_BODY" ntfy.sh/your-secret-topic'
# Telegram: 'curl -s "https://api.telegram.org/bot$TOKEN/sendMessage" -d chat_id=$CHAT --data-urlencode "text=$CHIEF_TITLE: $CHIEF_BODY"'
on = "attention"   # or "updates" to also get news that can wait
```

It fires when a run says `attention` and when a thread is stuck on a permission prompt.

**Not only code.** A project with no repositories works in its own `workspace/` folder, so a
project like `home` needs no setup: `chief init Home --goal "…"`, then talk to it.

## The dashboard

```sh
chief dashboard payments-latency
```

turns the project's web dashboard on, starts `chief serve` and opens a link that signs your
browser in.

![The dashboard's overview of a home project: one routine needs you because a fare dropped, and
each routine's tile shows what its last run found, its latest value and its trend](docs/dashboard-overview.png)

![The dashboard for a code project: the board on the left with a thread waiting on you, and a
thread's report rendered on the right](docs/dashboard.png)

<sub>Seeded demo data, not real projects.</sub>

- **Overview**: what needs you, then a tile per routine with what its last run found, the
  latest value of what it tracks and its trend. A routine's own page charts every value it
  tracks, lists what each run found, and shows its notes.
- **The board, live**: the same groups, tags and colours as the terminal board, what each
  working thread does and what a waiting one asks, and a red strip that takes you to the next
  thread that needs you.
- **The conversation** is the session itself: `claude attach` in a terminal in the page, for the
  coordinator and any thread. Permission prompts, questions and slash commands work as they do in
  your terminal. Opening a sleeping session wakes it; leaving the tab detaches and the session
  keeps running.
- **Reports** as formatted text, a thread's **activity** (its latest calls and subagents), and
  the actions: wake, stop, resolve, reopen, clean up (only when nothing would be lost), open the
  pull request.
- **Routines**: add and edit them with a schedule builder (a session, a command, or both), run
  one now, pause it, approve a command the coordinator wrote, see its runs and what a command
  printed.
- It works on a phone-sized screen, and follows your light or dark setting.

It listens on `127.0.0.1:4747` and every request needs the secret token in that link, so keep the
link to yourself: whoever has it can run the project's sessions. To reach it from another machine,
set `bind` (and `port`) in `~/.local/share/chief/serve.toml`, for example to your tailnet address;
the traffic is plain HTTP, so only on a network you trust or behind a TLS proxy. `chief dashboard
<slug> --off` turns a project's dashboard off; `chief serve stop` ends the server.

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
there. The board tags each thread with its account's label.

agp also starts each thread and watches it (`[threads] runner = "agp"`, the default; needs an agp
with `agp bg`). When a thread's account hits a usage limit, agp moves it to another pool account
with room, with its conversation and worktree, and it goes on there (`on_limit = "switch"`; `wait`
resumes it on the same account at the reset, `stay` leaves it). The board shows a limited thread
under Limited and a moved one with where it came from, and the coordinator hears of both. Ask the
coordinator to move a thread and it does. The coordinator itself is woken after its own limit
resets, never moved.

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
- **The dashboard** is off until you turn it on for a project, listens on localhost unless you
  say otherwise, and needs its token on every request; a change must also come from its own
  page, so another website can't steer it through your browser.
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
| `chief routine list\|add\|show\|run\|approve\|pause\|resume\|rm <slug> …` | The project's routines |
| `chief dashboard <slug> [--off] [--no-open]` | Turn the web dashboard on and print its link |
| `chief serve [start\|stop\|status\|install\|uninstall]` | The background server: routines and the dashboard |

`chief hook` and `chief mcp` are called by Claude Code's hooks and the coordinator's MCP config.

## How it works

Claude Code's background supervisor owns every session, and a SQLite database
(`~/.local/share/chief/chief.db`) is the shared state. Nothing of chief's has to stay running,
except `chief serve` when you use routines or the dashboard.

- The coordinator is a background Claude Code session with chief's MCP tools (`start_thread`,
  `list_threads`, `read_report`, `wake_thread`, `stop_thread`, `move_thread`, `resolve_thread`,
  and the routine tools) and no shell. A routine's run gets two tools of its own, `record_value`
  and `report_outcome`. It reaches threads
  with Claude Code's built-in `SendMessage` and hears back through idle notices.
- Threads are ordinary background sessions (`claude --bg`), revived with `claude respawn` when their
  process has ended.
- Claude Code hooks call `chief hook` on every session start, prompt, tool call, notification and
  stop. They record state and reports, and on each coordinator turn add a compact board snapshot
  and the memory index to its context.
- The board polls `claude agents` every 2 s and pull request status every 30 s; so does the
  dashboard, while someone has it open.
- `chief serve` looks for due routines every 15 s and starts each as a thread. The dashboard is
  served from the same process: an HTTP API over the same functions the board and the
  coordinator's tools use, the board as server-sent events, and `claude attach` over a WebSocket.
  The page is plain JavaScript and CSS compiled into the binary.

`docs/ARCHITECTURE.md` is the full contract, including what was verified against Claude Code and
how; `docs/PLAN.md` has the build plan and the live test results.

## Development

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

Tests never start real Claude sessions: they use fake `claude`, `gh`, `rex`, `tmux` and `agp`
binaries (`CHIEF_CLAUDE_BIN`, `CHIEF_GH_BIN`, `CHIEF_REX_BIN`, `CHIEF_TMUX_BIN`, `CHIEF_AGP_BIN`)
and a temporary `CHIEF_HOME`. `chief serve` is tested as the real binary on a free port. The tmux backend is also tested against a real tmux server on a private socket,
which the test kills; your own tmux server is never touched.

`scripts/demo/demo.sh` records the demo above (`docs/demo.gif`, `docs/demo.mp4`) with
[VHS](https://github.com/charmbracelet/vhs): the release build in its own tmux server, against a
fake `claude` and `gh` (`scripts/demo/bin/`) under a temporary `HOME`. It needs `vhs`, `tmux`, `jq`
and `python3`, and starts no real Claude session.
