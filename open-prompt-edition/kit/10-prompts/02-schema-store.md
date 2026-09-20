# Prompt 02 — Schema, store, migrations

## Task

Implement the SQLite layer: the schema ladder in `../03-schema/schema.sql`,
a store for scans/entries/type-totals/hotspots, and forward-only migrations.

## Requirements

1. Migrations track SQLite's `user_version`: v1 baseline (scans, entries,
   scan_file_types + indexes), v2 `ALTER TABLE scans ADD COLUMN hotspots
   TEXT`, v3 `ALTER TABLE entries ADD COLUMN file_count INTEGER; ADD COLUMN
   dir_count INTEGER` — the full ladder with per-column comments is
   `../03-schema/schema.sql`. Refuse to open a database whose version
   exceeds what you support.
2. Each migration step and its version bump are ONE transaction: a crash
   mid-migration leaves the database cleanly at the previous version,
   re-runnable — never schema-ahead-of-version.
3. Connections run with foreign keys ON; file databases use WAL.
4. Insert of a scan + its entries + type totals + hotspots summary is ONE
   transaction — a failed entry batch must roll the scan row back too.
5. Reads: get/list scans (newest `startedAt` first, id tiebreak), entries
   path-ordered, direct children by `parent_path`, single entry by path,
   filtered/sorted/paginated file pages (escape LIKE metacharacters in
   search needles), type totals (disk desc, name tiebreak), hotspots JSON
   round-trip, and entry `file_count`/`dir_count` round-trip (NULL stays
   NULL; the orderings are tabled in `../05-algorithms/`). Unknown scan is
   a not-found error, never an empty list.
6. Keep-last-N retention (`prune to last N` deletes older scans; entries and
   totals cascade). The API layer (prompt 04) calls it with N = 25 after
   every terminal persist.

## Exit criterion

Migration tests pass: fresh init reaches v3; a database frozen at each
older version, with rows, survives every later arm (new columns NULL);
a failed step does not advance `user_version`; a future version is refused.
Store round-trip tests decode the shared fixtures' semantics
(`../08-fixtures/`) — raw fixture bytes in, equal values back out.

## Sharp edges

- SQLite ships foreign keys OFF per connection — without the pragma, ON
  DELETE CASCADE silently never fires and deleted scans leak entries.
- `user_version` IS transactional in SQLite; use that. Separate
  DDL-then-bump has a crash window that wedges the DB un-openable.
- Two connections to one file need a busy timeout, or the second writer
  fails immediately with SQLITE_BUSY instead of waiting.
- Store the hotspots summary as the camelCase wire JSON — the DB string and
  the wire string are the same string (categories too).
