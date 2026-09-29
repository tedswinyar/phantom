#!/usr/bin/env bash
set -u

# Tests OF the changelog highlights splice (scripts/lib/release-notes.sh):
# a curated docs/releases/<version>.md lands under its version header and
# nowhere else; versions without one are untouched; regeneration does not
# double it (the changelog is rewritten first, so the splice is idempotent
# by construction — pinned here by running it on a fresh copy twice).

SCRIPT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
# shellcheck source=../lib/release-notes.sh
. "$SCRIPT_DIR/lib/release-notes.sh"

PASS=0
FAIL=0
t() {
  local desc="$1"; shift
  if "$@" >/dev/null 2>&1; then PASS=$((PASS + 1)); else FAIL=$((FAIL + 1)); echo "  ✗ $desc" >&2; fi
}
W="$(mktemp -d /tmp/phantom-notes-test.XXXXXX)"
trap 'rm -rf "$W"' EXIT
mkdir -p "$W/releases"
cat > "$W/CHANGELOG.md" <<'EOF'
# Changelog

All notable changes to Phantom.

## [2.0.0] - 2027-01-01

### Added

- Something new (phantom-xyz)

## [1.9.0] - 2026-12-01

### Fixed

- Something old (phantom-abc)
EOF
printf '# 2.0.0 highlights\n\n**Big.** The headline paragraph.\n\nSecond paragraph.\n' > "$W/releases/2.0.0.md"
cp "$W/CHANGELOG.md" "$W/original.md"

N="$(insert_highlights "$W/CHANGELOG.md" "$W/releases")"
t "reports one enriched version" [ "$N" = 1 ]
t "highlights sit directly under the 2.0.0 header, before its groups" \
  bash -c "awk '/^## \[2.0.0\]/{f=1} f' '$W/CHANGELOG.md' | sed -n '1,6p' | grep -q '^\*\*Big\.\*\*' && awk '/^## \[2.0.0\]/{f=1} f' '$W/CHANGELOG.md' | grep -n '^### Added' | grep -q '^[7-9]:'"
t "the highlights file's H1 is dropped" \
  bash -c "! grep -q '^# 2.0.0 highlights' '$W/CHANGELOG.md'"
t "1.9.0 (no highlights file) is byte-identical" \
  bash -c "diff <(awk '/^## \[1.9.0\]/{f=1} f' '$W/original.md') <(awk '/^## \[1.9.0\]/{f=1} f' '$W/CHANGELOG.md')"
t "every original line survives" \
  bash -c "while IFS= read -r l; do grep -qxF -- \"\$l\" '$W/CHANGELOG.md' || exit 1; done < '$W/original.md'"
cp "$W/original.md" "$W/again.md"
insert_highlights "$W/again.md" "$W/releases" >/dev/null
t "a fresh regeneration + splice reproduces the same file (idempotent by construction)" \
  cmp -s "$W/CHANGELOG.md" "$W/again.md"
# Library functions live in THIS shell: call them here, let `t` judge.
cp "$W/original.md" "$W/none.md"
N0="$(insert_highlights "$W/none.md" "$W/no-such-dir")"
t "no releases dir: zero enriched, changelog untouched" \
  bash -c "[ '$N0' = 0 ] && cmp -s '$W/none.md' '$W/original.md'"
if insert_highlights "$W/nope.md" "$W/releases" >/dev/null 2>&1; then MISSING_RC=0; else MISSING_RC=1; fi
t "missing changelog is an error" [ "$MISSING_RC" = 1 ]

# The real 1.1.0 highlights file exists and has no front matter.
ROOT="$(dirname "$SCRIPT_DIR")"
t "docs/releases/1.1.0.md exists and starts with the H1 the splice drops" \
  bash -c "head -1 '$ROOT/docs/releases/1.1.0.md' | grep -q '^# 1.1.0 highlights'"

echo "test-release-notes: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
