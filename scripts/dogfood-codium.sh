#!/usr/bin/env bash
# Launch an isolated VSCodium window for forge-composer dogfood.
# Does NOT touch your Cursor session or your normal VSCodium profile.
#
# Usage:
#   bash scripts/dogfood-codium.sh          # visible on $DISPLAY (or xvfb if headless)
#   DOGFOOD_HEADLESS=1 bash scripts/dogfood-codium.sh
#   bash scripts/dogfood-codium.sh --no-daemon   # assume composerd already running
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DOGFOOD="$ROOT/.dogfood"
STATE="$DOGFOOD/state"
USER_DIR="$DOGFOOD/codium-user"
EXT_DIR="$DOGFOOD/codium-ext"
WS="$DOGFOOD/workspace"
EXT="$ROOT/extension"
BIN="${COMPOSERD_BIN:-$ROOT/daemon/target/debug/composerd}"
START_DAEMON=1
for arg in "$@"; do
  case "$arg" in
    --no-daemon) START_DAEMON=0 ;;
  esac
done

mkdir -p "$STATE" "$USER_DIR" "$EXT_DIR" "$WS"
[[ -f "$WS/hello.py" ]] || printf 'print("hello from forge-composer dogfood")\n' > "$WS/hello.py"

if [[ ! -f "$EXT/dist/extension.js" ]]; then
  (cd "$EXT" && npm run build)
fi
if [[ ! -x "$BIN" ]]; then
  (cd "$ROOT/daemon" && cargo build -q -p composerd)
  BIN="$ROOT/daemon/target/debug/composerd"
fi

export FORGE_COMPOSER_STATE_DIR="$STATE"

if [[ "$START_DAEMON" -eq 1 ]]; then
  if [[ -f "$STATE/daemon.json" ]]; then
    port="$(python3 -c "import json;print(json.load(open('$STATE/daemon.json'))['port'])")"
    if curl -sf -o /dev/null -H "Authorization: Bearer $(cat "$STATE/auth.token")" \
         "http://127.0.0.1:${port}/sessions"; then
      echo "composerd already healthy on :$port"
    else
      echo "stale daemon.json — starting fresh composerd"
      rm -f "$STATE/daemon.json"
      START_DAEMON=1
    fi
  fi
  if ! curl -sf -o /dev/null -H "Authorization: Bearer $(cat "$STATE/auth.token" 2>/dev/null || true)" \
       "http://127.0.0.1:$(python3 -c "import json,sys
try: print(json.load(open('$STATE/daemon.json'))['port'])
except Exception: print(0)" 2>/dev/null || echo 0)/sessions" 2>/dev/null; then
    echo "starting composerd (state=$STATE)…"
    "$BIN" serve >"$DOGFOOD/composerd.log" 2>&1 &
    for i in $(seq 1 30); do
      [[ -f "$STATE/daemon.json" ]] && curl -sf -o /dev/null \
        -H "Authorization: Bearer $(cat "$STATE/auth.token")" \
        "http://127.0.0.1:$(python3 -c "import json;print(json.load(open('$STATE/daemon.json'))['port'])")/sessions" \
        && break
      sleep 0.2
    done
  fi
fi

port="$(python3 -c "import json;print(json.load(open('$STATE/daemon.json'))['port'])")"
echo "composerd: http://127.0.0.1:$port  (FORGE_COMPOSER_STATE_DIR=$STATE)"
echo "opening isolated codium (user-data=$USER_DIR)…"

CODUM_ARGS=(
  --user-data-dir="$USER_DIR"
  --extensions-dir="$EXT_DIR"
  --extensionDevelopmentPath="$EXT"
  --disable-workspace-trust
  --new-window
  "$WS"
)

if [[ "${DOGFOOD_HEADLESS:-0}" == "1" ]]; then
  exec xvfb-run -a -s "-screen 0 1600x900x24" codium "${CODUM_ARGS[@]}"
else
  # Separate profile only — still uses your current DISPLAY, not Cursor's process.
  exec codium "${CODUM_ARGS[@]}"
fi
