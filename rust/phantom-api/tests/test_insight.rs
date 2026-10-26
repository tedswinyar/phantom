// Integration tests for the insight routes (v1.1 Phase 3, phantom-mkn.9):
// explain a path, list stale projects, report the volume — over real HTTP
// on a temp database, from a tree with one safe hotspot.

use std::path::{Path, PathBuf};

use phantom_api::{AppState, build_router};
use phantom_core::ScanStore;

const KEY: &str = "test-key-not-secret";
const MIB: usize = 1024 * 1024;

struct TestServer {
    base: String,
    client: reqwest::Client,
    state: AppState,
    _dir: tempfile::TempDir,
}

impl TestServer {
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(ScanStore::open(&dir.path().join("phantom.db")).unwrap(), KEY.into());
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let router = build_router(state.clone());
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Self { base, client: reqwest::Client::new(), state, _dir: dir }
    }

    async fn get(&self, path: &str) -> reqwest::Response {
        self.client
            .get(format!("{}{path}", self.base))
            .header("x-api-key", KEY)
            .send()
            .await
            .unwrap()
    }

    async fn get_q(&self, path: &str, query: &[(&str, &str)]) -> reqwest::Response {
        self.client
            .get(format!("{}{path}", self.base))
            .query(query)
            .header("x-api-key", KEY)
            .send()
            .await
            .unwrap()
    }

    async fn scan(&self, root: &Path) -> String {
        let resp = self
            .client
            .post(format!("{}/scans", self.base))
            .header("x-api-key", KEY)
            .json(&serde_json::json!({ "rootPath": root.to_str().unwrap() }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 202);
        let v: serde_json::Value = resp.json().await.unwrap();
        let id = v["id"].as_str().unwrap().to_string();
        for _ in 0..600 {
            let v: serde_json::Value = self.get(&format!("/scans/{id}")).await.json().await.unwrap();
            if v["status"] != "running" {
                assert_eq!(v["status"], "complete", "{v}");
                return id;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("scan never finished");
    }
}

/// A Cargo project with a 2 MiB artifact whose SOURCE files were last
/// touched `source_age_days` ago (the artifact stays fresh — artifact
/// mtimes must not count as activity).
fn cargo_project(under: &Path, source_age_days: u64) -> PathBuf {
    let proj = under.join("root").join("proj");
    std::fs::create_dir_all(proj.join("src")).unwrap();
    std::fs::create_dir_all(proj.join("target").join("debug")).unwrap();
    std::fs::write(proj.join("Cargo.toml"), "[package]\nname = \"p\"\n").unwrap();
    std::fs::write(proj.join("Cargo.lock"), "# lock\n").unwrap();
    std::fs::write(proj.join("src").join("main.rs"), "fn main() {}\n").unwrap();
    std::fs::write(proj.join("target").join("debug").join("blob.bin"), vec![3u8; 2 * MIB]).unwrap();
    let then = std::time::SystemTime::now() - std::time::Duration::from_secs(source_age_days * 86_400);
    for f in ["Cargo.toml", "Cargo.lock", "src/main.rs"] {
        std::fs::File::options()
            .write(true)
            .open(proj.join(f))
            .unwrap()
            .set_modified(then)
            .unwrap();
    }
    proj.parent().unwrap().to_path_buf()
}

#[tokio::test]
async fn explain_composes_entry_group_and_verdict() {
    let s = TestServer::start().await;
    let root = cargo_project(s._dir.path(), 0);
    // Ordinary content that KEEPS a row (1.1.1: directories under 1 MiB
    // have none): a 1 MiB asset beside the sources.
    std::fs::create_dir_all(root.join("proj").join("assets")).unwrap();
    std::fs::write(root.join("proj").join("assets").join("photo.bin"), vec![5u8; MIB]).unwrap();
    let id = s.scan(&root).await;
    let target = root.join("proj").join("target");

    let resp = s.get_q(&format!("/scans/{id}/explain"), &[("path", target.to_str().unwrap())]).await;
    assert_eq!(resp.status(), 200);
    let v: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(v["path"], target.to_str().unwrap());
    assert_eq!(v["isDir"], true);
    assert!(v["diskSize"].as_u64().unwrap() >= 2 * MIB as u64);
    assert_eq!(v["privateSize"], v["diskSize"], "nothing shared in a fresh tree");
    assert_eq!(v["dataless"], false);
    assert_eq!(v["category"], "regenerableArtifact", "fresh project: not dormant");
    assert_eq!(v["hotspot"]["ruleId"], "cargo-target");
    assert_eq!(v["hotspot"]["riskTier"], "safe");
    assert_eq!(v["hotspot"]["command"], "cargo clean");
    assert_eq!(v["hotspot"]["matchedBy"], "topPath");
    assert_eq!(v["unreadableBelowCount"], 0);
    let summary = v["summary"].as_str().unwrap();
    assert!(summary.contains("classified regenerableArtifact (safe)"), "{summary}");

    // Ordinary content: no hotspot, an honest sentence.
    let assets = root.join("proj").join("assets");
    let v: serde_json::Value = s
        .get_q(&format!("/scans/{id}/explain"), &[("path", assets.to_str().unwrap())])
        .await
        .json()
        .await
        .unwrap();
    assert!(v["hotspot"].is_null());
    assert!(v["category"].is_null());
    assert!(v["summary"].as_str().unwrap().contains("not a hotspot"));

    // A directory folded by the 1.1.1 rule (src/ holds 13 bytes) has no row
    // to explain: 404, and the body says why and where its bytes went —
    // the project root, the nearest ancestor with a row. Mutation-proof:
    // a bare "not found" fails the `contains` checks.
    let src = root.join("proj").join("src");
    let resp = s.get_q(&format!("/scans/{id}/explain"), &[("path", src.to_str().unwrap())]).await;
    assert_eq!(resp.status(), 404);
    let e: serde_json::Value = resp.json().await.unwrap();
    let msg = e["error"].as_str().unwrap();
    assert!(msg.contains("not individually persisted"), "{msg}");
    assert!(msg.contains(&format!("{:?}", root.join("proj").to_str().unwrap())), "{msg}");

    // Error branches: missing path, unknown path, unknown key, unknown scan.
    assert_eq!(s.get(&format!("/scans/{id}/explain")).await.status(), 400);
    assert_eq!(s.get_q(&format!("/scans/{id}/explain"), &[("path", "/no/such")]).await.status(), 404);
    assert_eq!(s.get_q(&format!("/scans/{id}/explain"), &[("paths", "/x")]).await.status(), 400);
    assert_eq!(
        s.get_q("/scans/e7ae86e2-308b-444c-8a3d-cd21467ab442/explain", &[("path", "/x")]).await.status(),
        404
    );
}

#[tokio::test]
async fn stale_re_thresholds_the_persisted_activity() {
    let s = TestServer::start().await;
    // Sources last edited 200 days ago; the artifact is fresh.
    let root = cargo_project(s._dir.path(), 200);
    let id = s.scan(&root).await;

    // At the default 90 days the project is stale, with its artifact.
    let v: serde_json::Value = s.get(&format!("/scans/{id}/stale")).await.json().await.unwrap();
    assert_eq!(v["thresholdDays"], 90);
    assert_eq!(v["scanId"], id);
    assert_eq!(v["projectsEvaluated"], 1, "{v}");
    assert_eq!(v["unverifiable"], 0);
    assert_eq!(v["projects"].as_array().unwrap().len(), 1, "{v}");
    let p = &v["projects"][0];
    assert_eq!(p["root"], root.join("proj").to_str().unwrap());
    let days = p["lastActivityDays"].as_i64().unwrap();
    assert!((199..=201).contains(&days), "source mtimes, not the fresh artifact: {days}");
    assert_eq!(p["artifacts"][0]["ruleId"], "cargo-target");
    assert_eq!(p["artifacts"][0]["path"], root.join("proj").join("target").to_str().unwrap());
    assert_eq!(p["artifacts"][0]["category"], "staleProjectArtifact");
    assert!(p["artifactDiskSize"].as_u64().unwrap() >= 2 * MIB as u64);
    assert_eq!(v["artifactDiskSize"], p["artifactDiskSize"]);

    // Raise the bar past its age: not stale. Mutation-proof: drop the
    // `days >= threshold` filter and this still lists it.
    let v: serde_json::Value = s
        .get_q(&format!("/scans/{id}/stale"), &[("olderThan", "1y")])
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(v["thresholdDays"], 365);
    assert_eq!(v["projects"], serde_json::json!([]));
    assert_eq!(v["artifactDiskSize"], 0);
    assert_eq!(v["projectsEvaluated"], 1, "evaluated even when none qualifies");

    // Grammar: 3M works, garbage is a 400 naming the grammar.
    assert_eq!(s.get_q(&format!("/scans/{id}/stale"), &[("olderThan", "3M")]).await.status(), 200);
    let resp = s.get_q(&format!("/scans/{id}/stale"), &[("olderThan", "never")]).await;
    assert_eq!(resp.status(), 400);
    let e: serde_json::Value = resp.json().await.unwrap();
    assert!(e["error"].as_str().unwrap().contains("olderThan"), "{e}");
    assert_eq!(s.get_q(&format!("/scans/{id}/stale"), &[("older", "3M")]).await.status(), 400, "unknown key");
}

#[tokio::test]
async fn volume_reports_statfs_and_only_lists_snapshots_when_asked() {
    let s = TestServer::start().await;
    let v: serde_json::Value = s.get("/volume").await.json().await.unwrap();
    assert!(v["totalBytes"].as_u64().unwrap() > 0, "{v}");
    assert!(v["availableBytes"].as_u64().unwrap() <= v["freeBytes"].as_u64().unwrap());
    assert_eq!(v["usedBytes"].as_u64().unwrap(), v["totalBytes"].as_u64().unwrap() - v["freeBytes"].as_u64().unwrap());
    // Phase 4 (phantom-mkn.12 / 4p3): the volume's own usage and purgeable
    // space are filled from getattrlist / CoreFoundation, no subprocess.
    let own = v["volumeUsedBytes"].as_u64().expect("APFS reports its own usage");
    assert!(own <= v["usedBytes"].as_u64().unwrap());
    assert_eq!(v["hidden"]["otherVolumesBytes"].as_u64().unwrap(), v["usedBytes"].as_u64().unwrap() - own);
    // CoreFoundation answers for a GUI user and not for a headless one (the MBP
    // runner's `builder` got 0 back, 2026-09-17). With an answer, Finder's
    // "Available" is at least f_bavail and purgeable is the difference; without
    // one both are null — never a confident 0 (volume::sanitize_capacities).
    match v["importantUsageBytes"].as_u64() {
        Some(important) => {
            let available = v["availableBytes"].as_u64().unwrap();
            assert!(important >= available, "important {important} < available {available}");
            assert_eq!(v["purgeableBytes"].as_u64().unwrap(), important - available);
        }
        None => assert!(v["purgeableBytes"].is_null(), "no CoreFoundation answer must mean purgeable null: {v}"),
    }
    assert_ne!(v["opportunisticUsageBytes"].as_u64(), Some(0), "an opportunistic capacity of 0 is a non-answer");
    assert!(v["snapshotCount"].is_null(), "not asked, not run");
    assert!(v["snapshots"].is_null());
    // No scan named: the split is present with its scan fields null.
    for k in ["scanId", "scanRootPath", "scannedBytes", "unscannedBytes", "unreadableCount", "snapshotSuggestion"] {
        assert!(v["hidden"][k].is_null(), "{k}: {v}");
    }
    assert!(v["hidden"]["otherUserHomes"].is_array());
    assert!(v["mountPoint"].as_str().unwrap().starts_with('/'));
    let default_path = v["path"].as_str().unwrap();
    assert!(default_path == "/System/Volumes/Data" || default_path == "/", "{default_path}");

    // A named path resolves to its mount point.
    let v: serde_json::Value = s.get_q("/volume", &[("path", "/tmp")]).await.json().await.unwrap();
    assert_eq!(v["path"], "/tmp");
    assert!(v["totalBytes"].as_u64().unwrap() > 0);

    // Error branches.
    assert_eq!(s.get_q("/volume", &[("path", "/no/such/mount/anywhere")]).await.status(), 404);
    assert_eq!(s.get_q("/volume", &[("snapshots", "maybe")]).await.status(), 400);
    assert_eq!(s.get_q("/volume", &[("snapshot", "true")]).await.status(), 400, "unknown key");
    assert_eq!(s.get_q("/volume", &[("scanId", "not-a-uuid")]).await.status(), 400);
    assert_eq!(
        s.get_q("/volume", &[("scanId", "5e3c1a2b-8d4f-4c6e-9a1b-2f3d4e5f6a7b")]).await.status(),
        404,
        "unknown scan"
    );

    // Asked: the list is an array (possibly empty on a machine without
    // local snapshots) and the count matches it.
    let resp = s.get_q("/volume", &[("snapshots", "true")]).await;
    if resp.status() == 200 {
        let v: serde_json::Value = resp.json().await.unwrap();
        let list = v["snapshots"].as_array().expect("snapshots array when asked");
        assert_eq!(v["snapshotCount"].as_u64().unwrap() as usize, list.len());
        assert!(list.iter().all(|n| n.as_str().unwrap().starts_with("com.apple.TimeMachine.")));
    } else {
        // tmutil refused (e.g. a sandboxed CI runner): the reason is surfaced, not hidden.
        let e: serde_json::Value = resp.json().await.unwrap();
        assert!(e["error"].as_str().unwrap().contains("tmutil"), "{e}");
    }
}

/// The hidden-space split relative to a completed scan (phantom-mkn.12):
/// scannedBytes is the scan's totalDiskSize, unscannedBytes the volume's
/// own usage minus it, unreadableCount the scan's errorCount.
#[tokio::test]
async fn volume_with_a_scan_reports_the_used_minus_scanned_split() {
    let s = TestServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    cargo_project(dir.path(), 200);
    let id = s.scan(&dir.path().join("root")).await;
    let scan: serde_json::Value = s.get(&format!("/scans/{id}")).await.json().await.unwrap();
    let total = scan["totalDiskSize"].as_u64().unwrap();
    assert!(total >= 2 * MIB as u64);

    // The temp dir is on the data volume (or `/`): ask about the scan root
    // itself so the volume is the right one whatever the machine's layout.
    let root = dir.path().join("root");
    let v: serde_json::Value = s
        .get_q("/volume", &[("path", root.to_str().unwrap()), ("scanId", &id)])
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(v["hidden"]["scanId"], id);
    assert_eq!(v["hidden"]["scanRootPath"], root.to_str().unwrap());
    assert_eq!(v["hidden"]["scannedBytes"].as_u64().unwrap(), total);
    assert_eq!(v["hidden"]["unreadableCount"].as_u64().unwrap(), scan["errorCount"].as_u64().unwrap());
    let own = v["volumeUsedBytes"].as_u64().unwrap();
    assert_eq!(v["hidden"]["unscannedBytes"].as_u64().unwrap(), own - total);
    assert!(v["hidden"]["snapshotSuggestion"].is_null(), "snapshots not listed, nothing suggested");

    // A scan of a root on ANOTHER volume is refused: /dev is devfs.
    let resp = s.get_q("/volume", &[("path", "/dev"), ("scanId", &id)]).await;
    assert_eq!(resp.status(), 400);
    let v: serde_json::Value = resp.json().await.unwrap();
    assert!(v["error"].as_str().unwrap().contains("not"), "{v}");

    // A scan that did not complete has no trustworthy total: 409, not a split.
    let failed = phantom_core::Scan {
        id: uuid::Uuid::new_v4(),
        root_path: root.to_str().unwrap().into(),
        status: phantom_core::ScanStatus::Failed,
        started_at: chrono::Utc::now(),
        finished_at: Some(chrono::Utc::now()),
        total_disk_size: 0,
        total_logical_size: 0,
        file_count: 0,
        dir_count: 0,
        error_count: 0,
        unreadable_paths: Some(vec![]),
        total_private_size: Some(0),
        total_shared_size: Some(0),
        failure_reason: Some("interrupted: test".into()),
    };
    s.state.scan_store().insert_scan(&failed, &[], &[], None).unwrap();
    let resp = s.get_q("/volume", &[("path", root.to_str().unwrap()), ("scanId", &failed.id.to_string())]).await;
    assert_eq!(resp.status(), 409);
    let v: serde_json::Value = resp.json().await.unwrap();
    assert!(v["error"].as_str().unwrap().contains("failed"), "{v}");
}
