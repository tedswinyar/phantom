// The HTTP surface: camelCase wire format, explicit nulls, typed error
// mapping, /health open, every domain route behind key-file auth.

use axum::{
    Json, Router,
    extract::{
        FromRequest, FromRequestParts, Path, Query, Request, State,
        rejection::{JsonRejection, PathRejection, QueryRejection},
    },
    http::{StatusCode, header, request::Parts},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use phantom_core::CoreError;

use crate::{AppState, auth, scans};

/// Default page size when `?limit` is absent, and the hard ceiling on it.
/// A paginated GET with no params returns the first page at this size and a
/// continuation cursor when more rows remain — so a growing collection can
/// never return an unbounded list that blows an agent's context budget.
const DEFAULT_LIMIT: usize = 100;
const MAX_LIMIT: usize = 500;

/// Response header carrying the opaque continuation token for the next page,
/// present only when more rows remain. (Body stays a bare JSON array so every
/// existing decoder — Swift, fixtures, conformance — is unaffected.)
pub(crate) const NEXT_CURSOR_HEADER: &str = "x-next-cursor";

pub fn router(state: AppState) -> Router {
    let authed = Router::new()
        .route("/scans", get(scans::list_scans).post(scans::create_scan))
        .route(
            "/scans/{id}",
            get(scans::get_scan).delete(scans::delete_scan),
        )
        .route("/scans/{id}/cancel", post(scans::cancel_scan))
        .route("/scans/{id}/treemap", get(scans::get_treemap))
        .route("/scans/{id}/tree", get(scans::get_tree))
        .route("/scans/{id}/files", get(scans::list_files))
        .route("/scans/{id}/entry", get(scans::get_entry))
        .route("/scans/{id}/types", get(scans::get_types))
        .route("/scans/{id}/hotspots", get(scans::get_hotspots))
        .route("/scans/{id}/diff/{other}", get(scans::get_diff))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_api_key,
        ));

    Router::new()
        .route("/health", get(health))
        .merge(authed)
        // Force axum's built-in empty 404/405 responses into the `{error}`
        // contract shape too, so NO non-2xx path escapes it.
        .layer(middleware::map_response(error_shape_fallback))
        .with_state(state)
}

/// Wire version of API errors: `{"error": "<message>"}` with a meaningful
/// status. The message is safe to expose (validation text, "not found");
/// internal DB/schema detail is generalized to a generic 500 in the
/// `CoreError` mapping so SQLite strings never reach a client.
pub(crate) struct ApiError(pub(crate) StatusCode, pub(crate) String);

impl From<CoreError> for ApiError {
    fn from(e: CoreError) -> Self {
        match &e {
            CoreError::NotFound(_) => Self(StatusCode::NOT_FOUND, e.to_string()),
            CoreError::InvalidInput(_) => Self(StatusCode::BAD_REQUEST, e.to_string()),
            // Db / Schema / Io carry internal detail (e.g. "UNIQUE constraint
            // failed: notes.id"). Log the real error server-side; return a
            // generic message so nothing internal leaks over the wire.
            _ => {
                tracing::error!(error = %e, "internal server error");
                Self(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal server error".into(),
                )
            }
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({ "error": self.1 }))).into_response()
    }
}

// --- Extractors that route rejections through the `{error}` contract -------
//
// axum's stock `Json<T>` and `Path<T>` rejections return `text/plain`, which
// the CLI/MCP `resp.json()` cannot parse — an agent that sends a bad field or
// a malformed UUID gets "invalid JSON from API" instead of the real reason.
// These thin wrappers preserve the rejection's status (422 unknown-field,
// 400 bad-uuid) but re-clothe the body as `{"error": ...}`.

pub(crate) struct ApiJson<T>(pub(crate) T);

impl<S, T> FromRequest<S> for ApiJson<T>
where
    Json<T>: FromRequest<S, Rejection = JsonRejection>,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(value)) => Ok(Self(value)),
            Err(rejection) => Err(ApiError(rejection.status(), rejection.body_text())),
        }
    }
}

pub(crate) struct ApiPath<T>(pub(crate) T);

impl<S, T> FromRequestParts<S> for ApiPath<T>
where
    Path<T>: FromRequestParts<S, Rejection = PathRejection>,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        match Path::<T>::from_request_parts(parts, state).await {
            Ok(Path(value)) => Ok(Self(value)),
            Err(rejection) => Err(ApiError(rejection.status(), rejection.body_text())),
        }
    }
}

pub(crate) struct ApiQuery<T>(pub(crate) T);

impl<S, T> FromRequestParts<S> for ApiQuery<T>
where
    Query<T>: FromRequestParts<S, Rejection = QueryRejection>,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        match Query::<T>::from_request_parts(parts, state).await {
            Ok(Query(value)) => Ok(Self(value)),
            Err(rejection) => Err(ApiError(rejection.status(), rejection.body_text())),
        }
    }
}

/// Re-clothe axum's built-in EMPTY error responses (unmatched route → 404,
/// wrong method → 405) as `{"error": ...}`. Handler and extractor responses
/// already carry a JSON body + content-type, so they pass through untouched.
async fn error_shape_fallback(response: Response) -> Response {
    let status = response.status();
    let is_empty_builtin = matches!(
        status,
        StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
    ) && response.headers().get(header::CONTENT_TYPE).is_none();

    if is_empty_builtin {
        let message = if status == StatusCode::NOT_FOUND {
            "no such route"
        } else {
            "method not allowed"
        };
        return (status, Json(serde_json::json!({ "error": message }))).into_response();
    }
    response
}

// --- Handlers --------------------------------------------------------------

async fn health(State(state): State<AppState>) -> impl IntoResponse {
    // Touch the store so /health means "can serve requests", not "process
    // exists". `state.scan_store()` is poison-tolerant, so a prior handler
    // panic does not turn a working store into a false "degraded".
    let ok = state.scan_store().list_scans().is_ok();
    let status = if ok { "ok" } else { "degraded" };
    let code = if ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        code,
        Json(serde_json::json!({
            "status": status,
            "version": env!("CARGO_PKG_VERSION"),
        })),
    )
}

/// Shared `?limit` + `?cursor` validation for every paginated listing
/// (notes, scan files). Returns `(limit, offset)`; the cursor is an opaque
/// continuation token to clients, an offset to the stores.
pub(crate) fn parse_limit_cursor(
    limit: Option<&str>,
    cursor: Option<&str>,
) -> Result<(usize, usize), ApiError> {
    let limit = match limit.map(str::trim) {
        None | Some("") => DEFAULT_LIMIT,
        Some(s) => {
            let n: usize = s.parse().map_err(|_| {
                ApiError(
                    StatusCode::BAD_REQUEST,
                    format!("limit must be a positive integer (got {s:?})"),
                )
            })?;
            if n == 0 {
                return Err(ApiError(
                    StatusCode::BAD_REQUEST,
                    "limit must be at least 1".into(),
                ));
            }
            n.min(MAX_LIMIT)
        }
    };
    let offset = match cursor.map(str::trim) {
        None | Some("") => 0,
        Some(s) => s.parse().map_err(|_| {
            ApiError(
                StatusCode::BAD_REQUEST,
                format!("cursor is not a valid continuation token (got {s:?})"),
            )
        })?,
    };
    Ok((limit, offset))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Mutation-proof (Testing standard): change the generic-500 arm to return
    // `Self(INTERNAL_SERVER_ERROR, e.to_string())` and this fails, because the
    // internal detail would then leak into the body. (Db/Schema/Io all share
    // this arm; Schema needs no rusqlite dependency to construct in a test.)
    #[test]
    fn internal_errors_do_not_leak_to_clients() {
        let leaky = CoreError::Schema("secret_internal_detail: scans.id".into());
        let ApiError(status, body) = leaky.into();
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body, "internal server error");
        assert!(
            !body.contains("secret_internal_detail"),
            "internal detail must not reach the client"
        );
    }

    #[test]
    fn user_facing_errors_keep_their_message() {
        let ApiError(status, body) = CoreError::NotFound("scan abc".into()).into();
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body.contains("scan abc"));

        let ApiError(status, body) = CoreError::InvalidInput("title must not be empty".into()).into();
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("title"));
    }
}
