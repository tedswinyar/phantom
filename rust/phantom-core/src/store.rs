// SQLite persistence. ScanStore owns the scan domain — the ONE writer
// (via phantom-api); keep the shape when extending.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, Row, params};
use uuid::Uuid;

use crate::classify::HotspotsSummary;
use crate::persist;
use crate::plan::ReclaimPlan;
use crate::format::FileTypeTotal;
use crate::scan::{EntryFlags, Scan, ScanEntry, ScanStatus};
use crate::{CoreError, Result, schema, wire_time};

/// How long a writer waits on a lock held by another connection before
/// erroring. Tooling (backup verification, ad-hoc readers) can open a
/// second connection to the same file; without a busy timeout that second
/// connection fails immediately with SQLITE_BUSY instead of waiting out a
/// scan's entry batch.
const BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// The WAL's idle ceiling (phantom-cnr.1). The bundled SQLite ships with
/// `journal_size_limit = -1`, so a WAL only ever grows to the high-water mark
/// of the largest transaction — a 400–600 MB scan insert — and the PASSIVE
/// auto-checkpoint resets the write pointer without ever shrinking the file
/// (measured 2026-09-16: `phantom.db-wal` 512 MB while idle; 1.5 GB during a
/// VACUUM). With the limit set, any checkpoint that resets the WAL also
/// truncates it to at most this; [`ScanStore::checkpoint_truncate`] after
/// every large write takes it to zero.
pub const WAL_SIZE_LIMIT_BYTES: u64 = 64 * 1024 * 1024;

/// How long to wait before the one retry of a checkpoint a reader blocked.
const CHECKPOINT_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(100);

/// The byte budget never evicts a root below this many COMPLETED scans:
/// diff, verify and growth each need a pair (phantom-cnr.3). Not an
/// operator knob — the budget is (`PHANTOM_DB_BUDGET_BYTES`); the floor is
/// what keeps the budget from deleting the history those features read.
pub const BUDGET_FLOOR_PER_ROOT: usize = 2;

/// The floor protects a root only while its newest completed scan is this
/// many days old or younger (phantom-ccq, Ted 2026-09-22: "evicting scans
/// over 30 days is correct. We don't want a tool for optimizing disk space
/// gobbling up disk space unnecessarily"). A root nobody has scanned in a
/// month has floor 0: its scans are ordinary budget victims — oldest
/// first, only while the budget is exceeded — because a month-old history
/// is cheaper to regenerate than to keep (docs/data-safety.md). Exactly 30
/// days is still protected; the floor lapses one second later. Like the
/// floor itself, a core constant rather than an env knob.
pub const BUDGET_FLOOR_MAX_AGE_DAYS: i64 = 30;

/// One scan's estimated share of the database file
/// ([`ScanStore::scan_footprints`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanFootprint {
    pub id: Uuid,
    /// `root_path` with trailing slashes stripped (the retention identity).
    pub root: String,
    pub status: ScanStatus,
    /// The retention ordering key — what a root's age is measured by.
    pub started_at: chrono::DateTime<chrono::Utc>,
    /// Persisted entry rows (post-filter — what the scan actually stores).
    pub rows: u64,
    /// `rows × live bytes / all rows`; 0 when the database has no rows.
    pub estimated_bytes: u64,
}

/// Why a prune stopped above the budget: every root is at or below the
/// floor. Logged honestly rather than pretending the budget held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FloorOverBudget {
    /// Roots that still hold at least one completed scan.
    pub roots: usize,
    /// Completed scans those roots hold between them.
    pub scans: usize,
    /// What they are estimated to occupy.
    pub estimated_bytes: u64,
}

/// A root the floor did not protect: its newest completed scan was older
/// than [`BUDGET_FLOOR_MAX_AGE_DAYS`] when the budget pass took a scan
/// the floor would otherwise have kept. Reported only when the waiver
/// actually decided an eviction, so the API log names the root and its
/// age exactly when a month-old history went (phantom-ccq).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FloorWaived {
    /// `root_path` with trailing slashes stripped.
    pub root: String,
    /// `started_at` of the root's newest completed scan.
    pub newest_started_at: chrono::DateTime<chrono::Utc>,
    /// `now − newest_started_at` at prune time.
    pub age: chrono::Duration,
}

/// What [`ScanStore::prune_retention_budget`] did, for the API log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PruneReport {
    pub by_per_root: usize,
    pub by_total: usize,
    pub by_budget: usize,
    /// Estimated live bytes after the count caps, before the budget pass.
    pub estimated_bytes_before: u64,
    /// Estimated live bytes after the budget pass (what the file will hold
    /// once compaction returns the freed pages).
    pub estimated_bytes_after: u64,
    pub floor_over_budget: Option<FloorOverBudget>,
    /// Roots whose floor was waived by age, one entry per root, in the
    /// order their first below-floor scan was evicted.
    pub floor_waived: Vec<FloorWaived>,
}

impl PruneReport {
    pub fn deleted(&self) -> usize {
        self.by_per_root + self.by_total + self.by_budget
    }
}

pub struct ScanStore {
    conn: Connection,
    /// The file, for maintenance that runs on its own connection
    /// (`compact`); `None` for in-memory stores.
    path: Option<std::path::PathBuf>,
}

/// Filter + order for [`ScanStore::files_page`]. `file_type` matches the
/// stored lowercased extension (input is lowercased — the wire accepts any
/// case); `search` is a case-insensitive substring match on the full path.
#[derive(Debug, Clone, Copy, Default)]
pub struct FileQuery<'a> {
    pub file_type: Option<&'a str>,
    pub search: Option<&'a str>,
    pub sort: FileSort,
}

/// Sort order for file listings. `Size` (disk, descending) is the default —
/// this is a disk-space product; the biggest file is the headline.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FileSort {
    #[default]
    Size,
    Name,
    Path,
}

impl std::str::FromStr for FileSort {
    type Err = CoreError;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "size" => Ok(FileSort::Size),
            "name" => Ok(FileSort::Name),
            "path" => Ok(FileSort::Path),
            other => Err(CoreError::InvalidInput(format!(
                "sort must be size, name, or path (got {other:?})"
            ))),
        }
    }
}

/// A single page of file entries plus the continuation offset for the next
/// page (`None` on the last page).
#[derive(Debug)]
pub struct FilePage {
    pub files: Vec<ScanEntry>,
    pub next_offset: Option<usize>,
}

/// A MAGNITUDE (a size or a count) for SQLite's signed 64-bit INTEGER:
/// clamped at i64::MAX instead of wrapping negative (phantom-ap6). A
/// hostile or corrupt filesystem can report `st_blocks` near u64::MAX; a
/// wrapped value would sort first in `ORDER BY disk_size DESC`, fail the
/// 1 MiB persistence filter, and sum wrong — silent size corruption.
/// 9.2 EB is beyond any real volume, so the clamp is never met honestly.
fn sql_magnitude(v: u64) -> i64 {
    v.min(i64::MAX as u64) as i64
}

/// An IDENTIFIER (dev, ino, clone id, flag bits) for SQLite: bit-cast, not
/// clamped. Identity must round-trip exactly — clamping two distinct inode
/// numbers above i64::MAX would merge two files into one sharing group —
/// and `as i64` / `as u64` is a bijection (two's complement), so nothing is
/// lost; the stored integer is merely "negative" for the high half.
/// What `PRAGMA wal_checkpoint(TRUNCATE)` reported: SQLite's `(busy, log,
/// checkpointed)` triple, plus whether the one retry was needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckpointOutcome {
    /// A reader held the WAL and the checkpoint could not complete.
    pub busy: bool,
    /// Pages in the WAL before the checkpoint (-1 on a non-WAL connection).
    pub wal_pages: i64,
    /// Pages moved into the main file (-1 on a non-WAL connection).
    pub checkpointed_pages: i64,
    /// The first attempt was busy and a second was made.
    pub retried: bool,
}

fn sql_identity(v: u64) -> i64 {
    v as i64
}

impl ScanStore {
    /// Open (or create) a store at `path`, validating the schema. File
    /// stores run in WAL mode so readers do not block during the large
    /// entry batches a scan insert writes.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        // Deleted pages go back to the filesystem in steps (compact.rs). Only
        // takes effect on an EMPTY file, and switching to WAL below writes the
        // database header — so this must be the FIRST pragma (found by
        // `new_stores_are_incremental`, 2026-09-21). Silently ignored on an
        // existing file, whose one-time conversion is compact()'s guarded
        // decision (phantom-cnr.2).
        conn.execute_batch("PRAGMA auto_vacuum = INCREMENTAL;")?;
        // journal_mode returns the resulting mode as a row, so query_row
        // rather than pragma_update.
        conn.query_row("PRAGMA journal_mode=WAL", [], |_| Ok(()))?;
        // Also returns its value as a row. Without this the WAL never shrinks
        // (see WAL_SIZE_LIMIT_BYTES); `wal_ceiling_is_set_on_file_stores` pins it.
        conn.query_row(
            &format!("PRAGMA journal_size_limit = {WAL_SIZE_LIMIT_BYTES}"),
            [],
            |_| Ok(()),
        )?;
        Self::init(conn, Some(path.to_path_buf()))
    }

    pub fn open_in_memory() -> Result<Self> {
        // In-memory databases cannot use WAL; everything else is identical.
        Self::init(Connection::open_in_memory()?, None)
    }

    /// Where the file lives (`None` for an in-memory store).
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    fn init(conn: Connection, path: Option<std::path::PathBuf>) -> Result<Self> {
        // Stock SQLite ships with foreign keys OFF per connection; without
        // enforcement, ON DELETE CASCADE silently never fires and deleted
        // scans leak their entries forever. Our bundled libsqlite3-sys
        // happens to default them ON (verified by probe, 2026-08-31), so
        // this line is defense-in-depth against a build-config change, and
        // `entry_without_scan_violates_foreign_key` pins the behavior
        // whichever mechanism provides it.
        conn.pragma_update(None, "foreign_keys", true)?;
        schema::validate_or_init(&conn)?;
        Ok(Self { conn, path })
    }

    /// The raw connection, for `backup::backup_verified`'s snapshot API.
    pub(crate) fn connection(&self) -> &Connection {
        &self.conn
    }

    /// Fold the WAL into the main file and truncate it to zero —
    /// `PRAGMA wal_checkpoint(TRUNCATE)` — after every large write, so the
    /// `-wal` file never sits at the last scan's high-water mark. A reader
    /// holding the WAL open (the app's 1 Hz poll) makes the checkpoint
    /// `busy`; that is short, so wait [`CHECKPOINT_RETRY_DELAY`] and try once
    /// more, then log rather than fail — the write itself has committed and
    /// the next write will try again. On a non-WAL connection (in-memory
    /// stores) SQLite answers `(0, -1, -1)` and this is a no-op.
    pub fn checkpoint_truncate(&self) -> Result<CheckpointOutcome> {
        let mut outcome = self.checkpoint_once()?;
        if outcome.busy {
            std::thread::sleep(CHECKPOINT_RETRY_DELAY);
            outcome = self.checkpoint_once()?;
            outcome.retried = true;
        }
        if outcome.busy {
            tracing::warn!(
                wal_pages = outcome.wal_pages,
                checkpointed_pages = outcome.checkpointed_pages,
                "wal checkpoint (truncate) still busy after a retry — a reader holds the WAL; the next write will try again"
            );
        } else {
            tracing::debug!(
                wal_pages = outcome.wal_pages,
                checkpointed_pages = outcome.checkpointed_pages,
                retried = outcome.retried,
                "wal checkpoint (truncate)"
            );
        }
        Ok(outcome)
    }

    fn checkpoint_once(&self) -> Result<CheckpointOutcome> {
        let (busy, wal_pages, checkpointed_pages): (i64, i64, i64) = self.conn.query_row(
            "PRAGMA wal_checkpoint(TRUNCATE)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        Ok(CheckpointOutcome {
            busy: busy != 0,
            wal_pages,
            checkpointed_pages,
            retried: false,
        })
    }

    /// Persist a scan, its entries, its per-type totals, and its hotspots
    /// summary atomically — one transaction, one prepared statement executed
    /// per row. A crash mid-insert leaves the scans row as it was, never a
    /// scan with half its entries.
    ///
    /// Called TWICE per scan since v6 (phantom-aoa): once at start with the
    /// running metadata and no rows (so a server that dies mid-walk leaves
    /// a row to mark, not a scan that vanished), and once at the end with
    /// the terminal metadata and the results. The scans row is an UPSERT by
    /// id; the terminal call replaces every metadata column.
    ///
    /// Per ADR-0005 callers pass the `persist::persistable_entries` view of
    /// the walk, and `type_totals` computed from the FULL walk. `hotspots`
    /// is the Phase-5 classifier's summary over the full walk; `None` for
    /// cancelled/failed scans (partial results are discarded).
    pub fn insert_scan(
        &self,
        scan: &Scan,
        entries: &[ScanEntry],
        type_totals: &[FileTypeTotal],
        hotspots: Option<&HotspotsSummary>,
    ) -> Result<()> {
        // Stored as the camelCase wire JSON — the DB string and the wire
        // string are the same string, like entries.category.
        let hotspots_json = hotspots
            .map(|s| {
                serde_json::to_string(s).map_err(|e| {
                    CoreError::Schema(format!("cannot serialize hotspots summary: {e}"))
                })
            })
            .transpose()?;
        // Like hotspots: the stored string IS the wire string.
        let unreadable_json = scan
            .unreadable_paths
            .as_ref()
            .map(|u| {
                serde_json::to_string(u).map_err(|e| {
                    CoreError::Schema(format!("cannot serialize unreadable paths: {e}"))
                })
            })
            .transpose()?;
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO scans (id, root_path, status, started_at, finished_at,
                total_disk_size, total_logical_size, file_count, dir_count,
                error_count, hotspots, unreadable_paths,
                total_private_size, total_shared_size, failure_reason)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
             ON CONFLICT(id) DO UPDATE SET
                root_path = excluded.root_path, status = excluded.status,
                started_at = excluded.started_at, finished_at = excluded.finished_at,
                total_disk_size = excluded.total_disk_size,
                total_logical_size = excluded.total_logical_size,
                file_count = excluded.file_count, dir_count = excluded.dir_count,
                error_count = excluded.error_count, hotspots = excluded.hotspots,
                unreadable_paths = excluded.unreadable_paths,
                total_private_size = excluded.total_private_size,
                total_shared_size = excluded.total_shared_size,
                failure_reason = excluded.failure_reason",
            params![
                scan.id.to_string(),
                scan.root_path,
                scan.status.as_str(),
                wire_time::to_wire(&scan.started_at),
                scan.finished_at.as_ref().map(wire_time::to_wire),
                sql_magnitude(scan.total_disk_size),
                sql_magnitude(scan.total_logical_size),
                sql_magnitude(scan.file_count),
                sql_magnitude(scan.dir_count),
                sql_magnitude(scan.error_count),
                hotspots_json,
                unreadable_json,
                scan.total_private_size.map(sql_magnitude),
                scan.total_shared_size.map(sql_magnitude),
                scan.failure_reason,
            ],
        )?;
        // The terminal call after the running row, or a retry: replace any
        // results already stored for this id rather than stacking them.
        tx.execute(
            "DELETE FROM entries WHERE scan_id = ?1",
            params![scan.id.to_string()],
        )?;
        tx.execute(
            "DELETE FROM scan_file_types WHERE scan_id = ?1",
            params![scan.id.to_string()],
        )?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO entries (scan_id, path, parent_path, name, is_dir,
                    disk_size, logical_size, modified_at, file_type, category,
                    nlink, dev, ino, file_count, dir_count,
                    private_size, shared_size, clone_id, flags)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                    ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
            )?;
            let scan_id = scan.id.to_string();
            for e in entries {
                stmt.execute(params![
                    scan_id,
                    e.path,
                    e.parent_path,
                    e.name,
                    e.is_dir,
                    sql_magnitude(e.disk_size),
                    sql_magnitude(e.logical_size),
                    e.modified_at.as_ref().map(wire_time::to_wire),
                    e.file_type,
                    e.category,
                    sql_magnitude(e.nlink),
                    sql_identity(e.dev),
                    sql_identity(e.ino),
                    e.file_count.map(sql_magnitude),
                    e.dir_count.map(sql_magnitude),
                    e.private_size.map(sql_magnitude),
                    e.shared_size.map(sql_magnitude),
                    e.clone_id.map(sql_identity),
                    e.flags.map(|f| i64::from(f.bits())),
                ])?;
            }
        }
        {
            let mut stmt = tx.prepare(
                "INSERT INTO scan_file_types (scan_id, file_type, disk_size,
                    file_count)
                 VALUES (?1, ?2, ?3, ?4)",
            )?;
            let scan_id = scan.id.to_string();
            for t in type_totals {
                stmt.execute(params![
                    scan_id,
                    t.file_type,
                    sql_magnitude(t.disk_size),
                    sql_magnitude(t.file_count),
                ])?;
            }
        }
        tx.commit()?;
        // The insert is the largest transaction this store ever runs; fold
        // it into the main file now or the WAL keeps its size until restart.
        self.checkpoint_truncate()?;
        Ok(())
    }

    pub fn get_scan(&self, id: Uuid) -> Result<Scan> {
        self.conn
            .query_row(
                "SELECT id, root_path, status, started_at, finished_at,
                    total_disk_size, total_logical_size, file_count, dir_count,
                    error_count, unreadable_paths, total_private_size, total_shared_size,
                    failure_reason
                 FROM scans WHERE id = ?1",
                params![id.to_string()],
                row_to_scan,
            )
            .optional()?
            .ok_or_else(|| CoreError::NotFound(format!("scan {id}")))
    }

    /// All scans, newest first.
    pub fn list_scans(&self) -> Result<Vec<Scan>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, root_path, status, started_at, finished_at,
                total_disk_size, total_logical_size, file_count, dir_count,
                error_count, unreadable_paths, total_private_size, total_shared_size,
                failure_reason
             FROM scans ORDER BY started_at DESC, id",
        )?;
        let scans = stmt
            .query_map([], row_to_scan)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(scans)
    }

    /// Cold-start recovery (phantom-aoa): every row still `running` belongs
    /// to a walk the previous server process never finished — this process
    /// has no worker for it. Mark them `failed` with `reason` so they show
    /// up terminal and deletable instead of running forever (or, before v6,
    /// not at all). Returns how many rows were marked.
    pub fn mark_running_as_interrupted(&self, reason: &str) -> Result<usize> {
        let now = wire_time::to_wire(&chrono::Utc::now());
        let marked = self.conn.execute(
            "UPDATE scans SET status = 'failed', finished_at = ?1, failure_reason = ?2
             WHERE status = 'running'",
            params![now, reason],
        )?;
        Ok(marked)
    }

    /// Every entry of a scan, path-ordered (deterministic for parity tests).
    /// Unknown scan is NotFound, not an empty list — callers that typo an id
    /// should find out.
    pub fn entries(&self, scan_id: Uuid) -> Result<Vec<ScanEntry>> {
        self.get_scan(scan_id)?;
        let mut stmt = self.conn.prepare(
            "SELECT path, parent_path, name, is_dir, disk_size, logical_size,
                modified_at, file_type, category, nlink, dev, ino,
                file_count, dir_count, private_size, shared_size, clone_id, flags
             FROM entries WHERE scan_id = ?1 ORDER BY path",
        )?;
        let entries = stmt
            .query_map(params![scan_id.to_string()], row_to_entry)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(entries)
    }

    /// Every DIRECTORY row's (path, aggregated diskSize) for a scan — the
    /// diff computation's view (phantom-081). Path-ordered like `entries`;
    /// far lighter than materializing full rows for a 500k-directory scan.
    pub fn dir_sizes(&self, scan_id: Uuid) -> Result<Vec<(String, u64)>> {
        self.get_scan(scan_id)?;
        let mut stmt = self.conn.prepare(
            "SELECT path, disk_size FROM entries
             WHERE scan_id = ?1 AND is_dir = 1 ORDER BY path",
        )?;
        let rows = stmt
            .query_map(params![scan_id.to_string()], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u64))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Direct children of `parent_path` within a scan (`None` == the scan
    /// root). Served by the (scan_id, parent_path) index; `IS` matches NULL.
    pub fn children_of(
        &self,
        scan_id: Uuid,
        parent_path: Option<&str>,
    ) -> Result<Vec<ScanEntry>> {
        self.get_scan(scan_id)?;
        let mut stmt = self.conn.prepare(
            "SELECT path, parent_path, name, is_dir, disk_size, logical_size,
                modified_at, file_type, category, nlink, dev, ino,
                file_count, dir_count, private_size, shared_size, clone_id, flags
             FROM entries WHERE scan_id = ?1 AND parent_path IS ?2
             ORDER BY path",
        )?;
        let entries = stmt
            .query_map(params![scan_id.to_string(), parent_path], row_to_entry)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(entries)
    }

    /// One entry of a scan by exact path. Unknown scan and unknown path are
    /// both NotFound, with messages that tell the caller which one it was.
    ///
    /// A path with no row is NOT necessarily a path that does not exist:
    /// ADR-0005 (files) and 1.1.1 (directories, phantom-cnr.10) omit rows
    /// below 1 MiB. So the miss walks up the path until a persisted ancestor
    /// is found and names it — that row is where the missing path's bytes
    /// and counts live. Mutation-proof: return the bare "path … in scan …"
    /// message again and `entry_below_threshold_names_its_persisted_ancestor`
    /// (here and in the API suite) fails.
    pub fn entry(&self, scan_id: Uuid, path: &str) -> Result<ScanEntry> {
        self.get_scan(scan_id)?;
        match self.entry_row(scan_id, path)? {
            Some(entry) => Ok(entry),
            None => {
                let message = match self.nearest_persisted_ancestor(scan_id, path)? {
                    Some(ancestor) => format!(
                        "path {path:?} in scan {scan_id} has no row: {} \
                         (absent, or a file or subtree under 1 MiB); its bytes and counts \
                         are in the nearest persisted ancestor {ancestor:?}",
                        persist::NOT_INDIVIDUALLY_PERSISTED
                    ),
                    None => format!("path {path:?} in scan {scan_id}"),
                };
                Err(CoreError::NotFound(message))
            }
        }
    }

    fn entry_row(&self, scan_id: Uuid, path: &str) -> Result<Option<ScanEntry>> {
        Ok(self
            .conn
            .query_row(
                "SELECT path, parent_path, name, is_dir, disk_size, logical_size,
                    modified_at, file_type, category, nlink, dev, ino,
                file_count, dir_count, private_size, shared_size, clone_id, flags
                 FROM entries WHERE scan_id = ?1 AND path = ?2",
                params![scan_id.to_string(), path],
                row_to_entry,
            )
            .optional()?)
    }

    /// The closest strict ancestor of `path` that has a row in the scan, or
    /// None when no ancestor does (a path outside the scan root, or a scan
    /// that persisted no rows). One indexed lookup per component.
    pub fn nearest_persisted_ancestor(&self, scan_id: Uuid, path: &str) -> Result<Option<String>> {
        let mut cursor = Path::new(path).parent();
        while let Some(dir) = cursor {
            let key = dir.to_string_lossy();
            if key.is_empty() {
                break;
            }
            if self.entry_row(scan_id, &key)?.is_some() {
                return Ok(Some(key.into_owned()));
            }
            cursor = dir.parent();
        }
        Ok(None)
    }

    /// One page of a scan's FILE entries (directories excluded), filtered
    /// and sorted per `query`. Offset-based paging; the caller treats the returned offset as an opaque
    /// continuation token.
    pub fn files_page(
        &self,
        scan_id: Uuid,
        query: &FileQuery<'_>,
        limit: usize,
        offset: usize,
    ) -> Result<FilePage> {
        self.get_scan(scan_id)?;

        let mut sql = String::from(
            "SELECT path, parent_path, name, is_dir, disk_size, logical_size,
                modified_at, file_type, category, nlink, dev, ino,
                file_count, dir_count, private_size, shared_size, clone_id, flags
             FROM entries WHERE scan_id = ?1 AND is_dir = 0",
        );
        let mut params_vec: Vec<rusqlite::types::Value> =
            vec![scan_id.to_string().into()];
        if let Some(ft) = query.file_type {
            sql.push_str(" AND file_type = ?");
            params_vec.push(ft.to_lowercase().into());
        }
        if let Some(needle) = query.search {
            // Escape LIKE metacharacters so a user searching for a literal
            // `%` or `_` in a path matches it, not everything.
            let escaped = needle
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_");
            sql.push_str(" AND path LIKE ? ESCAPE '\\'");
            params_vec.push(format!("%{escaped}%").into());
        }
        sql.push_str(match query.sort {
            // Path is the deterministic tiebreak everywhere (UNIQUE per scan).
            FileSort::Size => " ORDER BY disk_size DESC, path",
            FileSort::Name => " ORDER BY name, path",
            FileSort::Path => " ORDER BY path",
        });
        // Fetch one extra row to learn whether a further page exists.
        let probe = limit.saturating_add(1);
        sql.push_str(" LIMIT ? OFFSET ?");
        params_vec.push((probe as i64).into());
        params_vec.push((offset as i64).into());

        let mut stmt = self.conn.prepare(&sql)?;
        let mut files = stmt
            .query_map(rusqlite::params_from_iter(params_vec), row_to_entry)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let has_more = files.len() > limit;
        if has_more {
            files.truncate(limit);
        }
        let next_offset = has_more.then(|| offset.saturating_add(limit));
        Ok(FilePage { files, next_offset })
    }

    /// Per-type totals of a scan, computed at persistence time from the full
    /// walk (ADR-0005). Largest disk footprint first, ties broken by type
    /// name — the same order `format::totals_by_file_type` produces.
    pub fn file_type_totals(&self, scan_id: Uuid) -> Result<Vec<FileTypeTotal>> {
        self.get_scan(scan_id)?;
        let mut stmt = self.conn.prepare(
            "SELECT file_type, disk_size, file_count FROM scan_file_types
             WHERE scan_id = ?1 ORDER BY disk_size DESC, file_type",
        )?;
        let totals = stmt
            .query_map(params![scan_id.to_string()], |row| {
                Ok(FileTypeTotal {
                    file_type: row.get(0)?,
                    disk_size: row.get::<_, i64>(1)? as u64,
                    file_count: row.get::<_, i64>(2)? as u64,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(totals)
    }

    /// The hotspots summary persisted with a scan, decoded from its stored
    /// wire JSON. `None` == no summary (cancelled/failed scans, or rows
    /// persisted before schema v2). Unknown scan is NotFound.
    pub fn hotspots(&self, scan_id: Uuid) -> Result<Option<HotspotsSummary>> {
        self.get_scan(scan_id)?;
        let raw: Option<String> = self.conn.query_row(
            "SELECT hotspots FROM scans WHERE id = ?1",
            params![scan_id.to_string()],
            |r| r.get(0),
        )?;
        raw.map(|json| {
            serde_json::from_str(&json).map_err(|e| {
                // Stored by us, so a parse failure is corruption, not user
                // input — surfaces as a generic 500, never a 400.
                CoreError::Schema(format!("stored hotspots summary is unreadable: {e}"))
            })
        })
        .transpose()
    }

    // --- Reclaim plans (v7) ------------------------------------------------------

    /// Persist a plan as its wire JSON (the stored string IS the wire
    /// string, like scans.hotspots). Plan ids are fresh per build, so this
    /// is a plain insert.
    pub fn insert_plan(&self, plan: &ReclaimPlan) -> Result<()> {
        let json = serde_json::to_string(plan)
            .map_err(|e| CoreError::Schema(format!("cannot serialize reclaim plan: {e}")))?;
        self.conn.execute(
            "INSERT INTO plans (id, scan_id, root_path, created_at, plan)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                plan.plan_id.to_string(),
                plan.scan_id.to_string(),
                plan.root_path,
                wire_time::to_wire(&plan.created_at),
                json,
            ],
        )?;
        Ok(())
    }

    pub fn get_plan(&self, id: Uuid) -> Result<ReclaimPlan> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT plan FROM plans WHERE id = ?1",
                params![id.to_string()],
                |r| r.get(0),
            )
            .optional()?;
        let raw = raw.ok_or_else(|| CoreError::NotFound(format!("plan {id}")))?;
        serde_json::from_str(&raw)
            .map_err(|e| CoreError::Schema(format!("stored reclaim plan is unreadable: {e}")))
    }

    /// Keep-last-N retention for plans, newest by created_at. Returns how
    /// many were deleted.
    pub fn prune_plans_to_last(&self, keep: usize) -> Result<usize> {
        let deleted = self.conn.execute(
            "DELETE FROM plans WHERE id NOT IN (
                SELECT id FROM plans ORDER BY created_at DESC, id LIMIT ?1
            )",
            params![keep as i64],
        )?;
        Ok(deleted)
    }

    /// Delete a scan; entries go with it via ON DELETE CASCADE.
    pub fn delete_scan(&self, id: Uuid) -> Result<()> {
        let deleted = self.conn.execute(
            "DELETE FROM scans WHERE id = ?1",
            params![id.to_string()],
        )?;
        if deleted == 0 {
            return Err(CoreError::NotFound(format!("scan {id}")));
        }
        self.checkpoint_truncate()?;
        Ok(())
    }

    /// Keep-last-N retention: delete every scan except the `keep` newest.
    /// Returns how many scans were deleted (their entries cascade).
    pub fn prune_to_last(&self, keep: usize) -> Result<usize> {
        let deleted = self.conn.execute(
            "DELETE FROM scans WHERE id NOT IN (
                SELECT id FROM scans ORDER BY started_at DESC, id LIMIT ?1
            )",
            params![keep as i64],
        )?;
        // prune_retention ends here too, so one checkpoint folds both deletes.
        self.checkpoint_truncate()?;
        Ok(deleted)
    }

    /// Retention (v1.1 Phase 6, phantom-9tt): keep the newest
    /// `keep_per_root` scans OF EACH ROOT, then the newest `keep_total`
    /// overall. Per-root first, so a history of `~` is never evicted by
    /// scans of other roots (Phase 4 made history a feature); the total
    /// bounds the file's growth. Roots are compared with trailing slashes
    /// stripped, like `same_root`. Entries and type totals cascade.
    pub fn prune_retention(&self, keep_per_root: usize, keep_total: usize) -> Result<usize> {
        let (per_root, total) = self.prune_counts(keep_per_root, keep_total)?;
        Ok(per_root + total)
    }

    /// The whole retention rule (phantom-cnr.3): the count caps of
    /// [`Self::prune_retention`] first, THEN a byte budget. While the
    /// database's estimated live bytes exceed `budget_bytes`, the globally
    /// oldest completed scan whose root still holds more than
    /// [`BUDGET_FLOOR_PER_ROOT`] completed scans is evicted. Count is the
    /// wrong unit on its own — a 13.9 MB Safari probe and a 280 GB home
    /// scan cost one slot each, while their rows cost ~7 KB and ~420 MB —
    /// so the counts stay as secondary bounds and bytes become the binding
    /// constraint (Ted's 2026-09-09 per-root-then-total decision is not
    /// reversed). The floor holds for a root whose newest completed scan
    /// is [`BUDGET_FLOOR_MAX_AGE_DAYS`] old or younger; an older root has
    /// floor 0 (phantom-ccq) and the report names it (`floor_waived`).
    /// When every protected root is at or below the floor and the
    /// estimate is still over budget, the report says so
    /// (`floor_over_budget`) and nothing more is deleted. Running scans
    /// and scans with no entry rows are never budget victims — evicting
    /// them frees nothing.
    pub fn prune_retention_budget(
        &self,
        keep_per_root: usize,
        keep_total: usize,
        budget_bytes: u64,
    ) -> Result<PruneReport> {
        self.prune_retention_budget_at(keep_per_root, keep_total, budget_bytes, chrono::Utc::now())
    }

    /// [`Self::prune_retention_budget`] with the clock injected: root age
    /// is `now − started_at` of the root's newest completed scan.
    pub fn prune_retention_budget_at(
        &self,
        keep_per_root: usize,
        keep_total: usize,
        budget_bytes: u64,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<PruneReport> {
        let (by_per_root, by_total) = self.prune_counts(keep_per_root, keep_total)?;

        let footprints = self.scan_footprints()?;
        let estimated_before: u64 = footprints.iter().map(|f| f.estimated_bytes).sum();
        let mut estimated = estimated_before;
        let mut complete_per_root: std::collections::HashMap<&str, usize> =
            std::collections::HashMap::new();
        let mut newest_complete: std::collections::HashMap<&str, chrono::DateTime<chrono::Utc>> =
            std::collections::HashMap::new();
        for f in footprints.iter().filter(|f| f.status == ScanStatus::Complete) {
            *complete_per_root.entry(f.root.as_str()).or_default() += 1;
            let newest = newest_complete.entry(f.root.as_str()).or_insert(f.started_at);
            if f.started_at > *newest {
                *newest = f.started_at;
            }
        }
        // The floor protects a root whose newest completed scan started on
        // or after this instant; exactly 30 days old is still protected.
        let floor_cutoff = now - chrono::Duration::days(BUDGET_FLOOR_MAX_AGE_DAYS);

        let mut victims: Vec<&ScanFootprint> = Vec::new();
        let mut floor_waived: Vec<FloorWaived> = Vec::new();
        // Oldest first — `scan_footprints` orders them that way.
        for f in &footprints {
            if estimated <= budget_bytes {
                break;
            }
            if f.status != ScanStatus::Complete || f.rows == 0 {
                continue;
            }
            let held = complete_per_root.get_mut(f.root.as_str()).expect("counted above");
            if *held <= BUDGET_FLOOR_PER_ROOT {
                let newest = newest_complete[f.root.as_str()];
                if newest >= floor_cutoff {
                    continue;
                }
                if !floor_waived.iter().any(|w| w.root == f.root) {
                    floor_waived.push(FloorWaived {
                        root: f.root.clone(),
                        newest_started_at: newest,
                        age: now - newest,
                    });
                }
            }
            *held -= 1;
            estimated = estimated.saturating_sub(f.estimated_bytes);
            victims.push(f);
        }
        for f in &victims {
            self.conn
                .execute("DELETE FROM scans WHERE id = ?1", params![f.id.to_string()])?;
        }
        if !victims.is_empty() {
            self.checkpoint_truncate()?;
        }

        let floor_over_budget = if estimated > budget_bytes {
            let roots = complete_per_root.values().filter(|n| **n > 0).count();
            let scans = complete_per_root.values().sum();
            Some(FloorOverBudget { roots, scans, estimated_bytes: estimated })
        } else {
            None
        };
        Ok(PruneReport {
            by_per_root,
            by_total,
            by_budget: victims.len(),
            estimated_bytes_before: estimated_before,
            estimated_bytes_after: estimated,
            floor_over_budget,
            floor_waived,
        })
    }

    /// The two count caps, returned separately for the report.
    fn prune_counts(&self, keep_per_root: usize, keep_total: usize) -> Result<(usize, usize)> {
        let per_root = self.conn.execute(
            "DELETE FROM scans WHERE id IN (
                SELECT id FROM (
                    SELECT id, ROW_NUMBER() OVER (
                        PARTITION BY rtrim(root_path, '/')
                        ORDER BY started_at DESC, id
                    ) AS rn FROM scans
                ) WHERE rn > ?1
            )",
            params![keep_per_root as i64],
        )?;
        let total = self.prune_to_last(keep_total)?;
        Ok((per_root, total))
    }

    /// What each scan costs on disk, estimated without a schema change:
    /// `rows(scan) × live bytes / rows(all)`, where live bytes is
    /// `(page_count − freelist_count) × page_size` and rows come from one
    /// `GROUP BY scan_id` over the entries table (a covering read of its
    /// `UNIQUE (scan_id, path)` index). `scans.file_count` is measured
    /// BEFORE the ADR-0005 persistence filter and says nothing about what
    /// was stored, so it is deliberately not used. Oldest scan first
    /// (`started_at` ascending, ties broken so the newest of a tie is
    /// last), roots compared with trailing slashes stripped.
    pub fn scan_footprints(&self) -> Result<Vec<ScanFootprint>> {
        let page_size: u64 = self.conn.query_row("PRAGMA page_size", [], |r| r.get::<_, i64>(0))? as u64;
        let page_count: u64 = self.conn.query_row("PRAGMA page_count", [], |r| r.get::<_, i64>(0))? as u64;
        let freelist: u64 = self.conn.query_row("PRAGMA freelist_count", [], |r| r.get::<_, i64>(0))? as u64;
        let live_bytes = page_count.saturating_sub(freelist).saturating_mul(page_size);

        let mut stmt = self.conn.prepare(
            "SELECT s.id, rtrim(s.root_path, '/'), s.status, COALESCE(e.n, 0), s.started_at
             FROM scans s
             LEFT JOIN (SELECT scan_id, COUNT(*) AS n FROM entries GROUP BY scan_id) e
               ON e.scan_id = s.id
             ORDER BY s.started_at ASC, s.id DESC",
        )?;
        let raw: Vec<(String, String, String, i64, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?
            .collect::<std::result::Result<_, _>>()?;
        let rows_all: u64 = raw.iter().map(|(_, _, _, n, _)| *n as u64).sum();

        raw.into_iter()
            .map(|(id, root, status, n, started)| {
                let rows = n as u64;
                // u128: rows × bytes can pass u64 on a large file.
                let estimated_bytes = if rows_all == 0 {
                    0
                } else {
                    ((rows as u128 * live_bytes as u128) / rows_all as u128) as u64
                };
                Ok(ScanFootprint {
                    id: id.parse().map_err(|e: uuid::Error| parse_err(e.to_string()))?,
                    root,
                    status: status.parse::<ScanStatus>().map_err(|e| parse_err(e.to_string()))?,
                    started_at: wire_time::from_wire(&started).map_err(|e| parse_err(e.to_string()))?,
                    rows,
                    estimated_bytes,
                })
            })
            .collect::<std::result::Result<Vec<_>, rusqlite::Error>>()
            .map_err(Into::into)
    }
}

fn parse_err(e: String) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, e.into())
}

fn row_to_scan(row: &Row<'_>) -> rusqlite::Result<Scan> {
    let id: String = row.get(0)?;
    let status: String = row.get(2)?;
    let started_at: String = row.get(3)?;
    let finished_at: Option<String> = row.get(4)?;
    Ok(Scan {
        id: id.parse().map_err(|e: uuid::Error| parse_err(e.to_string()))?,
        root_path: row.get(1)?,
        status: status
            .parse::<ScanStatus>()
            .map_err(|e| parse_err(e.to_string()))?,
        started_at: wire_time::from_wire(&started_at).map_err(|e| parse_err(e.to_string()))?,
        finished_at: finished_at
            .map(|s| wire_time::from_wire(&s).map_err(|e| parse_err(e.to_string())))
            .transpose()?,
        total_disk_size: row.get::<_, i64>(5)? as u64,
        total_logical_size: row.get::<_, i64>(6)? as u64,
        file_count: row.get::<_, i64>(7)? as u64,
        dir_count: row.get::<_, i64>(8)? as u64,
        error_count: row.get::<_, i64>(9)? as u64,
        // NULL == not recorded (pre-v4 row); DB string == wire string.
        unreadable_paths: row
            .get::<_, Option<String>>(10)?
            .map(|s| serde_json::from_str(&s).map_err(|e| parse_err(e.to_string())))
            .transpose()?,
        // NULL == not recorded (pre-v5 row).
        total_private_size: row.get::<_, Option<i64>>(11)?.map(|v| v as u64),
        total_shared_size: row.get::<_, Option<i64>>(12)?.map(|v| v as u64),
        failure_reason: row.get(13)?,
    })
}

fn row_to_entry(row: &Row<'_>) -> rusqlite::Result<ScanEntry> {
    let modified_at: Option<String> = row.get(6)?;
    Ok(ScanEntry {
        path: row.get(0)?,
        parent_path: row.get(1)?,
        name: row.get(2)?,
        is_dir: row.get(3)?,
        disk_size: row.get::<_, i64>(4)? as u64,
        logical_size: row.get::<_, i64>(5)? as u64,
        modified_at: modified_at
            .map(|s| wire_time::from_wire(&s).map_err(|e| parse_err(e.to_string())))
            .transpose()?,
        file_type: row.get(7)?,
        category: row.get(8)?,
        nlink: row.get::<_, i64>(9)? as u64,
        dev: row.get::<_, i64>(10)? as u64,
        ino: row.get::<_, i64>(11)? as u64,
        file_count: row.get::<_, Option<i64>>(12)?.map(|v| v as u64),
        dir_count: row.get::<_, Option<i64>>(13)?.map(|v| v as u64),
        // v5 columns: NULL == not recorded on pre-v5 rows. clone_id is
        // also NULL for every file that is not a pure clone.
        private_size: row.get::<_, Option<i64>>(14)?.map(|v| v as u64),
        shared_size: row.get::<_, Option<i64>>(15)?.map(|v| v as u64),
        clone_id: row.get::<_, Option<i64>>(16)?.map(|v| v as u64),
        flags: row
            .get::<_, Option<i64>>(17)?
            .map(|v| EntryFlags::from_bits(v as u32)),
    })
}

#[cfg(test)]
mod scan_tests {
    use super::*;

    fn store() -> ScanStore {
        ScanStore::open_in_memory().unwrap()
    }

    /// A complete scan with a fixed, parse-derived timestamp so round-trip
    /// equality never depends on the platform clock's resolution.
    fn scan_at(started: &str, root: &str) -> Scan {
        let started_at = wire_time::from_wire(started).unwrap();
        Scan {
            id: Uuid::new_v4(),
            root_path: root.to_string(),
            status: ScanStatus::Complete,
            started_at,
            finished_at: Some(started_at + chrono::Duration::seconds(65)),
            total_disk_size: 4096,
            total_logical_size: 3000,
            file_count: 2,
            dir_count: 1,
            error_count: 0,
            unreadable_paths: Some(Vec::new()),
            total_private_size: Some(0),
            total_shared_size: Some(0),
            failure_reason: None,
        }
    }

    fn entry(path: &str, parent: Option<&str>, disk: u64, is_dir: bool) -> ScanEntry {
        ScanEntry {
            path: path.to_string(),
            parent_path: parent.map(|s| s.to_string()),
            name: Path::new(path)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_string(),
            is_dir,
            disk_size: disk,
            logical_size: disk.saturating_sub(100),
            modified_at: if is_dir {
                None
            } else {
                Some(wire_time::from_wire("2026-03-17T14:30:00.123456Z").unwrap())
            },
            file_type: if is_dir { None } else { Some("txt".into()) },
            category: None,
            nlink: 1,
            dev: 42,
            ino: 7,
            file_count: None,
            dir_count: None,
            private_size: None,
            shared_size: None,
            clone_id: None,
            flags: None,
        }
    }

    fn tree(root: &str) -> Vec<ScanEntry> {
        vec![
            entry(root, None, 0, true),
            entry(&format!("{root}/a.txt"), Some(root), 2048, false),
            entry(&format!("{root}/b.txt"), Some(root), 2048, false),
        ]
    }

    #[test]
    fn insert_then_get_round_trips_scan_and_entries() {
        let s = store();
        let scan = scan_at("2026-03-17T14:30:00.123456Z", "/haunt");
        let entries = tree("/haunt");
        s.insert_scan(&scan, &entries, &[], None).unwrap();

        assert_eq!(s.get_scan(scan.id).unwrap(), scan);
        assert_eq!(s.entries(scan.id).unwrap(), entries);
    }

    // Mutation-proof: swap the file_count/dir_count column order in either
    // the INSERT params or row_to_entry and the asymmetric (42, 7) pair
    // flips; drop the columns and Some becomes None.
    #[test]
    fn entry_counts_round_trip_and_file_rows_stay_null() {
        let s = store();
        let scan = scan_at("2026-03-17T14:30:00.123456Z", "/haunt");
        let mut root = entry("/haunt", None, 0, true);
        root.file_count = Some(42);
        root.dir_count = Some(7);
        let file = entry("/haunt/a.txt", Some("/haunt"), 2048, false);
        s.insert_scan(&scan, &[root.clone(), file.clone()], &[], None)
            .unwrap();

        let back = s.entries(scan.id).unwrap();
        assert_eq!(back, vec![root, file], "counts survive the round-trip exactly");
        assert_eq!((back[1].file_count, back[1].dir_count), (None, None));
    }

    #[test]
    fn get_unknown_scan_is_not_found() {
        let err = store().get_scan(Uuid::new_v4()).unwrap_err();
        assert!(matches!(err, CoreError::NotFound(_)));
    }

    #[test]
    fn entries_of_unknown_scan_is_not_found() {
        let err = store().entries(Uuid::new_v4()).unwrap_err();
        assert!(matches!(err, CoreError::NotFound(_)));
    }

    // Mutation target: revert the ORDER BY in list_scans and this fails.
    /// v6 (phantom-aoa): the running row goes in at start, the terminal
    /// upsert replaces it and attaches the results — one row, never two.
    #[test]
    fn running_row_is_upserted_by_the_terminal_insert() {
        let store = store();
        let mut scan = Scan::new("/tmp/x");
        store.insert_scan(&scan, &[], &[], None).unwrap();
        let running = store.get_scan(scan.id).unwrap();
        assert_eq!(running.status, ScanStatus::Running);
        assert_eq!(running.failure_reason, None);

        scan.status = ScanStatus::Complete;
        scan.finished_at = Some(chrono::Utc::now());
        scan.total_disk_size = 4096;
        scan.file_count = 1;
        let entries = vec![entry("/tmp/x/a.bin", Some("/tmp/x"), 4096, false)];
        store.insert_scan(&scan, &entries, &[], None).unwrap();
        let done = store.get_scan(scan.id).unwrap();
        assert_eq!(done.status, ScanStatus::Complete);
        assert_eq!(done.total_disk_size, 4096);
        assert_eq!(store.list_scans().unwrap().len(), 1, "an upsert, not a second row");
        assert_eq!(store.entries(scan.id).unwrap().len(), 1);
    }

    #[test]
    fn interrupted_marking_touches_only_running_rows() {
        let store = store();
        let running = Scan::new("/tmp/running");
        store.insert_scan(&running, &[], &[], None).unwrap();
        let mut done = Scan::new("/tmp/done");
        done.status = ScanStatus::Complete;
        done.finished_at = Some(chrono::Utc::now());
        store.insert_scan(&done, &[], &[], None).unwrap();
        let mut failed = Scan::new("/tmp/failed");
        failed.status = ScanStatus::Failed;
        failed.finished_at = Some(chrono::Utc::now());
        failed.failure_reason = Some("walk error".into());
        store.insert_scan(&failed, &[], &[], None).unwrap();

        let marked = store.mark_running_as_interrupted("interrupted: test").unwrap();
        assert_eq!(marked, 1);
        let r = store.get_scan(running.id).unwrap();
        assert_eq!(r.status, ScanStatus::Failed);
        assert!(r.finished_at.is_some(), "interrupted rows become terminal");
        assert_eq!(r.failure_reason.as_deref(), Some("interrupted: test"));
        assert_eq!(store.get_scan(done.id).unwrap().status, ScanStatus::Complete);
        assert_eq!(
            store.get_scan(failed.id).unwrap().failure_reason.as_deref(),
            Some("walk error"),
            "an already-failed row keeps its own reason"
        );
        assert_eq!(store.mark_running_as_interrupted("again").unwrap(), 0, "idempotent");
    }

    #[test]
    fn plans_round_trip_prune_and_miss_cleanly() {
        let s = store();
        let raw = include_str!("../../../tests/fixtures/reclaim-plan.json");
        let mut plan: ReclaimPlan = serde_json::from_str(raw).unwrap();
        s.insert_plan(&plan).unwrap();
        assert_eq!(s.get_plan(plan.plan_id).unwrap(), plan, "the stored string is the wire string");
        assert!(matches!(s.get_plan(Uuid::new_v4()), Err(CoreError::NotFound(_))));
        // A plan outlives its scan (no FK): deleting an unrelated/absent scan
        // id is irrelevant here, but the row must not depend on scans at all.
        assert!(s.get_scan(plan.scan_id).is_err(), "precondition: the plan's scan is not in this store");
        assert!(s.get_plan(plan.plan_id).is_ok());

        // Retention keeps the newest.
        plan.plan_id = Uuid::new_v4();
        plan.created_at += chrono::Duration::seconds(60);
        s.insert_plan(&plan).unwrap();
        assert_eq!(s.prune_plans_to_last(1).unwrap(), 1);
        assert!(s.get_plan(plan.plan_id).is_ok(), "the newer plan survives");
    }

    #[test]
    fn list_scans_is_newest_first() {
        let s = store();
        let older = scan_at("2026-03-17T14:30:00.000000Z", "/old");
        let newer = scan_at("2026-03-18T09:00:00.000000Z", "/new");
        s.insert_scan(&older, &[], &[], None).unwrap();
        s.insert_scan(&newer, &[], &[], None).unwrap();
        let ids: Vec<Uuid> = s.list_scans().unwrap().into_iter().map(|x| x.id).collect();
        assert_eq!(ids, vec![newer.id, older.id]);
    }

    // Mutation target: drop the ORDER BY path in entries() and this fails
    // (insertion order below is deliberately not path order).
    #[test]
    fn entries_are_path_ordered() {
        let s = store();
        let scan = scan_at("2026-03-17T14:30:00.000000Z", "/haunt");
        let unordered = vec![
            entry("/haunt/z.txt", Some("/haunt"), 512, false),
            entry("/haunt", None, 0, true),
            entry("/haunt/a.txt", Some("/haunt"), 512, false),
        ];
        s.insert_scan(&scan, &unordered, &[], None).unwrap();
        let paths: Vec<String> = s
            .entries(scan.id)
            .unwrap()
            .into_iter()
            .map(|e| e.path)
            .collect();
        assert_eq!(paths, vec!["/haunt", "/haunt/a.txt", "/haunt/z.txt"]);
    }

    #[test]
    fn children_of_returns_only_direct_children() {
        let s = store();
        let scan = scan_at("2026-03-17T14:30:00.000000Z", "/haunt");
        let entries = vec![
            entry("/haunt", None, 0, true),
            entry("/haunt/sub", Some("/haunt"), 0, true),
            entry("/haunt/a.txt", Some("/haunt"), 512, false),
            entry("/haunt/sub/deep.txt", Some("/haunt/sub"), 512, false),
        ];
        s.insert_scan(&scan, &entries, &[], None).unwrap();

        let kids: Vec<String> = s
            .children_of(scan.id, Some("/haunt"))
            .unwrap()
            .into_iter()
            .map(|e| e.path)
            .collect();
        assert_eq!(kids, vec!["/haunt/a.txt", "/haunt/sub"]);

        // None == the scan root.
        let roots: Vec<String> = s
            .children_of(scan.id, None)
            .unwrap()
            .into_iter()
            .map(|e| e.path)
            .collect();
        assert_eq!(roots, vec!["/haunt"]);
    }

    #[test]
    fn children_of_unknown_scan_is_not_found() {
        let err = store().children_of(Uuid::new_v4(), None).unwrap_err();
        assert!(matches!(err, CoreError::NotFound(_)));
    }

    // dir_sizes backs the diff (phantom-081). Mutation-proof: flip the
    // `is_dir = 1` filter to `= 0` and the file rows appear / dirs vanish;
    // drop the ORDER BY and the assertion on order fails.
    #[test]
    fn dir_sizes_returns_only_directories_path_ordered_with_sizes() {
        let s = store();
        let scan = scan_at("2026-03-17T14:30:00.000000Z", "/haunt");
        let entries = vec![
            entry("/haunt", None, 4096, true),
            entry("/haunt/sub", Some("/haunt"), 1024, true),
            entry("/haunt/big.bin", Some("/haunt"), 2_000_000, false),
            entry("/haunt/sub/deep.txt", Some("/haunt/sub"), 512, false),
        ];
        s.insert_scan(&scan, &entries, &[], None).unwrap();

        let dirs = s.dir_sizes(scan.id).unwrap();
        assert_eq!(
            dirs,
            vec![
                ("/haunt".to_string(), 4096),
                ("/haunt/sub".to_string(), 1024),
            ],
            "only directory rows, path-ordered, carrying their persisted disk_size"
        );
    }

    #[test]
    fn dir_sizes_unknown_scan_is_not_found() {
        let err = store().dir_sizes(Uuid::new_v4()).unwrap_err();
        assert!(matches!(err, CoreError::NotFound(_)));
    }

    // Mutation target: remove the foreign_keys pragma in init() and this
    // fails — the cascade silently stops firing and the entries survive.
    #[test]
    fn delete_scan_cascades_to_entries() {
        let s = store();
        let scan = scan_at("2026-03-17T14:30:00.000000Z", "/haunt");
        s.insert_scan(&scan, &tree("/haunt"), &[], None).unwrap();

        s.delete_scan(scan.id).unwrap();

        assert!(matches!(s.get_scan(scan.id), Err(CoreError::NotFound(_))));
        let orphans: i64 = s
            .conn
            .query_row("SELECT COUNT(*) FROM entries", [], |r| r.get(0))
            .unwrap();
        assert_eq!(orphans, 0, "cascade must remove the scan's entries");
    }

    #[test]
    fn delete_unknown_scan_is_not_found() {
        let err = store().delete_scan(Uuid::new_v4()).unwrap_err();
        assert!(matches!(err, CoreError::NotFound(_)));
    }

    #[test]
    fn prune_keeps_the_newest_n() {
        let s = store();
        let oldest = scan_at("2026-03-15T00:00:00.000000Z", "/one");
        let middle = scan_at("2026-03-16T00:00:00.000000Z", "/two");
        let newest = scan_at("2026-03-17T00:00:00.000000Z", "/three");
        s.insert_scan(&oldest, &tree("/one"), &[], None).unwrap();
        s.insert_scan(&middle, &[], &[], None).unwrap();
        s.insert_scan(&newest, &[], &[], None).unwrap();

        assert_eq!(s.prune_to_last(2).unwrap(), 1);
        let ids: Vec<Uuid> = s.list_scans().unwrap().into_iter().map(|x| x.id).collect();
        assert_eq!(ids, vec![newest.id, middle.id]);
        // The pruned scan's entries cascade too.
        let orphans: i64 = s
            .conn
            .query_row("SELECT COUNT(*) FROM entries", [], |r| r.get(0))
            .unwrap();
        assert_eq!(orphans, 0);
    }

    /// Per-root retention: a root's oldest scan goes when IT has too many,
    /// never because another root scanned more; the total cap then bounds
    /// everything. Trailing slashes do not make a second root.
    #[test]
    fn prune_retention_is_per_root_then_total() {
        let s = store();
        let a1 = scan_at("2026-03-10T00:00:00.000000Z", "/a");
        let a2 = scan_at("2026-03-11T00:00:00.000000Z", "/a/");
        let a3 = scan_at("2026-03-12T00:00:00.000000Z", "/a");
        let b1 = scan_at("2026-03-01T00:00:00.000000Z", "/b"); // oldest overall
        let b2 = scan_at("2026-03-13T00:00:00.000000Z", "/b");
        for x in [&a1, &a2, &a3, &b1, &b2] {
            s.insert_scan(x, &[], &[], None).unwrap();
        }
        // Two per root: /a loses a1 (its oldest); /b keeps both even though
        // b1 is the oldest scan in the database.
        assert_eq!(s.prune_retention(2, 100).unwrap(), 1);
        let ids: Vec<Uuid> = s.list_scans().unwrap().into_iter().map(|x| x.id).collect();
        assert_eq!(ids, vec![b2.id, a3.id, a2.id, b1.id]);
        // Then the total cap: 3 overall drops the globally oldest (b1).
        assert_eq!(s.prune_retention(2, 3).unwrap(), 1);
        let ids: Vec<Uuid> = s.list_scans().unwrap().into_iter().map(|x| x.id).collect();
        assert_eq!(ids, vec![b2.id, a3.id, a2.id]);
        assert_eq!(s.prune_retention(2, 3).unwrap(), 0, "idempotent");
    }

    // --- Byte-budget retention (phantom-cnr.3) ------------------------------

    /// A complete scan of `root` at `started` carrying `rows` persisted
    /// entry rows (dirs of 9 files + the dir row = 10 rows per dir).
    fn scan_with_rows(started: &str, root: &str, dirs: usize) -> (Scan, Vec<ScanEntry>) {
        let mut scan = scan_at(started, root);
        let entries = bulk_entries(root, dirs, 9);
        scan.file_count = (dirs * 9) as u64;
        scan.dir_count = dirs as u64;
        (scan, entries)
    }

    fn ids(s: &ScanStore) -> Vec<Uuid> {
        s.list_scans().unwrap().into_iter().map(|x| x.id).collect()
    }

    fn footprint(s: &ScanStore, id: Uuid) -> ScanFootprint {
        s.scan_footprints().unwrap().into_iter().find(|f| f.id == id).unwrap()
    }

    fn at(wire: &str) -> chrono::DateTime<chrono::Utc> {
        wire_time::from_wire(wire).unwrap()
    }

    /// The budget tests above date their scans in March 2026; prune them
    /// under a clock a few days later so every root is fresh and the floor
    /// (phantom-cnr.3) is what is under test, not its age clause
    /// (phantom-ccq), which has its own tests below.
    fn prune(s: &ScanStore, per_root: usize, total: usize, budget: u64) -> Result<PruneReport> {
        s.prune_retention_budget_at(per_root, total, budget, at("2026-03-10T00:00:00.000000Z"))
    }

    /// The age clause (phantom-ccq, Ted 2026-09-22): a root whose newest
    /// completed scan is older than BUDGET_FLOOR_MAX_AGE_DAYS has floor 0,
    /// so its last two go like any other scan — oldest first, only while
    /// the budget is exceeded — while a fresh root's pair survives. The
    /// fresh root's OLDER scan is itself older than the cutoff, so the
    /// age must be read off the NEWEST scan. Mutation targets: drop the
    /// age clause and nothing is evicted; invert the comparison and the
    /// fresh root empties while the stale one stands; measure the oldest
    /// scan instead of the newest and the fresh root empties too.
    #[test]
    fn a_stale_roots_pair_is_evicted_while_a_fresh_roots_pair_survives() {
        let s = store();
        let (old1, e) = scan_with_rows("2026-01-01T00:00:00.000000Z", "/old", 10);
        s.insert_scan(&old1, &e, &[], None).unwrap();
        let (new1, e) = scan_with_rows("2026-01-10T00:00:00.000000Z", "/new", 10);
        s.insert_scan(&new1, &e, &[], None).unwrap();
        let (old2, e) = scan_with_rows("2026-01-15T00:00:00.000000Z", "/old/", 10);
        s.insert_scan(&old2, &e, &[], None).unwrap();
        let (new2, e) = scan_with_rows("2026-03-20T00:00:00.000000Z", "/new", 10);
        s.insert_scan(&new2, &e, &[], None).unwrap();

        // /old's newest is 75 days old; /new's newest is 11 days old (its
        // oldest is 80). A one-byte budget: only the floor decides.
        let now = at("2026-03-31T00:00:00.000000Z");
        let r = s.prune_retention_budget_at(25, 100, 1, now).unwrap();
        assert_eq!((r.by_per_root, r.by_total, r.by_budget), (0, 0, 2), "{r:?}");
        assert_eq!(ids(&s), vec![new2.id, new1.id], "the stale root's whole history went; the fresh pair stands");
        assert!(s.entries(old1.id).is_err() && s.entries(old2.id).is_err(), "entries cascaded");
        assert_eq!(
            r.floor_waived,
            vec![FloorWaived {
                root: "/old".into(),
                newest_started_at: old2.started_at,
                age: chrono::Duration::days(75),
            }],
            "the waiver names the root and its age, once"
        );
        // Still over budget, and only the protected root is left standing.
        assert_eq!(r.floor_over_budget.map(|f| (f.roots, f.scans)), Some((1, 2)));
        // Idempotent: the fresh pair is never touched.
        let again = s.prune_retention_budget_at(25, 100, 1, now).unwrap();
        assert_eq!(again.deleted(), 0);
        assert!(again.floor_waived.is_empty(), "no waiver is reported when nothing was evicted under one");
    }

    /// The boundary: a root whose newest completed scan is exactly 30 days
    /// old is still protected; one second later it is not. Also pins that
    /// a waiver is only reported when the budget actually needed it — under
    /// budget a stale root keeps its scans and the report is quiet.
    #[test]
    fn the_floor_holds_at_exactly_thirty_days_and_lapses_a_second_later() {
        let s = store();
        let (b1, e) = scan_with_rows("2026-01-01T00:00:00.000000Z", "/b", 10);
        s.insert_scan(&b1, &e, &[], None).unwrap();
        let (b2, e) = scan_with_rows("2026-01-31T00:00:00.000000Z", "/b", 10);
        s.insert_scan(&b2, &e, &[], None).unwrap();

        // 2026-01-31 + 30 days = 2026-03-02 (28-day February).
        let boundary = at("2026-03-02T00:00:00.000000Z");
        let r = s.prune_retention_budget_at(25, 100, 1, boundary).unwrap();
        assert_eq!(r.deleted(), 0, "exactly {BUDGET_FLOOR_MAX_AGE_DAYS} days old is still protected: {r:?}");
        assert!(r.floor_waived.is_empty());
        assert_eq!(ids(&s), vec![b2.id, b1.id]);

        // Under budget a stale root is left alone, and nothing is reported.
        let stale = boundary + chrono::Duration::seconds(1);
        let r = s.prune_retention_budget_at(25, 100, u64::MAX, stale).unwrap();
        assert_eq!(r.deleted(), 0);
        assert!(r.floor_waived.is_empty(), "no eviction, no waiver to report");

        let r = s.prune_retention_budget_at(25, 100, 1, stale).unwrap();
        assert_eq!(r.by_budget, 2, "one second past the boundary the floor is 0: {r:?}");
        assert!(ids(&s).is_empty());
        assert_eq!(r.floor_waived.len(), 1);
        assert_eq!(r.floor_waived[0].root, "/b");
        assert_eq!(r.floor_waived[0].age, chrono::Duration::days(30) + chrono::Duration::seconds(1));
        assert!(r.floor_over_budget.is_none(), "an empty store is under any budget");
    }

    /// The budget evicts the GLOBALLY oldest scan whose root can spare one,
    /// and stops — still over budget — the moment every root is at the
    /// floor. Mutation targets: drop the floor check and b1/a2 go too; walk
    /// the footprints newest-first and a3 goes instead of a1.
    #[test]
    fn budget_evicts_the_globally_oldest_above_the_floor_and_never_below_it() {
        let s = store();
        let (a1, e) = scan_with_rows("2026-03-01T00:00:00.000000Z", "/a", 10);
        s.insert_scan(&a1, &e, &[], None).unwrap();
        let (b1, e) = scan_with_rows("2026-03-02T00:00:00.000000Z", "/b", 10);
        s.insert_scan(&b1, &e, &[], None).unwrap();
        let (a2, e) = scan_with_rows("2026-03-03T00:00:00.000000Z", "/a/", 10);
        s.insert_scan(&a2, &e, &[], None).unwrap();
        let (b2, e) = scan_with_rows("2026-03-04T00:00:00.000000Z", "/b", 10);
        s.insert_scan(&b2, &e, &[], None).unwrap();
        let (a3, e) = scan_with_rows("2026-03-05T00:00:00.000000Z", "/a", 10);
        s.insert_scan(&a3, &e, &[], None).unwrap();

        // A one-byte budget: everything is over it, only the floor decides.
        let r = prune(&s, 25, 100, 1).unwrap();
        assert_eq!((r.by_per_root, r.by_total, r.by_budget), (0, 0, 1));
        assert_eq!(ids(&s), vec![a3.id, b2.id, a2.id, b1.id], "a1 — the oldest — is the one victim");
        assert_eq!(r.deleted(), 1);
        assert!(r.estimated_bytes_after < r.estimated_bytes_before);
        let floor = r.floor_over_budget.expect("still over budget, so the floor is reported");
        assert_eq!((floor.roots, floor.scans), (2, 4), "two roots holding two scans each");
        assert_eq!(floor.estimated_bytes, r.estimated_bytes_after);
        // Entries cascaded with the victim.
        assert!(s.entries(a1.id).is_err());
        // Idempotent: nothing more can go.
        let again = prune(&s, 25, 100, 1).unwrap();
        assert_eq!(again.deleted(), 0);
        assert_eq!(ids(&s).len(), 4);
    }

    /// The floor, on its own: a root's last two scans survive a budget that
    /// cannot possibly hold them — diff, verify and growth need the pair.
    /// Mutation target: drop the floor and the store empties.
    #[test]
    fn a_roots_last_two_scans_survive_a_tiny_budget() {
        let s = store();
        let (h1, e) = scan_with_rows("2026-03-01T00:00:00.000000Z", "/home", 40);
        s.insert_scan(&h1, &e, &[], None).unwrap();
        let (h2, e) = scan_with_rows("2026-03-02T00:00:00.000000Z", "/home", 40);
        s.insert_scan(&h2, &e, &[], None).unwrap();

        let r = prune(&s, 25, 100, 1).unwrap();
        assert_eq!(r.deleted(), 0, "at the floor already: nothing is deleted");
        assert_eq!(ids(&s), vec![h2.id, h1.id]);
        assert_eq!(r.estimated_bytes_after, r.estimated_bytes_before);
        assert!(r.floor_over_budget.is_some(), "and the shortfall is reported, not hidden");

        // A third scan takes the root above the floor; exactly the OLDEST goes.
        let (h3, e) = scan_with_rows("2026-03-03T00:00:00.000000Z", "/home", 40);
        s.insert_scan(&h3, &e, &[], None).unwrap();
        let r = prune(&s, 25, 100, 1).unwrap();
        assert_eq!(r.by_budget, 1);
        assert_eq!(ids(&s), vec![h3.id, h2.id]);
    }

    /// A scan is charged for the rows it STORES, not for `scans.file_count`
    /// (which is measured before the ADR-0005 filter): a probe of a million
    /// tiny files that persisted three rows is nearly free, a scan that
    /// persisted 200 rows costs 200 rows' worth. Mutation target: estimate
    /// from file_count and x1 looks like the whole database — evicting it
    /// "satisfies" the budget and x2 wrongly survives.
    #[test]
    fn a_scan_of_many_small_files_costs_what_its_rows_cost() {
        let s = store();
        let mut x1 = scan_at("2026-03-01T00:00:00.000000Z", "/x");
        x1.file_count = 1_000_000;
        s.insert_scan(&x1, &tree("/x"), &[], None).unwrap(); // 3 rows
        let (mut x2, e) = scan_with_rows("2026-03-02T00:00:00.000000Z", "/x", 20); // 200 rows
        x2.file_count = 5;
        s.insert_scan(&x2, &e, &[], None).unwrap();
        let (x3, e) = scan_with_rows("2026-03-03T00:00:00.000000Z", "/x", 20);
        s.insert_scan(&x3, &e, &[], None).unwrap();
        let (x4, e) = scan_with_rows("2026-03-04T00:00:00.000000Z", "/x", 20);
        s.insert_scan(&x4, &e, &[], None).unwrap();

        let f1 = footprint(&s, x1.id);
        let f2 = footprint(&s, x2.id);
        assert_eq!((f1.rows, f2.rows), (3, 200));
        assert!(
            f1.estimated_bytes * 50 < f2.estimated_bytes,
            "three rows cost far less than two hundred, whatever file_count says ({} vs {})",
            f1.estimated_bytes,
            f2.estimated_bytes
        );
        let all: Vec<ScanFootprint> = s.scan_footprints().unwrap();
        assert_eq!(all.iter().map(|f| f.id).collect::<Vec<_>>(), vec![x1.id, x2.id, x3.id, x4.id], "oldest first");
        let live: u64 = all.iter().map(|f| f.estimated_bytes).sum();

        // Ninety percent of today's bytes: the near-free x1 cannot get there
        // alone, so x2 must go too; x3 and x4 are then the floor.
        let r = prune(&s, 25, 100, live * 9 / 10).unwrap();
        assert_eq!(r.by_budget, 2, "{r:?}");
        assert_eq!(ids(&s), vec![x4.id, x3.id]);
        assert!(r.estimated_bytes_after <= live * 9 / 10);
        assert!(r.floor_over_budget.is_none(), "the budget held; nothing to report");
    }

    /// Running rows (a scan mid-walk has no entries yet) and scans that hold
    /// no rows are never budget victims — evicting them frees nothing — and
    /// only COMPLETED scans count toward the floor, so a failed attempt
    /// cannot make a root's real history look spare.
    #[test]
    fn budget_skips_running_failed_and_empty_scans() {
        let s = store();
        let mut running = scan_at("2026-02-01T00:00:00.000000Z", "/r");
        running.status = ScanStatus::Running;
        running.finished_at = None;
        s.insert_scan(&running, &[], &[], None).unwrap();
        let mut failed = scan_at("2026-02-02T00:00:00.000000Z", "/r");
        failed.status = ScanStatus::Failed;
        s.insert_scan(&failed, &[], &[], None).unwrap();
        let (c1, e) = scan_with_rows("2026-03-01T00:00:00.000000Z", "/r", 10);
        s.insert_scan(&c1, &e, &[], None).unwrap();
        let (c2, e) = scan_with_rows("2026-03-02T00:00:00.000000Z", "/r", 10);
        s.insert_scan(&c2, &e, &[], None).unwrap();

        // Two completed scans: the floor holds even though the root has
        // four rows in the scans table.
        let r = prune(&s, 25, 100, 1).unwrap();
        assert_eq!(r.deleted(), 0, "{r:?}");
        assert_eq!(r.floor_over_budget.map(|f| (f.roots, f.scans)), Some((1, 2)));

        let (c3, e) = scan_with_rows("2026-03-03T00:00:00.000000Z", "/r", 10);
        s.insert_scan(&c3, &e, &[], None).unwrap();
        let r = prune(&s, 25, 100, 1).unwrap();
        assert_eq!(r.by_budget, 1);
        assert_eq!(
            ids(&s),
            vec![c3.id, c2.id, failed.id, running.id],
            "c1 — the oldest COMPLETED scan with rows — went; the running and failed rows stand"
        );
    }

    /// Under budget, the budget pass is a no-op and the count caps still
    /// bound the collection on their own (they stay as secondary bounds).
    #[test]
    fn under_budget_the_count_caps_still_apply() {
        let s = store();
        for (i, day) in ["01", "02", "03"].iter().enumerate() {
            let (scan, e) = scan_with_rows(&format!("2026-03-{day}T00:00:00.000000Z"), "/c", 5 + i);
            s.insert_scan(&scan, &e, &[], None).unwrap();
        }
        let r = prune(&s, 2, 100, u64::MAX).unwrap();
        assert_eq!((r.by_per_root, r.by_total, r.by_budget), (1, 0, 0), "{r:?}");
        assert_eq!(r.estimated_bytes_before, r.estimated_bytes_after);
        assert!(r.floor_over_budget.is_none());
        assert_eq!(ids(&s).len(), 2);
        // The estimate accounts for the live file: every footprint's share
        // sums to (page_count − freelist) × page_size.
        let (pages, free, size): (i64, i64, i64) = (
            s.conn.query_row("PRAGMA page_count", [], |r| r.get(0)).unwrap(),
            s.conn.query_row("PRAGMA freelist_count", [], |r| r.get(0)).unwrap(),
            s.conn.query_row("PRAGMA page_size", [], |r| r.get(0)).unwrap(),
        );
        let live = ((pages - free) * size) as u64;
        let summed: u64 = s.scan_footprints().unwrap().iter().map(|f| f.estimated_bytes).sum();
        assert!(summed <= live && summed + 2 >= live, "integer division may drop a byte per scan: {summed} vs {live}");
    }

    /// phantom-ap6: a hostile size clamps to i64::MAX (never wraps negative,
    /// so ordering and the 1 MiB filter stay right); identifiers with the
    /// high bit set round-trip EXACTLY, because merging two inodes would be
    /// worse than a negative-looking integer in the database.
    #[test]
    fn hostile_magnitudes_clamp_and_identifiers_round_trip_exactly() {
        let s = store();
        let mut scan = scan_at("2026-03-17T00:00:00.000000Z", "/h");
        scan.total_disk_size = u64::MAX;
        scan.total_private_size = Some(u64::MAX - 1);
        let mut entries = tree("/h");
        let big = ScanEntry {
            path: "/h/huge".into(),
            parent_path: Some("/h".into()),
            name: "huge".into(),
            is_dir: false,
            disk_size: u64::MAX,
            logical_size: u64::MAX,
            modified_at: None,
            file_type: Some("bin".into()),
            category: None,
            nlink: 2,
            dev: u64::MAX - 7,
            ino: 1u64 << 63,
            file_count: None,
            dir_count: None,
            private_size: Some(u64::MAX),
            shared_size: Some(0),
            clone_id: Some(u64::MAX),
            flags: None,
        };
        entries.push(big.clone());
        s.insert_scan(&scan, &entries, &[], None).unwrap();

        let back = s.get_scan(scan.id).unwrap();
        assert_eq!(back.total_disk_size, i64::MAX as u64, "clamped, not wrapped");
        assert_eq!(back.total_private_size, Some(i64::MAX as u64));
        let e = s.entry(scan.id, "/h/huge").unwrap();
        assert_eq!(e.disk_size, i64::MAX as u64);
        assert_eq!(e.private_size, Some(i64::MAX as u64));
        assert_eq!((e.dev, e.ino, e.clone_id), (u64::MAX - 7, 1u64 << 63, Some(u64::MAX)), "identity intact");
        // The clamped row still sorts as the biggest, which a wrapped
        // (negative) value would not.
        let raw: i64 = s
            .conn
            .query_row("SELECT disk_size FROM entries WHERE path = '/h/huge'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(raw, i64::MAX);
        assert!(raw > 0);
    }

    #[test]
    fn prune_with_enough_room_deletes_nothing() {
        let s = store();
        s.insert_scan(&scan_at("2026-03-17T00:00:00.000000Z", "/x"), &[], &[], None)
            .unwrap();
        assert_eq!(s.prune_to_last(5).unwrap(), 0);
        assert_eq!(s.list_scans().unwrap().len(), 1);
    }

    #[test]
    fn prune_keep_zero_deletes_all() {
        let s = store();
        s.insert_scan(&scan_at("2026-03-17T00:00:00.000000Z", "/x"), &[], &[], None)
            .unwrap();
        assert_eq!(s.prune_to_last(0).unwrap(), 1);
        assert!(s.list_scans().unwrap().is_empty());
    }

    /// v6: a second insert of the same id is the terminal upsert, and it
    /// REPLACES the results rather than stacking a second copy of them —
    /// re-inserting a scan with entries must not trip UNIQUE(scan_id, path)
    /// or double the type totals.
    #[test]
    fn reinserting_a_scan_replaces_its_results() {
        let s = store();
        let scan = scan_at("2026-03-17T14:30:00.000000Z", "/haunt");
        let rows = vec![
            entry("/haunt", None, 4096, true),
            entry("/haunt/a.bin", Some("/haunt"), 4096, false),
        ];
        let totals = vec![FileTypeTotal {
            file_type: Some("bin".into()),
            disk_size: 4096,
            file_count: 1,
        }];
        s.insert_scan(&scan, &rows, &totals, None).unwrap();
        s.insert_scan(&scan, &rows, &totals, None).unwrap();
        assert_eq!(s.list_scans().unwrap().len(), 1);
        assert_eq!(s.entries(scan.id).unwrap().len(), 2, "entries replaced, not appended");
        assert_eq!(s.file_type_totals(scan.id).unwrap().len(), 1, "totals replaced, not appended");
    }

    /// Scan row + entry batch are ONE transaction. A failing entry (here: a
    /// duplicate path, rejected by UNIQUE(scan_id, path)) must roll back the
    /// scan row too. Mutation-proof: commit the scan insert separately from
    /// the entry batch and this fails, because the scan row would survive.
    #[test]
    fn failed_entry_batch_rolls_back_the_scan_row() {
        let s = store();
        let scan = scan_at("2026-03-17T14:30:00.000000Z", "/haunt");
        let dup = vec![
            entry("/haunt", None, 0, true),
            entry("/haunt/a.txt", Some("/haunt"), 512, false),
            entry("/haunt/a.txt", Some("/haunt"), 512, false),
        ];
        let err = s.insert_scan(&scan, &dup, &[], None).unwrap_err();
        assert!(matches!(err, CoreError::Db(_)));
        assert!(
            matches!(s.get_scan(scan.id), Err(CoreError::NotFound(_))),
            "scan row must not survive a failed entry batch"
        );
    }

    // Proves foreign keys are enforced on this connection: an entry cannot
    // reference a scan that does not exist.
    #[test]
    fn entry_without_scan_violates_foreign_key() {
        let s = store();
        let err = s.conn.execute(
            "INSERT INTO entries (scan_id, path, parent_path, name, is_dir,
                disk_size, logical_size, modified_at, file_type, category,
                nlink, dev, ino)
             VALUES ('no-such-scan', '/x', NULL, 'x', 0, 0, 0, NULL, NULL,
                NULL, 1, 0, 0)",
            [],
        );
        assert!(err.is_err(), "FK violation must be rejected");
    }

    // Proves the CHECK constraint pins the status vocabulary at the SQL
    // layer, independent of the Rust enum.
    #[test]
    fn unknown_status_string_violates_check_constraint() {
        let s = store();
        let err = s.conn.execute(
            "INSERT INTO scans (id, root_path, status, started_at, finished_at,
                total_disk_size, total_logical_size, file_count, dir_count,
                error_count)
             VALUES ('x', '/x', 'exploded', '2026-01-01T00:00:00.000000Z',
                NULL, 0, 0, 0, 0, 0)",
            [],
        );
        assert!(err.is_err(), "CHECK must reject unknown status strings");
    }

    #[test]
    fn file_store_runs_in_wal_mode_and_persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scans.db");
        let id = {
            let s = ScanStore::open(&path).unwrap();
            let mode: String = s
                .conn
                .query_row("PRAGMA journal_mode", [], |r| r.get(0))
                .unwrap();
            assert_eq!(mode, "wal");
            let scan = scan_at("2026-03-17T14:30:00.000000Z", "/durable");
            s.insert_scan(&scan, &tree("/durable"), &[], None).unwrap();
            scan.id
        };
        let s = ScanStore::open(&path).unwrap();
        assert_eq!(s.get_scan(id).unwrap().root_path, "/durable");
        assert_eq!(s.entries(id).unwrap().len(), 3);
    }

    // --- WAL policy (1.1.1, phantom-cnr.1) --------------------------------
    // The -wal file is measured directly: a 512 MB idle WAL was the symptom,
    // so the file size is the thing to pin, not a pragma's return value.

    fn wal_bytes(db: &Path) -> u64 {
        let wal = std::path::PathBuf::from(format!("{}-wal", db.display()));
        std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0)
    }

    /// `dirs` directories of `files_per_dir` files each, populated enough to
    /// cost real WAL pages (paths ~90 bytes, six sizes, a type).
    fn bulk_entries(root: &str, dirs: usize, files_per_dir: usize) -> Vec<ScanEntry> {
        let mut out = Vec::with_capacity(dirs * (files_per_dir + 1));
        for d in 0..dirs {
            let dir_path = format!("{root}/services/service-{d:04}/src/components");
            out.push(ScanEntry {
                path: dir_path.clone(),
                parent_path: Some(root.to_string()),
                name: format!("service-{d:04}"),
                is_dir: true,
                disk_size: 4096,
                logical_size: 4096,
                modified_at: None,
                file_type: None,
                category: None,
                nlink: 2,
                dev: 1,
                ino: (1_000_000 + d) as u64,
                file_count: Some(files_per_dir as u64),
                dir_count: Some(0),
                private_size: None,
                shared_size: None,
                clone_id: None,
                flags: None,
            });
            for f in 0..files_per_dir {
                out.push(ScanEntry {
                    path: format!("{dir_path}/module-{f:02}.generated.ts"),
                    parent_path: Some(dir_path.clone()),
                    name: format!("module-{f:02}.generated.ts"),
                    is_dir: false,
                    disk_size: 1_048_576 + (f as u64 * 512),
                    logical_size: 1_048_000 + (f as u64 * 512),
                    modified_at: None,
                    file_type: Some("ts".into()),
                    category: None,
                    nlink: 1,
                    dev: 1,
                    ino: (2_000_000 + d * 64 + f) as u64,
                    file_count: None,
                    dir_count: None,
                    private_size: None,
                    shared_size: None,
                    clone_id: None,
                    flags: None,
                });
            }
        }
        out
    }

    #[test]
    fn wal_ceiling_is_set_on_file_stores() {
        // Mutation target: drop the journal_size_limit pragma in open() and
        // this reads -1 — the bundled SQLite's default, the 512 MB idle WAL.
        let dir = tempfile::tempdir().unwrap();
        let s = ScanStore::open(&dir.path().join("scans.db")).unwrap();
        let limit: i64 = s.conn.query_row("PRAGMA journal_size_limit", [], |r| r.get(0)).unwrap();
        assert_eq!(limit as u64, WAL_SIZE_LIMIT_BYTES);
        assert_eq!(WAL_SIZE_LIMIT_BYTES, 64 * 1024 * 1024, "the documented ceiling (docs/data-safety.md)");
    }

    #[test]
    fn a_large_insert_leaves_the_wal_at_zero() {
        // Mutation target: drop checkpoint_truncate() after insert_scan's
        // commit and the -wal file holds the whole insert (megabytes).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scans.db");
        let s = ScanStore::open(&path).unwrap();
        let entries = bulk_entries("/big", 400, 49); // 20,000 rows
        let mut scan = scan_at("2026-03-17T14:30:00.000000Z", "/big");
        scan.file_count = 19_600;
        scan.dir_count = 400;
        s.insert_scan(&scan, &entries, &[], None).unwrap();
        assert_eq!(s.entries(scan.id).unwrap().len(), 20_000, "the insert itself is intact");
        assert_eq!(wal_bytes(&path), 0, "insert_scan must fold and truncate the WAL");
        assert!(wal_bytes(&path) <= WAL_SIZE_LIMIT_BYTES);
    }

    #[test]
    fn prune_and_delete_return_the_wal_to_its_floor() {
        // Mutation target: drop checkpoint_truncate() from prune_to_last (or
        // delete_scan) and the deletes' pages stay in the -wal file.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scans.db");
        let s = ScanStore::open(&path).unwrap();
        for (i, root) in ["/one", "/two", "/three"].iter().enumerate() {
            let mut scan = scan_at(&format!("2026-03-1{}T00:00:00.000000Z", i + 1), root);
            scan.file_count = 4_900;
            s.insert_scan(&scan, &bulk_entries(root, 100, 49), &[], None).unwrap();
        }
        assert_eq!(wal_bytes(&path), 0);

        // The delete must actually have written pages — otherwise a zero WAL
        // proves nothing. Measure through the raw connection first.
        let victim = s.list_scans().unwrap().last().unwrap().id;
        s.conn.execute("DELETE FROM scans WHERE id = ?1", params![victim.to_string()]).unwrap();
        assert!(wal_bytes(&path) > 0, "a raw delete leaves pages in the WAL (the thing the policy fixes)");
        s.checkpoint_truncate().unwrap();
        assert_eq!(wal_bytes(&path), 0);

        let remaining = s.list_scans().unwrap();
        assert_eq!(remaining.len(), 2);
        s.delete_scan(remaining[1].id).unwrap();
        assert_eq!(wal_bytes(&path), 0, "delete_scan must fold and truncate the WAL");

        assert_eq!(s.prune_retention(25, 0).unwrap(), 1);
        assert_eq!(wal_bytes(&path), 0, "prune_retention must fold and truncate the WAL");
        assert!(s.list_scans().unwrap().is_empty());
    }

    #[test]
    fn a_reader_holding_the_wal_makes_the_checkpoint_busy_and_it_retries_once() {
        // Mutation target: drop the retry and `retried` is false with the
        // reader gone by then — or the busy warning fires on every poll.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scans.db");
        let s = ScanStore::open(&path).unwrap();
        let scan = scan_at("2026-03-17T14:30:00.000000Z", "/held");
        s.insert_scan(&scan, &tree("/held"), &[], None).unwrap();
        // Dirty the WAL without checkpointing, then hold a read transaction
        // on a second connection: TRUNCATE cannot complete while it is open.
        s.conn.execute("UPDATE scans SET file_count = file_count + 1", []).unwrap();
        let reader = Connection::open(&path).unwrap();
        reader.execute_batch("BEGIN; SELECT count(*) FROM scans;").unwrap();
        let held = s.checkpoint_truncate().unwrap();
        assert!(held.busy, "a reader inside a transaction blocks TRUNCATE: {held:?}");
        assert!(held.retried, "the store must try a second time before giving up: {held:?}");
        reader.execute_batch("COMMIT").unwrap();
        let free = s.checkpoint_truncate().unwrap();
        assert!(!free.busy && !free.retried, "with the reader gone the first attempt succeeds: {free:?}");
        assert_eq!(wal_bytes(&path), 0);
    }

    #[test]
    fn in_memory_stores_treat_the_checkpoint_as_a_no_op() {
        let s = store();
        let out = s.checkpoint_truncate().unwrap();
        assert_eq!(out, CheckpointOutcome { busy: false, wal_pages: -1, checkpointed_pages: -1, retried: false });
    }

    #[test]
    fn entry_by_path_round_trips_and_misses_are_not_found() {
        let s = store();
        let scan = scan_at("2026-03-17T14:30:00.000000Z", "/haunt");
        let entries = tree("/haunt");
        s.insert_scan(&scan, &entries, &[], None).unwrap();

        assert_eq!(s.entry(scan.id, "/haunt/a.txt").unwrap(), entries[1]);
        let err = s.entry(scan.id, "/haunt/nope.txt").unwrap_err();
        assert!(matches!(err, CoreError::NotFound(_)));
        let err = s.entry(Uuid::new_v4(), "/haunt").unwrap_err();
        assert!(matches!(err, CoreError::NotFound(_)));
    }

    /// 1.1.1 (phantom-cnr.10): a path with no row is described, not merely
    /// missed — the message names the nearest ancestor that HAS a row, which
    /// is where the folded path's bytes and counts are. Mutation-proof:
    /// restore the bare "path … in scan …" message and the `contains`
    /// assertions fail; walk only one level up (parent instead of the whole
    /// chain) and the deep miss names nothing.
    #[test]
    fn entry_miss_below_threshold_names_its_nearest_persisted_ancestor() {
        let s = store();
        let scan = scan_at("2026-03-17T14:30:00.000000Z", "/haunt");
        // Only the root and one big file persisted; sub/ and everything
        // under it were folded into the root's aggregate.
        let entries = vec![
            entry("/haunt", None, 5_000_000, true),
            entry("/haunt/big.bin", Some("/haunt"), 4_000_000, false),
        ];
        s.insert_scan(&scan, &entries, &[], None).unwrap();

        let err = s.entry(scan.id, "/haunt/sub/deep/tiny.rs").unwrap_err();
        let CoreError::NotFound(msg) = err else { panic!("want NotFound, got {err:?}") };
        assert!(msg.contains(persist::NOT_INDIVIDUALLY_PERSISTED), "{msg}");
        assert!(msg.contains("\"/haunt\""), "must name the persisted ancestor: {msg}");
        assert!(msg.contains("\"/haunt/sub/deep/tiny.rs\""), "must name the asked path: {msg}");
        assert_eq!(
            s.nearest_persisted_ancestor(scan.id, "/haunt/sub/deep/tiny.rs").unwrap().as_deref(),
            Some("/haunt")
        );
        // The nearest, not the root: with sub/ persisted the message names sub/.
        let mut with_sub = entries.clone();
        with_sub.push(entry("/haunt/sub", Some("/haunt"), 1_048_576, true));
        s.insert_scan(&scan, &with_sub, &[], None).unwrap();
        let err = s.entry(scan.id, "/haunt/sub/deep").unwrap_err();
        assert!(err.to_string().contains("\"/haunt/sub\""), "{err}");

        // Outside the scan root nothing is an ancestor: the plain message,
        // no invented explanation.
        let err = s.entry(scan.id, "/elsewhere/x").unwrap_err();
        let msg = err.to_string();
        assert!(!msg.contains(persist::NOT_INDIVIDUALLY_PERSISTED), "{msg}");
        assert!(msg.contains("\"/elsewhere/x\""), "{msg}");
        assert_eq!(s.nearest_persisted_ancestor(scan.id, "/elsewhere/x").unwrap(), None);
        // A file that IS a row is simply returned.
        assert_eq!(s.entry(scan.id, "/haunt/big.bin").unwrap().path, "/haunt/big.bin");
    }

    fn files_fixture(s: &ScanStore) -> Scan {
        let scan = scan_at("2026-03-17T14:30:00.000000Z", "/haunt");
        let mut big = entry("/haunt/big.log", Some("/haunt"), 4096, false);
        big.file_type = Some("log".into());
        let mut pct = entry("/haunt/100%.txt", Some("/haunt"), 512, false);
        pct.file_type = Some("txt".into());
        let entries = vec![
            entry("/haunt", None, 0, true),
            entry("/haunt/a.txt", Some("/haunt"), 2048, false),
            entry("/haunt/b.txt", Some("/haunt"), 1024, false),
            big,
            pct,
        ];
        s.insert_scan(&scan, &entries, &[], None).unwrap();
        scan
    }

    // Mutation targets: flip `disk_size DESC` to ASC and the size case
    // fails; drop `is_dir = 0` and the directory row leaks into every case.
    #[test]
    fn files_page_sorts_by_each_order() {
        let s = store();
        let scan = files_fixture(&s);
        let q = |sort| FileQuery { sort, ..Default::default() };

        let by_size: Vec<String> = s
            .files_page(scan.id, &q(FileSort::Size), 10, 0)
            .unwrap()
            .files
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(by_size, vec!["big.log", "a.txt", "b.txt", "100%.txt"]);

        let by_name: Vec<String> = s
            .files_page(scan.id, &q(FileSort::Name), 10, 0)
            .unwrap()
            .files
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(by_name, vec!["100%.txt", "a.txt", "b.txt", "big.log"]);

        let by_path: Vec<String> = s
            .files_page(scan.id, &q(FileSort::Path), 10, 0)
            .unwrap()
            .files
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(by_path, vec!["100%.txt", "a.txt", "b.txt", "big.log"]);
    }

    #[test]
    fn files_page_filters_by_type_and_search() {
        let s = store();
        let scan = files_fixture(&s);

        let logs = s
            .files_page(
                scan.id,
                &FileQuery { file_type: Some("log"), ..Default::default() },
                10,
                0,
            )
            .unwrap()
            .files;
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].name, "big.log");

        // The wire is generous on case in; stored types are lowercase.
        let logs_upper = s
            .files_page(
                scan.id,
                &FileQuery { file_type: Some("LOG"), ..Default::default() },
                10,
                0,
            )
            .unwrap()
            .files;
        assert_eq!(logs_upper.len(), 1);

        let hits = s
            .files_page(
                scan.id,
                &FileQuery { search: Some("big"), ..Default::default() },
                10,
                0,
            )
            .unwrap()
            .files;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "big.log");
    }

    // Mutation target: drop the ESCAPE clause (or the metacharacter
    // escaping) and the literal `%` search matches every file.
    #[test]
    fn files_page_search_treats_like_metacharacters_literally() {
        let s = store();
        let scan = files_fixture(&s);
        let hits = s
            .files_page(
                scan.id,
                &FileQuery { search: Some("100%"), ..Default::default() },
                10,
                0,
            )
            .unwrap()
            .files;
        assert_eq!(hits.len(), 1, "literal %% must not act as a wildcard");
        assert_eq!(hits[0].name, "100%.txt");
    }

    #[test]
    fn files_page_paginates_with_continuation() {
        let s = store();
        let scan = files_fixture(&s);
        let q = FileQuery::default();

        let page = s.files_page(scan.id, &q, 3, 0).unwrap();
        assert_eq!(page.files.len(), 3);
        assert_eq!(page.next_offset, Some(3));
        let last = s.files_page(scan.id, &q, 3, 3).unwrap();
        assert_eq!(last.files.len(), 1);
        assert_eq!(last.next_offset, None, "final page has no continuation");

        // Exactly-limit page must not advertise a spurious next page.
        let exact = s.files_page(scan.id, &q, 4, 0).unwrap();
        assert_eq!(exact.files.len(), 4);
        assert_eq!(exact.next_offset, None);
    }

    #[test]
    fn files_page_unknown_scan_is_not_found() {
        let err = store()
            .files_page(Uuid::new_v4(), &FileQuery::default(), 10, 0)
            .unwrap_err();
        assert!(matches!(err, CoreError::NotFound(_)));
    }

    #[test]
    fn file_type_totals_round_trip_ordered_and_cascade() {
        let s = store();
        let scan = scan_at("2026-03-17T14:30:00.000000Z", "/haunt");
        let totals = vec![
            FileTypeTotal { file_type: Some("rs".into()), disk_size: 900, file_count: 3 },
            FileTypeTotal { file_type: None, disk_size: 500, file_count: 1 },
            FileTypeTotal { file_type: Some("txt".into()), disk_size: 500, file_count: 2 },
        ];
        // Insert deliberately out of order; read-back order is the contract.
        let shuffled = vec![totals[2].clone(), totals[0].clone(), totals[1].clone()];
        s.insert_scan(&scan, &[], &shuffled, None).unwrap();

        assert_eq!(s.file_type_totals(scan.id).unwrap(), totals);

        let err = s.file_type_totals(Uuid::new_v4()).unwrap_err();
        assert!(matches!(err, CoreError::NotFound(_)));

        s.delete_scan(scan.id).unwrap();
        let orphans: i64 = s
            .conn
            .query_row("SELECT COUNT(*) FROM scan_file_types", [], |r| r.get(0))
            .unwrap();
        assert_eq!(orphans, 0, "cascade must remove the scan's type totals");
    }

    // The shared fixture is the wire contract for the summary shape; the
    // store must round-trip those exact semantics (it stores the same JSON).
    const RAW_HOTSPOTS: &str = include_str!("../../../tests/fixtures/hotspots-summary.json");

    #[test]
    fn hotspots_round_trip_from_raw_fixture_bytes() {
        let s = store();
        let summary: HotspotsSummary = serde_json::from_str(RAW_HOTSPOTS).unwrap();
        let scan = scan_at("2026-03-17T14:30:00.000000Z", "/haunt");
        s.insert_scan(&scan, &[], &[], Some(&summary)).unwrap();

        let back = s.hotspots(scan.id).unwrap();
        assert_eq!(back, Some(summary));
    }

    #[test]
    fn hotspots_none_when_persisted_without_a_summary() {
        // Cancelled/failed scans persist a metadata-only row: NULL summary,
        // distinguishable from an unknown scan.
        let s = store();
        let scan = scan_at("2026-03-17T14:30:00.000000Z", "/haunt");
        s.insert_scan(&scan, &[], &[], None).unwrap();
        assert_eq!(s.hotspots(scan.id).unwrap(), None);
    }

    #[test]
    fn hotspots_of_unknown_scan_is_not_found() {
        let err = store().hotspots(Uuid::new_v4()).unwrap_err();
        assert!(matches!(err, CoreError::NotFound(_)));
    }

    #[test]
    fn corrupt_stored_hotspots_is_a_schema_error_not_invalid_input() {
        // A parse failure here is OUR corruption; it must map to the generic
        // 500 at the API (Schema), never a 400 blaming the caller.
        let s = store();
        let scan = scan_at("2026-03-17T14:30:00.000000Z", "/haunt");
        s.insert_scan(&scan, &[], &[], None).unwrap();
        s.conn
            .execute(
                "UPDATE scans SET hotspots = 'not json' WHERE id = ?1",
                params![scan.id.to_string()],
            )
            .unwrap();
        let err = s.hotspots(scan.id).unwrap_err();
        assert!(matches!(err, CoreError::Schema(_)));
    }

    #[test]
    fn entries_carry_persisted_categories() {
        // The category column round-trips through insert_scan → entries;
        // the Phase-5 post-pass relies on this seam.
        let s = store();
        let scan = scan_at("2026-03-17T14:30:00.000000Z", "/p");
        let mut root = entry("/p", None, 0, true);
        root.category = None;
        let mut nm = entry("/p/node_modules", Some("/p"), 0, true);
        nm.category = Some("regenerableArtifact".into());
        let mut file = entry("/p/node_modules/x.js", Some("/p/node_modules"), 2048, false);
        file.category = Some("regenerableArtifact".into());
        s.insert_scan(&scan, &[root, nm, file], &[], None).unwrap();

        let back = s.entries(scan.id).unwrap();
        let by_path: std::collections::HashMap<&str, Option<&str>> = back
            .iter()
            .map(|e| (e.path.as_str(), e.category.as_deref()))
            .collect();
        assert_eq!(by_path["/p"], None);
        assert_eq!(by_path["/p/node_modules"], Some("regenerableArtifact"));
        assert_eq!(by_path["/p/node_modules/x.js"], Some("regenerableArtifact"));
    }

    /// LOAD-BEARING MEASUREMENT (plan Phase 1): round-trip a 100k-entry
    /// synthetic scan and report wall time + DB size. The numbers decide
    /// full persistence vs the size-capped fallback before Phase 2 commits.
    /// Run manually: cargo test -p phantom-core measure_100k -- --ignored --nocapture
    #[test]
    #[ignore = "measurement, not a gate — run with --ignored --nocapture"]
    fn measure_100k_entry_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("measure.db");
        let s = ScanStore::open(&path).unwrap();

        // 2,000 dirs × 49 files + the dirs themselves = 100,000 entries with
        // realistic path lengths and populated optional fields.
        let root = "/Users/ghost/Code/haunted-monorepo";
        let modified = wire_time::from_wire("2026-03-17T14:30:00.123456Z").unwrap();
        let mut entries = Vec::with_capacity(100_000);
        for d in 0..2_000 {
            let dir_path = format!("{root}/services/service-{d:04}/src/components");
            entries.push(ScanEntry {
                path: dir_path.clone(),
                parent_path: Some(root.to_string()),
                name: format!("service-{d:04}"),
                is_dir: true,
                disk_size: 0,
                logical_size: 0,
                modified_at: None,
                file_type: None,
                category: None,
                nlink: 51,
                dev: 16777233,
                ino: 1_000_000 + d,
                file_count: None,
                dir_count: None,
                private_size: None,
                shared_size: None,
                clone_id: None,
                flags: None,
            });
            for f in 0..49 {
                entries.push(ScanEntry {
                    path: format!("{dir_path}/module-{f:02}.generated.ts"),
                    parent_path: Some(dir_path.clone()),
                    name: format!("module-{f:02}.generated.ts"),
                    is_dir: false,
                    disk_size: 4096 + (f * 512),
                    logical_size: 3900 + (f * 512),
                    modified_at: Some(modified),
                    file_type: Some("ts".into()),
                    category: None,
                    nlink: 1,
                    dev: 16777233,
                    ino: 2_000_000 + d * 49 + f,
                    file_count: None,
                    dir_count: None,
                    private_size: None,
                    shared_size: None,
                    clone_id: None,
                    flags: None,
                });
            }
        }
        assert_eq!(entries.len(), 100_000);

        let mut scan = scan_at("2026-03-17T14:30:00.000000Z", root);
        scan.file_count = 98_000;
        scan.dir_count = 2_000;

        let t0 = std::time::Instant::now();
        s.insert_scan(&scan, &entries, &[], None).unwrap();
        let insert = t0.elapsed();

        let t1 = std::time::Instant::now();
        let back = s.entries(scan.id).unwrap();
        let read = t1.elapsed();
        assert_eq!(back.len(), 100_000);

        // Fold the WAL into the main file so the size is the real footprint.
        s.conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
            .unwrap();
        let db_bytes = std::fs::metadata(&path).unwrap().len();

        println!("MEASUREMENT 100k entries:");
        println!("  insert: {insert:?}");
        println!("  read-back: {read:?}");
        println!("  round-trip: {:?}", insert + read);
        println!(
            "  db size: {db_bytes} bytes ({:.1} MB)",
            db_bytes as f64 / (1024.0 * 1024.0)
        );
    }
}
