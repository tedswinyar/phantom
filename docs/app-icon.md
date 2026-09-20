# App icon

A friendly ghost whose lower body is divided into treemap tiles — the Specter
Map — in the site palette (spectral violet `#a78bfa` family, one ectoplasm
mint `#6ee7b7` tile, darks `#0b0a12`/`#141221`/`#241f38` from
`website/hugo.yaml`), on the standard macOS Big Sur+ squircle (824×824 grid
at 100,100 on a 1024 canvas, no baked shadow).

## Files

| File | Role |
|---|---|
| `assets/icon/AppIcon.svg` | Source of truth. Edit THIS, never the rasters. |
| `assets/icon/small-sizes.css` | Override applied to the 16/32 px reps: hides the treemap tiling and enlarges the eyes so the mark reads when tiny. |
| `assets/icon/AppIcon.icns` | Committed artifact consumed by `scripts/build-app.sh` (copied to `Contents/Resources`, named by `CFBundleIconFile`). Committed so a fresh clone builds without librsvg. |
| `assets/icon/AppIcon-1024.png` | Master render, for docs/website reuse. |

## Regenerate

```bash
brew install librsvg     # once, for rsvg-convert
./scripts/make-icon.sh
```

The script renders every iconset rep directly from the SVG at its exact pixel
size (crisper than downscaling the 1024 master, and it lets the 16/32 px reps
swap in the simplified art), then packs them with `iconutil -c icns`.

Sanity check after a design change: look at the 1024 master AND the 32 px rep
(`iconutil -c iconset assets/icon/AppIcon.icns -o /tmp/x.iconset`) — detail
that reads at 1024 routinely turns to mush at 32.

The website favicon is intentionally not derived from this: the theme ships an
inline emoji data-URI favicon by design
(`website/themes/spooky/layouts/partials/head.html`).
