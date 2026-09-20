#!/usr/bin/env bash
set -u

# Table-driven tests for the name-form derivation used by init.sh.

SCRIPT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
# shellcheck source=../lib/names.sh
. "$SCRIPT_DIR/lib/names.sh"

PASS=0
FAIL=0

expect() {
  # expect <input> <slug> <snake> <pascal> <compact> <screaming>
  local input="$1" slug="$2" snake="$3" pascal="$4" compact="$5" screaming="$6"
  if ! phantom_derive_names "$input" 2>/dev/null; then
    FAIL=$((FAIL + 1))
    echo "  ✗ '$input' unexpectedly rejected" >&2
    return
  fi
  local ok=1
  [ "$NAME_SLUG" = "$slug" ] || { ok=0; echo "  ✗ '$input' slug: $NAME_SLUG != $slug" >&2; }
  [ "$NAME_SNAKE" = "$snake" ] || { ok=0; echo "  ✗ '$input' snake: $NAME_SNAKE != $snake" >&2; }
  [ "$NAME_PASCAL" = "$pascal" ] || { ok=0; echo "  ✗ '$input' pascal: $NAME_PASCAL != $pascal" >&2; }
  [ "$NAME_COMPACT" = "$compact" ] || { ok=0; echo "  ✗ '$input' compact: $NAME_COMPACT != $compact" >&2; }
  [ "$NAME_SCREAMING" = "$screaming" ] || { ok=0; echo "  ✗ '$input' screaming: $NAME_SCREAMING != $screaming" >&2; }
  if [ "$ok" = 1 ]; then PASS=$((PASS + 1)); else FAIL=$((FAIL + 1)); fi
}

reject() {
  local input="$1"
  if phantom_derive_names "$input" 2>/dev/null; then
    FAIL=$((FAIL + 1))
    echo "  ✗ '$input' should have been rejected" >&2
  else
    PASS=$((PASS + 1))
  fi
}

# Fixture names deliberately avoid the sentinel AND plausible stamp names —
# in a STAMPED repo the project's own name is (correctly) rejected as a
# collision, so a fixture that matched a real stamp name would flip this
# suite red (learned when test-init stamped "Ghost Note" and this table
# used the same name).
#      input                slug               snake              pascal            compact           screaming
expect "Wobbly Cauldron"    "wobbly-cauldron"  "wobbly_cauldron"  "WobblyCauldron"  "wobblycauldron"  "WOBBLY_CAULDRON"
expect "wobbly cauldron"    "wobbly-cauldron"  "wobbly_cauldron"  "WobblyCauldron"  "wobblycauldron"  "WOBBLY_CAULDRON"
expect "wobbly-cauldron"    "wobbly-cauldron"  "wobbly_cauldron"  "WobblyCauldron"  "wobblycauldron"  "WOBBLY_CAULDRON"
expect "WOBBLY CAULDRON"    "wobbly-cauldron"  "wobbly_cauldron"  "WobblyCauldron"  "wobblycauldron"  "WOBBLY_CAULDRON"
expect "Zanzibar"           "zanzibar"         "zanzibar"         "Zanzibar"        "zanzibar"        "ZANZIBAR"
expect "My Cool App 2"      "my-cool-app-2"    "my_cool_app_2"    "MyCoolApp2"      "mycoolapp2"      "MY_COOL_APP_2"
expect "a  b"               "a-b"              "a_b"              "AB"              "ab"              "A_B"

# The port derivation is stable and in range.
phantom_derive_names "Wobbly Cauldron" 2>/dev/null
P1="$NAME_DEFAULT_PORT"
phantom_derive_names "Wobbly Cauldron" 2>/dev/null
P2="$NAME_DEFAULT_PORT"
if [ "$P1" = "$P2" ] && [ "$P1" -ge 18000 ] && [ "$P1" -le 18999 ]; then
  PASS=$((PASS + 1))
else
  FAIL=$((FAIL + 1))
  echo "  ✗ port derivation unstable or out of range: $P1 vs $P2" >&2
fi

reject ""
reject "   "
reject "9lives"            # must start with a letter
reject "app!"              # punctuation
reject "café"              # non-ASCII
reject "Phantom"        # collides with the sentinel
reject "spooky-shell"      # collides with the template repo

echo "test-names: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
