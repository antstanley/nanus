//! Managed context: bounded, recoverable selection of what a request carries.
//!
//! The legacy fitter drops whole old user turns when a conversation outgrows its budget. Managed
//! context instead keeps **every user message** and selects among the model's own work — complete
//! assistant/tool *fragments* — while bounded, source-backed working notes and an explicit recall
//! path keep omitted evidence reachable. It is opt-in per session, and a session that never
//! enables it is read, replayed and written exactly as before.
//!
//! This module is the pure half. It owns:
//!
//! - the boundary payloads a version-3 session records and the link carries ([`records`],
//!   [`tooling`]), each validated beyond its JSON shape;
//! - fragment derivation, keyed by call identity ([`fragments`]);
//! - the fold of the accepted state and what it protects ([`state`]);
//! - compilation of the effective conversation ([`compile`]) and deterministic hard fitting
//!   ([`fit`]);
//! - proposal validation, staging and the revisions they become ([`proposal`]).
//!
//! It performs no I/O and grants no authority. The raw log stays authoritative: an effective
//! request is a derived view, and nothing here rewrites, reorders or deletes an event.

pub mod compile;
pub mod fit;
pub mod fragments;
pub mod ids;
pub mod limits;
pub mod notes;
pub mod proposal;
pub mod records;
pub mod state;
pub mod tooling;

pub use compile::{
    Effective, GoalProvenance, MANUAL_POLICY_V1, NoticeFacts, Reminder, Selection,
    derive_effective_context, manual_policy_digest,
};
pub use fragments::{Fragment, Fragments, MANAGE_TOOL, RECALL_TOOL};
pub use ids::{ArtifactId, Digest, ErrorCode, FragmentId, Hasher, NoteId};
pub use records::{
    ArtifactReceipt, AttemptOutcome, AttemptPhase, AttemptTimings, CaptureReason, CaptureStatus,
    CaptureStream, CheckpointReceipt, ContextDecision, ContextFrontier, ContextMode,
    ContextModeRecord, ContextPolicy, DecisionOutcome, Durability, ModeActor, ModeReason,
    NoteCategory, ProjectionRevision, RawEncoding, RecoveryReason, RecoveryRecord,
    RequestAttemptRecord, RevisionAuthor, RevisionReason, SelectionIdentity, SnapshotProfile,
    SourceField, SourceKind, SourceRef, UsageObservation, WorkingNote,
};
pub use state::{ManagedState, RecoveryPlan};
pub use tooling::{
    ContextManageInput, ContextRecallInput, ContextStatus, Coverage, FragmentDescriptor,
    ManageAction, ManageResult, ManageStatus, RecallAction, RecallEncoding, RecallHit,
    RecallResult, RecallStatus, RecallTarget,
};
