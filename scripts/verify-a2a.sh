#!/usr/bin/env bash
#
# End-to-end verification for the smallhead <-> DSH A2A link.
#
# Runs four stages and prints one PASS/FAIL line each:
#   1. hermetic protocol tests (cargo test, mock provider, loopback only)
#   2. a live smallhead A2A bridge + the smallhead client round trip
#   3. the DSH inbound A2A route, when the running DSH already serves it
#   4. the DSH -> smallhead client tool's route, when DSH serves it
#
# Stage 3/4 SKIP (not FAIL) until the DSH host has been restarted with this
# branch's plugin: the Loader caches the module it imported at startup, so a
# plugin that was already mounted keeps serving the old code until restart.
#
# Usage: scripts/verify-a2a.sh [dsh-base-url]   (default http://127.0.0.1:3080)

set -uo pipefail

DSH_BASE="${1:-http://127.0.0.1:3080}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# Must match the DSH plugin's `a2aPeer` (default http://127.0.0.1:3999) so that
# stage 4 (DSH -> smallhead) reaches this exact server.
A2A_PORT="${A2A_PORT:-3999}"
A2A_BASE="http://127.0.0.1:${A2A_PORT}"
PASS=0
FAIL=0
SKIP=0

say()  { printf '%s\n' "$*"; }
ok()   { PASS=$((PASS + 1)); say "  PASS  $*"; }
bad()  { FAIL=$((FAIL + 1)); say "  FAIL  $*"; }
skip() { SKIP=$((SKIP + 1)); say "  SKIP  $*"; }

# One JSON field from stdin, without requiring jq.
field() {
  python3 -c 'import json,sys; print(json.load(sys.stdin).get(sys.argv[1], ""))' "$1" 2>/dev/null
}

say "== A2A verification =="
say "repo:     $ROOT"
say "dsh base: $DSH_BASE"
say "a2a base: $A2A_BASE"
say

# ---------------------------------------------------------------- stage 1
say "[1] hermetic protocol tests"
if (cd "$ROOT" && cargo test -q -p smallhead-server >/tmp/verify-a2a-cargo.log 2>&1); then
  ok "cargo test -p smallhead-server"
else
  bad "cargo test -p smallhead-server (see /tmp/verify-a2a-cargo.log)"
fi
say

# ---------------------------------------------------------------- stage 2
say "[2] live smallhead bridge + client round trip"
(cd "$ROOT" && cargo build -q -p smallhead-server) || bad "cargo build"
BIN="$ROOT/target/debug/smallhead-server"

"$BIN" serve --port "$A2A_PORT" >/tmp/verify-a2a-server.log 2>&1 &
SERVER_PID=$!
cleanup() { kill "$SERVER_PID" >/dev/null 2>&1 || true; }
trap cleanup EXIT

for _ in $(seq 1 30); do
  [ "$(curl -s -o /dev/null -w '%{http_code}' "$A2A_BASE/health")" = "200" ] && break
  sleep 0.2
done

if [ "$(curl -s -o /dev/null -w '%{http_code}' "$A2A_BASE/health")" = "200" ]; then
  ok "GET /health -> 200"
else
  bad "GET /health (server log: /tmp/verify-a2a-server.log)"
fi

CARD_NAME="$(curl -s "$A2A_BASE/.well-known/agent-card.json" | field name)"
if [ "$CARD_NAME" = "smallhead" ]; then
  ok "agent card names smallhead"
else
  bad "agent card name was '$CARD_NAME'"
fi

SEND_OUT="$("$BIN" send "$A2A_BASE" "verify: 闭环自测" 2>&1)"
if [ "$(printf '%s' "$SEND_OUT" | field status)" = "done" ]; then
  ok "smallhead client -> smallhead server round trip (status done)"
else
  bad "client round trip: $SEND_OUT"
fi

TASK_ID="$(printf '%s' "$SEND_OUT" | field task_id)"
if [ -n "$TASK_ID" ] && [ "$(curl -s -o /dev/null -w '%{http_code}' "$A2A_BASE/a2a/tasks/$TASK_ID")" = "200" ]; then
  ok "GET /a2a/tasks/\$id -> 200"
else
  bad "task poll for '$TASK_ID'"
fi
say

# ---------------------------------------------------------------- stage 3
say "[3] DSH inbound A2A route (smallhead -> DSH)"
PROBE="$(curl -s -X POST "$DSH_BASE/smallhead/a2a/messages" \
  -H 'content-type: application/json' -d '{"from":"smallhead","text":"probe"}')"
if printf '%s' "$PROBE" | grep -q 'invalid A2A'; then
  ok "route is live and validates (400 invalid A2A kind)"
  REPLY="$(curl -s -X POST "$DSH_BASE/smallhead/a2a/messages" \
    -H 'content-type: application/json' \
    -d '{"from":"smallhead","to":"dsh","kind":"request","intent":"research","text":"verify: 请确认收到","ttl":3}')"
  STATUS="$(printf '%s' "$REPLY" | field status)"
  if [ "$STATUS" = "completed" ] || [ "$STATUS" = "queued" ]; then
    ok "DSH answered the inbound message (status=$STATUS)"
    say "        reply: $(printf '%s' "$REPLY" | field text | head -c 160)"
  else
    bad "DSH inbound reply: $REPLY"
  fi
else
  skip "DSH does not serve /smallhead/a2a/messages yet — restart the DSH host"
  say "        (the plugin row is mounted, but the Loader still holds the module"
  say "         it imported at startup; a restart re-imports this branch's code)"
fi
say

# ---------------------------------------------------------------- stage 4
say "[4] DSH -> smallhead client route"
SEND_PROBE="$(curl -s -X POST "$DSH_BASE/smallhead/api/a2a/send" \
  -H 'content-type: application/json' -d '{}')"
if printf '%s' "$SEND_PROBE" | grep -q 'text is required'; then
  ok "send route is live and validates"
  OUT="$(curl -s -X POST "$DSH_BASE/smallhead/api/a2a/send" \
    -H 'content-type: application/json' \
    -d "{\"text\":\"verify: DSH 发来问候\"}")"
  if [ "$(printf '%s' "$OUT" | field ok)" = "True" ]; then
    ok "DSH -> smallhead round trip (ok=true)"
    say "        reply: $(printf '%s' "$OUT" | field text | head -c 160)"
  else
    bad "DSH send route reply: $OUT"
  fi
elif printf '%s' "$SEND_PROBE" | grep -q 'method-not-allowed'; then
  skip "DSH serves the old plugin (no POST branch) — restart the DSH host"
else
  skip "DSH send route not reachable: ${SEND_PROBE:0:120}"
fi

say
say "== $PASS passed, $FAIL failed, $SKIP skipped =="
[ "$FAIL" -eq 0 ]
