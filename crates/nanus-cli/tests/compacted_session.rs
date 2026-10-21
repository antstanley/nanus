//! `nanus run --context-mode managed` on a compacted session: refused with `compacted_session`
//! before anything is written or sent, and never for a session that was not compacted.
//!
//! The real binary over a real store in a scratch home. The provider points at a closed local
//! port, so a run that gets as far as a request fails fast and sends nothing anywhere.

// A panic in a test *is* the assertion. The workspace denies the lint for production code.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::process::Command;

use nanus_adapter_store::JsonlStore;
use nanus_domain::{Session, SessionEvent, SessionId, TurnEndReason};
use nanus_ports::StorePort as _;

/// A legacy session with one completed turn, compacted when `compacted` is set.
fn legacy_session(id: &str, compacted: bool) -> Session {
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
    if compacted {
        let marker = session
            .compaction_event("the question was answered".into(), "m".into(), None)
            .unwrap();
        session.append(marker);
    }
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

/// Runs `nanus run --resume <id> --context-mode managed` in a scratch home and configuration.
fn run_managed(home: &Path, id: &str) -> std::process::Output {
    let config = home.join("config.toml");
    std::fs::write(
        &config,
        format!(
            "provider = \"deepseek\"\nbase_url = \"http://127.0.0.1:9\"\nworkspace_root = {:?}\n",
            home.display().to_string()
        ),
    )
    .unwrap();
    Command::new(env!("CARGO_BIN_EXE_nanus"))
        .args([
            "run",
            "--resume",
            id,
            "--context-mode",
            "managed",
            "continue",
        ])
        .env("NANUS_HOME", home)
        .env("NANUS_CONFIG", &config)
        .env("DEEPSEEK_API_KEY", "compacted-session-test-key")
        .output()
        .unwrap()
}

/// The stored log of `id`, as the bytes on disk.
fn stored(home: &Path, id: &str) -> String {
    std::fs::read_to_string(home.join("sessions").join(id).join("session.jsonl")).unwrap()
}

#[test]
fn managed_context_on_a_compacted_session_is_refused_with_compacted_session() {
    let home = tempfile::tempdir().unwrap();
    save(home.path(), &legacy_session("cut", true));
    let before = stored(home.path(), "cut");
    let output = run_managed(home.path(), "cut");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(stderr.contains("compacted_session"), "{stderr}");
    assert!(
        output.stdout.is_empty(),
        "stdout is the answer, and there is none"
    );
    assert_eq!(stored(home.path(), "cut"), before, "nothing was written");
}

#[test]
fn a_session_never_compacted_is_not_refused_as_compacted() {
    // The provider here, at a local endpoint with no managed evidence, cannot carry managed
    // context, so the switch is refused for that reason instead; what is pinned is that the
    // compaction refusal names only a session that was cut.
    let home = tempfile::tempdir().unwrap();
    save(home.path(), &legacy_session("whole", false));
    let output = run_managed(home.path(), "whole");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("compacted_session"), "{stderr}");
    assert!(
        stderr.contains("context policy could not be applied"),
        "{stderr}"
    );
}
