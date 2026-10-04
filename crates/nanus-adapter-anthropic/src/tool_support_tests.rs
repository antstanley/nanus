//! Endpoint isolation and literal effort controls for exact Messages tool models.

use futures::StreamExt as _;
use nanus_domain::{Message, ToolCall, ToolCallId, ToolName};
use nanus_ports::{
    ChatRequest, ImageInputSupport, LlmEvent, LlmPort as _, ReasoningEffort, ToolCallSupport,
};
use serde_json::json;

use crate::{AnthropicConfig, AnthropicLlm, DEFAULT_BASE_URL};

fn request(model: &str) -> ChatRequest {
    let call = ToolCall::new(
        ToolCallId::new("inspect"),
        ToolName::new("inspect").expect("name"),
        json!({}),
    );
    ChatRequest::new(
        model,
        vec![
            Message::user("fictional inspection"),
            Message::assistant(None, None, vec![call]),
            Message::Tool {
                call_id: ToolCallId::new("inspect"),
                content: "fictional result".into(),
                content_blocks: None,
                is_error: false,
            },
        ],
    )
    .with_max_tokens(8192)
}

#[test]
fn exact_messages_models_resolve_their_defaults_and_refuse_effort_coercion() {
    let llm =
        AnthropicLlm::new(AnthropicConfig::new("claude-opus-5-5", "fixture")).expect("adapter");
    for model in ["claude-opus-5-5", "claude-sonnet-5-5", "claude-fable-5-1"] {
        assert_eq!(
            llm.tool_call_support(model, None),
            ToolCallSupport::Supported
        );
        for effort in llm.effort_levels(model) {
            assert_eq!(
                llm.tool_call_support(model, Some(*effort)),
                ToolCallSupport::Supported
            );
        }
        for effort in [ReasoningEffort::None, ReasoningEffort::Minimal] {
            assert_eq!(
                llm.tool_call_support(model, Some(effort)),
                ToolCallSupport::Unsupported
            );
        }
        let request = request(model);
        assert!(request.tools.is_empty());
        assert!(llm.estimate_request(&request).is_ok());
        let encoded = llm.encode(&request);
        assert_eq!(encoded["messages"][1]["content"][0]["type"], "tool_use");
        assert_eq!(encoded["messages"][2]["content"][0]["type"], "tool_result");
        assert_eq!(
            encoded["messages"][2]["content"][0]["tool_use_id"],
            "inspect"
        );
        assert!(encoded.get("tool_choice").is_none());
    }
    assert_eq!(
        llm.tool_call_support("claude-opus-5-5-preview", None),
        ToolCallSupport::Unknown
    );
}

#[test]
fn gateways_paths_and_aliases_inherit_neither_tools_nor_promoted_image_metadata() {
    for endpoint in [
        "https://proxy.example",
        "http://api.anthropic.com/v1",
        "https://api.anthropic.com/v1/other",
        "https://api.anthropic.com.example/v1",
    ] {
        let llm = AnthropicLlm::new(AnthropicConfig::with_base_url(
            "claude-opus-5-5",
            "fixture",
            endpoint,
        ))
        .expect("adapter");
        assert_eq!(
            llm.tool_call_support("claude-opus-5-5", None),
            ToolCallSupport::Unknown
        );
        assert_eq!(
            llm.capabilities("claude-opus-5-5"),
            nanus_ports::ModelCapabilities::default()
        );
    }
    let llm = AnthropicLlm::new(AnthropicConfig::with_base_url(
        "claude-opus-5-5",
        "fixture",
        format!("{DEFAULT_BASE_URL}/"),
    ))
    .expect("slash");
    assert_eq!(
        llm.tool_call_support("claude-opus-5-5", None),
        ToolCallSupport::Supported
    );
    assert_eq!(
        llm.capabilities("claude-opus-5-5").image_input,
        ImageInputSupport::Supported
    );
}

#[tokio::test]
async fn unsupported_literal_effort_refuses_tool_replay_before_tcp_but_preserves_text_only() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listener");
    listener.set_nonblocking(true).expect("nonblocking");
    let client = reqwest::Client::builder()
        .no_proxy()
        .resolve("api.anthropic.com", listener.local_addr().expect("address"))
        .build()
        .expect("client");
    let llm = AnthropicLlm {
        config: AnthropicConfig::new("claude-opus-5-5", "fixture"),
        client,
    };
    for effort in [ReasoningEffort::None, ReasoningEffort::Minimal] {
        let mut request = request("claude-opus-5-5");
        request.reasoning_effort = Some(effort);
        assert!(llm.estimate_request(&request).is_err());
        let events: Vec<_> = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            llm.stream_chat(request).collect(),
        )
        .await
        .expect("bounded preflight");
        assert!(
            matches!(events.as_slice(), [LlmEvent::Error(message)] if message.contains("tool calling is unsupported"))
        );
        assert_eq!(
            listener.accept().expect_err("no contact").kind(),
            std::io::ErrorKind::WouldBlock
        );
        let mut text = ChatRequest::new("claude-opus-5-5", vec![Message::user("fictional text")]);
        text.reasoning_effort = Some(effort);
        assert!(llm.estimate_request(&text).is_ok());
    }
}
