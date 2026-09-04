#!/usr/bin/env bash
set -uo pipefail

# doctor.sh — check the development environment is ready to build, test, and
# release this project. Run it right after cloning/stamping.
#
# Exit 0 = everything required is present.
# Exit 1 = a REQUIRED tool or the pre-push hook is missing.
#
# Two tiers:
#   required  — needed to build + run the verify gate (incl. the security
#               gates, which verify.sh runs present-or-warn: doctor makes the
#               expectation explicit so "decorative" never happens silently).
#   release   — needed only to cut a release (reported, not fatal here).

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
rc=0

say()  { printf '%s\n' "$*"; }
ok()   { printf '  \033[0;32mok\033[0m   %s\n' "$*"; }
miss() { printf '  \033[0;31mMISS\033[0m %s — %s\n' "$1" "$2"; }

check() { # check <tool> <install hint> <tier: required|release>
  if command -v "$1" >/dev/null 2>&1; then
    ok "$1"
  else
    miss "$1" "$2"
    [ "$3" = required ] && rc=1
  fi
}

say "phantom doctor — checking your environment"
say ""
say "Required (build + verify gate, including the security gates):"
check cargo    "https://rustup.rs" required
check rustc    "https://rustup.rs" required
[ -d "$ROOT_DIR/swift" ] && check swift "xcode-select --install" required
check jq       "brew install jq" required
check cargo-deny "brew install cargo-deny — advisory/license gate (verify skips it with a warning if absent, which makes it decorative; install it)" required
check gitleaks   "brew install gitleaks — secret scan (same: verify warns-and-skips without it)" required
[ -d "$ROOT_DIR/website" ] && check hugo "brew install hugo" required
command -v bd >/dev/null 2>&1 && ok "bd (beads)" || miss "bd" "optional issue tracker — https://github.com/steveyegge/beads"

say ""
say "Release-only (needed to cut a signed release, not to develop):"
check git-cliff      "brew install git-cliff — changelog generation" release
check cargo-about    "cargo install cargo-about — THIRD-PARTY-NOTICES" release
check cargo-cyclonedx "cargo install cargo-cyclonedx — SBOM" release

say ""
HOOK="$ROOT_DIR/.git/hooks/pre-push"
if [ -f "$HOOK" ] && grep -q "VERIFY GATE" "$HOOK" 2>/dev/null; then
  ok "pre-push verify gate installed"
else
  miss "pre-push hook" "run ./scripts/install-hooks.sh"
  rc=1
fi

say ""
if [ "$rc" -eq 0 ]; then
  say "All required tooling present. You're ready: ./scripts/verify.sh"
else
  say "Some REQUIRED tooling is missing (see MISS above). Install it, then re-run ./scripts/doctor.sh"
fi
exit "$rc"
