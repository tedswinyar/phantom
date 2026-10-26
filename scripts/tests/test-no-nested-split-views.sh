#!/usr/bin/env bash
set -u

# Source guard for phantom-xb1 (GitHub tedswinyar/phantom#1): linked against
# the macOS 26 SDK and run on macOS 27, a VSplitView nested in an HSplitView
# (and `.inspector(isPresented:)` in its place) looped in windows under
# ~560pt tall — NSGenericException "more Update Constraints in Window passes
# than there are views" on every launch once a scan showed. The fix put the
# inspector beside the VSplitView in a plain HStack. The build server runs
# macOS 26 and cannot reproduce the loop, so the app's sources must not grow
# an HSplitView or an `.inspector(` again without this test being revisited
# on a macOS 27 machine. Comment lines are allowed to name them.

SCRIPT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
APP_SOURCES="$SCRIPT_DIR/../swift/Sources/Phantom"

PASS=0
FAIL=0

# Lines mentioning the pattern whose first non-blank characters are not `//`.
code_hits() {
  grep -rnE "$1" "$APP_SOURCES" --include='*.swift' | grep -vE '^[^:]+:[0-9]+:[[:space:]]*//'
}

check_absent() {
  local label="$1" pattern="$2" hits
  hits="$(code_hits "$pattern")"
  if [ -z "$hits" ]; then
    PASS=$((PASS + 1))
  else
    FAIL=$((FAIL + 1))
    echo "  ✗ $label is back in the app sources (phantom-xb1 layout loop on macOS 27):" >&2
    echo "$hits" | sed 's/^/      /' >&2
  fi
}

check_absent "HSplitView" '\bHSplitView\b'
check_absent ".inspector(isPresented:)" '\.inspector\('

# The guard must be able to see code at all — a moved source tree would
# otherwise pass vacuously.
if code_hits '\bVSplitView\b' | grep -q 'ContentView.swift'; then
  PASS=$((PASS + 1))
else
  FAIL=$((FAIL + 1))
  echo "  ✗ expected the VSplitView in ContentView.swift under $APP_SOURCES; the guard is not looking at the app" >&2
fi

echo "test-no-nested-split-views: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
