// Reclaim plans over HTTP (v1.1 Phase 3, phantom-mkn.7): build a plan from a
// completed scan's hotspots, read it back, render it as a script, and verify
// it against a rescan. Pure arithmetic over persisted rows — nothing here
// walks a disk or touches a file; the script is TEXT the caller runs.
//
//   POST /scans/{id}/plan      {maxTier?, minBytes?}   -> 201 ReclaimPlan
//   GET  /plans/{id}                                   -> ReclaimPlan
//   GET  /plans/{id}/script                            -> text/plain sh
//   POST /plans/{id}/verify    {afterScanId}           -> ReclaimVerification

use axum::{
    Json,
    extract::State,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use phantom_core::{CoreError, RiskTier, ScanStatus, plan};
use serde::Deserialize;
use uuid::Uuid;

use crate::AppState;
use crate::routes::{ApiError, ApiJson, ApiPath};
use crate::scans::{persisted_scan, same_root};

/// Keep-last-N retention for plans, like scans.
pub const KEEP_LAST_PLANS: usize = 25;

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PlanRequest {
    /// `safe` (default) or `caution`. `review` is refused: those groups are
    /// never plan items.
    #[serde(default)]
    max_tier: Option<RiskTier>,
    /// Skip items whose included paths' private bytes are below this (default 0).
    #[serde(default)]
    min_bytes: Option<u64>,
}

pub(crate) async fn create_plan(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
    ApiJson(req): ApiJson<PlanRequest>,
) -> Result<Response, ApiError> {
    let max_tier = match req.max_tier.unwrap_or(RiskTier::Safe) {
        RiskTier::Review => {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                "maxTier must be \"safe\" or \"caution\"; review groups are never plan items".into(),
            ));
        }
        t => t,
    };
    let scan = persisted_scan(&state, id)?;
    if scan.status != ScanStatus::Complete {
        return Err(ApiError(
            StatusCode::CONFLICT,
            format!(
                "scan {id} is {} — only a complete scan has hotspots to plan from",
                scan.status.as_str()
            ),
        ));
    }
    // Bind before use: the store guard drops at the end of the statement.
    let summary = state.scan_store().hotspots(id)?;
    let summary = summary.unwrap_or_else(phantom_core::HotspotsSummary::empty);
    let built = plan::build_plan(
        &scan,
        &summary,
        plan::PlanOptions {
            max_tier,
            min_bytes: req.min_bytes.unwrap_or(0),
        },
        // A committed fixture is git data, not a cache (phantom-2lz): the
        // rules are read from disk, no subprocess.
        &|p| phantom_core::gitignore::held_back_by_git(std::path::Path::new(p)),
        &|p| {
            let entry = state.scan_store().entry(id, p)?;
            entry.private_size.ok_or_else(|| {
                CoreError::InvalidInput(format!(
                    "path {p:?} has no private-byte measurement in scan {id}; run a fresh scan"
                ))
            })
        },
    )?;
    state.scan_store().insert_plan(&built)?;
    // Retention rides the insert; a prune failure must not fail the plan.
    let pruned = state.scan_store().prune_plans_to_last(KEEP_LAST_PLANS);
    match pruned {
        Ok(0) => {}
        Ok(n) => tracing::info!(plan = %built.plan_id, pruned = n, "retention: pruned oldest plans"),
        Err(e) => tracing::warn!(plan = %built.plan_id, error = %e, "plan prune failed; plan is persisted"),
    }
    Ok((StatusCode::CREATED, Json(built)).into_response())
}

pub(crate) async fn get_plan(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
) -> Result<Response, ApiError> {
    let plan = state.scan_store().get_plan(id)?;
    Ok(Json(plan).into_response())
}

/// The plan as a shell script — `text/plain`, not JSON, because the caller
/// writes it to a file and runs it (or pastes it). Dry-run by default; see
/// `phantom_core::plan::script`.
pub(crate) async fn get_plan_script(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
) -> Result<Response, ApiError> {
    let plan = state.scan_store().get_plan(id)?;
    let text = plan::script(&plan);
    Ok((
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        text,
    )
        .into_response())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct VerifyRequest {
    /// The rescan to compare against: complete, same root, started after
    /// the plan was built.
    after_scan_id: Uuid,
}

pub(crate) async fn verify_plan(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
    ApiJson(req): ApiJson<VerifyRequest>,
) -> Result<Response, ApiError> {
    let plan = state.scan_store().get_plan(id)?;
    let after = persisted_scan(&state, req.after_scan_id)?;
    if after.status != ScanStatus::Complete {
        return Err(ApiError(
            StatusCode::CONFLICT,
            format!(
                "scan {} is {} — only a complete rescan can verify a plan",
                after.id,
                after.status.as_str()
            ),
        ));
    }
    if !same_root(&plan.root_path, &after.root_path) {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!(
                "afterScanId covers {:?} but the plan was built for {:?}; rescan the plan's root",
                after.root_path, plan.root_path
            ),
        ));
    }
    if after.started_at < plan.created_at {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!(
                "afterScanId {} started before the plan was built; run the plan, then rescan",
                after.id
            ),
        ));
    }
    let before = match state.scan_store().get_scan(plan.scan_id) {
        Ok(s) => s,
        Err(CoreError::NotFound(_)) => {
            return Err(ApiError(
                StatusCode::CONFLICT,
                format!(
                    "the plan's scan {} is no longer stored (only the newest 25 scans of each root are kept); \
                     build a new plan from a fresh scan",
                    plan.scan_id
                ),
            ));
        }
        Err(e) => return Err(e.into()),
    };
    let before_dirs = state.scan_store().dir_sizes(before.id)?;
    let after_dirs = state.scan_store().dir_sizes(after.id)?;
    let root_delta = i128::from(before.total_disk_size) - i128::from(after.total_disk_size);
    let verification = plan::verify_plan(&plan, &before, &before_dirs, &after, &after_dirs);
    // Refuse rather than report a confident wrong number (phantom-9wc). All
    // four clauses must hold, and each rules out a legitimate reading:
    //   1. a plan-Trash folder that is NOT this plan's sits inside the root,
    //      while this plan's does not (so the headline is the root-delta
    //      fallback, which measures nothing once bytes move within the root);
    //   2. this plan's paths really did shrink — otherwise the honest answer
    //      is "nothing came back", and a leftover folder from an older plan
    //      the user has not emptied yet must not break that;
    //   3. the root did NOT shrink by what those paths shrank, i.e. the bytes
    //      are still inside it. Paths deleted outright, or moved off the root,
    //      keep the root delta meaningful and verify normally.
    // This is the shape that read −9.3 GB after 57.9 GB was actually freed.
    let per_item: i128 = verification
        .items
        .iter()
        .filter_map(|i| i.actual_freed_bytes)
        .map(i128::from)
        .sum();
    let root_missed_it =
        per_item > 0 && (per_item - root_delta) as f64 > per_item as f64 * plan::TOLERANCE;
    if root_missed_it {
        if let Some(other) = plan::foreign_plan_trash(plan.plan_id, &after_dirs) {
            return Err(ApiError(
                StatusCode::CONFLICT,
                format!(
                    "plan {}'s Trash folder is not in scan {}, but {:?} is, and this plan's paths \
                     shrank by {} while {:?} shrank by only {}. The paths were moved by a \
                     DIFFERENT plan's script, so what came back cannot be attributed to this \
                     plan. Verify the plan whose id that folder names, or run this plan's own \
                     script and rescan.",
                    plan.plan_id,
                    after.id,
                    other,
                    per_item,
                    plan.root_path,
                    root_delta
                ),
            ));
        }
    }
    Ok(Json(verification).into_response())
}
