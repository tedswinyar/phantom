# Folders tree + legend: design spec (phantom-chp)

Produced 2026-09-01 by an unbiased design-critique pass (fresh agent, surveyed
WinDirStat 1.x/2.x, WizTree, QDirStat, DaisyDisk, GrandPerspective, Disk
Inventory X, Baobab, SquirrelDisk, ncdu/gdu/dua) after Ted's direction to
learn from WinDirStat's tree list and type coloring. Companion prior-art
report (diskinv): notes on bead phantom-chp.

## Verified endpoint facts the design builds on

- `GET /scans/{id}/tree` takes ONLY `path` — one level of direct children per
  call (`scans.rs` TreeParams; store SQL is `ORDER BY path`). The CLI's
  `--depth` is client-side recursion. Lazy tree = one fetch per disclosure.
- Dir rows carry aggregated disk/logical sizes but NOT per-dir file counts.
  (True when written; SHOULD-8 below closed this — dir rows now carry
  full-walk fileCount/dirCount, null on pre-v3 rows.)
- Entries exist only for dirs + files >=1 MiB; smaller files fold into dir
  aggregates. Any client-side count is a structural undercount.

## Pane structure

The lower pane becomes ONE tabbed region — `[Folders (default) | Largest
Files | Reclaimable]` — with a persistent right-aligned "reclaim estimate: X"
chip in the tab-bar row. The flat table survives as Largest Files (global
top-N, search, type filter — things a tree can't do). Net pane count 4 -> 3.

```
┌──────────────────────────────────────────────────────────┬───────────┐
│ /Users/ghost ▸ Code ▸ phantom            [breadcrumb bar]  │           │
├──────────────────────────────────────────────────────────┤ Inspector │
│                   TREEMAP (Canvas)                       │  name     │
│                                                          │  Disk     │
│ [■.rs 12.4G][■.o 8.1G][■.mp4 5G][■…][■ other]  ◂legend  │  Logical  │
├──────────────────────────────────────────────────────────┤  % of scan│
│ [Folders][Largest Files][Reclaimable]  reclaim est: 21G  │  Modified │
│ Name            │ % of Parent    │ Disk    │ Logical │ M │  Category │
│ ▾ ▪ target      │ ▓▓▓▓▓▓░░ 62%   │ 12.4 GB │ 12.6 GB │   │  Hardlinks│
│   ▾ ▪ debug     │ ▓▓▓▓▓░░░ 81%   │ 10.0 GB │ 10.1 GB │   │  Path     │
│     ▸ ▪ deps    │ ▓▓▓░░░░░ 55%   │  5.5 GB │  5.6 GB │   │           │
│     (smaller files) ▓░░░░░░  8%   │  0.8 GB │    —    │   │           │
│   ▪ big.dmg ●   │ ▓░░░░░░░ 22%   │  2.7 GB │  2.7 GB │   │           │
└──────────────────────────────────────────────────────────┴───────────┘
(▪ type-color swatch; ● reclaimability category dot)
```

## The tree (Folders tab)

- **Widget: NSOutlineView in an NSViewRepresentable** (plain
  NSTableCellViews; DesignKit tokens bridged via NSColor). Reasons: lazy
  children is the native protocol; programmatic reveal (expandItem +
  scrollRowToVisible) is the WHOLE treemap→tree sync feature and SwiftUI
  Table has no scroll-to-row; free platform behavior (resizable/reorderable
  columns, header context menu, type-select, Option-click expand-all, sort
  descriptors, proven 100k-row perf). Finder's list view IS an outline view.
  Sync logic stays in ScansModel where MockAPIClient tests reach it.
  Fallback if AppKit refused: hand-flattened SwiftUI List (keeps scrollTo,
  forfeits native columns). Never OutlineGroup / Table(children:) — no
  programmatic reveal.
- **Columns**: Name (disclosure + type swatch + name) · % of Parent
  (bar + number) · Disk Size (headline, default sort desc, monospaced) ·
  Logical Size (secondary, hideable via header context menu) · Modified.
  NO file-count column until the wire extension lands (see below).
- **Bars scale to % of parent** (at root level that equals % of scan). Never
  sibling-normalize; never mix %-of-scan into the bar column (% of scan goes
  in the inspector).
- **Residual row**: every expanded dir whose fetched children sum to less
  than its aggregate gets a synthetic "(smaller files)" leaf — size =
  parent.diskSize − Σ(children.diskSize), rendered when > 0; secondary text,
  no swatch, not inspector-selectable. This is the wire's <1MiB folding
  policy made honest; the treemap's faint dir-fill residue is the same
  mechanism in map form — keep both.
- **Lazy load**: fetch on FIRST disclosure; cache `[path: [ScanEntry]]` in
  ScansModel, reset with the rest of per-scan state; root children fetched
  on scan select. Client re-sorts each children array by diskSize desc.
  Inline "Loading…" placeholder child; inline retry row on error. Serialize
  Option-click expand-all cascades, cap fetch concurrency ~4.

## Selection sync

`model.selectedEntryPath` stays the single source of truth.
- Tree row click → selectEntry (inspector + existing treemap stroke).
- Treemap file-tile click → select AND reveal in tree (expand ancestor
  chain, fetching uncached levels; scrollRowToVisible; select).
- Treemap dir click → drill map (existing) AND reveal in tree.
- Tree dir DOUBLE-click → re-root the map (rebuild root stack for arbitrary
  jumps). Single click must NOT re-root (each re-root is a server fetch;
  re-root-on-select is Baobab's most-complained-about behavior).
- Selected path deeper than the layout's requestDepth → no map highlight;
  acceptable, don't auto-re-root.

## Legend + colors

- Cap at **9 distinct hues + grey "other"**; categorical perception tops out
  ~8–12 and thin swatches are the hard case. WinDirStat's unlimited rotating
  palette is its dated part.
- Assign colors PER SCAN by descending per-type disk size (typeTotals
  arrives server-sorted); top 9 extensions take the slots. Colors change
  between scans BY DESIGN — the always-visible legend is what makes that ok
  (state this in a code comment).
- Token change: ordered `Palette.legend[0..<9]` replaces the fileType*
  roles for tiles/rows/legend; redesign hues for tile-size distinguishability
  in both appearances (cyan-vs-mint and brown-vs-orange currently fail).
  `categoryColor` (reclaimability axis) untouched.
- Legend = compact interactive chip strip on the treemap's bottom edge
  (swatch + .ext + size, horizontally scrollable). Chip click → highlight
  matching tiles (dim others) AND set fileTypeFilter on Largest Files.
  Demotes the type-filter popup.

## Prioritized backlog

MUST (the tree feature): tabbed lower pane + estimate chip; NSOutlineView
tree with the 5 columns; lazy loading as specced; residual row; full
selection sync incl. treemap→tree reveal + double-click re-root + wiring the
currently-dead `drillOut()` to Escape and Cmd-Up; %-of-parent bars.

SHOULD: legend strip + per-scan palette; wire extension for per-dir
fileCount/dirCount (persist post-pass, contract bump) then an Items column;
context menus (Reveal in Finder, Copy Path) on tree/table/tiles; Quick Look
on Space; reclaimability dot on categorized tree rows; scroll-triggered
paging replacing Load More + honest partial summary ("first 142 of 9,301");
View menu with shortcuts replacing toolbar toggle spam; "% of scan" in the
inspector.

COULD: /tree?depth= batched prefetch (only if disclosure latency is felt);
tree-hover → map-tile highlight; prefetch top child dirs; dominant-child
bolding; VoiceOver rotor for the map once the tree provides the accessible
hierarchy.

DO NOT: always-on treemap labels (settled); a permanent extension pane; an
unlimited palette; client-side file counts (structural lie); delete actions
anywhere (never-deletes posture survives the WinDirStat inspiration);
OutlineGroup/Table(children:); sibling-normalized bars; map re-root on
single selection; cushion-gradient rendering (dated, and it would repaint
the honest dir-residue mechanism).

## Current-UI defects found during the critique (fix with the MUSTs)

- `ScansModel.drillOut()` is dead code — nothing calls it; breadcrumb is the
  only way back out of a drill (no Escape, no Cmd-Up).
- Zero contextMenu and zero Quick Look in the app target.
- Flat-table summary sums loaded pages only but reads as a total.
- Canvas treemap is one opaque accessibility element; the tree becomes the
  accessible hierarchy.
- Four toolbar toggles is un-Mac; `sparkles` communicates nothing.
