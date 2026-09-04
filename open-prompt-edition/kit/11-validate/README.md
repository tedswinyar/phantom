# 11 — Validate: two implementations, one database

The strongest interop test (Rule 4). Run it whenever a second implementation
of any surface exists.

## The drill

1. Scan a deterministic fixture tree with implementation A (e.g., the
   reference CLI) — pin the tree's mtimes so datetimes are constants.
2. Read the scan, its files, types, and hotspots with implementation B —
   every field decodes, timestamps within 1µs, `diskSize` is the headline
   number on every surface.
3. Mutate with B: cancel a running scan, delete a terminal one.
4. Read with A — the cancelled scan shows `cancelled` with empty results;
   the deleted scan is a clean 404 everywhere.
5. Both directions again with the roles swapped.

The reference implementation runs a same-language version of this
continuously (`tests/e2e/run-e2e.sh`: CLI vs HTTP vs MCP). A new
implementation should first pass `09-conformance/run.sh` (point it at your
server: `run.sh <base-url> <api-key>`), then this drill.

## Checklist per new implementation

Each item is a runnable check with the expected output stated:

- [ ] `09-conformance/run.sh http://127.0.0.1:<your-port> <your-key>` →
      last line `conformance: N passed, 0 failed` (54 checks as of kit
      0.10.1; the harness refuses ports 8768–18309 — run your server on an
      ephemeral or low port for this).
- [ ] Your unit suite decodes every file in `tests/fixtures/` FROM RAW
      BYTES — including the nested treemap (with its `residual` rect) and
      hotspots shapes — and fails compilation-or-test if a wire field is
      added without decode coverage.
- [ ] `curl -s -H "x-api-key: $KEY" $BASE/scans/<id> | jq -r .startedAt`
      matches `^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{6}Z$` (canonical
      encode), and your decoder accepts all four variants in
      `tests/fixtures/datetime-variants.json`.
- [ ] Uppercase the UUID in any request path → 200; every UUID in any
      response body is lowercase.
- [ ] `jq 'has("finishedAt") and has("progress")'` is true on a TERMINAL
      scan view (present-as-null, nested depth included — entry
      `category`/`fileCount`/`dirCount`, rect `fileType`).
- [ ] Scan a tree containing a hardlinked pair and a `node_modules`:
      `.groups[0].ruleId == "node-modules"`, category strings byte-equal
      to the 06-interchange enum, `reclaimEstimate` counts the shared
      blocks ONCE and excludes any cloud-dataloaded bytes. (Hardlink dedup
      and the dataloaded exclusion are where independent implementations
      diverge first.)
- [ ] The five-step drill above against the reference implementation's
      database — both directions.
- [ ] New pitfalls discovered → added to `06-interchange/wire-format.md`
      and, if byte-reproducible, to `tests/fixtures/`
