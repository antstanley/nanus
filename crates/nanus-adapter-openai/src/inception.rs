//! Exact Inception Labs API contracts; API evidence does not qualify a gateway.
//!
//! Checked 2026-10-19 against <https://docs.inceptionlabs.ai/get-started>,
//! <https://docs.inceptionlabs.ai/get-started/models>,
//! <https://docs.inceptionlabs.ai/capabilities/reasoning-efforts>,
//! <https://docs.inceptionlabs.ai/capabilities/tool-use>, and the model list the service
//! itself returns at <https://api.inceptionlabs.ai/v1/chat/completions/models>.
//!
//! Mercury speaks `OpenAI`'s chat-completions shape, with two documented differences this
//! module owns. Its output field is `max_completion_tokens`, and its reasoning scale is
//! `instant`, `low`, `medium` and `high` rather than the neutral seven steps: every neutral
//! step travels as one of those four, so a request always names a value the provider
//! documents instead of being refused. The models are text-only — the service reports `text`
//! as the only input modality — so image input is recorded as unsupported rather than
//! unknown, and no image profile is invented.

use nanus_ports::{
    ChatRequest, ImageInputSupport, LlmError, LlmResult, ModelCapabilities, ReasoningEffort,
    ToolCallSupport,
};

use crate::{INCEPTION_BASE_URL, OpenAiConfig, Protocol, Vendor};

/// The chat models Inception offers, in cycling order.
///
/// `mercury-voice`, `mercury-edit-2` and `mercury-decide` are deliberately absent: they serve
/// other endpoints or are not generally available, so offering them here would let a client
/// select a model that cannot answer a chat request.
pub static MODELS: [&str; 3] = ["mercury-3", "mercury-2.5", "mercury-2"];

/// The steps Inception's chat models document, in increasing order.
///
/// `instant` is the provider's own lowest step; it has no neutral spelling, so the steps
/// below `low` travel there through [`effort_spelling`] rather than being offered as
/// themselves.
const EFFORTS: [ReasoningEffort; 3] = [
    ReasoningEffort::Low,
    ReasoningEffort::Medium,
    ReasoningEffort::High,
];

/// The sampling range the chat models document; an out-of-range value is reset by the
/// provider rather than refused, so this adapter refuses it before the request is sent.
const TEMPERATURE_MIN: f32 = 0.5;
const TEMPERATURE_MAX: f32 = 1.0;

/// Whether `model` is an offered chat model on the documented endpoint and chat wire.
///
/// Evidence belongs to that exact combination: a gateway, a model the table does not name,
/// or a Responses preference has none, and answers the default capabilities.
pub fn known_api(config: &OpenAiConfig, model: &str) -> bool {
    config.vendor() == Vendor::Inception
        && config.base_url().trim_end_matches('/') == INCEPTION_BASE_URL
        && matches!(
            config.resolve_protocol(model),
            Ok(Protocol::ChatCompletions)
        )
        && MODELS.contains(&model)
}

/// The documented combined-context and output ceilings of one offered model.
fn limits(model: &str) -> Option<(u32, u32)> {
    match model {
        "mercury-3" => Some((1_000_000, 131_072)),
        "mercury-2.5" => Some((260_000, 65_536)),
        "mercury-2" => Some((128_000, 50_000)),
        _ => None,
    }
}

/// The steps the interface may offer for an Inception model.
///
/// All three chat models accept the same four documented steps, so the model does not change
/// the answer. `instant` is not on the neutral scale, which is why it is not offered; a
/// configuration's `minimal` reaches it through [`effort_spelling`]. Inception documents no way
/// to switch reasoning off, so [`validate`] refuses a request that asks for `none`.
#[must_use]
pub fn effort_levels(_model: &str) -> &'static [ReasoningEffort] {
    &EFFORTS
}

/// The provider's spelling of a neutral effort.
///
/// Inception's scale is shorter than the neutral one, so the steps above `high` clamp to it and
/// `minimal` travels as `instant`, the provider's own lowest value. `none` would map the same way,
/// but [`validate`] refuses it for an exact model: `instant` is still a reasoning step, and
/// silently answering a request that asked for no reasoning would be a different request. The
/// mapping stays total because a raw encode is an inspection helper, and a gateway or unknown id
/// has no evidence on which to refuse the request.
#[must_use]
pub const fn effort_spelling(effort: ReasoningEffort) -> &'static str {
    match effort {
        ReasoningEffort::None | ReasoningEffort::Minimal => "instant",
        ReasoningEffort::Low => "low",
        ReasoningEffort::Medium => "medium",
        ReasoningEffort::High | ReasoningEffort::XHigh | ReasoningEffort::Max => "high",
    }
}

/// Returns the documented limits and text-only input for an exact API model.
///
/// Anything else — an unknown id, another endpoint, or a wire this provider does not serve —
/// inherits no limits and no image verdict.
#[must_use]
pub fn capabilities(config: &OpenAiConfig, model: &str) -> ModelCapabilities {
    if !known_api(config, model) {
        return ModelCapabilities::default();
    }
    let Some((context, output)) = limits(model) else {
        return ModelCapabilities::default();
    };
    ModelCapabilities {
        image_input: ImageInputSupport::Unsupported,
        image_profile: None,
        context_window_tokens: Some(context),
        max_input_tokens: Some(context),
        max_output_tokens: Some(output),
    }
}

/// Function tools are supported on every offered chat model on the documented endpoint.
///
/// The provider documents tool calling, including parallel calls, on this endpoint and lists
/// `tools` for all three offered models. The reasoning effort does not gate it, so the
/// requested step is not consulted.
#[must_use]
pub fn tool_support(
    config: &OpenAiConfig,
    model: &str,
    _request_effort: Option<ReasoningEffort>,
) -> ToolCallSupport {
    if known_api(config, model) {
        ToolCallSupport::Supported
    } else {
        ToolCallSupport::Unknown
    }
}

/// Validates known API text requests before encoding or transport.
///
/// `none` is refused rather than mapped to `instant`: the provider documents no way to switch
/// reasoning off, and its lowest step still reasons. The provider resets an out-of-range
/// temperature and reports a warning rather than refusing the request, but a harness that
/// silently sends a different value than the caller named is worse than one that says so. An
/// output reservation above the model's ceiling is refused for the same reason the provider
/// refuses it: it is not honoured by truncation.
///
/// # Errors
///
/// Returns [`LlmError::Unsupported`] for an effort asking to disable reasoning, an output
/// reservation outside the model's ceiling, and a temperature outside the documented range.
pub fn validate(config: &OpenAiConfig, request: &ChatRequest) -> LlmResult<()> {
    if !known_api(config, &request.model) {
        return Ok(());
    }
    let effort = request
        .reasoning_effort
        .unwrap_or_else(|| config.reasoning_effort());
    if effort == ReasoningEffort::None {
        return Err(LlmError::Unsupported {
            feature: "Inception has no way to disable reasoning; its lowest step is `instant`"
                .into(),
        });
    }
    let Some((_, max_output)) = limits(&request.model) else {
        return Ok(());
    };
    let output = request
        .max_tokens
        .unwrap_or_else(|| config.effective_max_tokens_for(&request.model));
    if output == 0 {
        return Err(LlmError::Unsupported {
            feature: "an Inception request reserves more than zero output tokens".into(),
        });
    }
    if output > max_output {
        return Err(LlmError::Unsupported {
            feature: format!("this Inception model's output ceiling is {max_output} tokens"),
        });
    }
    if let Some(temperature) = request.temperature.or_else(|| config.temperature())
        && !(TEMPERATURE_MIN..=TEMPERATURE_MAX).contains(&temperature)
    {
        return Err(LlmError::Unsupported {
            feature: "Inception accepts a temperature between 0.5 and 1.0".into(),
        });
    }
    Ok(())
}

/// Known API text requests must honor provider bounds even without caller budgets.
///
/// The same triggers z.ai uses apply here, so a caller that names no context budget and sends
/// no images is still measured against the exact model's ceiling before any HTTP.
#[must_use]
pub fn requires_preflight(config: &OpenAiConfig, request: &ChatRequest) -> bool {
    known_api(config, &request.model)
        || request.context_budget.is_some()
        || nanus_ports::capabilities::has_images(&request.messages)
}
