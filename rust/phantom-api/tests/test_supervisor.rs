// phantom-85r: the API must not outlive the app that spawned it. The app
// passes its pid as PHANTOM_SUPERVISOR_PID; the server polls it and exits
// cleanly once it is gone. Without this, every crash or force-quit of the
// app leaked a headless loopback server holding a rung of the port ladder
// — ten dev relaunches once filled the whole ladder and the eleventh launch
// failed with "no free port".
//
// These tests drive the REAL binary (the watch lives in main.rs, not the
// router) with a throwaway "supervisor": a `sleep` child whose pid stands in
// for the app's.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn spawn_api(supervisor_pid: Option<u32>, dir: &std::path::Path) -> Child {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_phantom-api"));
    cmd.env("PHANTOM_PROFILE", "test")
        .env("PHANTOM_DB_PATH", dir.join("phantom.db"))
        .env("PHANTOM_KEY_FILE", dir.join("api_key"))
        .env("PHANTOM_PORT", "0")
        .env_remove("PHANTOM_SUPERVISOR_PID")
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(pid) = supervisor_pid {
        cmd.env("PHANTOM_SUPERVISOR_PID", pid.to_string());
    }
    let mut child = cmd.spawn().expect("phantom-api spawns");
    // Wait for the port announcement: the server is up and its watch is
    // installed before we do anything to the supervisor.
    let stdout = child.stdout.take().unwrap();
    let mut line = String::new();
    BufReader::new(stdout).read_line(&mut line).unwrap();
    assert!(
        line.starts_with("phantom-api listening on"),
        "expected the announcement, got {line:?}"
    );
    child
}

/// Poll `try_wait` until the child exits or `timeout` passes.
fn wait_for_exit(child: &mut Child, timeout: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            return Some(status);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

#[test]
fn api_exits_when_its_supervisor_dies() {
    let dir = tempfile::tempdir().unwrap();
    let mut supervisor = Command::new("/bin/sleep")
        .arg("60")
        .spawn()
        .expect("sleep spawns");
    let mut api = spawn_api(Some(supervisor.id()), dir.path());

    // Alive while the supervisor is: the watch must not fire on a live pid.
    assert!(
        wait_for_exit(&mut api, Duration::from_millis(1500)).is_none(),
        "api exited while its supervisor was still alive"
    );

    supervisor.kill().unwrap();
    supervisor.wait().unwrap(); // reaped: the pid is truly gone, not a zombie

    let status = wait_for_exit(&mut api, Duration::from_secs(5))
        .expect("api must exit within 5s of its supervisor dying");
    assert!(status.success(), "the exit is a clean shutdown, got {status}");
}

#[test]
fn api_without_a_supervisor_stays_up() {
    let dir = tempfile::tempdir().unwrap();
    let mut api = spawn_api(None, dir.path());
    assert!(
        wait_for_exit(&mut api, Duration::from_millis(1500)).is_none(),
        "with no PHANTOM_SUPERVISOR_PID the api must live until signalled"
    );
    api.kill().unwrap();
    api.wait().unwrap();
}

#[test]
fn unparsable_supervisor_pid_is_ignored_not_fatal() {
    let dir = tempfile::tempdir().unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_phantom-api"));
    cmd.env("PHANTOM_PROFILE", "test")
        .env("PHANTOM_DB_PATH", dir.path().join("phantom.db"))
        .env("PHANTOM_KEY_FILE", dir.path().join("api_key"))
        .env("PHANTOM_PORT", "0")
        .env("PHANTOM_SUPERVISOR_PID", "not-a-pid")
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut api = cmd.spawn().unwrap();
    let mut line = String::new();
    BufReader::new(api.stdout.take().unwrap()).read_line(&mut line).unwrap();
    assert!(line.starts_with("phantom-api listening on"), "got {line:?}");
    assert!(wait_for_exit(&mut api, Duration::from_millis(1200)).is_none());
    api.kill().unwrap();
    api.wait().unwrap();
}
