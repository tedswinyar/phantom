#!/usr/bin/env bash
set -euo pipefail

# make-dmg-background.sh — render assets/dmg/background.svg to the PNG
# build-dmg.sh embeds in the DMG's hidden .background/ directory.
#
# Output (committed, so builds never need librsvg): assets/dmg/background.png
# — 1320x800 px at 144 DPI, which Finder draws as a sharp 660x400 pt window
# on Retina (and scales cleanly elsewhere). The icon positions Finder is
# scripted to use live in build-dmg.sh and MUST match the art's geometry
# contract (see the comment atop background.svg).
#
# Requires: rsvg-convert (brew install librsvg), sips (macOS).

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(dirname "$SCRIPT_DIR")"
SVG="$ROOT_DIR/assets/dmg/background.svg"
PNG="$ROOT_DIR/assets/dmg/background.png"

command -v rsvg-convert >/dev/null || {
  echo "make-dmg-background.sh: rsvg-convert not found (brew install librsvg)" >&2
  exit 1
}

rsvg-convert -w 1320 -h 800 "$SVG" -o "$PNG"
# 144 DPI == "these 1320px are 660pt": Finder reads the DPI to size the
# background in points, which is what makes it Retina-sharp.
sips -s dpiWidth 144 -s dpiHeight 144 "$PNG" >/dev/null
echo "make-dmg-background.sh: wrote $PNG ($(sips -g pixelWidth -g pixelHeight "$PNG" | awk '/pixel/{printf "%s ", $2}')px @144dpi)"
