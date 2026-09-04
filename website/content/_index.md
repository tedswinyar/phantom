---
# Landing page copy. Source of truth: docs/positioning.md — edit there first.
# 01-main.png is a real capture (scripts/capture-screenshots.sh). The two
# section images are named-but-uncaptured; the theme renders an explicit
# "Screenshot pending" box until richer interaction captures land after the
# v1.0 manual smoke, at which point the script drops files onto these names.
title: "Phantom"
hero:
  headline: "Your disk is full of ghosts"
  tagline: "Phantom scans your Mac, measures what every file actually occupies on disk, and classifies what's safe to reclaim — without deleting a single byte."
  audience: "For developers whose drives fill up with build artifacts, tool caches, and cloud-synced files"
  cta_primary:
    label: "See how it works"
    url: "#how-it-works"
  cta_secondary:
    label: "Download for macOS"
    url: "/download/"
  requirements: "macOS 14+ · local-only · no telemetry"
  screenshot: "screenshots/01-main.png"
  screenshot_alt: "Phantom's main window: the Specter Map treemap of a scanned checkout, Restless Spirits reclaim groups, and the file list"
  screenshot_title: "The main window"
  screenshot_body: "The newest scan opens on launch: its Specter Map up top, the Restless Spirits reclaim groups with copy-the-command actions, and the largest files beneath."

sections_header:
  title: "A scan that ends in an answer"
  subtitle: "Not another picture of the problem — a classified, sized, safe-to-act-on cleanup plan."
sections:
  - eyebrow: "Reclaimable"
    title: "Phantom tells you what's safe to reclaim, and how"
    body: "Every scan classifies what it finds: regenerable build artifacts (Rust <code>target/</code>, <code>node_modules</code>, DerivedData), tool-managed caches (where the fix is <code>toolbox clean</code>, not <code>rm -rf</code>), cloud files taking no real space, and artifacts of projects you haven't touched in months. Each category comes with an estimated reclaim and a suggested command. Phantom never deletes anything — you stay in charge of the trigger."
    image: "screenshots/02-reclaimable.png"
    image_alt: "The Reclaimable view grouping findings by category with reclaim estimates"
    image_body: "Grouped by category, sized by what deletion would actually free, with Reveal in Finder and copy-the-command actions."
  - eyebrow: "Physical size, everywhere"
    title: "Sizes you can act on, not sizes that flatter"
    body: "Every number in Phantom is physical — the blocks a file really occupies — because apparent size lies exactly where it matters: cloud-synced files can show hundreds of megabytes while occupying nothing, and hardlinked caches promise gigabytes that deletion won't return. Phantom reports what you'd actually get back, dedupes hardlinks, and keeps logical size as the secondary column it deserves to be."
    image: "screenshots/03-scan-progress.png"
    image_alt: "A scan in progress showing files seen, bytes seen, and the current path"
    image_body: "Scans run in the background with live progress — files seen, bytes seen, current path — and a cancel that works mid-flight and discards the partial walk."

integration:
  anchor: "agents"
  eyebrow: "Built for agents"
  title: "Your AI agent can drive it"
  body: "Phantom ships an MCP server, so Claude Code and friends can scan, query, and plan cleanups with the same authority and the same physical-size data as the app and the CLI."
  bullets:
    - "<code>scan_directory</code>, <code>list_scans</code>, <code>find_large_files</code>, <code>get_space_by_type</code>, <code>get_treemap</code>, <code>get_hotspots</code>"
    - "Same key-file auth as every other client"
    - "Errors surface as results, not protocol faults — agents can react"
  footnote: "Works with any MCP-speaking client."
  config_name: ".mcp.json"
  config_hint: "already in the repo"
  config_code: |
    {
      "mcpServers": {
        "phantom": {
          "command": "rust/target/debug/phantom-mcp"
        }
      }
    }

highlights_header:
  title: "Built to stay on your machine"
  subtitle: "A disk scanner sees everything on your disk. Phantom is built so that's the only place it goes."
highlights:
  - icon: "lock"
    title: "No telemetry, no cloud"
    body: "The API server binds loopback only, requires key-file auth, and makes zero outbound calls. Scan data never leaves your Mac."
  - icon: "terminal"
    title: "Scriptable to the bone"
    body: "Stable exit codes, <code>--json</code> everywhere, and a wire format specified down to the byte. Put a scan in a cron job or a shell pipeline."
  - icon: "shield"
    title: "Show and suggest, never delete"
    body: "Phantom classifies and estimates; the delete command is yours to run. Reveal in Finder or copy the suggested command — that's the whole blast radius."
  - icon: "database"
    title: "Scans persist"
    body: "Scan history lives in SQLite with forward-only migrations and verified backups. Query last week's scan without re-walking the disk: directories and files of 1&nbsp;MiB and up persist row-by-row, and type totals and the Reclaimable classification are computed from the full walk before that cut."

features_header:
  title: "Under the hood"
features:
  - icon: "refresh"
    title: "Async scans with progress and cancel"
    body: "Starting a scan returns immediately; progress streams while the walk runs, and cancel actually cancels — no frozen window on a million-file directory. A cancelled scan discards its partial results and records only that it happened: half a walk is never presented as an answer."
  - icon: "chart"
    title: "A treemap that lays out where you look"
    body: "Squarified layout computed server-side at your window's real size. Drill into a directory and the map re-roots and re-lays-out around it."
  - icon: "layers"
    title: "One answer across app, CLI, and agent"
    body: "The e2e gate asserts the app's API, the CLI, and the MCP server report byte-identical data for the same scan — three clients, one truth."

closing_cta:
  eyebrow: "Reclaim your disk"
  title: "Find out what's actually taking the space"
  body: "Scan your home directory, open the Reclaimable view, and see how many gigabytes are one safe command away."
  cta_primary:
    label: "Get started"
    url: "/docs/getting-started/"
  cta_secondary:
    label: "View on GitHub"
    url: "https://github.com/tedswinyar/phantom"
---

Phantom is a native macOS disk-space analyzer: a SwiftUI app, a CLI, and an
MCP server for AI agents, all speaking to one local Rust API that owns the
data. It measures physical disk usage, maps it, and classifies what is safe
to reclaim — and it never deletes anything itself.
