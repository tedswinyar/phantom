// Scan schedules (v1.2 Phase 1, phantom-adq.1): WHEN a scheduled root is
// due, as pure arithmetic. The API's scheduler task ticks once a minute and
// asks `is_due`; persistence (schema v8) and the HTTP surface land with the
// VERSION → 1.2.0 bump after the 1.1.0 tag. Nothing here runs anything.
//
// Two rules the arithmetic must make true by construction:
// - A schedule fires at a LOCAL hour boundary (03:00 by default), daily or
//   weekly, never at "creation time + 24 h" drifting through the day.
// - A missed window fires ONCE on the next tick, never N times: the fire
//   time recorded is `now`, and the next due boundary is computed from it,
//   so a Mac asleep for a week wakes to one scan, not seven.

use chrono::{DateTime, Duration, TimeZone, Timelike, Utc};
use serde::{Deserialize, Serialize};
use std::str::FromStr;

/// How often a scheduled root is scanned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Cadence {
    Daily,
    Weekly,
}

impl Cadence {
    pub const ALL: [Cadence; 2] = [Cadence::Daily, Cadence::Weekly];

    pub fn as_str(&self) -> &'static str {
        match self {
            Cadence::Daily => "daily",
            Cadence::Weekly => "weekly",
        }
    }

    /// Days between fires.
    pub fn days(&self) -> i64 {
        match self {
            Cadence::Daily => 1,
            Cadence::Weekly => 7,
        }
    }
}

impl FromStr for Cadence {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Cadence::ALL
            .into_iter()
            .find(|c| c.as_str() == s)
            .ok_or_else(|| format!("cadence must be daily or weekly (got {s:?})"))
    }
}

/// One root's schedule. `hour_of_day` is LOCAL (0–23); `last_fired_at` is
/// when the scheduler last started a scan for it (not the boundary it was
/// due at — see the once-per-window rule above).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Schedule {
    pub root_path: String,
    pub cadence: Cadence,
    pub hour_of_day: u32,
    pub enabled: bool,
    /// When the schedule was set — the anchor for the FIRST fire (the next
    /// boundary after it). Without it a never-fired schedule has no past to
    /// be due relative to and never fires (the first draft's bug).
    #[serde(with = "crate::wire_time")]
    pub created_at: DateTime<Utc>,
    #[serde(with = "crate::wire_time::option")]
    pub last_fired_at: Option<DateTime<Utc>>,
    /// gitignore-syntax exclusions applied to every scheduled scan (Phase 3
    /// fills this; present-as-null until then).
    pub exclude: Option<Vec<String>>,
}

/// The default hour: 03:00 local — the Mac is usually idle and awake-able.
pub const DEFAULT_HOUR_OF_DAY: u32 = 3;

impl Schedule {
    pub fn new(root_path: impl Into<String>, cadence: Cadence, created_at: DateTime<Utc>) -> Self {
        Self {
            root_path: root_path.into(),
            cadence,
            hour_of_day: DEFAULT_HOUR_OF_DAY,
            enabled: true,
            created_at,
            last_fired_at: None,
            exclude: None,
        }
    }

    /// Validate what the wire may carry: an hour in 0..=23.
    pub fn validate(&self) -> Result<(), String> {
        if self.hour_of_day > 23 {
            return Err(format!("hourOfDay must be 0–23 (got {})", self.hour_of_day));
        }
        if self.root_path.trim().is_empty() {
            return Err("rootPath must not be empty".into());
        }
        Ok(())
    }

    /// The instant this schedule is next due, in `tz`'s local clock.
    /// Never fired: the next `hour_of_day` boundary after `created_at` (a
    /// schedule set at 14:00 for 03:00 first fires tomorrow at 03:00).
    /// Fired before: the boundary that follows `last_fired_at` by the
    /// cadence — daily, the next boundary; weekly, the first boundary at
    /// least six days later (a fire at 03:04 Monday is due 03:00 next
    /// Monday). Independent of `now`, so "due" means "this instant has
    /// passed", not "recomputed to always be ahead".
    pub fn next_due<Tz: TimeZone>(&self, tz: &Tz) -> DateTime<Utc> {
        match self.last_fired_at {
            None => boundary_after(self.created_at, self.hour_of_day, tz),
            Some(last) => boundary_after(last + Duration::days(self.cadence.days() - 1), self.hour_of_day, tz),
        }
    }

    /// Enabled and past its due instant.
    pub fn is_due<Tz: TimeZone>(&self, now: DateTime<Utc>, tz: &Tz) -> bool {
        self.enabled && self.next_due(tz) <= now
    }

    /// Record a fire at `now`. The next due instant is computed from THIS
    /// time, which is what makes a missed window fire once.
    pub fn fired(&mut self, now: DateTime<Utc>) {
        self.last_fired_at = Some(now);
    }
}

/// The first instant strictly after `t` whose local time is `hour:00:00`.
/// A local time that does not exist (spring-forward gap) resolves to the
/// earliest valid instant after it; an ambiguous one (fall-back) to the
/// earlier of the two — either way the boundary is not skipped.
pub fn boundary_after<Tz: TimeZone>(t: DateTime<Utc>, hour: u32, tz: &Tz) -> DateTime<Utc> {
    let local = t.with_timezone(tz);
    let mut day = local.date_naive();
    // Today's boundary if it is still ahead, else tomorrow's.
    if local.hour() > hour || (local.hour() == hour && (local.minute() > 0 || local.second() > 0 || local.nanosecond() > 0)) || local.hour() == hour {
        day = day.succ_opt().expect("date arithmetic");
    }
    for _ in 0..3 {
        let naive = day.and_hms_opt(hour, 0, 0).expect("valid hour");
        match tz.from_local_datetime(&naive) {
            chrono::LocalResult::Single(dt) => return dt.with_timezone(&Utc),
            chrono::LocalResult::Ambiguous(a, b) => return std::cmp::min(a, b).with_timezone(&Utc),
            chrono::LocalResult::None => {
                // The hour does not exist that day (DST gap): the next
                // existing minute is the honest boundary.
                for m in 1..=120 {
                    let n = naive + Duration::minutes(m);
                    if let chrono::LocalResult::Single(dt) = tz.from_local_datetime(&n) {
                        return dt.with_timezone(&Utc);
                    }
                }
                day = day.succ_opt().expect("date arithmetic");
            }
        }
    }
    unreachable!("a local hour boundary exists within three days");
}

/// The scheduler's decision for one tick: which enabled, due schedules
/// should start a scan now, skipping roots that already have one running.
/// Pure; the caller supplies the running roots.
pub fn due_now<'a, Tz: TimeZone>(
    schedules: &'a [Schedule],
    now: DateTime<Utc>,
    tz: &Tz,
    running_roots: &[&str],
) -> Vec<&'a Schedule> {
    schedules
        .iter()
        .filter(|s| s.is_due(now, tz))
        .filter(|s| !running_roots.iter().any(|r| same_root(r, &s.root_path)))
        .collect()
}

fn same_root(a: &str, b: &str) -> bool {
    a.trim_end_matches('/') == b.trim_end_matches('/')
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;

    /// A fixed zone with no DST, so the arithmetic is the thing under test.
    fn tz() -> FixedOffset {
        FixedOffset::west_opt(7 * 3600).unwrap() // UTC−7
    }

    fn local(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        tz().with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn a_new_schedule_first_fires_at_the_next_boundary_after_creation() {
        // Set at 14:00 local → tomorrow 03:00; not due at 14:00, due from 03:00 on.
        let s = Schedule::new("/Users/ghost", Cadence::Daily, local(2026, 9, 9, 14, 0));
        assert_eq!(s.next_due(&tz()), local(2026, 9, 10, 3, 0));
        assert!(!s.is_due(local(2026, 9, 9, 14, 0), &tz()));
        assert!(!s.is_due(local(2026, 9, 10, 2, 59), &tz()));
        assert!(s.is_due(local(2026, 9, 10, 3, 0), &tz()));
        assert!(s.is_due(local(2026, 9, 10, 3, 1), &tz()));
        // Set at 02:59 → today 03:00.
        let early = Schedule::new("/r", Cadence::Daily, local(2026, 9, 9, 2, 59));
        assert_eq!(early.next_due(&tz()), local(2026, 9, 9, 3, 0));
        // Set exactly at 03:00 → tomorrow (strictly after).
        let on_the_hour = Schedule::new("/r", Cadence::Daily, local(2026, 9, 9, 3, 0));
        assert_eq!(on_the_hour.next_due(&tz()), local(2026, 9, 10, 3, 0));
    }

    #[test]
    fn daily_fires_at_the_hour_boundary_not_creation_time_plus_24h() {
        let mut s = Schedule::new("/r", Cadence::Daily, local(2026, 9, 8, 14, 0));
        s.fired(local(2026, 9, 9, 3, 4)); // the scheduler ticked at 03:04
        assert_eq!(s.next_due(&tz()), local(2026, 9, 10, 3, 0), "not 03:04 tomorrow");
        assert!(!s.is_due(local(2026, 9, 9, 23, 59), &tz()));
        assert!(s.is_due(local(2026, 9, 10, 3, 0), &tz()));
    }

    #[test]
    fn weekly_is_the_same_hour_a_week_later() {
        let mut s = Schedule::new("/r", Cadence::Weekly, local(2026, 9, 1, 12, 0));
        s.fired(local(2026, 9, 7, 3, 2)); // Monday 03:02
        assert_eq!(s.next_due(&tz()), local(2026, 9, 14, 3, 0), "next Monday 03:00");
        assert!(!s.is_due(local(2026, 9, 13, 3, 0), &tz()), "Sunday is too soon");
        assert!(s.is_due(local(2026, 9, 14, 3, 0), &tz()));
    }

    /// The Mac slept for nine days. One tick fires once; recording `now`
    /// (not the missed boundary) means the next due is tomorrow, not "now"
    /// again eight more times.
    #[test]
    fn a_missed_window_fires_once_then_realigns() {
        let mut s = Schedule::new("/r", Cadence::Daily, local(2026, 8, 30, 9, 0));
        s.fired(local(2026, 9, 1, 3, 0));
        let wake = local(2026, 9, 10, 11, 30);
        assert!(s.is_due(wake, &tz()));
        s.fired(wake);
        assert!(!s.is_due(wake + Duration::minutes(1), &tz()), "no catch-up storm");
        assert_eq!(s.next_due(&tz()), local(2026, 9, 11, 3, 0));
        // Weekly, same story: one fire, then a week from the hour boundary.
        let mut w = Schedule::new("/r", Cadence::Weekly, local(2026, 7, 1, 9, 0));
        w.fired(local(2026, 8, 1, 3, 0));
        assert!(w.is_due(wake, &tz()));
        w.fired(wake);
        assert!(!w.is_due(wake + Duration::days(3), &tz()));
        assert_eq!(w.next_due(&tz()), local(2026, 9, 17, 3, 0));
    }

    #[test]
    fn disabled_is_never_due_and_hour_is_validated() {
        let mut s = Schedule::new("/r", Cadence::Daily, local(2026, 1, 1, 0, 0));
        s.enabled = false;
        assert!(!s.is_due(local(2030, 1, 1, 12, 0), &tz()), "years overdue, still silent when disabled");
        assert!(s.validate().is_ok());
        s.hour_of_day = 24;
        assert!(s.validate().unwrap_err().contains("hourOfDay"));
        s.hour_of_day = 0;
        s.root_path = "  ".into();
        assert!(s.validate().unwrap_err().contains("rootPath"));
        // Hour 0 is a valid boundary (midnight).
        let m = Schedule { hour_of_day: 0, ..Schedule::new("/r", Cadence::Daily, local(2026, 9, 9, 23, 0)) };
        assert_eq!(m.next_due(&tz()), local(2026, 9, 10, 0, 0));
    }

    #[test]
    fn due_now_skips_roots_with_a_running_scan_and_respects_trailing_slashes() {
        let now = local(2026, 9, 9, 3, 30);
        let mut a = Schedule::new("/a", Cadence::Daily, local(2026, 9, 1, 0, 0));
        a.fired(local(2026, 9, 8, 3, 0));
        let mut b = Schedule::new("/b/", Cadence::Daily, local(2026, 9, 1, 0, 0));
        b.fired(local(2026, 9, 8, 3, 0));
        let c = Schedule::new("/c", Cadence::Daily, now); // set this instant → not due until tomorrow
        let all = [a.clone(), b.clone(), c];
        let due: Vec<&str> = due_now(&all, now, &tz(), &["/b"]).iter().map(|s| s.root_path.as_str()).collect();
        assert_eq!(due, vec!["/a"], "b has a running scan (slash cosmetic); c is not due yet");
        let ab = [a, b];
        let due: Vec<&str> = due_now(&ab, now, &tz(), &[]).iter().map(|s| s.root_path.as_str()).collect();
        assert_eq!(due, vec!["/a", "/b/"]);
    }

    #[test]
    fn dst_gap_and_overlap_do_not_skip_or_double_a_boundary() {
        // America/Los_Angeles: 2026-03-08 02:00 does not exist; 2026-11-01 01:00 exists twice.
        let la = chrono_tz::America::Los_Angeles;
        let before_gap = la.with_ymd_and_hms(2026, 3, 8, 1, 30, 0).unwrap().with_timezone(&Utc);
        let s = Schedule { hour_of_day: 2, ..Schedule::new("/r", Cadence::Daily, before_gap) };
        let due = s.next_due(&la);
        // 02:00 does not exist; the boundary is the first existing minute (03:00 PDT) — the same instant.
        assert_eq!(due, la.with_ymd_and_hms(2026, 3, 8, 3, 0, 0).unwrap().with_timezone(&Utc));
        let before_overlap = la.with_ymd_and_hms(2026, 11, 1, 0, 30, 0).unwrap().with_timezone(&Utc);
        let s1 = Schedule { hour_of_day: 1, ..Schedule::new("/r", Cadence::Daily, before_overlap) };
        let due = s1.next_due(&la);
        // The EARLIER 01:00 (PDT) — one boundary, not two.
        assert_eq!(due, before_overlap + Duration::minutes(30));
    }

    #[test]
    fn cadence_wire_spelling_and_json() {
        assert_eq!("daily".parse::<Cadence>().unwrap(), Cadence::Daily);
        assert!("hourly".parse::<Cadence>().is_err());
        let s = Schedule::new("/r", Cadence::Weekly, local(2026, 9, 9, 14, 0));
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["cadence"], "weekly");
        assert_eq!(v["hourOfDay"], 3);
        assert_eq!(v["createdAt"], "2026-09-09T21:00:00.000000Z", "6-digit Z form");
        assert!(v["lastFiredAt"].is_null(), "present-as-null");
        assert!(v["exclude"].is_null());
        let back: Schedule = serde_json::from_value(v).unwrap();
        assert_eq!(back, s);
    }
}
