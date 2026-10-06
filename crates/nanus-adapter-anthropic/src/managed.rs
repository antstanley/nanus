//! Managed-context preparation on the Messages API, and the rule for signed replay under it.
//!
//! A managed call is encoded **once**: the estimate admits that value, the digest names its
//! serialised bytes, and the prepared call sends exactly those bytes over a route and
//! credential resolved at the same moment, so nothing is re-encoded or re-resolved between
//! admission and dispatch.
//!
//! ## Signed replay
//!
//! Hiding a fragment, inserting the generated working-data message or changing the notice all
//! change the encoded prefix of every assistant turn after the change, and a thinking block's
//! signature is bound to the prefix that produced it. The decision here is to apply the
//! adapter's **existing** checked replay unchanged, per message:
//!
//! - an assistant turn whose retained blocks match its neutral text and calls *and* whose
//!   recorded prefix digest equals the digest of the encoded system, tools and preceding turns of
//!   this very body is sent as its original signed blocks;
//! - any other turn with replay is sent as its neutral text and tool calls, without thinking.
//!
//! That neutral form is the adapter's own, already used whenever the ordinary fitter elides a
//! turn, and it is admissible in every mode this path supports: the three models here run
//! adaptive thinking, for which sending history without its thinking blocks is the provider's
//! documented recovery for an edited conversation — the request is accepted and the model simply
//! answers without that earlier reasoning. Because a changed prefix makes every *later* digest
//! differ too, the signed blocks that survive are always a leading run of the conversation, the
//! one shape the provider's chain of thinking blocks accepts. No signature or digest is ever
//! rewritten or recomputed to make a turn match.
//!
//! One case has no neutral form: a turn that carries only replay — no text, no calls — whose
//! prefix changed, or that came from another protocol. The ordinary encoder skips it; a managed
//! request must not drop a message the projection kept, so preparation refuses it as
//! `protocol_incompatible` instead.
//!
//! In practice the fixed-schema notice changes between requests (its revision, counts and
//! estimate), and it travels in the top-level system field, so a managed conversation usually
//! sends its history neutrally. That costs earlier reasoning and the prompt cache, never
//! correctness.
//!
//! ## The grammar
//!
//! Leading system messages only — both are joined, in order, into the top-level system field,
//! exactly as the ordinary path joins its elision notice; a conversation that opens with a user
//! message; at most one generated working-data message, directly after the first user message and
//! never last, so it is never an assistant prefill, which these models refuse; and every
//! surviving call answered once, in place. A generated message beside an original assistant turn
//! is two consecutive assistant messages, which the Messages API accepts and reads as one turn:
//! nothing is merged here, no call is invented, and no generated text changes role.
//!
//! ## Output
//!
//! The reservation must be explicit and within the model's ceiling. The ordinary path clamps a
//! larger one silently; a managed request is refused instead, because its estimate must be of
//! the reservation it actually carries.

use std::collections::BTreeSet;

use nanus_domain::Message;
use nanus_domain::context::managed::compile::MEMORY_LABEL;
use nanus_domain::context::managed::limits::POLICY_VERSION;
use nanus_domain::context::managed::{Digest, ErrorCode, SelectionIdentity};
use nanus_ports::{
    LlmError, LlmPort as _, LlmResult, LlmStream, ManagedRequest, ManagedSupport,
    PreparedModelCall, RequestEstimate, ToolArgumentLimits,
};

use crate::{AnthropicConfig, AnthropicLlm, DEFAULT_BASE_URL, PROVIDER, wire};

/// The protocol label a managed call records, the same one its signed replay carries.
pub const PROTOCOL: &str = "anthropic.messages";

/// Whether `model` on this configuration can carry a managed projection.
pub fn support(config: &AnthropicConfig, model: &str) -> ManagedSupport {
    if config.base_url().trim_end_matches('/') == DEFAULT_BASE_URL
        && matches!(
            model,
            "claude-opus-5-5" | "claude-sonnet-5-5" | "claude-fable-5-1"
        )
    {
        ManagedSupport::Supported {
            policy_version: POLICY_VERSION,
        }
    } else {
        ManagedSupport::Unsupported
    }
}

/// An admitted, not-yet-sent managed message request.
///
/// It owns the exact body, the prefix digest its signed response will be bound to, and the
/// route and credential resolved when it was prepared; it has no setter and no serialisable
/// form, and it touches the network only once its stream is polled.
pub struct PreparedMessages {
    transport: crate::Transport,
    body: String,
    prefix_digest: String,
    estimate: RequestEstimate,
    digest: Digest,
    selection: SelectionIdentity,
}

impl core::fmt::Debug for PreparedMessages {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // The credential is deliberately absent, and so is the body: a `Debug` rendering
        // reaches logs, and the digest already names the body exactly.
        f.debug_struct("PreparedMessages")
            .field("estimate", &self.estimate)
            .field("digest", &self.digest)
            .field("selection", &self.selection)
            .finish_non_exhaustive()
    }
}

impl PreparedModelCall for PreparedMessages {
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
            transport,
            body,
            prefix_digest,
            ..
        } = *self;
        let accumulator = wire::StreamAccumulator::with_prefix(prefix_digest)
            .with_argument_limits(ToolArgumentLimits::managed());
        crate::send(transport, body, accumulator)
    }
}

#[cfg(test)]
impl PreparedMessages {
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
/// Refuses an unsupported model or endpoint, a request outside the managed grammar, a kept turn
/// with no admissible form, an output reservation that is absent or above the ceiling, and a
/// candidate that does not fit.
pub fn prepare(llm: &AnthropicLlm, managed: ManagedRequest) -> LlmResult<PreparedMessages> {
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
    let caps = llm.capabilities(&request.model);
    let ceiling = caps
        .max_output_tokens
        .unwrap_or(u32::MAX)
        .min(crate::model_max_output_tokens(&request.model));
    check_output(request.max_tokens, ceiling)?;
    validate_sequence(&request.messages)?;
    nanus_ports::capabilities::validate_image_input(caps, &request)?;
    nanus_ports::tool_support::validate_input(
        llm.tool_call_support(&request.model, request.reasoning_effort),
        &request,
    )?;

    // Encoded once: the estimate, the digest and the dispatch all read this one value.
    let (payload, skipped) = wire::build_counted(config, &request);
    if skipped > 0 {
        return Err(incompatible(
            "a kept assistant turn carries only replay this request cannot admit",
        ));
    }
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
    let transport = llm.transport();
    let selection = SelectionIdentity {
        provider: PROVIDER.to_owned(),
        endpoint_digest: Digest::of(transport.endpoint.as_bytes()),
        protocol: PROTOCOL.to_owned(),
        model: request.model,
        effort: payload["output_config"]["effort"]
            .as_str()
            .map(str::to_owned),
        epoch: selection_epoch,
    };
    Ok(PreparedMessages {
        transport,
        prefix_digest: wire::request_prefix(&payload),
        digest: Digest::of(body.as_bytes()),
        body,
        estimate,
        selection,
    })
}

/// Requires an explicit reservation no larger than the ceiling.
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
/// Replay is not judged here: whether a turn's signed blocks are admitted depends on the body
/// they are encoded into, so the encoder reports what it could not say.
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
    // Only the message directly after the first user message can be the generated one. A
    // reply elsewhere that happens to open with the label is the model's own text and is sent
    // as it is: identifying generated data anywhere by its words would let one reply wedge
    // every later request of its session.
    let anchor_generated = conversation.get(1).is_some_and(is_generated);
    if anchor_generated && conversation.len() <= 2 {
        return Err(incompatible("generated data is never the last message"));
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
