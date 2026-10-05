//! The fixed limits of context policy version 1.
//!
//! Initial engineering defaults rather than measured optima. They are constants rather than
//! configuration because every one of them is part of what `policy_version` promises: a future
//! tunable limit keeps an absolute bound and becomes part of the snapshot profile, so a proposal
//! made under one value cannot be accepted under another.

/// The only policy version this build reads and writes.
pub const POLICY_VERSION: u32 = 1;

/// The version of the generated-memory and notice rendering, bound into the notes digest.
pub const RENDERER_VERSION: u32 = 1;

/// The output reservation a policy starts with.
pub const DEFAULT_OUTPUT_RESERVE_TOKENS: u32 = 4_096;

/// Percent of the admissible input allowance at which a budget hint is given.
pub const SOFT_PRESSURE_PERCENT: u32 = 75;

/// Percent below which the budget hint re-arms.
pub const REARM_PERCENT: u32 = 60;

/// Completed model steps between two budget hints.
pub const REMINDER_MIN_STEPS: u32 = 4;

/// Percent of the allowance automatic reduction aims for once it has to act.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_limits_agree_with_each_other() {
        const {
            assert!(REARM_PERCENT < SOFT_PRESSURE_PERCENT);
            assert!(TARGET_PERCENT <= REARM_PERCENT);
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
