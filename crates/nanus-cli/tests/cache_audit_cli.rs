//! `nanus sessions cache`, end to end: a managed run of the real binary, then the audit of the
//! session it recorded, through the stock adapters, with no credential and no network.
//!
//! This needs the `test-endpoint` feature (`cargo nextest run --workspace --all-features` enables it).
//! The `OpenAI` transport is pointed at a local listener through `NANUS_TEST_OPENAI_ORIGIN`; the
//! support decisions still read the official endpoint, so the managed path is the production one.
//! The audit runs with the same variable, because the endpoint a request was sent to is part of the
//! route its intent recorded, and with no key at all.

#![cfg(feature = "test-endpoint")]
// A panic in a test *is* the assertion. The workspace denies the lint for production code.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::{Value, json};

/// Reads one HTTP request's body from `stream`.
fn read_body(stream: &mut std::net::TcpStream) -> Vec<u8> {
    let mut received = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        let count = stream.read(&mut chunk).unwrap();
        received.extend_from_slice(&chunk[..count]);
        let Some(end) = received.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(&received[..end]).to_ascii_lowercase();
        let length = head
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        let body_end = end.saturating_add(4).saturating_add(length);
        while received.len() < body_end {
            let count = stream.read(&mut chunk).unwrap();
            received.extend_from_slice(&chunk[..count]);
        }
        return received[end.saturating_add(4)..body_end].to_vec();
    }
}

/// Answers each request in turn with the next SSE body.
fn serve(listener: TcpListener, answers: Vec<String>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        for answer in answers {
            let (mut stream, _) = listener.accept().unwrap();
            let _ = read_body(&mut stream);
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\
                 connection: close\r\n\r\n",
                answer.len()
            );
            stream.write_all(head.as_bytes()).unwrap();
            stream.write_all(answer.as_bytes()).unwrap();
        }
    })
}

fn frame(value: &Value) -> String {
    format!("data: {value}\n\n")
}

/// A completion carrying `prompt` input tokens, `cached` of them read from cache.
fn completed(prompt: u64, cached: u64) -> String {
    frame(
        &json!({ "type": "response.completed", "response": { "usage": {
        "input_tokens": prompt,
        "output_tokens": 5,
        "input_tokens_details": { "cached_tokens": cached },
    } } }),
    )
}

/// A response that reads `notes.txt`.
fn read_call(prompt: u64, cached: u64) -> String {
    let arguments = json!({ "file_path": "notes.txt" }).to_string();
    let item = json!({ "type": "function_call", "id": "fc_1", "call_id": "call_1",
        "name": "read", "arguments": arguments });
    let mut sse = frame(&json!({ "type": "response.output_item.added", "item": {
        "type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "read",
        "arguments": "" } }));
    sse.push_str(&frame(
        &json!({ "type": "response.function_call_arguments.delta",
        "item_id": "fc_1", "delta": arguments }),
    ));
    sse.push_str(&frame(
        &json!({ "type": "response.output_item.done", "item": item }),
    ));
    sse.push_str(&completed(prompt, cached));
    sse
}

/// A response that answers `text`.
fn answer(text: &str, prompt: u64, cached: u64) -> String {
    let mut sse = frame(&json!({ "type": "response.output_text.delta", "delta": text }));
    sse.push_str(&completed(prompt, cached));
    sse
}

/// Writes the configuration a run and its audit share.
fn configure(home: &Path) -> std::path::PathBuf {
    let config = home.join("config.toml");
    std::fs::write(
        &config,
        format!(
            "provider = \"openai\"\nmodel = \"gpt-5.6-sol\"\nworkspace_root = {:?}\n",
            home.display().to_string()
        ),
    )
    .unwrap();
    config
}

/// Runs the binary in `home` with `arguments`, against `origin`, with or without a key.
fn nanus(home: &Path, origin: &str, key: bool, arguments: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_nanus"));
    command
        .args(arguments)
        .env("NANUS_HOME", home)
        .env("NANUS_CONFIG", configure(home))
        .env("NANUS_TEST_OPENAI_ORIGIN", origin)
        .env_remove("OPENAI_API_KEY");
    if key {
        command.env("OPENAI_API_KEY", "cli-test-key");
    }
    command.output().unwrap()
}

/// A managed two-step turn, recorded in a scratch home: a read, then the answer, whose request
/// the provider reports as having read the whole first one from cache.
fn recorded() -> (tempfile::TempDir, String) {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("notes.txt"), "the parser is done\n").unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = serve(
        listener,
        vec![read_call(9_000, 0), answer("done", 9_200, 9_000)],
    );
    let output = nanus(
        home.path(),
        &origin,
        true,
        &[
            "run",
            "--name",
            "audited",
            "--context-mode",
            "managed",
            "read the notes",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    server.join().unwrap();
    (home, origin)
}

#[test]
fn a_managed_run_audits_as_digest_verified_and_stable_with_no_key() {
    let (home, origin) = recorded();
    let output = nanus(
        home.path(),
        &origin,
        false,
        &["sessions", "cache", "audited", "--json"],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    let audit: Value = serde_json::from_slice(&output.stdout).unwrap();
    let requests = audit["requests"].as_array().unwrap();
    assert_eq!(requests.len(), 2, "{audit:#}");
    for request in requests {
        assert_eq!(request["verification"]["basis"], "digest", "{audit:#}");
        assert_eq!(request["verification"]["retention"], "short", "{audit:#}");
    }
    assert_eq!(requests[0]["attribution"]["class"], "first");
    assert_eq!(
        requests[0]["cache_read"],
        json!({ "state": "reported", "tokens": 0 })
    );
    assert_eq!(requests[1]["attribution"]["class"], "hit", "{audit:#}");
    assert_eq!(audit["summary"]["digest_verified"], 2);
    assert_eq!(audit["summary"]["read_percent"], 49);
}

#[test]
fn the_table_is_on_stdout_and_says_what_was_verified() {
    let (home, origin) = recorded();
    let output = nanus(
        home.path(),
        &origin,
        false,
        &["sessions", "cache", "audited"],
    );
    assert!(output.status.success());
    let table = String::from_utf8_lossy(&output.stdout);
    assert!(table.starts_with("cache audit of session "), "{table}");
    assert!(table.contains("prefix stable, hit"), "{table}");
    assert!(table.contains("2 verified by digest"), "{table}");
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn an_audit_on_another_endpoint_is_unverified_rather_than_guessed() {
    let (home, _) = recorded();
    // The route the intents recorded names the endpoint the requests went to; an audit that
    // would send elsewhere cannot have prepared them, so it claims nothing.
    let output = nanus(
        home.path(),
        "http://127.0.0.1:9",
        false,
        &["sessions", "cache", "audited", "--json"],
    );
    assert!(output.status.success());
    let audit: Value = serde_json::from_slice(&output.stdout).unwrap();
    for request in audit["requests"].as_array().unwrap() {
        assert_eq!(request["verification"]["basis"], "unverified", "{audit:#}");
        assert_eq!(request["attribution"]["class"], "unverified", "{audit:#}");
    }
    assert_eq!(audit["summary"]["digest_verified"], 0);
}

#[test]
fn a_session_that_cannot_be_read_fails_the_audit() {
    let home = tempfile::tempdir().unwrap();
    let output = nanus(
        home.path(),
        "http://127.0.0.1:9",
        false,
        &["sessions", "cache", "no-such-session"],
    );
    assert!(!output.status.success());
    assert!(output.stdout.is_empty(), "nothing is printed as an answer");
}
