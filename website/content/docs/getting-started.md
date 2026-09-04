---
title: "Getting started"
description: "Install Phantom, grant Full Disk Access, run your first scan."
---

## Install

Grab the notarized DMG from
[GitHub Releases](https://github.com/tedswinyar/phantom/releases/latest) and
drag Phantom to Applications. Or build from source — you need macOS 14+,
the Swift toolchain, and Rust (`jq` too if you want to run the test gates):

```bash
git clone https://github.com/tedswinyar/phantom.git && cd phantom
make app && open build/Phantom.app
```

The app bundles and supervises its own API server, and writes its auth key
file on first launch. Nothing to configure.

## Grant Full Disk Access

macOS blocks apps from reading most of your home directory (Desktop,
Documents, Mail, browser data) until you grant **Full Disk Access**. A disk
scanner without it produces confidently wrong totals — directories it cannot
enter count as errors, not as zero-cost truths — so grant it before your
first big scan:

1. Open **System Settings → Privacy & Security → Full Disk Access**.
2. Enable **Phantom** in the list (add it with **+** if it isn't listed).
3. Relaunch Phantom.

Phantom detects the missing grant: when a scan hits unreadable paths without
Full Disk Access, the app shows a callout with an **Open System Settings**
button that lands on the right pane (also in the Help menu any time). macOS
provides no way for an app to grant this itself — System Settings is the
ceiling for every app in this category.

## Your first scan (your first Haunt)

Pick a directory — your home directory is the interesting one — and start a
scan. Scans run in the background: you'll see files seen, bytes seen, and
the path currently being walked, and you can cancel at any point (a
cancelled scan discards its partial results and records only that it
happened). On a large home directory expect a few minutes for the first
full walk.

When it completes:

- **The Specter Map** — the treemap — shows where the bytes physically are.
  Click a directory to re-root the map there; click a file to inspect it.
- **Reclaimable** groups what the scan classified as safe to reclaim —
  regenerable build artifacts, tool-managed caches, cloud-dataloaded files,
  stale project artifacts — with an estimated total and a suggested command
  per group. Phantom never deletes anything; **Reveal in Finder** and
  **copy command** are the only actions.

Every size you see is physical (what the file occupies on disk), which is
why Phantom's numbers can disagree with `du` — and why Phantom's are the
ones deletion will actually honor.

## From the terminal

The same scans are available to scripts:

```bash
phantom scan ~/Code          # start a scan
phantom top                  # largest files and directories
phantom types                # usage by file type
phantom hotspots             # the Reclaimable classification
phantom tree --json          # everything speaks --json
```

Exit codes are stable and documented, so `phantom` behaves in pipelines and
cron jobs.

## Point an AI agent at it

The repository ships a `.mcp.json` wired to Phantom's MCP server, so an
MCP-speaking client (Claude Code and friends) picks it up with no extra
setup and gets the same tools: scan, query large files, fetch the treemap,
fetch hotspots.

## Where to go next

The full developer walkthrough is
[docs/getting-started.md](https://github.com/tedswinyar/phantom/blob/main/docs/getting-started.md)
in the repository, kept current with the code, alongside the architecture
decision records under `docs/adr/` and the rebuildable contract in
`open-prompt-edition/`.
