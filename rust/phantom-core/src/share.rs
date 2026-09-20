// The sharing ledger: which files share blocks with which, and whether ALL
// the sharers are inside the scan. `LinkCharger` answers "count these bytes
// once" (the du model, the headline `diskSize`); this answers the question
// deletion actually asks — "if I remove THIS set of paths, does anything
// still pin the blocks?" — for `privateSize` / `sharedSize` (phantom-mkn.2).
//
// A group is a `ChargeKey`: one hardlinked inode, or one APFS pure-clone
// stream (every member shares ALL its blocks, so the allocation is one
// number for the whole group). Deleting a set of paths frees the group's
// allocation only if the set holds EVERY reference: every hard link
// (`nlink`) of every member inode, and every clone (`clone_refcnt`). A
// reference outside the scan root can never be in the set, so such a group
// is never freeable from inside this scan — it is `shared`, wherever it is
// looked at from.
//
// Honest limits, stated once: a partially-modified clone has its OWN clone
// id yet still references most of the original's blocks; nothing per-file
// says who else holds an extent. So a pure-clone group is assumed to own
// its blocks (over-estimate when a modified sibling exists elsewhere), and
// a modified clone contributes exactly its kernel-reported private bytes
// (its shared portion is always reported as shared, even when the sharer is
// inside the same directory). Snapshot-trapped blocks are excluded from
// `privateSize` by the kernel, which is the truth deletion will discover.

use std::collections::HashMap;

use crate::format::ChargeKey;
use crate::scan::ScanEntry;

#[derive(Debug, Clone, Default)]
struct Group {
    /// Allocation of the shared stream/inode — identical on every member.
    alloc: u64,
    /// Paths in the scan that reference this group.
    seen: u64,
    /// Distinct inodes seen, each with its `nlink` (a clone group spans
    /// several inodes; a hardlink group is exactly one).
    inodes: HashMap<u64, u64>,
    /// `clone_refcnt` as the filesystem reported it: the number of full
    /// clones in the stream. `None` when unknown (synthetic entries, or a
    /// hardlink-only group) — then the members seen are taken as all of
    /// them.
    refcnt: Option<u64>,
}

impl Group {
    /// Every reference to these blocks lies inside the scan.
    fn complete(&self) -> bool {
        let all_inodes = self.refcnt.is_none_or(|r| r <= self.inodes.len() as u64);
        let all_links = self.seen >= self.inodes.values().sum::<u64>();
        all_inodes && all_links
    }
}

/// Per-scan facts about every sharing group, built from the FULL walk.
#[derive(Debug, Clone, Default)]
pub struct ShareLedger {
    groups: HashMap<ChargeKey, Group>,
}

impl ShareLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one FILE reference. `clone_refcnt` is the filesystem's count
    /// of full clones when known.
    pub fn observe(&mut self, e: &ScanEntry, clone_refcnt: Option<u64>) {
        let Some(key) = ChargeKey::of_entry(e) else { return };
        let g = self.groups.entry(key).or_default();
        if g.seen == 0 {
            g.alloc = e.disk_size;
        }
        g.seen += 1;
        g.inodes.entry(e.ino).or_insert(e.nlink);
        if let Some(r) = clone_refcnt {
            g.refcnt = Some(g.refcnt.map_or(r, |prev| prev.max(r)));
        }
    }

    /// A ledger over already-walked entries with no clone reference counts
    /// (tests, and re-derivation from persisted rows): clone groups are
    /// taken to be complete when every seen member's links are present.
    pub fn from_entries(entries: &[ScanEntry]) -> Self {
        let mut ledger = Self::new();
        for e in entries.iter().filter(|e| !e.is_dir) {
            ledger.observe(e, None);
        }
        ledger
    }

    /// Paths in the scan referencing this group (0 for an unknown key).
    pub fn seen(&self, key: ChargeKey) -> u64 {
        self.groups.get(&key).map_or(0, |g| g.seen)
    }

    /// The group's allocation — what freeing ALL of it returns.
    pub fn alloc(&self, key: ChargeKey) -> u64 {
        self.groups.get(&key).map_or(0, |g| g.alloc)
    }

    /// True when every reference to the group lies inside this scan.
    pub fn complete(&self, key: ChargeKey) -> bool {
        self.groups.get(&key).is_some_and(Group::complete)
    }

    /// Bytes freed by deleting a set of paths that contains `subset_seen`
    /// references to this group: the whole allocation when the set holds
    /// every reference in existence, otherwise nothing — the blocks stay
    /// pinned by the references left behind.
    pub fn freed_by(&self, key: ChargeKey, subset_seen: u64) -> u64 {
        match self.groups.get(&key) {
            Some(g) if g.complete() && subset_seen >= g.seen => g.alloc,
            _ => 0,
        }
    }

    /// What deleting exactly this ungrouped file frees: the kernel's private
    /// bytes when recorded, else its allocation (the pre-v5 assumption).
    pub fn ungrouped_private(e: &ScanEntry) -> u64 {
        e.private_size.unwrap_or(e.disk_size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, disk: u64, nlink: u64, ino: u64, clone_id: Option<u64>) -> ScanEntry {
        ScanEntry {
            path: path.to_string(),
            parent_path: Some("/r".to_string()),
            name: path.rsplit('/').next().unwrap().to_string(),
            is_dir: false,
            disk_size: disk,
            logical_size: disk,
            modified_at: None,
            file_type: None,
            category: None,
            nlink,
            dev: 1,
            ino,
            file_count: None,
            dir_count: None,
            private_size: None,
            shared_size: None,
            clone_id,
            flags: None,
        }
    }

    #[test]
    fn hardlink_group_with_every_link_seen_is_complete() {
        let entries = vec![file("/r/a", 100, 2, 7, None), file("/r/b", 100, 2, 7, None)];
        let ledger = ShareLedger::from_entries(&entries);
        let key = ChargeKey::Inode { dev: 1, ino: 7 };
        assert_eq!(ledger.seen(key), 2);
        assert!(ledger.complete(key));
        assert_eq!(ledger.freed_by(key, 2), 100, "both links in the set: freed");
        assert_eq!(ledger.freed_by(key, 1), 0, "one link left behind pins the blocks");
    }

    // Mutation-proof: compare `seen` against the inode count instead of the
    // nlink sum and this passes a group with a link OUTSIDE the scan.
    #[test]
    fn hardlink_with_a_link_outside_the_scan_is_never_freeable() {
        let entries = vec![file("/r/a", 100, 3, 7, None), file("/r/b", 100, 3, 7, None)];
        let ledger = ShareLedger::from_entries(&entries);
        let key = ChargeKey::Inode { dev: 1, ino: 7 };
        assert!(!ledger.complete(key), "nlink 3 but only 2 paths seen");
        assert_eq!(ledger.freed_by(key, 2), 0);
    }

    #[test]
    fn clone_group_completeness_uses_the_refcnt_when_known() {
        // Two pure clones (distinct inodes, same stream), refcnt says 3.
        let a = file("/r/a", 4096, 1, 10, Some(99));
        let b = file("/r/b", 4096, 1, 11, Some(99));
        let mut ledger = ShareLedger::new();
        ledger.observe(&a, Some(3));
        ledger.observe(&b, Some(3));
        let key = ChargeKey::Clone { dev: 1, clone_id: 99 };
        assert_eq!(ledger.seen(key), 2);
        assert!(!ledger.complete(key), "a third clone lives outside the scan");
        assert_eq!(ledger.freed_by(key, 2), 0);

        // Same two, refcnt 2: everything is here.
        let mut ledger = ShareLedger::new();
        ledger.observe(&a, Some(2));
        ledger.observe(&b, Some(2));
        assert!(ledger.complete(key));
        assert_eq!(ledger.freed_by(key, 2), 4096);
        assert_eq!(ledger.alloc(key), 4096);
    }

    #[test]
    fn clone_group_with_a_hardlinked_member_needs_every_link_too() {
        // a and hard are one inode (nlink 2), b is a clone: 3 paths, 2 inodes.
        let a = file("/r/a", 4096, 2, 10, Some(10));
        let hard = file("/r/hard", 4096, 2, 10, Some(10));
        let b = file("/r/b", 4096, 1, 11, Some(10));
        let key = ChargeKey::Clone { dev: 1, clone_id: 10 };
        let mut ledger = ShareLedger::new();
        ledger.observe(&a, Some(2));
        ledger.observe(&b, Some(2));
        assert!(!ledger.complete(key), "hard link not yet seen");
        ledger.observe(&hard, Some(2));
        assert!(ledger.complete(key));
        assert_eq!(ledger.freed_by(key, 3), 4096);
        assert_eq!(ledger.freed_by(key, 2), 0);
    }

    #[test]
    fn unknown_key_and_ungrouped_files() {
        let ledger = ShareLedger::from_entries(&[file("/r/solo", 50, 1, 1, None)]);
        let key = ChargeKey::Inode { dev: 1, ino: 1 };
        assert_eq!(ledger.seen(key), 0, "nlink 1 files are not groups");
        assert_eq!(ledger.freed_by(key, 1), 0);
        let mut solo = file("/r/solo", 50, 1, 1, None);
        assert_eq!(ShareLedger::ungrouped_private(&solo), 50, "no record: the allocation");
        solo.private_size = Some(8);
        assert_eq!(ShareLedger::ungrouped_private(&solo), 8, "recorded: the kernel's number");
    }
}
