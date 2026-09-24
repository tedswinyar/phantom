#!/usr/bin/env bash
set -euo pipefail

# release.sh — cut a release: validate version alignment, verify, build the
# DMG, regenerate release notes, tag.
#
# Usage: ./scripts/release.sh <version>       e.g. ./scripts/release.sh 0.2.0
#        DRY_RUN=1 ./scripts/release.sh <version>   # rehearse: every gate and
#             artifact, but no commit and no tag (pair with SKIP_NOTARIZE=1)
#
# Version alignment (VERSIONING.md): Version.swift and
# open-prompt-edition/VERSION must both equal <version>. The release BLOCKS
# on divergence — bump them first, deliberately.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(dirname "$SCRIPT_DIR")"

die() {
  printf 'release: ERROR: %s\n' "$*" >&2
  exit 1
}
info() { printf 'release: %s\n' "$*"; }

[ $# -eq 1 ] || die "usage: ./scripts/release.sh <version>"
VERSION="$1"
printf '%s' "$VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' || die "'$VERSION' is not MAJOR.MINOR.PATCH"

cd "$ROOT_DIR"

# ---------------------------------------------------------------------------
# Alignment gates
# ---------------------------------------------------------------------------
APP_VERSION="$(sed -n 's/.*static let marketing = "\(.*\)"/\1/p' swift/Sources/Phantom/Version.swift 2>/dev/null || true)"
if [ -d swift ] && [ "$APP_VERSION" != "$VERSION" ]; then
  die "Version.swift says '$APP_VERSION', release says '$VERSION'. Update swift/Sources/Phantom/Version.swift first."
fi

# GET /health reports env!("CARGO_PKG_VERSION") — the workspace version in
# rust/Cargo.toml — and the OPE says that IS the contract version. 1.0.0
# matched by luck; found 2026-09-09 when the 1.1 dogfood build answered
# `"version": "1.0.0"`. The release bump touches Version.swift, rust/Cargo.toml
# (Cargo.lock follows on the next build) and the plugin manifests together.
if [ -f rust/Cargo.toml ]; then
  CARGO_VERSION="$(sed -n '/^\[workspace.package\]/,/^\[/{s/^version = "\(.*\)"/\1/p;}' rust/Cargo.toml)"
  [ -n "$CARGO_VERSION" ] || CARGO_VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' rust/Cargo.toml | head -1)"
  [ "$CARGO_VERSION" = "$VERSION" ] || die "rust/Cargo.toml workspace version says '$CARGO_VERSION', release says '$VERSION'. GET /health reports it as the contract version; bump it with Version.swift."
fi

if [ -d open-prompt-edition ]; then
  OPE_VERSION="$(tr -d '[:space:]' < open-prompt-edition/VERSION)"
  [ "$OPE_VERSION" = "$VERSION" ] || die "open-prompt-edition/VERSION says '$OPE_VERSION', release says '$VERSION'. They move together (VERSIONING.md)."
fi

# The Claude Code plugin manifests carry the version too (v1.1 Phase 6,
# phantom-mkn.18): a marketplace user only receives an update when
# plugin.json's version moves, so it moves with the release or not at all.
for manifest in .claude-plugin/plugin.json .claude-plugin/marketplace.json; do
  [ -f "$manifest" ] || continue
  command -v jq >/dev/null || die "jq is required to check $manifest"
  case "$manifest" in
    *plugin.json) PLUGIN_VERSION="$(jq -r '.version // empty' "$manifest")" ;;
    *) PLUGIN_VERSION="$(jq -r '.plugins[0].version // empty' "$manifest")" ;;
  esac
  [ "$PLUGIN_VERSION" = "$VERSION" ] || die "$manifest says '${PLUGIN_VERSION:-<none>}', release says '$VERSION'. The plugin version moves with the release (docs/build-pipeline.md, Distribution artifacts)."
done

if [ -n "$(git status --porcelain)" ]; then
  if [ -n "${DRY_RUN:-}" ]; then
    info "DRY_RUN: tolerating a dirty working tree (a real release refuses)"
  else
    die "working tree is dirty; commit or stash first"
  fi
fi
git rev-parse "v$VERSION" >/dev/null 2>&1 && die "tag v$VERSION already exists"

# ---------------------------------------------------------------------------
# Gates, artifacts, notes
# ---------------------------------------------------------------------------
info "running verify"
./scripts/verify.sh || die "verify failed; no release from a red suite"

if [ -d swift ]; then
  info "building DMG"
  # The DMG is built BEFORE the tag exists (the release commit is made below and
  # tagged after), so the binaries cannot see their own tag. This build IS the
  # release: tell the build script so, and prove the label afterwards
  # (rust/build-support/git-describe.rs, phantom-cnr.6).
  PHANTOM_GIT_DESCRIBE="v$VERSION" ./scripts/build-dmg.sh || die "DMG build failed"
  HELPER="build/Phantom.app/Contents/Helpers/phantom"
  [ -x "$HELPER" ] || die "$HELPER missing after the build — build-app.sh no longer bundles the CLI?"
  GOT="$("$HELPER" --version 2>/dev/null || true)"
  [ "$GOT" = "phantom $VERSION" ] \
    || die "the bundled CLI reports '${GOT:-<nothing>}', the release is $VERSION — an untagged/dirty build must not ship"
fi

# Release tooling is REQUIRED for a release (unlike the develop-time gates,
# a release is a deliberate act — fail closed rather than ship stale notes /
# missing attribution). Run ./scripts/doctor.sh to install these.
for tool in git-cliff cargo-about cargo-cyclonedx; do
  command -v "$tool" >/dev/null || die "$tool is required to cut a release (see ./scripts/doctor.sh)"
done

# One generator, one invocation: release.sh used to run git-cliff itself and
# THEN call this script, whose tag-less rerun overwrote the [vX.Y.Z] header
# back to [Unreleased] — a tagged release shipped an unreleased changelog.
info "generating release notes"
./scripts/generate-release-notes.sh "v$VERSION" >/dev/null \
  || die "release notes generation failed"

info "regenerating THIRD-PARTY-NOTICES.html"
(cd rust && cargo about generate about.hbs -o ../THIRD-PARTY-NOTICES.html) \
  || die "cargo-about failed; attribution must be current for a release"

info "generating SBOM (CycloneDX)"
# One SBOM per crate, copied under a deterministic name each. (The previous
# `find … -exec cp {} sbom.cdx.json` kept whichever crate find listed LAST —
# a different SBOM depending on directory order.)
(cd rust && cargo cyclonedx --format json) || die "cargo-cyclonedx failed"
mkdir -p sbom
SBOM_COUNT=0
for crate in phantom-core phantom-api phantom-cli phantom-mcp; do
  if [ -f "rust/$crate/$crate.cdx.json" ]; then
    cp "rust/$crate/$crate.cdx.json" "sbom/$crate.cdx.json"
    SBOM_COUNT=$((SBOM_COUNT + 1))
  fi
done
[ "$SBOM_COUNT" -gt 0 ] || die "cargo-cyclonedx produced no SBOM files"
# cargo-cyclonedx embeds the build machine's absolute path in bom-refs
# (path+file:///Users/<name>/... — found in the 1.0.0 pre-flip sweep, 22
# hits). Neutralize to a stable /phantom root (also makes SBOMs
# deterministic across machines), then FAIL CLOSED if any /Users/ path
# survives — same posture as the binary path-leak gate.
for f in sbom/*.cdx.json; do
  perl -pi -e "s{file://\Q$ROOT_DIR\E}{file:///phantom}g" "$f"
  if grep -q "/Users/" "$f"; then
    die "SBOM $f still contains a /Users/ path (build-machine leak)"
  fi
done
info "SBOM: $SBOM_COUNT crate SBOMs in sbom/ (paths neutralized)"

if [ -n "${DRY_RUN:-}" ]; then
  info "DRY_RUN: skipping commit and tag. Artifacts on disk:"
  info "  CHANGELOG.md, THIRD-PARTY-NOTICES.html, sbom/, dist/"
  info "a real release from this state would tag v$VERSION"
  exit 0
fi

# Add only the paths that exist: `git add a b c` with ONE missing path stages
# NOTHING and exits 1, and the old `|| true` hid that — the notes commit never
# happened and the tag landed on the previous commit (Banshee's first pipeline
# rehearsal, 2026-09-15). Pinned by test-release-dry-run.sh.
RELEASE_FILES=()
for f in CHANGELOG.md website/content/changelog.md THIRD-PARTY-NOTICES.html sbom; do
  [ -e "$f" ] && RELEASE_FILES+=("$f")
done
[ "${#RELEASE_FILES[@]}" -gt 0 ] || die "no release files to commit (CHANGELOG.md, THIRD-PARTY-NOTICES.html, sbom/ all missing)"
git add "${RELEASE_FILES[@]}"
git diff --cached --quiet || git commit -m "release: v$VERSION notes + attribution + SBOM"

# ---------------------------------------------------------------------------
# Tag
# ---------------------------------------------------------------------------
git tag -a "v$VERSION" -m "Phantom v$VERSION"
info "tagged v$VERSION"
info "next: git push && git push --tags, then"
info "  ./scripts/publish-release.sh $VERSION   # DMG + Sparkle appcast -> public repo Releases"
