#!/bin/sh
# Fake `claude` for tests; never starts a real session. Tests symlink it into a temp dir and point
# chief at the link, so "$0"'s folder holds this run's files:
#   calls/<n>          the arguments of call n, each followed by a NUL byte
#   calls/<n>.cwd      the working directory of call n
#   <key>.stdout       canned stdout for a subcommand (<key> = first argument without dashes)
#   <key>.stdout.<k>   canned stdout for the k-th call of that subcommand (1-based), if present
#   <key>.stderr       canned stderr
#   <key>.exit         exit code (default 0)
dir=$(dirname "$0")
mkdir -p "$dir/calls"
n=$(ls "$dir/calls" | grep -cv '\.cwd$')
printf '%s\0' "$@" > "$dir/calls/$n"
pwd > "$dir/calls/$n.cwd"

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
exit "$(cat "$dir/$key.exit" 2>/dev/null || echo 0)"
