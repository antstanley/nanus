//! Exact API admission and real encoder/transport boundaries with fictional credentials.

use std::{net::TcpListener, time::Duration};

use futures::StreamExt as _;
use nanus_domain::{Message, ToolCall, ToolCallId, ToolName, ToolSchema};
use nanus_ports::{
    ChatRequest, ImageInputSupport, LlmEvent, LlmPort as _, ModelCapabilities, ReasoningEffort,
    ToolCallSupport,
};
use serde_json::json;

use crate::{
    OpenAiConfig, OpenAiLlm, Protocol, ProtocolPreference, Vendor, ZAI_BASE_URL,
    ZAI_CODING_BASE_URL,
};

const MODELS: [&str; 4] = ["glm-5.3-flashx", "glm-5.3-flash", "glm-5.3", "glm-5.2"];
const EFFORTS: [ReasoningEffort; 7] = [
    ReasoningEffort::None,
    ReasoningEffort::Minimal,
    ReasoningEffort::Low,
    ReasoningEffort::Medium,
    ReasoningEffort::High,
    ReasoningEffort::XHigh,
    ReasoningEffort::Max,
];

fn config(model: &str) -> OpenAiConfig {
    let mut config = OpenAiConfig::new(Vendor::Zai, model, "fictional-key");
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
fn each_exact_api_model_has_independent_text_tools_and_effort_metadata() {
    let llm = OpenAiLlm::new(config("glm-5.3")).expect("adapter");
    for model in MODELS {
        let caps = llm.capabilities(model);
        assert_eq!(caps.context_window_tokens, Some(1_000_000));
        assert_eq!(caps.max_input_tokens, Some(1_000_000));
        assert_eq!(caps.max_output_tokens, Some(131_072));
        assert_eq!(caps.image_profile, None);
        assert_eq!(
            caps.image_input,
            if matches!(model, "glm-5.3" | "glm-5.2") {
                ImageInputSupport::Unsupported
            } else {
                ImageInputSupport::Unknown
            }
        );
        assert_eq!(
            llm.tool_call_support(model, None),
            ToolCallSupport::Supported
        );
        for effort in EFFORTS {
            let allowed = model == "glm-5.2"
                || matches!(
                    effort,
                    ReasoningEffort::Low | ReasoningEffort::High | ReasoningEffort::Max
                );
            assert_eq!(llm.effort_levels(model).contains(&effort), allowed);
            assert_eq!(
                llm.tool_call_support(model, Some(effort)),
                if allowed {
                    ToolCallSupport::Supported
                } else {
                    ToolCallSupport::Unsupported
                }
            );
            assert_eq!(
                llm.estimate_request(&request(model).with_reasoning_effort(effort))
                    .is_ok(),
                allowed
            );
        }
    }
}

#[test]
fn api_evidence_never_leaks_to_coding_gateways_unknown_models_or_other_vendors() {
    for endpoint in [
        ZAI_CODING_BASE_URL,
        "https://gateway.invalid/api/paas/v4",
        "http://api.z.ai/api/paas/v4",
        "https://api.z.ai/api/paas/v4?query=1",
        "https://api.z.ai:443/api/paas/v4",
    ] {
        let llm = OpenAiLlm::new(OpenAiConfig::with_base_url(
            Vendor::Zai,
            "glm-5.3",
            "fictional",
            endpoint,
        ))
        .expect("adapter");
        assert_eq!(llm.capabilities("glm-5.3"), ModelCapabilities::default());
        assert_eq!(
            llm.tool_call_support("glm-5.3", None),
            ToolCallSupport::Unknown
        );
        assert_eq!(llm.config().reasoning_effort(), ReasoningEffort::Medium);
        assert!(llm.estimate_request(&request("glm-5.3")).is_ok());
    }
    let llm = OpenAiLlm::new(config("glm-5.3")).expect("adapter");
    for model in ["glm-future", "glm-5.3-2026", "glm-5.3 ", "gpt-6-astra"] {
        assert_eq!(llm.capabilities(model), ModelCapabilities::default());
        assert_eq!(llm.tool_call_support(model, None), ToolCallSupport::Unknown);
    }
    let llm = OpenAiLlm::new(OpenAiConfig::with_base_url(
        Vendor::OpenAi,
        "glm-5.3",
        "fictional",
        ZAI_BASE_URL,
    ))
    .expect("different vendor");
    assert_eq!(llm.capabilities("glm-5.3"), ModelCapabilities::default());
    assert_eq!(
        llm.tool_call_support("glm-5.3", None),
        ToolCallSupport::Unknown
    );
    let llm = OpenAiLlm::new(OpenAiConfig::with_base_url(
        Vendor::Zai,
        "glm-5.3",
        "fictional",
        format!("{ZAI_BASE_URL}/"),
    ))
    .expect("slash");
    assert_eq!(llm.capabilities("glm-5.3").max_output_tokens, Some(131_072));
}

#[test]
fn api_default_is_valid_and_explicit_config_or_request_efforts_remain_authoritative() {
    for model in MODELS {
        let mut config = config(model);
        assert_eq!(config.reasoning_effort(), ReasoningEffort::Max);
        config.set_reasoning_effort(ReasoningEffort::Medium);
        let llm = OpenAiLlm::new(config).expect("explicit config");
        assert_eq!(llm.reasoning_effort(model), Some(ReasoningEffort::Medium));
        assert_eq!(
            llm.estimate_request(&request(model)).is_ok(),
            model == "glm-5.2"
        );
        let overridden = request(model).with_reasoning_effort(ReasoningEffort::Low);
        assert!(llm.estimate_request(&overridden).is_ok());
        assert_eq!(llm.encode(&overridden)["reasoning_effort"], "low");
    }
    let mut changed =
        OpenAiConfig::with_base_url(Vendor::Zai, "glm-5.3", "fictional", ZAI_CODING_BASE_URL);
    changed.set_base_url(ZAI_BASE_URL).expect("endpoint edit");
    assert_eq!(changed.reasoning_effort(), ReasoningEffort::Medium);
    let llm = OpenAiLlm::new(changed).expect("adapter");
    assert!(llm.estimate_request(&request("glm-5.3")).is_err());
}

fn replay(model: &str) -> ChatRequest {
    let calls = ["first", "second"].map(|id| {
        ToolCall::new(
            ToolCallId::new(id),
            ToolName::new("inspect").expect("name"),
            json!({"item":id}),
        )
    });
    let mut request = request(model);
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
    request
}

#[test]
fn actual_chat_encoder_keeps_function_definitions_grouped_replay_and_glm52_none() {
    for model in MODELS {
        let llm = OpenAiLlm::new(config(model)).expect("adapter");
        let mut replay = replay(model);
        assert!(replay.tools.is_empty());
        assert!(llm.estimate_request(&replay).is_ok());
        let encoded = llm.encode(&replay);
        assert_eq!(encoded["messages"][1]["tool_calls"][0]["id"], "first");
        assert_eq!(encoded["messages"][1]["tool_calls"][1]["id"], "second");
        assert_eq!(encoded["messages"][2]["tool_call_id"], "first");
        assert_eq!(encoded["messages"][3]["tool_call_id"], "second");
        assert_eq!(encoded["thinking"]["type"], "enabled");
        assert_eq!(encoded["reasoning_effort"], "max");
        assert_eq!(encoded["max_tokens"], 8192);
        assert!(encoded.get("input").is_none());
        replay.tools.push(schema());
        let encoded = llm.encode(&replay);
        assert_eq!(encoded["tools"][0]["type"], "function");
        assert_eq!(encoded["tools"][0]["function"]["name"], "inspect");
        assert!(encoded.get("tool_choice").is_none());
    }
    let llm = OpenAiLlm::new(config("glm-5.2")).expect("adapter");
    for effort in [ReasoningEffort::None, ReasoningEffort::Minimal] {
        let request = replay("glm-5.2").with_reasoning_effort(effort);
        assert!(llm.estimate_request(&request).is_ok());
        assert_eq!(llm.encode(&request)["thinking"]["type"], "enabled");
        assert_eq!(
            llm.encode(&request)["reasoning_effort"],
            crate::effort_spelling(effort)
        );
    }
}

fn context_boundary(llm: &OpenAiLlm, extra_bytes: u32) -> ChatRequest {
    let mut request = ChatRequest::new("glm-5.3", vec![Message::user("")]).with_max_tokens(131_072);
    let framing = llm
        .estimate_request(&request)
        .expect("framing")
        .input_tokens;
    let bytes = 1_000_000_u32
        .checked_sub(131_072)
        .and_then(|n| n.checked_sub(framing))
        .and_then(|n| n.checked_add(extra_bytes))
        .expect("context minus output and framing");
    request.messages = vec![Message::user(
        "a".repeat(usize::try_from(bytes).expect("size")),
    )];
    request
}

#[test]
fn actual_serialized_context_output_and_function_boundaries_are_inclusive() {
    let llm = OpenAiLlm::new(config("glm-5.3")).expect("adapter");
    let mut request = context_boundary(&llm, 0);
    let caps = llm.capabilities(&request.model);
    let estimate = llm.estimate_request(&request).expect("exact context");
    assert_eq!(estimate.input_tokens + estimate.reservation, 1_000_000);
    assert!(estimate.fits(caps, &request));
    assert!(estimate.request_bytes < nanus_domain::content::RECORD_BYTES_MAX);
    request = context_boundary(&llm, 1);
    let above = llm.estimate_request(&request).expect("one byte above");
    assert_eq!(above.input_tokens + above.reservation, 1_000_001);
    assert!(
        !llm.estimate_request(&request)
            .expect("excess context")
            .fits(caps, &request)
    );
    for output in [1, 131_072] {
        assert!(
            llm.estimate_request(&self::request("glm-5.3").with_max_tokens(output))
                .is_ok()
        );
    }
    for output in [0, 131_073, u32::MAX] {
        assert!(
            llm.estimate_request(&self::request("glm-5.3").with_max_tokens(output))
                .is_err()
        );
    }
    let mut request = self::request("glm-5.3");
    request.tools = vec![schema(); 128];
    assert!(llm.estimate_request(&request).is_ok());
    request.tools.push(schema());
    assert!(llm.estimate_request(&request).is_err());
}

fn local_adapter() -> (OpenAiLlm, TcpListener) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    listener.set_nonblocking(true).expect("nonblocking");
    let client = reqwest::Client::builder()
        .no_proxy()
        .resolve("api.z.ai", listener.local_addr().expect("address"))
        .build()
        .expect("local client");
    (
        OpenAiLlm {
            client,
            config: config("glm-5.3"),
        },
        listener,
    )
}

async fn refused(llm: &OpenAiLlm, listener: &TcpListener, request: ChatRequest, reason: &str) {
    assert!(request.context_budget.is_none());
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
async fn invalid_api_text_output_effort_context_and_replay_refuse_before_tcp_without_budget() {
    let (llm, listener) = local_adapter();
    refused(
        &llm,
        &listener,
        request("glm-5.3").with_max_tokens(131_073),
        "output",
    )
    .await;
    for effort in [
        ReasoningEffort::None,
        ReasoningEffort::Minimal,
        ReasoningEffort::Medium,
        ReasoningEffort::XHigh,
    ] {
        refused(
            &llm,
            &listener,
            request("glm-5.3").with_reasoning_effort(effort),
            "reasoning effort",
        )
        .await;
        refused(
            &llm,
            &listener,
            replay("glm-5.3").with_reasoning_effort(effort),
            "tool calling is unsupported",
        )
        .await;
    }
    let request = context_boundary(&llm, 1);
    refused(&llm, &listener, request, "context-fit failure").await;
    let mut request = self::request("glm-5.3");
    request.tools = vec![schema(); 129];
    refused(&llm, &listener, request, "function-tool limit").await;
}

#[tokio::test]
async fn valid_api_request_reaches_local_transport_without_contacting_a_provider() {
    let (llm, listener) = local_adapter();
    let listener = tokio::net::TcpListener::from_std(listener).expect("async listener");
    let mut stream = llm.stream_chat(request("glm-5.3"));
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
