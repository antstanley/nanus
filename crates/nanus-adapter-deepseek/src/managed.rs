//! Managed-context preparation: one exact body, measured, digested and sent as it is.
//!
//! The ordinary path encodes a request each time it is asked about one, so the estimate a
//! caller reserved against and the bytes that went out could in principle differ. A managed
//! call cannot afford that: its estimate admits the request, its digest is recorded before any
//! HTTP, and a crash between the two must leave a record of exactly what was meant to be sent.
//! So preparation encodes **once**, measures that value, serialises it once, and the prepared
//! call sends those bytes over a route and credential resolved at the same moment.
//!
//! ## What is admitted
//!
//! Only the official endpoint and the two current models, because those are the combinations
//! whose output ceiling is declared (see [`crate::metadata`]) and for which this crate carries
//! wire fixtures. The request must be in the managed grammar the runner compiles:
//!
//! - one or more leading system messages (the harness prompt, then the fixed-schema notice), and
//!   no system message after the conversation begins;
//! - a conversation that opens with a user message;
//! - at most one generated working-data message — an assistant text turn with no calls, no
//!   reasoning and no replay, opening with the domain's provenance label — directly after the
//!   first user message and never last, so it can never be an assistant prefill;
//! - every surviving tool call answered by exactly one result, in place, and no result without
//!   its call — hiding takes whole fragments, so anything else is a projection error;
//! - no assistant turn that only another protocol's opaque replay could express, because the
//!   chat encoder skips such a turn and a managed request must not drop a message silently.
//!
//! A generated message followed by an original assistant turn puts two assistant messages side
//! by side. Chat completions has no alternation rule, so they are sent as they are; nothing is
//! merged, no call is invented and generated text is never promoted to a user or system role.
//!
//! ## Output
//!
//! The runner reserves output explicitly. A request without a ceiling, or one above the model's
//! declared ceiling, is refused rather than reduced: a silently smaller reservation would make
//! the estimate a lie about the request it admitted.

use std::collections::BTreeSet;

use nanus_domain::Message;
use nanus_domain::context::managed::compile::MEMORY_LABEL;
use nanus_domain::context::managed::limits::POLICY_VERSION;
use nanus_domain::context::managed::{Digest, ErrorCode, SelectionIdentity};
use nanus_ports::{
    LlmError, LlmResult, LlmStream, ManagedRequest, ManagedSupport, PreparedModelCall,
    RequestEstimate, ToolArgumentLimits,
};
use serde_json::Value;

use crate::{DEFAULT_BASE_URL, DeepSeekConfig, DeepSeekLlm, MODEL_FLASH, MODEL_PRO, PROVIDER};

/// The protocol label a managed call records: `DeepSeek`'s chat-completions wire.
pub const PROTOCOL: &str = "deepseek.chat";

/// Whether `model` on this configuration can carry a managed projection.
pub fn support(config: &DeepSeekConfig, model: &str) -> ManagedSupport {
    if config.base_url().trim_end_matches('/') == DEFAULT_BASE_URL
        && matches!(model, MODEL_FLASH | MODEL_PRO)
    {
        ManagedSupport::Supported {
            policy_version: POLICY_VERSION,
        }
    } else {
        ManagedSupport::Unsupported
    }
}

/// An admitted, not-yet-sent managed call.
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
        crate::send(transport, body, ToolArgumentLimits::managed())
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
/// Refuses an unsupported model or endpoint, a request outside the managed grammar, an output
/// reservation that is absent or above the declared ceiling, and a candidate that does not fit.
pub fn prepare(llm: &DeepSeekLlm, managed: ManagedRequest) -> LlmResult<PreparedChat> {
    let ManagedRequest {
        request,
        selection_epoch,
    } = managed;
    let config = llm.config();
    if !support(config, &request.model).supports(POLICY_VERSION) {
        return Err(refuse(
            ErrorCode::UnsupportedMode,
            "managed context is not supported for this model and endpoint",
        ));
    }
    let caps = crate::metadata::capabilities(config, &request.model);
    check_output(request.max_tokens, caps.max_output_tokens)?;
    validate_sequence(&request.messages)?;
    nanus_ports::capabilities::validate_image_input(caps, &request)?;

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
    let transport = llm.transport();
    let selection = SelectionIdentity {
        provider: PROVIDER.to_owned(),
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

/// Requires an explicit reservation no larger than a declared ceiling.
fn check_output(requested: Option<u32>, ceiling: Option<u32>) -> LlmResult<()> {
    let Some(output) = requested else {
        return Err(refuse(
            ErrorCode::ProtocolIncompatible,
            "a managed request reserves its output explicitly",
        ));
    };
    let Some(ceiling) = ceiling else {
        return Err(refuse(
            ErrorCode::ProtocolIncompatible,
            "this model declares no output ceiling to reserve against",
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
