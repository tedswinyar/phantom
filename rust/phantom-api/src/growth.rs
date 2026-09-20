// Growth over time (v1.1 Phase 4, phantom-mkn.11).
//
//   GET /scans/series?root=&groupBy=total|category|topLevelDir|extension
//       -> Growth
//
// A composition over persisted rows: the completed scans of `root`
// (matched like the diff endpoint — trailing slash cosmetic, canonical
// paths equal), each with the breakdown the scan already stored, plus one
// statfs for the volume's headroom. Nothing is re-walked and nothing is
// written.

use axum::{
    Json,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use phantom_core::{ScanStatus, growth, volume};
use phantom_core::growth::{GroupBy, Sample};
use serde::Deserialize;

use crate::AppState;
use crate::routes::{ApiError, ApiQuery};
use crate::scans::same_root;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SeriesParams {
    /// The scanned root (required — the CLI and MCP default it to the
    /// latest completed scan's root, the API does not guess).
    root: Option<String>,
    /// Wire spelling only: total (default) | category | topLevelDir | extension.
    group_by: Option<String>,
}

pub(crate) async fn get_series(
    State(state): State<AppState>,
    ApiQuery(q): ApiQuery<SeriesParams>,
) -> Result<Response, ApiError> {
    let root = q
        .root
        .map(|r| r.trim().to_string())
        .filter(|r| !r.is_empty())
        .ok_or_else(|| ApiError(StatusCode::BAD_REQUEST, "root query parameter is required".into()))?;
    let group_by: GroupBy = match q.group_by.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        None => GroupBy::Total,
        Some(s) => s.parse().map_err(|e: String| ApiError(StatusCode::BAD_REQUEST, e))?,
    };

    let store = state.scan_store();
    let scans: Vec<_> = store
        .list_scans()?
        .into_iter()
        .filter(|s| s.status == ScanStatus::Complete && same_root(&s.root_path, &root))
        .collect();
    if scans.is_empty() {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            format!("not found: no completed scans of {root:?} (scan it first; list_scans shows what exists)"),
        ));
    }

    let mut samples = Vec::with_capacity(scans.len());
    for scan in scans {
        let breakdown = match group_by {
            GroupBy::Total => Some(vec![("total".to_string(), scan.total_disk_size)]),
            GroupBy::Category => store.hotspots(scan.id)?.map(|summary| {
                growth::breakdown(summary.groups.iter().map(|g| (g.category.as_str().to_string(), g.disk_size)))
            }),
            GroupBy::TopLevelDir => Some(growth::breakdown(
                store
                    .children_of(scan.id, Some(&scan.root_path))?
                    .into_iter()
                    .filter(|e| e.is_dir)
                    .map(|e| (e.name, e.disk_size)),
            )),
            GroupBy::Extension => Some(growth::breakdown(
                store
                    .file_type_totals(scan.id)?
                    .into_iter()
                    .map(|t| (growth::extension_key(t.file_type.as_deref()), t.disk_size)),
            )),
        };
        samples.push(Sample { scan, breakdown });
    }
    drop(store);
    let available = volume::available_bytes(&root);
    Ok(Json(growth::build(group_by, samples, available, chrono::Utc::now())).into_response())
}
