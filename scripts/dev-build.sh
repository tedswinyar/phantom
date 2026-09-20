#!/usr/bin/env bash
set -euo pipefail

# dev-build.sh — fast loop for poking at the real app: debug-build the
# bundle, seed the dev database, and launch the app against the dev profile.

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"

"$ROOT_DIR/scripts/build-app.sh" --debug
"$ROOT_DIR/scripts/seed-dev-db.sh" || true

echo "dev-build.sh: launching build/Phantom.app (dev profile)"
PHANTOM_PROFILE=dev open "$ROOT_DIR/build/Phantom.app"
