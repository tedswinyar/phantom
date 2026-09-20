// Human-readable sizes and directory/type aggregation — the ONE home for
// helpers the old v0.1 repo duplicated (with drift) across the API, CLI, and
// MCP server. Every client formats and aggregates through here so three
// tools can never again give two answers for the same directory.
//
// All aggregation is over `disk_size` (st_blocks × 512) — THE size.
//
// Hardlinks: aggregates charge an inode ONCE per scan (first link in walk
// order carries the bytes, further links add 0) — the du model. Entry rows
// keep their true per-link sizes; `classify` depends on that to compute the
// naive `listedDiskSize` next to its own deduped totals. Every aggregation
// site shares [`LinkCharger`] and iterates the same entries in the same
// order, so first-seen attribution is identical everywhere.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::scan::ScanEntry;

/// Why an entry's bytes might already have been charged: it is another
/// hard link to an inode seen earlier, or another member of an APFS pure-
/// clone group seen earlier. The two id spaces are kept apart (a clone id
/// numerically equal to some unrelated inode number must not collide).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChargeKey {
    Inode { dev: u64, ino: u64 },
    Clone { dev: u64, clone_id: u64 },
}

impl ChargeKey {
    /// The sharing group an entry belongs to, or `None` for a file whose
    /// bytes nobody else references (`nlink <= 1`, not a pure clone).
    /// Clone membership wins: hard links to a cloned inode carry the same
    /// clone id as every other member, so one key covers the whole group.
    pub fn of(nlink: u64, dev: u64, ino: u64, clone_id: Option<u64>) -> Option<ChargeKey> {
        if let Some(clone_id) = clone_id {
            Some(ChargeKey::Clone { dev, clone_id })
        } else if nlink > 1 {
            Some(ChargeKey::Inode { dev, ino })
        } else {
            None
        }
    }

    pub fn of_entry(e: &ScanEntry) -> Option<ChargeKey> {
        Self::of(e.nlink, e.dev, e.ino, e.clone_id)
    }
}

/// Decides, entry by entry, whether a file's bytes are charged to a scan's
/// aggregates or were already charged through another reference to the
/// same blocks — another hard link to the inode, or another pure clone of
/// the data stream (phantom-mkn.1: `cp -c` / Finder Duplicate; st_blocks
/// reports the full allocation on every clone). One instance per
/// aggregation pass; feed it every FILE entry in walk order (directories
/// carry no bytes of their own — don't feed them).
///
/// Ungrouped files never enter the set, so memory stays proportional to
/// shared files only (215k in the specter target/debug that motivated
/// this, not the 4.7M files of a home-dir scan).
///
/// Memory circuit-breaker (phantom-d1h): the set is bounded at
/// [`LinkCharger::MAX_TRACKED_GROUPS`] distinct groups (~10M keys ≈ 400 MB;
/// the worst real tree seen was 215k). Past the cap a group that was not
/// already tracked is CHARGED — counted at every reference, like `du`
/// without dedup — instead of growing the set toward an OOM. Groups tracked
/// before the cap keep deduplicating. `saturated()` tells the caller the
/// totals became an over-count so it can say so; a wrong-but-bounded number
/// with a flag beats a killed process.
#[derive(Debug, Default)]
pub struct LinkCharger {
    seen: HashSet<ChargeKey>,
    /// Distinct groups the set may hold; `None` = the default cap.
    cap: Option<usize>,
    saturated: bool,
}

impl LinkCharger {
    /// Distinct sharing groups tracked before the breaker trips.
    pub const MAX_TRACKED_GROUPS: usize = 10_000_000;

    pub fn new() -> Self {
        Self::default()
    }

    /// A charger with a smaller cap — for tests; production uses the default.
    pub fn with_cap(cap: usize) -> Self {
        Self { seen: HashSet::new(), cap: Some(cap), saturated: false }
    }

    fn effective_cap(&self) -> usize {
        self.cap.unwrap_or(Self::MAX_TRACKED_GROUPS)
    }

    /// True once the breaker tripped: from then on untracked groups were
    /// counted at every reference, so deduped totals are an over-count.
    pub fn saturated(&self) -> bool {
        self.saturated
    }

    /// Distinct groups tracked so far.
    pub fn tracked_groups(&self) -> usize {
        self.seen.len()
    }

    /// True when this entry's blocks count; false when its sharing group
    /// was already charged to an earlier reference in this pass.
    pub fn charges(&mut self, nlink: u64, dev: u64, ino: u64, clone_id: Option<u64>) -> bool {
        match ChargeKey::of(nlink, dev, ino, clone_id) {
            None => true,
            Some(key) => {
                if self.seen.contains(&key) {
                    return false;
                }
                if self.seen.len() >= self.effective_cap() {
                    // Breaker: do not grow the set; charge this reference
                    // (and every later one of an untracked group).
                    self.saturated = true;
                    return true;
                }
                self.seen.insert(key)
            }
        }
    }

    pub fn charges_entry(&mut self, e: &ScanEntry) -> bool {
        self.charges(e.nlink, e.dev, e.ino, e.clone_id)
    }
}

/// Format a byte count for humans: `0 B` … `1.5 KB` … `2.3 TB`.
/// DECIMAL SI units (1000), one decimal place above bytes — the same
/// counting the Swift app gets from ByteCountFormatter's `.file` style, so
/// the CLI, the app, and Finder all say the same number for the same bytes.
/// (`du`/`df` print binary GiB; a du comparison must convert. Binary math
/// under a "GB" label — what this function did before phantom-2gw — is the
/// one combination that is always wrong.)
pub fn format_size(bytes: u64) -> String {
    const KB: u64 = 1000;
    const MB: u64 = 1000 * KB;
    const GB: u64 = 1000 * MB;
    const TB: u64 = 1000 * GB;
    const PB: u64 = 1000 * TB;
    const EB: u64 = 1000 * PB;

    // PB and EB rungs (phantom-ap6): a clamped i64::MAX total is 9.2 EB and
    // must read as such, not as "9223372036.9 TB".
    if bytes >= EB {
        format!("{:.1} EB", bytes as f64 / EB as f64)
    } else if bytes >= PB {
        format!("{:.1} PB", bytes as f64 / PB as f64)
    } else if bytes >= TB {
        format!("{:.1} TB", bytes as f64 / TB as f64)
    } else if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

/// Aggregate file disk sizes up into every ancestor directory within the
/// scan. Every scanned directory gets a key (empty dirs report 0); ancestors
/// above the scan root are not invented. Hardlink-deduped: an inode's bytes
/// land in the first link's ancestors only.
pub fn directory_disk_totals(entries: &[ScanEntry]) -> HashMap<String, u64> {
    let mut totals: HashMap<String, u64> = entries
        .iter()
        .filter(|e| e.is_dir)
        .map(|e| (e.path.clone(), 0))
        .collect();

    let mut links = LinkCharger::new();
    for entry in entries.iter().filter(|e| !e.is_dir) {
        if !links.charges_entry(entry) {
            continue;
        }
        let mut ancestor = entry.parent_path.as_deref().map(Path::new);
        while let Some(dir) = ancestor {
            let key = dir.to_string_lossy();
            match totals.get_mut(key.as_ref()) {
                Some(total) => {
                    *total += entry.disk_size;
                    ancestor = dir.parent();
                }
                // First non-scanned ancestor == we've left the scan root.
                None => break,
            }
        }
    }
    totals
}

/// Disk usage grouped by file type (lowercased extension). `file_type: None`
/// buckets directories' files with no extension; directories themselves are
/// excluded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileTypeTotal {
    /// `None` == no extension.
    pub file_type: Option<String>,
    pub disk_size: u64,
    pub file_count: u64,
}

/// Group files by type, largest disk footprint first (ties broken by type
/// name so output is deterministic). Hardlink-deduped: an inode's bytes land
/// in the first link's type bucket; further links still count toward
/// `file_count` (it counts directory entries, not inodes).
pub fn totals_by_file_type(entries: &[ScanEntry]) -> Vec<FileTypeTotal> {
    let mut by_type: HashMap<Option<String>, (u64, u64)> = HashMap::new();
    let mut links = LinkCharger::new();
    for entry in entries.iter().filter(|e| !e.is_dir) {
        let bucket = by_type.entry(entry.file_type.clone()).or_insert((0, 0));
        if links.charges_entry(entry) {
            bucket.0 += entry.disk_size;
        }
        bucket.1 += 1;
    }

    let mut totals: Vec<FileTypeTotal> = by_type
        .into_iter()
        .map(|(file_type, (disk_size, file_count))| FileTypeTotal {
            file_type,
            disk_size,
            file_count,
        })
        .collect();
    totals.sort_by(|a, b| {
        b.disk_size
            .cmp(&a.disk_size)
            .then_with(|| a.file_type.cmp(&b.file_type))
    });
    totals
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_formatter_has_pb_and_eb_rungs() {
        assert_eq!(format_size(1_500_000_000_000_000), "1.5 PB");
        assert_eq!(format_size(i64::MAX as u64), "9.2 EB");
        assert_eq!(format_size(u64::MAX), "18.4 EB");
        assert_eq!(format_size(999_000_000_000_000), "999.0 TB", "below the PB rung stays TB");
    }

    /// phantom-d1h: below the cap every group dedupes; at the cap an
    /// UNTRACKED group is charged at every reference and the charger says
    /// it saturated, while groups tracked before the cap keep deduping.
    #[test]
    fn link_charger_breaker_charges_untracked_groups_past_the_cap() {
        let mut c = LinkCharger::with_cap(2);
        assert!(c.charges(2, 1, 100, None), "group A first reference");
        assert!(!c.charges(2, 1, 100, None), "group A second reference deduped");
        assert!(c.charges(2, 1, 200, None), "group B first reference");
        assert_eq!(c.tracked_groups(), 2);
        assert!(!c.saturated());
        // Third distinct group: the set is full — charged, and charged again.
        assert!(c.charges(2, 1, 300, None));
        assert!(c.saturated());
        assert!(c.charges(2, 1, 300, None), "untracked group counts at every reference (over-count, bounded memory)");
        assert_eq!(c.tracked_groups(), 2, "the set did not grow");
        // Tracked groups still dedupe after the breaker tripped.
        assert!(!c.charges(2, 1, 100, None));
        assert!(!c.charges(2, 1, 200, None));
        // Ungrouped files never touch the set.
        assert!(c.charges(1, 1, 400, None));
        assert_eq!(c.tracked_groups(), 2);
    }

    #[test]
    fn link_charger_default_cap_is_ten_million_and_untripped() {
        let mut c = LinkCharger::new();
        for i in 0..1000u64 {
            assert!(c.charges(2, 1, i, None));
        }
        assert_eq!(c.tracked_groups(), 1000);
        assert!(!c.saturated());
        assert_eq!(LinkCharger::MAX_TRACKED_GROUPS, 10_000_000);
    }

    fn entry(path: &str, size: u64, is_dir: bool, parent: Option<&str>) -> ScanEntry {
        let file_type = if is_dir {
            None
        } else {
            Path::new(path)
                .extension()
                .and_then(|e| e.to_str())
                .map(|s| s.to_lowercase())
        };
        ScanEntry {
            path: path.to_string(),
            parent_path: parent.map(|s| s.to_string()),
            name: Path::new(path)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_string(),
            is_dir,
            disk_size: size,
            logical_size: size,
            modified_at: None,
            file_type,
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

    #[test]
    fn format_size_covers_every_unit_rung() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(999), "999 B");
        assert_eq!(format_size(1000), "1.0 KB");
        assert_eq!(format_size(1500), "1.5 KB");
        assert_eq!(format_size(1_000_000), "1.0 MB");
        assert_eq!(format_size(1_000_000_000), "1.0 GB");
        assert_eq!(format_size(1000_u64.pow(4)), "1.0 TB");
        assert_eq!(format_size(2_500_000_000_000), "2.5 TB");
    }

    // Mutation-proof (phantom-2gw): put the 1024 divisors back and every
    // assertion here fails — 1000 bytes reads "1000 B", a gigabyte reads
    // "976.6 MB". Decimal SI is the deliberate choice: it is what the Swift
    // app's .file style and Finder both say for the same bytes.
    #[test]
    fn format_size_is_decimal_si_not_binary() {
        assert_eq!(format_size(300_000_342_016), "300.0 GB", "the number Finder shows");
        assert_ne!(format_size(1_073_741_824), "1.0 GB", "2^30 bytes is 1.1 GB, not 1.0");
        assert_eq!(format_size(1_073_741_824), "1.1 GB");
    }

    #[test]
    fn format_size_boundaries_do_not_round_up_a_unit() {
        // One byte below each rung must stay in the smaller unit.
        assert_eq!(format_size(1_000_000 - 1), "1000.0 KB");
        assert_eq!(format_size(1_000_000_000 - 1), "1000.0 MB");
    }

    #[test]
    fn directory_totals_roll_up_to_every_ancestor() {
        let entries = vec![
            entry("/root", 0, true, None),
            entry("/root/src", 0, true, Some("/root")),
            entry("/root/src/main.rs", 500, false, Some("/root/src")),
            entry("/root/src/lib.rs", 300, false, Some("/root/src")),
            entry("/root/readme.txt", 200, false, Some("/root")),
        ];
        let totals = directory_disk_totals(&entries);
        assert_eq!(totals["/root"], 1000, "root sees all descendants, not just direct children");
        assert_eq!(totals["/root/src"], 800);
        assert_eq!(totals.len(), 2, "no ancestors above the scan root are invented");
    }

    #[test]
    fn empty_directory_reports_zero() {
        let entries = vec![
            entry("/root", 0, true, None),
            entry("/root/empty", 0, true, Some("/root")),
        ];
        let totals = directory_disk_totals(&entries);
        assert_eq!(totals["/root/empty"], 0);
        assert_eq!(totals["/root"], 0);
    }

    #[test]
    fn totals_by_type_groups_sorts_and_excludes_dirs() {
        let entries = vec![
            entry("/root", 0, true, None),
            entry("/root/a.rs", 500, false, Some("/root")),
            entry("/root/b.rs", 300, false, Some("/root")),
            entry("/root/c.txt", 600, false, Some("/root")),
            entry("/root/Makefile", 100, false, Some("/root")),
        ];
        let totals = totals_by_file_type(&entries);
        assert_eq!(
            totals,
            vec![
                FileTypeTotal { file_type: Some("rs".into()), disk_size: 800, file_count: 2 },
                FileTypeTotal { file_type: Some("txt".into()), disk_size: 600, file_count: 1 },
                FileTypeTotal { file_type: None, disk_size: 100, file_count: 1 },
            ]
        );
    }

    #[test]
    fn totals_by_type_breaks_size_ties_deterministically() {
        let entries = vec![
            entry("/root", 0, true, None),
            entry("/root/a.zzz", 400, false, Some("/root")),
            entry("/root/b.aaa", 400, false, Some("/root")),
        ];
        let types: Vec<Option<String>> = totals_by_file_type(&entries)
            .into_iter()
            .map(|t| t.file_type)
            .collect();
        assert_eq!(types, vec![Some("aaa".into()), Some("zzz".into())]);
    }

    /// Same file `entry` builds, plus explicit link identity.
    fn linked(path: &str, size: u64, parent: &str, nlink: u64, ino: u64) -> ScanEntry {
        let mut e = entry(path, size, false, Some(parent));
        e.nlink = nlink;
        e.dev = 7;
        e.ino = ino;
        e
    }

    // Mutation-proof: drop the `charges_entry` guard in directory_disk_totals
    // and /root reads 2000 (the inode counted twice); charge the SECOND link
    // instead of the first and /root/b gets the bytes.
    #[test]
    fn directory_totals_charge_a_hardlinked_inode_once_to_the_first_link() {
        let entries = vec![
            entry("/root", 0, true, None),
            entry("/root/a", 0, true, Some("/root")),
            entry("/root/b", 0, true, Some("/root")),
            linked("/root/a/one.bin", 1000, "/root/a", 2, 42),
            linked("/root/b/two.bin", 1000, "/root/b", 2, 42),
        ];
        let totals = directory_disk_totals(&entries);
        assert_eq!(totals["/root"], 1000, "one inode, counted once");
        assert_eq!(totals["/root/a"], 1000, "first link in walk order carries the bytes");
        assert_eq!(totals["/root/b"], 0, "the second link adds nothing");
    }

    #[test]
    fn type_totals_charge_a_hardlinked_inode_once_but_count_every_link() {
        let entries = vec![
            entry("/root", 0, true, None),
            linked("/root/first.log", 4096, "/root", 2, 42),
            linked("/root/second.txt", 4096, "/root", 2, 42),
            linked("/root/solo.txt", 100, "/root", 1, 43),
        ];
        let totals = totals_by_file_type(&entries);
        assert_eq!(
            totals,
            vec![
                // The shared inode's bytes land in the FIRST link's bucket…
                FileTypeTotal { file_type: Some("log".into()), disk_size: 4096, file_count: 1 },
                // …its second link still counts as an entry of its own type…
                FileTypeTotal { file_type: Some("txt".into()), disk_size: 100, file_count: 2 },
            ]
        );
    }

    #[test]
    fn distinct_inodes_with_nlink_above_one_all_charge() {
        // nlink > 1 does NOT mean "duplicate" — the other links may live
        // outside the scan. Two different inodes must both count.
        let entries = vec![
            entry("/root", 0, true, None),
            linked("/root/x.bin", 300, "/root", 2, 1),
            linked("/root/y.bin", 500, "/root", 2, 2),
        ];
        let totals = directory_disk_totals(&entries);
        assert_eq!(totals["/root"], 800);
    }

    // The shared fixture pins the /types wire shape for the Swift suite and
    // the OPE conformance harness, exactly like the scan fixtures.
    const RAW_TYPES: &str = include_str!("../../../tests/fixtures/types.json");

    #[test]
    fn file_type_totals_decode_from_raw_fixture_bytes() {
        let totals: Vec<FileTypeTotal> = serde_json::from_str(RAW_TYPES).unwrap();
        assert_eq!(
            totals,
            vec![
                FileTypeTotal { file_type: Some("log".into()), disk_size: 2_097_152, file_count: 3 },
                FileTypeTotal { file_type: Some("bin".into()), disk_size: 1_048_576, file_count: 42 },
                FileTypeTotal { file_type: Some("rs".into()), disk_size: 1_048_576, file_count: 7 },
                FileTypeTotal { file_type: None, disk_size: 4096, file_count: 1 },
            ]
        );
        // The fixture's order IS the contract order (disk desc, name tiebreak
        // on the bin/rs tie, null-type bucket where its size puts it) — the
        // exact order totals_by_file_type produces and the store reads back.
        let names: Vec<Option<&str>> =
            totals.iter().map(|t| t.file_type.as_deref()).collect();
        assert_eq!(names, vec![Some("log"), Some("bin"), Some("rs"), None]);
    }

    #[test]
    fn file_type_total_wire_shape_is_camel_case() {
        let t = FileTypeTotal {
            file_type: None,
            disk_size: 42,
            file_count: 1,
        };
        let v = serde_json::to_value(&t).unwrap();
        let obj = v.as_object().unwrap();
        assert!(obj.contains_key("fileType"));
        assert!(obj["fileType"].is_null(), "nullable present-as-null");
        assert!(obj.contains_key("diskSize"));
        assert!(obj.contains_key("fileCount"));
    }
}
