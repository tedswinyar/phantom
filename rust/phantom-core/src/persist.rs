// The ADR-0005 persistence post-pass: what a completed scan writes to SQLite.
//
// A full walk of a developer Mac is millions of entries (~590 bytes/row —
// gigabytes per scan), so completed scans persist a filtered view: every
// file whose diskSize is at least [`PERSIST_MIN_FILE_DISK_SIZE`], and every
// directory whose FULLY AGGREGATED subtree diskSize meets the same bound
// (1.1.1, phantom-cnr.10 — before that every directory persisted, and a
// home scan was 96% directory rows, 95% of them under 1 MiB: ~420 MB per
// scan instead of the ~40 MB ADR-0005 predicted). Small files and small
// subtrees still count — their bytes are folded into every persisted
// ancestor's totals and into the scan's totals — only their individual rows
// are omitted. Per-scan fileType totals are computed from the full walk
// BEFORE this filter (`format::totals_by_file_type`), so type breakdowns
// see everything.
//
// Three kinds of directory row are kept regardless of size, each with its
// whole ancestor chain so the tree stays connected from the root:
//   • the scan root (the anchor every reader starts from);
//   • every `topPath` of every hotspot group in the scan's summary — plan
//     creation and verify read private bytes BY PATH and must never meet a
//     missing row (`plans.rs`, `plan::verify_plan`);
//   • mount points (`EntryFlags::MOUNT_POINT`): a directory the walk did
//     NOT descend has 0 bytes by construction, and its row IS the boundary
//     (ADR-0006) — the threshold would otherwise erase every one of them.
// The size rule alone is monotone (a parent's aggregate includes every
// child's), so ancestor closure is only needed for the pinned rows.
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

use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::classify::HotspotsSummary;
use crate::format::{ChargeKey, LinkCharger};
use crate::scan::{EntryFlags, ScanEntry};
use crate::share::ShareLedger;

/// Files below this disk size are not individually persisted (ADR-0005),
/// and since 1.1.1 neither are directories whose whole subtree is below it.
/// The boundary is INCLUSIVE: a file (or subtree) of exactly 1 MiB is kept.
/// Anything below 1 MiB is never individually actionable for disk reclaim;
/// its bytes still appear in every persisted ancestor's aggregated totals.
pub const PERSIST_MIN_FILE_DISK_SIZE: u64 = 1_048_576; // 1 MiB

/// The phrase a reader uses when a path has no row because of this filter.
/// One constant so the API's 404 text, the tests, and the contract prose
/// cannot drift apart.
pub const NOT_INDIVIDUALLY_PERSISTED: &str = "not individually persisted";

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

/// What a directory visibly holds, for the row-persistence rule: its
/// du-model aggregate (`disk`) or, if larger, the allocation it touches
/// (`private + shared`). The two differ only for sharing groups, which the
/// du model charges to the FIRST reference's directory: a Finder-duplicated
/// tree reads `disk 0, shared 25 GB`, and dropping its row would also drop
/// the "deleting this frees nothing" verdict (phantom-mkn.1) that is the
/// whole point of persisting the split. Monotone like `disk`: a parent
/// touches every group its children touch.
fn visible_bytes(t: &DirTotals) -> u64 {
    t.disk.max(t.private.saturating_add(t.shared))
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

/// Filter a full walk down to what a completed scan persists. Every
/// directory gets its `disk_size`/`logical_size` replaced by the aggregate
/// over EVERY descendant file, filtered or not, its `file_count`/`dir_count`
/// set to the full-depth descendant counts, and its `private_size`/
/// `shared_size` set to the deletion-honest split; a directory ROW then
/// persists iff that aggregate `disk_size` is at least
/// [`PERSIST_MIN_FILE_DISK_SIZE`], or the directory is pinned (the scan
/// root, a hotspot `topPath` in `hotspots`, a mount point) — pinned rows
/// bring their ancestor chain with them. Files persist at or above the same
/// bound (their counts stay null). Input order is preserved.
///
/// Counts come from the same full-walk pass as the sizes, so the sub-1-MiB
/// files and subtrees whose rows are filtered out below still count — a
/// client counting persisted rows would structurally undercount.
///
/// Deduped (phantom-5ws, phantom-mkn.1): a sharing group's bytes land in
/// its FIRST reference's ancestors only, and only the first reference's row
/// is persisted — so persisted directory totals stay additive (children
/// never out-sum a parent) and the treemap geometry stays sound.
/// `file_count` still counts every reference.
pub fn persistable_entries(
    entries: &[ScanEntry],
    shares: &ShareLedger,
    hotspots: &HotspotsSummary,
) -> Vec<ScanEntry> {
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

    // Which directory rows survive (1.1.1). The size rule is monotone on its
    // own; the pinned set is closed over ancestors explicitly. Mutation
    // targets (phantom-cnr.10): keep every directory again → the home-shaped
    // fixture test fails; drop the topPaths pin → plan creation on a small
    // hotspot 404s; test the direct-children size instead of the subtree
    // aggregate → "every kept row's parent is kept" fails.
    let top_paths: HashSet<&str> = hotspots
        .groups
        .iter()
        .flat_map(|g| g.top_paths.iter().map(String::as_str))
        .collect();
    let mut kept_dirs: HashSet<&str> = HashSet::new();
    for e in entries.iter().filter(|e| e.is_dir) {
        let path = e.path.as_str();
        if visible_bytes(&totals[path]) >= PERSIST_MIN_FILE_DISK_SIZE {
            kept_dirs.insert(path);
            continue;
        }
        let is_root = e.parent_path.is_none();
        let is_mount_point = e.flags.is_some_and(|f| f.contains(EntryFlags::MOUNT_POINT));
        if is_root || is_mount_point || top_paths.contains(path) {
            // The pinned row and every scanned ancestor above it.
            let mut cursor = Some(Path::new(path));
            while let Some(dir) = cursor {
                let key = dir.to_str().unwrap_or("");
                if !totals.contains_key(key) || !kept_dirs.insert(key) {
                    break; // left the scan, or reached an already-kept chain
                }
                cursor = dir.parent();
            }
        }
    }

    entries
        .iter()
        .zip(&charged)
        .filter_map(|(e, &counts)| {
            if e.is_dir {
                if !kept_dirs.contains(e.path.as_str()) {
                    return None;
                }
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

    /// The common case: a ledger derived from the entries themselves, no
    /// hotspots.
    fn persist(entries: &[ScanEntry]) -> Vec<ScanEntry> {
        persistable_entries(entries, &ShareLedger::from_entries(entries), &HotspotsSummary::empty())
    }

    /// A summary whose first group names `top_paths` — decoded from the
    /// shared raw fixture (the parser standard), then re-pointed.
    fn summary_pinning(top_paths: &[&str]) -> HotspotsSummary {
        let raw = include_str!("../../../tests/fixtures/hotspots-summary.json");
        let mut s: HotspotsSummary = serde_json::from_str(raw).unwrap();
        s.groups.truncate(1);
        s.groups[0].top_paths = top_paths.iter().map(|p| p.to_string()).collect();
        s
    }

    fn kept_paths(kept: &[ScanEntry]) -> Vec<&str> {
        kept.iter().map(|e| e.path.as_str()).collect()
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
        // sub/ carries a 2 MiB file so its row survives the directory
        // threshold; its tiny.rs is filtered yet still counted.
        let entries = vec![
            entry("/r", None, 0, 0, true),
            entry("/r/sub", Some("/r"), 0, 0, true),
            entry("/r/big.bin", Some("/r"), 2_000_000, 1_900_000, false),
            entry("/r/small.txt", Some("/r"), 512, 10, false),
            entry("/r/sub/medium.log", Some("/r/sub"), 3_000_000, 2_900_000, false),
            entry("/r/sub/tiny.rs", Some("/r/sub"), 1024, 100, false),
        ];
        let kept = persist(&entries);

        let root = row(&kept, "/r");
        assert_eq!(
            (root.disk_size, root.logical_size),
            (2_000_000 + 512 + 3_000_000 + 1024, 1_900_000 + 10 + 2_900_000 + 100),
            "root aggregate must count filtered small files"
        );
        let sub = row(&kept, "/r/sub");
        assert_eq!((sub.disk_size, sub.logical_size), (3_000_000 + 1024, 2_900_000 + 100));

        // The small files' own rows are gone; the big file survives intact.
        assert!(!kept.iter().any(|e| e.name == "small.txt"));
        assert!(!kept.iter().any(|e| e.name == "tiny.rs"));
        let big = row(&kept, "/r/big.bin");
        assert_eq!(big.disk_size, 2_000_000);
        // Plain files with no recorded private size: the allocation is the
        // private size (nothing shares it), so every dir is fully private.
        assert_eq!(split(&kept, "/r"), (2_000_000 + 512 + 3_000_000 + 1024, 0));
        assert_eq!(split(&kept, "/r/sub"), (3_000_000 + 1024, 0));
    }

    /// 1.1.1: an empty directory has a 0-byte subtree, so its row is NOT
    /// persisted — its dir_count of 1 lives on in the root's counts. The
    /// root itself is kept even when the whole scan is below 1 MiB.
    #[test]
    fn empty_directory_is_not_persisted_but_the_root_still_is() {
        let entries = vec![
            entry("/r", None, 0, 0, true),
            entry("/r/empty", Some("/r"), 0, 0, true),
        ];
        let kept = persist(&entries);
        assert_eq!(kept_paths(&kept), vec!["/r"], "a 0-byte scan keeps only its root");
        let root = row(&kept, "/r");
        assert_eq!((root.disk_size, root.logical_size), (0, 0));
        // Zero counts are Some(0), not null — "we looked, there is nothing"
        // is different from "not recorded"; the empty child still counts.
        assert_eq!((root.file_count, root.dir_count), (Some(0), Some(1)));
        assert_eq!((root.private_size, root.shared_size), (Some(0), Some(0)));
    }

    /// A directory row that meets the threshold with zero persisted children
    /// — the shape the tree/treemap must render from its counts (a 1.1.1
    /// reader consequence): 400 tiny files add up past 1 MiB, none of them
    /// has a row, and the two subdirectories holding them have none either.
    #[test]
    fn kept_directory_can_have_counts_but_no_persisted_children() {
        let mut entries = vec![
            entry("/r", None, 0, 0, true),
            entry("/r/cache", Some("/r"), 0, 0, true),
            entry("/r/cache/a", Some("/r/cache"), 0, 0, true),
            entry("/r/cache/b", Some("/r/cache"), 0, 0, true),
        ];
        for i in 0..400 {
            let parent = if i % 2 == 0 { "/r/cache/a" } else { "/r/cache/b" };
            entries.push(entry(&format!("{parent}/f{i}"), Some(parent), 4096, 4000, false));
        }
        let kept = persist(&entries);
        assert_eq!(kept_paths(&kept), vec!["/r", "/r/cache"]);
        let cache = row(&kept, "/r/cache");
        assert_eq!(cache.disk_size, 400 * 4096);
        assert_eq!((cache.file_count, cache.dir_count), (Some(400), Some(2)));
    }

    // Mutation-proof: count from the PERSISTED rows instead of the full walk
    // (or filter before counting) and root drops to (2, 1) — small.txt,
    // tiny.rs and deep/ vanish. Swap the file/dir tallies and root reads (2, 3).
    #[test]
    fn directory_counts_are_full_depth_over_the_full_walk() {
        let entries = vec![
            entry("/r", None, 0, 0, true),
            entry("/r/sub", Some("/r"), 0, 0, true),
            entry("/r/sub/deep", Some("/r/sub"), 0, 0, true),
            entry("/r/big.bin", Some("/r"), 2_000_000, 1_900_000, false),
            entry("/r/small.txt", Some("/r"), 512, 10, false),
            entry("/r/sub/medium.log", Some("/r/sub"), 3_000_000, 2_900_000, false),
            entry("/r/sub/tiny.rs", Some("/r/sub"), 1024, 100, false),
        ];
        let kept = persist(&entries);

        let root = row(&kept, "/r");
        assert_eq!(root.file_count, Some(4), "filtered small files still count");
        assert_eq!(root.dir_count, Some(2), "full depth (sub + deep), self excluded");

        let sub = row(&kept, "/r/sub");
        assert_eq!((sub.file_count, sub.dir_count), (Some(2), Some(1)));
        // deep/ is empty: counted in sub's dir_count, no row of its own.
        assert!(!kept.iter().any(|e| e.path == "/r/sub/deep"));

        // File rows never carry counts — present-as-null on the wire.
        let big = row(&kept, "/r/big.bin");
        assert_eq!((big.file_count, big.dir_count), (None, None));
    }

    // -- the directory threshold (1.1.1, phantom-cnr.10) ------------------------

    /// A home-shaped walk: 3,000 tiny directories (node_modules, .git, tool
    /// caches — each holding one 4 KiB file) under 10 project roots, plus 5
    /// directories carrying a 2 MiB artifact. Before 1.1.1 every one of the
    /// 3,016 directories persisted; the 1 MiB rule keeps 16 — the root, the
    /// 10 project roots (each aggregates 300 × 4 KiB ≥ 1 MiB) and the 5
    /// artifact dirs. Mutation-proof: keep every directory again and 3,016
    /// rows come back (the live home scan: 550,137 → ~54,000).
    #[test]
    fn home_shaped_fixture_persists_about_ten_times_fewer_directory_rows() {
        let mut entries = vec![entry("/home", None, 0, 0, true)];
        for p in 0..10 {
            let proj = format!("/home/proj{p}");
            entries.push(entry(&proj, Some("/home"), 0, 0, true));
            for d in 0..300 {
                let tiny = format!("{proj}/node_modules{d}");
                entries.push(entry(&tiny, Some(&proj), 0, 0, true));
                entries.push(entry(&format!("{tiny}/index.js"), Some(&tiny), 4096, 3000, false));
            }
        }
        for a in 0..5 {
            let art = format!("/home/proj{a}/target");
            entries.push(entry(&art, Some(&format!("/home/proj{a}")), 0, 0, true));
            entries.push(entry(
                &format!("{art}/debug.bin"),
                Some(&art),
                2 * PERSIST_MIN_FILE_DISK_SIZE,
                2 * PERSIST_MIN_FILE_DISK_SIZE,
                false,
            ));
        }
        let dirs_in_walk = entries.iter().filter(|e| e.is_dir).count();
        assert_eq!(dirs_in_walk, 1 + 10 + 3000 + 5);

        let kept = persist(&entries);
        let dir_rows: Vec<&str> = kept.iter().filter(|e| e.is_dir).map(|e| e.path.as_str()).collect();
        assert_eq!(dir_rows.len(), 1 + 10 + 5, "root + 10 project roots + 5 artifact dirs: {dir_rows:?}");
        assert!(
            dir_rows.len() * 10 <= dirs_in_walk,
            "the rule must cut directory rows by at least 10x on a home-shaped walk"
        );
        assert!(dir_rows.iter().all(|p| !p.contains("node_modules")), "no tiny dir survives");
        // Their bytes did not vanish: each project root still aggregates
        // its 300 × 4 KiB of filtered subtree.
        assert_eq!(row(&kept, "/home/proj9").disk_size, 300 * 4096);
        assert_eq!(row(&kept, "/home/proj9").dir_count, Some(300));
        assert_eq!(kept.iter().filter(|e| !e.is_dir).count(), 5, "only the five 2 MiB files");
    }

    /// The boundary is the same inclusive constant as for files: a subtree
    /// of exactly 1 MiB keeps its row, 1 MiB − 1 does not.
    #[test]
    fn directory_boundary_is_inclusive_like_the_file_boundary() {
        let entries = vec![
            entry("/r", None, 0, 0, true),
            entry("/r/exact", Some("/r"), 0, 0, true),
            entry("/r/exact/half-a", Some("/r/exact"), PERSIST_MIN_FILE_DISK_SIZE / 2, 1, false),
            entry("/r/exact/half-b", Some("/r/exact"), PERSIST_MIN_FILE_DISK_SIZE / 2, 1, false),
            entry("/r/under", Some("/r"), 0, 0, true),
            entry("/r/under/half-a", Some("/r/under"), PERSIST_MIN_FILE_DISK_SIZE / 2, 1, false),
            entry("/r/under/rest", Some("/r/under"), PERSIST_MIN_FILE_DISK_SIZE / 2 - 1, 1, false),
        ];
        let kept = persist(&entries);
        assert_eq!(kept_paths(&kept), vec!["/r", "/r/exact"], "exactly 1 MiB of small files keeps the dir");
    }

    /// Every hotspot topPath is kept regardless of size — plan creation and
    /// verify read private bytes by path — and so is its ancestor chain, so
    /// the tree stays connected. Mutation-proof: drop the topPaths pin and
    /// `/r/proj/target` (300 KiB) vanishes; drop the ancestor closure and
    /// `/r/proj` (300 KiB, no big file) vanishes while its child stays.
    #[test]
    fn hotspot_top_paths_below_threshold_are_kept_with_their_ancestors() {
        let entries = vec![
            entry("/r", None, 0, 0, true),
            entry("/r/big.bin", Some("/r"), 4 * PERSIST_MIN_FILE_DISK_SIZE, 1, false),
            entry("/r/proj", Some("/r"), 0, 0, true),
            entry("/r/proj/target", Some("/r/proj"), 0, 0, true),
            entry("/r/proj/target/debug.bin", Some("/r/proj/target"), 300 * 1024, 1, false),
            entry("/r/proj/src", Some("/r/proj"), 0, 0, true),
            entry("/r/proj/src/main.rs", Some("/r/proj/src"), 200, 1, false),
        ];
        let without = persistable_entries(&entries, &ShareLedger::from_entries(&entries), &HotspotsSummary::empty());
        assert_eq!(kept_paths(&without), vec!["/r", "/r/big.bin"], "no pin: the small project is folded into the root");

        let pinned = summary_pinning(&["/r/proj/target"]);
        let with = persistable_entries(&entries, &ShareLedger::from_entries(&entries), &pinned);
        assert_eq!(
            kept_paths(&with),
            vec!["/r", "/r/big.bin", "/r/proj", "/r/proj/target"],
            "the topPath and its ancestor survive; the sibling src/ does not"
        );
        let target = row(&with, "/r/proj/target");
        assert_eq!(target.private_size, Some(300 * 1024), "the plan reads THIS number by path");
    }

    /// A topPath naming a path the walk never saw (a stale summary, a path
    /// outside the root) pins nothing and breaks nothing.
    #[test]
    fn unknown_top_path_is_ignored() {
        let entries = vec![
            entry("/r", None, 0, 0, true),
            entry("/r/small", Some("/r"), 0, 0, true),
            entry("/r/small/x", Some("/r/small"), 10, 10, false),
        ];
        let pinned = summary_pinning(&["/elsewhere/target", "/r/small/x"]);
        let kept = persistable_entries(&entries, &ShareLedger::from_entries(&entries), &pinned);
        assert_eq!(kept_paths(&kept), vec!["/r"]);
    }

    /// Mount points are 0-byte directories by construction (the walk did
    /// not descend them); their row IS the boundary, so they are kept.
    #[test]
    fn mount_points_are_kept_as_visible_boundaries() {
        let mut data = entry("/System/Volumes/Data", Some("/System/Volumes"), 0, 0, true);
        data.flags = Some(EntryFlags::MOUNT_POINT);
        let entries = vec![
            entry("/System/Volumes", None, 0, 0, true),
            data,
            entry("/System/Volumes/plain", Some("/System/Volumes"), 0, 0, true),
        ];
        let kept = persist(&entries);
        assert_eq!(kept_paths(&kept), vec!["/System/Volumes", "/System/Volumes/Data"]);
        assert!(row(&kept, "/System/Volumes/Data").flags.unwrap().contains(EntryFlags::MOUNT_POINT));
    }

    /// The structural invariant every reader relies on: the persisted rows
    /// form a tree connected from the root — each kept row's parent (when
    /// it is inside the scan) is itself kept. Exercised over every path to a
    /// row: size (deep big file under tiny parents), pin (deep topPath),
    /// and mount point. Mutation-proof: decide by a directory's DIRECT
    /// children's size instead of the subtree aggregate and `/r/a` (whose
    /// only content is a subdirectory) is dropped while `/r/a/b` stays.
    #[test]
    fn every_kept_rows_parent_is_kept() {
        let mut mount = entry("/r/mnt/vol", Some("/r/mnt"), 0, 0, true);
        mount.flags = Some(EntryFlags::MOUNT_POINT);
        let entries = vec![
            entry("/r", None, 0, 0, true),
            entry("/r/a", Some("/r"), 0, 0, true),
            entry("/r/a/b", Some("/r/a"), 0, 0, true),
            entry("/r/a/b/big.bin", Some("/r/a/b"), 3 * PERSIST_MIN_FILE_DISK_SIZE, 1, false),
            entry("/r/p", Some("/r"), 0, 0, true),
            entry("/r/p/q", Some("/r/p"), 0, 0, true),
            entry("/r/p/q/target", Some("/r/p/q"), 0, 0, true),
            entry("/r/p/q/target/o.bin", Some("/r/p/q/target"), 64, 64, false),
            entry("/r/mnt", Some("/r"), 0, 0, true),
            mount,
            entry("/r/tiny", Some("/r"), 0, 0, true),
            entry("/r/tiny/t", Some("/r/tiny"), 8, 8, false),
        ];
        let pinned = summary_pinning(&["/r/p/q/target"]);
        let kept = persistable_entries(&entries, &ShareLedger::from_entries(&entries), &pinned);
        let paths: HashSet<&str> = kept.iter().map(|e| e.path.as_str()).collect();
        for e in &kept {
            if let Some(parent) = &e.parent_path {
                assert!(paths.contains(parent.as_str()), "{} is kept but its parent {parent} is not", e.path);
            } else {
                assert_eq!(e.path, "/r", "only the scan root has no parent");
            }
        }
        // And the expected set, so the property is not vacuously true.
        assert_eq!(
            kept_paths(&kept),
            vec![
                "/r",
                "/r/a",
                "/r/a/b",
                "/r/a/b/big.bin",
                "/r/p",
                "/r/p/q",
                "/r/p/q/target",
                "/r/mnt",
                "/r/mnt/vol",
            ]
        );
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
        let kept = persistable_entries(&entries, &ledger, &HotspotsSummary::empty());
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
