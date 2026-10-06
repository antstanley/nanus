//! Managed-context preparation on the chat-completions wire, and its refusal everywhere else.
//!
//! A managed call is encoded **once**: the estimate admits that value, the digest names its
//! serialised bytes, and the prepared call sends exactly those bytes over a route and
//! credential resolved at the same moment, so nothing is re-encoded or re-resolved between
//! admission and dispatch.
//!
//! ## Which paths are supported
//!
//! Only the neutral chat-completions encoder, where a request is messages in order and nothing
//! the provider signed has to survive an edited history:
//!
//! - `OpenAI`'s API endpoint, for a model that resolves to chat completions there — automatic
//!   routing for an id before the `gpt-5.6` generation, or an exact chat preference — and whose
//!   function tools are not known to be refused at the configured effort;
//! - z.ai's API endpoint, for the exact models with recorded API evidence.
//!
//! **The Responses API is unsupported**, whatever routed the request there: automatic routing
//! for `gpt-5.6` and later, the subscription plan, an exact Responses preference, or bounded
//! stateless replay. A Responses continuation carries opaque items and receipts bound to a whole
//! turn, and a missing stateless replay check is not evidence that editing fragments is safe, so
//! preparation refuses before any HTTP, and never by switching wire, model or effort to find a
//! path that would accept the request. The coding plan and arbitrary gateways inherit nothing.
//!
//! ## The grammar
//!
//! Leading system messages only (the harness prompt, then the fixed-schema notice); a
//! conversation that opens with a user message; at most one generated working-data message,
//! directly after the first user message and never last, so it is never an assistant prefill;
//! every surviving call answered once, in place; and no turn only another protocol's replay can
//! express, which the chat encoder would otherwise skip silently. A generated message next to
//! an original assistant turn is sent as two assistant messages, because chat completions has no
//! alternation rule: nothing is merged, no call is invented, no generated text changes role.
//!
//! ## Output
//!
//! The reservation must be explicit and within the model's declared ceiling (the exact z.ai
//! model's, otherwise the vendor's); a larger one is refused rather than silently reduced.

use std::collections::BTreeSet;

use nanus_domain::Message;
use nanus_domain::context::managed::compile::MEMORY_LABEL;
use nanus_domain::context::managed::limits::POLICY_VERSION;
use nanus_domain::context::managed::{Digest, ErrorCode, SelectionIdentity};
use nanus_ports::{
    LlmError, LlmPort as _, LlmResult, LlmStream, ManagedRequest, ManagedSupport,
    PreparedModelCall, RequestEstimate, ToolArgumentLimits, ToolCallSupport,
};
use serde_json::Value;

use crate::{OPENAI_BASE_URL, OpenAiConfig, OpenAiLlm, Protocol, Vendor};

/// The protocol label a managed call records: the `OpenAI`-compatible chat-completions wire.
pub const PROTOCOL: &str = "openai.chat";

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
    };
    if chat && exact {
        ManagedSupport::Supported {
            policy_version: POLICY_VERSION,
        }
    } else {
        ManagedSupport::Unsupported
    }
}

/// An admitted, not-yet-sent managed chat call.
///
/// It owns the exact body and the route and credential resolved when it was prepared; it has
/// no setter and no serialisable form, and it touches the network only once its stream is
/// polled.
pub struct PreparedChat {
    transport: crate::Transport,
    body: String,
    estimate: RequestEstimate,
    digest: Digest,
    selection: SelectionIdentity,
}

impl core::fmt::Debug for PreparedChat {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // The credential is deliberately absent, and so is the body: a `Debug` rendering
        // reaches logs, and the digest already names the body exactly.
        f.debug_struct("PreparedChat")
            .field("estimate", &self.estimate)
            .field("digest", &self.digest)
            .field("selection", &self.selection)
            .finish_non_exhaustive()
    }
}

impl PreparedModelCall for PreparedChat {
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
            transport, body, ..
        } = *self;
        let mut accumulator = crate::wire::StreamAccumulator::default();
        accumulator.set_argument_limits(ToolArgumentLimits::managed());
        crate::send(transport, body, crate::Decoder::Chat(Box::new(accumulator)))
    }
}

#[cfg(test)]
impl PreparedChat {
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
/// Refuses any Responses route and any unsupported model or endpoint, a request outside the
/// managed grammar, an output reservation that is absent or above the declared ceiling, and a
/// candidate that does not fit.
pub fn prepare(llm: &OpenAiLlm, managed: ManagedRequest) -> LlmResult<PreparedChat> {
    let ManagedRequest {
        request,
        selection_epoch,
    } = managed;
    let config = llm.config();
    if config.stateless_responses()
        || llm.checked_protocol(&request)? == Protocol::Responses
        || !support(config, &request.model).supports(POLICY_VERSION)
    {
        return Err(refuse(
            ErrorCode::UnsupportedMode,
            "managed context is supported on the chat-completions API path only",
        ));
    }
    crate::zai::validate(config, &request)?;
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

    // Encoded once: the estimate, the digest and the dispatch all read this one value.
    let payload = crate::wire::build_request(config, &request);
    let estimate = nanus_ports::capabilities::estimate_payload(caps, &request, &payload)?;
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
    let transport = llm.transport(&llm.url(Protocol::ChatCompletions));
    let selection = SelectionIdentity {
        provider: config.vendor().as_str().to_owned(),
        endpoint_digest: Digest::of(transport.endpoint.as_bytes()),
        protocol: PROTOCOL.to_owned(),
        model: request.model,
        effort: payload
            .get("reasoning_effort")
            .and_then(Value::as_str)
            .map(str::to_owned),
        epoch: selection_epoch,
    };
    Ok(PreparedChat {
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
fn is_generated(message: &Message) -> bool {
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
    let generated: Vec<usize> = conversation
        .iter()
        .enumerate()
        .filter(|(_, message)| is_generated(message))
        .map(|(index, _)| index)
        .collect();
    match generated.as_slice() {
        [] => {}
        [1] if conversation.len() > 2 => {}
        [1] => return Err(incompatible("generated data is never the last message")),
        _ => {
            return Err(incompatible(
                "generated data appears once, directly after the first user message",
            ));
        }
    }
    validate_pairs(conversation)
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
