# Reclaimability: the taxonomy and the measurement rules

Phantom's headline claim is not "here is what's big" — every du clone does
that — it is "here is what you would actually get back, and how to get it
back safely." This document is the contract behind that claim. The
implementation is `rust/phantom-core/src/classify.rs`; every rule below was
earned in a real cleanup incident (dates cite the disk-cleanup playbook and
docs/troubleshooting.md).

## The posture: show and suggest, never delete

Phantom has **no delete API** — not in the core, not over HTTP, not in the
CLI or MCP surface. Classification output is a category plus an
*action hint*: a human-readable suggestion naming the safe tool
(`cargo clean`, `toolbox clean`, `brew cleanup`), never an operation Phantom
performs. Two reasons, both earned:

1. Tool-managed stores corrupt when deleted out from under their owner.
   `rm -rf ~/.toolbox/tools` orphans the `<version>.json` sidecars;
   `toolbox clean` took the same store from 18 GB to 9.5 GB safely
   (2026-08-20).
2. Automated staleness heuristics have deleted live work before: a
   depth-capped mtime check misread a project edited *that morning* as
   3-weeks dormant, and five active projects lost their build trees
   (2026-08-20). A wrong *suggestion* costs a shrug; a wrong *delete* costs
   an afternoon.

## The four measurement rules

### 1. `diskSize` is THE size

`diskSize` (`st_blocks × 512`) is the headline number everywhere;
`logicalSize` is a secondary field. They diverge in both directions: sparse
files (logical ≫ disk) and cloud-dataloaded files, where OneDrive/
CloudStorage reports full logical size while occupying ~0 blocks. `du`
reads apparent size and overstated a 147 MB OneDrive tree whose physical
footprint was 0.14 GB of nothing (2026-08-20). A reclaim estimate built on
logical size promises space that was never occupied.

### 2. Cloud-dataloaded detection needs a ratio AND a floor

A file is a dataloaded placeholder when **both** hold
(`CLOUD_DATALOADED_MIN_RATIO`, `CLOUD_DATALOADED_MIN_LOGICAL`):

- `logicalSize ≥ 8 × diskSize` — the logical claim dwarfs the blocks;
- `logicalSize ≥ 1 MiB` — without an absolute floor, every tiny file whose
  tail block rounds oddly would qualify.

The check is a per-file **override**: a placeholder inside `node_modules`
still frees ~nothing, so it moves to the cloud group instead of inflating
the regenerable estimate.

### 3. Hardlinks: reclaim counts each `(dev, ino)` once

Entries with `nlink > 1` share blocks; deleting one path frees nothing
until the last link goes. "17 GB" of `~/.cache/uv` freed only 5 GB
(2026-08-20). So:

- **Listings** may show every path (`listedDiskSize`).
- **Estimates** dedupe by `(dev, ino)` — per group for group totals, and
  globally for the scan-level `reclaimEstimate`, so blocks shared across
  two hotspots are never promised twice.
- `dev` is part of the key: the same inode number on two devices is two
  files.

### 4. Staleness comes from full-depth SOURCE mtimes

A project root is a directory containing `.git`, `Cargo.toml`, or
`package.json` — except when that marker itself sits under an artifact
directory (every package inside `node_modules` ships a `package.json`).
Staleness is the age of the **newest source-file mtime at full depth**
under the root, excluding artifact dirs (`target/`, `node_modules/`,
`.build/`, `dist/`, `build/`, `.next/`, `DerivedData`) and `.git`:

- Artifact mtimes lie: `cargo sweep` touches `target/` on every run.
- Depth caps lie: a `-maxdepth 3` walk misread a project edited that
  morning as dormant (2026-08-20).
- Missing evidence is not staleness: a project with no dated source files
  is *unknown*, never dormant.

A project is **dormant** at `≥ 90 days` (`DORMANT_AFTER_DAYS`; the boundary
is inclusive and pinned by test). Dormant + regenerable is the best reclaim
candidate there is — it sorts to the top of the list.

## The taxonomy

Seven categories, stored in `entries.category` as the camelCase wire
string. Each carries an action hint; registry rows may sharpen it.

| Category | Meaning | Posture |
|---|---|---|
| `staleProjectArtifact` | Regenerable artifact inside a dormant project | Top of the reclaim list |
| `regenerableArtifact` | A build regenerates it (`target/` with a `Cargo.toml` sibling, `node_modules`, `.venv`, `.build`, `dist`, `.next`) | Safe to reclaim; suggest the build tool |
| `toolManagedCache` | A cache OWNED by a tool (`~/.toolbox`, `~/.cargo`, Homebrew Cellar) | Suggest the tool's own clean command — e.g. "use `toolbox clean`, not rm -rf" |
| `cache` | App/OS cache (`Library/Caches`, DerivedData, Electron caches) | Safe to reclaim; the owner rebuilds it |
| `cloudDataloaded` | Placeholder; contents live in the cloud | Deleting frees ~nothing; excluded from the estimate |
| `reviewFirst` | Big and unclassified, or possibly holding state (git packs > 200 MB, Group Containers, agent worktrees, plain files ≥ 1 GiB) | Shown, never suggested |
| `wontRegenerate` | Deleting loses data (CloudStorage / iCloud Drive **originals** — a local delete propagates to the cloud) | Never reclaimable |

Only `staleProjectArtifact`, `regenerableArtifact`, `cache`, and
`toolManagedCache` count toward `reclaimEstimate`.

## The hotspot registry

Hotspot knowledge is a **data table** (`REGISTRY` in `classify.rs`), one
row per hotspot: matcher → category → hint. Adding a hotspot is adding a
row, not writing a function. Two matching invariants:

- **Component boundaries.** All path matching is path-component aware:
  `node_modules_backup` never matches the `node_modules` rule, and a
  dormant `/proj` never marks `/proj-two`'s artifacts stale. Substring
  matching is a review finding.
- **Prove regenerability, don't assume it.** `target/` is regenerable only
  WITH a `Cargo.toml` sibling; a generic `build/` or `dist/` only with a
  `package.json` sibling (this repo's own `build/Phantom.app` must never
  classify as regenerable). If the sibling is outside the scan, the claim
  is unprovable and the rule stays silent. This covers scanning an artifact
  directory DIRECTLY (root = `…/target`): the root's parent is outside the
  scan, so sibling-gated rules classify it as nothing, while sibling-free
  rules (`node_modules`) still classify the root — pinned by test.

Nested hotspots collapse to the outermost root so nothing is counted
twice; the first matching row wins, and the plain-large-file catch-all is
pinned (by test) as the registry's last row.

## Output

`classify(entries, now)` is a pure post-pass: per-entry category
assignments plus a `HotspotsSummary` — groups (rule, category, hint,
hardlink-deduped `diskSize`, naive `listedDiskSize`, top paths) and the
scan-level rollups (`reclaimEstimate`, `reviewDiskSize`, and the
dataloaded logical-vs-disk pair that quantifies the du-lie). The wire
shape is camelCase at every depth, pinned by
`tests/fixtures/hotspots-summary.json`.
