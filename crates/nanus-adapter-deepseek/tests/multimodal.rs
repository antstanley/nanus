//! Wire pixel equality for `DeepSeek` chat completions, and the refusals that must hold.
//!
//! These are local encoding proofs. The live acceptance of `deepseek-flash` is recorded in
//! `docs/vision-evidence.md` and exercised by `nanus-bundle/tests/live_vision.rs`.
#![allow(clippy::unwrap_used, clippy::panic)]

use base64::Engine as _;
use futures::StreamExt as _;
use nanus_adapter_deepseek::{DeepSeekConfig, DeepSeekLlm, MODEL_FLASH, MODEL_PRO};
use nanus_domain::{
    ContentBlock, Session, SessionEvent, SessionId, ToolCall, ToolCallId, ToolName,
};
use nanus_ports::{ChatRequest, LlmEvent, LlmPort, StorePort};
use serde_json::json;

const PNG: &[u8] = include_bytes!("../../nanus-domain/tests/data/tiny-green-triangle.png");
const JPEG: &[u8] = include_bytes!("../../nanus-domain/tests/data/tiny-green-triangle.jpg");

fn fixture() -> Session {
    let mut session = Session::new(SessionId::new("wire-pixels"), 123, "/caller");
    session.append(SessionEvent::UserMessage {
        text: "Inspect the fictional images".into(),
    });
    let calls: Vec<_> = ["first", "sibling", "last"]
        .into_iter()
        .map(|id| {
            ToolCall::new(
                ToolCallId::new(id),
                ToolName::new("inspect").unwrap(),
                json!({ "id": id }),
            )
        })
        .collect();
    session.append(SessionEvent::AssistantMessage {
        replay: None,
        text: None,
        reasoning: Some("thinking about the three calls".into()),
        tool_calls: calls,
        usage: None,
        interrupted: false,
        model: Some(MODEL_FLASH.into()),
        effort: None,
    });
    for (id, images) in [
        ("first", vec![("image/png", PNG)]),
        ("sibling", vec![]),
        ("last", vec![("image/jpeg", JPEG)]),
    ] {
        let mut blocks = vec![ContentBlock::Text(format!("original text for {id}"))];
        for (media, bytes) in images {
            blocks.push(ContentBlock::Image {
                media_type: media.into(),
                data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
            });
        }
        blocks.push(ContentBlock::Text(format!("trailing text for {id}")));
        session.append(SessionEvent::ToolResult {
            call_id: ToolCallId::new(id),
            content: "DISPLAY SUMMARY ONLY".into(),
            content_blocks: Some(blocks),
            is_error: id == "last",
        });
    }
    session
}

async fn reload(session: &Session) -> Session {
    let root = tempfile::tempdir().unwrap();
    let store = nanus_adapter_store::JsonlStore::new(root.path())
        .await
        .unwrap();
    store.save(session).await.unwrap();
    store.load(session.id()).await.unwrap()
}

fn decoded(encoded: &str) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .unwrap()
}

/// Every call is answered by a labelled tool message first; the original ordered text and the
/// exact pixels follow as one user message per result, after the whole tool group, so a result
/// is never separated from the assistant turn that is owed it.
#[tokio::test]
async fn tool_pixels_follow_the_tool_group_as_labelled_ordered_user_attachments() {
    let adapter = DeepSeekLlm::new(DeepSeekConfig::new(MODEL_FLASH, "fixture-key")).unwrap();
    let original = fixture();
    for session in [original.clone(), reload(&original).await] {
        let body = adapter.encode(&ChatRequest::new(MODEL_FLASH, session.derive_messages()));
        let messages = body["messages"].as_array().unwrap();
        let roles: Vec<_> = messages
            .iter()
            .map(|message| message["role"].as_str().unwrap())
            .collect();
        assert_eq!(
            roles,
            ["user", "assistant", "tool", "tool", "tool", "user", "user"]
        );
        // The reasoning of the tool-calling turn is still replayed ahead of the results.
        assert_eq!(
            messages[1]["reasoning_content"],
            "thinking about the three calls"
        );
        for (index, id, media, bytes, error) in [
            (5, "first", "image/png", PNG, false),
            (6, "last", "image/jpeg", JPEG, true),
        ] {
            let label = nanus_ports::capabilities::attachment_label(&ToolCallId::new(id), error);
            let content = messages[index]["content"].as_array().unwrap();
            assert_eq!(content[0]["text"], label);
            assert_eq!(content[1]["text"], format!("original text for {id}"));
            assert_eq!(content[3]["text"], format!("trailing text for {id}"));
            assert_eq!(content[2]["type"], "image_url");
            let prefix = format!("data:{media};base64,");
            let url = content[2]["image_url"]["url"].as_str().unwrap();
            assert_eq!(decoded(url.strip_prefix(&prefix).unwrap()), bytes);
        }
        // The tool message itself carries only the label; the pixels are never in it.
        assert_eq!(
            messages[2]["content"],
            nanus_ports::capabilities::attachment_label(&ToolCallId::new("first"), false)
        );
        assert!(!body.to_string().contains("DISPLAY SUMMARY ONLY"));
    }
}

/// A conversation without pixels is encoded exactly as it was before images existed.
#[test]
fn a_conversation_without_pixels_is_unchanged() {
    let adapter = DeepSeekLlm::new(DeepSeekConfig::new(MODEL_FLASH, "fixture-key")).unwrap();
    let plain = ChatRequest::new(MODEL_FLASH, vec![nanus_domain::Message::user("hi")]);
    assert_eq!(
        adapter.encode(&plain)["messages"],
        json!([{ "role": "user", "content": "hi" }])
    );
}

async fn refusal(adapter: &DeepSeekLlm, model: &str) -> String {
    let mut stream = adapter.stream_chat(ChatRequest::new(model, fixture().derive_messages()));
    match stream.next().await {
        Some(LlmEvent::Error(message)) => message,
        other => panic!("expected a refusal before any request, got {other:?}"),
    }
}

/// Pixels reach only the exact model and endpoint with evidence. Everything else is refused
/// locally, before any connection: Pro was shown live not to read a picture, and a changed
/// endpoint or an alias inherits nothing.
#[tokio::test]
async fn pixels_are_refused_before_http_for_everything_without_evidence() {
    let flash = DeepSeekLlm::new(DeepSeekConfig::new(MODEL_FLASH, "fixture-key")).unwrap();
    let pro = refusal(&flash, MODEL_PRO).await;
    assert!(pro.contains("Unsupported"), "{pro}");
    let alias = refusal(&flash, "deepseek-v4-flash").await;
    assert!(alias.to_lowercase().contains("image"), "{alias}");
    let proxy = DeepSeekLlm::new(DeepSeekConfig::with_base_url(
        MODEL_FLASH,
        "fixture-key",
        "http://127.0.0.1:1",
    ))
    .unwrap();
    let proxied = refusal(&proxy, MODEL_FLASH).await;
    assert!(proxied.to_lowercase().contains("image"), "{proxied}");
}
