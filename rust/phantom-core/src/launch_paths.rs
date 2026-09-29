// Launch-path awareness for reclaim plans (phantom-o2f): the paths that
// long-lived TOOL REGISTRATIONS are launched from.
//
// The 2026-09-16 gate run moved ~/Code/vigil/rust/target — correctly
// classified (Cargo build output, gitignored, rebuildable), and also the
// directory ~/.claude.json registered the vigil MCP server's command from
// (…/target/release/vigil-mcp). Every Claude Code session then failed to
// start that server until someone ran `cargo build --release`. phantom-djf
// covered builds in flight and phantom-2lz covered git-tracked paths; this
// is the third case: nothing is writing, git ignores it, a build brings it
// back — and this copy is still load-bearing. Phantom is right that the
// bytes are reclaimable and wrong that reclaiming them is free.
//
// Sources read at PLAN time (files only; no subprocess, nothing executed):
//   1. ~/.claude.json — `mcpServers.<name>.command` (+ absolute `args`) at
//      the top level and under every `projects.<dir>`; and each such
//      project's own `<dir>/.mcp.json`.
//   2. ~/.claude/settings.json, settings.local.json — `hooks.<Event>[]
//      .hooks[].command`: absolute path tokens of the command string.
//   3. ~/Library/LaunchAgents/*.plist — `Program` and `ProgramArguments`.
//   4. ~/bin/* and ~/.local/bin/* — symlink targets (the PATH shims).
//
// Each registration yields one or more TARGET paths: the path as written
// (with `~` expanded) and, when it exists, its canonical form, so a shim
// that chains through another symlink still names the real binary. A plan
// path that IS a target or CONTAINS one is a launch path: the plan drops it
// and says why (`plan.rs`). Sources that are absent or unparsable
// contribute nothing — a missing ~/.claude.json is not an error, it is a
// Mac without Claude Code.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::Value;

/// One registered launch target and where it was registered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchRegistration {
    /// Human text for the plan's `why`: `MCP server "vigil" (~/.claude.json)`.
    pub source: String,
    /// The executable (or script) the registration launches, as registered.
    pub target: PathBuf,
}

/// Every launch target found under one home directory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LaunchRegistry {
    pub registrations: Vec<LaunchRegistration>,
}

impl LaunchRegistry {
    /// Read the four sources under `home`. Never fails: an absent or
    /// unreadable source is skipped, a malformed one contributes what it
    /// can. `home` is the directory `~` stands for.
    pub fn load(home: &Path) -> Self {
        let mut regs = Vec::new();
        regs.extend(claude_json(home));
        regs.extend(claude_settings(home));
        regs.extend(launch_agents(home));
        regs.extend(bin_symlinks(home));
        LaunchRegistry { registrations: regs }
    }

    /// If `path` is, or contains, a registered launch target: the reason,
    /// naming the target and every registration behind it. `None` means the
    /// plan may treat the path as the classifier rated it.
    ///
    /// Containment is by path component (`/a/target` does not contain
    /// `/a/target-old/x`). Both the target as registered and its canonical
    /// form (when it exists) are compared, so a shim chain still matches.
    pub fn launch_path_for(&self, path: &str) -> Option<String> {
        let plan_path = normalize(Path::new(path));
        // The plan path's canonical form too: a scan root reached through a
        // symlink (/var → /private/var, a linked ~/Code) must still match a
        // registration written the other way.
        let plan_real = std::fs::canonicalize(&plan_path).ok();
        let mut hits: Vec<(&Path, &str)> = Vec::new();
        for reg in &self.registrations {
            let raw = reg.target.as_path();
            let target = normalize(raw);
            if contains_or_is(&plan_path, &target) {
                hits.push((raw, &reg.source));
                continue;
            }
            if let Ok(real) = std::fs::canonicalize(&target)
                && (contains_or_is(&plan_path, &real)
                    || plan_real.as_deref().is_some_and(|pr| contains_or_is(pr, &real)))
            {
                hits.push((raw, &reg.source));
            }
        }
        if hits.is_empty() {
            return None;
        }
        // Stable text: one clause per distinct target, sources in order.
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut clauses = Vec::new();
        for (target, source) in hits {
            let key = format!("{}\u{0}{}", target.display(), source);
            if seen.insert(key) {
                clauses.push(format!("{} is the launch path for {}", target.display(), source));
            }
        }
        Some(clauses.join("; "))
    }
}

/// Is `target` equal to `plan_path`, or strictly inside it?
fn contains_or_is(plan_path: &Path, target: &Path) -> bool {
    target == plan_path || target.starts_with(plan_path)
}

/// Resolve `.` and `..` lexically (no filesystem), so a relative shim
/// target like `~/.local/bin/../../Code/x` compares by components. Symlinks
/// are NOT followed here; `canonicalize` does that where the path exists.
fn normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// `~/x` → `<home>/x`; anything else unchanged.
fn expand_tilde(s: &str, home: &Path) -> PathBuf {
    match s.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None if s == "~" => home.to_path_buf(),
        None => PathBuf::from(s),
    }
}

/// A command or argument counts as a launch target only when it names a
/// path (absolute, or `~/`-relative). A bare `npx` or `uvx` is resolved by
/// the launching program's own PATH at launch time, which this reader
/// cannot know; the ~/bin and ~/.local/bin shims cover the user's own PATH.
fn as_path(s: &str, home: &Path) -> Option<PathBuf> {
    if s.starts_with('/') || s.starts_with("~/") {
        Some(expand_tilde(s, home))
    } else {
        None
    }
}

/// Absolute-path tokens of a shell command string (`/x/hook.sh --flag
/// /y/other` → both paths). Split on whitespace; quotes are not parsed —
/// a path with spaces in a hook command is rare and the false negative is
/// the safe direction (nothing extra is held back).
fn path_tokens(command: &str, home: &Path) -> Vec<PathBuf> {
    command
        .split_whitespace()
        .filter_map(|tok| as_path(tok.trim_matches(|c| c == '"' || c == '\''), home))
        .collect()
}

fn read_json(path: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// `mcpServers` object → registrations. `label` names the file for the
/// reason text.
fn mcp_servers(servers: &Value, label: &str, home: &Path) -> Vec<LaunchRegistration> {
    let Some(map) = servers.as_object() else { return Vec::new() };
    let mut out = Vec::new();
    for (name, server) in map {
        let source = format!("MCP server \"{name}\" ({label})");
        if let Some(cmd) = server["command"].as_str()
            && let Some(p) = as_path(cmd, home)
        {
            out.push(LaunchRegistration { source: source.clone(), target: p });
        }
        if let Some(args) = server["args"].as_array() {
            for a in args.iter().filter_map(Value::as_str) {
                if let Some(p) = as_path(a, home) {
                    out.push(LaunchRegistration { source: source.clone(), target: p });
                }
            }
        }
    }
    out
}

/// Source 1: `~/.claude.json` (user-scoped servers, per-project servers)
/// and every listed project's `.mcp.json`.
fn claude_json(home: &Path) -> Vec<LaunchRegistration> {
    let Some(v) = read_json(&home.join(".claude.json")) else { return Vec::new() };
    let mut out = mcp_servers(&v["mcpServers"], "~/.claude.json", home);
    if let Some(projects) = v["projects"].as_object() {
        for (dir, project) in projects {
            let label = format!("~/.claude.json project {dir}");
            out.extend(mcp_servers(&project["mcpServers"], &label, home));
            let mcp_json = Path::new(dir).join(".mcp.json");
            if let Some(pv) = read_json(&mcp_json) {
                out.extend(mcp_servers(&pv["mcpServers"], &format!("{}", mcp_json.display()), home));
            }
        }
    }
    out
}

/// Source 2: hook commands in `~/.claude/settings.json` and
/// `~/.claude/settings.local.json`.
fn claude_settings(home: &Path) -> Vec<LaunchRegistration> {
    let mut out = Vec::new();
    for (file, label) in [
        ("settings.json", "~/.claude/settings.json"),
        ("settings.local.json", "~/.claude/settings.local.json"),
    ] {
        let Some(v) = read_json(&home.join(".claude").join(file)) else { continue };
        let Some(events) = v["hooks"].as_object() else { continue };
        for (event, matchers) in events {
            let Some(matchers) = matchers.as_array() else { continue };
            for m in matchers {
                let Some(hooks) = m["hooks"].as_array() else { continue };
                for h in hooks {
                    let Some(cmd) = h["command"].as_str() else { continue };
                    for p in path_tokens(cmd, home) {
                        out.push(LaunchRegistration {
                            source: format!("{event} hook ({label})"),
                            target: p,
                        });
                    }
                }
            }
        }
    }
    out
}

/// Source 3: `~/Library/LaunchAgents/*.plist` — `Program` and every
/// `ProgramArguments` string. XML plists only (the form launchctl, brew and
/// hand-written agents use); a binary plist contributes nothing.
fn launch_agents(home: &Path) -> Vec<LaunchRegistration> {
    let dir = home.join("Library").join("LaunchAgents");
    let Ok(entries) = std::fs::read_dir(&dir) else { return Vec::new() };
    let mut out = Vec::new();
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "plist"))
        .collect();
    files.sort();
    for file in files {
        let Ok(text) = std::fs::read_to_string(&file) else { continue };
        let label = file
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let source = format!("LaunchAgent {label} (~/Library/LaunchAgents/{label}.plist)");
        for s in plist_program_strings(&text) {
            if let Some(p) = as_path(&s, home) {
                out.push(LaunchRegistration { source: source.clone(), target: p });
            }
        }
    }
    out
}

/// The `<string>` values under `<key>Program</key>` and inside the
/// `<array>` under `<key>ProgramArguments</key>`, from an XML plist. A
/// deliberately small reader: it walks the top-level dict's key/value
/// pairs by scanning for `<key>…</key>` and the element that follows.
/// Anything it does not understand yields nothing (never a false hold).
pub fn plist_program_strings(xml: &str) -> Vec<String> {
    let mut out = Vec::new();
    for key in ["Program", "ProgramArguments"] {
        let marker = format!("<key>{key}</key>");
        let Some(at) = xml.find(&marker) else { continue };
        let rest = xml[at + marker.len()..].trim_start();
        if let Some(inner) = rest.strip_prefix("<string>") {
            if let Some(end) = inner.find("</string>") {
                out.push(unescape_xml(&inner[..end]));
            }
        } else if let Some(inner) = rest.strip_prefix("<array>") {
            let Some(end) = inner.find("</array>") else { continue };
            let mut body = &inner[..end];
            while let Some(start) = body.find("<string>") {
                let after = &body[start + "<string>".len()..];
                let Some(close) = after.find("</string>") else { break };
                out.push(unescape_xml(&after[..close]));
                body = &after[close + "</string>".len()..];
            }
        }
    }
    out
}

fn unescape_xml(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
}

/// Source 4: symlinks in `~/bin` and `~/.local/bin`, resolved one hop
/// (relative targets against the link's directory). Canonical resolution
/// happens at match time so a target that no longer exists still counts
/// as registered.
fn bin_symlinks(home: &Path) -> Vec<LaunchRegistration> {
    let mut out = Vec::new();
    for (dir, label) in [(home.join("bin"), "~/bin"), (home.join(".local").join("bin"), "~/.local/bin")] {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        let mut links: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
        links.sort();
        for link in links {
            let Ok(target) = std::fs::read_link(&link) else { continue };
            let abs = if target.is_absolute() { target } else { dir.join(target) };
            let name = link.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            out.push(LaunchRegistration {
                source: format!("the {label}/{name} symlink"),
                target: abs,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A home directory shaped like the 2026-09-16 Mac: ~/.claude.json
    /// registers the vigil MCP server from its cargo target, a project's
    /// .mcp.json registers another, settings.json has a hook script, a
    /// LaunchAgent runs a binary out of Application Support, and ~/bin
    /// holds a shim into a SwiftPM .build directory.
    fn home() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(home.join(".claude")).unwrap();
        let vigil = home.join("Code/vigil/rust/target/release");
        fs::create_dir_all(&vigil).unwrap();
        fs::write(vigil.join("vigil-mcp"), b"#!/bin/sh\n").unwrap();
        let proj = home.join("Code/proj");
        fs::create_dir_all(proj.join(".venv/bin")).unwrap();
        fs::write(proj.join(".venv/bin/python"), b"").unwrap();
        fs::write(
            proj.join(".mcp.json"),
            serde_json::json!({ "mcpServers": { "proj": { "command": "~/Code/proj/.venv/bin/python", "args": ["-m", "proj_mcp"] } } })
                .to_string(),
        )
        .unwrap();
        fs::write(
            home.join(".claude.json"),
            serde_json::json!({
                "mcpServers": {
                    "vigil": { "type": "stdio", "command": format!("{}/vigil-mcp", vigil.display()), "args": [] },
                    "builder": { "command": "builder-mcp" },
                    "suds": { "command": "uvx", "args": ["--from", "/opt/suds/wheel", "suds-mcp"] }
                },
                "projects": {
                    proj.to_str().unwrap(): { "mcpServers": { "local": { "command": "npx", "args": ["-y", "x"] } } }
                }
            })
            .to_string(),
        )
        .unwrap();
        fs::write(
            home.join(".claude/settings.json"),
            serde_json::json!({ "hooks": { "PreToolUse": [ { "matcher": "Bash", "hooks": [ { "type": "command", "command": "~/.claude/hooks/guard.sh --strict /tmp/ledger" } ] } ] } })
                .to_string(),
        )
        .unwrap();
        let agents = home.join("Library/LaunchAgents");
        fs::create_dir_all(&agents).unwrap();
        fs::write(
            agents.join("com.example.api.plist"),
            format!(
                "<?xml version=\"1.0\"?>\n<plist version=\"1.0\">\n<dict>\n\t<key>Label</key>\n\t<string>com.example.api</string>\n\t<key>ProgramArguments</key>\n\t<array>\n\t\t<string>{}/Library/Application Support/example/bin/example-api</string>\n\t\t<string>--port</string>\n\t\t<string>1234</string>\n\t</array>\n\t<key>StandardOutPath</key>\n\t<string>{}/Library/Logs/example.log</string>\n</dict>\n</plist>\n",
                home.display(),
                home.display()
            ),
        )
        .unwrap();
        fs::write(
            agents.join("com.example.prog.plist"),
            "<plist version=\"1.0\"><dict><key>Program</key><string>/opt/tools/prog &amp; co/run</string></dict></plist>",
        )
        .unwrap();
        fs::write(agents.join("notes.txt"), "not a plist").unwrap();
        let gtk = home.join("Code/gtk/.build/arm64-apple-macosx/debug");
        fs::create_dir_all(&gtk).unwrap();
        fs::write(gtk.join("gtk"), b"").unwrap();
        fs::create_dir_all(home.join("bin")).unwrap();
        std::os::unix::fs::symlink(gtk.join("gtk"), home.join("bin/gtk")).unwrap();
        fs::write(home.join("bin/plain-script"), b"#!/bin/sh\n").unwrap();
        fs::create_dir_all(home.join(".local/bin")).unwrap();
        // A relative symlink, and a dangling one (the binary already moved).
        std::os::unix::fs::symlink("../../Code/proj/.venv/bin/python", home.join(".local/bin/py")).unwrap();
        std::os::unix::fs::symlink(home.join("Code/gone/target/release/gone"), home.join(".local/bin/gone")).unwrap();
        (tmp, home)
    }

    fn sources_for(reg: &LaunchRegistry, target: &Path) -> Vec<String> {
        reg.registrations
            .iter()
            .filter(|r| r.target == target)
            .map(|r| r.source.clone())
            .collect()
    }

    #[test]
    fn reads_every_source_and_ignores_bare_command_names() {
        let (_tmp, home) = home();
        let reg = LaunchRegistry::load(&home);
        let targets: BTreeSet<PathBuf> = reg.registrations.iter().map(|r| r.target.clone()).collect();
        let expected: BTreeSet<PathBuf> = [
            home.join("Code/vigil/rust/target/release/vigil-mcp"),
            PathBuf::from("/opt/suds/wheel"),
            home.join("Code/proj/.venv/bin/python"),
            home.join(".claude/hooks/guard.sh"),
            PathBuf::from("/tmp/ledger"),
            home.join("Library/Application Support/example/bin/example-api"),
            PathBuf::from("/opt/tools/prog & co/run"),
            home.join("Code/gtk/.build/arm64-apple-macosx/debug/gtk"),
            home.join(".local/bin/../../Code/proj/.venv/bin/python"),
            home.join("Code/gone/target/release/gone"),
        ]
        .into_iter()
        .collect();
        assert_eq!(targets, expected, "{:#?}", reg.registrations);
        // Bare names never become targets: builder-mcp, uvx, npx, -m, --port.
        assert!(reg.registrations.iter().all(|r| r.target.is_absolute()), "{:#?}", reg.registrations);
        // The reason text names the registration, not just the file.
        assert_eq!(
            sources_for(&reg, &home.join("Code/vigil/rust/target/release/vigil-mcp")),
            ["MCP server \"vigil\" (~/.claude.json)"]
        );
        assert_eq!(
            sources_for(&reg, &home.join(".claude/hooks/guard.sh")),
            ["PreToolUse hook (~/.claude/settings.json)"]
        );
        assert_eq!(
            sources_for(&reg, &home.join("Library/Application Support/example/bin/example-api")),
            ["LaunchAgent com.example.api (~/Library/LaunchAgents/com.example.api.plist)"]
        );
        assert_eq!(
            sources_for(&reg, &home.join("Code/gtk/.build/arm64-apple-macosx/debug/gtk")),
            ["the ~/bin/gtk symlink"]
        );
        let py = sources_for(&reg, &home.join("Code/proj/.venv/bin/python"));
        assert!(py.iter().any(|s| s.ends_with("/Code/proj/.mcp.json)")), "{py:?}");
        // StandardOutPath is not a launch path.
        assert!(!targets.contains(&home.join("Library/Logs/example.log")));
    }

    /// The vigil case exactly: the plan path is the `target/` directory,
    /// three components above the registered binary.
    #[test]
    fn a_plan_path_that_contains_a_registered_binary_is_a_launch_path_with_the_reason() {
        let (_tmp, home) = home();
        let reg = LaunchRegistry::load(&home);
        let target = home.join("Code/vigil/rust/target");
        let reason = reg.launch_path_for(target.to_str().unwrap()).expect("held");
        assert_eq!(
            reason,
            format!(
                "{}/release/vigil-mcp is the launch path for MCP server \"vigil\" (~/.claude.json)",
                target.display()
            )
        );
        // The binary itself, and the release/ directory between, are held too.
        assert!(reg.launch_path_for(&format!("{}/release", target.display())).is_some());
        assert!(reg.launch_path_for(&format!("{}/release/vigil-mcp", target.display())).is_some());
        // A sibling, a prefix-but-not-component, and the project root's
        // other artifacts are not.
        assert_eq!(reg.launch_path_for(&format!("{}-old", target.display())), None);
        assert_eq!(reg.launch_path_for(home.join("Code/vigil/rust/node_modules").to_str().unwrap()), None);
        assert_eq!(reg.launch_path_for(home.join("Code/other/target").to_str().unwrap()), None);
    }

    #[test]
    fn a_symlink_chain_and_a_dangling_shim_both_hold_their_targets() {
        let (_tmp, home) = home();
        let reg = LaunchRegistry::load(&home);
        // ~/.local/bin/py is a RELATIVE link to the venv python; the venv
        // directory is held through canonicalization of the raw target.
        let venv = home.join("Code/proj/.venv");
        let reason = reg.launch_path_for(venv.to_str().unwrap()).expect("held");
        assert!(reason.contains("the ~/.local/bin/py symlink"), "{reason}");
        assert!(reason.contains("MCP server \"proj\""), "{reason}");
        // The gone binary's target/ is still held: the registration exists
        // even though the file does not (that is precisely the post-incident state).
        let gone = home.join("Code/gone/target");
        assert_eq!(
            reg.launch_path_for(gone.to_str().unwrap()).unwrap(),
            format!("{}/release/gone is the launch path for the ~/.local/bin/gone symlink", gone.display())
        );
        // The SwiftPM .build shim.
        assert!(reg.launch_path_for(home.join("Code/gtk/.build").to_str().unwrap()).is_some());
    }

    #[test]
    fn an_empty_or_absent_home_holds_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = LaunchRegistry::load(tmp.path());
        assert!(reg.registrations.is_empty());
        assert_eq!(reg.launch_path_for("/anything"), None);
        let reg = LaunchRegistry::load(&tmp.path().join("does-not-exist"));
        assert!(reg.registrations.is_empty());
        // Malformed sources contribute nothing rather than failing the plan.
        fs::write(tmp.path().join(".claude.json"), "{ not json").unwrap();
        fs::create_dir_all(tmp.path().join("Library/LaunchAgents")).unwrap();
        fs::write(tmp.path().join("Library/LaunchAgents/bad.plist"), b"bplist00\x00\x01binary").unwrap();
        assert!(LaunchRegistry::load(tmp.path()).registrations.is_empty());
    }

    #[test]
    fn plist_reader_takes_program_and_program_arguments_only() {
        let xml = "<dict><key>Program</key><string>/a/b</string><key>ProgramArguments</key><array><string>/a/b</string><string>--x</string></array><key>WorkingDirectory</key><string>/w</string></dict>";
        assert_eq!(plist_program_strings(xml), ["/a/b", "/a/b", "--x"]);
        assert!(plist_program_strings("<dict><key>Label</key><string>x</string></dict>").is_empty());
        // A key whose value is an unexpected element yields nothing.
        assert!(plist_program_strings("<dict><key>Program</key><true/></dict>").is_empty());
    }

    #[test]
    fn normalize_resolves_dot_and_dotdot_without_touching_the_disk() {
        assert_eq!(normalize(Path::new("/h/.local/bin/../../Code/x")), PathBuf::from("/h/Code/x"));
        assert_eq!(normalize(Path::new("/h/./a/./b")), PathBuf::from("/h/a/b"));
        assert_eq!(normalize(Path::new("/a/target")), PathBuf::from("/a/target"));
    }

    #[test]
    fn path_tokens_take_only_absolute_and_tilde_paths() {
        let home = Path::new("/h");
        assert_eq!(
            path_tokens("~/.claude/hooks/x.sh --flag '/tmp/a b' name /var/y", home),
            [PathBuf::from("/h/.claude/hooks/x.sh"), PathBuf::from("/tmp/a"), PathBuf::from("/var/y")]
        );
        assert!(path_tokens("npx -y something", home).is_empty());
        assert_eq!(as_path("~", home), None, "a bare ~ is a directory, not a launch target");
    }
}
