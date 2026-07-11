#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────
# demo/setup.sh — Set up netem + TBF for the demo
#
# Usage:  sudo demo/setup.sh <interface> [loss%] [delay_ms] [jitter_ms] [rate_kbit]
#
# Defaults (matching docs/DEMO.md):
#   loss%       25
#   delay_ms    120
#   jitter_ms   40
#   rate_kbit   64
#
# Creates:
#   handle 1: netem loss <N>% delay <D>ms <J>ms
#   handle 2: tbf rate <R>kbit burst 8kb latency 400ms
#
# Based on: docs/DEMO.md §Setup
# ─────────────────────────────────────────────────────────────
set -euo pipefail

usage() {
  echo "Usage: sudo $0 <interface> [loss%] [delay_ms] [jitter_ms] [rate_kbit]"
  echo ""
  echo "  interface   Network interface (required, e.g. eth0 or lo)"
  echo "  loss%       Packet loss percentage  (default: 25)"
  echo "  delay_ms    Base delay in ms        (default: 120)"
  echo "  jitter_ms   Delay jitter in ms      (default: 40)"
  echo "  rate_kbit   Rate cap in kbit/s      (default: 64)"
  echo ""
  echo "Example:  sudo $0 eth0 25 120 40 64"
  echo "Teardown: sudo demo/teardown.sh <interface>"
  exit 1
}

# ── Argument validation ──────────────────────────────────────
if [[ $# -lt 1 ]]; then
  echo "ERROR: Missing required argument <interface>"
  usage
fi

IFACE="$1"
LOSS="${2:-25}"
DELAY="${3:-120}"
JITTER="${4:-40}"
RATE="${5:-64}"

# Validate interface exists
if ! ip link show "$IFACE" &>/dev/null; then
  echo "ERROR: Network interface '$IFACE' not found."
  echo "Available interfaces:"
  ip -br link show | awk '{print "  " $1}'
  exit 1
fi

# Validate numeric args
for arg_name in LOSS DELAY JITTER RATE; do
  val="${!arg_name}"
  if ! [[ "$val" =~ ^[0-9]+$ ]]; then
    echo "ERROR: $arg_name must be a non-negative integer, got '$val'"
    usage
  fi
done

if (( LOSS > 100 )); then
  echo "ERROR: loss% must be between 0 and 100, got '$LOSS'"
  usage
fi

# ── Safety: remove existing qdisc if present ─────────────────
echo "Cleaning any existing qdisc on $IFACE..."
sudo tc qdisc del dev "$IFACE" root 2>/dev/null || true

# ── Set up netem (root qdisc) ────────────────────────────────
echo "Setting up netem: loss ${LOSS}%, delay ${DELAY}ms ±${JITTER}ms"
sudo tc qdisc add dev "$IFACE" root handle 1: netem loss "${LOSS}%" delay "${DELAY}ms" "${JITTER}ms"

# ── Set up TBF (child qdisc for rate limiting) ───────────────
echo "Setting up TBF: rate ${RATE}kbit, burst 8kb, latency 400ms"
sudo tc qdisc add dev "$IFACE" parent 1: handle 2: tbf rate "${RATE}kbit" burst 8kb latency 400ms

# ── Report ───────────────────────────────────────────────────
echo ""
echo "╔══════════════════════════════════════════════════════╗"
echo "║  Demo Network Degradation Active                    ║"
echo "╠══════════════════════════════════════════════════════╣"
printf "║  Interface:  %-39s║\n" "$IFACE"
printf "║  Loss:       %-39s║\n" "${LOSS}%"
printf "║  Delay:      %-39s║\n" "${DELAY}ms ±${JITTER}ms"
printf "║  Rate cap:   %-39s║\n" "${RATE} kbit/s"
echo "║                                                      ║"
echo "║  Run demo/knob.sh <loss%> to change loss live         ║"
echo "║  Run demo/teardown.sh $IFACE to clean up       ║"
echo "╚══════════════════════════════════════════════════════╝"
echo ""
echo "Current qdisc configuration:"
tc qdisc show dev "$IFACE"
