#!/usr/bin/env bash
set -u

# Tests for check-ope-version-bump.sh: contract edits require a strictly
# increasing VERSION in the same push. Runs in a sandbox repo.

SCRIPT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
PASS=0
FAIL=0

t() {
  local desc="$1"
  shift
  if "$@" >/dev/null 2>&1; then
    PASS=$((PASS + 1))
  else
    FAIL=$((FAIL + 1))
    echo "  ✗ $desc" >&2
  fi
}

WORK="$(mktemp -d /tmp/phantom-ope-guard.XXXXXX)"
trap 'rm -rf "$WORK"' EXIT
R="$WORK/repo"
mkdir -p "$R/scripts" "$R/open-prompt-edition/kit/03-schema" "$R/tests/fixtures"
cp "$SCRIPT_DIR/check-ope-version-bump.sh" "$R/scripts/"
chmod +x "$R/scripts/check-ope-version-bump.sh"

g() { git -C "$R" -c user.email=t@t -c user.name=t "$@"; }
g init -q -b main
echo "0.1.0" > "$R/open-prompt-edition/VERSION"
echo "CREATE TABLE notes (id TEXT);" > "$R/open-prompt-edition/kit/03-schema/schema.sql"
echo '{"id": "x"}' > "$R/tests/fixtures/note-open.json"
g add -A && g commit -qm base
BASE="$(g rev-parse HEAD)"

# 1. Non-contract change → allowed without a bump.
mkdir -p "$R/open-prompt-edition/kit/10-prompts"
echo "prompt prose" > "$R/open-prompt-edition/kit/10-prompts/00-overview.md"
g add -A && g commit -qm "prose"
t "prose-only change passes without bump" \
  "$R/scripts/check-ope-version-bump.sh" "$BASE" "$(g rev-parse HEAD)"

# 2. Schema change without bump → blocked.
echo "CREATE TABLE notes (id TEXT, title TEXT);" > "$R/open-prompt-edition/kit/03-schema/schema.sql"
g add -A && g commit -qm "schema change, no bump"
t "schema change without bump is blocked" \
  bash -c "! '$R/scripts/check-ope-version-bump.sh' '$BASE' '$(g rev-parse HEAD)'"

# 3. Same change WITH a bump → allowed.
echo "0.2.0" > "$R/open-prompt-edition/VERSION"
g add -A && g commit -qm "bump"
t "schema change with bump passes" \
  "$R/scripts/check-ope-version-bump.sh" "$BASE" "$(g rev-parse HEAD)"

# 4. Downgrade is not a bump.
BASE2="$(g rev-parse HEAD)"
echo "CREATE TABLE notes (id TEXT, title TEXT, body TEXT);" > "$R/open-prompt-edition/kit/03-schema/schema.sql"
echo "0.1.9" > "$R/open-prompt-edition/VERSION"
g add -A && g commit -qm "downgrade"
t "version downgrade is blocked" \
  bash -c "! '$R/scripts/check-ope-version-bump.sh' '$BASE2' '$(g rev-parse HEAD)'"

# 5. Fixture change counts as a contract change.
g reset -q --hard "$BASE2"
echo '{"id": "x", "title": null}' > "$R/tests/fixtures/note-open.json"
g add -A && g commit -qm "fixture change, no bump"
t "fixture change without bump is blocked" \
  bash -c "! '$R/scripts/check-ope-version-bump.sh' '$BASE2' '$(g rev-parse HEAD)'"

# 6. Unresolvable base fails closed.
t "unresolvable base is blocked (fail closed)" \
  bash -c "! '$R/scripts/check-ope-version-bump.sh' deadbeef '$(g rev-parse HEAD)'"

# 7. --worktree mode: uncommitted contract change without bump → blocked;
#    with bump → passes.
echo "CREATE TABLE notes (id TEXT, title TEXT, extra TEXT);" > "$R/open-prompt-edition/kit/03-schema/schema.sql"
t "--worktree blocks uncommitted contract change without bump" \
  bash -c "! (cd '$R' && ./scripts/check-ope-version-bump.sh --worktree)"
echo "0.3.0" > "$R/open-prompt-edition/VERSION"
t "--worktree passes once the working tree carries a bump" \
  bash -c "cd '$R' && ./scripts/check-ope-version-bump.sh --worktree"
g checkout -q -- .

# 8. Pruned OPE layer → guard is a no-op.
g rm -qr open-prompt-edition >/dev/null
g commit -qm "prune ope"
t "pruned OPE layer passes trivially" \
  "$R/scripts/check-ope-version-bump.sh" "$BASE" "$(g rev-parse HEAD)"

echo "test-ope-version-guard: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
