//! A session recorded through one provider resumes on another.
//!
//! The store is shared, and the provider is a setting a reader can change between turns, so a
//! log written through stateless Responses is one every encoder may be handed. Each of them is
//! driven here by its adapter's own `encode` over a session that went through the store's text
//! form and the fold, which is the path a resumed turn takes.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use nanus_adapter_anthropic::{AnthropicConfig, AnthropicLlm};
use nanus_adapter_deepseek::{DeepSeekConfig, DeepSeekLlm};
use nanus_adapter_openai::{OpenAiConfig, OpenAiLlm, Vendor};
use nanus_domain::message::AssistantReplay;
use nanus_domain::{Message, Session, SessionEvent, SessionId};
use nanus_ports::ChatRequest;
use serde_json::{Value, json};

/// A completed Responses turn of opaque reasoning, and of the text it said when it said any.
fn session(text: Option<&str>) -> Vec<Message> {
    let mut blocks = vec![json!({"type":"reasoning", "id":"rs_original", "summary":[],
        "encrypted_content":"opaque-fictional-ciphertext", "status":null, "content":[]})];
    // A replay must say what the turn said, or the log refuses it on reload.
    if let Some(text) = text {
        blocks.push(
            json!({"type":"message", "id":"msg_original", "role":"assistant",
            "status":"completed", "content":[{"type":"output_text", "text":text,
                "annotations":[], "logprobs":[]}]}),
        );
    }
    let replay = AssistantReplay {
        protocol: "openai.responses".into(),
        prefix_digest: "A".repeat(64),
        context_receipt: None,
        blocks,
    };
    let mut session = Session::new(SessionId::new("resumed"), 123, "/fictional");
    session.append(SessionEvent::UserMessage {
        text: "first".into(),
    });
    session.append(SessionEvent::AssistantMessage {
        replay: Some(replay),
        text: text.map(str::to_owned),
        reasoning: None,
        tool_calls: vec![],
        usage: None,
        interrupted: false,
        model: Some("gpt-5.6".into()),
        effort: None,
    });
    session.append(SessionEvent::UserMessage {
        text: "second".into(),
    });
    let loaded = Session::from_jsonl(&session.try_to_jsonl().unwrap()).unwrap();
    loaded.derive_messages()
}

/// Every encoder a resumed turn could reach, each with the body it would send.
fn encoded(messages: &[Message]) -> Vec<(&'static str, Value)> {
    let anthropic = AnthropicLlm::new(AnthropicConfig::new("claude-sonnet-5-5", "key")).unwrap();
    let deepseek = DeepSeekLlm::new(DeepSeekConfig::new("deepseek-flash", "key")).unwrap();
    let chat = OpenAiLlm::new(OpenAiConfig::new(Vendor::OpenAi, "gpt-5", "key")).unwrap();
    let responses = OpenAiLlm::new(OpenAiConfig::new(Vendor::OpenAi, "gpt-5.6", "key")).unwrap();
    let request = |model: &str| ChatRequest::new(model, messages.to_vec());
    vec![
        ("anthropic", anthropic.encode(&request("claude-sonnet-5-5"))),
        ("deepseek", deepseek.encode(&request("deepseek-flash"))),
        ("openai_chat", chat.encode(&request("gpt-5"))),
        ("openai_responses", responses.encode(&request("gpt-5.6"))),
    ]
}

/// The roles of the conversational turns a body sends, whatever its wire calls the list.
fn roles(body: &Value) -> Vec<String> {
    let turns = body
        .get("messages")
        .or_else(|| body.get("input"))
        .and_then(Value::as_array)
        .expect("a body carries its turns");
    turns
        .iter()
        .filter_map(|turn| turn.get("role").and_then(Value::as_str))
        .filter(|role| *role != "system")
        .map(str::to_owned)
        .collect()
}

#[test]
fn a_turn_only_stateless_responses_can_replay_is_skipped_by_every_other_encoder() {
    let messages = session(None);
    assert_eq!(messages.len(), 3, "{messages:?}");
    // The fold keeps it — stateless Responses replays it — so the encoders are what must cope.
    assert!(
        matches!(&messages[1], Message::Assistant { text: None, tool_calls, replay: Some(_), .. }
            if tool_calls.is_empty()),
        "{messages:?}"
    );
    assert!(messages[1].is_replay_only());
    for (wire, body) in encoded(&messages) {
        assert_eq!(roles(&body), ["user", "user"], "{wire}: {body}");
        assert!(
            !body.to_string().contains("opaque-fictional-ciphertext"),
            "{wire} must not send another protocol's ciphertext"
        );
    }
}

#[test]
fn a_responses_turn_that_said_something_still_reaches_every_encoder() {
    let messages = session(Some("the answer"));
    assert!(
        !messages.iter().any(Message::is_replay_only),
        "{messages:?}"
    );
    for (wire, body) in encoded(&messages) {
        assert_eq!(
            roles(&body),
            ["user", "assistant", "user"],
            "{wire}: {body}"
        );
        assert!(body.to_string().contains("the answer"), "{wire}: {body}");
    }
}
