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
| `unreadablePaths` | array | **yes** | capped SAMPLE (first 100, walk order) of the entries behind `errorCount`, each `{path, reason}` with `reason` the OS error text — the count stays the truth. `[]` until the scan completes and for cancelled/failed scans; null == not recorded (rows persisted before schema v4 — same null-vs-empty contract as entry counts) |
| `progress` | object | **yes** | live while running, null once terminal |

`progress` fields (all non-null while present): `filesSeen` (integer),
`bytesSeen` (integer, DISK bytes, hardlink-deduped — consistent with every
other total, so it converges on `totalDiskSize`), `currentPath` (string).

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
    "dirCount": null
}
```

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
| `nlink` | integer | no | hardlink count; entries sharing (`dev`, `ino`) with `nlink` > 1 are one physical file. `nlink` > 1 on a persisted row also warns that deleting this path alone may free nothing — other links (possibly outside the scan) still pin the blocks |
| `dev` | integer | no | |
| `ino` | integer | no | |
| `fileCount` | integer | **yes** | directory rows: descendant FILES at full depth, aggregated server-side from the FULL walk (sub-1-MiB files count even though their rows are never persisted — counting fetched children is a structural undercount). File rows: always null. Also null on directory rows persisted before schema v3 (null = "not recorded", distinct from `0`) |
| `dirCount` | integer | **yes** | same contract for descendant DIRECTORIES at full depth, EXCLUDING the entry itself |

`entry-dir.json` pins the null-heavy directory case (`parentPath`,
`modifiedAt`, `fileType`, `category` all present-as-null) and the count
fields' populated case; `entry.json` (a file row) pins the counts'
null-and-present case — for these two fields the fixtures' null roles
deliberately invert. `entry-dir.json`'s counts (4200 files, 309 dirs)
cohere with `scan-complete.json` (`fileCount` 4200, `dirCount` 310
INCLUDING the root) to pin the excluding-self rule.

#### The category enum

The camelCase wire string is ALSO the stored database string — one string,
no mapping layer. Reference: `rust/phantom-core/src/classify.rs`
(`Category`); semantics in `../05-algorithms/`.

| Wire string | Meaning |
|---|---|
| `regenerableArtifact` | a build regenerates it (`target/` with a `Cargo.toml` sibling, `node_modules`, `.venv`, …) |
| `cache` | app/OS cache; the owner rebuilds it on demand |
| `toolManagedCache` | a cache OWNED by a tool that must do its own cleanup (`~/.cargo`, Homebrew Cellar) |
| `cloudDataloaded` | cloud placeholder: big logical size, ~zero blocks on disk |
| `staleProjectArtifact` | regenerable artifact inside a dormant project — top of the reclaim list |
| `reviewFirst` | big and unclassified, or possibly holding un-backed-up state |
| `wontRegenerate` | deleting loses data (cloud-synced originals) |

Only `regenerableArtifact`, `cache`, `toolManagedCache`, and
`staleProjectArtifact` count toward `reclaimEstimate` below;
`cloudDataloaded` is EXCLUDED (deleting a placeholder frees ~nothing).

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
(integer, **null** if the directory did not exist in A — created), `after`
(integer, **null** if absent from B — deleted), `delta` (signed integer,
`after − before` with an absent side counted as 0). Signed deltas saturate at
the i64 range rather than wrapping. The top-level deltas are exact; the
lists are a capped sample of the biggest per-directory movements.

### HotspotsSummary / HotspotGroup

Returned by `GET /scans/{id}/hotspots` as an OBJECT (the one results surface
that is not a bare array). `hotspots-summary.json` pins the full nested
shape (Rule 3), including a group whose `listedDiskSize` exceeds its
`diskSize` — the hardlink gap made visible.

```json
{
    "groups": [
        {
            "ruleId": "cargo-target",
            "label": "Rust target/ directories",
            "category": "staleProjectArtifact",
            "hint": "`cargo clean` or delete; the next `cargo build` regenerates it",
            "command": "cargo clean",
            "diskSize": 17179869184,
            "listedDiskSize": 17179869184,
            "logicalSize": 18179869184,
            "fileCount": 5120,
            "topPaths": ["/Users/ghost/Code/dormant/target"]
        }
    ],
    "reclaimEstimate": 22548578304,
    "reviewDiskSize": 0,
    "cloudDataloadedLogicalSize": 154140672,
    "cloudDataloadedDiskSize": 147456
}
```

Summary fields (none nullable; empty `groups` is `[]`, never null):

| Field | Type | Notes |
|---|---|---|
| `groups` | array of HotspotGroup | sorted: stale project artifacts first, then category priority, then deduped size descending, then `ruleId` |
| `reclaimEstimate` | integer | hardlink-deduped disk bytes across the four reclaimable categories ONLY, deduped GLOBALLY — a `(dev, ino)` spanning two groups counts once |
| `reviewDiskSize` | integer | deduped disk across `reviewFirst` + `wontRegenerate` — visible, never suggested |
| `cloudDataloadedLogicalSize` | integer | what dataloaded placeholders CLAIM… |
| `cloudDataloadedDiskSize` | integer | …versus the blocks they actually occupy |

Group fields (`command` nullable, present-as-null; everything else non-null):

| Field | Type | Notes |
|---|---|---|
| `ruleId` | string | stable registry key (survives label edits; safe to pin in clients) |
| `label` | string | human name of the hotspot kind |
| `category` | string | the category enum above |
| `hint` | string | human advice — illustrative text only; see below |
| `command` | string \| null | the ONE safe, copy-runnable cleanup command, or null when none honestly exists |
| `diskSize` | integer | hardlink-deduped physical bytes: what deleting the whole group would actually free. THE number |
| `listedDiskSize` | integer | naive per-entry sum; exceeds `diskSize` when hardlinks share blocks |
| `logicalSize` | integer | secondary |
| `fileCount` | integer | files in the group, over the FULL walk (files below the 1 MiB persistence threshold count here even though they have no entry rows) |
| `topPaths` | array of string | largest hotspot roots, biggest first, capped at 5 |

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

### ScanRequest

```json
{"rootPath": "string, required"}
```

Unknown keys → 422 — including `"root_path"`: a snake_case key is an unknown
field, not a lenient alias.

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
