//! Exact API tool contracts, separate from image profiles and generic token metadata.
//!
//! Sources (checked 2026-10-03): each exact model's page under
//! <https://developers.openai.com/api/docs/models/> and the protocol restrictions in
//! <https://developers.openai.com/api/docs/guides/function-calling>.
//! No API evidence is inherited by a subscription backend or arbitrary gateway.

use nanus_ports::{ReasoningEffort, ToolCallSupport};

use crate::{OPENAI_BASE_URL, OpenAiConfig, Protocol, Vendor};

/// Answers only independently documented exact API models on their actual configured wire.
pub fn support(
    config: &OpenAiConfig,
    model: &str,
    request_effort: Option<ReasoningEffort>,
) -> ToolCallSupport {
    if config.vendor() == Vendor::Zai {
        return crate::zai::tool_support(config, model, request_effort);
    }
    if config.vendor() != Vendor::OpenAi
        || config.base_url().trim_end_matches('/') != OPENAI_BASE_URL
        || !matches!(
            model,
            "gpt-6-astra"
                | "gpt-6.1-sol"
                | "gpt-6-luna"
                | "gpt-5.6-sol"
                | "gpt-5.6-terra"
                | "gpt-5.6-luna"
        )
    {
        return ToolCallSupport::Unknown;
    }
    let Ok(protocol) = config.resolve_protocol(model) else {
        return ToolCallSupport::Unsupported;
    };
    let effort = request_effort.unwrap_or_else(|| config.reasoning_effort());
    if !Vendor::OpenAi.effort_levels(model).contains(&effort) {
        return ToolCallSupport::Unsupported;
    }
    match (protocol, model, effort) {
        (Protocol::Responses, _, _) => ToolCallSupport::Supported,
        (Protocol::ChatCompletions, "gpt-6-astra" | "gpt-6.1-sol", _) => {
            ToolCallSupport::Unsupported
        }
        (Protocol::ChatCompletions, "gpt-6-luna", ReasoningEffort::None) => {
            ToolCallSupport::Supported
        }
        (Protocol::ChatCompletions, "gpt-6-luna", _) => ToolCallSupport::Unsupported,
        // The current initial evidence covers the older models' Responses wire only.
        (Protocol::ChatCompletions, _, _) => ToolCallSupport::Unknown,
    }
}
