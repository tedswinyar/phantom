// Backup conventions (docs/data-safety.md): a backup is not a backup until
// it has been restored somewhere else and read. `backup_verified` therefore
// copies AND re-opens the copy out-of-place, validating schema and row count
// before reporting success.

use std::path::Path;

use rusqlite::Connection;

use crate::{CoreError, Result, ScanStore, schema};

/// Snapshot the store to `dest`, then verify the snapshot by opening it
/// out-of-place and comparing scan row counts. Returns the verified count.
pub fn backup_verified(store: &ScanStore, dest: &Path) -> Result<usize> {
    if dest.exists() {
        return Err(CoreError::InvalidInput(format!(
            "backup destination already exists: {}",
            dest.display()
        )));
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let src = store.connection();
    let mut dst = Connection::open(dest)?;
    let backup = rusqlite::backup::Backup::new(src, &mut dst)?;
    backup.run_to_completion(5, std::time::Duration::from_millis(50), None)?;
    drop(backup);
    drop(dst);

    // Out-of-place verification: fresh connection, schema check, row count.
    let verify = Connection::open(dest)?;
    schema::validate_or_init(&verify)?;
    let backed_up: usize =
        verify.query_row("SELECT COUNT(*) FROM scans", [], |r| r.get(0))?;
    let original: usize = src.query_row("SELECT COUNT(*) FROM scans", [], |r| r.get(0))?;
    if backed_up != original {
        return Err(CoreError::Schema(format!(
            "backup row count {backed_up} != source {original}; backup at {} is \
             not trustworthy",
            dest.display()
        )));
    }
    Ok(backed_up)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Scan;

    fn store_with_scans(dir: &Path, count: usize) -> ScanStore {
        let store = ScanStore::open(&dir.join("live.db")).unwrap();
        for i in 0..count {
            let scan = Scan::new(format!("/haunt/{i}"));
            store.insert_scan(&scan, &[], &[], None).unwrap();
        }
        store
    }

    #[test]
    fn backup_copies_all_rows_and_verifies() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with_scans(dir.path(), 2);

        let dest = dir.path().join("backups/snap.db");
        let count = backup_verified(&store, &dest).unwrap();
        assert_eq!(count, 2);

        // The backup is independently openable and complete.
        let restored = ScanStore::open(&dest).unwrap();
        assert_eq!(restored.list_scans().unwrap().len(), 2);
    }

    #[test]
    fn refuses_to_overwrite_existing_backup() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with_scans(dir.path(), 0);
        let dest = dir.path().join("snap.db");
        std::fs::write(&dest, b"precious").unwrap();
        let err = backup_verified(&store, &dest).unwrap_err();
        assert!(matches!(err, CoreError::InvalidInput(_)));
        // And the existing file is untouched.
        assert_eq!(std::fs::read(&dest).unwrap(), b"precious");
    }

    #[test]
    fn backup_of_empty_store_verifies_at_zero() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with_scans(dir.path(), 0);
        let count = backup_verified(&store, &dir.path().join("snap.db")).unwrap();
        assert_eq!(count, 0);
    }

    /// THE RESTORE DRILL (docs/data-safety.md, launch checklist): a backup is
    /// not a backup until it has been restored somewhere else and READ. This
    /// backs up a populated store, restores the copy out-of-place, and reads
    /// every table's content back — scans, entries (categories included),
    /// type totals, and the hotspots summary decoded from its stored JSON.
    ///
    /// Run before each release:
    ///   cargo test -p phantom-core restore_drill -- --ignored --nocapture
    ///
    /// Set PHANTOM_DRILL_DB=/path/to/a/STOPPED/copy.db to drill against a
    /// real database instead of the synthetic one (never point it at a live
    /// profile — opening it adds a second writer and may migrate it).
    #[test]
    #[ignore = "release-checklist drill — run with --ignored --nocapture"]
    fn restore_drill_backup_restores_and_reads_back() {
        use crate::{HotspotsSummary, ScanEntry};

        let dir = tempfile::tempdir().unwrap();

        let (store, expected_scans) = match std::env::var("PHANTOM_DRILL_DB") {
            Ok(path) => {
                let store = ScanStore::open(Path::new(&path)).unwrap();
                let n = store.list_scans().unwrap().len();
                println!("drill: source {path} ({n} scans)");
                (store, n)
            }
            Err(_) => {
                // Synthetic: one scan carrying every kind of row the schema
                // holds, so the restore proves more than a row count.
                let store = ScanStore::open(&dir.path().join("live.db")).unwrap();
                let mut scan = Scan::new("/drill");
                scan.status = crate::ScanStatus::Complete;
                scan.finished_at = Some(scan.started_at);
                let summary: HotspotsSummary = serde_json::from_str(include_str!(
                    "../../../tests/fixtures/hotspots-summary.json"
                ))
                .unwrap();
                let entry: ScanEntry = serde_json::from_str(include_str!(
                    "../../../tests/fixtures/entry-dir.json"
                ))
                .unwrap();
                let totals = vec![crate::FileTypeTotal {
                    file_type: Some("rs".into()),
                    disk_size: 4096,
                    file_count: 1,
                }];
                store
                    .insert_scan(&scan, &[entry], &totals, Some(&summary))
                    .unwrap();
                println!("drill: synthetic source (1 scan, 1 entry, 1 type total, hotspots)");
                (store, 1)
            }
        };

        let dest = dir.path().join("restore/backup.db");
        let verified = backup_verified(&store, &dest).unwrap();
        assert_eq!(verified, expected_scans, "backup_verified count");
        drop(store); // the restore must not lean on the source

        // Restore = open the copy as a fresh store (the temp-profile step of
        // the data-safety drill) and READ everything.
        let restored = ScanStore::open(&dest).unwrap();
        let scans = restored.list_scans().unwrap();
        assert_eq!(scans.len(), expected_scans);
        let mut entries = 0usize;
        let mut hotspot_summaries = 0usize;
        for s in &scans {
            entries += restored.entries(s.id).unwrap().len();
            restored.file_type_totals(s.id).unwrap();
            if restored.hotspots(s.id).unwrap().is_some() {
                hotspot_summaries += 1;
            }
        }
        println!(
            "drill: RESTORED and read back {} scans, {} entries, {} hotspot summaries — PASS",
            scans.len(),
            entries,
            hotspot_summaries
        );
    }
}
