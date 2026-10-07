//! Exact-model limits and image support, each recorded only where it has evidence.
//!
//! Evidence: <https://api-docs.deepseek.com/api/list-models/> (checked 2026-10-03).
//! The response specifies combined input/output context, not a separate input quota.
//! Input is therefore bounded by that same context and every reservation is charged
//! against the combined limit. Retired aliases and custom endpoints inherit nothing.

use nanus_ports::{
    ChatRequest, ImageInputSupport, ImageProfile, LlmError, LlmResult, ModelCapabilities,
};

use crate::{DEFAULT_BASE_URL, DeepSeekConfig, MODEL_FLASH, MODEL_PRO};

/// Returns documented text limits only for exact models on the official endpoint.
pub fn capabilities(config: &DeepSeekConfig, model: &str) -> ModelCapabilities {
    if config.base_url().trim_end_matches('/') != DEFAULT_BASE_URL {
        return ModelCapabilities::default();
    }
    match model {
        // Flash is promoted on live evidence (docs/vision-evidence.md): four formats, a described
        // picture and a follow-up naming the call that produced it, on this endpoint and wire.
        MODEL_FLASH => {
            let mut caps = limits(ImageInputSupport::Supported);
            caps.image_profile = Some(ImageProfile::DeepSeekFlashAreaV1);
            caps
        }
        // Pro was shown not to read a picture on the same endpoint; it stays refused.
        MODEL_PRO => limits(ImageInputSupport::Unsupported),
        _ => ModelCapabilities::default(),
    }
}

fn limits(image_input: ImageInputSupport) -> ModelCapabilities {
    // Each exact model has these independently listed numbers in the models response.
    let caps = ModelCapabilities {
        image_input,
        image_profile: None,
        context_window_tokens: Some(1_048_576),
        max_input_tokens: Some(1_048_576),
        max_output_tokens: Some(393_216),
    };
    assert!(caps.context_window_tokens > caps.max_output_tokens);
    assert_eq!(caps.max_input_tokens, caps.context_window_tokens);
    caps
}

/// Enforces known output ceilings even when a caller has not opted into context budgeting.
pub fn requires_preflight(config: &DeepSeekConfig, request: &ChatRequest) -> bool {
    if request.context_budget.is_some() || nanus_ports::capabilities::has_images(&request.messages)
    {
        return true;
    }
    validate_output(config, request).is_err()
}

/// Rejects invalid output reservations only when this exact endpoint/model has known limits.
pub fn validate_output(config: &DeepSeekConfig, request: &ChatRequest) -> LlmResult<()> {
    let output = request
        .max_tokens
        .unwrap_or_else(|| config.max_tokens_for(&request.model));
    if capabilities(config, &request.model)
        .max_output_tokens
        .is_some_and(|limit| output == 0 || output > limit)
    {
        return Err(LlmError::Unsupported {
            feature: "output reservation exceeds model ceiling or is zero".into(),
        });
    }
    Ok(())
}

/// Exact Chat function-tool contract; all existing neutral effort mappings are accepted.
/// See <https://api-docs.deepseek.com/api/create-chat-completion/> (checked 2026-10-03).
pub fn tool_support(
    config: &DeepSeekConfig,
    model: &str,
    _request_effort: Option<nanus_ports::ReasoningEffort>,
) -> nanus_ports::ToolCallSupport {
    if config.base_url().trim_end_matches('/') != DEFAULT_BASE_URL {
        return nanus_ports::ToolCallSupport::Unknown;
    }
    match model {
        MODEL_FLASH | MODEL_PRO => nanus_ports::ToolCallSupport::Supported,
        _ => nanus_ports::ToolCallSupport::Unknown,
    }
}
