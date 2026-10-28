//! Managed-context preparation on the chat-completions and Responses wires, and its refusal
//! everywhere else.
//!
//! A managed call is encoded **once**: the estimate admits that value, the digest names its
//! serialised bytes, and the prepared call sends exactly those bytes over a route and
//! credential resolved at the same moment, so nothing is re-encoded or re-resolved between
//! admission and dispatch.
//!
//! ## Which paths are supported
//!
//! Only the neutral encoders, where a request is messages (or items) in order and nothing the
//! provider signed has to survive an edited history:
//!
//! - `OpenAI`'s API endpoint, for a model that resolves to chat completions there — automatic
//!   routing for an id before the `gpt-5.6` generation, or an exact chat preference — and whose
//!   function tools are not known to be refused at the configured effort;
//! - `OpenAI`'s API endpoint on the Responses wire, for every offered id: automatic routing for
//!   `gpt-5.6` and later, or an exact Responses preference;
//! - `OpenAI`'s `subscription` plan, on the Responses wire, under the stock automatic routing
//!   whose fallback is Responses. The plan refuses `max_output_tokens`, so the reservation is
//!   checked and charged here but never sent;
//! - z.ai's API endpoint, for the exact models with recorded API evidence;
//! - Inception's API endpoint, for the exact chat models with recorded API evidence.
//!
//! On the Responses wire history is sent plain, exactly as an ordinary request sends it: no
//! `include`, no `previous_response_id`, no reasoning item, and no encrypted replay. Stateless
//! Responses replay stays refused, as does an exact subscription preference and an automatic one
//! whose fallback is chat, since each would send a ceiling the plan refuses. Preparation refuses
//! before any HTTP, and never by switching wire, model or effort to find a path that would accept
//! the request. The coding plan and arbitrary gateways inherit nothing.
//!
//! ## The grammar
//!
//! Leading system messages only (the harness prompt, then a seeded session's handoff seed); a
//! conversation that opens with a user message and ends with a user message or tool results,
//! which is where the tail goes; at most one generated working-data message,
//! directly after the first user message and never last, so it is never an assistant prefill;
//! every surviving call answered once, in place; and no turn only another protocol's replay can
//! express, which the encoders would otherwise skip silently. A generated message next to an
//! original assistant turn is sent as two assistant messages, because neither wire has an
//! alternation rule: nothing is merged, no call is invented, no generated text changes role.
//!
//! ## Output
//!
//! The reservation must be explicit and within the model's declared ceiling (the exact z.ai
//! model's, otherwise the vendor's); a larger one is refused rather than silently reduced.

use std::collections::BTreeSet;

use nanus_domain::Message;
use nanus_domain::context::managed::compile::MEMORY_LABEL;
use nanus_domain::context::managed::limits::{LAYOUT_VERSION, POLICY_VERSION};
use nanus_domain::context::managed::{Digest, ErrorCode, SelectionIdentity};
use nanus_ports::{
    LlmError, LlmPort as _, LlmResult, LlmStream, ManagedRequest, ManagedSupport,
    PreparedModelCall, RequestEstimate, ToolArgumentLimits, ToolCallSupport,
};
use serde_json::Value;

use crate::{
    OPENAI_BASE_URL, OPENAI_SUBSCRIPTION_BASE_URL, OpenAiConfig, OpenAiLlm, Protocol,
    ProtocolPreference, Vendor,
};

/// The protocol label a managed chat-completions call records.
pub const PROTOCOL: &str = "openai.chat";

/// The protocol label a managed Responses call records.
pub const RESPONSES_PROTOCOL: &str = "openai.responses";

/// The label a call on `protocol` records.
const fn label(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::ChatCompletions => PROTOCOL,
        Protocol::Responses => RESPONSES_PROTOCOL,
    }
}

/// Whether `model` on this configuration can carry a managed projection.
pub fn support(config: &OpenAiConfig, model: &str) -> ManagedSupport {
    let chat = !config.stateless_responses()
        && config.account_id().is_none()
        && matches!(
            config.resolve_protocol(model),
            Ok(Protocol::ChatCompletions)
        );
    let exact = match config.vendor() {
        // Only the models this vendor offers: their output ceiling is the vendor's declared
        // one. An unknown id — an old model, a fine-tune — has no ceiling anyone has declared,
        // and a reservation it might refuse is exactly what managed preparation must not send.
        Vendor::OpenAi => {
            config.base_url().trim_end_matches('/') == OPENAI_BASE_URL
                && Vendor::OpenAi.models().contains(&model)
                && crate::tool_support::support(config, model, None) != ToolCallSupport::Unsupported
        }
        Vendor::Zai => crate::zai::known_api(config, model),
        // The same exactness as z.ai: the documented endpoint, an offered chat model, and a
        // tool verdict that is not a refusal. A gateway or an unknown id declares no ceiling,
        // and a reservation one might refuse is what managed preparation must not send.
        Vendor::Inception => {
            crate::inception::known_api(config, model)
                && crate::inception::tool_support(config, model, None)
                    != ToolCallSupport::Unsupported
        }
    };
    if (chat && exact) || responses_supported(config, model) {
        ManagedSupport::Supported {
            policy_version: POLICY_VERSION,
            layout_version: LAYOUT_VERSION,
        }
    } else {
        ManagedSupport::Unsupported
    }
}

/// Whether `model` can carry a managed projection on the Responses wire.
///
/// Two admitted routes, and both require the model's declared window and output ceiling:
///
/// - the API endpoint, which takes `max_output_tokens`, so its reservation is sent;
/// - the `subscription` endpoint under the stock automatic preference, whose fallback is
///   Responses. That plan refuses the field, so the reservation is enforced by accounting only.
///   An exact preference there promises a ceiling the endpoint refuses, and an automatic one
///   whose fallback is chat would send one, so both are refused.
fn responses_supported(config: &OpenAiConfig, model: &str) -> bool {
    if config.vendor() != Vendor::OpenAi
        || config.stateless_responses()
        || !matches!(config.resolve_protocol(model), Ok(Protocol::Responses))
        || !Vendor::OpenAi.models().contains(&model)
    {
        return false;
    }
    let caps = config.capabilities(model);
    if caps.context_window_tokens.is_none()
        || caps.max_output_tokens.is_none()
        || crate::tool_support::support(config, model, None) == ToolCallSupport::Unsupported
    {
        return false;
    }
    let base = config.base_url().trim_end_matches('/');
    let api =
        base == OPENAI_BASE_URL && config.account_id().is_none() && config.sends_output_ceiling();
    let subscription = base == OPENAI_SUBSCRIPTION_BASE_URL
        && matches!(config.protocol_preference(), ProtocolPreference::Automatic)
        && !config.sends_output_ceiling();
    api || subscription
}

/// An admitted, not-yet-sent managed call, on either wire.
///
/// It owns the exact body and the route and credential resolved when it was prepared; it has
/// no setter and no serialisable form, and it touches the network only once its stream is
/// polled.
pub struct PreparedCall {
    protocol: Protocol,
    transport: crate::Transport,
    body: String,
    estimate: RequestEstimate,
    digest: Digest,
    selection: SelectionIdentity,
}

impl core::fmt::Debug for PreparedCall {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // The credential is deliberately absent, and so is the body: a `Debug` rendering
        // reaches logs, and the digest already names the body exactly.
        f.debug_struct("PreparedCall")
            .field("protocol", &label(self.protocol))
            .field("estimate", &self.estimate)
            .field("digest", &self.digest)
            .field("selection", &self.selection)
            .finish_non_exhaustive()
    }
}

impl PreparedModelCall for PreparedCall {
    fn estimate(&self) -> RequestEstimate {
        self.estimate
    }

    fn request_digest(&self) -> &Digest {
        &self.digest
    }

    fn selection(&self) -> &SelectionIdentity {
        &self.selection
    }

    fn stream(self: Box<Self>) -> LlmStream {
        let Self {
            protocol,
            transport,
            body,
            ..
        } = *self;
        let decoder = match protocol {
            Protocol::ChatCompletions => {
                let mut accumulator = crate::wire::StreamAccumulator::default();
                accumulator.set_argument_limits(ToolArgumentLimits::managed());
                crate::Decoder::Chat(Box::new(accumulator))
            }
            Protocol::Responses => {
                let mut accumulator = crate::responses::StreamAccumulator::default();
                accumulator.set_argument_limits(ToolArgumentLimits::managed());
                crate::Decoder::Responses(Box::new(accumulator))
            }
        };
        crate::send(transport, body, decoder)
    }
}

#[cfg(test)]
impl PreparedCall {
    /// The exact bytes the call sends.
    pub fn body(&self) -> &str {
        &self.body
    }

    /// Points the call at a local fixture server. The body, digest and identity are untouched,
    /// which is the point: a fixture proves what is sent, not where the official host is.
    pub fn retarget(&mut self, endpoint: String) {
        self.transport.endpoint = endpoint;
    }
}

/// Prepares one managed call without any I/O.
///
/// # Errors
///
/// Refuses an unsupported model, endpoint or wire, a request outside the managed grammar, an
/// output reservation that is absent or above the declared ceiling, and a candidate that does
/// not fit.
pub fn prepare(llm: &OpenAiLlm, managed: ManagedRequest) -> LlmResult<PreparedCall> {
    let ManagedRequest {
        request,
        selection_epoch,
    } = managed;
    let config = llm.config();
    if config.stateless_responses()
        || !support(config, &request.model).supports(POLICY_VERSION, LAYOUT_VERSION)
    {
        return Err(refuse(
            ErrorCode::UnsupportedMode,
            "managed context is not supported for this model, endpoint or wire",
        ));
    }
    let protocol = llm.checked_protocol(&request)?;
    crate::zai::validate(config, &request)?;
    crate::inception::validate(config, &request)?;
    let caps = llm.capabilities(&request.model);
    let ceiling = caps
        .max_output_tokens
        .unwrap_or_else(|| config.vendor().max_output_tokens());
    check_output(request.max_tokens, ceiling)?;
    validate_sequence(&request.messages)?;
    nanus_ports::capabilities::validate_image_input(caps, &request)?;
    nanus_ports::tool_support::validate_input(
        llm.tool_call_support(&request.model, request.reasoning_effort),
        &request,
    )?;

    // Encoded once: the estimate, the digest and the dispatch all read this one value. Both
    // encoders are the ordinary ones, so a managed body differs from an ordinary one only in what
    // the managed compiler selected, the tail, and the head breakpoint.
    let payload = match protocol {
        Protocol::ChatCompletions => crate::wire::build_request(config, &request),
        Protocol::Responses => crate::responses::encode(config, &request, crate::Layout::Managed),
    };
    let estimate = nanus_ports::capabilities::estimate_managed_payload(caps, &request, &payload)?;
    if !estimate.fits(caps, &request) {
        return Err(refuse(
            ErrorCode::CandidateTooLarge,
            "the prepared request exceeds its input, reservation or byte limits",
        ));
    }
    let body = serde_json::to_string(&payload).map_err(|_| LlmError::Unsupported {
        feature: "could not encode the request as JSON".into(),
    })?;
    // Postcondition: the estimate measured exactly the bytes that will be sent.
    assert_eq!(body.len(), estimate.request_bytes);
    let transport = llm.transport(&llm.url(protocol));
    let effort = match protocol {
        Protocol::ChatCompletions => payload.get("reasoning_effort"),
        Protocol::Responses => payload
            .get("reasoning")
            .and_then(|reasoning| reasoning.get("effort")),
    }
    .and_then(Value::as_str)
    .map(str::to_owned);
    let selection = SelectionIdentity {
        provider: config.vendor().as_str().to_owned(),
        endpoint_digest: Digest::of(transport.endpoint.as_bytes()),
        protocol: label(protocol).to_owned(),
        model: request.model,
        effort,
        epoch: selection_epoch,
    };
    Ok(PreparedCall {
        protocol,
        transport,
        digest: Digest::of(body.as_bytes()),
        body,
        estimate,
        selection,
    })
}

/// Requires an explicit reservation no larger than the declared ceiling.
fn check_output(requested: Option<u32>, ceiling: u32) -> LlmResult<()> {
    let Some(output) = requested else {
        return Err(refuse(
            ErrorCode::ProtocolIncompatible,
            "a managed request reserves its output explicitly",
        ));
    };
    if output == 0 || output > ceiling {
        return Err(refuse(
            ErrorCode::CandidateTooLarge,
            "the output reservation exceeds the model's output ceiling",
        ));
    }
    Ok(())
}

/// Whether `message` is the generated working-data message the runner inserts.
pub fn is_generated(message: &Message) -> bool {
    matches!(
        message,
        Message::Assistant {
            text: Some(text),
            reasoning: None,
            replay: None,
            tool_calls,
        } if tool_calls.is_empty() && text.starts_with(MEMORY_LABEL)
    )
}

/// Checks the managed role grammar described in the module documentation.
///
/// # Errors
///
/// Returns `protocol_incompatible` naming the first rule the request breaks.
pub fn validate_sequence(messages: &[Message]) -> LlmResult<()> {
    let start = messages
        .iter()
        .position(|message| !matches!(message, Message::System { .. }))
        .ok_or_else(|| incompatible("a managed request carries a conversation"))?;
    let conversation = messages.get(start..).unwrap_or_default();
    if conversation
        .iter()
        .any(|message| matches!(message, Message::System { .. }))
    {
        return Err(incompatible("a system message follows the conversation"));
    }
    if !matches!(conversation.first(), Some(Message::User { .. })) {
        return Err(incompatible("the conversation opens with a user message"));
    }
    if conversation.iter().any(Message::is_replay_only) {
        return Err(incompatible(
            "an assistant turn only another protocol's replay can express",
        ));
    }
    // Only the message directly after the first user message can be the generated one. A
    // reply elsewhere that happens to open with the label is the model's own text and is sent
    // as it is: identifying generated data anywhere by its words would let one reply wedge
    // every later request of its session.
    let anchor_generated = conversation.get(1).is_some_and(is_generated);
    if anchor_generated && conversation.len() <= 2 {
        return Err(incompatible("generated data is never the last message"));
    }
    validate_pairs(conversation)?;
    // The tail is placed after the newest message, so there must be one it can follow.
    if !matches!(
        conversation.last(),
        Some(Message::User { .. } | Message::Tool { .. })
    ) {
        return Err(incompatible(
            "the conversation ends with a user message or tool results",
        ));
    }
    Ok(())
}

/// Every surviving call is answered once, in place; every result answers a surviving call.
fn validate_pairs(conversation: &[Message]) -> LlmResult<()> {
    let mut pending: Vec<&str> = Vec::new();
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for message in conversation {
        if let Message::Tool { call_id, .. } = message {
            let at = pending
                .iter()
                .position(|id| *id == call_id.as_str())
                .ok_or_else(|| incompatible("a tool result answers no surviving call"))?;
            pending.swap_remove(at);
            continue;
        }
        if !pending.is_empty() {
            return Err(incompatible("a surviving tool call has no result"));
        }
        for call in message.tool_calls() {
            if !seen.insert(call.id.as_str()) {
                return Err(incompatible("a tool call id appears twice"));
            }
            pending.push(call.id.as_str());
        }
    }
    if pending.is_empty() {
        Ok(())
    } else {
        Err(incompatible("a surviving tool call has no result"))
    }
}

/// A refusal carrying the managed error code as its stable prefix.
fn refuse(code: ErrorCode, reason: &str) -> LlmError {
    LlmError::Unsupported {
        feature: format!("{}: {reason}", code.as_str()),
    }
}

fn incompatible(reason: &str) -> LlmError {
    refuse(ErrorCode::ProtocolIncompatible, reason)
}
