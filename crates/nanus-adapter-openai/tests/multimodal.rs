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

/// A Responses-first model is sent pixels as input items: every call is answered with a label
/// first, then the original ordered text and pixels follow as one user item per result.
#[tokio::test]
async fn a_responses_first_model_gets_labelled_ordered_input_images_after_the_tool_group() {
    let adapter = nanus_adapter_openai::OpenAiLlm::new(nanus_adapter_openai::OpenAiConfig::new(
        nanus_adapter_openai::Vendor::OpenAi,
        "gpt-6-astra",
        "fixture-key",
    ))
    .unwrap();
    let original = fixture("gpt-6-astra");
    for session in [original.clone(), reload(&original).await] {
        let body = adapter.encode(&ChatRequest::new("gpt-6-astra", session.derive_messages()));
        assert!(body.get("messages").is_none(), "{body}");
        let items = body["input"].as_array().unwrap();
        let kinds: Vec<_> = items
            .iter()
            .map(|item| {
                item["type"]
                    .as_str()
                    .or_else(|| item["role"].as_str())
                    .unwrap_or_else(|| panic!("{item}"))
            })
            .collect();
        assert_eq!(
            kinds,
            [
                "user",
                "function_call",
                "function_call",
                "function_call",
                "function_call_output",
                "function_call_output",
                "function_call_output",
                "user",
                "user"
            ]
        );
        for (index, id, media, bytes, error) in [
            (7, "first", "image/png", PNG, false),
            (8, "last", "image/jpeg", JPEG, true),
        ] {
            let content = items[index]["content"].as_array().unwrap();
            assert_eq!(
                content[0]["text"],
                nanus_ports::capabilities::attachment_label(&ToolCallId::new(id), error)
            );
            assert_eq!(content[1]["text"], format!("original text for {id}"));
            assert_eq!(content[3]["text"], format!("trailing text for {id}"));
            assert_eq!(content[2]["type"], "input_image");
            let prefix = format!("data:{media};base64,");
            let url = content[2]["image_url"].as_str().unwrap();
            assert_eq!(decoded(url.strip_prefix(&prefix).unwrap()), bytes);
        }
        assert!(!body.to_string().contains("DISPLAY SUMMARY ONLY"));
    }
    // A conversation without pixels takes the same wire.
    let plain = ChatRequest::new("gpt-6-astra", vec![nanus_domain::Message::user("hi")]);
    assert!(adapter.encode(&plain).get("input").is_some());
}

#[tokio::test]
async fn openai_completes_the_tool_group_before_labelled_ordered_pixel_attachments_after_reload() {
    let adapter = nanus_adapter_openai::OpenAiLlm::new(nanus_adapter_openai::OpenAiConfig::new(
        nanus_adapter_openai::Vendor::OpenAi,
        "gpt-5",
        "fixture-key",
    ))
    .unwrap();
    let original = fixture("gpt-5");
    for session in [original.clone(), reload(&original).await] {
        let encoded = adapter.encode(&ChatRequest::new("gpt-5", session.derive_messages()));
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
    // Promoted on the live evidence in `docs/vision-evidence.md`.
    assert_eq!(
        adapter.capabilities("gpt-6-astra").image_input,
        nanus_ports::ImageInputSupport::Supported
    );
    // Each exact model carries its own profile; a neighbour without evidence has none.
    for model in [
        "gpt-6.1-sol",
        "gpt-6-luna",
        "gpt-5.6-sol",
        "gpt-5.6-terra",
        "gpt-5.6-luna",
    ] {
        let caps = adapter.capabilities(model);
        assert_eq!(caps.image_input, nanus_ports::ImageInputSupport::Supported);
        assert_eq!(caps.require_image_profile(model).unwrap().model(), model);
    }
    for model in ["gpt-5.5", "gpt-5", "gpt-6.2-sol", "gpt-6-astra-mini"] {
        assert_eq!(
            adapter.capabilities(model).image_input,
            nanus_ports::ImageInputSupport::Unknown,
            "{model}"
        );
    }
}

#[tokio::test]
async fn every_unpromoted_vendor_or_model_refuses_pixels_before_http() {
    use futures::StreamExt as _;
    for (vendor, model) in [
        (nanus_adapter_openai::Vendor::OpenAi, "gpt-5.5"),
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
async fn the_chatgpt_backend_supports_every_profiled_model_and_refuses_the_rest_before_http() {
    use futures::StreamExt as _;
    let mut config = nanus_adapter_openai::OpenAiConfig::with_base_url(
        nanus_adapter_openai::Vendor::OpenAi,
        "gpt-5.5",
        "fixture",
        "http://127.0.0.1:1",
    );
    config.set_protocol(nanus_adapter_openai::Protocol::Responses);
    let adapter = nanus_adapter_openai::OpenAiLlm::new(config).unwrap();
    // The backend was run with every profiled model, so those are Supported there too.
    for model in [
        "gpt-6-astra",
        "gpt-6.1-sol",
        "gpt-6-luna",
        "gpt-5.6-sol",
        "gpt-5.6-terra",
        "gpt-5.6-luna",
    ] {
        assert_eq!(
            adapter.capabilities(model).image_input,
            nanus_ports::ImageInputSupport::Supported,
            "{model}"
        );
    }
    // A model with no profile is refused before any HTTP, on this endpoint as on the API.
    let events: Vec<_> = adapter
        .stream_chat(ChatRequest::new(
            "gpt-5.5",
            fixture("gpt-5.5").derive_messages(),
        ))
        .collect()
        .await;
    assert!(
        matches!(events.as_slice(), [nanus_ports::LlmEvent::Error(reason)]
        if reason.contains("Unknown")),
        "{events:?}"
    );
}
