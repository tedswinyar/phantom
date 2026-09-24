// The CLI's argument contract against the real binary, offline
// (phantom-b7u): usage errors are exit 2 with the message on stderr, an
// unreachable server is exit 4, and the two offline subcommands produce
// their artifacts without touching the network.

use std::process::Command;

struct Run {
    code: i32,
    out: String,
    err: String,
}

fn phantom(args: &[&str]) -> Run {
    let o = Command::new(env!("CARGO_BIN_EXE_phantom"))
        .args(args)
        .env("PHANTOM_API_URL", "http://127.0.0.1:1") // nothing listens on port 1
        .env("PHANTOM_API_KEY", "not-a-real-key")
        .output()
        .expect("run phantom");
    Run {
        code: o.status.code().unwrap_or(-1),
        out: String::from_utf8_lossy(&o.stdout).into_owned(),
        err: String::from_utf8_lossy(&o.stderr).into_owned(),
    }
}

#[test]
fn usage_errors_exit_2_before_any_network() {
    for (args, needle) in [
        (vec!["frobnicate"], "unrecognized subcommand"),
        (vec!["scan"], "required"),                          // path missing
        (vec!["diff", "11111111-1111-4111-8111-111111111111"], "required"), // one of a pair
        (vec!["diff", "not-a-uuid", "also-not"], "invalid value"),
        (vec!["diff", "--since", "soon"], "since"),          // parsed before the request
        (vec!["growth", "--group-by", "bogus"], "invalid value"),
        (vec!["completions", "klingon"], "invalid value"),
        (vec!["top", "--limit", "many"], "invalid value"),
        (vec!["stale", "--scan", "nope"], "invalid value"),
        // The positional scan id is an ALIAS for --scan, never a second scan:
        // both at once is a usage error, even when they agree (phantom-cnr.4).
        // Mutation target: drop conflicts_with and this passes through to the
        // network (exit 4), failing here.
        (
            vec![
                "hotspots",
                "11111111-1111-4111-8111-111111111111",
                "--scan",
                "11111111-1111-4111-8111-111111111111",
            ],
            "cannot be used with",
        ),
        (vec!["top", "not-a-uuid"], "invalid value"),
        (vec!["volume", "--scan", "nope"], "invalid value"),
        (vec!["--bogus-flag", "health"], "unexpected argument"),
    ] {
        let r = phantom(&args);
        assert_eq!(r.code, 2, "{args:?}: stdout={:?} stderr={:?}", r.out, r.err);
        assert!(r.out.is_empty(), "{args:?}: usage errors keep stdout clean for pipelines");
        assert!(r.err.to_lowercase().contains(needle), "{args:?}: stderr={:?}", r.err);
    }
}

#[test]
fn every_subcommand_has_help_that_exits_0() {
    for sub in [
        "scan", "scans", "scans list", "scans show", "scans cancel", "scans delete", "top", "tree", "types",
        "hotspots", "diff", "plan", "verify", "explain", "stale", "volume", "growth", "health", "completions",
        "man",
    ] {
        let mut args: Vec<&str> = sub.split(' ').collect();
        args.push("--help");
        let r = phantom(&args);
        assert_eq!(r.code, 0, "{sub} --help: {}", r.err);
        assert!(r.out.contains("Usage:"), "{sub} --help: {}", r.out);
    }
}

#[test]
fn an_unreachable_server_is_exit_4_with_the_reason_on_stderr() {
    for args in [vec!["health"], vec!["scans", "list"], vec!["volume"], vec!["growth", "/x"]] {
        let r = phantom(&args);
        assert_eq!(r.code, 4, "{args:?}: {} {}", r.out, r.err);
        assert!(r.out.is_empty(), "{args:?}: nothing on stdout when the server is down");
        assert!(r.err.contains("phantom:"), "{args:?}: {}", r.err);
    }
}

#[test]
fn offline_subcommands_need_no_server() {
    let r = phantom(&["completions", "zsh"]);
    assert_eq!(r.code, 0, "{}", r.err);
    assert!(r.out.starts_with("#compdef phantom"), "{}", &r.out[..r.out.len().min(40)]);
    let r = phantom(&["completions", "bash"]);
    assert!(r.out.contains("_phantom"), "bash completion function");
    let r = phantom(&["completions", "fish"]);
    assert!(r.out.contains("complete -c phantom"));
    let r = phantom(&["man"]);
    assert_eq!(r.code, 0);
    assert!(r.out.contains(".TH phantom 1"), "roff title line");
    for sub in ["scan", "growth", "diff"] {
        assert!(r.out.contains(sub), "man page lists subcommand {sub}");
    }
    let r = phantom(&["--version"]);
    assert_eq!(r.code, 0);
    // "phantom 1.1.1" on a release build, "phantom 1.1.1 (v1.1.0-14-gabc-dirty)"
    // on any other: the Cargo version is always the second word, and anything
    // after it is the describe string in parentheses (phantom-cnr.6).
    let expected_prefix = format!("phantom {}", env!("CARGO_PKG_VERSION"));
    let out = r.out.trim_end();
    assert!(
        out.starts_with(&expected_prefix),
        "--version must lead with the Cargo version: {out:?}"
    );
    let rest = &out[expected_prefix.len()..];
    assert!(
        rest.is_empty() || (rest.starts_with(" (") && rest.ends_with(')')),
        "anything after the version is a parenthesised describe string: {out:?}"
    );
}
