// Integration tests for GET /scans/series (v1.1 Phase 4, phantom-mkn.11):
// the growth series over real HTTP on a temp database — two scans of one
// root with a file added between them, plus the error branches.

use std::path::Path;

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
        let state = AppState::new(ScanStore::open(&dir.path().join("phantom.db")).unwrap(), KEY.into());
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(listener, build_router(state)).await.unwrap();
        });
        Self { base, client: reqwest::Client::new(), _dir: dir }
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

    async fn scan(&self, root: &Path) -> serde_json::Value {
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
            let v: serde_json::Value = self
                .client
                .get(format!("{}/scans/{id}", self.base))
                .header("x-api-key", KEY)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            if v["status"] != "running" {
                assert_eq!(v["status"], "complete", "{v}");
                return v;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("scan never finished");
    }
}

fn write_mib(path: &Path, mib: usize, byte: u8) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, vec![byte; mib * MIB]).unwrap();
}

#[tokio::test]
async fn series_lists_the_roots_completed_scans_oldest_first_with_a_forecast() {
    let s = TestServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    write_mib(&root.join("alpha").join("a.mov"), 2, 1);
    write_mib(&root.join("beta").join("b.zip"), 1, 2);
    let first = s.scan(&root).await;
    // Grow beta by 3 MiB, then rescan: two points, the second bigger.
    write_mib(&root.join("beta").join("more.zip"), 3, 3);
    let second = s.scan(&root).await;
    // A different root must not leak into this root's series.
    let other = dir.path().join("elsewhere");
    write_mib(&other.join("x.bin"), 1, 4);
    s.scan(&other).await;

    let root_s = root.to_str().unwrap();
    let g: serde_json::Value = s.get_q("/scans/series", &[("root", root_s)]).await.json().await.unwrap();
    assert_eq!(g["rootPath"], root_s);
    assert_eq!(g["groupBy"], "total", "default");
    let points = g["points"].as_array().unwrap();
    assert_eq!(points.len(), 2, "{g}");
    assert_eq!(points[0]["scanId"], first["id"]);
    assert_eq!(points[1]["scanId"], second["id"]);
    assert_eq!(points[0]["totalDiskSize"], first["totalDiskSize"]);
    assert_eq!(points[1]["totalDiskSize"], second["totalDiskSize"]);
    assert!(points[1]["totalDiskSize"].as_u64().unwrap() > points[0]["totalDiskSize"].as_u64().unwrap());
    assert_eq!(g["series"], serde_json::json!([{ "key": "total", "values": [first["totalDiskSize"], second["totalDiskSize"]] }]));
    let f = &g["forecast"];
    assert_eq!(f["method"], "linear");
    assert_eq!(f["pointsUsed"], 2);
    // Two scans seconds apart: spanDays is rounded to hundredths on the wire
    // (0.0 here), while the fit itself used the raw milliseconds.
    assert!(f["spanDays"].as_f64().unwrap() >= 0.0);
    assert!(f["bytesPerDay"].as_i64().unwrap() > 0, "grew between the scans: {f}");
    assert_eq!(f["latestBytes"], second["totalDiskSize"]);
    assert!(f["availableBytes"].as_u64().unwrap() > 0, "the temp dir's volume is readable");
    // Two scans milliseconds apart make an enormous slope, so the rounded
    // hundredths can be 0.0 — the branch (growing + readable volume) is what
    // is under test, not the magnitude.
    assert!(f["daysUntilFull"].as_f64().unwrap() >= 0.0);
    assert!(f["projectedFullAt"].as_str().unwrap().ends_with('Z'));
    assert!(f["caveat"].as_str().unwrap().contains("assumes"));
    assert!(!f["caveat"].as_str().unwrap().contains("clone-aware"), "no pre-v5 point here");

    // topLevelDir: alpha and beta are the keys, ranked by the NEWEST scan
    // (beta grew past alpha); no `other` because every byte is in a child dir.
    let g: serde_json::Value = s
        .get_q("/scans/series", &[("root", root_s), ("groupBy", "topLevelDir")])
        .await
        .json()
        .await
        .unwrap();
    let keys: Vec<&str> = g["series"].as_array().unwrap().iter().map(|l| l["key"].as_str().unwrap()).collect();
    assert_eq!(keys, vec!["beta", "alpha"], "{g}");
    let beta = &g["series"][0]["values"];
    assert!(beta[1].as_u64().unwrap() > beta[0].as_u64().unwrap());
    assert_eq!(g["series"][1]["values"][0], g["series"][1]["values"][1], "alpha did not change");

    // extension: zip overtook mov.
    let g: serde_json::Value = s
        .get_q("/scans/series", &[("root", root_s), ("groupBy", "extension")])
        .await
        .json()
        .await
        .unwrap();
    let keys: Vec<&str> = g["series"].as_array().unwrap().iter().map(|l| l["key"].as_str().unwrap()).collect();
    assert_eq!(keys, vec!["zip", "mov"], "{g}");

    // category: no hotspots in this tree → a summary with no groups → the
    // breakdown is empty for every point → no series lines, points intact.
    let g: serde_json::Value = s
        .get_q("/scans/series", &[("root", root_s), ("groupBy", "category")])
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(g["points"].as_array().unwrap().len(), 2);
    assert_eq!(g["series"], serde_json::json!([]));

    // Trailing slash is cosmetic, like the diff endpoint.
    let g: serde_json::Value = s
        .get_q("/scans/series", &[("root", &format!("{root_s}/"))])
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(g["points"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn series_error_branches() {
    let s = TestServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    write_mib(&root.join("a.bin"), 1, 1);
    s.scan(&root).await;
    let root_s = root.to_str().unwrap();

    assert_eq!(s.get_q("/scans/series", &[]).await.status(), 400, "root is required");
    assert_eq!(s.get_q("/scans/series", &[("root", "   ")]).await.status(), 400);
    let resp = s.get_q("/scans/series", &[("root", root_s), ("groupBy", "top-level-dir")]).await;
    assert_eq!(resp.status(), 400, "kebab-case is the CLI's spelling, not the wire's");
    let e: serde_json::Value = resp.json().await.unwrap();
    assert!(e["error"].as_str().unwrap().contains("groupBy"), "{e}");
    assert_eq!(s.get_q("/scans/series", &[("root", root_s), ("group_by", "total")]).await.status(), 400, "unknown key");
    let resp = s.get_q("/scans/series", &[("root", "/no/such/root/anywhere")]).await;
    assert_eq!(resp.status(), 404);
    let e: serde_json::Value = resp.json().await.unwrap();
    assert!(e["error"].as_str().unwrap().contains("no completed scans"), "{e}");

    // One point: no forecast, one value per line.
    let g: serde_json::Value = s.get_q("/scans/series", &[("root", root_s)]).await.json().await.unwrap();
    assert_eq!(g["points"].as_array().unwrap().len(), 1);
    assert!(g["forecast"].is_null());
    assert_eq!(g["series"][0]["values"].as_array().unwrap().len(), 1);
}
