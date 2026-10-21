//! Compacting a legacy session: the marker that cuts its prompt, and the summary it carries.
//!
//! A legacy session has no recall tool, so a compaction is a one-way cut the human chooses. A
//! model writes a summary of the conversation up to a turn boundary, and a `context/compact`
//! record appended there makes legacy prompt assembly fold only what follows it, with the summary
//! leading. Nothing is deleted: the raw log stays the authority, and a legacy model simply cannot
//! read the cut part back. That is why the record is legacy, not managed. Managed context already
//! shrinks its own prompt, and a second cut there would need an exception to its protected floor.
//!
//! The positional invariants — the marker's boundary is its own sequence, its prefix digest is the
//! log's, it sits at a turn boundary with every call answered, and `supersedes` names the marker
//! before it — need the log around the record, so they are checked by the session that holds it.
//! What this module owns is the record's own shape and the text its summary must pass.

use serde::{Deserialize, Serialize};

use super::{COMPACTION_SUMMARY_BYTES_MAX, model_text_is_admissible};
use crate::context::managed::ids::{Digest, ErrorCode};
use crate::context::managed::limits::REFERENCE_ID_BYTES_MAX;
use crate::message::Usage;

/// How a compaction summary was produced.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionMethod {
    /// A maintenance request wrote the summary under the fixed instruction set.
    #[default]
    ModelGenerated,
}

/// A `context/compact` record: the active marker of a compaction cut.
///
/// The marker is itself the boundary: events with sequence below `boundary_event_count` are
/// outside the prompt from now on, while the raw log keeps them. A session has at most one active
/// marker, the newest; an older marker lies before the boundary of the newer one, and its summary
/// was given to the maintenance request that wrote the newer one, so the newest summary carries it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactionRecord {
    /// Exclusive event count of the marker itself. Must equal the record's own sequence.
    pub boundary_event_count: u64,
    /// BLAKE3 of the stored header and the first `boundary_event_count` event lines, byte for
    /// byte, with the header a body carries once a marker exists.
    pub prefix_blake3: Digest,
    /// The summary text, inline and bounded. It is rendered as a system message, so it passes
    /// the [text rule](crate::context::model_text_is_admissible) before it is stored.
    pub summary: String,
    /// BLAKE3 of `summary`.
    pub summary_blake3: Digest,
    /// The sequence of the previous marker, when one exists.
    pub supersedes: Option<u64>,
    /// How the summary was produced.
    pub method: CompactionMethod,
    /// The model id that wrote the summary.
    pub model: String,
    /// What the maintenance request cost, as the provider reported it. Summed into the session's
    /// usage, so a session's reported spend includes its compactions.
    #[serde(default)]
    pub usage: Option<Usage>,
}

impl CompactionRecord {
    /// Checks the invariants the record states about itself.
    ///
    /// The positional invariants need the log and are checked by the session. What remains is
    /// self-contained: the summary passes the text rule within its bound, its digest is truthful,
    /// and the model id is a bounded, plain reference.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::InvalidCompact`] for the first broken invariant.
    pub fn validate(&self) -> Result<(), ErrorCode> {
        if !model_text_is_admissible(&self.summary, COMPACTION_SUMMARY_BYTES_MAX)
            || Digest::of(self.summary.as_bytes()) != self.summary_blake3
        {
            return Err(ErrorCode::InvalidCompact);
        }
        if self.model.is_empty()
            || self.model.len() > REFERENCE_ID_BYTES_MAX
            || self.model.chars().any(char::is_control)
        {
            return Err(ErrorCode::InvalidCompact);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(summary: &str) -> CompactionRecord {
        CompactionRecord {
            boundary_event_count: 4,
            prefix_blake3: Digest::of(b"prefix"),
            summary: summary.to_owned(),
            summary_blake3: Digest::of(summary.as_bytes()),
            supersedes: None,
            method: CompactionMethod::ModelGenerated,
            model: "deepseek-flash".to_owned(),
            usage: None,
        }
    }

    #[test]
    fn a_summary_with_a_carriage_return_is_refused_as_an_invalid_compact() {
        assert_eq!(
            record("first line\r\nsecond").validate(),
            Err(ErrorCode::InvalidCompact)
        );
    }

    #[test]
    fn a_summary_that_opens_a_harness_label_line_is_refused() {
        assert_eq!(
            record("decisions\n  [NANUS working data]").validate(),
            Err(ErrorCode::InvalidCompact)
        );
    }

    #[test]
    fn a_summary_whose_digest_is_not_its_text_is_refused() {
        let mut forged = record("the user chose sqlite");
        forged.summary = "the user chose postgres".to_owned();
        assert_eq!(forged.validate(), Err(ErrorCode::InvalidCompact));
    }

    #[test]
    fn a_multi_line_markdown_summary_with_a_link_is_accepted() {
        assert_eq!(
            record("## Decisions\n- use sqlite\n[the plan](docs/plan.md)").validate(),
            Ok(())
        );
    }

    #[test]
    fn a_model_id_with_a_control_character_is_refused() {
        let mut bad = record("ok");
        bad.model = "deepseek\u{7}".to_owned();
        assert_eq!(bad.validate(), Err(ErrorCode::InvalidCompact));
    }
}
