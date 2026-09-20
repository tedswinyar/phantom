// phantom-mcp — MCP server over stdio (JSON-RPC 2.0), following the
// portfolio house pattern: hand-rolled protocol loop, blocking HTTP to the
// API hub for all data. Never opens the database.
//
// The scan tools are store-backed by construction: they read the API's
// persisted results (diskSize everywhere), so an agent asking three times
// gets one answer — the v0.1 design re-walked the disk on every call and
// reported logical sizes to boot.

mod progress;
mod protocol;
mod shape;

use std::io::{self, BufRead, Write};

use serde::Deserialize;
use serde_json::{Value, json};

use protocol::{MAX_RESULT_CHARS, negotiate_protocol_version, structured_content, tool_definitions};
use shape::{ResponseFormat, enforce_budget, project};

/// How long `scan_directory` will wait before handing back the running
/// view with a note (see wait_for_scan) — big trees walk for minutes and a
/// tool call should not block that long. Under the 2-minute mark at which
/// Claude Code backgrounds an MCP call, so a waited scan always returns
/// something the agent sees in the foreground (phantom-mkn.8).
const WAIT_CAP: std::time::Duration = std::time::Duration::from_secs(60);

/// How often `scan_directory` polls a running scan for its terminal state.
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(150);

#[derive(Deserialize)]
struct JsonRpcRequest {
    /// Must be exactly "2.0" (JSON-RPC 2.0 §4). Optional in the struct so
    /// the error can name the id the client sent (phantom-b7u).
    jsonrpc: Option<String>,
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
        // PHANTOM_API_URL, else the URL the running API published beside its
        // key file, else the registered port (phantom_core::discovery — the
        // day another vendor's agent sat on 8768 the default would have been it).
        let (base, source) = phantom_core::discovery::resolve_api_url(
            std::env::var("PHANTOM_API_URL").ok().as_deref(),
            phantom_core::discovery::published_url_file().as_deref(),
        );
        eprintln!("phantom-mcp: API {base} ({})", source.describe(phantom_core::discovery::published_url_file().as_deref()));
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

    /// The newest completed scan's root — `get_growth`'s default subject.
    fn latest_complete_scan_root(&self) -> Result<String, String> {
        let scans = self.call(reqwest::Method::GET, "/scans", None)?;
        scans
            .as_array()
            .and_then(|list| {
                list.iter()
                    .find(|s| s["status"] == "complete")
                    .and_then(|s| s["rootPath"].as_str())
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
    ///
    /// While waiting, every poll that passes the throttle becomes a
    /// `notifications/progress` on `notify` — but only when the client sent
    /// a `progressToken` (the spec's opt-in; `token` is None otherwise).
    fn wait_for_scan(
        &self,
        id: &str,
        token: Option<&Value>,
        notify: &mut dyn FnMut(Value),
    ) -> Result<Value, String> {
        let started = std::time::Instant::now();
        let mut throttle = progress::Throttle::new();
        loop {
            let mut v = self.call(reqwest::Method::GET, &format!("/scans/{id}"), None)?;
            if v["status"] != "running" {
                return Ok(v);
            }
            if let Some(token) = token {
                let files = v["progress"]["filesSeen"].as_u64().unwrap_or(0);
                if throttle.should_emit(std::time::Instant::now(), files) {
                    notify(progress::notification(token, &v));
                }
            }
            if started.elapsed() >= WAIT_CAP {
                if let Some(obj) = v.as_object_mut() {
                    obj.insert(
                        "note".into(),
                        json!(format!(
                            "still running after {}s; polling stopped — follow progress \
                             with scan_status (this id) and read results once status is \
                             complete, or stop it with cancel_scan",
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

    /// GET a TEXT body (the plan script). Non-2xx routes through the same
    /// `{error}` handling as JSON calls.
    fn text(&self, path: &str) -> Result<String, String> {
        let resp = self
            .http
            .get(format!("{}{path}", self.base))
            .header("x-api-key", &self.key)
            .send()
            .map_err(|e| format!("cannot reach phantom-api at {}: {e}", self.base))?;
        if !resp.status().is_success() {
            return Self::body_or_error(resp).map(|_| String::new());
        }
        resp.text()
            .map_err(|e| format!("cannot read API response body: {e}"))
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

/// The `scanId` a status/cancel call MUST name (no latest-completed default:
/// those tools are about one specific scan, usually a running one).
fn required_scan_id<'a>(args: &'a Value, tool: &str) -> Result<&'a str, String> {
    args["scanId"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| format!("{tool} requires a string 'scanId' (from scan_directory or list_scans)"))
}

fn handle_tool_call(
    api: &Api,
    name: &str,
    args: &Value,
    token: Option<&Value>,
    notify: &mut dyn FnMut(Value),
) -> Result<Value, String> {
    match name {
        "scan_directory" => {
            let path = args["path"]
                .as_str()
                .ok_or("scan_directory requires a string 'path'")?;
            let mut body = json!({ "rootPath": path });
            for key in ["crossVolumes", "verifyLocks", "toolEstimates"] {
                if let Some(b) = args[key].as_bool() {
                    body[key] = json!(b);
                }
            }
            if let Some(t) = args["olderThan"].as_str() {
                body["olderThan"] = json!(t);
            }
            let started = api.call(reqwest::Method::POST, "/scans", Some(body))?;
            if args["wait"] == false {
                return Ok(started);
            }
            let id = started["id"]
                .as_str()
                .ok_or("API returned a scan without an id")?;
            api.wait_for_scan(id, token, notify)
        }
        "scan_status" => {
            let id = required_scan_id(args, "scan_status")?;
            api.call(reqwest::Method::GET, &format!("/scans/{id}"), None)
        }
        "cancel_scan" => {
            let id = required_scan_id(args, "cancel_scan")?;
            api.call(reqwest::Method::POST, &format!("/scans/{id}/cancel"), None)
        }
        "plan_reclaim" => {
            let id = api.resolve_scan_id(args)?;
            let mut body = json!({});
            if let Some(t) = args["maxTier"].as_str() {
                body["maxTier"] = json!(t);
            }
            if let Some(b) = args["minBytes"].as_u64() {
                body["minBytes"] = json!(b);
            }
            let mut plan = api.call(reqwest::Method::POST, &format!("/scans/{id}/plan"), Some(body))?;
            if args["includeScript"] == true {
                let plan_id = plan["planId"]
                    .as_str()
                    .ok_or("API returned a plan without a planId")?
                    .to_string();
                let script = api.text(&format!("/plans/{plan_id}/script"))?;
                plan["script"] = json!(script);
            }
            Ok(plan)
        }
        "verify_reclaim" => {
            let plan_id = args["planId"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .ok_or("verify_reclaim requires a string 'planId' (from plan_reclaim)")?;
            let after_id = match args["afterScanId"].as_str().filter(|s| !s.trim().is_empty()) {
                Some(id) => id.to_string(),
                None => {
                    // Rescan the plan's root now, waiting like scan_directory.
                    let plan = api.call(reqwest::Method::GET, &format!("/plans/{plan_id}"), None)?;
                    let root = plan["rootPath"]
                        .as_str()
                        .ok_or("stored plan has no rootPath")?
                        .to_string();
                    let started = api.call(
                        reqwest::Method::POST,
                        "/scans",
                        Some(json!({ "rootPath": root })),
                    )?;
                    let id = started["id"]
                        .as_str()
                        .ok_or("API returned a scan without an id")?
                        .to_string();
                    let done = api.wait_for_scan(&id, token, notify)?;
                    match done["status"].as_str() {
                        Some("complete") => id,
                        Some("running") => {
                            return Err(format!(
                                "rescan {id} of {root} is still running after {}s; call                                  verify_reclaim again with afterScanId: {id:?} once scan_status                                  reports complete",
                                WAIT_CAP.as_secs()
                            ));
                        }
                        other => {
                            return Err(format!(
                                "rescan {id} ended {}; cannot verify — scan again",
                                other.unwrap_or("?")
                            ));
                        }
                    }
                }
            };
            api.call(
                reqwest::Method::POST,
                &format!("/plans/{plan_id}/verify"),
                Some(json!({ "afterScanId": after_id })),
            )
        }
        "get_volume_status" => {
            let query = query_string(&[
                ("path", scalar_arg(args, "path")),
                (
                    "snapshots",
                    args["snapshots"].as_bool().filter(|b| *b).map(|_| "true".to_string()),
                ),
                ("scanId", scalar_arg(args, "scanId")),
            ]);
            api.call(reqwest::Method::GET, &format!("/volume{query}"), None)
        }
        "explain_path" => {
            let path = args["path"]
                .as_str()
                .filter(|p| !p.trim().is_empty())
                .ok_or("explain_path requires a string 'path' (as the scan recorded it)")?;
            let id = api.resolve_scan_id(args)?;
            let query = query_string(&[("path", Some(path.to_string()))]);
            api.call(reqwest::Method::GET, &format!("/scans/{id}/explain{query}"), None)
        }
        "find_stale_projects" => {
            let id = api.resolve_scan_id(args)?;
            let query = query_string(&[("olderThan", scalar_arg(args, "olderThan"))]);
            api.call(reqwest::Method::GET, &format!("/scans/{id}/stale{query}"), None)
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
        "get_growth" => {
            let root = match args["root"].as_str().map(str::trim).filter(|r| !r.is_empty()) {
                Some(r) => r.to_string(),
                None => api.latest_complete_scan_root()?,
            };
            let query = query_string(&[("root", Some(root)), ("groupBy", scalar_arg(args, "groupBy"))]);
            api.call(reqwest::Method::GET, &format!("/scans/series{query}"), None)
        }
        "health" => api.health(),
        other => Err(format!("unknown tool: {other}")),
    }
}

/// One tool call, end to end: validate the format argument, run the tool,
/// project, then enforce the result budget. Returns the value AND its
/// rendering (the budget check had to render it anyway).
fn call_tool(
    api: &Api,
    name: &str,
    args: &Value,
    token: Option<&Value>,
    notify: &mut dyn FnMut(Value),
) -> Result<(Value, String), String> {
    let format = ResponseFormat::from_args(args)?;
    let result = handle_tool_call(api, name, args, token, notify)?;
    let projected = project(name, format, result);
    enforce_budget(name, args, projected, MAX_RESULT_CHARS)
}

/// `notify` receives server→client notifications to write BEFORE the
/// response (progress while a waited scan runs).
fn handle_request(
    api: &Api,
    req: &JsonRpcRequest,
    notify: &mut dyn FnMut(Value),
) -> Option<Value> {
    let id = req.id.clone()?; // Notifications (no id) get no response.
    // Protocol violations are JSON-RPC errors, not tool errors: a host
    // that speaks 1.0 or hands us positional params must learn it from the
    // error code (-32600 / -32602), not from a "tool errored" content block.
    if req.jsonrpc.as_deref() != Some("2.0") {
        return Some(error_response(
            id,
            -32600,
            &format!("invalid request: jsonrpc must be \"2.0\" (got {:?})", req.jsonrpc),
        ));
    }
    if !req.params.is_null() && !req.params.is_object() {
        return Some(error_response(
            id,
            -32602,
            "invalid params: params must be a JSON object (by-name), not an array",
        ));
    }
    let resp = match req.method.as_str() {
        // Echo a supported protocol revision (the client's, when we speak
        // it; else our latest). The response shape is the 2024-11-05 one
        // plus additive fields, so an older host reads it fine.
        "initialize" => response(
            id,
            json!({
                "protocolVersion": negotiate_protocol_version(req.params["protocolVersion"].as_str()),
                "capabilities": { "tools": {} },
                "serverInfo": {
                    "name": "phantom-mcp",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }),
        ),
        "tools/list" => response(id, json!({ "tools": tool_definitions() })),
        "tools/call" => {
            let Some(name) = req.params["name"].as_str() else {
                return Some(error_response(id, -32602, "invalid params: tools/call needs a string \"name\""));
            };
            let args = req.params.get("arguments").cloned().unwrap_or(json!({}));
            if !args.is_object() {
                return Some(error_response(id, -32602, "invalid params: \"arguments\" must be a JSON object"));
            }
            let token = progress::token_from(&req.params);
            match call_tool(api, name, &args, token.as_ref(), notify) {
                Ok((result, text)) => response(
                    id,
                    json!({
                        // `content` is what every host reads; the text is the
                        // API body (or its concise projection) pretty-printed.
                        // `structuredContent` is the same value as JSON for
                        // 2025-06-18 hosts — bare-array bodies wrap (see
                        // protocol::structured_content), text does not.
                        "content": [{ "type": "text", "text": text }],
                        "structuredContent": structured_content(name, &result)
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
        // Notifications go out on the same stdout, one line each, ahead of
        // the response. A closed pipe mid-notification is remembered and
        // ends the loop after this request like any other write failure.
        let mut pipe_closed = false;
        let mut notify = |v: Value| {
            let out = serde_json::to_string(&v).unwrap();
            if writeln!(stdout, "{out}").and_then(|_| stdout.flush()).is_err() {
                pipe_closed = true;
            }
        };
        let reply = match serde_json::from_str::<JsonRpcRequest>(&line) {
            Ok(req) => handle_request(&api, &req, &mut notify),
            Err(e) => Some(error_response(
                Value::Null,
                -32700,
                &format!("parse error: {e}"),
            )),
        };
        if pipe_closed {
            break;
        }
        if let Some(reply) = reply {
            let out = serde_json::to_string(&reply).unwrap();
            if writeln!(stdout, "{out}").and_then(|_| stdout.flush()).is_err() {
                break;
            }
        }
    }
}
