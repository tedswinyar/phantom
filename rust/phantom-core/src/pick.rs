//! pick.rs — which scan a result view reads when none is named, and the words
//! that say so. Shared by the CLI and the MCP server so the two surfaces print
//! the SAME header and the SAME warning, byte for byte (the e2e harness pins it).
//!
//! The hazard (phantom-cnr.4, 2026-09-16): every result command defaults to the
//! newest completed scan of ANY root. Ted ran `phantom plan` while working a
//! 291 GB home problem and it silently planned a 13.9 MB `~/Library/Safari`
//! probe scan taken minutes earlier; the plan read as plausible and described a
//! different tree. Nothing on the screen named the scan. Three remedies, all
//! schema-free and wire-free:
//!
//! 1. [`header_line`] — every human-format result view opens with one line
//!    naming the scan it read: `scan 5afa7a83 · /Users/ted · 280.4 GB · 4.09M
//!    files · 2 h ago`.
//! 2. [`default_warning`] — when the default was used AND another root has a
//!    completed scan that is both LARGER and no older than
//!    [`FRESH_ALTERNATIVE_WINDOW`], say so (stderr on the CLI, `note` on MCP).
//!    Older or smaller alternatives stay quiet: a week-old scan of `/` is not
//!    what a user who just scanned `~/Downloads` meant.
//! 3. The clients accept a positional scan id (an alias for `--scan`) — a UUID
//!    cannot be mistaken for anything else, and `--scan <id>` was easy to get
//!    wrong under pressure.
//!
//! The default itself does NOT change: newest completed scan, any root. A root
//! selector (`--root`) is additive wire and belongs to v1.2 (phantom-adq).

use chrono::{DateTime, Duration, Utc};
use uuid::Uuid;

use crate::format::format_size;
use crate::scan::{Scan, ScanStatus};

/// How recent a larger scan of another root must be to earn a warning.
pub const FRESH_ALTERNATIVE_WINDOW: Duration = Duration::hours(24);

/// The scan every result view defaults to: the most recently STARTED
/// completed scan of any root (a running or cancelled one has no readable
/// results). Ties (same instant) break on the lower id, as the API's
/// `ORDER BY started_at DESC, id` does, so the client-side pick matches the
/// list order the API serves.
pub fn newest_complete(scans: &[Scan]) -> Option<&Scan> {
    scans
        .iter()
        .filter(|s| s.status == ScanStatus::Complete)
        .max_by(|a, b| {
            a.started_at
                .cmp(&b.started_at)
                .then_with(|| b.id.cmp(&a.id))
        })
}

/// When the scan was taken, for age arithmetic: the finish when it has one,
/// else the start.
fn taken_at(scan: &Scan) -> DateTime<Utc> {
    scan.finished_at.unwrap_or(scan.started_at)
}

/// Another root's completed scan that is both larger than `chosen` and no
/// older than [`FRESH_ALTERNATIVE_WINDOW`] at `now` — the largest such, or
/// none. Same-root scans never qualify (the newest of a root IS the default).
pub fn larger_fresher_alternative<'a>(
    chosen: &Scan,
    scans: &'a [Scan],
    now: DateTime<Utc>,
) -> Option<&'a Scan> {
    scans
        .iter()
        .filter(|s| s.status == ScanStatus::Complete)
        .filter(|s| s.root_path != chosen.root_path)
        .filter(|s| s.total_disk_size > chosen.total_disk_size)
        .filter(|s| now.signed_duration_since(taken_at(s)) <= FRESH_ALTERNATIVE_WINDOW)
        .max_by(|a, b| {
            a.total_disk_size
                .cmp(&b.total_disk_size)
                .then_with(|| taken_at(a).cmp(&taken_at(b)))
        })
}

/// The first eight hex digits — what humans quote and what the app's sidebar
/// shows. The full id is always one `--json` away.
pub fn short_id(id: &Uuid) -> String {
    id.simple().to_string()[..8].to_string()
}

/// Counts the way sizes are shown: `12`, `550k`, `4.09M`, `1.20G`. Decimal,
/// like [`format_size`], so `4.09M files` and `280.4 GB` read as one system.
pub fn format_count(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{}k", n / 1_000),
        1_000_000..=999_999_999 => format!("{:.2}M", n as f64 / 1e6),
        _ => format!("{:.2}G", n as f64 / 1e9),
    }
}

/// Coarse relative age: `just now` under a minute, then minutes, hours, days.
/// Coarse on purpose — two surfaces rendering seconds apart must agree.
pub fn ago(then: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let secs = now.signed_duration_since(then).num_seconds().max(0);
    match secs {
        0..=59 => "just now".to_string(),
        60..=3_599 => format!("{} min ago", secs / 60),
        3_600..=86_399 => format!("{} h ago", secs / 3_600),
        _ => format!("{} d ago", secs / 86_400),
    }
}

/// `scan 5afa7a83 · /Users/ted · 280.4 GB · 4.09M files · 2 h ago` — the one
/// line every human-format result view opens with, whether the scan was named
/// or defaulted. `·`-separated so the last field (the age) can be stripped
/// for byte comparison across surfaces that render seconds apart.
pub fn header_line(scan: &Scan, now: DateTime<Utc>) -> String {
    format!(
        "scan {} · {} · {} · {} files · {}",
        short_id(&scan.id),
        scan.root_path,
        format_size(scan.total_disk_size),
        format_count(scan.file_count),
        ago(taken_at(scan), now)
    )
}

/// The warning for a DEFAULTED pick that has a larger, fresher rival of another
/// root: `defaulting to the newest scan (/Users/ted/Library/Safari, 13.9 MB); a
/// larger recent scan exists: /Users/ted (280.4 GB, 3 h ago) — <hint>`. `hint`
/// is the surface's own remedy (`pass --scan 5afa7a83-…` / `pass scanId
/// "5afa7a83-…"`). `None` when there is no such rival — silence is the common
/// case and must stay so, or the warning becomes noise.
pub fn default_warning(
    chosen: &Scan,
    scans: &[Scan],
    now: DateTime<Utc>,
    hint: &dyn Fn(&Scan) -> String,
) -> Option<String> {
    let alt = larger_fresher_alternative(chosen, scans, now)?;
    Some(format!(
        "defaulting to the newest scan ({}, {}); a larger recent scan exists: {} ({}, {}) — {}",
        chosen.root_path,
        format_size(chosen.total_disk_size),
        alt.root_path,
        format_size(alt.total_disk_size),
        ago(taken_at(alt), now),
        hint(alt)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(iso: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(iso).unwrap().with_timezone(&Utc)
    }

    fn scan(id: u128, root: &str, status: ScanStatus, finished: &str, disk: u64, files: u64) -> Scan {
        let started = at(finished) - Duration::minutes(5);
        Scan {
            id: Uuid::from_u128(id),
            root_path: root.to_string(),
            status,
            started_at: started,
            finished_at: Some(at(finished)),
            total_disk_size: disk,
            total_logical_size: disk,
            file_count: files,
            dir_count: 0,
            error_count: 0,
            unreadable_paths: None,
            total_private_size: None,
            total_shared_size: None,
            failure_reason: None,
        }
    }

    const NOW: &str = "2026-09-16T15:00:00Z";
    const GB: u64 = 1_000_000_000;

    /// Ted's 2026-09-16 shape: a huge home scan hours old, a tiny probe scan
    /// minutes old. The tiny one is the default — and the warning fires.
    fn teds_afternoon() -> Vec<Scan> {
        vec![
            scan(1, "/Users/ted/Library/Safari", ScanStatus::Complete, "2026-09-16T14:58:00Z", 13_900_000, 412),
            scan(2, "/Users/ted", ScanStatus::Complete, "2026-09-16T12:00:00Z", 280_400 * 1_000_000, 4_090_000),
            scan(3, "/Users/ted", ScanStatus::Complete, "2026-09-15T12:00:00Z", 289_600 * 1_000_000, 4_100_000),
        ]
    }

    #[test]
    fn default_is_the_newest_complete_scan_not_the_largest() {
        // Mutation target: pick max total_disk_size instead and this fails.
        let scans = teds_afternoon();
        assert_eq!(newest_complete(&scans).unwrap().id, Uuid::from_u128(1));
    }

    #[test]
    fn running_and_failed_scans_are_never_the_default() {
        let mut scans = teds_afternoon();
        scans.insert(0, scan(9, "/tmp/x", ScanStatus::Running, "2026-09-16T14:59:59Z", 1, 1));
        scans.insert(0, scan(8, "/tmp/y", ScanStatus::Failed, "2026-09-16T14:59:59Z", 1, 1));
        assert_eq!(newest_complete(&scans).unwrap().id, Uuid::from_u128(1));
        assert!(newest_complete(&scans[..2]).is_none(), "no completed scan → no default");
    }

    #[test]
    fn newest_is_by_start_time_regardless_of_list_order() {
        let mut scans = teds_afternoon();
        scans.reverse();
        assert_eq!(newest_complete(&scans).unwrap().id, Uuid::from_u128(1));
    }

    #[test]
    fn warning_names_the_larger_fresher_root_and_the_remedy() {
        let scans = teds_afternoon();
        let chosen = newest_complete(&scans).unwrap();
        let w = default_warning(chosen, &scans, at(NOW), &|alt| format!("pass --scan {}", alt.id)).unwrap();
        assert_eq!(
            w,
            "defaulting to the newest scan (/Users/ted/Library/Safari, 13.9 MB); a larger recent scan exists: \
             /Users/ted (280.4 GB, 3 h ago) — pass --scan 00000000-0000-0000-0000-000000000002"
        );
    }

    #[test]
    fn no_warning_when_the_larger_roots_scan_is_a_week_old() {
        // Mutation target: drop the 24 h clause and this fails.
        let scans = vec![
            scan(1, "/Users/ted/Downloads", ScanStatus::Complete, "2026-09-16T14:58:00Z", 2 * GB, 40),
            scan(2, "/", ScanStatus::Complete, "2026-09-09T12:00:00Z", 400 * GB, 5_000_000),
        ];
        let chosen = newest_complete(&scans).unwrap();
        assert!(default_warning(chosen, &scans, at(NOW), &|_| String::new()).is_none());
    }

    #[test]
    fn no_warning_when_the_only_recent_rival_is_smaller_or_the_same_root() {
        let scans = vec![
            scan(1, "/Users/ted", ScanStatus::Complete, "2026-09-16T14:58:00Z", 280 * GB, 4_000_000),
            scan(2, "/Users/ted", ScanStatus::Complete, "2026-09-16T10:00:00Z", 300 * GB, 4_100_000), // same root, larger, fresh: still no
            scan(3, "/Users/ted/Downloads", ScanStatus::Complete, "2026-09-16T14:00:00Z", 2 * GB, 40), // smaller
        ];
        let chosen = newest_complete(&scans).unwrap();
        assert!(larger_fresher_alternative(chosen, &scans, at(NOW)).is_none());
    }

    #[test]
    fn the_largest_qualifying_rival_wins() {
        let scans = vec![
            scan(1, "/Users/ted/Downloads", ScanStatus::Complete, "2026-09-16T14:58:00Z", 2 * GB, 40),
            scan(2, "/Users/ted", ScanStatus::Complete, "2026-09-16T12:00:00Z", 280 * GB, 4_000_000),
            scan(3, "/", ScanStatus::Complete, "2026-09-16T08:00:00Z", 400 * GB, 5_000_000),
            scan(4, "/Volumes/Old", ScanStatus::Running, "2026-09-16T14:59:00Z", 900 * GB, 1), // not complete
        ];
        let chosen = newest_complete(&scans).unwrap();
        assert_eq!(larger_fresher_alternative(chosen, &scans, at(NOW)).unwrap().id, Uuid::from_u128(3));
    }

    #[test]
    fn a_rival_exactly_at_the_window_edge_still_counts_and_a_second_past_it_does_not() {
        let edge = at(NOW) - FRESH_ALTERNATIVE_WINDOW;
        let mut inside = scan(2, "/", ScanStatus::Complete, NOW, 400 * GB, 1);
        inside.finished_at = Some(edge);
        let mut outside = inside.clone();
        outside.id = Uuid::from_u128(3);
        outside.finished_at = Some(edge - Duration::seconds(1));
        let chosen = scan(1, "/Users/ted/Downloads", ScanStatus::Complete, "2026-09-16T14:58:00Z", 2 * GB, 40);
        assert!(larger_fresher_alternative(&chosen, &[chosen.clone(), inside], at(NOW)).is_some());
        assert!(larger_fresher_alternative(&chosen, &[chosen.clone(), outside], at(NOW)).is_none());
    }

    #[test]
    fn header_line_is_the_pinned_shape() {
        let s = scan(0x5afa7a83_0000_0000_0000_000000000000, "/Users/ted", ScanStatus::Complete, "2026-09-16T13:00:00Z", 280_400 * 1_000_000, 4_090_000);
        assert_eq!(header_line(&s, at(NOW)), "scan 5afa7a83 · /Users/ted · 280.4 GB · 4.09M files · 2 h ago");
    }

    #[test]
    fn header_uses_the_start_when_a_scan_never_finished() {
        let mut s = scan(1, "/x", ScanStatus::Complete, "2026-09-16T14:30:00Z", 10, 1);
        s.finished_at = None; // started_at is 14:25
        assert!(header_line(&s, at(NOW)).ends_with("· 35 min ago"), "{}", header_line(&s, at(NOW)));
    }

    #[test]
    fn counts_and_ages_are_coarse_and_decimal() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(1_000), "1k");
        assert_eq!(format_count(550_137), "550k");
        assert_eq!(format_count(4_090_000), "4.09M");
        assert_eq!(format_count(1_200_000_000), "1.20G");
        let now = at(NOW);
        assert_eq!(ago(now, now), "just now");
        assert_eq!(ago(now - Duration::seconds(59), now), "just now");
        assert_eq!(ago(now - Duration::seconds(60), now), "1 min ago");
        assert_eq!(ago(now - Duration::minutes(59), now), "59 min ago");
        assert_eq!(ago(now - Duration::hours(1), now), "1 h ago");
        assert_eq!(ago(now - Duration::hours(23), now), "23 h ago");
        assert_eq!(ago(now - Duration::hours(24), now), "1 d ago");
        assert_eq!(ago(now + Duration::hours(1), now), "just now", "a clock skewed into the future is not negative");
    }
}
