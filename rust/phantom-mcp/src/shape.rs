// Result shaping: the `responseFormat` projection and the result-size
// budget. Both exist for token economics — an agent reading a home-directory
// treemap or a 500-row file page pays for every geometry float and every
// inode number whether it needs them or not.
//
// Rules that keep the e2e parity gate honest:
// - `detailed` (the default) is the HTTP body VERBATIM. Concise is opt-in.
// - The budget only ever bites a result that is already over
//   MAX_RESULT_CHARS; anything under passes through untouched. So every
//   fixture-sized answer (and everything the harness compares) is exact.
// - When the budget bites, the message says what call gets the rest. A
//   truncated answer that does not name the next move is a dead end.

use serde_json::{Map, Value, json};

use crate::protocol::MAX_RESULT_CHARS;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseFormat {
    Concise,
    Detailed,
}

impl ResponseFormat {
    /// Read `responseFormat` from the tool arguments. Absent = detailed
    /// (the verbatim body). A wrong value is an error naming the two valid
    /// ones rather than a silent fallback — an agent that typed "brief"
    /// meant something and should learn the word.
    pub fn from_args(args: &Value) -> Result<Self, String> {
        match &args["responseFormat"] {
            Value::Null => Ok(Self::Detailed),
            Value::String(s) if s == "concise" => Ok(Self::Concise),
            Value::String(s) if s == "detailed" => Ok(Self::Detailed),
            other => Err(format!(
                "responseFormat must be \"concise\" or \"detailed\" (got {other})"
            )),
        }
    }
}

/// Keep only `keys` of an object, in the object's own order. Non-objects
/// pass through (a projection never invents structure).
fn keep(value: &Value, keys: &[&str]) -> Value {
    match value.as_object() {
        Some(map) => Value::Object(
            map.iter()
                .filter(|(k, _)| keys.contains(&k.as_str()))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect::<Map<_, _>>(),
        ),
        None => value.clone(),
    }
}

fn keep_each(array: &Value, keys: &[&str]) -> Value {
    match array.as_array() {
        Some(items) => Value::Array(items.iter().map(|v| keep(v, keys)).collect()),
        None => array.clone(),
    }
}

/// The fields concise keeps per shape. Sizes an agent acts on (disk,
/// private) always survive; geometry, inode plumbing and logical sizes go.
pub const SCAN_CONCISE_KEYS: &[&str] = &[
    "id", "rootPath", "status", "startedAt", "finishedAt", "totalDiskSize",
    "totalPrivateSize", "fileCount", "errorCount", "progress", "note",
];
pub const ENTRY_CONCISE_KEYS: &[&str] = &["path", "diskSize", "privateSize", "fileType"];
pub const RECT_CONCISE_KEYS: &[&str] = &["path", "size", "depth", "isDir"];
pub const GROUP_CONCISE_KEYS: &[&str] = &[
    "ruleId", "label", "category", "riskTier", "why", "command", "diskSize", "privateSize",
    "topPaths",
];

/// Apply the requested format to a tool's result. Detailed is identity.
pub fn project(tool: &str, format: ResponseFormat, result: Value) -> Value {
    if format == ResponseFormat::Detailed {
        return result;
    }
    match tool {
        "scan_directory" | "scan_status" => keep(&result, SCAN_CONCISE_KEYS),
        "list_scans" => keep_each(&result, SCAN_CONCISE_KEYS),
        "find_large_files" => {
            let mut out = result.clone();
            out["files"] = keep_each(&result["files"], ENTRY_CONCISE_KEYS);
            out
        }
        "get_treemap" => {
            let mut out = result.clone();
            out["rects"] = keep_each(&result["rects"], RECT_CONCISE_KEYS);
            out
        }
        "get_hotspots" => {
            let mut out = result.clone();
            out["groups"] = keep_each(&result["groups"], GROUP_CONCISE_KEYS);
            out
        }
        _ => result,
    }
}

/// The text an agent receives: pretty-printed JSON, the same rendering the
/// v1.0 server used, so the parity gate's byte comparison still holds.
pub fn render(value: &Value) -> String {
    serde_json::to_string_pretty(value).expect("JSON values always render")
}

/// The treemap's default depth when the caller did not pass `maxDepth`
/// (the API's DEFAULT_TREEMAP_DEPTH); named in the truncation note.
const DEFAULT_TREEMAP_DEPTH: u64 = 4;

/// Enforce the result budget. Returns the (possibly reduced) value and its
/// rendering; the rendering is returned so callers never serialize twice.
///
/// - `get_treemap`: serve the deepest depth that fits and say so in a
///   `truncated` field (the layout at depth 1 is still the answer to
///   "where is the space"; the agent re-roots for the rest).
/// - `find_large_files`: a page is one unit — refuse it with a message
///   naming a `limit` that fits and pointing at concise.
/// - anything else passes through; their payloads are bounded by design.
pub fn enforce_budget(
    tool: &str,
    args: &Value,
    value: Value,
    budget: usize,
) -> Result<(Value, String), String> {
    let text = render(&value);
    if text.len() <= budget {
        return Ok((value, text));
    }
    match tool {
        "get_treemap" => Ok(prune_treemap(args, value, budget)),
        "find_large_files" => Err(files_over_budget(&value, text.len(), budget)),
        _ => Ok((value, text)),
    }
}

/// Drop rects deeper than `d` for the largest `d` whose rendering fits.
/// Depth 0 (the root rect alone) always fits in any sane budget; if even
/// that does not, the caller gets depth 0 anyway — over budget but honest.
fn prune_treemap(args: &Value, mut layout: Value, budget: usize) -> (Value, String) {
    let requested = args["maxDepth"]
        .as_u64()
        .or_else(|| args["maxDepth"].as_str().and_then(|s| s.parse().ok()))
        .unwrap_or(DEFAULT_TREEMAP_DEPTH);
    let all_rects = layout["rects"].as_array().cloned().unwrap_or_default();
    let deepest = all_rects
        .iter()
        .filter_map(|r| r["depth"].as_u64())
        .max()
        .unwrap_or(0);
    let biggest_child = all_rects
        .iter()
        .filter(|r| r["depth"] == 1 && r["isDir"] == true && r["residual"] != true)
        .max_by_key(|r| r["size"].as_u64().unwrap_or(0))
        .and_then(|r| r["path"].as_str())
        .map(str::to_string);

    let mut served = deepest.saturating_sub(1);
    loop {
        let kept: Vec<Value> = all_rects
            .iter()
            .filter(|r| r["depth"].as_u64().unwrap_or(0) <= served)
            .cloned()
            .collect();
        layout["rects"] = Value::Array(kept);
        layout["truncated"] = json!({
            "requestedDepth": requested,
            "servedDepth": served,
            "note": truncation_note(served, biggest_child.as_deref()),
        });
        let text = render(&layout);
        if text.len() <= budget || served == 0 {
            return (layout, text);
        }
        served -= 1;
    }
}

fn truncation_note(served: u64, biggest_child: Option<&str>) -> String {
    let reroot = match biggest_child {
        Some(p) => format!(" or re-root at the biggest child with root: {p:?}"),
        None => String::new(),
    };
    format!(
        "layout exceeded the {MAX_RESULT_CHARS}-character result budget; served to depth {served}. \
         For more detail call again with a smaller maxDepth{reroot}; \
         responseFormat: \"concise\" drops the geometry and fits several levels deeper."
    )
}

fn files_over_budget(page: &Value, chars: usize, budget: usize) -> String {
    let rows = page["files"].as_array().map(Vec::len).unwrap_or(0).max(1);
    let per_row = chars / rows;
    // Leave headroom for the envelope, never suggest zero.
    let fits = (budget.saturating_sub(200) / per_row.max(1)).max(1);
    format!(
        "this page of {rows} files is {chars} characters, over the {budget}-character result \
         budget. Call again with limit: {fits} (page with cursor for the rest), or add \
         responseFormat: \"concise\" to keep only path, diskSize, privateSize and fileType."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const RAW_ENTRY: &str = include_str!("../../../tests/fixtures/entry.json");
    const RAW_TREEMAP: &str = include_str!("../../../tests/fixtures/treemap.json");
    const RAW_HOTSPOTS: &str = include_str!("../../../tests/fixtures/hotspots-summary.json");
    const RAW_SCAN: &str = include_str!("../../../tests/fixtures/scan-complete.json");

    fn parse(raw: &str) -> Value {
        serde_json::from_str(raw).unwrap()
    }

    /// Sorted key list — serde_json's Map is a BTreeMap here, and the
    /// contract is WHICH keys survive, not their order.
    fn keys(v: &Value) -> Vec<String> {
        let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
        k.sort();
        k
    }

    fn sorted(keys: &[&str]) -> Vec<String> {
        let mut k: Vec<String> = keys.iter().map(|s| s.to_string()).collect();
        k.sort();
        k
    }

    #[test]
    fn response_format_defaults_to_detailed_and_rejects_typos() {
        assert_eq!(ResponseFormat::from_args(&json!({})), Ok(ResponseFormat::Detailed));
        assert_eq!(ResponseFormat::from_args(&json!({"responseFormat": "detailed"})), Ok(ResponseFormat::Detailed));
        assert_eq!(ResponseFormat::from_args(&json!({"responseFormat": "concise"})), Ok(ResponseFormat::Concise));
        let err = ResponseFormat::from_args(&json!({"responseFormat": "brief"})).unwrap_err();
        assert!(err.contains("concise") && err.contains("detailed") && err.contains("brief"), "{err}");
        assert!(ResponseFormat::from_args(&json!({"responseFormat": 1})).is_err());
    }

    #[test]
    fn detailed_is_identity_for_every_tool() {
        for tool in crate::protocol::TOOL_NAMES {
            let v = parse(RAW_HOTSPOTS);
            assert_eq!(project(tool, ResponseFormat::Detailed, v.clone()), v, "{tool}");
        }
    }

    #[test]
    fn concise_files_keep_exactly_the_acting_fields() {
        let page = json!({ "files": [parse(RAW_ENTRY)], "nextCursor": "abc" });
        let out = project("find_large_files", ResponseFormat::Concise, page);
        assert_eq!(out["nextCursor"], "abc", "the cursor must survive — it is how paging continues");
        assert_eq!(keys(&out["files"][0]), sorted(&["path", "diskSize", "privateSize", "fileType"]));
        assert_eq!(out["files"][0]["privateSize"], 0, "the clone's privateSize is the point of the row");
    }

    #[test]
    fn concise_treemap_drops_geometry_but_keeps_the_hierarchy() {
        let out = project("get_treemap", ResponseFormat::Concise, parse(RAW_TREEMAP));
        assert_eq!(out["rootPath"], "/Users/ghost/Code");
        assert_eq!(out["totalSize"], 4194304);
        let rects = out["rects"].as_array().unwrap();
        assert_eq!(rects.len(), 4, "every rect survives, including the residual");
        for r in rects {
            assert_eq!(keys(r), sorted(&["path", "size", "depth", "isDir"]));
        }
    }

    #[test]
    fn concise_hotspots_keep_tier_why_command_and_sizes() {
        let out = project("get_hotspots", ResponseFormat::Concise, parse(RAW_HOTSPOTS));
        assert_eq!(out["reclaimEstimate"], 20401094656u64, "totals survive");
        let g = &out["groups"][0];
        assert_eq!(
            keys(g),
            sorted(&["ruleId", "label", "category", "riskTier", "why", "command", "diskSize", "privateSize", "topPaths"])
        );
        assert_eq!(g["riskTier"], "safe");
        assert!(g.get("rebuildCost").is_none() && g.get("hint").is_none());
    }

    #[test]
    fn concise_scan_keeps_the_headline_numbers_and_progress() {
        let scan = project("scan_directory", ResponseFormat::Concise, parse(RAW_SCAN));
        // Every concise key except `note`, which only a capped wait adds.
        assert_eq!(
            keys(&scan),
            sorted(&["id", "rootPath", "status", "startedAt", "finishedAt", "totalDiskSize", "totalPrivateSize", "fileCount", "errorCount", "progress"])
        );
        let list = project("list_scans", ResponseFormat::Concise, json!([parse(RAW_SCAN)]));
        assert_eq!(list[0], scan, "list_scans projects each element the same way");
    }

    #[test]
    fn budget_passes_small_results_through_unchanged() {
        let v = parse(RAW_TREEMAP);
        let (out, text) = enforce_budget("get_treemap", &json!({}), v.clone(), MAX_RESULT_CHARS).unwrap();
        assert_eq!(out, v);
        assert_eq!(text, render(&v));
        assert!(out.get("truncated").is_none());
    }

    /// A synthetic four-level layout: one root, N dirs at depth 1, each
    /// with children at depth 2 and 3 — enough text to blow a small budget.
    fn wide_layout(dirs: usize) -> Value {
        let mut rects = vec![json!({
            "path": "/r", "name": "r", "size": dirs * 300, "x": 0.0, "y": 0.0,
            "width": 800.0, "height": 600.0, "depth": 0, "isDir": true, "fileType": null, "residual": false
        })];
        for i in 0..dirs {
            let size = 300 + i; // the LAST dir is the biggest
            rects.push(json!({
                "path": format!("/r/d{i}"), "name": format!("d{i}"), "size": size, "x": 0.0, "y": 0.0,
                "width": 10.0, "height": 10.0, "depth": 1, "isDir": true, "fileType": null, "residual": false
            }));
            for j in 0..3 {
                rects.push(json!({
                    "path": format!("/r/d{i}/f{j}.bin"), "name": format!("f{j}.bin"), "size": 100, "x": 0.0, "y": 0.0,
                    "width": 1.0, "height": 1.0, "depth": 2, "isDir": false, "fileType": "bin", "residual": false
                }));
                rects.push(json!({
                    "path": format!("/r/d{i}/sub/g{j}.bin"), "name": format!("g{j}.bin"), "size": 1, "x": 0.0, "y": 0.0,
                    "width": 1.0, "height": 1.0, "depth": 3, "isDir": false, "fileType": "bin", "residual": false
                }));
            }
        }
        json!({ "rootPath": "/r", "totalSize": dirs * 300, "rects": rects })
    }

    /// The rendering of `layout` pruned to depth <= d, carrying the same
    /// `truncated` field the server would add — so a budget set to exactly
    /// this length admits depth d and nothing deeper.
    fn rendered_at(layout: &Value, d: u64, requested: u64) -> String {
        let mut l = layout.clone();
        l["rects"] = Value::Array(
            layout["rects"].as_array().unwrap().iter()
                .filter(|r| r["depth"].as_u64().unwrap() <= d).cloned().collect(),
        );
        l["truncated"] = json!({
            "requestedDepth": requested, "servedDepth": d,
            "note": truncation_note(d, Some("/r/d19")),
        });
        render(&l)
    }

    #[test]
    fn treemap_over_budget_is_served_to_the_deepest_depth_that_fits() {
        let layout = wide_layout(20);
        // Depth 2 fits exactly; depth 3 (the full layout) does not.
        let budget = rendered_at(&layout, 2, 3).len();
        assert!(render(&layout).len() > budget);
        let (out, text) = enforce_budget("get_treemap", &json!({"maxDepth": 3}), layout, budget).unwrap();
        assert!(text.len() <= budget, "served {} chars over the {budget} budget", text.len());
        assert_eq!(out["truncated"]["servedDepth"], 2, "must serve the DEEPEST depth that fits, not merely a fit");
        assert_eq!(out["truncated"]["requestedDepth"], 3);
        let rects = out["rects"].as_array().unwrap();
        assert!(rects.iter().all(|r| r["depth"].as_u64().unwrap() <= 2));
        assert!(rects.iter().any(|r| r["depth"] == 2), "served depth must actually be present");
        let note = out["truncated"]["note"].as_str().unwrap();
        assert!(note.contains("maxDepth") && note.contains("concise"), "{note}");
        assert!(note.contains("\"/r/d19\""), "names the biggest child to re-root at: {note}");
    }

    #[test]
    fn treemap_truncation_defaults_the_requested_depth_to_the_api_default() {
        let layout = wide_layout(20);
        let budget = rendered_at(&layout, 1, 4).len();
        let (out, text) = enforce_budget("get_treemap", &json!({}), layout, budget).unwrap();
        assert_eq!(out["truncated"]["requestedDepth"], DEFAULT_TREEMAP_DEPTH);
        // Depth 2 does NOT fit this budget, so the loop must keep pruning
        // past its first candidate and actually check the rendering.
        assert_eq!(out["truncated"]["servedDepth"], 1);
        assert!(text.len() <= budget);
        // maxDepth may arrive as a string (agents send numbers both ways).
        let (out, _) = enforce_budget("get_treemap", &json!({"maxDepth": "2"}), wide_layout(20), budget).unwrap();
        assert_eq!(out["truncated"]["requestedDepth"], 2);
    }

    #[test]
    fn files_over_budget_is_refused_with_a_limit_that_fits() {
        let rows: Vec<Value> = (0..50).map(|_| parse(RAW_ENTRY)).collect();
        let page = json!({ "files": rows, "nextCursor": null });
        let full = render(&page).len();
        let budget = full / 2;
        let err = enforce_budget("find_large_files", &json!({}), page.clone(), budget).unwrap_err();
        assert!(err.contains("limit: "), "{err}");
        assert!(err.contains("concise"), "{err}");
        let suggested: usize = err.split("limit: ").nth(1).unwrap().split(' ').next().unwrap().parse().unwrap();
        assert!((1..50).contains(&suggested), "suggested {suggested}");
        // The suggested page really fits.
        let smaller = json!({ "files": page["files"].as_array().unwrap()[..suggested].to_vec(), "nextCursor": "x" });
        assert!(render(&smaller).len() <= budget, "suggested limit does not fit");
    }

    #[test]
    fn other_tools_are_never_truncated() {
        let big = json!({ "groups": (0..2000).map(|i| json!({"ruleId": format!("r{i}")})).collect::<Vec<_>>() });
        let (out, _) = enforce_budget("get_hotspots", &json!({}), big.clone(), 100).unwrap();
        assert_eq!(out, big);
    }
}
