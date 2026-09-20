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
    FileTypeTotal, HotspotsSummary, PathExplanation, RebuildKind, ReclaimPlan, ReclaimVerification,
    Growth, RiskTier, Scan, ScanDiff, ScanEntry, ScanStatus, StaleProjects, VolumeStatus, format::format_size,
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
        raw byte counts. All sizes are deduped (a hardlinked inode or an APFS \
        clone group counts once, like `du` with clone awareness); 'deleting \
        frees' lines show what removal would actually return."
)]
struct Cli {
    /// API base URL (default: PHANTOM_API_URL, else the URL the running API
    /// published to ~/Library/Application Support/phantom/api_url, else
    /// http://127.0.0.1:18770)
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
        /// Descend into other volumes mounted below the root (default: one
        /// filesystem, like `du -x` — mount points are recorded, not walked)
        #[arg(long)]
        cross_volumes: bool,
        /// Staleness threshold for the classifier: a project whose newest
        /// source edit and git activity are at least this old is dormant
        /// (90d, 12w, 3M, 1y, or bare days; default 90d)
        #[arg(long, value_name = "AGE")]
        older: Option<String>,
        /// Opt-in: run each project's read-only lockfile check (cargo
        /// metadata --locked, npm ci --dry-run, uv lock --locked) from fixed
        /// install paths, inside the project dir. Lowers a tier on failure;
        /// writes nothing. Do not use on checkouts you do not trust.
        #[arg(long)]
        verify_locks: bool,
        /// Opt-in: attach the owning tool's own dry-run number (docker
        /// system df, brew cleanup -n, uv cache size) to matching hotspots
        #[arg(long)]
        tool_estimates: bool,
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
        #[arg(requires = "scan_b", conflicts_with = "since")]
        scan_a: Option<Uuid>,
        /// The "after" scan id
        #[arg(requires = "scan_a", conflicts_with = "since")]
        scan_b: Option<Uuid>,
        /// Compare the newest completed scan against a baseline: a scan id,
        /// or a duration (7d, 2w, 3M, 1y, bare days) picking the newest
        /// scan of the same root at least that old. Exit 3 when none is
        #[arg(long, value_name = "DURATION|SCAN_ID")]
        since: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Build a reclaim PLAN from a completed scan's hotspots: the dry-run to
    /// confirm before anything moves. Items are safe groups by default
    /// (--max-tier caution admits caution groups; review never qualifies).
    /// Phantom never deletes: --script prints a shell script that MOVES the
    /// paths to the Trash when run with PHANTOM_APPLY=1, and only prints
    /// otherwise
    Plan {
        /// Scan id (default: the most recent completed scan)
        #[arg(long)]
        scan: Option<Uuid>,
        /// Highest risk tier to include: safe (default) or caution
        #[arg(long, value_parser = ["safe", "caution"])]
        max_tier: Option<String>,
        /// Skip groups that would free fewer bytes than this
        #[arg(long)]
        min_bytes: Option<u64>,
        /// Print the plan as a runnable (dry-run by default) shell script
        #[arg(long, conflicts_with = "json")]
        script: bool,
        #[arg(long)]
        json: bool,
    },
    /// Verify a plan: rescan its root (or use --after <scanId>) and report
    /// what actually came back versus what the plan promised. Exits 1 when
    /// the result is outside the 5% tolerance
    Verify {
        /// The plan id (from `phantom plan`)
        plan_id: Uuid,
        /// A completed rescan of the plan's root to compare against
        /// (default: scan it now and wait)
        #[arg(long)]
        after: Option<Uuid>,
        #[arg(long)]
        json: bool,
    },
    /// Explain one path of a scan: its three sizes, what deleting it frees,
    /// clone/hardlink/placeholder facts, the hotspot group that classified
    /// it, and unreadable subtrees below it — in one sentence and as JSON
    Explain {
        /// The path, exactly as the scan recorded it
        path: String,
        /// Scan id (default: the most recent completed scan)
        #[arg(long)]
        scan: Option<Uuid>,
        #[arg(long)]
        json: bool,
    },
    /// Projects whose newest source edit and git activity are older than a
    /// threshold, with the build artifacts inside them — re-thresholded from
    /// the scan's record, no re-walk
    Stale {
        /// Scan id (default: the most recent completed scan)
        #[arg(long)]
        scan: Option<Uuid>,
        /// Threshold: 90d, 12w, 3M, 1y, or bare days (default 90d)
        #[arg(long)]
        older: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// The volume's real numbers: total, used, free, available, purgeable
    /// (statfs + getattrlist + CoreFoundation on the data volume — `df /`
    /// reports the sealed system snapshot on APFS), and with --scan the
    /// hidden-space split: what the scan did not see, other volumes in the
    /// container, other users' homes
    Volume {
        /// Any path on the volume of interest (default: /System/Volumes/Data)
        path: Option<String>,
        /// Also list local Time Machine snapshots (runs /usr/bin/tmutil)
        #[arg(long)]
        snapshots: bool,
        /// A completed scan on this volume: reports used − scanned
        #[arg(long)]
        scan: Option<Uuid>,
        #[arg(long)]
        json: bool,
    },
    /// How a root grew across its scans, with a linear "disk full in N days"
    /// forecast and the caveat that travels with it. No re-walk: points are
    /// the persisted scans, oldest first
    Growth {
        /// The scanned root (default: the most recent completed scan's root)
        root: Option<String>,
        /// What each series line is keyed by
        #[arg(long, value_enum, default_value_t = GroupByArg::Total)]
        group_by: GroupByArg,
        #[arg(long)]
        json: bool,
    },
    /// Check that the API server is reachable and healthy
    Health,
    /// Print a shell completion script (bash, zsh, fish, elvish, powershell)
    /// to stdout. The app bundle ships these under
    /// Contents/Resources/completions; the Homebrew cask installs them.
    Completions {
        shell: clap_complete::Shell,
    },
    /// Print the man page (roff) to stdout. Shipped as
    /// Contents/Resources/man/man1/phantom.1.
    Man,
}

/// `--group-by` values, kebab-case on the command line, camelCase on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum GroupByArg {
    Total,
    Category,
    TopLevelDir,
    Extension,
}

impl GroupByArg {
    fn wire(self) -> &'static str {
        match self {
            GroupByArg::Total => "total",
            GroupByArg::Category => "category",
            GroupByArg::TopLevelDir => "topLevelDir",
            GroupByArg::Extension => "extension",
        }
    }
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
    /// Where `base` came from, for the cannot-reach diagnostic.
    base_source: String,
    key: String,
    http: reqwest::blocking::Client,
}

impl Client {
    fn new(api_url: Option<String>) -> Self {
        // --api-url / PHANTOM_API_URL, else the URL the running API published
        // beside its key file, else the registered port (phantom_core::
        // discovery — the day another vendor's agent sat on 8768 the default would
        // have been it, not Phantom).
        let override_url = api_url.or_else(|| std::env::var("PHANTOM_API_URL").ok());
        let url_file = phantom_core::discovery::published_url_file();
        let (base, source) =
            phantom_core::discovery::resolve_api_url(override_url.as_deref(), url_file.as_deref());
        let base_source = source.describe(url_file.as_deref());
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
            base_source,
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
            eprintln!("phantom: cannot reach API at {} ({}): {e}", self.base, self.base_source);
            eprintln!("phantom: is the server running? (open Phantom.app, or make start)");
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

    /// A request whose success body is TEXT (the plan script), not JSON.
    /// Non-2xx still routes through the shared `{error}` handling.
    fn request_text(&self, method: reqwest::Method, path: &str) -> String {
        let req = self
            .http
            .request(method, format!("{}{path}", self.base))
            .header("x-api-key", &self.key);
        let resp = self.dispatch(req);
        if !resp.status().is_success() {
            self.handle_response(resp);
            std::process::exit(EXIT_SERVER_ERROR);
        }
        resp.text().unwrap_or_else(|e| {
            eprintln!("phantom: cannot read response body from {}: {e}", self.base);
            std::process::exit(EXIT_SERVER_ERROR);
        })
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

    /// The root of the newest COMPLETE scan — what `phantom growth` charts
    /// when no root is named.
    fn latest_complete_scan_root(&self) -> String {
        let scans = self.request(reqwest::Method::GET, "/scans", None);
        scans
            .as_array()
            .and_then(|list| {
                list.iter()
                    .find(|s| s["status"] == "complete")
                    .and_then(|s| s["rootPath"].as_str())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| {
                eprintln!("phantom: no completed scans; run `phantom scan <path>` first");
                std::process::exit(EXIT_NOT_FOUND);
            })
    }

    /// `--since`: the newest COMPLETE scan is the "after" side; the baseline
    /// is the named scan id, or the newest scan of the same root at least
    /// that many days older (phantom_core::diff::resolve_since — the same
    /// resolution the app uses). A malformed spec is a usage error (2); no
    /// scan old enough is not-found (3) with the oldest one named.
    fn since_diff_pair(&self, since: &str) -> (String, String) {
        let spec = match phantom_core::diff::parse_since(since) {
            Ok(spec) => spec,
            Err(e) => {
                eprintln!("phantom: {e}");
                std::process::exit(2);
            }
        };
        let scans: Vec<Scan> = decode(self.request(reqwest::Method::GET, "/scans", None), "scan list");
        let Some(target) = scans.iter().find(|s| s.status == ScanStatus::Complete) else {
            eprintln!("phantom: no completed scans; run `phantom scan <path>` first");
            std::process::exit(EXIT_NOT_FOUND);
        };
        match phantom_core::diff::resolve_since(&scans, target, spec) {
            Ok(a) => (a.to_string(), target.id.to_string()),
            Err(phantom_core::CoreError::InvalidInput(m)) => {
                eprintln!("phantom: {m}");
                std::process::exit(2);
            }
            Err(e) => {
                eprintln!("phantom: {e}");
                std::process::exit(EXIT_NOT_FOUND);
            }
        }
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

/// The human view's tier badge: fixed-width, upper-case, unmistakable.
fn tier_badge(tier: RiskTier) -> &'static str {
    match tier {
        RiskTier::Safe => "SAFE   ",
        RiskTier::Caution => "CAUTION",
        RiskTier::Review => "REVIEW ",
    }
}

fn emit_json(value: &serde_json::Value) {
    println!("{}", serde_json::to_string_pretty(value).unwrap());
}

// --- Plan rendering ----------------------------------------------------------

fn print_plan(plan: &ReclaimPlan) {
    println!(
        "plan {}  ({} item(s), expected to free {}, tier ≤ {})",
        plan.plan_id,
        plan.item_count,
        format_size(plan.expected_freed_bytes),
        plan.max_tier.as_str()
    );
    println!("  from scan {} of {}", plan.scan_id, plan.root_path);
    if plan.items.is_empty() {
        println!("  nothing to reclaim at this tier");
    }
    for (n, item) in plan.items.iter().enumerate() {
        println!(
            "  {}. {}  [{}]  frees {}",
            n + 1,
            item.label,
            item.risk_tier.as_str(),
            format_size(item.expected_freed_bytes)
        );
        println!("     why: {}", item.why);
        if let Some(cmd) = &item.command {
            println!("     safe tool: {cmd}");
        }
        for p in &item.paths {
            println!("     {p}");
        }
    }
    let sk = plan.skipped;
    if sk.review + sk.above_tier + sk.below_min_bytes + sk.tracked > 0 {
        println!(
            "  skipped: {} review-only, {} above tier, {} below --min-bytes, {} tracked by git (inside a work tree, not ignored)",
            sk.review, sk.above_tier, sk.below_min_bytes, sk.tracked
        );
    }
    println!(
        "  next: `phantom plan --scan {} --script > plan.sh` (dry run), then PHANTOM_APPLY=1 sh plan.sh, then `phantom verify {}`",
        plan.scan_id, plan.plan_id
    );
}

fn print_verification(v: &ReclaimVerification) {
    // The headline is the API's actualFreedBytes: the root delta, or Σ of the
    // items' actuals when the plan's Trash folder sits inside the root
    // (phantom-grw). The sign convention is spelled out — the dogfood's
    // "actual +55.2 GB" for bytes FREED read as growth.
    println!(
        "verify {}  expected {}  {}  ({})",
        v.plan_id,
        format_size(v.expected_freed_bytes),
        format_freed(v.actual_freed_bytes),
        if v.within_tolerance { "within 5%" } else { "OUTSIDE 5%" }
    );
    println!("  before scan {}  after scan {}", v.before_scan_id, v.after_scan_id);
    for item in &v.items {
        match item.actual_freed_bytes {
            Some(actual) => println!(
                "  {}  expected {}  {}",
                item.label,
                format_size(item.expected_freed_bytes),
                format_freed(actual)
            ),
            None => println!(
                "  {}  expected {}  not measurable (paths were not directories in the before scan)",
                item.label,
                format_size(item.expected_freed_bytes)
            ),
        }
    }
    if v.shortfall_bytes > 0 {
        println!("  shortfall: {} less than promised", format_size(v.shortfall_bytes as u64));
    } else if v.shortfall_bytes < 0 {
        println!(
            "  {} MORE than promised came back — something outside the plan moved too",
            format_size(v.shortfall_bytes.unsigned_abs())
        );
    }
    println!("  the paths were moved to the Trash, not deleted: the space returns when the Trash is emptied");
}

/// Bytes that came back, with the direction in words: `freed 55.2 GB`, or
/// `grew 1.2 GB` when the measured tree got bigger. Never a bare `+`/`-`.
fn format_freed(delta: i64) -> String {
    if delta < 0 {
        format!("grew {}", format_size(delta.unsigned_abs()))
    } else {
        format!("freed {}", format_size(delta as u64))
    }
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
    if let Some(reason) = &s.failure_reason {
        println!("  reason: {reason}");
    }
    println!(
        "  disk: {} (logical {})",
        format_size(s.total_disk_size),
        format_size(s.total_logical_size)
    );
    // v1.1: what deleting the tree would actually free, and what other
    // links/clones/snapshots pin. Pre-v5 rows recorded neither.
    if let (Some(private), Some(shared)) = (s.total_private_size, s.total_shared_size) {
        println!(
            "  deleting frees: {}  (shared with other paths or snapshots: {})",
            format_size(private),
            format_size(shared)
        );
    }
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
    // Material sharing only (≥ 1% of the size): a directory whose links
    // share a few stray blocks would print "… 0 B shared" everywhere.
    let shared = match e.shared_size {
        Some(shared) if shared > e.disk_size / 100 => format!("  [{} shared]", format_size(shared)),
        _ => String::new(),
    };
    println!(
        "{:indent$}{}  {}{}{}",
        "",
        format_size(e.disk_size),
        name,
        counts,
        shared,
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
    // Behave like a Unix tool in a pipeline: `phantom top | head -5` must
    // exit quietly when head closes the pipe. Rust ignores SIGPIPE at
    // startup, so without this a `println!` to a closed pipe PANICS with a
    // stack trace — and, under `set -o pipefail`, turned a passing e2e grep
    // into a failure once the human view grew (2026-09-07).
    // SAFETY: setting a signal disposition before any thread exists.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let cli = Cli::parse();
    // Offline subcommands: no API, no key file, no network.
    match &cli.command {
        Command::Completions { shell } => {
            use clap::CommandFactory;
            let mut cmd = Cli::command();
            clap_complete::generate(*shell, &mut cmd, "phantom", &mut std::io::stdout());
            return;
        }
        Command::Man => {
            use clap::CommandFactory;
            use std::io::Write;
            let man = clap_mangen::Man::new(Cli::command());
            let mut out = Vec::new();
            man.render(&mut out).expect("render man page");
            std::io::stdout().write_all(&out).expect("write man page");
            return;
        }
        _ => {}
    }
    let client = Client::new(cli.api_url);

    match cli.command {
        Command::Plan {
            scan,
            max_tier,
            min_bytes,
            script,
            json,
        } => {
            let id = scan
                .map(|u| u.to_string())
                .unwrap_or_else(|| client.latest_complete_scan_id());
            let mut body = serde_json::json!({});
            if let Some(t) = max_tier {
                body["maxTier"] = serde_json::Value::String(t);
            }
            if let Some(b) = min_bytes {
                body["minBytes"] = serde_json::json!(b);
            }
            let v = client.request(reqwest::Method::POST, &format!("/scans/{id}/plan"), Some(body));
            if json {
                emit_json(&v);
                return;
            }
            let plan: ReclaimPlan = decode(v, "reclaim plan");
            if script {
                let text = client.request_text(
                    reqwest::Method::GET,
                    &format!("/plans/{}/script", plan.plan_id),
                );
                print!("{text}");
                return;
            }
            print_plan(&plan);
        }
        Command::Verify { plan_id, after, json } => {
            let after_id = match after {
                Some(id) => id.to_string(),
                None => {
                    // Rescan the plan's root now, the way `phantom scan` does.
                    let plan = client.request(reqwest::Method::GET, &format!("/plans/{plan_id}"), None);
                    let root = plan["rootPath"].as_str().unwrap_or_default().to_string();
                    if !json {
                        eprintln!("phantom: rescanning {root} to verify plan {plan_id}");
                    }
                    let started = client.request(
                        reqwest::Method::POST,
                        "/scans",
                        Some(serde_json::json!({ "rootPath": root })),
                    );
                    let id = started["id"].as_str().unwrap_or_default().to_string();
                    let done = client.wait_for_scan(&id, !json);
                    if done["status"] != "complete" {
                        eprintln!(
                            "phantom: rescan {id} ended {}; cannot verify",
                            done["status"].as_str().unwrap_or("?")
                        );
                        std::process::exit(EXIT_SERVER_ERROR);
                    }
                    id
                }
            };
            let v = client.request(
                reqwest::Method::POST,
                &format!("/plans/{plan_id}/verify"),
                Some(serde_json::json!({ "afterScanId": after_id })),
            );
            let within = v["withinTolerance"] == true;
            if json {
                emit_json(&v);
            } else {
                let verification: ReclaimVerification = decode(v, "reclaim verification");
                print_verification(&verification);
            }
            if !within {
                // Outside tolerance is a failed verification — in both output
                // modes, so scripts can trust the exit code.
                eprintln!("phantom: verification is outside the 5% tolerance");
                std::process::exit(EXIT_SERVER_ERROR);
            }
        }
        Command::Explain { path, scan, json } => {
            let id = scan
                .map(|u| u.to_string())
                .unwrap_or_else(|| client.latest_complete_scan_id());
            let v = client.request(
                reqwest::Method::GET,
                &format!("/scans/{id}/explain?path={}", encode_query_value(&path)),
                None,
            );
            if json {
                emit_json(&v);
                return;
            }
            let x: PathExplanation = decode(v, "path explanation");
            println!("{}", x.path);
            println!("  {}", x.summary);
            println!(
                "  disk {}  logical {}  frees {}  pinned {}",
                format_size(x.disk_size),
                format_size(x.logical_size),
                x.private_size.map_or("n/a".to_string(), format_size),
                x.shared_size.map_or("n/a".to_string(), format_size)
            );
            if !x.flags.is_empty() {
                println!("  flags: {}", x.flags.join(", "));
            }
            if let Some(h) = &x.hotspot {
                println!("  hotspot: {} [{}]  {}", h.label, h.risk_tier.as_str(), h.hint);
                if let Some(cmd) = &h.command {
                    println!("  safe tool: {cmd}");
                }
            }
            for u in &x.unreadable_below {
                println!("  unreadable: {}  ({})", u.path, u.reason);
            }
        }
        Command::Stale { scan, older, json } => {
            let id = scan
                .map(|u| u.to_string())
                .unwrap_or_else(|| client.latest_complete_scan_id());
            let query = older
                .map(|o| format!("?olderThan={}", encode_query_value(&o)))
                .unwrap_or_default();
            let v = client.request(reqwest::Method::GET, &format!("/scans/{id}/stale{query}"), None);
            if json {
                emit_json(&v);
                return;
            }
            let st: StaleProjects = decode(v, "stale projects");
            println!(
                "{} of {} projects under {} quiet for ≥ {} days ({} unverifiable, never listed); artifacts total {}",
                st.projects.len(),
                st.projects_evaluated,
                st.root_path,
                st.threshold_days,
                st.unverifiable,
                format_size(st.artifact_disk_size)
            );
            for p in &st.projects {
                println!(
                    "{}  {}  last activity {} days ago",
                    format_size(p.artifact_disk_size),
                    p.root,
                    p.last_activity_days
                );
                for a in &p.artifacts {
                    println!(
                        "    {}  {}  [{}]  {}",
                        format_size(a.disk_size),
                        a.path,
                        a.risk_tier.as_str(),
                        a.rule_id
                    );
                }
            }
        }
        Command::Volume { path, snapshots, scan, json } => {
            let mut params = Vec::new();
            if let Some(p) = &path {
                params.push(format!("path={}", encode_query_value(p)));
            }
            if snapshots {
                params.push("snapshots=true".to_string());
            }
            if let Some(id) = scan {
                params.push(format!("scanId={id}"));
            }
            let query = if params.is_empty() { String::new() } else { format!("?{}", params.join("&")) };
            let v = client.request(reqwest::Method::GET, &format!("/volume{query}"), None);
            if json {
                emit_json(&v);
                return;
            }
            let vol: VolumeStatus = decode(v, "volume status");
            println!("{} ({}, {})", vol.mount_point, vol.filesystem, vol.path);
            println!(
                "  total {}  used {}  free {}  available {}",
                format_size(vol.total_bytes),
                format_size(vol.used_bytes),
                format_size(vol.free_bytes),
                format_size(vol.available_bytes)
            );
            match (vol.volume_used_bytes, vol.hidden.other_volumes_bytes) {
                (Some(own), Some(other)) => println!(
                    "  this volume {}  other volumes in the container {}",
                    format_size(own),
                    format_size(other)
                ),
                _ => println!("  this volume: not reported by the filesystem"),
            }
            match (vol.purgeable_bytes, vol.important_usage_bytes) {
                (Some(p), Some(i)) => println!(
                    "  purgeable {}  (Finder's Available {} = available + purgeable)",
                    format_size(p),
                    format_size(i)
                ),
                _ => println!("  purgeable: not reported for this volume"),
            }
            if let Some(n) = vol.snapshot_count {
                println!("  local Time Machine snapshots: {n}");
                for name in vol.snapshots.iter().flatten() {
                    println!("    {name}");
                }
            }
            let h = &vol.hidden;
            if let Some(id) = h.scan_id {
                println!(
                    "  scan {id} of {}: scanned {}  unscanned on this volume {}  unreadable entries {}",
                    h.scan_root_path.as_deref().unwrap_or("?"),
                    h.scanned_bytes.map(format_size).unwrap_or_else(|| "?".into()),
                    h.unscanned_bytes.map(format_size).unwrap_or_else(|| "unknown".into()),
                    h.unreadable_count.unwrap_or(0)
                );
            }
            if !h.other_user_homes.is_empty() {
                println!("  other users' homes ({}):", h.other_user_homes.len());
                for u in &h.other_user_homes {
                    println!("    {}  {}", u.path, if u.readable { "readable" } else { "not readable" });
                }
            }
            if let Some(s) = &h.snapshot_suggestion {
                println!("  suggestion (never run by phantom): {s}");
            }
            println!("  note: {}", vol.note);
        }
        Command::Growth { root, group_by, json } => {
            // The flag takes the kebab-case spelling humans type (clap
            // rejects anything else, exit 2); the API takes the wire
            // spelling and nothing else.
            let wire_group = group_by.wire();
            let root = root.unwrap_or_else(|| client.latest_complete_scan_root());
            let query = format!("?root={}&groupBy={wire_group}", encode_query_value(&root));
            let v = client.request(reqwest::Method::GET, &format!("/scans/series{query}"), None);
            if json {
                emit_json(&v);
                return;
            }
            let g: Growth = decode(v, "growth series");
            println!("growth of {}  ({} scans, by {})", g.root_path, g.points.len(), g.group_by.as_str());
            for (i, p) in g.points.iter().enumerate() {
                let delta = if i == 0 {
                    String::new()
                } else {
                    let prev = g.points[i - 1].total_disk_size;
                    let d = p.total_disk_size as i128 - prev as i128;
                    format!("  {}{}", if d >= 0 { "+" } else { "-" }, format_size(d.unsigned_abs() as u64))
                };
                println!(
                    "  {}  {}  {} files{delta}",
                    p.started_at.format("%Y-%m-%d %H:%M"),
                    format_size(p.total_disk_size),
                    p.file_count
                );
            }
            for line in &g.series {
                let first = line.values.first().copied().flatten();
                let last = line.values.last().copied().flatten();
                let shown: Vec<String> = line
                    .values
                    .iter()
                    .map(|v| v.map(format_size).unwrap_or_else(|| "-".into()))
                    .collect();
                let trend = match (first, last) {
                    (Some(a), Some(b)) if b >= a => format!("+{}", format_size(b - a)),
                    (Some(a), Some(b)) => format!("-{}", format_size(a - b)),
                    _ => "n/a".into(),
                };
                println!("  {:<24} {}  ({trend})", line.key, shown.join(" → "));
            }
            match &g.forecast {
                None => println!("  forecast: none — {} scan(s); scan again later for a trend", g.points.len()),
                Some(f) => {
                    let rate = if f.bytes_per_day >= 0 {
                        format!("+{}/day", format_size(f.bytes_per_day as u64))
                    } else {
                        format!("-{}/day", format_size(f.bytes_per_day.unsigned_abs()))
                    };
                    match (f.days_until_full, f.projected_full_at) {
                        (Some(d), Some(at)) => println!(
                            "  forecast: {rate} over {:.1} days — disk full in {:.0} days (≈ {}) at this rate",
                            f.span_days,
                            d,
                            at.format("%Y-%m-%d")
                        ),
                        _ if f.bytes_per_day > 0 => println!(
                            "  forecast: {rate} over {:.1} days — the volume's headroom is unknown, so no full date",
                            f.span_days
                        ),
                        _ => println!("  forecast: {rate} over {:.1} days — not growing; no full date", f.span_days),
                    }
                    println!("  caveat: {}", f.caveat);
                }
            }
        }
        Command::Health => {
            let v = client.request(reqwest::Method::GET, "/health", None);
            emit_json(&v);
        }
        Command::Completions { .. } | Command::Man => unreachable!("handled before the client is built"),
        Command::Diff { scan_a, scan_b, since, json } => {
            let (a, b) = match (scan_a, scan_b, since) {
                (Some(a), Some(b), _) => (a.to_string(), b.to_string()),
                (_, _, Some(since)) => client.since_diff_pair(&since),
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
        Command::Scan {
            path,
            no_wait,
            cross_volumes,
            older,
            verify_locks,
            tool_estimates,
            json,
        } => {
            let mut body = serde_json::json!({
                "rootPath": path,
                "crossVolumes": cross_volumes,
                "verifyLocks": verify_locks,
                "toolEstimates": tool_estimates,
            });
            if let Some(older) = older {
                body["olderThan"] = serde_json::Value::String(older);
            }
            let started = client.request(reqwest::Method::POST, "/scans", Some(body));
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
                // The tier badge leads (v1.1): SAFE / CAUTION / REVIEW, then
                // the category and the label.
                println!(
                    "{}  {}  [{}]  {}",
                    format_size(g.disk_size),
                    tier_badge(g.risk_tier),
                    g.category.as_str(),
                    g.label
                );
                println!("  why: {}", g.why);
                // Surface the hardlink gap only when it is material (≥ 1%):
                // a group whose links share a few stray blocks would print
                // "listed 1.1 GB … frees 1.1 GB", which reads as a glitch.
                if g.listed_disk_size > g.disk_size + g.disk_size / 100 {
                    println!(
                        "  listed {} across {} files; hardlinks/clones share blocks, the group occupies {}",
                        format_size(g.listed_disk_size),
                        g.file_count,
                        format_size(g.disk_size)
                    );
                }
                // The deletion-honest number, when it differs materially from
                // the du-model size: links or clones outside the group (or
                // snapshots) keep the rest pinned.
                if g.private_size + g.disk_size / 100 < g.disk_size {
                    println!(
                        "  deleting frees {} — the rest is shared with paths outside this group or held by snapshots",
                        format_size(g.private_size)
                    );
                }
                println!("  hint: {}", g.hint);
                if g.rebuild_cost.kind != RebuildKind::None {
                    println!("  rebuild: {}", g.rebuild_cost.estimate);
                }
                if let Some(t) = &g.tool_estimate {
                    println!(
                        "  {} says {} reclaimable (`{}`): {}",
                        t.tool,
                        format_size(t.reclaimable_bytes),
                        t.command,
                        t.note
                    );
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// phantom-grw: the dogfood's `actual +55.2 GB` for bytes FREED read as
    /// growth. The direction is a word, never a sign.
    #[test]
    fn format_freed_spells_out_the_direction() {
        assert_eq!(format_freed(55_200_000_000), "freed 55.2 GB");
        assert_eq!(format_freed(0), "freed 0 B");
        assert_eq!(format_freed(-1_200_000_000), "grew 1.2 GB");
        assert!(!format_freed(5).starts_with('+'));
    }
}
