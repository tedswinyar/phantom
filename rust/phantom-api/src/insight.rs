// Insight routes (v1.1 Phase 3, phantom-mkn.9): explain a path, list stale
// projects, report the volume. Compositions over persisted rows and the
// stored summary (phantom_core::insight) plus one statfs (phantom_core::
// volume); the snapshot listing is the only subprocess and runs only when
// asked (`snapshots=true`).
//
//   GET /scans/{id}/explain?path=       -> PathExplanation
//   GET /scans/{id}/stale?olderThan=    -> StaleProjects
//   GET /volume?path=&snapshots=&scanId= -> VolumeStatus (+ hidden-space
//                                          split relative to a completed scan)

use axum::{
    Json,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use phantom_core::{HotspotsSummary, ScanStatus, classify, insight, volume};
use serde::Deserialize;
use uuid::Uuid;

use crate::AppState;
use crate::routes::{ApiError, ApiPath, ApiQuery};
use crate::scans::persisted_scan;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ExplainParams {
    path: Option<String>,
}

pub(crate) async fn explain_path(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
    ApiQuery(q): ApiQuery<ExplainParams>,
) -> Result<Response, ApiError> {
    let path = q.path.filter(|p| !p.is_empty()).ok_or_else(|| {
        ApiError(
            StatusCode::BAD_REQUEST,
            "path query parameter is required".into(),
        )
    })?;
    let scan = persisted_scan(&state, id)?;
    let entry = state.scan_store().entry(id, &path)?;
    let summary = state.scan_store().hotspots(id)?;
    let summary = summary.unwrap_or_else(HotspotsSummary::empty);
    let unreadable = scan.unreadable_paths.unwrap_or_default();
    Ok(Json(insight::explain(&entry, &summary, &unreadable)).into_response())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct StaleParams {
    /// Same grammar as ScanRequest.olderThan (90d, 12w, 3M, 1y, bare days).
    older_than: Option<String>,
}

pub(crate) async fn stale_projects(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
    ApiQuery(q): ApiQuery<StaleParams>,
) -> Result<Response, ApiError> {
    let threshold_days = match q.older_than.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => classify::parse_older_than(s)
            .map_err(|e| ApiError(StatusCode::BAD_REQUEST, e.to_string()))?,
        None => classify::DORMANT_AFTER_DAYS,
    };
    let scan = persisted_scan(&state, id)?;
    let summary = state.scan_store().hotspots(id)?;
    let summary = summary.unwrap_or_else(HotspotsSummary::empty);
    Ok(Json(insight::stale_projects(&scan, &summary, threshold_days)).into_response())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct VolumeParams {
    /// Any path on the volume of interest (default: the data volume).
    path: Option<String>,
    /// `true` runs `tmutil listlocalsnapshots` (fixed path, bounded).
    snapshots: Option<String>,
    /// A COMPLETED scan on this volume: fills `hidden.scannedBytes` /
    /// `unscannedBytes` / `unreadableCount` (phantom-mkn.12).
    scan_id: Option<String>,
}

pub(crate) async fn volume_status(
    State(state): State<AppState>,
    ApiQuery(q): ApiQuery<VolumeParams>,
) -> Result<Response, ApiError> {
    let path = q
        .path
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| volume::default_path().to_string());
    let snapshots = match q.snapshots.as_deref().map(str::trim) {
        None | Some("") | Some("false") | Some("0") => false,
        Some("true") | Some("1") => true,
        Some(other) => {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                format!("snapshots must be true or false (got {other:?})"),
            ));
        }
    };
    let scan = match q.scan_id.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        None => None,
        Some(raw) => {
            let id = Uuid::parse_str(raw)
                .map_err(|_| ApiError(StatusCode::BAD_REQUEST, format!("scanId must be a UUID (got {raw:?})")))?;
            let scan = persisted_scan(&state, id)?;
            if scan.status != ScanStatus::Complete {
                return Err(ApiError(
                    StatusCode::CONFLICT,
                    format!(
                        "scan {id} is {}; the hidden-space split needs a completed scan",
                        scan.status.as_str()
                    ),
                ));
            }
            Some(scan)
        }
    };
    // statfs/getattrlist/CoreFoundation are instant; tmutil is a bounded
    // child — off the async threads either way.
    let status = tokio::task::spawn_blocking(move || volume::volume_status(&path, snapshots, scan.as_ref()))
        .await
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("volume task failed: {e}")))??;
    Ok(Json(status).into_response())
}
