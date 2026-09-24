# Wire format — the exact bytes

This document is the authority for every byte that crosses an implementation
boundary. When it disagrees with code, one of them gets fixed and `VERSION`
gets bumped.

## The Five Rules

Inherited from the template's lineage (specter's interop guide), each paid
for by a real cross-implementation bug:

1. **Standard names are not wire formats.** "ISO 8601" is a family of ~20
   formats; "UUID" can be any case; "camelCase" is ambiguous across
   Pydantic/serde/Codable. For every field, write down the exact byte
   sequence — this file does.
2. **Specify the full round-trip.** Client sends → server stores → server
   returns. If ANY step changes casing, encoding, or structure, the
   round-trip breaks. Opaque pass-through fields are especially dangerous.
3. **Test nested objects at full depth.** Top-level casing gets tested
   naturally; nested objects escape scrutiny. Conformance fixtures must
   include deeply nested examples when the schema grows them.
4. **Two implementations, one database.** Create with A, read with B, modify
   with B, read with A. This repo runs that as a day-zero gate
   (`tests/e2e/run-e2e.sh`: CLI vs raw HTTP vs MCP).
5. **Datetime acceptance must be generous.** Producers differ (Python emits
   6 fractional digits, Swift/JS emit 3, some emit 0 or numeric offsets).
   Decode all of them; encode ONLY the canonical form.

## Datetimes

- **Canonical encode** (every implementation MUST produce exactly this):
  `2026-03-17T14:30:00.123456Z` — 6 fractional digits, `T` separator, `Z`
  suffix. Reference: `rust/phantom-core/src/wire_time.rs`,
  `swift/Sources/PhantomCore/WireDate.swift`.
- **Generous decode** (every implementation MUST accept all of):

  | Producer | Example |
  |---|---|
  | canonical / Python | `2026-03-17T14:30:00.123456Z` |
  | Swift / JavaScript | `2026-03-17T14:30:00.123Z` |
  | fractionless | `2026-03-17T14:30:00Z` |
  | numeric offset | `2026-03-17T14:30:00.123456+00:00`, `…+01:00` |

- Precision: microsecond. **Producers MUST emit exactly 6 fractional
  digits.** Decoders MUST accept more (7+), but sub-microsecond fidelity is
  NOT guaranteed and MUST NOT be relied on: an implementation may round or
  truncate the excess to microseconds, and two implementations may disagree
  by up to 1µs on such out-of-spec input (Rust's integer-nanosecond backing
  truncates; Swift's `Date` is Double-backed and rounds — they cannot be
  made byte-identical on 7+ digits, and the contract does not require it).
  The shared fixture `tests/fixtures/datetime-variants.json` pins agreement
  for all in-spec (≤6-digit) variants, cross-checked by both the Rust and
  Swift decode test suites.
- **Year is exactly 4 digits** (`\d{4}`). The last representable canonical
  instant is `9999-12-31T23:59:59.999999Z`; encoders MUST saturate there
  rather than emit a 5-digit year (a rounding carry at that boundary once
  produced `10000-…` in Swift — now clamped).
- Known trap: Foundation's `ISO8601DateFormatter`/ICU truncates fractional
  seconds to milliseconds both ways; the Swift implementation handles the
  fraction manually for this reason. Do not "simplify" it back.

## UUIDs

- Encode: lowercase, hyphenated (`e7ae86e2-308b-444c-8a3d-cd21467ab442`).
- Decode: accept any case, both in JSON bodies and URL paths. A wrong-case
  UUID is the same resource, not a 400.

## JSON conventions

- Keys are **camelCase at every nesting depth**.
- Nullable fields are **present-as-null**, never absent:
  `"finishedAt": null`, `"category": null`. **This binds the ENCODER, not just the
  decoder** — and language defaults will betray you: Swift's synthesized
  `Codable` uses `encodeIfPresent` and silently omits nil keys (the reference
  implementation shipped that bug until 2026-08-19; the fix is a hand-written
  `encode(to:)` with explicit `encodeNil`, pinned by a test that fails
  against the derived conformance). If your language "helpfully" drops
  nulls, write the encoder by hand and pin it.
- Servers reject unknown fields on write DTOs (422), so a client sending
  snake_case keys fails loudly instead of losing data (Rule 2).
- Arrays that are empty are `[]`, never `null`.

## Wire types — scan domain

All under the Five Rules: camelCase at every depth, nullable
present-as-null, canonical datetimes out / generous in, lowercase UUIDs out /
any case in. The exact bytes live in `tests/fixtures/` (`scan-running.json`,
`scan-complete.json`, `entry.json`, `entry-dir.json`, `treemap.json`,
`types.json`, `hotspots-summary.json`) and are executed by the Rust unit
tests, the Swift unit tests, and the conformance harness.

### Scan (wire view)

Everything the scan endpoints return is the `Scan` fields **plus a
`progress` key**: a live counters object while the scan runs, `null` once it
is terminal — present-as-null, never absent.

Running (`scan-running.json`):

```json
{
    "id": "0b54b774-19a1-4373-a423-77aa93e40e5b",
    "rootPath": "/Users/ghost",
    "status": "running",
    "startedAt": "2026-03-17T14:30:00Z",
    "finishedAt": null,
    "totalDiskSize": 0,
    "totalLogicalSize": 0,
    "fileCount": 0,
    "dirCount": 0,
    "errorCount": 0,
    "unreadablePaths": [],
    "totalPrivateSize": 0,
    "totalSharedSize": 0,
    "progress": {
        "filesSeen": 1337,
        "bytesSeen": 987654321,
        "currentPath": "/Users/ghost/Library/Caches/deep/file.bin"
    }
}
```

Terminal (`scan-complete.json`) has real totals, a non-null `finishedAt`,
and `"progress": null`. (The running fixture's fractionless `startedAt`
deliberately pins generous DECODE; encoders still emit the canonical
6-digit form.)

| Field | Type | Nullable | Notes |
|---|---|---|---|
| `id` | UUID | no | server-generated |
| `rootPath` | string | no | the directory scanned, as requested (trimmed) |
| `status` | string | no | `running` \| `complete` \| `cancelled` \| `failed` — exactly these lowercase strings; anything else is a decode error |
| `startedAt` | datetime | no | canonical form |
| `finishedAt` | datetime | **yes** | null until terminal |
| `totalDiskSize` | integer | no | sum of file diskSize (st_blocks × 512), hardlink-deduped — an inode sharing `(dev, ino)` across several links counts ONCE per scan. THE headline number; 0 while running and for cancelled/failed |
| `totalLogicalSize` | integer | no | secondary, kept for cloud-dataloaded detection (logical ≫ disk); deduped by the same rule |
| `fileCount` | integer | no | |
| `dirCount` | integer | no | |
| `errorCount` | integer | no | entries skipped as unreadable (permissions, races) |
| `unreadablePaths` | array | **yes** | capped SAMPLE (first 100) of the entries behind `errorCount`, each `{path, reason}` with `reason` the OS error text — the count stays the truth. `[]` until the scan completes and for cancelled/failed scans; null == not recorded (rows persisted before schema v4 — same null-vs-empty contract as entry counts) |
| `totalPrivateSize` | integer | **yes** | what deleting the whole tree would ACTUALLY free — the root directory's `privateSize` (see "Three sizes" below); ≤ `totalDiskSize`. 0 while running; null == not recorded (rows persisted before schema v5) |
| `totalSharedSize` | integer | **yes** | bytes under the root that hard links / clones outside it, or snapshots, keep pinned — the root's `sharedSize`. Same null contract |
| `failureReason` | string | **yes** | (1.1.0) why a `failed` scan failed: the walker's error text, or `interrupted: …` for a scan the server was running when it stopped (running rows are persisted at start since schema v6 and marked on the next cold start). Null for every other status; absent in pre-1.1 bodies, which decoders treat as null. Human text — show it, do not parse it |
| `progress` | object | **yes** | live while running, null once terminal |

`progress` fields (all non-null while present): `filesSeen` (integer),
`bytesSeen` (integer, DISK bytes, hardlink-deduped — consistent with every
other total, so it converges on `totalDiskSize`), `currentPath` (string).

### Three sizes (v1.1)

Every entry and every hotspot group carries three numbers, and they answer
three different questions. Getting this wrong is THE way a disk analyzer
lies (the competitive sweep found every peer telling at least one of them):

| Field | Question | Model |
|---|---|---|
| `diskSize` | how much is allocated? | `du`: st_blocks × 512, every sharing group — a hardlinked inode OR an APFS pure-clone stream — charged ONCE per scan to its first reference in walk order. THE headline size |
| `privateSize` | what would deleting THIS free right now? | deletion: a file's blocks not shared with any other file or snapshot (APFS `ATTR_CMNEXT_PRIVATESIZE`), forced to 0 while other hard links exist; a directory's is the rollup where a sharing group counts only if EVERY reference to it is inside the subtree AND inside the scan |
| `sharedSize` | what is pinned by something else? | `diskSize − privateSize` for a file; for a directory the allocation of every group also referenced outside it, plus the shared part of partially-cloned files |

`privateSize ≤ diskSize` always. A `cp -c` / Finder-Duplicate copy has
`diskSize` equal to the original (st_blocks reports the full allocation on
every clone) and `privateSize` 0 — the v1.0 "reclaimable" that freed
nothing. A file captured by a Time Machine local snapshot has `privateSize`
0 until the snapshot thins. `reclaimEstimate` and hotspot `privateSize` are
sums of private bytes; `diskSize` stays the size shown as size.

Honest limits, once: a MODIFIED clone has its own `cloneId`-less stream yet
still shares most blocks with its origin; nothing per-file says with whom,
so it counts at full `diskSize` and its `privateSize` is exactly the
rewritten blocks (the rest reads as shared even when the origin sits in the
same directory). A pure-clone group is assumed to own its blocks (an
over-estimate when a modified sibling exists elsewhere).

### ScanEntry

```json
{
    "path": "/Users/ghost/Code/phantom/Cargo.lock",
    "parentPath": "/Users/ghost/Code/phantom",
    "name": "Cargo.lock",
    "isDir": false,
    "diskSize": 49152,
    "logicalSize": 47811,
    "modifiedAt": "2026-03-17T14:30:00.123Z",
    "fileType": "lock",
    "category": null,
    "nlink": 1,
    "dev": 16777233,
    "ino": 42424242,
    "fileCount": null,
    "dirCount": null,
    "privateSize": 0,
    "sharedSize": 49152,
    "cloneId": 42424242,
    "flags": ["mayShareBlocks", "sharesAllBlocks"]
}
```

(The fixture file is a pure APFS clone: full allocation, nothing private,
a clone-group id, both clone flags.)

| Field | Type | Nullable | Notes |
|---|---|---|---|
| `path` | string | no | absolute; unique within a scan |
| `parentPath` | string | **yes** | null exactly for the scan root |
| `name` | string | no | final path component |
| `isDir` | boolean | no | |
| `diskSize` | integer | no | st_blocks × 512. Persisted DIRECTORY rows carry the aggregate over ALL descendant files (including ones below the 1 MiB persistence threshold), not zero. Aggregates are hardlink-deduped: an inode's bytes land in the FIRST link's ancestors (walk order — deterministic, the walk is sorted), and only that first link's file row is persisted, so children can never out-sum a parent |
| `logicalSize` | integer | no | same aggregation and dedup rules for directories |
| `modifiedAt` | datetime | **yes** | |
| `fileType` | string | **yes** | lowercased extension; null for directories and extensionless files |
| `category` | string | **yes** | reclaimability category, stamped by the classifier at scan completion; null == ordinary content. Exactly one of the enum strings below — anything else is a decode error |
| `nlink` | integer | no | hardlink count; entries sharing (`dev`, `ino`) with `nlink` > 1 are one physical file. `nlink` > 1 on a persisted row also warns that deleting this path alone may free nothing — other links (possibly outside the scan) still pin the blocks. Directory rows carry the filesystem's answer (APFS: 1), not st_nlink's subdirectory count |
| `dev` | integer | no | st_dev — NOTE: unified across an APFS volume group (`/`, `/Users` and `/System/Volumes/Data` report the same value), so it cannot detect the system↔data boundary; the `mountPoint` flag does |
| `ino` | integer | no | |
| `fileCount` | integer | **yes** | directory rows: descendant FILES at full depth, aggregated server-side from the FULL walk (sub-1-MiB files count even though their rows are never persisted — counting fetched children is a structural undercount). File rows: always null. Also null on directory rows persisted before schema v3 (null = "not recorded", distinct from `0`) |
| `dirCount` | integer | **yes** | same contract for descendant DIRECTORIES at full depth, EXCLUDING the entry itself |
| `privateSize` | integer | **yes** | "Three sizes" above. Files: 0 for a hard link (`nlink` > 1) or a pure clone, the rewritten blocks of a modified clone, `diskSize` for an ordinary file (or when the filesystem cannot say). Directory rows: the subtree rollup. null == not recorded (rows persisted before schema v5) |
| `sharedSize` | integer | **yes** | `diskSize − privateSize` on files; the pinned allocation on directories. Same null contract |
| `cloneId` | integer | **yes** | APFS clone-group id (`ATTR_CMNEXT_CLONEID`) when the file shares ALL its blocks with ≥ 1 other file; every member carries the same id and the group is charged once per scan (the `(dev, ino)` rule, by clone id). null == not a pure clone (a modified clone has its own stream), or the filesystem cannot say, or pre-v5. Directories: always null |
| `flags` | array of string | **yes** | filesystem facts, emitted in this fixed order: `mayShareBlocks` (EF_MAY_SHARE_BLOCKS), `sharesAllBlocks` (EF_SHARES_ALL_BLOCKS — a pure clone), `purgeable` (EF_IS_PURGEABLE), `sparse` (EF_IS_SPARSE), `dataless` (SF_DATALESS — a cloud placeholder whose contents are not local), `firmlink` (SF_FIRMLINK — followed by the walk), `mountPoint` (DIR_MNTSTATUS_MNTPOINT — not descended unless `crossVolumes`), `compressed` (UF_COMPRESSED — decmpfs; logical ≫ disk without being a cloud placeholder, and its clone attributes are meaningless so `privateSize` is its allocation). `[]` when none. Decoders MUST ignore unknown strings (a newer server may add one). null == not recorded (pre-v5) |

`entry-dir.json` pins the null-heavy directory case (`parentPath`,
`modifiedAt`, `fileType`, `category`, `cloneId` all present-as-null; `flags`
`[]`) and the count fields' populated case; `entry.json` (a file row) pins
the counts' null-and-present case and the populated clone case — for these
fields the fixtures' null roles deliberately invert. `entry-dir.json`'s counts (4200 files, 309 dirs)
cohere with `scan-complete.json` (`fileCount` 4200, `dirCount` 310
INCLUDING the root) to pin the excluding-self rule.

#### The category enum

The camelCase wire string is ALSO the stored database string — one string,
no mapping layer. Reference: `rust/phantom-core/src/classify.rs`
(`Category`); semantics in `../05-algorithms/`.

| Wire string | Meaning |
|---|---|
| `regenerableArtifact` | a build regenerates it (`target/` beside a `Cargo.toml`, `node_modules`, `.venv`, `build/` beside a `build.gradle`, …) |
| `cache` | app/OS cache; the owner rebuilds it on demand |
| `toolManagedCache` | a cache OWNED by a tool that must do its own cleanup (`~/.cargo`, Homebrew Cellar, `~/.cache/uv`) |
| `modelCache` | downloaded AI model weights (Ollama, Hugging Face hub, Whisper, PyTorch hub, vLLM, Triton) — re-downloadable, gigabytes each (v1.1) |
| `cloudDataloaded` | cloud placeholder: the walker's `dataless` flag (or, for rows without flags, big logical size with ~zero blocks) |
| `staleProjectArtifact` | regenerable artifact inside a dormant project — top of the reclaim list |
| `reviewFirst` | big and unclassified, or possibly holding un-backed-up state |
| `wontRegenerate` | deleting loses data (cloud-synced originals) |

Only `regenerableArtifact`, `cache`, `toolManagedCache`, `modelCache`, and
`staleProjectArtifact` count toward `reclaimEstimate` below;
`cloudDataloaded` is EXCLUDED (deleting a placeholder frees ~nothing).
Decoders MUST tolerate an unknown category string (render it as ordinary
content); `modelCache` was added in 1.1.0.

### TreemapLayout / TreemapRect

`treemap.json` pins the full nested shape (Rule 3: camelCase and
present-as-null verified INSIDE the nested rects, not just at the top).

```json
{
    "rootPath": "/Users/ghost/Code",
    "totalSize": 4194304,
    "rects": [
        {
            "path": "/Users/ghost/Code",
            "name": "Code",
            "size": 4194304,
            "x": 0.0,
            "y": 0.0,
            "width": 800.0,
            "height": 600.0,
            "depth": 0,
            "isDir": true,
            "fileType": null,
            "residual": false
        }
    ]
}
```

| Rect field | Type | Nullable | Notes |
|---|---|---|---|
| `path`, `name` | string | no | |
| `size` | integer | no | diskSize, aggregated for directories (hardlink-deduped like every persisted size) |
| `x`, `y`, `width`, `height` | number | no | absolute coordinates within the requested layout bounds |
| `depth` | integer | no | root rect is 0 |
| `isDir` | boolean | no | |
| `fileType` | string | **yes** | |
| `residual` | boolean | no | ALWAYS present (false on every real rect); true marks a synthesized pseudo-tile, below |

`rects` is `[]` (never null) for a scan with no persisted entries.

**Residual pseudo-tiles.** A directory's persisted children can under-sum
its aggregate (the sub-1-MiB persistence folding): the layout normalizes
children against the directory's TRUE size, so without help the remainder
renders as a bare parent slab that reads as a bug. When the shortfall is at
least **0.5% of the directory's size** (inclusive; smaller slivers are
suppressed — under the folding rule almost every directory has SOME
shortfall, and invisible slivers would multiply the rect count), the server
synthesizes exactly ONE pseudo-child per directory, squarified in size
order like any child:

- `residual: true`, `name: "smaller files"`, `isDir: false`,
  `fileType: null`, `depth` = parent depth + 1;
- `size` = directory size − Σ(children sizes);
- **`path` is the PARENT directory's path** — a client hit-test on the
  tile resolves to the parent (note for Identifiable-style clients: `path`
  is therefore NOT unique across rects; key by `(path, residual)`).
- Layout never recurses into a residual, and a directory with NO persisted
  children gets none (its own tile already reads as occupied).

The fixture's residual rect pins the shape; its sizes cohere (4 MiB root =
3 MiB of children + the 1 MiB residual).

### FileTypeTotal

Returned by `GET /scans/{id}/types` as a bare array, largest `diskSize`
first, ties broken by type name. `types.json` pins the shape AND the order
(it carries a deliberate size tie broken by name, and the null-type bucket).

```json
{"fileType": null, "diskSize": 42, "fileCount": 1}
```

`fileType: null` is the no-extension bucket; directories are excluded.
Hardlink-deduped: an inode's bytes land in its FIRST link's type bucket;
further links still add to `fileCount` (it counts directory entries, not
inodes), so a bucket's `fileCount` can exceed what its `diskSize` implies.

### ScanDiff

Returned by `GET /scans/{id}/diff/{other}` as an OBJECT. Positional: the
path's first id is `scanA` ("before"), the second is `scanB` ("after"), and
every delta reads **B − A** — a positive `diskDelta` means B is bigger.
Nothing checks timestamps; both ids are echoed so a reader can always tell
which way the arrow points. Both scans must be `complete` and cover the same
`rootPath` (a cancelled/failed scan persists no entries; a cross-root diff is
meaningless) — else the route errors (409 for non-complete, 400 for a root
mismatch). `scan-diff.json` pins the shape.

```json
{"path": "/Users/ghost/Code/new-project", "before": null, "after": 8388608, "delta": 8388608}
```

| Field | Type | Nullable | Notes |
|---|---|---|---|
| `scanA` | UUID | no | the "before" side (first path id) |
| `scanB` | UUID | no | the "after" side |
| `scanAStartedAt` | datetime | no | when scanA started (canonical form) — echoed so direction is detectable |
| `scanBStartedAt` | datetime | no | when scanB started |
| `reversedChronology` | boolean | **yes** | `true` when scanA started AFTER scanB (positional order is reverse-chronological, so every delta's sign is inverted from "what changed over time" — the trap when a caller feeds `list_scans`' newest-first order in directly); null when the order is natural |
| `rootPath` | string | no | the shared root; compared with trailing-slash and macOS symlink (`/tmp` vs `/private/tmp`) tolerance, not raw string equality |
| `diskDelta` | integer (signed) | no | B.totalDiskSize − A's; hardlink-deduped like the totals; exact |
| `logicalDelta` | integer (signed) | no | same for logical size |
| `fileCountDelta` | integer (signed) | no | B − A |
| `dirCountDelta` | integer (signed) | no | B − A |
| `errorCountDelta` | integer (signed) | no | B − A |
| `grown` | array | no | directories that got bigger, largest growth first (ties by path); capped at the top 20, floored at 1 MiB per directory — HOTSPOTS OF CHANGE, not a ledger |
| `freed` | array | no | directories that shrank, largest shrink first (same cap/floor) |

Each `grown`/`freed` element is a `DiffEntry`: `path` (string), `before`
(integer, **null** if the directory has no row in A — it did not exist, or
its subtree was under the 1 MiB directory rule there), `after` (integer,
**null** if it has no row in B), `delta` (signed integer, `after − before`
with an absent side counted as 0 — so a directory crossing 1 MiB between the
scans reads as created/deleted, right to within 1 MiB). Signed deltas saturate at
the i64 range rather than wrapping — a defensive guard only: every persisted
magnitude is clamped to i64::MAX on write (1.1.0, phantom-ap6), so the
difference of two stored values always fits and the saturation branch is
unreachable from persisted data (phantom-ars). The top-level deltas are
exact; the lists are a capped sample of the biggest per-directory movements.

**Integer limits (1.1.0).** Sizes and counts are stored as SQLite INTEGER,
clamped at i64::MAX (9.2 EB) rather than wrapped — a hostile `st_blocks`
cannot sort first or fail the 1 MiB persistence filter. Identifiers (`dev`,
`ino`, `cloneId`) are bit-cast and round-trip exactly; a value above
i64::MAX appears negative inside the database and correct on the wire.
Human formatting has PB and EB rungs so a clamped total reads "9.2 EB".

### HotspotsSummary / HotspotGroup

Since 1.1.0 (Phase 3) the summary ALSO carries `projects`: an array, one
element per project root the staleness rule evaluated, sorted by artifact
bytes descending then root — `root` (string), `lastActivityDays` (integer,
**nullable**: null == unverifiable, never stale), `dormant` (boolean, at the
scan's own threshold), `artifacts` (array of `{ruleId, path, category,
riskTier, diskSize}` — the hotspot roots directly inside the project,
biggest first). Summaries persisted before Phase 3 have no key; decoders
treat that as `[]`. `GET /scans/{id}/stale` re-thresholds this array.

Returned by `GET /scans/{id}/hotspots` as an OBJECT (the one results surface
that is not a bare array). `hotspots-summary.json` pins the full nested
shape (Rule 3), including a group whose `listedDiskSize` exceeds its
`diskSize` — the hardlink gap made visible — a `privateSize` (1 GiB) far below its
`diskSize` (5 GiB): the venvs outside the store pin the rest — and (1.1.0) a
group carrying a `toolEstimate` (the Homebrew Cellar with `brew cleanup -n`'s
number) beside groups where it is null.

```json
{
    "groups": [
        {
            "ruleId": "cargo-target",
            "label": "Rust target/ directories",
            "category": "staleProjectArtifact",
            "hint": "`cargo clean` or delete; the next `cargo build` regenerates it",
            "command": "cargo clean",
            "riskTier": "safe",
            "why": "Cargo build output beside a Cargo.toml; `cargo build` recreates it; a lockfile pins the dependency versions; the project's newest source edit and git activity are at least 90 days old.",
            "rebuildCost": {"kind": "compile", "estimate": "re-compile ≈ 17.2 GB of build output"},
            "toolEstimate": null,
            "diskSize": 17179869184,
            "listedDiskSize": 17179869184,
            "privateSize": 17179869184,
            "logicalSize": 18179869184,
            "fileCount": 5120,
            "topPaths": ["/Users/ghost/Code/dormant/target"]
        }
    ],
    "reclaimEstimate": 20401094656,
    "reviewDiskSize": 0,
    "cloudDataloadedLogicalSize": 154140672,
    "cloudDataloadedDiskSize": 147456
}
```

Summary fields (none nullable; empty `groups` is `[]`, never null):

| Field | Type | Notes |
|---|---|---|
| `groups` | array of HotspotGroup | sorted: stale project artifacts first, then category priority, then `riskTier` (safe first), then deduped size descending, then `ruleId` |
| `reclaimEstimate` | integer | Σ `privateSize` across the five reclaimable categories ONLY, settled GLOBALLY — a sharing group counts once, and only if every reference to it lies inside the reclaimable set (v1.1: this is what deletion would free, NOT Σ `diskSize`; the fixture's 16 + 1 + 2 GiB) |
| `reviewDiskSize` | integer | deduped disk across `reviewFirst` + `wontRegenerate` — visible, never suggested |
| `cloudDataloadedLogicalSize` | integer | what dataloaded placeholders CLAIM… |
| `cloudDataloadedDiskSize` | integer | …versus the blocks they actually occupy |

Group fields (`command` and `toolEstimate` nullable, present-as-null;
everything else non-null):

| Field | Type | Notes |
|---|---|---|
| `ruleId` | string | stable registry key (survives label edits; safe to pin in clients) |
| `label` | string | human name of the hotspot kind |
| `category` | string | the category enum above |
| `hint` | string | human advice — illustrative text only; see below |
| `command` | string \| null | the ONE safe, copy-runnable cleanup command, or null when none honestly exists |
| `riskTier` | string | `safe` \| `caution` \| `review` (1.1.0). The rubric (`../05-algorithms/`): `safe` = regenerable with its lockfile present (or a kind with no lockfile concept), or a cache the owner repopulates; `caution` = regenerable without its lockfile or with a failed verification, a tool-managed cache (use the tool's command), or a model cache (gigabytes to re-download); `review` = reviewFirst / wontRegenerate / cloudDataloaded. Decoders MUST treat an unknown tier as NOT safe. A summary persisted before 1.1.0 has no tier: decode it as `review` |
| `why` | string | ONE sentence, ending in `.`: what this is, what brings it back, and what the tier rests on (lockfile present / missing / verified / failed; project dormant at the threshold). Human text; wording may change — do not parse |
| `rebuildCost` | object | `{kind, estimate}`: `kind` ∈ `download` \| `compile` \| `none` (nothing to rebuild — or nothing can; the tier says which); `estimate` is a human line (`re-download ≈ 2.1 GB`, `re-compile ≈ 17.2 GB of build output`, `none — repopulated on demand`, `not applicable — nothing regenerates this`) |
| `toolEstimate` | object \| null | the owning tool's OWN dry-run number, present only when the scan asked (`toolEstimates: true`) and the tool exists at a fixed install path: `{tool, command, reclaimableBytes, note}` — `command` is the exact read-only command that ran; `note` says what the number covers. Null otherwise |
| `diskSize` | integer | deduped physical bytes (du model): every hardlinked inode and every pure-clone stream counted once within the group. THE size |
| `listedDiskSize` | integer | naive per-entry sum; exceeds `diskSize` when hardlinks or clones share blocks |
| `privateSize` | integer | what deleting the group's paths would ACTUALLY free: a sharing group counts only if every reference to it is inside this group (and inside the scan); ungrouped files contribute their kernel-reported private bytes. ≤ `diskSize`. THE number to promise |
| `logicalSize` | integer | secondary |
| `fileCount` | integer | files in the group, over the FULL walk (files below the 1 MiB persistence threshold count here even though they have no entry rows) |
| `topPaths` | array of string | the group's roots worth acting on, biggest first: every root whose private bytes reach 1 GiB, never fewer than 5 (the largest by disk fill the floor) and never more than 25. 1.1.0 listed a flat 5; a scan persisted by 1.1.0 keeps its 5 |

**`command` vs `hint`.** Phantom never deletes; `command` is the ONE safe,
copy-runnable cleanup command for the group (`cargo clean`,
`brew cleanup`), and it is **null whenever no such command honestly
exists**: advice-only rules (a `git status` check is not a cleanup),
mixed-tool caches where the rule cannot know the tool, rules whose only
"cleanup" is deleting the directory itself, and every `reviewFirst` /
`wontRegenerate` / `cloudDataloaded` group. `hint` is purely illustrative
human text — backticks inside it are typography, never semantics, and its
wording may change in any release. **Do not parse hints**; bind
copy-the-command affordances to `command` and hide them when it is null.

### ReclaimPlan (1.1.0)

Returned by `POST /scans/{id}/plan` (201) and `GET /plans/{id}` as an OBJECT.
`reclaim-plan.json` pins the shape. Every item IS a `HotspotGroup` the
classifier rated; the plan adds selection, an id and totals. Sizes are bytes.

| Field | Type | Nullable | Notes |
|---|---|---|---|
| `planId` | UUID | no | server-generated, lowercase |
| `scanId` | UUID | no | the completed scan the plan was built from — the "before" side of its verification |
| `rootPath` | string | no | the scan's root |
| `createdAt` | datetime | no | canonical form; a rescan must START after this to verify the plan |
| `maxTier` | string | no | `safe` \| `caution` — the highest tier admitted. Never `review` (a request for it is a 400) |
| `minBytes` | integer | no | items whose included paths' private bytes fall below this are skipped, after Git protection is applied |
| `items` | array | no | in the summary's order; may be `[]` |
| `itemCount` | integer | no | `items.length` |
| `expectedFreedBytes` | integer | no | Σ items' `expectedFreedBytes` |
| `skipped` | object | no | `{review, aboveTier, belowMinBytes, tracked}` counts — why groups were left out, so an empty plan is explicable. `tracked` (1.1.0): every path of the group sits inside a git work tree and is not ignored by its `.gitignore` rules — committed fixtures are git data, never plan paths. A group that keeps some paths is an item whose `why` ends with `N path(s) held back: …` |

Each item: `ruleId`, `label`, `category`, `riskTier`, `why` (strings, as on
the group), `command` (string, **nullable**, present-as-null), `paths`
(array of strings — the group's `topPaths`, biggest first, with Git-protected
paths removed), `expectedFreedBytes` (the sum of persisted `privateSize`
for the paths actually included), `diskSize` (the group's deduped
size, for the lists-as comparison).

The group's whole size is not the plan's promise: `topPaths` stops at 25
roots (and lists a root below 1 GiB private only to fill the floor of five),
and Git protection may exclude more paths. Summing each included
path's private bytes is conservative when shared blocks become free only
after several paths are removed together. A legacy scan with a null
private-byte measurement returns 400 with a request to rescan; a missing
path row returns 404. Neither case creates a plan with a guessed estimate.

### ReclaimVerification (1.1.0)

Returned by `POST /plans/{id}/verify` as an OBJECT. `reclaim-verification.json`
pins the shape.

| Field | Type | Nullable | Notes |
|---|---|---|---|
| `planId` | UUID | no | |
| `beforeScanId` | UUID | no | the plan's scan |
| `afterScanId` | UUID | no | the rescan |
| `rootPath` | string | no | |
| `verifiedAt` | datetime | no | when the comparison ran |
| `items` | array | no | one per plan item, same order |
| `expectedFreedBytes` | integer | no | the plan's total promise |
| `actualFreedBytes` | integer (signed) | no | the headline. before.totalDiskSize − after.totalDiskSize: EVERYTHING that changed under the root — except when the plan's own Trash folder (`…/.Trash/phantom-<planId>`) is a directory of the rescan (the Trash sits inside the root, as on a home scan): the moved bytes are then still counted under the root, so it is Σ of the items' `actualFreedBytes` instead. Negative means the measured tree grew. The space returns when the Trash is emptied |
| `shortfallBytes` | integer (signed) | no | expected − actual; positive: less came back than promised |
| `withinTolerance` | boolean | no | \|shortfall\| ≤ 5% of expected, in either direction (a large over-delivery means something outside the plan moved too); with expected 0, "nothing grew" |

Each item: `ruleId`, `label`, `paths`, `expectedFreedBytes`, and three
**nullable** integers — `actualFreedBytes` (Σ over the item's paths of before
directory size − after directory size, a path absent from the rescan counting
as 0 after), `beforeBytes`, `afterBytes` — all null together when none of the
paths was a persisted directory in the before scan.

### PathExplanation (1.1.0)

Returned by `GET /scans/{id}/explain?path=` as an OBJECT. `path-explanation.json`
pins the shape. `path`, `isDir`, `diskSize`, `logicalSize`, `privateSize`
(**nullable**), `sharedSize` (**nullable**), `flags` (the entry's wire flag
strings), `dataless` (boolean), `cloneId` (**nullable**), `nlink`, `category`
(**nullable**), `hotspot` (**nullable** object: `ruleId`, `label`, `riskTier`,
`why`, `command` (**nullable**), `hint`, `matchedBy` ∈ `topPath` \|
`category`), `unreadableBelow` (array of `{path, reason}` — the scan's sample
filtered to the subtree), `unreadableBelowCount`, `summary` (one sentence of
human text — show it, do not parse it).

### StaleProjects (1.1.0)

Returned by `GET /scans/{id}/stale?olderThan=` as an OBJECT. `stale-projects.json`
pins the shape. `scanId`, `rootPath`, `thresholdDays`, `projectsEvaluated`,
`unverifiable`, `projects` (array, biggest `artifactDiskSize` first, then
most days, then root; each `root`, `lastActivityDays`, `artifactDiskSize`,
`artifacts` — each `ruleId`, `path`, `category`, `riskTier`, `diskSize`),
`artifactDiskSize` (Σ over the listed projects). Unverifiable roots
(`lastActivityDays` null in the summary) are never listed.

### VolumeStatus (1.1.0)

Returned by `GET /volume` as an OBJECT. `volume-status.json` pins the shape.
`path`, `mountPoint`, `filesystem`, `totalBytes` (the APFS CONTAINER),
`usedBytes` (total − free: every volume in the container together),
`freeBytes` (to root), `availableBytes` (to an unprivileged process),
`volumeUsedBytes` (**nullable**: THIS volume's own consumption from
`getattrlist ATTR_VOL_SPACEUSED`; null when the filesystem does not report
it), `purgeableBytes` (**nullable**: `importantUsageBytes − availableBytes`
floored at 0; null when CoreFoundation has no answer), `importantUsageBytes`
(**nullable**: Finder's "Available", purgeable included),
`opportunisticUsageBytes` (**nullable**: the conservative twin),
`snapshotCount` (**nullable**; null unless `snapshots=true`), `snapshots`
(**nullable** array of names; same rule), `hidden` (an OBJECT, never null —
below), `note` (human text). Decoders of a Phase-3 body (no
`volumeUsedBytes` / `importantUsageBytes` / `opportunisticUsageBytes` /
`hidden`) MUST default them to null / the empty object.

**`hidden`** — the used − scanned decomposition, read-only. `scanId`,
`scanRootPath`, `scannedBytes` (the scan's `totalDiskSize`),
`unscannedBytes` (`volumeUsedBytes − scannedBytes` floored at 0),
`unreadableCount` (the scan's `errorCount`) — all **nullable**, null until
`scanId=` names a COMPLETED scan on this volume; `otherVolumesBytes`
(**nullable**: `usedBytes − volumeUsedBytes` floored at 0; null only without
`volumeUsedBytes`; independent of any scan); `otherUserHomes` (array, never
null, of `{path, readable}` — siblings of the current home under `/Users`,
sorted by path; empty off the volume holding `/Users`); `snapshotSuggestion`
(**nullable** string: a `tmutil thinlocalsnapshots <mount> <bytes> 4` line
for the USER; null unless `snapshots=true` listed at least one snapshot.
The server never runs it).

### Growth (1.1.0)

Returned by `GET /scans/series?root=&groupBy=` as an OBJECT. `growth.json`
pins the shape and its arithmetic. `rootPath` (as the newest scan recorded
it), `groupBy` (`total` | `category` | `topLevelDir` | `extension` — the
wire spelling is the only one accepted; `top-level-dir` is a 400), `points`
(array, OLDEST first: one per completed scan of the root — `scanId`,
`startedAt`, `totalDiskSize`, `totalPrivateSize` (**nullable**: null on
pre-v5 rows), `fileCount`), `series` (array of `{key, values}`; `values`
aligns positionally with `points`; each value **nullable**: null = that
scan recorded nothing for the breakdown, 0 = recorded as absent; at most 10
keys ranked by the newest point's value, then `other`), `forecast`
(**nullable** OBJECT: null with fewer than two points or a zero span —
`method` (`"linear"`), `pointsUsed`, `spanDays` (number), `bytesPerDay`
(signed integer), `latestBytes`, `availableBytes` (**nullable**),
`daysUntilFull` (**nullable** number: only when `bytesPerDay > 0` and the
volume is readable), `projectedFullAt` (**nullable** datetime), `caveat`
(human text that MUST be shown beside the number)), `note` (human text).

### ScanRequest

```json
{
    "rootPath": "string, required",
    "crossVolumes": false,
    "olderThan": "3M",
    "verifyLocks": false,
    "toolEstimates": false
}
```

Every key but `rootPath` is optional and defaults to off / the constant.
`olderThan` (1.1.0) is the classifier's dormancy threshold — `90d`, `12w`,
`3M`, `1y`, or bare days; months are 30 days and years 365 — validated at
request time: malformed → 400 `{error}` naming `olderThan`; a non-string →
422. `verifyLocks` and `toolEstimates` (1.1.0) are the two opt-in
subprocess features (`docs/threat-model.md` §4); a server that does not
implement them MUST still accept the keys. Unknown keys → 422 — including
`"root_path"`: a snake_case key is an unknown field, not a lenient alias.

## Error shape (all non-2xx responses, every domain)

```json
{"error": "rootPath must not be empty"}
```

**"All" is literal — there is no escape hatch.** Framework-generated
rejections leak `text/plain` by default and MUST be re-clothed in this shape:
unknown-field / malformed-body (422), malformed-path-param such as a non-UUID
id (400), unmatched route (404), wrong method (405). A `text/plain` body here
is the worst agent-first bug in the stack: the CLI and MCP server both do
`resp.json()`, so a non-JSON error is destroyed into "invalid JSON from API"
and the agent loses the one recovery signal it needed (see the reference
implementation's wrapper extractors + response fallback, and its
status-line-plus-raw-body client fallback for defense in depth).

## Authoritative fixtures

The exact bytes above live in `tests/fixtures/` (see `../08-fixtures/`) and
are executed by the Rust unit tests, the Swift unit tests, and the
conformance harness. When you find an interop bug: fix it, add the
triggering bytes as a fixture, cite it here, bump `VERSION`.
