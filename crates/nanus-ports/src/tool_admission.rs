//! Optional pure host admission for complete pending tool batches.
use nanus_domain::{SessionEvent, SessionId, ToolCall, ToolResult};

use crate::{ChatRequest, LlmResult, ModelCapabilities, RequestEstimate};

/// Fixed image-free admission refusal; hosts reserve its complete failure framing.
pub const TOOL_ADMISSION_REFUSAL_TEXT: &str =
    "tool not run: host batch admission refused capacity or authority";
/// Fixed replacement for a refused actual observation, with complete failure framing.
pub const TOOL_ADMISSION_RESULT_REFUSAL_TEXT: &str =
    "tool result refused: content exceeded admitted capacity or authority";

/// A host capacity/authority check that refused; never contains a credential.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("tool admission failed: {message}")]
pub struct AdmissionError {
    /// Diagnostic for the caller; model-visible refusals use a fixed bounded message.
    pub message: String,
}

/// Immutable projection after ordinary approval and before any tool effects.
///
/// Add every bounded failure slot, then reserve success envelopes in call order using
/// the same pure estimator and whole-turn fitting policy. Never drop the newest turn.
/// Endpoint/plan/account/protocol and authority epochs are captured by the host itself.
/// This borrowed view exposes no mutable session, model handle, credential or I/O port.
pub struct ToolBatchProjection<'a> {
    /// Stable session identity; runner identity is captured by the host callback.
    pub session_id: &'a SessionId,
    /// Parent turn and step.
    pub position: (u32, u32),
    /// Non-wrapping held adapter/model/effort generation.
    pub selection_epoch: u64,
    /// Complete unelided prospective system/schema/history request with every pending
    /// call and its actual denial or fixed failure slot; placeholders are not durable results.
    pub request: &'a ChatRequest,
    /// Same failure-slot base after the runner's whole-turn fitting. Full retained
    /// history above stays available for conservative bounds and host fitting policy.
    pub fitted_base: &'a ChatRequest,
    /// Actual retained durable events, including records omitted by message replay.
    /// No prospective failure slots have been appended here. Hosts bound complete
    /// checkpoint framing/header and future records separately from provider fitting.
    pub events: &'a [SessionEvent],
    /// All calls, including denied ones, in model order.
    pub calls: &'a [ToolCall],
    /// Already-refused observations; None means ordinary approval permits dispatch.
    pub outcomes: &'a [Option<ToolResult>],
    /// Metadata from the exact held model adapter.
    pub capabilities: ModelCapabilities,
    /// Same held adapter's pure translated request estimator; no HTTP/secret access.
    pub estimate: &'a dyn Fn(&ChatRequest) -> LlmResult<RequestEstimate>,
    /// The managed projection, when the session is in managed mode; `None` otherwise.
    ///
    /// `request` above stays the full original unelided prospective request and `events` the
    /// actual retained records: a host's durable quotas count raw records, while a lease that
    /// validates "the next request" validates this effective one.
    pub managed: Option<ManagedProjection<'a>>,
}

/// The effective request a managed step would send, beside the original one.
pub struct ManagedProjection<'a> {
    /// The accepted context revision the effective request was compiled from.
    pub revision: u64,
    /// The effective request with the same failure slots: the base the next step would use.
    pub effective: &'a ChatRequest,
    /// The held adapter's pure estimate of exactly the effective candidate.
    ///
    /// Final-result estimation may substitute only permitted pending result slots; it never
    /// changes the original history or the revision identity.
    pub estimate_effective: &'a dyn Fn(&ChatRequest) -> LlmResult<RequestEstimate>,
}

/// Opt-in complete-batch admission. Callbacks are synchronous and perform no tool
/// effects or provider/secret I/O; caller-local ledger/audit work stays host-owned.
pub trait ToolAdmission {
    /// Reserve the base failure slots and conservative complete success envelopes.
    ///
    /// On error, release any partially acquired logical capacity/unused policy receipts.
    /// A successful owned reservation stays held through ordered append and commit.
    fn reserve(
        &self,
        projection: &ToolBatchProjection<'_>,
    ) -> Result<Box<dyn ToolBatchReservation>, AdmissionError>;

    /// Retire caller-local unused handles when reserve never succeeded, including
    /// cancellation/future drop while awaiting approval or projection failure.
    /// Once reserve succeeds, the owned reservation's Drop has this responsibility.
    fn release_unreserved(&self, calls: &[ToolCall]) {
        let _ = calls;
    }
}

/// An owned logical-capacity lease; implement Drop for cancellation/error/future teardown.
///
/// Capacity is not permission. Caller physical workers retain independent source/process
/// leases until join, even when this object drops. Reserve the runner's fixed image-free
/// refusal text and all previously denied outcomes; errors never roll back tool effects.
pub trait ToolBatchReservation {
    /// Resolve capacity for this exact call once, in call order, before any batch effects.
    fn admit(&self, call: &ToolCall) -> Result<(), AdmissionError>;

    /// Recheck live host scope/selection/payment authority immediately before dispatch.
    /// This is an idempotent check, not a second reservation or source permission grant.
    fn before_dispatch(&self, call: &ToolCall) -> Result<(), AdmissionError>;

    /// Validate the caller's raw observation and globally bounded/media-normalized model
    /// result before delivery. Raw value/content remain untrusted: use bounded counters,
    /// never an unbounded serialization. Normalization can discard the private value.
    /// Refusal replaces model content with the reserved fixed image-free failure;
    /// caller audit policy must preserve already-performed effects independently.
    fn validate_result(
        &self,
        call: &ToolCall,
        raw: &ToolResult,
        retained: &ToolResult,
    ) -> Result<(), AdmissionError>;

    /// Commit the fitted next request after every observation was appended in call order.
    /// Errors close context before model HTTP. Capacity remains held until this returns.
    fn commit(&self, request: &ChatRequest) -> Result<(), AdmissionError>;
}
