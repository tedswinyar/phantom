#!/usr/bin/env bash
set -euo pipefail

# seed-dev-db.sh — populate the dev database with a real scan so the app
# has something to show. Boots a dev-profile API if one is not already
# running, seeds through the public API (never writes the DB directly),
# and leaves the server in whatever state it found it.

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"

# shellcheck source=lib/completion-guard.sh
. "$ROOT_DIR/scripts/lib/completion-guard.sh"
BIN="$ROOT_DIR/rust/target/debug"
DEV_URL="${PHANTOM_API_URL:-http://127.0.0.1:8778}"

if [ ! -x "$BIN/phantom" ]; then
  echo "seed-dev-db.sh: building..."
  (cd "$ROOT_DIR/rust" && cargo build --workspace)
fi

STARTED_PID=""
cleanup() {
  if [ -n "$STARTED_PID" ]; then
    kill "$STARTED_PID" 2>/dev/null || true
    wait "$STARTED_PID" 2>/dev/null || true
  fi
}
# On macOS bash 3.2 an EXIT trap turns a set -u abort into exit 0, so this script
# could crash and report success. `finish` is available from the source line above;
# the guard is armed here, where the trap used to be. See scripts/lib/completion-guard.sh.
arm_completion_guard "seed-dev-db" cleanup

if ! curl -sf "$DEV_URL/health" >/dev/null 2>&1; then
  echo "seed-dev-db.sh: starting a dev API..."
  PHANTOM_PROFILE=dev "$BIN/phantom-api" >/dev/null 2>&1 &
  STARTED_PID=$!
  for _ in $(seq 1 30); do
    curl -sf "$DEV_URL/health" >/dev/null 2>&1 && break
    sleep 0.2
  done
fi

export PHANTOM_API_URL="$DEV_URL"
# Scan this checkout: real directory structure, and its build trees give the
# classifier genuine regenerable-artifact hotspots to show off.
echo "seed-dev-db.sh: scanning $ROOT_DIR (waits for completion)..."
"$BIN/phantom" scan "$ROOT_DIR" >/dev/null

echo "seed-dev-db.sh: seeded the dev profile:"
"$BIN/phantom" scans list
finish 0
