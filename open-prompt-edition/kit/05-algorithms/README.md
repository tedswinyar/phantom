# 05 — Algorithms

Non-obvious logic an independent implementation must reproduce exactly.
"Non-obvious" means: two reasonable implementers would write it differently
and their outputs would diverge. References: `rust/phantom-core/src/treemap.rs`,
`rust/phantom-core/src/classify.rs`, `docs/reclaimability.md`.

## Port ladder

Given configured port P ≠ 0: attempt to bind `P, P+1, …, P+9` in order,
first success wins, announce it on stdout, else exit nonzero. P = 0 binds an
ephemeral port directly (no ladder). (Spec: `../04-config/config-spec.md`.)

## Persistence aggregation (ADR-0005)

At scan completion, before the entry filter drops sub-1-MiB file rows,
every directory's `diskSize`/`logicalSize` is replaced by the sum over ALL
descendant files at full depth (walking each file's ancestor chain up to the
scan root; the first ancestor outside the scan stops the walk). Consequence:
a persisted directory row's aggregate can exceed the sum of its persisted
children — the difference is the filtered small-file remainder. The treemap
below leans on this.

Pinned by: conformance ("root aggregate == scan total") and
`tests/e2e/run-e2e.sh` (sub/ aggregate includes its filtered file).

## Deterministic orderings

Ties are where divergence hides; every listing has a total order:

| Surface | Order |
|---|---|
| `GET /scans` | `startedAt` descending, then id ascending |
| `/files` sort=size | `diskSize` descending, then `path` ascending |
| `/files` sort=name | `name` ascending, then `path` |
| `/files` sort=path, `/tree` | `path` ascending (unique per scan — total) |
| `/types` | `diskSize` descending, then `fileType` ascending (`types.json` pins a tie) |
| hotspot groups | see the classifier §aggregation below |
| `topPaths` within a group | deduped size descending, then path ascending, capped at 5 |

## Squarified treemap (`GET /scans/{id}/treemap`)

Bruls, Huizing & van Wijk (2000), with these binding choices:

1. **Tree building.** Index persisted entries by `path`; children of a node
   are the entries whose `parentPath` equals its path. The layout root is
   the requested `root=` (default: the scan root).
2. **Node size.** Files: `max(diskSize, 1)` (zero-size nodes would vanish
   and break the row math). Directories: `max(child_sum, storedAggregate, 1)`
   — the persisted aggregate wins when the sub-1-MiB remainder was filtered.
3. **Recursion + emission.** Emit this node's rect, then, unless
   `depth == maxDepth` or the node is a file or childless: sort children by
   size DESCENDING (stable — ties keep the path order the children arrived
   in) and squarify them into this node's rect. The root rect is `depth: 0`
   and exactly fills the requested `(0, 0, width, height)`.
4. **Residual synthesis.** If the children under-sum the node by at least
   0.5% of its size (inclusive), append ONE `residual: true` pseudo-child
   at `node.size − child_sum` before the stable sort (so it lands AFTER
   equal-sized real children) and squarify it like any child; emit it as a
   leaf with the PARENT's path and never recurse into it. Below the
   threshold nothing is synthesized (the sliver stays parent background);
   a childless directory gets none. Wire semantics:
   `../06-interchange/wire-format.md`.
5. **Child normalization — against the parent's own size, not the child
   sum**: each child's target area is `(child.size / max(parent.size,
   child_sum)) × parentArea`. For a filtered tree the children cover only
   part of the directory's true bytes; inflating them to fill the parent
   would misrepresent their share — with the residual in the row the items
   sum to the parent's size and coverage is complete.
6. **Squarify proper.** Process sizes (already descending) into rows: with
   `short` = the remaining rect's shorter side, greedily extend the current
   row while the row's WORST aspect ratio does not get worse
   (`ratio <= best` continues; strictly worse closes the row). Worst ratio
   of a row with total `sum` against side `short`:
   `max((short² × size) / sum², sum² / (short² × size))` over each `size`.
   Lay the closed row along the short side — if the remaining rect is at
   least as wide as tall, the row is a vertical strip of width
   `remainingWidth × (rowSum / remainingArea)` with items stacked top-to-
   bottom, each `(size / rowSum) × remainingHeight` tall; otherwise the
   transpose. Subtract the strip from the remaining rect and repeat.
7. Coordinates are absolute within the requested bounds, floating point.
   Cross-implementation comparison MUST normalize floats (the e2e harness
   rounds to 1e-6) — the 17th decimal digit is not part of the contract.

Pinned by: `tests/fixtures/treemap.json` (shape), conformance +
`tests/e2e/run-e2e.sh` (layout at requested size, re-rooting, maxDepth=0).

## The reclaimability classifier (`GET /scans/{id}/hotspots`)

A pure post-pass over a completed scan's FULL walk (before the persistence
filter): input entries + a `now` timestamp → per-entry categories + the
`HotspotsSummary`. Runs ONCE at scan completion; results persist. Categories
and summary field semantics: `../06-interchange/wire-format.md`. Full rule
rationale (every rule was earned in a real cleanup incident):
`docs/reclaimability.md`.

**Constants** (`classify.rs`): dataloaded ratio 8× and floor 1 MiB; dormant
at ≥ 90 days; git packs surface above 200 MiB (strict >); plain files at
≥ 1 GiB (inclusive); 5 top paths per group.

### The hotspot registry

Hotspot knowledge is a DATA table, one row per hotspot: matcher → category →
hint. Ordering is precedence: the first matching row wins. Two matching
invariants:

- **Component boundaries.** All path matching is path-component aware:
  `node_modules_backup` never matches the `node_modules` rule; a dormant
  `/proj` never marks `/proj-two` stale. Substring matching is a defect.
- **Prove regenerability, don't assume it.** `target/` is regenerable only
  WITH a `Cargo.toml` sibling; generic `build/`/`dist/` only with a
  `package.json` sibling; `.build/` only with `Package.swift`. **A sibling
  outside the scan is unprovable: the rule stays silent** (an entry whose
  parent is not in the scan classifies as None — the classifier NEVER stats
  the filesystem).

**Matcher semantics** (paths are absolute `/`-separated strings; all
matching is COMPONENT-boundary aware — never substring):

- `DirNamed(name[, sibling])` — a directory whose final path component
  equals `name` exactly. When `sibling` is given, a file/dir of that name
  must exist in the SAME parent (checked against the scanned path set;
  parent outside the scan ⇒ unprovable ⇒ no match).
- `DirSuffix(a, b)` — a directory whose path ends with exactly the
  components `…/a/b`.
- `DirNamedWithin(names…, within)` — a directory named one of `names`
  whose parent path contains the component `within` anywhere.
- `GitPackFile(min)` — a FILE named `*.pack` whose parent path ends with
  `…/objects/pack` and whose diskSize is STRICTLY greater than `min`.
- `CloudDataloadedFile` — the per-file override defined above.
- `LargeFile(min)` — any otherwise-unmatched file with diskSize ≥ `min`
  (inclusive). MUST stay the LAST row: precedence is registry order.

**The registry, row by row.** `ruleId` and `label` are wire-visible
(HotspotGroup); `hint` strings are exact bytes — two implementations must
agree on them for the 11-validate parity drill (backtick characters
included). The hint is HUMAN text only; the machine-actionable safe command
rides HotspotGroup's first-class `command` field (null for advice-only
rules) — shape and per-rule values in `../06-interchange/wire-format.md`.
Never parse hints for commands.

| ruleId | label | matcher | category | hint (exact) | command |
|---|---|---|---|---|---|
| `cloud-dataloaded` | `Cloud-dataloaded placeholders` | CloudDataloadedFile | cloudDataloaded | `contents live in the cloud; local blocks are ~0 — deleting frees almost nothing` | null |
| `cargo-target` | `Rust target/ directories` | DirNamed(`target`, sibling `Cargo.toml`) | regenerableArtifact | `` `cargo clean` or delete; the next `cargo build` regenerates it `` | `cargo clean` |
| `node-modules` | `node_modules directories` | DirNamed(`node_modules`) | regenerableArtifact | `` `npm install` / `pnpm install` regenerates it `` | null |
| `python-venv` | `Python virtualenvs` | DirNamed(`.venv`) | regenerableArtifact | `` recreate with `uv venv` / `python -m venv` and reinstall `` | null |
| `swiftpm-build` | `SwiftPM .build directories` | DirNamed(`.build`, sibling `Package.swift`) | regenerableArtifact | `` `swift build` regenerates it `` | `swift package clean` |
| `next-build` | `.next build output` | DirNamed(`.next`) | regenerableArtifact | `` `next build` regenerates it `` | null |
| `js-build` | `JS build/ output` | DirNamed(`build`, sibling `package.json`) | regenerableArtifact | `the package's build script regenerates it` | null |
| `js-dist` | `JS dist/ output` | DirNamed(`dist`, sibling `package.json`) | regenerableArtifact | `the package's build script regenerates it` | null |
| `xcode-derived-data` | `Xcode DerivedData` | DirNamed(`DerivedData`) | cache | `Xcode regenerates it on the next build` | null |
| `library-caches` | `Library/Caches` | DirSuffix(`Library`, `Caches`) | cache | `app caches; apps rebuild them on demand` | null |
| `electron-app-cache` | `Electron app caches` | DirNamedWithin(`Cache`, `Code Cache`, `GPUCache`, `DawnCache`, `CachedData`, `Service Worker`; within `Application Support`) | cache | `Electron/Chromium cache; the app rebuilds it` | null |
| `group-containers` | `Library/Group Containers` | DirSuffix(`Library`, `Group Containers`) | reviewFirst | `shared app-group data; apps can lose state — review per container` | null |
| `dot-cache` | `~/.cache` | DirNamed(`.cache`) | toolManagedCache | `` per-tool clean commands (`uv cache clean`, `pnpm store prune`); hardlinked stores free less than they list `` | null |
| `dot-npm` | `~/.npm` | DirNamed(`.npm`) | toolManagedCache | `` `npm cache clean --force` `` | `npm cache clean --force` |
| `dot-cargo` | `~/.cargo` | DirNamed(`.cargo`) | toolManagedCache | `cargo registry/git caches; prune with cargo tooling, not rm -rf` | null |
| `dot-rustup` | `~/.rustup` | DirNamed(`.rustup`) | toolManagedCache | `` `rustup toolchain uninstall` unused toolchains `` | null |
| `dot-toolbox` | `~/.toolbox` | DirNamed(`.toolbox`) | toolManagedCache | `` use `toolbox clean`, not rm -rf `` | `toolbox clean` |
| `homebrew-cellar` | `Homebrew Cellar` | DirSuffix(`homebrew`, `Cellar`) | toolManagedCache | `` `brew cleanup` / `brew uninstall`, not rm -rf `` | `brew cleanup` |
| `homebrew-cellar-intel` | `Homebrew Cellar (Intel prefix)` | DirSuffix(`local`, `Cellar`) | toolManagedCache | `` `brew cleanup` / `brew uninstall`, not rm -rf `` | `brew cleanup` |
| `cloud-synced-originals` | `Cloud-synced originals (CloudStorage)` | DirSuffix(`Library`, `CloudStorage`) | wontRegenerate | `synced originals; a local delete propagates to the cloud copy` | null |
| `icloud-drive` | `Cloud-synced originals (iCloud Drive)` | DirSuffix(`Library`, `Mobile Documents`) | wontRegenerate | `synced originals; a local delete propagates to the cloud copy` | null |
| `agent-sessions` | `Agent session data (~/.claude/projects)` | DirSuffix(`.claude`, `projects`) | reviewFirst | `agent session history; prune old sessions after review` | null |
| `agent-worktrees` | `Agent worktrees` | DirNamed(`.worktrees`) | reviewFirst | `` worktrees can hold uncommitted work; check `git status` in each `` | null |
| `git-pack` | `Large git pack files` | GitPackFile(> 200 MiB) | reviewFirst | `` repository history; `git gc` / repack or re-clone shallow — review first `` | null |
| `large-file` | `Large files` | LargeFile(≥ 1 GiB) — LAST row | reviewFirst | `big and unclassified; review before deleting` | null |

(Markdown-escaping note: hint cells use `` `…` `` code fencing; the hint
VALUE is the text between the outer fences with single-backtick spans kept
verbatim, leading/trailing space trimmed.)

### The classification pass

1. **Directory roots.** Each directory that matches a dir-matcher row
   becomes a hotspot root; nested roots collapse to the OUTERMOST (an inner
   `node_modules` inside `~/.cache` belongs to the `.cache` group) so
   nothing is counted twice.
2. **Stale upgrade.** A project root is a directory containing `.git`,
   `Cargo.toml`, or `package.json` — except when the marker sits under an
   artifact directory (`target`, `node_modules`, `.build`, `dist`, `build`,
   `.next`, `DerivedData`) or `.git`. A project is DORMANT when the newest
   mtime over its full-depth SOURCE files (excluding artifact dirs and
   `.git` — artifact mtimes lie) is ≥ 90 days before `now`; the boundary is
   inclusive. **Missing evidence is not staleness**: a root with no dated
   source files is unknown, never dormant. A `regenerableArtifact` root
   strictly inside a dormant project upgrades to `staleProjectArtifact`.
3. **Per-entry assignment.** Files: the cloud-dataloaded override fires
   FIRST (`!isDir && logicalSize ≥ 1 MiB && logicalSize ≥ 8 × diskSize` —
   both the ratio and the absolute floor; a placeholder inside
   `node_modules` still frees ~nothing, so it moves to the cloud group
   rather than inflating the regenerable estimate); otherwise the deepest
   governing directory root's category; otherwise the standalone file rules
   (git pack, then large-file catch-all). Directories inherit their
   governing root's category — directory rows under a hotspot root DO carry
   categories. Everything else is None.
4. **Aggregation.** Group key = (registry rule, effective category) — the
   same rule can split into a stale and a non-stale group. Only FILES
   contribute bytes (dirs would double-count). Hardlinks (`nlink > 1`)
   dedupe by `(dev, ino)` — `dev` is part of the key — PER GROUP for the
   group's `diskSize`/`topPaths`, and GLOBALLY for the summary rollups, so
   blocks shared across two hotspots are never promised twice.
   `listedDiskSize` and `fileCount` are the naive per-entry totals.
   `reclaimEstimate` sums globally-deduped disk over the four reclaimable
   categories; `reviewDiskSize` over reviewFirst + wontRegenerate;
   the cloudDataloaded pair over dataloaded files. Groups sort:
   `staleProjectArtifact` first, then category priority (regenerable, tool-
   managed, cache, cloud, review, wont-regenerate), then deduped size
   descending, then `ruleId`.

Pinned by: `tests/fixtures/hotspots-summary.json` (wire shape), 37 unit
tests in `classify.rs` (every registry row, every boundary), integration
tests in `rust/phantom-api/tests/test_scans.rs` (full-walk classification,
dir-row categories, 409 gate), and `tests/e2e/run-e2e.sh` (three-view
parity + the filtered-small-file estimate pin).

## Template for new entries

```markdown
## <name>

<input> → <output>, stated precisely enough that two implementations agree
byte-for-byte. Include the tie-breaking rules; ties are where divergence
hides. Cite the conformance check or fixture that pins it.
```
