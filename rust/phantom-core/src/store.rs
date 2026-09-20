// SQLite persistence. ScanStore owns the scan domain — the ONE writer
// (via phantom-api); keep the shape when extending.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, Row, params};
use uuid::Uuid;

use crate::classify::HotspotsSummary;
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

pub struct ScanStore {
    conn: Connection,
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
        // journal_mode returns the resulting mode as a row, so query_row
        // rather than pragma_update.
        conn.query_row("PRAGMA journal_mode=WAL", [], |_| Ok(()))?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        // In-memory databases cannot use WAL; everything else is identical.
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        // Stock SQLite ships with foreign keys OFF per connection; without
        // enforcement, ON DELETE CASCADE silently never fires and deleted
        // scans leak their entries forever. Our bundled libsqlite3-sys
        // happens to default them ON (verified by probe, 2026-08-31), so
        // this line is defense-in-depth against a build-config change, and
        // `entry_without_scan_violates_foreign_key` pins the behavior
        // whichever mechanism provides it.
        conn.pragma_update(None, "foreign_keys", true)?;
        schema::validate_or_init(&conn)?;
        Ok(Self { conn })
    }

    /// The raw connection, for `backup::backup_verified`'s snapshot API.
    pub(crate) fn connection(&self) -> &Connection {
        &self.conn
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
    pub fn entry(&self, scan_id: Uuid, path: &str) -> Result<ScanEntry> {
        self.get_scan(scan_id)?;
        self.conn
            .query_row(
                "SELECT path, parent_path, name, is_dir, disk_size, logical_size,
                    modified_at, file_type, category, nlink, dev, ino,
                file_count, dir_count, private_size, shared_size, clone_id, flags
                 FROM entries WHERE scan_id = ?1 AND path = ?2",
                params![scan_id.to_string(), path],
                row_to_entry,
            )
            .optional()?
            .ok_or_else(|| CoreError::NotFound(format!("path {path:?} in scan {scan_id}")))
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
        Ok(deleted)
    }

    /// Retention (v1.1 Phase 6, phantom-9tt): keep the newest
    /// `keep_per_root` scans OF EACH ROOT, then the newest `keep_total`
    /// overall. Per-root first, so a history of `~` is never evicted by
    /// scans of other roots (Phase 4 made history a feature); the total
    /// bounds the file's growth. Roots are compared with trailing slashes
    /// stripped, like `same_root`. Entries and type totals cascade.
    pub fn prune_retention(&self, keep_per_root: usize, keep_total: usize) -> Result<usize> {
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
        Ok(per_root + total)
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
