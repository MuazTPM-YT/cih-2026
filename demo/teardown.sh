#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────
# demo/teardown.sh — Remove demo network degradation
#
# Usage:  sudo demo/teardown.sh <interface>
#
# Safe to run multiple times; safe to run after a partial setup.
# Removes any root qdisc on the interface, restoring normal
# network behaviour.
# ─────────────────────────────────────────────────────────────
set -euo pipefail

usage() {
  echo "Usage: sudo $0 <interface>"
  echo ""
  echo "  interface   Network interface to clean up (e.g. eth0 or lo)"
  echo ""
  echo "Safe to run after a partial setup or multiple times."
  exit 1
}

# ── Argument validation ──────────────────────────────────────
if [[ $# -lt 1 ]]; then
  echo "ERROR: Missing required argument <interface>"
  usage
fi

IFACE="$1"

# Validate interface exists
if ! ip link show "$IFACE" &>/dev/null; then
  echo "ERROR: Network interface '$IFACE' not found."
  echo "Available interfaces:"
  ip -br link show | awk '{print "  " $1}'
  exit 1
fi

# ── Remove qdisc (safe to run when none exists) ──────────────
echo "Removing qdisc on $IFACE..."
if sudo tc qdisc del dev "$IFACE" root 2>/dev/null; then
  echo "✓ Qdisc removed from $IFACE"
else
  echo "✓ No qdisc to remove on $IFACE (already clean)"
fi

echo ""
echo "╔══════════════════════════════════════════╗"
echo "║  Network degradation removed             ║"
echo "║  Interface: $IFACE is now normal   ║"
echo "╚══════════════════════════════════════════╝"
echo ""
echo "Current qdisc configuration:"
tc qdisc show dev "$IFACE"
