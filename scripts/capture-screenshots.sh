#!/usr/bin/env bash
set -euo pipefail

# capture-screenshots.sh — launch the built app against a seeded throwaway
# API and capture the main window into website/static/screenshots/ (or a
# given output dir). Screenshots feed the website and release notes.
#
# Usage: ./scripts/capture-screenshots.sh [output-dir]

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"

# shellcheck source=lib/completion-guard.sh
. "$ROOT_DIR/scripts/lib/completion-guard.sh"
OUT_DIR="${1:-$ROOT_DIR/website/static/screenshots}"
APP="$ROOT_DIR/build/Phantom.app/Contents/MacOS/Phantom"
BIN="$ROOT_DIR/rust/target/release"
WORK="$(mktemp -d /tmp/phantom-shots.XXXXXX)"
API_PID=""
APP_PID=""

cleanup() {
  [ -n "$APP_PID" ] && kill "$APP_PID" 2>/dev/null || true
  [ -n "$API_PID" ] && kill "$API_PID" 2>/dev/null || true
  wait 2>/dev/null || true
  rm -rf "$WORK"
}
# On macOS bash 3.2 an EXIT trap turns a set -u abort into exit 0, so this script
# could crash and report success. `finish` is available from the source line above;
# the guard is armed here, where the trap used to be. See scripts/lib/completion-guard.sh.
arm_completion_guard "capture-screenshots" cleanup

[ -x "$APP" ] || { echo "capture-screenshots: build the app first (make app)" >&2; finish 1; }
mkdir -p "$OUT_DIR"

# Seeded throwaway API on an ephemeral port.
PHANTOM_PROFILE=test PHANTOM_DB_PATH="$WORK/phantom.db" PHANTOM_PORT=0 \
PHANTOM_KEY_FILE="$WORK/api_key" "$BIN/phantom-api" >"$WORK/api.out" 2>/dev/null &
API_PID=$!
BASE=""
for _ in $(seq 1 50); do
  BASE="$(sed -n 's/.*listening on //p' "$WORK/api.out")"
  [ -n "$BASE" ] && break
  sleep 0.1
done
[ -n "$BASE" ] || { echo "capture-screenshots: API never announced" >&2; finish 1; }

export PHANTOM_API_URL="$BASE" PHANTOM_KEY_FILE="$WORK/api_key"
# A completed scan of this checkout gives the sidebar, treemap, and file
# list real content (build trees make genuine classifier hotspots).
"$BIN/phantom" scan "$ROOT_DIR" >/dev/null

PHANTOM_API_KEY="$(cat "$WORK/api_key")" PHANTOM_API_URL="$BASE" "$APP" >/dev/null 2>&1 &
APP_PID=$!
sleep 4

WID="$(swift -e '
import Cocoa
let windows = CGWindowListCopyWindowInfo(.optionOnScreenOnly, kCGNullWindowID) as? [[String: Any]] ?? []
for w in windows {
    let owner = w["kCGWindowOwnerName"] as? String ?? ""
    if owner == "Phantom" {
        if let id = w["kCGWindowNumber"] as? Int, id > 0 { print(id); break }
    }
}' 2>/dev/null)"
[ -n "$WID" ] || { echo "capture-screenshots: no app window found" >&2; finish 1; }

screencapture -l "$WID" -o "$OUT_DIR/01-main.png"
echo "capture-screenshots: wrote $OUT_DIR/01-main.png"
finish 0
