//! Exact z.ai API contracts; Coding Plan and gateways inherit no API evidence.
//!
//! The one exception is the token limits: the Coding Plan's exact models are given the API's
//! context window and output ceiling, so an unset budget means the same numbers on both plans.
//! Efforts, function-tool admission, image support, preflight and managed context stay API-only.
//!
//! Checked 2026-10-03: <https://docs.z.ai/guides/capabilities/thinking>,
//! <https://docs.z.ai/api-reference/llm/chat-completion>, and the GLM-5.3,
//! GLM-5.2 and GLM-5.3-Flash/FlashX model pages under <https://docs.z.ai/guides/>.
//! The advertised 1M context is conservatively interpreted as decimal 1,000,000;
//! the API reference states the exact output ceiling as 131,072.

use nanus_ports::{
    ChatRequest, ImageInputSupport, LlmError, LlmResult, ModelCapabilities, ReasoningEffort,
    ToolCallSupport,
};

use crate::{OpenAiConfig, Protocol, Vendor, ZAI_BASE_URL, ZAI_CODING_BASE_URL};

const CONTEXT_TOKENS: u32 = 1_000_000;
const OUTPUT_TOKENS: u32 = 131_072;
const FORCED_EFFORTS: [ReasoningEffort; 3] = [
    ReasoningEffort::Low,
    ReasoningEffort::High,
    ReasoningEffort::Max,
];
const DYNAMIC_EFFORTS: [ReasoningEffort; 7] = [
    ReasoningEffort::None,
    ReasoningEffort::Minimal,
    ReasoningEffort::Low,
    ReasoningEffort::Medium,
    ReasoningEffort::High,
    ReasoningEffort::XHigh,
    ReasoningEffort::Max,
];

/// Evidence requires the exact API endpoint, Chat wire and exact model id.
pub fn known_api(config: &OpenAiConfig, model: &str) -> bool {
    config.vendor() == Vendor::Zai
        && config.base_url().trim_end_matches('/') == ZAI_BASE_URL
        && matches!(
            config.resolve_protocol(model),
            Ok(Protocol::ChatCompletions)
        )
        && matches!(
            model,
            "glm-5.3-flashx" | "glm-5.3-flash" | "glm-5.3" | "glm-5.2"
        )
}

/// An exact model on the Coding Plan endpoint and Chat wire, which shares the API's limits only.
fn known_coding_plan(config: &OpenAiConfig, model: &str) -> bool {
    config.vendor() == Vendor::Zai
        && config.base_url().trim_end_matches('/') == ZAI_CODING_BASE_URL
        && matches!(
            config.resolve_protocol(model),
            Ok(Protocol::ChatCompletions)
        )
        && matches!(
            model,
            "glm-5.3-flashx" | "glm-5.3-flash" | "glm-5.3" | "glm-5.2"
        )
}

/// Returns API-specific efforts without changing the stock Coding Plan table.
pub fn efforts(config: &OpenAiConfig, model: &str) -> Option<&'static [ReasoningEffort]> {
    if !known_api(config, model) {
        return None;
    }
    Some(if model == "glm-5.2" {
        &DYNAMIC_EFFORTS
    } else {
        &FORCED_EFFORTS
    })
}

/// A known API model starts at the provider's recommended, accepted default.
pub fn default_effort(config: &OpenAiConfig) -> ReasoningEffort {
    if known_api(config, config.model()) {
        ReasoningEffort::Max
    } else {
        ReasoningEffort::Medium
    }
}

/// Text/output metadata does not imply verified image admission.
pub fn capabilities(config: &OpenAiConfig, model: &str) -> ModelCapabilities {
    if known_coding_plan(config, model) {
        // The API's limits and nothing else: image support stays Unknown without evidence.
        return ModelCapabilities {
            context_window_tokens: Some(CONTEXT_TOKENS),
            max_input_tokens: Some(CONTEXT_TOKENS),
            max_output_tokens: Some(OUTPUT_TOKENS),
            ..ModelCapabilities::default()
        };
    }
    if !known_api(config, model) {
        return ModelCapabilities::default();
    }
    ModelCapabilities {
        image_input: if matches!(model, "glm-5.3" | "glm-5.2") {
            ImageInputSupport::Unsupported
        } else {
            ImageInputSupport::Unknown
        },
        image_profile: None,
        context_window_tokens: Some(CONTEXT_TOKENS),
        max_input_tokens: Some(CONTEXT_TOKENS),
        max_output_tokens: Some(OUTPUT_TOKENS),
    }
}

/// Function tools are supported only at an effort the exact API model accepts.
pub fn tool_support(
    config: &OpenAiConfig,
    model: &str,
    request_effort: Option<ReasoningEffort>,
) -> ToolCallSupport {
    let Some(allowed) = efforts(config, model) else {
        return ToolCallSupport::Unknown;
    };
    if allowed.contains(&request_effort.unwrap_or_else(|| config.reasoning_effort())) {
        ToolCallSupport::Supported
    } else {
        ToolCallSupport::Unsupported
    }
}

/// Validates known API text and tool requests before encoding or transport.
pub fn validate(config: &OpenAiConfig, request: &ChatRequest) -> LlmResult<()> {
    let Some(allowed) = efforts(config, &request.model) else {
        return Ok(());
    };
    let effort = request
        .reasoning_effort
        .unwrap_or_else(|| config.reasoning_effort());
    if !allowed.contains(&effort) {
        return Err(LlmError::Unsupported {
            feature: "this z.ai API model does not accept the selected reasoning effort".into(),
        });
    }
    let output = request
        .max_tokens
        .unwrap_or_else(|| config.effective_max_tokens_for(&request.model));
    if !(1..=OUTPUT_TOKENS).contains(&output) || request.tools.len() > 128 {
        return Err(LlmError::Unsupported {
            feature: "this z.ai API request exceeds its output or function-tool limit".into(),
        });
    }
    Ok(())
}

/// Known text-only requests must honor provider bounds even without caller budgets.
pub fn requires_preflight(config: &OpenAiConfig, request: &ChatRequest) -> bool {
    known_api(config, &request.model)
        || request.context_budget.is_some()
        || nanus_ports::capabilities::has_images(&request.messages)
}
