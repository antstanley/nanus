use nanus_domain::{Message, ToolCall, ToolCallId, ToolName, ToolSchema};
use serde_json::json;

use crate::{ChatRequest, LlmPort, LlmStream, ReasoningEffort, ToolCallSupport};

use super::{has_tool_context, validate_input};

struct Legacy;
impl LlmPort for Legacy {
    fn model(&self) -> &'static str {
        "legacy"
    }
    fn stream_chat(&self, _request: ChatRequest) -> LlmStream {
        Box::pin(futures::stream::empty())
    }
}

fn call() -> ToolCall {
    ToolCall::new(
        ToolCallId::new("fixture-call"),
        ToolName::new("inspect").expect("name"),
        json!({}),
    )
}

#[test]
fn legacy_adapters_default_to_unknown_without_acquiring_authority() {
    let llm: &dyn LlmPort = &Legacy;
    for effort in [
        None,
        Some(ReasoningEffort::None),
        Some(ReasoningEffort::Max),
    ] {
        assert_eq!(
            llm.tool_call_support("anything", effort),
            ToolCallSupport::Unknown
        );
    }
}

#[test]
fn definitions_calls_and_results_each_require_support_even_without_current_definitions() {
    let mut definition = ChatRequest::new("fixture", vec![Message::user("inspect")]);
    definition.tools.push(ToolSchema {
        name: ToolName::new("inspect").expect("name"),
        description: "fictional inspection".into(),
        parameters: json!({"type":"object"}),
    });
    let assistant = ChatRequest::new(
        "fixture",
        vec![Message::assistant(None, None, vec![call()])],
    );
    let result = ChatRequest::new(
        "fixture",
        vec![Message::Tool {
            call_id: call().id,
            content: "fictional result".into(),
            content_blocks: None,
            is_error: false,
        }],
    );
    assert!(assistant.tools.is_empty());
    assert!(result.tools.is_empty());
    for request in [definition, assistant, result] {
        assert!(has_tool_context(&request));
        assert!(validate_input(ToolCallSupport::Unsupported, &request).is_err());
        assert!(validate_input(ToolCallSupport::Supported, &request).is_ok());
        assert!(validate_input(ToolCallSupport::Unknown, &request).is_ok());
    }
}

#[test]
fn ordinary_text_mentions_do_not_become_executable_tool_history() {
    let request = ChatRequest::new(
        "fixture",
        vec![
            Message::system("tools: []"),
            Message::user("tool_call_id is text here"),
            Message::assistant(Some("tool result".into()), None, vec![]),
        ],
    );
    assert!(!has_tool_context(&request));
    for support in [
        ToolCallSupport::Unsupported,
        ToolCallSupport::Supported,
        ToolCallSupport::Unknown,
    ] {
        assert!(validate_input(support, &request).is_ok());
    }
}
