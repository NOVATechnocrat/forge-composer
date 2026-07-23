#!/usr/bin/env bash
# M5 "Ship" hermetic oracle — proves the ship wave with NO live model
# (stub-llm-m5.py answers plain text and logs every request) and NO network.
# Prints M5-SHIP-OK on success; any failure exits nonzero.
#
# Proves:
#   B. model selector — GET /roles lists configured roles (401 without
#      bearer); a session created with role:"architect" sends its turns to
#      that role's model; POST /sessions/{id}/role (unknown role -> 400)
#      ledgers a role_switch event (actor:human, from/to) and the NEXT turn
#      provably uses the new role's model (stub request log + usage events).
#   C. visibility + gauge data — usage events carry the answering model;
#      /sessions/detail rows carry model, context_window (from the pricing
#      table), and last_prompt_tokens > 0.
#   D. workspace rules — <workspace>/AGENTS.md content reaches the model
#      inside the SYSTEM message under the exact label
#      'Workspace rules (AGENTS.md):', and a `rules` event (actor:system,
#      bytes > 0) is ledgered.
#   E. bootstrap — `composerd init --dir <tmp> --provider ollama` writes a
#      starter config ([roles.orchestrator], auto_approve_edits = true) only
#      when absent (second run exits nonzero, file byte-identical);
#      `composerd --version` prints the version.
set -euo pipefail

REPO="$(cd "$(dirname "$0")/../.." && pwd)"
ORACLE_DIR="$REPO/tests/oracle"

STATE="$(mktemp -d /tmp/fc-m5-oracle.XXXXXX)"
WS="$STATE/workspace"
STUB_LOG="$STATE/stub-requests.jsonl"
PIDS=()
cleanup() {
  for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null || true; done
  rm -rf "$STATE"
}
trap cleanup EXIT

fail() { echo "ASSERT FAIL: $*" >&2; exit 1; }

free_port() {
  python3 - <<'PY'
import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()
PY
}

echo "== build =="
( cd "$REPO/daemon" && cargo build -q ) || fail "cargo build"
BIN="$REPO/daemon/target/debug/composerd"
[ -x "$BIN" ] || fail "missing $BIN"

echo "== A. fixtures: workspace with AGENTS.md rules =="
mkdir -p "$WS"
printf 'House style: M5-RULES-TOKEN-whiskey. Keep functions small.\n' > "$WS/AGENTS.md"
printf 'hello\n' > "$WS/base.txt"

echo "== start scripted stub provider =="
STUB_PORT="$(free_port)"
python3 "$ORACLE_DIR/stub-llm-m5.py" "$STUB_PORT" "$STUB_LOG" &
PIDS+=($!)

DAEMON_PORT="$(free_port)"
cat > "$STATE/config.toml" <<EOF
[server]
port = $DAEMON_PORT

[providers.stub]
base_url = "http://127.0.0.1:$STUB_PORT/v1"

[roles.orchestrator]
provider = "stub"
model = "stub-m5"

[roles.architect]
provider = "stub"
model = "stub-architect"

[pricing."stub-m5"]
input_per_mtok = 1.0
output_per_mtok = 2.0
context_window = 1000
EOF

echo "== start daemon =="
FORGE_COMPOSER_STATE_DIR="$STATE" "$BIN" serve &
PIDS+=($!)
for _ in $(seq 1 100); do
  [ -f "$STATE/daemon.json" ] && curl -fsS "http://127.0.0.1:$DAEMON_PORT/health" >/dev/null 2>&1 && break
  sleep 0.1
done
curl -fsS "http://127.0.0.1:$DAEMON_PORT/health" >/dev/null 2>&1 || fail "daemon not healthy in 10s"

TOKEN="$(cat "$STATE/auth.token")"
BASE="http://127.0.0.1:$DAEMON_PORT"
AUTH=(-H "Authorization: Bearer $TOKEN")

events() { curl -fsS "${AUTH[@]}" "$BASE/sessions/$1/events?since=0"; }
await() { # sid needle label
  for _ in $(seq 1 150); do
    if events "$1" | grep -q "$2"; then return 0; fi
    sleep 0.1
  done
  fail "$3: '$2' not observed within 15s"
}
last_stub_model() {
  python3 - "$STUB_LOG" <<'PY'
import sys, json
last = None
for line in open(sys.argv[1]):
    last = json.loads(line)["body"].get("model")
print(last or "")
PY
}

echo "== B. selector: roles list, role-pinned session, ledgered mid-session switch =="
CODE="$(curl -s -o /dev/null -w '%{http_code}' "$BASE/roles")"
[ "$CODE" = "401" ] || fail "/roles without token: expected 401, got $CODE"
curl -fsS "${AUTH[@]}" "$BASE/roles" | python3 -c '
import sys, json
roles = {r["name"]: r for r in json.load(sys.stdin)["roles"]}
assert "orchestrator" in roles and "architect" in roles, f"roles missing: {list(roles)}"
arch, orch = roles["architect"], roles["orchestrator"]
assert arch["model"] == "stub-architect", f"architect row: {arch}"
assert orch["model"] == "stub-m5", f"orchestrator row: {orch}"
'
SID="$(curl -fsS "${AUTH[@]}" -X POST "$BASE/sessions" -H 'Content-Type: application/json' \
  -d "{\"workspace\":\"$WS\",\"role\":\"architect\"}" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')"
curl -fsS "${AUTH[@]}" -X POST "$BASE/sessions/$SID/message" -H 'Content-Type: application/json' \
  -d '{"text":"please M5-CHECK"}' >/dev/null
await "$SID" "M5-CHECK-ACK" "architect first turn"
[ "$(last_stub_model)" = "stub-architect" ] || fail "first turn hit '$(last_stub_model)', expected stub-architect"
CODE="$(curl -s -o /dev/null -w '%{http_code}' "${AUTH[@]}" -X POST "$BASE/sessions/$SID/role" \
  -H 'Content-Type: application/json' -d '{"role":"ghost-role"}')"
[ "$CODE" = "400" ] || fail "unknown role: expected 400, got $CODE"
curl -fsS "${AUTH[@]}" -X POST "$BASE/sessions/$SID/role" -H 'Content-Type: application/json' \
  -d '{"role":"orchestrator"}' >/dev/null
await "$SID" '"kind":"role_switch"' "role_switch event"
curl -fsS "${AUTH[@]}" -X POST "$BASE/sessions/$SID/message" -H 'Content-Type: application/json' \
  -d '{"text":"please M5-CHECK once more"}' >/dev/null
for _ in $(seq 1 150); do
  [ "$(last_stub_model)" = "stub-m5" ] && break
  sleep 0.1
done
[ "$(last_stub_model)" = "stub-m5" ] || fail "post-switch turn hit '$(last_stub_model)', expected stub-m5"
events "$SID" > "$STATE/b-events.json"
python3 - "$STATE/b-events.json" <<'PY' || exit 1
import sys, json
evs = json.load(open(sys.argv[1]))["events"]
sw = [e for e in evs if e["kind"] == "role_switch"]
assert sw, "no role_switch event"
s = sw[-1]
assert s["actor"] == "human", f"role_switch actor: {s['actor']} (switching models is a HUMAN act)"
assert s["body"].get("from") == "architect" and s["body"].get("to") == "orchestrator", \
    f"role_switch body: {s['body']}"
usage = [e for e in evs if e["kind"] == "usage"]
assert usage, "no usage events"
models = [e["body"].get("model") for e in usage]
assert models[0] == "stub-architect", f"first usage model: {models}"
assert models[-1] == "stub-m5", f"last usage model: {models}"
PY

echo "== C. visibility: detail carries model, context_window, last_prompt_tokens =="
curl -fsS "${AUTH[@]}" "$BASE/sessions/detail" | python3 -c "
import sys, json
rows = json.load(sys.stdin)['sessions']
row = next((r for r in rows if r['id'] == '$SID'), None)
assert row, 'session missing from detail'
assert row.get('model') == 'stub-m5', f\"detail model: {row.get('model')}\"
assert row.get('context_window') == 1000, f\"context_window: {row.get('context_window')}\"
assert row.get('last_prompt_tokens', 0) > 0, f\"last_prompt_tokens: {row.get('last_prompt_tokens')}\"
"

echo "== D. workspace rules: AGENTS.md in the SYSTEM prompt + rules event =="
python3 - "$STUB_LOG" <<'PY' || exit 1
import sys, json
found = False
for line in open(sys.argv[1]):
    body = json.loads(line)["body"]
    for m in body.get("messages", []):
        if m.get("role") == "system":
            c = m.get("content") or ""
            if "M5-RULES-TOKEN-whiskey" in c:
                assert "Workspace rules (AGENTS.md):" in c, \
                    "rules reached the system prompt without the label"
                found = True
assert found, "AGENTS.md content never reached a SYSTEM message"
PY
events "$SID" | python3 -c '
import sys, json
evs = json.load(sys.stdin)["events"]
rules = [e for e in evs if e["kind"] == "rules"]
assert rules, "no rules event on the ledger"
r = rules[-1]
actor, body = r["actor"], r["body"]
assert actor == "system", f"rules actor: {actor}"
assert body.get("path") == "AGENTS.md" and body.get("bytes", 0) > 0, f"rules body: {body}"
'

echo "== E. bootstrap: composerd init writes once; --version prints =="
"$BIN" --version | grep -q "composerd 0\." || fail "--version output wrong: $("$BIN" --version)"
INIT_DIR="$STATE/init-test"
mkdir -p "$INIT_DIR"
"$BIN" init --dir "$INIT_DIR" --provider ollama || fail "init failed"
[ -f "$INIT_DIR/config.toml" ] || fail "init wrote no config"
grep -q '\[roles.orchestrator\]' "$INIT_DIR/config.toml" || fail "init config missing orchestrator role"
grep -q 'auto_approve_edits = true' "$INIT_DIR/config.toml" || fail "init config missing edit parity default"
SHA1="$(sha256sum "$INIT_DIR/config.toml" | cut -d' ' -f1)"
if "$BIN" init --dir "$INIT_DIR" --provider ollama 2>/dev/null; then
  fail "second init must refuse to clobber an existing config"
fi
SHA2="$(sha256sum "$INIT_DIR/config.toml" | cut -d' ' -f1)"
[ "$SHA1" = "$SHA2" ] || fail "second init modified the existing config"

echo "M5-SHIP-OK"
