# chief

Local Claude Projects: a coordinator Claude session, background Claude Code threads, and a ratatui
board. Read `docs/ARCHITECTURE.md` before changing anything; it is the contract. `docs/PLAN.md` has
the milestones.

## Rules

- `docs/ARCHITECTURE.md` wins over code. If the code must differ, update the doc in the same commit.
- Hook commands (`chief hook …`) must never block or fail a Claude session: exit 0, keep stdout empty
  except the guard decision, the coordinator's context (each turn's board and memory, and at
  SessionStart its brief when that went stale, §8), the denial of a coordinator's look outside
  the project or past its turn's count (§6), the two background-work outputs
  (ARCHITECTURE §5): the denial of a subagent's background command and the Stop decision that asks
  a thread for its final report, and the Stop decision that asks a routine's run for its outcome
  (§13). Log errors to `<root>/logs/hook-errors.log`. The one exception is
  the coordinator's background watcher, `chief hook watch` (ARCHITECTURE §5 "Telling the
  coordinator"): it exits 2 with its notice on stderr to wake the coordinator.
- Never call `claude --bg --resume <id>` with extra arguments; revive with `claude respawn <id>`.
- Never start real Claude sessions from unit or integration tests. Use `CHIEF_CLAUDE_BIN` with a fake
  script and `CHIEF_HOME` with a temp dir. Only `scripts/e2e.sh` starts real sessions.
- Use `ratatui::crossterm`, not a separate crossterm dependency.
- Match the surrounding code: small modules, plain functions, `anyhow::Result` at the edges,
  `thiserror` only where callers match on errors. Comments explain why, not what.

## Commands

Before `cargo`, run `. ~/.cargo/env` if `cargo` isn't on PATH.

- `cargo build`
- `cargo test` (run targeted tests first: `cargo test state::`)
- `cargo clippy --all-targets -- -D warnings`
- `cargo fmt`

A milestone is done when build, tests, clippy and fmt are clean.
