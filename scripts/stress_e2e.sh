#!/usr/bin/env bash
# End-to-end degraded-network stress harness for the telemedicine gateway.
#
# Drives the REAL release binaries — tgw-field, tgw-gateway, tgw-netsim — over loopback,
# with tgw-netsim standing in for the degraded radio link (packet loss + bit-flip corruption
# + burst windows + jitter + a 64 kbps rate cap). For each (loss, corrupt) cell it sends a
# batch of vitals, then validates against the gateway's /api/observations API that:
#   1. every value the gateway persisted is EXACTLY one that was sent (no corrupt/garbage data);
#   2. plausibility flags land on out-of-range readings and clean readings carry none;
#   3. delivery success degrades gracefully with loss (and never silently drops).
#
# It also runs a live two-daemon peer-relay failover scenario (Fix 2): device A's direct link
# is 100% dead, and it must deliver by relaying through a healthy peer B.
#
# Usage:
#   scripts/stress_e2e.sh                 # full sweep + relay scenario
#   LOSS=0.5 CORRUPT=0.3 scripts/stress_e2e.sh single   # one ad-hoc cell
#
# Results (CSV + JSON dumps + logs) land in $WORK. Nothing here touches the repo.
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/release"
WORK="${WORK:-$ROOT/target/stress}"
SEED="${SEED:-1}"
BUNDLES_PER_CELL="${BUNDLES_PER_CELL:-2}"   # copies of each of the 5 payloads per cell

# Fixed loopback ports (runs are sequential with teardown between them).
GW_UDP=47100
GW_HTTP=8137
NS=47050

RESULTS="$WORK/results.csv"

log()  { printf '%s\n' "$*" >&2; }
hr()   { log "------------------------------------------------------------------"; }

setup() {
  rm -rf "$WORK"; mkdir -p "$WORK/keys" "$WORK/logs" "$WORK/dumps"
  "$BIN/tgw-field" keygen --out "$WORK/keys/psk.key" >/dev/null 2>&1

  cat > "$WORK/gateway.toml" <<EOF
[link]
bandwidth_bps = 56000
symbol_size = 1100
overhead_factor = 1.4
[retry]
nack_timeout_ms = 400
retry_backoff_ms = 400
max_retries = 6
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

  echo "loss,corrupt,profile,sent,field_delivered,gw_persisted,flags_correct,corruption_leaks,wall_s" > "$RESULTS"
}

# Write a field config pointing at $NS (netsim) as the gateway.
write_field_cfg() {
  local path="$1"
  cat > "$path" <<EOF
[link]
bandwidth_bps = 56000
symbol_size = 1100
overhead_factor = 1.4
[retry]
nack_timeout_ms = 400
retry_backoff_ms = 400
max_retries = 6
[net]
gateway_addr = "127.0.0.1:$NS"
listen_addr = "127.0.0.1:0"
http_addr = "127.0.0.1:0"
[crypto]
key_file = "$WORK/keys/psk.key"
[media]
image_max_bytes = 30000
EOF
}

start_gateway() {
  rm -f "$WORK/gateway.redb"
  TGW_DB_PATH="$WORK/gateway.redb" RUST_LOG=warn \
    "$BIN/tgw-gateway" --config "$WORK/gateway.toml" --static-dir "$ROOT/crates/tgw-gateway/static" \
    > "$WORK/logs/gateway.log" 2>&1 &
  GW_PID=$!
  # Wait for the HTTP API to answer.
  for _ in $(seq 1 50); do
    if curl -fs "http://127.0.0.1:$GW_HTTP/api/observations" >/dev/null 2>&1; then return 0; fi
    sleep 0.1
  done
  log "!! gateway did not come up; see $WORK/logs/gateway.log"; return 1
}

start_netsim() {
  local loss="$1" corrupt="$2" burst_ms="$3" jitter_ms="$4"
  RUST_LOG=warn "$BIN/tgw-netsim" \
    --loss "$loss" --corrupt "$corrupt" --seed "$SEED" \
    --burst-every-ms "$burst_ms" --jitter-ms "$jitter_ms" \
    --listen "127.0.0.1:$NS" --forward "127.0.0.1:$GW_UDP" \
    > "$WORK/logs/netsim.log" 2>&1 &
  NS_PID=$!
  sleep 0.3
}

kill_quiet() { for p in "$@"; do kill "$p" >/dev/null 2>&1; wait "$p" >/dev/null 2>&1; done; }

# The 5 canonical payloads. Format: id|args|kind|value|expect_flag
payloads() {
  cat <<'EOF'
clean-pulse|--pulse 78|pulse|78|
clean-spo2|--spo2 98|spo2|98|
flag-spo2-low|--spo2 40|spo2|40|spo2-out-of-range
flag-pulse-high|--pulse 350|pulse|350|heart-rate-out-of-range
flag-bp-high|--bp 320/100|bp||systolic-out-of-range
EOF
}

# Run one sweep cell. Args: loss corrupt profile-label burst_ms jitter_ms
run_cell() {
  local loss="$1" corrupt="$2" profile="$3" burst_ms="$4" jitter_ms="$5"
  hr; log "CELL  loss=$loss corrupt=$corrupt profile=$profile"
  local cfg="$WORK/field.toml"; write_field_cfg "$cfg"
  local expect="$WORK/dumps/expect_${loss}_${corrupt}_${profile}.json"
  local qpath="$WORK/field-queue.redb"; rm -f "$qpath"

  start_gateway || { echo "$loss,$corrupt,$profile,0,0,0,0,ERR,0" >> "$RESULTS"; return; }
  start_netsim "$loss" "$corrupt" "$burst_ms" "$jitter_ms"

  local sent=0 field_delivered=0
  : > "$expect"
  local t0; t0=$(date +%s.%N)
  local i=0
  while [ "$i" -lt "$BUNDLES_PER_CELL" ]; do
    while IFS='|' read -r name args kind value flag; do
      [ -z "$name" ] && continue
      local pid="P-${name}-${i}"
      sent=$((sent+1))
      # shellcheck disable=SC2086
      if TGW_QUEUE_PATH="$qpath" RUST_LOG=error timeout 30 \
           "$BIN/tgw-field" --config "$cfg" send-vitals $args --patient "$pid" \
           > "$WORK/logs/send_${loss}_${corrupt}_${i}_${name}.log" 2>&1; then
        field_delivered=$((field_delivered+1))
      fi
      printf '%s\t%s\t%s\t%s\n' "$pid" "$kind" "$value" "$flag" >> "$expect"
    done < <(payloads)
    i=$((i+1))
  done
  local t1; t1=$(date +%s.%N)
  local wall; wall=$(awk "BEGIN{printf \"%.1f\", $t1-$t0}")

  # Give the gateway a moment to persist the last receipt(s).
  sleep 0.5
  local obs="$WORK/dumps/obs_${loss}_${corrupt}_${profile}.json"
  curl -fs "http://127.0.0.1:$GW_HTTP/api/observations" > "$obs" 2>/dev/null || echo '[]' > "$obs"

  # Validate correctness in python: persisted values must match sent; flags must be right.
  local verdict; verdict=$(python3 "$ROOT/scripts/validate_obs.py" "$expect" "$obs")
  # verdict = "persisted flags_correct corruption_leaks"
  read -r gw_persisted flags_correct leaks <<< "$verdict"

  kill_quiet "$NS_PID" "$GW_PID"

  log "  sent=$sent  field_delivered=$field_delivered  gw_persisted=$gw_persisted  flags_correct=$flags_correct  corruption_leaks=$leaks  ${wall}s"
  echo "$loss,$corrupt,$profile,$sent,$field_delivered,$gw_persisted,$flags_correct,$leaks,$wall" >> "$RESULTS"
}

# ---- Live peer-relay failover (Fix 2) --------------------------------------------------
# Device A's direct link is 100% dead (netsim loss=1.0). B is healthy. A must relay via B.
relay_scenario() {
  hr; log "RELAY  device A (dead direct link) must deliver via healthy peer B"
  # A shared site-local multicast group: both daemons co-bind it (SO_REUSEPORT) and converge on
  # one host (Fix F3). A_RELAY/B_RELAY are each device's relay-listen address.
  local DISC_GROUP="239.255.7.66:47312" A_RELAY=47200 B_RELAY=47201 A_NS=47060
  start_gateway || { log "  relay: gateway down"; return; }

  # A dead direct link: netsim at A_NS dropping 100% toward the gateway.
  RUST_LOG=warn "$BIN/tgw-netsim" --loss 1.0 --corrupt 0 --burst-every-ms 3600000 --jitter-ms 1 \
    --listen "127.0.0.1:$A_NS" --forward "127.0.0.1:$GW_UDP" > "$WORK/logs/relay_netsim.log" 2>&1 &
  local ANS_PID=$!; sleep 0.3

  # Device B — healthy direct link + relay service; discovers A on the shared multicast group.
  cat > "$WORK/fieldB.toml" <<EOF
[link]
bandwidth_bps = 56000
symbol_size = 1100
overhead_factor = 1.4
[retry]
nack_timeout_ms = 400
retry_backoff_ms = 400
max_retries = 6
[net]
gateway_addr = "127.0.0.1:$GW_UDP"
listen_addr = "127.0.0.1:0"
http_addr = "127.0.0.1:0"
[crypto]
key_file = "$WORK/keys/psk.key"
[media]
image_max_bytes = 30000
[relay]
enabled = true
discovery_addr = "$DISC_GROUP"
relay_listen_addr = "127.0.0.1:$B_RELAY"
announce_interval_ms = 500
peer_ttl_ms = 8000
EOF
  # Device A — dead direct link (via A_NS), relay enabled, discovers B on the shared group.
  cat > "$WORK/fieldA.toml" <<EOF
[link]
bandwidth_bps = 56000
symbol_size = 1100
overhead_factor = 1.4
[retry]
nack_timeout_ms = 800
retry_backoff_ms = 500
max_retries = 3
# F1: re-arm A's STUCK bundle quickly so the daemon retries it through the relay this run.
stuck_retry_backoff_ms = 500
max_stuck_retries = 20
[net]
gateway_addr = "127.0.0.1:$A_NS"
listen_addr = "127.0.0.1:0"
http_addr = "127.0.0.1:0"
[crypto]
key_file = "$WORK/keys/psk.key"
[media]
image_max_bytes = 30000
[relay]
enabled = true
discovery_addr = "$DISC_GROUP"
relay_listen_addr = "127.0.0.1:$A_RELAY"
announce_interval_ms = 500
peer_ttl_ms = 8000
EOF

  local QA="$WORK/qA.redb" QB="$WORK/qB.redb"; rm -f "$QA" "$QB"

  # Start B's daemon (announces presence + serves relay requests).
  TGW_QUEUE_PATH="$QB" RUST_LOG=info "$BIN/tgw-field" --config "$WORK/fieldB.toml" daemon \
    > "$WORK/logs/relay_B.log" 2>&1 &
  local B_PID=$!
  sleep 1

  # A does a normal one-shot send over its DEAD direct link. One-shot is direct-only, so the
  # bundle cleanly reaches STUCK and is KEPT in A's queue (exit non-zero is expected). No
  # interrupt/kill trick — the daemon's F1 re-arm now reaches STUCK bundles through the queue.
  local RPID="P-RELAY-42"
  TGW_QUEUE_PATH="$QA" RUST_LOG=error \
    "$BIN/tgw-field" --config "$WORK/fieldA.toml" send-vitals --pulse 84 --patient "$RPID" \
    > "$WORK/logs/relay_A_send.log" 2>&1
  local a_send_rc=$?
  local a_state; a_state=$(TGW_QUEUE_PATH="$QA" "$BIN/tgw-field" --config "$WORK/fieldA.toml" status 2>/dev/null | grep RELAY | awk '{print $3}')

  # Start A's daemon: it discovers B on the shared multicast group (F3), re-arms the STUCK
  # bundle after the backoff (F1), and relays it through B on the next drain pass.
  TGW_QUEUE_PATH="$QA" RUST_LOG=info "$BIN/tgw-field" --config "$WORK/fieldA.toml" daemon \
    > "$WORK/logs/relay_A_daemon.log" 2>&1 &
  local A_PID=$!

  # Wait (up to ~20s) for the gateway to persist A's relayed bundle.
  local relay_ok=0
  for _ in $(seq 1 40); do
    if curl -fs "http://127.0.0.1:$GW_HTTP/api/observations" 2>/dev/null | grep -q "$RPID"; then
      relay_ok=1; break
    fi
    sleep 0.5
  done

  # Did the daemon re-arm the STUCK bundle (F1) for another pass?
  local rearmed=0
  grep -qi 're-armed' "$WORK/logs/relay_A_daemon.log" 2>/dev/null && rearmed=1
  # Did A discover peer B on one host (F3: SO_REUSEPORT + instance-id filter + multicast loop)?
  local discovered=0
  grep -qi 'peer relay' "$WORK/logs/relay_A_daemon.log" 2>/dev/null && discovered=1

  kill_quiet "$A_PID" "$B_PID" "$ANS_PID" "$GW_PID"

  log "  daemon re-armed STUCK bundle (F1) = $rearmed"
  log "  A discovered peer B on one host (F3) = $discovered  (1 expected now)"
  log "  relay_delivered_live = $relay_ok  (via the normal daemon queue path, no interrupt trick)"
  log "  one-shot send rc = $a_send_rc (non-zero expected: direct link dead); queue state = ${a_state:-n/a}"
  echo "RELAY,,failover,1,$rearmed,$relay_ok,NA,$discovered,NA" >> "$RESULTS"
}

main() {
  [ -x "$BIN/tgw-field" ] || { log "release binaries missing — run: cargo build --release --bins"; exit 1; }
  setup

  case "${1:-full}" in
    single)
      run_cell "${LOSS:-0.5}" "${CORRUPT:-0.0}" "adhoc" 3600000 5 ;;
    relay)
      relay_scenario ;;
    full)
      # Loss sweep (bursts off, minimal jitter → isolate random loss).
      for L in 0.0 0.25 0.5 0.75 0.9 1.0; do run_cell "$L" 0.0 "loss" 3600000 5; done
      # Corruption sweep (light loss, rising corruption → exercise the integrity gate).
      for C in 0.25 0.5 0.75 1.0; do run_cell 0.1 "$C" "corrupt" 3600000 5; done
      # Compound realistic degraded radio (loss + corruption + bursts + jitter + rate cap).
      run_cell 0.3 0.15 "realistic" 5000 40
      # Live peer-relay failover.
      relay_scenario ;;
  esac

  hr; log "RESULTS ($RESULTS):"; column -s, -t < "$RESULTS" >&2
}

main "$@"
