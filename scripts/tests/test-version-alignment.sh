#!/usr/bin/env bash
set -u

# test-version-alignment.sh — tests OF scripts/check-version-alignment.sh against
# scratch trees. The check runs on the real repository in verify.sh's scripts
# suite; these pin what it must catch: any one of the five sources drifting
# (naming the source and both values), a stale CHANGELOG heading, a missing
# plugin version, and that a tree with nothing to compare refuses instead of
# passing (phantom-cnr.6 — rust/Cargo.toml sat at 1.0.0 for two weeks while
# everything else said 1.1.0 and nothing noticed).

SCRIPT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
CHECK="$SCRIPT_DIR/check-version-alignment.sh"
PASS=0
FAIL=0
t() { local d="$1"; shift; if "$@" >/dev/null 2>&1; then PASS=$((PASS+1)); else FAIL=$((FAIL+1)); echo "  ✗ $d" >&2; fi; }
t_fails() { local d="$1"; shift; if "$@" >/dev/null 2>&1; then FAIL=$((FAIL+1)); echo "  ✗ $d (succeeded and should not have)" >&2; else PASS=$((PASS+1)); fi; }
# t_out <desc> <needle> <cmd...>: the command's combined output contains <needle>.
t_out() { local d="$1" n="$2"; shift 2; if "$@" 2>&1 | grep -qF -- "$n"; then PASS=$((PASS+1)); else FAIL=$((FAIL+1)); echo "  ✗ $d (output lacks: $n)" >&2; fi; }

WORK="$(mktemp -d /tmp/phantom-align-test.XXXXXX)"
trap 'rm -rf "$WORK"' EXIT
R="$WORK/repo"

reset_tree() { # <version> [changelog-heading]
  rm -rf "$R"; mkdir -p "$R/swift/Sources/Phantom" "$R/rust" "$R/open-prompt-edition" "$R/.claude-plugin"
  printf 'enum Version {\n    static let marketing = "%s"\n}\n' "$1" > "$R/swift/Sources/Phantom/Version.swift"
  printf '[workspace]\nmembers = []\n\n[workspace.package]\nversion = "%s"\nedition = "2024"\n\n[workspace.dependencies]\nserde = "1"\n' "$1" > "$R/rust/Cargo.toml"
  printf '%s\n' "$1" > "$R/open-prompt-edition/VERSION"
  printf '{"name":"phantom","version":"%s"}\n' "$1" > "$R/.claude-plugin/plugin.json"
  printf '{"name":"phantom","plugins":[{"name":"phantom","version":"%s"}]}\n' "$1" > "$R/.claude-plugin/marketplace.json"
  printf '# Changelog\n\n## [%s] - 2026-01-01\n\n- x\n' "${2:-$1}" > "$R/CHANGELOG.md"
}

# 1. Five sources aligned, CHANGELOG at the version: passes and names it.
reset_tree 1.1.1
t "aligned tree: exit 0" "$CHECK" "$R"
t_out "aligned tree: reports the version and source count" "5 sources agree on 1.1.1" "$CHECK" "$R"

# 2. A cycle in progress: [Unreleased] on top is fine.
reset_tree 1.1.1 Unreleased
t "CHANGELOG [Unreleased] during a cycle: exit 0" "$CHECK" "$R"

# 3. The real 2026-09 defect: Cargo left behind.
reset_tree 1.1.1
sed -i '' 's/version = "1.1.1"/version = "1.0.0"/' "$R/rust/Cargo.toml"
t_fails "rust/Cargo.toml behind: exit 1" "$CHECK" "$R"
t_out "names the lagging source" "rust/Cargo.toml" "$CHECK" "$R"
t_out "shows the lagging value" "1.0.0" "$CHECK" "$R"
t_out "shows the value the others hold" "1.1.1" "$CHECK" "$R"

# 4. Each other source drifting is caught too.
reset_tree 1.1.1; sed -i '' 's/1.1.1/1.1.0/' "$R/swift/Sources/Phantom/Version.swift"
t_fails "Version.swift behind: exit 1" "$CHECK" "$R"
reset_tree 1.1.1; printf '1.2.0\n' > "$R/open-prompt-edition/VERSION"
t_fails "OPE VERSION ahead: exit 1" "$CHECK" "$R"
reset_tree 1.1.1; printf '{"name":"phantom","version":"1.1.0"}\n' > "$R/.claude-plugin/plugin.json"
t_fails "plugin.json behind: exit 1" "$CHECK" "$R"
reset_tree 1.1.1; printf '{"name":"phantom","plugins":[{"name":"phantom"}]}\n' > "$R/.claude-plugin/marketplace.json"
t_fails "marketplace.json without a version: exit 1" "$CHECK" "$R"
t_out "a missing value is shown as <none>" "<none>" "$CHECK" "$R"

# 5. A stale CHANGELOG heading (the previous release) fails once the sources moved on.
reset_tree 1.1.1 1.1.0
t_fails "CHANGELOG still headed [1.1.0] while sources say 1.1.1: exit 1" "$CHECK" "$R"
t_out "names the heading rule" "must be [Unreleased] or [1.1.1]" "$CHECK" "$R"

# 6. Fewer sources (a pruned stamp) still compare what exists; one source alone refuses.
reset_tree 1.1.1; rm -f "$R/.claude-plugin/plugin.json" "$R/.claude-plugin/marketplace.json" "$R/swift/Sources/Phantom/Version.swift"
t "two sources present and equal: exit 0" "$CHECK" "$R"
rm -f "$R/rust/Cargo.toml"
t_fails "one source alone: refuses (exit 2), never a vacuous pass" "$CHECK" "$R"
t "the refusal is exit 2, distinct from a mismatch" bash -c '"$1" "$2" >/dev/null 2>&1; [ $? -eq 2 ]' _ "$CHECK" "$R"

echo "test-version-alignment: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
