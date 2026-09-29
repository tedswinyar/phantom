# Data safety — SQLite conventions

The user's data lives in one SQLite file. These conventions exist because
each of their absences has destroyed someone's data somewhere; the template
codifies them from day zero.

## Schema versioning: forward-only

- The schema version lives in SQLite's `user_version` pragma.
  `schema::validate_or_init` (`rust/phantom-core/src/schema.rs`) applies
  pending migrations on open and **refuses databases newer than the build**.
- Migrations are additive, one version step per function arm, and never
  rewritten once shipped. Downgrading the app against an upgraded database is
  a refusal, not a best-effort — silent corruption is worse than an error.
- Every migration ships with tests proving a failed step rolls back cleanly
  and a re-run completes it (`failed_migration_does_not_advance_user_version`
  and `interrupted_migration_is_recoverable` in `schema.rs`); once the ladder
  grows past the v1 baseline (ADR-0003), each new arm also proves existing
  rows survive it.

## Backups: not a backup until restored

`backup::backup_verified` (`rust/phantom-core/src/backup.rs`) is the only
approved snapshot path:

1. Uses SQLite's online backup API (consistent even mid-write).
2. **Re-opens the copy out-of-place**, validates its schema, and compares row
   counts before reporting success.
3. Refuses to overwrite an existing backup file.

A copy that has never been opened somewhere else is a hope, not a backup.
Restore drills belong in the release checklist: before each release, restore
the latest backup into a temp profile and open it.

### v1.0 posture: scan entries are regenerable

Scan data is derived — a rescan of the same root reproduces it. What a backup
must protect is the `scans` metadata (which roots were scanned, when, with
what totals), not the bulk `entries` rows:

- **Back up scans metadata; size-cap or exclude entries.** If entry volume
  makes whole-file snapshots expensive (the Phase-1 measurement puts 100k
  entries in the tens of MB), the backup path drops or caps `entries` before
  it grows a second storage tier. Losing entries costs one rescan; losing
  scan metadata costs history.
- Retention (`ScanStore::prune_retention`) bounds the live file's growth
  for the same reason: old scans are cheaper to regenerate than to keep
  forever. It is WIRED into scan completion: after every successful
  terminal persist, `finish_scan` keeps the newest **25 scans of each
  root**, then the newest **100 overall**, then — the binding constraint
  since 1.1.1 (phantom-cnr.3) — holds the history to a **2 GiB byte
  budget**: while the estimated live bytes exceed it, the globally oldest
  completed scan of any root that still has more than **2 completed scans**
  goes. The floor is never crossed for a root whose newest completed scan
  is 30 days old or younger (diff, verify and growth need a pair), so
  many large, recently scanned roots can exceed the budget — the API log
  then says so plainly ("budget 2.0 GiB exceeded by floor: …") rather than
  pretending the budget held. A root nobody has scanned in 30 days has no
  floor (phantom-ccq, Ted 2026-09-22): its scans are ordinary budget
  victims, oldest first, and the log names the root and its age when that
  happens ("floor waived for /x: its newest completed scan is 45 days
  old") — month-old history is cheaper to regenerate than to keep, which is
  this file's own argument. Bytes per scan are estimated as
  `rows(scan) × live bytes / rows(all)` (no schema change; `scans.file_count`
  is pre-filter and is deliberately not used). Count was the wrong unit on
  its own: measured 2026-09-16, 25 per root authorised ~10.8 GB for `~`
  alone. (`Retention` in `rust/phantom-api/src/config.rs`; the defaults are
  a product decision — per root since 1.1 so one folder's history is never
  evicted by another's; operators may override with
  `PHANTOM_KEEP_SCANS_PER_ROOT` / `PHANTOM_KEEP_SCANS` /
  `PHANTOM_DB_BUDGET_BYTES`, all positive, total ≥ per-root.) A prune
  failure never fails the scan — the scan is already persisted; the failure
  is logged and the next completion retries. Compaction (below) runs after
  every prune, so the evicted bytes reach the filesystem. Since 1.1.1
  (phantom-cnr.7) the retention line is written on EVERY completion, pruned
  or not, and each scan has a `scan requested` / `scan finished` pair naming
  its root, options, client User-Agent, rows and estimated bytes — the file's
  growth is attributable from the log alone (docs/troubleshooting.md, "A
  scan nobody asked for").
- `backup_verified` semantics are unchanged: whatever is backed up is
  re-opened out-of-place and verified before the backup is trusted.

## The API owns the file

Exactly one process opens the database: `phantom-api`. The CLI, MCP
server, and app go through HTTP. This is a data-safety property, not just an
architecture taste — SQLite corruption stories usually start with two
writers.

Test-profile guardrail: `PHANTOM_PROFILE=test` refuses any database path
under the prod data directory (`config.rs`), so a mis-exported env var in a
test run cannot touch real data.

## The WAL is bounded (1.1.1)

File stores run in WAL mode. The bundled SQLite ships with
`journal_size_limit = -1`, so a WAL only ever grows to the high-water mark of
the largest transaction — a 400–600 MB scan insert — and the automatic
PASSIVE checkpoint resets the write pointer without shrinking the file. That
is how `phantom.db-wal` sat at 512 MB while idle on 2026-09-16 and reached
1.5 GB during a VACUUM (phantom-cnr.1).

Two connection-level rules, no schema change:

- `ScanStore::open` sets `journal_size_limit` to **64 MiB**
  (`store::WAL_SIZE_LIMIT_BYTES`), so any checkpoint that resets the WAL
  also truncates it to at most that.
- After every large write — `insert_scan`, `delete_scan`, `prune_to_last`
  (and so `prune_retention`) — the store runs `PRAGMA
  wal_checkpoint(TRUNCATE)`, which folds the WAL into the main file and
  truncates it to zero. A reader holding the WAL (the app's 1 Hz poll) makes
  the checkpoint `busy`; the store waits 100 ms and tries once more, then
  logs a warning and moves on — the write has committed, and the next write
  tries again. The result triple is logged at debug.

**Operational rule:** a `phantom.db-wal` larger than 64 MiB once the API has
been up for a minute is a bug, not a state. `ls -l "$HOME/Library/Application
Support/phantom/"` shows it; the store tests measure the file directly.

## Deleted pages go back to the filesystem (1.1.1)

Until 1.1.1 nothing ever ran a VACUUM: retention and `DELETE /scans/{id}`
removed rows, SQLite kept the pages on its freelist, and the file never
shrank (pruning 25 scans to 3 left a 4.2 GB file that a manual VACUUM took
to 1.4 GB, 2026-09-16). `phantom_core::compact` (phantom-cnr.2):

- **New files are `auto_vacuum=INCREMENTAL`** — set by `ScanStore::open`
  before the schema exists. A file-header flag, not a schema version: the
  kit's `schema.sql` is unchanged and a 1.1.0 binary opens the file.
- **After every prune and every delete** the API runs one compaction pass on
  a connection of its own (the store mutex is held only to read the path):
  `incremental_vacuum` in 2000-page steps with a TRUNCATE checkpoint between
  steps, so the WAL stays under its ceiling. `DELETE /scans/{id}` runs it
  before answering 204, so the bytes are back when the CLI prints "deleted".
- **A pre-1.1.1 file (`auto_vacuum=NONE`) is converted ONCE**, by a real
  `VACUUM`, and only when all three hold: dead pages ≥ 256 MiB and ≥ 25 % of
  the file; no scan running; the volume has ≥ 2× the live size available
  (a VACUUM in WAL mode writes the whole new image into the WAL). Otherwise
  the refusal is logged with its numbers and tried again on the next write.
  Compaction must never make a disk emergency worse.

The API log carries `compaction: returned N MB to the filesystem (file now
M MB)`, or `compaction deferred: <why>`. Reporting the number on the wire is
v1.2 work (the storage surface on `/volume`).

## When you replace the Note slice

Keep all three properties: `user_version` migrations (forward-only, tested),
`backup_verified` semantics, single-writer via the API. The shapes are in
place; extend them rather than re-deriving.
