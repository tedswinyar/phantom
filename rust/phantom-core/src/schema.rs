// Schema versioning: forward-only migrations tracked via SQLite's
// user_version pragma. A database is never migrated backwards; restoring
// an older app version against a newer database refuses loudly instead of
// corrupting silently. (See docs/data-safety.md for the conventions.)
//
// The baseline is Phantom v1 (scans + entries), a one-time reset of the
// template's Note-slice ladder recorded in ADR-0003. From the first tagged
// release onward every change is a new arm and a CURRENT_VERSION bump.

use rusqlite::Connection;

use crate::{CoreError, Result};

/// Bump this when adding a migration below.
pub const CURRENT_VERSION: i64 = 4;

/// Validate an existing database or initialize a fresh one, applying any
/// pending forward migrations. Refuses databases newer than this build.
pub fn validate_or_init(conn: &Connection) -> Result<()> {
    let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;

    if version > CURRENT_VERSION {
        return Err(CoreError::Schema(format!(
            "database is schema v{version} but this build supports at most \
             v{CURRENT_VERSION}; refusing to open (upgrade the app, do not \
             downgrade the database)"
        )));
    }

    let mut v = version;
    while v < CURRENT_VERSION {
        apply_migration(conn, v)?;
        v += 1;
    }
    Ok(())
}

/// Apply one migration step (`from` → `from + 1`) and its `user_version`
/// bump as a SINGLE transaction. `user_version` is transactional in SQLite,
/// so a crash anywhere inside this call rolls back BOTH the DDL and the
/// version bump — the database is left at `from`, cleanly re-runnable. The
/// old shape (DDL auto-commits, then a separate bump) had a window where a
/// crash left the schema ahead of the version, wedging the DB un-openable
/// (`table notes already exists` / `duplicate column`) on the next boot.
fn apply_migration(conn: &Connection, from: i64) -> Result<()> {
    // `unchecked_transaction` gives us a transaction over `&Connection`
    // (we do not own it mutably here); dropped-without-commit == rollback.
    let tx = conn.unchecked_transaction()?;
    migrate_step(&tx, from)?;
    tx.pragma_update(None, "user_version", from + 1)?;
    tx.commit()?;
    Ok(())
}

/// Apply the single migration from `from` to `from + 1`.
/// Keep the pattern: one function, one version step, additive DDL only.
fn migrate_step(conn: &Connection, from: i64) -> Result<()> {
    match from {
        0 => {
            // v1: the Phantom baseline (ADR-0003).
            //
            // Sizes are bytes; disk_size is st_blocks × 512 (THE size),
            // logical_size is secondary. entries.category stays NULL until
            // the Phase-5 classifier; nlink/dev/ino feed its hardlink dedup.
            // UNIQUE (scan_id, path) doubles as the per-scan index (a scan
            // cannot contain the same path twice, and its index serves
            // scan_id-prefix lookups); (scan_id, parent_path) serves
            // direct-children queries.
            conn.execute_batch(
                "CREATE TABLE scans (
                    id                 TEXT PRIMARY KEY,
                    root_path          TEXT NOT NULL,
                    status             TEXT NOT NULL CHECK (status IN
                        ('running', 'complete', 'cancelled', 'failed')),
                    started_at         TEXT NOT NULL,
                    finished_at        TEXT,
                    total_disk_size    INTEGER NOT NULL,
                    total_logical_size INTEGER NOT NULL,
                    file_count         INTEGER NOT NULL,
                    dir_count          INTEGER NOT NULL,
                    error_count        INTEGER NOT NULL
                );

                CREATE TABLE entries (
                    scan_id      TEXT NOT NULL REFERENCES scans(id)
                                     ON DELETE CASCADE,
                    path         TEXT NOT NULL,
                    parent_path  TEXT,
                    name         TEXT NOT NULL,
                    is_dir       INTEGER NOT NULL,
                    disk_size    INTEGER NOT NULL,
                    logical_size INTEGER NOT NULL,
                    modified_at  TEXT,
                    file_type    TEXT,
                    category     TEXT,
                    nlink        INTEGER NOT NULL,
                    dev          INTEGER NOT NULL,
                    ino          INTEGER NOT NULL,
                    UNIQUE (scan_id, path)
                );
                CREATE INDEX entries_by_scan_parent
                    ON entries (scan_id, parent_path);

                -- Per-scan aggregate totals by file type, computed from the
                -- FULL walk before the ADR-0005 persistence filter, so type
                -- breakdowns see files whose individual rows were omitted.
                -- Joins the v1 baseline (not a v2 arm) inside the pre-release
                -- window ADR-0003 closes at the first tagged release.
                CREATE TABLE scan_file_types (
                    scan_id    TEXT NOT NULL REFERENCES scans(id)
                                   ON DELETE CASCADE,
                    file_type  TEXT,
                    disk_size  INTEGER NOT NULL,
                    file_count INTEGER NOT NULL,
                    UNIQUE (scan_id, file_type)
                );",
            )?;
        }
        1 => {
            // v2 (Phase 5): the per-scan hotspots summary, stored as the
            // camelCase wire JSON of `classify::HotspotsSummary` (the DB
            // string and the wire string are the same string, like
            // entries.category). NULL == no summary: cancelled/failed scans
            // (partial results are discarded) and rows persisted before v2.
            conn.execute_batch("ALTER TABLE scans ADD COLUMN hotspots TEXT;")?;
        }
        2 => {
            // v3 (phantom-chp): per-directory descendant counts, aggregated
            // from the FULL walk at persistence time (like dir disk sizes) —
            // the Folders tree's Items column; a client-side count over
            // persisted rows misses every sub-1-MiB file. NULL == not
            // recorded: file rows always, and dir rows persisted before v3
            // (the walk is gone; a backfill would be the undercount).
            conn.execute_batch(
                "ALTER TABLE entries ADD COLUMN file_count INTEGER;
                 ALTER TABLE entries ADD COLUMN dir_count INTEGER;",
            )?;
        }
        3 => {
            // v4 (phantom-671): capped unreadable-path sample, stored as the
            // camelCase wire JSON array of `scan::UnreadablePath` (DB string
            // == wire string, like hotspots). NULL == not recorded: rows
            // persisted before v4 (the walk is gone; no backfill). The
            // count column stays the truth; this is the first
            // UNREADABLE_SAMPLE_CAP paths with the OS's reason for each.
            conn.execute_batch("ALTER TABLE scans ADD COLUMN unreadable_paths TEXT;")?;
        }
        other => {
            return Err(CoreError::Schema(format!(
                "no migration step defined from v{other}"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Connection {
        Connection::open_in_memory().unwrap()
    }

    #[test]
    fn fresh_database_initializes_to_current_version() {
        let conn = mem();
        validate_or_init(&conn).unwrap();
        let v: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(v, CURRENT_VERSION);
    }

    #[test]
    fn baseline_creates_all_v1_tables() {
        let conn = mem();
        validate_or_init(&conn).unwrap();
        for table in ["scans", "entries", "scan_file_types"] {
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master
                     WHERE type = 'table' AND name = ?1",
                    [table],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(count, 1, "table {table} must exist");
        }
    }

    #[test]
    fn refuses_database_from_the_future() {
        let conn = mem();
        conn.pragma_update(None, "user_version", CURRENT_VERSION + 1)
            .unwrap();
        let err = validate_or_init(&conn).unwrap_err();
        assert!(matches!(err, CoreError::Schema(_)));
    }

    #[test]
    fn validate_is_idempotent_on_current_database() {
        let conn = mem();
        validate_or_init(&conn).unwrap();
        validate_or_init(&conn).unwrap();
    }

    /// A migration step and its version bump are one transaction: if the DDL
    /// fails, the `user_version` must NOT advance. Mutation-proof — move the
    /// `pragma_update` out of the transaction (or ahead of the DDL) and this
    /// fails, because a failed migration would then leave the version ahead
    /// of the schema: the exact wedge the atomic design exists to prevent.
    /// (The template proved this against its v1→v2 arm; post-ADR-0003 the
    /// conflict is synthesized against the baseline instead.)
    #[test]
    fn failed_migration_does_not_advance_user_version() {
        let conn = mem();
        // A v0 database in which `scans` ALREADY exists, so the baseline's
        // `CREATE TABLE scans` fails partway through the migration.
        conn.execute_batch("CREATE TABLE scans (id TEXT PRIMARY KEY);")
            .unwrap();

        let err = apply_migration(&conn, 0);
        assert!(err.is_err(), "duplicate-table CREATE must fail");

        let v: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(v, 0, "version must stay at 0 when the migration rolls back");
    }

    /// An interrupted migration (transaction rolled back, as a crash-in-flight
    /// leaves it under the atomic design) must leave the DB cleanly at the
    /// pre-migration version, and a re-run must complete it. Proves the
    /// "re-running after a partial migration is safe" guarantee.
    #[test]
    fn interrupted_migration_is_recoverable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("phantom.db");

        {
            let conn = Connection::open(&path).unwrap();

            // Simulate a migration interrupted mid-flight: apply the DDL and
            // the version bump inside a transaction, then roll it back (== a
            // crash before COMMIT under the atomic design).
            let tx = conn.unchecked_transaction().unwrap();
            migrate_step(&tx, 0).unwrap();
            tx.pragma_update(None, "user_version", 1).unwrap();
            tx.rollback().unwrap();

            // The interrupted migration left NO partial state.
            let v: i64 = conn
                .pragma_query_value(None, "user_version", |r| r.get(0))
                .unwrap();
            assert_eq!(v, 0, "rolled-back migration must leave version at 0");
            // Preparing a SELECT on a non-existent table fails at compile
            // time inside SQLite, so this is a clean table-presence probe.
            let scans_exists = conn.prepare("SELECT id FROM scans").is_ok();
            assert!(!scans_exists, "scans must not exist after rollback");
        }

        // Re-open: validate_or_init must finish the migration cleanly.
        let conn = Connection::open(&path).unwrap();
        validate_or_init(&conn).unwrap();
        let v: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(v, CURRENT_VERSION);
        let scans_exists = conn.prepare("SELECT id FROM scans").is_ok();
        assert!(scans_exists, "recovered migration must create the schema");
    }

    #[test]
    fn no_step_defined_beyond_current_version() {
        let conn = mem();
        let err = migrate_step(&conn, CURRENT_VERSION).unwrap_err();
        assert!(matches!(err, CoreError::Schema(_)));
    }

    /// Build a database frozen at v1 (the pre-hotspots baseline).
    fn v1_database() -> Connection {
        let conn = mem();
        apply_migration(&conn, 0).unwrap();
        let v: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(v, 1, "precondition: database is at v1");
        conn
    }

    /// The additive-migration promise (docs/data-safety.md): rows written
    /// under v1 survive the v2 arm untouched, with the new column NULL.
    #[test]
    fn v1_rows_survive_the_v2_migration_with_null_hotspots() {
        let conn = v1_database();
        conn.execute_batch(
            "INSERT INTO scans (id, root_path, status, started_at, finished_at,
                total_disk_size, total_logical_size, file_count, dir_count,
                error_count)
             VALUES ('scan-1', '/haunt', 'complete',
                '2026-01-01T00:00:00.000000Z', '2026-01-01T00:01:00.000000Z',
                4096, 4000, 2, 1, 0);
             INSERT INTO entries (scan_id, path, parent_path, name, is_dir,
                disk_size, logical_size, modified_at, file_type, category,
                nlink, dev, ino)
             VALUES ('scan-1', '/haunt', NULL, 'haunt', 1, 4096, 4000, NULL,
                NULL, NULL, 1, 0, 0);",
        )
        .unwrap();

        validate_or_init(&conn).unwrap();
        let v: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(v, CURRENT_VERSION);

        let (root, hotspots): (String, Option<String>) = conn
            .query_row(
                "SELECT root_path, hotspots FROM scans WHERE id = 'scan-1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(root, "/haunt");
        assert_eq!(hotspots, None, "pre-v2 rows carry NULL hotspots");
        let entries: i64 = conn
            .query_row("SELECT COUNT(*) FROM entries", [], |r| r.get(0))
            .unwrap();
        assert_eq!(entries, 1, "entry rows survive the migration");
    }

    /// The v2 arm is transactional like every other: a failing ALTER (the
    /// column already exists) must leave the version at 1, cleanly
    /// re-runnable — same wedge-prevention contract as the baseline test.
    #[test]
    fn failed_v2_migration_does_not_advance_user_version() {
        let conn = v1_database();
        conn.execute_batch("ALTER TABLE scans ADD COLUMN hotspots TEXT;")
            .unwrap();

        let err = apply_migration(&conn, 1);
        assert!(err.is_err(), "duplicate-column ALTER must fail");
        let v: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(v, 1, "version must stay at 1 when the v2 arm rolls back");
    }

    #[test]
    fn fresh_database_has_the_hotspots_column() {
        let conn = mem();
        validate_or_init(&conn).unwrap();
        // Preparing a SELECT on a missing column fails at compile time
        // inside SQLite — a clean column-presence probe.
        assert!(conn.prepare("SELECT hotspots FROM scans").is_ok());
    }

    /// Build a database frozen at v2 (post-hotspots, pre-counts).
    fn v2_database() -> Connection {
        let conn = v1_database();
        apply_migration(&conn, 1).unwrap();
        conn
    }

    /// Additive-migration promise for the v3 arm: entry rows written under
    /// v2 survive with NULL counts (not recorded ≠ zero).
    #[test]
    fn v2_entry_rows_survive_the_v3_migration_with_null_counts() {
        let conn = v2_database();
        conn.execute_batch(
            "INSERT INTO scans (id, root_path, status, started_at, finished_at,
                total_disk_size, total_logical_size, file_count, dir_count,
                error_count, hotspots)
             VALUES ('scan-1', '/haunt', 'complete',
                '2026-01-01T00:00:00.000000Z', '2026-01-01T00:01:00.000000Z',
                4096, 4000, 2, 1, 0, NULL);
             INSERT INTO entries (scan_id, path, parent_path, name, is_dir,
                disk_size, logical_size, modified_at, file_type, category,
                nlink, dev, ino)
             VALUES ('scan-1', '/haunt', NULL, 'haunt', 1, 4096, 4000, NULL,
                NULL, NULL, 1, 0, 0);",
        )
        .unwrap();

        validate_or_init(&conn).unwrap();
        let v: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(v, CURRENT_VERSION);

        let (files, dirs): (Option<i64>, Option<i64>) = conn
            .query_row(
                "SELECT file_count, dir_count FROM entries WHERE path = '/haunt'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((files, dirs), (None, None), "pre-v3 rows carry NULL counts");
    }

    /// The v3 arm is transactional: with dir_count pre-created, the SECOND
    /// ALTER fails and the FIRST must roll back with the version — neither a
    /// half-applied arm nor a version ahead of the schema.
    #[test]
    fn failed_v3_migration_rolls_back_both_alters() {
        let conn = v2_database();
        conn.execute_batch("ALTER TABLE entries ADD COLUMN dir_count INTEGER;")
            .unwrap();

        let err = apply_migration(&conn, 2);
        assert!(err.is_err(), "duplicate-column ALTER must fail");
        let v: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(v, 2, "version must stay at 2 when the v3 arm rolls back");
        assert!(
            conn.prepare("SELECT file_count FROM entries").is_err(),
            "the arm's first ALTER must roll back with the second"
        );
    }

    #[test]
    fn fresh_database_has_the_count_columns() {
        let conn = mem();
        validate_or_init(&conn).unwrap();
        assert!(conn.prepare("SELECT file_count, dir_count FROM entries").is_ok());
    }
}
