//! Wire pixel equality and model-neutral store replay; these are not live promotion evidence.
#![allow(clippy::unwrap_used)]

use base64::Engine as _;
use nanus_domain::{
    ContentBlock, Session, SessionEvent, SessionId, ToolCall, ToolCallId, ToolName,
};
use nanus_ports::{ChatRequest, LlmPort, StorePort};
use serde_json::json;

const PNG: &[u8] = include_bytes!("../../nanus-domain/tests/data/tiny-green-triangle.png");
const JPEG: &[u8] = include_bytes!("../../nanus-domain/tests/data/tiny-green-triangle.jpg");

fn fixture(model: &str) -> Session {
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
        reasoning: None,
        tool_calls: calls,
        usage: None,
        interrupted: false,
        model: Some(model.into()),
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

#[tokio::test]
async fn exact_anthropic_profiles_encode_ordered_pixels_nested_under_original_calls_after_reload() {
    for model in ["claude-opus-5-5", "claude-sonnet-5-5"] {
        let adapter = nanus_adapter_anthropic::AnthropicLlm::new(
            nanus_adapter_anthropic::AnthropicConfig::new(model, "fixture-key"),
        )
        .unwrap();
        let original = fixture(model);
        for session in [original.clone(), reload(&original).await] {
            let encoded = adapter.encode(&ChatRequest::new(model, session.derive_messages()));
            let results = encoded["messages"][2]["content"].as_array().unwrap();
            assert_eq!(
                results
                    .iter()
                    .map(|result| result["tool_use_id"].as_str().unwrap())
                    .collect::<Vec<_>>(),
                ["first", "sibling", "last"]
            );
            assert_eq!(results[2]["is_error"], true);
            assert_eq!(results[0]["content"][0]["text"], "original text for first");
            assert_eq!(results[0]["content"][2]["text"], "trailing text for first");
            assert_eq!(
                results[0]["content"][1]["source"]["media_type"],
                "image/png"
            );
            assert_eq!(
                decoded(results[0]["content"][1]["source"]["data"].as_str().unwrap()),
                PNG
            );
            assert_eq!(
                decoded(results[2]["content"][1]["source"]["data"].as_str().unwrap()),
                JPEG
            );
            assert!(!encoded.to_string().contains("DISPLAY SUMMARY ONLY"));
        }
        assert_eq!(
            adapter.capabilities(model).image_input,
            nanus_ports::ImageInputSupport::Unknown
        );
        assert!(adapter.capabilities(model).image_profile.is_none());
    }
}

#[tokio::test]
async fn unpromoted_images_are_refused_before_trying_the_configured_http_endpoint() {
    use futures::StreamExt as _;
    let adapter = nanus_adapter_anthropic::AnthropicLlm::new(
        nanus_adapter_anthropic::AnthropicConfig::with_base_url(
            "claude-opus-5-5",
            "fixture-key",
            "http://127.0.0.1:1",
        ),
    )
    .unwrap();
    let events: Vec<_> = adapter
        .stream_chat(ChatRequest::new(
            "claude-opus-5-5",
            fixture("claude-opus-5-5").derive_messages(),
        ))
        .collect()
        .await;
    assert!(
        matches!(events.as_slice(), [nanus_ports::LlmEvent::Error(message)] if message.contains("Unknown"))
    );
}
