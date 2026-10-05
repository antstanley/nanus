//! Optional host reservations before user/model records enter the original session.
use nanus_domain::{SessionEvent, SessionId, SessionSeq};

use crate::{
    AdmissionError, ChatRequest, LlmResult, ModelCapabilities, ReasoningEffort, RequestEstimate,
};

/// Static refusal diagnostic: arbitrary host errors never enter a capacity-exhausted log.
pub const RECORD_ADMISSION_REFUSAL_TEXT: &str =
    "host record admission refused capacity or authority";

/// Original prefix and literal user text, before either turn-start or user append.
pub struct TurnRecordProjection<'a> {
    /// Original session identity, not an allocated prospective copy.
    pub session_id: &'a SessionId,
    /// Every original retained durable event, including records omitted by replay.
    pub events: &'a [SessionEvent],
    /// Actual next durable sequence.
    pub next_sequence: SessionSeq,
    /// Proposed next turn.
    pub turn: u32,
    /// Exact literal caller input; no trimming or template interpretation.
    pub message: &'a str,
}

/// Original step prefix and exact held request, before step-start and provider contact.
pub struct StepRecordProjection<'a> {
    /// Original prefix identity.
    pub session_id: &'a SessionId,
    /// All actual durable records; no model/failure placeholder has been appended.
    pub events: &'a [SessionEvent],
    /// Actual next durable sequence.
    pub next_sequence: SessionSeq,
    /// Original turn and proposed step.
    pub position: (u32, u32),
    /// Non-wrapping held adapter/model/effort generation.
    pub selection_epoch: u64,
    /// Complete unelided system/schema/history request.
    pub request: &'a ChatRequest,
    /// Same request after existing whole-turn fitting.
    pub fitted_request: &'a ChatRequest,
    /// Effective effort recorded with the response, including the adapter default.
    pub effort: Option<ReasoningEffort>,
    /// Exact held model's metadata.
    pub capabilities: ModelCapabilities,
    /// The same held adapter's pure estimator; performs no provider/credential I/O.
    pub estimate: &'a dyn Fn(&ChatRequest) -> LlmResult<RequestEstimate>,
}

/// Exact original assistant event, before append or copied call-audit allocation.
pub struct ModelRecordProjection<'a> {
    /// Original session identity.
    pub session_id: &'a SessionId,
    /// Original retained prefix, ending at the admitted step start.
    pub events: &'a [SessionEvent],
    /// Sequence the assistant will receive.
    pub next_sequence: SessionSeq,
    /// Original turn and step.
    pub position: (u32, u32),
    /// Same held selection generation as step reservation.
    pub selection_epoch: u64,
    /// The actual proposed `AssistantMessage`, constructed by moving assembled fields.
    /// Hosts count raw arguments before its custom serializer and replay before joins.
    pub assistant: &'a SessionEvent,
}

/// Original closing record, checked against capacity already owned by the reservation.
pub struct RecordEndProjection<'a> {
    /// Original session identity.
    pub session_id: &'a SessionId,
    /// Complete retained prefix at this exact phase.
    pub events: &'a [SessionEvent],
    /// Sequence the closing record will receive.
    pub next_sequence: SessionSeq,
    /// Actual proposed `StepEnd` or `TurnEnd`; no copied state supplies original authority.
    pub event: &'a SessionEvent,
}

/// Optional synchronous admission, without source/process/provider/key effects.
/// Default absence preserves stock behavior. Capacity never grants execution authority.
pub trait RecordAdmission {
    /// Reserve complete turn-start/user and all closing paths before append.
    /// Refusal must release partial logical reservations; the session remains unchanged.
    fn reserve_turn(
        &self,
        projection: &TurnRecordProjection<'_>,
    ) -> Result<Box<dyn TurnRecordReservation>, AdmissionError>;
}

/// Turn capacity stays owned through `TurnEnd` or exceptional future teardown.
/// Implement `Drop` for logical release; physical workers retain independent leases.
pub trait TurnRecordReservation {
    /// Reserve the complete possible model opening and step closing before model contact.
    /// Share the original caller ledger with any complete tool-batch reservation.
    fn reserve_step(
        &self,
        projection: &StepRecordProjection<'_>,
    ) -> Result<Box<dyn StepRecordReservation>, AdmissionError>;

    /// Validate an actual `TurnEnd` before append. Failure cannot fabricate balanced history.
    fn validate_end(&self, projection: &RecordEndProjection<'_>) -> Result<(), AdmissionError>;
}

/// Step capacity stays owned through all tools, ordered results and `StepEnd`.
/// Default composition supplies no implementation or guessed capacity policy.
pub trait StepRecordReservation {
    /// Check complete original assistant and every future ordered call audit before copies,
    /// tool policy, approval or dispatch. Refusal retains none of the assistant/call records.
    fn validate_model(&self, projection: &ModelRecordProjection<'_>) -> Result<(), AdmissionError>;

    /// Validate the actual `StepEnd` against its reserved success/failure/interruption path.
    fn validate_end(&self, projection: &RecordEndProjection<'_>) -> Result<(), AdmissionError>;
}
