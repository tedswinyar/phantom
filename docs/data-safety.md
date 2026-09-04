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
- Keep-last-N retention (`ScanStore::prune_to_last`) bounds the live file's
  growth for the same reason: old scans are cheaper to regenerate than to
  keep forever. It is WIRED into scan completion: after every successful
  terminal persist, `finish_scan` prunes to the newest
  `KEEP_LAST_SCANS = 25` (a product decision pending a settings surface;
  the constant lives in `rust/phantom-api/src/scans.rs`). A prune failure
  never fails the scan — the scan is already persisted; the failure is
  logged and the next completion retries.
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

## When you replace the Note slice

Keep all three properties: `user_version` migrations (forward-only, tested),
`backup_verified` semantics, single-writer via the API. The shapes are in
place; extend them rather than re-deriving.
