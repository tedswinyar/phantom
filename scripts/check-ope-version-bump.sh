#!/usr/bin/env bash
# check-ope-version-bump.sh — enforce the OPE contract-versioning rule.
#
#   Usage: ./scripts/check-ope-version-bump.sh <base-sha> <head-sha>
#          ./scripts/check-ope-version-bump.sh --worktree
#
# --worktree checks the WORKING TREE against HEAD, so an agent can confirm
# its VERSION bump satisfies the gate before committing anything.
#
# THE RULE (inherited from specter-ehn8): OPE is the interop contract that
# independent implementations must match, so "the app leads, OPE follows" is
# allowed — but the follow must be EXPLICIT AND VERSIONED, never implied by a
# passing build. If a push changes an OPE contract surface, it must also bump
# open-prompt-edition/VERSION in the same push, and the bump must strictly
# increase (a downgrade is not a bump).
#
# SCOPE IS DELIBERATELY NARROW: only the contract surfaces trigger this —
# kit/02-contracts/, kit/03-schema/, kit/06-interchange/, and the shared
# fixtures in tests/fixtures/. Prose (prompts, audit templates, validation
# checklists) can be edited freely; a gate that fires on typos is a gate
# someone disables.
#
# Exit 0 = allowed. Exit 1 = blocked.

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VERSION_FILE="open-prompt-edition/VERSION"
CONTRACT_PATHS=(
  "open-prompt-edition/kit/02-contracts"
  "open-prompt-edition/kit/03-schema"
  "open-prompt-edition/kit/06-interchange"
  "tests/fixtures"
)

RED=$'\033[0;31m'; GREEN=$'\033[0;32m'; NC=$'\033[0m'
fail_msg() { printf "%s[✗]%s %s\n" "$RED" "$NC" "$*" >&2; }
ok_msg()   { printf "%s[✓]%s %s\n" "$GREEN" "$NC" "$*" >&2; }

WORKTREE=0
if [ "${1:-}" = "--worktree" ]; then
    WORKTREE=1
    BASE="HEAD"
    HEAD_SHA=""   # unused in worktree mode
elif [ $# -eq 2 ]; then
    BASE="$1"
    HEAD_SHA="$2"
else
    fail_msg "usage: $(basename "$0") <base-sha> <head-sha> | --worktree"
    exit 1
fi

cd "$REPO_ROOT" || exit 1

# No OPE layer (pruned project) = nothing to guard.
if [ ! -d open-prompt-edition ]; then
    exit 0
fi

if [ "$WORKTREE" = 1 ]; then
    changed=$(git diff --name-only HEAD -- "${CONTRACT_PATHS[@]}" 2>/dev/null)
    if [ -z "$changed" ]; then
        ok_msg "ope-version: no contract changes in the working tree"
        exit 0
    fi
    old_version=$(git show "HEAD:$VERSION_FILE" 2>/dev/null | tr -d '[:space:]')
    new_version=$(tr -d '[:space:]' < "$VERSION_FILE" 2>/dev/null)
    if [ -z "$new_version" ]; then
        fail_msg "contract files changed but $VERSION_FILE is missing/empty in the working tree."
        exit 1
    fi
    if [ -n "$old_version" ]; then
        highest=$(printf '%s\n%s\n' "$old_version" "$new_version" | sort -V | tail -1)
        if [ "$new_version" = "$old_version" ] || [ "$highest" != "$new_version" ]; then
            fail_msg "working tree changes contract files but VERSION is v${new_version} (HEAD: v${old_version}) — bump it before committing."
            fail_msg "Changed:"
            printf '%s\n' "$changed" | sed 's/^/    /' >&2
            exit 1
        fi
    fi
    ok_msg "ope-version: working-tree contract changes carry a bump to v${new_version}"
    exit 0
fi

# A base we cannot resolve means the rule CANNOT BE CHECKED — and a gate that
# cannot run must block, not print a green check and pass.
if ! git rev-parse --verify --quiet "$BASE^{commit}" >/dev/null; then
    fail_msg "ope-version: base '$BASE' is not a resolvable commit, so the"
    fail_msg "contract-version rule cannot be checked. Blocking rather than"
    fail_msg "passing silently as though checked."
    fail_msg "Fix: git fetch origin main (or pass a resolvable base)."
    exit 1
fi

changed=$(git diff --name-only "$BASE" "$HEAD_SHA" -- "${CONTRACT_PATHS[@]}" 2>/dev/null)
if [ -z "$changed" ]; then
    exit 0
fi

old_version=$(git show "$BASE:$VERSION_FILE" 2>/dev/null | tr -d '[:space:]')
new_version=$(git show "$HEAD_SHA:$VERSION_FILE" 2>/dev/null | tr -d '[:space:]')

if [ -z "$new_version" ]; then
    fail_msg "OPE contract files changed but $VERSION_FILE is missing at $HEAD_SHA. Push blocked."
    exit 1
fi

if [ -n "$old_version" ]; then
    highest=$(printf '%s\n%s\n' "$old_version" "$new_version" | sort -V | tail -1)
    if [ "$new_version" = "$old_version" ] || [ "$highest" != "$new_version" ]; then
        fail_msg "OPE contract files changed and the version went from v${old_version:-none} to v${new_version} — that is not a bump. Push blocked."
        fail_msg ""
        fail_msg "Changed contract surfaces:"
        printf '%s\n' "$changed" | sed 's/^/    /' >&2
        fail_msg ""
        fail_msg "OPE is the interop contract, so a change to it must be explicit and"
        fail_msg "versioned. Bump $VERSION_FILE (strictly increasing) in this push."
        fail_msg ""
        fail_msg "If the edit is NOT a contract change (a typo, a reworded description),"
        fail_msg "it still needs a bump or it needs to not be in this push — there is no"
        fail_msg "third option that leaves the contract honest."
        exit 1
    fi
fi

ok_msg "ope-version: contract files changed and VERSION was bumped to v${new_version}"
exit 0
