// `notifications/progress` for a waited scan_directory (phantom-mkn.8).
//
// The MCP spec: a client that wants progress puts `_meta.progressToken` on
// its request; the server MAY then send `notifications/progress` carrying
// that token, a monotonically increasing `progress`, an optional `total`,
// and a human `message`. No token, no notifications — ever. A walk has no
// knowable total (that is what the scan is for), so `total` is omitted and
// `progress` is the files-seen counter, which only goes up.
//
// Emission is throttled to one notification per second: the poll runs every
// 150 ms and a host rendering every poll would flicker.

use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// Minimum gap between two progress notifications for one call.
pub const MIN_INTERVAL: Duration = Duration::from_secs(1);

/// Decides when a poll result becomes a notification: the first one at once,
/// then at most one per [`MIN_INTERVAL`]. Also suppresses a repeat with no
/// new files (a stalled walk is not progress).
pub struct Throttle {
    last_emit: Option<Instant>,
    last_files: Option<u64>,
}

impl Throttle {
    pub fn new() -> Self {
        Self {
            last_emit: None,
            last_files: None,
        }
    }

    pub fn should_emit(&mut self, now: Instant, files_seen: u64) -> bool {
        let due = match self.last_emit {
            None => true,
            Some(t) => now.duration_since(t) >= MIN_INTERVAL,
        };
        let moved = self.last_files != Some(files_seen);
        if due && moved {
            self.last_emit = Some(now);
            self.last_files = Some(files_seen);
            true
        } else {
            false
        }
    }
}

impl Default for Throttle {
    fn default() -> Self {
        Self::new()
    }
}

/// The notification for one running-scan view. `token` is echoed as the
/// client sent it (string or integer — the spec allows both).
pub fn notification(token: &Value, scan_view: &Value) -> Value {
    let files = scan_view["progress"]["filesSeen"].as_u64().unwrap_or(0);
    let bytes = scan_view["progress"]["bytesSeen"].as_u64().unwrap_or(0);
    let current = scan_view["progress"]["currentPath"].as_str().unwrap_or("");
    let root = scan_view["rootPath"].as_str().unwrap_or("");
    json!({
        "jsonrpc": "2.0",
        "method": "notifications/progress",
        "params": {
            "progressToken": token,
            "progress": files,
            "message": format!(
                "scanning {root}: {files} files, {} so far — {current}",
                human_bytes(bytes)
            )
        }
    })
}

/// Read the client's progress token off a request's `params._meta`.
pub fn token_from(params: &Value) -> Option<Value> {
    match &params["_meta"]["progressToken"] {
        Value::Null => None,
        v @ (Value::String(_) | Value::Number(_)) => Some(v.clone()),
        _ => None,
    }
}

/// Short, human size for a progress line (binary units, one decimal).
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RAW_SCAN_RUNNING: &str = include_str!("../../../tests/fixtures/scan-running.json");

    #[test]
    fn notification_carries_the_token_files_and_a_readable_message() {
        let view: Value = serde_json::from_str(RAW_SCAN_RUNNING).unwrap();
        let n = notification(&json!("tok-1"), &view);
        assert_eq!(n["jsonrpc"], "2.0");
        assert_eq!(n["method"], "notifications/progress");
        assert_eq!(n["params"]["progressToken"], "tok-1");
        assert_eq!(n["params"]["progress"], 1337, "progress is the files-seen counter");
        assert!(n["params"].get("total").is_none(), "a walk has no knowable total");
        let msg = n["params"]["message"].as_str().unwrap();
        assert!(msg.contains("/Users/ghost") && msg.contains("1337 files") && msg.contains("941.9 MiB"), "{msg}");
        assert!(msg.ends_with("/Users/ghost/Library/Caches/deep/file.bin"), "{msg}");
        assert!(n.get("id").is_none(), "a notification has no id");
    }

    #[test]
    fn integer_tokens_are_echoed_as_integers() {
        let view: Value = serde_json::from_str(RAW_SCAN_RUNNING).unwrap();
        assert_eq!(notification(&json!(7), &view)["params"]["progressToken"], 7);
    }

    #[test]
    fn token_is_read_from_meta_and_only_when_string_or_integer() {
        assert_eq!(token_from(&json!({"_meta": {"progressToken": "abc"}})), Some(json!("abc")));
        assert_eq!(token_from(&json!({"_meta": {"progressToken": 12}})), Some(json!(12)));
        assert_eq!(token_from(&json!({"name": "scan_directory"})), None, "no _meta, no token");
        assert_eq!(token_from(&json!({"_meta": {"progressToken": true}})), None);
        assert_eq!(token_from(&json!({"_meta": {"progressToken": null}})), None);
    }

    #[test]
    fn throttle_emits_first_then_once_per_second_and_only_on_movement() {
        let t0 = Instant::now();
        let mut t = Throttle::new();
        assert!(t.should_emit(t0, 10), "first poll always emits");
        assert!(!t.should_emit(t0 + Duration::from_millis(150), 20), "too soon");
        assert!(!t.should_emit(t0 + Duration::from_millis(999), 30), "still too soon");
        assert!(t.should_emit(t0 + Duration::from_millis(1000), 30), "due");
        assert!(!t.should_emit(t0 + Duration::from_millis(2500), 30), "due but nothing moved");
        assert!(t.should_emit(t0 + Duration::from_millis(2500), 31), "moved");
    }

    #[test]
    fn human_bytes_rounds_to_one_decimal_in_binary_units() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(987_654_321), "941.9 MiB");
        assert_eq!(human_bytes(5 * 1024 * 1024 * 1024 * 1024), "5.0 TiB");
    }
}
