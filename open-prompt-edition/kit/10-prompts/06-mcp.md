# Prompt 06 — MCP server

## Task

The MCP stdio server per `../02-contracts/mcp-protocol.md` — a thin HTTP
client, exactly seven tools, JSON-RPC 2.0 one message per line.

## Requirements

1. Lifecycle: `initialize`, `tools/list`, `tools/call`, `ping`;
   notifications get no response; unknown methods are `-32601`.
2. The seven tools, their inputs, defaults, and omissions EXACTLY as the
   contract tables them — including `scan_directory` waiting by default,
   optional `scanId` resolving to the latest completed scan, and
   `find_large_files` having no `sort` input.
3. Result shape: pretty-printed JSON text content; HTTP body verbatim for
   every tool except `find_large_files`, which wraps as
   `{files, nextCursor}`.
4. Errors are tool RESULTS (`isError: true` + the API's message), never
   JSON-RPC faults; `health` returns its body even when 503.
5. Tool schemas teach the formats (`"format": "uuid"`; descriptions state
   that sizes are disk bytes and that hints are suggestions, never
   operations).

## Exit criterion

Self-runnable: `tools/list` returns EXACTLY the seven pinned names
(`find_large_files get_hotspots get_space_by_type get_treemap health
list_scans scan_directory`, sorted), and for each result tool the embedded
JSON text is byte-identical (after `jq -S` key-sorting) to the raw HTTP
body — e.g.

```bash
printf '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"get_hotspots","arguments":{"scanId":"<id>"}}}\n' \
  | your-mcp | jq -Sc '.result.content[0].text | fromjson'
curl -s -H "x-api-key: $KEY" "$BASE/scans/<id>/hotspots" | jq -Sc .
```

For `get_treemap`, normalize floats before comparing (round to 1e-6 — see
`../05-algorithms/`); for `find_large_files`, compare `.files` against the
HTTP array and check `nextCursor` mirrors the `X-Next-Cursor` header.

## Sharp edges

- Accept `limit`/`width`/`maxDepth` as JSON numbers OR strings — agents
  send both.
- Do not add tools "for completeness": the omissions (get_scan, cancel,
  delete, tree, entry) are decisions the contract documents, and the e2e
  capability gate fails on any drift in either direction.
