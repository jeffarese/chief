#!/bin/sh
# Fake `claude` for tests; never starts a real session. Tests symlink it into a temp dir and point
# chief at the link, so "$0"'s folder holds this run's files:
#   calls/<n>          the arguments of call n, each followed by a NUL byte
#   calls/<n>.cwd      the working directory of call n
#   calls/<n>.pid      the process id of call n
#   calls/<n>.env      the CLAUDE_CONFIG_DIR of call n (the account), or `unset`
#   <key>.stdout       canned stdout for a subcommand (<key> = first argument without dashes)
#   <key>.stdout.<k>   canned stdout for the k-th call of that subcommand (1-based), if present
#   <key>.stderr       canned stderr
#   <key>.exit         exit code (default 0)
#   <key>.exit.<k>     exit code for the k-th call of that subcommand, if present
#   <key>.<arg>.stdout canned stdout when the second argument is <arg>, and <key>.<arg>.exit its
#                      exit code: `agp exec <id> …` per account (`exec.cpb.stdout`)
#   <key>.sleep        after the output, sleep this many seconds in place (exec, same pid) instead
#                      of exiting: a `claude attach` that stays attached
# With CLAUDE_CONFIG_DIR set, `$CLAUDE_CONFIG_DIR/fake-<key>.stdout.<k>`, else
# `$CLAUDE_CONFIG_DIR/fake-<key>.stdout`, wins over the stdout files above: each account has its
# own supervisor and sessions.
# The same script stands in for `gh` (tests link it as `gh`; `pr list` has key `pr`), `tmux`
# (a leading `-S <socket>`, as chief passes it, is skipped: `-S s new-window …` has key
# `new-window`) and `agp` (`status --json` has key `status`).
dir=$(dirname "$0")
mkdir -p "$dir/calls"
n=$(ls "$dir/calls" | grep -c '^[0-9][0-9]*$')
printf '%s\0' "$@" > "$dir/calls/$n"
pwd > "$dir/calls/$n.cwd"
echo $$ > "$dir/calls/$n.pid"
printf '%s' "${CLAUDE_CONFIG_DIR-unset}" > "$dir/calls/$n.env"

first=$1
[ "$1" = "-S" ] && first=$3
key=$(printf '%s' "$first" | sed 's/^-*//')
count_file="$dir/$key.count"
k=$(( $(cat "$count_file" 2>/dev/null || echo 0) + 1 ))
echo "$k" > "$count_file"

account_stdout="${CLAUDE_CONFIG_DIR:+$CLAUDE_CONFIG_DIR/fake-$key.stdout}"
second=$(printf '%s' "${2-}" | tr -c 'A-Za-z0-9_-' '_')
if [ -n "$account_stdout" ] && [ -f "$account_stdout.$k" ]; then
  cat "$account_stdout.$k"
elif [ -n "$account_stdout" ] && [ -f "$account_stdout" ]; then
  cat "$account_stdout"
elif [ -n "$second" ] && [ -f "$dir/$key.$second.stdout" ]; then
  cat "$dir/$key.$second.stdout"
elif [ -f "$dir/$key.stdout.$k" ]; then
  cat "$dir/$key.stdout.$k"
elif [ -f "$dir/$key.stdout" ]; then
  cat "$dir/$key.stdout"
fi
[ -f "$dir/$key.stderr" ] && cat "$dir/$key.stderr" >&2
[ -f "$dir/$key.sleep" ] && exec sleep "$(cat "$dir/$key.sleep")"
if [ -f "$dir/$key.exit.$k" ]; then
  exit "$(cat "$dir/$key.exit.$k")"
fi
if [ -n "$second" ] && [ -f "$dir/$key.$second.exit" ]; then
  exit "$(cat "$dir/$key.$second.exit")"
fi
exit "$(cat "$dir/$key.exit" 2>/dev/null || echo 0)"
