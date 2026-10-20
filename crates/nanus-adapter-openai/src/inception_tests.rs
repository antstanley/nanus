//! Exact Inception admission and real encoder/transport boundaries with fictional credentials.

use std::{net::TcpListener, time::Duration};

use futures::StreamExt as _;
use nanus_domain::{Message, ToolCall, ToolCallId, ToolName, ToolSchema};
use nanus_ports::{
    ChatRequest, ImageInputSupport, LlmEvent, LlmPort as _, ManagedSupport, ModelCapabilities,
    ReasoningEffort, ToolCallSupport,
};
use serde_json::json;

use crate::{INCEPTION_BASE_URL, OpenAiConfig, OpenAiLlm, Protocol, ProtocolPreference, Vendor};

fn config(model: &str) -> OpenAiConfig {
    let mut config = OpenAiConfig::new(Vendor::Inception, model, "fictional-key");
    config
        .set_protocol_preference(ProtocolPreference::Exact(Protocol::ChatCompletions))
        .expect("exact Chat");
    config
}

fn request(model: &str) -> ChatRequest {
    ChatRequest::new(model, vec![Message::user("fictional text")]).with_max_tokens(8192)
}

fn schema() -> ToolSchema {
    ToolSchema {
        name: ToolName::new("inspect").expect("name"),
        description: "fictional inspection".into(),
        parameters: json!({"type":"object"}),
    }
}

#[test]
fn each_exact_api_model_reports_its_documented_limits_and_text_only_input() {
    let llm = OpenAiLlm::new(config("mercury-3")).expect("adapter");
    for (model, context, output) in [
        ("mercury-3", 1_000_000, 131_072),
        ("mercury-2.5", 260_000, 65_536),
        ("mercury-2", 128_000, 50_000),
    ] {
        let caps = llm.capabilities(model);
        assert_eq!(caps.context_window_tokens, Some(context), "{model}");
        assert_eq!(caps.max_input_tokens, Some(context), "{model}");
        assert_eq!(caps.max_output_tokens, Some(output), "{model}");
        // The service reports text as the only input modality, so this is a refusal rather
        // than the absence of evidence.
        assert_eq!(caps.image_input, ImageInputSupport::Unsupported, "{model}");
        assert_eq!(caps.image_profile, None, "{model}");
        assert!(
            llm.tool_call_support(model, None) == ToolCallSupport::Supported,
            "{model} lists tools"
        );
        for effort in [
            None,
            Some(ReasoningEffort::Minimal),
            Some(ReasoningEffort::Low),
            Some(ReasoningEffort::Medium),
            Some(ReasoningEffort::High),
            Some(ReasoningEffort::XHigh),
            Some(ReasoningEffort::Max),
        ] {
            assert_eq!(
                llm.tool_call_support(model, effort),
                ToolCallSupport::Supported,
                "{model}: {effort:?}"
            );
        }
        assert_eq!(
            llm.effort_levels(model),
            &[
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::High
            ][..],
            "{model}"
        );
        assert_eq!(
            llm.reasoning_effort(model),
            Some(ReasoningEffort::Medium),
            "{model}"
        );
    }
    // Every id the vendor offers has exact limits recorded, so the two lists cannot drift.
    for model in crate::inception::MODELS {
        assert!(
            llm.capabilities(model).max_output_tokens.is_some(),
            "{model} is offered but has no recorded ceiling"
        );
    }
}

#[test]
fn every_neutral_effort_except_none_names_a_value_the_provider_documents() {
    let llm = OpenAiLlm::new(config("mercury-3")).expect("adapter");
    for (effort, expected) in [
        (ReasoningEffort::Minimal, "instant"),
        (ReasoningEffort::Low, "low"),
        (ReasoningEffort::Medium, "medium"),
        (ReasoningEffort::High, "high"),
        (ReasoningEffort::XHigh, "high"),
        (ReasoningEffort::Max, "high"),
    ] {
        assert_eq!(crate::inception::effort_spelling(effort), expected);
        let request = request("mercury-3").with_reasoning_effort(effort);
        assert!(llm.estimate_request(&request).is_ok(), "{effort:?}");
        let body = llm.encode(&request);
        assert_eq!(body["reasoning_effort"], expected, "{effort:?}");
        // The four documented values cover every accepted neutral step, and none of them is
        // expressed as a switch the way z.ai's thinking mode is.
        assert!(body.get("thinking").is_none(), "{effort:?}");
    }
    // `none` is not a request to spend less; it is a request to switch reasoning off, and
    // Inception has no control for that. Refuse rather than silently send `instant`.
    assert_eq!(
        crate::inception::effort_spelling(ReasoningEffort::None),
        "instant",
        "the wire spelling stays total for raw inspection and unknown endpoints"
    );
    let refused = request("mercury-3").with_reasoning_effort(ReasoningEffort::None);
    let error = llm
        .estimate_request(&refused)
        .expect_err("Inception has no off switch");
    assert!(error.to_string().contains("disable reasoning"), "{error}");
}

#[test]
fn the_chat_encoder_names_the_output_field_and_the_requested_model() {
    let llm = OpenAiLlm::new(config("mercury-3")).expect("adapter");
    let body = llm.encode(&request("mercury-2.5"));
    assert_eq!(body["model"], "mercury-2.5");
    assert_eq!(body["max_completion_tokens"], 8192);
    assert!(body.get("max_tokens").is_none());
    assert_eq!(body["stream"], true);
    let mut with_tools = request("mercury-3");
    with_tools.tools.push(schema());
    let body = llm.encode(&with_tools);
    assert_eq!(body["tools"][0]["type"], "function");
    assert_eq!(body["tools"][0]["function"]["name"], "inspect");
    assert!(body["tools"][0]["function"].get("strict").is_none());
}

#[test]
fn api_evidence_never_leaks_to_gateways_unknown_models_or_other_vendors() {
    for endpoint in [
        "https://gateway.invalid/v1",
        "http://api.inceptionlabs.ai/v1",
        "https://api.inceptionlabs.ai/v1?query=1",
    ] {
        let llm = OpenAiLlm::new(OpenAiConfig::with_base_url(
            Vendor::Inception,
            "mercury-3",
            "fictional",
            endpoint,
        ))
        .expect("adapter");
        assert_eq!(
            llm.capabilities("mercury-3"),
            ModelCapabilities::default(),
            "{endpoint}"
        );
        assert_eq!(
            llm.tool_call_support("mercury-3", None),
            ToolCallSupport::Unknown,
            "{endpoint}"
        );
    }
    let llm = OpenAiLlm::new(config("mercury-3")).expect("adapter");
    for model in [
        "mercury-4",
        "mercury-3 ",
        "gpt-6-astra",
        "mercury-edit-2",
        "mercury-decide",
        "mercury-voice",
    ] {
        assert_eq!(
            llm.capabilities(model),
            ModelCapabilities::default(),
            "{model}"
        );
        assert_eq!(
            llm.tool_call_support(model, None),
            ToolCallSupport::Unknown,
            "{model}"
        );
    }
    let other = OpenAiLlm::new(OpenAiConfig::with_base_url(
        Vendor::OpenAi,
        "mercury-3",
        "fictional",
        INCEPTION_BASE_URL,
    ))
    .expect("another vendor");
    assert_eq!(
        other.capabilities("mercury-3"),
        ModelCapabilities::default()
    );
    assert_eq!(
        other.tool_call_support("mercury-3", None),
        ToolCallSupport::Unknown
    );
    // A trailing slash names the same endpoint, so its evidence still applies.
    let slash = OpenAiLlm::new(OpenAiConfig::with_base_url(
        Vendor::Inception,
        "mercury-3",
        "fictional",
        format!("{INCEPTION_BASE_URL}/"),
    ))
    .expect("slash");
    assert_eq!(
        slash.capabilities("mercury-3").max_output_tokens,
        Some(131_072)
    );
}

#[test]
fn responses_is_refused_and_automatic_routing_stays_on_chat() {
    let mut config = config("mercury-3");
    assert_eq!(config.protocol_for("mercury-3"), Protocol::ChatCompletions);
    let refused = config.set_protocol_preference(ProtocolPreference::Exact(Protocol::Responses));
    assert!(refused.is_err(), "Inception has no Responses API");
    // A refused policy leaves the configuration unchanged.
    assert_eq!(
        config.protocol_preference(),
        ProtocolPreference::Exact(Protocol::ChatCompletions)
    );
    let automatic = OpenAiConfig::new(Vendor::Inception, "mercury-3", "fictional");
    assert_eq!(
        automatic.protocol_for("mercury-3"),
        Protocol::ChatCompletions
    );
    // The exact preference is not the only route: the fallback a library host can set with
    // `set_protocol` is refused at construction as well.
    let mut fallback = OpenAiConfig::new(Vendor::Inception, "mercury-3", "fictional");
    fallback.set_protocol(Protocol::Responses);
    assert!(OpenAiLlm::new(fallback).is_err(), "no Responses endpoint");
}

#[test]
fn output_reservations_outside_the_model_ceiling_are_refused() {
    let llm = OpenAiLlm::new(config("mercury-3")).expect("adapter");
    for (model, at_ceiling, above) in [
        ("mercury-3", 131_072, 131_073),
        ("mercury-2.5", 65_536, 65_537),
        ("mercury-2", 50_000, 50_001),
    ] {
        assert!(
            llm.estimate_request(&request(model).with_max_tokens(at_ceiling))
                .is_ok(),
            "{model} at its ceiling"
        );
        assert!(
            llm.estimate_request(&request(model).with_max_tokens(above))
                .is_err(),
            "{model} above its ceiling"
        );
        assert!(
            llm.estimate_request(&request(model).with_max_tokens(0))
                .is_err(),
            "{model} with no reservation"
        );
    }
    // A model this endpoint has no ceiling for inherits the conservative fallback, which is the
    // smallest offered model's ceiling, so an unknown id is never asked for more than every
    // offered model accepts.
    assert_eq!(
        llm.config().effective_max_tokens_for("mercury-future"),
        50_000
    );
}

#[test]
fn the_documented_temperature_range_is_enforced_before_dispatch() {
    let llm = OpenAiLlm::new(config("mercury-3")).expect("adapter");
    for (temperature, accepted) in [(0.5, true), (1.0, true), (0.4, false), (1.1, false)] {
        assert_eq!(
            llm.estimate_request(&request("mercury-3").with_temperature(temperature))
                .is_ok(),
            accepted,
            "{temperature}"
        );
    }
    let mut below = config("mercury-3");
    below
        .set_temperature(0.25)
        .expect("finite and non-negative");
    let llm = OpenAiLlm::new(below).expect("adapter");
    assert!(llm.estimate_request(&request("mercury-3")).is_err());
}

#[test]
fn exact_chat_models_can_carry_a_managed_projection() {
    let llm = OpenAiLlm::new(config("mercury-3")).expect("adapter");
    for model in crate::inception::MODELS {
        assert_eq!(
            llm.managed_support(model),
            ManagedSupport::Supported { policy_version: 1 },
            "{model}"
        );
    }
    assert_eq!(
        llm.managed_support("mercury-future"),
        ManagedSupport::Unsupported
    );
    let gateway = OpenAiLlm::new(OpenAiConfig::with_base_url(
        Vendor::Inception,
        "mercury-3",
        "fictional",
        "https://gateway.invalid/v1",
    ))
    .expect("gateway");
    assert_eq!(
        gateway.managed_support("mercury-3"),
        ManagedSupport::Unsupported
    );
}

/// An adapter whose client resolves Inception's host to a local listener, so a request that is
/// not refused is visible as TCP contact rather than as a live provider call.
fn local_adapter() -> (OpenAiLlm, TcpListener) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    listener.set_nonblocking(true).expect("nonblocking");
    let client = reqwest::Client::builder()
        .no_proxy()
        .resolve(
            "api.inceptionlabs.ai",
            listener.local_addr().expect("address"),
        )
        .build()
        .expect("local client");
    (
        OpenAiLlm {
            client,
            config: config("mercury-3"),
        },
        listener,
    )
}

/// The fatal case: the request is refused before any TCP contact, and the stream ends.
async fn refused(llm: &OpenAiLlm, listener: &TcpListener, request: ChatRequest, reason: &str) {
    let mut stream = llm.stream_chat(request);
    let event = tokio::time::timeout(Duration::from_secs(1), stream.next())
        .await
        .expect("preflight");
    assert!(matches!(event, Some(LlmEvent::Error(ref message)) if message.contains(reason)));
    assert!(stream.next().await.is_none());
    assert_eq!(
        listener.accept().expect_err("no TCP contact").kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[tokio::test]
async fn invalid_api_output_and_temperature_refuse_before_tcp_without_budgets() {
    let (llm, listener) = local_adapter();
    refused(
        &llm,
        &listener,
        request("mercury-3").with_max_tokens(131_073),
        "output ceiling",
    )
    .await;
    refused(
        &llm,
        &listener,
        request("mercury-2").with_max_tokens(50_001),
        "output ceiling",
    )
    .await;
    refused(
        &llm,
        &listener,
        request("mercury-3").with_temperature(0.25),
        "temperature",
    )
    .await;
    refused(
        &llm,
        &listener,
        request("mercury-3").with_reasoning_effort(ReasoningEffort::None),
        "disable reasoning",
    )
    .await;
    assert!(listener.accept().is_err(), "still no TCP contact");
}

#[tokio::test]
async fn a_valid_api_request_reaches_local_transport_without_contacting_a_provider() {
    let (llm, listener) = local_adapter();
    let listener = tokio::net::TcpListener::from_std(listener).expect("async listener");
    let mut request = request("mercury-3");
    request.tools.push(schema());
    let mut stream = llm.stream_chat(request);
    let (peer, event) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(
            async {
                let (socket, peer) = listener.accept().await.expect("local contact");
                drop(socket);
                peer
            },
            stream.next()
        )
    })
    .await
    .expect("bounded local dispatch");
    assert!(peer.ip().is_loopback());
    assert!(matches!(event, Some(LlmEvent::Error(_))));
}

#[test]
fn a_tool_replay_is_encoded_with_its_call_ids_in_order() {
    let llm = OpenAiLlm::new(config("mercury-3")).expect("adapter");
    let calls = ["first", "second"].map(|id| {
        ToolCall::new(
            ToolCallId::new(id),
            ToolName::new("inspect").expect("name"),
            json!({"item": id}),
        )
    });
    let mut request = request("mercury-3");
    request
        .messages
        .push(Message::assistant(None, None, calls.to_vec()));
    for id in ["first", "second"] {
        request.messages.push(Message::Tool {
            call_id: ToolCallId::new(id),
            content: format!("{id} result"),
            content_blocks: None,
            is_error: id == "second",
        });
    }
    request.tools.push(schema());
    assert!(llm.estimate_request(&request).is_ok());
    let body = llm.encode(&request);
    assert_eq!(body["messages"][1]["tool_calls"][0]["id"], "first");
    assert_eq!(body["messages"][1]["tool_calls"][1]["id"], "second");
    assert_eq!(body["messages"][2]["tool_call_id"], "first");
    assert_eq!(body["messages"][3]["tool_call_id"], "second");
}
