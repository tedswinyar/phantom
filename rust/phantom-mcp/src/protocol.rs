// MCP protocol surface: version negotiation, the tool table (with
// annotations, outputSchema and the result-size hint), and the wrapping
// that turns an HTTP body into `structuredContent`.
//
// The tool table is the agent's whole picture of Phantom, so it is pinned
// three ways: the e2e capability gate compares the sorted name list, the
// unit tests here compare the ORDER (a host renders tools/list in order,
// so a reshuffle is a UX change) and check every outputSchema against the
// shared wire fixtures in tests/fixtures/ — the same raw bytes the Rust,
// Swift and conformance suites parse — so a schema cannot drift from the
// wire without a test naming the key.

use serde_json::{Value, json};

/// The newest protocol revision this server speaks. Annotations,
/// `outputSchema` and `structuredContent` need 2025-06-18; the response
/// shape is a superset of 2024-11-05, so older hosts keep working.
pub const LATEST_PROTOCOL_VERSION: &str = "2025-06-18";

/// Every revision we answer `initialize` with. Per the spec a server echoes
/// the client's requested version when it supports it and otherwise
/// answers with one it does (the latest).
pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

/// The result-size budget, in characters of the pretty-printed JSON text.
/// Claude Code caps a tool result at ~25k tokens (≈100k chars) before it
/// spills to a file the agent then has to grep; a result that stays under
/// this is one the agent can actually read. Advertised per tool through
/// `_meta["anthropic/maxResultSizeChars"]` and ENFORCED in shape.rs for the
/// two tools whose payload grows with the tree (treemap, large files).
pub const MAX_RESULT_CHARS: usize = 100_000;

/// The `_meta` key hosts read for the result budget.
pub const MAX_RESULT_SIZE_META_KEY: &str = "anthropic/maxResultSizeChars";

/// Which protocol revision to answer `initialize` with.
pub fn negotiate_protocol_version(requested: Option<&str>) -> &'static str {
    match requested {
        Some(v) => SUPPORTED_PROTOCOL_VERSIONS
            .iter()
            .copied()
            .find(|s| *s == v)
            .unwrap_or(LATEST_PROTOCOL_VERSION),
        None => LATEST_PROTOCOL_VERSION,
    }
}

/// Tool names in tools/list order. The e2e gate pins the SET; the unit
/// test pins this ORDER — the scan tool first (it is how everything
/// starts), then the result tools in the order an agent tends to need
/// them, health last.
#[cfg_attr(not(test), allow(dead_code))] // pinned by the tests below
pub const TOOL_NAMES: [&str; 16] = [
    "get_volume_status",
    "scan_directory",
    "scan_status",
    "cancel_scan",
    "list_scans",
    "find_large_files",
    "get_space_by_type",
    "get_treemap",
    "get_hotspots",
    "explain_path",
    "find_stale_projects",
    "plan_reclaim",
    "verify_reclaim",
    "diff_scans",
    "get_growth",
    "health",
];

/// Tools that grow with the tree and therefore carry the result budget
/// hint and the enforcement in shape.rs.
#[cfg_attr(not(test), allow(dead_code))] // pinned by the tests below
pub const BUDGETED_TOOLS: [&str; 2] = ["get_treemap", "find_large_files"];

/// Tools whose result changes shape under `responseFormat: "concise"`.
#[cfg_attr(not(test), allow(dead_code))] // pinned by the tests below
pub const CONCISE_TOOLS: [&str; 6] = [
    "scan_directory",
    "scan_status",
    "list_scans",
    "find_large_files",
    "get_treemap",
    "get_hotspots",
];

/// Tools that write to Phantom's own store, and how. scan_directory records
/// a NEW scan per call (and, opt-in, spawns read-only probes): not read-only,
/// not idempotent. cancel_scan flips a flag on a running scan: not read-only,
/// but cancelling twice is the same as once. Nothing here destroys anything
/// (partial results of a cancelled scan were never promised) and nothing
/// leaves the local API. The spec's defaults are destructive=true /
/// openWorld=true, so an unannotated tool looks worse than any of these are.
#[cfg_attr(not(test), allow(dead_code))] // pinned by the tests below
pub const WRITING_TOOLS: [&str; 4] = ["scan_directory", "cancel_scan", "plan_reclaim", "verify_reclaim"];

/// Writers whose every call makes something NEW (a scan, a plan, a rescan):
/// not idempotent. cancel_scan is the one writer that is.
#[cfg_attr(not(test), allow(dead_code))] // pinned by the tests below
pub const NON_IDEMPOTENT_TOOLS: [&str; 3] = ["scan_directory", "plan_reclaim", "verify_reclaim"];

fn annotations(title: &str, read_only: bool, idempotent: bool) -> Value {
    json!({
        "title": title,
        "readOnlyHint": read_only,
        "destructiveHint": false,
        "idempotentHint": idempotent,
        "openWorldHint": false
    })
}

/// The common case: reads, and reading twice is reading once.
fn read_only(title: &str) -> Value {
    annotations(title, true, true)
}

// --- Output schemas -----------------------------------------------------------
//
// Each describes the tool's `structuredContent`. Where a tool supports
// `responseFormat: "concise"`, the schema lists every DETAILED property and
// marks as `required` only the ones both formats carry; the description
// says so. Sizes are disk bytes (st_blocks × 512), deduped, unless named
// otherwise.

fn bytes(desc: &str) -> Value {
    json!({ "type": "integer", "minimum": 0, "description": desc })
}

fn nullable(t: &str, desc: &str) -> Value {
    json!({ "type": [t, "null"], "description": desc })
}

fn datetime(desc: &str) -> Value {
    json!({ "type": "string", "format": "date-time", "description": desc })
}

fn nullable_datetime(desc: &str) -> Value {
    json!({ "type": ["string", "null"], "format": "date-time", "description": desc })
}

pub fn scan_schema() -> Value {
    json!({
        "type": "object",
        "description": "A scan view. Concise carries id, rootPath, status, \
            startedAt, finishedAt, totalDiskSize, totalPrivateSize, fileCount, \
            errorCount (and progress while running); detailed adds the rest.",
        "properties": {
            "id": { "type": "string", "format": "uuid" },
            "rootPath": { "type": "string" },
            "status": { "type": "string", "enum": ["running", "complete", "cancelled", "failed"] },
            "startedAt": datetime("When the walk began"),
            "finishedAt": nullable_datetime("When the scan reached a terminal status; null while running"),
            "totalDiskSize": bytes("Allocated disk bytes, hardlink- and clone-deduped (the du model)"),
            "totalLogicalSize": bytes("Apparent (logical) bytes — never the headline number"),
            "totalPrivateSize": bytes("What deleting the whole tree would actually free"),
            "totalSharedSize": bytes("Bytes pinned by clones, hard links or snapshots outside the tree"),
            "fileCount": { "type": "integer", "minimum": 0 },
            "dirCount": { "type": "integer", "minimum": 0 },
            "errorCount": { "type": "integer", "minimum": 0, "description": "Entries that could not be read; the truth behind unreadablePaths" },
            "unreadablePaths": {
                "type": "array",
                "description": "A capped SAMPLE (first 100) of the unreadable entries",
                "items": {
                    "type": "object",
                    "properties": { "path": { "type": "string" }, "reason": { "type": "string" } },
                    "required": ["path", "reason"]
                }
            },
            "progress": {
                "type": ["object", "null"],
                "description": "Live counters while running; null once terminal",
                "properties": {
                    "filesSeen": { "type": "integer", "minimum": 0 },
                    "bytesSeen": bytes("Disk bytes seen so far"),
                    "currentPath": { "type": "string" }
                },
                "required": ["filesSeen", "bytesSeen", "currentPath"]
            },
            "failureReason": nullable("string", "Why a failed scan failed: the walker's error, or \"interrupted: …\" for a scan the server was running when it stopped. Null otherwise"),
            "note": { "type": "string", "description": "Only on a waited scan_directory that gave up waiting: how to keep following the scan" }
        },
        "required": ["id", "rootPath", "status", "startedAt", "finishedAt", "totalDiskSize", "totalPrivateSize", "fileCount", "errorCount"]
    })
}

pub fn entry_schema() -> Value {
    json!({
        "type": "object",
        "description": "A file row. Concise carries path, diskSize, privateSize, fileType; detailed adds the rest.",
        "properties": {
            "path": { "type": "string" },
            "parentPath": nullable("string", "null for the scan root"),
            "name": { "type": "string" },
            "isDir": { "type": "boolean" },
            "diskSize": bytes("Allocated disk bytes"),
            "logicalSize": bytes("Apparent bytes"),
            "privateSize": bytes("What deleting this entry frees: 0 for a hard link, an APFS pure clone, or a snapshot-held file"),
            "sharedSize": bytes("diskSize − privateSize"),
            "modifiedAt": nullable_datetime("mtime; null when the entry could not be stat'd (a scan root that vanished)"),
            "fileType": nullable("string", "Lowercased extension; null when there is none"),
            "category": nullable("string", "Reclaimability category when a hotspot rule matched; else null"),
            "nlink": { "type": "integer", "minimum": 1 },
            "dev": { "type": "integer" },
            "ino": { "type": "integer" },
            "cloneId": nullable("integer", "APFS clone-group id when the file shares blocks; null otherwise"),
            "fileCount": nullable("integer", "Directories only"),
            "dirCount": nullable("integer", "Directories only"),
            "flags": {
                "type": "array",
                "items": { "type": "string", "enum": ["dataless", "purgeable", "sparse", "compressed", "mayShareBlocks", "sharesAllBlocks"] },
                "description": "Filesystem facts about the entry"
            }
        },
        "required": ["path", "diskSize", "privateSize", "fileType"]
    })
}

pub fn type_total_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "fileType": nullable("string", "Lowercased extension; null = no extension"),
            "diskSize": bytes("Disk bytes of every file of this type, from the FULL walk"),
            "fileCount": { "type": "integer", "minimum": 0 }
        },
        "required": ["fileType", "diskSize", "fileCount"]
    })
}

pub fn treemap_schema() -> Value {
    json!({
        "type": "object",
        "description": "Squarified layout. Concise rects carry path, size, depth, isDir \
            (no geometry); detailed adds name, x, y, width, height, fileType, residual. \
            `truncated` appears only when the layout exceeded the result budget \
            and was served to a shallower depth.",
        "properties": {
            "rootPath": { "type": "string" },
            "totalSize": bytes("Disk bytes under rootPath"),
            "rects": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "For a residual pseudo-tile, the PARENT directory's path" },
                        "name": { "type": "string" },
                        "size": bytes("Disk bytes (aggregated for directories)"),
                        "x": { "type": "number" },
                        "y": { "type": "number" },
                        "width": { "type": "number" },
                        "height": { "type": "number" },
                        "depth": { "type": "integer", "minimum": 0, "description": "0 = the root rect" },
                        "isDir": { "type": "boolean" },
                        "fileType": nullable("string", "Lowercased extension for files; null otherwise"),
                        "residual": { "type": "boolean", "description": "True for the synthesized 'smaller files' remainder of a directory" }
                    },
                    "required": ["path", "size", "depth", "isDir"]
                }
            },
            "truncated": {
                "type": "object",
                "properties": {
                    "requestedDepth": { "type": "integer", "minimum": 0 },
                    "servedDepth": { "type": "integer", "minimum": 0 },
                    "note": { "type": "string", "description": "Names the next call that gets the rest" }
                },
                "required": ["requestedDepth", "servedDepth", "note"]
            }
        },
        "required": ["rootPath", "totalSize", "rects"]
    })
}

pub fn hotspots_schema() -> Value {
    json!({
        "type": "object",
        "description": "The reclaimability summary. Concise groups carry ruleId, label, \
            category, riskTier, why, command, diskSize, privateSize, topPaths; detailed \
            adds hint, rebuildCost, toolEstimate, listedDiskSize, logicalSize, fileCount.",
        "properties": {
            "groups": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "ruleId": { "type": "string" },
                        "label": { "type": "string" },
                        "category": { "type": "string", "description": "Reclaimability category (e.g. staleProjectArtifact, toolManagedCache, cloudDataloaded, reviewFirst)" },
                        "hint": { "type": "string", "description": "Human hint naming the safe tool; never an operation Phantom performs" },
                        "command": nullable("string", "The tool's own clean command when one exists (e.g. `cargo clean`)"),
                        "riskTier": { "type": "string", "enum": ["safe", "caution", "review"], "description": "safe: act after confirmation; caution: read `why` first; review: never a suggestion" },
                        "why": { "type": "string", "description": "One sentence: why this tier" },
                        "rebuildCost": {
                            "type": "object",
                            "properties": {
                                "kind": { "type": "string", "enum": ["download", "compile", "none"] },
                                "estimate": { "type": "string" }
                            },
                            "required": ["kind", "estimate"]
                        },
                        "toolEstimate": {
                            "type": ["object", "null"],
                            "description": "The owning tool's own dry-run number; null unless the scan asked for toolEstimates",
                            "properties": {
                                "tool": { "type": "string" },
                                "command": { "type": "string" },
                                "reclaimableBytes": bytes("What the tool itself says it would free"),
                                "note": { "type": "string" }
                            },
                            "required": ["tool", "command", "reclaimableBytes", "note"]
                        },
                        "diskSize": bytes("Deduped disk bytes across the group's paths"),
                        "listedDiskSize": bytes("Naive sum of the paths' sizes (hardlinked stores list more than they occupy)"),
                        "privateSize": bytes("What deleting the group's paths would actually free — quote THIS"),
                        "logicalSize": bytes("Apparent bytes"),
                        "fileCount": { "type": "integer", "minimum": 0 },
                        "topPaths": { "type": "array", "items": { "type": "string" } }
                    },
                    "required": ["ruleId", "label", "category", "riskTier", "why", "command", "diskSize", "privateSize", "topPaths"]
                }
            },
            "reclaimEstimate": bytes("Sum of privateSize over the reclaimable categories; excludes cloud placeholders"),
            "reviewDiskSize": bytes("Disk bytes in review-only groups"),
            "cloudDataloadedLogicalSize": bytes("Apparent bytes of cloud placeholders"),
            "cloudDataloadedDiskSize": bytes("Local blocks of cloud placeholders (≈0)"),
            "projects": {
                "type": "array",
                "description": "Every project root the staleness rule evaluated (biggest artifact bytes first); find_stale_projects re-thresholds this",
                "items": {
                    "type": "object",
                    "properties": {
                        "root": { "type": "string" },
                        "lastActivityDays": nullable("integer", "Days since newest git activity or source edit at scan time; null == unverifiable, never stale"),
                        "dormant": { "type": "boolean", "description": "At the scan's own threshold" },
                        "artifacts": { "type": "array", "items": artifact_schema() }
                    },
                    "required": ["root", "lastActivityDays", "dormant", "artifacts"]
                }
            }
        },
        "required": ["groups", "reclaimEstimate", "reviewDiskSize", "cloudDataloadedLogicalSize", "cloudDataloadedDiskSize", "projects"]
    })
}

pub fn diff_schema() -> Value {
    let movement = json!({
        "type": "object",
        "properties": {
            "path": { "type": "string" },
            "before": nullable("integer", "Disk bytes in scanA; null when the directory did not exist"),
            "after": nullable("integer", "Disk bytes in scanB; null when it was deleted"),
            "delta": { "type": "integer", "description": "after − before, signed" }
        },
        "required": ["path", "before", "after", "delta"]
    });
    json!({
        "type": "object",
        "properties": {
            "scanA": { "type": "string", "format": "uuid", "description": "The older scan" },
            "scanB": { "type": "string", "format": "uuid", "description": "The newer scan" },
            "scanAStartedAt": datetime("When scanA began"),
            "scanBStartedAt": datetime("When scanB began"),
            "reversedChronology": { "type": ["boolean", "null"], "description": "true when scanA is actually the newer scan — every sign is inverted" },
            "rootPath": { "type": "string" },
            "diskDelta": { "type": "integer", "description": "B − A disk bytes, signed" },
            "logicalDelta": { "type": "integer" },
            "fileCountDelta": { "type": "integer" },
            "dirCountDelta": { "type": "integer" },
            "errorCountDelta": { "type": "integer" },
            "grown": { "type": "array", "items": movement.clone() },
            "freed": { "type": "array", "items": movement }
        },
        "required": ["scanA", "scanB", "scanAStartedAt", "scanBStartedAt", "reversedChronology", "rootPath", "diskDelta", "logicalDelta", "fileCountDelta", "dirCountDelta", "errorCountDelta", "grown", "freed"]
    })
}

fn tier_enum(desc: &str) -> Value {
    json!({ "type": "string", "enum": ["safe", "caution", "review"], "description": desc })
}

pub fn plan_schema() -> Value {
    json!({
        "type": "object",
        "description": "A reclaim plan: the dry-run to confirm before anything moves. Every             item is a hotspot group the classifier rated; review groups are never items.",
        "properties": {
            "planId": { "type": "string", "format": "uuid" },
            "scanId": { "type": "string", "format": "uuid", "description": "The completed scan the plan was built from — verify_reclaim's before side" },
            "rootPath": { "type": "string" },
            "createdAt": datetime("When the plan was built; a rescan must start after this to verify it"),
            "maxTier": tier_enum("Highest tier admitted (safe or caution; review is never admitted)"),
            "minBytes": bytes("Groups freeing less than this were skipped"),
            "items": {
                "type": "array",
                "description": "In priority order: stale project artifacts first, then by category, safe before caution, biggest first",
                "items": {
                    "type": "object",
                    "properties": {
                        "ruleId": { "type": "string" },
                        "label": { "type": "string" },
                        "category": { "type": "string" },
                        "riskTier": tier_enum("safe: act after confirmation; caution: read why first"),
                        "why": { "type": "string" },
                        "command": nullable("string", "The owning tool's own clean command (run it in each project dir) when one exists — the alternative to moving the paths"),
                        "paths": { "type": "array", "items": { "type": "string" }, "description": "What to move, biggest first" },
                        "expectedFreedBytes": bytes("The group's privateSize: what removing these paths ACTUALLY frees — the number to promise"),
                        "diskSize": bytes("The group's deduped disk bytes, for the lists-as comparison")
                    },
                    "required": ["ruleId", "label", "category", "riskTier", "why", "command", "paths", "expectedFreedBytes", "diskSize"]
                }
            },
            "itemCount": { "type": "integer", "minimum": 0 },
            "expectedFreedBytes": bytes("Σ expectedFreedBytes over the items"),
            "skipped": {
                "type": "object",
                "description": "Why groups were left out — an empty plan is explicable",
                "properties": {
                    "review": { "type": "integer", "minimum": 0, "description": "review tier or never-reclaimable category" },
                    "aboveTier": { "type": "integer", "minimum": 0 },
                    "belowMinBytes": { "type": "integer", "minimum": 0 },
                    "tracked": { "type": "integer", "minimum": 0, "description": "every path sits inside a git work tree and is not ignored by its rules — git data, never moved; an item whose why says 'held back' lost some paths the same way" }
                },
                "required": ["review", "aboveTier", "belowMinBytes", "tracked"]
            },
            "script": { "type": "string", "description": "Only when includeScript was true: the plan as a POSIX sh script — dry-run by default, PHANTOM_APPLY=1 moves paths to the Trash and logs" }
        },
        "required": ["planId", "scanId", "rootPath", "createdAt", "maxTier", "minBytes", "items", "itemCount", "expectedFreedBytes", "skipped"]
    })
}

pub fn verification_schema() -> Value {
    json!({
        "type": "object",
        "description": "What a rescan shows against a plan's promise.",
        "properties": {
            "planId": { "type": "string", "format": "uuid" },
            "beforeScanId": { "type": "string", "format": "uuid" },
            "afterScanId": { "type": "string", "format": "uuid" },
            "rootPath": { "type": "string" },
            "verifiedAt": datetime("When the comparison ran"),
            "items": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "ruleId": { "type": "string" },
                        "label": { "type": "string" },
                        "paths": { "type": "array", "items": { "type": "string" } },
                        "expectedFreedBytes": bytes("The plan's promise for this item"),
                        "actualFreedBytes": { "type": ["integer", "null"], "description": "Σ (before − after) over the item's directories; null when none was a persisted directory" },
                        "beforeBytes": nullable("integer", "Σ of the paths' sizes before"),
                        "afterBytes": nullable("integer", "Σ after (0 for paths that are gone)")
                    },
                    "required": ["ruleId", "label", "paths", "expectedFreedBytes", "actualFreedBytes", "beforeBytes", "afterBytes"]
                }
            },
            "expectedFreedBytes": bytes("The plan's total promise"),
            "actualFreedBytes": { "type": "integer", "description": "before.totalDiskSize − after.totalDiskSize: everything that changed under the root; negative means the tree grew" },
            "shortfallBytes": { "type": "integer", "description": "expected − actual; positive: less came back than promised" },
            "withinTolerance": { "type": "boolean", "description": "|shortfall| ≤ 5% of expected, either direction" }
        },
        "required": ["planId", "beforeScanId", "afterScanId", "rootPath", "verifiedAt", "items", "expectedFreedBytes", "actualFreedBytes", "shortfallBytes", "withinTolerance"]
    })
}

pub fn explanation_schema() -> Value {
    json!({
        "type": "object",
        "description": "Why one path is what it is, and what deleting it would do.",
        "properties": {
            "path": { "type": "string" },
            "isDir": { "type": "boolean" },
            "diskSize": bytes("Allocated disk bytes — THE size"),
            "logicalSize": bytes("Apparent bytes"),
            "privateSize": nullable("integer", "What deleting this path actually frees; null == not recorded (pre-1.1 row)"),
            "sharedSize": nullable("integer", "Bytes pinned by clones, hard links or snapshots"),
            "flags": { "type": "array", "items": { "type": "string" }, "description": "The entry's filesystem flags (dataless, purgeable, sparse, compressed, mayShareBlocks, sharesAllBlocks, …)" },
            "dataless": { "type": "boolean", "description": "A cloud placeholder whose bytes are not local — deleting frees ~nothing" },
            "cloneId": nullable("integer", "APFS clone-group id when the file shares blocks"),
            "nlink": { "type": "integer", "minimum": 1 },
            "category": nullable("string", "The classifier's category, or null for ordinary content"),
            "hotspot": {
                "type": ["object", "null"],
                "description": "The hotspot group that classified it; null for ordinary content",
                "properties": {
                    "ruleId": { "type": "string" },
                    "label": { "type": "string" },
                    "riskTier": tier_enum("safe / caution / review"),
                    "why": { "type": "string" },
                    "command": nullable("string", "The owning tool's own clean command, when one exists"),
                    "hint": { "type": "string" },
                    "matchedBy": { "type": "string", "enum": ["topPath", "category"], "description": "topPath: the path is (under) one of the group's listed roots; category: matched by category alone" }
                },
                "required": ["ruleId", "label", "riskTier", "why", "command", "hint", "matchedBy"]
            },
            "unreadableBelow": {
                "type": "array",
                "description": "The scan's unreadable-path SAMPLE, filtered to this path and below (permissions or TCC)",
                "items": { "type": "object", "properties": { "path": { "type": "string" }, "reason": { "type": "string" } }, "required": ["path", "reason"] }
            },
            "unreadableBelowCount": { "type": "integer", "minimum": 0 },
            "summary": { "type": "string", "description": "The verdict in one sentence — quote it" }
        },
        "required": ["path", "isDir", "diskSize", "logicalSize", "privateSize", "sharedSize", "flags", "dataless", "cloneId", "nlink", "category", "hotspot", "unreadableBelow", "unreadableBelowCount", "summary"]
    })
}

fn artifact_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "ruleId": { "type": "string" },
            "path": { "type": "string" },
            "category": { "type": "string" },
            "riskTier": tier_enum("safe / caution / review"),
            "diskSize": bytes("Deduped disk bytes under this artifact root")
        },
        "required": ["ruleId", "path", "category", "riskTier", "diskSize"]
    })
}

pub fn stale_schema() -> Value {
    json!({
        "type": "object",
        "description": "Projects quiet for at least thresholdDays, with the build artifacts inside them.",
        "properties": {
            "scanId": { "type": "string", "format": "uuid" },
            "rootPath": { "type": "string" },
            "thresholdDays": { "type": "integer", "minimum": 0, "description": "The threshold applied to this answer (from olderThan; default 90)" },
            "projectsEvaluated": { "type": "integer", "minimum": 0, "description": "Project roots the staleness rule saw in the scan" },
            "unverifiable": { "type": "integer", "minimum": 0, "description": "Roots with no dated evidence — never stale, never listed" },
            "projects": {
                "type": "array",
                "description": "Biggest artifact bytes first",
                "items": {
                    "type": "object",
                    "properties": {
                        "root": { "type": "string" },
                        "lastActivityDays": { "type": "integer", "minimum": 0, "description": "Days since the newest git activity or source edit, as of the scan" },
                        "artifactDiskSize": bytes("Σ artifacts' diskSize"),
                        "artifacts": { "type": "array", "items": artifact_schema() }
                    },
                    "required": ["root", "lastActivityDays", "artifactDiskSize", "artifacts"]
                }
            },
            "artifactDiskSize": bytes("Σ over the listed projects")
        },
        "required": ["scanId", "rootPath", "thresholdDays", "projectsEvaluated", "unverifiable", "projects", "artifactDiskSize"]
    })
}

pub fn volume_schema() -> Value {
    json!({
        "type": "object",
        "description": "The volume holding `path` (default: the data volume): statfs for the \
            container, getattrlist for this volume's own usage, CoreFoundation for purgeable, \
            and the hidden-space split.",
        "properties": {
            "path": { "type": "string" },
            "mountPoint": { "type": "string" },
            "filesystem": { "type": "string" },
            "totalBytes": bytes("APFS container size"),
            "usedBytes": bytes("total − free: every volume in the container together"),
            "freeBytes": bytes("Free to root"),
            "availableBytes": bytes("Free to an unprivileged process — the honest headroom"),
            "volumeUsedBytes": nullable("integer", "THIS volume's own consumption (getattrlist ATTR_VOL_SPACEUSED); null when the filesystem does not report it"),
            "purgeableBytes": nullable("integer", "importantUsageBytes − availableBytes: space macOS may reclaim on its own (caches, local snapshots); null when CoreFoundation has no answer"),
            "importantUsageBytes": nullable("integer", "Finder's 'Available' — free for an important write, purgeable included"),
            "opportunisticUsageBytes": nullable("integer", "Free for an opportunistic write — the conservative figure"),
            "snapshotCount": nullable("integer", "Local Time Machine snapshots; null unless snapshots: true"),
            "snapshots": { "type": ["array", "null"], "items": { "type": "string" }, "description": "Their names; null unless asked" },
            "hidden": {
                "type": "object",
                "description": "Where the bytes a scan did not see went. Always present; scan-relative fields are null until scanId names a completed scan.",
                "properties": {
                    "scanId": nullable("string", "The completed scan the split is relative to"),
                    "scanRootPath": nullable("string", "Its root"),
                    "scannedBytes": nullable("integer", "The scan's totalDiskSize (deduped allocated bytes)"),
                    "unscannedBytes": nullable("integer", "volumeUsedBytes − scannedBytes, floored at 0: bytes on this volume the scan did not count (outside its root, other users, unreadable, snapshot-pinned)"),
                    "otherVolumesBytes": nullable("integer", "usedBytes − volumeUsedBytes: System, Preboot, Recovery, VM sharing the container; independent of any scan"),
                    "otherUserHomes": {
                        "type": "array",
                        "description": "Home directories under /Users other than the current user's; sizes are not reported (unreadable ones cannot be measured). Empty off the data volume",
                        "items": {
                            "type": "object",
                            "properties": {
                                "path": { "type": "string" },
                                "readable": { "type": "boolean", "description": "access(R_OK|X_OK) succeeded — it could be scanned" }
                            },
                            "required": ["path", "readable"]
                        }
                    },
                    "unreadableCount": nullable("integer", "The scan's errorCount; null without a scan"),
                    "snapshotSuggestion": nullable("string", "A `tmutil thinlocalsnapshots` command line for the USER to run; null when snapshots were not listed or none exist. Phantom never runs it")
                },
                "required": ["scanId", "scanRootPath", "scannedBytes", "unscannedBytes", "otherVolumesBytes", "otherUserHomes", "unreadableCount", "snapshotSuggestion"]
            },
            "note": { "type": "string" }
        },
        "required": ["path", "mountPoint", "filesystem", "totalBytes", "usedBytes", "freeBytes", "availableBytes", "volumeUsedBytes", "purgeableBytes", "importantUsageBytes", "opportunisticUsageBytes", "snapshotCount", "snapshots", "hidden", "note"]
    })
}

pub fn growth_schema() -> Value {
    json!({
        "type": "object",
        "description": "How one root grew across its completed scans (oldest first) with a linear forecast.",
        "properties": {
            "rootPath": { "type": "string", "description": "As the newest scan recorded it" },
            "groupBy": { "type": "string", "enum": ["total", "category", "topLevelDir", "extension"] },
            "points": {
                "type": "array",
                "description": "One per completed scan, oldest first",
                "items": {
                    "type": "object",
                    "properties": {
                        "scanId": { "type": "string", "format": "uuid" },
                        "startedAt": datetime("When the scan started"),
                        "totalDiskSize": bytes("THE size: deduped allocated bytes under the root"),
                        "totalPrivateSize": nullable("integer", "Null on pre-v1.1 scans, which also counted APFS clones twice"),
                        "fileCount": { "type": "integer", "minimum": 0 }
                    },
                    "required": ["scanId", "startedAt", "totalDiskSize", "totalPrivateSize", "fileCount"]
                }
            },
            "series": {
                "type": "array",
                "description": "Ranked by the newest point's value; at most 10 keys plus `other`",
                "items": {
                    "type": "object",
                    "properties": {
                        "key": { "type": "string" },
                        "values": {
                            "type": "array",
                            "description": "Aligned with points; null = that scan recorded nothing for this breakdown, 0 = recorded as absent",
                            "items": { "type": ["integer", "null"], "minimum": 0 }
                        }
                    },
                    "required": ["key", "values"]
                }
            },
            "forecast": {
                "type": ["object", "null"],
                "description": "Null with fewer than two points or a zero span",
                "properties": {
                    "method": { "type": "string", "enum": ["linear"] },
                    "pointsUsed": { "type": "integer", "minimum": 2 },
                    "spanDays": { "type": "number", "description": "First point to last" },
                    "bytesPerDay": { "type": "integer", "description": "Fitted slope; negative when shrinking" },
                    "latestBytes": bytes("The newest point's totalDiskSize"),
                    "availableBytes": nullable("integer", "statfs f_bavail of the root's volume now; null if the root cannot be stat'd"),
                    "daysUntilFull": nullable("number", "availableBytes / bytesPerDay when growing and the volume is readable; else null"),
                    "projectedFullAt": nullable_datetime("now + daysUntilFull"),
                    "caveat": { "type": "string", "description": "Travels with the number; quote it when you quote the number" }
                },
                "required": ["method", "pointsUsed", "spanDays", "bytesPerDay", "latestBytes", "availableBytes", "daysUntilFull", "projectedFullAt", "caveat"]
            },
            "note": { "type": "string" }
        },
        "required": ["rootPath", "groupBy", "points", "series", "forecast", "note"]
    })
}

pub fn health_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "status": { "type": "string", "enum": ["ok", "degraded"] },
            "version": { "type": "string" }
        },
        "required": ["status", "version"]
    })
}

/// `structuredContent` MUST be a JSON object, but two tools' HTTP bodies
/// are bare arrays. The TEXT content stays the body verbatim (the e2e
/// parity gate compares it byte-for-byte); only the structured twin wraps.
pub fn structured_content(tool: &str, result: &Value) -> Value {
    match tool {
        "list_scans" => json!({ "scans": result }),
        "get_space_by_type" => json!({ "types": result }),
        _ => result.clone(),
    }
}

/// The `scanId` property every result tool shares.
fn scan_id_property() -> Value {
    json!({
        "type": "string",
        "format": "uuid",
        "description": "Scan UUID. Omit to use the most recent completed scan. \
            Only the newest 25 scans of each root (100 overall) are kept — a stale id 404s; rescan rather \
            than retrying the id."
    })
}

/// The `responseFormat` property of the tools whose payload has a concise
/// projection. Default is detailed so the text content stays the HTTP body
/// verbatim.
fn response_format_property(concise_means: &str) -> Value {
    json!({
        "type": "string",
        "enum": ["concise", "detailed"],
        "description": format!(
            "detailed (default) returns the API body verbatim; concise {concise_means}. \
             Prefer concise unless you need the extra fields — it is a fraction of the tokens."
        )
    })
}

fn budget_meta() -> Value {
    json!({ MAX_RESULT_SIZE_META_KEY: MAX_RESULT_CHARS })
}

pub fn tool_definitions() -> Value {
    json!([
        {
            "name": "get_volume_status",
            "title": "Volume status",
            "description": "How bad is it, and where is 'System Data': the \
                volume's total, used, free, available and purgeable bytes — \
                anchor here BEFORE scanning, and come back with a scanId \
                AFTER one for the hidden-space split. Reads the DATA volume \
                by default (/System/Volumes/Data); on APFS `df /` reports the \
                sealed system snapshot and is wrong for this question. \
                usedBytes is the whole APFS container; volumeUsedBytes is \
                this volume alone and hidden.otherVolumesBytes the rest \
                (System, Preboot, Recovery, VM). purgeableBytes is what \
                Finder's 'Available' silently adds to availableBytes (from \
                CoreFoundation, no subprocess). With scanId (a COMPLETED scan \
                on this volume): hidden.unscannedBytes = volumeUsedBytes − \
                the scan's totalDiskSize — outside the root, other users' \
                homes (hidden.otherUserHomes lists them with readability), \
                unreadable entries (hidden.unreadableCount), snapshot-pinned \
                blocks. snapshots: true also lists local Time Machine \
                snapshots — the usual reason freed space does not show up for \
                hours — by running /usr/bin/tmutil (fixed path, read-only, \
                10 s bound; off by default so nothing runs unless asked) and \
                fills hidden.snapshotSuggestion with a `tmutil \
                thinlocalsnapshots` line for the USER to run; Phantom never \
                runs it and never deletes.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Any path on the volume of interest (default: the data volume)"
                    },
                    "snapshots": {
                        "type": "boolean",
                        "description": "Also list local Time Machine snapshots via tmutil (default false)"
                    },
                    "scanId": {
                        "type": "string",
                        "format": "uuid",
                        "description": "A completed scan on this volume, for the used − scanned split (from scan_directory or list_scans). Omit before scanning."
                    }
                },
                "required": []
            },
            "outputSchema": volume_schema(),
            "annotations": read_only("Volume status")
        },
        {
            "name": "scan_directory",
            "title": "Scan a directory",
            "description": "Scan a directory tree and record its disk usage. \
                By default waits for the scan to finish and returns the \
                completed scan. totalDiskSize is allocated disk bytes, DEDUPED \
                (a hardlinked inode or an APFS clone group counts once, like \
                `du` — a tree of cargo/pnpm hardlinks or a Finder-duplicated \
                folder totals far less than the naive sum). totalPrivateSize \
                is what deleting the whole tree would ACTUALLY free (clones, \
                hard links and snapshots pin the rest: totalSharedSize). One \
                filesystem by default: mount points of other volumes are \
                recorded but not walked (crossVolumes: true descends). Large \
                trees (a home directory) can take MINUTES to walk: prefer \
                wait: false and poll with scan_status. A waited call gives up \
                after 60s and returns the still-running view with a note \
                field; keep following it via scan_status. While it waits, a \
                caller that sent _meta.progressToken receives \
                notifications/progress (files seen, bytes, current path) \
                about once a second. The completed scan \
                also carries unreadablePaths: a capped SAMPLE (first 100) of \
                the entries behind errorCount — errorCount is the truth, the \
                list is a where-did-it-fail sample. Every call records a NEW \
                scan (not idempotent); only the newest 25 per root (100 overall) are kept.",
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
                    },
                    "crossVolumes": {
                        "type": "boolean",
                        "description": "Descend into other volumes mounted below the root (default false: one filesystem, like du -x)"
                    },
                    "olderThan": {
                        "type": "string",
                        "description": "Staleness threshold for the classifier: a project whose newest source edit and git activity are at least this old is dormant (90d, 12w, 3M, 1y, or bare days; default 90d)"
                    },
                    "verifyLocks": {
                        "type": "boolean",
                        "description": "Opt-in (default false): run each project's read-only lockfile check (cargo metadata --locked, npm ci --dry-run, uv lock --locked) from fixed install paths inside the project dir; a failure lowers the group's riskTier. Nothing is written. Do not enable on checkouts you do not trust."
                    },
                    "toolEstimates": {
                        "type": "boolean",
                        "description": "Opt-in (default false): attach the owning tool's own dry-run number (docker system df, brew cleanup -n, uv cache size) to matching hotspot groups as toolEstimate"
                    },
                    "responseFormat": response_format_property(
                        "drops the logical/shared sizes, dirCount and the unreadablePaths sample"
                    )
                },
                "required": ["path"]
            },
            "outputSchema": scan_schema(),
            "annotations": annotations("Scan a directory", false, false)
        },
        {
            "name": "scan_status",
            "title": "Scan status",
            "description": "Current view of ONE scan by id: live progress \
                (filesSeen, bytesSeen, currentPath) while it runs; totals \
                once terminal. Use it after scan_directory with wait: false, \
                or when a waited call returned still running with a note. \
                Cheap — poll every few seconds. status is running | complete \
                | cancelled | failed; a failed scan carries failureReason \
                (\"interrupted: …\" means the server restarted mid-walk; scan \
                again). Unknown or pruned id: not found.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "scanId": {
                        "type": "string",
                        "format": "uuid",
                        "description": "The scan to look at (from scan_directory or list_scans). Required — there is no default here; list_scans shows every scan."
                    },
                    "responseFormat": response_format_property(
                        "keeps id, rootPath, status, times, totalDiskSize, totalPrivateSize, fileCount, errorCount (and progress while running)"
                    )
                },
                "required": ["scanId"]
            },
            "outputSchema": scan_schema(),
            "annotations": read_only("Scan status")
        },
        {
            "name": "cancel_scan",
            "title": "Cancel a scan",
            "description": "Stop a RUNNING scan cooperatively. The walker \
                bails at its next entry; the answer is the scan view, whose \
                status may still read running for a moment — poll scan_status \
                for the terminal `cancelled`. Partial results are discarded \
                (the scan stays listed as cancelled, with no readable \
                results). Already complete/cancelled/failed: an error saying \
                so. Files on disk are never touched by any Phantom tool.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "scanId": {
                        "type": "string",
                        "format": "uuid",
                        "description": "The running scan to stop. Required."
                    }
                },
                "required": ["scanId"]
            },
            "outputSchema": scan_schema(),
            "annotations": annotations("Cancel a scan", false, true)
        },
        {
            "name": "list_scans",
            "title": "List scans",
            "description": "List recorded scans, NEWEST FIRST — running scans \
                included (their progress field carries live counters). All \
                sizes are disk bytes (st_blocks × 512), hardlink-deduped (an \
                inode counts once, like `du`). Note the order when picking a \
                pair for diff_scans: element 0 is the newest, so passing \
                [0] as scanA and [1] as scanB is REVERSE-chronological — put \
                the older scan (higher index) in scanA to read deltas forward. \
                The text content is the bare array; structuredContent wraps it \
                as {scans: [...]}.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "responseFormat": response_format_property(
                        "keeps id, rootPath, status, times, totalDiskSize, totalPrivateSize, fileCount, errorCount (and progress while running)"
                    )
                },
                "required": []
            },
            "outputSchema": {
                "type": "object",
                "properties": { "scans": { "type": "array", "items": scan_schema() } },
                "required": ["scans"]
            },
            "annotations": read_only("List scans")
        },
        {
            "name": "find_large_files",
            "title": "Find large files",
            "description": "Largest files of a scan, by disk size descending. \
                Returns {files, nextCursor}: when nextCursor is non-null, more \
                files remain — call again with cursor set to that value. Only \
                files of at least 1 MiB disk size are recorded individually; \
                smaller files are folded into directory totals. Each file \
                carries diskSize (allocated) AND privateSize (what deleting \
                it frees: 0 for a hard link or an APFS pure clone — cloneId \
                non-null — and 0 for a file held by a snapshot); trust \
                privateSize when advising a deletion. flags names filesystem \
                facts: dataless (cloud placeholder), purgeable, sparse, \
                mayShareBlocks, sharesAllBlocks. Default page is 100 files \
                (server cap 500); a page over the result budget is refused \
                with a message naming a limit that fits — prefer \
                responseFormat: concise for wide pages.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "scanId": scan_id_property(),
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Max files per page (server caps it at 500). Omit for the default page size (100)."
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
                    },
                    "responseFormat": response_format_property(
                        "keeps path, diskSize, privateSize, fileType per file"
                    )
                },
                "required": []
            },
            "outputSchema": {
                "type": "object",
                "properties": {
                    "files": { "type": "array", "items": entry_schema() },
                    "nextCursor": nullable("string", "Continuation token; null on the last page")
                },
                "required": ["files", "nextCursor"]
            },
            "annotations": read_only("Find large files"),
            "_meta": budget_meta()
        },
        {
            "name": "get_space_by_type",
            "title": "Space by file type",
            "description": "Disk usage of a scan grouped by file type \
                (lowercased extension; null = no extension), largest first. \
                Computed from the FULL walk, so small files count here even \
                though they have no individual file rows. The text content is \
                the bare array; structuredContent wraps it as {types: [...]}.",
            "inputSchema": {
                "type": "object",
                "properties": { "scanId": scan_id_property() },
                "required": []
            },
            "outputSchema": {
                "type": "object",
                "properties": { "types": { "type": "array", "items": type_total_schema() } },
                "required": ["types"]
            },
            "annotations": read_only("Space by file type")
        },
        {
            "name": "get_treemap",
            "title": "Treemap layout",
            "description": "Squarified treemap layout of a scan's disk usage. \
                Pass root to re-root the layout at a subdirectory; width/height \
                to lay out at a specific view size; maxDepth to limit nesting \
                (default 4). The response grows with directory breadth: a \
                layout over the result budget is served to the deepest depth \
                that fits and carries a `truncated` field naming the next call \
                — re-root at the biggest child or lower maxDepth. For a size \
                hierarchy without geometry use responseFormat: concise.",
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
                    },
                    "responseFormat": response_format_property(
                        "keeps path, size, depth, isDir per rect — the hierarchy without the geometry"
                    )
                },
                "required": []
            },
            "outputSchema": treemap_schema(),
            "annotations": read_only("Treemap layout"),
            "_meta": budget_meta()
        },
        {
            "name": "get_hotspots",
            "title": "Reclaimable hotspots",
            "description": "Reclaimable-space hotspots of a scan, classified \
                when the scan completed. Returns {groups, reclaimEstimate, \
                reviewDiskSize, cloudDataloadedLogicalSize, \
                cloudDataloadedDiskSize}. Each group carries diskSize \
                (deduped allocation, du model) AND privateSize (what deleting \
                the group's paths would ACTUALLY free: hard links or APFS \
                clones referenced outside the group, and snapshot-held \
                blocks, contribute 0). reclaimEstimate is the sum of \
                privateSize over the reclaimable categories only; it \
                excludes cloud-dataloaded placeholders. Quote privateSize \
                when promising freed space. Each group also carries riskTier \
                (safe | caution | review — safe means a lockfile pins the \
                rebuild or a cache its owner repopulates; caution means the \
                rebuild may differ, costs a large download, or must go through \
                the tool's own command; review means data may be lost), a \
                one-sentence why, rebuildCost {kind: download|compile|none, \
                estimate}, and toolEstimate (the tool's own dry-run number, \
                or null unless the scan asked for toolEstimates). Treat \
                review as not-a-suggestion. Phantom never deletes: each \
                group carries an action hint naming the safe tool (e.g. \
                `cargo clean`), not an operation.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "scanId": scan_id_property(),
                    "responseFormat": response_format_property(
                        "keeps ruleId, label, category, riskTier, why, command, diskSize, privateSize, topPaths per group"
                    )
                },
                "required": []
            },
            "outputSchema": hotspots_schema(),
            "annotations": read_only("Reclaimable hotspots")
        },
        {
            "name": "explain_path",
            "title": "Explain a path",
            "description": "Why is THIS path here and what would deleting it do: \
                its three sizes (diskSize, logicalSize, privateSize — what \
                deleting actually frees), sharing facts (cloneId, nlink, \
                sharedSize), whether it is a cloud placeholder (dataless), \
                the hotspot group that classified it (tier, why, safe \
                command) or 'not a hotspot', and the unreadable subtrees \
                below it (permissions / TCC — the sizes undercount there). \
                `summary` is the verdict in one sentence; quote it. The path \
                must be one the scan recorded (a persisted directory, or a \
                file ≥ 1 MiB); anything else is not found.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "scanId": scan_id_property(),
                    "path": {
                        "type": "string",
                        "description": "The path, exactly as the scan recorded it (from get_treemap, find_large_files or get_hotspots)"
                    }
                },
                "required": ["path"]
            },
            "outputSchema": explanation_schema(),
            "annotations": read_only("Explain a path")
        },
        {
            "name": "find_stale_projects",
            "title": "Find stale projects",
            "description": "Projects whose newest source edit AND git activity \
                are at least olderThan old (default 90d), with the build \
                artifacts inside each (rule, path, tier, bytes), biggest \
                first. Re-thresholded from what the scan recorded — no \
                re-walk, so any threshold is instant. Staleness only ranks: a \
                stale artifact's tier still comes from its lockfile. Roots \
                with no dated evidence are `unverifiable` and never listed. \
                Feed the result to plan_reclaim (the artifacts are its \
                staleProjectArtifact groups) rather than acting on paths \
                directly.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "scanId": scan_id_property(),
                    "olderThan": {
                        "type": "string",
                        "description": "Threshold: 90d, 12w, 3M, 1y, or bare days (default 90d)"
                    }
                },
                "required": []
            },
            "outputSchema": stale_schema(),
            "annotations": read_only("Find stale projects")
        },
        {
            "name": "plan_reclaim",
            "title": "Plan a reclaim",
            "description": "Build the dry-run PLAN from a completed scan's hotspots: \
                ordered items {ruleId, label, category, riskTier, why, command, \
                paths, expectedFreedBytes} plus planId and totals. Items are safe \
                groups by default; maxTier: \"caution\" admits caution groups (read \
                each why first); review groups are NEVER items. expectedFreedBytes \
                is privateSize — the honest number. Show the plan to the user and \
                get an explicit yes BEFORE acting. Phantom never deletes: with \
                includeScript: true the result carries `script`, a POSIX sh script \
                that only prints until run with PHANTOM_APPLY=1, when it MOVES each \
                path into ~/.Trash/phantom-<planId>/ and logs. A path that cannot \
                be moved is printed as `FAILED: <path> — <reason>` and the script \
                continues; it ends with `moved N, failed N, skipped N` and exits 1 \
                if anything failed — report failed paths as not freed. Run it (or \
                the items' own commands) in your shell under the user's permission \
                system, then call verify_reclaim with the planId. Each call records \
                a NEW plan (only the last 25 are kept).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "scanId": scan_id_property(),
                    "maxTier": {
                        "type": "string",
                        "enum": ["safe", "caution"],
                        "description": "Highest risk tier to include (default safe)"
                    },
                    "minBytes": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Skip groups that would free fewer bytes than this (default 0)"
                    },
                    "includeScript": {
                        "type": "boolean",
                        "description": "Also return the plan as a runnable dry-run shell script in `script` (default false)"
                    }
                },
                "required": []
            },
            "outputSchema": plan_schema(),
            "annotations": annotations("Plan a reclaim", false, false)
        },
        {
            "name": "verify_reclaim",
            "title": "Verify a reclaim",
            "description": "After the plan was run: compare its promise with a rescan. \
                With no afterScanId this scans the plan's root now (waiting like \
                scan_directory, with progress notifications if you sent a \
                progressToken); if that rescan is still running after 60s the call \
                errors with the afterScanId to pass on the next call. Returns per \
                item expected vs actual (before/after directory bytes) and the \
                headline actualFreedBytes, shortfallBytes and withinTolerance \
                (|shortfall| ≤ 5% of expected). The headline is the root delta, \
                or Σ of the items' actuals when the plan's Trash folder sits \
                inside the scanned root (a home scan) — the root delta would \
                count the moved bytes again. Positive means freed, negative \
                means the tree grew. Report actual, not expected, to the user, \
                and say that the paths are in the Trash: the space returns when \
                the Trash is emptied. A rescan must be complete, cover the \
                plan's root and have started after the plan was built.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "planId": {
                        "type": "string",
                        "format": "uuid",
                        "description": "The plan to verify (from plan_reclaim). Required."
                    },
                    "afterScanId": {
                        "type": "string",
                        "format": "uuid",
                        "description": "A completed rescan of the plan's root to compare against. Omit to rescan now."
                    }
                },
                "required": ["planId"]
            },
            "outputSchema": verification_schema(),
            "annotations": annotations("Verify a reclaim", false, false)
        },
        {
            "name": "diff_scans",
            "title": "Diff two scans",
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
                        "description": "The 'before' scan id — the OLDER scan. Only the newest 25 scans of each root are kept; a pruned baseline is gone (rescanning makes a NEW scan, not the old one), so diff soon after scanning."
                    },
                    "scanB": {
                        "type": "string",
                        "format": "uuid",
                        "description": "The 'after' scan id (the newer one)"
                    }
                },
                "required": ["scanA", "scanB"]
            },
            "outputSchema": diff_schema(),
            "annotations": read_only("Diff two scans")
        },
        {
            "name": "get_growth",
            "title": "Growth over time",
            "description": "How a root grew across its completed scans, with a \
                linear 'disk full in N days' forecast. No re-walk: points are \
                the persisted scans of `root` (default: the newest completed \
                scan's root), oldest first, each with totalDiskSize; `series` \
                breaks the bytes down by groupBy — total (one line), category \
                (Σ hotspot bytes per classifier category; null for scans with \
                no summary), topLevelDir (the root's child directories, the \
                rest as `other`) or extension — at most 10 keys plus `other`, \
                ranked by the newest scan. `forecast` is an ordinary least-\
                squares line: bytesPerDay, and daysUntilFull = the volume's \
                available bytes / bytesPerDay ONLY when growing (null when \
                shrinking, flat, or with fewer than two points). ALWAYS relay \
                `forecast.caveat` beside the number: it assumes the rate \
                continues, nothing is reclaimed, and this root alone fills \
                the volume. Only the newest 25 scans of each root are kept, so the history \
                is the retention window. An unknown root is a not-found error \
                naming list_scans.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "root": {
                        "type": "string",
                        "description": "The scanned root path (default: the newest completed scan's root)"
                    },
                    "groupBy": {
                        "type": "string",
                        "enum": ["total", "category", "topLevelDir", "extension"],
                        "description": "What each series line is keyed by (default total)"
                    }
                },
                "required": []
            },
            "outputSchema": growth_schema(),
            "annotations": read_only("Growth over time")
        },
        {
            "name": "health",
            "title": "API health",
            "description": "Check the API server's health. Returns {status, version}; \
                status is \"ok\" when the datastore is usable, \"degraded\" otherwise.",
            "inputSchema": { "type": "object", "properties": {}, "required": [] },
            "outputSchema": health_schema(),
            "annotations": read_only("API health")
        }
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    // Shared wire fixtures — the same raw bytes the Rust, Swift and
    // conformance suites parse (tests/fixtures/README.md).
    const RAW_SCAN_COMPLETE: &str = include_str!("../../../tests/fixtures/scan-complete.json");
    const RAW_SCAN_RUNNING: &str = include_str!("../../../tests/fixtures/scan-running.json");
    const RAW_SCAN_INTERRUPTED: &str = include_str!("../../../tests/fixtures/scan-interrupted.json");
    const RAW_ENTRY: &str = include_str!("../../../tests/fixtures/entry.json");
    const RAW_ENTRY_DIR: &str = include_str!("../../../tests/fixtures/entry-dir.json");
    const RAW_TYPES: &str = include_str!("../../../tests/fixtures/types.json");
    const RAW_TREEMAP: &str = include_str!("../../../tests/fixtures/treemap.json");
    const RAW_HOTSPOTS: &str = include_str!("../../../tests/fixtures/hotspots-summary.json");
    const RAW_DIFF: &str = include_str!("../../../tests/fixtures/scan-diff.json");
    const RAW_PLAN: &str = include_str!("../../../tests/fixtures/reclaim-plan.json");
    const RAW_EXPLANATION: &str = include_str!("../../../tests/fixtures/path-explanation.json");
    const RAW_STALE: &str = include_str!("../../../tests/fixtures/stale-projects.json");
    const RAW_VOLUME: &str = include_str!("../../../tests/fixtures/volume-status.json");
    const RAW_VERIFICATION: &str = include_str!("../../../tests/fixtures/reclaim-verification.json");
    const RAW_GROWTH: &str = include_str!("../../../tests/fixtures/growth.json");

    fn parse(raw: &str) -> Value {
        serde_json::from_str(raw).expect("fixture is valid JSON")
    }

    fn tools() -> Vec<Value> {
        tool_definitions().as_array().unwrap().clone()
    }

    fn tool(name: &str) -> Value {
        tools()
            .into_iter()
            .find(|t| t["name"] == name)
            .unwrap_or_else(|| panic!("no tool {name}"))
    }

    /// A small structural validator: every key the fixture carries must be
    /// declared in the schema (at every depth), every `required` key must
    /// be present in the fixture, and declared types must admit the
    /// fixture's value. Not a full JSON Schema engine — enough to catch a
    /// wire key the schema forgot, a typo'd key, or a nullable the schema
    /// calls non-null.
    fn assert_covers(schema: &Value, value: &Value, at: &str) {
        let declared = schema["type"].clone();
        let admits = |t: &str| match &declared {
            Value::String(s) => s == t,
            Value::Array(a) => a.iter().any(|x| x == t),
            _ => true,
        };
        match value {
            Value::Null => assert!(admits("null"), "{at}: fixture is null but schema type is {declared}"),
            Value::Bool(_) => assert!(admits("boolean"), "{at}: boolean not admitted by {declared}"),
            Value::Number(n) => {
                let ok = admits("number") || (n.is_i64() || n.is_u64()) && admits("integer");
                assert!(ok, "{at}: number {n} not admitted by {declared}");
                if let Some(min) = schema["minimum"].as_i64()
                    && let Some(v) = n.as_i64()
                {
                    assert!(v >= min, "{at}: {v} below schema minimum {min}");
                }
            }
            Value::String(s) => {
                assert!(admits("string"), "{at}: string not admitted by {declared}");
                if let Some(en) = schema["enum"].as_array() {
                    assert!(en.iter().any(|e| e == s), "{at}: {s:?} not in enum {en:?}");
                }
            }
            Value::Array(items) => {
                assert!(admits("array"), "{at}: array not admitted by {declared}");
                for (i, item) in items.iter().enumerate() {
                    assert_covers(&schema["items"], item, &format!("{at}[{i}]"));
                }
            }
            Value::Object(map) => {
                assert!(admits("object"), "{at}: object not admitted by {declared}");
                let props = schema["properties"]
                    .as_object()
                    .unwrap_or_else(|| panic!("{at}: object schema without properties"));
                for (k, v) in map {
                    let sub = props
                        .get(k)
                        .unwrap_or_else(|| panic!("{at}: wire key {k:?} is not in the outputSchema"));
                    assert_covers(sub, v, &format!("{at}.{k}"));
                }
                if let Some(req) = schema["required"].as_array() {
                    for r in req {
                        let r = r.as_str().unwrap();
                        assert!(map.contains_key(r), "{at}: required key {r:?} missing from fixture");
                        assert!(props.contains_key(r), "{at}: required key {r:?} not declared in properties");
                    }
                }
            }
        }
    }

    #[test]
    fn tools_list_order_is_fixed() {
        let names: Vec<String> = tools().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect();
        assert_eq!(names, TOOL_NAMES.iter().map(|s| s.to_string()).collect::<Vec<_>>(), "tools/list order changed");
    }

    #[test]
    fn every_tool_is_annotated_and_titled() {
        for t in tools() {
            let name = t["name"].as_str().unwrap();
            let a = &t["annotations"];
            assert!(t["title"].is_string(), "{name}: no title");
            assert_eq!(a["title"], t["title"], "{name}: annotation title differs from tool title");
            for hint in ["readOnlyHint", "destructiveHint", "idempotentHint", "openWorldHint"] {
                assert!(a[hint].is_boolean(), "{name}: {hint} missing (spec default would make it look worse)");
            }
            assert_eq!(a["destructiveHint"], false, "{name}: nothing here destroys anything");
            assert_eq!(a["openWorldHint"], false, "{name}: every tool talks to the local API only");
            assert!(t["outputSchema"]["type"] == "object", "{name}: outputSchema must describe an object (structuredContent)");
            assert!(t["inputSchema"]["type"] == "object", "{name}: inputSchema must be an object schema");
        }
    }

    #[test]
    fn writers_and_non_idempotent_tools_are_exactly_the_pinned_sets() {
        for t in tools() {
            let name = t["name"].as_str().unwrap();
            let writes = WRITING_TOOLS.contains(&name);
            assert_eq!(t["annotations"]["readOnlyHint"], !writes, "{name}: readOnlyHint");
            assert_eq!(
                t["annotations"]["idempotentHint"],
                !NON_IDEMPOTENT_TOOLS.contains(&name),
                "{name}: idempotentHint (cancelling twice is cancelling once; a scan, a plan, a rescan are each NEW)"
            );
        }
        assert_eq!(WRITING_TOOLS, ["scan_directory", "cancel_scan", "plan_reclaim", "verify_reclaim"]);
        assert_eq!(NON_IDEMPOTENT_TOOLS, ["scan_directory", "plan_reclaim", "verify_reclaim"]);
        // Nothing an agent can reach is destructive: plans MOVE to the Trash
        // and only when the user runs the script.
        for t in tools() {
            assert_eq!(t["annotations"]["destructiveHint"], false);
        }
    }

    #[test]
    fn insight_schemas_cover_their_fixtures_and_the_tools_are_read_only() {
        assert_covers(&tool("explain_path")["outputSchema"], &parse(RAW_EXPLANATION), "explanation");
        // A path with no hotspot and a pinned clone must also validate.
        let mut plain = parse(RAW_EXPLANATION);
        plain["hotspot"] = Value::Null;
        plain["category"] = Value::Null;
        plain["cloneId"] = json!(42);
        plain["flags"] = json!(["mayShareBlocks", "sharesAllBlocks"]);
        plain["unreadableBelow"] = json!([{ "path": "/x", "reason": "Permission denied (os error 13)" }]);
        assert_covers(&tool("explain_path")["outputSchema"], &plain, "explanation/plain");
        assert_covers(&tool("find_stale_projects")["outputSchema"], &parse(RAW_STALE), "stale");
        assert_covers(&tool("get_volume_status")["outputSchema"], &parse(RAW_VOLUME), "volume");
        let mut quiet = parse(RAW_VOLUME);
        quiet["snapshotCount"] = Value::Null;
        quiet["snapshots"] = Value::Null;
        quiet["volumeUsedBytes"] = Value::Null;
        quiet["purgeableBytes"] = Value::Null;
        quiet["importantUsageBytes"] = Value::Null;
        quiet["opportunisticUsageBytes"] = Value::Null;
        for k in ["scanId", "scanRootPath", "scannedBytes", "unscannedBytes", "otherVolumesBytes", "unreadableCount", "snapshotSuggestion"] {
            quiet["hidden"][k] = Value::Null;
        }
        quiet["hidden"]["otherUserHomes"] = json!([]);
        assert_covers(&tool("get_volume_status")["outputSchema"], &quiet, "volume/not-asked-no-scan");
        for name in ["explain_path", "find_stale_projects", "get_volume_status"] {
            assert_eq!(tool(name)["annotations"]["readOnlyHint"], true, "{name}");
            assert_eq!(tool(name)["annotations"]["idempotentHint"], true, "{name}");
        }
        assert_eq!(tool("explain_path")["inputSchema"]["required"], json!(["path"]));
        assert_eq!(TOOL_NAMES[0], "get_volume_status", "the anchor comes first in tools/list");
    }

    #[test]
    fn growth_schema_covers_the_fixture_and_its_null_forecast() {
        assert_covers(&tool("get_growth")["outputSchema"], &parse(RAW_GROWTH), "growth");
        let mut one = parse(RAW_GROWTH);
        one["forecast"] = Value::Null;
        one["points"] = json!([one["points"][2].clone()]);
        one["series"] = json!([{ "key": "total", "values": [null] }]);
        one["groupBy"] = json!("total");
        assert_covers(&tool("get_growth")["outputSchema"], &one, "growth/one-point");
        let mut shrinking = parse(RAW_GROWTH);
        shrinking["forecast"]["bytesPerDay"] = json!(-5);
        shrinking["forecast"]["daysUntilFull"] = Value::Null;
        shrinking["forecast"]["projectedFullAt"] = Value::Null;
        shrinking["forecast"]["availableBytes"] = Value::Null;
        assert_covers(&tool("get_growth")["outputSchema"], &shrinking, "growth/shrinking");
        assert_eq!(tool("get_growth")["annotations"]["readOnlyHint"], true);
        assert_eq!(tool("get_growth")["annotations"]["idempotentHint"], true);
        assert_eq!(tool("get_growth")["inputSchema"]["required"], json!([]));
        assert_eq!(TOOL_NAMES[TOOL_NAMES.len() - 2], "get_growth", "after diff_scans, before health");
    }

    #[test]
    fn plan_and_verification_schemas_cover_their_fixtures() {
        assert_covers(&tool("plan_reclaim")["outputSchema"], &parse(RAW_PLAN), "plan");
        let mut with_script = parse(RAW_PLAN);
        with_script["script"] = json!("#!/bin/sh\n");
        assert_covers(&tool("plan_reclaim")["outputSchema"], &with_script, "plan+script");
        assert_covers(&tool("verify_reclaim")["outputSchema"], &parse(RAW_VERIFICATION), "verification");
        assert_eq!(tool("verify_reclaim")["inputSchema"]["required"], json!(["planId"]));
        assert_eq!(tool("plan_reclaim")["inputSchema"]["properties"]["maxTier"]["enum"], json!(["safe", "caution"]), "review is not offered");
    }

    #[test]
    fn scan_status_and_cancel_scan_require_the_id_and_return_a_scan_view() {
        for name in ["scan_status", "cancel_scan"] {
            let t = tool(name);
            assert_eq!(t["inputSchema"]["required"], json!(["scanId"]), "{name}: scanId is required (no latest-completed default here)");
            assert_eq!(t["outputSchema"], scan_schema(), "{name} answers with the scan view");
        }
        assert!(tool("cancel_scan")["inputSchema"]["properties"].get("responseFormat").is_none());
    }

    #[test]
    fn budget_meta_is_on_exactly_the_growing_tools() {
        for t in tools() {
            let name = t["name"].as_str().unwrap();
            let budgeted = BUDGETED_TOOLS.contains(&name);
            let meta = t.get("_meta").and_then(|m| m.get(MAX_RESULT_SIZE_META_KEY));
            match (budgeted, meta) {
                (true, Some(v)) => assert_eq!(v.as_u64(), Some(MAX_RESULT_CHARS as u64), "{name}"),
                (true, None) => panic!("{name}: budgeted tool lacks _meta[{MAX_RESULT_SIZE_META_KEY}]"),
                (false, Some(_)) => panic!("{name}: unexpected result-size meta"),
                (false, None) => {}
            }
        }
    }

    #[test]
    fn concise_tools_declare_response_format_and_the_rest_do_not() {
        for t in tools() {
            let name = t["name"].as_str().unwrap();
            let has = t["inputSchema"]["properties"].get("responseFormat").is_some();
            assert_eq!(has, CONCISE_TOOLS.contains(&name), "{name}: responseFormat property");
            if has {
                assert_eq!(
                    t["inputSchema"]["properties"]["responseFormat"]["enum"],
                    json!(["concise", "detailed"]),
                    "{name}"
                );
            }
        }
    }

    // --- outputSchema vs the shared fixtures (raw bytes) --------------------

    #[test]
    fn scan_schema_covers_both_scan_fixtures() {
        for raw in [RAW_SCAN_COMPLETE, RAW_SCAN_RUNNING, RAW_SCAN_INTERRUPTED] {
            for name in ["scan_directory", "scan_status", "cancel_scan"] {
                assert_covers(&tool(name)["outputSchema"], &parse(raw), name);
            }
        }
        let wrapped = structured_content("list_scans", &json!([parse(RAW_SCAN_COMPLETE), parse(RAW_SCAN_RUNNING)]));
        assert_covers(&tool("list_scans")["outputSchema"], &wrapped, "list_scans");
    }

    #[test]
    fn files_schema_covers_the_entry_fixtures() {
        let page = json!({ "files": [parse(RAW_ENTRY), parse(RAW_ENTRY_DIR)], "nextCursor": null });
        assert_covers(&tool("find_large_files")["outputSchema"], &page, "files");
        let more = json!({ "files": [parse(RAW_ENTRY)], "nextCursor": "MTAw" });
        assert_covers(&tool("find_large_files")["outputSchema"], &more, "files");
    }

    #[test]
    fn types_schema_covers_the_fixture_once_wrapped() {
        let wrapped = structured_content("get_space_by_type", &parse(RAW_TYPES));
        assert_eq!(wrapped["types"], parse(RAW_TYPES), "wrap must not alter the array");
        assert_covers(&tool("get_space_by_type")["outputSchema"], &wrapped, "types");
    }

    #[test]
    fn treemap_schema_covers_the_fixture() {
        assert_covers(&tool("get_treemap")["outputSchema"], &parse(RAW_TREEMAP), "treemap");
    }

    #[test]
    fn hotspots_schema_covers_the_fixture() {
        assert_covers(&tool("get_hotspots")["outputSchema"], &parse(RAW_HOTSPOTS), "hotspots");
    }

    #[test]
    fn diff_schema_covers_the_fixture() {
        assert_covers(&tool("diff_scans")["outputSchema"], &parse(RAW_DIFF), "diff");
    }

    #[test]
    fn health_schema_covers_both_answers() {
        for body in [json!({"status": "ok", "version": "1.1.0"}), json!({"status": "degraded", "version": "1.1.0"})] {
            assert_covers(&tool("health")["outputSchema"], &body, "health");
        }
    }

    #[test]
    fn structured_content_only_wraps_the_bare_array_tools() {
        let obj = json!({ "a": 1 });
        for name in TOOL_NAMES {
            let s = structured_content(name, &obj);
            match name {
                "list_scans" => assert_eq!(s, json!({ "scans": obj })),
                "get_space_by_type" => assert_eq!(s, json!({ "types": obj })),
                _ => assert_eq!(s, obj, "{name} must pass its body through"),
            }
        }
    }

    // --- version negotiation -------------------------------------------------

    #[test]
    fn negotiation_echoes_a_supported_version_and_falls_back_to_latest() {
        for v in SUPPORTED_PROTOCOL_VERSIONS {
            assert_eq!(negotiate_protocol_version(Some(v)), *v);
        }
        assert_eq!(negotiate_protocol_version(Some("2099-01-01")), LATEST_PROTOCOL_VERSION);
        assert_eq!(negotiate_protocol_version(Some("")), LATEST_PROTOCOL_VERSION);
        assert_eq!(negotiate_protocol_version(None), LATEST_PROTOCOL_VERSION);
        assert_eq!(SUPPORTED_PROTOCOL_VERSIONS[0], LATEST_PROTOCOL_VERSION, "latest must lead the list");
    }
}
