// JSON-RPC protocol violations against the real phantom-mcp binary
// (phantom-b7u). No API server: every case here is decided before a tool
// runs, or proves that an unreachable API is a TOOL error (isError content),
// never a crash or a hang. One request per line, one response per line.

use std::io::Write;
use std::process::{Command, Stdio};

fn talk(lines: &[&str]) -> Vec<serde_json::Value> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_phantom-mcp"))
        .env("PHANTOM_API_URL", "http://127.0.0.1:1") // nothing listens on port 1
        .env("PHANTOM_API_KEY", "not-a-real-key")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn phantom-mcp");
    {
        let mut stdin = child.stdin.take().unwrap();
        for l in lines {
            writeln!(stdin, "{l}").unwrap();
        }
    } // EOF ends the loop
    let out = child.wait_with_output().expect("phantom-mcp exits");
    assert!(out.status.success(), "phantom-mcp must exit 0 on EOF, got {:?}", out.status);
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("non-JSON line {l:?}: {e}")))
        .collect()
}

fn one(line: &str) -> serde_json::Value {
    let mut v = talk(&[line]);
    assert_eq!(v.len(), 1, "exactly one response for one request: {v:?}");
    v.remove(0)
}

#[test]
fn malformed_json_is_a_parse_error_with_null_id() {
    let r = one("{this is not json");
    assert_eq!(r["jsonrpc"], "2.0");
    assert!(r["id"].is_null());
    assert_eq!(r["error"]["code"], -32700);
    assert!(r["error"]["message"].as_str().unwrap().starts_with("parse error"));
}

#[test]
fn wrong_or_missing_jsonrpc_version_is_invalid_request_naming_the_id() {
    let r = one(r#"{"jsonrpc":"1.0","id":7,"method":"ping"}"#);
    assert_eq!(r["id"], 7);
    assert_eq!(r["error"]["code"], -32600);
    assert!(r["error"]["message"].as_str().unwrap().contains("2.0"), "{r}");
    let r = one(r#"{"id":"abc","method":"ping"}"#);
    assert_eq!(r["id"], "abc");
    assert_eq!(r["error"]["code"], -32600);
}

#[test]
fn a_request_without_an_id_is_a_notification_and_gets_no_reply() {
    // Notification, then a real ping: exactly one line comes back, for the ping.
    let v = talk(&[
        r#"{"jsonrpc":"2.0","method":"tools/list"}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#,
    ]);
    assert_eq!(v.len(), 1, "{v:?}");
    assert_eq!(v[0]["id"], 2);
    assert_eq!(v[0]["result"], serde_json::json!({}));
}

#[test]
fn positional_params_are_invalid_params() {
    let r = one(r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":["health"]}"#);
    assert_eq!(r["error"]["code"], -32602);
    assert!(r["error"]["message"].as_str().unwrap().contains("object"), "{r}");
    let r = one(r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"arguments":{}}}"#);
    assert_eq!(r["error"]["code"], -32602, "no tool name: {r}");
    let r = one(r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"health","arguments":[1,2]}}"#);
    assert_eq!(r["error"]["code"], -32602, "arguments must be an object: {r}");
}

#[test]
fn unknown_method_and_unknown_tool_are_told_apart() {
    let r = one(r#"{"jsonrpc":"2.0","id":6,"method":"resources/list"}"#);
    assert_eq!(r["error"]["code"], -32601, "{r}");
    // An unknown TOOL is a tool-level error (the host asked a valid method
    // with a name we do not have), reported in-band so the agent can read it.
    let r = one(r#"{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"delete_everything","arguments":{}}}"#);
    assert!(r["error"].is_null(), "{r}");
    assert_eq!(r["result"]["isError"], true);
    assert!(r["result"]["content"][0]["text"].as_str().unwrap().contains("unknown tool"), "{r}");
}

#[test]
fn an_unreachable_api_is_a_tool_error_not_a_crash() {
    let r = one(r#"{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"health","arguments":{}}}"#);
    assert_eq!(r["result"]["isError"], true, "{r}");
    let text = r["result"]["content"][0]["text"].as_str().unwrap();
    assert!(!text.is_empty());
    // And the server is still alive for the next line.
    let v = talk(&[
        r#"{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"list_scans","arguments":{}}}"#,
        r#"{"jsonrpc":"2.0","id":11,"method":"ping"}"#,
    ]);
    assert_eq!(v.len(), 2);
    assert_eq!(v[1]["id"], 11);
}

#[test]
fn hostile_argument_types_are_tool_errors_with_readable_text() {
    for (args, needle) in [
        (r#"{"scanId":12345}"#, "scanId"),          // a number where a uuid string belongs
        (r#"{"path":["/a","/b"]}"#, "path"),           // an array where a string belongs
        (r#"{"responseFormat":"yaml"}"#, "responseFormat"),
    ] {
        let line = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"explain_path","arguments":{args}}}}}"#
        );
        let r = one(&line);
        assert_eq!(r["result"]["isError"], true, "{args}: {r}");
        let text = r["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains(needle) || text.contains("path"), "{args} → {text}");
    }
    // Blank lines between requests are skipped, not parse errors.
    let v = talk(&["", "   ", r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#]);
    assert_eq!(v.len(), 1);
}
