// The ADR-0005 persistence post-pass: what a completed scan writes to SQLite.
//
// A full walk of a developer Mac is millions of entries (~590 bytes/row —
// gigabytes per scan), so completed scans persist a filtered view: every
// directory, carrying its FULLY AGGREGATED totals, plus every file whose
// diskSize is at least [`PERSIST_MIN_FILE_DISK_SIZE`]. Small files still
// count — their bytes are folded into every ancestor directory's totals and
// into the scan's totals — only their individual rows are omitted. Per-scan
// fileType totals are computed from the full walk BEFORE this filter
// (`format::totals_by_file_type`), so type breakdowns see everything.
//
// This is a post-pass at persistence time; the in-flight scan registry holds
// the full walk in memory, so live progress and completion totals are exact.
//
// Directory rows also get their `privateSize` / `sharedSize` here (v1.1,
// phantom-mkn.2): what deleting the subtree would free versus what other
// references pin. A sharing group (hardlinked inode, pure-clone stream) is
// private to exactly the directories that contain EVERY one of its
// references — and only if every reference is inside the scan at all (the
// `ShareLedger` knows the filesystem's counts). Everywhere else it is
// shared. Ungrouped files contribute their own kernel-reported private
// bytes and the remainder as shared.

use std::collections::HashMap;
use std::path::Path;

use crate::format::{ChargeKey, LinkCharger};
use crate::scan::ScanEntry;
use crate::share::ShareLedger;

/// Files below this disk size are not individually persisted (ADR-0005).
/// The boundary is INCLUSIVE: a file of exactly 1 MiB is kept. A file below
/// 1 MiB is never individually actionable for disk reclaim; its bytes still
/// appear in every ancestor directory's aggregated totals.
pub const PERSIST_MIN_FILE_DISK_SIZE: u64 = 1_048_576; // 1 MiB

/// Per-directory aggregate over the FULL walk: sizes, descendant counts,
/// and the deletion-honest split.
#[derive(Default, Clone, Copy)]
struct DirTotals {
    disk: u64,
    logical: u64,
    files: u64,
    dirs: u64,
    private: u64,
    shared: u64,
}

/// Every scanned ancestor of `member` (its parent, grandparent, … up to and
/// including the scan root) gains one reference. The walk left the scan
/// root at the first ancestor that is not a scanned directory.
fn count_refs_per_dir<'a>(
    members: impl Iterator<Item = &'a str>,
    is_scanned_dir: impl Fn(&str) -> bool,
) -> HashMap<&'a str, u64> {
    let mut counts: HashMap<&'a str, u64> = HashMap::new();
    for member in members {
        let mut ancestor = Path::new(member).parent();
        while let Some(dir) = ancestor {
            let key = dir.to_str().unwrap_or("");
            if !is_scanned_dir(key) {
                break;
            }
            *counts.entry(key).or_insert(0) += 1;
            ancestor = dir.parent();
        }
    }
    counts
}

/// Filter a full walk down to what a completed scan persists: all
/// directories — their `disk_size`/`logical_size` replaced by the aggregate
/// over EVERY descendant file, filtered or not, their `file_count`/
/// `dir_count` set to the full-depth descendant counts, and their
/// `private_size`/`shared_size` set to the deletion-honest split — plus
/// files at or above [`PERSIST_MIN_FILE_DISK_SIZE`] (their counts stay
/// null). Input order is preserved.
///
/// Counts come from the same full-walk pass as the sizes, so the sub-1-MiB
/// files whose rows are filtered out below still count — a client counting
/// persisted rows would structurally undercount.
///
/// Deduped (phantom-5ws, phantom-mkn.1): a sharing group's bytes land in
/// its FIRST reference's ancestors only, and only the first reference's row
/// is persisted — so persisted directory totals stay additive (children
/// never out-sum a parent) and the treemap geometry stays sound.
/// `file_count` still counts every reference.
pub fn persistable_entries(entries: &[ScanEntry], shares: &ShareLedger) -> Vec<ScanEntry> {
    // The charge decision per entry, in input (walk) order — shared verdicts
    // for the rollup pass and the row-emission pass below, and the same
    // first-seen attribution the scanner used for the scan totals.
    let mut links = LinkCharger::new();
    let charged: Vec<bool> = entries
        .iter()
        .map(|e| !e.is_dir && links.charges_entry(e))
        .collect();

    // Totals per directory, over the FULL entry set. Same ancestor-walk as
    // `format::directory_disk_totals`, aggregating sizes and counts in one
    // pass per entry.
    let mut totals: HashMap<&str, DirTotals> = entries
        .iter()
        .filter(|e| e.is_dir)
        .map(|e| (e.path.as_str(), DirTotals::default()))
        .collect();
    // Sharing groups and where their references live.
    let mut groups: HashMap<ChargeKey, Vec<&str>> = HashMap::new();

    for (entry, &counts) in entries.iter().zip(&charged) {
        let key = if entry.is_dir { None } else { ChargeKey::of_entry(entry) };
        if let Some(key) = key {
            groups.entry(key).or_default().push(entry.path.as_str());
        }
        // An ungrouped file's own split; grouped files are settled below.
        let (own_private, own_shared) = match (entry.is_dir, key) {
            (false, None) => {
                let p = ShareLedger::ungrouped_private(entry).min(entry.disk_size);
                (p, entry.disk_size - p)
            }
            _ => (0, 0),
        };
        let mut ancestor = entry.parent_path.as_deref().map(Path::new);
        // Files contribute bytes + a file count to every ancestor; dirs
        // contribute only a dir count (their bytes ARE their descendants').
        while let Some(dir) = ancestor {
            let key = dir.to_string_lossy();
            match totals.get_mut(key.as_ref()) {
                Some(t) => {
                    if entry.is_dir {
                        t.dirs += 1;
                    } else {
                        if counts {
                            t.disk += entry.disk_size;
                            t.logical += entry.logical_size;
                        }
                        t.files += 1;
                        t.private += own_private;
                        t.shared += own_shared;
                    }
                    ancestor = dir.parent();
                }
                // First non-scanned ancestor == we've left the scan root.
                None => break,
            }
        }
    }

    // Settle each sharing group: a directory holding EVERY reference (and
    // the group being wholly inside the scan) may free it — private; any
    // other directory that touches it only shares it. Mutation target
    // (phantom-mkn.2): drop the `freed_by` test and a pnpm store linked
    // from outside the scan reads as reclaimable.
    for (key, members) in &groups {
        let alloc = shares.alloc(*key);
        let refs = count_refs_per_dir(members.iter().copied(), |d| totals.contains_key(d));
        for (dir, n) in refs {
            let t = totals.get_mut(dir).expect("counted dirs are scanned dirs");
            if shares.freed_by(*key, n) > 0 {
                t.private += alloc;
            } else {
                t.shared += alloc;
            }
        }
    }

    entries
        .iter()
        .zip(&charged)
        .filter_map(|(e, &counts)| {
            if e.is_dir {
                let t = totals[e.path.as_str()];
                let mut dir = e.clone();
                dir.disk_size = t.disk;
                dir.logical_size = t.logical;
                dir.file_count = Some(t.files);
                dir.dir_count = Some(t.dirs);
                dir.private_size = Some(t.private);
                dir.shared_size = Some(t.shared);
                Some(dir)
            } else if counts && e.disk_size >= PERSIST_MIN_FILE_DISK_SIZE {
                Some(e.clone())
            } else {
                None
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, parent: Option<&str>, disk: u64, logical: u64, is_dir: bool) -> ScanEntry {
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
            logical_size: logical,
            modified_at: None,
            file_type: None,
            category: None,
            nlink: 1,
            dev: 0,
            ino: 0,
            file_count: None,
            dir_count: None,
            private_size: None,
            shared_size: None,
            clone_id: None,
            flags: None,
        }
    }

    /// The common case: a ledger derived from the entries themselves.
    fn persist(entries: &[ScanEntry]) -> Vec<ScanEntry> {
        persistable_entries(entries, &ShareLedger::from_entries(entries))
    }

    fn row<'a>(kept: &'a [ScanEntry], path: &str) -> &'a ScanEntry {
        kept.iter()
            .find(|e| e.path == path)
            .unwrap_or_else(|| panic!("no row {path}"))
    }

    fn split(kept: &[ScanEntry], path: &str) -> (u64, u64) {
        let r = row(kept, path);
        (r.private_size.unwrap(), r.shared_size.unwrap())
    }

    #[test]
    fn threshold_is_one_mebibyte() {
        // The ADR-0005 number is load-bearing product behavior; a silent
        // "tuning" edit must fail a test, not slip through.
        assert_eq!(PERSIST_MIN_FILE_DISK_SIZE, 1024 * 1024);
    }

    // Mutation-proof: change `>=` to `>` in the filter and the exact-boundary
    // case fails; change it to `<` (or drop the filter) and the below-boundary
    // case fails. The boundary is inclusive by decision, not accident.
    #[test]
    fn boundary_is_inclusive_on_both_sides() {
        let entries = vec![
            entry("/r", None, 0, 0, true),
            entry("/r/exact.bin", Some("/r"), PERSIST_MIN_FILE_DISK_SIZE, 1, false),
            entry(
                "/r/under.bin",
                Some("/r"),
                PERSIST_MIN_FILE_DISK_SIZE - 1,
                1,
                false,
            ),
        ];
        let kept = persist(&entries);
        let names: Vec<&str> = kept.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["r", "exact.bin"], "exactly 1 MiB is KEPT, 1 MiB - 1 is not");
    }

    #[test]
    fn directory_totals_include_the_filtered_remainder() {
        let entries = vec![
            entry("/r", None, 0, 0, true),
            entry("/r/sub", Some("/r"), 0, 0, true),
            entry("/r/big.bin", Some("/r"), 2_000_000, 1_900_000, false),
            entry("/r/small.txt", Some("/r"), 512, 10, false),
            entry("/r/sub/tiny.rs", Some("/r/sub"), 1024, 100, false),
        ];
        let kept = persist(&entries);

        let root = row(&kept, "/r");
        assert_eq!(
            (root.disk_size, root.logical_size),
            (2_000_000 + 512 + 1024, 1_900_000 + 10 + 100),
            "root aggregate must count filtered small files"
        );
        let sub = row(&kept, "/r/sub");
        assert_eq!((sub.disk_size, sub.logical_size), (1024, 100));

        // The small files' own rows are gone; the big file survives intact.
        assert!(!kept.iter().any(|e| e.name == "small.txt"));
        assert!(!kept.iter().any(|e| e.name == "tiny.rs"));
        let big = row(&kept, "/r/big.bin");
        assert_eq!(big.disk_size, 2_000_000);
        // Plain files with no recorded private size: the allocation is the
        // private size (nothing shares it), so every dir is fully private.
        assert_eq!(split(&kept, "/r"), (2_000_000 + 512 + 1024, 0));
        assert_eq!(split(&kept, "/r/sub"), (1024, 0));
    }

    #[test]
    fn empty_directory_is_kept_with_zero_totals() {
        let entries = vec![
            entry("/r", None, 0, 0, true),
            entry("/r/empty", Some("/r"), 0, 0, true),
        ];
        let kept = persist(&entries);
        assert_eq!(kept.len(), 2);
        let empty = row(&kept, "/r/empty");
        assert_eq!((empty.disk_size, empty.logical_size), (0, 0));
        // Zero counts are Some(0), not null — "we looked, there is nothing"
        // is different from "not recorded".
        assert_eq!((empty.file_count, empty.dir_count), (Some(0), Some(0)));
        assert_eq!((empty.private_size, empty.shared_size), (Some(0), Some(0)));
    }

    // Mutation-proof: count from the PERSISTED rows instead of the full walk
    // (or filter before counting) and root drops to (1, 1) — small.txt and
    // tiny.rs vanish. Swap the file/dir tallies and root reads (2, 3).
    #[test]
    fn directory_counts_are_full_depth_over_the_full_walk() {
        let entries = vec![
            entry("/r", None, 0, 0, true),
            entry("/r/sub", Some("/r"), 0, 0, true),
            entry("/r/sub/deep", Some("/r/sub"), 0, 0, true),
            entry("/r/big.bin", Some("/r"), 2_000_000, 1_900_000, false),
            entry("/r/small.txt", Some("/r"), 512, 10, false),
            entry("/r/sub/tiny.rs", Some("/r/sub"), 1024, 100, false),
        ];
        let kept = persist(&entries);

        let root = row(&kept, "/r");
        assert_eq!(root.file_count, Some(3), "filtered small files still count");
        assert_eq!(root.dir_count, Some(2), "full depth (sub + deep), self excluded");

        let sub = row(&kept, "/r/sub");
        assert_eq!((sub.file_count, sub.dir_count), (Some(1), Some(1)));
        let deep = row(&kept, "/r/sub/deep");
        assert_eq!((deep.file_count, deep.dir_count), (Some(0), Some(0)));

        // File rows never carry counts — present-as-null on the wire.
        let big = row(&kept, "/r/big.bin");
        assert_eq!((big.file_count, big.dir_count), (None, None));
    }

    fn linked(path: &str, parent: &str, disk: u64, nlink: u64, ino: u64) -> ScanEntry {
        let mut e = entry(path, Some(parent), disk, disk, false);
        e.nlink = nlink;
        e.dev = 7;
        e.ino = ino;
        e.private_size = Some(0);
        e.shared_size = Some(disk);
        e
    }

    // Mutation-proof (phantom-5ws): drop the `counts` guard in the rollup
    // and root doubles to 4 MiB; drop it in the emission filter and the
    // duplicate row reappears; charge the second link instead and /r/b gets
    // the bytes while /r/a reads 0.
    #[test]
    fn hardlinked_inode_charges_first_link_only_and_duplicate_row_is_dropped() {
        const TWO_MIB: u64 = 2 * PERSIST_MIN_FILE_DISK_SIZE;
        let entries = vec![
            entry("/r", None, 0, 0, true),
            entry("/r/a", Some("/r"), 0, 0, true),
            entry("/r/b", Some("/r"), 0, 0, true),
            linked("/r/a/one.bin", "/r/a", TWO_MIB, 2, 42),
            linked("/r/b/two.bin", "/r/b", TWO_MIB, 2, 42),
        ];
        let kept = persist(&entries);

        let root = row(&kept, "/r");
        assert_eq!(root.disk_size, TWO_MIB, "one inode, one charge");
        assert_eq!(root.file_count, Some(2), "both links still count as entries");

        let a = row(&kept, "/r/a");
        let b = row(&kept, "/r/b");
        assert_eq!(a.disk_size, TWO_MIB, "first link in walk order carries the bytes");
        assert_eq!(b.disk_size, 0, "second link's dir charges nothing");

        // Only the charged link's row is persisted — a client summing file
        // rows within a dir can never out-sum the dir's own aggregate. Pin
        // the EXACT set (not just "two.bin absent"): a duplicate row emitted
        // at disk_size 0 would pass a name-only check but inflate the count.
        assert_eq!(kept.len(), 4, "root + a + b + one.bin — no fourth file row");
        assert!(kept.iter().any(|e| e.name == "one.bin"));
        assert!(
            !kept.iter().any(|e| e.name == "two.bin"),
            "the duplicate link's row must not persist at all"
        );
    }

    // -- the deletion-honest split (phantom-mkn.2) -----------------------------

    /// Both links inside one directory: deleting that directory frees the
    /// inode. Its parent too. The du-model `diskSize` and the deletion-model
    /// `privateSize` agree here.
    #[test]
    fn hardlink_group_wholly_inside_a_directory_is_private_to_it() {
        const TWO_MIB: u64 = 2 * PERSIST_MIN_FILE_DISK_SIZE;
        let entries = vec![
            entry("/r", None, 0, 0, true),
            entry("/r/store", Some("/r"), 0, 0, true),
            linked("/r/store/one.bin", "/r/store", TWO_MIB, 2, 42),
            linked("/r/store/two.bin", "/r/store", TWO_MIB, 2, 42),
        ];
        let kept = persist(&entries);
        assert_eq!(split(&kept, "/r/store"), (TWO_MIB, 0));
        assert_eq!(split(&kept, "/r"), (TWO_MIB, 0));
    }

    /// The uv/pnpm shape: the store and the venv each hold a link. Deleting
    /// EITHER directory frees nothing; deleting their common ancestor frees
    /// the inode. Note `diskSize` (du model) puts the bytes in /r/a alone —
    /// the two numbers answer different questions.
    #[test]
    fn hardlink_group_spanning_two_directories_is_shared_in_each_and_private_above() {
        const TWO_MIB: u64 = 2 * PERSIST_MIN_FILE_DISK_SIZE;
        let entries = vec![
            entry("/r", None, 0, 0, true),
            entry("/r/a", Some("/r"), 0, 0, true),
            entry("/r/b", Some("/r"), 0, 0, true),
            linked("/r/a/one.bin", "/r/a", TWO_MIB, 2, 42),
            linked("/r/b/two.bin", "/r/b", TWO_MIB, 2, 42),
        ];
        let kept = persist(&entries);
        assert_eq!(split(&kept, "/r/a"), (0, TWO_MIB), "a's link is pinned by b's");
        assert_eq!(split(&kept, "/r/b"), (0, TWO_MIB));
        assert_eq!(split(&kept, "/r"), (TWO_MIB, 0), "the common ancestor holds every link");
        assert_eq!(row(&kept, "/r/a").disk_size, TWO_MIB, "du model: first link's dir");
        assert_eq!(row(&kept, "/r/b").disk_size, 0);
    }

    // Mutation-proof (phantom-mkn.2): make `freed_by` ignore the ledger's
    // completeness (compare `n` against the members seen instead of nlink)
    // and the root reads private — promising to free bytes a link outside
    // the scan still pins. THE "17 GB listed, 5 GB freed" lie, at the
    // directory level.
    #[test]
    fn hardlink_with_a_link_outside_the_scan_is_shared_all_the_way_up() {
        const TWO_MIB: u64 = 2 * PERSIST_MIN_FILE_DISK_SIZE;
        let entries = vec![
            entry("/r", None, 0, 0, true),
            entry("/r/venv", Some("/r"), 0, 0, true),
            // nlink 3: the third link lives in ~/.cache, outside the scan.
            linked("/r/venv/one.bin", "/r/venv", TWO_MIB, 3, 42),
            linked("/r/venv/two.bin", "/r/venv", TWO_MIB, 3, 42),
        ];
        let kept = persist(&entries);
        assert_eq!(split(&kept, "/r/venv"), (0, TWO_MIB));
        assert_eq!(split(&kept, "/r"), (0, TWO_MIB), "even the root cannot free it");
        assert_eq!(row(&kept, "/r").disk_size, TWO_MIB, "…yet du charges it here");
    }

    fn cloned(path: &str, parent: &str, disk: u64, ino: u64, clone_id: u64) -> ScanEntry {
        let mut e = entry(path, Some(parent), disk, disk, false);
        e.dev = 7;
        e.ino = ino;
        e.clone_id = Some(clone_id);
        e.private_size = Some(0);
        e.shared_size = Some(disk);
        e
    }

    /// A Finder-duplicated tree: the copy's files are pure clones of the
    /// original's. Deleting the copy frees nothing; deleting both frees the
    /// stream once — exactly the hardlink rule, keyed by clone id.
    #[test]
    fn pure_clone_group_is_charged_once_and_private_only_where_complete() {
        const FOUR_MIB: u64 = 4 * PERSIST_MIN_FILE_DISK_SIZE;
        let entries = vec![
            entry("/r", None, 0, 0, true),
            entry("/r/orig", Some("/r"), 0, 0, true),
            entry("/r/copy", Some("/r"), 0, 0, true),
            cloned("/r/orig/big.bin", "/r/orig", FOUR_MIB, 10, 10),
            cloned("/r/copy/big.bin", "/r/copy", FOUR_MIB, 11, 10),
        ];
        let kept = persist(&entries);
        assert_eq!(row(&kept, "/r").disk_size, FOUR_MIB, "two clones, one allocation");
        assert_eq!(row(&kept, "/r/orig").disk_size, FOUR_MIB, "first member's dir (du model)");
        assert_eq!(row(&kept, "/r/copy").disk_size, 0);
        assert_eq!(split(&kept, "/r/copy"), (0, FOUR_MIB), "deleting the copy frees nothing");
        assert_eq!(split(&kept, "/r/orig"), (0, FOUR_MIB));
        assert_eq!(split(&kept, "/r"), (FOUR_MIB, 0));
        assert_eq!(kept.len(), 4, "only the first member's file row persists");
    }

    /// A clone group whose other member lives outside the scan (the ledger
    /// knows: refcnt 2, one seen) is shared everywhere.
    #[test]
    fn clone_with_its_twin_outside_the_scan_is_shared() {
        const FOUR_MIB: u64 = 4 * PERSIST_MIN_FILE_DISK_SIZE;
        let entries = vec![
            entry("/r", None, 0, 0, true),
            cloned("/r/big.bin", "/r", FOUR_MIB, 10, 10),
        ];
        let mut ledger = ShareLedger::new();
        ledger.observe(&entries[1], Some(2));
        let kept = persistable_entries(&entries, &ledger);
        assert_eq!(split(&kept, "/r"), (0, FOUR_MIB));
    }

    /// A modified clone has no group (its own stream id) but the kernel
    /// says exactly which blocks are its own; a snapshot-trapped plain file
    /// says none are. Both flow straight into the directory split.
    #[test]
    fn ungrouped_files_contribute_their_kernel_private_bytes() {
        const FOUR_MIB: u64 = 4 * PERSIST_MIN_FILE_DISK_SIZE;
        let mut modified = entry("/r/modified.bin", Some("/r"), FOUR_MIB, FOUR_MIB, false);
        modified.private_size = Some(16_384);
        modified.shared_size = Some(FOUR_MIB - 16_384);
        let mut trapped = entry("/r/in-snapshot.bin", Some("/r"), FOUR_MIB, FOUR_MIB, false);
        trapped.private_size = Some(0);
        trapped.shared_size = Some(FOUR_MIB);
        let entries = vec![entry("/r", None, 0, 0, true), modified, trapped];
        let kept = persist(&entries);
        assert_eq!(row(&kept, "/r").disk_size, 2 * FOUR_MIB, "both count at full allocation");
        assert_eq!(split(&kept, "/r"), (16_384, 2 * FOUR_MIB - 16_384));
    }

    #[test]
    fn input_order_is_preserved() {
        let entries = vec![
            entry("/r", None, 0, 0, true),
            entry("/r/z.bin", Some("/r"), 2_000_000, 1, false),
            entry("/r/a.bin", Some("/r"), 3_000_000, 1, false),
        ];
        let paths: Vec<String> = persist(&entries)
            .into_iter()
            .map(|e| e.path)
            .collect();
        assert_eq!(paths, vec!["/r", "/r/z.bin", "/r/a.bin"]);
    }

    #[test]
    fn refs_per_dir_counts_every_scanned_ancestor() {
        let dirs = ["/r", "/r/a", "/r/a/deep", "/r/b"];
        let counts = count_refs_per_dir(
            ["/r/a/deep/x", "/r/a/y", "/r/b/z"].into_iter(),
            |d| dirs.contains(&d),
        );
        assert_eq!(counts["/r"], 3);
        assert_eq!(counts["/r/a"], 2);
        assert_eq!(counts["/r/a/deep"], 1);
        assert_eq!(counts["/r/b"], 1);
        assert!(!counts.contains_key("/"), "stops at the scan root");
    }
}
