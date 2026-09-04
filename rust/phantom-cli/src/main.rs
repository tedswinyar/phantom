// phantom — CLI client. Talks HTTP to phantom-api; never opens the
// database directly. Designed for scripting: --json on every read, stable
// exit codes, all diagnostics on stderr.
//
// Exit codes:
//   0  success
//   1  server rejected the request (4xx/5xx other than 404)
//   2  usage error (clap)
//   3  not found
//   4  cannot reach the API server
//
// All sizes shown to humans go through phantom_core::format::format_size,
// and every size IS diskSize (st_blocks × 512) — logicalSize appears only
// as an explicitly-labelled secondary figure.

use clap::{Parser, Subcommand};
use phantom_core::{
    FileTypeTotal, HotspotsSummary, Scan, ScanDiff, ScanEntry, ScanStatus,
    format::format_size,
};
use uuid::Uuid;

const EXIT_SERVER_ERROR: i32 = 1;
const EXIT_NOT_FOUND: i32 = 3;
const EXIT_NO_CONNECTION: i32 = 4;

/// How often `phantom scan` (and `scans show --wait`, if it ever grows one)
/// polls a running scan for progress.
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(150);

#[derive(Parser)]
#[command(
    name = "phantom",
    version,
    about = "Phantom command-line client",
    long_about = "Phantom command-line client.\n\n\
        Sizes are DECIMAL (1 GB = 1000 MB), matching Finder and the Phantom \
        app. `du -h` and `df -h` print BINARY (1 GiB = 1024 MiB), so their \
        numbers run ~7% smaller for the same bytes — that gap is the unit \
        convention, not a disagreement about the data. Use --json for exact \
        raw byte counts. All sizes are hardlink-deduped (an inode counts \
        once, like `du`)."
)]
struct Cli {
    /// API base URL (default: PHANTOM_API_URL or http://127.0.0.1:8768)
    #[arg(long, global = true)]
    api_url: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Scan a directory tree and report its disk usage
    Scan {
        /// Directory to scan
        path: String,
        /// Return immediately with the running scan instead of waiting
        #[arg(long)]
        no_wait: bool,
        #[arg(long)]
        json: bool,
    },
    /// Manage recorded scans
    Scans {
        #[command(subcommand)]
        command: ScansCommand,
    },
    /// Largest files of a scan (disk size, descending)
    Top {
        /// Scan id (default: the most recent completed scan)
        #[arg(long)]
        scan: Option<Uuid>,
        /// Maximum files to return (server caps this)
        #[arg(long)]
        limit: Option<u32>,
        /// Only files of this type (extension, any case)
        #[arg(long = "type")]
        file_type: Option<String>,
        /// Only files whose path contains this substring
        #[arg(long)]
        search: Option<String>,
        /// Continuation token from a previous page's "more files" hint
        #[arg(long)]
        cursor: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Directory tree of a scan, with aggregated sizes
    Tree {
        /// Scan id (default: the most recent completed scan)
        #[arg(long)]
        scan: Option<Uuid>,
        /// Directory to start from (default: the scan root)
        #[arg(long)]
        path: Option<String>,
        /// How many levels to descend (1 = direct children)
        #[arg(long, default_value_t = 2)]
        depth: usize,
        #[arg(long)]
        json: bool,
    },
    /// Disk usage by file type (computed from the full walk)
    Types {
        /// Scan id (default: the most recent completed scan)
        #[arg(long)]
        scan: Option<Uuid>,
        #[arg(long)]
        json: bool,
    },
    /// Reclaimable-space hotspots of a scan (classified at scan completion).
    /// Phantom never deletes — every entry is a suggestion with a safe tool.
    Hotspots {
        /// Scan id (default: the most recent completed scan)
        #[arg(long)]
        scan: Option<Uuid>,
        #[arg(long)]
        json: bool,
    },
    /// Compare two completed scans of the same root: what grew, what was
    /// freed. Deltas read B − A, so pass the older scan first. With no ids,
    /// diffs the two most recent completed scans of the most recently
    /// scanned root — "what changed since last time?" is zero arguments
    Diff {
        /// The "before" scan id
        #[arg(requires = "scan_b")]
        scan_a: Option<Uuid>,
        /// The "after" scan id
        #[arg(requires = "scan_a")]
        scan_b: Option<Uuid>,
        #[arg(long)]
        json: bool,
    },
    /// Check that the API server is reachable and healthy
    Health,
}

#[derive(Subcommand)]
enum ScansCommand {
    /// List all scans, newest first (running scans included)
    List {
        #[arg(long)]
        json: bool,
    },
    /// Show one scan by id (live progress while it runs)
    Show {
        id: Uuid,
        #[arg(long)]
        json: bool,
    },
    /// Cancel a running scan (partial results are discarded)
    Cancel {
        id: Uuid,
        #[arg(long)]
        json: bool,
    },
    /// Delete a scan and all its recorded entries
    Delete { id: Uuid },
}


struct Client {
    base: String,
    key: String,
    http: reqwest::blocking::Client,
}

impl Client {
    fn new(api_url: Option<String>) -> Self {
        let base = api_url
            .or_else(|| std::env::var("PHANTOM_API_URL").ok())
            .unwrap_or_else(|| "http://127.0.0.1:8768".into());
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
            // A hung API must not stall a script/agent indefinitely (rust M4).
            http: reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .connect_timeout(std::time::Duration::from_secs(5))
                .build()
                .unwrap_or_else(|_| reqwest::blocking::Client::new()),
        }
    }

    fn dispatch(&self, req: reqwest::blocking::RequestBuilder) -> reqwest::blocking::Response {
        req.send().unwrap_or_else(|e| {
            eprintln!("phantom: cannot reach API at {}: {e}", self.base);
            eprintln!("phantom: is the server running? (make start)");
            std::process::exit(EXIT_NO_CONNECTION);
        })
    }

    /// Turn a response into JSON, or exit with a controlled diagnostic.
    /// Falls back to `status line + raw body` when the body is NOT JSON, so a
    /// text/plain error from the API (or any proxy in front of it) is never
    /// swallowed into a misleading "invalid JSON" (agentapi C1).
    fn handle_response(&self, resp: reqwest::blocking::Response) -> serde_json::Value {
        let status = resp.status();
        let text = resp.text().unwrap_or_else(|e| {
            eprintln!("phantom: cannot read response body from {}: {e}", self.base);
            std::process::exit(EXIT_SERVER_ERROR);
        });
        let exit_for = |status: reqwest::StatusCode| {
            if status == reqwest::StatusCode::NOT_FOUND {
                EXIT_NOT_FOUND
            } else {
                EXIT_SERVER_ERROR
            }
        };
        match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(value) => {
                if !status.is_success() {
                    let msg = value["error"].as_str().unwrap_or("unknown error");
                    eprintln!("phantom: {} ({})", msg, status.as_u16());
                    std::process::exit(exit_for(status));
                }
                value
            }
            Err(_) => {
                let body = text.trim();
                if !status.is_success() {
                    let shown = if body.is_empty() { "<empty response body>" } else { body };
                    eprintln!("phantom: {shown} ({})", status.as_u16());
                    std::process::exit(exit_for(status));
                }
                eprintln!("phantom: invalid JSON from server: {body}");
                std::process::exit(EXIT_SERVER_ERROR);
            }
        }
    }

    fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> serde_json::Value {
        let mut req = self
            .http
            .request(method, format!("{}{path}", self.base))
            .header("x-api-key", &self.key);
        if let Some(body) = body {
            req = req.json(&body);
        }
        let resp = self.dispatch(req);
        self.handle_response(resp)
    }

    /// A request whose success answer is `204 No Content` (DELETE). Any
    /// non-204 routes through the normal error handling.
    fn request_no_content(&self, method: reqwest::Method, path: &str) {
        let req = self
            .http
            .request(method, format!("{}{path}", self.base))
            .header("x-api-key", &self.key);
        let resp = self.dispatch(req);
        if resp.status() == reqwest::StatusCode::NO_CONTENT {
            return;
        }
        // Not the 204 contract: let the shared handler surface the {error}.
        self.handle_response(resp);
        eprintln!("phantom: expected 204 No Content from DELETE {path}");
        std::process::exit(EXIT_SERVER_ERROR);
    }

    /// GET a paginated listing: the bare-array body plus the continuation
    /// cursor from the `X-Next-Cursor` header when more rows remain.
    fn get_page(&self, path: &str) -> (serde_json::Value, Option<String>) {
        let req = self
            .http
            .get(format!("{}{path}", self.base))
            .header("x-api-key", &self.key);
        let resp = self.dispatch(req);
        let next = resp
            .headers()
            .get("x-next-cursor")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        (self.handle_response(resp), next)
    }

    /// The scan every result command defaults to: the most recent COMPLETED
    /// scan (a running or cancelled one has no readable results).
    fn latest_complete_scan_id(&self) -> String {
        let scans = self.request(reqwest::Method::GET, "/scans", None);
        scans
            .as_array()
            .and_then(|list| {
                list.iter()
                    .find(|s| s["status"] == "complete")
                    .and_then(|s| s["id"].as_str())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| {
                eprintln!("phantom: no completed scans; run `phantom scan <path>` first");
                std::process::exit(EXIT_NOT_FOUND);
            })
    }

    /// The zero-argument diff pair: the newest COMPLETE scan plus the
    /// next-older complete scan of the SAME root. Returns (older, newer) so
    /// the deltas read "since last time". /scans is newest-first.
    fn latest_diff_pair(&self) -> (String, String) {
        let scans = self.request(reqwest::Method::GET, "/scans", None);
        let empty = Vec::new();
        let completes: Vec<&serde_json::Value> = scans
            .as_array()
            .unwrap_or(&empty)
            .iter()
            .filter(|s| s["status"] == "complete")
            .collect();
        let Some(newest) = completes.first() else {
            eprintln!("phantom: no completed scans; run `phantom scan <path>` first");
            std::process::exit(EXIT_NOT_FOUND);
        };
        let root = &newest["rootPath"];
        let Some(older) = completes.iter().skip(1).find(|s| &s["rootPath"] == root) else {
            eprintln!(
                "phantom: only one completed scan of {root}; scan it again (or pass two scan ids)"
            );
            std::process::exit(EXIT_NOT_FOUND);
        };
        (
            older["id"].as_str().unwrap_or_default().to_string(),
            newest["id"].as_str().unwrap_or_default().to_string(),
        )
    }

    /// Poll a scan until it reaches a terminal status, optionally rendering
    /// live progress on stderr. Returns the terminal wire view.
    fn wait_for_scan(&self, id: &str, show_progress: bool) -> serde_json::Value {
        loop {
            let v = self.request(reqwest::Method::GET, &format!("/scans/{id}"), None);
            if v["status"] != "running" {
                if show_progress {
                    eprintln!(); // end the \r progress line
                }
                return v;
            }
            if show_progress {
                let files = v["progress"]["filesSeen"].as_u64().unwrap_or(0);
                let bytes = v["progress"]["bytesSeen"].as_u64().unwrap_or(0);
                let current = v["progress"]["currentPath"].as_str().unwrap_or("");
                let shown: String = current.chars().take(60).collect();
                // Trailing spaces wipe leftovers from a longer previous line.
                eprint!("\rscanning: {files} files, {}  {shown:<60}", format_size(bytes));
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }
}

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

/// Decode a wire value through the shared core type, or exit cleanly on
/// version/wire-format skew (e.g. a required field the server added that
/// this build doesn't know). A handled `EXIT_SERVER_ERROR` with a diagnostic
/// beats a panic backtrace.
fn decode<T: serde::de::DeserializeOwned>(value: serde_json::Value, what: &str) -> T {
    serde_json::from_value(value).unwrap_or_else(|e| {
        eprintln!(
            "phantom: server returned a {what} this client cannot parse \
             (wire-format skew? upgrade the client): {e}"
        );
        std::process::exit(EXIT_SERVER_ERROR);
    })
}

fn emit_json(value: &serde_json::Value) {
    println!("{}", serde_json::to_string_pretty(value).unwrap());
}

// --- Scan rendering ----------------------------------------------------------

fn print_scan_line(s: &Scan) {
    println!(
        "{}  [{}]  {}  {}  ({})",
        s.id,
        s.status.as_str(),
        format_size(s.total_disk_size),
        s.root_path,
        s.started_at.format("%Y-%m-%d %H:%M")
    );
}

fn print_scan_block(value: &serde_json::Value) {
    let s: Scan = decode(value.clone(), "scan");
    print_scan_line(&s);
    if s.status == ScanStatus::Running {
        let files = value["progress"]["filesSeen"].as_u64().unwrap_or(0);
        let bytes = value["progress"]["bytesSeen"].as_u64().unwrap_or(0);
        println!("  progress: {files} files, {} so far", format_size(bytes));
        return;
    }
    println!(
        "  disk: {} (logical {})",
        format_size(s.total_disk_size),
        format_size(s.total_logical_size)
    );
    println!(
        "  files: {}  dirs: {}  errors: {}",
        s.file_count, s.dir_count, s.error_count
    );
    // phantom-671: cluster the sampled unreadable paths by parent so
    // "3724 errors, mostly under one build dir" is readable at a glance.
    // The sample is capped; errorCount stays the truth.
    if let Some(sample) = &s.unreadable_paths {
        if !sample.is_empty() {
            let mut by_parent: std::collections::HashMap<&str, u64> = std::collections::HashMap::new();
            for u in sample {
                let parent = std::path::Path::new(&u.path)
                    .parent()
                    .and_then(|p| p.to_str())
                    .unwrap_or("(unknown)");
                *by_parent.entry(parent).or_insert(0) += 1;
            }
            let mut clusters: Vec<(&str, u64)> = by_parent.into_iter().collect();
            clusters.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
            println!(
                "  unreadable (sample of {} of {} errors), clustered by parent:",
                sample.len(),
                s.error_count
            );
            for (parent, n) in clusters.iter().take(5) {
                println!("    {n:>4}  {parent}");
            }
            if clusters.len() > 5 {
                println!("    …and {} more parents (--json for the full sample)", clusters.len() - 5);
            }
            println!("    first: {}  ({})", sample[0].path, sample[0].reason);
        }
    }
    if let Some(finished) = s.finished_at {
        println!("  finished: {}", finished.format("%Y-%m-%d %H:%M:%S"));
    }
}

fn print_entry_line(e: &ScanEntry, indent: usize) {
    let name = if e.is_dir { format!("{}/", e.name) } else { e.name.clone() };
    // Full-walk descendant counts; dir rows from pre-v3 scans carry none.
    let counts = match (e.file_count, e.dir_count) {
        (Some(files), Some(dirs)) => format!("  ({files} files, {dirs} dirs)"),
        _ => String::new(),
    };
    println!(
        "{:indent$}{}  {}{}",
        "",
        format_size(e.disk_size),
        name,
        counts,
        indent = indent * 2
    );
}

/// Fetch the tree under `path` (server default: the scan root) down to
/// `depth` levels, depth-first. Each level is one `/tree` call; children
/// arrive path-ordered from the store.
fn fetch_tree(
    client: &Client,
    scan_id: &str,
    path: Option<&str>,
    depth: usize,
    level: usize,
    out: &mut Vec<(usize, serde_json::Value)>,
) {
    if depth == 0 {
        return;
    }
    let query = query_string(&[("path", path.map(str::to_string))]);
    let v = client.request(
        reqwest::Method::GET,
        &format!("/scans/{scan_id}/tree{query}"),
        None,
    );
    for child in v.as_array().cloned().unwrap_or_default() {
        let is_dir = child["isDir"].as_bool().unwrap_or(false);
        let child_path = child["path"].as_str().map(str::to_string);
        out.push((level, child));
        if is_dir && depth > 1 {
            fetch_tree(client, scan_id, child_path.as_deref(), depth - 1, level + 1, out);
        }
    }
}


/// A size delta with its sign: `+1.5 GB` / `-206.7 MB` (format_size is
/// unsigned; the sign carries the direction).
fn format_signed(delta: i64) -> String {
    if delta < 0 {
        format!("-{}", format_size(delta.unsigned_abs()))
    } else {
        format!("+{}", format_size(delta as u64))
    }
}

fn main() {
    let cli = Cli::parse();
    let client = Client::new(cli.api_url);

    match cli.command {
        Command::Health => {
            let v = client.request(reqwest::Method::GET, "/health", None);
            emit_json(&v);
        }
        Command::Diff { scan_a, scan_b, json } => {
            let (a, b) = match (scan_a, scan_b) {
                (Some(a), Some(b)) => (a.to_string(), b.to_string()),
                // clap's mutual `requires` guarantees both-or-neither.
                _ => client.latest_diff_pair(),
            };
            let v = client.request(reqwest::Method::GET, &format!("/scans/{a}/diff/{b}"), None);
            if json {
                emit_json(&v);
                return;
            }
            let d: ScanDiff = decode(v, "scan diff");
            println!("diff {}  ({} -> {})", d.root_path, d.scan_a, d.scan_b);
            if d.reversed_chronology == Some(true) {
                // The sign-inversion trap: scanA is the NEWER scan, so every
                // delta reads reverse-chronologically (a directory that grew
                // over time shows as freed here). Say so loudly.
                eprintln!(
                    "phantom: note — scanA is NEWER than scanB, so these deltas are \
                     reverse-chronological (grown/freed are swapped vs 'what changed since \
                     the older scan'). Pass the older scan first to read them forward."
                );
            }
            println!(
                "  disk: {}  (logical {})",
                format_signed(d.disk_delta),
                format_signed(d.logical_delta)
            );
            println!(
                "  files: {:+}  dirs: {:+}  errors: {:+}",
                d.file_count_delta, d.dir_count_delta, d.error_count_delta
            );
            if d.grown.is_empty() && d.freed.is_empty() {
                println!("  no directory moved by 1 MB or more");
            }
            if !d.grown.is_empty() {
                println!("  grown:");
                for e in &d.grown {
                    let marker = if e.before.is_none() { "  (new)" } else { "" };
                    println!("    {}  {}{marker}", format_signed(e.delta), e.path);
                }
            }
            if !d.freed.is_empty() {
                println!("  freed:");
                for e in &d.freed {
                    let marker = if e.after.is_none() { "  (gone)" } else { "" };
                    println!("    {}  {}{marker}", format_signed(e.delta), e.path);
                }
            }
        }
        Command::Scan { path, no_wait, json } => {
            let started = client.request(
                reqwest::Method::POST,
                "/scans",
                Some(serde_json::json!({ "rootPath": path })),
            );
            let id = started["id"].as_str().unwrap_or_default().to_string();
            if no_wait {
                if json {
                    emit_json(&started);
                } else {
                    print_scan_block(&started);
                    eprintln!("phantom: follow it with `phantom scans show {id}`");
                }
                return;
            }
            let done = client.wait_for_scan(&id, !json);
            if json {
                emit_json(&done);
            } else {
                print_scan_block(&done);
            }
            if done["status"] != "complete" {
                // A scan that ended cancelled/failed is a failed command —
                // in both output modes, so scripts can trust the exit code.
                std::process::exit(EXIT_SERVER_ERROR);
            }
        }
        Command::Scans { command } => match command {
            ScansCommand::List { json } => {
                let v = client.request(reqwest::Method::GET, "/scans", None);
                if json {
                    emit_json(&v);
                    return;
                }
                let items = v.as_array().cloned().unwrap_or_default();
                if items.is_empty() {
                    println!("no scans");
                }
                for item in items {
                    print_scan_line(&decode(item, "scan"));
                }
            }
            ScansCommand::Show { id, json } => {
                let v = client.request(reqwest::Method::GET, &format!("/scans/{id}"), None);
                if json {
                    emit_json(&v);
                } else {
                    print_scan_block(&v);
                }
            }
            ScansCommand::Cancel { id, json } => {
                let v = client.request(
                    reqwest::Method::POST,
                    &format!("/scans/{id}/cancel"),
                    None,
                );
                if json {
                    emit_json(&v);
                } else {
                    print_scan_block(&v);
                    eprintln!("phantom: cancel requested; poll `phantom scans show {id}`");
                }
            }
            ScansCommand::Delete { id } => {
                client.request_no_content(reqwest::Method::DELETE, &format!("/scans/{id}"));
                println!("deleted {id}");
            }
        },
        Command::Top {
            scan,
            limit,
            file_type,
            search,
            cursor,
            json,
        } => {
            let id = scan
                .map(|u| u.to_string())
                .unwrap_or_else(|| client.latest_complete_scan_id());
            let filtered = file_type.is_some() || search.is_some();
            let query = query_string(&[
                ("limit", limit.map(|l| l.to_string())),
                ("fileType", file_type),
                ("search", search),
                ("cursor", cursor),
            ]);
            let (v, next) = client.get_page(&format!("/scans/{id}/files{query}"));
            if json {
                emit_json(&v);
            } else {
                let items = v.as_array().cloned().unwrap_or_default();
                if items.is_empty() {
                    // An empty FILTER result and an empty scan read very
                    // differently — say which one happened (phantom-auw).
                    if filtered {
                        println!(
                            "no recorded files match the filter (files under 1 MiB are not kept individually)"
                        );
                    } else {
                        println!("no files recorded (files under 1 MiB are not kept individually)");
                    }
                }
                for item in items {
                    let e: ScanEntry = decode(item, "entry");
                    println!("{}  {}", format_size(e.disk_size), e.path);
                }
            }
            if let Some(next) = next {
                eprintln!("phantom: more files available; pass --cursor {next} to continue");
            }
        }
        Command::Tree {
            scan,
            path,
            depth,
            json,
        } => {
            let id = scan
                .map(|u| u.to_string())
                .unwrap_or_else(|| client.latest_complete_scan_id());
            let mut out = Vec::new();
            fetch_tree(&client, &id, path.as_deref(), depth, 0, &mut out);
            if json {
                // Flattened across levels, path-sorted (the documented order;
                // DFS order is not path order for punctuation-heavy names).
                let mut values: Vec<serde_json::Value> =
                    out.into_iter().map(|(_, v)| v).collect();
                values.sort_by(|a, b| {
                    a["path"].as_str().unwrap_or("").cmp(b["path"].as_str().unwrap_or(""))
                });
                emit_json(&serde_json::Value::Array(values));
            } else {
                if out.is_empty() {
                    println!("no entries");
                }
                for (level, item) in out {
                    print_entry_line(&decode(item, "entry"), level);
                }
            }
        }
        Command::Types { scan, json } => {
            let id = scan
                .map(|u| u.to_string())
                .unwrap_or_else(|| client.latest_complete_scan_id());
            let v = client.request(
                reqwest::Method::GET,
                &format!("/scans/{id}/types"),
                None,
            );
            if json {
                emit_json(&v);
                return;
            }
            let items = v.as_array().cloned().unwrap_or_default();
            if items.is_empty() {
                println!("no file types recorded");
            }
            for item in items {
                let t: FileTypeTotal = decode(item, "file-type total");
                let label = match t.file_type.as_deref() {
                    Some(ext) => format!(".{ext}"),
                    None => "(no extension)".into(),
                };
                println!(
                    "{}  {} files  {label}",
                    format_size(t.disk_size),
                    t.file_count
                );
            }
        }
        Command::Hotspots { scan, json } => {
            let id = scan
                .map(|u| u.to_string())
                .unwrap_or_else(|| client.latest_complete_scan_id());
            let v = client.request(
                reqwest::Method::GET,
                &format!("/scans/{id}/hotspots"),
                None,
            );
            if json {
                emit_json(&v);
                return;
            }
            // Decode through the shared type so the CLI can never silently
            // render a shape the other clients would fail to parse.
            let s: HotspotsSummary = decode(v, "hotspots summary");
            if s.groups.is_empty() {
                println!("no hotspots found");
            }
            for g in &s.groups {
                println!(
                    "{}  [{}]  {}",
                    format_size(g.disk_size),
                    g.category.as_str(),
                    g.label
                );
                // Surface the hardlink gap only when it is material (≥ 1%):
                // a group whose links share a few stray blocks would print
                // "listed 1.1 GB … frees 1.1 GB", which reads as a glitch.
                if g.listed_disk_size > g.disk_size + g.disk_size / 100 {
                    println!(
                        "  listed {} across {} files; hardlinks share blocks, deleting frees {}",
                        format_size(g.listed_disk_size),
                        g.file_count,
                        format_size(g.disk_size)
                    );
                }
                println!("  hint: {}", g.hint);
                for path in &g.top_paths {
                    println!("  {path}");
                }
            }
            let reclaimable_groups = s
                .groups
                .iter()
                .filter(|g| g.category.is_reclaimable())
                .count();
            println!(
                "reclaim estimate: {} across the {reclaimable_groups} reclaimable groups above  (review first adds: {})",
                format_size(s.reclaim_estimate),
                format_size(s.review_disk_size)
            );
            if s.cloud_dataloaded_logical_size > 0 {
                println!(
                    "cloud placeholders claim {} but occupy {} on disk",
                    format_size(s.cloud_dataloaded_logical_size),
                    format_size(s.cloud_dataloaded_disk_size)
                );
            }
        }
    }
}
