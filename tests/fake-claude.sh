#!/bin/sh
# Fake `claude` for tests; never starts a real session. Tests symlink it into a temp dir and point
# chief at the link, so "$0"'s folder holds this run's files:
#   calls/<n>          the arguments of call n, each followed by a NUL byte
#   calls/<n>.cwd      the working directory of call n
#   calls/<n>.pid      the process id of call n
#   <key>.stdout       canned stdout for a subcommand (<key> = first argument without dashes)
#   <key>.stdout.<k>   canned stdout for the k-th call of that subcommand (1-based), if present
#   <key>.stderr       canned stderr
#   <key>.exit         exit code (default 0)
#   <key>.exit.<k>     exit code for the k-th call of that subcommand, if present
#   <key>.sleep        after the output, sleep this many seconds in place (exec, same pid) instead
#                      of exiting: a `claude attach` that stays attached
# The same script stands in for `gh` (tests link it as `gh`; `pr list` has key `pr`).
dir=$(dirname "$0")
mkdir -p "$dir/calls"
n=$(ls "$dir/calls" | grep -c '^[0-9][0-9]*$')
printf '%s\0' "$@" > "$dir/calls/$n"
pwd > "$dir/calls/$n.cwd"
echo $$ > "$dir/calls/$n.pid"

key=$(printf '%s' "$1" | sed 's/^-*//')
count_file="$dir/$key.count"
k=$(( $(cat "$count_file" 2>/dev/null || echo 0) + 1 ))
echo "$k" > "$count_file"

if [ -f "$dir/$key.stdout.$k" ]; then
  cat "$dir/$key.stdout.$k"
elif [ -f "$dir/$key.stdout" ]; then
  cat "$dir/$key.stdout"
fi
[ -f "$dir/$key.stderr" ] && cat "$dir/$key.stderr" >&2
[ -f "$dir/$key.sleep" ] && exec sleep "$(cat "$dir/$key.sleep")"
if [ -f "$dir/$key.exit.$k" ]; then
  exit "$(cat "$dir/$key.exit.$k")"
fi
exit "$(cat "$dir/$key.exit" 2>/dev/null || echo 0)"
