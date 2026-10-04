//! Real adapter capability, encoder and pre-HTTP boundaries with fictional local fixtures.

use std::net::TcpListener;
use std::time::Duration;

use futures::StreamExt as _;
use nanus_domain::{Message, ToolCall, ToolCallId, ToolName, ToolSchema};
use nanus_ports::{
    ChatRequest, ImageInputSupport, LlmEvent, LlmPort as _, ReasoningEffort, ToolCallSupport,
};
use serde_json::json;

use crate::{
    OPENAI_BASE_URL, OPENAI_SUBSCRIPTION_BASE_URL, OpenAiConfig, OpenAiLlm, Protocol,
    ProtocolPreference, Vendor,
};

fn config(model: &str, protocol: Protocol) -> OpenAiConfig {
    let mut config = OpenAiConfig::new(Vendor::OpenAi, model, "fictional-key");
    config
        .set_protocol_preference(ProtocolPreference::Exact(protocol))
        .expect("exact wire");
    config
}

fn call(id: &str) -> ToolCall {
    ToolCall::new(
        ToolCallId::new(id),
        ToolName::new("inspect").expect("name"),
        json!({"item":id}),
    )
}

fn replay(model: &str) -> ChatRequest {
    ChatRequest::new(
        model,
        vec![
            Message::user("fictional inspection"),
            Message::assistant(None, None, vec![call("first"), call("second")]),
            Message::Tool {
                call_id: ToolCallId::new("first"),
                content: "first result".into(),
                content_blocks: None,
                is_error: false,
            },
            Message::Tool {
                call_id: ToolCallId::new("second"),
                content: "second result".into(),
                content_blocks: None,
                is_error: true,
            },
        ],
    )
    .with_max_tokens(8192)
}

fn definition(model: &str) -> ChatRequest {
    let mut request = ChatRequest::new(model, vec![Message::user("fictional inspection")]);
    request.tools.push(ToolSchema {
        name: ToolName::new("inspect").expect("name"),
        description: "fictional inspection".into(),
        parameters: json!({"type":"object"}),
    });
    request
}

#[test]
fn each_exact_api_responses_model_has_independent_tools_and_effort_support() {
    let llm = OpenAiLlm::new(config("gpt-6-astra", Protocol::Responses)).expect("adapter");
    for model in [
        "gpt-6-astra",
        "gpt-6.1-sol",
        "gpt-6-luna",
        "gpt-5.6-sol",
        "gpt-5.6-terra",
        "gpt-5.6-luna",
    ] {
        assert_eq!(
            llm.tool_call_support(model, None),
            ToolCallSupport::Supported
        );
        for effort in Vendor::OpenAi.effort_levels(model) {
            assert_eq!(
                llm.tool_call_support(model, Some(*effort)),
                ToolCallSupport::Supported
            );
        }
        assert_eq!(
            llm.tool_call_support(model, Some(ReasoningEffort::Minimal)),
            ToolCallSupport::Unsupported
        );
    }
    for model in ["gpt-6-astra", "gpt-6.1-sol"] {
        assert_eq!(
            llm.tool_call_support(model, Some(ReasoningEffort::None)),
            ToolCallSupport::Unsupported
        );
    }
}

#[test]
fn exact_chat_restrictions_and_default_vs_explicit_disabled_reasoning_are_distinct() {
    let llm = OpenAiLlm::new(config("gpt-6-luna", Protocol::ChatCompletions)).expect("adapter");
    for model in ["gpt-6-astra", "gpt-6.1-sol"] {
        for effort in [
            None,
            Some(ReasoningEffort::None),
            Some(ReasoningEffort::Low),
        ] {
            assert_eq!(
                llm.tool_call_support(model, effort),
                ToolCallSupport::Unsupported
            );
        }
    }
    assert_eq!(
        llm.tool_call_support("gpt-6-luna", None),
        ToolCallSupport::Unsupported
    );
    for effort in Vendor::OpenAi.effort_levels("gpt-6-luna") {
        let expected = if *effort == ReasoningEffort::None {
            ToolCallSupport::Supported
        } else {
            ToolCallSupport::Unsupported
        };
        assert_eq!(llm.tool_call_support("gpt-6-luna", Some(*effort)), expected);
    }
    let mut disabled = config("gpt-6-luna", Protocol::ChatCompletions);
    disabled.set_reasoning_effort(ReasoningEffort::None);
    let disabled = OpenAiLlm::new(disabled).expect("disabled config");
    assert_eq!(
        disabled.tool_call_support("gpt-6-luna", None),
        ToolCallSupport::Supported
    );
    assert_eq!(
        disabled.tool_call_support("gpt-6-luna", Some(ReasoningEffort::High)),
        ToolCallSupport::Unsupported
    );
}

#[test]
fn unknown_ids_endpoints_subscription_and_vendors_inherit_no_api_evidence() {
    for endpoint in [
        "https://proxy.example",
        "http://api.openai.com/v1",
        "https://api.openai.com/v1/other",
        "https://api.openai.com.example/v1",
        OPENAI_SUBSCRIPTION_BASE_URL,
    ] {
        let mut custom =
            OpenAiConfig::with_base_url(Vendor::OpenAi, "gpt-6-astra", "fixture", endpoint);
        custom
            .set_protocol_preference(ProtocolPreference::Exact(Protocol::Responses))
            .expect("wire");
        let llm = OpenAiLlm::new(custom).expect("custom adapter");
        assert_eq!(
            llm.tool_call_support("gpt-6-astra", None),
            ToolCallSupport::Unknown
        );
    }
    let llm = OpenAiLlm::new(config("gpt-6-astra", Protocol::Responses)).expect("adapter");
    for model in [
        "gpt-6.9-future",
        "gpt-6",
        "gpt-6.1-sol-preview",
        "GPT-6-ASTRA",
        "custom",
    ] {
        assert_eq!(llm.tool_call_support(model, None), ToolCallSupport::Unknown);
    }
    let llm = OpenAiLlm::new(OpenAiConfig::new(Vendor::Zai, "glm-future", "fixture"))
        .expect("unknown zai");
    assert_eq!(
        llm.tool_call_support("glm-future", None),
        ToolCallSupport::Unknown
    );
    let mut slash = config("gpt-6-astra", Protocol::Responses);
    slash
        .set_base_url(format!("{OPENAI_BASE_URL}/"))
        .expect("slash");
    assert_eq!(
        OpenAiLlm::new(slash)
            .expect("slash adapter")
            .tool_call_support("gpt-6-astra", None),
        ToolCallSupport::Supported
    );
}

#[test]
fn actual_encoders_keep_grouped_replay_and_explicit_none_without_promoting_chat_images() {
    for model in [
        "gpt-6-astra",
        "gpt-6.1-sol",
        "gpt-6-luna",
        "gpt-5.6-sol",
        "gpt-5.6-terra",
        "gpt-5.6-luna",
    ] {
        let llm = OpenAiLlm::new(config(model, Protocol::Responses)).expect("adapter");
        let request = replay(model);
        assert!(request.tools.is_empty());
        assert!(llm.estimate_request(&request).is_ok());
        let encoded = llm.encode(&request);
        assert_eq!(encoded["input"][1]["type"], "function_call");
        assert_eq!(encoded["input"][1]["call_id"], "first");
        assert_eq!(encoded["input"][2]["call_id"], "second");
        assert_eq!(encoded["input"][3]["type"], "function_call_output");
        assert_eq!(encoded["input"][3]["call_id"], "first");
        assert_eq!(encoded["input"][4]["call_id"], "second");
        assert_eq!(encoded["max_output_tokens"], 8192);
    }
    let llm = OpenAiLlm::new(config("gpt-6-luna", Protocol::ChatCompletions)).expect("chat");
    let mut request = replay("gpt-6-luna");
    request.reasoning_effort = Some(ReasoningEffort::None);
    assert!(llm.estimate_request(&request).is_ok());
    let encoded = llm.encode(&request);
    assert_eq!(encoded["reasoning_effort"], "none");
    assert_eq!(encoded["messages"][1]["tool_calls"][0]["id"], "first");
    assert_eq!(encoded["messages"][2]["tool_call_id"], "first");
    assert_eq!(
        llm.capabilities("gpt-6-luna").image_input,
        ImageInputSupport::Unknown
    );
    let responses = OpenAiLlm::new(config("gpt-6.1-sol", Protocol::Responses)).expect("responses");
    assert_eq!(
        responses.capabilities("gpt-6.1-sol").image_input,
        ImageInputSupport::Supported
    );
    assert_eq!(
        responses.tool_call_support("gpt-6.1-sol", Some(ReasoningEffort::None)),
        ToolCallSupport::Unsupported
    );
}

fn local_adapter(config: OpenAiConfig) -> (OpenAiLlm, TcpListener) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("local listener");
    listener.set_nonblocking(true).expect("nonblocking");
    let client = reqwest::Client::builder()
        .no_proxy()
        .resolve("api.openai.com", listener.local_addr().expect("address"))
        .build()
        .expect("fictional local client");
    (OpenAiLlm { client, config }, listener)
}

async fn refused(llm: &OpenAiLlm, listener: &TcpListener, request: ChatRequest) {
    assert!(llm.estimate_request(&request).is_err());
    let mut stream = llm.stream_chat(request);
    let event = tokio::time::timeout(Duration::from_secs(1), stream.next())
        .await
        .expect("preflight");
    assert!(
        matches!(event, Some(LlmEvent::Error(ref message)) if message.contains("tool calling is unsupported"))
    );
    assert!(stream.next().await.is_none());
    assert_eq!(
        listener.accept().expect_err("no TCP contact").kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[tokio::test]
async fn unsupported_definitions_calls_results_and_empty_tool_list_replay_refuse_before_tcp() {
    for model in ["gpt-6-astra", "gpt-6.1-sol", "gpt-6-luna"] {
        let (llm, listener) = local_adapter(config(model, Protocol::ChatCompletions));
        refused(&llm, &listener, definition(model)).await;
        let mut only_call = ChatRequest::new(
            model,
            vec![Message::assistant(None, None, vec![call("first")])],
        );
        only_call.max_tokens = Some(8192);
        refused(&llm, &listener, only_call).await;
        let only_result = ChatRequest::new(
            model,
            vec![replay(model).messages.pop().expect("last result")],
        );
        refused(&llm, &listener, only_result).await;
        refused(&llm, &listener, replay(model)).await;
    }
    let (llm, listener) = local_adapter(config("gpt-6.1-sol", Protocol::Responses));
    let mut request = replay("gpt-6.1-sol");
    request.reasoning_effort = Some(ReasoningEffort::None);
    refused(&llm, &listener, request).await;
}

#[tokio::test]
async fn exact_chat_text_reaches_the_local_transport_and_keeps_its_selected_wire() {
    let (llm, listener) = local_adapter(config("gpt-6.1-sol", Protocol::ChatCompletions));
    let listener = tokio::net::TcpListener::from_std(listener).expect("async listener");
    let request = ChatRequest::new("gpt-6.1-sol", vec![Message::user("fictional text only")]);
    assert!(llm.estimate_request(&request).is_ok());
    assert!(llm.encode(&request).get("messages").is_some());
    assert!(llm.encode(&request).get("input").is_none());
    let mut stream = llm.stream_chat(request);
    let (peer, event) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(
            async {
                let (socket, peer) = listener.accept().await.expect("local contact");
                drop(socket); // End TLS before any provider HTTP request.
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
