#!/bin/sh
# Fake `claude` for tests; never starts a real session. Tests symlink it into a temp dir and point
# chief at the link, so "$0"'s folder holds this run's files:
#   calls/<n>          the arguments of call n, each followed by a NUL byte
#   calls/<n>.cwd      the working directory of call n
#   calls/<n>.env      the CLAUDE_CONFIG_DIR of call n (the account), or `unset`
#   <key>.stdout       canned stdout for a subcommand (<key> = first argument without dashes)
#   <key>.stdout.<k>   canned stdout for the k-th call of that subcommand (1-based), if present
#   <key>.stderr       canned stderr
#   <key>.exit         exit code (default 0)
#   <key>.exit.<k>     exit code for the k-th call of that subcommand, if present
# With CLAUDE_CONFIG_DIR set, `$CLAUDE_CONFIG_DIR/fake-<key>.stdout.<k>`, else
# `$CLAUDE_CONFIG_DIR/fake-<key>.stdout`, wins over the stdout files above: each account has its
# own supervisor and sessions.
# The same script stands in for `gh` (tests link it as `gh`; `pr list` has key `pr`).
dir=$(dirname "$0")
mkdir -p "$dir/calls"
n=$(ls "$dir/calls" | grep -cv '\.')
printf '%s\0' "$@" > "$dir/calls/$n"
pwd > "$dir/calls/$n.cwd"
printf '%s' "${CLAUDE_CONFIG_DIR-unset}" > "$dir/calls/$n.env"

key=$(printf '%s' "$1" | sed 's/^-*//')
count_file="$dir/$key.count"
k=$(( $(cat "$count_file" 2>/dev/null || echo 0) + 1 ))
echo "$k" > "$count_file"

account_stdout="${CLAUDE_CONFIG_DIR:+$CLAUDE_CONFIG_DIR/fake-$key.stdout}"
if [ -n "$account_stdout" ] && [ -f "$account_stdout.$k" ]; then
  cat "$account_stdout.$k"
elif [ -n "$account_stdout" ] && [ -f "$account_stdout" ]; then
  cat "$account_stdout"
elif [ -f "$dir/$key.stdout.$k" ]; then
  cat "$dir/$key.stdout.$k"
elif [ -f "$dir/$key.stdout" ]; then
  cat "$dir/$key.stdout"
fi
[ -f "$dir/$key.stderr" ] && cat "$dir/$key.stderr" >&2
if [ -f "$dir/$key.exit.$k" ]; then
  exit "$(cat "$dir/$key.exit.$k")"
fi
exit "$(cat "$dir/$key.exit" 2>/dev/null || echo 0)"
