// Run the adapter-free policy/cancellation fixture as a real downstream consumer too.
#[path = "../../../crates/nanus-bundle/tests/embedding.rs"]
mod embedding;

#[cfg(feature = "providers")]
#[test]
fn explicit_provider_dependencies_are_portable() {
    let _ = nanus_adapter_anthropic::AnthropicConfig::new("claude-sonnet-5-5", "fixture");
    let _ = nanus_adapter_openai::OpenAiConfig::new(
        nanus_adapter_openai::Vendor::OpenAi,
        "gpt-6-astra",
        "fixture",
    );
    let _ = nanus_adapter_deepseek::DeepSeekConfig::new("deepseek-flash", "fixture");
}

#[cfg(feature = "providers")]
#[test]
fn every_provider_preserves_plain_text_block_line_boundaries() {
    use nanus_domain::{ContentBlock, Message, ToolCall, ToolCallId, ToolName};
    use nanus_ports::ChatRequest;
    use serde_json::json;
    let call = ToolCall::new(
        ToolCallId::new("first"),
        ToolName::new("inspect").unwrap(),
        json!({}),
    );
    let messages = vec![
        Message::user("inspect"),
        Message::assistant(None, None, vec![call]),
        Message::Tool {
            call_id: ToolCallId::new("first"),
            content: "first\nsecond\n".into(),
            content_blocks: Some(vec![
                ContentBlock::Text("first".into()),
                ContentBlock::Text("second".into()),
            ]),
            is_error: false,
        },
    ];
    let anthropic = nanus_adapter_anthropic::AnthropicLlm::new(
        nanus_adapter_anthropic::AnthropicConfig::new("claude-sonnet-5-5", "fixture"),
    )
    .unwrap();
    let encoded = anthropic.encode(&ChatRequest::new("claude-sonnet-5-5", messages.clone()));
    assert_eq!(
        encoded["messages"][2]["content"][0]["content"],
        "first\nsecond\n"
    );
    let openai = nanus_adapter_openai::OpenAiLlm::new(nanus_adapter_openai::OpenAiConfig::new(
        nanus_adapter_openai::Vendor::OpenAi,
        "gpt-6-astra",
        "fixture",
    ))
    .unwrap();
    assert_eq!(
        // A chat-first model: `gpt-5.6` and later are sent to the Responses API instead.
        openai.encode(&ChatRequest::new("gpt-5", messages.clone()))["messages"][2]["content"],
        "first\nsecond\n"
    );
    let mut config = openai.config().clone();
    config.set_protocol(nanus_adapter_openai::Protocol::Responses);
    let responses = nanus_adapter_openai::OpenAiLlm::new(config).unwrap();
    assert_eq!(
        responses.encode(&ChatRequest::new("gpt-6-astra", messages.clone()))["input"][2]["output"],
        "first\nsecond\n"
    );
    let deepseek = nanus_adapter_deepseek::DeepSeekLlm::new(
        nanus_adapter_deepseek::DeepSeekConfig::new("deepseek-flash", "fixture"),
    )
    .unwrap();
    assert_eq!(
        deepseek.encode(&ChatRequest::new("deepseek-flash", messages))["messages"][2]["content"],
        "first\nsecond\n"
    );
}

#[cfg(feature = "providers")]
#[test]
fn actual_wire_output_is_the_reserved_ceiling_and_unset_requests_keep_config_defaults() {
    use nanus_domain::Message;
    use nanus_ports::ChatRequest;
    let deepseek = nanus_adapter_deepseek::DeepSeekLlm::new(
        nanus_adapter_deepseek::DeepSeekConfig::new("deepseek-flash", "fixture"),
    )
    .unwrap();
    let request = ChatRequest::new("deepseek-flash", vec![Message::user("inspect")]);
    assert_eq!(
        deepseek.encode(&request)["max_tokens"],
        deepseek.config().max_tokens()
    );
    assert_eq!(
        deepseek.encode(&request.with_max_tokens(8192))["max_tokens"],
        8192
    );
}
