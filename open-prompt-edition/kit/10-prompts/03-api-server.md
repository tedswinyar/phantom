# Prompt 03 — API server, auth, health

## Task

Stand up the HTTP server shell: port ladder + stdout announcement, key-file
auth, `/health`, and the error-shape contract — no domain endpoints yet.

## Requirements

1. Bind per `../04-config/config-spec.md`: configured port, ladder +0..9,
   `0` = ephemeral; announce `phantom-api listening on
   http://127.0.0.1:<port>` on stdout, flushed, exactly once. Loopback only.
2. Key file: create on first start (UUIDv4, one line, mode 0600, parents
   created); empty/whitespace file regenerates. Creation MUST be
   exclusive-create with adopt-on-race — the full semantics are in
   `../04-config/config-spec.md` §Key file; create-then-truncate splits two
   racing servers onto different keys.
3. Every route except `/health` requires `X-Api-Key` matching the key file;
   failure is `401 {"error": "missing or invalid X-Api-Key"}`; comparison is
   constant-time.
4. `/health` touches the datastore: `200 {status: "ok", version}` or
   `503 {status: "degraded", …}` — "process exists" is not health.
5. **Every non-2xx body is `{"error": "..."}`** — including framework
   rejections (unknown field 422, malformed UUID 400, unmatched route 404,
   wrong method 405). Wrap or re-clothe whatever your framework emits.
6. Graceful shutdown on SIGINT/SIGTERM (drain in-flight requests; later
   prompts hook scan cancellation into this drain).

## Exit criterion

`../09-conformance/run.sh <your-base-url> <key>`: the health + auth section
and every error-shape check green.

## Sharp edges

- The stock rejections of most web frameworks are `text/plain`. A non-JSON
  error body is destroyed by every client that does `resp.json()` — the
  single worst agent-first bug in the reference lineage.
- Internal errors (SQL strings, paths) MUST NOT leak: generic
  `500 {"error": "internal server error"}`, log the detail server-side.
- Server stderr goes to a log file no user reads. Anything a user must see
  travels over the API.
