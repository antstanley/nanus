//! The managed handoff: the maintenance request that drafts a new session's seed, and the three
//! steps the link orders to commit it.
//!
//! A draft goes through the managed step transaction like a turn's request does: the frozen call is
//! checkpointed as an intent before anything is sent, and its outcome is a finished attempt record,
//! both with `kind: handoff`. Those are the only records a draft writes. The commit touches two
//! sessions and the store has no transaction across them, so it is split into steps the link runs
//! in order:
//!
//! 1. [`AgentRunner::check_handoff_source`]: the source still holds the draft's base.
//! 2. [`AgentRunner::seed_session`] then [`AgentRunner::checkpoint_seeded`]: the new session, with
//!    its opening `context/mode` and `context/seed`, created and saved first. If this fails nothing
//!    exists.
//! 3. [`AgentRunner::record_handoff`]: the back-link in the source, checkpointed through the
//!    source's own checkpoint. If this fails the new session is complete and its seed names where
//!    it came from.

use nanus_domain::context::managed::{
    AttemptOutcome, AttemptPhase, ContextFrontier, ContextMode, ContextModeRecord, Digest,
    ErrorCode, HandoffRecord, HandoffSeed, MaintenanceAttemptKind, ManagedState, ModeActor,
    ModeReason, RequestAttemptRecord, UsageObservation, handoff_narrative_is_admissible, limits,
    split_handoff_document, state,
};
use nanus_domain::context::model_text_is_admissible;
use nanus_domain::{Session, SessionEvent, SessionId};
use nanus_ports::{CheckpointReason, PersistenceState, TurnControl, TurnRuntime};

use super::super::maintenance::{HANDOFF_INSTRUCTION, HandoffDraft, civil_date, one_line};
use super::super::{AgentRunner, Assembled, Silent};
use super::prepare::PreparedMaintenance;
use super::{ManagedTurn, refusal};
use crate::BundleError;

/// The whole document's bound, from the managed limits.
const DOCUMENT_BYTES_MAX: usize = limits::HANDOFF_DOCUMENT_BYTES_MAX;

/// The refusal a handoff gives when the session cannot be handed off from.
fn unavailable(message: &'static str) -> BundleError {
    BundleError::managed(ErrorCode::HandoffUnavailable, message)
}

/// The narrative the response carries, or the reason the response is refused.
///
/// A response that asks for a tool is refused, and so is one whose text breaks the text rule or
/// imitates a header field: the narrative is refused, never repaired.
fn narrative_of(reply: &Assembled) -> Result<String, BundleError> {
    if reply.finish == nanus_ports::FinishReason::Length {
        return Err(unavailable(
            "the handoff stopped at the output ceiling and is not complete",
        ));
    }
    if !reply.calls.is_empty() {
        return Err(unavailable(
            "the handoff request asked for a tool; nothing was run",
        ));
    }
    let narrative = reply.text.trim().to_owned();
    if !handoff_narrative_is_admissible(&narrative) {
        return Err(unavailable(
            "the handoff narrative breaks the text rule or imitates the header",
        ));
    }
    Ok(narrative)
}

impl AgentRunner {
    /// Drafts a handoff from a managed session: one model call that writes the narrative of the
    /// seed.
    ///
    /// The session must be managed and idle, and the request is the selection it already holds,
    /// with the fixed instruction set as its final message. The intent is checkpointed before
    /// anything is sent, and the outcome is a finished attempt record. A response that asks for a
    /// tool, breaks the text rule or imitates a header field is recorded as `refused` and refused;
    /// so is a document over its bound. The base frontier is taken after the finished record, so
    /// the draft names the state it describes.
    ///
    /// # Errors
    ///
    /// Returns `handoff_unavailable` for a legacy, busy or recorded session, or for a response
    /// refused as above; `candidate_too_large` when the held selection does not fit the request;
    /// and a managed refusal when the runtime cannot record the attempt.
    pub async fn draft_handoff(
        &self,
        session: &mut Session,
        runtime: TurnRuntime<'_>,
    ) -> Result<HandoffDraft, BundleError> {
        self.draft_handoff_controlled(session, runtime, None).await
    }

    /// Drafts a handoff as [`AgentRunner::draft_handoff`] does, stopping when `control` is
    /// cancelled.
    ///
    /// Whatever a crash left open is closed first, as before a turn, so an earlier draft's intent
    /// with no outcome is settled on its own rather than swept up later. A stop ends the model call
    /// at once; the attempt is recorded `cancelled` and the draft is refused as
    /// `handoff_unavailable`, because a narrative cut short is not one a reader can review.
    ///
    /// # Errors
    ///
    /// As [`AgentRunner::draft_handoff`], and `handoff_unavailable` for an interrupted draft.
    pub async fn draft_handoff_controlled(
        &self,
        session: &mut Session,
        runtime: TurnRuntime<'_>,
        control: Option<&dyn TurnControl>,
    ) -> Result<HandoffDraft, BundleError> {
        let policy = Self::context_policy(session);
        if !session.is_managed_body() || policy.mode != ContextMode::Managed {
            return Err(unavailable(
                "a handoff needs an idle session whose context is managed",
            ));
        }
        let checkpoint = runtime
            .checkpoint
            .ok_or_else(|| refusal(ErrorCode::UnsupportedMode))?;
        // Recovery comes before the idle check: a turn a crash left open is closed here, as it is
        // before a turn, rather than refusing the session for a turn nobody is running.
        self.recover(session, runtime).await?;
        if session.turn_open() {
            return Err(unavailable(
                "a handoff needs an idle session whose context is managed",
            ));
        }
        let durable = u64::try_from(session.event_count()).unwrap_or(u64::MAX);
        let turn = ManagedTurn::new(checkpoint, runtime.context, policy, durable);
        self.check_managed_ready(session, &turn)?;
        let _hold = self.hold_selection(true)?;
        Self::check_capacity(session, limits::CLOSING_RESERVE_BYTES)?;
        let PreparedMaintenance {
            call,
            revision,
            management,
        } = self.prepare_maintenance(session, &turn, HANDOFF_INSTRUCTION)?;
        let mut attempt = self.started_attempt(
            session,
            (session.log().current_turn(), 0),
            (&*call, revision, management),
        );
        attempt.kind = Some(MaintenanceAttemptKind::Handoff);
        self.record_intent(session, &turn, None, &attempt, &mut Silent)
            .await?;
        let mut stream = call.stream();
        let assembled = self
            .consume_stream(
                &mut stream,
                &mut Silent,
                control,
                nanus_ports::ToolArgumentLimits::managed(),
            )
            .await;
        drop(stream);
        let (outcome, usage, narrative) = self.judge_handoff(session, assembled);
        self.finish_maintenance(session, &turn, &attempt, outcome, usage)
            .await?;
        let narrative = narrative?;
        let revision = ManagedTurn::revision(session);
        let base = state::frontier(session, revision).map_err(refusal)?;
        self.handoff_draft(session, base, &narrative)
    }

    /// What a handoff response is recorded as, what it cost, and the narrative or its refusal.
    ///
    /// The whole document is checked here, before the attempt is finished, so a narrative whose
    /// document would exceed its bound is recorded `refused` as every other refused narrative is,
    /// not `completed` and then refused. The header is built against the frontier the finished
    /// record will leave: the count one past the current one, and a digest of the same fixed width,
    /// so the provisional document is exactly as long as the real one.
    fn judge_handoff(
        &self,
        session: &Session,
        assembled: Result<Assembled, BundleError>,
    ) -> (
        AttemptOutcome,
        Option<nanus_domain::Usage>,
        Result<String, BundleError>,
    ) {
        let reply = match assembled {
            Ok(reply) => reply,
            Err(error) => return (AttemptOutcome::Failed, None, Err(error)),
        };
        let usage = reply.usage;
        if reply.interrupted {
            let stopped = unavailable("the handoff request was interrupted; nothing was drafted");
            return (AttemptOutcome::Cancelled, usage, Err(stopped));
        }
        let provisional = ContextFrontier {
            session_id: session.id().as_str().to_owned(),
            event_count: u64::try_from(session.event_count())
                .unwrap_or(u64::MAX)
                .saturating_add(1),
            prefix_blake3: Digest::empty(),
            projection_revision: ManagedTurn::revision(session),
        };
        let judged = narrative_of(&reply).and_then(|narrative| {
            self.handoff_draft(session, provisional, &narrative)
                .map(|_| narrative)
        });
        match judged {
            Ok(narrative) => (AttemptOutcome::Completed, usage, Ok(narrative)),
            Err(error) => (AttemptOutcome::Refused, usage, Err(error)),
        }
    }

    /// Records the finished attempt of a maintenance request and checkpoints it.
    async fn finish_maintenance(
        &self,
        session: &mut Session,
        turn: &ManagedTurn<'_>,
        attempt: &RequestAttemptRecord,
        outcome: AttemptOutcome,
        usage: Option<nanus_domain::Usage>,
    ) -> Result<(), BundleError> {
        let mut finished = attempt.clone();
        finished.phase = AttemptPhase::Finished;
        finished.outcome = Some(outcome);
        finished.usage = usage.as_ref().map(UsageObservation::from_usage);
        finished.finished_at_ms = Some(self.clock.now_ms());
        let mut candidate = session.clone();
        candidate.append(SessionEvent::RequestAttempt {
            payload: Box::new(finished),
        });
        self.commit(turn, &candidate, CheckpointReason::SettledStep, &mut Silent)
            .await?;
        *session = candidate;
        Ok(())
    }

    /// Assembles the document: the deterministic header, then the narrative, checked as one text.
    fn handoff_draft(
        &self,
        session: &Session,
        base: ContextFrontier,
        narrative: &str,
    ) -> Result<HandoffDraft, BundleError> {
        let text = format!("{}\n\n{narrative}", self.handoff_header(session, &base));
        // The document is checked as the seed will be on every read: the header, the narrative
        // within its own bound, and the whole within the document's.
        let whole_ok = model_text_is_admissible(&text, DOCUMENT_BYTES_MAX)
            && split_handoff_document(&text)
                .is_some_and(|(_, narrative)| handoff_narrative_is_admissible(narrative));
        if !whole_ok {
            return Err(unavailable(
                "the handoff document exceeds its bound or breaks the text rule",
            ));
        }
        Ok(HandoffDraft {
            document_blake3: Digest::of(text.as_bytes()),
            text,
            base,
            source_session_id: session.id().as_str().to_owned(),
        })
    }

    /// The deterministic header: facts the log already holds, never model text.
    ///
    /// The field lines are fixed names, so a narrative that imitates one is refused. Every value is
    /// a single line.
    fn handoff_header(&self, session: &Session, base: &ContextFrontier) -> String {
        let goal = session.goal().map_or_else(
            || String::from("none"),
            |goal| {
                format!(
                    "{} (phase {}, revision {})",
                    one_line(goal.objective()),
                    goal.phase().as_str(),
                    goal.revision()
                )
            },
        );
        let policy = Self::context_policy(session);
        let effort = self.effort().map_or("none", |effort| effort.as_str());
        format!(
            "source_session: {}\n\
             source_frontier: events={} prefix_blake3={} projection_revision={}\n\
             goal: {goal}\n\
             context_mode: {} output_reserve={} capture_shell={}\n\
             model: {} effort: {effort}\n\
             workspace: {}\n\
             date: {}",
            one_line(session.id().as_str()),
            base.event_count,
            base.prefix_blake3.as_str(),
            base.projection_revision,
            policy.mode.as_str(),
            policy.output_reserve_tokens,
            policy.capture_shell,
            one_line(&self.model()),
            one_line(session.cwd()),
            civil_date(self.clock.now_ms()),
        )
    }

    /// Refuses a handoff whose source has moved since the draft: `stale_draft`.
    ///
    /// Checked before any record is made: the source must be idle, still managed, hold exactly the
    /// draft's base, and that base must still be a prefix of its log.
    ///
    /// # Errors
    ///
    /// Returns `handoff_unavailable` for a busy or legacy source, and `stale_draft` when it moved.
    pub fn check_handoff_source(source: &Session, draft: &HandoffDraft) -> Result<(), BundleError> {
        if source.turn_open() || Self::context_policy(source).mode != ContextMode::Managed {
            return Err(unavailable(
                "the source session is busy or no longer managed",
            ));
        }
        let count = u64::try_from(source.event_count()).unwrap_or(u64::MAX);
        if count != draft.base.event_count || state::check_frontier(source, &draft.base).is_err() {
            return Err(BundleError::managed(
                ErrorCode::StaleDraft,
                "the session changed after this handoff was drafted; draft it again",
            ));
        }
        Ok(())
    }

    /// The new session's opening records, appended to a fresh empty session: the source's mode and
    /// policy, then the seed.
    ///
    /// `fresh` is what the host created (its origin and identity). The policy is copied whole,
    /// output reserve and shell-evidence capture included, so a handoff can neither turn capture on
    /// nor off. Checked as a managed fold before it is returned, so a seed that could not be read
    /// back is never handed to a checkpoint.
    ///
    /// # Errors
    ///
    /// Returns `handoff_unavailable` for a non-empty session, a source that is not managed, or a
    /// draft from another session.
    pub fn seed_session(
        &self,
        mut fresh: Session,
        source: &Session,
        draft: &HandoffDraft,
    ) -> Result<Session, BundleError> {
        let policy = Self::context_policy(source);
        if fresh.event_count() != 0
            || policy.mode != ContextMode::Managed
            || draft.source_session_id != source.id().as_str()
        {
            return Err(unavailable(
                "a seed needs an empty session and a managed source it was drafted from",
            ));
        }
        fresh.upgrade_to_managed_body();
        fresh.append(SessionEvent::ContextMode {
            payload: Box::new(ContextModeRecord {
                policy,
                actor: ModeActor::Human,
                reason: ModeReason::Enable,
                previous_revision: 0,
            }),
        });
        fresh.append(SessionEvent::ContextSeed {
            payload: Box::new(HandoffSeed {
                text: draft.text.clone(),
                document_blake3: draft.document_blake3.clone(),
                source_session_id: draft.source_session_id.clone(),
                source_frontier: draft.base.clone(),
            }),
        });
        ManagedState::fold(fresh.log())
            .map_err(|code| BundleError::managed(code, "the seeded session does not validate"))?;
        Ok(fresh)
    }

    /// Checkpoints a seeded session through `runtime`'s checkpoint: the new session's own first
    /// save.
    ///
    /// # Errors
    ///
    /// Returns the checkpoint's refusal or unknown outcome, as every managed commit reports it.
    pub async fn checkpoint_seeded(
        &self,
        seeded: &Session,
        runtime: TurnRuntime<'_>,
    ) -> Result<PersistenceState, BundleError> {
        let checkpoint = runtime
            .checkpoint
            .ok_or_else(|| refusal(ErrorCode::UnsupportedMode))?;
        let turn = ManagedTurn::new(checkpoint, runtime.context, Self::context_policy(seeded), 0);
        self.commit(&turn, seeded, CheckpointReason::Handoff, &mut Silent)
            .await?;
        turn.persistence()
            .ok_or_else(|| refusal(ErrorCode::CheckpointNotCommitted))
    }

    /// The source session with the back-link appended, checked and not saved.
    ///
    /// What [`AgentRunner::record_handoff`] checkpoints, and what a host reconciles against the
    /// disk when that checkpoint's outcome is unknown: the candidate is the same in both.
    ///
    /// # Errors
    ///
    /// Returns `stale_draft` or `handoff_unavailable` from [`AgentRunner::check_handoff_source`].
    pub fn back_linked(
        source: &Session,
        draft: &HandoffDraft,
        new_session_id: &SessionId,
    ) -> Result<Session, BundleError> {
        Self::check_handoff_source(source, draft)?;
        let mut candidate = source.clone();
        candidate.append(SessionEvent::ContextHandoff {
            payload: Box::new(HandoffRecord {
                source_frontier: draft.base.clone(),
                new_session_id: new_session_id.as_str().to_owned(),
                document_blake3: draft.document_blake3.clone(),
                confirmed: true,
            }),
        });
        Ok(candidate)
    }

    /// Records the back-link in the source session and checkpoints it through the source's
    /// checkpoint.
    ///
    /// Refuses first with [`AgentRunner::check_handoff_source`], so a moved source records nothing.
    /// On success `source` holds the back-link.
    ///
    /// # Errors
    ///
    /// Returns `stale_draft` or `handoff_unavailable` from the check, and the checkpoint's refusal
    /// or unknown outcome otherwise.
    pub async fn record_handoff(
        &self,
        source: &mut Session,
        draft: &HandoffDraft,
        new_session_id: &SessionId,
        runtime: TurnRuntime<'_>,
    ) -> Result<PersistenceState, BundleError> {
        let candidate = Self::back_linked(source, draft, new_session_id)?;
        let checkpoint = runtime
            .checkpoint
            .ok_or_else(|| refusal(ErrorCode::UnsupportedMode))?;
        let turn = ManagedTurn::new(checkpoint, runtime.context, Self::context_policy(source), 0);
        self.commit(&turn, &candidate, CheckpointReason::Handoff, &mut Silent)
            .await?;
        *source = candidate;
        turn.persistence()
            .ok_or_else(|| refusal(ErrorCode::CheckpointNotCommitted))
    }
}
