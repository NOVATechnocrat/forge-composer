#!/usr/bin/env bash
# M3 "Physics" hermetic oracle — proves the Judge bridge, verdict purity, the
# provider escalation chain, and the mint seal end-to-end with NO live model
# (stub-llm-m3.py script-acts the orchestrator) and a FIXTURE forgeloop
# checkout (fake journal-gate.sh + canary mint.sh — the real tree is never
# touched). Prints M3-PHYSICS-OK on success; any failure exits nonzero.
#
# Proves (design §4/§6/§10, Law 4, D8):
#   B. Judge bridge — the run_gate tool runs harness/journal-gate.sh in the
#      configured forgeloop dir and appends a `verdict` event (actor:judge)
#      whose decision/intent are COPIED from the journal file on disk and
#      whose journal_path exists; the final reply cites the decision.
#   C. verdict purity (Law 4) — a RED gate with no journal evidence yields an
#      `error` event and ZERO verdict events: verdicts are never synthesized.
#   D. escalation chain (§6, D6) — the orchestrator's primary provider is a
#      DEAD port; every turn must succeed via the configured fallback tier,
#      each hop ledgered as an `error` event with escalated_to (no silent
#      rerouting), and usage cost follows the tier that actually answered.
#   E. mint seal (D8) — a scripted attempt to run mint.sh through the
#      terminal tool is hard-DENIED (no approval_request, deny reason cites
#      the seal) and the fixture's mint canary proves nothing executed.
set -euo pipefail

REPO="$(cd "$(dirname "$0")/../.." && pwd)"
ORACLE_DIR="$REPO/tests/oracle"

STATE="$(mktemp -d /tmp/fc-m3-oracle.XXXXXX)"
WS="$STATE/workspace"
FL="$STATE/fakeforgeloop"
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

echo "== A. fixtures: workspace + fake forgeloop checkout (gate + mint canary) =="
mkdir -p "$WS" "$FL/harness" "$FL/runs"
printf 'hello\n' > "$WS/base.txt"
cat > "$FL/harness/journal-gate.sh" <<'EOF'
#!/usr/bin/env bash
# Fixture gate: demo-app is GREEN with a real journal; bad-app is RED with
# no evidence. Mirrors the real journal-gate.sh output contract.
case "${1:-}" in
  demo-app)
    mkdir -p runs
    printf '%s\n' '{"schema":"forgeloop.run.v1","decision":"fail","intent":"earlier iteration"}' > runs/m3-fixture.jsonl
    printf '%s\n' '{"schema":"forgeloop.run.v1","decision":"pass","intent":"m3 demo intent"}' >> runs/m3-fixture.jsonl
    echo "GATE GREEN: 'demo-app' — evidence: runs/m3-fixture.jsonl (decision:pass, 0s fresh)"
    exit 0
    ;;
  *)
    echo "GATE RED: '${1:-}' — no green journal" >&2
    exit 1
    ;;
esac
EOF
cat > "$FL/harness/mint.sh" <<'EOF'
#!/usr/bin/env bash
# Mint canary: if this ever executes, the seal is broken.
echo "minted" > "$(dirname "$0")/../MINTED"
EOF
chmod +x "$FL/harness/journal-gate.sh" "$FL/harness/mint.sh"

echo "== start scripted stub provider (the GOOD tier) =="
STUB_PORT="$(free_port)"
python3 "$ORACLE_DIR/stub-llm-m3.py" "$STUB_PORT" "$STUB_LOG" &
PIDS+=($!)
DEAD_PORT="$(free_port)"   # nothing listens here — the primary tier is a stall

DAEMON_PORT="$(free_port)"
cat > "$STATE/config.toml" <<EOF
[server]
port = $DAEMON_PORT

[providers.stub-bad]
base_url = "http://127.0.0.1:$DEAD_PORT/v1"

[providers.stub-good]
base_url = "http://127.0.0.1:$STUB_PORT/v1"

[roles.orchestrator]
provider = "stub-bad"
model = "bad-model"
escalation = ["fallback"]

[roles.fallback]
provider = "stub-good"
model = "stub-m3"

[pricing."stub-m3"]
input_per_mtok = 1.0
output_per_mtok = 2.0

[forgeloop]
dir = "$FL"
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

new_session() {
  curl -fsS "${AUTH[@]}" -X POST "$BASE/sessions" -H 'Content-Type: application/json' \
    -d "{\"workspace\":\"$WS\"}" | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])'
}
send() {
  curl -fsS "${AUTH[@]}" -X POST "$BASE/sessions/$1/message" -H 'Content-Type: application/json' \
    -d "{\"text\":\"$2\"}" >/dev/null
}
events() { curl -fsS "${AUTH[@]}" "$BASE/sessions/$1/events?since=0"; }
await() { # sid needle label
  for _ in $(seq 1 150); do
    if events "$1" | grep -q "$2"; then return 0; fi
    sleep 0.1
  done
  fail "$3: '$2' not observed within 15s"
}

echo "== D. escalation: dead primary tier, ledgered hop, priced fallback =="
SID_D="$(new_session)"
send "$SID_D" "please M3-ESCALATE"
await "$SID_D" "M3-ESCALATED-OK" "escalated final reply"
events "$SID_D" > "$STATE/d-events.json"
python3 - "$STATE/d-events.json" <<'PY' || exit 1
import sys, json
evs = json.load(open(sys.argv[1]))["events"]
hops = [e for e in evs if e["kind"] == "error" and e["body"].get("escalated_to")]
assert hops, "no ledgered escalation hop — silent rerouting is the Cursor failure we left"
assert hops[0]["body"]["escalated_to"] == "fallback", f"escalated_to: {hops[0]['body']}"
usage = [e for e in evs if e["kind"] == "usage"]
assert usage, "no usage event"
priced = [e for e in usage if e["body"].get("cost_usd", 0) > 0]
assert priced, f"cost did not follow the answering tier (stub-m3 is priced): {[e['body'] for e in usage]}"
PY

echo "== B. Judge bridge: run_gate -> pointer-copied verdict event =="
SID_B="$(new_session)"
send "$SID_B" "please M3-RUN-GATE"
await "$SID_B" '"kind":"verdict"' "verdict event"
await "$SID_B" "M3-GATE-DONE" "gate final reply"
events "$SID_B" > "$STATE/b-events.json"
python3 - "$STATE/b-events.json" <<'PY' || exit 1
import sys, json, os
evs = json.load(open(sys.argv[1]))["events"]
vs = [e for e in evs if e["kind"] == "verdict"]
assert len(vs) == 1, f"expected exactly one verdict event, got {len(vs)}"
v = vs[0]
assert v["actor"] == "judge", f"verdict actor: {v['actor']}"
b = v["body"]
assert b.get("oracle_id") == "demo-app", f"oracle_id: {b}"
assert b.get("decision") == "pass", f"decision: {b}"
assert b.get("intent") == "m3 demo intent", f"intent: {b}"
jp = b.get("journal_path", "")
assert os.path.isabs(jp) and os.path.exists(jp), f"journal_path not an existing absolute path: {jp}"
# Pointer honesty: the event's decision must equal the journal's LAST decision line.
last = None
for line in open(jp):
    line = line.strip()
    if not line:
        continue
    d = json.loads(line)
    if "decision" in d:
        last = d["decision"]
assert last == b["decision"], f"verdict body ({b['decision']}) diverges from journal ({last})"
finals = [e for e in evs if e["kind"] == "message" and "M3-GATE-DONE" in e["body"].get("text", "")]
assert finals, "no final reply"
assert "gate demo-app: pass" in finals[-1]["body"]["text"], "final reply does not cite the decision"
PY

echo "== C. verdict purity: RED gate with no journal -> error event, ZERO verdicts =="
SID_C="$(new_session)"
send "$SID_C" "please M3-RUN-BAD-GATE"
await "$SID_C" "M3-BADGATE-SEEN" "bad-gate final reply"
events "$SID_C" > "$STATE/c-events.json"
python3 - "$STATE/c-events.json" <<'PY' || exit 1
import sys, json
evs = json.load(open(sys.argv[1]))["events"]
assert not any(e["kind"] == "verdict" for e in evs), \
    "SYNTHESIZED VERDICT: a red gate with no journal produced a verdict event (Law 4 violation)"
errs = [e for e in evs if e["kind"] == "error" and "no journal evidence" in e["body"].get("error", "")]
assert errs, "no 'no journal evidence' error event for the red gate"
results = [e for e in evs if e["kind"] == "tool_result" and e["body"].get("name") == "run_gate"]
assert results and results[-1]["body"].get("ok") is False, "red gate tool_result should be ok:false"
PY

echo "== E. mint seal: terminal mint.sh attempt hard-denied, canary intact =="
SID_E="$(new_session)"
send "$SID_E" "please M3-TRY-MINT"
await "$SID_E" "M3-MINT-SEEN" "mint-attempt final reply"
[ ! -f "$FL/MINTED" ] || fail "MINT CANARY TRIPPED: mint.sh executed"
events "$SID_E" > "$STATE/e-events.json"
python3 - "$STATE/e-events.json" <<'PY' || exit 1
import sys, json
evs = json.load(open(sys.argv[1]))["events"]
assert not any(e["kind"] == "approval_request" for e in evs), \
    "mint.sh must be a hard deny, not an ask"
results = [e for e in evs if e["kind"] == "tool_result" and e["body"].get("name") == "terminal"]
assert results, "no tool_result for the mint attempt"
r = results[-1]["body"]
assert r.get("denied") is True, f"mint attempt not denied: {r}"
assert "mint.sh is sealed" in r.get("output", ""), f"deny reason does not cite the seal: {r}"
PY

echo "M3-PHYSICS-OK"
