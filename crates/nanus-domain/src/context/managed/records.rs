//! The records managed context writes into a session, and the values they carry.
//!
//! Every type here is a boundary payload: it is written into a version-3 session body or sent
//! over the link, so each one rejects unknown fields and each one with an invariant the JSON
//! shape cannot express has a `validate` that states it. Schema validity is necessary and never
//! sufficient — a revision that parses is not yet a revision that may be installed.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::ids::{ArtifactId, Digest, ErrorCode, FragmentId, NoteId};
use super::limits;

/// Whether a session's requests are built by replaying the log or by the managed projection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextMode {
    /// Whole-log replay with the whole-turn fitter; the behaviour every session had before.
    #[default]
    Legacy,
    /// Fragment selection, working notes and checkpoints.
    Managed,
}

impl ContextMode {
    /// Returns the mode as it is written.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Legacy => "legacy",
            Self::Managed => "managed",
        }
    }

    /// Parses a mode name.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "legacy" => Some(Self::Legacy),
            "managed" => Some(Self::Managed),
            _ => None,
        }
    }
}

/// How a session's context is managed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextPolicy {
    /// Legacy replay or managed projection.
    pub mode: ContextMode,
    /// Output tokens every managed request reserves.
    pub output_reserve_tokens: u32,
    /// Whether shell output is archived before its preview is cut.
    pub capture_shell: bool,
    /// The policy version, which fixes every limit and the rendered text.
    pub policy_version: u32,
}

impl Default for ContextPolicy {
    fn default() -> Self {
        Self {
            mode: ContextMode::Legacy,
            output_reserve_tokens: limits::DEFAULT_OUTPUT_RESERVE_TOKENS,
            capture_shell: false,
            policy_version: limits::POLICY_VERSION,
        }
    }
}

impl ContextPolicy {
    /// The managed policy with the default reservation and no capture.
    #[must_use]
    pub fn managed() -> Self {
        Self {
            mode: ContextMode::Managed,
            ..Self::default()
        }
    }

    /// Checks the combinations the fields cannot express alone.
    ///
    /// # Errors
    ///
    /// Refuses a zero reservation, an unknown policy version, and capture without managed mode:
    /// capture is evidence for recall, and recall exists only in managed mode.
    pub fn validate(&self) -> Result<(), ErrorCode> {
        if self.policy_version != limits::POLICY_VERSION || self.output_reserve_tokens == 0 {
            return Err(ErrorCode::UnsupportedMode);
        }
        if self.capture_shell && self.mode != ContextMode::Managed {
            return Err(ErrorCode::UnsupportedMode);
        }
        Ok(())
    }

    /// Returns the policy a disable or reset selects: legacy, capture off, reservation kept.
    #[must_use]
    pub const fn disabled(self) -> Self {
        Self {
            mode: ContextMode::Legacy,
            capture_shell: false,
            output_reserve_tokens: self.output_reserve_tokens,
            policy_version: self.policy_version,
        }
    }
}

/// An immutable point in a session: how many events, their digest, and the accepted revision.
///
/// Sequence `N` is inside the frontier exactly when `N < event_count`, so an empty log is
/// frontier zero. The digest covers the stored header and the first `event_count` event lines
/// byte for byte, which is what lets a stale proposal or a shortened file be told apart from the
/// state it claims to describe.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextFrontier {
    /// The session the frontier belongs to.
    pub session_id: String,
    /// The exclusive event count.
    pub event_count: u64,
    /// SHA-256 of the stored header and the first `event_count` event lines.
    pub prefix_sha256: Digest,
    /// The context revision accepted at this point.
    pub projection_revision: u64,
}

/// Whether a source reference names a log event or an archived artifact.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    /// A neutral text field of a session event.
    Event,
    /// A retained archive object.
    Artifact,
}

/// The exact field a source reference addresses.
///
/// Never a serialized envelope and never an opaque replay field: a reference can only point at
/// text a person could have read in the transcript, or at archived raw bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceField {
    /// A user message's text.
    UserText,
    /// An assistant message's visible text.
    AssistantText,
    /// An assistant message's reasoning text.
    AssistantReasoning,
    /// A tool result's rendered text.
    ToolText,
    /// One text block of a tool result.
    ToolBlock,
    /// An archived artifact's raw bytes.
    Artifact,
}

impl SourceField {
    /// Returns the field as it is written.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UserText => "user_text",
            Self::AssistantText => "assistant_text",
            Self::AssistantReasoning => "assistant_reasoning",
            Self::ToolText => "tool_text",
            Self::ToolBlock => "tool_block",
            Self::Artifact => "artifact",
        }
    }
}

/// A resolved byte range of a current-session source, with the digest of the whole source.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceRef {
    /// Event or artifact.
    pub kind: SourceKind,
    /// The event's sequence, for an event source.
    pub event_seq: Option<u64>,
    /// The text block's index, for a `tool_block` source.
    pub block_index: Option<u8>,
    /// The artifact, for an artifact source.
    pub artifact_id: Option<ArtifactId>,
    /// The first byte of the range.
    pub offset: u64,
    /// The range's length in bytes.
    pub length: u64,
    /// SHA-256 of the *entire* selected source, independent of the range.
    pub source_digest: Digest,
    /// The addressed field.
    pub field: SourceField,
}

impl SourceRef {
    /// Checks that the selector fields agree with the kind and field.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::InvalidReference`] for a mixed selector or an overflowing range.
    pub fn validate_shape(&self) -> Result<(), ErrorCode> {
        check_selector(
            self.kind,
            self.field,
            self.event_seq,
            self.block_index,
            self.artifact_id.as_ref(),
        )?;
        self.offset
            .checked_add(self.length)
            .map(|_| ())
            .ok_or(ErrorCode::InvalidReference)
    }
}

/// Checks the selector rule shared by a source reference and a recall target.
pub(crate) fn check_selector(
    kind: SourceKind,
    field: SourceField,
    event_seq: Option<u64>,
    block_index: Option<u8>,
    artifact: Option<&ArtifactId>,
) -> Result<(), ErrorCode> {
    let valid = match kind {
        SourceKind::Event => {
            event_seq.is_some()
                && artifact.is_none()
                && field != SourceField::Artifact
                && (field == SourceField::ToolBlock) == block_index.is_some()
        }
        SourceKind::Artifact => {
            field == SourceField::Artifact
                && artifact.is_some()
                && event_seq.is_none()
                && block_index.is_none()
        }
    };
    if valid {
        Ok(())
    } else {
        Err(ErrorCode::InvalidReference)
    }
}

/// How much a working note's author claims to know.
///
/// None of these certify truth. They are the author's labels, kept so a later reader — and the
/// model itself — can tell a claim it observed from one it inferred.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoteCategory {
    /// Seen in a cited source.
    Observed,
    /// Concluded from cited sources.
    Inferred,
    /// Still open.
    Unresolved,
    /// Replaced by a later note or observation.
    Superseded,
}

impl NoteCategory {
    /// Returns the category as it is written.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Observed => "observed",
            Self::Inferred => "inferred",
            Self::Unresolved => "unresolved",
            Self::Superseded => "superseded",
        }
    }
}

/// One bounded, source-backed claim the model keeps across omitted history.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkingNote {
    /// Stable id.
    pub id: NoteId,
    /// The claim, at most 512 Unicode scalar values.
    pub claim: String,
    /// The author's label.
    pub category: NoteCategory,
    /// One to four references to the evidence.
    pub sources: Vec<SourceRef>,
}

/// Who authored a projection revision.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RevisionAuthor {
    /// A validated model proposal.
    Model,
    /// Deterministic hard fitting.
    Automatic,
    /// A host or human action.
    Host,
}

/// Why a projection revision was made.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RevisionReason {
    /// The model proposed it through `context_manage`.
    ModelProposal,
    /// The request did not fit and the oldest eligible fragments were hidden.
    HardFit,
    /// A person reset the session's context.
    Reset,
}

/// One accepted selection: the *full* hidden set and the *complete* note array, never a patch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectionRevision {
    /// This revision, `base_revision + 1`.
    pub revision: u64,
    /// The revision it was made against.
    pub base_revision: u64,
    /// The frontier it was made against.
    pub base_frontier: ContextFrontier,
    /// Every hidden fragment, strictly increasing.
    pub hidden: Vec<FragmentId>,
    /// Every accepted note.
    pub notes: Vec<WorkingNote>,
    /// Who made it.
    pub author: RevisionAuthor,
    /// Why.
    pub reason: RevisionReason,
    /// The policy version it was validated under.
    pub policy_version: u32,
    /// The decision that accepted it.
    pub decision_id: String,
    /// The goal revision its notes are bound to.
    pub goal_revision: Option<u64>,
    /// SHA-256 of the canonical note array with the policy and renderer version.
    pub notes_digest: Digest,
    /// The snapshot profile it was made against.
    pub base_profile_digest: Digest,
}

impl ProjectionRevision {
    /// Checks the invariants a revision record states about itself.
    ///
    /// # Errors
    ///
    /// Returns the code for the first broken invariant.
    pub fn validate(&self) -> Result<(), ErrorCode> {
        let next = self
            .base_revision
            .checked_add(1)
            .ok_or(ErrorCode::StaleBase)?;
        if self.revision != next || self.base_frontier.projection_revision != self.base_revision {
            return Err(ErrorCode::StaleBase);
        }
        if self.policy_version != limits::POLICY_VERSION {
            return Err(ErrorCode::UnsupportedMode);
        }
        let increasing = self.hidden.windows(2).all(|pair| pair[0] < pair[1]);
        if !increasing || self.hidden.len() > limits::HIDDEN_MAX {
            return Err(ErrorCode::InvalidFragment);
        }
        if self.decision_id.is_empty() || self.decision_id.len() > limits::DECISION_ID_MAX {
            return Err(ErrorCode::InvalidFragment);
        }
        if self.reason == RevisionReason::Reset
            && (!self.hidden.is_empty() || !self.notes.is_empty())
        {
            return Err(ErrorCode::InvalidFragment);
        }
        super::notes::validate_notes(&self.notes)?;
        if super::notes::notes_digest(&self.notes) != self.notes_digest {
            return Err(ErrorCode::InvalidReference);
        }
        Ok(())
    }
}

/// Who changed a session's context mode. Never the model.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModeActor {
    /// A person, through a client or the command line.
    Human,
    /// The host, applying its configuration.
    Host,
}

/// Why a session's context mode changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModeReason {
    /// Managed mode was turned on.
    Enable,
    /// Managed mode was turned off.
    Disable,
    /// The selection was emptied and legacy selected.
    Reset,
    /// The host's configuration changed a field.
    ConfigurationChange,
}

/// A change of a session's context policy, persisted before it is acknowledged.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextModeRecord {
    /// The policy after the change.
    pub policy: ContextPolicy,
    /// Who changed it.
    pub actor: ModeActor,
    /// Why.
    pub reason: ModeReason,
    /// The accepted revision when it changed.
    pub previous_revision: u64,
}

/// What became of one proposal, automatic fit or reset.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionOutcome {
    /// Validated input is waiting for the step to settle.
    Staged,
    /// A checkpoint acknowledged the revision.
    Accepted,
    /// It was refused; the previous revision stays selected.
    Rejected,
    /// The turn stopped before validation began.
    Cancelled,
}

/// The outcome of one context decision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextDecision {
    /// Host-generated id.
    pub decision_id: String,
    /// What happened.
    pub outcome: DecisionOutcome,
    /// The revision accepted, when one was.
    pub revision: Option<u64>,
    /// Why it was refused, when it was.
    pub error_code: Option<ErrorCode>,
}

/// Which output stream an artifact captured.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureStream {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}

impl CaptureStream {
    /// Returns the stream as it is written.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        }
    }
}

/// How much of a stream an artifact holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureStatus {
    /// Every observed byte, through end of file.
    Complete,
    /// A prefix of what was observed.
    Partial,
    /// Nothing was retained.
    Unavailable,
}

/// Why a capture ended the way it did.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureReason {
    /// The stream reached end of file.
    Eof,
    /// A quota refused more bytes.
    Quota,
    /// The archive write failed.
    WriteError,
    /// Reading the pipe failed.
    ReadError,
    /// A sink deadline passed.
    Timeout,
    /// The call was cancelled.
    Cancelled,
    /// The drain deadline passed before end of file.
    DrainExpired,
    /// The host cannot capture here.
    Unsupported,
}

/// The authoritative manifest of one archived stream.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactReceipt {
    /// The object, absent when nothing was retained.
    pub artifact_id: Option<ArtifactId>,
    /// The call whose output this is.
    pub call_id: String,
    /// Which stream.
    pub stream: CaptureStream,
    /// Bytes retained in the object.
    pub retained_bytes: u64,
    /// Bytes the pump actually read.
    pub observed_bytes: u64,
    /// SHA-256 of the retained bytes.
    pub retained_sha256: Option<Digest>,
    /// How much was retained.
    pub status: CaptureStatus,
    /// Why it ended.
    pub reason: CaptureReason,
    /// Always `raw`.
    pub encoding: RawEncoding,
    /// SHA-256 of each 64 KiB chunk, including a shorter final one.
    pub chunk_sha256: Vec<Digest>,
}

/// The only artifact encoding version 1 writes: the bytes exactly as the pipe gave them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RawEncoding {
    /// Unmodified bytes.
    Raw,
}

impl ArtifactReceipt {
    /// Checks the status, length, digest and chunk invariants.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::SourceCorrupt`] for a receipt that contradicts itself.
    pub fn validate(&self) -> Result<(), ErrorCode> {
        let bad = Err(ErrorCode::SourceCorrupt);
        if self.call_id.is_empty() || self.call_id.len() > 256 {
            return bad;
        }
        if self.retained_bytes > limits::CAPTURE_STREAM_BYTES_MAX {
            return bad;
        }
        let chunks = self
            .retained_bytes
            .div_ceil(limits::ARTIFACT_CHUNK_BYTES_U64);
        let consistent = match self.status {
            CaptureStatus::Complete => {
                self.artifact_id.is_some()
                    && self.retained_sha256.is_some()
                    && self.retained_bytes == self.observed_bytes
                    && self.reason == CaptureReason::Eof
            }
            CaptureStatus::Partial => {
                self.artifact_id.is_some()
                    && self.retained_sha256.is_some()
                    && self.retained_bytes <= self.observed_bytes
                    && self.reason != CaptureReason::Eof
            }
            CaptureStatus::Unavailable => {
                self.artifact_id.is_none()
                    && self.retained_sha256.is_none()
                    && self.retained_bytes == 0
                    && self.chunk_sha256.is_empty()
            }
        };
        let chunked = self.status == CaptureStatus::Unavailable
            || u64::try_from(self.chunk_sha256.len()).is_ok_and(|count| count == chunks);
        if consistent && chunked && self.chunk_sha256.len() <= limits::ARTIFACT_CHUNKS_MAX {
            Ok(())
        } else {
            bad
        }
    }
}

/// The exact route a request took, without credentials.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectionIdentity {
    /// The provider name.
    pub provider: String,
    /// SHA-256 of the canonical endpoint.
    pub endpoint_digest: Digest,
    /// The wire protocol.
    pub protocol: String,
    /// The model id.
    pub model: String,
    /// The reasoning effort, when one was in force.
    pub effort: Option<String>,
    /// The runner's selection epoch.
    pub epoch: u64,
}

/// Provider-reported usage with every counter separately nullable: missing is not zero.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageObservation {
    /// Input tokens.
    pub prompt_tokens: Option<u64>,
    /// Output tokens.
    pub completion_tokens: Option<u64>,
    /// Reasoning tokens.
    pub reasoning_tokens: Option<u64>,
    /// Prompt-cache reads.
    pub cache_read_tokens: Option<u64>,
    /// Prompt-cache writes.
    pub cache_write_tokens: Option<u64>,
    /// What the counters mean.
    pub semantics_version: String,
    /// An allowlisted copy of the provider's counters.
    pub raw_counters: BTreeMap<String, u64>,
}

impl UsageObservation {
    /// The semantics of a [`crate::Usage`] reported through the neutral stream.
    pub const NEUTRAL_SEMANTICS: &'static str = "nanus.usage.v1";

    /// Reads the neutral usage record into nullable counters.
    ///
    /// The neutral record cannot say which counters a provider omitted, so every one it carries
    /// is reported; cache writes are not in it at all and stay absent.
    #[must_use]
    pub fn from_usage(usage: &crate::Usage) -> Self {
        let mut raw_counters = BTreeMap::new();
        raw_counters.insert(
            "cache_hit_tokens".to_owned(),
            u64::from(usage.cache_hit_tokens),
        );
        raw_counters.insert(
            "cache_miss_tokens".to_owned(),
            u64::from(usage.cache_miss_tokens),
        );
        Self {
            prompt_tokens: Some(u64::from(usage.prompt_tokens)),
            completion_tokens: Some(u64::from(usage.completion_tokens)),
            reasoning_tokens: Some(u64::from(usage.reasoning_tokens)),
            cache_read_tokens: Some(u64::from(usage.cache_hit_tokens)),
            cache_write_tokens: None,
            semantics_version: Self::NEUTRAL_SEMANTICS.to_owned(),
            raw_counters,
        }
    }
}

/// Whether an attempt record is the intent or the outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptPhase {
    /// Recorded before HTTP; it may never have been sent.
    Started,
    /// Recorded at the settled checkpoint.
    Finished,
}

/// How a request attempt ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptOutcome {
    /// An admitted response completed.
    Completed,
    /// The stream or its validation failed.
    Failed,
    /// The turn was stopped.
    Cancelled,
    /// It was refused before dispatch.
    Refused,
    /// A crash left the intent with no outcome: it may or may not have been sent.
    UnknownDispatch,
}

/// Monotonic durations of one attempt, each absent when it was not measured.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttemptTimings {
    /// Until the response head.
    pub response_head_ms: Option<u64>,
    /// Until the first token.
    pub first_token_ms: Option<u64>,
    /// From the first token to the last.
    pub decode_ms: Option<u64>,
    /// The whole attempt.
    pub total_ms: Option<u64>,
}

/// One model request attempt: its intent before HTTP, then its outcome.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestAttemptRecord {
    /// Stable across the start and finish records.
    pub attempt_id: String,
    /// The attempt this one retried.
    pub retry_of: Option<String>,
    /// The turn.
    pub turn: u64,
    /// The step.
    pub step: u64,
    /// The route.
    pub selection: SelectionIdentity,
    /// The context revision the request was built from.
    pub projection_revision: u64,
    /// SHA-256 of the body actually prepared for dispatch.
    pub request_digest: Digest,
    /// Intent or outcome.
    pub phase: AttemptPhase,
    /// The outcome, for a finished record.
    pub outcome: Option<AttemptOutcome>,
    /// Usage the provider reported, when it did.
    pub usage: Option<UsageObservation>,
    /// The admitted assistant event, for a completed attempt.
    pub assistant_seq: Option<u64>,
    /// The management fragment the request carried, copied from its preparation.
    pub included_management_fragments: Vec<FragmentId>,
    /// Wall-clock start.
    pub started_at_ms: u64,
    /// Wall-clock finish.
    pub finished_at_ms: Option<u64>,
    /// Monotonic durations.
    pub timings_ms: Option<AttemptTimings>,
}

impl RequestAttemptRecord {
    /// Checks the phase rules.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::SourceCorrupt`] for a record that contradicts its phase.
    pub fn validate(&self) -> Result<(), ErrorCode> {
        let ids_ok = !self.attempt_id.is_empty()
            && self.attempt_id.len() <= limits::DECISION_ID_MAX
            && self.included_management_fragments.len() <= 1;
        let phase_ok = match self.phase {
            AttemptPhase::Started => {
                self.outcome.is_none()
                    && self.usage.is_none()
                    && self.assistant_seq.is_none()
                    && self.finished_at_ms.is_none()
                    && self.timings_ms.is_none()
            }
            AttemptPhase::Finished => {
                self.outcome.is_some()
                    && (self.assistant_seq.is_none()
                        || self.outcome == Some(AttemptOutcome::Completed))
            }
        };
        if ids_ok && phase_ok {
            Ok(())
        } else {
            Err(ErrorCode::SourceCorrupt)
        }
    }
}

/// Why a recovery record was written.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryReason {
    /// A checkpoint held an open turn, which is closed as interrupted.
    SettledOpenTurn,
    /// An uncertain commit was reconciled against the disk.
    CommitReconciled,
}

/// The record a resumed session writes before admitting new work.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryRecord {
    /// The frontier recovery started from.
    pub recovered_frontier: ContextFrontier,
    /// The turn it closed.
    pub turn: u64,
    /// Intents with no outcome, now marked unknown.
    pub unmatched_attempt_ids: Vec<String>,
    /// Why.
    pub reason: RecoveryReason,
}

/// How strong a checkpoint's durability claim is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Durability {
    /// Atomic replacement survives a process crash.
    ProcessCrash,
    /// File and parent directory are synchronized.
    PowerLoss,
}

/// What a successful checkpoint commit proves.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointReceipt {
    /// The frontier now on disk; its digest covers the whole stored file.
    pub frontier: ContextFrontier,
    /// SHA-256 of the event lines alone.
    pub body_digest: Digest,
    /// The durability grade.
    pub durability: Durability,
}

/// Everything a request was built under, so a stale proposal can be detected exactly.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotProfile {
    /// The route.
    pub selection: SelectionIdentity,
    /// SHA-256 of the system prompt.
    pub system_prompt_digest: Digest,
    /// SHA-256 of the offered schemas.
    pub tool_schema_digest: Digest,
    /// The policy.
    pub policy: ContextPolicy,
    /// The goal revision.
    pub goal_revision: Option<u64>,
}

impl SnapshotProfile {
    /// Returns the digest a proposal must echo.
    #[must_use]
    pub fn digest(&self) -> Digest {
        Digest::of(serde_json::to_string(self).unwrap_or_default().as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest() -> Digest {
        Digest::of(b"x")
    }

    #[test]
    fn capture_needs_managed_mode_and_a_reservation() {
        assert!(ContextPolicy::default().validate().is_ok());
        assert!(ContextPolicy::managed().validate().is_ok());
        let capture_alone = ContextPolicy {
            capture_shell: true,
            ..ContextPolicy::default()
        };
        assert_eq!(capture_alone.validate(), Err(ErrorCode::UnsupportedMode));
        let zero = ContextPolicy {
            output_reserve_tokens: 0,
            ..ContextPolicy::managed()
        };
        assert_eq!(zero.validate(), Err(ErrorCode::UnsupportedMode));
        let capture = ContextPolicy {
            capture_shell: true,
            ..ContextPolicy::managed()
        };
        assert!(!capture.disabled().capture_shell);
        assert_eq!(capture.disabled().mode, ContextMode::Legacy);
    }

    #[test]
    fn an_unknown_field_is_refused() {
        let raw = r#"{"mode":"managed","output_reserve_tokens":1,"capture_shell":false,
            "policy_version":1,"extra":true}"#;
        assert!(serde_json::from_str::<ContextPolicy>(raw).is_err());
    }

    #[test]
    fn a_source_selector_must_match_its_kind() {
        let mut source = SourceRef {
            kind: SourceKind::Event,
            event_seq: Some(3),
            block_index: None,
            artifact_id: None,
            offset: 0,
            length: 4,
            source_digest: digest(),
            field: SourceField::ToolText,
        };
        assert!(source.validate_shape().is_ok());
        source.block_index = Some(0);
        assert_eq!(source.validate_shape(), Err(ErrorCode::InvalidReference));
        source.field = SourceField::ToolBlock;
        assert!(source.validate_shape().is_ok());
        source.kind = SourceKind::Artifact;
        assert_eq!(source.validate_shape(), Err(ErrorCode::InvalidReference));
        source.offset = u64::MAX;
        source.kind = SourceKind::Event;
        assert_eq!(source.validate_shape(), Err(ErrorCode::InvalidReference));
    }

    fn receipt(status: CaptureStatus, retained: u64, observed: u64) -> ArtifactReceipt {
        let available = status != CaptureStatus::Unavailable;
        let chunks = usize::try_from(retained.div_ceil(65_536)).unwrap_or(0);
        ArtifactReceipt {
            artifact_id: available
                .then(|| ArtifactId::parse("a:0190a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b"))
                .flatten(),
            call_id: "c-1".to_owned(),
            stream: CaptureStream::Stdout,
            retained_bytes: retained,
            observed_bytes: observed,
            retained_sha256: available.then(digest),
            status,
            reason: match status {
                CaptureStatus::Complete => CaptureReason::Eof,
                CaptureStatus::Partial => CaptureReason::Quota,
                CaptureStatus::Unavailable => CaptureReason::Unsupported,
            },
            encoding: RawEncoding::Raw,
            chunk_sha256: if available {
                vec![digest(); chunks]
            } else {
                Vec::new()
            },
        }
    }

    #[test]
    fn a_receipt_states_its_lengths_and_chunks_truthfully() {
        assert!(
            receipt(CaptureStatus::Complete, 65_537, 65_537)
                .validate()
                .is_ok()
        );
        assert!(receipt(CaptureStatus::Complete, 0, 0).validate().is_ok());
        assert!(receipt(CaptureStatus::Partial, 10, 99).validate().is_ok());
        assert!(
            receipt(CaptureStatus::Unavailable, 0, 99)
                .validate()
                .is_ok()
        );
        assert!(receipt(CaptureStatus::Complete, 10, 11).validate().is_err());
        let mut short = receipt(CaptureStatus::Complete, 65_537, 65_537);
        short.chunk_sha256.pop();
        assert!(short.validate().is_err());
        let mut partial_eof = receipt(CaptureStatus::Partial, 1, 2);
        partial_eof.reason = CaptureReason::Eof;
        assert!(partial_eof.validate().is_err());
    }

    #[test]
    fn an_attempt_record_obeys_its_phase() {
        let mut record = RequestAttemptRecord {
            attempt_id: "a1".to_owned(),
            retry_of: None,
            turn: 1,
            step: 1,
            selection: SelectionIdentity {
                provider: "p".to_owned(),
                endpoint_digest: digest(),
                protocol: "chat".to_owned(),
                model: "m".to_owned(),
                effort: None,
                epoch: 0,
            },
            projection_revision: 0,
            request_digest: digest(),
            phase: AttemptPhase::Started,
            outcome: None,
            usage: None,
            assistant_seq: None,
            included_management_fragments: Vec::new(),
            started_at_ms: 0,
            finished_at_ms: None,
            timings_ms: None,
        };
        assert!(record.validate().is_ok());
        record.outcome = Some(AttemptOutcome::Completed);
        assert!(record.validate().is_err());
        record.phase = AttemptPhase::Finished;
        record.assistant_seq = Some(4);
        assert!(record.validate().is_ok());
        record.outcome = Some(AttemptOutcome::Failed);
        assert!(
            record.validate().is_err(),
            "only a completed attempt has a response"
        );
    }
}
