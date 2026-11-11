//! The fixed limits of context policy version 1.
//!
//! Initial engineering defaults rather than measured optima. They are constants rather than
//! configuration because every one of them is part of what `policy_version` promises: a future
//! tunable limit keeps an absolute bound and becomes part of the snapshot profile, so a proposal
//! made under one value cannot be accepted under another.
//!
//! [`DEFAULT_WORKING_TARGET_TOKENS`] is the one default here that a host may replace, and it is
//! not a limit in that sense. It decides when the tail asks the model to reduce, what pressure the
//! tail states, how far the runner's own fit goes once a provider's cache has lapsed, and, through
//! that pressure, whether a small hide is staged anyway (see `break_even`). It never decides what a
//! request may carry or whether a staged proposal is accepted: those are measured against the hard
//! allowance alone. A fit it shapes is recorded with its whole hidden set, so reading it
//! back never asks for the target. So a proposal staged under one target is exactly as valid under
//! another, and the target is host configuration beside `context_budget`, which already moves the
//! allowance without being recorded.

/// The only policy version this build reads and writes.
pub const POLICY_VERSION: u32 = 1;

/// The renderer version bound into every recorded revision's notes digest.
///
/// The digest is checked when a revision is read, so moving this number would make every existing
/// managed session unreadable. Where the notice and the working data are rendered is versioned by
/// [`LAYOUT_VERSION`] instead, which no session records.
pub const RENDERER_VERSION: u32 = 1;

/// The placeholder reservation of a following policy, which no model has resolved yet.
///
/// A policy is built before any model is known, and [`ContextPolicy::validate`] refuses a zero
/// reservation, so the placeholder keeps the number non-zero. It is never a reservation in force:
/// recording a following policy replaces it with the resolved number, and a request resolves its
/// own.
///
/// [`ContextPolicy::validate`]: crate::context::managed::records::ContextPolicy::validate
pub const DEFAULT_OUTPUT_RESERVE_TOKENS: u32 = 4_096;

/// The working target of a managed request, in estimated tokens, when the host names none.
///
/// The size the soft machinery keeps a session near: the tail's pressure is the estimate as a
/// percentage of it, and the budget hint is given at [`SOFT_PRESSURE_PERCENT`] of it. The hard
/// allowance is not moved by it, so fitting, which rewrites the cached prefix, still acts only
/// over the allowance while the cache is warm; what moves is when the model is asked to shrink the
/// request itself, in one batched proposal. Once the provider's cache has lapsed, rewriting the
/// prefix costs nothing extra, and the runner fits toward [`TARGET_PERCENT`] of the target itself.
///
/// The number is from the arithmetic of a large window. On a 922,000-token allowance a hint at
/// three quarters of the allowance is not given until about 690,000 estimated tokens, and a
/// recorded session grew to about 574,000 real prompt tokens a step without one. The legacy path
/// dropped history in the same session at about 313,000. At 200,000 the hint is given at 150,000
/// and re-arms below 120,000, so a nudged managed session sends less than a legacy one would,
/// and a step whose cache misses pays for about a third of the prompt it paid for before. It is
/// clamped to the allowance (see `compile::working_target`), so a model whose allowance is
/// smaller — every library host's 64,000 default, a 128,000-token window — is unaffected.
pub const DEFAULT_WORKING_TARGET_TOKENS: u32 = 200_000;

/// Percent of the working target at which a budget hint is given.
///
/// The working target is the admissible input allowance whenever the allowance is the smaller.
pub const SOFT_PRESSURE_PERCENT: u32 = 75;

/// Percent of the working target below which the budget hint re-arms.
pub const REARM_PERCENT: u32 = 60;

/// Completed model steps between two budget hints.
pub const REMINDER_MIN_STEPS: u32 = 4;

/// Percent of the bound automatic reduction aims for once it has to act: the allowance, or the
/// working target when the provider's cache has lapsed.
pub const TARGET_PERCENT: u32 = 60;

/// Raw argument bytes a `context_manage` call may carry, checked before JSON parsing.
pub const MANAGE_ARGUMENT_BYTES_MAX: usize = 16 * 1024;

/// Raw argument bytes a `context_recall` call may carry, checked before JSON parsing.
pub const RECALL_ARGUMENT_BYTES_MAX: usize = 2 * 1024;

/// Notes in one revision.
pub const NOTES_MAX: usize = 32;

/// Unicode scalar values in one note's claim.
pub const NOTE_CHARS_MAX: usize = 512;

/// Source references on one note.
pub const NOTE_SOURCES_MAX: usize = 4;

/// Encoded bytes of a revision's whole note array.
pub const NOTES_BYTES_MAX: usize = 8 * 1024;

/// Hidden fragment ids in one revision. Reaching it refuses further reduction.
pub const HIDDEN_MAX: usize = 4_096;

/// Encoded bytes of one revision record, framing included.
pub const REVISION_RECORD_BYTES_MAX: usize = 64 * 1024;

/// Completed substantive fragments that stay visible however tight the budget is.
pub const RECENT_PROTECTED: usize = 2;

/// Encoded bytes of the generated recovery catalog.
pub const CATALOG_BYTES_MAX: usize = 2 * 1024;

/// Encoded bytes of a managed request's tail: the notice line and the catalog.
///
/// The notice line is numbers, enums and opaque ids, well under a kilobyte at every field's widest,
/// and the catalog has its own bound.
pub const TAIL_BYTES_MAX: usize = CATALOG_BYTES_MAX + 1024;

/// The managed request layout this build compiles.
///
/// Layout 2 put the per-step notice and the catalog in `ChatRequest::context_tail`, which the
/// adapter places after the newest message. Layout 3 anchors the generated working-data message
/// where its text last changed rather than after the first user message, and names it by index in
/// `ChatRequest::working_data` rather than by position: it may follow a tool result or an answer
/// and precede a user or an assistant message.
///
/// An adapter declares it beside the policy version it implements, so one built for an earlier
/// layout — which would refuse, or misread, a working-data message anywhere but second — cannot
/// claim support. It is not recorded in a session, so moving it costs no session anything.
pub const LAYOUT_VERSION: u32 = 3;

/// Encoded bytes of one inspect result.
pub const INSPECT_BYTES_MAX: usize = 8 * 1024;

/// Fragment descriptors on one inspect page.
pub const INSPECT_PAGE_MAX: usize = 40;

/// Characters of a fragment descriptor's summary.
pub const DESCRIPTOR_SUMMARY_CHARS_MAX: usize = 256;

/// Encoded bytes of one recall result.
pub const RECALL_BYTES_MAX: usize = 8 * 1024;

/// Hits a search returns when no limit is given.
pub const RECALL_HITS_DEFAULT: usize = 20;

/// Hits a search may return.
pub const RECALL_HITS_MAX: usize = 40;

/// Characters of one search hit's excerpt.
pub const RECALL_EXCERPT_CHARS_MAX: usize = 1_024;

/// Bytes one recall reads at a time.
pub const RECALL_CHUNK_BYTES: usize = 64 * 1024;

/// Source bytes one recall call may examine.
pub const RECALL_WORK_BYTES: usize = 256 * 1024;

/// Characters of a recall query.
pub const RECALL_QUERY_CHARS_MAX: usize = 256;

/// Characters of an opaque cursor.
pub const CURSOR_CHARS_MAX: usize = 512;

/// Bytes one captured stream may retain.
pub const CAPTURE_STREAM_BYTES_MAX: u64 = 8 * 1024 * 1024;

/// Archived bytes one session may hold.
pub const CAPTURE_SESSION_BYTES_MAX: u64 = 128 * 1024 * 1024;

/// Archived bytes one store may hold, live reservations and orphans included.
pub const CAPTURE_STORE_BYTES_MAX: u64 = 1024 * 1024 * 1024;

/// Staged capture buffers per call, across both streams.
pub const CAPTURE_STAGING_BYTES_MAX: usize = 128 * 1024;

/// Milliseconds a sink write or finalize may take.
pub const CAPTURE_DEADLINE_MS: u64 = 5_000;

/// Bytes of one artifact integrity chunk.
pub const ARTIFACT_CHUNK_BYTES: usize = 64 * 1024;

/// [`ARTIFACT_CHUNK_BYTES`] as the width receipts count in.
pub const ARTIFACT_CHUNK_BYTES_U64: u64 = 64 * 1024;

/// Chunk digests one receipt may carry: 8 MiB in 64 KiB chunks.
pub const ARTIFACT_CHUNKS_MAX: usize = 128;

/// Encoded bytes of one fixed-schema failure diagnostic.
pub const DIAGNOSTIC_BYTES_MAX: usize = 1_024;

/// Characters of a decision or attempt id.
pub const DECISION_ID_MAX: usize = 96;

/// Bytes the host keeps free for the records that close a step and a turn.
///
/// Two attempt records, a decision, a recovery record, a revision and the step and turn ends,
/// with room for their framing. Reserved before a request so an honest ending is never the
/// record that does not fit.
pub const CLOSING_RESERVE_BYTES: usize = 96 * 1024;

/// Encoded bytes of one handoff document: the deterministic header plus the model narrative.
pub const HANDOFF_DOCUMENT_BYTES_MAX: usize = 32 * 1024;

/// Encoded bytes of a handoff's model-written narrative alone.
pub const HANDOFF_NARRATIVE_BYTES_MAX: usize = 16 * 1024;

/// Bytes of an id the harness stores as a reference: a session id, a model id, a handoff's new
/// session id. Host-generated or model-named, and bounded either way so a record stays small.
pub const REFERENCE_ID_BYTES_MAX: usize = 256;

/// Drafts a connection may hold of each kind: one handoff draft, one compaction draft.
///
/// Drafts are the server's, keyed by connection and dropped on detach, replacement or commit,
/// so a client cannot pile up model calls it never previews.
pub const HANDOFF_DRAFTS_PER_CONNECTION: usize = 1;

/// See [`HANDOFF_DRAFTS_PER_CONNECTION`].
pub const COMPACT_DRAFTS_PER_CONNECTION: usize = 1;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_limits_agree_with_each_other() {
        const {
            assert!(REARM_PERCENT < SOFT_PRESSURE_PERCENT);
            assert!(TARGET_PERCENT <= REARM_PERCENT);
            assert!(DEFAULT_WORKING_TARGET_TOKENS > 0);
            assert!(NOTES_BYTES_MAX < REVISION_RECORD_BYTES_MAX);
            assert!(REVISION_RECORD_BYTES_MAX < CLOSING_RESERVE_BYTES);
            assert!(RECALL_CHUNK_BYTES <= RECALL_WORK_BYTES);
        }
        let chunks = CAPTURE_STREAM_BYTES_MAX.div_ceil(ARTIFACT_CHUNK_BYTES_U64);
        assert_eq!(usize::try_from(chunks).ok(), Some(ARTIFACT_CHUNKS_MAX));
        assert_eq!(
            u64::try_from(ARTIFACT_CHUNK_BYTES).ok(),
            Some(ARTIFACT_CHUNK_BYTES_U64)
        );
    }
}
