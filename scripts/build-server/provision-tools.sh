#!/usr/bin/env bash
set -euo pipefail
# provision-tools.sh — as the build user: the toolchain a release needs, then
# doctor. Homebrew formulae are installed by the admin (brew is owned by the admin
# account); cargo tools and rustup are per-user and installed here.
export PATH="/opt/homebrew/bin:$HOME/.cargo/bin:/usr/local/bin:$PATH"
ROOT_DIR="$(cd "$(dirname "$0")/../.." && pwd)"
info() { printf 'provision-tools: %s\n' "$*"; }

if ! command -v rustup >/dev/null 2>&1; then
  info "installing rustup (non-interactive)"
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path --profile minimal
  # shellcheck source=/dev/null
  . "$HOME/.cargo/env"
fi
(cd "$ROOT_DIR/rust" && rustup show active-toolchain) || true
# cargo-about ships its binary behind the `cli` feature; a bare `cargo install
# cargo-about` compiles for a minute and installs nothing ("none of the package's
# binaries are available for install"). Seen on Banshee's build server, 2026-09-13.
command -v cargo-about >/dev/null 2>&1 || { info "cargo install cargo-about"; cargo install --locked cargo-about --features cli; }
command -v cargo-cyclonedx >/dev/null 2>&1 || { info "cargo install cargo-cyclonedx"; cargo install --locked cargo-cyclonedx; }

# verify.sh under CI=1 is fail-closed on every gate tool (docs/ci.md), and the
# website suite needs hugo — so the build user needs all of them, not just the
# release-only ones.
missing=""
for t in gh git-cliff cargo-deny gitleaks jq hugo; do
  command -v "$t" >/dev/null 2>&1 || missing="$missing $t"
done
if [ -n "$missing" ]; then
  cat >&2 <<MSG
provision-tools: Homebrew formulae missing:$missing
  As the admin user on this machine:  brew install$missing
MSG
  exit 1
fi
# The identity the release job commits notes and tags under: PHANTOM_GIT_NAME /
# PHANTOM_GIT_EMAIL, defaulting to the repository owner and a GitHub noreply address
# (GitHub's private-email enforcement rejects anything else — CLAUDE.md Gotchas).
# shellcheck source=build-host.sh
. "$ROOT_DIR/scripts/build-server/build-host.sh"
git config --global user.name  >/dev/null 2>&1 || git config --global user.name "${PHANTOM_GIT_NAME:-$(phantom_repo | cut -d/ -f1)}"
git config --global user.email >/dev/null 2>&1 || git config --global user.email "${PHANTOM_GIT_EMAIL:-8061019+tedswinyar@users.noreply.github.com}"
info "git identity: $(git config --global user.name) <$(git config --global user.email)>"
# publish-release.sh and recut-public.sh push to GitHub over HTTPS through gh's
# credential helper, so no token ever sits in a remote URL.
if gh auth status >/dev/null 2>&1; then gh auth setup-git >/dev/null; info "gh credential helper installed"; fi
"$ROOT_DIR/scripts/doctor.sh"
