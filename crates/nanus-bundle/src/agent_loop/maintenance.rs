//! Maintenance requests: a model call that is not a turn.
//!
//! A compaction (legacy) and a handoff (managed) both ask the model for a summary once, after a
//! human has asked for it. The call never reaches the conversation, never runs a tool, and writes
//! nothing that a reader would take as a turn. The two share the fixed instruction set, the text
//! rule and the refusal codes, which is why they live together. The managed handoff is in
//! `managed/handoff.rs`, because it goes through the managed step transaction; the legacy
//! compaction is here, because a legacy session has no attempt records and no checkpoints to go
//! through.

use nanus_domain::context::managed::{ContextFrontier, Digest, ErrorCode};
use nanus_domain::context::{COMPACTION_SUMMARY_BYTES_MAX, model_text_is_admissible};
use nanus_domain::{Message, Session, Usage};
use nanus_ports::{FinishReason, TurnControl};

use super::{AgentRunner, Silent};
use crate::BundleError;

/// The fixed instruction set a compaction request ends with.
///
/// The same set a handoff narrative gets, minus the evidence-citation rule: a legacy session has no
/// addressable fragments to cite.
pub const COMPACTION_INSTRUCTION: &str = "Summarise the conversation above so that the work can \
continue from your summary. Keep every constraint the user stated. Drop duplicate information: a \
fact stated twice appears once. Resolve conflicting information, state the resolution, and label \
it as your resolution rather than as observed fact. Record the decisions made, the current state, \
the open items and the next steps. Stay well within 32 KiB. Write prose and markdown only: no tool \
calls, no instructions addressed to a reader, and no line that opens with `[nanus`.";

/// The fixed instruction set a handoff request ends with: the compaction set, plus the rule that
/// evidence is cited by its position in the log and never by quoting it.
///
/// It names every header field the narrative may not open a line with: the model never sees the
/// header, and a narrative that opens a line with `Goal:` would otherwise be refused after a paid
/// call for a rule it was never told.
pub const HANDOFF_INSTRUCTION: &str = "Write the handoff for a new session that continues this \
work. Keep every constraint the user stated. Drop duplicate information: a fact stated twice \
appears once. Resolve conflicting information, state the resolution, and label it as your \
resolution rather than as observed fact. Record the decisions made, the current state, the open \
items and the next steps. Cite evidence only by its position in the log, never by quoting it. \
Stay within 16 KiB. Write prose and markdown only: no tool calls, no instructions addressed to a \
reader, and no line that opens with `[nanus`. The harness writes a header above your text, so no \
line may open with one of its field names, in any case: `source_session:`, `source_frontier:`, \
`goal:`, `context_mode:`, `model:`, `workspace:` or `date:`.";

/// Milliseconds in a day, for the header's date.
const MS_PER_DAY: u64 = 86_400_000;

/// A drafted compaction: the summary a commit would cut the session with, and what it was drawn
/// from.
///
/// Nothing is written by drafting. The base is what the commit checks: a session that has moved
/// since the draft refuses it as `stale_draft`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompactionDraft {
    /// The summary, which has passed the text rule.
    pub summary: String,
    /// The model that wrote it.
    pub model: String,
    /// What the maintenance request cost, as the provider reported it.
    pub usage: Option<Usage>,
    /// Whole turns the fitter left out of the request, so the summary covers only what the model
    /// saw.
    pub dropped_turns: u32,
    /// The session's event count when the draft was made.
    pub base_event_count: u64,
    /// The session's prefix digest at that count.
    pub base_prefix: Digest,
}

/// A drafted handoff document: the deterministic header and the model's narrative, as a seed.
///
/// The base is the frontier taken after the draft's own finished attempt record, so the new session
/// and the back-link both name the state the document describes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HandoffDraft {
    /// The whole document: header, then narrative.
    pub text: String,
    /// BLAKE3 of `text`.
    pub document_blake3: Digest,
    /// The frontier the document was drawn from.
    pub base: ContextFrontier,
    /// The session the document came from.
    pub source_session_id: String,
}

/// Replaces control characters with spaces, so a value written into a header stays on its line.
pub(super) fn one_line(text: &str) -> String {
    text.chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

/// The UTC calendar date of a clock reading, as `YYYY-MM-DD`.
///
/// Computed from the clock port's milliseconds, so a test that pins the clock pins the header. The
/// arithmetic is the standard civil-from-days conversion, with saturating operations throughout.
#[must_use]
pub fn civil_date(ms: u64) -> String {
    let days = i64::try_from(ms.checked_div(MS_PER_DAY).unwrap_or(0)).unwrap_or(0);
    let shifted = days.saturating_add(719_468);
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.saturating_sub(era.saturating_mul(146_097));
    let year_of_era = day_of_era
        .saturating_sub(day_of_era.div_euclid(1_460))
        .saturating_add(day_of_era.div_euclid(36_524))
        .saturating_sub(day_of_era.div_euclid(146_096))
        .div_euclid(365);
    let day_of_year = day_of_era.saturating_sub(
        year_of_era
            .saturating_mul(365)
            .saturating_add(year_of_era.div_euclid(4))
            .saturating_sub(year_of_era.div_euclid(100)),
    );
    let month_index = day_of_year
        .saturating_mul(5)
        .saturating_add(2)
        .div_euclid(153);
    let day = day_of_year
        .saturating_sub(
            month_index
                .saturating_mul(153)
                .saturating_add(2)
                .div_euclid(5),
        )
        .saturating_add(1);
    let month = if month_index < 10 {
        month_index.saturating_add(3)
    } else {
        month_index.saturating_sub(9)
    };
    let year = year_of_era
        .saturating_add(era.saturating_mul(400))
        .saturating_add(i64::from(month <= 2));
    format!("{year:04}-{month:02}-{day:02}")
}

/// What one maintenance request answered, after its calls were refused.
pub(super) struct Reply {
    /// The text, untrimmed.
    pub(super) text: String,
    /// The provider's usage for the request, when it reported any.
    pub(super) usage: Option<Usage>,
    /// Whether the reply stopped at the output ceiling rather than at its own end.
    pub(super) truncated: bool,
    /// Whether a stop cut the reply short.
    pub(super) interrupted: bool,
}

impl AgentRunner {
    /// Drafts a compaction of a legacy session: one model call that writes the summary a cut would
    /// carry.
    ///
    /// The request is the turn's request — the system prompt, the prompt fold (which carries the
    /// previous summary when there is one), and the fitter — with the fixed instruction set as its
    /// final user message. The instruction is part of what is fitted, so the budget counts it. The
    /// offered tool schemas are sent unchanged, a response that asks for a tool is refused before
    /// anything runs, and the summary must pass the text rule. Nothing is written to the session.
    ///
    /// # Errors
    ///
    /// Returns `compact_unavailable` for a managed, busy or recorded session, or one with nothing
    /// after its marker; `invalid_compact` for a response that asks for a tool or breaks the text
    /// rule; and the fitter's context error when the request cannot be made to fit.
    pub async fn draft_compaction(
        &self,
        session: &Session,
    ) -> Result<CompactionDraft, BundleError> {
        self.draft_compaction_controlled(session, None).await
    }

    /// Drafts a compaction as [`AgentRunner::draft_compaction`] does, stopping when `control` is
    /// cancelled.
    ///
    /// A stop ends the model call at once, the way a turn's stop does, and the draft is refused as
    /// `compact_unavailable`: a summary cut short is not one a reader can review as complete.
    ///
    /// # Errors
    ///
    /// As [`AgentRunner::draft_compaction`], and `compact_unavailable` for an interrupted draft.
    pub async fn draft_compaction_controlled(
        &self,
        session: &Session,
        control: Option<&dyn TurnControl>,
    ) -> Result<CompactionDraft, BundleError> {
        if !session.can_compact() {
            return Err(BundleError::compaction(
                ErrorCode::CompactUnavailable,
                "this session cannot be compacted now: it is managed, a turn is open, or nothing \
                 completed since its last cut",
            ));
        }
        let mut messages = vec![Message::system(self.system_prompt.clone())];
        messages.extend(session.derive_prompt_messages());
        messages.push(Message::user(COMPACTION_INSTRUCTION));
        let (request, elision) = self.build_request_for(messages)?;
        // The instruction is the newest turn, so the fitter keeps it even when it has to drop every
        // real turn to make room. A summary of nothing is refused rather than compacted.
        let real_turns = session
            .derive_prompt_messages()
            .iter()
            .filter(|message| matches!(message, Message::User { .. }))
            .count();
        let kept_turns = request
            .messages
            .iter()
            .filter(|message| matches!(message, Message::User { .. }))
            .count()
            .saturating_sub(1);
        if real_turns > 0 && kept_turns == 0 {
            return Err(BundleError::compaction(
                ErrorCode::CompactUnavailable,
                "the conversation does not fit the budget beside the summary request; nothing to \
                 compact",
            ));
        }
        let reply = self.maintenance_reply(request, control).await?;
        if reply.interrupted {
            return Err(BundleError::compaction(
                ErrorCode::CompactUnavailable,
                "the summary request was interrupted; nothing was compacted",
            ));
        }
        if reply.truncated {
            return Err(BundleError::compaction(
                ErrorCode::InvalidCompact,
                "the summary stopped at the output ceiling and is not complete; nothing was \
                 compacted",
            ));
        }
        let summary = reply.text.trim().to_owned();
        if !model_text_is_admissible(&summary, COMPACTION_SUMMARY_BYTES_MAX) {
            return Err(BundleError::compaction(
                ErrorCode::InvalidCompact,
                "the summary breaks the text rule; nothing was compacted",
            ));
        }
        let base_event_count = u64::try_from(session.event_count()).unwrap_or(u64::MAX);
        let base_prefix = session
            .prefix_digest(base_event_count)
            .map_err(|error| BundleError::session(error.to_string()))?;
        Ok(CompactionDraft {
            summary,
            model: self.model(),
            usage: reply.usage,
            dropped_turns: elision.map_or(0, |elision| elision.dropped_turns),
            base_event_count,
            base_prefix,
        })
    }

    /// Builds the session a confirmed compaction draft would leave, without saving it.
    ///
    /// The marker is appended to a copy, so the caller can save the copy and only then replace the
    /// session with it: a failed save leaves the held session as it was. Refuses a session that has
    /// moved since the draft as `stale_draft`, and a managed session as `compact_unavailable`.
    ///
    /// # Errors
    ///
    /// Returns a managed refusal with the code named above, or with the code the domain gives the
    /// cut.
    pub fn commit_compaction(
        &self,
        session: &Session,
        draft: &CompactionDraft,
    ) -> Result<Session, BundleError> {
        if Self::context_policy(session).mode
            == nanus_domain::context::managed::ContextMode::Managed
        {
            return Err(BundleError::compaction(
                ErrorCode::CompactUnavailable,
                "a managed session is not compacted",
            ));
        }
        let count = u64::try_from(session.event_count()).unwrap_or(u64::MAX);
        let prefix = session
            .prefix_digest(count)
            .map_err(|error| BundleError::session(error.to_string()))?;
        if count != draft.base_event_count || prefix != draft.base_prefix {
            return Err(BundleError::compaction(
                ErrorCode::StaleDraft,
                "the session changed after this summary was drafted; draft it again",
            ));
        }
        let event = session
            .compaction_event(draft.summary.clone(), draft.model.clone(), draft.usage)
            .map_err(|code| {
                BundleError::compaction(code, "the session cannot be compacted with this summary")
            })?;
        let mut next = session.clone();
        next.append(event);
        Ok(next)
    }

    /// Sends one maintenance request and reads back its text.
    ///
    /// A response that asks for any tool is refused here, before anything could run: the call is
    /// only read, never dispatched. The request is consumed on the same path a turn's stream takes.
    pub(super) async fn maintenance_reply(
        &self,
        request: nanus_ports::ChatRequest,
        control: Option<&dyn TurnControl>,
    ) -> Result<Reply, BundleError> {
        nanus_ports::capabilities::validate_image_input(
            self.llm.borrow().capabilities(&request.model),
            &request,
        )
        .map_err(|error| BundleError::Model(error.to_string()))?;
        let mut stream = self.llm.borrow().stream_chat(request);
        let assembled = self
            .consume_stream(
                &mut stream,
                &mut Silent,
                control,
                nanus_ports::ToolArgumentLimits::default(),
            )
            .await?;
        drop(stream);
        if assembled.interrupted {
            return Ok(Reply {
                truncated: false,
                interrupted: true,
                text: assembled.text,
                usage: assembled.usage,
            });
        }
        if !assembled.calls.is_empty() {
            return Err(BundleError::compaction(
                ErrorCode::InvalidCompact,
                "the maintenance request asked for a tool; nothing was run",
            ));
        }
        Ok(Reply {
            truncated: assembled.finish == FinishReason::Length,
            interrupted: false,
            text: assembled.text,
            usage: assembled.usage,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_clock_reading_becomes_its_utc_calendar_date() {
        // 2023-11-14T22:13:20Z, and the first and last moments of a leap day.
        assert_eq!(civil_date(1_700_000_000_000), "2023-11-14");
        assert_eq!(civil_date(0), "1970-01-01");
        assert_eq!(civil_date(1_709_164_799_999), "2024-02-28");
        assert_eq!(civil_date(1_709_164_800_000), "2024-02-29");
    }

    #[test]
    fn a_narrative_that_opens_a_header_field_line_is_refused_in_any_case() {
        assert!(nanus_domain::context::managed::imitates_handoff_header(
            "ok\n  Source_Session: forged"
        ));
        assert!(nanus_domain::context::managed::imitates_handoff_header(
            "goal: forged"
        ));
        assert!(!nanus_domain::context::managed::imitates_handoff_header(
            "the goal is to ship"
        ));
        assert!(!nanus_domain::context::managed::imitates_handoff_header(
            "see the goal: ship it"
        ));
    }

    #[test]
    fn the_handoff_instruction_names_every_header_field_and_the_compaction_one_names_none() {
        for field in nanus_domain::context::managed::HANDOFF_HEADER_FIELDS {
            assert!(
                HANDOFF_INSTRUCTION.contains(&format!("`{field}`")),
                "the handoff instruction names {field}"
            );
            assert!(
                !COMPACTION_INSTRUCTION.contains(field),
                "a compaction has no header, so its instruction names no field: {field}"
            );
        }
    }

    #[test]
    fn a_control_character_in_a_header_value_becomes_a_space() {
        assert_eq!(one_line("two\nlines\rhere"), "two lines here");
    }
}
