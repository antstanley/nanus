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
async fn openai_completes_the_tool_group_before_labelled_ordered_pixel_attachments_after_reload() {
    let adapter = nanus_adapter_openai::OpenAiLlm::new(nanus_adapter_openai::OpenAiConfig::new(
        nanus_adapter_openai::Vendor::OpenAi,
        "gpt-6-astra",
        "fixture-key",
    ))
    .unwrap();
    let original = fixture("gpt-6-astra");
    for session in [original.clone(), reload(&original).await] {
        let encoded = adapter.encode(&ChatRequest::new("gpt-6-astra", session.derive_messages()));
        let messages = encoded["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 7);
        assert_eq!(
            messages[2..5]
                .iter()
                .map(|message| message["tool_call_id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["first", "sibling", "last"]
        );
        assert!(
            messages[2..5]
                .iter()
                .all(|message| message["role"] == "tool")
        );
        for (index, id, media, bytes, error) in [
            (5, "first", "image/png", PNG, false),
            (6, "last", "image/jpeg", JPEG, true),
        ] {
            assert_eq!(messages[index]["role"], "user");
            let content = messages[index]["content"].as_array().unwrap();
            assert_eq!(
                content[0]["text"],
                nanus_ports::capabilities::attachment_label(&ToolCallId::new(id), error)
            );
            assert_eq!(content[1]["text"], format!("original text for {id}"));
            assert_eq!(content[3]["text"], format!("trailing text for {id}"));
            assert_eq!(content[2]["image_url"]["detail"], "high");
            let prefix = format!("data:{media};base64,");
            let url = content[2]["image_url"]["url"].as_str().unwrap();
            assert_eq!(decoded(url.strip_prefix(&prefix).unwrap()), bytes);
        }
        assert_eq!(
            messages[3]["content"],
            "original text for sibling\ntrailing text for sibling\n"
        );
        assert!(!encoded.to_string().contains("DISPLAY SUMMARY ONLY"));
    }
    assert_eq!(
        adapter.capabilities("gpt-6-astra").image_input,
        nanus_ports::ImageInputSupport::Unknown
    );
}

#[tokio::test]
async fn every_unpromoted_vendor_or_model_refuses_pixels_before_http() {
    use futures::StreamExt as _;
    for (vendor, model) in [
        (nanus_adapter_openai::Vendor::OpenAi, "gpt-6-astra"),
        (nanus_adapter_openai::Vendor::OpenAi, "arbitrary-alias"),
        (nanus_adapter_openai::Vendor::Zai, "gpt-6-astra"),
    ] {
        let adapter = nanus_adapter_openai::OpenAiLlm::new(
            nanus_adapter_openai::OpenAiConfig::with_base_url(
                vendor,
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
            matches!(events.as_slice(), [nanus_ports::LlmEvent::Error(message)] if message.contains("Unknown"))
        );
    }
}

#[tokio::test]
async fn responses_refuses_images_with_a_specific_pre_http_reason() {
    use futures::StreamExt as _;
    let mut config = nanus_adapter_openai::OpenAiConfig::with_base_url(
        nanus_adapter_openai::Vendor::OpenAi,
        "gpt-6-astra",
        "fixture",
        "http://127.0.0.1:1",
    );
    config.set_protocol(nanus_adapter_openai::Protocol::Responses);
    let adapter = nanus_adapter_openai::OpenAiLlm::new(config).unwrap();
    let events: Vec<_> = adapter
        .stream_chat(ChatRequest::new(
            "gpt-6-astra",
            fixture("gpt-6-astra").derive_messages(),
        ))
        .collect()
        .await;
    assert!(
        matches!(events.as_slice(), [nanus_ports::LlmEvent::Error(reason)]
        if reason == "unsupported-image-protocol: OpenAI Responses")
    );
}
