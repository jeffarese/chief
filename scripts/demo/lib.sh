# Shared by the demo's fakes, scene player and director. The demo kit is copied into a temp dir
# (`DEMO`), so every path here is relative to it, never to the repository.
DEMO=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
WORLD=$DEMO/world
mkdir -p "$WORLD/agents" "$WORLD/pr" "$WORLD/log"

# The short id Claude Code would give session `name`: stable, so re-runs record the same video.
short_of() { printf '%s' "$1" | shasum | cut -c1-8; }

# One `claude agents --json` row: short name cwd state status.
agent_set() {
  local short=$1 name=$2 cwd=$3 state=$4 status=$5
  printf '{"id":"%s","sessionId":"%s-0000-4000-8000-000000000000","name":"%s","cwd":"%s","kind":"background","startedAt":%s,"state":"%s","status":"%s","pid":%s}' \
    "$short" "$short" "$name" "$cwd" "$(date +%s)000" "$state" "$status" "$$" \
    > "$WORLD/agents/$short.json.tmp"
  mv "$WORLD/agents/$short.json.tmp" "$WORLD/agents/$short.json"
}

# Changes a listed session's state and status, keeping the rest of its row.
agent_state() {
  local file=$WORLD/agents/$1.json
  [ -f "$file" ] || return 0
  jq -c --arg s "$2" --arg t "$3" '.state = $s | .status = $t' "$file" > "$file.tmp"
  mv "$file.tmp" "$file"
}

# The short id of the session whose name starts with `prefix`, if one is listed.
short_named() {
  local f
  for f in "$WORLD"/agents/*.json; do
    [ -e "$f" ] || continue
    if jq -e --arg p "$1" '.name | startswith($p)' "$f" > /dev/null; then
      basename "$f" .json
      return
    fi
  done
}

# Opens or approves the pull request of branch `branch` (`gh pr list --head <branch>`). Its head
# is the branch's tip, which chief checks is on the thread's branch.
pr_set() {
  local branch=$1 review=$2 head
  head=$(git -C "$HOME/work/payments-api" rev-parse "$branch")
  printf '[{"url":"https://github.com/acme/payments-api/pull/%s","state":"OPEN","isDraft":false,"reviewDecision":"%s","createdAt":"%s","isCrossRepository":false,"headRefOid":"%s"}]' \
    "$3" "$review" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$head" > "$WORLD/pr/$branch.json"
}
