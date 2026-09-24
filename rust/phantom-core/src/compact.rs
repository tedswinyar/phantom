//! compact.rs — give deleted pages back to the filesystem (phantom-cnr.2).
//!
//! Until 1.1.1 nothing in Phantom ever ran a VACUUM: `prune_retention` and
//! `delete_scan` removed rows, SQLite kept the pages on its freelist, and the
//! file never shrank. Measured 2026-09-16: pruning 25 scans to 3 left
//! `phantom.db` at 4.2 GB; a manual VACUUM took it to 1.4 GB. Retention was
//! cosmetic on the filesystem and `phantom scans delete` printed "deleted"
//! while freeing nothing — the phantom-grw honesty class.
//!
//! The decision (docs/research/v1.1.1-plan.md, "compaction"):
//!
//! * **New files are `auto_vacuum=INCREMENTAL`** — `ScanStore::open` sets the
//!   pragma before the schema exists (it only takes effect on an empty file).
//!   A file-header flag, not a `user_version` bump: `schema.sql` is unchanged
//!   and a 1.1.0 binary opens the file fine.
//! * **Existing files (`auto_vacuum=NONE`) convert ONCE**, after a prune or
//!   delete, only when the dead pages are worth it (≥ 256 MiB and ≥ 25 % of
//!   the file), no scan is running, and the volume has ≥ 2× the live size
//!   available — a VACUUM in WAL mode writes the whole new image into the
//!   WAL (the 1.5 GB WAL Ted saw), and it must never make a disk emergency
//!   worse (Ted's case: 4.2 GB file, 5.7 GiB free → refused, correctly). A
//!   refusal is reported with its numbers and retried on the next write.
//! * **Thereafter every prune/delete runs `incremental_vacuum`** in
//!   [`CompactionPolicy::step_pages`] steps with a TRUNCATE checkpoint between
//!   steps, so the WAL stays under the 64 MiB ceiling (phantom-cnr.1).
//!
//! Compaction runs on its OWN connection so the caller does not hold the
//! store mutex for tens of seconds: WAL readers proceed against the old
//! snapshot and the app's 1 Hz poll never stalls into `.failed`.

use std::path::Path;

use rusqlite::Connection;

use crate::{CoreError, Result};
use crate::store::WAL_SIZE_LIMIT_BYTES;

/// When an existing `auto_vacuum=NONE` file may be converted, and how big an
/// incremental step is. `Default` is production; tests shrink the thresholds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CompactionPolicy {
    /// Dead pages must total at least this many bytes …
    pub min_dead_bytes: u64,
    /// … AND at least this fraction of the file (0.25 = a quarter).
    pub min_dead_fraction: f64,
    /// The volume must have this many times the LIVE size available.
    pub free_space_multiple: u64,
    /// Pages per `incremental_vacuum` step (a TRUNCATE checkpoint between).
    pub step_pages: u64,
}

impl Default for CompactionPolicy {
    fn default() -> Self {
        Self {
            min_dead_bytes: 256 * 1024 * 1024,
            min_dead_fraction: 0.25,
            free_space_multiple: 2,
            step_pages: 2000,
        }
    }
}

/// `PRAGMA auto_vacuum`'s three answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoVacuum {
    None,
    Full,
    Incremental,
}

impl AutoVacuum {
    fn from_pragma(v: i64) -> Result<Self> {
        match v {
            0 => Ok(Self::None),
            1 => Ok(Self::Full),
            2 => Ok(Self::Incremental),
            other => Err(CoreError::InvalidInput(format!("unknown auto_vacuum value {other}"))),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Full => "full",
            Self::Incremental => "incremental",
        }
    }
}

/// What one compaction pass did, with the numbers the API logs.
#[derive(Debug, Clone, PartialEq)]
pub struct CompactionReport {
    pub auto_vacuum_before: AutoVacuum,
    pub auto_vacuum_after: AutoVacuum,
    /// A one-time `VACUUM` converted the file to INCREMENTAL this pass.
    pub converted: bool,
    /// Why nothing was done to a NONE file, with the numbers. `None` when the
    /// pass acted (or the file was already INCREMENTAL / FULL).
    pub refusal: Option<String>,
    pub page_size: u64,
    /// Freelist pages × page size before the pass.
    pub dead_bytes_before: u64,
    pub file_bytes_before: u64,
    pub file_bytes_after: u64,
}

impl CompactionReport {
    /// Bytes the filesystem got back (0 when the file did not shrink).
    pub fn returned_bytes(&self) -> u64 {
        self.file_bytes_before.saturating_sub(self.file_bytes_after)
    }
}

/// One compaction pass over the database at `db_path`, on a connection of
/// its own. `scan_running` gates only the one-time VACUUM (a scan insert
/// arriving mid-VACUUM would wait on the lock past BUSY_TIMEOUT).
/// `available_bytes` reads the volume's headroom for the path it is given
/// (production: `volume::available_bytes`; tests inject a number).
pub fn compact(
    db_path: &Path,
    policy: &CompactionPolicy,
    scan_running: bool,
    available_bytes: &dyn Fn(&Path) -> Result<u64>,
) -> Result<CompactionReport> {
    // Connection::open would CREATE an empty database here; maintenance of a
    // file that does not exist is an error, never a new file.
    if !db_path.is_file() {
        return Err(CoreError::NotFound(format!("database {}", db_path.display())));
    }
    let conn = open_maintenance(db_path)?;
    let mode = auto_vacuum(&conn)?;
    let page_size = pragma_u64(&conn, "page_size")?;
    let page_count = pragma_u64(&conn, "page_count")?;
    let freelist = pragma_u64(&conn, "freelist_count")?;
    let dead = freelist * page_size;
    let live = page_count.saturating_sub(freelist) * page_size;
    let file_before = file_len(db_path);

    let mut converted = false;
    let mut refusal = None;
    match mode {
        AutoVacuum::Incremental => incremental_vacuum(&conn, policy.step_pages)?,
        AutoVacuum::Full => {}
        AutoVacuum::None => {
            let threshold = policy
                .min_dead_bytes
                .max((file_before as f64 * policy.min_dead_fraction) as u64);
            if dead < threshold {
                refusal = Some(format!(
                    "dead pages {} below the conversion threshold {} (file {})",
                    mb(dead),
                    mb(threshold),
                    mb(file_before)
                ));
            } else if scan_running {
                refusal = Some(format!(
                    "a scan is running; VACUUM of {} dead / {} live waits for the next quiet write",
                    mb(dead),
                    mb(live)
                ));
            } else {
                let volume = db_path.parent().unwrap_or(db_path);
                let avail = available_bytes(volume)?;
                let need = live.saturating_mul(policy.free_space_multiple);
                if avail < need {
                    refusal = Some(format!(
                        "volume has {} available, VACUUM needs {} ({}× the {} live); not making a full disk worse",
                        mb(avail),
                        mb(need),
                        policy.free_space_multiple,
                        mb(live)
                    ));
                } else {
                    // The pragma alone changes nothing on a populated file;
                    // the VACUUM rewrites it under the new setting.
                    conn.execute_batch("PRAGMA auto_vacuum = INCREMENTAL; VACUUM;")?;
                    converted = true;
                }
            }
        }
    }
    checkpoint_truncate(&conn);
    let mode_after = auto_vacuum(&conn)?;
    Ok(CompactionReport {
        auto_vacuum_before: mode,
        auto_vacuum_after: mode_after,
        converted,
        refusal,
        page_size,
        dead_bytes_before: dead,
        file_bytes_before: file_before,
        file_bytes_after: file_len(db_path),
    })
}

/// Return every freelist page in steps, folding the WAL between steps so it
/// never holds more than one step's worth of rewritten pages.
fn incremental_vacuum(conn: &Connection, step_pages: u64) -> Result<()> {
    let mut remaining = pragma_u64(conn, "freelist_count")?;
    while remaining > 0 {
        conn.execute_batch(&format!("PRAGMA incremental_vacuum({step_pages});"))?;
        checkpoint_truncate(conn);
        let now = pragma_u64(conn, "freelist_count")?;
        if now >= remaining {
            break; // no progress: leave rather than spin
        }
        remaining = now;
    }
    Ok(())
}

fn open_maintenance(db_path: &Path) -> Result<Connection> {
    let conn = Connection::open(db_path)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.query_row("PRAGMA journal_mode=WAL", [], |_| Ok(()))?;
    conn.query_row(&format!("PRAGMA journal_size_limit = {WAL_SIZE_LIMIT_BYTES}"), [], |_| Ok(()))?;
    Ok(conn)
}

fn auto_vacuum(conn: &Connection) -> Result<AutoVacuum> {
    let v: i64 = conn.query_row("PRAGMA auto_vacuum", [], |r| r.get(0))?;
    AutoVacuum::from_pragma(v)
}

fn pragma_u64(conn: &Connection, name: &str) -> Result<u64> {
    let v: i64 = conn.query_row(&format!("PRAGMA {name}"), [], |r| r.get(0))?;
    Ok(v.max(0) as u64)
}

/// Best effort: a reader holding the WAL leaves the pages for the next pass.
fn checkpoint_truncate(conn: &Connection) {
    let _ = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()));
}

fn file_len(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

fn mb(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1e6)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::{Scan, ScanEntry, ScanStatus};
    use crate::store::ScanStore;
    use chrono::{DateTime, Utc};
    use std::path::PathBuf;
    use uuid::Uuid;

    fn at(iso: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(iso).unwrap().with_timezone(&Utc)
    }

    fn complete_scan(root: &str, when: &str) -> Scan {
        Scan {
            id: Uuid::new_v4(),
            root_path: root.to_string(),
            status: ScanStatus::Complete,
            started_at: at(when),
            finished_at: Some(at(when)),
            total_disk_size: 0,
            total_logical_size: 0,
            file_count: 0,
            dir_count: 0,
            error_count: 0,
            unreadable_paths: None,
            total_private_size: None,
            total_shared_size: None,
            failure_reason: None,
        }
    }

    /// `dirs` × (`files_per_dir` + 1) rows with ~90-byte paths — real pages.
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

    fn wal_bytes(db: &Path) -> u64 {
        file_len(&PathBuf::from(format!("{}-wal", db.display())))
    }

    fn mode_of(db: &Path) -> AutoVacuum {
        let c = Connection::open(db).unwrap();
        auto_vacuum(&c).unwrap()
    }

    /// A pre-1.1.1 file: schema initialised on a plain connection, so
    /// `auto_vacuum` is NONE — exactly Ted's prod database today.
    fn legacy_file(dir: &Path) -> PathBuf {
        let path = dir.join("legacy.db");
        let c = Connection::open(&path).unwrap();
        c.query_row("PRAGMA journal_mode=WAL", [], |_| Ok(())).unwrap();
        crate::schema::validate_or_init(&c).unwrap();
        drop(c);
        assert_eq!(mode_of(&path), AutoVacuum::None);
        path
    }

    /// Insert one ~5k-row scan and delete it, leaving its pages dead.
    /// Returns (bytes the scan added to the file, scan id).
    fn insert_then_delete(s: &ScanStore, path: &Path, root: &str) -> u64 {
        let before = file_len(path);
        let scan = complete_scan(root, "2026-03-17T14:30:00Z");
        s.insert_scan(&scan, &bulk_entries(root, 100, 49), &[], None).unwrap();
        let added = file_len(path) - before;
        assert!(added > 1_000_000, "the fixture must cost real pages: {added}");
        s.delete_scan(scan.id).unwrap();
        added
    }

    const PLENTY: fn(&Path) -> Result<u64> = |_| Ok(u64::MAX / 4);
    const NOTHING: fn(&Path) -> Result<u64> = |_| Ok(0);
    fn small() -> CompactionPolicy {
        CompactionPolicy { min_dead_bytes: 1, min_dead_fraction: 0.01, free_space_multiple: 2, step_pages: 200 }
    }

    #[test]
    fn new_stores_are_incremental() {
        // Mutation target: drop the auto_vacuum pragma from ScanStore::open
        // and a fresh file reads NONE — the 1.1.0 behaviour.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fresh.db");
        let _s = ScanStore::open(&path).unwrap();
        assert_eq!(mode_of(&path), AutoVacuum::Incremental);
        let mem = ScanStore::open_in_memory().unwrap();
        drop(mem); // in-memory stores accept the same open path without complaint
    }

    #[test]
    fn opening_a_legacy_file_does_not_change_its_mode() {
        // The pragma is a no-op on a populated file; conversion is compact()'s
        // decision, never a side effect of opening.
        let dir = tempfile::tempdir().unwrap();
        let path = legacy_file(dir.path());
        let _s = ScanStore::open(&path).unwrap();
        assert_eq!(mode_of(&path), AutoVacuum::None);
    }

    #[test]
    fn file_shrinks_after_pruning_an_incremental_store() {
        // Mutation target: skip incremental_vacuum (or its loop) and the
        // freelist stays, the file does not shrink, and this fails.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fresh.db");
        let s = ScanStore::open(&path).unwrap();
        let keeper = complete_scan("/keep", "2026-03-18T00:00:00Z");
        s.insert_scan(&keeper, &bulk_entries("/keep", 4, 9), &[], None).unwrap();
        let added = insert_then_delete(&s, &path, "/big");
        let bloated = file_len(&path);
        assert!(bloated > added, "deleting returned nothing by itself: {bloated}");

        let report = compact(&path, &CompactionPolicy::default(), true, &NOTHING).unwrap();
        assert_eq!(report.auto_vacuum_before, AutoVacuum::Incremental);
        assert!(!report.converted && report.refusal.is_none(), "{report:?}");
        assert!(
            report.returned_bytes() >= added * 8 / 10,
            "expected ≥ 80% of the {added} bytes back, got {} ({report:?})",
            report.returned_bytes()
        );
        assert_eq!(file_len(&path), report.file_bytes_after);
        assert_eq!(wal_bytes(&path), 0, "steps checkpoint the WAL as they go");
        assert_eq!(s.get_scan(keeper.id).unwrap().root_path, "/keep", "the survivor is intact");
        assert_eq!(s.entries(keeper.id).unwrap().len(), 40);
        // Nothing left to do: a second pass is a no-op that returns 0 bytes.
        let again = compact(&path, &CompactionPolicy::default(), false, &NOTHING).unwrap();
        assert_eq!(again.returned_bytes(), 0);
    }

    #[test]
    fn legacy_file_is_converted_once_when_worth_it_and_space_allows() {
        let dir = tempfile::tempdir().unwrap();
        let path = legacy_file(dir.path());
        let s = ScanStore::open(&path).unwrap();
        let keeper = complete_scan("/keep", "2026-03-18T00:00:00Z");
        s.insert_scan(&keeper, &bulk_entries("/keep", 4, 9), &[], None).unwrap();
        let added = insert_then_delete(&s, &path, "/big");

        let report = compact(&path, &small(), false, &PLENTY).unwrap();
        assert!(report.converted, "{report:?}");
        assert_eq!(report.auto_vacuum_before, AutoVacuum::None);
        assert_eq!(report.auto_vacuum_after, AutoVacuum::Incremental);
        assert_eq!(mode_of(&path), AutoVacuum::Incremental, "the conversion is persistent");
        assert!(report.returned_bytes() >= added * 8 / 10, "{report:?}");
        assert_eq!(wal_bytes(&path), 0, "the VACUUM image is folded and the WAL truncated");
        assert_eq!(s.entries(keeper.id).unwrap().len(), 40, "the survivor is intact");
        // From here on it is an incremental store: the next delete shrinks
        // the file without any VACUUM or guard.
        let added2 = insert_then_delete(&s, &path, "/again");
        let next = compact(&path, &CompactionPolicy::default(), true, &NOTHING).unwrap();
        assert!(!next.converted && next.refusal.is_none());
        assert!(next.returned_bytes() >= added2 * 8 / 10, "{next:?}");
    }

    #[test]
    fn vacuum_is_refused_when_the_volume_lacks_twice_the_live_size() {
        // Mutation target: drop the free-space guard and the legacy file is
        // converted — on a full disk, the operation that must not run.
        let dir = tempfile::tempdir().unwrap();
        let path = legacy_file(dir.path());
        let s = ScanStore::open(&path).unwrap();
        insert_then_delete(&s, &path, "/big");
        let before = file_len(&path);

        let report = compact(&path, &small(), false, &NOTHING).unwrap();
        assert!(!report.converted, "{report:?}");
        let why = report.refusal.as_deref().expect("a refusal with numbers");
        assert!(why.contains("available") && why.contains("2×"), "{why}");
        assert_eq!(mode_of(&path), AutoVacuum::None);
        assert_eq!(file_len(&path), before, "a refused pass changes nothing");
        assert_eq!(report.returned_bytes(), 0);
    }

    #[test]
    fn vacuum_is_refused_while_a_scan_is_running() {
        let dir = tempfile::tempdir().unwrap();
        let path = legacy_file(dir.path());
        let s = ScanStore::open(&path).unwrap();
        insert_then_delete(&s, &path, "/big");
        let report = compact(&path, &small(), true, &PLENTY).unwrap();
        assert!(!report.converted);
        assert!(report.refusal.as_deref().unwrap().contains("scan is running"), "{report:?}");
        assert_eq!(mode_of(&path), AutoVacuum::None);
    }

    #[test]
    fn vacuum_is_refused_below_the_dead_page_threshold() {
        // Production thresholds: 256 MiB AND 25 % — a few MB of dead pages
        // is not worth a rewrite of the whole file.
        let dir = tempfile::tempdir().unwrap();
        let path = legacy_file(dir.path());
        let s = ScanStore::open(&path).unwrap();
        insert_then_delete(&s, &path, "/big");
        let report = compact(&path, &CompactionPolicy::default(), false, &PLENTY).unwrap();
        assert!(!report.converted);
        assert!(report.refusal.as_deref().unwrap().contains("below the conversion threshold"), "{report:?}");
        // …and the fraction alone can refuse too: 1 byte minimum, but 99 %.
        let fraction_only = CompactionPolicy { min_dead_bytes: 1, min_dead_fraction: 0.99, ..small() };
        let report = compact(&path, &fraction_only, false, &PLENTY).unwrap();
        assert!(!report.converted, "{report:?}");
    }

    #[test]
    fn a_full_auto_vacuum_file_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("full.db");
        let c = Connection::open(&path).unwrap();
        c.execute_batch("PRAGMA auto_vacuum = FULL;").unwrap();
        crate::schema::validate_or_init(&c).unwrap();
        drop(c);
        let report = compact(&path, &small(), false, &PLENTY).unwrap();
        assert_eq!(report.auto_vacuum_before, AutoVacuum::Full);
        assert!(!report.converted && report.refusal.is_none());
    }

    #[test]
    fn a_missing_file_is_an_error_not_a_new_database() {
        // compact() must never create a database where none was.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope.db");
        let err = compact(&path, &small(), false, &PLENTY).unwrap_err();
        assert!(matches!(err, CoreError::NotFound(_)), "{err}");
        assert!(!path.exists(), "compact() must not have created a file");
    }
}
