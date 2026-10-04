//! Documented text limits and refusal before the adapter can contact an endpoint.

use std::net::TcpListener;

use futures::StreamExt as _;
use nanus_domain::Message;
use nanus_ports::{ChatRequest, ImageInputSupport, LlmEvent, LlmPort as _, ModelCapabilities};

use crate::{DEFAULT_BASE_URL, DeepSeekConfig, DeepSeekLlm, MODEL_FLASH, MODEL_PRO};

fn adapter(config: DeepSeekConfig) -> DeepSeekLlm {
    DeepSeekLlm::new(config).expect("fixture client")
}

fn request(model: &str, output: u32) -> ChatRequest {
    let mut request = ChatRequest::new(model, vec![Message::user("")]);
    request.max_tokens = Some(output);
    request.context_budget = Some(1_048_576);
    request
}

#[test]
fn both_exact_models_report_documented_limits_and_only_flash_reads_images() {
    let llm = adapter(DeepSeekConfig::new(MODEL_PRO, "fixture-key"));
    for (model, image_input, profile) in [
        (
            MODEL_FLASH,
            ImageInputSupport::Supported,
            Some(nanus_ports::ImageProfile::DeepSeekFlashAreaV1),
        ),
        (MODEL_PRO, ImageInputSupport::Unsupported, None),
    ] {
        let caps = llm.capabilities(model);
        assert_eq!(caps.context_window_tokens, Some(1_048_576));
        assert_eq!(caps.max_input_tokens, Some(1_048_576));
        assert_eq!(caps.max_output_tokens, Some(393_216));
        assert_eq!(caps.image_input, image_input);
        assert_eq!(caps.image_profile, profile);
        assert_eq!(caps.require_image_profile(model).is_ok(), profile.is_some());
    }
}

#[test]
fn aliases_custom_models_and_changed_endpoints_do_not_inherit_limits() {
    let llm = adapter(DeepSeekConfig::new(MODEL_FLASH, "fixture-key"));
    for model in [
        "deepseek-chat",
        "deepseek-reasoner",
        "deepseek-v4-flash",
        "deepseek-flash-preview",
        "custom",
        "DEEPSEEK-FLASH",
    ] {
        assert_eq!(llm.capabilities(model), ModelCapabilities::default());
    }
    for base in [
        "https://proxy.example",
        "https://api.deepseek.com/other",
        "http://api.deepseek.com",
        "https://api.deepseek.com.example",
    ] {
        let config = DeepSeekConfig::with_base_url(MODEL_FLASH, "fixture-key", base);
        let llm = adapter(config);
        assert_eq!(llm.capabilities(MODEL_FLASH), ModelCapabilities::default());
        assert_eq!(llm.capabilities(MODEL_PRO), ModelCapabilities::default());
    }
    let config =
        DeepSeekConfig::with_base_url(MODEL_FLASH, "fixture-key", format!("{DEFAULT_BASE_URL}/"));
    assert!(
        adapter(config)
            .capabilities(MODEL_FLASH)
            .max_input_tokens
            .is_some()
    );
}

#[test]
fn actual_encoders_accept_the_exact_output_limit_and_reject_one_more() {
    for model in [MODEL_FLASH, MODEL_PRO] {
        let llm = adapter(DeepSeekConfig::new(model, "fixture-key"));
        let at_limit = request(model, 393_216);
        let estimate = llm.estimate_request(&at_limit).expect("valid reservation");
        assert_eq!(llm.encode(&at_limit)["max_tokens"], 393_216);
        assert_eq!(estimate.reservation, 393_216);
        assert!(estimate.fits(llm.capabilities(model), &at_limit));
        assert!(llm.estimate_request(&request(model, 393_217)).is_err());
        assert!(llm.estimate_request(&request(model, 0)).is_err());
        assert!(llm.estimate_request(&request(model, 1)).is_ok());
    }
}

#[test]
fn assembled_input_plus_reservation_is_checked_below_at_and_above_context() {
    for model in [MODEL_FLASH, MODEL_PRO] {
        let llm = adapter(DeepSeekConfig::new(model, "fixture-key"));
        let mut request = request(model, 393_216);
        let overhead = llm.estimate_request(&request).expect("empty request");
        let remaining = 1_048_576_u32
            .checked_sub(overhead.reservation)
            .and_then(|value| value.checked_sub(overhead.input_tokens))
            .expect("headroom");
        for (delta, fits) in [(-1_i64, true), (0, true), (1, false)] {
            let length = usize::try_from(
                i64::from(remaining)
                    .checked_add(delta)
                    .expect("bounded addition"),
            )
            .expect("positive length");
            request.messages = vec![Message::user("x".repeat(length))];
            let estimate = llm.estimate_request(&request).expect("bounded encoding");
            assert_eq!(estimate.fits(llm.capabilities(model), &request), fits);
            assert_eq!(
                nanus_ports::capabilities::validate_estimate(
                    llm.capabilities(model),
                    &request,
                    estimate
                )
                .is_ok(),
                fits
            );
        }
    }
}

fn local_transport(config: DeepSeekConfig) -> (DeepSeekLlm, TcpListener) {
    // Preserve the exact public endpoint while forcing any mistaken network dispatch
    // to this fixture. No provider credentials, proxy or remote server can be used.
    let listener = TcpListener::bind("127.0.0.1:0").expect("local listener");
    listener.set_nonblocking(true).expect("nonblocking accept");
    let client = reqwest::Client::builder()
        .no_proxy()
        .resolve(
            "api.deepseek.com",
            listener.local_addr().expect("local address"),
        )
        .build()
        .expect("local client");
    (DeepSeekLlm { client, config }, listener)
}

async fn refused_without_contact(llm: &DeepSeekLlm, listener: &TcpListener, request: ChatRequest) {
    let mut stream = llm.stream_chat(request);
    let event = tokio::time::timeout(std::time::Duration::from_secs(1), stream.next())
        .await
        .expect("preflight returns immediately");
    assert!(matches!(event, Some(LlmEvent::Error(ref message))
        if message.contains("output reservation exceeds model ceiling")
        || message.contains("context-fit failure")));
    assert!(stream.next().await.is_none());
    assert_eq!(
        listener.accept().expect_err("no TCP contact").kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[tokio::test]
async fn invalid_request_and_configured_output_refuse_before_tcp_without_context_opt_in() {
    for model in [MODEL_FLASH, MODEL_PRO] {
        let mut config = DeepSeekConfig::new(model, "fixture-key");
        config
            .set_max_tokens(393_217)
            .expect("positive configuration");
        let (llm, listener) = local_transport(config);
        for output in [0, 393_217] {
            let mut request = request(model, output);
            request.context_budget = None;
            refused_without_contact(&llm, &listener, request).await;
        }
        refused_without_contact(
            &llm,
            &listener,
            ChatRequest::new(model, vec![Message::user("hi")]),
        )
        .await;
    }
}

#[tokio::test]
async fn assembled_context_overflow_refuses_before_tcp_and_valid_override_keeps_preflight_policy() {
    let mut config = DeepSeekConfig::new(MODEL_FLASH, "fixture-key");
    config
        .set_max_tokens(393_217)
        .expect("positive configuration");
    let (llm, listener) = local_transport(config);
    let mut oversized = request(MODEL_FLASH, 8_192);
    oversized.messages = vec![Message::user("x".repeat(1_048_576))];
    refused_without_contact(&llm, &listener, oversized).await;
    let mut overridden = request(MODEL_FLASH, 8_192);
    overridden.context_budget = None;
    assert!(!crate::metadata::requires_preflight(
        llm.config(),
        &overridden
    ));
    let unknown = DeepSeekConfig::new("custom-model", "fixture-key");
    assert!(!crate::metadata::requires_preflight(
        &unknown,
        &ChatRequest::new("custom-model", vec![Message::user("hi")])
    ));
}

#[tokio::test]
async fn the_refusal_fixture_routes_a_valid_request_to_its_local_listener() {
    let (llm, listener) = local_transport(DeepSeekConfig::new(MODEL_FLASH, "fixture-key"));
    let listener = tokio::net::TcpListener::from_std(listener).expect("async listener");
    let mut valid = ChatRequest::new(MODEL_FLASH, vec![Message::user("fictional fixture")]);
    valid.max_tokens = Some(8_192);
    let mut stream = llm.stream_chat(valid);
    let (peer, event) = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        tokio::join!(
            async {
                let (socket, peer) = listener.accept().await.expect("fixture TCP contact");
                drop(socket); // End TLS without ever receiving an HTTP provider request.
                peer
            },
            stream.next()
        )
    })
    .await
    .expect("local dispatch is bounded");
    assert!(peer.ip().is_loopback());
    assert!(matches!(event, Some(LlmEvent::Error(_))));
}

#[test]
fn exact_chat_tool_contract_is_independent_of_images_and_uses_existing_effort_mapping() {
    use nanus_ports::{ReasoningEffort, ToolCallSupport};
    let llm = adapter(DeepSeekConfig::new(MODEL_FLASH, "fixture-key"));
    for model in [MODEL_FLASH, MODEL_PRO] {
        for effort in [
            None,
            Some(ReasoningEffort::None),
            Some(ReasoningEffort::Minimal),
            Some(ReasoningEffort::Low),
            Some(ReasoningEffort::Medium),
            Some(ReasoningEffort::High),
            Some(ReasoningEffort::XHigh),
            Some(ReasoningEffort::Max),
        ] {
            assert_eq!(
                llm.tool_call_support(model, effort),
                ToolCallSupport::Supported
            );
        }
    }
    assert_eq!(
        llm.tool_call_support("deepseek-chat", None),
        ToolCallSupport::Unknown
    );
    let proxy = adapter(DeepSeekConfig::with_base_url(
        MODEL_FLASH,
        "fixture-key",
        "https://proxy.example",
    ));
    assert_eq!(
        proxy.tool_call_support(MODEL_FLASH, None),
        ToolCallSupport::Unknown
    );
}

#[test]
fn exact_chat_replays_preserve_reasoning_calls_results_and_no_forced_choice() {
    use nanus_domain::{ToolCall, ToolCallId, ToolName};
    use serde_json::json;
    for model in [MODEL_FLASH, MODEL_PRO] {
        let llm = adapter(DeepSeekConfig::new(model, "fixture-key"));
        let call = ToolCall::new(
            ToolCallId::new("inspect"),
            ToolName::new("inspect").expect("name"),
            json!({}),
        );
        let mut request = ChatRequest::new(
            model,
            vec![
                Message::user("fictional inspection"),
                Message::assistant(None, Some("fictional reasoning".into()), vec![call]),
                Message::Tool {
                    call_id: ToolCallId::new("inspect"),
                    content: "fictional result".into(),
                    content_blocks: None,
                    is_error: false,
                },
            ],
        )
        .with_max_tokens(8192);
        request.context_budget = Some(64000);
        assert!(request.tools.is_empty());
        assert!(llm.estimate_request(&request).is_ok());
        let encoded = llm.encode(&request);
        assert_eq!(
            encoded["messages"][1]["reasoning_content"],
            "fictional reasoning"
        );
        assert_eq!(encoded["messages"][1]["tool_calls"][0]["id"], "inspect");
        assert_eq!(encoded["messages"][2]["tool_call_id"], "inspect");
        assert!(encoded.get("tool_choice").is_none());
    }
}
