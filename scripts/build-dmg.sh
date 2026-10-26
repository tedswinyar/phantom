#!/usr/bin/env bash
set -euo pipefail

# build-dmg.sh — produce dist/Phantom-<version>.dmg: build → sign →
# notarize + staple the .app → DMG → notarize → staple the DMG.
#
# Usage:
#   ./scripts/build-dmg.sh          # full pipeline
#   ./scripts/build-dmg.sh dmg      # DMG + notarize only; reuses last .app
#   SKIP_NOTARIZE=1 ./scripts/build-dmg.sh   # signed-but-unnotarized DMG
#
# Identity comes from scripts/release.conf (gitignored; see
# release.conf.example). Without it: ad-hoc signing, notarization skipped —
# a fresh clone needs zero Apple credentials to produce a local DMG.
#
# Notarization credentials live in the DATA-PROTECTION keychain, which can
# become inaccessible to CLI tools after sleep/iCloud events. Hence:
#   1. a fail-fast pre-flight (seconds) before the multi-minute build, and
#   2. non-interactive self-healing when APPLE_ID / APPLE_TEAM_ID /
#      APPLE_APP_SPECIFIC_PASSWORD are exported.
# docs/build-pipeline.md has the full failure-mode table.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(dirname "$SCRIPT_DIR")"

# shellcheck source=lib/completion-guard.sh
. "$ROOT_DIR/scripts/lib/completion-guard.sh"
APP_NAME="Phantom"
APP_DIR="$ROOT_DIR/build/$APP_NAME.app"
DIST_DIR="$ROOT_DIR/dist"

MODE="${1:-full}"

SIGNING_IDENTITY="-"
NOTARIZE_PROFILE=""
# PHANTOM_RELEASE_CONF points at the release.conf to use (default: the gitignored
# one beside this script); the build server keeps it outside the checkout.
RELEASE_CONF="${PHANTOM_RELEASE_CONF:-$SCRIPT_DIR/release.conf}"
if [ -f "$RELEASE_CONF" ]; then
  # shellcheck source=/dev/null
  . "$RELEASE_CONF"
fi

# The notarytool auth flags, or SKIP_NOTARIZE when nothing is configured. A
# half-configured API key is FATAL here, not a skip: on the build server that
# would otherwise fall through to the interactive profile and fail minutes
# later for the wrong reason (scripts/lib/notary.sh; test-notary.sh pins it).
# shellcheck source=lib/notary.sh
. "$ROOT_DIR/scripts/lib/notary.sh"
SKIP_NOTARIZE="${SKIP_NOTARIZE:-}"
NOTARY_ARGS=()
if notary_lines="$(phantom_notary_args)"; then
  while IFS= read -r a; do NOTARY_ARGS+=("$a"); done <<< "$notary_lines"
else
  case $? in
    1) SKIP_NOTARIZE=1 ;;
    *) printf 'build-dmg: ERROR: notarization credentials are misconfigured (see above)\n' >&2; finish 1 ;;
  esac
fi

VERSION="$(sed -n 's/.*static let marketing = "\(.*\)"/\1/p' \
  "$ROOT_DIR/swift/Sources/Phantom/Version.swift")"
[ -n "$VERSION" ] || { echo "build-dmg: cannot read version" >&2; finish 1; }
DMG_PATH="$DIST_DIR/$APP_NAME-$VERSION.dmg"

info() { printf 'build-dmg: %s\n' "$*"; }
die() {
  printf 'build-dmg: ERROR: %s\n' "$*" >&2
  finish 1
}

# ---------------------------------------------------------------------------
# Notarization pre-flight: fail in seconds, not after a five-minute build.
# ---------------------------------------------------------------------------
preflight_notary() {
  [ -n "$SKIP_NOTARIZE" ] && return 0
  if xcrun notarytool history "${NOTARY_ARGS[@]}" >/dev/null 2>&1; then
    info "notary pre-flight OK ($(phantom_notary_describe))"
    return 0
  fi
  info "notary credentials not accepted: $(phantom_notary_describe)"
  if [ -n "${NOTARY_KEY_PATH:-}" ]; then
    die "the App Store Connect API key was rejected — wrong key id / issuer id, key
revoked, or offline. Check App Store Connect → Users and Access → Integrations;
scripts/doctor.sh --release reproduces this in seconds. Or SKIP_NOTARIZE=1."
  fi
  if [ -n "${APPLE_ID:-}" ] && [ -n "${APPLE_TEAM_ID:-}" ] && [ -n "${APPLE_APP_SPECIFIC_PASSWORD:-}" ]; then
    info "self-healing: re-provisioning the keychain profile"
    xcrun notarytool store-credentials "$NOTARIZE_PROFILE" \
      --apple-id "$APPLE_ID" --team-id "$APPLE_TEAM_ID" \
      --password "$APPLE_APP_SPECIFIC_PASSWORD" --no-validate \
      || die "could not re-provision notary credentials"
    xcrun notarytool history "${NOTARY_ARGS[@]}" >/dev/null 2>&1 \
      || die "profile still inaccessible after re-provisioning"
    info "self-heal succeeded"
    return 0
  fi
  die "notary credentials unavailable. Either run any notarytool command
interactively to unlock the keychain, export APPLE_ID/APPLE_TEAM_ID/
APPLE_APP_SPECIFIC_PASSWORD for self-healing, configure an App Store Connect
API key (NOTARY_KEY_PATH/NOTARY_KEY_ID/NOTARY_ISSUER_ID — what a build server
needs), or use SKIP_NOTARIZE=1."
}

preflight_notary

# Submit one artifact (a zip of the .app, or the DMG) and wait for the
# verdict. Each failure class has a different recovery; `$2` names the
# unnotarized artifact left behind so the message can point at it.
notarize_submit() { # path label
  local path="$1" label="$2" out status sub_id
  info "submitting $label to Apple notary service (this can take a few minutes)"
  set +e
  out="$(xcrun notarytool submit "$path" "${NOTARY_ARGS[@]}" --wait 2>&1)"
  status=$?
  set -e
  if [ "$status" -ne 0 ] || ! printf '%s' "$out" | grep -q 'status: Accepted'; then
    printf '%s\n' "$out" >&2
    if printf '%s' "$out" | grep -q 'No Keychain password'; then
      die "credential disappeared mid-build. Re-run; pre-flight will self-heal if env vars are set. A signed-but-unnotarized $label is at $path"
    elif printf '%s' "$out" | grep -qE 'id: [0-9a-f-]+'; then
      sub_id="$(printf '%s' "$out" | grep -oE 'id: [0-9a-f-]+' | head -1 | cut -d' ' -f2)"
      die "notary service rejected the $label. Inspect:
  xcrun notarytool log $sub_id ${NOTARY_ARGS[*]}
A signed-but-unnotarized $label is at $path"
    else
      die "network/transient notary failure on the $label. Retry just this step:
  ./scripts/build-dmg.sh dmg
A signed-but-unnotarized $label is at $path"
    fi
  fi
}

# ---------------------------------------------------------------------------
# Build the app (unless reusing)
# ---------------------------------------------------------------------------
if [ "$MODE" != "dmg" ]; then
  "$SCRIPT_DIR/build-app.sh"
fi
[ -d "$APP_DIR" ] || die "no app bundle at $APP_DIR (run without 'dmg' first)"

# ---------------------------------------------------------------------------
# Notarize + staple the .app ITSELF (phantom-ojj.1). Stapling only the DMG
# left the installed app without a ticket: Gatekeeper accepted it via an
# online lookup, so first launch on an OFFLINE Mac — or a Sparkle-installed
# update, which never sees the DMG — could be refused. The ticket is
# stapled here, before the app is copied into the DMG, so both the DMG and
# the app inside it carry one.
# ---------------------------------------------------------------------------
if [ -z "$SKIP_NOTARIZE" ]; then
  if xcrun stapler validate "$APP_DIR" >/dev/null 2>&1; then
    info "$APP_NAME.app already carries a notarization ticket"
  else
    APP_ZIP="$(mktemp -d /tmp/phantom-app-notary.XXXXXX)/$APP_NAME.zip"
    ditto -c -k --keepParent "$APP_DIR" "$APP_ZIP"
    notarize_submit "$APP_ZIP" ".app"
    rm -rf "$(dirname "$APP_ZIP")"
    info "notarized; stapling $APP_NAME.app"
    xcrun stapler staple "$APP_DIR" >/dev/null
    xcrun stapler validate "$APP_DIR" >/dev/null || die "app staple validation failed"
  fi
fi

# ---------------------------------------------------------------------------
# DMG — styled: background art, 128pt icons, app on the left, an arrow in
# the art pointing at the Applications alias on the right. The icon
# coordinates here are a GEOMETRY CONTRACT with assets/dmg/background.svg
# (regenerate the PNG with scripts/make-dmg-background.sh if either moves).
# Styling needs Finder scripting (an Automation/TCC prompt on first run);
# SKIP_DMG_STYLE=1 falls back to the plain unstyled DMG.
# ---------------------------------------------------------------------------
mkdir -p "$DIST_DIR"
rm -f "$DMG_PATH"
STAGING="$(mktemp -d /tmp/phantom-dmg.XXXXXX)"
RW_DMG="$STAGING-rw.dmg"
# On macOS bash 3.2 an EXIT trap turns a set -u abort into exit 0, so this script
# could crash and report success. `finish` is available from the source line above;
# the guard is armed here, where the trap used to be. See scripts/lib/completion-guard.sh.
cleanup() { rm -rf "$STAGING" "$RW_DMG"; return 0; }
arm_completion_guard "build-dmg" cleanup
cp -R "$APP_DIR" "$STAGING/"
ln -s /Applications "$STAGING/Applications"

BACKGROUND_PNG="$ROOT_DIR/assets/dmg/background.png"
if [ -z "${SKIP_DMG_STYLE:-}" ] && [ -f "$BACKGROUND_PNG" ]; then
  mkdir -p "$STAGING/.background"
  cp "$BACKGROUND_PNG" "$STAGING/.background/background.png"

  info "creating styled DMG"
  hdiutil create -volname "$APP_NAME $VERSION" -srcfolder "$STAGING" \
    -ov -format UDRW -fs HFS+ "$RW_DMG" >/dev/null
  MOUNT_POINT="/Volumes/$APP_NAME $VERSION"
  # A stale mount from an aborted run shadows ours; clear it.
  [ -d "$MOUNT_POINT" ] && hdiutil detach "$MOUNT_POINT" -force >/dev/null 2>&1
  hdiutil attach "$RW_DMG" -readwrite -noverify -nobrowse >/dev/null

  # Finder writes the layout into the volume's .DS_Store. The window bounds
  # are chrome-inclusive; 660x400 content puts the icon row and the art's
  # arrow on the same line.
  if ! osascript >/dev/null <<OSA
tell application "Finder"
  tell disk "$APP_NAME $VERSION"
    open
    set current view of container window to icon view
    set toolbar visible of container window to false
    set statusbar visible of container window to false
    set the bounds of container window to {400, 120, 1060, 548}
    set viewOptions to the icon view options of container window
    set arrangement of viewOptions to not arranged
    set icon size of viewOptions to 128
    set text size of viewOptions to 13
    set background picture of viewOptions to file ".background:background.png"
    set position of item "$APP_NAME.app" of container window to {165, 180}
    set position of item "Applications" of container window to {495, 180}
    close
    open
    update without registering applications
    delay 1
    close
  end tell
end tell
OSA
  then
    hdiutil detach "$MOUNT_POINT" -force >/dev/null 2>&1 || true
    die "Finder styling failed (Automation permission? approve the prompt and
re-run, or SKIP_DMG_STYLE=1 for a plain DMG)"
  fi
  sync
  hdiutil detach "$MOUNT_POINT" >/dev/null \
    || { sleep 2; hdiutil detach "$MOUNT_POINT" -force >/dev/null; }
  info "compressing styled DMG -> $DMG_PATH"
  hdiutil convert "$RW_DMG" -format UDZO -o "$DMG_PATH" >/dev/null
else
  info "creating $DMG_PATH (unstyled)"
  hdiutil create -volname "$APP_NAME $VERSION" -srcfolder "$STAGING" \
    -ov -format UDZO "$DMG_PATH" >/dev/null
fi

if [ "$SIGNING_IDENTITY" != "-" ]; then
  info "signing DMG"
  if [ -n "${SIGNING_KEYCHAIN:-}" ]; then
    codesign --force --timestamp --keychain "$SIGNING_KEYCHAIN" --sign "$SIGNING_IDENTITY" "$DMG_PATH"
  else
    codesign --force --sign "$SIGNING_IDENTITY" "$DMG_PATH"
  fi
fi

# ---------------------------------------------------------------------------
# Notarize + staple
# ---------------------------------------------------------------------------
if [ -n "$SKIP_NOTARIZE" ]; then
  info "SKIP_NOTARIZE set (or no profile configured): DMG is NOT notarized."
  info "It works locally; Gatekeeper will warn on other Macs."
else
  notarize_submit "$DMG_PATH" "DMG"
  info "notarized; stapling DMG"
  xcrun stapler staple "$DMG_PATH" >/dev/null
  xcrun stapler validate "$DMG_PATH" >/dev/null || die "DMG staple validation failed"
fi

info "done: $DMG_PATH"
finish 0
