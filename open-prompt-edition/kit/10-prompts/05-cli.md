# Prompt 05 — CLI

## Task

A scripting-friendly CLI client of the HTTP API. It never opens the
database and never walks the filesystem.

## Requirements

1. Subcommands: `scan <path>` (waits, polling with live stderr progress;
   `--no-wait` returns the running view), `scans list|show|cancel|delete`,
   `top` (files: `--type`, `--search`, `--limit`, `--cursor`), `tree`
   (`--path`, `--depth`; each level is one `/tree` call), `types`,
   `hotspots`, `health`.
2. Every read takes `--json` (raw wire body, pretty-printed); human output
   formats sizes from diskSize ONLY — logical sizes appear only explicitly
   labelled.
3. Result commands default `--scan` to the most recent COMPLETED scan; with
   none, exit 3 with "no completed scans; run `phantom scan <path>` first".
4. Stable exit codes: 0 success; 1 server rejected (4xx/5xx except 404);
   2 usage error; 3 not found; 4 cannot reach the API. A scan that ends
   cancelled/failed exits 1 even in `--json` mode.
5. Config per `../04-config/config-spec.md` (`PHANTOM_API_URL`,
   `PHANTOM_API_KEY`/`PHANTOM_KEY_FILE`); request timeouts so a hung API
   cannot stall a script forever.
6. Error handling: surface the API's `{error}` message with the status; a
   non-JSON error body is shown raw (status line + body), never reported as
   "invalid JSON".
7. Pagination: bare-array bodies; the `X-Next-Cursor` header becomes a
   stderr hint ("pass --cursor <token> to continue").

## Exit criterion

Self-runnable parity (Rule 4, `../06-interchange/wire-format.md`): for a
completed scan, each of these pairs is byte-identical —

```bash
your-cli scans show <id> --json | jq -Sc .
curl -s -H "x-api-key: $KEY" "$BASE/scans/<id>" | jq -Sc .
# likewise for: top ↔ /files, types ↔ /types, hotspots ↔ /hotspots
```

— and the exit-code table holds: unknown id → 3, cancel-of-terminal → 1,
usage error → 2, server down → 4.

## Sharp edges

- Decode wire bodies through your shared wire types before rendering human
  output, so the CLI cannot silently render a shape other clients would
  fail on — and exit with a clean wire-skew diagnostic instead of a panic.
- All diagnostics on stderr; stdout is exclusively the data.
