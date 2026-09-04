#!/usr/bin/env bash
set -euo pipefail

# generate-release-notes.sh — regenerate CHANGELOG.md from git history via
# git-cliff, and mirror it into the website's changelog page when the
# website layer exists. Called by release.sh; safe to run any time.
#
# Usage: ./scripts/generate-release-notes.sh [vX.Y.Z]
#   With a tag argument the unreleased commits are rendered under that
#   version header (what release.sh needs); without it, under [Unreleased].

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
TAG="${1:-}"

if ! command -v git-cliff >/dev/null; then
  echo "generate-release-notes: git-cliff not found (brew install git-cliff)" >&2
  exit 1
fi

cd "$ROOT_DIR"
if [ -n "$TAG" ]; then
  git-cliff --config cliff.toml --tag "$TAG" -o CHANGELOG.md
else
  git-cliff --config cliff.toml -o CHANGELOG.md
fi
echo "generate-release-notes: wrote CHANGELOG.md"

if [ -d website/content ]; then
  {
    printf -- '---\ntitle: "Changelog"\nlayout: "changelog"\n---\n\n'
    # Strip the H1 — the page layout supplies the title.
    sed '1,/^All notable changes/d' CHANGELOG.md
  } > website/content/changelog.md
  echo "generate-release-notes: mirrored into website/content/changelog.md"
fi
