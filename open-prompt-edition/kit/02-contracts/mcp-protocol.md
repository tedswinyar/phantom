# MCP protocol contract

`phantom-mcp` is an MCP server over stdio: JSON-RPC 2.0, one message per
line, protocol version `2024-11-05`. It is a thin client of the HTTP API —
it never opens the database, and it never re-walks the filesystem: every
tool reads what the API serves. (The v0.1 predecessor re-walked the disk on
every call and reported logical sizes; an agent asking three times got three
answers. Store-backed by construction is the fix.)

## Lifecycle methods

- `initialize` → `{protocolVersion, capabilities: {tools: {}}, serverInfo}`
- `tools/list` → the tool table below
- `ping` → `{}`
- Notifications (no `id`) receive no response.
- Unknown method → JSON-RPC error `-32601`.

## Tools — exactly seven

The tool set is **byte-pinned** by the e2e capability gate
(`tests/e2e/run-e2e.sh` compares `tools/list` against a sorted literal
list): adding or removing a tool is a deliberate act that must update the
gate, this table, and `VERSION` in the same push.

| Tool | Input | Behavior |
|---|---|---|
| `scan_directory` | `{path: string (required), wait?: boolean}` | POST /scans. **Waits for completion by default** and returns the terminal scan view. `wait: false` returns the 202 running view immediately; poll via `list_scans`. |
| `list_scans` | `{}` | GET /scans — every scan, newest first, running ones with live `progress` |
| `find_large_files` | `{scanId?, fileType?: string, search?: string, limit?: integer, cursor?: string}` | GET /scans/{id}/files (paginated; wrapped, see below). No `sort` input — the tool always serves the default disk-size-descending order; an agent that wants another order has the CLI/HTTP surfaces. |
| `get_space_by_type` | `{scanId?}` | GET /scans/{id}/types |
| `get_treemap` | `{scanId?, width?: number, height?: number, maxDepth?: integer, root?: string}` | GET /scans/{id}/treemap — omitted dimensions use the server defaults (800×600, depth 4) |
| `get_hotspots` | `{scanId?}` | GET /scans/{id}/hotspots — the reclaimability summary; sizes are hardlink-deduped disk bytes; hints name safe tools, never operations |
| `diff_scans` | `{scanA: string (required), scanB: string (required)}` | GET /scans/{scanA}/diff/{scanB} — what grew/freed between two completed scans of the same root; deltas read B − A, so `scanA` is the older scan. Both required (no default pair on the agent surface — an agent names the two scans it means). The response echoes `scanAStartedAt`/`scanBStartedAt` and sets `reversedChronology: true` if `scanA` is actually the newer scan (signs inverted) — `list_scans` is newest-first, so feeding [0],[1] straight in trips this. |
| `health` | `{}` | GET /health |

**`scanId` is optional on every result tool**: omitted, the tool resolves to
the most recent COMPLETED scan (a running or cancelled scan has no readable
results); with no completed scans the tool errors with
"no completed scans; run scan_directory first". The property declares
`"format": "uuid"` so the schema itself teaches the id format; any-case
UUIDs are accepted.

**Deliberate omissions** (not gaps — decisions):

- No `get_scan` poll tool: `list_scans` subsumes it (the collection is small
  under keep-last-N retention), and `scan_directory`'s description points
  pollers there.
- No `cancel_scan` / `delete_scan`: lifecycle mutation is off the v1.0 agent
  surface; `scan_directory` waits by default, so an agent rarely holds a
  running scan it needs to abort. The CLI covers cancel/delete.
- No `tree` / `entry` tools: `get_treemap` is the agent-shaped hierarchy
  view (one call, aggregated, bounded depth — better token economics than
  walking `tree` level by level).

## Cross-surface capability is a TABLE, not a rule

The three client surfaces (HTTP, CLI, MCP) are deliberately NOT one-to-one.
The e2e harness pins this exact table; a new endpoint must decide its row.

| Operation | HTTP | CLI | MCP |
|---|---|---|---|
| start scan (wait / no-wait) | ✓ | ✓ | ✓ |
| list scans / poll one | ✓ (`/scans`, `/scans/{id}`) | ✓ (`scans list`, `scans show`) | ✓ (`list_scans` only) |
| files (filter / sort / page) | ✓ | ✓ (`top`) | ✓ (no sort) |
| types | ✓ | ✓ | ✓ |
| hotspots | ✓ | ✓ | ✓ |
| diff two scans | ✓ | ✓ (`diff`) | ✓ (`diff_scans`) |
| treemap | ✓ | — | ✓ |
| tree / entry | ✓ | ✓ (`tree`) | — |
| cancel / delete | ✓ | ✓ | — |
| health | ✓ | ✓ | ✓ |

e2e parity is three-way byte-identical where all three surfaces exist
(scan views, files, types, hotspots), two-way for treemap (HTTP↔MCP, floats
normalized to 1e-6 before comparing) and tree (HTTP↔CLI at depth 1), and
behavior-only for entry/cancel/delete.

## Result shape

Success: `result.content = [{type: "text", text: "<pretty-printed JSON>"}]`.
For every tool EXCEPT `find_large_files` the embedded JSON is the API's
HTTP body verbatim — bare arrays stay bare arrays, objects stay objects.

**The one paginated tool wraps its result** so an agent never has to read
HTTP headers: `find_large_files` returns
`{"files": [<ScanEntry>…], "nextCursor": <token|null>}`. When `nextCursor`
is non-null, more rows remain — call again with `cursor` set to that token.
(The HTTP surface carries the token in the `X-Next-Cursor` header; the tool
lifts it inline. `nextCursor` is present-as-null on the last page.)

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
