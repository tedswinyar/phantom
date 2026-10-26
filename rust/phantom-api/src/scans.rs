// The async scan lifecycle. POST /scans answers 202 with the scan's id
// before the walk starts; the walk runs on a blocking thread, publishing
// live progress through the ScanRegistry; on a terminal state the result is
// handed off to SQLite per ADR-0005 (files ≥ 1 MiB and — since 1.1.1 —
// directories whose subtree is ≥ 1 MiB or that are hotspot topPaths, mount
// points or the root; plus per-type totals computed from the full walk).
// Cancellation is cooperative and DISCARDS partial results — only a
// metadata row records the attempt.
//
// Wire shape: `Scan` fields plus a `progress` object that is live counters
// while running and null once terminal (nullable-present-as-null).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use chrono::Utc;
use phantom_core::{
    CoreError, FileQuery, FileSort, HotspotsSummary, ProgressSnapshot, Scan, ScanError,
    ScanOptions, ScanOutcome, ScanProgress, ScanStatus, classify, diff, format, persist, probe, scanner,
    treemap,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::AppState;
use crate::routes::{ApiError, ApiJson, ApiPath, ApiQuery, NEXT_CURSOR_HEADER, parse_limit_cursor};

/// Retention after every successful terminal persist: the newest
/// [`crate::config::Retention::DEFAULT_PER_ROOT`] scans of each root, then
/// the newest [`crate::config::Retention::DEFAULT_TOTAL`] overall, then a
/// byte budget of [`crate::config::Retention::DEFAULT_BUDGET_BYTES`] that
/// evicts the globally oldest scan of any root still above
/// [`phantom_core::BUDGET_FLOOR_PER_ROOT`] — a floor that lapses once a
/// root's newest scan is older than
/// [`phantom_core::BUDGET_FLOOR_MAX_AGE_DAYS`] (phantom-ccq); entries and
/// type totals cascade. Old scans are cheaper to rescan than to keep forever
/// (docs/data-safety.md); the per-root split keeps one root's history from
/// evicting another's (phantom-9tt); bytes are the binding unit because a
/// slot costs the same for a 14 MB probe and a 280 GB home (phantom-cnr.3).
/// Operators override with `PHANTOM_KEEP_SCANS_PER_ROOT` /
/// `PHANTOM_KEEP_SCANS` / `PHANTOM_DB_BUDGET_BYTES`; `AppState.retention`
/// is the resolved value. Kept as a name for the tests and docs that cite it.
pub const KEEP_LAST_SCANS: usize = crate::config::Retention::DEFAULT_PER_ROOT;

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
    headers: HeaderMap,
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

    let options = ScanOptions {
        cross_volumes: req.cross_volumes,
    };
    // Classifier knobs are validated NOW: a bad threshold is a 400 here,
    // not a scan that silently used the default.
    let dormant_after_days = match req.older_than.as_deref() {
        Some(s) => Some(
            classify::parse_older_than(s)
                .map_err(|e| ApiError(StatusCode::BAD_REQUEST, e.to_string()))?,
        ),
        None => None,
    };
    let completion = CompletionOptions {
        dormant_after_days,
        verify_locks: req.verify_locks,
        tool_estimates: req.tool_estimates,
    };
    let scan = Scan::new(root);
    // Persist the running row FIRST (phantom-aoa): if this process dies
    // mid-walk the next one finds a row to mark interrupted rather than a
    // scan that never existed. Registry second, so a failed insert leaves
    // nothing behind.
    if let Err(e) = state.scan_store().insert_scan(&scan, &[], &[], None) {
        return Err(e.into());
    }
    // Provenance (phantom-cnr.7): one line per POST /scans, with the root,
    // every option and the caller's User-Agent, so a scan nobody remembers
    // asking for can be traced to the binary that asked (the "unrequested"
    // scans of 2026-09-16 were agent FDA probes and verify's implicit
    // rescans — established only by grepping Claude transcripts, because
    // this log held start/shutdown lines and not one scan).
    tracing::info!(
        id = %scan.id,
        root = %root,
        cross_volumes = req.cross_volumes,
        older_than = %req.older_than.as_deref().unwrap_or("-"),
        verify_locks = req.verify_locks,
        tool_estimates = req.tool_estimates,
        client = %client_of(&headers),
        "scan requested"
    );
    let (progress, cancel) = state.registry.register(scan.clone());
    let view = ScanView {
        scan: scan.clone(),
        progress: Some(progress.snapshot()),
    };
    spawn_scan(state, scan.id, root_path, options, completion, progress, cancel);
    Ok((StatusCode::ACCEPTED, Json(view)).into_response())
}

/// The request's `User-Agent` for the log — `phantom-cli/<v>`,
/// `phantom-mcp/<v> (<host>)`, `Phantom/<v>` — or `-` when the caller sent
/// none. A request header is not a contract surface; this is a log field.
fn client_of(headers: &HeaderMap) -> &str {
    headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("-")
}

/// What the completion post-pass does beyond the walk: the classifier's
/// threshold and the two opt-in subprocess features (docs/threat-model.md
/// §4). Everything defaults to off / the constant.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CompletionOptions {
    pub dormant_after_days: Option<i64>,
    pub verify_locks: bool,
    pub tool_estimates: bool,
}

fn spawn_scan(
    state: AppState,
    id: Uuid,
    root: PathBuf,
    options: ScanOptions,
    completion: CompletionOptions,
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
            let result = scanner::scan_directory_with(&root, options, &progress, &cancel);
            finish_scan_with(&worker_state, id, result, completion);
        })
        .await;
        if joined.is_err() {
            // The walk or the handoff panicked; keep the scan visible.
            tracing::error!(%id, "scan worker panicked");
            state.registry.mark_failed(id, "the scan worker panicked; see the server log");
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
    finish_scan_with(state, id, result, CompletionOptions::default())
}

/// [`finish_scan`] with the request's classifier knobs. The opt-in
/// subprocess probes run HERE, on the walker's blocking thread, after the
/// walk and before persistence — bounded per child and capped per rule
/// (`phantom_core::probe`); nothing they do can fail the scan.
pub fn finish_scan_with(
    state: &AppState,
    id: Uuid,
    result: Result<ScanOutcome, ScanError>,
    completion: CompletionOptions,
) {
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
            let verifier = completion
                .verify_locks
                .then(|| probe::lock_verifier(probe::DEFAULT_TIMEOUT));
            let classify_options = classify::ClassifyOptions {
                dormant_after_days: completion.dormant_after_days,
                verify: verifier.as_ref().map(|v| v as &dyn Fn(&_, &str) -> _),
            };
            let mut classification = classify::classify_with_options(
                &outcome.entries,
                &outcome.shares,
                Utc::now(),
                &classify_options,
            );
            if completion.tool_estimates {
                probe::attach_tool_estimates(&mut classification.summary);
            }
            let mut walk = outcome.entries;
            for (entry, category) in walk.iter_mut().zip(&classification.categories) {
                entry.category = category.map(|c| c.as_str().to_string());
            }
            // 1.1.1 (phantom-cnr.10): directory rows obey the 1 MiB rule
            // too, except the root, mount points and every hotspot topPath
            // — the summary is an INPUT to the filter so plan/verify never
            // meet a missing row.
            let entries =
                persist::persistable_entries(&walk, &outcome.shares, &classification.summary);
            // The scan's share totals ARE the root directory's rollup —
            // the first persisted row is the root (input order preserved).
            if let Some(root) = entries.iter().find(|e| e.path == scan.root_path) {
                scan.total_private_size = root.private_size;
                scan.total_shared_size = root.shared_size;
            }
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
            scan.failure_reason = Some(e.to_string());
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
            log_finished(state, &scan, entries.len());
            // Retention rides the same completion path as the insert. A
            // prune failure must NOT fail the scan — it is already safely
            // persisted; the next completion retries the prune anyway.
            let retention = state.retention;
            let pruned = state.scan_store().prune_retention_budget(
                retention.per_root,
                retention.total,
                retention.budget_bytes,
            );
            match pruned {
                Ok(report) => log_prune(id, retention, &report),
                Err(e) => {
                    tracing::warn!(%id, error = %e, "retention prune failed; scan is persisted")
                }
            }
            // The prune's pages (and the scan's own churn) go back to the
            // filesystem now, on a connection of its own — the store lock
            // above is already released (phantom-cnr.2).
            state.compact_after_write("scan complete");
        }
        Err(e) => {
            tracing::error!(
                %id, error = %e,
                "cannot persist terminal scan; keeping it visible as failed"
            );
            state
                .registry
                .mark_failed(id, &format!("could not persist the results: {e}"));
        }
    }
}

/// The terminal line for a scan (phantom-cnr.7): status, wall time, what
/// the walk saw, how many rows were persisted and what they are estimated
/// to cost in the database — the pair to `scan requested`, so the log alone
/// answers "which scans filled the file, and how much did each cost".
fn log_finished(state: &AppState, scan: &phantom_core::Scan, rows: usize) {
    let estimated_db_bytes = state
        .scan_store()
        .scan_footprints()
        .ok()
        .and_then(|fs| fs.into_iter().find(|f| f.id == scan.id))
        .map_or(0, |f| f.estimated_bytes);
    let duration_s = scan
        .finished_at
        .map_or(0.0, |f| (f - scan.started_at).num_milliseconds() as f64 / 1000.0);
    tracing::info!(
        id = %scan.id,
        root = %scan.root_path,
        status = %scan.status.as_str(),
        duration_s,
        files = scan.file_count,
        dirs = scan.dir_count,
        bytes = scan.total_disk_size,
        rows,
        estimated_db_bytes,
        "scan finished"
    );
}

/// The prune, in the API log, with bytes before and after — on EVERY
/// completion, pruned or not, so the history's size is always on record
/// (phantom-cnr.7) — and, when the floor kept the budget from holding, that
/// fact stated plainly rather than a silent shortfall (phantom-cnr.3). When
/// a root's floor was waived because nobody had scanned it in
/// [`phantom_core::BUDGET_FLOOR_MAX_AGE_DAYS`] days, one line names the
/// root and its age (phantom-ccq): a month-old history vanishing must be
/// traceable to the rule that took it.
fn log_prune(id: Uuid, retention: crate::config::Retention, report: &phantom_core::PruneReport) {
    for waived in &report.floor_waived {
        tracing::info!(
            %id,
            root = %waived.root,
            age_days = waived.age.num_days(),
            "retention: floor waived for {}: its newest completed scan is {} days old (older than {} days), so its scans were ordinary budget victims",
            waived.root,
            waived.age.num_days(),
            phantom_core::BUDGET_FLOOR_MAX_AGE_DAYS
        );
    }
    tracing::info!(
        %id,
        per_root = report.by_per_root,
        total = report.by_total,
        budget = report.by_budget,
        "retention: pruned {} scans; estimated {} -> {} of a {} budget",
        report.deleted(),
        gib(report.estimated_bytes_before),
        gib(report.estimated_bytes_after),
        gib(retention.budget_bytes)
    );
    if let Some(floor) = report.floor_over_budget {
        tracing::warn!(
            %id,
            "retention: budget {} exceeded by floor: {} roots x up to {} scans ({} scans) = {}; nothing below the floor is deleted while a root's newest scan is {} days old or younger — raise PHANTOM_DB_BUDGET_BYTES or delete scans by hand",
            gib(retention.budget_bytes),
            floor.roots,
            phantom_core::BUDGET_FLOOR_PER_ROOT,
            floor.scans,
            gib(floor.estimated_bytes),
            phantom_core::BUDGET_FLOOR_MAX_AGE_DAYS
        );
    }
}

/// Bytes as a one-decimal GiB figure for the log ("2.0 GiB").
fn gib(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
}

// --- Read side: the registry↔SQLite merge ----------------------------------

/// The single merge point for scan metadata. The DB row is the truth for
/// status; since v6 a RUNNING row is in the DB too, and its live progress
/// (and any status the registry already moved past the row — a worker that
/// panicked, an insert that failed) comes from the registry. The registry
/// alone serves the brief window before the running row is inserted.
fn scan_view(state: &AppState, id: Uuid) -> Result<ScanView, ApiError> {
    match state.scan_store().get_scan(id) {
        Ok(scan) => Ok(merge_live(&state.registry, scan)),
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

/// Attach the registry's live view to a persisted row: progress while the
/// walk runs, and the registry's terminal status when it is ahead of the
/// row (the row says running, the worker already failed).
fn merge_live(registry: &crate::registry::ScanRegistry, scan: Scan) -> ScanView {
    if scan.status != ScanStatus::Running {
        return ScanView { scan, progress: None };
    }
    match registry.snapshot(scan.id) {
        Some((live, progress)) if live.status == ScanStatus::Running => ScanView {
            scan,
            progress: Some(progress),
        },
        Some((live, _)) => ScanView { scan: live, progress: None },
        None => ScanView { scan, progress: None },
    }
}

fn still_running(id: Uuid) -> ApiError {
    ApiError(
        StatusCode::CONFLICT,
        format!("scan {id} is still running; results are available once it finishes"),
    )
}

/// A scan whose RESULTS are readable, i.e. persisted in a terminal state. A
/// known in-flight scan is a 409 pointing the caller back at the progress
/// surface, not a 404 — and, since v6, not an empty result set either: the
/// running row exists but has no entries yet.
pub(crate) fn persisted_scan(state: &AppState, id: Uuid) -> Result<Scan, ApiError> {
    match state.scan_store().get_scan(id) {
        Ok(scan) if scan.status == ScanStatus::Running => Err(still_running(id)),
        Ok(scan) => Ok(scan),
        Err(CoreError::NotFound(_)) => match state.registry.snapshot(id) {
            Some(_) => Err(still_running(id)),
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
        .map(|scan| merge_live(&state.registry, scan))
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
    headers: HeaderMap,
    ApiPath(id): ApiPath<Uuid>,
) -> Result<Response, ApiError> {
    // A running row (v6 persists them) or a running registry entry: deleting
    // mid-walk would race the completion handoff (the worker would upsert
    // a fresh row right after the delete).
    if let Ok(scan) = state.scan_store().get_scan(id)
        && scan.status == ScanStatus::Running
    {
        return Err(ApiError(
            StatusCode::CONFLICT,
            format!("scan {id} is still running; cancel it before deleting"),
        ));
    }
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
    tracing::info!(%id, client = %client_of(&headers), "scan deleted");
    // Synchronously, so the bytes ARE back when the CLI prints "deleted":
    // "deleted" while freeing nothing was the phantom-grw honesty class
    // (phantom-cnr.2). Off the runtime thread — a one-time VACUUM of a
    // pre-1.1.1 file can take tens of seconds.
    let compacting = state.clone();
    tokio::task::spawn_blocking(move || compacting.compact_after_write("delete"))
        .await
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("compaction task failed: {e}")))?;
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
    let explicit = q.root.clone().filter(|r| !r.is_empty());
    let root = explicit.clone().unwrap_or_else(|| scan.root_path.clone());

    // `root=` re-roots AND re-lays-out server-side: the layout below is
    // computed over the requested subtree at the requested view size. An
    // explicit root with no row is the store's NotFound — since 1.1.1 that
    // message names the nearest persisted ancestor when the path was merely
    // folded (phantom-cnr.10), so it goes through `entry`, not a bare 404.
    // (Bind before use: the store guard drops at the end of the statement.)
    if let Some(explicit) = explicit.as_deref() {
        let root_entry = state.scan_store().entry(id, explicit)?;
        if !root_entry.is_dir {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                format!("treemap root must be a directory: {root}"),
            ));
        }
    }
    let entries = state.scan_store().entries(id)?;
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
    let summary = summary.unwrap_or_else(HotspotsSummary::empty);
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
pub(crate) fn same_root(a: &str, b: &str) -> bool {
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
