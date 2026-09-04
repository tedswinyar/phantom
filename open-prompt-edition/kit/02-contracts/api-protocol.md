# HTTP API contract

Base URL: announced by the server on stdout at startup
(`phantom-api listening on http://127.0.0.1:<port>`). The configured port
is a request; the announcement is the truth (port-ladder behavior, see
`../04-config/config-spec.md`).

Auth: every endpoint except `/health` requires header `X-Api-Key: <key>`
matching the server's key file. Failure → `401` with body
`{"error": "missing or invalid X-Api-Key"}`. The comparison MUST be
constant-time (no early exit on the first differing byte). Wire formats:
`../06-interchange/wire-format.md`.

**Every non-2xx response is the `{"error": "..."}` shape — no exceptions.**
This includes malformed-body/unknown-field rejections (422), malformed-path
rejections (400), and the built-in unmatched-route (404) and wrong-method
(405) responses. An implementation MUST route these through the error shape
too; a `text/plain` rejection breaks every client that does `resp.json()`
(the CLI and MCP server both do), turning the real reason into "invalid JSON".

**400 vs 422, the principle:** `422` means "these bytes are not a valid
request shape" (unknown fields, wrong types, unparseable body); `400` means
"this is a well-formed request whose VALUES I reject" (empty rootPath,
out-of-range number, cancelling twice). Consequence for implementers: type
request fields loosely enough that every out-of-range value reaches the
validator and earns a consistent `400` — query parameters in particular are
taken as raw strings and validated by hand, so `?limit=banana` is a clean
`400 {error}`, not a framework's text/plain rejection.

**Query parameters are STRICT.** An unknown query key on any scan-results
route (`?filetype=` for `?fileType=`, `?depth=` for `?maxDepth=`) is a
`400 {error}` naming the offending field — never a silently unfiltered
response. A typo that quietly returns every row is the worst failure mode a
listing endpoint has.

## Status codes, the complete table

Status codes and the `{"error": "..."}` SHAPE are normative; the message
TEXT inside `error` is illustrative and may be reworded in any release —
clients and conformance checks match on status and shape, never on exact
message strings.


| Code | Meaning | Produced by |
|---|---|---|
| 200 | read succeeded | every GET |
| 202 | accepted, work continues | POST /scans (walk starting), POST /scans/{id}/cancel (cancel requested) |
| 204 | deleted, no body | DELETE /scans/{id} |
| 400 | well-formed request, rejected value | empty/blank `rootPath`; `rootPath` not a directory; non-UUID `{id}` path param; bad `limit`/`cursor`/`width`/`height`/`maxDepth`/`sort` value; UNKNOWN query key on a results route; tree/treemap target not a directory; missing `?path` on `/entry` |
| 401 | missing or wrong `X-Api-Key` | every route except `/health` |
| 404 | valid request, no such thing | unknown scan id; unknown `root=`/`?path=` within a scan; unmatched route |
| 405 | wrong method on a real route | router built-in, re-clothed as `{error}` |
| 409 | valid request, wrong lifecycle state | cancel of a terminal scan; delete of a running scan; any results endpoint while the scan is still running |
| 415 | POST body without `Content-Type: application/json` | JSON extractor, re-clothed as `{error}` |
| 422 | malformed request shape | unknown fields, wrong types, unparseable body |
| 500 | internal failure | generic `{"error": "internal server error"}` — internal detail (SQLite strings, paths) MUST NOT leak into the body |
| 503 | store unusable | GET /health degraded |

## GET /health — open, no auth

- `200 {"status": "ok", "version": "<semver>"}` when the store is usable
- `503 {"status": "degraded", …}` when it is not
- MUST touch the datastore; "process exists" is not health.

## The scan lifecycle

A scan is asynchronous. Its wire view is the `Scan` object plus a `progress`
key: a live counters object while the scan runs, `null` once it is terminal
(nullable-present-as-null, like every nullable field). Shapes:
`../06-interchange/wire-format.md`.

Statuses: `running` → exactly one of `complete` | `cancelled` | `failed`.
Terminal is terminal; there are no other transitions.

**The two homes and the merge rule.** A running scan lives in an in-memory
registry (identity + live progress + cancel flag); on reaching a terminal
state it is persisted to SQLite and only then removed from the registry.
Every read merges the two with the DB row winning, and a scan MUST never be
invisible: persist-then-remove means the worst case is a brief window where
both sides know it, so listings dedupe by id. If the terminal persist fails,
the scan stays in the registry marked `failed` rather than vanishing.

**What persists (ADR-0005 — the 1 MiB visibility rule).** A `complete` scan
persists a filtered view of the walk:

- **every directory**, its `diskSize`/`logicalSize` replaced by the aggregate
  over ALL descendant files — filtered or not;
- **every file whose `diskSize` ≥ 1,048,576 bytes** (1 MiB). The boundary is
  inclusive: exactly 1 MiB is kept.
- **per-scan `fileType` totals computed from the FULL walk**, before the
  filter — so `/types` sees every file, including the ones whose individual
  rows were omitted.

This is wire-visible product behavior: `/files`, `/tree`, and `/entry` only
see directories and files ≥ 1 MiB; small files' bytes still appear in every
ancestor directory's totals and in the scan totals. A `cancelled` or `failed`
scan persists its metadata row ONLY — zero totals, no entries, no type
totals; partial results are discarded, never half-persisted.

### POST /scans

Body: `{"rootPath": "<absolute directory path>"}`. Unknown fields → 422.

- `202` **immediately** — the response is the scan's wire view with
  `status: "running"` and a zeroed `progress` object; the walk starts after
  the response. The scan's `id` is in the body (no `Location` header).
- `400` → `rootPath` empty/whitespace (server trims first), or not an
  existing directory (pre-flighted so a typo'd path fails now, not as a
  `failed` scan discovered by polling; the walker re-checks — TOCTOU).
- `422` → unknown fields or malformed body.

**Poll semantics:** poll `GET /scans/{id}` and watch `status`. While
`running`, `progress` carries monotonically advancing `filesSeen`/`bytesSeen`
(disk bytes, st_blocks × 512) and the walker's `currentPath`. Once terminal,
`progress` is `null` and `finishedAt` is set. There is no push channel;
polling is the contract.

### GET /scans

- `200` → bare JSON array of scan wire views: persisted scans (with
  `progress: null`) merged with in-flight scans (with live `progress`),
  newest `startedAt` first, ties broken by id ascending. A scan in the
  persist-handoff window MUST appear exactly once (DB row wins). No
  pagination — scan counts are small by construction: after every successful
  terminal persist the server prunes the collection to the newest 25 scans
  (entries and totals cascade; the constant is a product decision pending a
  settings surface). Files within a scan are the thing that paginates.
  Consequence for clients: a scan id is NOT durable forever — the 26th
  completion evicts the oldest scan, and later reads of it are clean 404s.

### GET /scans/{id}

- `200` → the scan's wire view (merge rule above)
- `400` → `{id}` is not a UUID (any case is valid)
- `404` → valid UUID, no such scan anywhere

### POST /scans/{id}/cancel

Empty body.

- `202` → the scan was in flight and its cancel flag is now set; body is the
  current wire view (very likely still `running`). Cancellation is
  cooperative — the walker stops at its next entry. Poll for the terminal
  status; the scan lands as `cancelled` with **partial results discarded**
  (metadata row only). A cancel racing completion can lose: the flag may be
  set after the walk finished, in which case the scan still terminates
  `cancelled` (results discarded), or the handoff may already have run, in
  which case the poll shows `complete`.
- `409` → the scan is already terminal:
  `{"error": "scan <id> is already <status>; cannot cancel"}`.
- `404` → no such scan.

### DELETE /scans/{id}

- `204` → the scan row and everything hanging off it (entries, type totals)
  are gone — cascade, not best-effort. Also deletes the edge case of a
  terminal scan stuck registry-only because its persist failed: deleting that
  is just forgetting it.
- `409` → the scan is still `running`
  (`{"error": "scan <id> is still running; cancel it before deleting"}`).
  Deleting mid-walk would race the completion handoff, which would persist a
  fresh row right after the delete.
- `404` → no such scan.

## Scan results

Every results endpoint reads persisted data, so all of them share a gate:

- `409` → the scan is known but still in flight:
  `{"error": "scan <id> is still running; results are available once it
  finishes"}` — a pointer back at the polling surface, not a 404.
- `404` → no such scan.

### GET /scans/{id}/treemap?width=&height=&maxDepth=&root=

Server-side squarified treemap layout (the algorithm spec lives in
`../05-algorithms/`). All parameters optional:

| Param | Meaning | Default | Bad value |
|---|---|---|---|
| `width` | layout width, positive finite number | `800` | 400 |
| `height` | layout height, positive finite number | `600` | 400 |
| `maxDepth` | levels to recurse; `0` = root only | `4` | 400 (must be a non-negative integer) |
| `root` | path to re-root at | the scan's `rootPath` | see below |

- `200` → `TreemapLayout` (`../06-interchange/wire-format.md`): `rootPath`,
  `totalSize`, and `rects` with absolute coordinates within
  `(0, 0, width, height)`. The root rect is depth 0.
- **`root=` re-roots AND re-lays-out server-side**: the layout is computed
  over the requested subtree at the requested view size — a drill-down is a
  fresh layout call, not client-side rectangle math.
- `400` → the `root=` path exists in the scan but is a file, not a directory.
- `404` → an explicit `root=` path is not in the scan
  (`{"error": "not found: path \"<root>\" in scan <id>"}`).
- No entries and no explicit `root=` (a cancelled/failed scan): `200` with
  the honest empty layout — `totalSize: 0`, `rects: []` — not an error.

### GET /scans/{id}/tree?path=

Direct children of a directory within the scan.

- `200` → bare array of `ScanEntry`. `path` defaults to the scan's
  `rootPath`.
- `400` → the target exists but is a file (`{"error": "not a directory: …"}`).
- `404` → an explicit `?path=` is not in the scan.
- Default root missing (cancelled/failed scan persisted nothing): `200 []`.

### GET /scans/{id}/files?fileType=&search=&sort=&limit=&cursor=

File rows only — directories are excluded, and per the 1 MiB rule only files
≥ 1 MiB exist to list.

| Param | Meaning |
|---|---|
| `fileType` | exact match on the stored lowercased extension; input is lowercased first, so any case matches |
| `search` | case-insensitive substring match on the full path; `%`, `_`, and `\` in the needle match literally (the implementation must escape them, not pass them to LIKE raw) |
| `sort` | `size` (default: `diskSize` descending) \| `name` \| `path`. Ties always break by `path` (unique per scan → fully deterministic order). Anything else → `400 {"error": "sort must be size, name, or path (got …)"}` |
| `limit`, `cursor` | pagination, below |

- `200` → bare array of `ScanEntry`; continuation rides the `X-Next-Cursor`
  response header, present only when more rows remain.

#### Pagination (`?limit`, `?cursor`)

- `?limit=<n>` — return at most `n` rows (server clamps to a max of 500;
  `n` must be a positive integer or the response is `400 {error}`).
  Omitted → the server default page size (100).
- `?cursor=<token>` — continue after a previous page. The token is **opaque**:
  pass back verbatim what the server gave you; do not construct or parse it.
  An unparseable cursor is `400 {error}`.
- **Continuation lives in the `X-Next-Cursor` response header**, present only
  when more rows remain after this page. Absent header = last page. (The
  cursor rides a header, not the body, so the body stays a bare array for
  every existing client; agents that page simply read the header — or use
  the MCP `find_large_files` tool, which surfaces `nextCursor` inline in its
  result.)
- No params = the first page at the default limit. As a scan grows, this
  bounds the response instead of returning an unbounded list.

### GET /scans/{id}/entry?path=

One entry (file or directory) by exact path.

- `200` → `ScanEntry`. Directory entries carry their AGGREGATED
  `diskSize`/`logicalSize` (the persisted rollup), not zero — and their
  full-walk descendant `fileCount`/`dirCount`
  (`../06-interchange/wire-format.md`); file entries carry those two
  present-as-null. The same fields ride every entry-serving surface
  (`/tree`, `/files`).
- `400` → `?path` missing or empty
  (`{"error": "path query parameter is required"}`).
- `404` → path not in the scan.

### GET /scans/{id}/types

Per-type disk totals, computed from the FULL walk at persistence time — the
one results surface that still sees the filtered small files.

- `200` → bare array of `FileTypeTotal`, largest `diskSize` first, ties
  broken by type name (deterministic). `fileType: null` is the
  no-extension bucket; directories are excluded from the totals.
- Cancelled/failed scan: `200 []`.

### GET /scans/{id}/hotspots

The per-scan reclaimability summary. It is computed ONCE, by the classifier
post-pass at scan completion, over the FULL walk (so a hotspot made of
sub-1-MiB files still totals correctly even though those files have no
individual entry rows), and persisted with the scan — repeated reads return
the identical document; nothing is re-walked or re-classified on read. The
classifier algorithm (registry, category semantics, staleness, hardlink
dedup) is specified in `../05-algorithms/`.

- `200` → a `HotspotsSummary` OBJECT, not an array
  (`../06-interchange/wire-format.md`): `groups` (sorted; may be `[]`),
  `reclaimEstimate`, `reviewDiskSize`, `cloudDataloadedLogicalSize`,
  `cloudDataloadedDiskSize`.
- Shares the results gate above (409 while running, 404 unknown).
- A terminal scan with NO stored summary (`cancelled`/`failed` — partial
  results are discarded — or a row persisted before schema v2) serves the
  honest EMPTY summary (`groups: []`, all rollups `0`), matching the
  tree/treemap posture for result-less scans. A `complete` scan that simply
  found no hotspots serves the same empty shape; the two are deliberately
  indistinguishable on this surface.
- The classifier post-pass also stamps `entries.category` (the camelCase
  wire string, pinned in `../06-interchange/wire-format.md`) onto the
  persisted entry rows — DIRECTORY rows under a hotspot root included — so
  `/files`, `/tree`, and `/entry` carry per-entry categories. Ordinary
  content keeps `category: null` (present-as-null).
- Phantom NEVER deletes. Every group carries a human-readable `hint` and a
  first-class `command` field — the ONE safe, copy-runnable cleanup command,
  or null when none honestly exists. Bind affordances to `command`, never
  parse the hint (`../06-interchange/wire-format.md`).

### GET /scans/{id}/diff/{other}

Compare two scans of the same root: what grew, what was freed. Positional —
`{id}` is "before" (`scanA`), `{other}` is "after" (`scanB`), and every delta
reads B − A.

- `200` → a `ScanDiff` OBJECT (`../06-interchange/wire-format.md`): exact
  top-level deltas plus `grown`/`freed`, the top-20 per-direction directory
  movements (floored at 1 MiB — persisted dir totals fold sub-1-MiB files, so
  smaller deltas are below the data's own granularity).
- BOTH scans must be `complete`: a `cancelled`/`failed` scan persists no
  entries, so diffing against one would report the whole tree as freed. A
  non-complete terminal scan (or one still running) is a `409` conflict,
  same gate as the other results surfaces.
- The two scans must cover the same `rootPath`; a mismatch is a `400` (the
  comparison is meaningless, not merely early), distinct from the 409.
- `404` if either id is unknown.

## Versioning: the 1.x additive policy

`GET /health`'s `version` IS the contract version (it moves with
`open-prompt-edition/VERSION` and the app version — VERSIONING.md). Within
1.x:

- **Responses may gain fields; nothing else changes.** New response fields
  are additive and nullable (or carry a server-side default), and land in
  the same release as their fixture updates, conformance rows, and VERSION
  bump — the three move together or not at all.
- **Clients MUST ignore unknown response keys** and decode any
  post-1.0 field as optional, so a 1.0 client reads 1.x responses and a
  1.x client reads persisted 1.0 data.
- **Request DTOs stay strict** (`deny_unknown_fields` / 422 and the strict
  query rule above are permanent). A client wanting to SEND a new 1.x
  request field must gate it on the `/health` version — the server will not
  silently ignore fields it doesn't know.
- **No vendor extension fields in 1.x.** An independent implementation
  claiming conformance emits EXACTLY the documented key sets — the
  fixtures' key-set checks assume it, and an extra key is a conformance
  failure, not an extension point.
- Removals, renames, type changes, and semantic changes to existing fields
  are 2.0 material.

## CLI contract notes

The `phantom` CLI is a stable scripting surface alongside HTTP:

- Exit codes: `0` success, `1` server-rejected request, `2` usage error,
  `3` not found, `4` API unreachable. **Codes above 4 are reserved for
  minor versions** — scripts must treat unknown nonzero codes as failure,
  not match on them exhaustively.
- `phantom health` always emits JSON (with or without `--json`).
- Paginated human/`--json` listings print their continuation hint on
  stderr in the stable form
  `phantom: more files available; pass --cursor <token> to continue` —
  the `--cursor <token>` fragment is the machine-readable part; the
  surrounding prose may be reworded.

## Conformance

`../09-conformance/run.sh` exercises the rows above against a live
implementation, black-box over HTTP. An implementation that passes it and
decodes the shared fixtures (`../08-fixtures/`) from raw bytes is
wire-compatible. Its key-set agreement checks compare EXACT key sets
against the fixtures — the no-vendor-extensions rule above is what makes
that comparison sound.
