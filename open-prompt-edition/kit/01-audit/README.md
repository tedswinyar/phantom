# 01 — Audit

Before rebuilding this system, answer these questions BY READING the
reference implementation, and record the answers here. An audit that is not
written down gets re-done by the next implementer.

## Questions a rebuild must answer

1. **Data model**: what entities exist, which fields are nullable, what are
   the state transitions and their rules?
   *Answer:* one aggregate — `Scan` (metadata + totals) with its `entries`
   (per-path rows), `scan_file_types` (per-type totals), and a persisted
   `hotspots` summary. State machine: `running` → exactly one of
   `complete` | `cancelled` | `failed`; terminal is terminal
   (`07-behavior/scans.md`). Nullables: `finishedAt`, `progress` (wire
   view), entry `parentPath`/`modifiedAt`/`fileType`/`category`/
   `fileCount`/`dirCount` (the counts: null on file rows, populated on
   dir rows), treemap rect `fileType`, scan `hotspots` column. Only
   `complete` scans persist results; cancelled/failed persist a metadata
   row only, and the collection prunes to the newest 25 after each
   terminal persist.
2. **Ownership**: which process writes the database? (One: the API. Every
   other surface — CLI, MCP server, GUI app — is an HTTP client. This is a
   data-safety property: SQLite corruption stories start with two writers.)
3. **Trust boundary**: what does the API key actually protect against, and
   what is deliberately out of scope? (Local co-users and stray browser JS;
   NOT network attackers — the server binds loopback only. Also note the
   product's own posture: Phantom NEVER deletes files; classification output
   is suggestions with safe-tool hints, so there is no destructive surface
   to protect.)
4. **Wire format edge cases**: which formats are stricter/looser than their
   standard names suggest? (All of `06-interchange/wire-format.md`: 6-digit
   canonical datetimes with generous decode, lowercase-out/any-case-in
   UUIDs, present-as-null, the category enum strings, the hotspots hint
   convention.)
5. **Failure surfaces**: where do errors become visible? (API `{error}`
   shape on EVERY non-2xx; server stderr → a log file the user never sees —
   anything a user must see travels over the API; CLI exit codes 0/1/2/3/4;
   MCP `isError` text results.)
6. **Config resolution order**: env → key file → defaults; which profile
   invariants are safety properties? (`04-config/config-spec.md` — test
   mode's refusal of prod paths is load-bearing.)
7. **Sizes**: which number is THE size? (`diskSize` = st_blocks × 512,
   everywhere, on every surface; `logicalSize` is a labelled secondary. The
   headline claim of the product depends on this — `du`-style logical sizes
   overstate cloud-dataloaded trees whose physical footprint is ~0.)
8. **What is deliberately absent?** No delete API anywhere. No MCP tools
   for cancel/delete/tree/entry/get_scan (`02-contracts/mcp-protocol.md`
   documents the omissions as decisions). No pagination on `/scans`
   (keep-last-N retention bounds it). No push channel — polling is the
   contract.

## Audit log

| Date | Auditor | Scope | Findings |
|---|---|---|---|
| 2026-09-01 | Phase-7 kit fill (agent, lead-verified) | whole system at v1.0-pre (post Note-slice removal, post hotspots) | answers above; drafted 02/03/04/06 reconciled against code — contradictions fixed were: MCP tool list (7, not 10), `scan_directory` waits by default, `find_large_files` has no `sort` input, `scanId` optional everywhere, `/hotspots` returns an object not an array, schema is v2 with no notes table |
