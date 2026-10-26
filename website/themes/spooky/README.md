# spooky theme

A dark, single-product marketing theme for a small macOS tool: landing page,
content pages, and a changelog fed from `content/releases/`.

Two rules hold this theme together, and breaking either one costs a stamped
project real work:

1. **No product copy in the theme.** Headlines, section text, screenshots, and
   CTA labels live in `content/`. The theme only decides layout.
2. **No network at build or render time.** No web fonts, no CDN, no remote
   JS, no analytics. `hugo build` works on a plane, and so does the rendered
   page. Icons are inline SVG and the favicon is an inline data URI.

## Re-skinning

Every colour and font is a site param, so a re-skin is an edit to `hugo.yaml`
and nothing else. Defaults live in one place: the `:root` block at the top of
`assets/css/style.css`, which is executed as a Go template at build time.

```yaml
params:
  logo_mark: "👻"      # emoji used for the nav logo, hero, and favicon
  logo_text: "my-app"  # monospace wordmark beside it
  radius: "16px"
  colors:
    bg: "#0a0a0f"        # page background
    surface: "#12121a"   # cards, code blocks, diagram panels
    border: "#1a1a2e"    # hairlines and dividers
    accent: "#4ade80"    # primary accent: links, buttons, headings
    accent_hover: "#22c55e"
    accent_alt: "#a78bfa" # secondary accent, alternating emphasis
    info: "#60a5fa"      # third accent, used by the architecture diagram
    text: "#e2e8f0"
    text_dim: "#94a3b8"
    text_muted: "#64748b"
  fonts:
    sans: "-apple-system, BlinkMacSystemFont, 'SF Pro Display', sans-serif"
    mono: "'SF Mono', 'Fira Code', monospace"
```

Set only what you want to change; anything omitted falls back to the default.
Param keys must be lowercase or snake_case — Hugo lowercases config keys, so a
camelCase key silently misses its lookup and you get the default instead.

Translucent tints (glows, hover borders, badge fills) are derived from
`accent`, `accent_alt`, and `info` with `color-mix()`, so changing those three
hexes recolours the whole site. Avoid adding raw `rgba()` literals — they
ignore the params and are exactly what makes a theme un-skinnable.

## Layouts

| File | Used for |
|---|---|
| `baseof.html` | Shell: head, nav, footer, screenshot lightbox |
| `home.html` | Landing page, driven entirely by `content/_index.md` front matter |
| `page.html` | Regular content pages |
| `list.html` | Section index pages |
| `changelog.html` | Opt in with `layout: changelog`; renders `content/releases/` |

## Partials

- `head.html` — meta, favicon, fingerprinted stylesheet
- `nav.html` — logo plus the `main` menu
- `footer.html` — the `footer` menu if defined, otherwise `main`
- `screenshot.html` — responsive image with click-to-zoom, **falling back to a
  dashed placeholder when the asset is missing** so a project with no
  screenshots yet still renders a complete page
- `icon.html` — inline SVG by name (`layers`, `shield`, `check`, `refresh`,
  `keyboard`, `install`, `terminal`, `plug`, `database`, `mail`, `chart`,
  `lock`); unknown names fall back to `check`

## Components available to content pages

`markup.goldmark.renderer.unsafe` is on, so content pages can use these
directly. They are here for pages the layouts don't cover:

- **Architecture diagram** — `.arch-diagram-visual` wrapping `.arch-row`,
  `.arch-hub`, `.arch-box` (with `.source`, `.inner`, `.satellite`, `.agent`),
  `.arch-arrow`, and `.arch-db`. Boxes take a `<span>` for a subtitle. This is
  the one place `colors.info` is used. A fenced code block with an ASCII
  diagram is a perfectly good alternative and survives copy-paste better.
- **Placeholder box** — `.placeholder` with `.placeholder-badge`,
  `.placeholder-title`, `.placeholder-body`, if you want a "coming soon" slot
  outside the screenshot partial.

## Landing page front matter

`content/_index.md` supports `hero`, `sections`, `highlights`, `integration`,
and `features`, each optional — delete a key and that band stops rendering.
The page body renders last, inside the closing prose block. See the shipped
`content/_index.md` for a worked example of all five.

Screenshots referenced from front matter are asset paths relative to
`assets/`, e.g. `images/hero.png` means `website/assets/images/hero.png`.
