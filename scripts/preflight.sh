#!/usr/bin/env bash
# Pre-demo verification: proves the two headline constraints and the offline-first
# guarantee on THIS machine, with real release binaries, in a few minutes, no root.
#
#   1. UNIT/INTEGRATION  cargo test --workspace (FEC, wire, queue, FHIR, netsim, API).
#   2. EXTREME LINK      one stress cell at 25% packet loss + 56 kbps through tgw-netsim
#                        (delegates to scripts/stress_e2e.sh single) — data must land
#                        intact and correctly flagged at the gateway.
#   3. OFFLINE-FIRST     total blackout: vitals are captured while the gateway is DOWN,
#                        must be KEPT in the persistent queue (never dropped), and must
#                        auto-deliver when the gateway comes back and the daemon drains.
#
# Usage:
#   scripts/preflight.sh              # everything
#   scripts/preflight.sh --fast      # skip the workspace test suite (steps 2–3 only)
#
# Exit code 0 = all green; non-zero = at least one check failed (see the log lines).
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/release"
WORK="$ROOT/target/preflight"
GW_UDP=47130
GW_HTTP=8139

FAILURES=0
log()  { printf '%s\n' "$*" >&2; }
hr()   { log "=================================================================="; }
pass() { log "PASS  $*"; }
fail() { log "FAIL  $*"; FAILURES=$((FAILURES + 1)); }

kill_quiet() { for p in "$@"; do kill "$p" >/dev/null 2>&1; wait "$p" >/dev/null 2>&1; done; }

# ---------------------------------------------------------------- build + unit tests
hr; log "STEP 0  building release binaries"
(cd "$ROOT" && cargo build --release --workspace --bins) || { fail "release build"; exit 1; }

if [ "${1:-}" != "--fast" ]; then
  hr; log "STEP 1  workspace test suite (FEC / wire / queue / FHIR / netsim / API)"
  if (cd "$ROOT" && cargo test --workspace --quiet); then
    pass "workspace tests"
  else
    fail "workspace tests"
  fi
fi

# ---------------------------------------------------------------- extreme-link cell
hr; log "STEP 2  extreme link: 25% loss + 56 kbps through tgw-netsim (real binaries)"
if LOSS=0.25 CORRUPT=0.0 WORK="$ROOT/target/preflight-stress" \
     "$ROOT/scripts/stress_e2e.sh" single >/dev/null 2>&1; then
  RES="$ROOT/target/preflight-stress/results.csv"
  # CSV: loss,corrupt,profile,sent,field_delivered,gw_persisted,flags_correct,corruption_leaks,wall_s
  LINE=$(tail -n 1 "$RES")
  SENT=$(echo "$LINE" | cut -d, -f4)
  PERSISTED=$(echo "$LINE" | cut -d, -f6)
  LEAKS=$(echo "$LINE" | cut -d, -f8)
  log "        sent=$SENT persisted=$PERSISTED corruption_leaks=$LEAKS"
  if [ "$PERSISTED" = "$SENT" ] && [ "${LEAKS:-1}" = "0" ]; then
    pass "all $SENT bundles delivered intact at 25% loss / 56 kbps"
  else
    fail "delivery under 25% loss: persisted=$PERSISTED of sent=$SENT, leaks=$LEAKS"
  fi
else
  fail "stress cell did not run (see target/preflight-stress/logs)"
fi

# ---------------------------------------------------------------- offline-first blackout
hr; log "STEP 3  offline-first: capture during a TOTAL blackout, deliver on recovery"
rm -rf "$WORK"; mkdir -p "$WORK/keys" "$WORK/logs"
"$BIN/tgw-field" keygen --out "$WORK/keys/psk.key" >/dev/null 2>&1

write_cfg() { # $1 = path
  cat > "$1" <<EOF
[link]
bandwidth_bps = 56000
symbol_size = 1100
overhead_factor = 1.4
[retry]
nack_timeout_ms = 400
retry_backoff_ms = 400
max_retries = 3
stuck_retry_backoff_ms = 500
max_stuck_retries = 20
[net]
gateway_addr = "127.0.0.1:$GW_UDP"
listen_addr = "127.0.0.1:0"
http_addr = "127.0.0.1:0"
[crypto]
key_file = "$WORK/keys/psk.key"
[media]
image_max_bytes = 30000
EOF
}
write_cfg "$WORK/field.toml"
cat > "$WORK/gateway.toml" <<EOF
[link]
bandwidth_bps = 56000
symbol_size = 1100
overhead_factor = 1.4
[retry]
nack_timeout_ms = 400
retry_backoff_ms = 400
max_retries = 3
[net]
gateway_addr = "127.0.0.1:$GW_UDP"
listen_addr = "127.0.0.1:$GW_UDP"
http_addr = "127.0.0.1:$GW_HTTP"
[crypto]
key_file = "$WORK/keys/psk.key"
[media]
image_max_bytes = 30000
[storage]
db_path = "$WORK/gateway.redb"
EOF

QUEUE="$WORK/field-queue.redb"
PATIENT="P-BLACKOUT-1"

# 3a. Gateway is DOWN. The send must fail loudly (non-zero exit) but KEEP the bundle.
TGW_QUEUE_PATH="$QUEUE" RUST_LOG=error timeout 60 \
  "$BIN/tgw-field" --config "$WORK/field.toml" send-vitals --pulse 91 --patient "$PATIENT" \
  > "$WORK/logs/blackout_send.log" 2>&1
RC=$?
STATE=$(TGW_QUEUE_PATH="$QUEUE" "$BIN/tgw-field" status 2>/dev/null | awk 'NR>1 && NF>2 {print $3; exit}')
if [ "$RC" -ne 0 ] && [ -n "$STATE" ]; then
  pass "blackout capture kept in the persistent queue (state=$STATE, exit=$RC)"
else
  fail "blackout capture: exit=$RC state=${STATE:-missing} (expected non-zero exit + kept bundle)"
fi

# 3b. Gateway comes back; the daemon must drain the kept bundle with NO re-entry of data.
RUST_LOG=warn TGW_DB_PATH="$WORK/gateway.redb" \
  "$BIN/tgw-gateway" --config "$WORK/gateway.toml" --static-dir "$ROOT/hospital-ui" \
  > "$WORK/logs/gateway.log" 2>&1 &
GW_PID=$!
for _ in $(seq 1 50); do
  curl -fs "http://127.0.0.1:$GW_HTTP/api/observations" >/dev/null 2>&1 && break
  sleep 0.1
done

TGW_QUEUE_PATH="$QUEUE" RUST_LOG=info \
  "$BIN/tgw-field" --config "$WORK/field.toml" daemon > "$WORK/logs/daemon.log" 2>&1 &
FD_PID=$!

RECOVERED=0
for _ in $(seq 1 40); do
  if curl -fs "http://127.0.0.1:$GW_HTTP/api/observations" 2>/dev/null | grep -q "$PATIENT"; then
    RECOVERED=1; break
  fi
  sleep 0.5
done
kill_quiet "$FD_PID" "$GW_PID"

if [ "$RECOVERED" = 1 ]; then
  pass "queued bundle auto-delivered after the gateway returned (no data loss)"
else
  fail "queued bundle was NOT delivered after recovery (see $WORK/logs)"
fi

# ---------------------------------------------------------------- verdict
hr
if [ "$FAILURES" -eq 0 ]; then
  log "PREFLIGHT: ALL CHECKS PASSED — demo-ready."
  exit 0
else
  log "PREFLIGHT: $FAILURES CHECK(S) FAILED — see logs above."
  exit 1
fi
