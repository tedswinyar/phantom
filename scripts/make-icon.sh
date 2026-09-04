#!/usr/bin/env bash
set -euo pipefail

# make-icon.sh — regenerate the app icon rasters from the SVG source.
#
# Source of truth: assets/icon/AppIcon.svg (+ assets/icon/small-sizes.css,
# applied to the 16px/32px reps so the mark still reads when tiny).
# Outputs (committed, so builds never need librsvg):
#   assets/icon/AppIcon.icns        — consumed by scripts/build-app.sh
#   assets/icon/AppIcon-1024.png    — master render, for docs/website reuse
#
# Requires: rsvg-convert (brew install librsvg), iconutil (macOS).
# Each rep is rendered from the SVG at its exact pixel size rather than
# downscaled from the 1024 master — crisper edges, and it lets the small
# sizes swap in the simplified art.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(dirname "$SCRIPT_DIR")"
ICON_DIR="$ROOT_DIR/assets/icon"
SVG="$ICON_DIR/AppIcon.svg"
SMALL_CSS="$ICON_DIR/small-sizes.css"
ICONSET="$ROOT_DIR/build/icon/AppIcon.iconset"

for tool in rsvg-convert iconutil; do
  command -v "$tool" >/dev/null || {
    echo "make-icon.sh: $tool not found (rsvg-convert: brew install librsvg)" >&2
    exit 1
  }
done

rm -rf "$ICONSET"
mkdir -p "$ICONSET"

# rep-name  pixel-size  variant
REPS="
icon_16x16.png      16   small
icon_16x16@2x.png   32   small
icon_32x32.png      32   small
icon_32x32@2x.png   64   full
icon_128x128.png    128  full
icon_128x128@2x.png 256  full
icon_256x256.png    256  full
icon_256x256@2x.png 512  full
icon_512x512.png    512  full
icon_512x512@2x.png 1024 full
"

echo "$REPS" | while read -r name px variant; do
  [ -z "$name" ] && continue
  if [ "$variant" = small ]; then
    rsvg-convert -w "$px" -h "$px" --stylesheet "$SMALL_CSS" "$SVG" -o "$ICONSET/$name"
  else
    rsvg-convert -w "$px" -h "$px" "$SVG" -o "$ICONSET/$name"
  fi
done

cp "$ICONSET/icon_512x512@2x.png" "$ICON_DIR/AppIcon-1024.png"
iconutil -c icns "$ICONSET" -o "$ICON_DIR/AppIcon.icns"

echo "==> Wrote $ICON_DIR/AppIcon.icns and $ICON_DIR/AppIcon-1024.png"
