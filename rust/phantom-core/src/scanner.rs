// Walks a directory tree and collects per-entry metadata. Pure filesystem →
// values; no persistence (that's store.rs) and no HTTP.
//
// Unreadable entries (permission errors, files that vanish mid-scan) are
// skipped and counted in `error_count` rather than failing the scan.
// Cancellation is cooperative: the walk loop checks `cancel` on every entry
// and bails with `ScanError::Cancelled`; the caller discards partial results.

use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use chrono::{DateTime, Utc};
use jwalk::WalkDir;
use serde::Serialize;

use crate::format::LinkCharger;
use crate::scan::{ScanEntry, UnreadablePath};

/// How many unreadable paths a scan records verbatim (walk order). A sample,
/// not the ledger — `error_count` stays the truth; the cap keeps a scan of a
/// dying disk from ballooning the scan row (phantom-671).
pub const UNREADABLE_SAMPLE_CAP: usize = 100;

/// Live progress counters, shared between a running scan and its observers
/// (the Phase-2 API polls this from another thread while the walk runs).
#[derive(Debug, Default)]
pub struct ScanProgress {
    files_seen: AtomicU64,
    bytes_seen: AtomicU64,
    current_path: Mutex<String>,
}

/// A point-in-time copy of the counters, in wire shape.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgressSnapshot {
    pub files_seen: u64,
    /// Disk bytes (st_blocks × 512), consistent with every other total —
    /// hardlink-deduped like them, so it converges on `totalDiskSize`.
    pub bytes_seen: u64,
    pub current_path: String,
}

impl ScanProgress {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn snapshot(&self) -> ProgressSnapshot {
        ProgressSnapshot {
            files_seen: self.files_seen.load(Ordering::Relaxed),
            bytes_seen: self.bytes_seen.load(Ordering::Relaxed),
            current_path: self.current_path.lock().unwrap().clone(),
        }
    }

    fn record(&self, path: &str, is_dir: bool, disk_size: u64) {
        if !is_dir {
            self.files_seen.fetch_add(1, Ordering::Relaxed);
            self.bytes_seen.fetch_add(disk_size, Ordering::Relaxed);
        }
        *self.current_path.lock().unwrap() = path.to_string();
    }
}

/// What a finished walk produces: the entries plus the totals the caller
/// folds into its `Scan` row. The scan's identity and lifecycle (id, status,
/// timestamps) belong to the caller, not the walker.
///
/// Totals are hardlink-deduped (an inode counts once, `LinkCharger`); the
/// entries carry TRUE per-link sizes — `classify` needs them for its naive
/// `listedDiskSize`, and downstream aggregators re-run the same charge
/// decisions over the same order.
#[derive(Debug)]
pub struct ScanOutcome {
    pub entries: Vec<ScanEntry>,
    pub total_disk_size: u64,
    pub total_logical_size: u64,
    pub file_count: u64,
    pub dir_count: u64,
    pub error_count: u64,
    /// First [`UNREADABLE_SAMPLE_CAP`] paths behind `error_count`, with the
    /// OS's reason for each.
    pub unreadable: Vec<UnreadablePath>,
}

#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    #[error("invalid path (not valid UTF-8): {0}")]
    InvalidPath(String),
    #[error("not a directory: {0}")]
    NotADirectory(String),
    #[error("scan cancelled")]
    Cancelled,
}

/// st_blocks → bytes, saturating: a hostile or corrupt stat with blocks near
/// u64::MAX must clamp to u64::MAX, not wrap to a small number — a wrapped
/// size would silently understate an entry (and in release builds `*` wraps
/// without panicking).
fn blocks_to_bytes(blocks: u64) -> u64 {
    blocks.saturating_mul(512)
}

/// Scan a directory tree, updating `progress` as it goes and checking
/// `cancel` on every entry.
pub fn scan_directory(
    root: &Path,
    progress: &ScanProgress,
    cancel: &AtomicBool,
) -> Result<ScanOutcome, ScanError> {
    let root_str = root
        .to_str()
        .ok_or_else(|| ScanError::InvalidPath(root.display().to_string()))?
        .to_string();
    if !root.is_dir() {
        return Err(ScanError::NotADirectory(root_str));
    }

    let mut entries = Vec::new();
    let mut file_count: u64 = 0;
    let mut dir_count: u64 = 0;
    let mut total_disk_size: u64 = 0;
    let mut total_logical_size: u64 = 0;
    let mut error_count: u64 = 0;
    let mut links = LinkCharger::new();
    let mut unreadable: Vec<UnreadablePath> = Vec::new();
    let record_unreadable = |unreadable: &mut Vec<UnreadablePath>, path: String, reason: String| {
        if unreadable.len() < UNREADABLE_SAMPLE_CAP {
            unreadable.push(UnreadablePath { path, reason });
        }
    };

    for entry in WalkDir::new(root).skip_hidden(false).sort(true) {
        if cancel.load(Ordering::Relaxed) {
            return Err(ScanError::Cancelled);
        }

        // Unreadable entries (permissions, vanished mid-scan) are counted,
        // not fatal. Each error site also records a capped path+reason
        // sample so 3724 errors can answer "where?" (phantom-671).
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                error_count += 1;
                let path = e
                    .path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "(unknown path)".to_string());
                record_unreadable(&mut unreadable, path, e.to_string());
                continue;
            }
        };
        // jwalk reports a directory whose CHILDREN could not be listed on
        // the directory's own (otherwise Ok) entry, not as an Err item.
        if let Some(e) = &entry.read_children_error {
            error_count += 1;
            record_unreadable(
                &mut unreadable,
                entry.path().display().to_string(),
                e.to_string(),
            );
        }
        let metadata = match entry.metadata() {
            Ok(m) => m,
            Err(e) => {
                error_count += 1;
                record_unreadable(
                    &mut unreadable,
                    entry.path().display().to_string(),
                    e.to_string(),
                );
                continue;
            }
        };

        let path = entry.path();
        let path_str = path.display().to_string();
        let name = entry.file_name().to_string_lossy().to_string();

        let is_dir = metadata.is_dir();
        let logical_size = if is_dir { 0 } else { metadata.len() };
        // st_blocks is in 512-byte units; this is actual disk usage — THE
        // size. It diverges from logical in both directions: sparse files
        // (logical ≫ disk) and dataloaded cloud files (logical ≫ disk ≈ 0).
        let disk_size = if is_dir { 0 } else { blocks_to_bytes(metadata.blocks()) };

        let file_type = if is_dir {
            None
        } else {
            path.extension()
                .and_then(|e| e.to_str())
                .map(|s| s.to_lowercase())
        };

        let modified_at: Option<DateTime<Utc>> = metadata.modified().ok().map(|t| t.into());

        // The scan root has no parent within the scan.
        let parent_path = if path_str == root_str {
            None
        } else {
            path.parent().map(|p| p.display().to_string())
        };

        // A hardlinked inode's bytes count once per scan — first link in
        // walk order wins (deterministic: the walk is sorted). file_count
        // still counts every link; it counts directory entries, not inodes.
        let counts =
            !is_dir && links.charges(metadata.nlink(), metadata.dev(), metadata.ino());
        if is_dir {
            dir_count += 1;
        } else {
            file_count += 1;
            if counts {
                total_disk_size += disk_size;
                total_logical_size += logical_size;
            }
        }
        progress.record(&path_str, is_dir, if counts { disk_size } else { 0 });

        entries.push(ScanEntry {
            path: path_str,
            parent_path,
            name,
            is_dir,
            disk_size,
            logical_size,
            modified_at,
            file_type,
            category: None,
            nlink: metadata.nlink(),
            dev: metadata.dev(),
            ino: metadata.ino(),
            file_count: None,
            dir_count: None,
        });
    }

    Ok(ScanOutcome {
        entries,
        total_disk_size,
        total_logical_size,
        file_count,
        dir_count,
        error_count,
        unreadable,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn scan(root: &Path) -> Result<ScanOutcome, ScanError> {
        scan_directory(root, &ScanProgress::new(), &AtomicBool::new(false))
    }

    #[test]
    fn scans_a_tree_and_totals_files_and_dirs() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("hello.txt"), "hello world").unwrap();
        fs::create_dir(dir.path().join("subdir")).unwrap();
        fs::write(dir.path().join("subdir/nested.rs"), "fn main() {}").unwrap();

        let out = scan(dir.path()).unwrap();
        assert_eq!(out.file_count, 2);
        assert_eq!(out.dir_count, 2, "root + subdir");
        assert_eq!(out.error_count, 0);
        assert_eq!(out.total_logical_size, 11 + 12);
        // Disk size is whole blocks, so it's at least the logical size here
        // (no sparse files in this tree) and block-aligned.
        assert!(out.total_disk_size >= out.total_logical_size);
        assert_eq!(out.total_disk_size % 512, 0);
        assert_eq!(out.entries.len(), 4);
    }

    #[test]
    fn root_has_no_parent_and_children_point_at_parents() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "a").unwrap();
        let root_str = dir.path().display().to_string();

        let out = scan(dir.path()).unwrap();
        let root = out.entries.iter().find(|e| e.path == root_str).unwrap();
        assert_eq!(root.parent_path, None, "scan root has no parent");
        let file = out.entries.iter().find(|e| e.name == "a.txt").unwrap();
        assert_eq!(file.parent_path.as_deref(), Some(root_str.as_str()));
        assert_eq!(file.file_type.as_deref(), Some("txt"));
    }

    #[test]
    fn progress_counters_match_final_totals() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.bin"), vec![0u8; 3000]).unwrap();
        fs::write(dir.path().join("b.bin"), vec![0u8; 5000]).unwrap();

        let progress = ScanProgress::new();
        let out = scan_directory(dir.path(), &progress, &AtomicBool::new(false)).unwrap();

        let snap = progress.snapshot();
        assert_eq!(snap.files_seen, out.file_count);
        assert_eq!(snap.bytes_seen, out.total_disk_size, "progress counts disk bytes");
        assert!(!snap.current_path.is_empty());
    }

    #[test]
    fn progress_snapshot_wire_shape_is_camel_case() {
        let progress = ScanProgress::new();
        progress.record("/tmp/x", false, 512);
        let v = serde_json::to_value(progress.snapshot()).unwrap();
        let obj = v.as_object().unwrap();
        assert!(obj.contains_key("filesSeen"));
        assert!(obj.contains_key("bytesSeen"));
        assert_eq!(obj["currentPath"], "/tmp/x");
    }

    #[test]
    fn cancellation_aborts_the_walk() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "a").unwrap();

        // Flag already set: the very first loop iteration must bail.
        let cancel = AtomicBool::new(true);
        let err = scan_directory(dir.path(), &ScanProgress::new(), &cancel).unwrap_err();
        assert!(matches!(err, ScanError::Cancelled));
    }

    #[test]
    fn nonexistent_root_is_not_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("no-such-dir");
        let err = scan(&missing).unwrap_err();
        assert!(matches!(err, ScanError::NotADirectory(_)));
    }

    #[test]
    fn file_root_is_not_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("plain.txt");
        fs::write(&file, "not a dir").unwrap();
        let err = scan(&file).unwrap_err();
        assert!(matches!(err, ScanError::NotADirectory(_)));
    }

    #[test]
    fn non_utf8_root_is_invalid_path() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let bad = Path::new(OsStr::from_bytes(b"/tmp/\xff\xfe"));
        let err = scan(bad).unwrap_err();
        assert!(matches!(err, ScanError::InvalidPath(_)));
    }

    #[test]
    fn hardlinked_files_share_dev_ino_and_report_nlink() {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("original.bin");
        fs::write(&original, vec![7u8; 2048]).unwrap();
        fs::hard_link(&original, dir.path().join("linked.bin")).unwrap();

        let out = scan(dir.path()).unwrap();
        let a = out.entries.iter().find(|e| e.name == "original.bin").unwrap();
        let b = out.entries.iter().find(|e| e.name == "linked.bin").unwrap();
        assert_eq!(a.nlink, 2);
        assert_eq!(b.nlink, 2);
        assert_eq!((a.dev, a.ino), (b.dev, b.ino), "hardlinks share (dev, ino)");
        // Both links are entries; both rows carry the inode's TRUE size
        // (classify's listedDiskSize depends on that).
        assert_eq!(out.file_count, 2);
        assert_eq!(a.disk_size, b.disk_size);
        assert!(a.disk_size >= 2048, "rows keep true per-link sizes");
    }

    // Mutation-proof (phantom-5ws): drop the LinkCharger guard on the totals
    // and total_disk_size doubles — this test is the one that fails. The
    // motivating case: specter/target/debug, 33.1 GB naive vs 5.2 GB real
    // across 214,877 cargo hardlinks.
    #[test]
    fn hardlinked_inode_counts_once_in_scan_totals_and_progress() {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("original.bin");
        fs::write(&original, vec![7u8; 4096]).unwrap();
        fs::hard_link(&original, dir.path().join("linked.bin")).unwrap();
        fs::hard_link(&original, dir.path().join("third.bin")).unwrap();
        // A control file proves unrelated bytes still count.
        fs::write(dir.path().join("solo.bin"), vec![1u8; 1024]).unwrap();

        let solo_scan = {
            let solo_dir = tempfile::tempdir().unwrap();
            fs::write(solo_dir.path().join("original.bin"), vec![7u8; 4096]).unwrap();
            fs::write(solo_dir.path().join("solo.bin"), vec![1u8; 1024]).unwrap();
            scan(solo_dir.path()).unwrap()
        };

        let progress = ScanProgress::new();
        let out = scan_directory(dir.path(), &progress, &AtomicBool::new(false)).unwrap();

        assert_eq!(out.file_count, 4, "every link is still an entry");
        assert_eq!(
            out.total_disk_size, solo_scan.total_disk_size,
            "three links to one inode must total the same as one copy"
        );
        assert_eq!(out.total_logical_size, solo_scan.total_logical_size);
        assert_eq!(
            progress.snapshot().bytes_seen,
            out.total_disk_size,
            "live progress converges on the deduped total"
        );
    }

    // Mutation-proof: revert saturating_mul to `*` and this wraps to
    // 18446744073709551104 % 2^64 = u64::MAX - 511 … actually panics in
    // debug and wraps in release — either way the equality below fails.
    #[test]
    fn block_conversion_saturates_instead_of_wrapping() {
        assert_eq!(blocks_to_bytes(u64::MAX), u64::MAX, "clamp, never wrap");
        assert_eq!(blocks_to_bytes(0), 0);
        assert_eq!(blocks_to_bytes(8), 4096, "the normal case is exact");
    }

    /// Symlink behavior pin (safety review): jwalk's follow_links defaults
    /// to FALSE and the walker relies on that default. This test protects
    /// against a jwalk upgrade (or a "helpful" .follow_links(true)) flipping
    /// it: a symlink LOOP must terminate, and a symlink escaping the scan
    /// root must not pull the target's bytes into the totals.
    #[test]
    fn symlinks_are_not_followed_loops_terminate_escapes_do_not_count() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        // outside.bin lives OUTSIDE the scan root; 1 MiB of real blocks.
        let outside = dir.path().join("outside.bin");
        fs::write(&outside, vec![7u8; 1024 * 1024]).unwrap();
        let root = dir.path().join("root");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("real.txt"), "real bytes").unwrap();
        // (a) a loop: root/loop → root itself. Followed, this never ends.
        symlink(&root, root.join("loop")).unwrap();
        // (b) an escape: root/escape → the outside file.
        symlink(&outside, root.join("escape")).unwrap();

        // Terminating AT ALL is the loop assertion.
        let out = scan(&root).unwrap();

        assert_eq!(out.error_count, 0, "symlinks are entries, not errors");
        // The escape target's megabyte is NOT in the totals: symlinks are
        // recorded as their own ~0-block lstat selves.
        assert!(
            out.total_disk_size < 1024 * 1024,
            "escape target's bytes must not count: {}",
            out.total_disk_size
        );
        let escape = out.entries.iter().find(|e| e.name == "escape").unwrap();
        assert!(!escape.is_dir, "unfollowed symlink is not a directory");
        assert!(
            escape.disk_size < 4096,
            "symlink occupies ~0 blocks, got {}",
            escape.disk_size
        );
        let loop_link = out.entries.iter().find(|e| e.name == "loop").unwrap();
        assert!(!loop_link.is_dir, "a dir symlink unfollowed is still not a dir");
        // Exactly the four expected entries — the loop was not descended.
        assert_eq!(out.entries.len(), 4, "root + real.txt + loop + escape: {:?}",
            out.entries.iter().map(|e| &e.name).collect::<Vec<_>>());
    }

    #[test]
    fn unreadable_subdir_is_counted_not_fatal() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("ok.txt"), "fine").unwrap();
        let locked = dir.path().join("locked");
        fs::create_dir(&locked).unwrap();
        fs::write(locked.join("hidden.txt"), "unreachable").unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

        let out = scan(dir.path());
        // Restore before asserting so tempdir cleanup works even on failure.
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

        let out = out.unwrap();
        assert!(out.error_count >= 1, "unreadable dir must be counted");
        assert!(out.entries.iter().any(|e| e.name == "ok.txt"));
        assert!(
            !out.entries.iter().any(|e| e.name == "hidden.txt"),
            "children of an unreadable dir are unreachable"
        );
        // phantom-671: the count comes with a sample naming WHERE and WHY.
        // Mutation-proof: drop any record_unreadable call and this fails.
        assert_eq!(out.unreadable.len() as u64, out.error_count);
        let locked_str = locked.display().to_string();
        let hit = out
            .unreadable
            .iter()
            .find(|u| u.path == locked_str)
            .expect("the locked dir must appear in the sample");
        assert!(
            hit.reason.to_lowercase().contains("permission")
                || hit.reason.contains("os error"),
            "reason carries the OS error: {}",
            hit.reason
        );
    }

    #[test]
    fn unreadable_sample_is_capped_but_count_is_not() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let n = UNREADABLE_SAMPLE_CAP + 7;
        let mut locked = Vec::new();
        for i in 0..n {
            let d = dir.path().join(format!("locked-{i:04}"));
            fs::create_dir(&d).unwrap();
            fs::set_permissions(&d, fs::Permissions::from_mode(0o000)).unwrap();
            locked.push(d);
        }

        let out = scan(dir.path());
        for d in &locked {
            fs::set_permissions(d, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let out = out.unwrap();
        assert_eq!(out.error_count, n as u64, "every error is counted");
        assert_eq!(
            out.unreadable.len(),
            UNREADABLE_SAMPLE_CAP,
            "the sample stops at the cap"
        );
    }
}
