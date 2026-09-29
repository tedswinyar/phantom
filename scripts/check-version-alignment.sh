#!/usr/bin/env bash
set -euo pipefail

# check-version-alignment.sh [root] — every source of "what version is this?"
# must agree, on every push, not only on release day.
#
#   swift/Sources/Phantom/Version.swift      the app's marketing version
#   rust/Cargo.toml [workspace.package]      GET /health, --version, MCP serverInfo
#   open-prompt-edition/VERSION              the contract version
#   .claude-plugin/plugin.json               the plugin marketplace only updates
#   .claude-plugin/marketplace.json            when these move
#   CHANGELOG.md top heading                 "[Unreleased]" during a cycle, or
#                                            exactly the version at/after release
#
# VERSIONING.md: they move TOGETHER at the START of a release cycle, to the
# version being assembled; release.sh's equality assert stays as the last line.
# Until 2026-09-20 nothing on the push path checked this and rust/Cargo.toml sat
# at 1.0.0 for two weeks of 1.1 development — every binary and GET /health lied
# about what it was (phantom-cnr.6). Runs in verify.sh's scripts suite; tested by
# scripts/tests/test-version-alignment.sh against scratch trees.
#
# Exit 0: aligned (prints the version). Exit 1: names every source and its
# value. Exit 2: fewer than two sources present (nothing to compare — a pruned
# template stamp must fail loudly rather than pass vacuously).

ROOT="${1:-$(cd "$(dirname "$0")/.." && pwd)}"
cd "$ROOT"

NAMES=""
VALUES=""
add() { NAMES="$NAMES$1"$'\n'; VALUES="$VALUES$2"$'\n'; }

[ -f swift/Sources/Phantom/Version.swift ] \
  && add "swift/Sources/Phantom/Version.swift" \
         "$(sed -n 's/.*static let marketing = "\(.*\)"/\1/p' swift/Sources/Phantom/Version.swift | head -1)"
[ -f rust/Cargo.toml ] \
  && add "rust/Cargo.toml" \
         "$(sed -n '/^\[workspace.package\]/,/^\[/{s/^version = "\(.*\)"/\1/p;}' rust/Cargo.toml | head -1)"
[ -f open-prompt-edition/VERSION ] \
  && add "open-prompt-edition/VERSION" "$(tr -d '[:space:]' < open-prompt-edition/VERSION)"
if [ -f .claude-plugin/plugin.json ]; then
  command -v jq >/dev/null || { echo "check-version-alignment: jq is required to read .claude-plugin/*.json" >&2; exit 2; }
  add ".claude-plugin/plugin.json" "$(jq -r '.version // empty' .claude-plugin/plugin.json)"
fi
if [ -f .claude-plugin/marketplace.json ]; then
  command -v jq >/dev/null || { echo "check-version-alignment: jq is required to read .claude-plugin/*.json" >&2; exit 2; }
  add ".claude-plugin/marketplace.json" "$(jq -r '.plugins[0].version // empty' .claude-plugin/marketplace.json)"
fi

COUNT="$(printf '%s' "$NAMES" | grep -c . || true)"
if [ "$COUNT" -lt 2 ]; then
  echo "check-version-alignment: only $COUNT version source(s) present under $ROOT — nothing to compare" >&2
  exit 2
fi

FIRST="$(printf '%s' "$VALUES" | grep -m1 . || true)"
DISTINCT="$(printf '%s' "$VALUES" | sort -u | grep -c . || true)"
# A source that is present but carries NO version (a manifest without the key)
# is a disagreement, not a bystander: the marketplace would never update.
EMPTY="$(printf '%s' "$VALUES" | grep -c '^$' || true)"
HEADING=""
HEADING_OK=1
if [ -f CHANGELOG.md ]; then
  HEADING="$(sed -n 's/^## \[\([^]]*\)\].*/\1/p' CHANGELOG.md | head -1)"
  case "$HEADING" in
    Unreleased|"$FIRST") ;;
    *) HEADING_OK=0 ;;
  esac
fi

if [ "$DISTINCT" -eq 1 ] && [ "$EMPTY" -eq 0 ] && [ -n "$FIRST" ] && [ "$HEADING_OK" -eq 1 ]; then
  echo "check-version-alignment: $COUNT sources agree on $FIRST${HEADING:+ (CHANGELOG: [$HEADING])}"
  exit 0
fi

echo "check-version-alignment: FAILED — the version sources disagree:" >&2
paste -d'\t' <(printf '%s' "$NAMES") <(printf '%s' "$VALUES") | awk -F'\t' '{ printf "  %-40s %s\n", $1, ($2 == "" ? "<none>" : $2) }' >&2
if [ -n "$HEADING" ]; then
  printf '  %-40s [%s]%s\n' "CHANGELOG.md top heading" "$HEADING" \
    "$([ "$HEADING_OK" -eq 1 ] || printf ' — must be [Unreleased] or [%s]' "$FIRST")" >&2
fi
echo "check-version-alignment: move them together (VERSIONING.md, 'Version Alignment')" >&2
exit 1
