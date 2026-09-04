// The async scan lifecycle. POST /scans answers 202 with the scan's id
// before the walk starts; the walk runs on a blocking thread, publishing
// live progress through the ScanRegistry; on a terminal state the result is
// handed off to SQLite per ADR-0005 (directories + files ≥ 1 MiB, plus
// per-type totals computed from the full walk). Cancellation is cooperative
// and DISCARDS partial results — only a metadata row records the attempt.
//
// Wire shape: `Scan` fields plus a `progress` object that is live counters
// while running and null once terminal (nullable-present-as-null).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use axum::{
    Json,
    extract::State,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use chrono::Utc;
use phantom_core::{
    CoreError, FileQuery, FileSort, HotspotsSummary, ProgressSnapshot, Scan, ScanError,
    ScanOutcome, ScanProgress, ScanStatus, classify, diff, format, persist, scanner, treemap,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::AppState;
use crate::routes::{ApiError, ApiJson, ApiPath, ApiQuery, NEXT_CURSOR_HEADER, parse_limit_cursor};

/// Keep-last-N retention: after every successful terminal persist, scans
/// beyond the newest N are pruned (entries and type totals cascade). 25 is a
/// PRODUCT DECISION (Ted, 2026-09-01), pending a settings surface — old
/// scans are cheaper to rescan than to keep forever (docs/data-safety.md).
pub const KEEP_LAST_SCANS: usize = 25;

/// Server-side treemap layout defaults, used when the client does not send
/// its actual view size (CLI/MCP convenience; the app always sends one).
const DEFAULT_TREEMAP_WIDTH: f64 = 800.0;
const DEFAULT_TREEMAP_HEIGHT: f64 = 600.0;
const DEFAULT_TREEMAP_DEPTH: usize = 4;

/// The wire view of a scan: the `Scan` fields, plus live `progress` while it
/// runs (null once terminal).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ScanView {
    #[serde(flatten)]
    scan: Scan,
    progress: Option<ProgressSnapshot>,
}

// --- Lifecycle --------------------------------------------------------------

pub(crate) async fn create_scan(
    State(state): State<AppState>,
    ApiJson(req): ApiJson<phantom_core::ScanRequest>,
) -> Result<Response, ApiError> {
    let root = req.root_path.trim();
    if root.is_empty() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "rootPath must not be empty".into(),
        ));
    }
    let root_path = PathBuf::from(root);
    // Pre-flight the obvious failure so a typo'd path is a 400 now, not a
    // `failed` scan to discover by polling. The walker re-checks (TOCTOU).
    if !root_path.is_dir() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!("not a directory: {root}"),
        ));
    }

    let scan = Scan::new(root);
    let (progress, cancel) = state.registry.register(scan.clone());
    let view = ScanView {
        scan: scan.clone(),
        progress: Some(progress.snapshot()),
    };
    spawn_scan(state, scan.id, root_path, progress, cancel);
    Ok((StatusCode::ACCEPTED, Json(view)).into_response())
}

fn spawn_scan(
    state: AppState,
    id: Uuid,
    root: PathBuf,
    progress: Arc<ScanProgress>,
    cancel: Arc<AtomicBool>,
) {
    tokio::spawn(async move {
        // Test-only pacing point (see AppState::scan_hold): parks the worker
        // BEFORE the walk so integration tests can order cancel/list/delete
        // against a scan that is deterministically still running.
        if let Some(hold) = state.scan_hold.clone() {
            hold.notified().await;
        }
        let worker_state = state.clone();
        let joined = tokio::task::spawn_blocking(move || {
            let result = scanner::scan_directory(&root, &progress, &cancel);
            finish_scan(&worker_state, id, result);
        })
        .await;
        if joined.is_err() {
            // The walk or the handoff panicked; keep the scan visible.
            tracing::error!(%id, "scan worker panicked");
            state.registry.mark_failed(id);
        }
    });
}

/// Terminal handoff: fold the walk result into a terminal `Scan`, persist it
/// (with the ADR-0005 entry filter and full-walk type totals), and only then
/// release the registry entry.
///
/// The ordering is the contract: a scan must never be INVISIBLE (in neither
/// place). Persist-then-remove means the worst case is a brief window where
/// both sides know the scan — readers prefer the DB row and listings dedupe
/// by id. Remove-then-persist would open a window (and, on insert failure, a
/// permanent state) where the scan is in neither.
/// `handoff_failure_keeps_the_scan_visible` in tests/test_scans.rs fails if
/// this order is reverted.
///
/// Public so integration tests can drive the handoff deterministically; it
/// is not part of the HTTP surface.
pub fn finish_scan(state: &AppState, id: Uuid, result: Result<ScanOutcome, ScanError>) {
    let Some((mut scan, _)) = state.registry.snapshot(id) else {
        return; // nothing to hand off (never registered, or already done)
    };
    scan.finished_at = Some(Utc::now());

    let (entries, type_totals, hotspots) = match result {
        Ok(outcome) if !state.registry.cancel_requested(id) => {
            scan.status = ScanStatus::Complete;
            scan.total_disk_size = outcome.total_disk_size;
            scan.total_logical_size = outcome.total_logical_size;
            scan.file_count = outcome.file_count;
            scan.dir_count = outcome.dir_count;
            scan.error_count = outcome.error_count;
            scan.unreadable_paths = Some(outcome.unreadable.clone());
            // ADR-0005 ordering: type totals AND the Phase-5 classifier see
            // the FULL walk (a hotspot made of small files must still total
            // correctly); the entry filter runs after. Categories are
            // stamped onto the full walk first so the persisted subset —
            // dir rows included — carries them. The full walk's rows carry
            // TRUE per-link sizes — classify's listedDiskSize needs them;
            // hardlink dedup happens inside each aggregator (phantom-5ws).
            let totals = format::totals_by_file_type(&outcome.entries);
            let classification = classify::classify(&outcome.entries, Utc::now());
            let mut walk = outcome.entries;
            for (entry, category) in walk.iter_mut().zip(&classification.categories) {
                entry.category = category.map(|c| c.as_str().to_string());
            }
            let entries = persist::persistable_entries(&walk);
            (entries, totals, Some(classification.summary))
        }
        // Cancelled — the walker bailed, or the flag was set in the gap
        // after the walk finished. Partial results are discarded either way;
        // the metadata row records that the scan happened.
        Ok(_) | Err(ScanError::Cancelled) => {
            scan.status = ScanStatus::Cancelled;
            (Vec::new(), Vec::new(), None)
        }
        Err(e) => {
            tracing::error!(%id, error = %e, "scan failed");
            scan.status = ScanStatus::Failed;
            (Vec::new(), Vec::new(), None)
        }
    };

    // Bind BEFORE matching: a guard temporary in the match scrutinee lives
    // for the whole match, and the Ok arm below locks the store again for
    // the prune — with the non-reentrant std Mutex that is a self-deadlock
    // (the exact Phase-2 get_tree bug from the Gotchas list; it bit this
    // very block during review-fix development, 2026-09-01).
    let inserted = state
        .scan_store()
        .insert_scan(&scan, &entries, &type_totals, hotspots.as_ref());
    match inserted {
        Ok(()) => {
            state.registry.remove(id);
            // Retention rides the same completion path as the insert. A
            // prune failure must NOT fail the scan — it is already safely
            // persisted; the next completion retries the prune anyway.
            let pruned = state.scan_store().prune_to_last(KEEP_LAST_SCANS);
            match pruned {
                Ok(0) => {}
                Ok(n) => tracing::info!(%id, pruned = n, "retention: pruned oldest scans"),
                Err(e) => {
                    tracing::warn!(%id, error = %e, "retention prune failed; scan is persisted")
                }
            }
        }
        Err(e) => {
            tracing::error!(
                %id, error = %e,
                "cannot persist terminal scan; keeping it visible as failed"
            );
            state.registry.mark_failed(id);
        }
    }
}

// --- Read side: the registry↔SQLite merge ----------------------------------

/// The single merge point for scan metadata: the DB row is the terminal
/// truth and wins; the registry serves scans the DB does not know yet.
fn scan_view(state: &AppState, id: Uuid) -> Result<ScanView, ApiError> {
    match state.scan_store().get_scan(id) {
        Ok(scan) => Ok(ScanView { scan, progress: None }),
        Err(CoreError::NotFound(_)) => match state.registry.snapshot(id) {
            Some((scan, progress)) => Ok(ScanView {
                scan,
                progress: Some(progress),
            }),
            None => Err(not_found(id)),
        },
        Err(e) => Err(e.into()),
    }
}

/// A scan whose RESULTS are readable, i.e. persisted. A known in-flight scan
/// is a 409 pointing the caller back at the progress surface, not a 404.
fn persisted_scan(state: &AppState, id: Uuid) -> Result<Scan, ApiError> {
    match state.scan_store().get_scan(id) {
        Ok(scan) => Ok(scan),
        Err(CoreError::NotFound(_)) => match state.registry.snapshot(id) {
            Some(_) => Err(ApiError(
                StatusCode::CONFLICT,
                format!("scan {id} is still running; results are available once it finishes"),
            )),
            None => Err(not_found(id)),
        },
        Err(e) => Err(e.into()),
    }
}

fn not_found(id: Uuid) -> ApiError {
    ApiError(StatusCode::NOT_FOUND, format!("not found: scan {id}"))
}

pub(crate) async fn get_scan(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
) -> Result<Response, ApiError> {
    Ok(Json(scan_view(&state, id)?).into_response())
}

pub(crate) async fn list_scans(State(state): State<AppState>) -> Result<Response, ApiError> {
    let persisted = state.scan_store().list_scans()?;
    let known: std::collections::HashSet<Uuid> = persisted.iter().map(|s| s.id).collect();
    let mut views: Vec<ScanView> = persisted
        .into_iter()
        .map(|scan| ScanView { scan, progress: None })
        .collect();
    // In-flight scans, skipping any already persisted (the handoff window
    // has a scan briefly in both places; it must never be listed twice).
    for (scan, progress) in state.registry.list() {
        if !known.contains(&scan.id) {
            views.push(ScanView {
                scan,
                progress: Some(progress),
            });
        }
    }
    // Newest first, id tiebreak — the store's ordering, kept after the merge.
    views.sort_by(|a, b| {
        b.scan
            .started_at
            .cmp(&a.scan.started_at)
            .then_with(|| a.scan.id.cmp(&b.scan.id))
    });
    Ok(Json(views).into_response())
}

pub(crate) async fn cancel_scan(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
) -> Result<Response, ApiError> {
    if state.registry.request_cancel(id) {
        // Accepted: the walker stops at its next entry. Poll the scan for
        // the terminal status. (A cancel racing scan completion can lose;
        // the poll reveals which side won.)
        let view = scan_view(&state, id)?;
        return Ok((StatusCode::ACCEPTED, Json(view)).into_response());
    }
    match state.scan_store().get_scan(id) {
        Ok(scan) => Err(ApiError(
            StatusCode::CONFLICT,
            format!("scan {id} is already {}; cannot cancel", scan.status.as_str()),
        )),
        Err(CoreError::NotFound(_)) => Err(not_found(id)),
        Err(e) => Err(e.into()),
    }
}

pub(crate) async fn delete_scan(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
) -> Result<Response, ApiError> {
    if let Some((scan, _)) = state.registry.snapshot(id)
        && state.scan_store().get_scan(id).is_err()
    {
        if scan.status == ScanStatus::Running {
            // Deleting mid-walk would race the completion handoff (the
            // worker would persist a fresh row right after the delete).
            return Err(ApiError(
                StatusCode::CONFLICT,
                format!("scan {id} is still running; cancel it before deleting"),
            ));
        }
        // Terminal in the registry only (its DB insert failed): deleting it
        // is just forgetting it.
        state.registry.remove(id);
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    state.scan_store().delete_scan(id)?; // entries + type totals cascade
    Ok(StatusCode::NO_CONTENT.into_response())
}

// --- Results: treemap / tree / files / entry / types -------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct TreemapParams {
    /// Kept as strings so a bad value earns a clean `400 {error}` from our
    /// own validation rather than a stock text/plain Query rejection.
    width: Option<String>,
    height: Option<String>,
    max_depth: Option<String>,
    root: Option<String>,
}

fn parse_dimension(name: &str, value: Option<&str>, default: f64) -> Result<f64, ApiError> {
    match value.map(str::trim) {
        None | Some("") => Ok(default),
        Some(s) => {
            let v: f64 = s.parse().map_err(|_| {
                ApiError(
                    StatusCode::BAD_REQUEST,
                    format!("{name} must be a positive number (got {s:?})"),
                )
            })?;
            if !v.is_finite() || v <= 0.0 {
                return Err(ApiError(
                    StatusCode::BAD_REQUEST,
                    format!("{name} must be a positive number (got {s:?})"),
                ));
            }
            Ok(v)
        }
    }
}

pub(crate) async fn get_treemap(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
    ApiQuery(q): ApiQuery<TreemapParams>,
) -> Result<Response, ApiError> {
    let width = parse_dimension("width", q.width.as_deref(), DEFAULT_TREEMAP_WIDTH)?;
    let height = parse_dimension("height", q.height.as_deref(), DEFAULT_TREEMAP_HEIGHT)?;
    let max_depth = match q.max_depth.as_deref().map(str::trim) {
        None | Some("") => DEFAULT_TREEMAP_DEPTH,
        Some(s) => s.parse().map_err(|_| {
            ApiError(
                StatusCode::BAD_REQUEST,
                format!("maxDepth must be a non-negative integer (got {s:?})"),
            )
        })?,
    };

    let scan = persisted_scan(&state, id)?;
    let root = q
        .root
        .clone()
        .filter(|r| !r.is_empty())
        .unwrap_or_else(|| scan.root_path.clone());
    let entries = state.scan_store().entries(id)?;

    // `root=` re-roots AND re-lays-out server-side: the layout below is
    // computed over the requested subtree at the requested view size.
    if let Some(root_entry) = entries.iter().find(|e| e.path == root) {
        if !root_entry.is_dir {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                format!("treemap root must be a directory: {root}"),
            ));
        }
    } else if q.root.is_some() {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            format!("not found: path {root:?} in scan {id}"),
        ));
    }
    // (No entries and no explicit root: a cancelled/failed scan persists no
    // results — serve the honest empty layout.)

    let layout = treemap::layout(&entries, &root, (0.0, 0.0, width, height), max_depth);
    Ok(Json(layout).into_response())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct TreeParams {
    path: Option<String>,
}

/// Direct children of a directory within the scan (default: the scan root).
pub(crate) async fn get_tree(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
    ApiQuery(q): ApiQuery<TreeParams>,
) -> Result<Response, ApiError> {
    let scan = persisted_scan(&state, id)?;
    let explicit = q.path.clone().filter(|p| !p.is_empty());
    let target = explicit.clone().unwrap_or_else(|| scan.root_path.clone());

    // Bind before matching: a guard temporary in the match scrutinee lives
    // for the whole match, and the Ok arm locks the store again — with the
    // non-reentrant std Mutex that is a self-deadlock.
    let target_entry = state.scan_store().entry(id, &target);
    match target_entry {
        Ok(e) if !e.is_dir => Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!("not a directory: {target}"),
        )),
        Ok(_) => {
            let children = state.scan_store().children_of(id, Some(&target))?;
            Ok(Json(children).into_response())
        }
        // Default root missing == a cancelled/failed scan persisted no
        // results; the honest answer is an empty listing, not a 404.
        Err(CoreError::NotFound(_)) if explicit.is_none() => {
            Ok(Json(Vec::<phantom_core::ScanEntry>::new()).into_response())
        }
        Err(e) => Err(e.into()),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct FilesParams {
    file_type: Option<String>,
    search: Option<String>,
    sort: Option<String>,
    limit: Option<String>,
    cursor: Option<String>,
}

pub(crate) async fn list_files(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
    ApiQuery(q): ApiQuery<FilesParams>,
) -> Result<Response, ApiError> {
    let (limit, offset) = parse_limit_cursor(q.limit.as_deref(), q.cursor.as_deref())?;
    let sort: FileSort = match q.sort.as_deref().map(str::trim) {
        None | Some("") => FileSort::default(),
        Some(s) => s.parse()?, // InvalidInput → 400 via the CoreError mapping
    };
    persisted_scan(&state, id)?;

    let query = FileQuery {
        file_type: q.file_type.as_deref().map(str::trim).filter(|s| !s.is_empty()),
        search: q.search.as_deref().filter(|s| !s.is_empty()),
        sort,
    };
    let page = state.scan_store().files_page(id, &query, limit, offset)?;

    let mut response = Json(page.files).into_response();
    if let Some(next) = page.next_offset {
        // `next` is ASCII digits, so this never fails.
        if let Ok(value) = header::HeaderValue::from_str(&next.to_string()) {
            response
                .headers_mut()
                .insert(header::HeaderName::from_static(NEXT_CURSOR_HEADER), value);
        }
    }
    Ok(response)
}

/// Per-type disk totals, computed from the FULL walk at persistence time
/// (ADR-0005) — the one result surface that still sees the filtered small
/// files. Largest disk footprint first, ties broken by type name.
pub(crate) async fn get_types(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
) -> Result<Response, ApiError> {
    persisted_scan(&state, id)?;
    let totals = state.scan_store().file_type_totals(id)?;
    Ok(Json(totals).into_response())
}

/// The Phase-5 reclaimability summary, persisted with the scan by the
/// completion post-pass. Same 409-while-running semantics as /types. A scan
/// with no stored summary (cancelled/failed — partial results are discarded)
/// serves the honest empty summary, matching the tree/treemap posture.
pub(crate) async fn get_hotspots(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
) -> Result<Response, ApiError> {
    persisted_scan(&state, id)?;
    // Bind before use: the store guard from `hotspots()` drops at the end of
    // this statement, well before any further store access.
    let summary = state.scan_store().hotspots(id)?;
    let summary = summary.unwrap_or_else(|| HotspotsSummary {
        groups: Vec::new(),
        reclaim_estimate: 0,
        review_disk_size: 0,
        cloud_dataloaded_logical_size: 0,
        cloud_dataloaded_disk_size: 0,
    });
    Ok(Json(summary).into_response())
}

/// Diff two completed scans of the same root (phantom-081): positional —
/// the second id is "after", deltas read B − A. Both sides must be
/// COMPLETE (a cancelled/failed scan persists no entries, so a diff
/// against one would report the whole tree as freed — a lie): non-complete
/// terminal scans are a 409 conflict like results-while-running; a root
/// mismatch is a 400 (the comparison is meaningless, not merely early).
/// Two scan roots name the same directory? Raw string equality is too
/// strict on macOS: `/tmp` and `/private/tmp` are one directory (a symlink),
/// and a trailing slash is cosmetic. Canonicalize both and compare the
/// resolved paths; fall back to a trailing-slash-insensitive string match
/// when a root no longer exists on disk (canonicalize would fail, but the
/// stored strings can still match). (review: macOS path aliasing.)
fn same_root(a: &str, b: &str) -> bool {
    let trim = |s: &str| s.trim_end_matches('/').to_string();
    if trim(a) == trim(b) {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => false,
    }
}

pub(crate) async fn get_diff(
    State(state): State<AppState>,
    ApiPath((id, other)): ApiPath<(Uuid, Uuid)>,
) -> Result<Response, ApiError> {
    let a = persisted_scan(&state, id)?;
    let b = persisted_scan(&state, other)?;
    for s in [&a, &b] {
        if s.status != ScanStatus::Complete {
            return Err(ApiError(
                StatusCode::CONFLICT,
                format!(
                    "scan {} is {} — only complete scans have results to diff",
                    s.id,
                    s.status.as_str()
                ),
            ));
        }
    }
    if !same_root(&a.root_path, &b.root_path) {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!(
                "scans cover different roots ({:?} vs {:?}); a diff needs the same root",
                a.root_path, b.root_path
            ),
        ));
    }
    let a_dirs = state.scan_store().dir_sizes(id)?;
    let b_dirs = state.scan_store().dir_sizes(other)?;
    Ok(Json(diff::diff(&a, &a_dirs, &b, &b_dirs)).into_response())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct EntryParams {
    path: Option<String>,
}

pub(crate) async fn get_entry(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
    ApiQuery(q): ApiQuery<EntryParams>,
) -> Result<Response, ApiError> {
    let path = q.path.filter(|p| !p.is_empty()).ok_or_else(|| {
        ApiError(
            StatusCode::BAD_REQUEST,
            "path query parameter is required".into(),
        )
    })?;
    persisted_scan(&state, id)?;
    let entry = state.scan_store().entry(id, &path)?;
    Ok(Json(entry).into_response())
}

#[cfg(test)]
mod same_root_tests {
    use super::same_root;

    #[test]
    fn trailing_slash_is_cosmetic() {
        assert!(same_root("/Users/x/Code", "/Users/x/Code/"));
        assert!(same_root("/a/", "/a"));
    }

    #[test]
    fn distinct_roots_do_not_match() {
        // Neither exists on disk, so canonicalize fails and the trimmed
        // string compare (correctly) rejects them.
        assert!(!same_root("/no/such/alpha", "/no/such/beta"));
    }

    #[test]
    fn macos_tmp_alias_matches_via_canonicalize() {
        // On macOS /tmp is a symlink to /private/tmp; both resolve equal.
        // On Linux this canonicalizes to itself on both sides (still equal).
        // Guard on existence so the test is portable.
        if std::path::Path::new("/tmp").exists() {
            assert!(same_root("/tmp", "/private/tmp") || same_root("/tmp", "/tmp"));
        }
    }
}
