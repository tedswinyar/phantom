// phantom-mcp — MCP server over stdio (JSON-RPC 2.0), following the
// portfolio house pattern: hand-rolled protocol loop, blocking HTTP to the
// API hub for all data. Never opens the database.
//
// The scan tools are store-backed by construction: they read the API's
// persisted results (diskSize everywhere), so an agent asking three times
// gets one answer — the v0.1 design re-walked the disk on every call and
// reported logical sizes to boot.

use std::io::{self, BufRead, Write};

use serde::Deserialize;
use serde_json::{Value, json};

const PROTOCOL_VERSION: &str = "2024-11-05";

/// How long `scan_directory` will wait before handing back the running
/// view with a note (see wait_for_scan) — big trees walk for minutes and a
/// tool call should not block that long.
const WAIT_CAP: std::time::Duration = std::time::Duration::from_secs(120);

/// How often `scan_directory` polls a running scan for its terminal state.
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(150);

#[derive(Deserialize)]
struct JsonRpcRequest {
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

fn response(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0", "id": id,
        "error": { "code": code, "message": message }
    })
}

struct Api {
    base: String,
    key: String,
    http: reqwest::blocking::Client,
}

impl Api {
    fn from_env() -> Self {
        let base = std::env::var("PHANTOM_API_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:8768".into());
        let key = std::env::var("PHANTOM_API_KEY")
            .ok()
            .or_else(|| {
                let path = std::env::var("PHANTOM_KEY_FILE")
                    .map(std::path::PathBuf::from)
                    .ok()
                    .or_else(|| dirs::config_dir().map(|d| d.join("phantom/api_key")))?;
                std::fs::read_to_string(path).ok()
            })
            .map(|k| k.trim().to_string())
            .unwrap_or_default();
        Self {
            base: base.trim_end_matches('/').to_string(),
            key,
            // A hung API must not stall the agent's tool call forever (rust M4).
            http: reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .connect_timeout(std::time::Duration::from_secs(5))
                .build()
                .unwrap_or_else(|_| reqwest::blocking::Client::new()),
        }
    }

    fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value, String> {
        let mut req = self
            .http
            .request(method, format!("{}{path}", self.base))
            .header("x-api-key", &self.key);
        if let Some(body) = body {
            req = req.json(&body);
        }
        let resp = req
            .send()
            .map_err(|e| format!("cannot reach phantom-api at {}: {e}", self.base))?;
        Self::body_or_error(resp)
    }

    /// The scan every result tool defaults to: the most recent COMPLETED
    /// scan (a running or cancelled one has no readable results).
    fn latest_complete_scan_id(&self) -> Result<String, String> {
        let scans = self.call(reqwest::Method::GET, "/scans", None)?;
        scans
            .as_array()
            .and_then(|list| {
                list.iter()
                    .find(|s| s["status"] == "complete")
                    .and_then(|s| s["id"].as_str())
                    .map(str::to_string)
            })
            .ok_or_else(|| "no completed scans; run scan_directory first".to_string())
    }

    /// Resolve the scan a result tool should read: an explicit `scanId`
    /// argument wins; otherwise the latest completed scan.
    fn resolve_scan_id(&self, args: &Value) -> Result<String, String> {
        match args["scanId"].as_str() {
            Some(id) => Ok(id.to_string()),
            None => self.latest_complete_scan_id(),
        }
    }

    /// Poll GET /scans/{id} until the scan reaches a terminal status, or
    /// until WAIT_CAP elapses. A capped wait returns the RUNNING view with a
    /// `note` field explaining how to keep following it — better than
    /// holding an agent's tool call hostage to a multi-minute walk (freeze
    /// review R5). `note` is an MCP-envelope field, not part of the HTTP
    /// wire contract.
    fn wait_for_scan(&self, id: &str) -> Result<Value, String> {
        let started = std::time::Instant::now();
        loop {
            let mut v = self.call(reqwest::Method::GET, &format!("/scans/{id}"), None)?;
            if v["status"] != "running" {
                return Ok(v);
            }
            if started.elapsed() >= WAIT_CAP {
                if let Some(obj) = v.as_object_mut() {
                    obj.insert(
                        "note".into(),
                        json!(format!(
                            "still running after {}s; polling stopped — follow progress \
                             with list_scans and read results once status is complete",
                            WAIT_CAP.as_secs()
                        )),
                    );
                }
                return Ok(v);
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    /// GET /scans/{id}/files with the standard filters, returning the
    /// agent-facing envelope `{files: [...], nextCursor: <token|null>}` —
    /// same continuation idiom the paginated routes share.
    fn find_large_files(&self, id: &str, query: &str) -> Result<Value, String> {
        let resp = self
            .http
            .get(format!("{}/scans/{id}/files{query}", self.base))
            .header("x-api-key", &self.key)
            .send()
            .map_err(|e| format!("cannot reach phantom-api at {}: {e}", self.base))?;
        let next = resp
            .headers()
            .get("x-next-cursor")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let files = Self::body_or_error(resp)?;
        Ok(json!({ "files": files, "nextCursor": next }))
    }

    /// GET /health. Unlike [`call`], a `503 degraded` body is a valid answer
    /// an agent should SEE (the store is unusable), not an error to hide — so
    /// return the JSON body regardless of status.
    fn health(&self) -> Result<Value, String> {
        let resp = self
            .http
            .get(format!("{}/health", self.base))
            .send()
            .map_err(|e| format!("cannot reach phantom-api at {}: {e}", self.base))?;
        let status = resp.status();
        let text = resp
            .text()
            .map_err(|e| format!("cannot read API response body: {e}"))?;
        serde_json::from_str::<Value>(&text).map_err(|_| {
            let body = text.trim();
            let shown = if body.is_empty() { "<empty response body>" } else { body };
            format!("health returned a non-JSON body: {shown} ({})", status.as_u16())
        })
    }

    /// Parse a response into JSON, or an error string. When the body is NOT
    /// JSON (e.g. a text/plain error from the API or a proxy), fall back to
    /// `status line + raw body` so the real failure reaches the agent instead
    /// of a misleading "invalid JSON from API" (agentapi C1).
    fn body_or_error(resp: reqwest::blocking::Response) -> Result<Value, String> {
        let status = resp.status();
        let text = resp
            .text()
            .map_err(|e| format!("cannot read API response body: {e}"))?;
        match serde_json::from_str::<Value>(&text) {
            Ok(value) => {
                if !status.is_success() {
                    let msg = value["error"].as_str().unwrap_or("unknown error");
                    return Err(format!("{msg} ({})", status.as_u16()));
                }
                Ok(value)
            }
            Err(_) => {
                let body = text.trim();
                let shown = if body.is_empty() { "<empty response body>" } else { body };
                if status.is_success() {
                    Err(format!("unexpected non-JSON success body from API: {shown}"))
                } else {
                    Err(format!("{shown} ({})", status.as_u16()))
                }
            }
        }
    }
}

/// The `scanId` property every result tool shares.
fn scan_id_property() -> Value {
    json!({
        "type": "string",
        "format": "uuid",
        "description": "Scan UUID. Omit to use the most recent completed scan. \
            Only the last 25 scans are kept — a stale id 404s; rescan rather \
            than retrying the id."
    })
}

fn tool_definitions() -> Value {
    json!([
        {
            "name": "scan_directory",
            "description": "Scan a directory tree and record its disk usage. \
                By default waits for the scan to finish and returns the \
                completed scan. Totals are disk bytes (st_blocks × 512), \
                HARDLINK-DEDUPED (an inode counts once, like `du` — a tree of \
                cargo/pnpm hardlinks totals far less than the naive sum). \
                Large trees (a home directory) can take MINUTES to walk: \
                prefer wait: false and poll with list_scans. A waited call \
                gives up after 120s and returns the still-running view with \
                a note field; keep following it via list_scans. The completed \
                scan also carries unreadablePaths: a capped SAMPLE (first 100, \
                walk order) of the entries behind errorCount — errorCount is \
                the truth, the list is a where-did-it-fail sample.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute path of the directory to scan"
                    },
                    "wait": {
                        "type": "boolean",
                        "description": "Wait for completion (default true)"
                    }
                },
                "required": ["path"]
            }
        },
        {
            "name": "list_scans",
            "description": "List recorded scans, NEWEST FIRST — running scans \
                included (their progress field carries live counters). All \
                sizes are disk bytes (st_blocks × 512), hardlink-deduped (an \
                inode counts once, like `du`). Note the order when picking a \
                pair for diff_scans: element 0 is the newest, so passing \
                [0] as scanA and [1] as scanB is REVERSE-chronological — put \
                the older scan (higher index) in scanA to read deltas forward.",
            "inputSchema": { "type": "object", "properties": {}, "required": [] }
        },
        {
            "name": "find_large_files",
            "description": "Largest files of a scan, by disk size descending. \
                Returns {files, nextCursor}: when nextCursor is non-null, more \
                files remain — call again with cursor set to that value. Only \
                files of at least 1 MiB disk size are recorded individually; \
                smaller files are folded into directory totals.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "scanId": scan_id_property(),
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Max files per page (server caps it). Omit for the default page size."
                    },
                    "fileType": {
                        "type": "string",
                        "description": "Only files with this extension (any case)"
                    },
                    "search": {
                        "type": "string",
                        "description": "Only files whose path contains this substring"
                    },
                    "cursor": {
                        "type": "string",
                        "description": "Opaque continuation token from a previous call's nextCursor. Omit for the first page."
                    }
                },
                "required": []
            }
        },
        {
            "name": "get_space_by_type",
            "description": "Disk usage of a scan grouped by file type \
                (lowercased extension; null = no extension), largest first. \
                Computed from the FULL walk, so small files count here even \
                though they have no individual file rows.",
            "inputSchema": {
                "type": "object",
                "properties": { "scanId": scan_id_property() },
                "required": []
            }
        },
        {
            "name": "get_treemap",
            "description": "Squarified treemap layout of a scan's disk usage. \
                Pass root to re-root the layout at a subdirectory; width/height \
                to lay out at a specific view size; maxDepth to limit nesting. \
                The response grows with directory breadth — for large scans \
                use maxDepth 1-2 and re-root to drill instead of fetching \
                deep layouts.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "scanId": scan_id_property(),
                    "root": {
                        "type": "string",
                        "description": "Directory path to re-root at (default: the scan root)"
                    },
                    "width": {
                        "type": "number",
                        "description": "Layout width in points (default 800)"
                    },
                    "height": {
                        "type": "number",
                        "description": "Layout height in points (default 600)"
                    },
                    "maxDepth": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Levels to recurse; 0 = root only (default 4)"
                    }
                },
                "required": []
            }
        },
        {
            "name": "get_hotspots",
            "description": "Reclaimable-space hotspots of a scan, classified \
                when the scan completed. Returns {groups, reclaimEstimate, \
                reviewDiskSize, cloudDataloadedLogicalSize, \
                cloudDataloadedDiskSize}. Sizes are hardlink-deduped disk \
                bytes; reclaimEstimate covers only safely-regenerable \
                categories and excludes cloud-dataloaded placeholders. \
                Phantom never deletes: each group carries an action hint \
                naming the safe tool (e.g. `cargo clean`), not an operation.",
            "inputSchema": {
                "type": "object",
                "properties": { "scanId": scan_id_property() },
                "required": []
            }
        },
        {
            "name": "diff_scans",
            "description": "Compare two COMPLETED scans of the same root: what \
                grew, what was freed. Deltas read scanB − scanA, so pass the \
                OLDER scan as scanA (get ids from list_scans, which is \
                newest-first — the older scan is the HIGHER index). Returns \
                {scanA, scanB, scanAStartedAt, scanBStartedAt, \
                reversedChronology, rootPath, diskDelta, logicalDelta, \
                fileCountDelta, dirCountDelta, errorCountDelta, grown, freed}. \
                reversedChronology is true if you passed them newest-first (a \
                grown directory then shows as freed) — check it before \
                trusting the signs. grown/freed list the biggest per-directory \
                movements (a created dir has before: null, a deleted one \
                after: null). Sizes are hardlink-deduped disk bytes; a signed \
                delta is bytes B minus bytes A. Both scans must be complete \
                and cover the same root, else an error names why.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "scanA": {
                        "type": "string",
                        "format": "uuid",
                        "description": "The 'before' scan id — the OLDER scan. Only the last 25 scans are kept; a pruned baseline is gone (rescanning makes a NEW scan, not the old one), so diff soon after scanning."
                    },
                    "scanB": {
                        "type": "string",
                        "format": "uuid",
                        "description": "The 'after' scan id (the newer one)"
                    }
                },
                "required": ["scanA", "scanB"]
            }
        },
        {
            "name": "health",
            "description": "Check the API server's health. Returns {status, version}; \
                status is \"ok\" when the datastore is usable, \"degraded\" otherwise.",
            "inputSchema": { "type": "object", "properties": {}, "required": [] }
        }
    ])
}

/// Render an argument as a query-string scalar: accepts a JSON string or a
/// JSON number (agents send `limit` either way), rejects anything else.
fn scalar_arg(args: &Value, key: &str) -> Option<String> {
    match &args[key] {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Assemble a query string from (key, value) pairs, skipping absent values.
/// Percent-encode a query VALUE (RFC 3986): unreserved bytes pass;
/// everything else — including `&`, `+`, `%`, `#`, spaces — becomes %XX.
/// Without this, a path like "a&b" silently truncates into a second query
/// parameter server-side (freeze review R3).
fn encode_query_value(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for b in v.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn query_string(params: &[(&str, Option<String>)]) -> String {
    let joined: Vec<String> = params
        .iter()
        .filter_map(|(k, v)| v.as_ref().map(|v| format!("{k}={}", encode_query_value(v))))
        .collect();
    if joined.is_empty() {
        String::new()
    } else {
        format!("?{}", joined.join("&"))
    }
}

fn handle_tool_call(api: &Api, name: &str, args: &Value) -> Result<Value, String> {
    match name {
        "scan_directory" => {
            let path = args["path"]
                .as_str()
                .ok_or("scan_directory requires a string 'path'")?;
            let started = api.call(
                reqwest::Method::POST,
                "/scans",
                Some(json!({ "rootPath": path })),
            )?;
            if args["wait"] == false {
                return Ok(started);
            }
            let id = started["id"]
                .as_str()
                .ok_or("API returned a scan without an id")?;
            api.wait_for_scan(id)
        }
        "list_scans" => api.call(reqwest::Method::GET, "/scans", None),
        "find_large_files" => {
            let id = api.resolve_scan_id(args)?;
            let query = query_string(&[
                ("limit", scalar_arg(args, "limit")),
                ("fileType", scalar_arg(args, "fileType")),
                ("search", scalar_arg(args, "search")),
                ("cursor", scalar_arg(args, "cursor")),
            ]);
            api.find_large_files(&id, &query)
        }
        "get_space_by_type" => {
            let id = api.resolve_scan_id(args)?;
            api.call(reqwest::Method::GET, &format!("/scans/{id}/types"), None)
        }
        "get_hotspots" => {
            let id = api.resolve_scan_id(args)?;
            api.call(
                reqwest::Method::GET,
                &format!("/scans/{id}/hotspots"),
                None,
            )
        }
        "get_treemap" => {
            let id = api.resolve_scan_id(args)?;
            let query = query_string(&[
                ("root", scalar_arg(args, "root")),
                ("width", scalar_arg(args, "width")),
                ("height", scalar_arg(args, "height")),
                ("maxDepth", scalar_arg(args, "maxDepth")),
            ]);
            api.call(
                reqwest::Method::GET,
                &format!("/scans/{id}/treemap{query}"),
                None,
            )
        }
        "diff_scans" => {
            let a = args["scanA"]
                .as_str()
                .ok_or("diff_scans requires a string 'scanA' (the older scan)")?;
            let b = args["scanB"]
                .as_str()
                .ok_or("diff_scans requires a string 'scanB' (the newer scan)")?;
            api.call(
                reqwest::Method::GET,
                &format!("/scans/{a}/diff/{b}"),
                None,
            )
        }
        "health" => api.health(),
        other => Err(format!("unknown tool: {other}")),
    }
}

fn handle_request(api: &Api, req: &JsonRpcRequest) -> Option<Value> {
    let id = req.id.clone()?; // Notifications (no id) get no response.
    let resp = match req.method.as_str() {
        "initialize" => response(
            id,
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": { "tools": {} },
                "serverInfo": {
                    "name": "phantom-mcp",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }),
        ),
        "tools/list" => response(id, json!({ "tools": tool_definitions() })),
        "tools/call" => {
            let name = req.params["name"].as_str().unwrap_or_default();
            let args = req.params.get("arguments").cloned().unwrap_or(json!({}));
            match handle_tool_call(api, name, &args) {
                Ok(result) => response(
                    id,
                    json!({
                        "content": [{
                            "type": "text",
                            "text": serde_json::to_string_pretty(&result).unwrap()
                        }]
                    }),
                ),
                Err(msg) => response(
                    id,
                    json!({
                        "content": [{ "type": "text", "text": msg }],
                        "isError": true
                    }),
                ),
            }
        }
        "ping" => response(id, json!({})),
        _ => error_response(id, -32601, &format!("method not found: {}", req.method)),
    };
    Some(resp)
}

fn main() {
    let api = Api::from_env();
    let stdin = io::stdin();
    let mut stdout = io::stdout();

    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<JsonRpcRequest>(&line) {
            Ok(req) => handle_request(&api, &req),
            Err(e) => Some(error_response(
                Value::Null,
                -32700,
                &format!("parse error: {e}"),
            )),
        };
        if let Some(reply) = reply {
            let out = serde_json::to_string(&reply).unwrap();
            if writeln!(stdout, "{out}").and_then(|_| stdout.flush()).is_err() {
                break;
            }
        }
    }
}
