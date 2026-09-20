#!/usr/bin/env bash
set -euo pipefail

# publish-release.sh — publish a cut release to the PUBLIC repo: generate
# the EdDSA-signed Sparkle appcast over the notarized DMG, then create the
# GitHub Release on tedswinyar/phantom (the public repo — NOT this working
# repo) with the DMG + appcast.xml attached. The app's SUFeedURL points at
# releases/latest/download/appcast.xml, so publishing here IS the update
# channel (phantom-pxt).
#
# Run AFTER release.sh has tagged and build-dmg.sh has notarized+stapled:
#   ./scripts/publish-release.sh <version>
#
# Deliberate act, fail closed: requires the DMG on disk, the Sparkle
# generate_appcast tool (fetched by swift build), the EdDSA private key in
# the login Keychain (generate_keys), and gh auth. Refuses to overwrite an
# existing release.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(dirname "$SCRIPT_DIR")"

# shellcheck source=lib/completion-guard.sh
. "$ROOT_DIR/scripts/lib/completion-guard.sh"
PUBLIC_REPO="tedswinyar/phantom"
# PHANTOM_RELEASE_CONF points at the release.conf to use (default: the gitignored
# one beside this script). It may name SPARKLE_PRIVATE_KEY_FILE so a headless build
# server signs the appcast from a file instead of the login Keychain.
RELEASE_CONF="${PHANTOM_RELEASE_CONF:-$SCRIPT_DIR/release.conf}"
if [ -f "$RELEASE_CONF" ]; then
  # shellcheck source=/dev/null
  . "$RELEASE_CONF"
fi

die() { printf 'publish-release: ERROR: %s\n' "$*" >&2; finish 1; }
info() { printf 'publish-release: %s\n' "$*"; }

[ $# -eq 1 ] || die "usage: ./scripts/publish-release.sh <version>"
VERSION="$1"
printf '%s' "$VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' || die "'$VERSION' is not MAJOR.MINOR.PATCH"

DMG="$ROOT_DIR/dist/Phantom-$VERSION.dmg"
[ -f "$DMG" ] || die "missing $DMG (run release.sh / build-dmg.sh first)"

# Releases build ONLY from the private working repo: CFBundleVersion is the
# repo's commit count and Sparkle REFUSES downgrades, so a DMG built from a
# repo with a smaller count (the squashed public one) would be permanently
# uninstallable for existing users. This guard pins the canonical repo.
ORIGIN_URL="$(git -C "$ROOT_DIR" remote get-url origin 2>/dev/null || true)"
case "$ORIGIN_URL" in
  https://github.com/tedswinyar/phantom-dev.git|https://github.com/tedswinyar/phantom-dev|git@github.com:tedswinyar/phantom-dev.git) : ;;
  *) die "releases must be built from the canonical private repo tedswinyar/phantom-dev (origin: ${ORIGIN_URL:-none}); CFBundleVersion monotonicity depends on it" ;;
esac

# The DMG must be stapled — the recipient's first launch is Gatekeeper's
# verdict — AND so must the .app inside it: a Sparkle-installed update
# never sees the DMG, and an offline Mac cannot look the ticket up
# (phantom-ojj.1; v1.0.0 shipped with only the DMG stapled).
xcrun stapler validate "$DMG" >/dev/null 2>&1 \
  || die "$DMG is not stapled (notarize first; SKIP_NOTARIZE builds must not ship)"
STAPLE_MOUNT="$(mktemp -d /tmp/phantom-staple-check.XXXXXX)"
hdiutil attach "$DMG" -readonly -nobrowse -mountpoint "$STAPLE_MOUNT" >/dev/null \
  || die "cannot mount $DMG to check the app's ticket"
if xcrun stapler validate "$STAPLE_MOUNT/Phantom.app" >/dev/null 2>&1; then
  APP_STAPLED=1
else
  APP_STAPLED=""
fi
hdiutil detach "$STAPLE_MOUNT" >/dev/null 2>&1 || { sleep 2; hdiutil detach "$STAPLE_MOUNT" -force >/dev/null 2>&1 || true; }
rmdir "$STAPLE_MOUNT" 2>/dev/null || true
[ -n "$APP_STAPLED" ] || die "Phantom.app inside $DMG carries no notarization ticket (rebuild with the current build-dmg.sh, which staples the app before packaging)"

GENERATE_APPCAST="$ROOT_DIR/swift/.build/artifacts/sparkle/Sparkle/bin/generate_appcast"
[ -x "$GENERATE_APPCAST" ] || die "missing Sparkle generate_appcast (run swift build once)"

command -v gh >/dev/null || die "gh CLI is required"
gh auth status >/dev/null 2>&1 || die "gh is not authenticated"

gh release view "v$VERSION" --repo "$PUBLIC_REPO" >/dev/null 2>&1 \
  && die "release v$VERSION already exists on $PUBLIC_REPO"

# ---------------------------------------------------------------------------
# Appcast: generate over a staging dir holding ONLY this DMG, so the feed
# carries exactly one item per release and never picks up stray artifacts.
# generate_appcast signs each item with the EdDSA key from the Keychain.
# ---------------------------------------------------------------------------
STAGE="$(mktemp -d /tmp/phantom-appcast.XXXXXX)"
# On macOS bash 3.2 an EXIT trap turns a set -u abort into exit 0, so this script
# could crash and report success. `finish` is available from the source line above;
# the guard is armed here, where the trap used to be. See scripts/lib/completion-guard.sh.
cleanup() { rm -rf "$STAGE"; return 0; }
arm_completion_guard "publish-release" cleanup
cp "$DMG" "$STAGE/"
info "generating signed appcast"
# The EdDSA private key comes from a FILE when SPARKLE_PRIVATE_KEY_FILE is set
# (the build server; docs/build-server.md), else from the login Keychain as
# generate_keys stored it. The key is never regenerated: a new key strands every
# installed copy.
if [ -n "${SPARKLE_PRIVATE_KEY_FILE:-}" ]; then
  [ -r "$SPARKLE_PRIVATE_KEY_FILE" ] || die "SPARKLE_PRIVATE_KEY_FILE=$SPARKLE_PRIVATE_KEY_FILE is not readable"
  info "signing with the EdDSA key from $SPARKLE_PRIVATE_KEY_FILE"
  "$GENERATE_APPCAST" --ed-key-file "$SPARKLE_PRIVATE_KEY_FILE" "$STAGE" >/dev/null \
    || die "generate_appcast failed (key file $SPARKLE_PRIVATE_KEY_FILE)"
else
  "$GENERATE_APPCAST" "$STAGE" >/dev/null || die "generate_appcast failed (EdDSA key in Keychain?)"
fi
APPCAST="$STAGE/appcast.xml"
[ -f "$APPCAST" ] || die "generate_appcast produced no appcast.xml"

# The feed must be signed and must reference this DMG — an unsigned item
# means Sparkle on the user's machine will refuse the update.
grep -q 'edSignature' "$APPCAST" || die "appcast has no edSignature — item is unsigned"
grep -q "Phantom-$VERSION.dmg" "$APPCAST" || die "appcast does not reference Phantom-$VERSION.dmg"

# generate_appcast writes enclosure URLs relative to the staging dir; the
# public download URL is the release asset. Rewrite to the canonical asset
# URL for this tag (immutable, unlike latest/).
ASSET_URL="https://github.com/$PUBLIC_REPO/releases/download/v$VERSION/Phantom-$VERSION.dmg"
perl -pi -e "s{url=\"[^\"]*Phantom-\Q$VERSION\E\.dmg\"}{url=\"$ASSET_URL\"}" "$APPCAST"
grep -q "url=\"$ASSET_URL\"" "$APPCAST" || die "could not rewrite the enclosure URL"

# ---------------------------------------------------------------------------
# Publish: release notes come from the changelog section release.sh built.
# ---------------------------------------------------------------------------
NOTES_FILE="$STAGE/notes.md"
awk "/^## \[$VERSION\]/{flag=1; next} /^## \[/{flag=0} flag" "$ROOT_DIR/CHANGELOG.md" > "$NOTES_FILE"
[ -s "$NOTES_FILE" ] || echo "Phantom v$VERSION" > "$NOTES_FILE"

# ---------------------------------------------------------------------------
# Distribution artifacts (v1.1 Phase 6, phantom-mkn.18): the MCPB bundle is
# a release asset; the cask and server.json are written to dist/ for the
# human steps that follow (tap push, registry publish). Pure renderers in
# scripts/lib/distribution.sh, tested by scripts/tests/test-distribution.sh.
# ---------------------------------------------------------------------------
# shellcheck source=lib/distribution.sh
. "$ROOT_DIR/scripts/lib/distribution.sh"
MCPB="$ROOT_DIR/dist/phantom-mcp-$VERSION.mcpb"
build_mcpb "$ROOT_DIR/build/Phantom.app" "$VERSION" "$MCPB" || die "could not build the MCPB bundle"
MCPB_URL="https://github.com/$PUBLIC_REPO/releases/download/v$VERSION/phantom-mcp-$VERSION.mcpb"
render_server_json "$VERSION" "$MCPB_URL" "$(sha256_of "$MCPB")" "$PUBLIC_REPO" > "$ROOT_DIR/dist/server.json"
render_cask "$VERSION" "$ASSET_URL" "$(sha256_of "$DMG")" > "$ROOT_DIR/dist/phantom.rb"
info "distribution artifacts: $MCPB, dist/server.json, dist/phantom.rb"

info "creating release v$VERSION on $PUBLIC_REPO"
gh release create "v$VERSION" \
  --repo "$PUBLIC_REPO" \
  --title "Phantom v$VERSION" \
  --notes-file "$NOTES_FILE" \
  "$DMG" "$APPCAST" "$MCPB" \
  || die "gh release create failed"

info "published: https://github.com/$PUBLIC_REPO/releases/tag/v$VERSION"
info "Sparkle feed: https://github.com/$PUBLIC_REPO/releases/latest/download/appcast.xml"
info "NEXT (human): copy dist/phantom.rb to the tap as Casks/phantom.rb and push;"
info "              mcp-publisher publish dist/server.json (after mcp-publisher login github);"
info "              optionally sign the MCPB: npx @anthropic-ai/mcpb sign $MCPB"
finish 0
