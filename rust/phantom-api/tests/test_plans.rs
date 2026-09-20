// Integration tests for reclaim plans (v1.1 Phase 3, phantom-mkn.7): the
// whole loop over real HTTP on a temp database — scan a tree with a safe
// hotspot, build the plan, read it back and as a script, "run" it (the test
// removes the artifact the way the script would move it), rescan, verify —
// plus every refusal (review tier, running scan, wrong root, stale rescan).

use std::path::{Path, PathBuf};

use phantom_api::{AppState, build_router};
use phantom_core::ScanStore;

const KEY: &str = "test-key-not-secret";
const MIB: usize = 1024 * 1024;

struct TestServer {
    base: String,
    client: reqwest::Client,
    _dir: tempfile::TempDir,
}

impl TestServer {
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("phantom.db");
        let state = AppState::new(ScanStore::open(&db).unwrap(), KEY.into());
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(listener, build_router(state)).await.unwrap();
        });
        Self {
            base,
            client: reqwest::Client::new(),
            _dir: dir,
        }
    }

    fn workdir(&self) -> &Path {
        self._dir.path()
    }

    async fn post(&self, path: &str, body: serde_json::Value) -> reqwest::Response {
        self.client
            .post(format!("{}{path}", self.base))
            .header("x-api-key", KEY)
            .json(&body)
            .send()
            .await
            .unwrap()
    }

    async fn get(&self, path: &str) -> reqwest::Response {
        self.client
            .get(format!("{}{path}", self.base))
            .header("x-api-key", KEY)
            .send()
            .await
            .unwrap()
    }

    /// Scan `root` and poll to completion; returns the terminal view.
    async fn scan(&self, root: &Path) -> serde_json::Value {
        let resp = self
            .post("/scans", serde_json::json!({ "rootPath": root.to_str().unwrap() }))
            .await;
        assert_eq!(resp.status(), 202);
        let v: serde_json::Value = resp.json().await.unwrap();
        let id = v["id"].as_str().unwrap().to_string();
        for _ in 0..600 {
            let v: serde_json::Value = self.get(&format!("/scans/{id}")).await.json().await.unwrap();
            if v["status"] != "running" {
                assert_eq!(v["status"], "complete", "{v}");
                return v;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("scan {id} never finished");
    }
}

/// A dormant-looking Cargo project with a 2 MiB build artifact: Cargo.toml
/// (detection file) + Cargo.lock (lockfile present → tier safe) + target/.
fn cargo_project(under: &Path) -> PathBuf {
    let proj = under.join("planroot").join("proj");
    std::fs::create_dir_all(proj.join("src")).unwrap();
    std::fs::create_dir_all(proj.join("target").join("debug")).unwrap();
    std::fs::write(proj.join("Cargo.toml"), "[package]\nname = \"p\"\nversion = \"0.1.0\"\n").unwrap();
    std::fs::write(proj.join("Cargo.lock"), "# lock\n").unwrap();
    std::fs::write(proj.join("src").join("main.rs"), "fn main() {}\n").unwrap();
    std::fs::write(proj.join("target").join("debug").join("blob.bin"), vec![7u8; 2 * MIB]).unwrap();
    proj.parent().unwrap().to_path_buf()
}

#[tokio::test]
async fn plan_promises_only_its_listed_paths_and_applies_the_size_floor_to_them() {
    let s = TestServer::start().await;
    let root = s.workdir().join("many-projects");
    for i in 0..7 {
        cargo_project(&root.join(format!("project-{i}")));
    }
    let scan = s.scan(&root).await;
    let scan_id = scan["id"].as_str().unwrap();
    let resp = s
        .post(&format!("/scans/{scan_id}/plan"), serde_json::json!({}))
        .await;
    assert_eq!(resp.status(), 201);
    let plan: serde_json::Value = resp.json().await.unwrap();
    let items = plan["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{plan}");
    let item = &items[0];
    assert_eq!(item["paths"].as_array().unwrap().len(), 5, "{item}");
    // Seven independent 2 MiB artifacts were scanned, but the script will
    // move only five. Its promise must exclude the two unlisted artifacts.
    let expected = 10 * MIB as u64;
    assert_eq!(item["expectedFreedBytes"], expected, "{plan}");
    assert_eq!(plan["expectedFreedBytes"], expected, "{plan}");

    let resp = s
        .post(
            &format!("/scans/{scan_id}/plan"),
            serde_json::json!({ "minBytes": expected + 1 }),
        )
        .await;
    assert_eq!(resp.status(), 201);
    let empty: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(empty["itemCount"], 0, "{empty}");
    assert_eq!(empty["skipped"]["belowMinBytes"], 1, "{empty}");
    assert_eq!(empty["expectedFreedBytes"], 0, "{empty}");

    // Leave just one of the listed paths ignored in a newly-created work
    // tree. The same saved scan must now promise only that path's bytes.
    let kept = item["paths"][0].as_str().unwrap();
    let relative = Path::new(kept).strip_prefix(&root).unwrap();
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::write(
        root.join(".gitignore"),
        format!("/{}\n", relative.display()),
    )
    .unwrap();
    let resp = s
        .post(&format!("/scans/{scan_id}/plan"), serde_json::json!({}))
        .await;
    assert_eq!(resp.status(), 201);
    let filtered: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        filtered["items"][0]["paths"],
        serde_json::json!([kept]),
        "{filtered}"
    );
    assert_eq!(filtered["expectedFreedBytes"], 2 * MIB as u64, "{filtered}");
}

#[tokio::test]
async fn plan_uses_persisted_private_bytes_and_refuses_missing_measurements() {
    let s = TestServer::start().await;
    let root = cargo_project(s.workdir());
    let scanned = s.scan(&root).await;
    let id = uuid::Uuid::parse_str(scanned["id"].as_str().unwrap()).unwrap();
    let store = ScanStore::open(&s.workdir().join("phantom.db")).unwrap();
    let scan = store.get_scan(id).unwrap();
    let summary = store.hotspots(id).unwrap().unwrap();
    let totals = store.file_type_totals(id).unwrap();
    let mut entries = store.entries(id).unwrap();
    let target = root.join("proj/target").to_str().unwrap().to_string();
    let target_index = entries.iter().position(|e| e.path == target).unwrap();

    // A saved clone-aware measurement can be much smaller than the
    // allocation or the group's total. Read that measurement verbatim.
    entries[target_index].private_size = Some(256);
    store
        .insert_scan(&scan, &entries, &totals, Some(&summary))
        .unwrap();
    let resp = s
        .post(&format!("/scans/{id}/plan"), serde_json::json!({}))
        .await;
    assert_eq!(resp.status(), 201);
    let plan: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(plan["expectedFreedBytes"], 256, "{plan}");

    // Legacy scans without private-byte measurements need a rescan; zero
    // or the whole group's size would both be invented estimates.
    entries[target_index].private_size = None;
    store
        .insert_scan(&scan, &entries, &totals, Some(&summary))
        .unwrap();
    let resp = s
        .post(&format!("/scans/{id}/plan"), serde_json::json!({}))
        .await;
    assert_eq!(resp.status(), 400);
    let error: serde_json::Value = resp.json().await.unwrap();
    assert!(
        error["error"]
            .as_str()
            .unwrap()
            .contains("run a fresh scan"),
        "{error}"
    );

    entries.remove(target_index);
    store
        .insert_scan(&scan, &entries, &totals, Some(&summary))
        .unwrap();
    let resp = s
        .post(&format!("/scans/{id}/plan"), serde_json::json!({}))
        .await;
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn plan_build_read_script_and_refusals() {
    let s = TestServer::start().await;
    let root = cargo_project(s.workdir());
    let scan = s.scan(&root).await;
    let scan_id = scan["id"].as_str().unwrap();

    // Default plan: safe only.
    let resp = s.post(&format!("/scans/{scan_id}/plan"), serde_json::json!({})).await;
    assert_eq!(resp.status(), 201);
    let plan: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(plan["scanId"], scan_id);
    assert_eq!(plan["maxTier"], "safe");
    assert_eq!(plan["itemCount"], 1, "{plan}");
    let item = &plan["items"][0];
    assert_eq!(item["ruleId"], "cargo-target");
    assert_eq!(item["riskTier"], "safe", "Cargo.lock present → safe: {item}");
    assert_eq!(item["command"], "cargo clean");
    let expected = item["expectedFreedBytes"].as_u64().unwrap();
    assert!(expected >= 2 * MIB as u64, "{expected}");
    assert_eq!(plan["expectedFreedBytes"], expected);
    assert_eq!(item["paths"][0].as_str().unwrap(), root.join("proj").join("target").to_str().unwrap());
    let plan_id = plan["planId"].as_str().unwrap();

    // Read back byte-equal; unknown plan is a 404.
    let again: serde_json::Value = s.get(&format!("/plans/{plan_id}")).await.json().await.unwrap();
    assert_eq!(again, plan);
    assert_eq!(s.get("/plans/e7ae86e2-308b-444c-8a3d-cd21467ab442").await.status(), 404);

    // The script: text/plain, quotes the path, never deletes.
    let resp = s.get(&format!("/plans/{plan_id}/script")).await;
    assert_eq!(resp.status(), 200);
    assert!(resp.headers()["content-type"].to_str().unwrap().starts_with("text/plain"));
    let script = resp.text().await.unwrap();
    assert!(script.starts_with("#!/bin/sh\n"), "{script}");
    assert!(script.contains(&format!("apply '{}'", root.join("proj").join("target").display())));
    assert!(script.contains("PHANTOM_APPLY"));
    assert!(!script.contains("rm "), "the script must never delete");

    // Refusals.
    let resp = s.post(&format!("/scans/{scan_id}/plan"), serde_json::json!({ "maxTier": "review" })).await;
    assert_eq!(resp.status(), 400);
    let v: serde_json::Value = resp.json().await.unwrap();
    assert!(v["error"].as_str().unwrap().contains("never plan items"), "{v}");
    let resp = s.post(&format!("/scans/{scan_id}/plan"), serde_json::json!({ "max_tier": "safe" })).await;
    assert!(resp.status().is_client_error(), "snake_case key must be refused, got {}", resp.status());
    let resp = s.post("/scans/e7ae86e2-308b-444c-8a3d-cd21467ab442/plan", serde_json::json!({})).await;
    assert_eq!(resp.status(), 404);

    // minBytes above the artifact → an honest empty plan with the skip counted.
    let resp = s
        .post(&format!("/scans/{scan_id}/plan"), serde_json::json!({ "minBytes": 100 * MIB }))
        .await;
    assert_eq!(resp.status(), 201);
    let empty: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(empty["itemCount"], 0);
    assert_eq!(empty["skipped"]["belowMinBytes"], 1, "{empty}");
    assert_eq!(empty["expectedFreedBytes"], 0);
}

#[tokio::test]
async fn verify_measures_the_freed_bytes_and_refuses_the_wrong_rescan() {
    let s = TestServer::start().await;
    let root = cargo_project(s.workdir());
    let before = s.scan(&root).await;
    let before_id = before["id"].as_str().unwrap();
    let plan: serde_json::Value = s
        .post(&format!("/scans/{before_id}/plan"), serde_json::json!({}))
        .await
        .json()
        .await
        .unwrap();
    let plan_id = plan["planId"].as_str().unwrap();
    let expected = plan["expectedFreedBytes"].as_u64().unwrap();

    // A rescan that PREDATES the plan cannot verify it — mutation-proof:
    // drop the started_at check and this returns 200.
    let resp = s
        .post(&format!("/plans/{plan_id}/verify"), serde_json::json!({ "afterScanId": before_id }))
        .await;
    assert_eq!(resp.status(), 400);
    let v: serde_json::Value = resp.json().await.unwrap();
    assert!(v["error"].as_str().unwrap().contains("before the plan"), "{v}");

    // "Run" the plan: what the script's PHANTOM_APPLY=1 does, minus the Trash.
    std::fs::remove_dir_all(root.join("proj").join("target")).unwrap();
    let after = s.scan(&root).await;
    let after_id = after["id"].as_str().unwrap();

    let resp = s
        .post(&format!("/plans/{plan_id}/verify"), serde_json::json!({ "afterScanId": after_id }))
        .await;
    assert_eq!(resp.status(), 200);
    let v: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(v["planId"], plan_id);
    assert_eq!(v["beforeScanId"], before_id);
    assert_eq!(v["afterScanId"], after_id);
    assert_eq!(v["expectedFreedBytes"], expected);
    let actual = v["actualFreedBytes"].as_i64().unwrap();
    assert!(actual > 0, "{v}");
    assert_eq!(v["withinTolerance"], true, "{v}");
    let item = &v["items"][0];
    assert_eq!(item["actualFreedBytes"].as_i64().unwrap(), expected as i64, "the whole target/ came back: {item}");
    assert_eq!(item["afterBytes"], 0);
    assert_eq!(item["beforeBytes"], expected);

    // Wrong root → 400; unknown plan → 404; unknown rescan → 404.
    let other = s.workdir().join("elsewhere");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(other.join("x.bin"), vec![1u8; MIB]).unwrap();
    let other_scan = s.scan(&other).await;
    let resp = s
        .post(
            &format!("/plans/{plan_id}/verify"),
            serde_json::json!({ "afterScanId": other_scan["id"] }),
        )
        .await;
    assert_eq!(resp.status(), 400);
    let v: serde_json::Value = resp.json().await.unwrap();
    assert!(v["error"].as_str().unwrap().contains("rescan the plan's root"), "{v}");
    let resp = s
        .post(
            "/plans/e7ae86e2-308b-444c-8a3d-cd21467ab442/verify",
            serde_json::json!({ "afterScanId": after_id }),
        )
        .await;
    assert_eq!(resp.status(), 404);
    let resp = s
        .post(
            &format!("/plans/{plan_id}/verify"),
            serde_json::json!({ "afterScanId": "e7ae86e2-308b-444c-8a3d-cd21467ab442" }),
        )
        .await;
    assert_eq!(resp.status(), 404);
    let resp = s
        .post(&format!("/plans/{plan_id}/verify"), serde_json::json!({ "after_scan_id": after_id }))
        .await;
    assert!(resp.status().is_client_error(), "snake_case key must be refused");
}

/// phantom-9wc, over HTTP: two plans off ONE scan have identical items, so
/// running one script and verifying the OTHER id used to fall back silently
/// to the root delta — which measures nothing once the bytes move to a Trash
/// folder inside the scanned root. That is how the 2026-09-16 gate run's raw
/// root delta read −9.3 GB after 57.9 GB was genuinely freed. The API must
/// refuse instead of answering.
///
/// Mutation-proof: delete the `root_missed_it`/`foreign_plan_trash` block in
/// `plans::verify_plan` and the final assertion flips from 409 to 200 with a
/// near-zero `actualFreedBytes`. Narrow the guard to drop the "our paths
/// shrank" clause and the leftover-folder case below breaks instead.
#[tokio::test]
async fn verifying_a_sibling_plan_is_refused_rather_than_answered_from_the_root_delta() {
    let s = TestServer::start().await;
    let root = cargo_project(s.workdir());
    let before = s.scan(&root).await;
    let before_id = before["id"].as_str().unwrap();

    // Two plans from the SAME scan: identical items, different ids — exactly
    // what `phantom plan --json` followed by `phantom plan --script` produces.
    let mut ids = Vec::new();
    for _ in 0..2 {
        let p: serde_json::Value = s
            .post(&format!("/scans/{before_id}/plan"), serde_json::json!({}))
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(p["itemCount"], 1, "{p}");
        ids.push((p["planId"].as_str().unwrap().to_string(), p["expectedFreedBytes"].as_u64().unwrap()));
    }
    let (plan_a, expected) = ids[0].clone();
    let (plan_b, _) = ids[1].clone();
    assert_ne!(plan_a, plan_b);

    // A leftover plan folder from an OLDER plan the user has not emptied yet,
    // with our artifact still in place. Nothing of ours shrank, so the honest
    // answer is "nothing came back" — a stale folder must not turn a working
    // verify into an error.
    let stale = root.join(".Trash").join("phantom-3f6b9c02-1d4e-4a77-9b3c-8e5f0a1d2c34");
    std::fs::create_dir_all(&stale).unwrap();
    std::fs::write(stale.join("junk.bin"), vec![3u8; MIB]).unwrap();
    let untouched = s.scan(&root).await;
    let resp = s
        .post(
            &format!("/plans/{plan_a}/verify"),
            serde_json::json!({ "afterScanId": untouched["id"] }),
        )
        .await;
    assert_eq!(resp.status(), 200, "a stale foreign folder alone must not refuse");
    let v: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(v["items"][0]["actualFreedBytes"].as_i64().unwrap(), 0, "{v}");

    // Now run plan B's script for real: the artifact moves into B's Trash
    // folder INSIDE the scanned root, so the bytes never leave the root.
    let b_trash = root.join(".Trash").join(format!("phantom-{plan_b}"));
    std::fs::create_dir_all(&b_trash).unwrap();
    std::fs::rename(root.join("proj").join("target"), b_trash.join("_proj_target")).unwrap();
    let after = s.scan(&root).await;
    let after_id = after["id"].as_str().unwrap();

    // B verifies normally, on the phantom-grw branch: its own folder is in the
    // rescan, so the headline is Σ per-item and not the confounded root delta.
    let resp = s
        .post(&format!("/plans/{plan_b}/verify"), serde_json::json!({ "afterScanId": after_id }))
        .await;
    assert_eq!(resp.status(), 200);
    let v: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(v["actualFreedBytes"].as_i64().unwrap(), expected as i64, "{v}");
    assert_eq!(v["withinTolerance"], true, "{v}");

    // A is the trap: same paths, same shrinkage, but its folder is not there.
    // The root barely changed (the bytes are still under it), so answering at
    // all would be a confident wrong number.
    let resp = s
        .post(&format!("/plans/{plan_a}/verify"), serde_json::json!({ "afterScanId": after_id }))
        .await;
    assert_eq!(resp.status(), 409, "sibling plan must be refused, not answered");
    let v: serde_json::Value = resp.json().await.unwrap();
    let err = v["error"].as_str().unwrap();
    assert!(err.contains("DIFFERENT plan's script"), "{err}");
    assert!(err.contains(&plan_a), "the error names the plan asked about: {err}");
    assert!(err.contains(".Trash/phantom-"), "and the folder that explains it: {err}");
}

/// phantom-2lz, over HTTP: the same artifact inside a git work tree is held
/// back until the tree's .gitignore ignores it. Dogfood 2026-09-09 moved 22
/// committed fixture directories; a plan built from a scan containing a
/// checkout must leave git data alone. Mutation-proof: have the API pass
/// `&|_| false` to build_plan and the first plan gains an item.
#[tokio::test]
async fn plan_holds_back_artifacts_a_git_work_tree_does_not_ignore() {
    let s = TestServer::start().await;
    let root = cargo_project(s.workdir());
    // Make planroot a work tree with no ignore rules at all.
    std::fs::create_dir_all(root.join(".git")).unwrap();
    let scan = s.scan(&root).await;
    let scan_id = scan["id"].as_str().unwrap();

    let plan: serde_json::Value = s
        .post(&format!("/scans/{scan_id}/plan"), serde_json::json!({}))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(plan["itemCount"], 0, "an un-ignored target/ in a work tree is git data: {plan}");
    assert_eq!(plan["skipped"]["tracked"], 1, "{plan}");
    assert_eq!(plan["expectedFreedBytes"], 0);

    // The checkout ignores its target/: same scan, now a candidate.
    std::fs::write(root.join(".gitignore"), "target\n").unwrap();
    let plan: serde_json::Value = s
        .post(&format!("/scans/{scan_id}/plan"), serde_json::json!({}))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(plan["itemCount"], 1, "{plan}");
    assert_eq!(plan["skipped"]["tracked"], 0, "{plan}");
    assert_eq!(
        plan["items"][0]["paths"][0].as_str().unwrap(),
        root.join("proj").join("target").to_str().unwrap()
    );
}
