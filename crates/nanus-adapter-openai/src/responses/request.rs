//! Pure stateless request preparation. Stock dispatch does not opt into this API yet.
use std::collections::{BTreeSet, VecDeque};

use nanus_domain::message::ReplayContext;
use nanus_domain::{Message, ToolCallId};
use nanus_ports::{ChatRequest, LlmError, LlmResult, ModelCapabilities, ToolCallSupport};
use serde::Serialize;
use serde_json::{Value, json};

use crate::{OPENAI_BASE_URL, OpenAiConfig, Protocol, ProtocolPreference, Vendor};

mod digest;

/// An admitted exact JSON body and receipts, for a caller-managed stateless Responses dispatch.
/// Preparation performs no HTTP or tool execution and authenticates no provider ciphertext.
#[derive(Debug)]
pub struct PreparedResponses {
    /// Actual original-item body, with store=false and explicit output ceiling.
    pub body: Value,
    /// Consistency binding of controls, original source, fitting and dispatched body receipts.
    pub prefix_digest: String,
    /// Bounded original/fitted request evidence for the completed response.
    pub context_receipt: ReplayContext,
}

fn refused() -> LlmError {
    LlmError::Unsupported {
        feature: "incompatible stateless Responses request or replay".into(),
    }
}

pub fn prepare(
    config: &OpenAiConfig,
    request: &ChatRequest,
    caps: ModelCapabilities,
    support: ToolCallSupport,
) -> LlmResult<PreparedResponses> {
    prepare_mode(config, request, caps, support, false).map(|(prepared, _)| prepared)
}

pub fn estimate(
    config: &OpenAiConfig,
    request: &ChatRequest,
    caps: ModelCapabilities,
    support: ToolCallSupport,
) -> LlmResult<nanus_ports::RequestEstimate> {
    prepare_mode(config, request, caps, support, true).map(|(_, estimate)| estimate)
}

fn prepare_mode(
    config: &OpenAiConfig,
    request: &ChatRequest,
    caps: ModelCapabilities,
    support: ToolCallSupport,
    prospective: bool,
) -> LlmResult<(PreparedResponses, nanus_ports::RequestEstimate)> {
    validate(config, request, caps, support)?;
    let source = request
        .source_history
        .as_deref()
        .unwrap_or(&request.messages);
    nanus_ports::capabilities::validate_history_image_input(caps, &request.model, source)?;
    let budget = request.context_budget.ok_or_else(refused)?;
    let projection = if prospective {
        nanus_domain::context::identify_tool_result_projection(source, &request.messages, budget)
    } else {
        nanus_domain::context::identify_projection(source, &request.messages, budget)
    }
    .map_err(|_| refused())?;
    nanus_domain::content::serialized_size(source, nanus_domain::content::SESSION_BYTES_MAX)
        .map_err(|_| refused())?;
    let controls = controls(config, request, source)?;
    let source_digest = source_prefixes(source, &controls)?;
    crate::function_policy::validate(config, request)?;
    let mut body = super::build_with_input(
        config,
        request,
        super::encode_replay_input(&request.messages),
    );
    body["include"] = json!(["reasoning.encrypted_content"]);
    let wire_digest = digest::json(&body, nanus_domain::content::RECORD_BYTES_MAX)?;
    let context_receipt = ReplayContext {
        budget,
        dropped_turns: projection.dropped_turns,
        dropped_messages: projection.dropped_messages,
        source_digest,
        wire_digest,
    };
    let prefix_digest = binding(&controls, &context_receipt)?;
    let estimate = nanus_ports::capabilities::estimate_payload(caps, request, &body)?;
    nanus_ports::capabilities::validate_estimate(caps, request, estimate)?;
    Ok((
        PreparedResponses {
            body,
            prefix_digest,
            context_receipt,
        },
        estimate,
    ))
}

fn validate(
    config: &OpenAiConfig,
    request: &ChatRequest,
    caps: ModelCapabilities,
    support: ToolCallSupport,
) -> LlmResult<()> {
    let output = request.max_tokens.filter(|v| *v > 0).ok_or_else(refused)?;
    let budget = request
        .context_budget
        .filter(|v| *v > 0)
        .ok_or_else(refused)?;
    if config.vendor() != Vendor::OpenAi
        || config.base_url().trim_end_matches('/') != OPENAI_BASE_URL
        || config.account_id().is_some()
        || config.response_limits().is_none()
        || config.protocol_preference() != ProtocolPreference::Exact(Protocol::Responses)
        || config.resolve_protocol(&request.model)? != Protocol::Responses
        || support != ToolCallSupport::Supported
        || !config.sends_output_ceiling()
        || output > config.effective_max_tokens()
        || request.separate_reasoning_tokens != 0
        || request
            .temperature
            .is_some_and(|v| !v.is_finite() || v < 0.0)
        || caps.context_window_tokens.is_none_or(|max| budget > max)
        || caps.max_input_tokens.is_none()
        || caps.max_output_tokens.is_none_or(|max| output > max)
    {
        return Err(refused());
    }
    nanus_ports::capabilities::validate_image_input(caps, request)?;
    nanus_domain::content::serialized_size(
        &request.messages,
        nanus_domain::content::RECORD_BYTES_MAX,
    )
    .map_err(|_| refused())?;
    nanus_domain::content::serialized_size(&request.tools, nanus_domain::content::RECORD_BYTES_MAX)
        .map_err(|_| refused())?;
    Ok(())
}

// Borrow all variable-size controls so their JSON envelope is counted before cloning.
#[derive(Serialize)]
struct Controls<'a> {
    endpoint: &'static str,
    protocol: &'static str,
    model: &'a str,
    effort: &'static str,
    instructions: Vec<&'a str>,
    tools: &'a [nanus_domain::ToolSchema],
    strict: Option<bool>,
    max_output_tokens: Option<u32>,
    temperature: Option<f32>,
}

fn controls(config: &OpenAiConfig, request: &ChatRequest, source: &[Message]) -> LlmResult<Value> {
    let mut instructions = Vec::new();
    for message in source {
        if let Message::System { text } = message {
            instructions.push(text.as_str());
        } else {
            break;
        }
    }
    let value = Controls {
        endpoint: "https://api.openai.com/v1/responses",
        protocol: "openai.responses",
        model: &request.model,
        effort: crate::effort_spelling(
            request
                .reasoning_effort
                .unwrap_or_else(|| config.reasoning_effort()),
        ),
        instructions,
        tools: &request.tools,
        strict: config.function_strictness(),
        max_output_tokens: request.max_tokens,
        temperature: request.temperature.or_else(|| config.temperature()),
    };
    nanus_domain::content::serialized_size(&value, nanus_domain::content::RECORD_BYTES_MAX)
        .map_err(|_| refused())?;
    let value = serde_json::to_value(value).map_err(|_| refused())?;
    Ok(value)
}

fn binding(controls: &Value, context: &ReplayContext) -> LlmResult<String> {
    digest::json(
        &json!({"controls":controls,"context":context}),
        nanus_domain::content::RECORD_BYTES_MAX,
    )
}

fn source_prefixes(source: &[Message], controls: &Value) -> LlmResult<String> {
    let mut hash = digest::Source::new()?;
    let mut turns = Vec::new();
    let mut pending = VecDeque::new();
    let mut seen = BTreeSet::new();
    let head = source
        .iter()
        .position(|message| matches!(message, Message::User { .. }))
        .ok_or_else(refused)?;
    for (index, message) in source.iter().enumerate() {
        match message {
            Message::System { .. } if index < head => {}
            Message::User { .. } if pending.is_empty() => turns.push(index),
            Message::Assistant {
                replay: Some(replay),
                text,
                tool_calls,
                ..
            } if pending.is_empty() => {
                if replay.protocol != "openai.responses" {
                    return Err(refused());
                }
                replay
                    .validate_response(text.as_deref(), tool_calls)
                    .map_err(|_| refused())?;
                let context = replay.context_receipt.as_deref().ok_or_else(refused)?;
                validate_original_fit(context, &turns, head)?;
                if context.source_digest != hash.prefix()?
                    || replay.prefix_digest != binding(controls, context)?
                {
                    return Err(refused());
                }
                for call in tool_calls {
                    if !seen.insert(call.id.as_str()) {
                        return Err(refused());
                    }
                    pending.push_back(&call.id);
                }
            }
            Message::Tool { call_id, .. } => answer(&mut pending, call_id)?,
            _ => return Err(refused()),
        }
        hash.append(message)?;
    }
    if !pending.is_empty() {
        return Err(refused());
    }
    hash.prefix()
}

fn validate_original_fit(context: &ReplayContext, turns: &[usize], head: usize) -> LlmResult<()> {
    context.validate().map_err(|_| refused())?;
    let dropped = usize::try_from(context.dropped_turns).map_err(|_| refused())?;
    let cut = *turns.get(dropped).ok_or_else(refused)?;
    let count = cut.checked_sub(head).ok_or_else(refused)?;
    if u32::try_from(count).map_err(|_| refused())? != context.dropped_messages {
        return Err(refused());
    }
    Ok(())
}

fn answer(pending: &mut VecDeque<&ToolCallId>, call: &ToolCallId) -> LlmResult<()> {
    if pending.pop_front() != Some(call) {
        return Err(refused());
    }
    Ok(())
}
