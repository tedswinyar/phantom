#!/usr/bin/env bash
set -euo pipefail

# start.sh — run the API server in the dev profile, building if needed.
# The dev profile uses its own database (phantom-dev.db) and port
# (8778), so it never touches prod data.
#
# Usage: ./scripts/start.sh [--prod]

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
PROFILE="dev"
if [ "${1:-}" = "--prod" ]; then
  PROFILE="prod"
fi

BIN="$ROOT_DIR/rust/target/debug/phantom-api"
if [ ! -x "$BIN" ]; then
  echo "start.sh: building phantom-api..."
  (cd "$ROOT_DIR/rust" && cargo build -p phantom-api)
fi

echo "start.sh: starting phantom-api ($PROFILE profile); Ctrl-C to stop"
exec env PHANTOM_PROFILE="$PROFILE" "$BIN"
