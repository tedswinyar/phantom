---
title: "Features"
---

## Reclaimable: the scan that ends in a plan

Most disk analyzers stop at the picture. Phantom classifies what it finds
and tells you what a cleanup would actually return — and, since 1.1, how
much to trust each suggestion:

- **Regenerable build artifacts** — Rust `target/`, `node_modules`, `.venv`,
  SwiftPM `.build`, Xcode DerivedData, Gradle, Maven, Bazel, Go vendor, and
  twenty-odd more project types. An artifact counts **only beside its
  detection file**: a directory merely *named* `target` doesn't qualify.
- **Tool-managed caches** — stores that belong to a tool with its own
  cleanup verb. Phantom suggests `toolbox clean` or `npm cache clean`, not
  `rm -rf`, because hand-deleting these desyncs the tool's own metadata.
  Opt in and Phantom asks the tool for its own dry-run number
  (`brew cleanup -n`, `docker system df`, `uv cache size`).
- **Model caches** — Ollama, Hugging Face hub, Whisper, PyTorch hub: a
  category of their own because each one is gigabytes and re-downloadable.
- **Cloud-dataloaded files** — files whose logical size dwarfs their disk
  footprint. A cloud-synced folder can show hundreds of megabytes in `du`
  while occupying almost nothing; deleting it locally reclaims almost
  nothing. Phantom flags the divergence instead of counting it as loot.
- **Stale project artifacts** — build trees in projects quiet for 90+ days
  (your threshold). Quiet means the newest source edit AND the newest git
  activity; artifact mtimes never count (build tools re-touch them on every
  run), and a project with neither signal is *unverifiable*, never dormant.
- **Review first / won't regenerate** — the honest remainder. Phantom puts
  it in a separate bucket rather than padding the reclaim estimate.

**Every group states its risk tier and why.** `safe` means a lockfile is
present and the tool rebuilds from it; `caution` means the lockfile is
missing or the rebuild is a big download; `review` is never a suggestion.
Each group also says what getting the bytes back costs (a download, a
compile, nothing) and carries two actions: **Reveal in Finder** and **copy
the suggested command**. Phantom never deletes anything.

## The plan you confirm, and the receipt

A scan can become a **reclaim plan**: the safe groups (or safe + caution,
your call), each path with the bytes deleting it would actually free, and a
shell script that does nothing unless you run it with `PHANTOM_APPLY=1` —
and then moves to the Trash, never `rm`. Rescan afterwards and **verify**
reports, per path, what came back versus what was promised, within a stated
tolerance. An agent gets the same loop through the MCP server and a bundled
skill: scan → plan → you confirm → run → verify.

## Every size is physical

The headline number for every file and directory is its physical footprint —
the blocks it occupies on disk — not its apparent size. The distinction is
not pedantry:

- Hardlink-heavy stores (uv, pnpm) overcount in naive walks; a 17 GB cache
  can free 5 GB because the rest of its blocks are shared with live
  installs. Phantom dedupes hardlinks by `(device, inode)` in every total.
- **APFS clones** overcount the same way: `cp -c`, Time Machine local
  snapshots and Xcode's copies share blocks that `du` counts twice. Phantom
  reads the clone attributes the filesystem keeps and reports three sizes
  for everything: what it occupies (`diskSize`), what deleting it would
  free (`privateSize`), and what is pinned by another copy (`sharedSize`).
  On one developer's `~/Code`, 1.0 overstated by 3.7 GB of clones.
- Cloud-dataloaded files overcount the other way: full logical size, near
  zero blocks. Compressed files (most of `/System`) look like it but are
  not; Phantom tells them apart by the flags, not by a ratio.

Logical size stays visible as a secondary field — the divergence between the
two is itself a signal Phantom uses. And the same number appears everywhere:
app, CLI, and MCP server are held to byte-identical output by an end-to-end
parity gate.

## Where is "System Data"?

The volume's used space is never the sum of your files, and macOS's own
tools disagree about the gap. Phantom decomposes it, read-only: what the
APFS container holds versus this volume alone (the other volumes — System,
Preboot, Recovery, VM — are the difference), how much is **purgeable** (what
Finder's "Available" silently adds), the local Time Machine snapshots, the
other users' homes and whether you can read them, the entries a scan could
not read, and finally **used minus scanned** — the bytes on this volume your
scan did not see. When snapshots are pinning space it names the
`tmutil thinlocalsnapshots` command; it never runs it.

## Is it getting worse?

Every scan is kept, so a root's **history** is already there: a sparkline
beside each scan in the sidebar, a History pane with the growth of the
whole root or broken down by category, top-level folder or file extension,
and a linear forecast — "disk full in N days" — that always travels with
its caveat (it assumes the rate continues, nothing is reclaimed, and this
root alone fills the volume). The Folders tree marks what is **new or grew
since the last scan**, the treemap outlines it, and any two scans of a root
can be compared — which is also how a plan's estimate is proved after the
fact. `phantom diff --since 7d` does the same from a shell.

## Scans you don't have to babysit

Starting a scan returns immediately. Progress — files seen, bytes seen, the
path currently being walked — updates live, and cancel takes effect
mid-walk. Cancelling **discards the partial results**: a cancelled scan
records that it happened and nothing more, because half a walk presented as
an answer is worse than no answer. A scan whose root disappears mid-walk
fails and says so; a scan the app was running when it quit is marked
interrupted on the next launch, never left "running" forever.

Completed scans persist in SQLite, so you can query, compare against your
memory of last time, or feed an agent without re-walking the disk. What
persists is deliberately bounded: every directory, and every file of 1 MiB
or more, gets its own row; smaller files fold into their directory's
totals. The per-type breakdown and the Reclaimable classification are
computed from the **full walk** before that cut, so a million tiny cache
files still show up in the accounting even though they don't each get a
row.

## The treemap

A squarified treemap, laid out server-side at the actual pixel size of your
window — not scaled from a fixed canvas. Click a directory and the map
re-roots there and re-computes the layout for the new subtree. Files are
colored by type; the inspector shows the details of whatever you select,
including why a path was classified the way it was and what deleting it
would free.

## A CLI and an MCP server, not an afterthought

The `phantom` CLI scans, lists top offenders, prints trees, breaks usage
down by file type, reports hotspots, builds and verifies plans, explains a
path, finds stale projects, reads the volume, charts growth and diffs scans
— with `--json` on everything, stable exit codes for scripting, shell
completions and a man page. The MCP server exposes the same data to AI
agents as sixteen tools: `get_volume_status`, `scan_directory`,
`scan_status`, `cancel_scan`, `list_scans`, `find_large_files`,
`get_space_by_type`, `get_treemap`, `get_hotspots`, `explain_path`,
`find_stale_projects`, `plan_reclaim`, `verify_reclaim`, `diff_scans`,
`get_growth`, `health` — each with a typed output schema and annotations
that say which ones write. Install it as a Claude Code plugin (this
repository is the marketplace), a Claude Desktop bundle, or a Cursor
one-click. All three clients read the same scans from the same API with the
same auth.

## Local-only by construction

The API server binds 127.0.0.1, requires a key file that never leaves your
disk, and makes zero outbound calls. There is no telemetry to opt out of.
See the [architecture page](/architecture/) for why that's structural rather
than a settings toggle.
