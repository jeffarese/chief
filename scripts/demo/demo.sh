#!/bin/bash
# Records docs/demo.gif and docs/demo.mp4 with VHS: the real chief launcher, pane and board in
# tmux, against a fake `claude` and `gh` that play a scripted project. No real Claude session
# starts, nothing touches your own chief home, Claude config or tmux server.
#
#   scripts/demo/demo.sh            record
#   scripts/demo/demo.sh --setup    only build the demo world and print its env file (to poke at)
set -euo pipefail

repo=$(cd "$(dirname "$0")/../.." && pwd)
for tool in vhs tmux jq git python3; do
  command -v "$tool" > /dev/null || { echo "demo: $tool is required" >&2; exit 1; }
done

. ~/.cargo/env 2> /dev/null || true
(cd "$repo" && cargo build --release --quiet)

# Canonical (macOS /tmp is /private/tmp), so chief writes paths under it as ~/….
demo=$(mktemp -d "$(cd /tmp && pwd -P)/chief-demo.XXXX")
cp -R "$repo/scripts/demo/." "$demo/"
# Outside target/, or chief warns that `cargo clean` would switch its hooks off.
cp "$repo/target/release/chief" "$demo/bin/chief"
user=$demo/home
api=$user/work/payments-api
mkdir -p "$api" "$user/.claude"
echo '{}' > "$user/.claude.json"
git -C "$api" init --quiet --initial-branch=main
git -C "$api" -c user.name=demo -c user.email=demo@example.com commit --quiet --allow-empty -m init

# Sourced before the demo's tmux starts, so every pane inherits it.
cat > "$demo/env" <<EOF
export HOME='$user'
export PATH='$demo/bin:$PATH'
export CHIEF_CLAUDE_BIN='$demo/bin/claude'
export CHIEF_GH_BIN='$demo/bin/gh'
export CHIEF_REX_BIN='$demo/bin/no-rex'
export CHIEF_AGP_BIN='$demo/bin/no-agp'
export CHIEF_NO_NOTIFY=1
export DEMO='$demo'
unset TMUX TMUX_PANE REX_SESSION REX_BLOCK REX_SERVER
unset XDG_DATA_HOME XDG_CONFIG_HOME CLAUDE_CONFIG_DIR CHIEF_HOME CHIEF_CLAUDE_DIR CHIEF_CLAUDE_CONFIG
export PS1='%F{8}~/work/payments-api%f %F{5}❯%f '
cd '$api'
EOF

if [ "${1-}" = --setup ]; then
  echo "$demo/env"
  exit 0
fi

cleanup() {
  tmux -L chief-demo kill-server 2> /dev/null || true
  rm -rf "$demo"
}
trap cleanup EXIT

(cd "$demo" && vhs demo.tape)
cp "$demo/demo.gif" "$demo/demo.mp4" "$repo/docs/"
echo "demo: wrote docs/demo.gif and docs/demo.mp4"
