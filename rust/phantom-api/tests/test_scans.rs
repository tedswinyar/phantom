// Integration tests for the async scan lifecycle: boot the real router on an
// ephemeral port with a temp database and drive it over real HTTP — the same
// surface the CLI, MCP server, Swift app, and OPE conformance harness hit.
//
// Determinism: `AppState::scan_hold` parks every scan worker BEFORE its walk
// until the test calls `notify_one()`, so cancel/list/delete can be ordered
// against a scan that is provably still running — no sleeps, no races.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use phantom_api::{AppState, build_router, scans::finish_scan};
use phantom_core::{Scan, ScanEntry, ScanOutcome, ScanStore};
use uuid::Uuid;

const KEY: &str = "test-key-not-secret";
const MIB: u64 = 1024 * 1024;

struct TestServer {
    base: String,
    client: reqwest::Client,
    state: AppState,
    db: PathBuf,
    dir: tempfile::TempDir,
}

impl TestServer {
    async fn start() -> Self {
        Self::boot(false).await
    }

    /// A server whose scan workers park before walking until
    /// `self.release_scan()` is called.
    async fn start_held() -> Self {
        Self::boot(true).await
    }

    async fn boot(held: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("phantom.db");
        let mut state = AppState::new(ScanStore::open(&db).unwrap(), KEY.into());
        if held {
            state.scan_hold = Some(Arc::new(tokio::sync::Notify::new()));
        }
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let router_state = state.clone();
        tokio::spawn(async move {
            axum::serve(listener, build_router(router_state)).await.unwrap();
        });
        Self {
            base,
            client: reqwest::Client::new(),
            state,
            db,
            dir,
        }
    }

    fn release_scan(&self) {
        self.state
            .scan_hold
            .as_ref()
            .expect("server was not started with a hold")
            .notify_one();
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

    async fn post_empty(&self, path: &str) -> reqwest::Response {
        self.client
            .post(format!("{}{path}", self.base))
            .header("x-api-key", KEY)
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

    async fn delete(&self, path: &str) -> reqwest::Response {
        self.client
            .delete(format!("{}{path}", self.base))
            .header("x-api-key", KEY)
            .send()
            .await
            .unwrap()
    }

    /// Build the deterministic fixture tree and return its root path.
    ///
    ///   root/big.bin        1 MiB   → exactly ON the persistence boundary
    ///   root/small.txt      10 B    → below it (row filtered, bytes counted)
    ///   root/sub/medium.log 2 MiB   → above it
    ///   root/sub/tiny.rs    100 B   → below it
    ///   root/empty/                 → empty dir, persisted
    fn build_fixture_tree(&self) -> String {
        let root = self.dir.path().join("fixture");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("big.bin"), vec![7u8; MIB as usize]).unwrap();
        std::fs::write(root.join("small.txt"), b"tiny bytes").unwrap();
        std::fs::create_dir(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/medium.log"), vec![9u8; 2 * MIB as usize]).unwrap();
        std::fs::write(root.join("sub/tiny.rs"), vec![1u8; 100]).unwrap();
        std::fs::create_dir(root.join("empty")).unwrap();
        root.display().to_string()
    }

    /// POST /scans for `root`, asserting the 202 contract; returns the id.
    async fn start_scan(&self, root: &str) -> String {
        let resp = self
            .post("/scans", serde_json::json!({ "rootPath": root }))
            .await;
        assert_eq!(resp.status(), 202, "POST /scans answers 202 immediately");
        let v: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(v["status"], "running");
        assert!(v["finishedAt"].is_null());
        assert!(
            v["progress"]["filesSeen"].is_u64(),
            "202 body carries live progress: {v}"
        );
        v["id"].as_str().unwrap().to_string()
    }

    /// Poll GET /scans/{id} until the status is terminal.
    async fn poll_terminal(&self, id: &str) -> serde_json::Value {
        for _ in 0..1000 {
            let v: serde_json::Value = self
                .get(&format!("/scans/{id}"))
                .await
                .json()
                .await
                .unwrap();
            if v["status"] != "running" {
                return v;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("scan {id} never reached a terminal status");
    }

    /// Scan the fixture tree end-to-end; returns (root, id, terminal view).
    async fn scan_fixture(&self) -> (String, String, serde_json::Value) {
        let root = self.build_fixture_tree();
        let id = self.start_scan(&root).await;
        let done = self.poll_terminal(&id).await;
        assert_eq!(done["status"], "complete");
        (root, id, done)
    }
}

// --- Lifecycle ---------------------------------------------------------------

#[tokio::test]
async fn scan_lifecycle_end_to_end() {
    let s = TestServer::start().await;
    let (_root, id, done) = s.scan_fixture().await;

    // Full wire contract on the terminal view: camelCase keys, nullable
    // fields present (progress goes null once terminal).
    let obj = done.as_object().unwrap();
    for key in [
        "id", "rootPath", "status", "startedAt", "finishedAt", "totalDiskSize",
        "totalLogicalSize", "fileCount", "dirCount", "errorCount", "progress",
    ] {
        assert!(obj.contains_key(key), "missing wire key {key}");
    }
    assert!(obj["progress"].is_null(), "terminal scans carry progress: null");
    let finished = obj["finishedAt"].as_str().unwrap();
    assert!(
        finished.len() == 27 && finished.ends_with('Z') && finished.contains('.'),
        "finishedAt not canonical: {finished}"
    );
    assert_eq!(obj["id"].as_str().unwrap(), id);

    // Totals come from the FULL walk (small files count).
    assert_eq!(obj["fileCount"], 4);
    assert_eq!(obj["dirCount"], 3, "root + sub + empty");
    assert_eq!(obj["errorCount"], 0);
    let total = obj["totalDiskSize"].as_u64().unwrap();
    assert!(total >= 3 * MIB, "all four files' disk bytes: {total}");
    assert_eq!(total % 512, 0, "disk bytes are whole 512-byte blocks");
    assert!(obj["totalLogicalSize"].as_u64().unwrap() >= 3 * MIB + 110);
}

#[tokio::test]
async fn persistence_keeps_dirs_and_big_files_and_folds_the_rest_into_totals() {
    let s = TestServer::start().await;
    let (root, id, done) = s.scan_fixture().await;

    // Files at/above 1 MiB survive as rows; below-threshold rows are gone.
    let files: serde_json::Value = s
        .get(&format!("/scans/{id}/files"))
        .await
        .json()
        .await
        .unwrap();
    let names: Vec<&str> = files
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        vec!["medium.log", "big.bin"],
        "size-desc; big.bin sits exactly ON the inclusive 1 MiB boundary"
    );

    // The filtered small files still count: the persisted root directory's
    // aggregate equals the scan's full-walk total, byte for byte.
    let entry: serde_json::Value = s
        .get(&format!("/scans/{id}/entry?path={root}"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(entry["isDir"], true);
    assert_eq!(entry["parentPath"], serde_json::Value::Null);
    assert_eq!(entry["diskSize"], done["totalDiskSize"]);
    assert_eq!(entry["logicalSize"], done["totalLogicalSize"]);

    // Descendant counts, aggregated over the FULL walk like the sizes —
    // mutation-proof: count from persisted rows instead and the root shows
    // 2 files (small.txt/tiny.rs vanish); swap the tallies and it shows
    // (2, 4). dirCount excludes the entry itself.
    assert_eq!(entry["fileCount"], 4, "filtered small files still count");
    assert_eq!(entry["dirCount"], 2, "sub + empty; self excluded");
    let sub: serde_json::Value = s
        .get(&format!("/scans/{id}/entry?path={root}/sub"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(sub["fileCount"], 2);
    assert_eq!(sub["dirCount"], 0);
    // File rows carry the counts present-as-null, never absent.
    let file_row: serde_json::Value = s
        .get(&format!("/scans/{id}/entry?path={root}/big.bin"))
        .await
        .json()
        .await
        .unwrap();
    for key in ["fileCount", "dirCount"] {
        assert!(file_row.as_object().unwrap().contains_key(key), "missing {key}");
        assert!(file_row[key].is_null(), "{key} must be null on a file row");
    }
}

#[tokio::test]
async fn types_route_serves_full_walk_totals_largest_first() {
    let s = TestServer::start().await;
    let (_root, id, _done) = s.scan_fixture().await;

    // txt/rs come from files whose ROWS were filtered out at persistence —
    // mutation-proof: compute the totals after the filter in finish_scan and
    // those two vanish.
    let v: serde_json::Value = s
        .get(&format!("/scans/{id}/types"))
        .await
        .json()
        .await
        .unwrap();
    let items = v.as_array().unwrap();

    // Wire shape: camelCase keys, nullable fileType present-as-null capable.
    for key in ["fileType", "diskSize", "fileCount"] {
        assert!(items[0].as_object().unwrap().contains_key(key), "missing {key}");
    }

    // Order is part of the contract: largest disk footprint first, type-name
    // tiebreak — mutation-proof: drop the ORDER BY in
    // `ScanStore::file_type_totals` and this exact sequence fails.
    let types: Vec<&str> = items
        .iter()
        .map(|t| t["fileType"].as_str().unwrap())
        .collect();
    assert_eq!(
        types,
        vec!["log", "bin", "rs", "txt"],
        "log (2 MiB) > bin (1 MiB) > rs (100 B) vs txt (10 B): disk-desc"
    );
    for t in items {
        assert_eq!(t["fileCount"], 1);
    }
    let by_type: std::collections::HashMap<&str, u64> = items
        .iter()
        .map(|t| (t["fileType"].as_str().unwrap(), t["diskSize"].as_u64().unwrap()))
        .collect();
    assert!(by_type["txt"] > 0, "filtered small file still totalled");
    assert!(by_type["rs"] > 0, "filtered small file still totalled");
}

#[tokio::test]
async fn types_route_error_branches() {
    let s = TestServer::start_held().await;

    // Unknown scan → 404; malformed uuid → 400, error-shaped.
    let resp = s
        .get("/scans/e7ae86e2-308b-444c-8a3d-cd21467ab442/types")
        .await;
    assert_eq!(resp.status(), 404);
    let resp = s.get("/scans/not-a-uuid/types").await;
    assert_eq!(resp.status(), 400);
    let v: serde_json::Value = resp.json().await.unwrap();
    assert!(v["error"].is_string(), "rejection must be error-shaped: {v}");

    // Running scan → 409 pointing back at the progress surface —
    // mutation-proof: drop the `persisted_scan` gate in `get_types` and this
    // returns 200 with an empty array instead.
    let root = s.build_fixture_tree();
    let id = s.start_scan(&root).await;
    let resp = s.get(&format!("/scans/{id}/types")).await;
    assert_eq!(resp.status(), 409);
    let v: serde_json::Value = resp.json().await.unwrap();
    assert!(v["error"].as_str().unwrap().contains("still running"));

    s.release_scan();
    s.poll_terminal(&id).await;
}

// --- Retention -----------------------------------------------------------------

/// Completing a scan prunes the collection to the newest KEEP_LAST_SCANS.
/// 25 synthetic older scans + 1 real completion = 26 → the OLDEST synthetic
/// is pruned, its entries cascading with it. Mutation-proof: remove the
/// prune_to_last call from finish_scan and the list stays at 26 with the
/// oldest scan still present.
#[tokio::test]
async fn completion_prunes_to_the_newest_keep_last_scans() {
    use phantom_api::scans::KEEP_LAST_SCANS;

    let s = TestServer::start().await;

    // Seed KEEP_LAST_SCANS synthetic scans, all older than "now", oldest
    // first; the oldest carries an entry so the cascade is observable.
    let mut oldest_id = None;
    for i in 0..KEEP_LAST_SCANS {
        let mut scan = Scan::new(format!("/synthetic/{i}"));
        scan.status = phantom_core::ScanStatus::Complete;
        scan.started_at = chrono::Utc::now() - chrono::Duration::minutes((KEEP_LAST_SCANS - i) as i64);
        scan.finished_at = Some(scan.started_at);
        let entries = if i == 0 {
            oldest_id = Some(scan.id);
            vec![ScanEntry {
                path: "/synthetic/0".into(),
                parent_path: None,
                name: "0".into(),
                is_dir: true,
                disk_size: 0,
                logical_size: 0,
                modified_at: None,
                file_type: None,
                category: None,
                nlink: 1,
                dev: 0,
                ino: 0,
                file_count: Some(0),
                dir_count: Some(0),
            }]
        } else {
            Vec::new()
        };
        s.state
            .scan_store()
            .insert_scan(&scan, &entries, &[], None)
            .unwrap();
    }
    let oldest_id = oldest_id.unwrap();

    // One real completion pushes the count to KEEP_LAST_SCANS + 1; the
    // completion path must prune back down.
    let (_root, _new_id, _done) = s.scan_fixture().await;

    let list: serde_json::Value = s.get("/scans").await.json().await.unwrap();
    let items = list.as_array().unwrap();
    assert_eq!(
        items.len(),
        KEEP_LAST_SCANS,
        "completion must prune to the newest {KEEP_LAST_SCANS}"
    );
    assert!(
        !items.iter().any(|v| v["id"] == oldest_id.to_string()),
        "the oldest scan must be the one pruned"
    );
    // Its entries cascaded with it — not orphaned in the entries table.
    let store = &s.state;
    let orphans = store.scan_store().entries(oldest_id);
    assert!(orphans.is_err(), "pruned scan must be NotFound, entries cascaded");
}

// --- Hotspots (Phase 5) --------------------------------------------------------

impl TestServer {
    /// A tree with one hotspot: node_modules holding a file on each side of
    /// the ADR-0005 persistence boundary, plus an ordinary file outside it.
    ///
    ///   root/node_modules/chunk.bin 1 MiB  → persisted, categorized
    ///   root/node_modules/tiny.js   10 B   → row filtered; the classifier
    ///                                        still counts it (full walk)
    ///   root/plain.bin              1 MiB  → persisted, NO category
    fn build_hotspot_tree(&self) -> String {
        let root = self.dir.path().join("hot");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(root.join("node_modules")).unwrap();
        std::fs::write(root.join("node_modules/chunk.bin"), vec![3u8; MIB as usize]).unwrap();
        std::fs::write(root.join("node_modules/tiny.js"), b"tiny bytes").unwrap();
        std::fs::write(root.join("plain.bin"), vec![4u8; MIB as usize]).unwrap();
        root.display().to_string()
    }
}

#[tokio::test]
async fn hotspots_classify_on_completion_and_persist_categories() {
    let s = TestServer::start().await;
    let root = s.build_hotspot_tree();
    let id = s.start_scan(&root).await;
    let done = s.poll_terminal(&id).await;
    assert_eq!(done["status"], "complete");

    // The summary, over the wire: camelCase at every depth, one group.
    let v: serde_json::Value = s
        .get(&format!("/scans/{id}/hotspots"))
        .await
        .json()
        .await
        .unwrap();
    let obj = v.as_object().unwrap();
    for key in [
        "groups",
        "reclaimEstimate",
        "reviewDiskSize",
        "cloudDataloadedLogicalSize",
        "cloudDataloadedDiskSize",
    ] {
        assert!(obj.contains_key(key), "summary missing wire key {key}");
    }
    let groups = v["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 1, "exactly the node_modules group: {v}");
    let g = &groups[0];
    assert_eq!(g["ruleId"], "node-modules");
    assert_eq!(g["category"], "regenerableArtifact");
    // command is first-class and HONEST: node_modules' only cleanup is
    // deleting the directory, so it carries null, present-as-null.
    assert!(g.as_object().unwrap().contains_key("command"));
    assert!(g["command"].is_null(), "node-modules must not offer a command: {g}");
    assert_eq!(
        g["topPaths"],
        serde_json::json!([format!("{root}/node_modules")])
    );
    // Full-walk classification: tiny.js has no persisted row, yet it counts
    // here — mutation-proof: classify AFTER persistable_entries in
    // finish_scan and fileCount drops to 1 / diskSize to exactly 1 MiB.
    assert_eq!(g["fileCount"], 2, "the filtered small file still counts");
    let group_disk = g["diskSize"].as_u64().unwrap();
    assert!(group_disk > MIB, "tiny.js blocks must be in the total: {group_disk}");
    assert_eq!(v["reclaimEstimate"].as_u64().unwrap(), group_disk);
    assert_eq!(v["reviewDiskSize"], 0);
    assert_eq!(v["cloudDataloadedLogicalSize"], 0);
    assert_eq!(v["cloudDataloadedDiskSize"], 0);

    // Categories persisted on the entry rows — the DIR row included (lead
    // decision: dir rows under hotspot roots get categories at persist time).
    let dir_row: serde_json::Value = s
        .get(&format!("/scans/{id}/entry?path={root}/node_modules"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(dir_row["isDir"], true);
    assert_eq!(dir_row["category"], "regenerableArtifact");
    let file_row: serde_json::Value = s
        .get(&format!("/scans/{id}/entry?path={root}/node_modules/chunk.bin"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(file_row["category"], "regenerableArtifact");

    // Ordinary content stays uncategorized — present-as-null, never absent.
    let plain: serde_json::Value = s
        .get(&format!("/scans/{id}/entry?path={root}/plain.bin"))
        .await
        .json()
        .await
        .unwrap();
    assert!(plain.as_object().unwrap().contains_key("category"));
    assert!(plain["category"].is_null());
}

#[tokio::test]
async fn hotspots_of_a_scan_with_no_hotspots_is_the_empty_summary() {
    let s = TestServer::start().await;
    let (_root, id, _done) = s.scan_fixture().await;

    let v: serde_json::Value = s
        .get(&format!("/scans/{id}/hotspots"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(v["groups"], serde_json::json!([]));
    assert_eq!(v["reclaimEstimate"], 0);
    assert_eq!(v["reviewDiskSize"], 0);
    assert_eq!(v["cloudDataloadedLogicalSize"], 0);
    assert_eq!(v["cloudDataloadedDiskSize"], 0);
}

#[tokio::test]
async fn hotspots_error_branches() {
    let s = TestServer::start_held().await;

    // Unknown scan → 404; malformed uuid → 400, error-shaped.
    let resp = s
        .get("/scans/e7ae86e2-308b-444c-8a3d-cd21467ab442/hotspots")
        .await;
    assert_eq!(resp.status(), 404);
    let resp = s.get("/scans/not-a-uuid/hotspots").await;
    assert_eq!(resp.status(), 400);
    let v: serde_json::Value = resp.json().await.unwrap();
    assert!(v["error"].is_string(), "rejection must be error-shaped: {v}");

    // Running scan → 409, matching the /types semantics — mutation-proof:
    // drop the `persisted_scan` gate in `get_hotspots` and this returns 200.
    let root = s.build_fixture_tree();
    let id = s.start_scan(&root).await;
    let resp = s.get(&format!("/scans/{id}/hotspots")).await;
    assert_eq!(resp.status(), 409);
    let v: serde_json::Value = resp.json().await.unwrap();
    assert!(v["error"].as_str().unwrap().contains("still running"));

    s.release_scan();
    s.poll_terminal(&id).await;
}

// --- Cancel ------------------------------------------------------------------

#[tokio::test]
async fn cancel_mid_scan_is_deterministic_and_discards_partial_results() {
    let s = TestServer::start_held().await;
    let root = s.build_fixture_tree();
    let id = s.start_scan(&root).await;

    // The worker is parked pre-walk: the scan is running, verifiably.
    let v: serde_json::Value = s.get(&format!("/scans/{id}")).await.json().await.unwrap();
    assert_eq!(v["status"], "running");

    // Results are not readable while running — 409, not 404 or empty.
    let resp = s.get(&format!("/scans/{id}/files")).await;
    assert_eq!(resp.status(), 409);
    let e: serde_json::Value = resp.json().await.unwrap();
    assert!(e["error"].as_str().unwrap().contains("still running"));

    let resp = s.post_empty(&format!("/scans/{id}/cancel")).await;
    assert_eq!(resp.status(), 202);

    // Release the walk; it sees the flag at its first entry and bails.
    s.release_scan();
    let done = s.poll_terminal(&id).await;
    assert_eq!(done["status"], "cancelled");
    assert!(done["finishedAt"].is_string());
    assert!(done["progress"].is_null());

    // Partial results DISCARDED: the metadata row records the attempt, the
    // result surfaces are empty.
    assert_eq!(done["fileCount"], 0);
    assert_eq!(done["totalDiskSize"], 0);
    let files: serde_json::Value = s
        .get(&format!("/scans/{id}/files"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(files.as_array().unwrap().len(), 0);
    let tree: serde_json::Value = s
        .get(&format!("/scans/{id}/tree"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(tree.as_array().unwrap().len(), 0);
    // No stored summary either: the honest empty summary, same posture.
    let hs: serde_json::Value = s
        .get(&format!("/scans/{id}/hotspots"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(hs["groups"], serde_json::json!([]));
    assert_eq!(hs["reclaimEstimate"], 0);

    // Cancelling a terminal scan is a conflict, not a no-op.
    let resp = s.post_empty(&format!("/scans/{id}/cancel")).await;
    assert_eq!(resp.status(), 409);
    let e: serde_json::Value = resp.json().await.unwrap();
    assert!(e["error"].as_str().unwrap().contains("cancelled"));
}

#[tokio::test]
async fn cancel_error_branches() {
    let s = TestServer::start().await;

    let resp = s
        .post_empty("/scans/e7ae86e2-308b-444c-8a3d-cd21467ab442/cancel")
        .await;
    assert_eq!(resp.status(), 404);

    let resp = s.post_empty("/scans/not-a-uuid/cancel").await;
    assert_eq!(resp.status(), 400);
    let v: serde_json::Value = resp.json().await.unwrap();
    assert!(v["error"].is_string(), "rejection must be error-shaped: {v}");

    // Cancel after completion → 409 naming the terminal status.
    let (_root, id, _done) = s.scan_fixture().await;
    let resp = s.post_empty(&format!("/scans/{id}/cancel")).await;
    assert_eq!(resp.status(), 409);
    let v: serde_json::Value = resp.json().await.unwrap();
    assert!(v["error"].as_str().unwrap().contains("complete"));
}

// --- The registry↔SQLite handoff --------------------------------------------

#[tokio::test]
async fn list_merges_in_flight_and_persisted_exactly_once() {
    let s = TestServer::start_held().await;
    let root = s.build_fixture_tree();

    // One persisted scan…
    let done_id = s.start_scan(&root).await;
    s.release_scan();
    s.poll_terminal(&done_id).await;
    tokio::time::sleep(Duration::from_millis(3)).await; // distinct startedAt

    // …and one deterministically in flight.
    let running_id = s.start_scan(&root).await;

    let list: serde_json::Value = s.get("/scans").await.json().await.unwrap();
    let items = list.as_array().unwrap();
    assert_eq!(items.len(), 2, "one persisted + one in-flight, no doubles");
    assert_eq!(items[0]["id"].as_str().unwrap(), running_id, "newest first");
    assert_eq!(items[0]["status"], "running");
    assert!(items[0]["progress"].is_object());
    assert_eq!(items[1]["id"].as_str().unwrap(), done_id);
    assert_eq!(items[1]["status"], "complete");
    assert!(items[1]["progress"].is_null());

    // After completion the same scan appears once, from the DB side.
    s.release_scan();
    s.poll_terminal(&running_id).await;
    let list: serde_json::Value = s.get("/scans").await.json().await.unwrap();
    let ids: Vec<&str> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec![running_id.as_str(), done_id.as_str()]);
    assert!(
        list.as_array().unwrap().iter().all(|v| v["status"] == "complete"),
        "handed off: {list}"
    );
    assert!(
        s.state.registry.snapshot(Uuid::parse_str(&running_id).unwrap()).is_none(),
        "after the handoff the registry side is released (never double-held)"
    );
}

/// THE ordering test (persist first, release the registry second). The walk
/// outcome is crafted so the DB insert FAILS (duplicate entry paths violate
/// UNIQUE(scan_id, path) and roll the whole insert back). Correct ordering
/// leaves the scan visible in the registry as `failed`; the reverted
/// ordering (remove, then insert) leaves it in NEITHER place and the GET
/// below 404s. Revert the order in `finish_scan` to watch this fail.
#[tokio::test]
async fn handoff_failure_keeps_the_scan_visible() {
    let s = TestServer::start().await;
    let scan = Scan::new("/synthetic");
    let id = scan.id;
    s.state.registry.register(scan);

    let dup = ScanEntry {
        path: "/synthetic".into(),
        parent_path: None,
        name: "synthetic".into(),
        is_dir: true,
        disk_size: 0,
        logical_size: 0,
        modified_at: None,
        file_type: None,
        category: None,
        nlink: 1,
        dev: 0,
        ino: 0,
        file_count: None,
        dir_count: None,
    };
    let outcome = ScanOutcome {
        entries: vec![dup.clone(), dup],
        total_disk_size: 0,
        total_logical_size: 0,
        file_count: 0,
        dir_count: 1,
        error_count: 0,
        unreadable: Vec::new(),
    };
    finish_scan(&s.state, id, Ok(outcome));

    let resp = s.get(&format!("/scans/{id}")).await;
    assert_eq!(
        resp.status(),
        200,
        "a scan whose persist failed must stay visible, never vanish"
    );
    let v: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(v["status"], "failed");

    // Defined delete semantics for the registry-only terminal residue.
    let resp = s.delete(&format!("/scans/{id}")).await;
    assert_eq!(resp.status(), 204);
    assert_eq!(s.get(&format!("/scans/{id}")).await.status(), 404);
}

// --- Delete ------------------------------------------------------------------

#[tokio::test]
async fn delete_scan_cascades_and_running_scans_refuse() {
    let s = TestServer::start_held().await;
    let root = s.build_fixture_tree();

    // In-flight: deletion is refused until cancelled (defined semantics).
    let running_id = s.start_scan(&root).await;
    let resp = s.delete(&format!("/scans/{running_id}")).await;
    assert_eq!(resp.status(), 409);
    let v: serde_json::Value = resp.json().await.unwrap();
    assert!(v["error"].as_str().unwrap().contains("cancel"));

    s.release_scan();
    let done = s.poll_terminal(&running_id).await;
    assert_eq!(done["status"], "complete");

    // Terminal: delete cascades; every result surface 404s afterwards.
    let resp = s.delete(&format!("/scans/{running_id}")).await;
    assert_eq!(resp.status(), 204);
    assert_eq!(s.get(&format!("/scans/{running_id}")).await.status(), 404);
    assert_eq!(
        s.get(&format!("/scans/{running_id}/files")).await.status(),
        404
    );
    let store = ScanStore::open(&s.db).unwrap();
    let orphan_check = store.entries(Uuid::parse_str(&running_id).unwrap());
    assert!(orphan_check.is_err(), "entries cascade with the scan row");

    // Unknown id → 404.
    let resp = s
        .delete("/scans/e7ae86e2-308b-444c-8a3d-cd21467ab442")
        .await;
    assert_eq!(resp.status(), 404);
}

// --- Treemap -----------------------------------------------------------------

#[tokio::test]
async fn treemap_lays_out_at_view_size_and_reroots() {
    let s = TestServer::start().await;
    let (root, id, _done) = s.scan_fixture().await;

    let v: serde_json::Value = s
        .get(&format!("/scans/{id}/treemap?width=400&height=300"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(v["rootPath"].as_str().unwrap(), root);
    let rects = v["rects"].as_array().unwrap();
    let root_rect = &rects[0];
    assert_eq!(root_rect["x"], 0.0);
    assert_eq!(root_rect["y"], 0.0);
    assert_eq!(root_rect["width"], 400.0);
    assert_eq!(root_rect["height"], 300.0);
    let names: Vec<&str> = rects.iter().map(|r| r["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"big.bin"));
    assert!(names.contains(&"medium.log"));
    assert!(
        !names.contains(&"small.txt"),
        "sub-1-MiB rows are not persisted, so they cannot be rects"
    );

    // root= re-roots AND re-lays-out: the subtree root fills the new bounds.
    let sub = format!("{root}/sub");
    let v: serde_json::Value = s
        .get(&format!(
            "/scans/{id}/treemap?width=200&height=100&root={sub}"
        ))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(v["rootPath"].as_str().unwrap(), sub);
    let rects = v["rects"].as_array().unwrap();
    assert!(rects.iter().all(|r| r["path"].as_str().unwrap().starts_with(&sub)));
    assert_eq!(rects[0]["width"], 200.0, "re-laid-out at the requested size");
    assert_eq!(rects[0]["height"], 100.0);
    // The re-rooted total is sub's aggregate — including its filtered file.
    let sub_entry: serde_json::Value = s
        .get(&format!("/scans/{id}/entry?path={sub}"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(v["totalSize"], sub_entry["diskSize"]);

    // maxDepth=0 → only the root rect.
    let v: serde_json::Value = s
        .get(&format!("/scans/{id}/treemap?maxDepth=0"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(v["rects"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn treemap_error_branches() {
    let s = TestServer::start().await;
    let (root, id, _done) = s.scan_fixture().await;

    // Unknown re-root path → 404; re-root on a file → 400.
    let resp = s
        .get(&format!("/scans/{id}/treemap?root={root}/nope"))
        .await;
    assert_eq!(resp.status(), 404);
    let resp = s
        .get(&format!("/scans/{id}/treemap?root={root}/big.bin"))
        .await;
    assert_eq!(resp.status(), 400);
    let v: serde_json::Value = resp.json().await.unwrap();
    assert!(v["error"].as_str().unwrap().contains("directory"));

    // Bad geometry → 400, error-shaped, naming the parameter.
    for query in ["width=abc", "width=0", "height=-5", "maxDepth=x", "maxDepth=-1"] {
        let resp = s.get(&format!("/scans/{id}/treemap?{query}")).await;
        assert_eq!(resp.status(), 400, "{query} must be rejected");
        let v: serde_json::Value = resp.json().await.unwrap();
        assert!(v["error"].is_string(), "{query} rejection must be error-shaped");
    }

    // Unknown scan → 404.
    let resp = s
        .get("/scans/e7ae86e2-308b-444c-8a3d-cd21467ab442/treemap")
        .await;
    assert_eq!(resp.status(), 404);
}

// --- Tree / entry ------------------------------------------------------------

#[tokio::test]
async fn tree_lists_direct_children_of_root_and_of_a_path() {
    let s = TestServer::start().await;
    let (root, id, _done) = s.scan_fixture().await;

    // Default: the scan root's direct children, path-ordered. small.txt's
    // row was filtered at persistence.
    let v: serde_json::Value = s.get(&format!("/scans/{id}/tree")).await.json().await.unwrap();
    let names: Vec<&str> = v
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["big.bin", "empty", "sub"]);

    // Directories carry their persisted aggregates on this surface.
    let sub = v
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == "sub")
        .unwrap();
    assert!(sub["diskSize"].as_u64().unwrap() >= 2 * MIB);

    let v: serde_json::Value = s
        .get(&format!("/scans/{id}/tree?path={root}/sub"))
        .await
        .json()
        .await
        .unwrap();
    let names: Vec<&str> = v
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["medium.log"]);

    // Error branches: unknown path 404, file path 400, unknown scan 404.
    let resp = s.get(&format!("/scans/{id}/tree?path={root}/nope")).await;
    assert_eq!(resp.status(), 404);
    let resp = s
        .get(&format!("/scans/{id}/tree?path={root}/big.bin"))
        .await;
    assert_eq!(resp.status(), 400);
    let resp = s
        .get("/scans/e7ae86e2-308b-444c-8a3d-cd21467ab442/tree")
        .await;
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn entry_returns_one_row_and_validates_its_params() {
    let s = TestServer::start().await;
    let (root, id, _done) = s.scan_fixture().await;

    let v: serde_json::Value = s
        .get(&format!("/scans/{id}/entry?path={root}/big.bin"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(v["name"], "big.bin");
    assert_eq!(v["isDir"], false);
    assert_eq!(v["fileType"], "bin");
    assert_eq!(v["diskSize"].as_u64().unwrap(), MIB);

    let resp = s.get(&format!("/scans/{id}/entry")).await;
    assert_eq!(resp.status(), 400, "path param is required");
    let v: serde_json::Value = resp.json().await.unwrap();
    assert!(v["error"].as_str().unwrap().contains("path"));

    let resp = s.get(&format!("/scans/{id}/entry?path={root}/nope")).await;
    assert_eq!(resp.status(), 404);
    let resp = s
        .get("/scans/e7ae86e2-308b-444c-8a3d-cd21467ab442/entry?path=/x")
        .await;
    assert_eq!(resp.status(), 404);
}

// --- Files: filters, sort, pagination -----------------------------------------

#[tokio::test]
async fn files_filters_sorts_and_paginates() {
    let s = TestServer::start().await;
    let (root, id, _done) = s.scan_fixture().await;
    let base = format!("/scans/{id}/files");

    // fileType is matched case-generously (stored lowercase).
    for ft in ["log", "LOG"] {
        let v: serde_json::Value = s
            .get(&format!("{base}?fileType={ft}"))
            .await
            .json()
            .await
            .unwrap();
        let items = v.as_array().unwrap();
        assert_eq!(items.len(), 1, "fileType={ft}");
        assert_eq!(items[0]["name"], "medium.log");
    }

    // search: substring on the full path.
    let v: serde_json::Value = s
        .get(&format!("{base}?search=big"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(v.as_array().unwrap().len(), 1);
    assert_eq!(v[0]["name"], "big.bin");
    let v: serde_json::Value = s
        .get(&format!("{base}?search=/sub/"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(v[0]["name"], "medium.log");

    // sort=name flips the size-desc default order.
    let v: serde_json::Value = s
        .get(&format!("{base}?sort=name"))
        .await
        .json()
        .await
        .unwrap();
    let names: Vec<&str> = v
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["big.bin", "medium.log"]);

    // Continuation-token pagination, template idiom: X-Next-Cursor header,
    // bare-array body, no spurious cursor on the last page.
    let resp = s.get(&format!("{base}?limit=1")).await;
    let cursor = resp
        .headers()
        .get("x-next-cursor")
        .expect("more rows remain → X-Next-Cursor present")
        .to_str()
        .unwrap()
        .to_string();
    let page1: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(page1.as_array().unwrap().len(), 1);
    assert_eq!(page1[0]["name"], "medium.log");

    let resp = s.get(&format!("{base}?limit=1&cursor={cursor}")).await;
    assert!(
        resp.headers().get("x-next-cursor").is_none(),
        "last page must not advertise a continuation"
    );
    let page2: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(page2[0]["name"], "big.bin");

    // Bad params → 400, error-shaped, naming the parameter.
    for (query, needle) in [
        ("sort=bogus", "sort"),
        ("limit=nope", "limit"),
        ("limit=0", "limit"),
        ("cursor=xyz", "cursor"),
    ] {
        let resp = s.get(&format!("{base}?{query}")).await;
        assert_eq!(resp.status(), 400, "{query}");
        let v: serde_json::Value = resp.json().await.unwrap();
        assert!(
            v["error"].as_str().unwrap().contains(needle),
            "{query} error must name the parameter: {v}"
        );
    }

    // A filter that matches nothing is an empty page, not an error.
    let v: serde_json::Value = s
        .get(&format!("{base}?fileType=zzz&search={root}"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(v.as_array().unwrap().len(), 0);

    // Unknown scan → 404; malformed uuid → 400.
    let resp = s
        .get("/scans/e7ae86e2-308b-444c-8a3d-cd21467ab442/files")
        .await;
    assert_eq!(resp.status(), 404);
    let resp = s.get("/scans/not-a-uuid/files").await;
    assert_eq!(resp.status(), 400);
}

// --- Validation & auth ---------------------------------------------------------

#[tokio::test]
async fn create_scan_validates_its_body() {
    let s = TestServer::start().await;

    // Missing field and unknown field are loud 422s in the {error} shape.
    let resp = s.post("/scans", serde_json::json!({})).await;
    assert_eq!(resp.status(), 422);
    let v: serde_json::Value = resp.json().await.unwrap();
    assert!(v["error"].is_string());

    let resp = s
        .post(
            "/scans",
            serde_json::json!({ "rootPath": "/tmp", "maxDepth": 3 }),
        )
        .await;
    assert_eq!(resp.status(), 422);

    // Wire contract is camelCase; snake_case is an unknown field, not an alias.
    let resp = s
        .post("/scans", serde_json::json!({ "root_path": "/tmp" }))
        .await;
    assert_eq!(resp.status(), 422);

    // Empty, nonexistent, and non-directory roots fail NOW (400), not as a
    // `failed` scan to discover by polling.
    let resp = s.post("/scans", serde_json::json!({ "rootPath": "  " })).await;
    assert_eq!(resp.status(), 400);

    let resp = s
        .post("/scans", serde_json::json!({ "rootPath": "/no/such/dir/anywhere" }))
        .await;
    assert_eq!(resp.status(), 400);
    let v: serde_json::Value = resp.json().await.unwrap();
    assert!(v["error"].as_str().unwrap().contains("not a directory"));

    let file = s.dir.path().join("plain.txt");
    std::fs::write(&file, "not a dir").unwrap();
    let resp = s
        .post(
            "/scans",
            serde_json::json!({ "rootPath": file.display().to_string() }),
        )
        .await;
    assert_eq!(resp.status(), 400);

    // Nothing was registered or persisted by any of the rejects.
    let v: serde_json::Value = s.get("/scans").await.json().await.unwrap();
    assert_eq!(v.as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn scan_routes_require_the_api_key() {
    let s = TestServer::start().await;
    let (_root, id, _done) = s.scan_fixture().await;

    for path in [
        "/scans".to_string(),
        format!("/scans/{id}"),
        format!("/scans/{id}/treemap"),
        format!("/scans/{id}/tree"),
        format!("/scans/{id}/files"),
        format!("/scans/{id}/entry?path=/x"),
        format!("/scans/{id}/types"),
        format!("/scans/{id}/hotspots"),
    ] {
        let resp = reqwest::get(format!("{}{path}", s.base)).await.unwrap();
        assert_eq!(resp.status(), 401, "GET {path} without key");
        let resp = reqwest::Client::new()
            .get(format!("{}{path}", s.base))
            .header("x-api-key", "wrong-key")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401, "GET {path} with wrong key");
    }

    let resp = reqwest::Client::new()
        .post(format!("{}/scans", s.base))
        .json(&serde_json::json!({ "rootPath": "/tmp" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    let resp = reqwest::Client::new()
        .delete(format!("{}/scans/{id}", s.base))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
}

// --- API-wide contract (ported from the retired Note slice's suite) -----------

// R2 (freeze review): query params are STRICT. An unknown/misspelled query
// key on a results route is a 400 naming the field, not a silently
// unfiltered listing — `?filetype=` quietly returning EVERY file is the
// failure mode this kills. Mutation-proof: drop `deny_unknown_fields` from
// FilesParams and this goes green-to-red.
// The 415 row (freeze review R7): a POST body without the JSON
// content-type is 415, re-clothed as {error} like every non-2xx.
#[tokio::test]
async fn post_without_json_content_type_is_415_with_error_shape() {
    let s = TestServer::start().await;
    let resp = reqwest::Client::new()
        .post(format!("{}/scans", s.base))
        .header("x-api-key", KEY)
        .header("content-type", "text/plain")
        .body("{\"rootPath\": \"/tmp\"}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 415);
    let v: serde_json::Value = resp.json().await.unwrap();
    assert!(v.get("error").is_some(), "415 must keep the {{error}} shape");
}

#[tokio::test]
async fn unknown_query_keys_are_rejected_with_the_error_shape() {
    let s = TestServer::start().await;
    let (_root, id, _done) = s.scan_fixture().await;

    let resp = s.get(&format!("/scans/{id}/files?filetype=rs")).await;
    assert_eq!(resp.status(), 400, "misspelled key must not mean 'unfiltered'");
    let v: serde_json::Value = resp.json().await.unwrap();
    let msg = v["error"].as_str().expect("{error} shape");
    assert!(msg.contains("filetype"), "the rejection names the field: {msg}");

    // The other strict result routes, spot-checked.
    for path in [
        format!("/scans/{id}/treemap?depth=2"),
        format!("/scans/{id}/tree?root=/x"),
        format!("/scans/{id}/entry?file=/x"),
    ] {
        let resp = s.get(&path).await;
        assert_eq!(resp.status(), 400, "GET {path}");
        let v: serde_json::Value = resp.json().await.unwrap();
        assert!(v.get("error").is_some(), "GET {path} must keep the {{error}} shape");
    }
}


#[tokio::test]
async fn health_is_open_and_reports_version() {
    let s = TestServer::start().await;
    // Deliberately no api key header.
    let resp = reqwest::get(format!("{}/health", s.base)).await.unwrap();
    assert_eq!(resp.status(), 200);
    let v: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(v["status"], "ok");
    assert!(v["version"].is_string());
}

#[tokio::test]
async fn builtin_404_and_405_use_the_error_shape() {
    // agentapi L2: unmatched routes (404) and wrong-method (405) must not be
    // empty-bodied — they go through the `{error}` contract too. Revert the
    // `error_shape_fallback` layer and these bodies are empty → this fails.
    let s = TestServer::start().await;

    // Unmatched route → 404 JSON (no auth needed; it is not under /scans).
    let resp = s.get("/nonexistent").await;
    assert_eq!(resp.status(), 404);
    let v: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(v["error"], "no such route");

    // Wrong method on a real route → 405 JSON (send the key so we pass auth
    // and reach the method router; /scans routes GET and POST only).
    let resp = s.delete("/scans").await;
    assert_eq!(resp.status(), 405);
    let v: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(v["error"], "method not allowed");
}

#[tokio::test]
async fn uppercase_uuid_is_accepted_in_path() {
    let s = TestServer::start().await;
    let (_root, id, _done) = s.scan_fixture().await;
    let resp = s.get(&format!("/scans/{}", id.to_uppercase())).await;
    assert_eq!(resp.status(), 200);
    // Canonical form comes back lowercase regardless of the path's casing.
    let v: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(v["id"], id.to_lowercase());
}

/// The walker's own cancel flag is what the SIGTERM drain flips
/// (`registry.cancel_all()`, wired in main.rs). Process-level signal
/// delivery can't run inside this harness; this pins the same seam the
/// drain uses: cancel_all → walker bails → cancelled row lands in SQLite.
#[tokio::test]
async fn cancel_all_drains_an_in_flight_scan_to_a_cancelled_row() {
    let s = TestServer::start_held().await;
    let root = s.build_fixture_tree();
    let id = s.start_scan(&root).await;

    assert_eq!(s.state.registry.cancel_all(), 1);
    s.release_scan();
    let done = s.poll_terminal(&id).await;
    assert_eq!(done["status"], "cancelled");

    // The row is in SQLite, not just registry memory — it survives a "restart"
    // (a fresh store connection).
    let store = ScanStore::open(&s.db).unwrap();
    let scan = store.get_scan(Uuid::parse_str(&id).unwrap()).unwrap();
    assert_eq!(scan.status.as_str(), "cancelled");
}

// --- Scan diff (phantom-081) -------------------------------------------------

/// Entry builder for synthetic diff scans; sizes are already what the walk
/// would produce (persist aggregates dirs from the file rows below them).
fn diff_entry(path: &str, parent: Option<&str>, disk: u64, is_dir: bool) -> ScanEntry {
    ScanEntry {
        path: path.into(),
        parent_path: parent.map(Into::into),
        name: std::path::Path::new(path)
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned(),
        is_dir,
        disk_size: disk,
        logical_size: disk,
        modified_at: None,
        file_type: None,
        category: None,
        nlink: 1,
        dev: 1,
        ino: path.len() as u64 * 31 + disk, // unique-enough per fixture
        file_count: None,
        dir_count: None,
    }
}

fn finish_synthetic(s: &TestServer, root: &str, files: &[(&str, &str, u64)], dirs: &[(&str, Option<&str>)]) -> Uuid {
    let scan = Scan::new(root);
    let id = scan.id;
    s.state.registry.register(scan);
    let mut entries = vec![diff_entry(root, None, 0, true)];
    for (path, parent) in dirs {
        entries.push(diff_entry(path, Some(parent.unwrap_or(root)), 0, true));
    }
    let mut total = 0;
    for (path, parent, size) in files {
        entries.push(diff_entry(path, Some(parent), *size, false));
        total += size;
    }
    let outcome = ScanOutcome {
        entries,
        total_disk_size: total,
        total_logical_size: total,
        file_count: files.len() as u64,
        dir_count: 1 + dirs.len() as u64,
        error_count: 0,
        unreadable: Vec::new(),
    };
    finish_scan(&s.state, id, Ok(outcome));
    id
}

/// The full arc: grown (created dir, before null), freed (vanished dir,
/// after null), exact top-level deltas — over the REAL router and store.
#[tokio::test]
async fn diff_reports_grown_freed_and_exact_totals() {
    let s = TestServer::start().await;
    let a = finish_synthetic(
        &s,
        "/synthroot",
        &[("/synthroot/big/a.bin", "/synthroot/big", 60 * MIB)],
        &[("/synthroot/big", None)],
    );
    let b = finish_synthetic(
        &s,
        "/synthroot",
        &[("/synthroot/new/b.bin", "/synthroot/new", 8 * MIB)],
        &[("/synthroot/new", None)],
    );

    let resp = s.get(&format!("/scans/{a}/diff/{b}")).await;
    assert_eq!(resp.status(), 200);
    let v: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(v["rootPath"], "/synthroot");
    assert_eq!(v["diskDelta"], -(52 * MIB as i64));
    assert_eq!(v["fileCountDelta"], 0);
    assert_eq!(
        v["grown"][0]["path"], "/synthroot/new",
        "created dir grows from null"
    );
    assert!(v["grown"][0]["before"].is_null());
    let freed_paths: Vec<&str> = v["freed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect();
    assert_eq!(freed_paths, vec!["/synthroot/big", "/synthroot"], "-60MiB before -52MiB");
    assert!(v["freed"][0]["after"].is_null(), "vanished dir frees to null");

    // Positional direction: swapping the ids flips every sign.
    let resp = s.get(&format!("/scans/{b}/diff/{a}")).await;
    let flipped: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(flipped["diskDelta"], 52 * MIB as i64);
}

#[tokio::test]
async fn diff_rejects_root_mismatch_with_400() {
    let s = TestServer::start().await;
    let a = finish_synthetic(&s, "/rootA", &[], &[]);
    let b = finish_synthetic(&s, "/rootB", &[], &[]);
    let resp = s.get(&format!("/scans/{a}/diff/{b}")).await;
    assert_eq!(resp.status(), 400);
    let v: serde_json::Value = resp.json().await.unwrap();
    assert!(v["error"].as_str().unwrap().contains("different roots"));
}

#[tokio::test]
async fn diff_against_a_running_scan_is_409() {
    let s = TestServer::start().await;
    let a = finish_synthetic(&s, "/synthroot", &[], &[]);
    let running = Scan::new("/synthroot");
    let running_id = running.id;
    s.state.registry.register(running);
    let resp = s.get(&format!("/scans/{a}/diff/{running_id}")).await;
    assert_eq!(resp.status(), 409);
}
