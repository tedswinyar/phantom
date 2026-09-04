// Scan diff (phantom-081): what changed between two scans of the same root.
// Pure computation — the store supplies each scan's persisted DIRECTORY
// aggregates; validation (same root, both complete) belongs to the caller.
//
// Direction is positional, not temporal: `b` is "after", so a positive
// delta means B is bigger. The obvious call passes the older scan as `a`;
// nothing here checks timestamps, and the wire echoes both ids so a reader
// can always tell which way the arrow points.
//
// The grown/freed lists are HOTSPOTS OF CHANGE, not a ledger — capped at
// [`DIFF_TOP_N`] per direction and floored at [`DIFF_MIN_DELTA`] (persisted
// dir totals fold sub-1-MiB files, so smaller per-directory deltas are
// below the data's own granularity). The top-level deltas are exact.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

use crate::scan::Scan;

/// Directories reported per direction, largest |delta| first.
pub const DIFF_TOP_N: usize = 20;
/// Per-directory deltas smaller than this are noise at the persistence
/// layer's own granularity (the 1 MiB file-row fold) and are not listed.
pub const DIFF_MIN_DELTA: u64 = 1_048_576;

/// One directory's change. `before`/`after` are the persisted aggregate
/// disk sizes; null means the directory was absent from that scan (created
/// or deleted between the two) and its side counts as 0 in `delta`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffEntry {
    pub path: String,
    pub before: Option<u64>,
    pub after: Option<u64>,
    /// after − before (absent side = 0), saturating at the i64 range.
    pub delta: i64,
}

/// The wire view of a diff. Top-level deltas come from the scan rows and
/// are exact; `grown`/`freed` list the biggest per-directory movements.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanDiff {
    /// The "before" side (first path segment of the request).
    pub scan_a: Uuid,
    /// The "after" side.
    pub scan_b: Uuid,
    /// When each side started. Echoed so a reader can tell whether the
    /// positional order matches chronological order — the deltas read B − A
    /// regardless, so if `scan_a_started_at` is LATER than
    /// `scan_b_started_at`, every sign is inverted from "what changed over
    /// time" (the sign-inversion trap the review caught: list_scans is
    /// newest-first, so the naive (newest, older) call flips every delta).
    #[serde(with = "crate::wire_time")]
    pub scan_a_started_at: DateTime<Utc>,
    #[serde(with = "crate::wire_time")]
    pub scan_b_started_at: DateTime<Utc>,
    /// Set when `scan_a` started AFTER `scan_b` (positional order is
    /// reverse-chronological): the deltas' signs are inverted relative to
    /// "what changed since the older scan". Null when the order is natural.
    pub reversed_chronology: Option<bool>,
    pub root_path: String,
    /// Exact: B's totalDiskSize − A's (hardlink-deduped like the totals).
    pub disk_delta: i64,
    pub logical_delta: i64,
    pub file_count_delta: i64,
    pub dir_count_delta: i64,
    pub error_count_delta: i64,
    /// Directories that got bigger, largest growth first (ties by path).
    pub grown: Vec<DiffEntry>,
    /// Directories that shrank, largest shrink first (ties by path).
    pub freed: Vec<DiffEntry>,
}

/// after − before without u64 wrap; saturates at the i64 range (the same
/// no-wrap posture as `blocks_to_bytes` — a clamped delta beats a wrong
/// sign).
fn signed_delta(before: u64, after: u64) -> i64 {
    i64::try_from(after as i128 - before as i128).unwrap_or(if after > before {
        i64::MAX
    } else {
        i64::MIN
    })
}

/// Diff two scans from their persisted directory aggregates.
/// `a_dirs`/`b_dirs` are (path, aggregated diskSize) for DIRECTORY rows
/// only — the store's `dir_sizes` view. Callers validate root equality and
/// completeness; this function just does the arithmetic.
pub fn diff(a: &Scan, a_dirs: &[(String, u64)], b: &Scan, b_dirs: &[(String, u64)]) -> ScanDiff {
    let before: HashMap<&str, u64> = a_dirs.iter().map(|(p, s)| (p.as_str(), *s)).collect();
    let after: HashMap<&str, u64> = b_dirs.iter().map(|(p, s)| (p.as_str(), *s)).collect();

    let mut changed: Vec<DiffEntry> = Vec::new();
    for (path, &b_size) in &after {
        let a_size = before.get(path).copied();
        let delta = signed_delta(a_size.unwrap_or(0), b_size);
        if delta.unsigned_abs() >= DIFF_MIN_DELTA {
            changed.push(DiffEntry {
                path: (*path).to_string(),
                before: a_size,
                after: Some(b_size),
                delta,
            });
        }
    }
    // Directories that vanished entirely (present in A only).
    for (path, &a_size) in &before {
        if !after.contains_key(path) {
            let delta = signed_delta(a_size, 0);
            if delta.unsigned_abs() >= DIFF_MIN_DELTA {
                changed.push(DiffEntry {
                    path: (*path).to_string(),
                    before: Some(a_size),
                    after: None,
                    delta,
                });
            }
        }
    }

    let mut grown: Vec<DiffEntry> = changed.iter().filter(|e| e.delta > 0).cloned().collect();
    grown.sort_by(|x, y| y.delta.cmp(&x.delta).then_with(|| x.path.cmp(&y.path)));
    grown.truncate(DIFF_TOP_N);

    let mut freed: Vec<DiffEntry> = changed.into_iter().filter(|e| e.delta < 0).collect();
    freed.sort_by(|x, y| x.delta.cmp(&y.delta).then_with(|| x.path.cmp(&y.path)));
    freed.truncate(DIFF_TOP_N);

    ScanDiff {
        scan_a: a.id,
        scan_b: b.id,
        scan_a_started_at: a.started_at,
        scan_b_started_at: b.started_at,
        // Only ever Some(true): a marker an agent/CLI can branch on without
        // parsing two timestamps. Null (not false) when the order is natural,
        // matching the wire's "present-as-null == the ordinary case" habit.
        reversed_chronology: (a.started_at > b.started_at).then_some(true),
        root_path: a.root_path.clone(),
        disk_delta: signed_delta(a.total_disk_size, b.total_disk_size),
        logical_delta: signed_delta(a.total_logical_size, b.total_logical_size),
        file_count_delta: signed_delta(a.file_count, b.file_count),
        dir_count_delta: signed_delta(a.dir_count, b.dir_count),
        error_count_delta: signed_delta(a.error_count, b.error_count),
        grown,
        freed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::ScanStatus;
    use chrono::Utc;

    fn scan(disk: u64, files: u64) -> Scan {
        scan_at(disk, files, "2026-03-17T14:30:00.000000Z")
    }

    fn scan_at(disk: u64, files: u64, started: &str) -> Scan {
        let started_at = crate::wire_time::from_wire(started).unwrap();
        Scan {
            id: Uuid::new_v4(),
            root_path: "/r".into(),
            status: ScanStatus::Complete,
            started_at,
            finished_at: Some(started_at),
            total_disk_size: disk,
            total_logical_size: disk,
            file_count: files,
            dir_count: 1,
            error_count: 0,
            unreadable_paths: Some(Vec::new()),
        }
    }

    const MIB: u64 = 1_048_576;

    fn dirs(v: &[(&str, u64)]) -> Vec<(String, u64)> {
        v.iter().map(|(p, s)| (p.to_string(), *s)).collect()
    }

    #[test]
    fn identical_scans_diff_to_zero_everywhere() {
        let a = scan(10 * MIB, 5);
        let b = scan(10 * MIB, 5);
        let d = dirs(&[("/r", 10 * MIB), ("/r/x", 4 * MIB)]);
        let out = diff(&a, &d, &b, &d);
        assert_eq!(out.disk_delta, 0);
        assert_eq!(out.file_count_delta, 0);
        assert!(out.grown.is_empty() && out.freed.is_empty());
    }

    // Mutation-proof: swap the a/b arguments anywhere inside `diff` and the
    // signs here flip; drop the vanished-dir loop and /r/gone disappears.
    #[test]
    fn grown_freed_and_vanished_directories_report_correctly() {
        let a = scan(100 * MIB, 50);
        let b = scan(70 * MIB, 40);
        let before = dirs(&[("/r", 100 * MIB), ("/r/big", 60 * MIB), ("/r/gone", 30 * MIB)]);
        let after = dirs(&[("/r", 70 * MIB), ("/r/big", 62 * MIB), ("/r/new", 8 * MIB)]);
        let out = diff(&a, &before, &b, &after);

        assert_eq!(out.disk_delta, -(30 * MIB as i64));
        assert_eq!(out.file_count_delta, -10);

        assert_eq!(
            out.grown,
            vec![
                DiffEntry { path: "/r/new".into(), before: None, after: Some(8 * MIB), delta: 8 * MIB as i64 },
                DiffEntry { path: "/r/big".into(), before: Some(60 * MIB), after: Some(62 * MIB), delta: 2 * MIB as i64 },
            ],
            "created dir carries before: null; growth sorted largest first"
        );
        assert_eq!(
            out.freed,
            vec![
                DiffEntry { path: "/r".into(), before: Some(100 * MIB), after: Some(70 * MIB), delta: -(30 * MIB as i64) },
                DiffEntry { path: "/r/gone".into(), before: Some(30 * MIB), after: None, delta: -(30 * MIB as i64) },
            ],
            "vanished dir carries after: null; -30MiB tie broken by path"
        );
    }

    // The sign-inversion trap (review C1): list_scans is newest-first, so a
    // naive (newest, older) call reads deltas reverse-chronologically. The
    // diff can't refuse it (direction is positional by design), but it MUST
    // flag it. Mutation-proof: flip the `>` to `<` in the reversed_chronology
    // line and this fails; drop the timestamps and the wire can't detect it.
    #[test]
    fn reverse_chronological_order_is_flagged_and_timestamps_echoed() {
        let older = scan_at(100 * MIB, 50, "2026-03-01T00:00:00.000000Z");
        let newer = scan_at(70 * MIB, 40, "2026-03-08T00:00:00.000000Z");
        let d = dirs(&[("/r", 0)]);

        // Natural order (older -> newer): no flag, disk shrank.
        let natural = diff(&older, &d, &newer, &d);
        assert_eq!(natural.reversed_chronology, None, "natural order is unflagged");
        assert_eq!(natural.disk_delta, -(30 * MIB as i64));
        assert_eq!(natural.scan_a_started_at, older.started_at);
        assert_eq!(natural.scan_b_started_at, newer.started_at);

        // Reversed (newer as A): the SAME trees now read +30 MiB, and the
        // flag is set so a consumer knows the sign is reverse-chronological.
        let reversed = diff(&newer, &d, &older, &d);
        assert_eq!(reversed.reversed_chronology, Some(true), "reverse order is flagged");
        assert_eq!(reversed.disk_delta, 30 * MIB as i64);
    }

    #[test]
    fn deltas_below_the_floor_are_not_listed_but_totals_stay_exact() {
        let a = scan(MIB, 1);
        let b = scan(MIB + 512, 1);
        let out = diff(
            &a,
            &dirs(&[("/r", MIB)]),
            &b,
            &dirs(&[("/r", MIB + 512)]),
        );
        assert_eq!(out.disk_delta, 512, "top-level delta is exact");
        assert!(out.grown.is_empty(), "512 B of dir movement is sub-floor noise");
    }

    #[test]
    fn lists_cap_at_top_n_keeping_the_largest() {
        let a = scan(0, 0);
        let b = scan(0, 0);
        let before = dirs(&[]);
        let after: Vec<(String, u64)> = (0..DIFF_TOP_N + 5)
            .map(|i| (format!("/r/d{i:03}"), (i as u64 + 1) * MIB))
            .collect();
        let out = diff(&a, &before, &b, &after);
        assert_eq!(out.grown.len(), DIFF_TOP_N);
        assert_eq!(
            out.grown[0].delta,
            (DIFF_TOP_N as i64 + 5) * MIB as i64,
            "the largest movement survives the cap"
        );
    }

    // A pre-v4 scan reads back with unreadable_paths: None; diffing it
    // against a post-v4 scan (Some) must not panic or misbehave. diff reads
    // only sizes/counts, never unreadable_paths — this pins that (TR gap).
    #[test]
    fn diffs_across_the_v4_boundary_ignoring_unreadable_paths() {
        let mut old = scan(50 * MIB, 30);
        old.unreadable_paths = None; // pre-v4 row
        let new = scan(60 * MIB, 35); // Some(vec![]) from the builder
        let out = diff(&old, &dirs(&[("/r", 0)]), &new, &dirs(&[("/r", 0)]));
        assert_eq!(out.disk_delta, 10 * MIB as i64);
        assert_eq!(out.file_count_delta, 5);
    }

    #[test]
    fn signed_delta_saturates_instead_of_wrapping() {
        assert_eq!(signed_delta(0, u64::MAX), i64::MAX);
        assert_eq!(signed_delta(u64::MAX, 0), i64::MIN);
        assert_eq!(signed_delta(7, 3), -4);
    }

    // The shared fixture pins the wire shape for the Swift suite and the
    // OPE conformance harness, exactly like the scan fixtures.
    const RAW_DIFF: &str = include_str!("../../../tests/fixtures/scan-diff.json");

    #[test]
    fn decodes_from_raw_fixture_bytes() {
        let d: ScanDiff = serde_json::from_str(RAW_DIFF).unwrap();
        assert_eq!(d.root_path, "/Users/ghost/Code");
        assert_eq!(d.disk_delta, -31_457_280);
        assert_eq!(d.reversed_chronology, None, "natural order fixture");
        assert!(d.scan_a_started_at < d.scan_b_started_at, "A older than B");
        assert_eq!(d.grown.len(), 1);
        assert_eq!(d.freed.len(), 2);
        let created = &d.grown[0];
        assert_eq!(created.before, None, "created dir: before is null");
        let vanished = &d.freed[1];
        assert_eq!(vanished.after, None, "vanished dir: after is null");
    }

    #[test]
    fn encodes_camel_case_with_explicit_nulls() {
        let d: ScanDiff = serde_json::from_str(RAW_DIFF).unwrap();
        let v = serde_json::to_value(&d).unwrap();
        for key in [
            "scanA", "scanB", "scanAStartedAt", "scanBStartedAt",
            "reversedChronology", "rootPath", "diskDelta", "logicalDelta",
            "fileCountDelta", "dirCountDelta", "errorCountDelta", "grown", "freed",
        ] {
            assert!(v.as_object().unwrap().contains_key(key), "missing {key}");
        }
        assert!(v["reversedChronology"].is_null(), "unflagged order is present-as-null");
        assert_eq!(v["scanAStartedAt"], "2026-03-10T09:00:00.000000Z", "canonical datetime");
        assert!(v["grown"][0]["before"].is_null(), "nullable present-as-null at depth");
        assert_eq!(v["scanA"], v["scanA"].as_str().unwrap().to_lowercase(), "uuids lowercase out");
    }
}
