#!/usr/bin/env bash
set -euo pipefail

# build-app.sh — assemble build/Phantom.app from the SwiftPM executable
# plus the embedded Rust binaries.
#
# Usage:
#   ./scripts/build-app.sh            # release build, ad-hoc signed
#   ./scripts/build-app.sh --debug    # debug build (faster, for local poking)
#
# Signing: ad-hoc ("-") by default so a fresh clone needs no Apple
# credentials. scripts/release.conf (gitignored; see release.conf.example)
# provides SIGNING_IDENTITY for real Developer ID signing — build-dmg.sh
# layers notarization on top of this script's output.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(dirname "$SCRIPT_DIR")"
APP_NAME="Phantom"
BUNDLE_ID="com.tedswinyar.phantom"

CONFIGURATION="release"
if [ "${1:-}" = "--debug" ]; then
  CONFIGURATION="debug"
fi

VERSION="$(sed -n 's/.*static let marketing = "\(.*\)"/\1/p' \
  "$ROOT_DIR/swift/Sources/Phantom/Version.swift")"
if [ -z "$VERSION" ]; then
  echo "build-app.sh: cannot read version from Version.swift" >&2
  exit 1
fi

# CFBundleVersion must be deterministic and monotonic: default it to the git
# commit count so two builds of the same commit produce the same build number
# (was hardcoded 1 — devops review 2026-08-20). Override with BUILD_NUMBER.
if [ -z "${BUILD_NUMBER:-}" ]; then
  BUILD_NUMBER="$(git -C "$ROOT_DIR" rev-list --count HEAD 2>/dev/null || echo 1)"
fi

SIGNING_IDENTITY="-"
if [ -f "$SCRIPT_DIR/release.conf" ]; then
  # shellcheck source=/dev/null
  . "$SCRIPT_DIR/release.conf"
fi

echo "==> Building $APP_NAME $VERSION ($CONFIGURATION)"
(cd "$ROOT_DIR/swift" && swift build -c "$CONFIGURATION")
# Release binaries must not embed the build machine's paths: rustc bakes
# file!()/panic paths like /Users/<name>/.cargo/registry/... into every
# binary (adversarial review 2026-09-02: 300+ hits per binary), which
# `strings` on a shipped DMG happily reveals. Remap the identifying
# prefixes; the strings gate below fails the build if any survive.
RELEASE_RUSTFLAGS="--remap-path-prefix=$ROOT_DIR=/phantom --remap-path-prefix=$HOME/.cargo=/cargo --remap-path-prefix=$HOME=/home"
if [ "$CONFIGURATION" = release ]; then
  (cd "$ROOT_DIR/rust" && RUSTFLAGS="${RUSTFLAGS:-} $RELEASE_RUSTFLAGS" cargo build --workspace --release)
else
  (cd "$ROOT_DIR/rust" && cargo build --workspace)
fi

APP_BINARY="$ROOT_DIR/swift/.build/$CONFIGURATION/$APP_NAME"
RUST_BIN_DIR="$ROOT_DIR/rust/target/$CONFIGURATION"
for f in "$APP_BINARY" "$RUST_BIN_DIR/phantom-api" "$RUST_BIN_DIR/phantom-mcp" "$RUST_BIN_DIR/phantom"; do
  if [ ! -f "$f" ]; then
    echo "build-app.sh: missing build product: $f" >&2
    exit 1
  fi
done

APP_DIR="$ROOT_DIR/build/$APP_NAME.app"
echo "==> Assembling $APP_DIR"
rm -rf "$APP_DIR"
mkdir -p "$APP_DIR/Contents/MacOS" "$APP_DIR/Contents/Resources"

cp "$APP_BINARY" "$APP_DIR/Contents/MacOS/$APP_NAME"

# Sparkle auto-update (phantom-pxt): the SwiftPM build links
# @rpath/Sparkle.framework and relies on the framework sitting next to the
# binary in .build/; the bundle embeds it in Contents/Frameworks and adds
# the matching rpath. ditto (not cp) preserves the framework's symlink
# structure — a dereferenced copy breaks codesign.
SPARKLE_SRC="$ROOT_DIR/swift/.build/artifacts/sparkle/Sparkle/Sparkle.xcframework/macos-arm64_x86_64/Sparkle.framework"
if [ ! -d "$SPARKLE_SRC" ]; then
  echo "build-app.sh: missing Sparkle.framework artifact at $SPARKLE_SRC (swift build fetches it)" >&2
  exit 1
fi
mkdir -p "$APP_DIR/Contents/Frameworks"
ditto "$SPARKLE_SRC" "$APP_DIR/Contents/Frameworks/Sparkle.framework"
install_name_tool -add_rpath "@executable_path/../Frameworks" \
  "$APP_DIR/Contents/MacOS/$APP_NAME" 2>/dev/null || true
# Strip the build machine's Xcode-toolchain rpath — unnecessary build
# provenance in a shipped binary (review, 2026-09-03). The system rpaths
# (/usr/lib/swift, @loader_path, @executable_path/../Frameworks) remain.
otool -l "$APP_DIR/Contents/MacOS/$APP_NAME" | awk '/LC_RPATH/{f=1} f && /path /{print $2; f=0}' \
  | grep -E '^/Applications/Xcode' | while read -r rp; do
    install_name_tool -delete_rpath "$rp" "$APP_DIR/Contents/MacOS/$APP_NAME" 2>/dev/null || true
  done
# App icon: committed artifact, regenerated from SVG by scripts/make-icon.sh.
ICON_ICNS="$ROOT_DIR/assets/icon/AppIcon.icns"
if [ ! -f "$ICON_ICNS" ]; then
  echo "build-app.sh: missing $ICON_ICNS (regenerate with scripts/make-icon.sh)" >&2
  exit 1
fi
cp "$ICON_ICNS" "$APP_DIR/Contents/Resources/AppIcon.icns"
# Server + MCP ride in MacOS/ so Bundle.url(forAuxiliaryExecutable:) finds
# them. The CLI must NOT: on case-insensitive APFS "phantom" and the app
# executable "Phantom" are the same path, and the copy silently replaces the
# app binary with the CLI (bundle launches as clap help). Helpers/ instead.
cp "$RUST_BIN_DIR/phantom-api" "$APP_DIR/Contents/MacOS/"
cp "$RUST_BIN_DIR/phantom-mcp" "$APP_DIR/Contents/MacOS/"
mkdir -p "$APP_DIR/Contents/Helpers"
cp "$RUST_BIN_DIR/phantom" "$APP_DIR/Contents/Helpers/"
if cmp -s "$APP_DIR/Contents/MacOS/$APP_NAME" "$RUST_BIN_DIR/phantom"; then
  echo "build-app.sh: case-fold clobber — MacOS/$APP_NAME is the CLI binary" >&2
  exit 1
fi

# Path-leak gate (release only; debug builds legitimately carry dev paths):
# no binary in the bundle may contain a /Users/ path. This is the check that
# keeps the remap honest — a toolchain change that reintroduces paths fails
# HERE, not in a reviewer's `strings` on the shipped DMG.
if [ "$CONFIGURATION" = release ]; then
  for bin in "$APP_DIR/Contents/MacOS/$APP_NAME" "$APP_DIR/Contents/MacOS/phantom-api" \
             "$APP_DIR/Contents/MacOS/phantom-mcp" "$APP_DIR/Contents/Helpers/phantom"; do
    if strings "$bin" | grep -q "/Users/"; then
      echo "build-app.sh: $bin embeds /Users/ paths (build-machine leak):" >&2
      strings "$bin" | grep "/Users/" | head -3 >&2
      exit 1
    fi
  done
  echo "==> Path-leak gate: clean (no /Users/ strings in shipped binaries)"
fi

cat > "$APP_DIR/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>       <string>$APP_NAME</string>
    <key>CFBundleIconFile</key>         <string>AppIcon</string>
    <key>CFBundleIdentifier</key>       <string>$BUNDLE_ID</string>
    <key>CFBundleName</key>             <string>Phantom</string>
    <key>CFBundlePackageType</key>      <string>APPL</string>
    <key>CFBundleShortVersionString</key><string>$VERSION</string>
    <key>CFBundleVersion</key>          <string>${BUILD_NUMBER:-1}</string>
    <key>LSMinimumSystemVersion</key>   <string>14.0</string>
    <key>NSHighResolutionCapable</key>  <true/>
    <key>LSApplicationCategoryType</key><string>public.app-category.productivity</string>
    <!-- Sparkle (phantom-pxt): the appcast lives on the public repo's
         GitHub Releases (no domain needed); the EdDSA PUBLIC key pairs
         with the private key in the developer Keychain (generate_keys).
         Sparkle asks the user before any automatic check — this feed is
         the app's only outbound connection (SECURITY.md). -->
    <key>SUFeedURL</key>                <string>https://github.com/tedswinyar/phantom/releases/latest/download/appcast.xml</string>
    <key>SUPublicEDKey</key>            <string>xlrex9nrn9ecK01wrieD4P3sLprUnjXan7HLGTGFcOg=</string>
</dict>
</plist>
PLIST

echo "==> Signing (identity: ${SIGNING_IDENTITY})"
# --options runtime (hardened runtime) is REQUIRED by the notary service on
# every executable — without it the submission comes back "status: Invalid"
# after the multi-minute upload. Harmless under ad-hoc signing, so it is
# unconditional. (Secure timestamps are automatic with a Developer ID
# identity; hardened runtime never is.)
for bin in phantom-api phantom-mcp; do
  codesign --force --options runtime --sign "$SIGNING_IDENTITY" "$APP_DIR/Contents/MacOS/$bin"
done
codesign --force --options runtime --sign "$SIGNING_IDENTITY" "$APP_DIR/Contents/Helpers/phantom"
# Sparkle: nested components first (Sparkle's documented non-Xcode order),
# then the framework, then the app. The XPC services keep their shipped
# entitlements (--preserve-metadata) — stripping them breaks the installer.
SPARKLE_FW="$APP_DIR/Contents/Frameworks/Sparkle.framework"
for xpc in "$SPARKLE_FW"/Versions/B/XPCServices/*.xpc; do
  [ -e "$xpc" ] || continue
  codesign --force --options runtime --preserve-metadata=entitlements \
    --sign "$SIGNING_IDENTITY" "$xpc"
done
if [ -e "$SPARKLE_FW/Versions/B/Autoupdate" ]; then
  codesign --force --options runtime --sign "$SIGNING_IDENTITY" \
    "$SPARKLE_FW/Versions/B/Autoupdate"
fi
if [ -e "$SPARKLE_FW/Versions/B/Updater.app" ]; then
  codesign --force --options runtime --sign "$SIGNING_IDENTITY" \
    "$SPARKLE_FW/Versions/B/Updater.app"
fi
codesign --force --options runtime --sign "$SIGNING_IDENTITY" "$SPARKLE_FW"
codesign --force --options runtime --sign "$SIGNING_IDENTITY" "$APP_DIR"

# Verify-after-sign (review, 2026-09-03: an invalidated bundle — modified
# after sealing by a later partial rebuild — sat on disk looking shippable).
# A bundle this script produces is VALID or the script FAILS; nothing in
# between can exist.
codesign --verify --deep --strict "$APP_DIR" \
  || { echo "build-app.sh: signature verification FAILED on the assembled bundle" >&2; exit 1; }
echo "==> Signature verified (deep, strict)"

echo "==> Done: $APP_DIR"
