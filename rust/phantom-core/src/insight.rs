// Insight views (v1.1 Phase 3, phantom-mkn.9): pure compositions over what
// a scan already persisted, shaped for the question an agent actually asks.
//
// - `explain`: "why is THIS path here, and what would deleting it do?" —
//   the entry's three sizes, its sharing facts, the hotspot group that
//   classified it, and the unreadable subtrees beneath it, in one object
//   with a one-sentence verdict.
// - `stale_projects`: "which projects have gone quiet?" — the staleness
//   rule's per-project record, re-thresholded at read time.
//
// Nothing here walks a disk. Both are arithmetic over persisted rows and
// the stored summary, so three surfaces (HTTP, CLI, MCP) answer identically.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::classify::{Category, HotspotGroup, HotspotsSummary, ProjectActivity, RiskTier};
use crate::format::format_size;
use crate::scan::{EntryFlags, Scan, ScanEntry, UnreadablePath};

/// The hotspot group a path belongs to, pared to what explains it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PathHotspot {
    pub rule_id: String,
    pub label: String,
    pub risk_tier: RiskTier,
    pub why: String,
    pub command: Option<String>,
    pub hint: String,
    /// `topPath` when the path is (or lies under) one of the group's listed
    /// roots; `category` when it was matched by category alone (the group's
    /// root list is capped, so a deep file can miss it).
    pub matched_by: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PathExplanation {
    pub path: String,
    pub is_dir: bool,
    /// Allocated disk bytes — THE size.
    pub disk_size: u64,
    /// Apparent bytes; the du-lie when it dwarfs diskSize.
    pub logical_size: u64,
    /// What deleting this path actually frees; null == not recorded (pre-v5).
    pub private_size: Option<u64>,
    /// Bytes pinned by clones / hard links outside it, or snapshots.
    pub shared_size: Option<u64>,
    /// The entry's filesystem flags, as on the wire.
    pub flags: Vec<String>,
    /// A cloud placeholder whose bytes are not local (SF_DATALESS).
    pub dataless: bool,
    pub clone_id: Option<u64>,
    pub nlink: u64,
    pub category: Option<Category>,
    pub hotspot: Option<PathHotspot>,
    /// The scan's unreadable-path SAMPLE filtered to this path and below.
    pub unreadable_below: Vec<UnreadablePath>,
    pub unreadable_below_count: u64,
    /// The verdict in one sentence. Human text; wording may change.
    pub summary: String,
}

/// Compose the explanation for a persisted entry.
pub fn explain(entry: &ScanEntry, summary: &HotspotsSummary, unreadable: &[UnreadablePath]) -> PathExplanation {
    let flags = entry.flags.unwrap_or(EntryFlags::empty());
    let flag_names: Vec<String> = flags.names().into_iter().map(str::to_string).collect();
    let dataless = flags.contains(EntryFlags::DATALESS);
    let category = entry
        .category
        .as_deref()
        .and_then(|c| c.parse::<Category>().ok());
    let hotspot = find_group(&entry.path, category, &summary.groups);
    let unreadable_below: Vec<UnreadablePath> = unreadable
        .iter()
        .filter(|u| u.path == entry.path || is_under(&u.path, &entry.path))
        .cloned()
        .collect();
    let explanation_summary = sentence(entry, dataless, category, hotspot.as_ref(), unreadable_below.len());
    PathExplanation {
        path: entry.path.clone(),
        is_dir: entry.is_dir,
        disk_size: entry.disk_size,
        logical_size: entry.logical_size,
        private_size: entry.private_size,
        shared_size: entry.shared_size,
        flags: flag_names,
        dataless,
        clone_id: entry.clone_id,
        nlink: entry.nlink,
        category,
        hotspot,
        unreadable_below_count: unreadable_below.len() as u64,
        unreadable_below,
        summary: explanation_summary,
    }
}

fn is_under(path: &str, root: &str) -> bool {
    let root = root.trim_end_matches('/');
    path.len() > root.len() + 1 && path.starts_with(root) && path.as_bytes()[root.len()] == b'/'
}

fn find_group(path: &str, category: Option<Category>, groups: &[HotspotGroup]) -> Option<PathHotspot> {
    let pared = |g: &HotspotGroup, matched_by: &str| PathHotspot {
        rule_id: g.rule_id.clone(),
        label: g.label.clone(),
        risk_tier: g.risk_tier,
        why: g.why.clone(),
        command: g.command.clone(),
        hint: g.hint.clone(),
        matched_by: matched_by.to_string(),
    };
    if let Some(g) = groups
        .iter()
        .find(|g| g.top_paths.iter().any(|p| p == path || is_under(path, p)))
    {
        return Some(pared(g, "topPath"));
    }
    let category = category?;
    groups
        .iter()
        .find(|g| g.category == category)
        .map(|g| pared(g, "category"))
}

fn sentence(
    entry: &ScanEntry,
    dataless: bool,
    category: Option<Category>,
    hotspot: Option<&PathHotspot>,
    unreadable: usize,
) -> String {
    let kind = if entry.is_dir { "directory" } else { "file" };
    let mut parts = vec![format!(
        "{kind}: {} on disk ({} apparent)",
        format_size(entry.disk_size),
        format_size(entry.logical_size)
    )];
    if dataless {
        parts.push("a cloud placeholder whose bytes are not local — deleting it frees almost nothing".into());
    } else if let Some(private) = entry.private_size {
        if private + entry.disk_size / 100 < entry.disk_size {
            parts.push(format!(
                "deleting frees {} — the other {} is pinned by clones, hard links or snapshots",
                format_size(private),
                format_size(entry.shared_size.unwrap_or(entry.disk_size - private))
            ));
        } else {
            parts.push(format!("deleting frees {}", format_size(private)));
        }
    }
    if entry.nlink > 1 {
        parts.push(format!("{} hard links share this inode", entry.nlink));
    }
    match (category, hotspot) {
        (Some(c), Some(h)) => parts.push(format!(
            "classified {} ({}): {}",
            c.as_str(),
            h.risk_tier.as_str(),
            h.why.trim_end_matches('.')
        )),
        (Some(c), None) => parts.push(format!("classified {}", c.as_str())),
        (None, _) => parts.push("not a hotspot: ordinary content, nothing to suggest".into()),
    }
    if unreadable > 0 {
        parts.push(format!(
            "{unreadable} unreadable entr{} below it (permissions or TCC) — the sizes above undercount",
            if unreadable == 1 { "y" } else { "ies" }
        ));
    }
    let mut s = parts.join("; ");
    s.push('.');
    s
}

// --- Stale projects -------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StaleProject {
    pub root: String,
    /// Days since the newest git activity or source edit, as of the scan.
    pub last_activity_days: i64,
    /// Σ artifacts' diskSize.
    pub artifact_disk_size: u64,
    pub artifacts: Vec<crate::classify::ProjectArtifact>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StaleProjects {
    pub scan_id: Uuid,
    pub root_path: String,
    /// The threshold applied here (days). Independent of the scan's own.
    pub threshold_days: i64,
    /// How many project roots the rule evaluated in the scan.
    pub projects_evaluated: u64,
    /// Roots with no dated evidence — never stale, listed nowhere.
    pub unverifiable: u64,
    /// Dormant at `thresholdDays`, biggest artifact bytes first.
    pub projects: Vec<StaleProject>,
    pub artifact_disk_size: u64,
}

/// Re-threshold the persisted per-project activity. A project is stale
/// when `lastActivityDays ≥ threshold_days`; unverifiable roots (null) are
/// never stale, whatever the threshold.
pub fn stale_projects(scan: &Scan, summary: &HotspotsSummary, threshold_days: i64) -> StaleProjects {
    let mut projects: Vec<StaleProject> = summary
        .projects
        .iter()
        .filter_map(|p: &ProjectActivity| {
            let days = p.last_activity_days?;
            (days >= threshold_days).then(|| StaleProject {
                root: p.root.clone(),
                last_activity_days: days,
                artifact_disk_size: p.artifacts.iter().map(|a| a.disk_size).sum(),
                artifacts: p.artifacts.clone(),
            })
        })
        .collect();
    projects.sort_by(|a, b| {
        b.artifact_disk_size
            .cmp(&a.artifact_disk_size)
            .then_with(|| b.last_activity_days.cmp(&a.last_activity_days))
            .then_with(|| a.root.cmp(&b.root))
    });
    StaleProjects {
        scan_id: scan.id,
        root_path: scan.root_path.clone(),
        threshold_days,
        projects_evaluated: summary.projects.len() as u64,
        unverifiable: summary.projects.iter().filter(|p| p.last_activity_days.is_none()).count() as u64,
        artifact_disk_size: projects.iter().map(|p| p.artifact_disk_size).sum(),
        projects,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RAW_ENTRY: &str = include_str!("../../../tests/fixtures/entry.json");
    const RAW_HOTSPOTS: &str = include_str!("../../../tests/fixtures/hotspots-summary.json");
    const RAW_SCAN: &str = include_str!("../../../tests/fixtures/scan-complete.json");
    const RAW_EXPLANATION: &str = include_str!("../../../tests/fixtures/path-explanation.json");
    const RAW_STALE: &str = include_str!("../../../tests/fixtures/stale-projects.json");

    fn summary() -> HotspotsSummary {
        serde_json::from_str(RAW_HOTSPOTS).unwrap()
    }
    fn scan() -> Scan {
        serde_json::from_str(RAW_SCAN).unwrap()
    }

    #[test]
    fn a_pure_clone_explains_its_pinned_bytes_and_is_not_a_hotspot() {
        let entry: ScanEntry = serde_json::from_str(RAW_ENTRY).unwrap();
        let x = explain(&entry, &summary(), &[]);
        assert_eq!(x.path, "/Users/ghost/Code/phantom/Cargo.lock");
        assert!(!x.is_dir);
        assert_eq!(x.private_size, Some(0));
        assert_eq!(x.shared_size, Some(49152));
        assert_eq!(x.clone_id, Some(42424242));
        assert!(!x.dataless);
        assert_eq!(x.flags, vec!["mayShareBlocks", "sharesAllBlocks"], "the wire names, as on the entry");
        assert_eq!(x.category, None);
        assert_eq!(x.hotspot, None);
        assert!(x.summary.contains("deleting frees 0 B"), "{}", x.summary);
        assert!(x.summary.contains("pinned by clones"), "{}", x.summary);
        assert!(x.summary.contains("not a hotspot"), "{}", x.summary);
        assert!(x.summary.ends_with('.'));
    }

    fn artifact_entry(path: &str) -> ScanEntry {
        let mut e: ScanEntry = serde_json::from_str(RAW_ENTRY).unwrap();
        e.path = path.to_string();
        e.is_dir = true;
        e.disk_size = 17_179_869_184;
        e.logical_size = 18_179_869_184;
        e.private_size = Some(17_179_869_184);
        e.shared_size = Some(0);
        e.clone_id = None;
        e.flags = Some(EntryFlags::empty());
        e.category = Some("staleProjectArtifact".into());
        e
    }

    #[test]
    fn a_hotspot_root_explains_through_its_group_by_top_path() {
        let e = artifact_entry("/Users/ghost/Code/dormant/target");
        let x = explain(&e, &summary(), &[]);
        let h = x.hotspot.as_ref().unwrap();
        assert_eq!(h.rule_id, "cargo-target");
        assert_eq!(h.risk_tier, RiskTier::Safe);
        assert_eq!(h.command.as_deref(), Some("cargo clean"));
        assert_eq!(h.matched_by, "topPath");
        assert_eq!(x.category, Some(Category::StaleProjectArtifact));
        assert!(x.summary.contains("classified staleProjectArtifact (safe)"), "{}", x.summary);
        // A file deep under the root matches the same way.
        let deep = explain(&artifact_entry("/Users/ghost/Code/dormant/target/debug/deps/x.rlib"), &summary(), &[]);
        assert_eq!(deep.hotspot.as_ref().unwrap().matched_by, "topPath");
        // A prefix sibling does NOT.
        let sib = explain(&artifact_entry("/Users/ghost/Code/dormant/target-two"), &summary(), &[]);
        assert_eq!(sib.hotspot.as_ref().map(|h| h.matched_by.as_str()), Some("category"), "falls back to the category match");
    }

    #[test]
    fn unreadable_sample_is_filtered_to_the_subtree() {
        let e = artifact_entry("/Users/ghost/Code");
        let unreadable = vec![
            UnreadablePath { path: "/Users/ghost/Code/locked".into(), reason: "Permission denied (os error 13)".into() },
            UnreadablePath { path: "/Users/ghost/Library/Mail".into(), reason: "Operation not permitted (os error 1)".into() },
            UnreadablePath { path: "/Users/ghost/Code".into(), reason: "Operation not permitted (os error 1)".into() },
        ];
        let x = explain(&e, &summary(), &unreadable);
        assert_eq!(x.unreadable_below_count, 2, "the path itself and its subtree, not siblings");
        assert!(x.summary.contains("2 unreadable entries below it"), "{}", x.summary);
        let one = explain(&e, &summary(), &unreadable[..1]);
        assert!(one.summary.contains("1 unreadable entry below it"), "{}", one.summary);
    }

    #[test]
    fn dataless_placeholder_is_named_as_such() {
        let mut e = artifact_entry("/Users/ghost/Library/CloudStorage/OneDrive/big.pptx");
        e.is_dir = false;
        e.flags = Some(EntryFlags::DATALESS);
        e.category = Some("cloudDataloaded".into());
        e.disk_size = 147_456;
        e.logical_size = 154_140_672;
        e.private_size = Some(147_456);
        e.shared_size = Some(0);
        let x = explain(&e, &summary(), &[]);
        assert!(x.dataless);
        assert_eq!(x.flags, vec!["dataless"]);
        assert!(x.summary.contains("cloud placeholder"), "{}", x.summary);
        assert_eq!(x.hotspot.as_ref().unwrap().rule_id, "cloud-dataloaded");
    }

    #[test]
    fn explanation_fixture_round_trips() {
        let x: PathExplanation = serde_json::from_str(RAW_EXPLANATION).unwrap();
        let raw: serde_json::Value = serde_json::from_str(RAW_EXPLANATION).unwrap();
        assert_eq!(serde_json::to_value(&x).unwrap(), raw);
        assert_eq!(x.hotspot.as_ref().unwrap().rule_id, "cargo-target");
    }

    // --- stale ------------------------------------------------------------------

    #[test]
    fn stale_re_thresholds_and_never_lists_unverifiable_roots() {
        let s = summary();
        let at90 = stale_projects(&scan(), &s, 90);
        assert_eq!(at90.projects_evaluated, 3);
        assert_eq!(at90.unverifiable, 1);
        assert_eq!(at90.projects.len(), 1);
        assert_eq!(at90.projects[0].root, "/Users/ghost/Code/dormant");
        assert_eq!(at90.projects[0].artifact_disk_size, 17_179_869_184);
        assert_eq!(at90.artifact_disk_size, 17_179_869_184);
        assert_eq!(at90.threshold_days, 90);
        // Lower the bar: the active project (0 days) qualifies at 0, the
        // undated one never does.
        let at0 = stale_projects(&scan(), &s, 0);
        let roots: Vec<&str> = at0.projects.iter().map(|p| p.root.as_str()).collect();
        assert_eq!(roots, ["/Users/ghost/Code/dormant", "/Users/ghost/Code/phantom"], "biggest artifacts first");
        // Raise it: nothing.
        assert!(stale_projects(&scan(), &s, 121).projects.is_empty());
        assert_eq!(stale_projects(&scan(), &s, 120).projects.len(), 1, "threshold is inclusive");
    }

    #[test]
    fn stale_fixture_round_trips() {
        let x: StaleProjects = serde_json::from_str(RAW_STALE).unwrap();
        let raw: serde_json::Value = serde_json::from_str(RAW_STALE).unwrap();
        assert_eq!(serde_json::to_value(&x).unwrap(), raw);
        assert_eq!(x.projects[0].artifacts[0].rule_id, "cargo-target");
    }
}
