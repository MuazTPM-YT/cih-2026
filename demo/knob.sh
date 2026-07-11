#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────
# demo/knob.sh — Turn the packet-loss dial during the demo
#
# Usage:  sudo demo/knob.sh <loss%> [interface]
#   loss%      Integer 0–100 (packet loss percentage)
#   interface  Network interface (default: eth0)
#
# Wraps: tc qdisc change dev <iface> root handle 1: netem loss <N>%
# The qdisc must already exist (run demo/setup.sh first).
# ─────────────────────────────────────────────────────────────
set -euo pipefail

usage() {
  echo "Usage: sudo $0 <loss%> [interface]"
  echo ""
  echo "  loss%      Packet loss percentage (integer 0–100)"
  echo "  interface  Network interface (default: eth0)"
  echo ""
  echo "Example:  sudo $0 40 eth0"
  echo ""
  echo "Requires: setup.sh must have been run first to create the qdisc."
  exit 1
}

# ── Argument validation ──────────────────────────────────────
if [[ $# -lt 1 ]]; then
  echo "ERROR: Missing required argument <loss%>"
  usage
fi

LOSS="$1"
IFACE="${2:-eth0}"

# Validate loss is an integer 0–100
if ! [[ "$LOSS" =~ ^[0-9]+$ ]]; then
  echo "ERROR: loss% must be an integer, got '$LOSS'"
  usage
fi

if (( LOSS < 0 || LOSS > 100 )); then
  echo "ERROR: loss% must be between 0 and 100, got '$LOSS'"
  usage
fi

# Validate interface exists
if ! ip link show "$IFACE" &>/dev/null; then
  echo "ERROR: Network interface '$IFACE' not found."
  echo "Available interfaces:"
  ip -br link show | awk '{print "  " $1}'
  exit 1
fi

# ── Apply the change ─────────────────────────────────────────
echo "╔══════════════════════════════════════════╗"
echo "║  Changing packet loss on $IFACE"
echo "╚══════════════════════════════════════════╝"
echo ""

sudo tc qdisc change dev "$IFACE" root handle 1: netem loss "${LOSS}%"

echo ""
echo "┌──────────────────────────────────────────┐"
echo "│                                          │"
printf "│     📡  PACKET LOSS:  %-3s%%               │\n" "$LOSS"
echo "│     Interface: $IFACE"
echo "│                                          │"
echo "└──────────────────────────────────────────┘"
echo ""
echo "Current qdisc configuration:"
tc qdisc show dev "$IFACE"
