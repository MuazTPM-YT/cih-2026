#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
# demo/run-client.sh — spin up the FIELD/CLIENT side on THIS device.
#
# Starts the field bridge: serves the capture UI (:8091) and, on Submit / POST
# /api/capture, runs the REAL send path (seal → RaptorQ → UDP → AEAD receipt) to
# the hospital device's gateway. The capture UI is a browser page on this device.
#
# Usage:  demo/run-client.sh <HOSPITAL_IP> [ui_port]
#           <HOSPITAL_IP>  the IP printed by demo/run-hospital.sh (default UI port 8091)
#
# Requires keys/device-a.key to be the SAME file as on the hospital device.
# ─────────────────────────────────────────────────────────────────────────────
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

HOSP="${1:-}"
PORT="${2:-8091}"
if [ -z "$HOSP" ]; then
  echo "usage: demo/run-client.sh <HOSPITAL_IP> [ui_port]"
  echo "  <HOSPITAL_IP> is printed by demo/run-hospital.sh on the hospital device."
  exit 1
fi

FIELD="target/release/tgw-field"
[ -x "$FIELD" ] || { echo "building tgw-field (release)…"; cargo build --release -p tgw-field; }

if [ ! -f keys/device-a.key ]; then
  echo "ERROR: keys/device-a.key not found on this device."
  echo "  Copy it from the hospital device (same repo path) — both sides MUST share one PSK."
  exit 1
fi

lan_ip() { ip -4 route get 1 2>/dev/null | awk '{print $7; exit}' || hostname -I 2>/dev/null | awk '{print $1}'; }
IP="$(lan_ip)"; IP="${IP:-<this-device-ip>}"
KEYHASH="$(sha256sum keys/device-a.key | cut -c1-16)"
# Each client device keeps its own store-and-forward queue.
QUEUE="${TGW_QUEUE_PATH:-$ROOT/field-queue.redb}"

cat <<EOF

  ╔══════════════════════════════════════════════════════════════╗
  ║  FIELD CLIENT  (capture UI + real UDP sender)                 ║
  ╠══════════════════════════════════════════════════════════════╣
  ║  Capture UI:  http://$IP:$PORT/   (or http://localhost:$PORT/)
  ║  Sending to:  $HOSP:47000   (hospital gateway)
  ║  PSK id:      $KEYHASH   (must match the hospital)
  ╠══════════════════════════════════════════════════════════════╣
  ║  Open the Capture UI, submit vitals, then watch them appear   ║
  ║  on the hospital dashboard (http://$HOSP:8080/).
  ║                                                                ║
  ║  Firewall: allow inbound TCP $PORT here (to open the UI).
  ║  Ctrl-C to stop.                                              ║
  ╚══════════════════════════════════════════════════════════════╝

EOF

# Override the sample config's LAN placeholder with the real hospital IP; bind the UI on
# 0.0.0.0 so it can be opened from this device (or another) on the LAN.
exec env \
  TGW_GATEWAY_ADDR="$HOSP:47000" \
  TGW_LISTEN_ADDR="0.0.0.0:0" \
  TGW_QUEUE_PATH="$QUEUE" \
  RUST_LOG=info \
  "$FIELD" --config config/field.toml serve --http "0.0.0.0:$PORT" --ui-dir field-ui
