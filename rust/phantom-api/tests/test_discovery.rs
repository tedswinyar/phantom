// The API publishes its bound URL to a file beside its database (test
// profile) so the CLI and MCP find it when the registered port is taken —
// the day another vendor's agent sat on 8768–8769 (2026-09-09). Drives the
// real binary: the file must match the stdout announcement byte-for-byte
// and disappear on a graceful stop.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn spawn_api(dir: &std::path::Path) -> Child {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_phantom-api"));
    cmd.env("PHANTOM_PROFILE", "test")
        .env("PHANTOM_DB_PATH", dir.join("phantom.db"))
        .env("PHANTOM_KEY_FILE", dir.join("api_key"))
        .env("PHANTOM_PORT", "0")
        .env_remove("PHANTOM_SUPERVISOR_PID")
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    cmd.spawn().expect("spawn phantom-api")
}

fn wait_for_exit(child: &mut Child, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if child.try_wait().expect("try_wait").is_some() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

#[test]
fn the_published_url_matches_the_announcement_and_is_removed_on_shutdown() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = spawn_api(dir.path());
    let stdout = child.stdout.take().expect("piped stdout");
    let mut line = String::new();
    BufReader::new(stdout).read_line(&mut line).expect("read the announcement");
    let announced = line
        .trim()
        .strip_prefix("phantom-api listening on ")
        .expect("announcement line")
        .to_string();
    assert!(announced.starts_with("http://127.0.0.1:"), "{announced}");

    // The file is written BEFORE the announcement is printed, so it is there
    // as soon as a supervisor has read the line.
    let file = dir.path().join("api_url");
    let published = phantom_core::discovery::read_published_url(&file).expect("api_url published beside the test database");
    assert_eq!(published, announced, "the file and stdout tell the same truth");
    assert_eq!(
        phantom_core::discovery::resolve_api_url(None, Some(&file)),
        (announced.clone(), phantom_core::discovery::UrlSource::PublishedFile)
    );

    // Graceful stop removes it.
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    assert!(wait_for_exit(&mut child, Duration::from_secs(10)), "api exits on SIGTERM");
    assert!(!file.exists(), "a clean shutdown unpublishes its URL");
}

#[test]
fn a_second_server_does_not_lose_its_file_to_the_first_ones_shutdown() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("api_url");
    // Simulate a newer server having taken over the file, then stop the old one.
    let mut old = spawn_api(dir.path());
    let stdout = old.stdout.take().unwrap();
    let mut line = String::new();
    BufReader::new(stdout).read_line(&mut line).unwrap();
    phantom_core::discovery::publish_url(&file, "http://127.0.0.1:65000").unwrap();
    unsafe {
        libc::kill(old.id() as i32, libc::SIGTERM);
    }
    assert!(wait_for_exit(&mut old, Duration::from_secs(10)));
    assert_eq!(
        phantom_core::discovery::read_published_url(&file).as_deref(),
        Some("http://127.0.0.1:65000"),
        "the old server only removes a file that still names ITS url"
    );
}
