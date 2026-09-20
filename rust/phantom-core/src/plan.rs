// Reclaim plans (v1.1 Phase 3, phantom-mkn.7): the dry-run an agent (or a
// person) confirms BEFORE anything moves, and the verification that reports
// what actually came back afterwards.
//
// A plan is derived, not computed: every item IS a hotspot group the
// classifier already rated — its tier, its why, its command, its paths and
// the private bytes of the paths it actually includes. The plan adds only
// selection (which tiers, a size floor), an id, and totals. `review` groups
// never become items, whatever the caller asks for; that is the contract
// an agent may act on (docs/reclaimability.md, "tier rubric").
//
// Phantom still never deletes. The plan's script moves paths to a per-plan
// folder in the Trash and logs; it is DRY-RUN unless PHANTOM_APPLY=1; the
// USER runs it (or the agent, inside its own permission system).
// Verification is arithmetic over two persisted scans — the one the plan
// was built from and a rescan — through the same directory aggregates the
// diff engine uses.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::classify::{Category, HotspotGroup, HotspotsSummary, RiskTier};
use crate::scan::Scan;

/// Agreement threshold for `withinTolerance`: |expected − actual| ≤ 5% of
/// expected (the G3 gate's number).
pub const TOLERANCE: f64 = 0.05;

/// What the caller may select. Defaults are the conservative ones: only
/// `safe` groups, no size floor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanOptions {
    /// Highest tier admitted. `Review` is refused by the API (never a plan
    /// item); here it is clamped to `Caution` so the type cannot express a
    /// plan that deletes review-only groups.
    pub max_tier: RiskTier,
    /// Items whose included paths' private bytes fall below this are skipped.
    pub min_bytes: u64,
}

impl Default for PlanOptions {
    fn default() -> Self {
        Self {
            max_tier: RiskTier::Safe,
            min_bytes: 0,
        }
    }
}

fn tier_rank(t: RiskTier) -> u8 {
    match t {
        RiskTier::Safe => 0,
        RiskTier::Caution => 1,
        RiskTier::Review => 2,
    }
}

/// One thing to remove: a hotspot group, with what removing it is expected
/// to free. Field names ARE the wire keys.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReclaimPlanItem {
    pub rule_id: String,
    pub label: String,
    pub category: Category,
    pub risk_tier: RiskTier,
    /// The classifier's one sentence: what this is, what brings it back,
    /// what the tier rests on.
    pub why: String,
    /// The owning tool's own clean command when one exists (`cargo clean`);
    /// the script names it as the alternative to moving the paths.
    pub command: Option<String>,
    /// The group's paths, biggest first (the group's `topPaths`).
    pub paths: Vec<String>,
    /// Sum of the persisted private bytes of the included paths, excluding
    /// unlisted roots and paths held back by Git.
    pub expected_freed_bytes: u64,
    /// The group's deduped `diskSize`, for the "lists as" comparison.
    pub disk_size: u64,
}

/// Why groups were left out, so an empty plan is explicable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanSkipped {
    /// Tier `review`, or a category that is never reclaimable (cloud
    /// placeholders, review-first, won't-regenerate). Never a plan item.
    pub review: u64,
    /// Reclaimable, but rated above the requested `maxTier`.
    pub above_tier: u64,
    /// Reclaimable and within tier, but included private bytes under `minBytes`.
    pub below_min_bytes: u64,
    /// Every path of the group sits inside a git work tree and is not
    /// ignored by its rules — committed fixtures, not caches (phantom-2lz).
    /// A group that keeps at least one path is an item instead, its `why`
    /// naming how many paths were held back. Absent in plans stored before
    /// 1.1.0, hence the default.
    #[serde(default)]
    pub tracked: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReclaimPlan {
    pub plan_id: Uuid,
    /// The completed scan the plan was built from — the "before" side of
    /// its verification.
    pub scan_id: Uuid,
    pub root_path: String,
    #[serde(with = "crate::wire_time")]
    pub created_at: DateTime<Utc>,
    pub max_tier: RiskTier,
    pub min_bytes: u64,
    /// In the summary's order: stale project artifacts first, then by
    /// category priority, tier (safe first), deduped size descending.
    pub items: Vec<ReclaimPlanItem>,
    pub item_count: u64,
    /// Σ `expectedFreedBytes` over the items.
    pub expected_freed_bytes: u64,
    pub skipped: PlanSkipped,
}

/// Select the groups a caller may act on and shape them as a plan.
///
/// `held_back_by_git` answers, per path, "is this inside a git work tree
/// and NOT ignored by its rules?" — the API passes
/// [`crate::gitignore::held_back_by_git`]; tests pass a closure. Such a
/// path is git data, never a plan path (dogfood 2026-09-09 moved 22
/// committed fixture directories).
///
/// `private_bytes` reads a path's private-byte aggregate from this scan.
/// Hotspot groups contain only their biggest five paths, so their whole
/// group size is not a promise the script can keep (phantom-0a7).
pub fn build_plan(
    scan: &Scan,
    summary: &HotspotsSummary,
    options: PlanOptions,
    held_back_by_git: &dyn Fn(&str) -> bool,
    private_bytes: &dyn Fn(&str) -> crate::Result<u64>,
) -> crate::Result<ReclaimPlan> {
    // The type cannot ask for review groups.
    let max_tier = match options.max_tier {
        RiskTier::Review => RiskTier::Caution,
        t => t,
    };
    let mut items = Vec::new();
    let mut skipped = PlanSkipped::default();
    for group in &summary.groups {
        if !group.category.is_reclaimable() || group.risk_tier == RiskTier::Review {
            skipped.review += 1;
            continue;
        }
        if tier_rank(group.risk_tier) > tier_rank(max_tier) {
            skipped.above_tier += 1;
            continue;
        }
        if group.top_paths.is_empty() {
            skipped.below_min_bytes += 1;
            continue;
        }
        let (kept, held): (Vec<String>, Vec<String>) =
            group.top_paths.iter().cloned().partition(|p| !held_back_by_git(p));
        if kept.is_empty() {
            skipped.tracked += 1;
            continue;
        }
        let expected = kept.iter().try_fold(0_u64, |total, path| {
            private_bytes(path).map(|bytes| total.saturating_add(bytes))
        })?;
        if expected < options.min_bytes {
            skipped.below_min_bytes += 1;
            continue;
        }
        let mut item = item_from(group);
        item.paths = kept;
        item.expected_freed_bytes = expected;
        if !held.is_empty() {
            item.why = format!(
                "{} {} path{} held back: inside a git work tree and not ignored by its .gitignore, so git data rather than a cache.",
                item.why,
                held.len(),
                if held.len() == 1 { "" } else { "s" }
            );
        }
        items.push(item);
    }
    let expected_freed_bytes = items.iter().fold(0_u64, |total, item| {
        total.saturating_add(item.expected_freed_bytes)
    });
    Ok(ReclaimPlan {
        plan_id: Uuid::new_v4(),
        scan_id: scan.id,
        root_path: scan.root_path.clone(),
        created_at: Utc::now(),
        max_tier,
        min_bytes: options.min_bytes,
        item_count: items.len() as u64,
        expected_freed_bytes,
        items,
        skipped,
    })
}

fn item_from(group: &HotspotGroup) -> ReclaimPlanItem {
    ReclaimPlanItem {
        rule_id: group.rule_id.clone(),
        label: group.label.clone(),
        category: group.category,
        risk_tier: group.risk_tier,
        why: group.why.clone(),
        command: group.command.clone(),
        paths: group.top_paths.clone(),
        expected_freed_bytes: 0,
        disk_size: group.disk_size,
    }
}

// --- The script -----------------------------------------------------------------

/// POSIX single-quote a string for `sh`: wrap in `'…'`, and write an
/// embedded `'` as `'\''`. Safe for any byte sequence a path can hold.
pub fn sh_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// The plan as a shell script (the rmlint / QDirStat pattern). DRY RUN by
/// default: prints `would move: <path>` per path. `PHANTOM_APPLY=1` moves
/// each path into `$HOME/.Trash/phantom-<planId>/` and appends a line per
/// move to `$HOME/.Trash/phantom-<planId>.log`. Nothing is ever unlinked —
/// emptying the Trash stays the user's act. Paths are single-quoted, so a
/// path with spaces, `$`, `&` or a quote is inert. A move that fails (macOS
/// refuses to rename `~/Library/Caches`, for one) is printed as `FAILED:
/// <path> — <reason>`, logged as `<path>\tFAILED\t<reason>`, and the run
/// goes on to the next path; the script ends with `moved N, failed N,
/// skipped N` and exits 1 only if something failed.
pub fn script(plan: &ReclaimPlan) -> String {
    let id = plan.plan_id;
    let mut s = String::new();
    s.push_str("#!/bin/sh\n");
    s.push_str(&format!(
        "# Phantom reclaim plan {id}\n# built {} from scan {} of {}\n",
        crate::wire_time::to_wire(&plan.created_at),
        plan.scan_id,
        plan.root_path
    ));
    s.push_str(&format!(
        "# {} item(s), expected to free {} bytes ({}).\n",
        plan.item_count,
        plan.expected_freed_bytes,
        crate::format::format_size(plan.expected_freed_bytes)
    ));
    s.push_str(
        "#\n# DRY RUN by default: prints what it would move and touches nothing.\n\
         # Run with PHANTOM_APPLY=1 to MOVE each path into the Trash folder below.\n\
         # Nothing is deleted: empty the Trash yourself once you are satisfied.\n\
         # Before applying, TWO checks Phantom cannot make for you:\n\
         #  1. Builds in flight — pgrep -fl 'cargo|swift-build|xcodebuild|npm|gradle|mvn'.\n\
         #     A target/ or .build/ a tool is writing to right now must not move under it.\n\
         #  2. Paths a long-lived TOOL is launched FROM, even though nothing is building:\n\
         #     MCP servers registered in ~/.claude.json, launchd plists, PATH shims. Such a\n\
         #     path is rebuildable and still load-bearing — moving it breaks the tool until\n\
         #     someone rebuilds it (2026-09-16: this took out an MCP server whose command\n\
         #     was <repo>/rust/target/release/<bin>). grep your registrations for the paths\n\
         #     below. Either way: remove that apply line, or wait.\n",
    );
    s.push_str(&format!(
        "# Afterwards run `phantom verify {id}` — THIS plan's id, which names the Trash\n\
         # folder above. Verifying a different plan built from the same scan reports a\n\
         # wrong number, so use this id and no other.\n"
    ));
    // No `set -e`: a failed move is reported and the run goes on (dogfood
    // 2026-09-09: `mv ~/Library/Caches` was denied and the ten items after
    // it were never attempted). The exit status says whether anything failed.
    s.push_str("set -u\n");
    // `$HOME` + the ONE suffix definition the two verify readers use, so the
    // folder the script writes and the folder they look for cannot drift.
    let trash = trash_dir_suffix(plan.plan_id);
    s.push_str(&format!("TRASH=\"$HOME{trash}\"\n"));
    s.push_str(&format!("LOG=\"$HOME{trash}.log\"\n"));
    s.push_str("MOVED=0; FAILED=0; SKIPPED=0; WOULD=0\n");
    s.push_str(
        "apply() {\n\
         \x20 if [ ! -e \"$1\" ]; then echo \"skip (gone): $1\"; SKIPPED=$((SKIPPED + 1)); return 0; fi\n\
         \x20 if [ \"${PHANTOM_APPLY:-0}\" = 1 ]; then\n\
         \x20   if ! err=$(mkdir -p \"$TRASH\" 2>&1); then\n\
         \x20     echo \"FAILED: $1 — cannot create $TRASH: $err\"; FAILED=$((FAILED + 1)); return 0\n\
         \x20   fi\n\
         \x20   dest=\"$TRASH/$(printf '%s' \"$1\" | tr '/' '_')\"\n\
         \x20   if err=$(mv -- \"$1\" \"$dest\" 2>&1); then\n\
         \x20     printf '%s\\t%s\\n' \"$1\" \"$dest\" >> \"$LOG\"\n\
         \x20     echo \"moved: $1 -> $dest\"; MOVED=$((MOVED + 1))\n\
         \x20   else\n\
         \x20     printf '%s\\tFAILED\\t%s\\n' \"$1\" \"$err\" >> \"$LOG\"\n\
         \x20     echo \"FAILED: $1 — ${err:-mv failed}\"; FAILED=$((FAILED + 1))\n\
         \x20   fi\n\
         \x20 else\n\
         \x20   echo \"would move: $1\"; WOULD=$((WOULD + 1))\n\
         \x20 fi\n\
         }\n",
    );
    for (n, item) in plan.items.iter().enumerate() {
        s.push('\n');
        s.push_str(&format!(
            "# {}. {} [{}] — expected {} ({})\n#    {}\n",
            n + 1,
            item.label,
            item.risk_tier.as_str(),
            crate::format::format_size(item.expected_freed_bytes),
            item.category.as_str(),
            item.why
        ));
        if let Some(cmd) = &item.command {
            s.push_str(&format!(
                "#    alternative: run `{cmd}` in each project directory instead of moving\n"
            ));
        }
        for p in &item.paths {
            s.push_str(&format!("apply {}\n", sh_quote(p)));
        }
    }
    s.push('\n');
    s.push_str(&format!(
        "if [ \"${{PHANTOM_APPLY:-0}}\" = 1 ]; then\n\
         \x20 echo \"moved $MOVED, failed $FAILED, skipped $SKIPPED (already gone)\"\n\
         \x20 echo \"log: $LOG\"\n\
         else\n\
         \x20 echo \"dry run — would move $WOULD path(s), $SKIPPED already gone; re-run with PHANTOM_APPLY=1 to move to Trash\"\n\
         fi\n\
         echo \"expected to free {}; verify with: phantom verify {id}\"\n\
         [ \"$FAILED\" -eq 0 ] || exit 1\n",
        crate::format::format_size(plan.expected_freed_bytes)
    ));
    s
}

// --- Verification ---------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifyItem {
    pub rule_id: String,
    pub label: String,
    pub paths: Vec<String>,
    pub expected_freed_bytes: u64,
    /// Σ over the item's paths of (before dir size − after dir size), a
    /// path absent from the rescan counting as 0 after. Null when none of
    /// the paths was a persisted directory in the before scan (nothing to
    /// measure against).
    pub actual_freed_bytes: Option<i64>,
    /// Σ of the paths' directory sizes in the before scan; null as above.
    pub before_bytes: Option<u64>,
    /// Σ in the after scan (0 for paths that are gone); null as above.
    pub after_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReclaimVerification {
    pub plan_id: Uuid,
    /// The plan's scan.
    pub before_scan_id: Uuid,
    /// The rescan.
    pub after_scan_id: Uuid,
    pub root_path: String,
    #[serde(with = "crate::wire_time")]
    pub verified_at: DateTime<Utc>,
    pub items: Vec<VerifyItem>,
    /// The plan's promise.
    pub expected_freed_bytes: u64,
    /// The headline. before.totalDiskSize − after.totalDiskSize: everything
    /// that changed under the root, not just the plan's paths — EXCEPT when
    /// the plan's own Trash folder (`…/.Trash/phantom-<planId>`) is a
    /// directory of the rescan, i.e. the Trash sits inside the root (a home
    /// scan): the moved bytes are then still counted under the root, so the
    /// headline is Σ of the items' `actualFreedBytes` instead. Signed;
    /// negative means the measured tree GREW between the scans. Either way
    /// the bytes are in the Trash: the space returns when it is emptied.
    pub actual_freed_bytes: i64,
    /// expected − actual (signed). Positive: less came back than promised.
    pub shortfall_bytes: i64,
    /// |shortfall| ≤ 5% of expected (TOLERANCE), in EITHER direction — a
    /// large over-delivery means something outside the plan moved too, and
    /// that is worth a look. With expected 0 it is simply "nothing grew".
    pub within_tolerance: bool,
}

/// Compare the plan's promise with what a rescan shows. `before_dirs` and
/// `after_dirs` are the store's `dir_sizes` for the two scans (the same
/// aggregates the diff engine reads).
pub fn verify_plan(
    plan: &ReclaimPlan,
    before: &Scan,
    before_dirs: &[(String, u64)],
    after: &Scan,
    after_dirs: &[(String, u64)],
) -> ReclaimVerification {
    let before_map: HashMap<&str, u64> = before_dirs.iter().map(|(p, s)| (p.as_str(), *s)).collect();
    let after_map: HashMap<&str, u64> = after_dirs.iter().map(|(p, s)| (p.as_str(), *s)).collect();
    let items: Vec<VerifyItem> = plan
        .items
        .iter()
        .map(|item| {
            let mut before_sum: Option<u64> = None;
            let mut after_sum: u64 = 0;
            for p in &item.paths {
                if let Some(b) = before_map.get(p.as_str()) {
                    before_sum = Some(before_sum.unwrap_or(0) + b);
                    after_sum += after_map.get(p.as_str()).copied().unwrap_or(0);
                }
            }
            let (before_bytes, after_bytes, actual) = match before_sum {
                Some(b) => (Some(b), Some(after_sum), Some(signed(b, after_sum))),
                None => (None, None, None),
            };
            VerifyItem {
                rule_id: item.rule_id.clone(),
                label: item.label.clone(),
                paths: item.paths.clone(),
                expected_freed_bytes: item.expected_freed_bytes,
                actual_freed_bytes: actual,
                before_bytes,
                after_bytes,
            }
        })
        .collect();
    // The script's Trash folder for THIS plan. When it is a directory row of
    // the rescan it sits inside the root (a home-directory scan), so the
    // moved bytes are still counted under the root and the root delta says
    // nothing about the plan — dogfood 2026-09-09 read −782 MB after freeing
    // 63 GB. The headline is then Σ per-item actuals: what the plan's own
    // paths shrank by. The space itself returns when the Trash is emptied.
    let trash_suffix = trash_dir_suffix(plan.plan_id);
    let trash_inside_root = after_map.keys().any(|p| p.ends_with(&trash_suffix));
    let actual_freed_bytes = if trash_inside_root {
        items
            .iter()
            .filter_map(|i| i.actual_freed_bytes)
            .fold(0i64, |acc, b| acc.saturating_add(b))
    } else {
        signed(before.total_disk_size, after.total_disk_size)
    };
    let expected = plan.expected_freed_bytes;
    let shortfall_bytes = i64::try_from(expected as i128 - actual_freed_bytes as i128)
        .unwrap_or(i64::MAX);
    let within_tolerance = if expected == 0 {
        actual_freed_bytes >= 0
    } else {
        (shortfall_bytes.unsigned_abs() as f64) <= expected as f64 * TOLERANCE
    };
    ReclaimVerification {
        plan_id: plan.plan_id,
        before_scan_id: before.id,
        after_scan_id: after.id,
        root_path: plan.root_path.clone(),
        verified_at: Utc::now(),
        items,
        expected_freed_bytes: expected,
        actual_freed_bytes,
        shortfall_bytes,
        within_tolerance,
    }
}

/// The plan-Trash folder name the generated script uses, for `plan_id`.
/// One definition, so the script writer and the two readers below cannot
/// drift apart.
fn trash_dir_suffix(plan_id: Uuid) -> String {
    format!("/.Trash/phantom-{plan_id}")
}

/// Does the rescan contain a plan-Trash folder belonging to a DIFFERENT
/// plan? If so, return that plan's folder path.
///
/// Why this exists (phantom-9wc): [`verify_plan`]'s headline is Σ per-item
/// actuals only when THIS plan's Trash folder is a directory of the rescan.
/// When it is absent the headline falls back to the root delta — correct
/// when the bytes left the root, and catastrophically wrong when they
/// merely moved to some OTHER plan's Trash inside the same root, because
/// the root delta then measures nothing (the 2026-09-16 gate run: a −9.3 GB
/// root delta after freeing 57.9 GB). The CLI makes that easy to hit: `plan
/// --script` conflicts with `--json`, so an auditable run builds two plans
/// with identical items, runs one script and can verify the other id.
///
/// The caller refuses to verify in that case rather than reporting a
/// confident wrong number. Only a foreign folder is suspicious: paths
/// deleted outright, or moved out of the root, leave the root delta
/// meaningful and are verified normally.
pub fn foreign_plan_trash(plan_id: Uuid, after_dirs: &[(String, u64)]) -> Option<String> {
    let ours = trash_dir_suffix(plan_id);
    if after_dirs.iter().any(|(p, _)| p.ends_with(&ours)) {
        return None;
    }
    after_dirs
        .iter()
        .map(|(p, _)| p)
        .find(|p| {
            // A plan folder is `<trash>/phantom-<uuid>` and nothing below it.
            match p.rsplit_once("/.Trash/") {
                Some((_, tail)) => {
                    tail.strip_prefix("phantom-").is_some_and(|id| Uuid::parse_str(id).is_ok())
                }
                None => false,
            }
        })
        .cloned()
}

/// before − after without u64 wrap; saturates at the i64 range.
fn signed(before: u64, after: u64) -> i64 {
    i64::try_from(before as i128 - after as i128).unwrap_or(if before > after {
        i64::MAX
    } else {
        i64::MIN
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const RAW_SCAN: &str = include_str!("../../../tests/fixtures/scan-complete.json");
    const RAW_HOTSPOTS: &str = include_str!("../../../tests/fixtures/hotspots-summary.json");
    const RAW_PLAN: &str = include_str!("../../../tests/fixtures/reclaim-plan.json");
    const RAW_VERIFICATION: &str =
        include_str!("../../../tests/fixtures/reclaim-verification.json");

    fn scan() -> Scan {
        serde_json::from_str(RAW_SCAN).unwrap()
    }
    fn summary() -> HotspotsSummary {
        serde_json::from_str(RAW_HOTSPOTS).unwrap()
    }

    fn private_bytes(path: &str) -> crate::Result<u64> {
        match path {
            "/Users/ghost/Code/dormant/target" => Ok(17_179_869_184),
            "/Users/ghost/.cache" => Ok(1_073_741_824),
            "/opt/homebrew/Cellar" => Ok(2_147_483_648),
            "/Users/ghost/Code/phantom/tests/fixtures/maven/target" => Ok(4096),
            _ => Err(crate::CoreError::NotFound(path.into())),
        }
    }

    // The fixture summary: cargo-target (safe, 17 GB private), dot-cache
    // (caution, 1 GB private), homebrew-cellar (caution, 2 GB private),
    // cloud-dataloaded (review, not reclaimable).

    #[test]
    fn default_plan_takes_only_safe_reclaimable_groups() {
        let plan = build_plan(&scan(), &summary(), PlanOptions::default(), &|_| false, &private_bytes).unwrap();
        assert_eq!(plan.items.len(), 1);
        assert_eq!(plan.item_count, 1);
        let item = &plan.items[0];
        assert_eq!(item.rule_id, "cargo-target");
        assert_eq!(item.risk_tier, RiskTier::Safe);
        assert_eq!(item.paths, vec!["/Users/ghost/Code/dormant/target"]);
        assert_eq!(item.expected_freed_bytes, 17_179_869_184, "privateSize, not diskSize");
        assert_eq!(item.command.as_deref(), Some("cargo clean"));
        assert_eq!(plan.expected_freed_bytes, 17_179_869_184);
        assert_eq!(plan.skipped, PlanSkipped { review: 1, above_tier: 2, below_min_bytes: 0, tracked: 0 });
        assert_eq!(plan.scan_id, scan().id);
        assert_eq!(plan.root_path, "/Users/ghost/Code");
        assert_eq!(plan.max_tier, RiskTier::Safe);
    }

    #[test]
    fn caution_admits_the_caution_groups_in_summary_order_and_review_never() {
        let plan = build_plan(
            &scan(),
            &summary(),
            PlanOptions { max_tier: RiskTier::Caution, min_bytes: 0 },
            &|_| false,
            &private_bytes,
        ).unwrap();
        let ids: Vec<&str> = plan.items.iter().map(|i| i.rule_id.as_str()).collect();
        assert_eq!(ids, ["cargo-target", "dot-cache", "homebrew-cellar"]);
        assert_eq!(plan.expected_freed_bytes, 17_179_869_184 + 1_073_741_824 + 2_147_483_648);
        assert_eq!(plan.skipped, PlanSkipped { review: 1, above_tier: 0, below_min_bytes: 0, tracked: 0 });
        // Asking for review is clamped: the same plan, and the plan says caution.
        let clamped = build_plan(
            &scan(),
            &summary(),
            PlanOptions { max_tier: RiskTier::Review, min_bytes: 0 },
            &|_| false,
            &private_bytes,
        ).unwrap();
        assert_eq!(clamped.max_tier, RiskTier::Caution);
        assert_eq!(clamped.items.len(), 3, "review groups never become items");
    }

    #[test]
    fn min_bytes_floors_by_private_size() {
        let plan = build_plan(
            &scan(),
            &summary(),
            PlanOptions { max_tier: RiskTier::Caution, min_bytes: 2_000_000_000 },
            &|_| false,
            &private_bytes,
        ).unwrap();
        let ids: Vec<&str> = plan.items.iter().map(|i| i.rule_id.as_str()).collect();
        // dot-cache LISTS 5 GB but frees 1 GB — the floor reads the honest number.
        assert_eq!(ids, ["cargo-target", "homebrew-cellar"]);
        assert_eq!(plan.skipped.below_min_bytes, 1);
    }

    /// Dogfood 2026-09-09 (phantom-2lz): the safe plan for ~ moved 22
    /// committed fixture directories under tests/fixtures/projects —
    /// correctly classified artifacts, but git data. A path inside a git
    /// work tree that its .gitignore rules do not ignore is held back: the
    /// item keeps only the remaining paths and its why says so; a group
    /// with nothing left is counted in `skipped.tracked`.
    #[test]
    fn paths_held_back_by_git_leave_the_plan_and_are_counted() {
        let mut summary = summary();
        let cargo = summary.groups.iter_mut().find(|g| g.rule_id == "cargo-target").unwrap();
        cargo.top_paths = vec![
            "/Users/ghost/Code/dormant/target".to_string(),
            "/Users/ghost/Code/phantom/tests/fixtures/maven/target".to_string(),
        ];
        let held: &dyn Fn(&str) -> bool = &|p| p.contains("/tests/fixtures/");
        let plan = build_plan(&scan(), &summary, PlanOptions::default(), held, &private_bytes).unwrap();
        assert_eq!(plan.items.len(), 1);
        assert_eq!(plan.items[0].paths, vec!["/Users/ghost/Code/dormant/target".to_string()]);
        assert!(
            plan.items[0].why.contains("1 path held back") && plan.items[0].why.contains("git"),
            "the why says what was left out and why: {}",
            plan.items[0].why
        );
        assert_eq!(plan.skipped.tracked, 0, "the group still has a path, so it is an item");
        assert_eq!(plan.expected_freed_bytes, 17_179_869_184);

        // Every path held back: no item, counted.
        let all: &dyn Fn(&str) -> bool = &|_| true;
        let plan = build_plan(&scan(), &summary, PlanOptions::default(), all, &private_bytes).unwrap();
        assert!(plan.items.is_empty());
        assert_eq!(plan.item_count, 0);
        assert_eq!(plan.expected_freed_bytes, 0);
        assert_eq!(plan.skipped, PlanSkipped { review: 1, above_tier: 2, below_min_bytes: 0, tracked: 1 });

        // Nothing held back: the plan is exactly what it was.
        let none: &dyn Fn(&str) -> bool = &|_| false;
        let plan = build_plan(&scan(), &summary, PlanOptions::default(), none, &private_bytes).unwrap();
        assert_eq!(plan.items[0].paths.len(), 2);
        assert_eq!(plan.expected_freed_bytes, 17_179_869_184 + 4096);
        assert_eq!(plan.skipped.tracked, 0);
        assert!(!plan.items[0].why.contains("held back"));
    }

    #[test]
    fn listed_private_bytes_override_the_group_total_and_drive_the_floor() {
        let mut summary = summary();
        summary.groups.truncate(1);
        let plan = build_plan(
            &scan(),
            &summary,
            PlanOptions::default(),
            &|_| false,
            &|_| Ok(256),
        )
        .unwrap();
        assert_eq!(plan.items[0].expected_freed_bytes, 256);
        assert_eq!(plan.expected_freed_bytes, 256);
        let below_floor = build_plan(
            &scan(),
            &summary,
            PlanOptions {
                min_bytes: 257,
                ..PlanOptions::default()
            },
            &|_| false,
            &|_| Ok(256),
        )
        .unwrap();
        assert!(below_floor.items.is_empty());
        assert_eq!(below_floor.skipped.below_min_bytes, 1);
    }

    #[test]
    fn missing_path_measurements_fail_the_plan_instead_of_guessing() {
        let result = build_plan(
            &scan(),
            &summary(),
            PlanOptions::default(),
            &|_| false,
            &|p| Err(crate::CoreError::NotFound(p.into())),
        );
        assert!(matches!(result, Err(crate::CoreError::NotFound(_))));
    }

    #[test]
    fn plan_fixture_parses_from_raw_bytes_and_round_trips_its_keys() {
        let plan: ReclaimPlan = serde_json::from_str(RAW_PLAN).unwrap();
        assert_eq!(plan.item_count, 1);
        assert_eq!(plan.items[0].category, Category::StaleProjectArtifact);
        assert_eq!(plan.max_tier, RiskTier::Safe);
        let raw: serde_json::Value = serde_json::from_str(RAW_PLAN).unwrap();
        let out = serde_json::to_value(&plan).unwrap();
        assert_eq!(out, raw, "encode must reproduce the fixture exactly (camelCase, nulls present)");
    }

    #[test]
    fn verification_fixture_parses_from_raw_bytes_and_round_trips() {
        let v: ReclaimVerification = serde_json::from_str(RAW_VERIFICATION).unwrap();
        assert!(v.within_tolerance);
        assert_eq!(v.items[0].actual_freed_bytes, Some(17_179_869_184));
        let raw: serde_json::Value = serde_json::from_str(RAW_VERIFICATION).unwrap();
        assert_eq!(serde_json::to_value(&v).unwrap(), raw);
    }

    // --- script ---------------------------------------------------------------

    #[test]
    fn sh_quote_makes_any_path_inert() {
        assert_eq!(sh_quote("/a b"), "'/a b'");
        assert_eq!(sh_quote("/it's"), "'/it'\\''s'");
        assert_eq!(sh_quote("/$HOME/`x`/&"), "'/$HOME/`x`/&'");
    }

    fn plan_with_paths(paths: &[&str]) -> ReclaimPlan {
        let mut plan: ReclaimPlan = serde_json::from_str(RAW_PLAN).unwrap();
        plan.items[0].paths = paths.iter().map(|p| p.to_string()).collect();
        plan
    }

    /// Run the generated script through /bin/sh in a throwaway HOME: dry
    /// run must print and touch nothing; PHANTOM_APPLY=1 must MOVE the path
    /// into the Trash folder and log it, never unlink it.
    #[test]
    fn script_dry_runs_by_default_and_moves_to_trash_on_apply() {
        let home = tempfile::tempdir().unwrap();
        let victim_dir = home.path().join("proj it's here");
        std::fs::create_dir_all(victim_dir.join("target")).unwrap();
        let victim = victim_dir.join("target");
        std::fs::write(victim.join("a.o"), b"bytes").unwrap();
        let plan = plan_with_paths(&[victim.to_str().unwrap()]);
        let text = script(&plan);
        let script_path = home.path().join("plan.sh");
        std::fs::write(&script_path, &text).unwrap();

        // Syntax-clean.
        let syntax = std::process::Command::new("/bin/sh")
            .arg("-n")
            .arg(&script_path)
            .status()
            .unwrap();
        assert!(syntax.success(), "script must parse: {text}");

        let run = |apply: bool| {
            let mut cmd = std::process::Command::new("/bin/sh");
            cmd.arg(&script_path)
                .env_clear()
                .env("PATH", "/bin:/usr/bin")
                .env("HOME", home.path());
            if apply {
                cmd.env("PHANTOM_APPLY", "1");
            }
            let out = cmd.output().unwrap();
            assert!(out.status.success(), "script failed: {}", String::from_utf8_lossy(&out.stderr));
            String::from_utf8_lossy(&out.stdout).to_string()
        };

        let dry = run(false);
        assert!(dry.contains("would move: ") && dry.contains("target"), "{dry}");
        assert!(dry.contains("dry run"), "{dry}");
        assert!(victim.join("a.o").exists(), "dry run must not touch the path");
        assert!(!home.path().join(".Trash").exists(), "dry run must not create the trash folder");

        let applied = run(true);
        assert!(applied.contains("moved: "), "{applied}");
        assert!(!victim.exists(), "apply moves the path away");
        let trash = home.path().join(".Trash").join(format!("phantom-{}", plan.plan_id));
        let moved: Vec<_> = std::fs::read_dir(&trash).unwrap().map(|e| e.unwrap().path()).collect();
        assert_eq!(moved.len(), 1, "exactly one entry in the plan's trash folder");
        assert!(moved[0].join("a.o").exists(), "the bytes survive the move — nothing is unlinked");
        let log = std::fs::read_to_string(home.path().join(".Trash").join(format!("phantom-{}.log", plan.plan_id))).unwrap();
        assert!(log.contains(victim.to_str().unwrap()), "{log}");

        // A second apply skips the now-gone path instead of failing.
        let again = run(true);
        assert!(again.contains("skip (gone)"), "{again}");
    }

    /// Dogfood 2026-09-09 (phantom-lel): `mv ~/Library/Caches` was denied
    /// by macOS and `set -e` killed the script at item 95 of 105 — the ten
    /// items after it were never attempted, exit 1, no summary. A failed
    /// move must be reported, the run must go on, and the script must end
    /// with moved/failed/skipped counts and exit non-zero only because
    /// something failed.
    #[test]
    fn script_reports_a_failed_move_continues_and_summarizes() {
        let home = tempfile::tempdir().unwrap();
        // A victim whose PARENT is read-only: rename needs write on the
        // parent, so mv fails with Permission denied, like ~/Library/Caches.
        let locked_parent = home.path().join("locked");
        let stuck = locked_parent.join("target");
        std::fs::create_dir_all(&stuck).unwrap();
        std::fs::write(stuck.join("a.o"), b"bytes").unwrap();
        let movable = home.path().join("proj").join("target");
        std::fs::create_dir_all(&movable).unwrap();
        std::fs::write(movable.join("b.o"), b"bytes").unwrap();
        let plan = plan_with_paths(&[stuck.to_str().unwrap(), movable.to_str().unwrap()]);
        let script_path = home.path().join("plan.sh");
        std::fs::write(&script_path, script(&plan)).unwrap();

        let run = |apply: bool| {
            let mut cmd = std::process::Command::new("/bin/sh");
            cmd.arg(&script_path)
                .env_clear()
                .env("PATH", "/bin:/usr/bin")
                .env("HOME", home.path());
            if apply {
                cmd.env("PHANTOM_APPLY", "1");
            }
            let out = cmd.output().unwrap();
            (out.status.code(), String::from_utf8_lossy(&out.stdout).to_string())
        };

        // Dry run: both would move, nothing fails, exit 0, counts printed.
        let (code, dry) = run(false);
        assert_eq!(code, Some(0), "{dry}");
        assert!(dry.contains("would move 2 path(s)"), "{dry}");

        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&locked_parent, std::fs::Permissions::from_mode(0o555)).unwrap();
        let (code, applied) = run(true);
        // Restore before asserting so a failure still lets the tempdir go.
        std::fs::set_permissions(&locked_parent, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(code, Some(1), "a failed move is a non-zero exit, after the run: {applied}");
        let stuck_s = stuck.to_str().unwrap();
        let failed_line = applied.lines().find(|l| l.starts_with("FAILED: ")).unwrap_or_else(|| panic!("{applied}"));
        assert!(failed_line.contains(stuck_s) && failed_line.contains("Permission denied"), "{failed_line}");
        assert!(applied.contains(&format!("moved: {}", movable.display())), "the NEXT item still ran: {applied}");
        assert!(!movable.exists() && stuck.join("a.o").exists(), "movable moved, stuck untouched");
        assert!(applied.contains("moved 1, failed 1, skipped 0"), "summary line: {applied}");
        let log = std::fs::read_to_string(home.path().join(".Trash").join(format!("phantom-{}.log", plan.plan_id))).unwrap();
        assert!(log.lines().any(|l| l.starts_with(stuck_s) && l.contains("\tFAILED\t")), "the log records the failure: {log}");
    }

    #[test]
    fn script_names_the_command_alternative_and_the_verify_step() {
        let plan: ReclaimPlan = serde_json::from_str(RAW_PLAN).unwrap();
        let text = script(&plan);
        assert!(text.starts_with("#!/bin/sh\n"));
        assert!(text.contains("alternative: run `cargo clean`"), "{text}");
        assert!(text.contains(&format!("phantom verify {}", plan.plan_id)));
        assert!(text.contains("apply '/Users/ghost/Code/dormant/target'"));
        assert!(!text.contains("rm "), "the script never deletes");
        // phantom-djf: the header tells the runner to check for builds in
        // flight before applying — Phantom does not detect a live cargo/xcodebuild.
        assert!(text.contains("Builds in flight"), "{text}");
        // phantom-o2f: nor does it detect that a path is where a long-lived
        // tool is launched FROM. The 2026-09-16 gate run moved an MCP
        // server's own binary; the header has to name that case too.
        assert!(text.contains("launched FROM"), "{text}");
        assert!(text.contains("~/.claude.json"), "{text}");
        // phantom-9wc: the header names THIS plan's id for the verify step,
        // because two plans off one scan have identical items and only this
        // id names the Trash folder the script writes.
        assert!(
            text.contains(&format!("phantom verify {}` — THIS plan's id", plan.plan_id)),
            "{text}"
        );
    }

    // --- verification -----------------------------------------------------------

    fn scan_with(id: Uuid, total: u64) -> Scan {
        let mut s = scan();
        s.id = id;
        s.total_disk_size = total;
        s
    }

    #[test]
    fn verification_sums_per_item_and_measures_the_root_delta() {
        let plan = plan_with_paths(&["/r/a/target", "/r/b/target"]);
        let before = scan_with(plan.scan_id, 10_000);
        let after = scan_with(Uuid::new_v4(), 3_000);
        let before_dirs = vec![
            ("/r".to_string(), 10_000),
            ("/r/a/target".to_string(), 4_000),
            ("/r/b/target".to_string(), 3_000),
        ];
        // a/target gone, b/target shrank to 500
        let after_dirs = vec![("/r".to_string(), 3_000), ("/r/b/target".to_string(), 500)];
        let v = verify_plan(&plan, &before, &before_dirs, &after, &after_dirs);
        assert_eq!(v.items.len(), 1);
        assert_eq!(v.items[0].before_bytes, Some(7_000));
        assert_eq!(v.items[0].after_bytes, Some(500));
        assert_eq!(v.items[0].actual_freed_bytes, Some(6_500));
        assert_eq!(v.actual_freed_bytes, 7_000, "root delta: everything that changed");
        assert_eq!(v.before_scan_id, before.id);
        assert_eq!(v.after_scan_id, after.id);
    }

    /// Dogfood 2026-09-09 (phantom-grw): the script moves paths into
    /// `$HOME/.Trash/phantom-<planId>/`, which sits INSIDE a home-directory
    /// scan, so the rescan counts the moved bytes again and the root delta
    /// reads ~0 (−782 MB after freeing 63 GB) while every per-item line is
    /// right. When the plan's own Trash folder is a directory row of the
    /// rescan, the headline is Σ per-item actuals, not the root delta.
    #[test]
    fn headline_is_the_items_sum_when_the_plans_trash_folder_sits_inside_the_root() {
        let mut plan = plan_with_paths(&["/r/a/target", "/r/b/target"]);
        plan.expected_freed_bytes = 7_000;
        plan.items[0].expected_freed_bytes = 7_000;
        let before = scan_with(plan.scan_id, 10_000);
        // Both targets moved into the Trash under the root; 200 bytes grew
        // elsewhere. The root total barely moved — up, in fact.
        let after = scan_with(Uuid::new_v4(), 10_200);
        let before_dirs = vec![
            ("/r".to_string(), 10_000),
            ("/r/a/target".to_string(), 4_000),
            ("/r/b/target".to_string(), 3_000),
        ];
        let trash = format!("/r/.Trash/phantom-{}", plan.plan_id);
        let after_dirs = vec![
            ("/r".to_string(), 10_200),
            ("/r/.Trash".to_string(), 7_000),
            (trash, 7_000),
        ];
        let v = verify_plan(&plan, &before, &before_dirs, &after, &after_dirs);
        assert_eq!(v.items[0].actual_freed_bytes, Some(7_000));
        assert_eq!(v.actual_freed_bytes, 7_000, "Σ per-item, not the confounded root delta (−200)");
        assert_eq!(v.shortfall_bytes, 0);
        assert!(v.within_tolerance, "the plan delivered what it promised: {v:?}");

        // Another plan's Trash folder is not this plan's: the root delta
        // stands. This arithmetic is deliberately unchanged, but it is a
        // number no client should ever see — the −200 is the confounded
        // reading. `foreign_plan_trash` detects exactly this shape and the
        // API refuses to verify instead of returning it (phantom-9wc).
        let other = vec![
            ("/r".to_string(), 10_200),
            (format!("/r/.Trash/phantom-{}", Uuid::new_v4()), 7_000),
        ];
        let v = verify_plan(&plan, &before, &before_dirs, &after, &other);
        assert_eq!(v.actual_freed_bytes, -200);
        assert!(!v.within_tolerance);
        assert!(foreign_plan_trash(plan.plan_id, &other).is_some());
    }

    /// phantom-9wc: `plan --script` conflicts with `--json`, so an auditable
    /// CLI run builds two plans with identical items and can run one script
    /// while verifying the other id. That makes the root-delta fallback
    /// silently wrong, so the shape has to be detectable.
    #[test]
    fn a_foreign_plans_trash_folder_is_detected_only_when_ours_is_absent() {
        let ours = Uuid::new_v4();
        let theirs = Uuid::new_v4();
        let ours_dir = format!("/r/.Trash/phantom-{ours}");
        let theirs_dir = format!("/r/.Trash/phantom-{theirs}");

        // Ours present — nothing to report, even alongside a foreign folder
        // the user has not emptied yet.
        let both = vec![(ours_dir.clone(), 7_000), (theirs_dir.clone(), 1_000)];
        assert_eq!(foreign_plan_trash(ours, &both), None);
        assert_eq!(foreign_plan_trash(ours, &[(ours_dir.clone(), 7_000)]), None);

        // Only theirs — the 9wc shape.
        assert_eq!(
            foreign_plan_trash(ours, &[(theirs_dir.clone(), 7_000)]),
            Some(theirs_dir.clone())
        );
        // Symmetric: from the other plan's point of view ours is the foreign
        // one — but only when THEIR folder is the one missing. With both
        // present, each plan finds its own and reports nothing.
        assert_eq!(foreign_plan_trash(theirs, &[(ours_dir.clone(), 7_000)]), Some(ours_dir));
        assert_eq!(foreign_plan_trash(theirs, &both), None);

        // Nothing that merely LOOKS like a plan folder counts: the Trash
        // itself, unrelated junk, a non-uuid suffix, or a path BELOW a plan
        // folder (only the folder row itself names a plan id).
        for dirs in [
            vec![("/r".to_string(), 10_000)],
            vec![("/r/.Trash".to_string(), 7_000)],
            vec![("/r/.Trash/Banshee-0.1.4.dmg".to_string(), 7_000)],
            vec![("/r/.Trash/phantom-not-a-uuid".to_string(), 7_000)],
            vec![(format!("{theirs_dir}/_Users_x_Code_a_target"), 7_000)],
        ] {
            assert_eq!(foreign_plan_trash(ours, &dirs), None, "false positive on {dirs:?}");
        }
    }

    /// The script writes the folder that the two verify readers look for.
    /// Pin them to one definition — a drift here reinstates the −9.3 GB
    /// headline without any test noticing.
    #[test]
    fn the_script_moves_into_the_folder_the_verifier_looks_for() {
        let plan = plan_with_paths(&["/r/a/target"]);
        let script = script(&plan);
        let suffix = trash_dir_suffix(plan.plan_id);
        assert!(
            script.contains(&format!("TRASH=\"$HOME{suffix}\"")),
            "script must move into $HOME{suffix}: {script}"
        );
        let after_dirs = vec![(format!("/r{suffix}"), 1_000)];
        assert_eq!(foreign_plan_trash(plan.plan_id, &after_dirs), None);
        let v = verify_plan(
            &plan,
            &scan_with(plan.scan_id, 1_000),
            &[("/r/a/target".to_string(), 1_000)],
            &scan_with(Uuid::new_v4(), 1_000),
            &after_dirs,
        );
        assert_eq!(v.actual_freed_bytes, 1_000, "the Σ per-item branch must fire: {v:?}");
    }

    #[test]
    fn tolerance_is_five_percent_of_expected() {
        let mut plan = plan_with_paths(&["/r/t"]);
        plan.expected_freed_bytes = 1_000;
        plan.items[0].expected_freed_bytes = 1_000;
        let before = scan_with(plan.scan_id, 5_000);
        let dirs = vec![("/r/t".to_string(), 1_000)];
        // Freed 960 of 1000 promised: within 5%.
        let v = verify_plan(&plan, &before, &dirs, &scan_with(Uuid::new_v4(), 4_040), &[]);
        assert_eq!(v.shortfall_bytes, 40);
        assert!(v.within_tolerance);
        // Freed 940: outside.
        let v = verify_plan(&plan, &before, &dirs, &scan_with(Uuid::new_v4(), 4_060), &[]);
        assert_eq!(v.shortfall_bytes, 60);
        assert!(!v.within_tolerance);
        // Over-delivered by a lot (other things were removed too) is NOT
        // within: the rule is symmetric — the number disagrees with the
        // promise either way, and the agent should say so.
        let v = verify_plan(&plan, &before, &dirs, &scan_with(Uuid::new_v4(), 1_000), &[]);
        assert!(v.shortfall_bytes < 0 && !v.within_tolerance);
        // Over-delivered within 5% is fine.
        let v = verify_plan(&plan, &before, &dirs, &scan_with(Uuid::new_v4(), 3_960), &[]);
        assert_eq!(v.shortfall_bytes, -40);
        assert!(v.within_tolerance);
        // Tree GREW: negative actual, not within.
        let v = verify_plan(&plan, &before, &dirs, &scan_with(Uuid::new_v4(), 9_000), &[]);
        assert!(v.actual_freed_bytes < 0 && !v.within_tolerance);
    }

    #[test]
    fn a_path_that_was_never_a_directory_row_measures_as_null() {
        let plan = plan_with_paths(&["/r/not-a-dir-row"]);
        let before = scan_with(plan.scan_id, 100);
        let v = verify_plan(&plan, &before, &[("/r".into(), 100)], &scan_with(Uuid::new_v4(), 100), &[]);
        assert_eq!(v.items[0].actual_freed_bytes, None);
        assert_eq!(v.items[0].before_bytes, None);
        assert_eq!(v.items[0].after_bytes, None);
    }

    #[test]
    fn empty_plan_verifies_as_within_when_nothing_grew() {
        let mut plan = plan_with_paths(&[]);
        plan.items.clear();
        plan.item_count = 0;
        plan.expected_freed_bytes = 0;
        let before = scan_with(plan.scan_id, 100);
        let v = verify_plan(&plan, &before, &[], &scan_with(Uuid::new_v4(), 100), &[]);
        assert!(v.within_tolerance && v.actual_freed_bytes == 0 && v.items.is_empty());
        let grew = verify_plan(&plan, &before, &[], &scan_with(Uuid::new_v4(), 200), &[]);
        assert!(!grew.within_tolerance);
    }
}
