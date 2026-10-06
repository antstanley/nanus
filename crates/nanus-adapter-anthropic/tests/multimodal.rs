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
        content_blocks: None,
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
        // Promoted on the live evidence in `docs/vision-evidence.md`, with its own profile.
        let promoted = adapter.capabilities(model);
        assert_eq!(
            promoted.image_input,
            nanus_ports::ImageInputSupport::Supported
        );
        assert_eq!(
            promoted.require_image_profile(model).unwrap().model(),
            model
        );
    }
}

#[tokio::test]
async fn unpromoted_images_are_refused_before_trying_the_configured_http_endpoint() {
    use futures::StreamExt as _;
    for model in [
        "claude-fable-5-1",
        "claude-haiku-4-5-20251001",
        "arbitrary-alias",
    ] {
        let adapter = nanus_adapter_anthropic::AnthropicLlm::new(
            nanus_adapter_anthropic::AnthropicConfig::with_base_url(
                model,
                "fixture-key",
                "http://127.0.0.1:1",
            ),
        )
        .unwrap();
        let events: Vec<_> = adapter
            .stream_chat(ChatRequest::new(model, fixture(model).derive_messages()))
            .collect()
            .await;
        assert!(
            matches!(events.as_slice(), [nanus_ports::LlmEvent::Error(message)] if message.contains("Unknown")),
            "{model}: {events:?}"
        );
    }
}

fn direct_user_session() -> Session {
    let mut session = Session::new(SessionId::new("human-pixels"), 123, "/fictional");
    let mut blocks = vec![ContentBlock::Text("question".into())];
    for (media, bytes) in [("image/png", PNG), ("image/jpeg", JPEG)] {
        blocks.push(ContentBlock::Image {
            media_type: media.into(),
            data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
        });
        blocks.push(ContentBlock::Text(format!("after {media}")));
    }
    session.append(SessionEvent::UserMessage {
        text: "DISPLAY SUMMARY ONLY".into(),
        content_blocks: Some(blocks),
    });
    session
}

#[tokio::test]
async fn direct_user_images_preserve_order_and_pixels_after_store_reload_without_invented_calls() {
    for model in ["claude-opus-5-5", "claude-sonnet-5-5"] {
        let adapter = nanus_adapter_anthropic::AnthropicLlm::new(
            nanus_adapter_anthropic::AnthropicConfig::new(model, "fixture-key"),
        )
        .unwrap();
        let original = direct_user_session();
        for session in [original.clone(), reload(&original).await] {
            let request = ChatRequest::new(model, session.derive_messages()).with_max_tokens(2048);
            let body = adapter.encode(&request);
            assert_eq!(body["messages"].as_array().unwrap().len(), 1);
            let content = &body["messages"][0]["content"];
            assert_eq!(content.as_array().unwrap().len(), 5);
            assert_eq!(content[0]["text"], "question");
            assert_eq!(content[2]["text"], "after image/png");
            assert_eq!(content[4]["text"], "after image/jpeg");
            for (index, media, bytes) in [(1, "image/png", PNG), (3, "image/jpeg", JPEG)] {
                assert_eq!(content[index]["source"]["media_type"], media);
                assert_eq!(
                    decoded(content[index]["source"]["data"].as_str().unwrap()),
                    bytes
                );
            }
            assert!(request.tools.is_empty());
            assert!(!body.to_string().contains("DISPLAY SUMMARY ONLY"));
            assert!(!body.to_string().contains("function_call"));
            assert!(!body.to_string().contains("tool_use_id"));
        }
    }
}

#[test]
fn shared_admission_counts_user_and_tool_pixels_and_validates_typed_text() {
    use nanus_domain::Message;
    use nanus_ports::capabilities::{validate_history_image_input, validate_image_input};
    let model = "claude-opus-5-5";
    let adapter = nanus_adapter_anthropic::AnthropicLlm::new(
        nanus_adapter_anthropic::AnthropicConfig::new(model, "fixture-key"),
    )
    .unwrap();
    let caps = adapter.capabilities(model);
    let mut request =
        ChatRequest::new(model, direct_user_session().derive_messages()).with_max_tokens(2048);
    assert!(validate_image_input(caps, &request).is_ok());
    let estimate = adapter.estimate_request(&request).unwrap();
    assert_eq!(estimate.images, 2);
    assert!(estimate.input_tokens > 0);
    assert!(validate_image_input(nanus_ports::ModelCapabilities::default(), &request).is_err());
    request.model = "unknown-model".into();
    assert!(validate_image_input(caps, &request).is_err());
    request.model = model.into();
    let blocks = request.messages[0].content_blocks().unwrap().to_vec();
    for _ in 0..3 {
        request.messages.push(Message::Tool {
            call_id: ToolCallId::new("fixture"),
            content: "summary".into(),
            content_blocks: Some(blocks.clone()),
            is_error: false,
        });
    }
    assert!(validate_image_input(caps, &request).is_ok());
    assert_eq!(adapter.estimate_request(&request).unwrap().images, 8);
    request.messages.push(request.messages[0].clone());
    assert!(validate_image_input(caps, &request).is_err());
    assert!(validate_history_image_input(caps, model, &request.messages).is_ok());
    for invalid in [
        vec![],
        vec![ContentBlock::Text("x".into()); 33],
        vec![ContentBlock::Image {
            media_type: "image/png".into(),
            data_base64: "broken".into(),
        }],
    ] {
        request.messages = vec![Message::User {
            text: "summary".into(),
            content_blocks: Some(invalid),
        }];
        assert!(validate_image_input(caps, &request).is_err());
        assert!(adapter.estimate_request(&request).is_err());
    }
}
