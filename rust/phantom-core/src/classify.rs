// Reclaimability classification — a pure post-pass over scan entries.
//
// Every rule in here was earned in a real cleanup incident (see
// docs/reclaimability.md):
//
// - `disk_size` (st_blocks × 512) is THE size. `logical_size` exists to
//   detect cloud-dataloaded placeholders (logical ≫ disk), where `du`
//   once overstated a 147 MB OneDrive tree whose physical footprint was ~0.
//   Since v1.1 the walker's `dataless` flag is the primary signal; the
//   ratio heuristic only speaks for rows that carry no flags.
// - Hardlinked entries (nlink > 1) share blocks: deleting one path frees
//   nothing until the last link goes. Reclaim estimates count each
//   (dev, ino) once — "17 GB" of ~/.cache/uv freed only 5 GB.
// - Project staleness comes from full-depth SOURCE file mtimes and git
//   activity, never artifact mtimes (cargo-sweep touches target/ on every
//   run) and never a depth-capped walk (a -maxdepth 3 check misread a
//   project edited that morning as dormant; five active projects lost
//   their targets).
// - An artifact counts only next to its DETECTION file: a bare `target/`
//   is a directory that happens to be called target.
// - Phantom NEVER deletes. Categories carry action HINTS; the API surface
//   shows and suggests only.

use std::collections::{BTreeMap, HashMap, HashSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::format::{ChargeKey, format_size};
use crate::scan::{EntryFlags, ScanEntry};
use crate::share::ShareLedger;
use crate::{CoreError, Result};

/// A cloud-dataloaded placeholder shows at least this many times more
/// logical than physical bytes…
pub const CLOUD_DATALOADED_MIN_RATIO: u64 = 8;
/// …and at least this much logical size. Without an absolute floor, every
/// tiny file whose tail block rounds oddly would look "dataloaded".
pub const CLOUD_DATALOADED_MIN_LOGICAL: u64 = 1024 * 1024; // 1 MiB

/// A project whose newest source mtime / git activity is at least this
/// old is dormant. The default; `ClassifyOptions::dormant_after_days`
/// (`--older`, `olderThan`) overrides it per scan.
pub const DORMANT_AFTER_DAYS: i64 = 90;

/// Git pack files above this physical size are worth surfacing on their own.
pub const GIT_PACK_REVIEW_MIN_DISK: u64 = 200 * 1024 * 1024; // 200 MiB

/// Plain files above this physical size are surfaced even when no other
/// rule knows anything about them.
pub const LARGE_FILE_REVIEW_MIN_DISK: u64 = 1024 * 1024 * 1024; // 1 GiB

/// A group lists EVERY root worth acting on (1.1.1, phantom-cnr.5): each
/// root whose deletion-honest private bytes reach
/// [`TOP_PATH_WORTH_ACTING_BYTES`], never fewer than [`TOP_PATHS_FLOOR`]
/// (the 1.1.0 behaviour for small groups — the largest roots by deduped
/// disk) and never more than [`TOP_PATHS_CAP`] (a `node_modules` group with
/// 400 roots must not flood a response). The plan is built from this list,
/// so before 1.1.1 a group of seven 9–26 GB worktree targets offered five
/// and reclaiming the rest took a second scan+plan cycle (2026-09-16).
pub const TOP_PATHS_FLOOR: usize = 5;
pub const TOP_PATHS_CAP: usize = 25;
pub const TOP_PATH_WORTH_ACTING_BYTES: u64 = 1024 * 1024 * 1024; // 1 GiB

/// Files under `.git` whose mtime moves when HEAD moves: a commit, merge,
/// checkout, reset or pull. The reflog's mtime IS the time of the last
/// HEAD update, which for a commit is the commit time — read without
/// opening the repository (the classifier never touches the filesystem).
/// `FETCH_HEAD` and `index` are deliberately absent: a background fetch or
/// a `git status` rewrites them without any human activity.
pub const GIT_ACTIVITY_FILES: &[&str] = &["logs/HEAD", "COMMIT_EDITMSG", "ORIG_HEAD", "HEAD"];

/// Reclaimability category. Stored in `entries.category` as the camelCase
/// wire string; NEVER an instruction to delete — Phantom shows and suggests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Category {
    /// A build regenerates it (cargo target/, node_modules, …).
    RegenerableArtifact,
    /// An app or OS cache; the owner rebuilds it on demand.
    Cache,
    /// A cache OWNED by a tool that must do its own cleanup.
    ToolManagedCache,
    /// Downloaded AI model weights (Ollama, Hugging Face hub, Whisper,
    /// PyTorch hub, vLLM/Triton): re-downloadable, but gigabytes each.
    ModelCache,
    /// Cloud placeholder: big logical size, ~zero blocks on disk.
    CloudDataloaded,
    /// Regenerable artifact inside a dormant project — top of the list.
    StaleProjectArtifact,
    /// Big and unclassified, or possibly holding un-backed-up state.
    ReviewFirst,
    /// Deleting it loses data (e.g. cloud-synced originals).
    WontRegenerate,
}

impl Category {
    pub fn as_str(&self) -> &'static str {
        match self {
            Category::RegenerableArtifact => "regenerableArtifact",
            Category::Cache => "cache",
            Category::ToolManagedCache => "toolManagedCache",
            Category::ModelCache => "modelCache",
            Category::CloudDataloaded => "cloudDataloaded",
            Category::StaleProjectArtifact => "staleProjectArtifact",
            Category::ReviewFirst => "reviewFirst",
            Category::WontRegenerate => "wontRegenerate",
        }
    }

    /// Every category, for exhaustive tests and docs.
    pub const ALL: [Category; 8] = [
        Category::RegenerableArtifact,
        Category::Cache,
        Category::ToolManagedCache,
        Category::ModelCache,
        Category::CloudDataloaded,
        Category::StaleProjectArtifact,
        Category::ReviewFirst,
        Category::WontRegenerate,
    ];

    /// Category-level action hint. Registry rows may carry a sharper,
    /// tool-specific hint; this is the fallback the UI can always show.
    pub fn action_hint(&self) -> &'static str {
        match self {
            Category::RegenerableArtifact => "safe to delete; the next build regenerates it",
            Category::Cache => "safe to delete; the owning app rebuilds it on demand",
            Category::ToolManagedCache => {
                "use the owning tool's clean command (e.g. `toolbox clean`), not rm -rf"
            }
            Category::ModelCache => {
                "downloaded model weights; the tool re-downloads them on next use — remove with the tool, not rm -rf"
            }
            Category::CloudDataloaded => {
                "placeholder only — contents live in the cloud; deleting frees almost nothing"
            }
            Category::StaleProjectArtifact => {
                "regenerable artifact in a dormant project; best reclaim candidate"
            }
            Category::ReviewFirst => "review before touching; may hold un-backed-up state",
            Category::WontRegenerate => "deleting loses data; not reclaimable",
        }
    }

    /// True when deleting frees the space without losing anything a build,
    /// tool, or app cannot recreate. Only these count toward the reclaim
    /// estimate.
    pub fn is_reclaimable(&self) -> bool {
        matches!(
            self,
            Category::RegenerableArtifact
                | Category::Cache
                | Category::ToolManagedCache
                | Category::ModelCache
                | Category::StaleProjectArtifact
        )
    }

    /// Sort rank for the summary: best reclaim candidates first.
    fn priority(&self) -> u8 {
        match self {
            Category::StaleProjectArtifact => 0,
            Category::RegenerableArtifact => 1,
            Category::ToolManagedCache => 2,
            Category::Cache => 3,
            Category::ModelCache => 4,
            Category::CloudDataloaded => 5,
            Category::ReviewFirst => 6,
            Category::WontRegenerate => 7,
        }
    }
}

impl std::str::FromStr for Category {
    type Err = CoreError;

    fn from_str(s: &str) -> Result<Self> {
        Category::ALL
            .iter()
            .copied()
            .find(|c| c.as_str() == s)
            .ok_or_else(|| CoreError::InvalidInput(format!("unknown category: {s:?}")))
    }
}

/// How risky deleting a group is, stated instead of implied by the
/// category (v1.1, phantom-mkn.4). The rubric lives in
/// docs/reclaimability.md; the classifier applies it in [`tier_for`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RiskTier {
    /// Regenerable with pinned inputs, or a cache its owner repopulates.
    Safe,
    /// Regenerable, but the rebuild may differ (no lockfile), costs a large
    /// download, or must go through the owning tool's own command.
    Caution,
    /// Deleting may lose data, or frees nothing worth having. The decode
    /// default, so a summary persisted before v1.1 reads as unrated rather
    /// than as safe.
    #[default]
    Review,
}

impl RiskTier {
    pub fn as_str(&self) -> &'static str {
        match self {
            RiskTier::Safe => "safe",
            RiskTier::Caution => "caution",
            RiskTier::Review => "review",
        }
    }
}

/// What getting the bytes back costs, in kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RebuildKind {
    /// Fetched again from a network source (packages, model weights).
    Download,
    /// Rebuilt locally from sources (build output, bytecode, indexes).
    Compile,
    /// No rebuild step exists: either nothing is lost (a cache the owner
    /// repopulates, a placeholder) or nothing can bring it back (the tier
    /// and `why` say which).
    #[default]
    None,
}

/// Rebuild cost on the wire: the kind plus a one-line human estimate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RebuildCost {
    pub kind: RebuildKind,
    pub estimate: String,
}

impl Default for RebuildCost {
    /// The decode default for summaries persisted before v1.1.
    fn default() -> Self {
        RebuildCost {
            kind: RebuildKind::None,
            estimate: "not assessed (classified before v1.1)".to_string(),
        }
    }
}

fn legacy_why() -> String {
    "classified before v1.1; tier not assessed — rescan for a rating".to_string()
}

/// A tool's OWN dry-run number for a group (v1.1, phantom-mkn.19): opt-in,
/// from a fixed absolute path, never from `$PATH`. Nullable on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolEstimate {
    /// The tool that produced it (`docker`, `brew`, `uv`).
    pub tool: String,
    /// The exact read-only command that was run.
    pub command: String,
    /// What the tool says its own cleanup would free, in bytes.
    pub reclaimable_bytes: u64,
    /// One line of context: what the number covers, or why it is partial.
    pub note: String,
}

/// Whether a regenerable artifact's inputs are pinned, and whether that was
/// checked. Part of the group key: the `why` sentence names it, so every
/// root in a group must share it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum LockState {
    /// The rule has no lockfile concept (compile output, caches).
    NotApplicable,
    /// A lockfile sits beside the detection file.
    Present,
    /// Present AND the opt-in read-only verify command succeeded.
    Verified,
    /// Present but the verify command failed: the lockfile is out of date.
    Failed,
    /// No lockfile beside the detection file (or the parent is outside
    /// the scan, so it cannot be observed).
    Missing,
}

/// Outcome of the opt-in lockfile verification (`ClassifyOptions::verify`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockVerdict {
    Verified,
    /// The verify command exited non-zero or timed out; the text is a
    /// short, human reason.
    Failed(String),
    /// The tool was not found at any fixed path, or the run was skipped.
    Unavailable,
}

/// The read-only command that checks a lockfile against its manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockVerify {
    /// Only this lockfile makes the command meaningful (`npm ci` cannot
    /// read a pnpm lock).
    pub requires: &'static str,
    /// Tool name; the probe module maps it to fixed absolute paths.
    pub tool: &'static str,
    /// Arguments after the tool. Nothing here writes.
    pub args: &'static [&'static str],
}

/// How a registry row recognizes its hotspot. All path matching is
/// COMPONENT-boundary aware: `node_modules_backup` never matches a
/// `node_modules` rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Matcher {
    /// A directory whose final path component equals `name` exactly, with
    /// no proof required: the name is unambiguous (`node_modules`).
    DirNamed { name: &'static str },
    /// A directory named like one of `artifacts` whose PARENT holds one of
    /// `detection` — the detection-file → artifact-dirs table. Patterns:
    /// exact name, `*.ext` (any file with that extension), or `prefix-*`.
    /// Parent outside the scan ⇒ unprovable ⇒ no match.
    ProjectArtifact {
        detection: &'static [&'static str],
        artifacts: &'static [&'static str],
    },
    /// A directory whose path ends with exactly these components.
    DirSuffix { components: &'static [&'static str] },
    /// A directory whose PARENT path ends with exactly these components —
    /// each child of `…/Library/Caches` is a root, never the directory
    /// itself. For directories macOS will not let the owner rename
    /// (phantom-11h): the plan's paths must be things that can move.
    ChildOfDirSuffix { components: &'static [&'static str] },
    /// A directory named one of `names` somewhere below a `within`
    /// component (Electron caches under Application Support).
    DirNamedWithin {
        names: &'static [&'static str],
        within: &'static str,
    },
    /// A `*.pack` file under `objects/pack/` bigger than `min_disk_size`.
    GitPackFile { min_disk_size: u64 },
    /// Any file the walker flagged `dataless`, or — for rows without flags
    /// — whose logical size dwarfs its physical size (see the
    /// CLOUD_DATALOADED_* constants). Evaluated FIRST, as a per-file
    /// override: a dataloaded file inside node_modules still frees ~nothing.
    CloudDataloadedFile,
    /// Any otherwise-unmatched file at least `min_disk_size` on disk.
    /// Must be the LAST row: it is the catch-all.
    LargeFile { min_disk_size: u64 },
}

/// One row of the hotspot registry: recognize → categorize → hint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HotspotRule {
    /// Stable group key (survives label edits; safe to pin in clients).
    pub id: &'static str,
    pub label: &'static str,
    pub matcher: Matcher,
    pub category: Category,
    /// Human advice. Purely illustrative text — backticks in it are
    /// typography, never semantics; clients must not parse it.
    pub hint: &'static str,
    /// The ONE safe, copy-runnable cleanup command, or None
    /// when no such command honestly exists: advice-only rules (a `git
    /// status` CHECK is not a cleanup), mixed-tool rules that cannot know
    /// the tool, rules whose only "cleanup" is deleting the directory
    /// itself (Phantom never emits rm -rf), and every reviewFirst /
    /// wontRegenerate / cloudDataloaded row.
    pub command: Option<&'static str>,
    /// The first clause of the group's `why` sentence: what this is and
    /// what brings it back. The classifier appends the lockfile and
    /// staleness clauses.
    pub why: &'static str,
    /// Lockfiles that pin this artifact's inputs; any one beside the
    /// detection file makes the tier `safe`. Empty = no lockfile concept.
    pub lockfiles: &'static [&'static str],
    /// The read-only command that checks the lockfile (opt-in).
    pub verify: Option<LockVerify>,
    pub rebuild: RebuildKind,
    /// Keep this root as its own group even inside an enclosing hotspot
    /// root (the enclosing group's totals then exclude it). For specific
    /// stores that would otherwise vanish into a generic parent (`~/.cache`).
    pub carve_out: bool,
}

/// Shorthand for the registry rows that carry no lockfile/verify/carve-out.
macro_rules! rule {
    (
        id: $id:expr, label: $label:expr, matcher: $matcher:expr, category: $category:expr,
        hint: $hint:expr, command: $command:expr, why: $why:expr, rebuild: $rebuild:expr
        $(, lockfiles: $lockfiles:expr)? $(, verify: $verify:expr)? $(, carve_out: $carve:expr)?
    ) => {
        HotspotRule {
            id: $id,
            label: $label,
            matcher: $matcher,
            category: $category,
            hint: $hint,
            command: $command,
            why: $why,
            rebuild: $rebuild,
            lockfiles: rule!(@opt &[], $($lockfiles)?),
            verify: rule!(@opt None, $($verify)?),
            carve_out: rule!(@opt false, $($carve)?),
        }
    };
    (@opt $default:expr, ) => { $default };
    (@opt $default:expr, $value:expr) => { $value };
}

const JS_LOCKFILES: &[&str] = &["package-lock.json", "pnpm-lock.yaml", "yarn.lock", "bun.lock", "bun.lockb"];
const PY_LOCKFILES: &[&str] = &["uv.lock", "poetry.lock", "Pipfile.lock", "pdm.lock"];
const PY_MANIFESTS: &[&str] = &["pyproject.toml", "setup.py", "setup.cfg", "requirements.txt", "Pipfile", "tox.ini"];

const CARGO_VERIFY: LockVerify = LockVerify {
    requires: "Cargo.lock",
    tool: "cargo",
    args: &["metadata", "--locked", "--offline", "--no-deps", "--format-version", "1"],
};
const NPM_VERIFY: LockVerify = LockVerify {
    requires: "package-lock.json",
    tool: "npm",
    args: &["ci", "--dry-run", "--ignore-scripts", "--offline", "--no-audit", "--no-fund"],
};
const UV_VERIFY: LockVerify = LockVerify {
    requires: "uv.lock",
    tool: "uv",
    args: &["lock", "--locked", "--offline"],
};

/// The hotspot registry. Knowledge lives in DATA rows, not a function per
/// case — adding a hotspot is adding a row. Ordering is precedence:
/// the first matching row wins for an entry. The project rows are the
/// detection-file → artifact-dirs table (kondo's 24 project types plus
/// Bazel and Go vendor; docs/reclaimability.md has the table).
pub const REGISTRY: &[HotspotRule] = &[
    rule! {
        id: "cloud-dataloaded",
        label: "Cloud-dataloaded placeholders",
        matcher: Matcher::CloudDataloadedFile,
        category: Category::CloudDataloaded,
        hint: "contents live in the cloud; local blocks are ~0 — deleting frees almost nothing",
        command: None,
        why: "a cloud placeholder whose contents are not local; deleting it frees almost no blocks",
        rebuild: RebuildKind::None
    },
    // --- project artifacts: an artifact counts only next to its detection file
    rule! {
        id: "cargo-target",
        label: "Rust target/ directories",
        matcher: Matcher::ProjectArtifact { detection: &["Cargo.toml"], artifacts: &["target"] },
        category: Category::RegenerableArtifact,
        hint: "`cargo clean` or delete; the next `cargo build` regenerates it",
        command: Some("cargo clean"),
        why: "Cargo build output beside a Cargo.toml; `cargo build` recreates it",
        rebuild: RebuildKind::Compile,
        lockfiles: &["Cargo.lock"],
        verify: Some(CARGO_VERIFY)
    },
    rule! {
        id: "node-modules",
        label: "node_modules directories",
        matcher: Matcher::DirNamed { name: "node_modules" },
        category: Category::RegenerableArtifact,
        hint: "`npm install` / `pnpm install` regenerates it",
        command: None,
        why: "installed JavaScript dependencies; the package manager reinstalls them",
        rebuild: RebuildKind::Download,
        lockfiles: JS_LOCKFILES,
        verify: Some(NPM_VERIFY)
    },
    rule! {
        id: "python-venv",
        label: "Python virtualenvs",
        matcher: Matcher::DirNamed { name: ".venv" },
        category: Category::RegenerableArtifact,
        hint: "recreate with `uv venv` / `python -m venv` and reinstall",
        command: None,
        why: "a Python virtualenv; recreating it reinstalls the packages",
        rebuild: RebuildKind::Download,
        lockfiles: PY_LOCKFILES,
        verify: Some(UV_VERIFY)
    },
    rule! {
        id: "swiftpm-build",
        label: "SwiftPM .build directories",
        matcher: Matcher::ProjectArtifact { detection: &["Package.swift"], artifacts: &[".build", ".swiftpm"] },
        category: Category::RegenerableArtifact,
        hint: "`swift build` regenerates it",
        command: Some("swift package clean"),
        why: "SwiftPM build output beside a Package.swift; `swift build` recreates it",
        rebuild: RebuildKind::Compile,
        lockfiles: &["Package.resolved"]
    },
    rule! {
        id: "next-build",
        label: ".next build output",
        matcher: Matcher::DirNamed { name: ".next" },
        category: Category::RegenerableArtifact,
        hint: "`next build` regenerates it",
        command: None,
        why: "Next.js build output; `next build` recreates it",
        rebuild: RebuildKind::Compile
    },
    // `build`/`dist` are generic names; the package.json detection keeps
    // the rule from eating e.g. this repo's build/Phantom.app.
    rule! {
        id: "js-build",
        label: "JS build/ output",
        matcher: Matcher::ProjectArtifact { detection: &["package.json"], artifacts: &["build"] },
        category: Category::RegenerableArtifact,
        hint: "the package's build script regenerates it",
        command: None,
        why: "build output beside a package.json; the package's build script recreates it",
        rebuild: RebuildKind::Compile
    },
    rule! {
        id: "js-dist",
        label: "JS dist/ output",
        matcher: Matcher::ProjectArtifact { detection: &["package.json"], artifacts: &["dist"] },
        category: Category::RegenerableArtifact,
        hint: "the package's build script regenerates it",
        command: None,
        why: "bundled output beside a package.json; the package's build script recreates it",
        rebuild: RebuildKind::Compile
    },
    rule! {
        id: "react-native-cache",
        label: "React Native / Expo caches",
        matcher: Matcher::ProjectArtifact { detection: &["package.json"], artifacts: &[".expo", ".metro"] },
        category: Category::RegenerableArtifact,
        hint: "Expo / Metro bundler caches; the next `expo start` rebuilds them",
        command: None,
        why: "Expo and Metro bundler caches beside a package.json; the next start rebuilds them",
        rebuild: RebuildKind::Compile
    },
    rule! {
        id: "turborepo-cache",
        label: "Turborepo .turbo caches",
        matcher: Matcher::ProjectArtifact { detection: &["turbo.json"], artifacts: &[".turbo"] },
        category: Category::RegenerableArtifact,
        hint: "Turborepo task cache; the next `turbo run` rebuilds it",
        command: None,
        why: "Turborepo task cache beside a turbo.json; the next run rebuilds it",
        rebuild: RebuildKind::Compile
    },
    rule! {
        id: "gradle-build",
        label: "Gradle build output",
        matcher: Matcher::ProjectArtifact {
            detection: &["build.gradle", "build.gradle.kts", "settings.gradle", "settings.gradle.kts"],
            artifacts: &["build", ".gradle"]
        },
        category: Category::RegenerableArtifact,
        hint: "`gradle clean` or delete; the next build regenerates it",
        command: Some("gradle clean"),
        why: "Gradle build output beside a build.gradle; the next build recreates it",
        rebuild: RebuildKind::Compile
    },
    rule! {
        id: "maven-target",
        label: "Maven target/ directories",
        matcher: Matcher::ProjectArtifact { detection: &["pom.xml"], artifacts: &["target"] },
        category: Category::RegenerableArtifact,
        hint: "`mvn clean` or delete; the next `mvn package` regenerates it",
        command: Some("mvn clean"),
        why: "Maven build output beside a pom.xml; `mvn package` recreates it",
        rebuild: RebuildKind::Compile
    },
    rule! {
        id: "sbt-target",
        label: "sbt target/ directories",
        matcher: Matcher::ProjectArtifact { detection: &["build.sbt"], artifacts: &["target"] },
        category: Category::RegenerableArtifact,
        hint: "`sbt clean` or delete; the next `sbt compile` regenerates it",
        command: Some("sbt clean"),
        why: "sbt build output beside a build.sbt; `sbt compile` recreates it",
        rebuild: RebuildKind::Compile
    },
    rule! {
        id: "cmake-build",
        label: "CMake build directories",
        matcher: Matcher::ProjectArtifact { detection: &["CMakeLists.txt"], artifacts: &["build", "cmake-build-*"] },
        category: Category::RegenerableArtifact,
        hint: "delete and re-run `cmake -B build`; the next build regenerates it",
        command: None,
        why: "CMake build tree beside a CMakeLists.txt; configuring again recreates it",
        rebuild: RebuildKind::Compile
    },
    rule! {
        id: "unity-library",
        label: "Unity Library / Temp / Obj / Logs",
        matcher: Matcher::ProjectArtifact {
            detection: &["Assembly-CSharp.csproj", "ProjectSettings"],
            artifacts: &["Library", "Temp", "Obj", "Logs"]
        },
        category: Category::RegenerableArtifact,
        hint: "Unity reimports the Library on next open (slow for big projects)",
        command: None,
        why: "Unity's import cache and temp output beside the project settings; reopening the project reimports them",
        rebuild: RebuildKind::Compile
    },
    rule! {
        id: "unreal-intermediate",
        label: "Unreal Binaries / Intermediate / DerivedDataCache",
        matcher: Matcher::ProjectArtifact {
            detection: &["*.uproject"],
            artifacts: &["Binaries", "Intermediate", "DerivedDataCache"]
        },
        category: Category::RegenerableArtifact,
        hint: "Unreal regenerates them on the next build / editor open",
        command: None,
        why: "Unreal build intermediates beside a .uproject; the next build recreates them",
        rebuild: RebuildKind::Compile
    },
    rule! {
        id: "python-caches",
        label: "Python tool caches (.mypy_cache, .pytest_cache, .tox, …)",
        matcher: Matcher::ProjectArtifact {
            detection: PY_MANIFESTS,
            artifacts: &[".mypy_cache", ".pytest_cache", ".ruff_cache", ".tox", ".nox", "__pypackages__"]
        },
        category: Category::RegenerableArtifact,
        hint: "the tools rebuild them on the next run",
        command: None,
        why: "Python tool caches beside a project manifest; mypy, pytest, ruff, tox recreate them",
        rebuild: RebuildKind::Compile
    },
    rule! {
        id: "python-bytecode",
        label: "__pycache__ directories",
        matcher: Matcher::ProjectArtifact { detection: &["*.py"], artifacts: &["__pycache__"] },
        category: Category::RegenerableArtifact,
        hint: "Python rewrites bytecode on the next import",
        command: None,
        why: "compiled bytecode beside its .py sources; the next import rewrites it",
        rebuild: RebuildKind::Compile
    },
    rule! {
        id: "jupyter-checkpoints",
        label: "Jupyter .ipynb_checkpoints",
        matcher: Matcher::ProjectArtifact { detection: &["*.ipynb"], artifacts: &[".ipynb_checkpoints"] },
        category: Category::RegenerableArtifact,
        hint: "autosave copies of the notebooks beside them",
        command: None,
        why: "notebook autosave checkpoints beside their .ipynb files; Jupyter recreates them on save",
        rebuild: RebuildKind::None
    },
    rule! {
        id: "pixi-env",
        label: "Pixi .pixi environments",
        matcher: Matcher::ProjectArtifact { detection: &["pixi.toml"], artifacts: &[".pixi"] },
        category: Category::RegenerableArtifact,
        hint: "`pixi install` recreates it",
        command: None,
        why: "Pixi environments beside a pixi.toml; `pixi install` recreates them",
        rebuild: RebuildKind::Download,
        lockfiles: &["pixi.lock"]
    },
    rule! {
        id: "flutter-build",
        label: "Flutter/Dart .dart_tool and build",
        matcher: Matcher::ProjectArtifact { detection: &["pubspec.yaml"], artifacts: &[".dart_tool", "build"] },
        category: Category::RegenerableArtifact,
        hint: "`flutter clean`; the next `flutter pub get` / build regenerates them",
        command: Some("flutter clean"),
        why: "Dart/Flutter build output beside a pubspec.yaml; `flutter build` recreates it",
        rebuild: RebuildKind::Compile,
        lockfiles: &["pubspec.lock"]
    },
    rule! {
        id: "elixir-build",
        label: "Elixir _build directories",
        matcher: Matcher::ProjectArtifact { detection: &["mix.exs"], artifacts: &["_build", ".elixir_ls"] },
        category: Category::RegenerableArtifact,
        hint: "`mix clean`; the next `mix compile` regenerates it",
        command: Some("mix clean"),
        why: "Mix build output beside a mix.exs; `mix compile` recreates it",
        rebuild: RebuildKind::Compile,
        lockfiles: &["mix.lock"]
    },
    rule! {
        id: "zig-cache",
        label: "Zig cache and output",
        matcher: Matcher::ProjectArtifact { detection: &["build.zig"], artifacts: &["zig-cache", ".zig-cache", "zig-out"] },
        category: Category::RegenerableArtifact,
        hint: "the next `zig build` regenerates them",
        command: None,
        why: "Zig build cache and output beside a build.zig; `zig build` recreates them",
        rebuild: RebuildKind::Compile
    },
    rule! {
        id: "godot-import",
        label: "Godot .godot import cache",
        matcher: Matcher::ProjectArtifact { detection: &["project.godot"], artifacts: &[".godot"] },
        category: Category::RegenerableArtifact,
        hint: "Godot reimports assets on next open",
        command: None,
        why: "Godot's import cache beside a project.godot; reopening the project reimports it",
        rebuild: RebuildKind::Compile
    },
    rule! {
        id: "dotnet-bin-obj",
        label: ".NET bin/ and obj/",
        matcher: Matcher::ProjectArtifact { detection: &["*.csproj", "*.fsproj", "*.sln"], artifacts: &["bin", "obj"] },
        category: Category::RegenerableArtifact,
        hint: "`dotnet clean`; the next `dotnet build` regenerates them",
        command: Some("dotnet clean"),
        why: ".NET build output beside a project file; `dotnet build` recreates it",
        rebuild: RebuildKind::Compile
    },
    rule! {
        id: "terraform-providers",
        label: "Terraform .terraform directories",
        matcher: Matcher::ProjectArtifact { detection: &["*.tf", ".terraform.lock.hcl"], artifacts: &[".terraform"] },
        category: Category::RegenerableArtifact,
        hint: "`terraform init` re-downloads providers and modules",
        command: None,
        why: "downloaded Terraform providers and modules beside the configuration; `terraform init` refetches them",
        rebuild: RebuildKind::Download,
        lockfiles: &[".terraform.lock.hcl"]
    },
    rule! {
        id: "cocoapods-pods",
        label: "CocoaPods Pods/ directories",
        matcher: Matcher::ProjectArtifact { detection: &["Podfile"], artifacts: &["Pods"] },
        category: Category::RegenerableArtifact,
        hint: "`pod install` regenerates it",
        command: None,
        why: "installed CocoaPods beside a Podfile; `pod install` reinstalls them",
        rebuild: RebuildKind::Download,
        lockfiles: &["Podfile.lock"]
    },
    rule! {
        id: "composer-vendor",
        label: "Composer vendor/ directories",
        matcher: Matcher::ProjectArtifact { detection: &["composer.json"], artifacts: &["vendor"] },
        category: Category::RegenerableArtifact,
        hint: "`composer install` regenerates it",
        command: None,
        why: "installed PHP dependencies beside a composer.json; `composer install` reinstalls them",
        rebuild: RebuildKind::Download,
        lockfiles: &["composer.lock"]
    },
    rule! {
        id: "go-vendor",
        label: "Go vendor/ directories",
        matcher: Matcher::ProjectArtifact { detection: &["go.mod"], artifacts: &["vendor"] },
        category: Category::RegenerableArtifact,
        hint: "`go mod vendor` regenerates it",
        command: None,
        why: "vendored Go modules beside a go.mod; `go mod vendor` refetches them",
        rebuild: RebuildKind::Download,
        lockfiles: &["go.sum"]
    },
    rule! {
        id: "stack-work",
        label: "Haskell .stack-work directories",
        matcher: Matcher::ProjectArtifact { detection: &["stack.yaml"], artifacts: &[".stack-work"] },
        category: Category::RegenerableArtifact,
        hint: "`stack clean`; the next `stack build` regenerates it",
        command: Some("stack clean"),
        why: "Stack build output beside a stack.yaml; `stack build` recreates it",
        rebuild: RebuildKind::Compile
    },
    rule! {
        id: "cabal-dist",
        label: "Haskell dist-newstyle directories",
        matcher: Matcher::ProjectArtifact { detection: &["cabal.project", "*.cabal"], artifacts: &["dist-newstyle"] },
        category: Category::RegenerableArtifact,
        hint: "`cabal clean`; the next `cabal build` regenerates it",
        command: Some("cabal clean"),
        why: "Cabal build output beside a cabal project; `cabal build` recreates it",
        rebuild: RebuildKind::Compile
    },
    rule! {
        id: "bazel-output",
        label: "Bazel bazel-* output links",
        matcher: Matcher::ProjectArtifact {
            detection: &["WORKSPACE", "WORKSPACE.bazel", "WORKSPACE.bzlmod", "MODULE.bazel"],
            artifacts: &["bazel-*"]
        },
        category: Category::RegenerableArtifact,
        hint: "`bazel clean`; the output base itself lives under the Bazel cache",
        command: Some("bazel clean"),
        why: "Bazel output beside a WORKSPACE or MODULE.bazel; `bazel build` recreates it",
        rebuild: RebuildKind::Compile
    },
    // --- caches
    rule! {
        id: "xcode-derived-data",
        label: "Xcode DerivedData",
        matcher: Matcher::DirNamed { name: "DerivedData" },
        category: Category::Cache,
        hint: "Xcode regenerates it on the next build",
        command: None,
        why: "Xcode's per-project build cache; the next build recreates it",
        rebuild: RebuildKind::Compile
    },
    rule! {
        id: "library-caches",
        label: "Library/Caches",
        // Per CHILD, never the directory: macOS refuses to rename
        // ~/Library/Caches itself even for the owner (phantom-11h), and a
        // TCC-protected container the walker cannot read has no children,
        // so nothing under it is planned.
        matcher: Matcher::ChildOfDirSuffix { components: &["Library", "Caches"] },
        category: Category::Cache,
        hint: "per-app caches; apps rebuild them on demand (Library/Caches itself cannot be moved)",
        command: None,
        why: "a per-app cache folder under Library/Caches; the app repopulates it on demand",
        rebuild: RebuildKind::None
    },
    rule! {
        id: "electron-app-cache",
        label: "Electron app caches",
        matcher: Matcher::DirNamedWithin {
            names: &["Cache", "Code Cache", "GPUCache", "DawnCache", "CachedData", "Service Worker"],
            within: "Application Support"
        },
        category: Category::Cache,
        hint: "Electron/Chromium cache; the app rebuilds it",
        command: None,
        why: "a Chromium-style cache inside an Electron app's support directory; the app rebuilds it",
        rebuild: RebuildKind::None
    },
    rule! {
        id: "group-containers",
        label: "Library/Group Containers",
        matcher: Matcher::DirSuffix { components: &["Library", "Group Containers"] },
        category: Category::ReviewFirst,
        hint: "shared app-group data; apps can lose state — review per container",
        command: None,
        why: "shared app-group data that may hold state no app can rebuild",
        rebuild: RebuildKind::None
    },
    // --- AI model caches: carved out of ~/.cache so they surface on a home scan
    rule! {
        id: "ollama-models",
        label: "Ollama models",
        matcher: Matcher::DirSuffix { components: &[".ollama", "models"] },
        category: Category::ModelCache,
        hint: "`ollama list` then `ollama rm <model>`; `ollama pull` re-downloads",
        command: None,
        why: "downloaded Ollama model weights; `ollama pull` fetches them again",
        rebuild: RebuildKind::Download,
        carve_out: true
    },
    rule! {
        id: "huggingface-hub",
        label: "Hugging Face hub cache",
        matcher: Matcher::DirSuffix { components: &["huggingface", "hub"] },
        category: Category::ModelCache,
        hint: "`huggingface-cli delete-cache` picks revisions; models re-download on next load",
        command: None,
        why: "downloaded Hugging Face models and datasets; the library re-downloads them on next load",
        rebuild: RebuildKind::Download,
        carve_out: true
    },
    rule! {
        id: "whisper-models",
        label: "Whisper models",
        matcher: Matcher::DirSuffix { components: &[".cache", "whisper"] },
        category: Category::ModelCache,
        hint: "Whisper re-downloads a model the next time it is loaded",
        command: None,
        why: "downloaded Whisper model weights; the next load re-downloads them",
        rebuild: RebuildKind::Download,
        carve_out: true
    },
    rule! {
        id: "torch-hub",
        label: "PyTorch hub cache",
        matcher: Matcher::DirSuffix { components: &[".cache", "torch", "hub"] },
        category: Category::ModelCache,
        hint: "`torch.hub` re-downloads checkpoints on next use",
        command: None,
        why: "downloaded PyTorch hub checkpoints; torch.hub re-downloads them on next use",
        rebuild: RebuildKind::Download,
        carve_out: true
    },
    rule! {
        id: "vllm-cache",
        label: "vLLM cache",
        matcher: Matcher::DirSuffix { components: &[".cache", "vllm"] },
        category: Category::ModelCache,
        hint: "vLLM rebuilds its cache on the next server start",
        command: None,
        why: "vLLM's compiled kernels and model cache; the next server start rebuilds it",
        rebuild: RebuildKind::Download,
        carve_out: true
    },
    rule! {
        id: "triton-cache",
        label: "Triton kernel cache",
        matcher: Matcher::DirSuffix { components: &[".triton", "cache"] },
        category: Category::ModelCache,
        hint: "Triton recompiles kernels on demand",
        command: None,
        why: "Triton's compiled GPU kernels; they recompile on demand",
        rebuild: RebuildKind::Compile,
        carve_out: true
    },
    // --- tool-managed caches
    rule! {
        id: "uv-cache",
        label: "uv cache",
        matcher: Matcher::DirSuffix { components: &[".cache", "uv"] },
        category: Category::ToolManagedCache,
        hint: "`uv cache prune` drops unused entries; `uv cache clean` empties it — venvs hardlink into it, so it frees less than it lists",
        command: Some("uv cache prune"),
        why: "uv's package cache; venvs hardlink into it and uv refetches what they need",
        rebuild: RebuildKind::Download,
        carve_out: true
    },
    rule! {
        id: "dot-cache",
        label: "~/.cache",
        matcher: Matcher::DirNamed { name: ".cache" },
        category: Category::ToolManagedCache,
        hint: "per-tool clean commands (`uv cache clean`, `pnpm store prune`); \
               hardlinked stores free less than they list",
        command: None,
        why: "the shared per-tool cache directory; each tool refetches its own entries",
        rebuild: RebuildKind::Download
    },
    rule! {
        id: "dot-npm",
        label: "~/.npm",
        matcher: Matcher::DirNamed { name: ".npm" },
        category: Category::ToolManagedCache,
        hint: "`npm cache clean --force`",
        command: Some("npm cache clean --force"),
        why: "npm's package cache; npm refetches packages on the next install",
        rebuild: RebuildKind::Download
    },
    rule! {
        id: "dot-cargo",
        label: "~/.cargo",
        matcher: Matcher::DirNamed { name: ".cargo" },
        category: Category::ToolManagedCache,
        hint: "cargo registry/git caches; prune with cargo tooling, not rm -rf",
        command: None,
        why: "cargo's registry and git caches (and installed binaries); cargo refetches crates on the next build",
        rebuild: RebuildKind::Download
    },
    rule! {
        id: "dot-rustup",
        label: "~/.rustup",
        matcher: Matcher::DirNamed { name: ".rustup" },
        category: Category::ToolManagedCache,
        hint: "`rustup toolchain uninstall` unused toolchains",
        command: None,
        why: "installed Rust toolchains; rustup re-downloads one on demand",
        rebuild: RebuildKind::Download
    },
    rule! {
        id: "dot-toolbox",
        label: "~/.toolbox",
        matcher: Matcher::DirNamed { name: ".toolbox" },
        category: Category::ToolManagedCache,
        hint: "use `toolbox clean`, not rm -rf",
        command: Some("toolbox clean"),
        why: "toolbox-managed tool versions with sidecar metadata; only `toolbox clean` removes them consistently",
        rebuild: RebuildKind::Download
    },
    rule! {
        id: "homebrew-cellar",
        label: "Homebrew Cellar",
        matcher: Matcher::DirSuffix { components: &["homebrew", "Cellar"] },
        category: Category::ToolManagedCache,
        hint: "`brew cleanup` / `brew uninstall`, not rm -rf",
        command: Some("brew cleanup"),
        why: "installed Homebrew formulae; `brew cleanup` drops superseded versions and `brew install` refetches",
        rebuild: RebuildKind::Download
    },
    rule! {
        id: "homebrew-cellar-intel",
        label: "Homebrew Cellar (Intel prefix)",
        matcher: Matcher::DirSuffix { components: &["local", "Cellar"] },
        category: Category::ToolManagedCache,
        hint: "`brew cleanup` / `brew uninstall`, not rm -rf",
        command: Some("brew cleanup"),
        why: "installed Homebrew formulae; `brew cleanup` drops superseded versions and `brew install` refetches",
        rebuild: RebuildKind::Download
    },
    rule! {
        id: "docker-desktop-data",
        label: "Docker Desktop VM disk",
        matcher: Matcher::DirSuffix { components: &["com.docker.docker", "Data"] },
        category: Category::ToolManagedCache,
        hint: "`docker system df` shows what is reclaimable inside; `docker system prune` frees it",
        command: Some("docker system prune"),
        why: "the Docker Desktop VM disk image; images and volumes inside it are freed by `docker system prune`, never by deleting the file",
        rebuild: RebuildKind::Download
    },
    // --- review-only and informational
    rule! {
        id: "cloud-synced-originals",
        label: "Cloud-synced originals (CloudStorage)",
        matcher: Matcher::DirSuffix { components: &["Library", "CloudStorage"] },
        category: Category::WontRegenerate,
        hint: "synced originals; a local delete propagates to the cloud copy",
        command: None,
        why: "cloud-synced originals; a local delete propagates to the cloud copy",
        rebuild: RebuildKind::None
    },
    rule! {
        id: "icloud-drive",
        label: "Cloud-synced originals (iCloud Drive)",
        matcher: Matcher::DirSuffix { components: &["Library", "Mobile Documents"] },
        category: Category::WontRegenerate,
        hint: "synced originals; a local delete propagates to the cloud copy",
        command: None,
        why: "iCloud Drive originals; a local delete propagates to the cloud copy",
        rebuild: RebuildKind::None
    },
    rule! {
        id: "agent-sessions",
        label: "Agent session data (~/.claude/projects)",
        matcher: Matcher::DirSuffix { components: &[".claude", "projects"] },
        category: Category::ReviewFirst,
        hint: "agent session history; prune old sessions after review",
        command: None,
        why: "agent session transcripts that nothing regenerates",
        rebuild: RebuildKind::None
    },
    rule! {
        id: "agent-worktrees",
        label: "Agent worktrees",
        matcher: Matcher::DirNamed { name: ".worktrees" },
        category: Category::ReviewFirst,
        hint: "worktrees can hold uncommitted work; check `git status` in each",
        command: None,
        why: "git worktrees that may hold uncommitted work",
        rebuild: RebuildKind::None
    },
    rule! {
        id: "git-pack",
        label: "Large git pack files",
        matcher: Matcher::GitPackFile { min_disk_size: GIT_PACK_REVIEW_MIN_DISK },
        category: Category::ReviewFirst,
        hint: "repository history; `git gc` / repack or re-clone shallow — review first",
        command: None,
        why: "repository history; only a repack or a shallow re-clone shrinks it",
        rebuild: RebuildKind::None
    },
    rule! {
        id: "large-file",
        label: "Large files",
        matcher: Matcher::LargeFile { min_disk_size: LARGE_FILE_REVIEW_MIN_DISK },
        category: Category::ReviewFirst,
        hint: "big and unclassified; review before deleting",
        command: None,
        why: "a large file no rule recognizes",
        rebuild: RebuildKind::None
    },
];

/// One hotspot group in the per-scan summary: every entry matched by the
/// same registry row at the same effective category, tier and lock state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HotspotGroup {
    pub rule_id: String,
    pub label: String,
    pub category: Category,
    pub hint: String,
    /// The one safe cleanup command, or null. Nullable-present-as-null on
    /// the wire; `default` so summaries persisted before this field decode
    /// as None instead of failing the row.
    #[serde(default)]
    pub command: Option<String>,
    /// How risky deleting this group is (v1.1). `default` (= `review`) so
    /// pre-v1.1 summaries decode as unrated, never as safe.
    #[serde(default)]
    pub risk_tier: RiskTier,
    /// One sentence: what this is, what brings it back, and what the tier
    /// rests on (lockfile present or missing, project dormant).
    #[serde(default = "legacy_why")]
    pub why: String,
    /// What getting the bytes back costs.
    #[serde(default)]
    pub rebuild_cost: RebuildCost,
    /// The owning tool's own dry-run number, when the scan opted in and the
    /// tool was found at a fixed path. Nullable-present-as-null.
    #[serde(default)]
    pub tool_estimate: Option<ToolEstimate>,
    /// Deduped physical bytes (du model): every hardlinked inode and every
    /// pure-clone stream counted once within the group. THE size.
    pub disk_size: u64,
    /// Naive per-entry sum. Exceeds `disk_size` when hardlinks or clones
    /// share blocks (the "17 GB listed, 5 GB freed" gap made visible).
    pub listed_disk_size: u64,
    /// What deleting the group's paths would ACTUALLY free (v1.1): a
    /// sharing group counts only if EVERY reference to it is inside this
    /// group (and inside the scan); ungrouped files contribute their
    /// kernel-reported private bytes (a pure clone: 0; a snapshot-trapped
    /// file: 0; a modified clone: its rewritten blocks). ≤ `disk_size`.
    /// `default` so summaries persisted before v1.1 decode (as 0).
    #[serde(default)]
    pub private_size: u64,
    pub logical_size: u64,
    pub file_count: u64,
    /// The group's roots worth acting on, biggest first: every root with
    /// ≥ [`TOP_PATH_WORTH_ACTING_BYTES`] private, at least
    /// [`TOP_PATHS_FLOOR`] roots, at most [`TOP_PATHS_CAP`] (1.1.1).
    pub top_paths: Vec<String>,
}

/// One hotspot root directly inside a project (v1.1 Phase 3): the
/// artifact the staleness rule would let a caller reclaim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectArtifact {
    pub rule_id: String,
    pub path: String,
    pub category: Category,
    pub risk_tier: RiskTier,
    /// Deduped disk bytes under this root (the group's per-root share).
    pub disk_size: u64,
}

/// What the staleness rule saw for one project root, recorded at
/// classification time so a client can re-threshold WITHOUT re-walking
/// (the persisted entries have lost the sub-1-MiB source files the rule
/// read). `lastActivityDays` is `now − max(git activity, newest source
/// mtime)` at scan time; null == unverifiable (no evidence, never stale).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectActivity {
    pub root: String,
    pub last_activity_days: Option<i64>,
    /// Dormant at the SCAN's threshold (`olderThan`, default 90 d).
    pub dormant: bool,
    /// Hotspot roots directly inside this project, biggest first.
    pub artifacts: Vec<ProjectArtifact>,
}

/// Per-scan hotspot summary. Serialized camelCase; the surface pass
/// persists it and serves it as GET /scans/{id}/hotspots.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HotspotsSummary {
    /// Sorted: stale project artifacts first, then by category priority,
    /// then tier (safe first), then deduped size descending.
    pub groups: Vec<HotspotGroup>,
    /// What deleting every RECLAIMABLE hotspot (regenerableArtifact,
    /// staleProjectArtifact, cache, toolManagedCache, modelCache) would
    /// actually free: the sum of `privateSize` across them, settled
    /// GLOBALLY — a sharing group counts once, and only if every reference
    /// to it lies inside the reclaimable set. Since v1.1 this is Σ
    /// privateSize, not Σ diskSize: a Finder-duplicated project's
    /// `node_modules` reclaims ~0.
    pub reclaim_estimate: u64,
    /// Deduped disk across reviewFirst + wontRegenerate — visible, never
    /// suggested.
    pub review_disk_size: u64,
    /// The du-lie, quantified: what dataloaded placeholders CLAIM…
    pub cloud_dataloaded_logical_size: u64,
    /// …versus the blocks they actually occupy.
    pub cloud_dataloaded_disk_size: u64,
    /// Every project root the staleness rule evaluated (v1.1 Phase 3),
    /// sorted by artifact bytes descending then root. `default` so
    /// summaries persisted before Phase 3 decode with none.
    #[serde(default)]
    pub projects: Vec<ProjectActivity>,
}

impl HotspotsSummary {
    /// The honest nothing-classified summary (cancelled/failed scans, and
    /// scans that simply found no hotspots serve the same shape).
    pub fn empty() -> Self {
        HotspotsSummary {
            groups: Vec::new(),
            reclaim_estimate: 0,
            review_disk_size: 0,
            cloud_dataloaded_logical_size: 0,
            cloud_dataloaded_disk_size: 0,
            projects: Vec::new(),
        }
    }
}

/// Result of classifying one scan's entries.
#[derive(Debug, Clone, PartialEq)]
pub struct Classification {
    /// Parallel to the input slice: `categories[i]` is entry `i`'s
    /// category (None = ordinary content, nothing to say about it).
    pub categories: Vec<Option<Category>>,
    pub summary: HotspotsSummary,
}

/// A lockfile verifier: given the rule and the PROJECT directory (the one
/// holding the detection file), run the rule's read-only verify command.
/// The classifier itself never runs anything — the caller injects this
/// when the scan opted in (`verifyLocks`), and tests inject a closure.
pub type LockVerifier<'v> = &'v dyn Fn(&HotspotRule, &str) -> LockVerdict;

/// Per-scan classifier knobs.
#[derive(Clone, Copy, Default)]
pub struct ClassifyOptions<'v> {
    /// Dormancy threshold in days; `None` = [`DORMANT_AFTER_DAYS`].
    pub dormant_after_days: Option<i64>,
    /// Opt-in lockfile verification.
    pub verify: Option<LockVerifier<'v>>,
}

impl ClassifyOptions<'_> {
    fn threshold_days(&self) -> i64 {
        self.dormant_after_days.unwrap_or(DORMANT_AFTER_DAYS)
    }
}

/// Parse a staleness threshold: `90d`, `12w`, `3M`, `1y`, or bare days.
/// Months are 30 days and years 365 — a threshold, not a calendar.
pub fn parse_older_than(s: &str) -> Result<i64> {
    let s = s.trim();
    let invalid = || CoreError::InvalidInput(format!(
        "olderThan must be a positive count of d(ays), w(eeks), M(onths) or y(ears), e.g. 3M or 90d; got {s:?}"
    ));
    if s.is_empty() {
        return Err(invalid());
    }
    let (digits, unit) = match s.char_indices().find(|(_, c)| !c.is_ascii_digit()) {
        Some((i, _)) => (&s[..i], &s[i..]),
        None => (s, "d"),
    };
    let n: i64 = digits.parse().map_err(|_| invalid())?;
    let per_unit = match unit {
        "d" => 1,
        "w" => 7,
        "M" => 30,
        "y" => 365,
        _ => return Err(invalid()),
    };
    let days = n.checked_mul(per_unit).ok_or_else(invalid)?;
    if days <= 0 {
        return Err(invalid());
    }
    Ok(days)
}

/// True when a file is a cloud-dataloaded placeholder. With flags recorded
/// (every v1.1 walk), the walker's `dataless` bit is the whole answer:
/// `compressed` and `sparse` files also show logical ≫ disk, but their
/// bytes are local. Only a row WITHOUT flags (pre-v5, synthetic) falls back
/// to the ratio-and-floor heuristic.
pub fn is_cloud_dataloaded(entry: &ScanEntry) -> bool {
    if entry.is_dir {
        return false;
    }
    match entry.flags {
        Some(flags) => flags.contains(EntryFlags::DATALESS),
        None => {
            entry.logical_size >= CLOUD_DATALOADED_MIN_LOGICAL
                && entry.logical_size >= entry.disk_size.saturating_mul(CLOUD_DATALOADED_MIN_RATIO)
        }
    }
}

// ---------------------------------------------------------------------------
// Component-boundary path helpers. Paths are absolute, '/'-separated strings
// (the scanner produces them); matching NEVER uses substring containment.

fn components(path: &str) -> impl Iterator<Item = &str> {
    path.split('/').filter(|c| !c.is_empty())
}

fn ends_with_components(path: &str, suffix: &[&str]) -> bool {
    let comps: Vec<&str> = components(path).collect();
    comps.len() >= suffix.len() && comps[comps.len() - suffix.len()..] == *suffix
}

fn has_component(path: &str, name: &str) -> bool {
    components(path).any(|c| c == name)
}

/// Strict descendant test with a component boundary: `/a/node_modules_backup`
/// is NOT under `/a/node_modules`.
/// Is `path` strictly under ANY of `roots`? Walks the ancestor chain
/// (O(depth)) against a set instead of testing every root (O(roots)) —
/// the three call sites were O(roots²) / O(candidates × roots) at 10k
/// hotspot roots (phantom-45s; same answers, pinned by the collapse and
/// staleness tests).
fn under_any(path: &str, roots: &HashSet<&str>) -> bool {
    ancestors(path).any(|a| roots.contains(a))
}

/// Ancestor chain of `path`, nearest first, excluding `path` itself:
/// `/a/b/c` → `/a/b`, `/a`.
fn ancestors(path: &str) -> impl Iterator<Item = &str> {
    std::iter::successors(parent_of(path), |p| parent_of(p))
}

fn parent_of(path: &str) -> Option<&str> {
    path.rfind('/').filter(|&i| i > 0).map(|i| &path[..i])
}

/// Registry name patterns: exact, `*.ext` (suffix), or `prefix-*`.
fn name_matches(pattern: &str, name: &str) -> bool {
    if let Some(ext) = pattern.strip_prefix('*') {
        name.len() > ext.len() && name.ends_with(ext)
    } else if let Some(prefix) = pattern.strip_suffix('*') {
        name.len() > prefix.len() && name.starts_with(prefix)
    } else {
        pattern == name
    }
}

/// The scan's directory index: every path, and every directory's child
/// names — what sibling and detection checks consult instead of the disk.
pub struct ScanIndex<'a> {
    children: HashMap<&'a str, Vec<&'a str>>,
}

impl<'a> ScanIndex<'a> {
    pub fn build(entries: &'a [ScanEntry]) -> Self {
        let mut children: HashMap<&str, Vec<&str>> = HashMap::new();
        for e in entries {
            if let Some(parent) = e.parent_path.as_deref() {
                children.entry(parent).or_default().push(e.name.as_str());
            }
        }
        ScanIndex { children }
    }

    /// Does `dir` hold a child whose name matches any of `patterns`? A
    /// directory outside the scan holds nothing observable.
    fn dir_has(&self, dir: &str, patterns: &[&str]) -> bool {
        self.children
            .get(dir)
            .is_some_and(|names| names.iter().any(|n| patterns.iter().any(|p| name_matches(p, n))))
    }
}

// ---------------------------------------------------------------------------
// Matching

impl Matcher {
    /// Does this row match `entry`? File-only matchers never match dirs
    /// and vice versa.
    fn matches(&self, entry: &ScanEntry, index: &ScanIndex) -> bool {
        match *self {
            Matcher::DirNamed { name } => entry.is_dir && entry.name == name,
            Matcher::ProjectArtifact { detection, artifacts } => {
                entry.is_dir
                    && artifacts.iter().any(|a| name_matches(a, &entry.name))
                    // Parent outside the scan: the detection file is not
                    // observable, so the claim of regenerability is not
                    // provable. Stay conservative.
                    && entry
                        .parent_path
                        .as_deref()
                        .is_some_and(|parent| index.dir_has(parent, detection))
            }
            Matcher::DirSuffix { components } => {
                entry.is_dir && ends_with_components(&entry.path, components)
            }
            Matcher::ChildOfDirSuffix { components } => {
                entry.is_dir
                    && entry
                        .parent_path
                        .as_deref()
                        .is_some_and(|p| ends_with_components(p, components))
            }
            Matcher::DirNamedWithin { names, within } => {
                entry.is_dir
                    && names.contains(&entry.name.as_str())
                    && entry
                        .parent_path
                        .as_deref()
                        .is_some_and(|p| has_component(p, within))
            }
            Matcher::GitPackFile { min_disk_size } => {
                !entry.is_dir
                    && entry.name.ends_with(".pack")
                    && entry.disk_size > min_disk_size
                    && entry
                        .parent_path
                        .as_deref()
                        .is_some_and(|p| ends_with_components(p, &["objects", "pack"]))
            }
            Matcher::CloudDataloadedFile => is_cloud_dataloaded(entry),
            Matcher::LargeFile { min_disk_size } => {
                !entry.is_dir && entry.disk_size >= min_disk_size
            }
        }
    }

    fn is_dir_matcher(&self) -> bool {
        matches!(
            self,
            Matcher::DirNamed { .. }
                | Matcher::ProjectArtifact { .. }
                | Matcher::DirSuffix { .. }
                | Matcher::ChildOfDirSuffix { .. }
                | Matcher::DirNamedWithin { .. }
        )
    }
}

/// Every name that marks a directory as a project root: `.git` plus each
/// project row's detection patterns.
fn project_markers() -> Vec<&'static str> {
    let mut out = vec![".git"];
    for rule in REGISTRY {
        if let Matcher::ProjectArtifact { detection, .. } = rule.matcher {
            out.extend_from_slice(detection);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tier, why, rebuild cost

/// The tier rubric (docs/reclaimability.md). Pure function of the rule,
/// the effective category and the lock state.
pub fn tier_for(rule: &HotspotRule, category: Category, lock: LockState) -> RiskTier {
    match category {
        Category::ReviewFirst | Category::WontRegenerate | Category::CloudDataloaded => {
            RiskTier::Review
        }
        Category::ToolManagedCache | Category::ModelCache => RiskTier::Caution,
        Category::Cache => RiskTier::Safe,
        Category::RegenerableArtifact | Category::StaleProjectArtifact => {
            if rule.lockfiles.is_empty() {
                RiskTier::Safe
            } else {
                match lock {
                    LockState::Present | LockState::Verified => RiskTier::Safe,
                    LockState::Missing | LockState::Failed | LockState::NotApplicable => {
                        RiskTier::Caution
                    }
                }
            }
        }
    }
}

/// The group's one-sentence justification.
fn why_sentence(
    rule: &HotspotRule,
    category: Category,
    lock: LockState,
    verify_note: Option<&str>,
    threshold_days: i64,
) -> String {
    let mut s = rule.why.to_string();
    match lock {
        LockState::NotApplicable => {}
        LockState::Present => s.push_str("; a lockfile pins the dependency versions"),
        LockState::Verified => {
            s.push_str("; a lockfile pins the dependency versions and `");
            s.push_str(verify_note.unwrap_or("verify"));
            s.push_str("` confirmed it is current");
        }
        LockState::Failed => {
            s.push_str("; a lockfile is present but verification failed (`");
            s.push_str(verify_note.unwrap_or("verify"));
            s.push_str("`), so a reinstall may resolve different versions");
        }
        LockState::Missing => {
            s.push_str("; no lockfile (");
            s.push_str(&rule.lockfiles.join(", "));
            s.push_str(") beside it, so a reinstall may resolve different versions");
        }
    }
    if category == Category::StaleProjectArtifact {
        s.push_str(&format!(
            "; the project's newest source edit and git activity are at least {threshold_days} days old"
        ));
    }
    s.push('.');
    s
}

fn rebuild_cost(rule: &HotspotRule, category: Category, disk_size: u64) -> RebuildCost {
    let estimate = match rule.rebuild {
        RebuildKind::Download => format!("re-download ≈ {}", format_size(disk_size)),
        RebuildKind::Compile => format!("re-compile ≈ {} of build output", format_size(disk_size)),
        RebuildKind::None => match category {
            Category::Cache | Category::RegenerableArtifact | Category::StaleProjectArtifact => {
                "none — repopulated on demand".to_string()
            }
            _ => "not applicable — nothing regenerates this".to_string(),
        },
    };
    RebuildCost { kind: rule.rebuild, estimate }
}

// ---------------------------------------------------------------------------
// Staleness

/// What the scan can say about a project's recent activity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activity {
    /// Newest of (source mtime, git HEAD movement) is this many days old.
    LastSeen(i64),
    /// No `.git` activity file and no dated source under the root: the
    /// evidence is missing. NEVER read as stale.
    Unverifiable,
}

/// Per project root: `now − max(git activity, newest source mtime)`.
/// Project roots are directories holding `.git` or a detection file, unless
/// the root itself sits under a hotspot root (every package inside
/// node_modules ships a package.json) or under `.git`. Source files are
/// every dated file not under a hotspot root or `.git` — artifact mtimes
/// lie. Git activity is the mtime of the reflog and friends
/// ([`GIT_ACTIVITY_FILES`]), read as entries, never by opening the repo.
pub fn project_activity<'a>(
    entries: &'a [ScanEntry],
    hotspot_roots: &[&str],
    now: DateTime<Utc>,
) -> BTreeMap<&'a str, Activity> {
    let markers = project_markers();
    let hotspot_set: HashSet<&str> = hotspot_roots.iter().copied().collect();
    let under_hotspot = |path: &str| under_any(path, &hotspot_set);
    let roots: HashSet<&str> = entries
        .iter()
        .filter(|e| markers.iter().any(|m| name_matches(m, &e.name)))
        .filter_map(|e| e.parent_path.as_deref())
        .filter(|root| !under_hotspot(root) && !has_component(root, ".git"))
        .collect();

    let mut newest: HashMap<&str, DateTime<Utc>> = HashMap::new();
    for entry in entries.iter().filter(|e| !e.is_dir) {
        let Some(mtime) = entry.modified_at else { continue };
        // Git activity: the marker files under <root>/.git/.
        if let Some(git_root) = git_activity_root(&entry.path) {
            if roots.contains(git_root) {
                let slot = newest.entry(git_root).or_insert(mtime);
                if mtime > *slot {
                    *slot = mtime;
                }
            }
            continue;
        }
        // Source files only: nothing under a hotspot root or .git counts.
        if has_component(&entry.path, ".git") || under_hotspot(&entry.path) {
            continue;
        }
        for anc in ancestors(&entry.path) {
            if let Some(root) = roots.get(anc) {
                let slot = newest.entry(*root).or_insert(mtime);
                if mtime > *slot {
                    *slot = mtime;
                }
            }
        }
    }

    roots
        .into_iter()
        .map(|root| {
            let activity = match newest.get(root) {
                Some(m) => Activity::LastSeen(now.signed_duration_since(*m).num_days()),
                None => Activity::Unverifiable,
            };
            (root, activity)
        })
        .collect()
}

/// `<root>/.git/<activity file>` → `<root>`.
fn git_activity_root(path: &str) -> Option<&str> {
    GIT_ACTIVITY_FILES.iter().find_map(|f| {
        let suffix = format!("/.git/{f}");
        path.strip_suffix(suffix.as_str()).filter(|r| !r.is_empty())
    })
}

/// Project roots dormant at the default threshold: roots whose activity is
/// at least [`DORMANT_AFTER_DAYS`] old. Unverifiable roots are never
/// dormant. (Hotspot roots are derived from the entries, as in `classify`.)
pub fn dormant_project_roots(entries: &[ScanEntry], now: DateTime<Utc>) -> Vec<String> {
    let index = ScanIndex::build(entries);
    let roots = hotspot_roots(entries, &index);
    let root_paths: Vec<&str> = roots.iter().map(|r| r.path).collect();
    dormant_roots(&project_activity(entries, &root_paths, now), DORMANT_AFTER_DAYS)
}

fn dormant_roots(activity: &BTreeMap<&str, Activity>, threshold_days: i64) -> Vec<String> {
    activity
        .iter()
        .filter(|(_, a)| matches!(a, Activity::LastSeen(days) if *days >= threshold_days))
        .map(|(root, _)| root.to_string())
        .collect()
}

// ---------------------------------------------------------------------------
// The classifier

struct Root<'a> {
    path: &'a str,
    rule: &'static HotspotRule,
    category: Category,
    lock: LockState,
    /// The verify command's text, for the why sentence.
    verify_note: Option<String>,
}

/// Directory hotspot roots (first matching registry row wins), collapsed to
/// the OUTERMOST root so nothing is counted twice — except carve-out rows,
/// which keep their own root inside an enclosing hotspot. Sorted by path
/// length, so an ancestor always precedes its descendants.
fn hotspot_roots<'a>(entries: &'a [ScanEntry], index: &ScanIndex) -> Vec<Root<'a>> {
    let mut dir_roots: Vec<Root> = entries
        .iter()
        .filter(|e| e.is_dir)
        .filter_map(|e| {
            REGISTRY
                .iter()
                .find(|r| r.matcher.is_dir_matcher() && r.matcher.matches(e, index))
                .map(|rule| Root {
                    path: &e.path,
                    rule,
                    category: rule.category,
                    lock: LockState::NotApplicable,
                    verify_note: None,
                })
        })
        .collect();
    dir_roots.sort_by_key(|r| r.path.len());
    let mut kept: Vec<Root> = Vec::new();
    // Every ancestor of a root is shorter, so by the time a root is
    // visited every kept root it could sit under is already in the set.
    let mut kept_set: HashSet<&str> = HashSet::new();
    for root in dir_roots {
        let nested = under_any(root.path, &kept_set);
        if !nested || root.rule.carve_out {
            kept_set.insert(root.path);
            kept.push(root);
        }
    }
    kept
}

/// Classify a scan's entries. Pure: no store, no filesystem — everything is
/// derived from the entries plus `now` (injected so dormancy is testable).
/// The `topPaths` rule (phantom-cnr.5). `roots` is the group's roots in
/// `topPaths` order — deduped disk descending, then path ascending — and
/// `private_by_root` what deleting each root would actually free. Every
/// root at or above [`TOP_PATH_WORTH_ACTING_BYTES`] is listed, up to
/// [`TOP_PATHS_CAP`]; when fewer than [`TOP_PATHS_FLOOR`] qualify the list
/// is filled to the floor with the next-largest roots by disk, so a group
/// of small roots still names its five largest exactly as 1.1.0 did. The
/// output keeps `roots`' order whichever way it was built.
///
/// Mutation targets: `take(5)` here and
/// `a_group_of_seven_nine_gib_roots_lists_all_seven` fails; drop the floor
/// fill and `a_group_of_small_roots_still_lists_five` fails; drop the cap
/// and `a_four_hundred_root_group_lists_twenty_five` fails; test disk
/// instead of private and `top_paths_measure_private_bytes_not_disk` fails.
fn select_top_paths(roots: &[(&str, u64)], private_by_root: &BTreeMap<&str, u64>) -> Vec<String> {
    let worth_acting_on = |path: &str| {
        private_by_root.get(path).copied().unwrap_or(0) >= TOP_PATH_WORTH_ACTING_BYTES
    };
    let mut listed: Vec<bool> = roots.iter().map(|(p, _)| worth_acting_on(p)).collect();
    let mut count = listed.iter().filter(|l| **l).count();
    // Over the cap: keep the largest by disk (roots is already in that order).
    if count > TOP_PATHS_CAP {
        let mut kept = 0;
        for l in listed.iter_mut() {
            if *l {
                kept += 1;
                if kept > TOP_PATHS_CAP {
                    *l = false;
                }
            }
        }
        count = TOP_PATHS_CAP;
    }
    // Under the floor: fill with the next-largest roots by disk.
    for l in listed.iter_mut() {
        if count >= TOP_PATHS_FLOOR {
            break;
        }
        if !*l {
            *l = true;
            count += 1;
        }
    }
    roots
        .iter()
        .zip(listed)
        .filter(|(_, l)| *l)
        .map(|((p, _), _)| p.to_string())
        .collect()
}

pub fn classify(entries: &[ScanEntry], now: DateTime<Utc>) -> Classification {
    classify_with(entries, &ShareLedger::from_entries(entries), now)
}

/// [`classify`] with the walker's sharing ledger, so clone groups with a
/// member outside the scan are known to be unfreeable.
pub fn classify_with(entries: &[ScanEntry], shares: &ShareLedger, now: DateTime<Utc>) -> Classification {
    classify_with_options(entries, shares, now, &ClassifyOptions::default())
}

/// [`classify_with`] plus the per-scan knobs: the dormancy threshold and
/// the opt-in lockfile verifier.
pub fn classify_with_options(
    entries: &[ScanEntry],
    shares: &ShareLedger,
    now: DateTime<Utc>,
    options: &ClassifyOptions,
) -> Classification {
    let index = ScanIndex::build(entries);
    let threshold_days = options.threshold_days();

    // 1. Directory hotspot roots.
    let mut kept = hotspot_roots(entries, &index);

    // 2. Staleness: a regenerable artifact inside a dormant project is the
    //    best reclaim candidate there is. Unverifiable projects never
    //    upgrade.
    let root_paths: Vec<&str> = kept.iter().map(|r| r.path).collect();
    let activity = project_activity(entries, &root_paths, now);
    let dormant = dormant_roots(&activity, threshold_days);
    let dormant_set: HashSet<&str> = dormant.iter().map(String::as_str).collect();
    for root in &mut kept {
        if root.category == Category::RegenerableArtifact && under_any(root.path, &dormant_set) {
            root.category = Category::StaleProjectArtifact;
        }
    }

    // 3. Lock state per root: a lockfile beside the detection file (the
    //    root's parent), optionally verified by the injected runner.
    for root in &mut kept {
        if root.rule.lockfiles.is_empty() {
            continue;
        }
        let parent = parent_of(root.path);
        let present = parent.is_some_and(|p| index.dir_has(p, root.rule.lockfiles));
        root.lock = if !present {
            LockState::Missing
        } else {
            match (options.verify, root.rule.verify, parent) {
                (Some(run), Some(v), Some(project_dir)) if index.dir_has(project_dir, &[v.requires]) => {
                    let note = format!("{} {}", v.tool, v.args.join(" "));
                    match run(root.rule, project_dir) {
                        LockVerdict::Verified => {
                            root.verify_note = Some(note);
                            LockState::Verified
                        }
                        LockVerdict::Failed(reason) => {
                            root.verify_note = Some(format!("{note}: {reason}"));
                            LockState::Failed
                        }
                        LockVerdict::Unavailable => LockState::Present,
                    }
                }
                _ => LockState::Present,
            }
        };
    }
    let root_by_path: HashMap<&str, usize> =
        kept.iter().enumerate().map(|(i, r)| (r.path, i)).collect();

    // 4. Per-entry assignment. Files: cloud-dataloaded override first, then
    //    the deepest governing dir root, then standalone file rules. Dirs
    //    inherit their governing root's category.
    let cloud_rule = REGISTRY
        .iter()
        .find(|r| r.matcher == Matcher::CloudDataloadedFile)
        .expect("registry must carry the cloud-dataloaded row");
    let file_rules: Vec<&'static HotspotRule> = REGISTRY
        .iter()
        .filter(|r| !r.matcher.is_dir_matcher() && r.matcher != Matcher::CloudDataloadedFile)
        .collect();

    // Group key: (rule, effective category, lock state). Tier and why are
    // functions of the key, so every root in a group shares them.
    #[derive(Clone, Copy)]
    struct Assignment {
        rule: &'static HotspotRule,
        category: Category,
        lock: LockState,
        /// Index into `kept` for a governed entry; None for a file rule.
        root: Option<usize>,
    }
    let mut assigned: Vec<Option<Assignment>> = Vec::with_capacity(entries.len());
    for entry in entries {
        let governing = std::iter::once(entry.path.as_str())
            .chain(ancestors(&entry.path))
            .find_map(|p| root_by_path.get(p).copied());
        let assignment = if !entry.is_dir && is_cloud_dataloaded(entry) {
            Some(Assignment {
                rule: cloud_rule,
                category: cloud_rule.category,
                lock: LockState::NotApplicable,
                root: None,
            })
        } else if let Some(i) = governing {
            let root = &kept[i];
            Some(Assignment {
                rule: root.rule,
                category: root.category,
                lock: root.lock,
                root: Some(i),
            })
        } else if !entry.is_dir {
            file_rules
                .iter()
                .find(|r| r.matcher.matches(entry, &index))
                .map(|r| Assignment {
                    rule: r,
                    category: r.category,
                    lock: LockState::NotApplicable,
                    root: None,
                })
        } else {
            None
        };
        assigned.push(assignment);
    }

    // 5. Aggregate groups. Hardlinks and clones dedupe by ChargeKey — per
    //    group for group totals, globally for the summary rollups.
    struct Acc<'a> {
        rule: &'static HotspotRule,
        category: Category,
        lock: LockState,
        verify_note: Option<String>,
        disk: u64,
        listed: u64,
        logical: u64,
        files: u64,
        seen: HashSet<ChargeKey>,
        /// Ungrouped files' private bytes, plus (below) every sharing
        /// group wholly inside this hotspot group.
        private: u64,
        /// References per sharing group seen inside this hotspot group.
        refs: HashMap<ChargeKey, u64>,
        /// Deduped disk per hotspot root (the `topPaths` order).
        per_root: BTreeMap<&'a str, u64>,
        /// Ungrouped private bytes per hotspot root; sharing groups wholly
        /// inside one root are credited to it after the loop (cnr.5).
        per_root_private: BTreeMap<&'a str, u64>,
        /// The one root every reference of a sharing group sat under, or
        /// `None` once a second root was seen — such a group's bytes count
        /// for the hotspot group but for no single root.
        ref_root: HashMap<ChargeKey, Option<&'a str>>,
    }
    let mut groups: BTreeMap<(&str, Category, LockState), Acc> = BTreeMap::new();
    let mut global_seen: HashSet<ChargeKey> = HashSet::new();
    // The reclaim estimate's ledger: ungrouped private bytes, and
    // references per sharing group across ALL reclaimable groups.
    let mut reclaim_private: u64 = 0;
    let mut reclaim_refs: HashMap<ChargeKey, u64> = HashMap::new();
    let mut summary = HotspotsSummary::empty();
    // Deduped disk per hotspot root, for the per-project artifact list.
    let mut root_disk: HashMap<&str, u64> = HashMap::new();

    for (entry, assignment) in entries.iter().zip(&assigned) {
        let Some(a) = assignment else { continue };
        if entry.is_dir {
            continue; // dirs carry disk_size 0; files carry the bytes
        }
        let root_path = a.root.map_or(entry.path.as_str(), |i| kept[i].path);
        let acc = groups
            .entry((a.rule.id, a.category, a.lock))
            .or_insert_with(|| Acc {
                rule: a.rule,
                category: a.category,
                lock: a.lock,
                verify_note: a.root.and_then(|i| kept[i].verify_note.clone()),
                disk: 0,
                listed: 0,
                logical: 0,
                files: 0,
                seen: HashSet::new(),
                private: 0,
                refs: HashMap::new(),
                per_root: BTreeMap::new(),
                per_root_private: BTreeMap::new(),
                ref_root: HashMap::new(),
            });
        acc.listed += entry.disk_size;
        acc.logical += entry.logical_size;
        acc.files += 1;
        let key = ChargeKey::of_entry(entry);
        let first_in_group = key.is_none_or(|k| acc.seen.insert(k));
        if first_in_group {
            acc.disk += entry.disk_size;
            *acc.per_root.entry(root_path).or_insert(0) += entry.disk_size;
            if a.root.is_some() {
                *root_disk.entry(root_path).or_insert(0) += entry.disk_size;
            }
        }
        // The deletion-honest side: an ungrouped file frees its own
        // private bytes; a sharing group is settled after the loop, once
        // every reference inside the hotspot group has been counted.
        match key {
            None => {
                let private = ShareLedger::ungrouped_private(entry).min(entry.disk_size);
                acc.private += private;
                *acc.per_root_private.entry(root_path).or_insert(0) += private;
            }
            Some(k) => {
                *acc.refs.entry(k).or_insert(0) += 1;
                acc.ref_root
                    .entry(k)
                    .and_modify(|seen| {
                        if *seen != Some(root_path) {
                            *seen = None;
                        }
                    })
                    .or_insert(Some(root_path));
            }
        }
        // Group-level `seen` and the global set are independent: a sharing
        // group spanning two hotspot groups counts once here even though
        // each group saw it first.
        let first_globally = key.is_none_or(|k| global_seen.insert(k));
        match a.category {
            c if c.is_reclaimable() => match key {
                None => reclaim_private += ShareLedger::ungrouped_private(entry).min(entry.disk_size),
                Some(k) => *reclaim_refs.entry(k).or_insert(0) += 1,
            },
            Category::ReviewFirst | Category::WontRegenerate if first_globally => {
                summary.review_disk_size += entry.disk_size
            }
            Category::CloudDataloaded if first_globally => {
                summary.cloud_dataloaded_disk_size += entry.disk_size;
                summary.cloud_dataloaded_logical_size += entry.logical_size;
            }
            _ => {}
        }
    }
    // Mutation target (the plan's #2): use `disk_size` here instead of the
    // settled private bytes and `cloned_tree_reclaims_nothing` fails.
    summary.reclaim_estimate = reclaim_private
        + reclaim_refs
            .iter()
            .map(|(k, n)| shares.freed_by(*k, *n))
            .sum::<u64>();

    let mut out: Vec<HotspotGroup> = groups
        .into_values()
        .map(|acc| {
            let mut roots: Vec<(&str, u64)> = acc.per_root.into_iter().collect();
            roots.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
            let private_size = acc.private
                + acc
                    .refs
                    .iter()
                    .map(|(k, n)| shares.freed_by(*k, *n))
                    .sum::<u64>();
            // A sharing group settles for a root only when every reference
            // sat under that root — the same rule the group applies to the
            // scan, one level down. Spread across roots, it counts for the
            // group and for no single root (conservative for the threshold).
            let mut per_root_private = acc.per_root_private;
            for (k, n) in &acc.refs {
                if let Some(Some(root)) = acc.ref_root.get(k) {
                    *per_root_private.entry(root).or_insert(0) += shares.freed_by(*k, *n);
                }
            }
            let tier = tier_for(acc.rule, acc.category, acc.lock);
            HotspotGroup {
                rule_id: acc.rule.id.to_string(),
                label: acc.rule.label.to_string(),
                category: acc.category,
                hint: acc.rule.hint.to_string(),
                command: acc.rule.command.map(str::to_string),
                risk_tier: tier,
                why: why_sentence(acc.rule, acc.category, acc.lock, acc.verify_note.as_deref(), threshold_days),
                rebuild_cost: rebuild_cost(acc.rule, acc.category, acc.disk),
                tool_estimate: None,
                disk_size: acc.disk,
                listed_disk_size: acc.listed,
                private_size,
                logical_size: acc.logical,
                file_count: acc.files,
                top_paths: select_top_paths(&roots, &per_root_private),
            }
        })
        .collect();
    out.sort_by(|a, b| {
        a.category
            .priority()
            .cmp(&b.category.priority())
            .then_with(|| a.risk_tier.cmp(&b.risk_tier))
            .then_with(|| b.disk_size.cmp(&a.disk_size))
            .then_with(|| a.rule_id.cmp(&b.rule_id))
    });
    summary.groups = out;

    // 6. Per-project activity (Phase 3): what the staleness rule saw, with
    //    the hotspot roots directly inside each project, so `stale` can
    //    re-threshold from the persisted summary.
    // Roots grouped by parent once (O(roots)), not filtered per project
    // (O(projects × roots) — 1.4 s of a 1.75 s classify at 10k projects,
    // phantom-45s); dormant as a set for the same reason.
    let mut roots_by_parent: HashMap<&str, Vec<&Root>> = HashMap::new();
    for r in &kept {
        if let Some(parent) = parent_of(r.path) {
            roots_by_parent.entry(parent).or_default().push(r);
        }
    }
    let dormant_set: HashSet<&str> = dormant.iter().map(String::as_str).collect();
    let mut projects: Vec<ProjectActivity> = activity
        .iter()
        .map(|(root, act)| {
            let mut artifacts: Vec<ProjectArtifact> = roots_by_parent
                .get(root)
                .map(|v| v.as_slice())
                .unwrap_or(&[])
                .iter()
                .map(|r| ProjectArtifact {
                    rule_id: r.rule.id.to_string(),
                    path: r.path.to_string(),
                    category: r.category,
                    risk_tier: tier_for(r.rule, r.category, r.lock),
                    disk_size: root_disk.get(r.path).copied().unwrap_or(0),
                })
                .collect();
            artifacts.sort_by(|a, b| b.disk_size.cmp(&a.disk_size).then_with(|| a.path.cmp(&b.path)));
            ProjectActivity {
                root: root.to_string(),
                last_activity_days: match act {
                    Activity::LastSeen(d) => Some(*d),
                    Activity::Unverifiable => None,
                },
                dormant: dormant_set.contains(root),
                artifacts,
            }
        })
        .collect();
    // Biggest artifact bytes first, then root; the key is computed once per
    // project, not once per comparison.
    projects.sort_by_cached_key(|p| {
        (std::cmp::Reverse(p.artifacts.iter().map(|a| a.disk_size).sum::<u64>()), p.root.clone())
    });
    summary.projects = projects;

    Classification {
        categories: assigned
            .iter()
            .map(|a| a.as_ref().map(|a| a.category))
            .collect(),
        summary,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone};
    use std::str::FromStr;

    // The shared fixture pins the summary's wire shape for the Swift side
    // and the OPE conformance harness, exactly like the scan fixtures.
    const RAW_SUMMARY: &str = include_str!("../../../tests/fixtures/hotspots-summary.json");

    /// Fixed "now" so dormancy math is deterministic.
    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 31, 12, 0, 0).unwrap()
    }

    fn days_ago(days: i64) -> Option<DateTime<Utc>> {
        Some(now() - Duration::days(days))
    }

    fn base(path: &str, is_dir: bool) -> ScanEntry {
        ScanEntry {
            path: path.to_string(),
            parent_path: parent_of(path).map(|s| s.to_string()),
            name: path.rsplit('/').next().unwrap().to_string(),
            is_dir,
            disk_size: 0,
            logical_size: 0,
            modified_at: None,
            file_type: None,
            category: None,
            nlink: 1,
            dev: 1,
            ino: 0,
            file_count: None,
            dir_count: None,
            private_size: None,
            shared_size: None,
            clone_id: None,
            flags: None,
        }
    }

    fn dir(path: &str) -> ScanEntry {
        base(path, true)
    }

    fn file(path: &str, disk: u64) -> ScanEntry {
        let mut e = base(path, false);
        e.disk_size = disk;
        e.logical_size = disk;
        // Unique inode per file unless a test overrides it.
        e.ino = 1_000_000 + path.len() as u64 * 31 + disk;
        e
    }

    /// Run manually: cargo test -p phantom-core measure_1m_classify -- --ignored --nocapture
    /// 10k npm projects × (package.json, lockfile, node_modules/ with 100
    /// files) ≈ 1.03M entries and 10k hotspot roots — the shape phantom-45s
    /// worried about (kept.iter().any() per root is O(k²) in roots).
    #[test]
    #[ignore = "measurement, not a gate — run with --ignored --nocapture"]
    fn measure_1m_classify() {
        let projects = 10_000;
        let mut entries = Vec::with_capacity(projects * 104 + 2);
        entries.push(dir("/r"));
        for p in 0..projects {
            let proj = format!("/r/p{p:05}");
            entries.push(dir(&proj));
            entries.push(file(&format!("{proj}/package.json"), 512));
            entries.push(file(&format!("{proj}/package-lock.json"), 4096));
            let nm = format!("{proj}/node_modules");
            entries.push(dir(&nm));
            for f in 0..100 {
                entries.push(file(&format!("{nm}/m{f:03}.js"), 2048));
            }
        }
        eprintln!("entries: {}", entries.len());
        let t = std::time::Instant::now();
        let index = ScanIndex::build(&entries);
        eprintln!("index: {:?}", t.elapsed());
        let t = std::time::Instant::now();
        let roots = hotspot_roots(&entries, &index);
        eprintln!("hotspot_roots ({} roots): {:?}", roots.len(), t.elapsed());
        let root_paths: Vec<&str> = roots.iter().map(|r| r.path).collect();
        let t = std::time::Instant::now();
        let activity = project_activity(&entries, &root_paths, now());
        eprintln!("project_activity ({} projects): {:?}", activity.len(), t.elapsed());
        let t = std::time::Instant::now();
        let c = classify(&entries, now());
        eprintln!("classify total: {:?}  groups={}  projects={}", t.elapsed(), c.summary.groups.len(), c.summary.projects.len());
    }

    fn category_of(entries: &[ScanEntry], path: &str) -> Option<Category> {
        let c = classify(entries, now());
        let i = entries.iter().position(|e| e.path == path).unwrap();
        c.categories[i]
    }

    fn group<'a>(summary: &'a HotspotsSummary, rule_id: &str) -> &'a HotspotGroup {
        summary
            .groups
            .iter()
            .find(|g| g.rule_id == rule_id)
            .unwrap_or_else(|| panic!("no group {rule_id} in {:?}", summary.groups))
    }

    // -- registry rows, one scenario each ---------------------------------

    #[test]
    fn cargo_target_with_manifest_sibling_is_regenerable() {
        let entries = vec![
            dir("/p"),
            file("/p/Cargo.toml", 10),
            dir("/p/target"),
            file("/p/target/debug.bin", 500),
        ];
        assert_eq!(
            category_of(&entries, "/p/target/debug.bin"),
            Some(Category::RegenerableArtifact)
        );
        assert_eq!(
            category_of(&entries, "/p/target"),
            Some(Category::RegenerableArtifact)
        );
        // Project sources are ordinary content.
        assert_eq!(category_of(&entries, "/p/Cargo.toml"), None);
    }

    #[test]
    fn bare_target_without_manifest_sibling_is_not_regenerable() {
        // A directory that HAPPENS to be called target is not a build dir.
        let entries = vec![dir("/p"), dir("/p/target"), file("/p/target/data.bin", 500)];
        assert_eq!(category_of(&entries, "/p/target"), None);
        assert_eq!(category_of(&entries, "/p/target/data.bin"), None);
    }

    /// Scanning an artifact dir DIRECTLY (root = …/target): the root entry's
    /// parent_path is None (the parent is outside the scan), so the
    /// Cargo.toml sibling is unprovable and the conservative rule keeps the
    /// sibling-gated rule silent — even though the real parent on disk may
    /// well hold a Cargo.toml. Pinned per the 2026-09-01 safety review.
    #[test]
    fn scan_root_that_is_a_sibling_gated_artifact_dir_classifies_none() {
        let mut root = dir("/Users/ghost/Code/foo/target");
        root.parent_path = None; // scan root: parent outside the scan
        let entries = vec![root, file("/Users/ghost/Code/foo/target/debug.bin", 500)];
        assert_eq!(category_of(&entries, "/Users/ghost/Code/foo/target"), None);
        assert_eq!(
            category_of(&entries, "/Users/ghost/Code/foo/target/debug.bin"),
            None,
            "children inherit the root's non-classification"
        );
    }

    /// The sibling-FREE half of the same edge: a scan rooted at
    /// node_modules needs no proof — the root (and everything under it)
    /// classifies regenerable.
    #[test]
    fn scan_root_that_is_a_sibling_free_hotspot_classifies() {
        let mut root = dir("/Users/ghost/Code/foo/node_modules");
        root.parent_path = None;
        let entries = vec![root, file("/Users/ghost/Code/foo/node_modules/x.js", 100)];
        assert_eq!(
            category_of(&entries, "/Users/ghost/Code/foo/node_modules"),
            Some(Category::RegenerableArtifact)
        );
        assert_eq!(
            category_of(&entries, "/Users/ghost/Code/foo/node_modules/x.js"),
            Some(Category::RegenerableArtifact)
        );
    }

    #[test]
    fn component_boundary_node_modules_backup_never_matches() {
        let entries = vec![
            dir("/p"),
            dir("/p/node_modules"),
            file("/p/node_modules/x.js", 100),
            dir("/p/node_modules_backup"),
            file("/p/node_modules_backup/y.js", 100),
        ];
        assert_eq!(
            category_of(&entries, "/p/node_modules/x.js"),
            Some(Category::RegenerableArtifact)
        );
        assert_eq!(category_of(&entries, "/p/node_modules_backup"), None);
        assert_eq!(category_of(&entries, "/p/node_modules_backup/y.js"), None);
    }

    #[test]
    fn every_regenerable_dir_row_matches_its_shape() {
        let entries = vec![
            dir("/p"),
            file("/p/package.json", 1),
            file("/p/Package.swift", 1),
            dir("/p/.venv"),
            file("/p/.venv/lib.so", 10),
            dir("/p/.build"),
            file("/p/.build/o.o", 10),
            dir("/p/.next"),
            file("/p/.next/page.js", 10),
            dir("/p/build"),
            file("/p/build/out.js", 10),
            dir("/p/dist"),
            file("/p/dist/bundle.js", 10),
        ];
        for path in ["/p/.venv", "/p/.build", "/p/.next", "/p/build", "/p/dist"] {
            assert_eq!(
                category_of(&entries, path),
                Some(Category::RegenerableArtifact),
                "{path}"
            );
        }
    }

    #[test]
    fn generic_build_dir_without_package_json_is_untouched() {
        // build/Phantom.app in this very repo must never be "regenerable".
        let entries = vec![dir("/repo"), dir("/repo/build"), file("/repo/build/app.bin", 10)];
        assert_eq!(category_of(&entries, "/repo/build"), None);
    }

    #[test]
    fn cache_rows_match_their_shapes() {
        let entries = vec![
            dir("/u/Library"),
            dir("/u/Library/Caches"),
            file("/u/Library/Caches/app/f", 10),
            dir("/u/Library/Caches/app"),
            dir("/u/Library/Developer/Xcode/DerivedData"),
            file("/u/Library/Developer/Xcode/DerivedData/P-abc/x.o", 10),
            dir("/u/Library/Application Support/Slack"),
            dir("/u/Library/Application Support/Slack/Cache"),
            file("/u/Library/Application Support/Slack/Cache/blob", 10),
        ];
        // phantom-11h: macOS refuses to rename ~/Library/Caches itself
        // (dogfood 2026-09-09: Permission denied for the owner), so the
        // hotspot roots — and therefore the plan's paths — are its per-app
        // CHILDREN, each of which moves. The directory is not a root.
        assert_eq!(category_of(&entries, "/u/Library/Caches"), None);
        assert_eq!(category_of(&entries, "/u/Library/Caches/app"), Some(Category::Cache));
        assert_eq!(
            category_of(&entries, "/u/Library/Caches/app/f"),
            Some(Category::Cache)
        );
        let c = classify(&entries, now());
        assert_eq!(group(&c.summary, "library-caches").top_paths, vec!["/u/Library/Caches/app".to_string()]);
        assert_eq!(
            category_of(&entries, "/u/Library/Developer/Xcode/DerivedData"),
            Some(Category::Cache)
        );
        assert_eq!(
            category_of(&entries, "/u/Library/Application Support/Slack/Cache/blob"),
            Some(Category::Cache)
        );
    }

    /// The same per-child rule reaches sandboxed apps' caches
    /// (`Containers/<bundle>/Data/Library/Caches/<app>`); a TCC-protected
    /// container the walker could not read has no children in the scan,
    /// so nothing under it is planned. A stray file directly inside
    /// Library/Caches is not a cache root either.
    #[test]
    fn library_caches_plans_each_child_including_container_caches() {
        let entries = vec![
            dir("/u/Library"),
            dir("/u/Library/Caches"),
            file("/u/Library/Caches/loose.db", 10),
            dir("/u/Library/Caches/com.apple.Safari"),
            file("/u/Library/Caches/com.apple.Safari/Cache.db", 300),
            dir("/u/Library/Caches/Google"),
            file("/u/Library/Caches/Google/blob", 100),
            dir("/u/Library/Containers"),
            dir("/u/Library/Containers/com.example.app"),
            dir("/u/Library/Containers/com.example.app/Data"),
            dir("/u/Library/Containers/com.example.app/Data/Library"),
            dir("/u/Library/Containers/com.example.app/Data/Library/Caches"),
            dir("/u/Library/Containers/com.example.app/Data/Library/Caches/com.example.app"),
            file("/u/Library/Containers/com.example.app/Data/Library/Caches/com.example.app/x", 50),
            // Unreadable (TCC) container: the directory row exists, nothing below it.
            dir("/u/Library/Containers/com.apple.mail"),
            dir("/u/Library/Containers/com.apple.mail/Data"),
            dir("/u/Library/Containers/com.apple.mail/Data/Library"),
            dir("/u/Library/Containers/com.apple.mail/Data/Library/Caches"),
        ];
        let c = classify(&entries, now());
        let g = group(&c.summary, "library-caches");
        assert_eq!(
            g.top_paths,
            vec![
                "/u/Library/Caches/com.apple.Safari".to_string(),
                "/u/Library/Caches/Google".to_string(),
                "/u/Library/Containers/com.example.app/Data/Library/Caches/com.example.app".to_string(),
            ],
            "per-app children, biggest first; never a Library/Caches directory itself"
        );
        assert_eq!(g.disk_size, 450, "the loose file and the unreadable container count for nothing");
        assert_eq!(category_of(&entries, "/u/Library/Caches/loose.db"), None);
        assert_eq!(category_of(&entries, "/u/Library/Containers/com.apple.mail/Data/Library/Caches"), None);
        assert!(g.why.contains("per-app"), "the why says what a path is: {}", g.why);
    }

    #[test]
    fn tool_managed_rows_match_and_toolbox_hint_is_exact() {
        let entries = vec![
            dir("/u/.cache"),
            file("/u/.cache/uv/a", 10),
            dir("/u/.npm"),
            file("/u/.npm/a", 10),
            dir("/u/.cargo"),
            file("/u/.cargo/a", 10),
            dir("/u/.rustup"),
            file("/u/.rustup/a", 10),
            dir("/u/.toolbox"),
            file("/u/.toolbox/tools/a", 10),
            dir("/opt/homebrew/Cellar"),
            file("/opt/homebrew/Cellar/jq/1/bin", 10),
            dir("/usr/local/Cellar"),
            file("/usr/local/Cellar/jq/1/bin", 10),
        ];
        for path in ["/u/.cache", "/u/.npm", "/u/.cargo", "/u/.rustup", "/u/.toolbox",
                     "/opt/homebrew/Cellar", "/usr/local/Cellar"] {
            assert_eq!(
                category_of(&entries, path),
                Some(Category::ToolManagedCache),
                "{path}"
            );
        }
        let c = classify(&entries, now());
        assert_eq!(
            group(&c.summary, "dot-toolbox").hint,
            "use `toolbox clean`, not rm -rf"
        );
    }

    #[test]
    fn review_first_rows_match_their_shapes() {
        let entries = vec![
            dir("/u/Library/Group Containers"),
            file("/u/Library/Group Containers/g.id/data", 10),
            dir("/u/.claude/projects"),
            file("/u/.claude/projects/-u-Code-x/session.jsonl", 10),
            dir("/u/gt/.worktrees"),
            file("/u/gt/.worktrees/wt1/main.rs", 10),
        ];
        for path in [
            "/u/Library/Group Containers",
            "/u/.claude/projects",
            "/u/gt/.worktrees",
        ] {
            assert_eq!(category_of(&entries, path), Some(Category::ReviewFirst), "{path}");
        }
    }

    #[test]
    fn cloud_synced_originals_wont_regenerate() {
        let entries = vec![
            dir("/u/Library/CloudStorage"),
            file("/u/Library/CloudStorage/OneDrive/doc.docx", 4096),
            dir("/u/Library/Mobile Documents"),
            file("/u/Library/Mobile Documents/com~apple~CloudDocs/n.txt", 4096),
        ];
        assert_eq!(
            category_of(&entries, "/u/Library/CloudStorage/OneDrive/doc.docx"),
            Some(Category::WontRegenerate)
        );
        assert_eq!(
            category_of(&entries, "/u/Library/Mobile Documents/com~apple~CloudDocs/n.txt"),
            Some(Category::WontRegenerate)
        );
    }

    #[test]
    fn git_pack_over_threshold_is_review_first_boundary_exact() {
        let over = {
            let mut e = file("/r/.git/objects/pack/pack-a.pack", GIT_PACK_REVIEW_MIN_DISK + 1);
            e.logical_size = e.disk_size;
            e
        };
        let at = file("/r/.git/objects/pack/pack-b.pack", GIT_PACK_REVIEW_MIN_DISK);
        let misplaced = file("/r/loose/pack-c.pack", GIT_PACK_REVIEW_MIN_DISK + 1);
        let entries = vec![over, at, misplaced];
        assert_eq!(
            category_of(&entries, "/r/.git/objects/pack/pack-a.pack"),
            Some(Category::ReviewFirst)
        );
        // "> 200 MB", strictly: exactly at the threshold does not fire.
        assert_eq!(category_of(&entries, "/r/.git/objects/pack/pack-b.pack"), None);
        // Right name, wrong place: not a git pack.
        assert_eq!(category_of(&entries, "/r/loose/pack-c.pack"), None);
    }

    #[test]
    fn plain_large_file_is_review_first_boundary_inclusive() {
        let entries = vec![
            file("/u/Movies/raw.mov", LARGE_FILE_REVIEW_MIN_DISK),
            file("/u/Movies/small.mov", LARGE_FILE_REVIEW_MIN_DISK - 1),
        ];
        assert_eq!(
            category_of(&entries, "/u/Movies/raw.mov"),
            Some(Category::ReviewFirst)
        );
        assert_eq!(category_of(&entries, "/u/Movies/small.mov"), None);
    }

    #[test]
    fn large_file_inside_a_hotspot_stays_with_its_group() {
        // The catch-all only fires for otherwise-unmatched files.
        let mut big = file("/p/node_modules/huge.bin", LARGE_FILE_REVIEW_MIN_DISK + 1);
        big.logical_size = big.disk_size;
        let entries = vec![dir("/p"), dir("/p/node_modules"), big];
        assert_eq!(
            category_of(&entries, "/p/node_modules/huge.bin"),
            Some(Category::RegenerableArtifact)
        );
    }

    // -- cloud-dataloaded edges -------------------------------------------

    fn cloud_file(path: &str, disk: u64, logical: u64) -> ScanEntry {
        let mut e = file(path, disk);
        e.logical_size = logical;
        e
    }

    #[test]
    fn dataloaded_at_exact_ratio_and_floor() {
        let disk = 1024 * 1024; // 1 MiB physical
        let e = cloud_file("/u/OneDrive/a.pptx", disk, disk * CLOUD_DATALOADED_MIN_RATIO);
        assert!(is_cloud_dataloaded(&e), "exact ratio must qualify");
        let entries = vec![e];
        assert_eq!(
            category_of(&entries, "/u/OneDrive/a.pptx"),
            Some(Category::CloudDataloaded)
        );
    }

    #[test]
    fn one_byte_under_the_ratio_is_not_dataloaded() {
        let disk = 1024 * 1024;
        let e = cloud_file("/u/OneDrive/b.pptx", disk, disk * CLOUD_DATALOADED_MIN_RATIO - 1);
        assert!(!is_cloud_dataloaded(&e));
    }

    #[test]
    fn zero_block_file_at_the_logical_floor_is_dataloaded() {
        // The du-lie case: full logical size, no blocks at all.
        let e = cloud_file("/u/OneDrive/c.bin", 0, CLOUD_DATALOADED_MIN_LOGICAL);
        assert!(is_cloud_dataloaded(&e));
    }

    #[test]
    fn tiny_file_never_qualifies_even_at_infinite_ratio() {
        // Absolute floor: sparse-ish tiny files are noise, not placeholders.
        let e = cloud_file("/u/OneDrive/d.bin", 0, CLOUD_DATALOADED_MIN_LOGICAL - 1);
        assert!(!is_cloud_dataloaded(&e));
    }

    #[test]
    fn dataloaded_override_beats_the_governing_hotspot() {
        // Even inside node_modules, a placeholder frees ~nothing; it must
        // move to the cloud group, not inflate the regenerable estimate.
        let placeholder = cloud_file("/p/node_modules/big.node", 512, 64 * 1024 * 1024);
        let entries = vec![
            dir("/p"),
            dir("/p/node_modules"),
            file("/p/node_modules/x.js", 100),
            placeholder,
        ];
        assert_eq!(
            category_of(&entries, "/p/node_modules/big.node"),
            Some(Category::CloudDataloaded)
        );
        let c = classify(&entries, now());
        assert_eq!(group(&c.summary, "node-modules").file_count, 1);
        let cloud = group(&c.summary, "cloud-dataloaded");
        assert_eq!(cloud.disk_size, 512);
        assert_eq!(cloud.logical_size, 64 * 1024 * 1024);
        assert_eq!(c.summary.cloud_dataloaded_logical_size, 64 * 1024 * 1024);
        assert_eq!(c.summary.cloud_dataloaded_disk_size, 512);
        assert_eq!(c.summary.reclaim_estimate, 100, "placeholder must not inflate reclaim");
    }

    // -- staleness ----------------------------------------------------------

    fn project(marker_mtime_days: i64, artifact_mtime_days: i64) -> Vec<ScanEntry> {
        let mut src = file("/p/src/main.rs", 100);
        src.modified_at = days_ago(marker_mtime_days);
        let mut art = file("/p/target/debug.bin", 5000);
        art.modified_at = days_ago(artifact_mtime_days);
        vec![
            dir("/p"),
            file("/p/Cargo.toml", 10),
            dir("/p/src"),
            src,
            dir("/p/target"),
            art,
        ]
    }

    /// Phase 3: the summary records what the staleness rule saw per project
    /// root, with the hotspot roots directly inside each, so `stale` can
    /// re-threshold from the persisted summary.
    #[test]
    fn summary_records_per_project_activity_with_artifacts() {
        let mut manifest = file("/p/Cargo.toml", 100);
        manifest.modified_at = days_ago(120);
        let mut lock = file("/p/Cargo.lock", 100);
        lock.modified_at = days_ago(120);
        let mut fresh_artifact = file("/p/target/debug/x.rlib", 4096);
        fresh_artifact.modified_at = days_ago(0); // artifact mtimes never count
        let mut undated_manifest = file("/q/package.json", 100);
        undated_manifest.modified_at = None;
        let entries = vec![
            dir("/p"),
            manifest,
            lock,
            dir("/p/target"),
            dir("/p/target/debug"),
            fresh_artifact,
            dir("/q"),
            undated_manifest,
            dir("/q/node_modules"),
            file("/q/node_modules/a.js", 2048),
        ];
        let c = classify(&entries, now());
        let projects = &c.summary.projects;
        assert_eq!(projects.len(), 2, "{projects:?}");
        // Biggest artifact bytes first.
        assert_eq!(projects[0].root, "/p");
        assert_eq!(projects[0].last_activity_days, Some(120));
        assert!(projects[0].dormant);
        assert_eq!(projects[0].artifacts.len(), 1);
        let a = &projects[0].artifacts[0];
        assert_eq!((a.rule_id.as_str(), a.path.as_str()), ("cargo-target", "/p/target"));
        assert_eq!(a.category, Category::StaleProjectArtifact);
        assert_eq!(a.risk_tier, RiskTier::Safe);
        assert_eq!(a.disk_size, 4096);
        assert_eq!(projects[1].root, "/q");
        assert_eq!(projects[1].last_activity_days, None, "no dated evidence: unverifiable");
        assert!(!projects[1].dormant, "unverifiable is never dormant");
        assert_eq!(projects[1].artifacts[0].rule_id, "node-modules");
        assert_eq!(projects[1].artifacts[0].category, Category::RegenerableArtifact);
        assert_eq!(
            projects[1].artifacts[0].risk_tier,
            RiskTier::Caution,
            "the artifact's tier is the group's rule: node_modules without a lockfile is caution"
        );
        // The shared fixture carries the same shape.
        let s: HotspotsSummary = serde_json::from_str(RAW_SUMMARY).unwrap();
        assert_eq!(s.projects.len(), 3);
        assert_eq!(s.projects[2].last_activity_days, None);
    }

    #[test]
    fn project_dormant_at_exactly_ninety_days_upgrades_to_stale() {
        let entries = project(DORMANT_AFTER_DAYS, 0);
        assert_eq!(
            category_of(&entries, "/p/target"),
            Some(Category::StaleProjectArtifact)
        );
        assert_eq!(
            category_of(&entries, "/p/target/debug.bin"),
            Some(Category::StaleProjectArtifact)
        );
    }

    #[test]
    fn project_edited_yesterday_stays_regenerable() {
        let entries = project(1, 200);
        assert_eq!(
            category_of(&entries, "/p/target"),
            Some(Category::RegenerableArtifact)
        );
    }

    #[test]
    fn one_day_short_of_dormant_stays_regenerable() {
        let entries = project(DORMANT_AFTER_DAYS - 1, 300);
        assert_eq!(
            category_of(&entries, "/p/target"),
            Some(Category::RegenerableArtifact)
        );
    }

    #[test]
    fn artifact_mtimes_do_not_wake_a_dormant_project() {
        // cargo-sweep touched target/ this morning; sources are 120 days
        // old. The project is dormant — artifact mtimes lie.
        let entries = project(120, 0);
        assert_eq!(
            category_of(&entries, "/p/target"),
            Some(Category::StaleProjectArtifact)
        );
    }

    #[test]
    fn undated_project_is_not_dormant() {
        // No source mtimes at all: unknown, not stale. Never upgrade on
        // missing evidence.
        let entries = vec![
            dir("/p"),
            file("/p/Cargo.toml", 10),
            dir("/p/target"),
            file("/p/target/debug.bin", 5000),
        ];
        assert_eq!(
            category_of(&entries, "/p/target"),
            Some(Category::RegenerableArtifact)
        );
    }

    #[test]
    fn dormant_project_does_not_stale_a_path_prefix_sibling() {
        // "/proj" is dormant; "/proj-two" is active. A prefix comparison
        // without a component boundary would upgrade /proj-two/target too.
        let mut old_src = file("/proj/src/lib.rs", 10);
        old_src.modified_at = days_ago(365);
        let mut new_src = file("/proj-two/src/lib.rs", 10);
        new_src.modified_at = days_ago(1);
        let entries = vec![
            dir("/proj"),
            file("/proj/Cargo.toml", 1),
            dir("/proj/src"),
            old_src,
            dir("/proj-two"),
            file("/proj-two/Cargo.toml", 1),
            dir("/proj-two/src"),
            new_src,
            dir("/proj-two/target"),
            file("/proj-two/target/b.bin", 300),
        ];
        assert_eq!(
            category_of(&entries, "/proj-two/target"),
            Some(Category::RegenerableArtifact)
        );
    }

    #[test]
    fn package_json_inside_node_modules_is_not_a_project_root() {
        let mut dep_manifest = file("/p/node_modules/dep/package.json", 5);
        dep_manifest.modified_at = days_ago(400);
        let entries = vec![dir("/p"), dir("/p/node_modules"), dep_manifest];
        assert_eq!(dormant_project_roots(&entries, now()), Vec::<String>::new());
    }

    #[test]
    fn stale_and_active_projects_split_the_same_rule_into_two_groups() {
        let mut old_src = file("/old/src/lib.rs", 10);
        old_src.modified_at = days_ago(365);
        let mut new_src = file("/new/src/lib.rs", 10);
        new_src.modified_at = days_ago(1);
        let entries = vec![
            dir("/old"),
            file("/old/Cargo.toml", 1),
            dir("/old/src"),
            old_src,
            dir("/old/target"),
            file("/old/target/a.bin", 700),
            dir("/new"),
            file("/new/Cargo.toml", 1),
            dir("/new/src"),
            new_src,
            dir("/new/target"),
            file("/new/target/b.bin", 300),
        ];
        let c = classify(&entries, now());
        let stale = group(&c.summary, "cargo-target");
        // Groups are sorted stale-first; the first cargo-target group is
        // the dormant one.
        assert_eq!(stale.category, Category::StaleProjectArtifact);
        assert_eq!(stale.disk_size, 700);
        assert_eq!(stale.top_paths, vec!["/old/target".to_string()]);
        let active = c
            .summary
            .groups
            .iter()
            .find(|g| g.rule_id == "cargo-target" && g.category == Category::RegenerableArtifact)
            .unwrap();
        assert_eq!(active.disk_size, 300);
        assert!(
            c.summary.groups.iter().position(|g| g.category == Category::StaleProjectArtifact)
                < c.summary.groups.iter().position(|g| g.category == Category::RegenerableArtifact),
            "stale artifacts sort to the top of the reclaim list"
        );
    }

    // -- hardlink dedup -----------------------------------------------------

    fn hardlinked(path: &str, disk: u64, dev: u64, ino: u64) -> ScanEntry {
        let mut e = file(path, disk);
        e.nlink = 2;
        e.dev = dev;
        e.ino = ino;
        e
    }

    #[test]
    fn hardlinks_within_a_group_count_once_in_reclaim() {
        // The uv incident: the listing says 200, deletion frees 100.
        let entries = vec![
            dir("/u/.cache"),
            hardlinked("/u/.cache/uv/a", 100, 7, 42),
            hardlinked("/u/.cache/uv/b", 100, 7, 42),
        ];
        let c = classify(&entries, now());
        let g = group(&c.summary, "dot-cache");
        assert_eq!(g.listed_disk_size, 200, "listings show every path");
        assert_eq!(g.disk_size, 100, "shared blocks count once");
        assert_eq!(g.private_size, 100, "both links are inside: deleting frees it");
        assert_eq!(g.file_count, 2);
        assert_eq!(c.summary.reclaim_estimate, 100);
    }

    /// The venv shape: one link in the hotspot, its twin in a source tree
    /// outside any hotspot. du charges the hotspot; deletion frees nothing.
    #[test]
    fn hardlink_with_its_twin_outside_the_hotspot_is_not_reclaimable() {
        let entries = vec![
            dir("/u/.cache"),
            dir("/u/src"),
            hardlinked("/u/.cache/uv/a", 100, 7, 42),
            hardlinked("/u/src/venv/a", 100, 7, 42),
        ];
        let c = classify(&entries, now());
        let g = group(&c.summary, "dot-cache");
        assert_eq!(g.disk_size, 100, "du model still charges the first link");
        assert_eq!(g.private_size, 0, "the venv's link pins the blocks");
        assert_eq!(c.summary.reclaim_estimate, 0);
    }

    fn cloned(path: &str, disk: u64, ino: u64, clone_id: u64) -> ScanEntry {
        let mut e = file(path, disk);
        e.dev = 7;
        e.ino = ino;
        e.clone_id = Some(clone_id);
        e.private_size = Some(0);
        e.shared_size = Some(disk);
        e
    }

    // Mutation-proof (phantom-mkn.1, the plan's mutation #2): swap
    // `privateSize` for `diskSize` in the estimate and this reads 1 MiB
    // instead of 0. A Finder-duplicated project: the copy's node_modules is
    // a hotspot, but every file in it is a pure clone of the original's —
    // deleting the copy frees ~nothing.
    #[test]
    fn cloned_tree_reclaims_nothing() {
        let entries = vec![
            dir("/u/orig"),
            dir("/u/orig/node_modules"),
            dir("/u/copy"),
            dir("/u/copy/node_modules"),
            cloned("/u/orig/node_modules/a.js", 1 << 20, 10, 10),
            cloned("/u/copy/node_modules/a.js", 1 << 20, 11, 10),
            // A plain file in the copy IS freed.
            file("/u/copy/node_modules/own.js", 4096),
        ];
        let c = classify(&entries, now());
        let g = group(&c.summary, "node-modules");
        assert_eq!(g.listed_disk_size, (2 << 20) + 4096, "every path listed");
        assert_eq!(g.disk_size, (1 << 20) + 4096, "the clone stream counts once");
        // Both node_modules dirs are the SAME hotspot group, so every
        // reference to the stream is inside it: freeable. Deleting both
        // node_modules frees the stream once plus the plain file.
        assert_eq!(g.private_size, (1 << 20) + 4096);
        assert_eq!(c.summary.reclaim_estimate, (1 << 20) + 4096);

        // Now the original is NOT a hotspot (plain sources): the copy's
        // node_modules alone reclaims only its own plain file.
        let entries = vec![
            dir("/u/orig"),
            dir("/u/orig/src"),
            dir("/u/copy"),
            dir("/u/copy/node_modules"),
            cloned("/u/orig/src/a.js", 1 << 20, 10, 10),
            cloned("/u/copy/node_modules/a.js", 1 << 20, 11, 10),
            file("/u/copy/node_modules/own.js", 4096),
        ];
        let c = classify(&entries, now());
        let g = group(&c.summary, "node-modules");
        assert_eq!(g.disk_size, (1 << 20) + 4096, "du still says a megabyte");
        assert_eq!(g.private_size, 4096, "deleting the cloned tree frees ~0");
        assert_eq!(c.summary.reclaim_estimate, 4096);
    }

    /// A modified clone's rewritten blocks are reclaimable; the rest reads
    /// as shared. A snapshot-trapped file reclaims nothing.
    #[test]
    fn ungrouped_private_bytes_flow_into_the_estimate() {
        let mut modified = file("/u/.cache/modified.bin", 1 << 20);
        modified.private_size = Some(16_384);
        let mut trapped = file("/u/.cache/trapped.bin", 1 << 20);
        trapped.private_size = Some(0);
        let entries = vec![dir("/u/.cache"), modified, trapped];
        let c = classify(&entries, now());
        let g = group(&c.summary, "dot-cache");
        assert_eq!(g.disk_size, 2 << 20);
        assert_eq!(g.private_size, 16_384);
        assert_eq!(c.summary.reclaim_estimate, 16_384);
    }

    #[test]
    fn same_inode_on_different_devices_is_not_a_hardlink() {
        let entries = vec![
            dir("/u/.cache"),
            hardlinked("/u/.cache/a", 100, 7, 42),
            hardlinked("/u/.cache/b", 100, 8, 42),
        ];
        let c = classify(&entries, now());
        assert_eq!(group(&c.summary, "dot-cache").disk_size, 200);
    }

    #[test]
    fn hardlink_spanning_two_groups_counts_once_globally() {
        // Each group honestly reports its own deduped size, but the global
        // estimate must not promise the same blocks twice.
        let entries = vec![
            dir("/u/.cache"),
            dir("/u/.npm"),
            hardlinked("/u/.cache/a", 100, 7, 42),
            hardlinked("/u/.npm/a", 100, 7, 42),
        ];
        let c = classify(&entries, now());
        assert_eq!(group(&c.summary, "dot-cache").disk_size, 100);
        assert_eq!(group(&c.summary, "dot-npm").disk_size, 100);
        assert_eq!(c.summary.reclaim_estimate, 100);
    }

    // -- nesting / governance -----------------------------------------------

    #[test]
    fn nested_hotspots_collapse_to_the_outermost_root() {
        let entries = vec![
            dir("/u/.cache"),
            dir("/u/.cache/some-tool/node_modules"),
            file("/u/.cache/some-tool/node_modules/x.js", 50),
        ];
        let c = classify(&entries, now());
        assert_eq!(group(&c.summary, "dot-cache").disk_size, 50);
        assert!(
            !c.summary.groups.iter().any(|g| g.rule_id == "node-modules"),
            "inner hotspot must not double-count: {:?}",
            c.summary.groups
        );
        assert_eq!(
            category_of(&entries, "/u/.cache/some-tool/node_modules/x.js"),
            Some(Category::ToolManagedCache)
        );
    }

    #[test]
    fn review_first_and_wont_regenerate_feed_review_not_reclaim() {
        let entries = vec![
            dir("/u/Library/Group Containers"),
            file("/u/Library/Group Containers/g/data", 400),
            dir("/u/Library/CloudStorage"),
            file("/u/Library/CloudStorage/OneDrive/doc", 600),
        ];
        let c = classify(&entries, now());
        assert_eq!(c.summary.reclaim_estimate, 0);
        assert_eq!(c.summary.review_disk_size, 1000);
    }

    const GIB: u64 = 1024 * 1024 * 1024;

    /// `n` hotspot roots of one rule, `/code/p{i}/node_modules`, root i
    /// holding one file of `size(i)` bytes.
    fn node_modules_roots(n: usize, size: impl Fn(usize) -> u64) -> Vec<ScanEntry> {
        let mut entries = vec![dir("/code")];
        for i in 0..n {
            let root = format!("/code/p{i}/node_modules");
            entries.push(dir(&format!("/code/p{i}")));
            entries.push(dir(&root));
            entries.push(file(&format!("{root}/x.js"), size(i)));
        }
        entries
    }

    #[test]
    fn a_group_of_seven_nine_gib_roots_lists_all_seven() {
        // 2026-09-16: seven worktree targets of 9–26 GB in one group, the
        // plan offered five (phantom-cnr.5).
        let entries = node_modules_roots(7, |i| (9 + i as u64) * GIB);
        let c = classify(&entries, now());
        let g = group(&c.summary, "node-modules");
        assert_eq!(g.top_paths.len(), 7, "every root worth acting on is listed");
        assert_eq!(g.top_paths[0], "/code/p6/node_modules", "biggest root first");
        assert_eq!(g.top_paths[6], "/code/p0/node_modules");
    }

    #[test]
    fn a_group_of_small_roots_still_lists_five() {
        let entries = node_modules_roots(TOP_PATHS_FLOOR + 2, |i| (i as u64 + 1) * 100);
        let c = classify(&entries, now());
        let g = group(&c.summary, "node-modules");
        assert_eq!(g.top_paths.len(), TOP_PATHS_FLOOR, "the 1.1.0 floor for small groups");
        assert_eq!(
            g.top_paths[0],
            format!("/code/p{}/node_modules", TOP_PATHS_FLOOR + 1),
            "biggest root first"
        );
    }

    #[test]
    fn a_four_hundred_root_group_lists_twenty_five() {
        let entries = node_modules_roots(400, |i| GIB + i as u64);
        let c = classify(&entries, now());
        let g = group(&c.summary, "node-modules");
        assert_eq!(g.top_paths.len(), TOP_PATHS_CAP);
        assert_eq!(g.top_paths[0], "/code/p399/node_modules", "the cap keeps the largest");
        assert_eq!(g.top_paths[24], "/code/p375/node_modules");
        assert_eq!(g.file_count, 400, "the cap trims the list, never the group");
    }

    #[test]
    fn top_paths_measure_private_bytes_not_disk() {
        // Six roots of 2 GiB private and one 50 GiB root whose bytes are
        // all pinned (a snapshot or a clone: PRIVATESIZE 0). Deleting the
        // big one frees nothing, so it is not "worth acting on" and the six
        // that are come first — even though it is the largest by disk.
        let mut entries = node_modules_roots(6, |_| 2 * GIB);
        entries.push(dir("/code/pinned"));
        entries.push(dir("/code/pinned/node_modules"));
        let mut pinned = file("/code/pinned/node_modules/x.js", 50 * GIB);
        pinned.private_size = Some(0);
        entries.push(pinned);
        let c = classify(&entries, now());
        let g = group(&c.summary, "node-modules");
        assert_eq!(g.top_paths.len(), 6);
        assert!(
            !g.top_paths.iter().any(|p| p.contains("/pinned/")),
            "a root that frees nothing is not worth acting on: {:?}",
            g.top_paths
        );
        assert_eq!(g.disk_size, 62 * GIB, "the group's size still counts it");
        assert_eq!(g.private_size, 12 * GIB);

        // Below the floor the fill is by disk, so the pinned root appears
        // then — the 1.1.0 list, biggest first.
        let mut entries = node_modules_roots(3, |_| 2 * GIB);
        entries.push(dir("/code/pinned"));
        entries.push(dir("/code/pinned/node_modules"));
        let mut pinned = file("/code/pinned/node_modules/x.js", 50 * GIB);
        pinned.private_size = Some(0);
        entries.push(pinned);
        let c = classify(&entries, now());
        let g = group(&c.summary, "node-modules");
        assert_eq!(g.top_paths.len(), 4, "everything there is, up to the floor");
        assert_eq!(g.top_paths[0], "/code/pinned/node_modules", "filled in disk order");
    }

    #[test]
    fn a_sharing_group_counts_for_a_root_only_when_wholly_inside_it() {
        // Five singleton roots of 2 GiB, plus p5 and p6 holding the two
        // links of ONE 2 GiB inode. The group frees that inode (both links
        // inside it), so the group's private is 12 GiB; but neither p5 nor
        // p6 alone frees it, so neither is worth acting on and the list is
        // the five singletons. (Deduped disk charges the inode to the root
        // seen first, p5, which is why p6 never appears at all.) Mutation:
        // credit the sharing group to the first root seen and p5 is listed
        // as a sixth root whose deletion frees nothing.
        let mut entries = node_modules_roots(5, |_| 2 * GIB);
        for (i, name) in [(5, "x.js"), (6, "x.js")] {
            entries.push(dir(&format!("/code/p{i}")));
            entries.push(dir(&format!("/code/p{i}/node_modules")));
            let mut f = file(&format!("/code/p{i}/node_modules/{name}"), 2 * GIB);
            f.nlink = 2;
            f.ino = 4242;
            f.private_size = Some(0);
            f.shared_size = Some(2 * GIB);
            entries.push(f);
        }
        let c = classify(&entries, now());
        let g = group(&c.summary, "node-modules");
        assert_eq!(g.disk_size, 12 * GIB, "one inode, counted once");
        assert_eq!(g.private_size, 12 * GIB, "both links inside the group: deleting the group frees it");
        assert_eq!(g.top_paths.len(), 5, "a root that frees nothing on its own is not listed: {:?}", g.top_paths);
        assert!(!g.top_paths.iter().any(|p| p.contains("/p5/")), "{:?}", g.top_paths);

        // Now the same inode's two links under ONE root: that root frees
        // it, so beside five singleton roots it is the sixth worth acting on.
        let mut entries = node_modules_roots(5, |_| 2 * GIB);
        entries.push(dir("/code/shared"));
        entries.push(dir("/code/shared/node_modules"));
        let mut a = file("/code/shared/node_modules/x.js", 2 * GIB);
        let mut b = file("/code/shared/node_modules/y.js", 2 * GIB);
        for f in [&mut a, &mut b] {
            f.nlink = 2;
            f.ino = 4242;
            f.private_size = Some(0);
            f.shared_size = Some(2 * GIB);
        }
        entries.push(a);
        entries.push(b);
        let c = classify(&entries, now());
        let g = group(&c.summary, "node-modules");
        assert_eq!(g.private_size, 12 * GIB);
        assert_eq!(g.top_paths.len(), 6, "the linked pair's root is worth acting on too: {:?}", g.top_paths);
        assert!(g.top_paths.contains(&"/code/shared/node_modules".to_string()));
    }

    // -- wire shape -----------------------------------------------------------

    #[test]
    fn summary_decodes_from_raw_fixture_bytes() {
        let s: HotspotsSummary = serde_json::from_str(RAW_SUMMARY).unwrap();
        assert_eq!(s.groups.len(), 4);
        assert_eq!(s.groups[0].category, Category::StaleProjectArtifact);
        // v1.1 tier fields, from the raw bytes.
        assert_eq!(s.groups[0].risk_tier, RiskTier::Safe);
        assert!(s.groups[0].why.ends_with("at least 90 days old."), "{}", s.groups[0].why);
        assert_eq!(s.groups[0].rebuild_cost.kind, RebuildKind::Compile);
        assert_eq!(s.groups[0].tool_estimate, None);
        assert_eq!(s.groups[1].risk_tier, RiskTier::Caution);
        assert_eq!(s.groups[1].rebuild_cost.kind, RebuildKind::Download);
        let brew = s.groups[2].tool_estimate.as_ref().expect("brew group carries a tool estimate");
        assert_eq!((brew.tool.as_str(), brew.reclaimable_bytes), ("brew", 734_003_200));
        assert_eq!(brew.command, "brew cleanup -n");
        assert_eq!(s.groups[3].risk_tier, RiskTier::Review);
        assert_eq!(s.groups[3].rebuild_cost.kind, RebuildKind::None);
        // command: a real cleanup command where one honestly exists, null
        // where none does (mixed-tool caches, informational categories).
        assert_eq!(s.groups[0].command.as_deref(), Some("cargo clean"));
        assert_eq!(s.groups[1].command, None, "mixed-tool cache has no ONE command");
        assert_eq!(s.groups[3].command, None, "cloudDataloaded is informational");
        assert_eq!(s.groups[1].rule_id, "dot-cache");
        assert_eq!(s.groups[1].disk_size, 5_368_709_120);
        assert_eq!(s.groups[1].listed_disk_size, 18_253_611_008);
        // The uv store: du says 5 GB, deleting frees 1 GB (the venvs pin
        // the rest); the estimate is Σ privateSize.
        assert_eq!(s.groups[1].private_size, 1_073_741_824);
        assert_eq!(s.groups[0].private_size, 17_179_869_184);
        assert_eq!(s.reclaim_estimate, 20_401_094_656);
        assert_eq!(s.cloud_dataloaded_logical_size, 154_140_672);
        assert_eq!(s.cloud_dataloaded_disk_size, 147_456);
    }

    #[test]
    fn summary_encodes_camel_case_at_every_depth() {
        let s: HotspotsSummary = serde_json::from_str(RAW_SUMMARY).unwrap();
        let v = serde_json::to_value(&s).unwrap();
        let obj = v.as_object().unwrap();
        for key in [
            "groups",
            "reclaimEstimate",
            "reviewDiskSize",
            "cloudDataloadedLogicalSize",
            "cloudDataloadedDiskSize",
        ] {
            assert!(obj.contains_key(key), "summary missing {key}");
        }
        let g = v["groups"][0].as_object().unwrap();
        for key in [
            "ruleId", "label", "category", "hint", "command", "riskTier", "why", "rebuildCost",
            "toolEstimate", "diskSize", "listedDiskSize", "privateSize", "logicalSize",
            "fileCount", "topPaths",
        ] {
            assert!(g.contains_key(key), "group missing {key}");
        }
        assert_eq!(g["category"], "staleProjectArtifact");
        assert_eq!(g["riskTier"], "safe");
        assert_eq!(g["rebuildCost"]["kind"], "compile");
        assert!(g["rebuildCost"].as_object().unwrap().contains_key("estimate"));
        // Nullable-present-as-null: a group without a tool estimate carries the key.
        assert!(g.contains_key("toolEstimate") && g["toolEstimate"].is_null());
        // The brew group's estimate is camelCase at depth too.
        let brew = v["groups"][2]["toolEstimate"].as_object().unwrap();
        for key in ["tool", "command", "reclaimableBytes", "note"] {
            assert!(brew.contains_key(key), "toolEstimate missing {key}");
        }
        // Nullable-present-as-null: a command-less group still carries the key.
        let no_cmd = v["groups"][1].as_object().unwrap();
        assert!(no_cmd.contains_key("command"), "null command must be PRESENT");
        assert!(no_cmd["command"].is_null());
    }

    /// Summaries persisted before the command field decode generously: the
    /// key may be ABSENT in old rows and reads as None (canonical encode is
    /// still present-as-null).
    #[test]
    fn summary_without_command_key_decodes_as_none() {
        let raw = RAW_SUMMARY.replace("\n      \"command\": \"cargo clean\",", "");
        assert!(!raw.contains("cargo clean\","), "precondition: key removed");
        let s: HotspotsSummary = serde_json::from_str(&raw).unwrap();
        assert_eq!(s.groups[0].command, None);
    }

    /// Summaries persisted before v1.1 have no privateSize: decode as 0
    /// (canonical encode is still present).
    #[test]
    fn summary_without_private_size_decodes_as_zero() {
        let raw = RAW_SUMMARY.replace("\n      \"privateSize\": 17179869184,", "");
        assert!(!raw.contains("\"privateSize\": 17179869184"), "precondition: key removed");
        let s: HotspotsSummary = serde_json::from_str(&raw).unwrap();
        assert_eq!(s.groups[0].private_size, 0);
    }

    /// R1's honesty rule as a registry INVARIANT: informational and
    /// review-only categories never carry a copy-runnable command — a
    /// `git status` CHECK is not a cleanup, and Phantom never suggests
    /// deleting data it classified as risky.
    #[test]
    fn advice_only_categories_never_carry_a_command() {
        for rule in REGISTRY {
            match rule.category {
                Category::ReviewFirst
                | Category::WontRegenerate
                | Category::CloudDataloaded => {
                    assert_eq!(
                        rule.command, None,
                        "rule {} ({:?}) must not offer a command",
                        rule.id, rule.category
                    );
                }
                _ => {}
            }
        }
        // And the flagship regenerable rule DOES carry its real one.
        let cargo = REGISTRY.iter().find(|r| r.id == "cargo-target").unwrap();
        assert_eq!(cargo.command, Some("cargo clean"));
        // The motivating bug: the worktrees rule's git-status hint must
        // never again surface as a cleanup command.
        let worktrees = REGISTRY.iter().find(|r| r.id == "agent-worktrees").unwrap();
        assert_eq!(worktrees.command, None);
    }

    #[test]
    fn category_round_trips_through_db_strings() {
        for c in Category::ALL {
            assert_eq!(Category::from_str(c.as_str()).unwrap(), c);
            // The DB string and the wire string are the same string.
            assert_eq!(
                serde_json::to_value(c).unwrap(),
                serde_json::Value::String(c.as_str().to_string())
            );
        }
    }

    #[test]
    fn category_from_unknown_string_is_invalid_input() {
        let err = Category::from_str("deletable").unwrap_err();
        assert!(matches!(err, CoreError::InvalidInput(_)));
    }

    #[test]
    fn every_category_carries_an_action_hint_and_never_a_delete_verb_alone() {
        for c in Category::ALL {
            assert!(!c.action_hint().is_empty());
        }
        assert!(Category::ToolManagedCache.action_hint().contains("toolbox clean"));
    }

    #[test]
    fn registry_carries_exactly_one_cloud_row_and_ends_with_the_catch_all() {
        let clouds = REGISTRY
            .iter()
            .filter(|r| r.matcher == Matcher::CloudDataloadedFile)
            .count();
        assert_eq!(clouds, 1);
        assert!(
            matches!(REGISTRY.last().unwrap().matcher, Matcher::LargeFile { .. }),
            "the catch-all must stay last: precedence is registry order"
        );
    }

    // -- v1.1 Phase 2: the project table -----------------------------------------

    /// A concrete name for a registry pattern: `*.py` → `main.py`,
    /// `cmake-build-*` → `cmake-build-debug`, else the name itself.
    fn concrete(pattern: &str) -> String {
        if let Some(ext) = pattern.strip_prefix('*') {
            format!("main{ext}")
        } else if let Some(prefix) = pattern.strip_suffix('*') {
            format!("{prefix}debug")
        } else {
            pattern.to_string()
        }
    }

    /// EVERY project row, generically: the artifact classifies regenerable
    /// beside its detection file and as nothing without it. This is the
    /// plan's mutation #1 as a table-wide invariant — remove a detection
    /// pattern from any row and the "with detection" half fails for it.
    #[test]
    fn every_project_row_needs_its_detection_file() {
        let mut rows = 0;
        for rule in REGISTRY {
            let Matcher::ProjectArtifact { detection, artifacts } = rule.matcher else { continue };
            rows += 1;
            for det in detection {
                for art in artifacts {
                    let art_dir = format!("/p/{}", concrete(art));
                    let art_file = format!("{art_dir}/a.bin");
                    let with = vec![
                        dir("/p"),
                        file(&format!("/p/{}", concrete(det)), 1),
                        dir(&art_dir),
                        file(&art_file, 500),
                    ];
                    assert_eq!(
                        category_of(&with, &art_file),
                        Some(Category::RegenerableArtifact),
                        "{} : {det} → {art}",
                        rule.id
                    );
                    let c = classify(&with, now());
                    assert_eq!(c.summary.groups.len(), 1, "{}: exactly one group", rule.id);
                    assert_eq!(c.summary.groups[0].rule_id, rule.id, "{det} → {art} must land on its own row");
                    let without = vec![dir("/p"), dir(&art_dir), file(&art_file, 500)];
                    assert_eq!(
                        category_of(&without, &art_file),
                        None,
                        "{}: {art} without {det} must be nothing",
                        rule.id
                    );
                }
            }
        }
        // kondo's 24 types (node_modules and .venv are separate sibling-free
        // rows) plus Bazel and Go vendor: the table is at least this wide.
        assert!(rows >= 28, "project table has {rows} rows");
    }

    /// Gate G2: the checked-in fixture projects, walked by the REAL scanner,
    /// classify to EXACTLY one group per project type — and the decoys
    /// (same artifact names, no detection file) to nothing. Delete a
    /// detection file under tests/fixtures/projects and this fails.
    /// Rebuild a tree under `dst` by READING and WRITING every file — never
    /// `fs::copy`, which is `clonefile` on APFS and would make the copy a
    /// pure clone of the source (CLAUDE.md Gotchas). Fresh writes allocate
    /// fresh blocks, and fresh blocks belong to no existing APFS snapshot:
    /// the MBP build server takes hourly local Time Machine snapshots, and a
    /// snapshot pins the blocks of every file that existed when it was taken,
    /// so the checked-out fixtures there report `PRIVATESIZE` 0 — deleting
    /// them would free nothing until the snapshot expires, which is TRUE and
    /// exactly what Phantom says about snapshot-pinned space. The assertions
    /// below ("every reclaimable byte is private") are a claim about a tree
    /// nothing else references, so the test must make that tree itself
    /// (first runner CI on mbp-phantom-dev, 2026-09-17: node-modules private
    /// 0 vs disk 8192).
    fn copy_tree_with_fresh_writes(src: &std::path::Path, dst: &std::path::Path) {
        std::fs::create_dir_all(dst).unwrap();
        for entry in std::fs::read_dir(src).unwrap() {
            let entry = entry.unwrap();
            let target = dst.join(entry.file_name());
            let ft = entry.file_type().unwrap();
            assert!(!ft.is_symlink(), "fixture tree holds a symlink: {}", entry.path().display());
            if ft.is_dir() {
                copy_tree_with_fresh_writes(&entry.path(), &target);
            } else {
                std::fs::write(&target, std::fs::read(entry.path()).unwrap()).unwrap();
            }
        }
    }

    #[test]
    fn fixture_projects_classify_exactly() {
        use crate::scanner::{ScanProgress, scan_directory};
        use std::sync::atomic::AtomicBool;
        let checked_in = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/projects");
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("projects");
        copy_tree_with_fresh_writes(&checked_in.canonicalize().unwrap(), &root);
        let root = root.canonicalize().unwrap();
        let outcome = scan_directory(&root, &ScanProgress::default(), &AtomicBool::new(false)).unwrap();
        let c = classify_with(&outcome.entries, &outcome.shares, Utc::now());

        let root_str = root.to_str().unwrap();
        let mut got: Vec<(String, String)> = c
            .summary
            .groups
            .iter()
            .flat_map(|g| {
                g.top_paths
                    .iter()
                    .map(move |p| (g.rule_id.clone(), p.strip_prefix(root_str).unwrap().to_string()))
            })
            .collect();
        got.sort();
        let mut want: Vec<(String, String)> = [
            ("bazel-output", "/bazel/bazel-out"),
            ("cabal-dist", "/cabal/dist-newstyle"),
            ("cargo-target", "/cargo/target"),
            ("cmake-build", "/cmake/cmake-build-debug"),
            ("cocoapods-pods", "/cocoapods/Pods"),
            ("composer-vendor", "/composer/vendor"),
            ("dotnet-bin-obj", "/dotnet/obj"),
            ("elixir-build", "/elixir/_build"),
            ("flutter-build", "/flutter/.dart_tool"),
            ("go-vendor", "/go/vendor"),
            ("godot-import", "/godot/.godot"),
            ("gradle-build", "/gradle/build"),
            ("js-build", "/js-build/build"),
            ("js-dist", "/js-dist/dist"),
            ("jupyter-checkpoints", "/jupyter/.ipynb_checkpoints"),
            ("maven-target", "/maven/target"),
            ("next-build", "/next/.next"),
            ("node-modules", "/node/node_modules"),
            ("pixi-env", "/pixi/.pixi"),
            ("python-bytecode", "/python-bytecode/__pycache__"),
            ("python-caches", "/python-caches/.pytest_cache"),
            ("python-venv", "/python-venv/.venv"),
            ("react-native-cache", "/react-native/.expo"),
            ("sbt-target", "/sbt/target"),
            ("stack-work", "/stack/.stack-work"),
            ("swiftpm-build", "/swiftpm/.build"),
            ("terraform-providers", "/terraform/.terraform"),
            ("turborepo-cache", "/turborepo/.turbo"),
            ("unity-library", "/unity/Library"),
            ("unreal-intermediate", "/unreal/Intermediate"),
            ("zig-cache", "/zig/zig-out"),
        ]
        .iter()
        .map(|(r, p)| (r.to_string(), p.to_string()))
        .collect();
        want.sort();
        assert_eq!(got, want, "the exact hotspot set over tests/fixtures/projects");
        // Nothing under decoys/ is classified, and the nested dep's
        // build/ belongs to node_modules, not to a js-build group.
        for (e, cat) in outcome.entries.iter().zip(&c.categories) {
            if e.path.contains("/decoys/") {
                assert_eq!(*cat, None, "decoy classified: {}", e.path);
            }
            if e.path.ends_with("/node_modules/dep/build/out.js") {
                assert_eq!(*cat, Some(Category::RegenerableArtifact));
            }
        }
        assert!(!c.summary.groups.iter().any(|g| g.top_paths.iter().any(|p| p.contains("/dep/build"))));
        // Every group here is regenerable (the copy's mtimes are fresh) and
        // every reclaimable byte is private: the tree was just written, so
        // no clone, hardlink or snapshot references its blocks. (On the
        // checked-in tree this was false on a Time Machine host — see
        // copy_tree_with_fresh_writes.)
        for g in &c.summary.groups {
            assert_eq!(g.category, Category::RegenerableArtifact, "{}", g.rule_id);
            assert_eq!(g.private_size, g.disk_size, "{}", g.rule_id);
        }
    }

    #[test]
    fn wildcard_detection_needs_the_extension_not_a_bare_dot_file() {
        // `*.py` must not accept a file literally named ".py", nor "py".
        assert!(name_matches("*.py", "main.py"));
        assert!(!name_matches("*.py", ".py"));
        assert!(!name_matches("*.py", "py"));
        assert!(name_matches("bazel-*", "bazel-out"));
        assert!(!name_matches("bazel-*", "bazel-"));
        assert!(!name_matches("target", "target2"));
    }

    // -- v1.1 Phase 2: tiers, why, rebuild cost -------------------------------------

    fn cargo_project(with_lock: bool) -> Vec<ScanEntry> {
        let mut v = vec![dir("/p"), file("/p/Cargo.toml", 10), dir("/p/target"), file("/p/target/debug.bin", 5000)];
        if with_lock {
            v.push(file("/p/Cargo.lock", 10));
        }
        v
    }

    /// The plan's mutation #3: delete the lockfile and the tier drops from
    /// `safe`. Both halves pinned, with the why sentence naming the reason.
    #[test]
    fn lockfile_gates_safe_for_regenerable_artifacts() {
        let c = classify(&cargo_project(true), now());
        let g = group(&c.summary, "cargo-target");
        assert_eq!(g.risk_tier, RiskTier::Safe);
        assert!(g.why.contains("a lockfile pins the dependency versions"), "{}", g.why);
        assert_eq!(g.rebuild_cost.kind, RebuildKind::Compile);
        assert_eq!(g.rebuild_cost.estimate, "re-compile ≈ 5.0 KB of build output");

        let c = classify(&cargo_project(false), now());
        let g = group(&c.summary, "cargo-target");
        assert_eq!(g.risk_tier, RiskTier::Caution);
        assert!(g.why.contains("no lockfile (Cargo.lock) beside it"), "{}", g.why);
        assert!(g.why.ends_with('.'), "one sentence: {}", g.why);
    }

    #[test]
    fn a_scan_rooted_at_node_modules_cannot_prove_its_lockfile() {
        let mut root = dir("/u/x/node_modules");
        root.parent_path = None;
        let entries = vec![root, file("/u/x/node_modules/a.js", 100)];
        let c = classify(&entries, now());
        let g = group(&c.summary, "node-modules");
        assert_eq!(g.category, Category::RegenerableArtifact);
        assert_eq!(g.risk_tier, RiskTier::Caution, "parent outside the scan: unobservable ≠ present");
        assert_eq!(g.rebuild_cost.kind, RebuildKind::Download);
        assert_eq!(g.rebuild_cost.estimate, "re-download ≈ 100 B");
    }

    #[test]
    fn locked_and_unlocked_projects_split_the_same_rule_into_two_groups() {
        let entries = vec![
            dir("/a"), file("/a/Cargo.toml", 1), file("/a/Cargo.lock", 1), dir("/a/target"), file("/a/target/x", 700),
            dir("/b"), file("/b/Cargo.toml", 1), dir("/b/target"), file("/b/target/y", 300),
        ];
        let c = classify(&entries, now());
        let cargo: Vec<&HotspotGroup> = c.summary.groups.iter().filter(|g| g.rule_id == "cargo-target").collect();
        assert_eq!(cargo.len(), 2, "{:?}", c.summary.groups);
        assert_eq!((cargo[0].risk_tier, cargo[0].disk_size), (RiskTier::Safe, 700), "safe sorts first");
        assert_eq!((cargo[1].risk_tier, cargo[1].disk_size), (RiskTier::Caution, 300));
        assert_eq!(c.summary.reclaim_estimate, 1000);
    }

    #[test]
    fn tier_rubric_covers_every_category() {
        let cargo = REGISTRY.iter().find(|r| r.id == "cargo-target").unwrap();
        let caches = REGISTRY.iter().find(|r| r.id == "library-caches").unwrap();
        let noop = LockState::NotApplicable;
        assert_eq!(tier_for(caches, Category::Cache, noop), RiskTier::Safe);
        assert_eq!(tier_for(caches, Category::ToolManagedCache, noop), RiskTier::Caution);
        assert_eq!(tier_for(caches, Category::ModelCache, noop), RiskTier::Caution);
        for c in [Category::ReviewFirst, Category::WontRegenerate, Category::CloudDataloaded] {
            assert_eq!(tier_for(caches, c, noop), RiskTier::Review, "{c:?}");
        }
        for c in [Category::RegenerableArtifact, Category::StaleProjectArtifact] {
            assert_eq!(tier_for(cargo, c, LockState::Present), RiskTier::Safe);
            assert_eq!(tier_for(cargo, c, LockState::Verified), RiskTier::Safe);
            assert_eq!(tier_for(cargo, c, LockState::Missing), RiskTier::Caution);
            assert_eq!(tier_for(cargo, c, LockState::Failed), RiskTier::Caution);
            // A rule with no lockfile concept is safe outright.
            assert_eq!(tier_for(caches, c, noop), RiskTier::Safe);
        }
    }

    #[test]
    fn every_rule_has_a_why_and_review_rows_never_promise_a_rebuild() {
        for rule in REGISTRY {
            assert!(!rule.why.is_empty() && !rule.why.ends_with('.'), "{}: why is a clause, the classifier ends the sentence", rule.id);
            if matches!(rule.category, Category::ReviewFirst | Category::WontRegenerate | Category::CloudDataloaded) {
                assert_eq!(rule.rebuild, RebuildKind::None, "{}", rule.id);
                assert!(rule.lockfiles.is_empty() && rule.verify.is_none(), "{}", rule.id);
            }
            if let Some(v) = rule.verify {
                assert!(rule.lockfiles.contains(&v.requires), "{}: verify requires a listed lockfile", rule.id);
            }
            if rule.carve_out {
                assert!(matches!(rule.category, Category::ModelCache | Category::ToolManagedCache), "{}", rule.id);
            }
        }
    }

    /// Summaries persisted before v1.1 carry none of the tier fields: they
    /// decode as UNRATED (review, a legacy why, a none rebuild, no tool
    /// estimate) — never as safe.
    #[test]
    fn summary_without_tier_fields_decodes_as_unrated() {
        let mut v: serde_json::Value = serde_json::from_str(RAW_SUMMARY).unwrap();
        let g = v["groups"][0].as_object_mut().unwrap();
        for key in ["riskTier", "why", "rebuildCost", "toolEstimate"] {
            assert!(g.remove(key).is_some(), "precondition: {key} present in the fixture");
        }
        let s: HotspotsSummary = serde_json::from_value(v).unwrap();
        let g = &s.groups[0];
        assert_eq!(g.risk_tier, RiskTier::Review);
        assert!(g.why.contains("classified before v1.1"), "{}", g.why);
        assert_eq!(g.rebuild_cost.kind, RebuildKind::None);
        assert!(g.rebuild_cost.estimate.contains("not assessed"));
        assert_eq!(g.tool_estimate, None);
        // Canonical encode is present-as-null / present for every field.
        let out = serde_json::to_value(&s).unwrap();
        let g = out["groups"][0].as_object().unwrap();
        assert_eq!(g["riskTier"], "review");
        assert!(g.contains_key("toolEstimate") && g["toolEstimate"].is_null());
    }

    #[test]
    fn model_caches_are_reclaimable_with_caution_and_a_redownload_cost() {
        let entries = vec![
            dir("/u/.ollama"), dir("/u/.ollama/models"), file("/u/.ollama/models/blobs/sha256-a", 4_000_000_000),
            dir("/u/.cache"), dir("/u/.cache/huggingface"), dir("/u/.cache/huggingface/hub"),
            file("/u/.cache/huggingface/hub/models--x/w.safetensors", 2_000_000_000),
            file("/u/.cache/pip/other", 1000),
            dir("/u/.cache/whisper"), file("/u/.cache/whisper/base.pt", 500),
            dir("/u/.cache/torch"), dir("/u/.cache/torch/hub"), file("/u/.cache/torch/hub/ck.pth", 600),
            dir("/u/.cache/vllm"), file("/u/.cache/vllm/k", 700),
            dir("/u/.triton"), dir("/u/.triton/cache"), file("/u/.triton/cache/k.cubin", 800),
        ];
        let c = classify(&entries, now());
        for id in ["ollama-models", "huggingface-hub", "whisper-models", "torch-hub", "vllm-cache", "triton-cache"] {
            let g = group(&c.summary, id);
            assert_eq!(g.category, Category::ModelCache, "{id}");
            assert_eq!(g.risk_tier, RiskTier::Caution, "{id}");
            assert_eq!(g.command, None, "{id}: no single honest command");
        }
        assert_eq!(group(&c.summary, "ollama-models").rebuild_cost.estimate, "re-download ≈ 4.0 GB");
        // Carve-out: the hub inside ~/.cache is its OWN group, and the
        // .cache group's bytes exclude it — nothing is counted twice.
        assert_eq!(group(&c.summary, "huggingface-hub").disk_size, 2_000_000_000);
        assert_eq!(group(&c.summary, "dot-cache").disk_size, 1000, "only pip/other remains in ~/.cache");
        assert_eq!(
            category_of(&entries, "/u/.cache/huggingface/hub/models--x/w.safetensors"),
            Some(Category::ModelCache)
        );
        assert_eq!(category_of(&entries, "/u/.cache/pip/other"), Some(Category::ToolManagedCache));
        // Model caches count toward the estimate (re-downloadable).
        assert_eq!(c.summary.reclaim_estimate, 4_000_000_000 + 2_000_000_000 + 1000 + 500 + 600 + 700 + 800);
    }

    #[test]
    fn uv_cache_is_carved_out_of_dot_cache_with_its_own_command() {
        let entries = vec![
            dir("/u/.cache"), dir("/u/.cache/uv"), file("/u/.cache/uv/archive/a", 300),
            file("/u/.cache/pnpm/b", 200),
        ];
        let c = classify(&entries, now());
        assert_eq!(group(&c.summary, "uv-cache").disk_size, 300);
        assert_eq!(group(&c.summary, "uv-cache").command.as_deref(), Some("uv cache prune"));
        assert_eq!(group(&c.summary, "dot-cache").disk_size, 200);
    }

    // -- v1.1 Phase 2: staleness with git, threshold, unverifiable ----------------

    fn flagged(path: &str, disk: u64, logical: u64, flags: EntryFlags) -> ScanEntry {
        let mut e = file(path, disk);
        e.logical_size = logical;
        e.flags = Some(flags);
        e
    }

    #[test]
    fn dataless_flag_is_the_cloud_signal_when_flags_are_recorded() {
        // A tiny placeholder below the heuristic's floor is still a placeholder.
        let e = flagged("/u/OneDrive/a.docx", 0, 4096, EntryFlags::DATALESS);
        assert!(is_cloud_dataloaded(&e));
        // decmpfs: logical ≫ disk, bytes local — NOT a placeholder.
        let e = flagged("/u/big.dat", 512, 64 << 20, EntryFlags::COMPRESSED);
        assert!(!is_cloud_dataloaded(&e));
        // A sparse file at a huge ratio: not a placeholder either.
        let e = flagged("/u/vm.img", 1 << 20, 64 << 30, EntryFlags::SPARSE);
        assert!(!is_cloud_dataloaded(&e));
        // Flags recorded and empty: the heuristic does NOT fire.
        let e = flagged("/u/odd.bin", 512, 64 << 20, EntryFlags::empty());
        assert!(!is_cloud_dataloaded(&e));
        // No flags (pre-v5 row): the heuristic still speaks.
        let e = cloud_file("/u/old.bin", 512, 64 << 20);
        assert!(is_cloud_dataloaded(&e));
        // And a compressed file that is ALSO big lands in large-file review, not cloud.
        let big = flagged("/u/big.dat", LARGE_FILE_REVIEW_MIN_DISK, 64 << 30, EntryFlags::COMPRESSED);
        assert_eq!(category_of(&[big], "/u/big.dat"), Some(Category::ReviewFirst));
    }

    fn git_project(src_days: i64, git_days: Option<i64>) -> Vec<ScanEntry> {
        let mut src = file("/p/src/main.rs", 100);
        src.modified_at = days_ago(src_days);
        let mut v = vec![dir("/p"), file("/p/Cargo.toml", 10), file("/p/Cargo.lock", 10), dir("/p/src"), src, dir("/p/target"), file("/p/target/debug.bin", 5000)];
        if let Some(d) = git_days {
            v.push(dir("/p/.git"));
            v.push(dir("/p/.git/logs"));
            let mut reflog = file("/p/.git/logs/HEAD", 50);
            reflog.modified_at = days_ago(d);
            v.push(reflog);
            // An old object under .git must NOT count as source activity.
            let mut obj = file("/p/.git/objects/ab/cdef", 50);
            obj.modified_at = days_ago(1);
            v.push(obj);
        }
        v
    }

    #[test]
    fn staleness_is_the_later_of_git_activity_and_source_mtime() {
        // Sources old, but HEAD moved yesterday (a commit): active.
        let entries = git_project(200, Some(1));
        assert_eq!(category_of(&entries, "/p/target"), Some(Category::RegenerableArtifact));
        // Sources edited yesterday, last commit long ago (uncommitted work): active.
        let entries = git_project(1, Some(200));
        assert_eq!(category_of(&entries, "/p/target"), Some(Category::RegenerableArtifact));
        // Both old: dormant.
        let entries = git_project(200, Some(150));
        assert_eq!(category_of(&entries, "/p/target"), Some(Category::StaleProjectArtifact));
        let c = classify(&entries, now());
        let g = group(&c.summary, "cargo-target");
        assert!(g.why.contains("at least 90 days old"), "{}", g.why);
        assert_eq!(g.risk_tier, RiskTier::Safe, "stale + locked is still safe; staleness is ranking");
    }

    /// The plan's mutation #2: touching an artifact's mtime changes nothing.
    #[test]
    fn touching_an_artifact_does_not_change_staleness() {
        let mut entries = git_project(200, Some(150));
        let before = classify(&entries, now());
        let art = entries.iter_mut().find(|e| e.path == "/p/target/debug.bin").unwrap();
        art.modified_at = days_ago(0);
        let after = classify(&entries, now());
        assert_eq!(before.summary, after.summary, "artifact mtimes lie; the summary must not move");
        assert_eq!(group(&after.summary, "cargo-target").category, Category::StaleProjectArtifact);
    }

    #[test]
    fn unverifiable_project_is_never_stale() {
        // No .git, no dated source: unverifiable, stays regenerable.
        let entries = vec![dir("/p"), file("/p/Cargo.toml", 10), dir("/p/target"), file("/p/target/debug.bin", 5000)];
        let index = ScanIndex::build(&entries);
        let roots = hotspot_roots(&entries, &index);
        let root_paths: Vec<&str> = roots.iter().map(|r| r.path).collect();
        let activity = project_activity(&entries, &root_paths, now());
        assert_eq!(activity.get("/p"), Some(&Activity::Unverifiable));
        assert_eq!(dormant_project_roots(&entries, now()), Vec::<String>::new());
        assert_eq!(category_of(&entries, "/p/target"), Some(Category::RegenerableArtifact));
        // With a reflog alone (sources unreadable), the evidence is the git activity.
        let entries = git_project(0, Some(120)).into_iter().filter(|e| e.path != "/p/src/main.rs").collect::<Vec<_>>();
        let index = ScanIndex::build(&entries);
        let roots = hotspot_roots(&entries, &index);
        let root_paths: Vec<&str> = roots.iter().map(|r| r.path).collect();
        assert_eq!(project_activity(&entries, &root_paths, now()).get("/p"), Some(&Activity::LastSeen(120)));
    }

    #[test]
    fn threshold_option_moves_the_dormancy_boundary() {
        let entries = git_project(45, None);
        let shares = ShareLedger::from_entries(&entries);
        let default = classify(&entries, now());
        assert_eq!(group(&default.summary, "cargo-target").category, Category::RegenerableArtifact);
        let opts = ClassifyOptions { dormant_after_days: Some(30), verify: None };
        let tight = classify_with_options(&entries, &shares, now(), &opts);
        let g = group(&tight.summary, "cargo-target");
        assert_eq!(g.category, Category::StaleProjectArtifact);
        assert!(g.why.contains("at least 30 days old"), "{}", g.why);
        let opts = ClassifyOptions { dormant_after_days: Some(46), verify: None };
        let loose = classify_with_options(&entries, &shares, now(), &opts);
        assert_eq!(group(&loose.summary, "cargo-target").category, Category::RegenerableArtifact);
    }

    #[test]
    fn older_than_parses_units_and_rejects_nonsense() {
        assert_eq!(parse_older_than("90d").unwrap(), 90);
        assert_eq!(parse_older_than("90").unwrap(), 90);
        assert_eq!(parse_older_than("12w").unwrap(), 84);
        assert_eq!(parse_older_than("3M").unwrap(), 90);
        assert_eq!(parse_older_than(" 1y ").unwrap(), 365);
        for bad in ["", "0d", "-3d", "3m", "3 months", "abc", "3Md", "99999999999999999999d"] {
            let err = parse_older_than(bad).unwrap_err();
            assert!(matches!(err, CoreError::InvalidInput(_)), "{bad:?} → {err}");
        }
    }

    #[test]
    fn project_markers_include_git_and_every_detection_pattern() {
        let m = project_markers();
        assert!(m.contains(&".git") && m.contains(&"Cargo.toml") && m.contains(&"*.py") && m.contains(&"go.mod"));
    }

    // -- v1.1 Phase 2: opt-in lockfile verification (the seam, not the tool) -------

    #[test]
    fn injected_verifier_moves_the_lock_state_and_tier() {
        let entries = cargo_project(true);
        let shares = ShareLedger::from_entries(&entries);
        let seen = std::cell::RefCell::new(Vec::new());
        let verified = |rule: &HotspotRule, dir: &str| {
            seen.borrow_mut().push((rule.id, dir.to_string()));
            LockVerdict::Verified
        };
        let opts = ClassifyOptions { dormant_after_days: None, verify: Some(&verified) };
        let c = classify_with_options(&entries, &shares, now(), &opts);
        let g = group(&c.summary, "cargo-target");
        assert_eq!(g.risk_tier, RiskTier::Safe);
        assert!(g.why.contains("`cargo metadata --locked --offline --no-deps --format-version 1` confirmed it is current"), "{}", g.why);
        assert_eq!(*seen.borrow(), vec![("cargo-target", "/p".to_string())], "runs in the PROJECT dir, once");

        let failed = |_: &HotspotRule, _: &str| LockVerdict::Failed("exit 101".to_string());
        let opts = ClassifyOptions { dormant_after_days: None, verify: Some(&failed) };
        let c = classify_with_options(&entries, &shares, now(), &opts);
        let g = group(&c.summary, "cargo-target");
        assert_eq!(g.risk_tier, RiskTier::Caution, "a stale lockfile is not safe");
        assert!(g.why.contains("verification failed (`cargo metadata") && g.why.contains("exit 101"), "{}", g.why);

        let unavailable = |_: &HotspotRule, _: &str| LockVerdict::Unavailable;
        let opts = ClassifyOptions { dormant_after_days: None, verify: Some(&unavailable) };
        let c = classify_with_options(&entries, &shares, now(), &opts);
        let g = group(&c.summary, "cargo-target");
        assert_eq!(g.risk_tier, RiskTier::Safe, "tool absent: the lockfile still counts, unverified");
        assert!(g.why.ends_with("a lockfile pins the dependency versions."), "{}", g.why);

        // A pnpm lock does not trigger the npm verifier (it requires package-lock.json).
        let entries = vec![dir("/p"), file("/p/package.json", 1), file("/p/pnpm-lock.yaml", 1), dir("/p/node_modules"), file("/p/node_modules/a", 5)];
        let shares = ShareLedger::from_entries(&entries);
        let calls = std::cell::Cell::new(0);
        let count = |_: &HotspotRule, _: &str| { calls.set(calls.get() + 1); LockVerdict::Verified };
        let opts = ClassifyOptions { dormant_after_days: None, verify: Some(&count) };
        let c = classify_with_options(&entries, &shares, now(), &opts);
        assert_eq!(calls.get(), 0);
        assert_eq!(group(&c.summary, "node-modules").risk_tier, RiskTier::Safe);
        // Without any verifier, nothing is called and Present is the state.
        let c = classify(&entries, now());
        assert!(group(&c.summary, "node-modules").why.ends_with("pins the dependency versions."));
    }

    #[test]
    fn tier_strings_round_trip() {
        for t in [RiskTier::Safe, RiskTier::Caution, RiskTier::Review] {
            assert_eq!(serde_json::to_value(t).unwrap(), serde_json::Value::String(t.as_str().into()));
        }
        assert_eq!(serde_json::to_value(RebuildKind::None).unwrap(), "none");
        assert_eq!(serde_json::to_value(RebuildKind::Download).unwrap(), "download");
        assert_eq!(serde_json::to_value(RebuildKind::Compile).unwrap(), "compile");
        assert_eq!(Category::from_str("modelCache").unwrap(), Category::ModelCache);
    }
}
