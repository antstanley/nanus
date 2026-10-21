//! A compacted legacy session on the chat-completions wire: the summary is a leading system message
//! after the prompt, and the conversation opens with the first prompt after the cut. A session that
//! never compacts encodes exactly as it always did.

use nanus_domain::context::COMPACTION_LABEL;
use nanus_domain::{Message, Session, SessionEvent, SessionId, TurnEndReason};
use nanus_ports::ChatRequest;
use serde_json::json;

use super::build_request;
use crate::{DeepSeekConfig, MODEL_FLASH};

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
        model: Some(MODEL_FLASH.into()),
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
            .compaction_event(SUMMARY.into(), MODEL_FLASH.into(), None)
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
    ChatRequest::new(MODEL_FLASH, messages)
}

#[test]
fn a_compaction_summary_is_a_leading_system_message_after_the_prompt() {
    let config = DeepSeekConfig::new(MODEL_FLASH, "test-key");
    let body = build_request(&config, &legacy_request(true));
    let turns = body["messages"].as_array().cloned().unwrap_or_default();
    assert_eq!(turns.len(), 3, "{turns:?}");
    assert_eq!(
        turns[0],
        json!({ "role": "system", "content": "you are nanus" })
    );
    assert_eq!(
        turns[1],
        json!({ "role": "system", "content": format!("{COMPACTION_LABEL}\n{SUMMARY}") })
    );
    assert_eq!(turns[2]["role"], json!("user"));
    assert_eq!(turns[2]["content"], json!("second question"));
    assert!(
        !body.to_string().contains("first question"),
        "the cut part is not sent"
    );
}

#[test]
fn a_session_with_no_marker_sends_no_summary() {
    let config = DeepSeekConfig::new(MODEL_FLASH, "test-key");
    let body = build_request(&config, &legacy_request(false));
    let roles: Vec<String> = body["messages"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|turn| turn["role"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(roles, ["system", "user", "assistant", "user"]);
    assert!(!body.to_string().contains("Compacted history"));
    assert_eq!(
        body.to_string(),
        build_request(&config, &full_fold_request()).to_string(),
        "byte for byte the body the whole fold encodes"
    );
}
