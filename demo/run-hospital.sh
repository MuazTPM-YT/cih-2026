#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
# demo/run-hospital.sh — spin up the HOSPITAL side on THIS device.
#
# Starts the gateway: UDP receiver (:47000) + live dashboard/API (:8080, serving
# hospital-ui/). The field/client device sends here over the LAN.
#
# Usage:  demo/run-hospital.sh [--reset]
#           --reset   wipe the stored observations (gateway.redb) before starting
#
# The field/client device must (1) share the SAME keys/device-a.key and
# (2) point at this device's IP — both are printed below.
# ─────────────────────────────────────────────────────────────────────────────
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

RESET=0
[ "${1:-}" = "--reset" ] && RESET=1

GW="target/release/tgw-gateway"
FIELD="target/release/tgw-field"

# Build only what's missing.
[ -x "$GW" ]    || { echo "building tgw-gateway (release)…"; cargo build --release -p tgw-gateway; }
[ -x "$FIELD" ] || { echo "building tgw-field (release, for keygen)…"; cargo build --release -p tgw-field; }

# Shared pre-shared key: generate once; the CLIENT device needs the SAME file.
if [ ! -f keys/device-a.key ]; then
  mkdir -p keys
  "$FIELD" keygen --out keys/device-a.key
  echo "generated a new PSK — copy keys/device-a.key to the client device (same path)."
fi

[ "$RESET" = 1 ] && { rm -f gateway.redb; echo "store reset: gateway.redb removed."; }

lan_ip() { ip -4 route get 1 2>/dev/null | awk '{print $7; exit}' || hostname -I 2>/dev/null | awk '{print $1}'; }
IP="$(lan_ip)"; IP="${IP:-<this-device-ip>}"
KEYHASH="$(sha256sum keys/device-a.key | cut -c1-16)"

cat <<EOF

  ╔══════════════════════════════════════════════════════════════╗
  ║  HOSPITAL SERVER  (gateway + dashboard)                       ║
  ╠══════════════════════════════════════════════════════════════╣
  ║  Dashboard:   http://$IP:8080/
  ║  Gateway UDP: $IP:47000   (field → here)
  ║  PSK id:      $KEYHASH   (must match the client)
  ╠══════════════════════════════════════════════════════════════╣
  ║  On the CLIENT device:                                        ║
  ║    1. copy  keys/device-a.key  to the same repo path there    ║
  ║    2. run   demo/run-client.sh $IP
  ║                                                                ║
  ║  Firewall: allow inbound TCP 8080 and UDP 47000 here.         ║
  ║  Ctrl-C to stop.                                              ║
  ╚══════════════════════════════════════════════════════════════╝

EOF

# gateway.toml already binds 0.0.0.0 for both UDP and HTTP, so it is LAN-reachable.
exec env RUST_LOG=info "$GW" --config config/gateway.toml --static-dir hospital-ui
