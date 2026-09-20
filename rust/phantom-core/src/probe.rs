// Opt-in subprocess probes (v1.1 Phase 2, phantom-mkn.5 / phantom-mkn.19):
// the read-only lockfile checks that back `LockState::Verified`, and the
// tool-native dry-run numbers that ride `HotspotGroup.toolEstimate`.
//
// The rules (docs/threat-model.md §4, each pinned by a test below):
//
// - Executables come from a FIXED list of absolute install paths. `$PATH` is
//   never consulted; a tool at none of them is `Unavailable`, not searched for.
// - Children run with a scrubbed environment (HOME + the tool's own dir on
//   PATH), stdin closed, and a wall-clock timeout after which they are killed.
// - Output is parsed into a number or a short reason. Nothing from it is
//   executed, and only a short excerpt is ever stored.
// - Nothing here writes: every command is the tool's own dry-run / read-only
//   mode. Adding a command that writes is a threat-model change, not a row.
//
// The classifier never calls this module. The API's completion path builds a
// verifier from `lock_verifier` when the scan opted in, and calls
// `attach_tool_estimates` when it asked for tool numbers.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::classify::{HotspotRule, HotspotsSummary, LockVerdict, LockVerify, ToolEstimate, TOP_PATHS_PER_GROUP};

/// Wall-clock bound per child process.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20);

/// Verify runs per registry rule per scan. A code folder with hundreds of
/// Cargo projects must not fan out into hundreds of `cargo metadata` runs;
/// the largest roots (the ones the summary names) are the ones checked.
pub const MAX_VERIFY_RUNS_PER_RULE: usize = TOP_PATHS_PER_GROUP;

/// At most this many bytes of a child's stderr survive into a reason.
const REASON_EXCERPT: usize = 160;

/// A tool and the ONLY places it may be executed from. `~/` expands to
/// `$HOME`; order is preference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tool {
    pub name: &'static str,
    pub candidates: &'static [&'static str],
}

pub const TOOLS: &[Tool] = &[
    Tool { name: "cargo", candidates: &["~/.cargo/bin/cargo", "/opt/homebrew/bin/cargo", "/usr/local/bin/cargo"] },
    Tool { name: "npm", candidates: &["/opt/homebrew/bin/npm", "/usr/local/bin/npm"] },
    Tool { name: "uv", candidates: &["~/.local/bin/uv", "/opt/homebrew/bin/uv", "/usr/local/bin/uv"] },
    Tool {
        name: "docker",
        candidates: &["/usr/local/bin/docker", "/opt/homebrew/bin/docker", "/Applications/Docker.app/Contents/Resources/bin/docker"],
    },
    Tool { name: "brew", candidates: &["/opt/homebrew/bin/brew", "/usr/local/bin/brew"] },
];

/// The first candidate path that exists for `tool`, or None. `home` is the
/// user's home for `~/` candidates; `exists` is injected so tests can pin
/// the order and the never-`$PATH` rule without a real toolchain.
pub fn resolve_with(tool: &str, home: Option<&Path>, exists: impl Fn(&Path) -> bool) -> Option<PathBuf> {
    let t = TOOLS.iter().find(|t| t.name == tool)?;
    t.candidates
        .iter()
        .filter_map(|c| match c.strip_prefix("~/") {
            Some(rest) => home.map(|h| h.join(rest)),
            None => Some(PathBuf::from(c)),
        })
        .find(|p| exists(p))
}

/// [`resolve_with`] against the real filesystem and `$HOME`.
pub fn resolve(tool: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    resolve_with(tool, home.as_deref(), |p| p.is_file())
}

/// What a bounded run produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOutcome {
    /// Exit code; None when the child was killed (timeout) or died by signal.
    pub status: Option<i32>,
    pub timed_out: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Run `exe args…` with a scrubbed environment, no stdin, and a wall-clock
/// timeout; on expiry the child is killed. The io::Error case is "could not
/// start" (missing binary, not executable).
pub fn run_bounded(exe: &Path, args: &[&str], cwd: Option<&Path>, timeout: Duration) -> std::io::Result<RunOutcome> {
    let mut cmd = Command::new(exe);
    cmd.args(args)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(home) = std::env::var_os("HOME") {
        cmd.env("HOME", home);
    }
    if let Some(dir) = exe.parent() {
        cmd.env("PATH", dir);
    }
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    let mut child = cmd.spawn()?;
    // Drain both pipes on threads so a chatty child cannot block on a full
    // pipe while we wait for it.
    let drain = |pipe: Option<std::process::ChildStdout>| {
        std::thread::spawn(move || {
            let mut s = String::new();
            if let Some(mut p) = pipe {
                let _ = p.read_to_string(&mut s);
            }
            s
        })
    };
    let out = drain(child.stdout.take());
    let err_pipe = child.stderr.take();
    let err = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(mut p) = err_pipe {
            let _ = p.read_to_string(&mut s);
        }
        s
    });
    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let status = loop {
        if let Some(st) = child.try_wait()? {
            break st.code();
        }
        if Instant::now() >= deadline {
            timed_out = true;
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    Ok(RunOutcome {
        status,
        timed_out,
        stdout: out.join().unwrap_or_default(),
        stderr: err.join().unwrap_or_default(),
    })
}

fn excerpt(s: &str) -> String {
    let line = s.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    let mut e: String = line.chars().take(REASON_EXCERPT).collect();
    if line.chars().count() > REASON_EXCERPT {
        e.push('…');
    }
    e
}

/// Run one lockfile check with an already-resolved executable.
pub fn verify_lock_with(exe: Option<&Path>, verify: &LockVerify, project_dir: &Path, timeout: Duration) -> LockVerdict {
    let Some(exe) = exe else { return LockVerdict::Unavailable };
    match run_bounded(exe, verify.args, Some(project_dir), timeout) {
        Err(e) => LockVerdict::Failed(format!("could not start {}: {e}", verify.tool)),
        Ok(o) if o.timed_out => LockVerdict::Failed(format!("timed out after {}s", timeout.as_secs())),
        Ok(RunOutcome { status: Some(0), .. }) => LockVerdict::Verified,
        Ok(o) => {
            let code = o.status.map_or("signal".to_string(), |c| format!("exit {c}"));
            let why = excerpt(if o.stderr.trim().is_empty() { &o.stdout } else { &o.stderr });
            LockVerdict::Failed(if why.is_empty() { code } else { format!("{code}: {why}") })
        }
    }
}

/// The verifier the API injects into `ClassifyOptions::verify` when a scan
/// opted in: resolves the rule's tool from the fixed paths, caps runs per
/// rule, and never panics into the classifier.
pub fn lock_verifier(timeout: Duration) -> impl Fn(&HotspotRule, &str) -> LockVerdict {
    let runs: RefCell<HashMap<&'static str, usize>> = RefCell::new(HashMap::new());
    move |rule: &HotspotRule, project_dir: &str| {
        let Some(v) = rule.verify else { return LockVerdict::Unavailable };
        let mut runs = runs.borrow_mut();
        let n = runs.entry(rule.id).or_insert(0);
        if *n >= MAX_VERIFY_RUNS_PER_RULE {
            return LockVerdict::Unavailable;
        }
        *n += 1;
        drop(runs);
        let exe = resolve(v.tool);
        verify_lock_with(exe.as_deref(), &v, Path::new(project_dir), timeout)
    }
}

// ---------------------------------------------------------------------------
// Tool-native estimates

/// One dry-run probe and the registry rows its number belongs to.
#[derive(Debug, Clone, Copy)]
pub struct ToolProbe {
    pub rule_ids: &'static [&'static str],
    pub tool: &'static str,
    pub args: &'static [&'static str],
    pub note: &'static str,
    /// Wall-clock bound for THIS probe. `brew cleanup -n` evaluates every
    /// installed formula and took ~2 min on a 300-formula Mac (2026-09-07);
    /// the default bound would make its estimate always null.
    pub timeout: Duration,
    /// stdout → reclaimable bytes; None when the output is not understood.
    pub parse: fn(&str) -> Option<u64>,
}

pub const PROBES: &[ToolProbe] = &[
    ToolProbe {
        rule_ids: &["docker-desktop-data"],
        tool: "docker",
        args: &["system", "df", "--format", "json"],
        note: "images, containers, volumes and build cache Docker reports reclaimable; `docker system prune` frees them inside the VM disk",
        timeout: DEFAULT_TIMEOUT,
        parse: parse_docker_system_df,
    },
    ToolProbe {
        rule_ids: &["homebrew-cellar", "homebrew-cellar-intel"],
        tool: "brew",
        args: &["cleanup", "-n"],
        note: "superseded formula versions and downloads brew would remove; the Cellar itself stays",
        timeout: Duration::from_secs(180),
        parse: parse_brew_cleanup,
    },
    ToolProbe {
        rule_ids: &["uv-cache"],
        tool: "uv",
        args: &["cache", "size"],
        note: "the whole uv cache, which `uv cache clean` empties; `uv cache prune` frees less",
        timeout: DEFAULT_TIMEOUT,
        parse: parse_uv_cache_size,
    },
];

/// `1.2GB`, `734.3MB`, `512kB`, `0B` → bytes. Docker prints decimal units;
/// Homebrew prints binary ones under the same letters — `decimal` picks.
pub fn parse_size(s: &str, decimal: bool) -> Option<u64> {
    let s = s.trim();
    let split = s.find(|c: char| !(c.is_ascii_digit() || c == '.'))?;
    let (num, unit) = (&s[..split], s[split..].trim());
    let n: f64 = num.parse().ok()?;
    let base: f64 = if decimal { 1000.0 } else { 1024.0 };
    let exp = match unit.to_ascii_lowercase().as_str() {
        "b" => 0,
        "kb" | "kib" => 1,
        "mb" | "mib" => 2,
        "gb" | "gib" => 3,
        "tb" | "tib" => 4,
        _ => return None,
    };
    Some((n * base.powi(exp)).round() as u64)
}

/// `docker system df --format json`: one JSON object per line with a
/// `Reclaimable` field like `"1.2GB (60%)"`. Sum across the types.
pub fn parse_docker_system_df(stdout: &str) -> Option<u64> {
    let mut total = 0u64;
    let mut rows = 0;
    for line in stdout.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let v: serde_json::Value = serde_json::from_str(line).ok()?;
        let r = v.get("Reclaimable")?.as_str()?;
        let size = r.split_whitespace().next()?;
        total += parse_size(size, true)?;
        rows += 1;
    }
    (rows > 0).then_some(total)
}

/// `brew cleanup -n`: prefer the `would free approximately X` total; else
/// sum the `(… size)` of every `Would remove:` line; nothing to remove is 0.
pub fn parse_brew_cleanup(stdout: &str) -> Option<u64> {
    for line in stdout.lines() {
        if let Some(rest) = line.split("would free approximately ").nth(1) {
            let size = rest.split_whitespace().next()?;
            return parse_size(size, false);
        }
    }
    let mut total = 0u64;
    for line in stdout.lines().filter(|l| l.starts_with("Would remove:")) {
        let inside = line.rsplit('(').next()?.trim_end_matches(')');
        let size = inside.rsplit(", ").next()?;
        total += parse_size(size, false)?;
    }
    Some(total)
}

/// `uv cache size`: a bare byte count on stdout (warnings go to stderr).
pub fn parse_uv_cache_size(stdout: &str) -> Option<u64> {
    stdout.split_whitespace().last()?.parse().ok()
}

/// Run every probe whose rule appears in the summary and attach the numbers.
/// Each probe runs at most once, under its own bound; a missing tool, a
/// failed run or unparsable output leaves `toolEstimate` null for its groups.
pub fn attach_tool_estimates(summary: &mut HotspotsSummary) {
    attach_tool_estimates_with(summary, |probe| {
        let exe = resolve(probe.tool)?;
        let out = run_bounded(&exe, probe.args, None, probe.timeout).ok()?;
        if out.timed_out || out.status != Some(0) {
            return None;
        }
        (probe.parse)(&out.stdout)
    });
}

/// [`attach_tool_estimates`] with the runner injected (tests).
pub fn attach_tool_estimates_with(summary: &mut HotspotsSummary, run: impl Fn(&ToolProbe) -> Option<u64>) {
    for probe in PROBES {
        if !summary.groups.iter().any(|g| probe.rule_ids.contains(&g.rule_id.as_str())) {
            continue;
        }
        let Some(bytes) = run(probe) else { continue };
        for g in summary.groups.iter_mut().filter(|g| probe.rule_ids.contains(&g.rule_id.as_str())) {
            g.tool_estimate = Some(ToolEstimate {
                tool: probe.tool.to_string(),
                command: format!("{} {}", probe.tool, probe.args.join(" ")),
                reclaimable_bytes: bytes,
                note: probe.note.to_string(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::{Category, HotspotGroup, RebuildCost, RiskTier, REGISTRY};

    const DOCKER_DF: &str = include_str!("../../../tests/fixtures/probes/docker-system-df.jsonl");
    const BREW_CLEANUP: &str = include_str!("../../../tests/fixtures/probes/brew-cleanup-n.txt");
    const BREW_NO_TOTAL: &str = include_str!("../../../tests/fixtures/probes/brew-cleanup-n-no-total.txt");
    const BREW_NOTHING: &str = include_str!("../../../tests/fixtures/probes/brew-cleanup-n-nothing.txt");
    const UV_SIZE: &str = include_str!("../../../tests/fixtures/probes/uv-cache-size.txt");

    // -- fixed paths, never $PATH ---------------------------------------------------

    #[test]
    fn resolve_takes_the_first_existing_fixed_candidate_and_expands_home() {
        let home = Path::new("/Users/ghost");
        let only_local = |p: &Path| p == Path::new("/usr/local/bin/cargo");
        assert_eq!(resolve_with("cargo", Some(home), only_local), Some(PathBuf::from("/usr/local/bin/cargo")));
        let all = |_: &Path| true;
        assert_eq!(resolve_with("cargo", Some(home), all), Some(PathBuf::from("/Users/ghost/.cargo/bin/cargo")));
        // No HOME: the ~/ candidate is skipped, not turned into a relative path.
        assert_eq!(resolve_with("cargo", None, all), Some(PathBuf::from("/opt/homebrew/bin/cargo")));
        let none = |_: &Path| false;
        assert_eq!(resolve_with("brew", Some(home), none), None);
        assert_eq!(resolve_with("not-a-tool", Some(home), all), None);
    }

    #[test]
    fn every_candidate_is_absolute_and_every_verify_tool_is_known() {
        for t in TOOLS {
            for c in t.candidates {
                assert!(c.starts_with('/') || c.starts_with("~/"), "{}: {c} is not a fixed path", t.name);
            }
        }
        for rule in REGISTRY {
            if let Some(v) = rule.verify {
                assert!(TOOLS.iter().any(|t| t.name == v.tool), "{}: verify tool {} has no fixed paths", rule.id, v.tool);
                assert!(v.args.iter().any(|a| *a == "--offline" || *a == "-n"), "{}: verify must be offline", rule.id);
            }
        }
        for p in PROBES {
            assert!(TOOLS.iter().any(|t| t.name == p.tool), "probe tool {} has no fixed paths", p.tool);
            for id in p.rule_ids {
                assert!(REGISTRY.iter().any(|r| r.id == *id), "probe names unknown rule {id}");
            }
        }
    }

    // -- the bounded runner ----------------------------------------------------------

    #[test]
    fn run_bounded_reports_exit_code_output_and_timeout() {
        let sh = Path::new("/bin/sh");
        let ok = run_bounded(sh, &["-c", "echo out; echo err 1>&2; exit 3"], None, Duration::from_secs(5)).unwrap();
        assert_eq!((ok.status, ok.timed_out), (Some(3), false));
        assert_eq!((ok.stdout.trim(), ok.stderr.trim()), ("out", "err"));

        let slow = run_bounded(Path::new("/bin/sleep"), &["5"], None, Duration::from_millis(150)).unwrap();
        assert!(slow.timed_out && slow.status.is_none(), "{slow:?}");

        // The environment is scrubbed: only HOME and a one-entry PATH.
        // (PATH is just /bin here, so name the binaries: this IS the scrub.)
        let env = run_bounded(sh, &["-c", "/usr/bin/env"], None, Duration::from_secs(5)).unwrap();
        let keys: Vec<&str> = env.stdout.lines().filter_map(|l| l.split('=').next()).collect();
        assert!(keys.iter().all(|k| *k == "HOME" || *k == "PATH" || *k == "PWD" || *k == "SHLVL" || *k == "_"), "{keys:?}");
        assert!(env.stdout.lines().any(|l| l == "PATH=/bin"), "{}", env.stdout);

        // cwd is honoured.
        let cwd = run_bounded(sh, &["-c", "pwd"], Some(Path::new("/tmp")), Duration::from_secs(5)).unwrap();
        assert!(cwd.stdout.trim().ends_with("tmp"), "{}", cwd.stdout);

        // A missing executable is an error, not a verdict.
        assert!(run_bounded(Path::new("/no/such/tool"), &[], None, Duration::from_secs(1)).is_err());
    }

    #[test]
    fn verify_lock_covers_every_branch() {
        let dir = Path::new("/tmp");
        let t = Duration::from_secs(5);
        let mk = |args: &'static [&'static str]| LockVerify { requires: "x.lock", tool: "sh", args };
        assert_eq!(verify_lock_with(None, &mk(&["-c", "exit 0"]), dir, t), LockVerdict::Unavailable);
        let sh = Some(Path::new("/bin/sh"));
        assert_eq!(verify_lock_with(sh, &mk(&["-c", "exit 0"]), dir, t), LockVerdict::Verified);
        assert_eq!(
            verify_lock_with(sh, &mk(&["-c", "echo 'lock file needs to be updated' 1>&2; exit 101"]), dir, t),
            LockVerdict::Failed("exit 101: lock file needs to be updated".into())
        );
        // stdout is the fallback reason; no output at all leaves just the code.
        assert_eq!(verify_lock_with(sh, &mk(&["-c", "echo nope; exit 2"]), dir, t), LockVerdict::Failed("exit 2: nope".into()));
        assert_eq!(verify_lock_with(sh, &mk(&["-c", "exit 4"]), dir, t), LockVerdict::Failed("exit 4".into()));
        match verify_lock_with(Some(Path::new("/bin/sleep")), &mk(&["5"]), dir, Duration::from_millis(150)) {
            LockVerdict::Failed(r) => assert!(r.starts_with("timed out"), "{r}"),
            other => panic!("{other:?}"),
        }
        match verify_lock_with(Some(Path::new("/no/such/tool")), &mk(&[]), dir, t) {
            LockVerdict::Failed(r) => assert!(r.starts_with("could not start sh"), "{r}"),
            other => panic!("{other:?}"),
        }
        // Long stderr is excerpted, never stored whole.
        let long = "x".repeat(500);
        match verify_lock_with(sh, &mk(Box::leak(vec!["-c", Box::leak(format!("echo {long} 1>&2; exit 1").into_boxed_str())].into_boxed_slice())), dir, t) {
            LockVerdict::Failed(r) => assert!(r.chars().count() < 200 && r.ends_with('…'), "{}", r.len()),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn lock_verifier_caps_runs_per_rule_and_skips_rules_without_verify() {
        let verifier = lock_verifier(Duration::from_secs(1));
        let no_verify = REGISTRY.iter().find(|r| r.verify.is_none()).unwrap();
        assert_eq!(verifier(no_verify, "/tmp"), LockVerdict::Unavailable);
        // cargo IS on this machine's fixed paths or it is not; either way the
        // cap holds: past MAX runs every call is Unavailable without running.
        let cargo = REGISTRY.iter().find(|r| r.id == "cargo-target").unwrap();
        let mut outcomes = Vec::new();
        for _ in 0..(MAX_VERIFY_RUNS_PER_RULE + 2) {
            outcomes.push(verifier(cargo, "/tmp"));
        }
        assert_eq!(outcomes[MAX_VERIFY_RUNS_PER_RULE], LockVerdict::Unavailable);
        assert_eq!(outcomes[MAX_VERIFY_RUNS_PER_RULE + 1], LockVerdict::Unavailable);
    }

    // -- parsers, from raw fixture bytes ------------------------------------------

    #[test]
    fn size_units_decimal_and_binary() {
        assert_eq!(parse_size("1.2GB", true), Some(1_200_000_000));
        assert_eq!(parse_size("512kB", true), Some(512_000));
        assert_eq!(parse_size("0B", true), Some(0));
        assert_eq!(parse_size("4.4MB", false), Some((4.4f64 * 1024.0 * 1024.0).round() as u64));
        assert_eq!(parse_size("1.0KB", false), Some(1024));
        assert_eq!(parse_size("336.1KB", false), Some(344_166));
        assert_eq!(parse_size("garbage", true), None);
        assert_eq!(parse_size("12 parsecs", true), None);
        assert_eq!(parse_size("", true), None);
    }

    #[test]
    fn docker_system_df_sums_reclaimable_across_types() {
        assert_eq!(parse_docker_system_df(DOCKER_DF), Some(1_200_000_000 + 0 + 734_300_000 + 512_000));
        assert_eq!(parse_docker_system_df(""), None, "no rows is not zero");
        assert_eq!(parse_docker_system_df("not json"), None);
        assert_eq!(parse_docker_system_df(r#"{"Type":"Images"}"#), None, "missing field");
    }

    #[test]
    fn brew_cleanup_prefers_the_total_and_falls_back_to_the_rows() {
        assert_eq!(parse_brew_cleanup(BREW_CLEANUP), parse_size("5.9MB", false));
        let rows = parse_size("4.4MB", false).unwrap() + 1024;
        assert_eq!(parse_brew_cleanup(BREW_NO_TOTAL), Some(rows));
        assert_eq!(parse_brew_cleanup(BREW_NOTHING), Some(0), "nothing to clean is an honest zero");
        assert_eq!(parse_brew_cleanup("Would remove: /x (weird)"), None);
    }

    #[test]
    fn uv_cache_size_is_a_bare_byte_count() {
        assert_eq!(parse_uv_cache_size(UV_SIZE), Some(1_810_251_776));
        assert_eq!(parse_uv_cache_size(""), None);
        assert_eq!(parse_uv_cache_size("1.2GiB"), None);
    }

    fn group(rule_id: &str) -> HotspotGroup {
        HotspotGroup {
            rule_id: rule_id.into(),
            label: "l".into(),
            category: Category::ToolManagedCache,
            hint: "h".into(),
            command: None,
            risk_tier: RiskTier::Caution,
            why: "w.".into(),
            rebuild_cost: RebuildCost::default(),
            tool_estimate: None,
            disk_size: 10,
            listed_disk_size: 10,
            private_size: 10,
            logical_size: 10,
            file_count: 1,
            top_paths: vec![],
        }
    }

    #[test]
    fn attach_runs_each_probe_once_only_for_present_rules_and_leaves_null_on_failure() {
        let mut s = HotspotsSummary::empty();
        s.groups = vec![group("homebrew-cellar"), group("homebrew-cellar-intel"), group("uv-cache"), group("dot-cache")];
        let calls = RefCell::new(Vec::new());
        attach_tool_estimates_with(&mut s, |p| {
            calls.borrow_mut().push(p.tool);
            match p.tool {
                "brew" => Some(4242),
                _ => None, // uv: tool missing / failed
            }
        });
        assert_eq!(*calls.borrow(), vec!["brew", "uv"], "docker has no group: never run; brew once for two groups");
        let brew = s.groups[0].tool_estimate.as_ref().unwrap();
        assert_eq!((brew.tool.as_str(), brew.reclaimable_bytes, brew.command.as_str()), ("brew", 4242, "brew cleanup -n"));
        assert_eq!(s.groups[1].tool_estimate.as_ref().map(|t| t.reclaimable_bytes), Some(4242));
        assert_eq!(s.groups[2].tool_estimate, None, "a failed probe stays null");
        assert_eq!(s.groups[3].tool_estimate, None);
    }
}
