//! `nanus run --resume <id> --context-mode managed --context-output-reserve model`, end to end: the
//! real binary writes the following policy into the stored session at version 6, and its turn reserves
//! the model's ceiling on the wire.
//!
//! This needs the `test-endpoint` feature (`cargo nextest run --workspace --all-features` enables it).
//! The `OpenAI` transport is pointed at a local listener through `NANUS_TEST_OPENAI_ORIGIN`; the support
//! decisions still read the official endpoint, so the managed path is the production one.

#![cfg(feature = "test-endpoint")]
// A panic in a test *is* the assertion. The workspace denies the lint for production code.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::path::Path;
use std::process::Command;

use nanus_adapter_store::JsonlStore;
use nanus_domain::{Session, SessionEvent, SessionId, TurnEndReason};
use nanus_ports::StorePort as _;

/// A legacy session with one completed turn, not yet managed.
fn legacy_session(id: &str) -> Session {
    let mut session = Session::new(SessionId::new(id), 0, "/w");
    session.append(SessionEvent::TurnStart { turn: 0 });
    session.append(SessionEvent::UserMessage {
        text: "question".into(),
        content_blocks: None,
    });
    session.append(SessionEvent::AssistantMessage {
        replay: None,
        text: Some("answer".into()),
        reasoning: None,
        tool_calls: Vec::new(),
        usage: None,
        interrupted: false,
        model: None,
        effort: None,
    });
    session.append(SessionEvent::TurnEnd {
        turn: 0,
        reason: TurnEndReason::Completed,
    });
    session
}

/// Saves `session` under `home`, as the store the binary opens there would.
fn save(home: &Path, session: &Session) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let store = JsonlStore::new(home).await.unwrap();
        store.save(session).await.unwrap();
    });
}

/// Serves one request with a Responses answer, and returns the body it received.
fn serve_once(listener: TcpListener) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
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
            received = received[end.saturating_add(4)..body_end].to_vec();
            break;
        }
        let answer = format!(
            "data: {}\n\ndata: {}\n\n",
            serde_json::json!({ "type": "response.output_text.delta", "delta": "ok" }),
            serde_json::json!({ "type": "response.completed", "response": {} })
        );
        let head = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\
             connection: close\r\n\r\n",
            answer.len()
        );
        stream.write_all(head.as_bytes()).unwrap();
        stream.write_all(answer.as_bytes()).unwrap();
        received
    })
}

#[test]
fn a_resumed_session_asking_for_model_records_a_following_policy_and_reserves_the_ceiling() {
    let home = tempfile::tempdir().unwrap();
    save(home.path(), &legacy_session("resume-me"));
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = serve_once(listener);

    let config = home.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            "provider = \"openai\"\nmodel = \"gpt-5.6-sol\"\nworkspace_root = {:?}\n",
            home.path().display().to_string()
        ),
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_nanus"))
        .args([
            "run",
            "--resume",
            "resume-me",
            "--context-mode",
            "managed",
            "--context-output-reserve",
            "model",
            "continue",
        ])
        .env("NANUS_HOME", home.path())
        .env("NANUS_CONFIG", &config)
        .env("OPENAI_API_KEY", "cli-test-key")
        .env("NANUS_TEST_OPENAI_ORIGIN", &origin)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");

    let sent: serde_json::Value = serde_json::from_slice(&server.join().unwrap()).unwrap();
    assert_eq!(
        sent["max_output_tokens"],
        serde_json::json!(128_000),
        "the model's ceiling"
    );

    let stored =
        std::fs::read_to_string(home.path().join("sessions/resume-me/session.jsonl")).unwrap();
    let header: serde_json::Value = serde_json::from_str(stored.lines().next().unwrap()).unwrap();
    assert_eq!(
        header["version"],
        serde_json::json!(6),
        "a following policy is version six"
    );
    assert!(
        stored.contains("\"output_reserve_follows_model\":true"),
        "the recorded policy follows the model"
    );
}
