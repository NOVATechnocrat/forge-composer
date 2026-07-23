#!/usr/bin/env bash
# M0 "Spine" hermetic oracle — proves the daemon's event-sourced session lifecycle
# end-to-end with NO live model (stub-llm.py plays the provider) and NO shared state
# (fresh temp state dir). Prints M0-SPINE-OK on success; any failure exits nonzero.
#
# Proves: build; auth (401 without bearer token); session create; human message ->
# orchestrator reply via OpenAI-compatible streaming (stub sentinel observed); usage
# event; schema v1 on every event; secret REDACTION in persisted ledger bytes;
# CLI readback == API readback; reattach (kill daemon, restart, nothing lost, still live).
set -euo pipefail

REPO="$(cd "$(dirname "$0")/../.." && pwd)"
ORACLE_DIR="$REPO/tests/oracle"

STATE="$(mktemp -d /tmp/fc-m0-oracle.XXXXXX)"
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

echo "== start stub provider =="
STUB_PORT="$(free_port)"
python3 "$ORACLE_DIR/stub-llm.py" "$STUB_PORT" &
PIDS+=($!)

# The planted secret: MUST never appear in persisted ledger bytes.
export FC_TEST_KEY="sk-test-REDACTME-31337"

DAEMON_PORT="$(free_port)"
cat > "$STATE/config.toml" <<EOF
[server]
port = $DAEMON_PORT

[providers.stub]
base_url = "http://127.0.0.1:$STUB_PORT/v1"
api_key_env = "FC_TEST_KEY"

[roles.orchestrator]
provider = "stub"
model = "stub-model"
EOF

start_daemon() {
  FORGE_COMPOSER_STATE_DIR="$STATE" "$BIN" serve &
  DPID=$!
  PIDS+=($DPID)
  for _ in $(seq 1 100); do
    [ -f "$STATE/daemon.json" ] && curl -fsS "http://127.0.0.1:$DAEMON_PORT/health" >/dev/null 2>&1 && return 0
    sleep 0.1
  done
  fail "daemon did not become healthy in 10s"
}

echo "== start daemon =="
start_daemon
TOKEN="$(cat "$STATE/auth.token")"
[ -n "$TOKEN" ] || fail "auth.token empty"
BASE="http://127.0.0.1:$DAEMON_PORT"
AUTH=(-H "Authorization: Bearer $TOKEN")

echo "== auth: no token must be 401 =="
CODE="$(curl -s -o /dev/null -w '%{http_code}' "$BASE/sessions")"
[ "$CODE" = "401" ] || fail "expected 401 without token, got $CODE"

echo "== session lifecycle =="
SID="$(curl -fsS "${AUTH[@]}" -X POST "$BASE/sessions" | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')"
[ -n "$SID" ] || fail "no session id"

curl -fsS "${AUTH[@]}" -X POST "$BASE/sessions/$SID/message" \
  -H 'Content-Type: application/json' \
  -d "{\"text\":\"ping $FC_TEST_KEY\"}" >/dev/null || fail "post message"

events_json() { curl -fsS "${AUTH[@]}" "$BASE/sessions/$SID/events?since=0"; }

echo "== await orchestrator reply (stub sentinel) =="
OK=0
for _ in $(seq 1 100); do
  if events_json | grep -q "FORGE-COMPOSER-STUB-REPLY"; then OK=1; break; fi
  sleep 0.1
done
[ "$OK" = 1 ] || fail "no orchestrator reply with stub sentinel within 10s"

echo "== event invariants: schema v1 everywhere, usage present =="
events_json > "$STATE/events-snapshot.json"
python3 - "$STATE/events-snapshot.json" <<'PY' || exit 1
import sys, json
data = json.load(open(sys.argv[1]))
evs = data["events"]
assert evs, "no events"
for e in evs:
    assert e["v"] == "forgeloop.composer.event.v1", f"bad schema: {e['v']}"
    assert e["seq"] >= 1
kinds = [e["kind"] for e in evs]
actors = [e["actor"] for e in evs]
assert "message" in kinds, kinds
assert "usage" in kinds, kinds
assert "human" in actors and "orchestrator" in actors, actors
seqs = [e["seq"] for e in evs]
assert seqs == sorted(seqs) and len(set(seqs)) == len(seqs), f"seqs not dense/ordered: {seqs}"
PY

echo "== redaction: planted key must never persist =="
if grep -rq "$FC_TEST_KEY" "$STATE/sessions"; then
  fail "planted secret found in ledger bytes"
fi
grep -rq "\[REDACTED\]" "$STATE/sessions" || fail "no [REDACTED] marker in ledger"

echo "== CLI readback == API readback =="
API_NORM="$(events_json | python3 -c 'import sys,json;print("\n".join("|".join([str(e["seq"]),e["kind"],e["actor"]]) for e in json.load(sys.stdin)["events"]))')"
CLI_NORM="$(FORGE_COMPOSER_STATE_DIR="$STATE" "$BIN" ledger "$SID" | python3 -c 'import sys,json;print("\n".join("|".join([str(e["seq"]),e["kind"],e["actor"]]) for e in (json.loads(l) for l in sys.stdin if l.strip())))')"
[ -n "$API_NORM" ] || fail "empty API readback"
[ "$API_NORM" = "$CLI_NORM" ] || fail "CLI readback != API readback"$'\nAPI:\n'"$API_NORM"$'\nCLI:\n'"$CLI_NORM"

echo "== unknown session on CLI must exit 2 =="
set +e
FORGE_COMPOSER_STATE_DIR="$STATE" "$BIN" ledger "01JUNKJUNKJUNKJUNKJUNKJUNK" >/dev/null 2>&1
RC=$?
set -e
[ "$RC" = "2" ] || fail "expected exit 2 for unknown session, got $RC"

echo "== reattach: kill daemon, restart, nothing lost, still live =="
N_BEFORE="$(events_json | python3 -c 'import sys,json;print(len(json.load(sys.stdin)["events"]))')"
kill "$DPID" 2>/dev/null || true
wait "$DPID" 2>/dev/null || true
start_daemon
N_AFTER="$(events_json | python3 -c 'import sys,json;print(len(json.load(sys.stdin)["events"]))')"
[ "$N_BEFORE" = "$N_AFTER" ] || fail "events lost across restart ($N_BEFORE -> $N_AFTER)"

curl -fsS "${AUTH[@]}" -X POST "$BASE/sessions/$SID/message" \
  -H 'Content-Type: application/json' -d '{"text":"ping again"}' >/dev/null || fail "post after restart"
OK=0
for _ in $(seq 1 100); do
  N_NOW="$(events_json | python3 -c 'import sys,json;print(len(json.load(sys.stdin)["events"]))')"
  if [ "$N_NOW" -gt "$((N_AFTER + 1))" ]; then OK=1; break; fi
  sleep 0.1
done
[ "$OK" = 1 ] || fail "no new reply after restart"

echo "M0-SPINE-OK"
