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

use std::collections::HashMap;
use std::path::Path;

use crate::format::LinkCharger;
use crate::scan::ScanEntry;

/// Files below this disk size are not individually persisted (ADR-0005).
/// The boundary is INCLUSIVE: a file of exactly 1 MiB is kept. A file below
/// 1 MiB is never individually actionable for disk reclaim; its bytes still
/// appear in every ancestor directory's aggregated totals.
pub const PERSIST_MIN_FILE_DISK_SIZE: u64 = 1_048_576; // 1 MiB

/// Per-directory aggregate over the FULL walk: sizes and descendant counts.
#[derive(Default, Clone, Copy)]
struct DirTotals {
    disk: u64,
    logical: u64,
    files: u64,
    dirs: u64,
}

/// Filter a full walk down to what a completed scan persists: all
/// directories — their `disk_size`/`logical_size` replaced by the aggregate
/// over EVERY descendant file, filtered or not, and their
/// `file_count`/`dir_count` set to the full-depth descendant counts — plus
/// files at or above [`PERSIST_MIN_FILE_DISK_SIZE`] (their counts stay
/// null). Input order is preserved.
///
/// Counts come from the same full-walk pass as the sizes, so the sub-1-MiB
/// files whose rows are filtered out below still count — a client counting
/// persisted rows would structurally undercount.
///
/// Hardlink-deduped (phantom-5ws): an inode's bytes land in its FIRST link's
/// ancestors only, and only the first link's row is persisted — so persisted
/// directory totals stay additive (children never out-sum a parent) and the
/// treemap geometry stays sound. `file_count` still counts every link.
pub fn persistable_entries(entries: &[ScanEntry]) -> Vec<ScanEntry> {
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

    for (entry, &counts) in entries.iter().zip(&charged) {
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
                    }
                    ancestor = dir.parent();
                }
                // First non-scanned ancestor == we've left the scan root.
                None => break,
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
        }
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
        let kept = persistable_entries(&entries);
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
        let kept = persistable_entries(&entries);

        let root = kept.iter().find(|e| e.path == "/r").unwrap();
        assert_eq!(
            (root.disk_size, root.logical_size),
            (2_000_000 + 512 + 1024, 1_900_000 + 10 + 100),
            "root aggregate must count filtered small files"
        );
        let sub = kept.iter().find(|e| e.path == "/r/sub").unwrap();
        assert_eq!((sub.disk_size, sub.logical_size), (1024, 100));

        // The small files' own rows are gone; the big file survives intact.
        assert!(!kept.iter().any(|e| e.name == "small.txt"));
        assert!(!kept.iter().any(|e| e.name == "tiny.rs"));
        let big = kept.iter().find(|e| e.name == "big.bin").unwrap();
        assert_eq!(big.disk_size, 2_000_000);
    }

    #[test]
    fn empty_directory_is_kept_with_zero_totals() {
        let entries = vec![
            entry("/r", None, 0, 0, true),
            entry("/r/empty", Some("/r"), 0, 0, true),
        ];
        let kept = persistable_entries(&entries);
        assert_eq!(kept.len(), 2);
        let empty = kept.iter().find(|e| e.path == "/r/empty").unwrap();
        assert_eq!((empty.disk_size, empty.logical_size), (0, 0));
        // Zero counts are Some(0), not null — "we looked, there is nothing"
        // is different from "not recorded".
        assert_eq!((empty.file_count, empty.dir_count), (Some(0), Some(0)));
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
        let kept = persistable_entries(&entries);

        let root = kept.iter().find(|e| e.path == "/r").unwrap();
        assert_eq!(root.file_count, Some(3), "filtered small files still count");
        assert_eq!(root.dir_count, Some(2), "full depth (sub + deep), self excluded");

        let sub = kept.iter().find(|e| e.path == "/r/sub").unwrap();
        assert_eq!((sub.file_count, sub.dir_count), (Some(1), Some(1)));
        let deep = kept.iter().find(|e| e.path == "/r/sub/deep").unwrap();
        assert_eq!((deep.file_count, deep.dir_count), (Some(0), Some(0)));

        // File rows never carry counts — present-as-null on the wire.
        let big = kept.iter().find(|e| e.path == "/r/big.bin").unwrap();
        assert_eq!((big.file_count, big.dir_count), (None, None));
    }

    fn linked(path: &str, parent: &str, disk: u64, nlink: u64, ino: u64) -> ScanEntry {
        let mut e = entry(path, Some(parent), disk, disk, false);
        e.nlink = nlink;
        e.dev = 7;
        e.ino = ino;
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
        let kept = persistable_entries(&entries);

        let root = kept.iter().find(|e| e.path == "/r").unwrap();
        assert_eq!(root.disk_size, TWO_MIB, "one inode, one charge");
        assert_eq!(root.file_count, Some(2), "both links still count as entries");

        let a = kept.iter().find(|e| e.path == "/r/a").unwrap();
        let b = kept.iter().find(|e| e.path == "/r/b").unwrap();
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

    #[test]
    fn input_order_is_preserved() {
        let entries = vec![
            entry("/r", None, 0, 0, true),
            entry("/r/z.bin", Some("/r"), 2_000_000, 1, false),
            entry("/r/a.bin", Some("/r"), 3_000_000, 1, false),
        ];
        let paths: Vec<String> = persistable_entries(&entries)
            .into_iter()
            .map(|e| e.path)
            .collect();
        assert_eq!(paths, vec!["/r", "/r/z.bin", "/r/a.bin"]);
    }
}
