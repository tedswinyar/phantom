//! git-describe.rs — ONE build script shared by phantom-cli, phantom-mcp and
//! phantom-api (`build = "../build-support/git-describe.rs"` in each Cargo.toml).
//!
//! An untagged build must say so (phantom-cnr.6): on 2026-09-16 every binary on
//! main reported 1.0.0 while carrying every post-1.0 fix, and `--version` could
//! not tell a dogfood build from the release. Two compile-time env vars:
//!
//!   PHANTOM_BUILD_DESCRIBE   `git describe --tags --dirty --always` of the source
//!                            tree, or the PHANTOM_GIT_DESCRIBE override, or
//!                            "release" when there is no .git (a source tarball)
//!   PHANTOM_VERSION_DISPLAY  what `--version` prints after the binary name:
//!                            "1.1.1" when the describe IS the tag v1.1.1 (or no
//!                            git), else "1.1.1 (v1.1.0-14-g3896576-dirty)"
//!
//! release.sh builds BEFORE it tags (the DMG is built from the commit that
//! becomes the release commit's parent), so the shipped binary can never see its
//! own tag; release.sh therefore exports PHANTOM_GIT_DESCRIBE=v<version> for the
//! build and asserts afterwards that `phantom --version` prints exactly
//! "phantom <version>". GET /health and the MCP serverInfo keep the plain Cargo
//! version — adding a build key is additive wire and belongs to v1.2.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let pkg = std::env::var("CARGO_PKG_VERSION").expect("cargo sets CARGO_PKG_VERSION");
    println!("cargo:rerun-if-env-changed=PHANTOM_GIT_DESCRIBE");
    let describe = match std::env::var("PHANTOM_GIT_DESCRIBE") {
        Ok(d) if !d.trim().is_empty() => d.trim().to_string(),
        _ => git_describe(),
    };
    let display = if describe == format!("v{pkg}") || describe == "release" {
        pkg.clone()
    } else {
        format!("{pkg} ({describe})")
    };
    println!("cargo:rustc-env=PHANTOM_BUILD_DESCRIBE={describe}");
    println!("cargo:rustc-env=PHANTOM_VERSION_DISPLAY={display}");
}

fn git_describe() -> String {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR");
    let git = |args: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(&manifest_dir)
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    let Some(git_dir) = git(&["rev-parse", "--git-dir"]) else {
        return "release".to_string();
    };
    // Rebuild when HEAD or the tags move. The index is deliberately NOT listed:
    // `git describe --dirty` refreshes it, and a build script that watches a
    // file its own command touches rebuilds forever. Dirtiness is therefore
    // as of the last build that re-ran this script — good enough for a label.
    let git_dir_path = if Path::new(&git_dir).is_absolute() {
        PathBuf::from(&git_dir)
    } else {
        Path::new(&manifest_dir).join(&git_dir)
    };
    for f in ["HEAD", "packed-refs", "refs/tags"] {
        println!("cargo:rerun-if-changed={}", git_dir_path.join(f).display());
    }
    git(&["describe", "--tags", "--dirty", "--always"]).unwrap_or_else(|| "unknown".to_string())
}
