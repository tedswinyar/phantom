---
title: "Features"
---

## Reclaimable: the scan that ends in a plan

Most disk analyzers stop at the picture. Phantom classifies what it finds
and tells you what a cleanup would actually return:

- **Regenerable build artifacts** — Rust `target/` (with a `Cargo.toml`
  sibling, so a directory merely *named* target doesn't qualify),
  `node_modules`, `.venv`, `dist`, SwiftPM `.build`, Xcode DerivedData.
  Delete them and the tool rebuilds them; the only cost is a slower next
  build.
- **Tool-managed caches** — stores that belong to a tool with its own
  cleanup verb. Phantom suggests `toolbox clean` or `npm cache clean`, not
  `rm -rf`, because hand-deleting these desyncs the tool's own metadata.
- **Cloud-dataloaded files** — files whose logical size dwarfs their disk
  footprint. A cloud-synced folder can show hundreds of megabytes in `du`
  while occupying almost nothing; deleting it locally reclaims almost
  nothing. Phantom flags the divergence instead of counting it as loot.
- **Stale project artifacts** — build trees in projects whose *source*
  hasn't changed in 90+ days. Staleness comes from source-file mtimes at
  full depth, never artifact mtimes (build tools re-touch those on every
  run) and never a depth-capped sample (which once misread a project edited
  that morning as three weeks dormant).
- **Review first / won't regenerate** — the honest remainder. Phantom puts
  it in a separate bucket rather than padding the reclaim estimate.

Each group carries an estimated reclaim and two actions: **Reveal in
Finder** and **copy the suggested command**. Phantom never deletes anything.

## Every size is physical

The headline number for every file and directory is its physical footprint —
the blocks it occupies on disk — not its apparent size. The distinction is
not pedantry:

- Hardlink-heavy stores (uv, pnpm) overcount in naive walks; a 17 GB cache
  can free 5 GB because the rest of its blocks are shared with live
  installs. Phantom dedupes hardlinks by `(device, inode)` in every total.
- Cloud-dataloaded files overcount the other way: full logical size, near
  zero blocks.
- Sparse files diverge in the opposite direction again.

Logical size stays visible as a secondary field — the divergence between the
two is itself a signal Phantom uses. And the same number appears everywhere:
app, CLI, and MCP server are held to byte-identical output by an end-to-end
parity gate.

## Scans you don't have to babysit

Starting a scan returns immediately. Progress — files seen, bytes seen, the
path currently being walked — updates live, and cancel takes effect
mid-walk. Cancelling **discards the partial results**: a cancelled scan
records that it happened and nothing more, because half a walk presented as
an answer is worse than no answer.

Completed scans persist in SQLite, so you can query, compare against your
memory of last time, or feed an agent without re-walking the disk. What
persists is deliberately bounded: every directory, and every file of 1 MiB
or more, gets its own row; smaller files fold into their directory's
totals. The per-type breakdown and the Reclaimable classification are
computed from the **full walk** before that cut, so a million tiny cache
files still show up in the accounting even though they don't each get a
row.

## The Specter Map

A squarified treemap, laid out server-side at the actual pixel size of your
window — not scaled from a fixed canvas. Click a directory and the map
re-roots there and re-computes the layout for the new subtree. Files are
colored by type; the inspector (the **Séance**, if you're feeling thematic)
shows the details of whatever you select.

## A CLI and an MCP server, not an afterthought

The `phantom` CLI scans, lists top offenders, prints trees, breaks usage
down by file type, and reports hotspots — with `--json` on everything and
stable exit codes for scripting. The MCP server exposes the same data to AI
agents: `scan_directory`, `list_scans`, `find_large_files`,
`get_space_by_type`, `get_treemap`, `get_hotspots`. All three clients read
the same scans from the same API with the same auth.

## Local-only by construction

The API server binds 127.0.0.1, requires a key file that never leaves your
disk, and makes zero outbound calls. There is no telemetry to opt out of.
See the [architecture page](/architecture/) for why that's structural rather
than a settings toggle.
