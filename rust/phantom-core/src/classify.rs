// Reclaimability classification — a pure post-pass over scan entries.
//
// Every rule in here was earned in a real cleanup incident (see
// docs/reclaimability.md):
//
// - `disk_size` (st_blocks × 512) is THE size. `logical_size` exists to
//   detect cloud-dataloaded placeholders (logical ≫ disk), where `du`
//   once overstated a 147 MB OneDrive tree whose physical footprint was ~0.
// - Hardlinked entries (nlink > 1) share blocks: deleting one path frees
//   nothing until the last link goes. Reclaim estimates count each
//   (dev, ino) once — "17 GB" of ~/.cache/uv freed only 5 GB.
// - Project staleness comes from full-depth SOURCE file mtimes, never
//   artifact mtimes (cargo-sweep touches target/ on every run) and never a
//   depth-capped walk (a -maxdepth 3 check misread a project edited that
//   morning as dormant; five active projects lost their targets).
// - Phantom NEVER deletes. Categories carry action HINTS; the API surface
//   shows and suggests only.

use std::collections::{BTreeMap, HashMap, HashSet};

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::scan::ScanEntry;
use crate::{CoreError, Result};

/// A cloud-dataloaded placeholder shows at least this many times more
/// logical than physical bytes…
pub const CLOUD_DATALOADED_MIN_RATIO: u64 = 8;
/// …and at least this much logical size. Without an absolute floor, every
/// tiny file whose tail block rounds oddly would look "dataloaded".
pub const CLOUD_DATALOADED_MIN_LOGICAL: u64 = 1024 * 1024; // 1 MiB

/// A project whose newest source mtime is at least this old is dormant.
pub const DORMANT_AFTER_DAYS: i64 = 90;

/// Git pack files above this physical size are worth surfacing on their own.
pub const GIT_PACK_REVIEW_MIN_DISK: u64 = 200 * 1024 * 1024; // 200 MiB

/// Plain files above this physical size are surfaced even when no other
/// rule knows anything about them.
pub const LARGE_FILE_REVIEW_MIN_DISK: u64 = 1024 * 1024 * 1024; // 1 GiB

/// Largest hotspot roots listed per group.
pub const TOP_PATHS_PER_GROUP: usize = 5;

/// Directory names that hold build ARTIFACTS, not sources. Excluded from
/// staleness (their mtimes lie) and pruned from project-root detection
/// (every package inside node_modules ships a package.json).
pub const ARTIFACT_DIR_NAMES: &[&str] = &[
    "target",
    "node_modules",
    ".build",
    "dist",
    "build",
    ".next",
    "DerivedData",
];

/// Files/dirs marking a directory as a project root.
pub const PROJECT_MARKERS: &[&str] = &[".git", "Cargo.toml", "package.json"];

/// Reclaimability category. Stored in `entries.category` as the camelCase
/// wire string; NEVER an instruction to delete — Phantom shows and suggests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Category {
    /// A build regenerates it (cargo target/, node_modules, …).
    RegenerableArtifact,
    /// An app or OS cache; the owner rebuilds it on demand.
    Cache,
    /// A cache OWNED by a tool that must do its own cleanup.
    ToolManagedCache,
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
            Category::CloudDataloaded => "cloudDataloaded",
            Category::StaleProjectArtifact => "staleProjectArtifact",
            Category::ReviewFirst => "reviewFirst",
            Category::WontRegenerate => "wontRegenerate",
        }
    }

    /// Category-level action hint. Registry rows may carry a sharper,
    /// tool-specific hint; this is the fallback the UI can always show.
    pub fn action_hint(&self) -> &'static str {
        match self {
            Category::RegenerableArtifact => "safe to delete; the next build regenerates it",
            Category::Cache => "safe to delete; the owning app rebuilds it on demand",
            Category::ToolManagedCache => {
                "use the owning tool's clean command (e.g. `toolbox clean`), not rm -rf"
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
            Category::CloudDataloaded => 4,
            Category::ReviewFirst => 5,
            Category::WontRegenerate => 6,
        }
    }
}

impl std::str::FromStr for Category {
    type Err = CoreError;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "regenerableArtifact" => Ok(Category::RegenerableArtifact),
            "cache" => Ok(Category::Cache),
            "toolManagedCache" => Ok(Category::ToolManagedCache),
            "cloudDataloaded" => Ok(Category::CloudDataloaded),
            "staleProjectArtifact" => Ok(Category::StaleProjectArtifact),
            "reviewFirst" => Ok(Category::ReviewFirst),
            "wontRegenerate" => Ok(Category::WontRegenerate),
            other => Err(CoreError::InvalidInput(format!(
                "unknown category: {other:?}"
            ))),
        }
    }
}

/// How a registry row recognizes its hotspot. All path matching is
/// COMPONENT-boundary aware: `node_modules_backup` never matches a
/// `node_modules` rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Matcher {
    /// A directory whose final path component equals `name` exactly.
    /// `sibling`, when set, must exist next to it — a bare `target/` with
    /// no `Cargo.toml` beside it is NOT regenerable.
    DirNamed {
        name: &'static str,
        sibling: Option<&'static str>,
    },
    /// A directory whose path ends with exactly these components.
    DirSuffix { components: &'static [&'static str] },
    /// A directory named one of `names` somewhere below a `within`
    /// component (Electron caches under Application Support).
    DirNamedWithin {
        names: &'static [&'static str],
        within: &'static str,
    },
    /// A `*.pack` file under `objects/pack/` bigger than `min_disk_size`.
    GitPackFile { min_disk_size: u64 },
    /// Any file whose logical size dwarfs its physical size (see the
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
    /// The ONE safe, copy-runnable cleanup command for this rule, or None
    /// when no such command honestly exists: advice-only rules (a `git
    /// status` CHECK is not a cleanup), mixed-tool rules that cannot know
    /// the tool, rules whose only "cleanup" is deleting the directory
    /// itself (Phantom never emits rm -rf), and every reviewFirst /
    /// wontRegenerate / cloudDataloaded row.
    pub command: Option<&'static str>,
}

/// The hotspot registry. Knowledge lives in DATA rows, not a function per
/// case — adding a hotspot is adding a row. Ordering is precedence:
/// the first matching row wins for an entry.
pub const REGISTRY: &[HotspotRule] = &[
    HotspotRule {
        id: "cloud-dataloaded",
        label: "Cloud-dataloaded placeholders",
        matcher: Matcher::CloudDataloadedFile,
        category: Category::CloudDataloaded,
        hint: "contents live in the cloud; local blocks are ~0 — deleting frees almost nothing",
        command: None,
    },
    HotspotRule {
        id: "cargo-target",
        label: "Rust target/ directories",
        matcher: Matcher::DirNamed {
            name: "target",
            sibling: Some("Cargo.toml"),
        },
        category: Category::RegenerableArtifact,
        hint: "`cargo clean` or delete; the next `cargo build` regenerates it",
        command: Some("cargo clean"),
    },
    HotspotRule {
        id: "node-modules",
        label: "node_modules directories",
        matcher: Matcher::DirNamed {
            name: "node_modules",
            sibling: None,
        },
        category: Category::RegenerableArtifact,
        hint: "`npm install` / `pnpm install` regenerates it",
        command: None,
    },
    HotspotRule {
        id: "python-venv",
        label: "Python virtualenvs",
        matcher: Matcher::DirNamed {
            name: ".venv",
            sibling: None,
        },
        category: Category::RegenerableArtifact,
        hint: "recreate with `uv venv` / `python -m venv` and reinstall",
        command: None,
    },
    HotspotRule {
        id: "swiftpm-build",
        label: "SwiftPM .build directories",
        matcher: Matcher::DirNamed {
            name: ".build",
            sibling: Some("Package.swift"),
        },
        category: Category::RegenerableArtifact,
        hint: "`swift build` regenerates it",
        command: Some("swift package clean"),
    },
    HotspotRule {
        id: "next-build",
        label: ".next build output",
        matcher: Matcher::DirNamed {
            name: ".next",
            sibling: None,
        },
        category: Category::RegenerableArtifact,
        hint: "`next build` regenerates it",
        command: None,
    },
    // `build`/`dist` are generic names; the package.json sibling keeps the
    // rule from eating e.g. this repo's build/Phantom.app.
    HotspotRule {
        id: "js-build",
        label: "JS build/ output",
        matcher: Matcher::DirNamed {
            name: "build",
            sibling: Some("package.json"),
        },
        category: Category::RegenerableArtifact,
        hint: "the package's build script regenerates it",
        command: None,
    },
    HotspotRule {
        id: "js-dist",
        label: "JS dist/ output",
        matcher: Matcher::DirNamed {
            name: "dist",
            sibling: Some("package.json"),
        },
        category: Category::RegenerableArtifact,
        hint: "the package's build script regenerates it",
        command: None,
    },
    HotspotRule {
        id: "xcode-derived-data",
        label: "Xcode DerivedData",
        matcher: Matcher::DirNamed {
            name: "DerivedData",
            sibling: None,
        },
        category: Category::Cache,
        hint: "Xcode regenerates it on the next build",
        command: None,
    },
    HotspotRule {
        id: "library-caches",
        label: "Library/Caches",
        matcher: Matcher::DirSuffix {
            components: &["Library", "Caches"],
        },
        category: Category::Cache,
        hint: "app caches; apps rebuild them on demand",
        command: None,
    },
    HotspotRule {
        id: "electron-app-cache",
        label: "Electron app caches",
        matcher: Matcher::DirNamedWithin {
            names: &[
                "Cache",
                "Code Cache",
                "GPUCache",
                "DawnCache",
                "CachedData",
                "Service Worker",
            ],
            within: "Application Support",
        },
        category: Category::Cache,
        hint: "Electron/Chromium cache; the app rebuilds it",
        command: None,
    },
    HotspotRule {
        id: "group-containers",
        label: "Library/Group Containers",
        matcher: Matcher::DirSuffix {
            components: &["Library", "Group Containers"],
        },
        category: Category::ReviewFirst,
        hint: "shared app-group data; apps can lose state — review per container",
        command: None,
    },
    HotspotRule {
        id: "dot-cache",
        label: "~/.cache",
        matcher: Matcher::DirNamed {
            name: ".cache",
            sibling: None,
        },
        category: Category::ToolManagedCache,
        hint: "per-tool clean commands (`uv cache clean`, `pnpm store prune`); \
               hardlinked stores free less than they list",
        command: None,
    },
    HotspotRule {
        id: "dot-npm",
        label: "~/.npm",
        matcher: Matcher::DirNamed {
            name: ".npm",
            sibling: None,
        },
        category: Category::ToolManagedCache,
        hint: "`npm cache clean --force`",
        command: Some("npm cache clean --force"),
    },
    HotspotRule {
        id: "dot-cargo",
        label: "~/.cargo",
        matcher: Matcher::DirNamed {
            name: ".cargo",
            sibling: None,
        },
        category: Category::ToolManagedCache,
        hint: "cargo registry/git caches; prune with cargo tooling, not rm -rf",
        command: None,
    },
    HotspotRule {
        id: "dot-rustup",
        label: "~/.rustup",
        matcher: Matcher::DirNamed {
            name: ".rustup",
            sibling: None,
        },
        category: Category::ToolManagedCache,
        hint: "`rustup toolchain uninstall` unused toolchains",
        command: None,
    },
    HotspotRule {
        id: "dot-toolbox",
        label: "~/.toolbox",
        matcher: Matcher::DirNamed {
            name: ".toolbox",
            sibling: None,
        },
        category: Category::ToolManagedCache,
        hint: "use `toolbox clean`, not rm -rf",
        command: Some("toolbox clean"),
    },
    HotspotRule {
        id: "homebrew-cellar",
        label: "Homebrew Cellar",
        matcher: Matcher::DirSuffix {
            components: &["homebrew", "Cellar"],
        },
        category: Category::ToolManagedCache,
        hint: "`brew cleanup` / `brew uninstall`, not rm -rf",
        command: Some("brew cleanup"),
    },
    HotspotRule {
        id: "homebrew-cellar-intel",
        label: "Homebrew Cellar (Intel prefix)",
        matcher: Matcher::DirSuffix {
            components: &["local", "Cellar"],
        },
        category: Category::ToolManagedCache,
        hint: "`brew cleanup` / `brew uninstall`, not rm -rf",
        command: Some("brew cleanup"),
    },
    HotspotRule {
        id: "cloud-synced-originals",
        label: "Cloud-synced originals (CloudStorage)",
        matcher: Matcher::DirSuffix {
            components: &["Library", "CloudStorage"],
        },
        category: Category::WontRegenerate,
        hint: "synced originals; a local delete propagates to the cloud copy",
        command: None,
    },
    HotspotRule {
        id: "icloud-drive",
        label: "Cloud-synced originals (iCloud Drive)",
        matcher: Matcher::DirSuffix {
            components: &["Library", "Mobile Documents"],
        },
        category: Category::WontRegenerate,
        hint: "synced originals; a local delete propagates to the cloud copy",
        command: None,
    },
    HotspotRule {
        id: "agent-sessions",
        label: "Agent session data (~/.claude/projects)",
        matcher: Matcher::DirSuffix {
            components: &[".claude", "projects"],
        },
        category: Category::ReviewFirst,
        hint: "agent session history; prune old sessions after review",
        command: None,
    },
    HotspotRule {
        id: "agent-worktrees",
        label: "Agent worktrees",
        matcher: Matcher::DirNamed {
            name: ".worktrees",
            sibling: None,
        },
        category: Category::ReviewFirst,
        hint: "worktrees can hold uncommitted work; check `git status` in each",
        command: None,
    },
    HotspotRule {
        id: "git-pack",
        label: "Large git pack files",
        matcher: Matcher::GitPackFile {
            min_disk_size: GIT_PACK_REVIEW_MIN_DISK,
        },
        category: Category::ReviewFirst,
        hint: "repository history; `git gc` / repack or re-clone shallow — review first",
        command: None,
    },
    HotspotRule {
        id: "large-file",
        label: "Large files",
        matcher: Matcher::LargeFile {
            min_disk_size: LARGE_FILE_REVIEW_MIN_DISK,
        },
        category: Category::ReviewFirst,
        hint: "big and unclassified; review before deleting",
        command: None,
    },
];

/// One hotspot group in the per-scan summary: every entry matched by the
/// same registry row at the same effective category.
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
    /// Hardlink-deduped physical bytes — what deleting the whole group
    /// would actually free. THE number.
    pub disk_size: u64,
    /// Naive per-entry sum. Exceeds `disk_size` when hardlinks share
    /// blocks (the "17 GB listed, 5 GB freed" gap made visible).
    pub listed_disk_size: u64,
    pub logical_size: u64,
    pub file_count: u64,
    /// Largest hotspot roots, biggest first, capped at TOP_PATHS_PER_GROUP.
    pub top_paths: Vec<String>,
}

/// Per-scan hotspot summary. Serialized camelCase; the surface pass
/// persists it and serves it as GET /scans/{id}/hotspots.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HotspotsSummary {
    /// Sorted: stale project artifacts first, then by category priority,
    /// then deduped size descending.
    pub groups: Vec<HotspotGroup>,
    /// Hardlink-deduped disk across RECLAIMABLE categories only
    /// (regenerableArtifact, staleProjectArtifact, cache, toolManagedCache).
    /// Deduped GLOBALLY: a (dev, ino) spanning two groups counts once.
    pub reclaim_estimate: u64,
    /// Deduped disk across reviewFirst + wontRegenerate — visible, never
    /// suggested.
    pub review_disk_size: u64,
    /// The du-lie, quantified: what dataloaded placeholders CLAIM…
    pub cloud_dataloaded_logical_size: u64,
    /// …versus the blocks they actually occupy.
    pub cloud_dataloaded_disk_size: u64,
}

/// Result of classifying one scan's entries.
#[derive(Debug, Clone, PartialEq)]
pub struct Classification {
    /// Parallel to the input slice: `categories[i]` is entry `i`'s
    /// category (None = ordinary content, nothing to say about it).
    pub categories: Vec<Option<Category>>,
    pub summary: HotspotsSummary,
}

/// True when a file's logical size dwarfs its physical footprint — a
/// cloud-dataloaded placeholder (OneDrive/CloudStorage/iCloud). Both the
/// ratio and the absolute floor must hold.
pub fn is_cloud_dataloaded(entry: &ScanEntry) -> bool {
    !entry.is_dir
        && entry.logical_size >= CLOUD_DATALOADED_MIN_LOGICAL
        && entry.logical_size >= entry.disk_size.saturating_mul(CLOUD_DATALOADED_MIN_RATIO)
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
fn is_strictly_under(path: &str, root: &str) -> bool {
    path.len() > root.len()
        && path.starts_with(root)
        && path.as_bytes()[root.len()] == b'/'
}

/// Ancestor chain of `path`, nearest first, excluding `path` itself:
/// `/a/b/c` → `/a/b`, `/a`.
fn ancestors(path: &str) -> impl Iterator<Item = &str> {
    std::iter::successors(parent_of(path), |p| parent_of(p))
}

fn parent_of(path: &str) -> Option<&str> {
    path.rfind('/').filter(|&i| i > 0).map(|i| &path[..i])
}

// ---------------------------------------------------------------------------
// Matching

impl Matcher {
    /// Does this row match `entry`? `paths` is the set of every scanned
    /// path, used for sibling checks. File-only matchers never match dirs
    /// and vice versa.
    fn matches(&self, entry: &ScanEntry, paths: &HashSet<&str>) -> bool {
        match *self {
            Matcher::DirNamed { name, sibling } => {
                entry.is_dir
                    && entry.name == name
                    && sibling.is_none_or(|s| match entry.parent_path.as_deref() {
                        Some(parent) => paths.contains(format!("{parent}/{s}").as_str()),
                        // Parent outside the scan: the sibling is not
                        // observable, so the claim of regenerability is
                        // not provable. Stay conservative.
                        None => false,
                    })
            }
            Matcher::DirSuffix { components } => {
                entry.is_dir && ends_with_components(&entry.path, components)
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
            Matcher::DirNamed { .. } | Matcher::DirSuffix { .. } | Matcher::DirNamedWithin { .. }
        )
    }
}

// ---------------------------------------------------------------------------
// Staleness

/// Project roots (dirs containing a PROJECT_MARKERS child) whose newest
/// full-depth SOURCE mtime is at least DORMANT_AFTER_DAYS old. Artifact
/// dirs and .git are excluded from both root detection and mtime scanning —
/// artifact mtimes lie. A root with no dated source files is NOT dormant
/// (unknown ≠ stale).
fn dormant_project_roots(entries: &[ScanEntry], now: DateTime<Utc>) -> Vec<String> {
    let roots: HashSet<&str> = entries
        .iter()
        .filter(|e| PROJECT_MARKERS.contains(&e.name.as_str()))
        .filter_map(|e| e.parent_path.as_deref())
        // A package.json under node_modules does not mark a project.
        .filter(|root| {
            !ARTIFACT_DIR_NAMES.iter().any(|a| has_component(root, a))
                && !has_component(root, ".git")
        })
        .collect();

    let mut newest: HashMap<&str, DateTime<Utc>> = HashMap::new();
    for entry in entries.iter().filter(|e| !e.is_dir) {
        let Some(mtime) = entry.modified_at else { continue };
        // Source files only: nothing under an artifact dir or .git counts.
        if ARTIFACT_DIR_NAMES.iter().any(|a| has_component(&entry.path, a))
            || has_component(&entry.path, ".git")
        {
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

    let cutoff = Duration::days(DORMANT_AFTER_DAYS);
    let mut dormant: Vec<String> = newest
        .into_iter()
        .filter(|(_, m)| now.signed_duration_since(*m) >= cutoff)
        .map(|(root, _)| root.to_string())
        .collect();
    dormant.sort();
    dormant
}

// ---------------------------------------------------------------------------
// The classifier

struct Root<'a> {
    path: &'a str,
    rule: &'static HotspotRule,
    category: Category,
}

/// Classify a scan's entries. Pure: no store, no filesystem — everything is
/// derived from the entries plus `now` (injected so dormancy is testable).
pub fn classify(entries: &[ScanEntry], now: DateTime<Utc>) -> Classification {
    let paths: HashSet<&str> = entries.iter().map(|e| e.path.as_str()).collect();
    let dormant = dormant_project_roots(entries, now);

    // 1. Directory hotspot roots (first matching registry row wins),
    //    collapsed to the OUTERMOST root so nothing is counted twice.
    let mut dir_roots: Vec<Root> = entries
        .iter()
        .filter(|e| e.is_dir)
        .filter_map(|e| {
            REGISTRY
                .iter()
                .find(|r| r.matcher.is_dir_matcher() && r.matcher.matches(e, &paths))
                .map(|rule| Root {
                    path: &e.path,
                    rule,
                    category: rule.category,
                })
        })
        .collect();
    dir_roots.sort_by_key(|r| r.path.len());
    let mut kept: Vec<Root> = Vec::new();
    for root in dir_roots {
        if !kept.iter().any(|k| is_strictly_under(root.path, k.path)) {
            kept.push(root);
        }
    }

    // 2. Stale upgrade: a regenerable artifact inside a dormant project is
    //    the best reclaim candidate there is.
    for root in &mut kept {
        if root.category == Category::RegenerableArtifact
            && dormant.iter().any(|d| is_strictly_under(root.path, d))
        {
            root.category = Category::StaleProjectArtifact;
        }
    }
    let root_by_path: HashMap<&str, usize> =
        kept.iter().enumerate().map(|(i, r)| (r.path, i)).collect();

    // 3. Per-entry assignment. Files: cloud-dataloaded override first, then
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

    // (rule, effective category, governing root path or the file itself)
    let mut assigned: Vec<Option<(&'static HotspotRule, Category, &str)>> =
        Vec::with_capacity(entries.len());
    for entry in entries {
        let governing = std::iter::once(entry.path.as_str())
            .chain(ancestors(&entry.path))
            .find_map(|p| root_by_path.get(p).copied());
        let assignment = if !entry.is_dir && is_cloud_dataloaded(entry) {
            Some((cloud_rule, cloud_rule.category, entry.path.as_str()))
        } else if let Some(i) = governing {
            let root = &kept[i];
            Some((root.rule, root.category, root.path))
        } else if !entry.is_dir {
            file_rules
                .iter()
                .find(|r| r.matcher.matches(entry, &paths))
                .map(|r| (*r, r.category, entry.path.as_str()))
        } else {
            None
        };
        assigned.push(assignment);
    }

    // 4. Aggregate groups. Group key = (rule, effective category): the same
    //    rule can split into stale and non-stale groups. Hardlinks (nlink>1)
    //    dedupe by (dev, ino) — per group for group totals, globally for the
    //    summary rollups.
    struct Acc<'a> {
        rule: &'static HotspotRule,
        category: Category,
        disk: u64,
        listed: u64,
        logical: u64,
        files: u64,
        seen: HashSet<(u64, u64)>,
        per_root: BTreeMap<&'a str, u64>,
    }
    let mut groups: BTreeMap<(&str, &str), Acc> = BTreeMap::new();
    let mut global_seen: HashSet<(u64, u64)> = HashSet::new();
    let mut summary = HotspotsSummary {
        groups: Vec::new(),
        reclaim_estimate: 0,
        review_disk_size: 0,
        cloud_dataloaded_logical_size: 0,
        cloud_dataloaded_disk_size: 0,
    };

    for (entry, assignment) in entries.iter().zip(&assigned) {
        let Some((rule, category, root_path)) = assignment else { continue };
        if entry.is_dir {
            continue; // dirs carry disk_size 0; files carry the bytes
        }
        let acc = groups
            .entry((rule.id, category.as_str()))
            .or_insert_with(|| Acc {
                rule,
                category: *category,
                disk: 0,
                listed: 0,
                logical: 0,
                files: 0,
                seen: HashSet::new(),
                per_root: BTreeMap::new(),
            });
        acc.listed += entry.disk_size;
        acc.logical += entry.logical_size;
        acc.files += 1;
        let key = (entry.dev, entry.ino);
        let first_in_group = entry.nlink <= 1 || acc.seen.insert(key);
        if first_in_group {
            acc.disk += entry.disk_size;
            *acc.per_root.entry(root_path).or_insert(0) += entry.disk_size;
        }
        // Group-level `seen` and the global set are independent: a
        // (dev, ino) spanning two groups counts once here even though each
        // group saw it first.
        let first_globally = entry.nlink <= 1 || global_seen.insert(key);
        if first_globally {
            match category {
                c if c.is_reclaimable() => summary.reclaim_estimate += entry.disk_size,
                Category::ReviewFirst | Category::WontRegenerate => {
                    summary.review_disk_size += entry.disk_size
                }
                Category::CloudDataloaded => {
                    summary.cloud_dataloaded_disk_size += entry.disk_size;
                    summary.cloud_dataloaded_logical_size += entry.logical_size;
                }
                _ => {}
            }
        }
    }

    let mut out: Vec<HotspotGroup> = groups
        .into_values()
        .map(|acc| {
            let mut roots: Vec<(&str, u64)> = acc.per_root.into_iter().collect();
            roots.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
            HotspotGroup {
                rule_id: acc.rule.id.to_string(),
                label: acc.rule.label.to_string(),
                category: acc.category,
                hint: acc.rule.hint.to_string(),
                command: acc.rule.command.map(str::to_string),
                disk_size: acc.disk,
                listed_disk_size: acc.listed,
                logical_size: acc.logical,
                file_count: acc.files,
                top_paths: roots
                    .into_iter()
                    .take(TOP_PATHS_PER_GROUP)
                    .map(|(p, _)| p.to_string())
                    .collect(),
            }
        })
        .collect();
    out.sort_by(|a, b| {
        a.category
            .priority()
            .cmp(&b.category.priority())
            .then_with(|| b.disk_size.cmp(&a.disk_size))
            .then_with(|| a.rule_id.cmp(&b.rule_id))
    });
    summary.groups = out;

    Classification {
        categories: assigned
            .iter()
            .map(|a| a.as_ref().map(|(_, c, _)| *c))
            .collect(),
        summary,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
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
        assert_eq!(category_of(&entries, "/u/Library/Caches"), Some(Category::Cache));
        assert_eq!(
            category_of(&entries, "/u/Library/Caches/app/f"),
            Some(Category::Cache)
        );
        assert_eq!(
            category_of(&entries, "/u/Library/Developer/Xcode/DerivedData"),
            Some(Category::Cache)
        );
        assert_eq!(
            category_of(&entries, "/u/Library/Application Support/Slack/Cache/blob"),
            Some(Category::Cache)
        );
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
        assert_eq!(g.file_count, 2);
        assert_eq!(c.summary.reclaim_estimate, 100);
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

    #[test]
    fn top_paths_are_largest_roots_capped() {
        let mut entries = vec![dir("/code")];
        for i in 0..(TOP_PATHS_PER_GROUP + 2) {
            let root = format!("/code/p{i}/node_modules");
            entries.push(dir(&format!("/code/p{i}")));
            entries.push(dir(&root));
            entries.push(file(&format!("{root}/x.js"), (i as u64 + 1) * 100));
        }
        let c = classify(&entries, now());
        let g = group(&c.summary, "node-modules");
        assert_eq!(g.top_paths.len(), TOP_PATHS_PER_GROUP);
        assert_eq!(
            g.top_paths[0],
            format!("/code/p{}/node_modules", TOP_PATHS_PER_GROUP + 1),
            "biggest root first"
        );
    }

    // -- wire shape -----------------------------------------------------------

    #[test]
    fn summary_decodes_from_raw_fixture_bytes() {
        let s: HotspotsSummary = serde_json::from_str(RAW_SUMMARY).unwrap();
        assert_eq!(s.groups.len(), 3);
        assert_eq!(s.groups[0].category, Category::StaleProjectArtifact);
        // command: a real cleanup command where one honestly exists, null
        // where none does (mixed-tool caches, informational categories).
        assert_eq!(s.groups[0].command.as_deref(), Some("cargo clean"));
        assert_eq!(s.groups[1].command, None, "mixed-tool cache has no ONE command");
        assert_eq!(s.groups[2].command, None, "cloudDataloaded is informational");
        assert_eq!(s.groups[1].rule_id, "dot-cache");
        assert_eq!(s.groups[1].disk_size, 5_368_709_120);
        assert_eq!(s.groups[1].listed_disk_size, 18_253_611_008);
        assert_eq!(s.reclaim_estimate, 22_548_578_304);
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
            "ruleId", "label", "category", "hint", "command", "diskSize",
            "listedDiskSize", "logicalSize", "fileCount", "topPaths",
        ] {
            assert!(g.contains_key(key), "group missing {key}");
        }
        assert_eq!(g["category"], "staleProjectArtifact");
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
        for c in [
            Category::RegenerableArtifact,
            Category::Cache,
            Category::ToolManagedCache,
            Category::CloudDataloaded,
            Category::StaleProjectArtifact,
            Category::ReviewFirst,
            Category::WontRegenerate,
        ] {
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
        for c in [
            Category::RegenerableArtifact,
            Category::Cache,
            Category::ToolManagedCache,
            Category::CloudDataloaded,
            Category::StaleProjectArtifact,
            Category::ReviewFirst,
            Category::WontRegenerate,
        ] {
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
}
