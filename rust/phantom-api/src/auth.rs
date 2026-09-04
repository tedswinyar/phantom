// Key-file auth: a random key generated on first run, stored 0600, sent by
// clients as `X-Api-Key`. This is a local-loopback trust boundary — it keeps
// other local users and stray browser JS out, nothing more. /health is
// unauthenticated so process supervisors can probe without the key.

use std::io::Write;
use std::path::Path;

use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};
use uuid::Uuid;

use crate::AppState;

pub const API_KEY_HEADER: &str = "x-api-key";

/// How many times the creation-race loser re-reads the winner's file before
/// treating it as a pre-existing blank (see `create_new_or_adopt`).
const KEY_READBACK_RETRIES: u32 = 5;
const KEY_READBACK_DELAY: std::time::Duration = std::time::Duration::from_millis(20);

/// Load the API key from `path`, generating one (0600) if absent.
///
/// Concurrent-start safe: creation uses O_EXCL (`create_new`), so when two
/// API processes race here exactly ONE writes a key and the other ADOPTS it
/// by re-reading the file. The old create+truncate version let both write:
/// last write won the file while each process held its own key in memory —
/// clients reading the file then got permanent 401s from the other process.
pub fn load_or_create_key(path: &Path) -> std::io::Result<String> {
    if let Some(key) = read_nonempty_key(path) {
        return Ok(key);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    create_new_or_adopt(path, Uuid::new_v4().to_string())
}

fn read_nonempty_key(path: &Path) -> Option<String> {
    let existing = std::fs::read_to_string(path).ok()?;
    let key = existing.trim().to_string();
    if key.is_empty() { None } else { Some(key) }
}

/// Try to create the key file EXCLUSIVELY with `candidate`. If another
/// process won the race (AlreadyExists), adopt ITS key by re-reading — with
/// a small bounded wait, because the winner may sit between creating the
/// file and writing the key. A file that stays blank through the retries is
/// not a race window; it is a pre-existing empty key file, which the config
/// contract says to regenerate (truncate-write).
///
/// Factored out of `load_or_create_key` so the race path is unit-testable:
/// a real two-process collision cannot run in a test, but AlreadyExists →
/// re-read can.
fn create_new_or_adopt(path: &Path, candidate: String) -> std::io::Result<String> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    match opts.open(path) {
        Ok(mut f) => {
            writeln!(f, "{candidate}")?;
            Ok(candidate)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            for _ in 0..KEY_READBACK_RETRIES {
                if let Some(winner) = read_nonempty_key(path) {
                    return Ok(winner);
                }
                std::thread::sleep(KEY_READBACK_DELAY);
            }
            // Persistently blank: the documented regenerate-a-blank-file
            // case, not a mid-write window.
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            let mut f = opts.open(path)?;
            writeln!(f, "{candidate}")?;
            Ok(candidate)
        }
        Err(e) => Err(e),
    }
}

pub async fn require_api_key(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let presented = request
        .headers()
        .get(API_KEY_HEADER)
        .and_then(|v| v.to_str().ok());
    // Constant-time compare so key verification cannot leak, via response
    // timing, how many leading bytes were correct. `constant_time_eq` returns
    // false immediately on a length mismatch (length is not secret here) and
    // otherwise compares every byte regardless of where they differ. `&str`
    // `!=` would short-circuit on the first differing byte — a timing side
    // channel. Loopback noise dwarfs the signal today, but this is the
    // template's auth primitive: it should model the right habit.
    let authorized = match presented {
        Some(p) => constant_time_eq::constant_time_eq(p.as_bytes(), state.api_key.as_bytes()),
        None => false,
    };
    if !authorized {
        return (
            StatusCode::UNAUTHORIZED,
            axum::Json(serde_json::json!({
                "error": "missing or invalid X-Api-Key"
            })),
        )
            .into_response();
    }
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_key_once_and_reuses_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("api_key");
        let first = load_or_create_key(&path).unwrap();
        let second = load_or_create_key(&path).unwrap();
        assert_eq!(first, second);
        assert!(!first.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn key_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("api_key");
        load_or_create_key(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn blank_key_file_is_regenerated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("api_key");
        std::fs::write(&path, "\n").unwrap();
        let key = load_or_create_key(&path).unwrap();
        assert!(!key.is_empty());
    }

    /// The creation race, simulated: the file already exists with the
    /// WINNER's key when create_new runs. The loser must ADOPT the winner's
    /// key, never keep its own candidate — the old create+truncate code kept
    /// the candidate AND clobbered the file, splitting server and clients
    /// onto different keys (permanent 401s).
    #[test]
    fn losing_the_creation_race_adopts_the_winners_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("api_key");
        std::fs::write(&path, "winner-key\n").unwrap();

        let adopted = create_new_or_adopt(&path, "loser-candidate".into()).unwrap();
        assert_eq!(adopted, "winner-key", "the loser must adopt, not overwrite");
        // And the file still carries the winner's key.
        assert_eq!(std::fs::read_to_string(&path).unwrap().trim(), "winner-key");
    }

    /// The read-back-blank edge: the file exists but stays empty through the
    /// bounded retries (a pre-existing blank file, not a mid-write window) —
    /// regenerate per the config contract, with the candidate.
    #[test]
    fn persistently_blank_file_after_lost_race_is_regenerated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("api_key");
        std::fs::write(&path, "").unwrap();

        let key = create_new_or_adopt(&path, "candidate-key".into()).unwrap();
        assert_eq!(key, "candidate-key");
        assert_eq!(std::fs::read_to_string(&path).unwrap().trim(), "candidate-key");
    }

    // The constant-time compare must still be a CORRECT equality: exact match
    // accepts, and every mismatch class (wrong byte, prefix, different length)
    // rejects. Timing cannot be asserted in a unit test, but this pins that a
    // "simplification" back to prefix/loose matching would be caught.
    #[test]
    fn key_comparison_is_exact_equality() {
        use constant_time_eq::constant_time_eq;
        let key = "e7ae86e2-308b-444c-8a3d-cd21467ab442";
        assert!(constant_time_eq(key.as_bytes(), key.as_bytes()));
        // One byte off.
        assert!(!constant_time_eq(
            key.as_bytes(),
            "e7ae86e2-308b-444c-8a3d-cd21467ab443".as_bytes()
        ));
        // A correct prefix is not a match.
        assert!(!constant_time_eq(key.as_bytes(), "e7ae86e2".as_bytes()));
        // Empty presented key never matches a real one.
        assert!(!constant_time_eq(key.as_bytes(), b""));
    }
}
