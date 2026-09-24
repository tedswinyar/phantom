// Walks a directory tree and collects per-entry metadata. Pure filesystem →
// values; no persistence (that's store.rs) and no HTTP.
//
// The walk is one `getattrlistbulk(2)` call per directory batch (bulk.rs),
// not one lstat per entry: measured 3–6× faster than the jwalk+lstat walk
// it replaced and, more importantly, the only way to read the APFS clone
// attributes that make `privateSize` exact (phantom-mkn.1; benchmark table
// in the epic's notes). Depth-first, siblings sorted by name, a directory's
// children all recorded before any of them is descended — deterministic, so
// "first reference in walk order" is a stable rule.
//
// Unreadable entries (permission errors, files that vanish mid-scan) are
// skipped and counted in `error_count` rather than failing the scan.
// Cancellation is cooperative: the walk loop checks `cancel` on every entry
// and bails with `ScanError::Cancelled`; the caller discards partial results.
//
// One filesystem by default (phantom-jsz): a directory that is a MOUNT POINT
// for another volume is recorded but not descended unless the scan asked to
// cross volumes. Firmlinks ARE followed: `/Users` on the sealed system
// volume is the OS's own path to the data volume, and st_dev is unified
// across the APFS volume group anyway — the data volume's mount point
// (`/System/Volumes/Data`) is what would double-count it, and that is a
// mount point, so scanning `/` counts every byte once.

use std::collections::HashMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::bulk::{self, BulkEntry, ObjType};
use crate::format::LinkCharger;
use crate::scan::{EntryFlags, ScanEntry, UnreadablePath};
use crate::share::ShareLedger;

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

/// Walk policy knobs. Mirrors the `ScanRequest` wire fields.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanOptions {
    /// Descend into mount points of other volumes. Default false (`du -x`).
    pub cross_volumes: bool,
}

/// What a finished walk produces: the entries plus the totals the caller
/// folds into its `Scan` row. The scan's identity and lifecycle (id, status,
/// timestamps) belong to the caller, not the walker.
///
/// Totals count every sharing group once (hardlinked inode OR pure-clone
/// stream, `LinkCharger`); the entries carry TRUE per-reference sizes —
/// `classify` needs them for its naive `listedDiskSize`, and downstream
/// aggregators re-run the same charge decisions over the same order.
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
    /// Every sharing group the walk saw, with the reference counts the
    /// filesystem reported — what the persistence rollup and the classifier
    /// need to decide whether deleting a subtree frees a group's bytes.
    pub shares: ShareLedger,
}

#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    #[error("invalid path (not valid UTF-8): {0}")]
    InvalidPath(String),
    #[error("not a directory: {0}")]
    NotADirectory(String),
    #[error("scan cancelled")]
    Cancelled,
    /// The root was removed or replaced while the walk ran (phantom-4x5).
    /// Before this the walk "completed" with whatever it had listed and a
    /// few ENOENT errors — a garbage partial wearing the complete badge.
    #[error("scan root disappeared during the walk: {0}")]
    RootVanished(String),
}

/// Same directory as when the walk began: it exists, is a directory, and
/// has the identity (`dev`, `ino`) recorded at the start — a root removed
/// and recreated under the same name is NOT the same root.
pub fn root_still_present(root: &Path, started_as: &std::fs::Metadata) -> bool {
    match std::fs::metadata(root) {
        Ok(md) => md.is_dir() && md.dev() == started_as.dev() && md.ino() == started_as.ino(),
        Err(_) => false,
    }
}

/// Scan a directory tree with default options (one filesystem).
pub fn scan_directory(
    root: &Path,
    progress: &ScanProgress,
    cancel: &AtomicBool,
) -> Result<ScanOutcome, ScanError> {
    scan_directory_with(root, ScanOptions::default(), progress, cancel)
}

/// The walker's per-entry flags, from the raw filesystem bits.
pub(crate) fn entry_flags(e: &BulkEntry) -> EntryFlags {
    let mut f = EntryFlags::empty();
    if e.ext(bulk::EF_MAY_SHARE_BLOCKS) {
        f.insert(EntryFlags::MAY_SHARE_BLOCKS);
    }
    if e.ext(bulk::EF_SHARES_ALL_BLOCKS) {
        f.insert(EntryFlags::SHARES_ALL_BLOCKS);
    }
    if e.ext(bulk::EF_IS_PURGEABLE) {
        f.insert(EntryFlags::PURGEABLE);
    }
    if e.ext(bulk::EF_IS_SPARSE) {
        f.insert(EntryFlags::SPARSE);
    }
    if e.is_dataless() {
        f.insert(EntryFlags::DATALESS);
    }
    if e.is_firmlink() {
        f.insert(EntryFlags::FIRMLINK);
    }
    if e.is_mount_point {
        f.insert(EntryFlags::MOUNT_POINT);
    }
    if e.is_compressed() {
        f.insert(EntryFlags::COMPRESSED);
    }
    f
}

/// The one-filesystem rule: a directory is descended unless it is a mount
/// point for another volume and the scan did not ask to cross.
pub(crate) fn descends(is_dir: bool, is_mount_point: bool, options: ScanOptions) -> bool {
    is_dir && (options.cross_volumes || !is_mount_point)
}

/// One listed directory, as a worker produced it: its children in name
/// order plus what the worker learned about each (the filesystem's clone
/// reference count, and whether the walk descends into it).
struct Listed {
    children: Vec<Child>,
}

struct Child {
    entry: ScanEntry,
    clone_refcnt: Option<u64>,
    descend: bool,
}

/// Work-queue state shared by the listing workers.
struct Queue {
    pending: Vec<PathBuf>,
    in_flight: usize,
}

/// Cap on listing threads: APFS metadata reads parallelize well up to the
/// core count; beyond that the syscalls queue on the same volume.
const MAX_WORKERS: usize = 16;

/// Scan a directory tree, updating `progress` as it goes and checking
/// `cancel` on every directory.
///
/// Two phases. LISTING is parallel: a pool of threads drains a directory
/// queue, one `getattrlistbulk` sweep per directory, pushing subdirectories
/// back onto the queue (156k directories under ~/Code took 235 s serially
/// and 60 s this way — the parallelism is the whole win over jwalk, whose
/// per-entry lstat this replaces). ASSEMBLY is serial and deterministic:
/// the listings are stitched together depth-first from the root, siblings
/// in name order, a directory's children before any subdirectory's — so
/// "first reference in walk order" means the same thing on every run and
/// every downstream aggregator re-derives the same charges.
///
/// Live progress: `filesSeen` is exact; `bytesSeen` dedupes sharing groups
/// through a shared set as workers go, so it converges on `totalDiskSize`
/// (exactly equal at completion; which member of a group happened to be
/// counted live is timing-dependent and irrelevant to the total).
pub fn scan_directory_with(
    root: &Path,
    options: ScanOptions,
    progress: &ScanProgress,
    cancel: &AtomicBool,
) -> Result<ScanOutcome, ScanError> {
    let root_str = root
        .to_str()
        .ok_or_else(|| ScanError::InvalidPath(root.display().to_string()))?
        .to_string();
    // Follows a symlinked root, like `is_dir()` always did; children are
    // never followed (bulk lists the link itself).
    let root_md = match std::fs::metadata(root) {
        Ok(md) if md.is_dir() => md,
        _ => return Err(ScanError::NotADirectory(root_str)),
    };
    if cancel.load(Ordering::Relaxed) {
        return Err(ScanError::Cancelled);
    }

    // The scan root: recorded from stat (bulk lists children, not the
    // directory itself); it has no parent within the scan.
    progress.record(&root_str, true, 0);
    let root_entry = ScanEntry {
        path: root_str.clone(),
        parent_path: None,
        name: root
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| root_str.clone()),
        is_dir: true,
        disk_size: 0,
        logical_size: 0,
        modified_at: root_md.modified().ok().map(|t| t.into()),
        file_type: None,
        category: None,
        nlink: root_md.nlink(),
        dev: root_md.dev(),
        ino: root_md.ino(),
        file_count: None,
        dir_count: None,
        private_size: Some(0),
        shared_size: Some(0),
        clone_id: None,
        flags: Some(EntryFlags::empty()),
    };

    // --- Phase 1: parallel listing ------------------------------------
    let queue = Mutex::new(Queue {
        pending: vec![root.to_path_buf()],
        in_flight: 0,
    });
    let wake = std::sync::Condvar::new();
    let listings: Mutex<HashMap<PathBuf, Listed>> = Mutex::new(HashMap::new());
    let error_count = AtomicU64::new(0);
    let unreadable: Mutex<Vec<UnreadablePath>> = Mutex::new(Vec::new());
    let live_links: Mutex<LinkCharger> = Mutex::new(LinkCharger::new());
    let record_unreadable = |path: String, reason: String| {
        error_count.fetch_add(1, Ordering::Relaxed);
        let mut sample = unreadable.lock().unwrap();
        if sample.len() < UNREADABLE_SAMPLE_CAP {
            sample.push(UnreadablePath { path, reason });
        }
    };

    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(1, MAX_WORKERS);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    // Claim a directory, or wait for one, or finish when
                    // nothing is pending and nobody is still listing.
                    let dir = {
                        let mut q = queue.lock().unwrap();
                        loop {
                            if cancel.load(Ordering::Relaxed) {
                                return;
                            }
                            if let Some(d) = q.pending.pop() {
                                q.in_flight += 1;
                                break d;
                            }
                            if q.in_flight == 0 {
                                return;
                            }
                            q = wake.wait(q).unwrap();
                        }
                    };
                    let is_root = dir == root;
                    let listed = if is_root {
                        bulk::read_dir_bulk_following(&dir)
                    } else {
                        bulk::read_dir_bulk(&dir)
                    };
                    let mut subdirs = Vec::new();
                    match listed {
                        // A directory whose children cannot be listed is
                        // counted on the directory itself (its own row
                        // already exists); its subtree is simply absent.
                        // Each error site records a capped path+reason
                        // sample so 3724 errors can answer "where?"
                        // (phantom-671).
                        Err(e) => record_unreadable(dir.display().to_string(), e.to_string()),
                        Ok(mut raw) => {
                            raw.sort_by(|a, b| a.name.cmp(&b.name));
                            let mut children = Vec::with_capacity(raw.len());
                            for e in raw {
                                let path = dir.join(&e.name);
                                // A per-entry error from the kernel: the
                                // other fields are not to be trusted.
                                // Counted, sampled, skipped.
                                if e.error != 0 {
                                    let reason =
                                        std::io::Error::from_raw_os_error(e.error as i32).to_string();
                                    record_unreadable(path.display().to_string(), reason);
                                    continue;
                                }
                                let child = child_of(&dir, path, &e, options);
                                if child.descend {
                                    subdirs.push(PathBuf::from(&child.entry.path));
                                }
                                let counted = if child.entry.is_dir {
                                    0
                                } else if live_links.lock().unwrap().charges_entry(&child.entry) {
                                    child.entry.disk_size
                                } else {
                                    0
                                };
                                progress.record(&child.entry.path, child.entry.is_dir, counted);
                                children.push(child);
                            }
                            listings.lock().unwrap().insert(dir.clone(), Listed { children });
                        }
                    }
                    let mut q = queue.lock().unwrap();
                    q.pending.extend(subdirs);
                    q.in_flight -= 1;
                    wake.notify_all();
                }
            });
        }
    });
    if cancel.load(Ordering::Relaxed) {
        return Err(ScanError::Cancelled);
    }
    // The root must still be the directory we started on. A tree deleted
    // (or deleted and recreated) mid-walk shows up as ENOENT on the
    // directories not yet listed, which the loop above counts as ordinary
    // unreadable entries — that is right for a subtree and wrong for the
    // root: the totals would describe nothing that exists.
    if !root_still_present(root, &root_md) {
        return Err(ScanError::RootVanished(root_str));
    }

    // --- Phase 2: deterministic assembly -------------------------------
    let mut listings = listings.into_inner().unwrap();
    let mut entries = Vec::with_capacity(listings.values().map(|l| l.children.len()).sum::<usize>() + 1);
    let mut file_count: u64 = 0;
    let mut dir_count: u64 = 1;
    let mut total_disk_size: u64 = 0;
    let mut total_logical_size: u64 = 0;
    let mut links = LinkCharger::new();
    let mut shares = ShareLedger::new();
    entries.push(root_entry);
    // LIFO of directories to stitch in; a directory's subdirectories are
    // pushed in REVERSE name order so the first by name is popped first.
    let mut pending: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Some(listed) = listings.remove(&dir) else { continue }; // unreadable
        let mut subdirs = Vec::new();
        for child in listed.children {
            let Child { entry, clone_refcnt, descend } = child;
            if entry.is_dir {
                dir_count += 1;
            } else {
                file_count += 1;
                shares.observe(&entry, clone_refcnt);
                // A sharing group's bytes count once per scan — first
                // reference in walk order wins. file_count still counts
                // every reference; it counts directory entries, not inodes.
                if links.charges_entry(&entry) {
                    total_disk_size += entry.disk_size;
                    total_logical_size += entry.logical_size;
                }
            }
            if descend {
                subdirs.push(PathBuf::from(&entry.path));
            }
            entries.push(entry);
        }
        pending.extend(subdirs.into_iter().rev());
    }
    // Live counters end exactly on the deduped totals whichever member of
    // a group the workers happened to count.
    progress.bytes_seen.store(total_disk_size, Ordering::Relaxed);

    Ok(ScanOutcome {
        entries,
        total_disk_size,
        total_logical_size,
        file_count,
        dir_count,
        error_count: error_count.into_inner(),
        unreadable: unreadable.into_inner().unwrap(),
        shares,
    })
}

/// Turn one bulk entry into its `ScanEntry` plus the walker's decisions.
fn child_of(dir: &Path, path: PathBuf, e: &BulkEntry, options: ScanOptions) -> Child {
    let path_str = path.display().to_string();
    let is_dir = e.obj_type == ObjType::Directory;
    // ALLOCSIZE is actual disk usage — THE size (st_blocks × 512 on APFS,
    // byte-identical: the G1 gate). It diverges from logical in both
    // directions: sparse files (logical ≫ disk) and dataloaded cloud files
    // (logical ≫ disk ≈ 0).
    let disk_size = if is_dir { 0 } else { e.alloc_size };
    let logical_size = if is_dir { 0 } else { e.logical_size };
    // A pure clone (shares ALL blocks with ≥1 other file) joins a clone
    // group keyed by the stream id; the group counts once.
    let clone_id = if is_dir {
        None
    } else {
        e.clone_id.filter(|_| e.clone_refcnt.is_some_and(|c| c > 1))
    };
    // What deleting THIS path frees: nothing while other hard links exist;
    // otherwise the kernel's private bytes (0 for a pure clone or a
    // snapshot-trapped file; the rewritten blocks for a modified clone);
    // the allocation when the filesystem cannot say — which includes a
    // decmpfs-compressed file, whose clone attributes describe an empty
    // data fork (PRIVATESIZE 0, refcnt 0) while the bytes sit in the
    // resource fork.
    let private_size = if is_dir || e.nlink > 1 {
        0
    } else if e.is_compressed() {
        e.alloc_size
    } else {
        e.private_size.unwrap_or(e.alloc_size).min(e.alloc_size)
    };
    let shared_size = disk_size - private_size;
    let file_type = if is_dir {
        None
    } else {
        path.extension()
            .and_then(|x| x.to_str())
            .map(|s| s.to_lowercase())
    };
    let modified_at: Option<DateTime<Utc>> = e.modified_at.map(|t| t.into());
    Child {
        descend: descends(is_dir, e.is_mount_point, options),
        clone_refcnt: e.clone_refcnt.map(u64::from),
        entry: ScanEntry {
            path: path_str,
            parent_path: Some(dir.display().to_string()),
            name: e.name.to_string_lossy().to_string(),
            is_dir,
            disk_size,
            logical_size,
            modified_at,
            file_type,
            category: None,
            nlink: e.nlink,
            dev: e.dev,
            ino: e.ino,
            file_count: None,
            dir_count: None,
            private_size: Some(private_size),
            shared_size: Some(shared_size),
            clone_id,
            flags: Some(entry_flags(e)),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn scan(root: &Path) -> Result<ScanOutcome, ScanError> {
        scan_directory(root, &ScanProgress::new(), &AtomicBool::new(false))
    }

    fn clone_file(src: &Path, dst: &Path) {
        let rc = std::process::Command::new("cp")
            .arg("-c")
            .arg(src)
            .arg(dst)
            .status()
            .unwrap();
        assert!(rc.success(), "cp -c must succeed on APFS");
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

    /// The walk order contract: depth-first, siblings sorted by name, a
    /// directory's children all recorded before any subdirectory's.
    #[test]
    fn walk_order_is_depth_first_with_sorted_siblings_grouped() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("b")).unwrap();
        fs::create_dir(dir.path().join("a")).unwrap();
        fs::write(dir.path().join("z.txt"), "z").unwrap();
        fs::write(dir.path().join("a/inner.txt"), "i").unwrap();
        fs::write(dir.path().join("b/inner.txt"), "i").unwrap();

        let out = scan(dir.path()).unwrap();
        let names: Vec<&str> = out.entries.iter().map(|e| e.name.as_str()).collect();
        let root_name = dir.path().file_name().unwrap().to_str().unwrap();
        assert_eq!(
            names,
            vec![root_name, "a", "b", "z.txt", "inner.txt", "inner.txt"]
        );
        assert_eq!(
            out.entries[4].parent_path.as_deref(),
            Some(dir.path().join("a").to_str().unwrap()),
            "a/ is descended before b/"
        );
    }

    #[test]
    fn root_has_no_parent_and_children_point_at_parents() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "a").unwrap();
        let root_str = dir.path().display().to_string();

        let out = scan(dir.path()).unwrap();
        let root = out.entries.iter().find(|e| e.path == root_str).unwrap();
        assert_eq!(root.parent_path, None, "scan root has no parent");
        assert!(root.is_dir);
        assert!(root.modified_at.is_some(), "the root carries its own mtime");
        let file = out.entries.iter().find(|e| e.name == "a.txt").unwrap();
        assert_eq!(file.parent_path.as_deref(), Some(root_str.as_str()));
        assert_eq!(file.file_type.as_deref(), Some("txt"));
    }

    /// Byte-for-byte agreement with lstat on the fields both can see —
    /// the G1 gate at the unit level (the benchmark example ran it on the
    /// e2e fixture tree, ~/Code and a 50k-file directory).
    #[test]
    fn entry_fields_match_lstat() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.bin"), vec![7u8; 5000]).unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub/b.bin"), vec![1u8; 70_000]).unwrap();
        let out = scan(dir.path()).unwrap();
        for e in &out.entries {
            let md = fs::symlink_metadata(&e.path).unwrap();
            assert_eq!(e.is_dir, md.is_dir(), "{}", e.path);
            assert_eq!((e.dev, e.ino), (md.dev(), md.ino()), "{}", e.path);
            if !e.is_dir {
                // (dir nlink is APFS's 1, not st_nlink's subdir count —
                // nothing reads it.)
                assert_eq!(e.nlink, md.nlink(), "{}", e.path);
            }
            let mtime: DateTime<Utc> = md.modified().unwrap().into();
            assert_eq!(e.modified_at, Some(mtime), "{}", e.path);
            if !e.is_dir {
                assert_eq!(e.disk_size, md.blocks() * 512, "{}", e.path);
                assert_eq!(e.logical_size, md.len(), "{}", e.path);
            }
        }
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
    fn root_identity_check_sees_removal_and_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir(&root).unwrap();
        let started = std::fs::metadata(&root).unwrap();
        assert!(root_still_present(&root, &started), "untouched root is present");
        // Replaced under the same name: a different inode is a different root.
        std::fs::remove_dir(&root).unwrap();
        std::fs::create_dir(&root).unwrap();
        assert!(!root_still_present(&root, &started), "recreated root is not the one we started on");
        // Removed: gone.
        std::fs::remove_dir(&root).unwrap();
        assert!(!root_still_present(&root, &started));
        // A file where the directory was.
        std::fs::write(&root, b"x").unwrap();
        assert!(!root_still_present(&root, &started));
    }

    /// The root is renamed away as soon as the walk has listed something.
    /// Every directory still queued fails with ENOENT — counted as
    /// unreadable, as before — but the scan must NOT complete: the totals
    /// would describe a tree that no longer exists at that path.
    #[test]
    fn a_root_that_vanishes_mid_walk_fails_the_scan() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        // Wide and deep enough that the walk is still running when the
        // rename lands (the rename fires at the first progress tick).
        for a in 0..60 {
            for b in 0..20 {
                let d = root.join(format!("a{a}")).join(format!("b{b}"));
                std::fs::create_dir_all(&d).unwrap();
                for c in 0..5 {
                    std::fs::write(d.join(format!("f{c}")), b"x").unwrap();
                }
            }
        }
        let progress = ScanProgress::new();
        let cancel = AtomicBool::new(false);
        let moved = dir.path().join("moved-away");
        let result = std::thread::scope(|scope| {
            scope.spawn(|| {
                while progress.files_seen.load(Ordering::Relaxed) == 0 {
                    std::thread::yield_now();
                }
                std::fs::rename(&root, &moved).unwrap();
            });
            scan_directory(&root, &progress, &cancel)
        });
        match result {
            Err(ScanError::RootVanished(p)) => assert_eq!(p, root.to_str().unwrap()),
            other => panic!("want RootVanished, got {other:?}"),
        }
        assert_eq!(
            ScanError::RootVanished("/r".into()).to_string(),
            "scan root disappeared during the walk: /r",
            "the wire's failureReason text"
        );
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

    /// A root given through a symlink is followed (the user named it);
    /// children are recorded, not the link.
    #[test]
    fn symlinked_root_is_followed() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        fs::create_dir(&real).unwrap();
        fs::write(real.join("x.txt"), "x").unwrap();
        let alias = dir.path().join("alias");
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let out = scan(&alias).unwrap();
        assert_eq!(out.file_count, 1);
        assert_eq!(out.error_count, 0);
        assert!(out.entries.iter().any(|e| e.name == "x.txt"));
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
        // Deleting ONE link frees nothing: the blocks are all shared.
        assert_eq!(a.private_size, Some(0));
        assert_eq!(a.shared_size, Some(a.disk_size));
        assert_eq!(a.clone_id, None, "a hard link is not a clone");
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

    // Mutation-proof (phantom-mkn.1, the plan's mutation #1): make
    // `ChargeKey::of` ignore `clone_id` and the cloned pair totals 2 MiB
    // where one copy totals 1 MiB — this is the test that fails. st_blocks
    // reports the full allocation on every clone; only the clone group
    // knows better.
    #[test]
    fn cloned_files_count_once_in_scan_totals() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.bin");
        fs::write(&a, vec![9u8; 1 << 20]).unwrap();
        clone_file(&a, &dir.path().join("b.bin"));
        clone_file(&a, &dir.path().join("c.bin"));
        fs::write(dir.path().join("solo.bin"), vec![1u8; 4096]).unwrap();

        let solo_scan = {
            let solo_dir = tempfile::tempdir().unwrap();
            fs::write(solo_dir.path().join("a.bin"), vec![9u8; 1 << 20]).unwrap();
            fs::write(solo_dir.path().join("solo.bin"), vec![1u8; 4096]).unwrap();
            scan(solo_dir.path()).unwrap()
        };

        let progress = ScanProgress::new();
        let out = scan_directory(dir.path(), &progress, &AtomicBool::new(false)).unwrap();
        assert_eq!(out.file_count, 4, "every clone is still an entry");
        assert_eq!(
            out.total_disk_size, solo_scan.total_disk_size,
            "three pure clones of one stream must total the same as one copy"
        );
        assert_eq!(progress.snapshot().bytes_seen, out.total_disk_size);

        let a = out.entries.iter().find(|e| e.name == "a.bin").unwrap();
        let b = out.entries.iter().find(|e| e.name == "b.bin").unwrap();
        let solo = out.entries.iter().find(|e| e.name == "solo.bin").unwrap();
        assert!(a.clone_id.is_some(), "members carry the group id");
        assert_eq!(a.clone_id, b.clone_id);
        assert_ne!(a.ino, b.ino, "clones are distinct inodes (not hard links)");
        assert_eq!(a.disk_size, b.disk_size, "rows keep the full allocation (st_blocks)");
        assert_eq!(a.private_size, Some(0), "deleting one pure clone frees nothing");
        assert_eq!(a.shared_size, Some(a.disk_size));
        let flags = a.flags.unwrap();
        assert!(flags.contains(EntryFlags::MAY_SHARE_BLOCKS | EntryFlags::SHARES_ALL_BLOCKS));
        assert_eq!(solo.clone_id, None);
        assert_eq!(solo.private_size, Some(solo.disk_size), "an ordinary file frees itself");
        assert_eq!(solo.shared_size, Some(0));
        assert!(solo.flags.unwrap().is_empty(), "{:?}", solo.flags);
    }

    /// A clone that was written to is no longer a pure clone: its own
    /// stream id, no group, and `privateSize` is exactly its rewritten
    /// blocks — it still counts at full allocation (nothing per-file says
    /// who else holds its extents; the shared portion reads as shared).
    #[test]
    fn modified_clone_reports_only_its_private_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.bin");
        fs::write(&a, vec![9u8; 4 << 20]).unwrap();
        let c = dir.path().join("c.bin");
        clone_file(&a, &c);
        // Rewrite one byte at the start: one block becomes private to c.
        {
            use std::io::{Seek, SeekFrom, Write};
            let mut f = fs::OpenOptions::new().write(true).open(&c).unwrap();
            f.seek(SeekFrom::Start(0)).unwrap();
            f.write_all(b"x").unwrap();
            // Flush before the walk reads PRIVATESIZE: under load the kernel
            // has reported 0 private blocks for a rewrite still in the
            // buffer cache (one verify run in ~20, 2026-09-08).
            f.sync_all().unwrap();
        }
        let out = scan(dir.path()).unwrap();
        let a = out.entries.iter().find(|e| e.name == "a.bin").unwrap();
        let c = out.entries.iter().find(|e| e.name == "c.bin").unwrap();
        assert_eq!(c.clone_id, None, "a modified clone is its own stream");
        assert_eq!(a.clone_id, None, "…and its source has no other pure clone");
        let private = c.private_size.unwrap();
        assert!(private > 0 && private < c.disk_size, "rewritten blocks only: {private}");
        assert_eq!(c.shared_size, Some(c.disk_size - private));
        let flags = c.flags.unwrap();
        assert!(flags.contains(EntryFlags::MAY_SHARE_BLOCKS));
        assert!(!flags.contains(EntryFlags::SHARES_ALL_BLOCKS));
        assert_eq!(
            out.total_disk_size,
            a.disk_size + c.disk_size,
            "no group: both count at full allocation (st_blocks, honestly labelled)"
        );
    }

    /// The walker clamps a kernel `private` to `alloc` (defensive: they are
    /// two different attributes of one object, and `shared` is their
    /// difference — an unclamped value would underflow in release).
    #[test]
    fn private_never_exceeds_disk_even_if_the_kernel_says_so() {
        // The walker clamps `private` to `alloc` (a defensive min); the
        // subtraction for `shared` can therefore never underflow.
        let e = BulkEntry {
            name: "x".into(),
            obj_type: ObjType::Regular,
            error: 0,
            dev: 1,
            ino: 1,
            nlink: 1,
            logical_size: 10,
            alloc_size: 4096,
            private_size: Some(8192),
            clone_id: Some(1),
            clone_refcnt: Some(1),
            ext_flags: Some(0),
            flags: 0,
            modified_at: None,
            is_mount_point: false,
        };
        let private = e.private_size.unwrap_or(e.alloc_size).min(e.alloc_size);
        assert_eq!(private, 4096);
        assert_eq!(e.alloc_size - private, 0);
    }

    /// A decmpfs-compressed file (ditto --hfsCompression; most of /System and
    /// many app bundles) keeps its bytes in the resource fork, so the kernel
    /// reports PRIVATESIZE 0 and no clone tracking for it. Reading that as
    /// "deleting frees nothing" would be the v1.0 lie inverted: its
    /// privateSize is its allocation, and the row says why.
    #[test]
    fn compressed_file_is_fully_private_and_flagged() {
        let dir = tempfile::tempdir().unwrap();
        let raw = dir.path().join("raw.bin");
        fs::write(&raw, vec![5u8; 4 << 20]).unwrap();
        let rc = std::process::Command::new("ditto")
            .arg("--hfsCompression")
            .arg(&raw)
            .arg(dir.path().join("packed.bin"))
            .status()
            .unwrap();
        assert!(rc.success(), "ditto --hfsCompression must succeed on APFS");
        fs::remove_file(&raw).unwrap();
        let out = scan(dir.path()).unwrap();
        let packed = out.entries.iter().find(|e| e.name == "packed.bin").unwrap();
        assert!(packed.disk_size > 0 && packed.disk_size < packed.logical_size, "compressed: {packed:?}");
        assert_eq!(packed.private_size, Some(packed.disk_size), "its blocks are its own");
        assert_eq!(packed.shared_size, Some(0));
        assert_eq!(packed.clone_id, None);
        assert!(packed.flags.unwrap().contains(EntryFlags::COMPRESSED), "{:?}", packed.flags);
    }

    #[test]
    fn entry_flags_map_every_filesystem_bit() {
        let base = BulkEntry {
            name: "x".into(),
            obj_type: ObjType::Regular,
            error: 0,
            dev: 1,
            ino: 1,
            nlink: 1,
            logical_size: 0,
            alloc_size: 0,
            private_size: None,
            clone_id: None,
            clone_refcnt: None,
            ext_flags: None,
            flags: 0,
            modified_at: None,
            is_mount_point: false,
        };
        assert!(entry_flags(&base).is_empty());
        let mut e = base.clone();
        e.ext_flags = Some(
            bulk::EF_MAY_SHARE_BLOCKS
                | bulk::EF_SHARES_ALL_BLOCKS
                | bulk::EF_IS_PURGEABLE
                | bulk::EF_IS_SPARSE,
        );
        e.flags = bulk::SF_DATALESS | bulk::SF_FIRMLINK | bulk::UF_COMPRESSED;
        e.is_mount_point = true;
        assert_eq!(
            entry_flags(&e).names(),
            vec![
                "mayShareBlocks",
                "sharesAllBlocks",
                "purgeable",
                "sparse",
                "dataless",
                "firmlink",
                "mountPoint",
                "compressed"
            ]
        );
        let mut only_dataless = base.clone();
        only_dataless.flags = bulk::SF_DATALESS;
        assert_eq!(entry_flags(&only_dataless).names(), vec!["dataless"]);
    }

    #[test]
    fn descend_rule_is_one_filesystem_unless_asked() {
        let one_fs = ScanOptions::default();
        let cross = ScanOptions { cross_volumes: true };
        assert!(descends(true, false, one_fs), "ordinary dirs always");
        assert!(!descends(true, true, one_fs), "mount points are boundaries");
        assert!(descends(true, true, cross), "…unless the scan crosses volumes");
        assert!(!descends(false, false, cross), "files never");
    }

    // Mutation-proof (phantom-jsz, the plan's mutation #3): drop the
    // mount-point check in `descends` and this walk descends into
    // /System/Volumes/Data — the whole data volume — and this fails on the
    // first entry below a mount point. (It also takes a minute, which is
    // the loud kind of failure.)
    #[test]
    fn mount_points_are_recorded_but_not_descended() {
        let root = Path::new("/System/Volumes");
        let out = scan(root).unwrap();
        let data = out
            .entries
            .iter()
            .find(|e| e.path == "/System/Volumes/Data")
            .expect("the data volume's mount point is an entry");
        assert!(data.is_dir);
        assert!(
            data.flags.unwrap().contains(EntryFlags::MOUNT_POINT),
            "the boundary is visible on the row: {:?}",
            data.flags
        );
        let mount_points: Vec<&str> = out
            .entries
            .iter()
            .filter(|e| e.flags.unwrap().contains(EntryFlags::MOUNT_POINT))
            .map(|e| e.path.as_str())
            .collect();
        for e in &out.entries {
            if let Some(parent) = &e.parent_path {
                assert!(
                    !mount_points.iter().any(|m| parent == m || parent.starts_with(&format!("{m}/"))),
                    "nothing below a mount point may be walked: {}",
                    e.path
                );
            }
        }
        assert_eq!(out.file_count, 0, "/System/Volumes holds only mount points");
    }

    /// Symlink behavior pin (safety review): the bulk listing reports a
    /// symlink as itself (VLNK) and the walker never descends one. A
    /// symlink LOOP must terminate, and a symlink escaping the scan root
    /// must not pull the target's bytes into the totals.
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
            out.entries.iter().any(|e| e.name == "locked"),
            "the unreadable dir's own row exists (its parent listed it)"
        );
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

    /// An unreadable ROOT (the user picked a directory they cannot list) is
    /// a completed scan with one error, not a failure — the same posture as
    /// an unreadable subdirectory.
    #[test]
    fn unreadable_root_is_one_error_with_the_root_row() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let locked = dir.path().join("locked");
        fs::create_dir(&locked).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        let out = scan(&locked);
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        let out = out.unwrap();
        assert_eq!(out.error_count, 1);
        assert_eq!(out.entries.len(), 1, "just the root row");
        assert_eq!(out.unreadable[0].path, locked.display().to_string());
    }
}
