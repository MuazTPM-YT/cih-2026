#!/usr/bin/env bash
# Serves field-ui/ and hospital-ui/ from one origin so their shared
# localStorage/BroadcastChannel bridge (shared/store.js) actually works.
# Usage: demo/serve-local-ui.sh [port]
set -euo pipefail

PORT="${1:-8090}"
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

echo "Serving ${ROOT_DIR} on http://localhost:${PORT}"
echo "  Field capture app:      http://localhost:${PORT}/field-ui/"
echo "  Hospital dashboard:     http://localhost:${PORT}/hospital-ui/"
echo "Both must be opened as http://localhost:${PORT}/... (same origin) for live sync to work."
echo "Press Ctrl-C to stop."

cd "${ROOT_DIR}"
exec python3 -m http.server "${PORT}"
