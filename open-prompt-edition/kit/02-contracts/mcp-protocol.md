# MCP protocol contract

`phantom-mcp` is an MCP server over stdio: JSON-RPC 2.0, one message per
line. It speaks protocol revisions `2025-06-18`, `2025-03-26` and
`2024-11-05` (since 1.1.0; v1.0 spoke only `2024-11-05`). It is a thin client of the HTTP API —
it never opens the database, and it never re-walks the filesystem: every
tool reads what the API serves. (The v0.1 predecessor re-walked the disk on
every call and reported logical sizes; an agent asking three times got three
answers. Store-backed by construction is the fix.)

## Lifecycle methods

- `initialize` → `{protocolVersion, capabilities: {tools: {}}, serverInfo}`.
  **Version negotiation (1.1.0):** the server echoes the client's
  `protocolVersion` when it is one of the three it speaks, and otherwise
  answers with the latest (`2025-06-18`). The response shape is the
  `2024-11-05` one plus additive fields, so a 2024-11-05 host works unchanged.
- `tools/list` → the tool table below, **in a fixed order** (`get_volume_status`
  first — the anchor — then `scan_directory`, …, `health` last; pinned by a
  unit test — hosts render the list in order)
- `ping` → `{}`
- Notifications (no `id`) receive no response.
- Unknown method → JSON-RPC error `-32601`.
- Protocol violations are JSON-RPC errors, never tool errors (1.1.0,
  phantom-b7u): malformed JSON → `-32700` with `id: null`; a `jsonrpc` other
  than `"2.0"` (or missing) → `-32600` naming the id the client sent;
  positional `params` (an array), a `tools/call` without a string `name`,
  or `arguments` that are not an object → `-32602`. A request without an
  `id` is a notification and gets no reply. An UNKNOWN TOOL name is a tool
  error (`isError: true` content), because the method was valid; an
  unreachable API is likewise a tool error, and the server keeps serving.

## Tools — exactly sixteen (1.1.0; v1.0 shipped eight)

The tool set is **byte-pinned** by the e2e capability gate
(`tests/e2e/run-e2e.sh` compares `tools/list` against a sorted literal
list): adding or removing a tool is a deliberate act that must update the
gate, this table, and `VERSION` in the same push.

| Tool | Input | Behavior |
|---|---|---|
| `get_volume_status` (1.1.0) | `{path?: string, snapshots?: boolean, scanId?: uuid}` | GET /volume — the anchor before scanning AND the "where is System Data" answer after one: statfs (container), `getattrlist` (this volume's `volumeUsedBytes`) and CoreFoundation (`purgeableBytes`, `importantUsageBytes`) on the DATA volume by default, no subprocess; `hidden` carries `otherVolumesBytes` always and, with `scanId` (a COMPLETED scan on this volume, else the API's 409/400 surfaces as the tool error), `scannedBytes` / `unscannedBytes` / `unreadableCount`; `otherUserHomes` with readability; `snapshots: true` runs `/usr/bin/tmutil listlocalsnapshots` (fixed path, bounded, off by default) and fills `hidden.snapshotSuggestion` — a command for the USER, never run. Listed FIRST in tools/list |
| `scan_directory` | `{path: string (required), wait?: boolean, crossVolumes?: boolean, olderThan?: string, verifyLocks?: boolean, toolEstimates?: boolean, responseFormat?}` | POST /scans. **Waits for completion by default** (capped at **60 s** since 1.1.0, was 120 s; a capped wait returns the running view plus a `note` naming `scan_status`) and returns the terminal scan view. `wait: false` returns the 202 running view immediately; poll via `scan_status`. While waiting, a request that carried `_meta.progressToken` receives `notifications/progress` (see below). `crossVolumes` (default false), `olderThan` (dormancy threshold, e.g. `3M`; a malformed value is the API's 400, surfaced as the tool error), `verifyLocks` and `toolEstimates` (opt-in read-only subprocess probes, default false) map straight onto the request body. |
| `scan_status` (1.1.0) | `{scanId: string (required), responseFormat?}` | GET /scans/{id} — one scan's view: live `progress` while running, totals once terminal, `failureReason` on a failed one. **`scanId` is required** (no latest-completed default: this tool is about one specific, usually running, scan). Unknown id → not found |
| `cancel_scan` (1.1.0) | `{scanId: string (required)}` | POST /scans/{id}/cancel — cooperative stop; answers 202 with the view (status may still read `running`; poll `scan_status`). Already terminal → the API's 409 as a tool error ("already complete; cannot cancel"). The first agent-facing mutation: annotated `readOnlyHint: false`, `destructiveHint: false` (partial results were never promised), `idempotentHint: true` |
| `list_scans` | `{responseFormat?}` | GET /scans — every scan, newest first, running ones with live `progress` |
| `find_large_files` | `{scanId?, fileType?: string, search?: string, limit?: integer, cursor?: string}` | GET /scans/{id}/files (paginated; wrapped, see below). No `sort` input — the tool always serves the default disk-size-descending order; an agent that wants another order has the CLI/HTTP surfaces. |
| `get_space_by_type` | `{scanId?}` | GET /scans/{id}/types |
| `get_treemap` | `{scanId?, width?: number, height?: number, maxDepth?: integer, root?: string}` | GET /scans/{id}/treemap — omitted dimensions use the server defaults (800×600, depth 4) |
| `get_hotspots` | `{scanId?}` | GET /scans/{id}/hotspots — the reclaimability summary; `diskSize` is deduped disk bytes (hardlinks AND APFS clone groups count once), `privateSize` / `reclaimEstimate` are what deletion actually frees; hints name safe tools, never operations. Since 1.1.0 each group carries `riskTier` (safe / caution / review), `why`, `rebuildCost` and a nullable `toolEstimate`; the tool description tells the agent to treat `review` as not-a-suggestion and to quote `privateSize` |
| `explain_path` (1.1.0) | `{scanId?, path: string (required)}` | GET /scans/{id}/explain — one path's three sizes, sharing facts, `dataless`, the pared hotspot (or null), the unreadable sample below it, and a one-sentence `summary`. A path the scan did not record is not found |
| `find_stale_projects` (1.1.0) | `{scanId?, olderThan?: string}` | GET /scans/{id}/stale — the scan's recorded per-project activity re-thresholded (default 90d; the `olderThan` grammar), with each project's artifacts. Unverifiable roots are never listed |
| `plan_reclaim` (1.1.0) | `{scanId?, maxTier?: "safe" \| "caution", minBytes?: integer, includeScript?: boolean}` | POST /scans/{id}/plan — the dry-run plan (`ReclaimPlan`); `includeScript: true` adds `script` (GET /plans/{id}/script) to the result object. Writes a plan row: `readOnlyHint: false`, `idempotentHint: false` (a new planId per call), `destructiveHint: false`. The description tells the agent to show the plan and get an explicit yes before acting, and that `review` is never planned |
| `verify_reclaim` (1.1.0) | `{planId: string (required), afterScanId?: string}` | POST /plans/{id}/verify. Without `afterScanId` the tool first POSTs a rescan of the plan's root and waits like `scan_directory` (60 s cap, progress notifications if a token was sent); a rescan still running at the cap is a tool ERROR naming the `afterScanId` to pass next time. Same annotations as plan_reclaim (it may start a scan) |
| `diff_scans` | `{scanA: string (required), scanB: string (required)}` | GET /scans/{scanA}/diff/{scanB} — what grew/freed between two completed scans of the same root; deltas read B − A, so `scanA` is the older scan. Both required (no default pair on the agent surface — an agent names the two scans it means). The response echoes `scanAStartedAt`/`scanBStartedAt` and sets `reversedChronology: true` if `scanA` is actually the newer scan (signs inverted) — `list_scans` is newest-first, so feeding [0],[1] straight in trips this. |
| `get_growth` (1.1.0) | `{root?: string, groupBy?: total|category|topLevelDir|extension}` | GET /scans/series — how `root` (default: the newest completed scan's root, resolved client-side from GET /scans like `scanId` defaults) grew across its completed scans, oldest first, with a linear forecast; `daysUntilFull` only when growing; the tool description tells the agent to relay `forecast.caveat` beside the number. An unknown root is the API's 404, surfaced as the tool error. No re-walk. |
| `health` | `{}` | GET /health |

### Tool metadata (1.1.0)

Every tool carries, beside `name`/`description`/`inputSchema`:

- **`title`** — a human label for hosts that show one.
- **`annotations`** — `{title, readOnlyHint, destructiveHint: false,
  idempotentHint, openWorldHint: false}`. Every tool is read-only and
  idempotent **except the writers**: `scan_directory`, `plan_reclaim` and
  `verify_reclaim` (`readOnlyHint: false`, `idempotentHint: false`: each
  call records a NEW scan / plan / rescan in Phantom's own store, and the
  opt-in probes spawn subprocesses) and `cancel_scan` (`readOnlyHint:
  false`, `idempotentHint: true`). Nothing is `destructiveHint: true`: plans
  MOVE to the Trash, and only when the user runs the script. The spec's defaults are
  `destructiveHint: true` / `openWorldHint: true`, so an unannotated tool
  reads as worse than any of these are; that is why they are set explicitly.
- **`outputSchema`** — a JSON Schema for the tool's `structuredContent`
  (always an object; see "Result shape"). The schemas are checked against
  the shared wire fixtures in `tests/fixtures/` by `phantom-mcp`'s unit
  tests: every key a fixture carries must be declared, every `required` key
  must be present. A wire change that forgets the schema fails there.
- **`_meta["anthropic/maxResultSizeChars"]: 100000`** on exactly
  `get_treemap` and `find_large_files` — the two payloads that grow with the
  tree. The same number is ENFORCED server-side (below).

### `responseFormat: "concise" | "detailed"` (1.1.0)

`scan_directory`, `list_scans`, `find_large_files`, `get_treemap` and
`get_hotspots` accept `responseFormat`. **`detailed` is the default and is
the HTTP body verbatim** (the parity gate depends on that). `concise` keeps
the fields an agent acts on and drops the rest:

| Tool | concise keeps |
|---|---|
| `scan_directory`, `list_scans` (per scan) | `id, rootPath, status, startedAt, finishedAt, totalDiskSize, totalPrivateSize, fileCount, errorCount` (+ `progress` while running, `note` on a capped wait) |
| `find_large_files` (per file) | `path, diskSize, privateSize, fileType`; the envelope's `nextCursor` survives |
| `get_treemap` (per rect) | `path, size, depth, isDir` — the hierarchy without the geometry |
| `get_hotspots` (per group) | `ruleId, label, category, riskTier, why, command, diskSize, privateSize, topPaths`; the summary totals survive |

Any other value is a tool error naming the two valid ones (no silent
fallback). Tools not in the table ignore the argument.

### `notifications/progress` (1.1.0)

Only for a waited `scan_directory`, and only when the request carried
`params._meta.progressToken` (string or integer — echoed as sent). While the
call waits, the server writes notification lines on stdout AHEAD of the
response, at most one per second and only when `filesSeen` moved:

```json
{"jsonrpc":"2.0","method":"notifications/progress","params":{"progressToken":"t1","progress":48211,"message":"scanning /Users/ghost: 48211 files, 12.4 GiB so far — /Users/ghost/Library/Caches/x"}}
```

`progress` is the files-seen counter (monotonic); there is no `total` — a
walk has no knowable total, that is what the scan is for. No token, no
notifications, ever. The e2e harness pins: every line before the response
is such a notification echoing the token; a token-less call produces exactly
one line.

### Result budget (1.1.0)

A result whose pretty-printed text exceeds **100,000 characters** (≈ the
25k-token cap hosts apply before spilling a result to a file) is shaped so
the agent still gets an answer and the next move:

- `get_treemap`: rects deeper than the deepest depth that fits are dropped
  and the layout gains `truncated: {requestedDepth, servedDepth, note}`; the
  note names `maxDepth`, the biggest depth-1 directory to `root` at, and
  `responseFormat: "concise"`. `truncated` is absent on an untruncated
  layout (so the verbatim rule holds for every result under budget).
- `find_large_files`: the page is refused (`isError: true`) with a message
  naming a `limit` that fits and pointing at `concise`. A page is one unit —
  the server cannot invent a continuation cursor for a partial page.
- Every other tool's payload is bounded by design and passes through.

**`scanId` is optional on every result tool**: omitted, the tool resolves to
the most recent COMPLETED scan (a running or cancelled scan has no readable
results); with no completed scans the tool errors with
"no completed scans; run scan_directory first". The property declares
`"format": "uuid"` so the schema itself teaches the id format; any-case
UUIDs are accepted.

**Deliberate omissions** (not gaps — decisions):

- No `delete_scan`: forgetting a scan is the user's call, not the agent's;
  the CLI and the app cover it. (v1.0 also omitted `scan_status` and
  `cancel_scan`; 1.1.0 added both because a no-wait scan needs a poll
  surface an agent can name, and a runaway home-directory scan needs a stop.)
- No `tree` / `entry` tools: `get_treemap` is the agent-shaped hierarchy
  view (one call, aggregated, bounded depth — better token economics than
  walking `tree` level by level).

## Cross-surface capability is a TABLE, not a rule

The three client surfaces (HTTP, CLI, MCP) are deliberately NOT one-to-one.
The e2e harness pins this exact table; a new endpoint must decide its row.

| Operation | HTTP | CLI | MCP |
|---|---|---|---|
| start scan (wait / no-wait) | ✓ | ✓ | ✓ |
| list scans / poll one | ✓ (`/scans`, `/scans/{id}`) | ✓ (`scans list`, `scans show`) | ✓ (`list_scans`, `scan_status`) |
| files (filter / sort / page) | ✓ | ✓ (`top`) | ✓ (no sort) |
| types | ✓ | ✓ | ✓ |
| hotspots | ✓ | ✓ | ✓ |
| diff two scans | ✓ | ✓ (`diff`) | ✓ (`diff_scans`) |
| treemap | ✓ | — | ✓ |
| tree / entry | ✓ | ✓ (`tree`) | — |
| cancel / delete | ✓ | ✓ | cancel ✓ (`cancel_scan`), delete — |
| growth series + forecast (1.1.0) | ✓ (`/scans/series`) | ✓ (`growth`) | ✓ (`get_growth`) |
| explain a path / stale projects / volume (1.1.0) | ✓ (`/scans/{id}/explain`, `/scans/{id}/stale`, `/volume`) | ✓ (`explain`, `stale`, `volume`) | ✓ (`explain_path`, `find_stale_projects`, `get_volume_status`) |
| plan / script / verify (1.1.0) | ✓ (`/scans/{id}/plan`, `/plans/{id}`, `/plans/{id}/script`, `/plans/{id}/verify`) | ✓ (`plan`, `plan --script`, `verify`) | ✓ (`plan_reclaim` [+ `includeScript`], `verify_reclaim`) |
| health | ✓ | ✓ | ✓ |

e2e parity is three-way byte-identical where all three surfaces exist
(scan views, files, types, hotspots), two-way for treemap (HTTP↔MCP, floats
normalized to 1e-6 before comparing) and tree (HTTP↔CLI at depth 1), and
behavior-only for entry/cancel/delete.

## Result shape

Success: `result.content = [{type: "text", text: "<pretty-printed JSON>"}]`
**plus, since 1.1.0, `result.structuredContent`** — the same value as JSON
for hosts that read it. `structuredContent` must be an object, so the two
bare-array bodies wrap THERE and only there: `list_scans` →
`{"scans": [...]}`, `get_space_by_type` → `{"types": [...]}`. The text stays
the bare array.
For every tool EXCEPT `find_large_files` the embedded JSON text is the API's
HTTP body verbatim — bare arrays stay bare arrays, objects stay objects
(under the default `responseFormat` and the result budget, above).

**The one paginated tool wraps its result** so an agent never has to read
HTTP headers: `find_large_files` returns
`{"files": [<ScanEntry>…], "nextCursor": <token|null>}`. When `nextCursor`
is non-null, more rows remain — call again with `cursor` set to that token.
(The HTTP surface carries the token in the `X-Next-Cursor` header; the tool
lifts it inline. `nextCursor` is present-as-null on the last page.)

**A defaulted `scanId` carries a `note` (1.1.1).** Every result tool that
takes `scanId` defaults to the most recent completed scan of ANY root. When
the argument was omitted, the object body gains one MCP-envelope field —
`note` — whose first line names the scan that was read (`scanId omitted; read
scan 5afa7a83 · /Users/ted · 280.4 GB · 4.09M files · 2 h ago`) and whose
second line, only when another root has a completed scan that is both larger
and less than 24 h old, warns and names the remedy (`… — pass scanId "<id>"`).
The header and warning text are the CLI's, byte for byte (the CLI prints the
header as the first stdout line of every human view and the warning on
stderr; `phantom_core::pick` is the one definition). An explicit `scanId`
adds nothing: the body stays the HTTP body verbatim. `get_space_by_type`'s
bare-array body has nowhere to carry a note and carries none. `note` is not
on the HTTP wire and is not in any outputSchema's `required` list.

**`health` returns its body even when degraded:** a `503 {"status":
"degraded"}` is a real answer the agent should see, so it is NOT surfaced as
`isError`.

**House rule: errors surface as tool RESULTS, not protocol faults.**
API-level failures (4xx/5xx) return `result.isError = true` with the API's
error message as text, so the calling agent sees "scan X is still running;
results are available once it finishes" rather than a JSON-RPC fault. The
scan lifecycle leans on this hard — 409-while-running and
409-already-terminal are recoverable states an agent must be able to read
and act on (poll, then retry), never opaque failures.

## Configuration

Same env convention as every client: `PHANTOM_API_URL`,
`PHANTOM_API_KEY` / `PHANTOM_KEY_FILE`
(`../04-config/config-spec.md`).
