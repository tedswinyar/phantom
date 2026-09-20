// Growth over time (v1.1 Phase 4, phantom-mkn.11): every scan is already
// persisted; this reads the completed scans of one root oldest-first and
// turns them into a series plus a linear "disk full in N days" forecast.
// No new data collection — a point IS a scan row, and a breakdown IS what
// the scan already stored (top-level directory aggregates, per-extension
// totals, the hotspot summary's per-category bytes).
//
// The forecast is deliberately naive and says so: an ordinary least-squares
// line through (days since first scan, totalDiskSize). It assumes the recent
// rate continues, that nothing is reclaimed, and that this root is the only
// thing filling the volume — none of which is true, which is why `caveat`
// ships on the wire beside the number and the CLI prints it every time.
//
// Pure: the API gathers the per-scan breakdowns from the store and the
// volume's available bytes; `build` does the arithmetic on values, so the
// fixture can pin every number.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;
use uuid::Uuid;

use crate::scan::Scan;

/// Series keys kept per breakdown, ranked by the NEWEST point; the rest fold
/// into [`OTHER_KEY`]. Ten lines is the most a sparkline legend survives.
pub const MAX_KEYS: usize = 10;
/// Where everything past [`MAX_KEYS`] (and, for `topLevelDir`, the files
/// directly under the root) goes.
pub const OTHER_KEY: &str = "other";
/// The `extension` key for files without one (the wire's `fileType: null`).
pub const NO_EXTENSION_KEY: &str = "(none)";
/// Fewer points than this and the forecast is null — a line through one
/// point is not a fit.
pub const MIN_FORECAST_POINTS: usize = 2;

/// What one series line is keyed by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum GroupBy {
    /// One line: the scan's `totalDiskSize`.
    Total,
    /// Σ hotspot-group `diskSize` per classifier category; null for a scan
    /// with no stored summary.
    Category,
    /// The root's direct child DIRECTORIES by their persisted aggregate;
    /// files directly under the root land in `other`.
    TopLevelDir,
    /// Per-extension totals (`scan_file_types`).
    Extension,
}

impl GroupBy {
    pub const ALL: [GroupBy; 4] = [GroupBy::Total, GroupBy::Category, GroupBy::TopLevelDir, GroupBy::Extension];

    pub fn as_str(&self) -> &'static str {
        match self {
            GroupBy::Total => "total",
            GroupBy::Category => "category",
            GroupBy::TopLevelDir => "topLevelDir",
            GroupBy::Extension => "extension",
        }
    }
}

impl FromStr for GroupBy {
    type Err = String;
    /// The wire spelling (camelCase) is the only one accepted — `top-level-dir`
    /// is the CLI's flag value and the CLI maps it.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        GroupBy::ALL
            .into_iter()
            .find(|g| g.as_str() == s)
            .ok_or_else(|| {
                format!(
                    "groupBy must be one of total, category, topLevelDir, extension (got {s:?})"
                )
            })
    }
}

/// One scan, as a point on the x axis (oldest first on the wire).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GrowthPoint {
    pub scan_id: Uuid,
    #[serde(with = "crate::wire_time")]
    pub started_at: DateTime<Utc>,
    /// THE size (deduped allocated bytes under the root).
    pub total_disk_size: u64,
    /// Null on scans persisted before the clone-aware walker (schema v5) —
    /// those points also counted APFS clones twice.
    pub total_private_size: Option<u64>,
    pub file_count: u64,
}

/// One line: a value per point, positionally aligned with `points`. Null
/// means the scan recorded nothing for this breakdown (no hotspot summary),
/// 0 means it recorded the key as absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GrowthLine {
    pub key: String,
    pub values: Vec<Option<u64>>,
}

/// Ordinary least squares over (days since the first point, totalDiskSize).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GrowthForecast {
    /// Always `"linear"` — present so a smarter method can replace it
    /// without a shape change.
    pub method: String,
    pub points_used: usize,
    /// Days between the first and the last point.
    pub span_days: f64,
    /// The fitted slope; negative when the root is shrinking.
    pub bytes_per_day: i64,
    /// The newest point's `totalDiskSize` — the forecast starts here, not at
    /// the fitted line's value.
    pub latest_bytes: u64,
    /// statfs `f_bavail` of the volume holding the root at request time;
    /// null when the root cannot be stat'd (removed since).
    pub available_bytes: Option<u64>,
    /// `availableBytes / bytesPerDay` when the slope is positive and the
    /// volume is readable; null when shrinking, flat, or unreadable.
    pub days_until_full: Option<f64>,
    #[serde(with = "crate::wire_time::option")]
    pub projected_full_at: Option<DateTime<Utc>>,
    pub caveat: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Growth {
    /// As the newest scan recorded it.
    pub root_path: String,
    pub group_by: GroupBy,
    /// Oldest first.
    pub points: Vec<GrowthPoint>,
    /// Ranked by the newest point's value, `other` last.
    pub series: Vec<GrowthLine>,
    /// Null with fewer than [`MIN_FORECAST_POINTS`] points or a zero span.
    pub forecast: Option<GrowthForecast>,
    pub note: String,
}

/// One scan's contribution: the row plus its breakdown for the chosen
/// `groupBy`. `None` == the scan recorded nothing for that breakdown.
pub struct Sample {
    pub scan: Scan,
    pub breakdown: Option<Vec<(String, u64)>>,
}

/// Assemble the wire object. `samples` are complete scans of one root in
/// ANY order (sorted here, oldest first, ties by id); `available` is the
/// volume's headroom now; `now` anchors `projectedFullAt`.
pub fn build(group_by: GroupBy, mut samples: Vec<Sample>, available: Option<u64>, now: DateTime<Utc>) -> Growth {
    samples.sort_by(|a, b| a.scan.started_at.cmp(&b.scan.started_at).then_with(|| a.scan.id.cmp(&b.scan.id)));
    let root_path = samples.last().map(|s| s.scan.root_path.clone()).unwrap_or_default();
    let points: Vec<GrowthPoint> = samples
        .iter()
        .map(|s| GrowthPoint {
            scan_id: s.scan.id,
            started_at: s.scan.started_at,
            total_disk_size: s.scan.total_disk_size,
            total_private_size: s.scan.total_private_size,
            file_count: s.scan.file_count,
        })
        .collect();
    let series = series(group_by, &samples);
    let forecast = forecast(&points, available, now);
    Growth { root_path, group_by, points, series, forecast, note: note() }
}

/// Keys ranked by the newest point that HAS a breakdown; the top
/// [`MAX_KEYS`] stay, everything else (and, for `topLevelDir`, the
/// unlisted remainder up to the scan total) folds into `other`.
fn series(group_by: GroupBy, samples: &[Sample]) -> Vec<GrowthLine> {
    let newest = samples.iter().rev().find_map(|s| s.breakdown.as_ref());
    let Some(newest) = newest else {
        return Vec::new();
    };
    let mut ranked: Vec<(&String, &u64)> = newest.iter().map(|(k, v)| (k, v)).collect();
    ranked.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    let kept: Vec<String> = ranked.iter().take(MAX_KEYS).map(|(k, _)| (*k).clone()).collect();
    let kept_set: BTreeSet<&String> = kept.iter().collect();
    // Does any point carry something outside the kept keys? Then `other`
    // exists for every point (0 where a point has nothing extra).
    let mut needs_other = false;
    for s in samples {
        if let Some(b) = &s.breakdown {
            let listed: u64 = b.iter().filter(|(k, _)| kept_set.contains(k)).map(|(_, v)| v).sum();
            let extra = b.iter().any(|(k, _)| !kept_set.contains(k));
            let remainder = match group_by {
                GroupBy::TopLevelDir => s.scan.total_disk_size.saturating_sub(listed) > 0,
                _ => false,
            };
            if extra || remainder {
                needs_other = true;
            }
        }
    }
    let mut lines: Vec<GrowthLine> = kept
        .iter()
        .map(|k| GrowthLine {
            key: k.clone(),
            values: samples
                .iter()
                .map(|s| {
                    s.breakdown
                        .as_ref()
                        .map(|b| b.iter().filter(|(bk, _)| bk == k).map(|(_, v)| *v).sum::<u64>())
                })
                .collect(),
        })
        .collect();
    if needs_other {
        lines.push(GrowthLine {
            key: OTHER_KEY.to_string(),
            values: samples
                .iter()
                .map(|s| {
                    s.breakdown.as_ref().map(|b| {
                        let listed: u64 = b.iter().filter(|(k, _)| kept_set.contains(k)).map(|(_, v)| v).sum();
                        match group_by {
                            // The scan total is the truth; whatever the
                            // listed children do not cover is the rest.
                            GroupBy::TopLevelDir => s.scan.total_disk_size.saturating_sub(listed),
                            _ => b.iter().filter(|(k, _)| !kept_set.contains(k)).map(|(_, v)| v).sum(),
                        }
                    })
                })
                .collect(),
        });
    }
    lines
}

/// Least squares on (days since first, bytes). Two points make a line
/// through both; a zero span (two scans at the same instant) is no fit.
pub fn forecast(points: &[GrowthPoint], available: Option<u64>, now: DateTime<Utc>) -> Option<GrowthForecast> {
    if points.len() < MIN_FORECAST_POINTS {
        return None;
    }
    let first = points[0].started_at;
    let xs: Vec<f64> = points.iter().map(|p| days_between(first, p.started_at)).collect();
    let ys: Vec<f64> = points.iter().map(|p| p.total_disk_size as f64).collect();
    let n = xs.len() as f64;
    let span_days = xs[xs.len() - 1];
    if span_days <= 0.0 {
        return None;
    }
    let span_days = round2(span_days);
    let mean_x = xs.iter().sum::<f64>() / n;
    let mean_y = ys.iter().sum::<f64>() / n;
    let sxx: f64 = xs.iter().map(|x| (x - mean_x).powi(2)).sum();
    let sxy: f64 = xs.iter().zip(&ys).map(|(x, y)| (x - mean_x) * (y - mean_y)).sum();
    let slope = if sxx > 0.0 { sxy / sxx } else { 0.0 };
    let bytes_per_day = slope.round().clamp(i64::MIN as f64, i64::MAX as f64) as i64;
    let latest_bytes = points[points.len() - 1].total_disk_size;
    let days_until_full_exact = match available {
        Some(avail) if bytes_per_day > 0 => Some(avail as f64 / bytes_per_day as f64),
        _ => None,
    };
    let days_until_full = days_until_full_exact.map(round2);
    let projected_full_at = days_until_full_exact.and_then(|d| {
        let ms = (d * 86_400_000.0).round();
        if ms.is_finite() && ms < i64::MAX as f64 {
            now.checked_add_signed(Duration::milliseconds(ms as i64))
        } else {
            None
        }
    });
    Some(GrowthForecast {
        method: "linear".into(),
        points_used: points.len(),
        span_days,
        bytes_per_day,
        latest_bytes,
        available_bytes: available,
        days_until_full,
        projected_full_at,
        caveat: caveat(points),
    })
}

/// The two floats on the wire are rounded to hundredths. A raw f64 such as
/// 0.000024305555555555554 does not survive a JSON round trip byte-for-byte
/// (serde re-emits the shortest repr, jq keeps the literal), which broke the
/// three-surface parity gate on a 2-second span (e2e §16, 2026-09-08). A
/// hundredth of a day is 14 minutes — far below what a forecast can claim.
pub fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

fn days_between(a: DateTime<Utc>, b: DateTime<Utc>) -> f64 {
    (b - a).num_milliseconds() as f64 / 86_400_000.0
}

/// The words that travel with the number. Human text; not a contract.
pub fn caveat(points: &[GrowthPoint]) -> String {
    let mut s = String::from(
        "Linear fit through this root's completed scans: it assumes the recent rate continues, that nothing \
         is reclaimed, and that this root is the only thing filling the volume — none of which holds for long. \
         Fewer than three points is a line through noise, not a trend.",
    );
    if points.iter().any(|p| p.total_private_size.is_none()) {
        s.push_str(
            " Points with totalPrivateSize null predate the clone-aware walker and counted APFS clones twice, \
             so the earliest values run a few GB high.",
        );
    }
    s
}

fn note() -> String {
    "points are this root's completed scans, oldest first; series values align with points positionally \
     (null = that scan recorded nothing for this breakdown, 0 = recorded as absent). Only the newest 25 scans \
     of each root (100 overall) are kept, so the history is as long as the retention window, not as long as the disk's."
        .to_string()
}

/// The keys a breakdown uses for `extension`: the wire's null type becomes
/// [`NO_EXTENSION_KEY`].
pub fn extension_key(file_type: Option<&str>) -> String {
    file_type.map(str::to_string).unwrap_or_else(|| NO_EXTENSION_KEY.to_string())
}

/// Convenience for the API: a breakdown from any (key, bytes) iterator,
/// summing repeated keys.
pub fn breakdown<I: IntoIterator<Item = (String, u64)>>(items: I) -> Vec<(String, u64)> {
    let mut map: BTreeMap<String, u64> = BTreeMap::new();
    for (k, v) in items {
        *map.entry(k).or_insert(0) += v;
    }
    map.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::ScanStatus;
    use chrono::TimeZone;

    const RAW: &str = include_str!("../../../tests/fixtures/growth.json");

    fn at(day: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, day, 9, 0, 0).unwrap()
    }

    fn scan(id: &str, day: u32, total: u64, private: Option<u64>) -> Scan {
        Scan {
            id: Uuid::parse_str(id).unwrap(),
            root_path: "/Users/ghost".into(),
            status: ScanStatus::Complete,
            started_at: at(day),
            finished_at: Some(at(day)),
            total_disk_size: total,
            total_logical_size: total,
            file_count: 1000 + day as u64,
            dir_count: 10,
            error_count: 0,
            unreadable_paths: Some(vec![]),
            total_private_size: private,
            total_shared_size: private.map(|p| total - p),
            failure_reason: None,
        }
    }

    const A: &str = "11111111-1111-4111-8111-111111111111";
    const B: &str = "22222222-2222-4222-8222-222222222222";
    const C: &str = "33333333-3333-4333-8333-333333333333";

    fn three_samples(group_by: GroupBy) -> Vec<Sample> {
        let mk = |id, day, total, private, b: Option<Vec<(&str, u64)>>| Sample {
            scan: scan(id, day, total, private),
            breakdown: b.map(|v| v.into_iter().map(|(k, n)| (k.to_string(), n)).collect()),
        };
        match group_by {
            GroupBy::Total => vec![
                mk(A, 1, 100, None, Some(vec![("total", 100)])),
                mk(B, 4, 130, Some(120), Some(vec![("total", 130)])),
                mk(C, 7, 160, Some(150), Some(vec![("total", 160)])),
            ],
            GroupBy::TopLevelDir => vec![
                // Deliberately out of order: build sorts oldest-first.
                mk(C, 7, 160, Some(150), Some(vec![("Code", 90), ("Library", 50), ("Movies", 5)])),
                mk(A, 1, 100, None, Some(vec![("Code", 60), ("Library", 30)])),
                mk(B, 4, 130, Some(120), Some(vec![("Code", 80), ("Library", 40), ("Movies", 2)])),
            ],
            GroupBy::Category => vec![
                mk(A, 1, 100, None, None), // pre-v2: no summary stored
                mk(B, 4, 130, Some(120), Some(vec![("cache", 10), ("regenerableArtifact", 30)])),
                mk(C, 7, 160, Some(150), Some(vec![("cache", 12), ("regenerableArtifact", 45)])),
            ],
            GroupBy::Extension => vec![
                mk(A, 1, 100, None, Some(vec![("mov", 40), ("(none)", 10)])),
                mk(B, 4, 130, Some(120), Some(vec![("mov", 50), ("(none)", 12), ("zip", 3)])),
                mk(C, 7, 160, Some(150), Some(vec![("mov", 60), ("(none)", 14), ("zip", 9)])),
            ],
        }
    }

    #[test]
    fn fixture_round_trips_and_its_numbers_are_the_arithmetic() {
        let g: Growth = serde_json::from_str(RAW).unwrap();
        let raw: serde_json::Value = serde_json::from_str(RAW).unwrap();
        assert_eq!(serde_json::to_value(&g).unwrap(), raw, "encode reproduces the fixture bytes");
        assert_eq!(g.group_by, GroupBy::TopLevelDir);
        assert_eq!(g.points.len(), 3);
        assert!(g.points.windows(2).all(|w| w[0].started_at < w[1].started_at), "oldest first");
        // Rebuild the forecast from the fixture's own points and it must
        // match the fixture's forecast (same available, same now).
        let f = g.forecast.as_ref().unwrap();
        let rebuilt = forecast(&g.points, f.available_bytes, f.projected_full_at.unwrap() - Duration::milliseconds((f.days_until_full.unwrap() * 86_400_000.0).round() as i64)).unwrap();
        assert_eq!(rebuilt.bytes_per_day, f.bytes_per_day);
        assert_eq!(rebuilt.span_days, f.span_days);
        assert_eq!(rebuilt.days_until_full, f.days_until_full);
        assert_eq!(rebuilt.latest_bytes, g.points.last().unwrap().total_disk_size);
        // Series align with points and `other` is last.
        for line in &g.series {
            assert_eq!(line.values.len(), g.points.len(), "{}", line.key);
        }
        assert_eq!(g.series.last().unwrap().key, OTHER_KEY);
    }

    #[test]
    fn build_sorts_oldest_first_and_ranks_keys_by_the_newest_point() {
        let g = build(GroupBy::TopLevelDir, three_samples(GroupBy::TopLevelDir), Some(1_000), at(7));
        let ids: Vec<String> = g.points.iter().map(|p| p.scan_id.to_string()).collect();
        assert_eq!(ids, vec![A, B, C]);
        assert_eq!(g.root_path, "/Users/ghost");
        let keys: Vec<&str> = g.series.iter().map(|l| l.key.as_str()).collect();
        assert_eq!(keys, vec!["Code", "Library", "Movies", OTHER_KEY]);
        // Movies did not exist in the first scan: 0, not null (the scan
        // recorded its children; the key was absent).
        assert_eq!(g.series[2].values, vec![Some(0), Some(2), Some(5)]);
        // other = scan total − listed children (files under the root).
        assert_eq!(g.series[3].values, vec![Some(10), Some(8), Some(15)]);
        assert_eq!(g.series[0].values, vec![Some(60), Some(80), Some(90)]);
    }

    #[test]
    fn a_scan_without_a_breakdown_is_null_not_zero() {
        let g = build(GroupBy::Category, three_samples(GroupBy::Category), None, at(7));
        let keys: Vec<&str> = g.series.iter().map(|l| l.key.as_str()).collect();
        assert_eq!(keys, vec!["regenerableArtifact", "cache"], "ranked by the newest value; no other needed");
        assert_eq!(g.series[0].values, vec![None, Some(30), Some(45)]);
        assert_eq!(g.series[1].values, vec![None, Some(10), Some(12)]);
    }

    #[test]
    fn total_is_one_line_equal_to_the_points() {
        let g = build(GroupBy::Total, three_samples(GroupBy::Total), Some(300), at(7));
        assert_eq!(g.series.len(), 1);
        assert_eq!(g.series[0].key, "total");
        let from_points: Vec<Option<u64>> = g.points.iter().map(|p| Some(p.total_disk_size)).collect();
        assert_eq!(g.series[0].values, from_points);
    }

    #[test]
    fn more_than_max_keys_fold_into_other() {
        let mut b: Vec<(String, u64)> = (0..(MAX_KEYS + 3)).map(|i| (format!("k{i:02}"), 100 - i as u64)).collect();
        b.reverse();
        let samples = vec![Sample { scan: scan(A, 1, 5_000, Some(5_000)), breakdown: Some(b) }];
        let g = build(GroupBy::Extension, samples, None, at(1));
        assert_eq!(g.series.len(), MAX_KEYS + 1);
        assert_eq!(g.series[0].key, "k00", "biggest first");
        assert_eq!(g.series[MAX_KEYS].key, OTHER_KEY);
        // k10, k11, k12 = 90 + 89 + 88.
        assert_eq!(g.series[MAX_KEYS].values, vec![Some(267)]);
        assert!(g.forecast.is_none(), "one point is not a fit");
    }

    #[test]
    fn forecast_is_the_least_squares_line_and_days_until_full_needs_a_positive_slope() {
        let g = build(GroupBy::Total, three_samples(GroupBy::Total), Some(600), at(7));
        let f = g.forecast.unwrap();
        // 100 → 130 → 160 over days 0, 3, 6: exactly 10 bytes/day.
        assert_eq!(f.bytes_per_day, 10);
        assert_eq!(f.span_days, 6.0);
        assert_eq!(f.points_used, 3);
        assert_eq!(f.latest_bytes, 160);
        assert_eq!(f.days_until_full, Some(60.0));
        assert_eq!(f.projected_full_at, Some(at(7) + Duration::days(60)));
        assert!(f.caveat.contains("clone-aware walker"), "a pre-v5 point is called out: {}", f.caveat);

        // Shrinking: slope negative, no full date.
        let mut s = three_samples(GroupBy::Total);
        s[0].scan.total_disk_size = 200;
        s[1].scan.total_disk_size = 150;
        s[2].scan.total_disk_size = 100;
        let f = build(GroupBy::Total, s, Some(600), at(7)).forecast.unwrap();
        assert!(f.bytes_per_day < 0);
        assert_eq!(f.days_until_full, None);
        assert_eq!(f.projected_full_at, None);

        // Flat: slope 0, no full date. Unreadable volume: no full date even
        // when growing.
        let mut s = three_samples(GroupBy::Total);
        for x in &mut s {
            x.scan.total_disk_size = 100;
        }
        assert_eq!(build(GroupBy::Total, s, Some(600), at(7)).forecast.unwrap().days_until_full, None);
        let f = build(GroupBy::Total, three_samples(GroupBy::Total), None, at(7)).forecast.unwrap();
        assert_eq!(f.bytes_per_day, 10);
        assert_eq!(f.days_until_full, None);
        assert_eq!(f.available_bytes, None);
    }

    /// A 35-minute span is 0.02 days on the wire, and the emitted text
    /// re-parses and re-emits to the same bytes (what the e2e parity gate
    /// compares across HTTP, CLI and MCP).
    #[test]
    fn wire_floats_are_rounded_and_survive_a_json_round_trip() {
        let a = scan(A, 1, 100, Some(100));
        let mut b = scan(B, 1, 200, Some(200));
        b.started_at = a.started_at + Duration::minutes(35);
        let g = build(
            GroupBy::Total,
            vec![
                Sample { scan: a, breakdown: Some(vec![("total".into(), 100)]) },
                Sample { scan: b, breakdown: Some(vec![("total".into(), 200)]) },
            ],
            Some(1_000_000),
            at(2),
        );
        let f = g.forecast.as_ref().unwrap();
        assert_eq!(f.span_days, 0.02);
        assert_eq!(f.days_until_full.map(|d| (d * 100.0).round() / 100.0), f.days_until_full, "already hundredths");
        // Through a Value twice (what the CLI and MCP do to the API's body):
        // the number literals must come out identical the second time.
        let text = serde_json::to_string(&g).unwrap();
        let once: serde_json::Value = serde_json::from_str(&text).unwrap();
        let t1 = serde_json::to_string(&once).unwrap();
        let twice: serde_json::Value = serde_json::from_str(&t1).unwrap();
        assert_eq!(serde_json::to_string(&twice).unwrap(), t1, "re-emission is byte-identical");
        assert!(text.contains("\"spanDays\":0.02"), "{text}");
        assert!(t1.contains("\"spanDays\":0.02"), "{t1}");
    }

    #[test]
    fn two_points_at_the_same_instant_are_no_fit() {
        let s = vec![
            Sample { scan: scan(A, 1, 100, Some(100)), breakdown: Some(vec![("total".into(), 100)]) },
            Sample { scan: scan(B, 1, 200, Some(200)), breakdown: Some(vec![("total".into(), 200)]) },
        ];
        assert!(build(GroupBy::Total, s, Some(1), at(1)).forecast.is_none());
    }

    #[test]
    fn no_pre_v5_point_no_clone_caveat() {
        let s = three_samples(GroupBy::Total).into_iter().skip(1).collect();
        let f = build(GroupBy::Total, s, None, at(7)).forecast.unwrap();
        assert!(!f.caveat.contains("clone-aware"), "{}", f.caveat);
        assert!(f.caveat.contains("Fewer than three points"));
    }

    #[test]
    fn group_by_parses_only_the_wire_spelling() {
        for g in GroupBy::ALL {
            assert_eq!(g.as_str().parse::<GroupBy>().unwrap(), g);
            assert_eq!(serde_json::to_value(g).unwrap(), serde_json::Value::String(g.as_str().into()));
        }
        assert!("top-level-dir".parse::<GroupBy>().is_err());
        assert!("TOTAL".parse::<GroupBy>().is_err());
        assert!("".parse::<GroupBy>().unwrap_err().contains("groupBy"));
    }

    #[test]
    fn breakdown_sums_repeated_keys_and_names_the_missing_extension() {
        let b = breakdown(vec![("a".to_string(), 1), ("b".to_string(), 2), ("a".to_string(), 3)]);
        assert_eq!(b, vec![("a".to_string(), 4), ("b".to_string(), 2)]);
        assert_eq!(extension_key(None), NO_EXTENSION_KEY);
        assert_eq!(extension_key(Some("mov")), "mov");
    }
}
