//! A compacted legacy session on the Messages wire: the summary is a leading system message, so it
//! joins the top-level `system` field after the prompt, and the conversation opens with the first
//! prompt after the cut. A session that never compacts encodes exactly as it always did.

use nanus_domain::context::COMPACTION_LABEL;
use nanus_domain::{Message, Session, SessionEvent, SessionId, TurnEndReason};
use nanus_ports::ChatRequest;
use serde_json::json;

use super::build_request;
use crate::AnthropicConfig;

const MODEL: &str = "claude-sonnet-4-20250514";
const SUMMARY: &str = "## Decisions\n- use sqlite\n[the plan](docs/plan.md)";

/// One completed turn: a prompt, an answer, and its ending.
fn answered(session: &mut Session, turn: u32, prompt: &str, answer: &str) {
    session.append(SessionEvent::TurnStart { turn });
    session.append(SessionEvent::UserMessage {
        text: prompt.into(),
        content_blocks: None,
    });
    session.append(SessionEvent::AssistantMessage {
        replay: None,
        text: Some(answer.into()),
        reasoning: None,
        tool_calls: Vec::new(),
        usage: None,
        interrupted: false,
        model: Some(MODEL.into()),
        effort: None,
    });
    session.append(SessionEvent::TurnEnd {
        turn,
        reason: TurnEndReason::Completed,
    });
}

/// The legacy request a turn after the first would send, cut by a marker when `compact` is set.
fn legacy_request(compact: bool) -> ChatRequest {
    prompt_request(compact, Session::derive_prompt_messages)
}

/// The request the same uncut session sent before compaction existed: the whole fold of its log.
fn full_fold_request() -> ChatRequest {
    prompt_request(false, Session::derive_messages)
}

/// The request built from the session `compact` describes, folded by `fold`.
fn prompt_request(compact: bool, fold: fn(&Session) -> Vec<Message>) -> ChatRequest {
    let mut session = Session::new(SessionId::new("legacy-wire"), 0, "/w");
    answered(&mut session, 0, "first question", "first answer");
    if compact {
        let marker = session
            .compaction_event(SUMMARY.into(), MODEL.into(), None)
            .unwrap();
        session.append(marker);
    }
    session.append(SessionEvent::TurnStart { turn: 1 });
    session.append(SessionEvent::UserMessage {
        text: "second question".into(),
        content_blocks: None,
    });
    assert!(
        session.try_to_jsonl().is_ok(),
        "a session the store would write"
    );
    let mut messages = vec![Message::system("you are nanus")];
    messages.extend(fold(&session));
    ChatRequest::new(MODEL, messages)
}

#[test]
fn a_compaction_summary_is_the_system_block_after_the_prompt() {
    let body = build_request(
        &AnthropicConfig::new(MODEL, "test-key"),
        &legacy_request(true),
    );
    // Its own block after the prompt's, so the prompt's block can be a cache entry every session
    // shares; a signature binds to that block form too.
    assert_eq!(
        body["system"],
        json!([
            { "type": "text", "text": "you are nanus" },
            { "type": "text", "text": format!("{COMPACTION_LABEL}\n{SUMMARY}") },
        ])
    );
    let canonical = super::encode_canonical(
        &AnthropicConfig::new(MODEL, "test-key"),
        &legacy_request(true),
    )
    .0;
    assert_eq!(canonical["system"], body["system"]);
    let turns = body["messages"].as_array().cloned().unwrap_or_default();
    assert_eq!(turns.len(), 1, "only the prompt after the cut: {turns:?}");
    assert_eq!(turns[0]["role"], json!("user"));
    assert!(
        !body.to_string().contains("first question"),
        "the cut part is not sent"
    );
}

#[test]
fn a_session_with_no_marker_sends_no_summary() {
    let body = build_request(
        &AnthropicConfig::new(MODEL, "test-key"),
        &legacy_request(false),
    );
    assert_eq!(
        body["system"],
        json!([{ "type": "text", "text": "you are nanus" }])
    );
    let turns = body["messages"].as_array().cloned().unwrap_or_default();
    assert_eq!(turns.len(), 3, "{turns:?}");
    assert!(!body.to_string().contains("Compacted history"));
    let whole = build_request(
        &AnthropicConfig::new(MODEL, "test-key"),
        &full_fold_request(),
    );
    assert_eq!(
        body.to_string(),
        whole.to_string(),
        "byte for byte the body the whole fold encodes"
    );
}
