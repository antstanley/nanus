//! Settling a managed step, and the idle operations: policy change, reset and recovery.

use nanus_domain::context::managed::{
    ArtifactId, AttemptOutcome, AttemptPhase, CaptureStatus, ContextDecision, ContextMode,
    ContextModeRecord, ContextPolicy, DecisionOutcome, Digest, ErrorCode, ManagedState, ModeActor,
    ModeReason, NoticeFacts, ProjectionRevision, RequestAttemptRecord, Selection, UsageObservation,
    limits, notes, proposal, state,
};
use nanus_domain::{Session, SessionEvent, StepOutcome};
use nanus_ports::{CheckpointReason, PersistenceState, TurnControl, TurnRuntime};

use super::step::StreamFacts;
use super::{ManagedTurn, refusal};
use crate::BundleError;
use crate::agent_loop::{AgentRunner, Progress, Silent, is_cancelled};

/// A proposal that passed validation, with the decision id the host gave it.
#[derive(Clone, Debug)]
pub(in crate::agent_loop) struct Staged {
    /// The validated proposal.
    pub(in crate::agent_loop) staged: proposal::Staged,
    /// The host-generated decision id.
    pub(in crate::agent_loop) decision_id: String,
}

/// Looks published artifacts up by id, from the session's own receipts.
pub(in crate::agent_loop) fn artifact_facts(
    session: &Session,
) -> impl Fn(&ArtifactId) -> Option<notes::ArtifactFacts> + '_ {
    move |id| {
        session.log().events().iter().find_map(|event| match event {
            SessionEvent::ArtifactPublished { payload }
                if payload.artifact_id.as_ref() == Some(id)
                    && payload.status != CaptureStatus::Unavailable =>
            {
                payload
                    .retained_sha256
                    .clone()
                    .map(|retained_sha256| notes::ArtifactFacts {
                        retained_bytes: payload.retained_bytes,
                        retained_sha256,
                    })
            }
            _ => None,
        })
    }
}

impl AgentRunner {
    /// Settles a step: the attempt's outcome, any staged proposal, and one checkpoint.
    pub(in crate::agent_loop) async fn settle(
        &self,
        session: &mut Session,
        turn: &ManagedTurn<'_>,
        step: (
            RequestAttemptRecord,
            StreamFacts,
            &Result<StepOutcome, BundleError>,
        ),
        progress: &mut dyn Progress,
        control: Option<&dyn TurnControl>,
    ) -> Result<(), BundleError> {
        let (attempt, facts, result) = step;
        let finished = self.finished_attempt(attempt, &facts, result);
        let staged = turn.step.borrow_mut().staged.take();
        let mut candidate = session.clone();
        let mut reason = CheckpointReason::SettledStep;
        let decision = staged.map(|staged| {
            let stopped =
                is_cancelled(progress, control) || matches!(result, Ok(StepOutcome::Interrupted));
            let outcome = if stopped {
                Err(ErrorCode::Cancelled)
            } else {
                self.evaluate(session, turn, &staged)
            };
            match outcome {
                Ok(revision) => {
                    reason = CheckpointReason::ContextRevision;
                    let decision = decided(
                        &staged,
                        DecisionOutcome::Accepted,
                        Some(revision.revision),
                        None,
                    );
                    candidate.append(SessionEvent::ContextRevision {
                        payload: Box::new(revision),
                    });
                    decision
                }
                Err(ErrorCode::Cancelled) => decided(
                    &staged,
                    DecisionOutcome::Cancelled,
                    None,
                    Some(ErrorCode::Cancelled),
                ),
                Err(code) => decided(&staged, DecisionOutcome::Rejected, None, Some(code)),
            }
        });
        if let Some(decision) = &decision {
            candidate.append(SessionEvent::ContextDecision {
                payload: Box::new(decision.clone()),
            });
        }
        candidate.append(SessionEvent::RequestAttempt {
            payload: Box::new(finished.clone()),
        });
        match self.commit(turn, &candidate, reason, progress).await {
            Ok(_) => {
                *session = candidate;
                if let Some(decision) = &decision {
                    progress.context_decision(decision);
                }
                Self::count_step(turn);
                Ok(())
            }
            Err(error) => {
                // The effects stay; the revision does not. The attempt's outcome and, for a
                // proposal, its refusal go with the one terminal attempt that remains.
                if let Some(decision) = decision {
                    let refused = ContextDecision {
                        outcome: DecisionOutcome::Rejected,
                        revision: None,
                        error_code: Some(ErrorCode::CheckpointNotCommitted),
                        ..decision
                    };
                    session.append(SessionEvent::ContextDecision {
                        payload: Box::new(refused),
                    });
                }
                session.append(SessionEvent::RequestAttempt {
                    payload: Box::new(finished),
                });
                Err(error)
            }
        }
    }

    fn count_step(turn: &ManagedTurn<'_>) {
        let mut reminder = turn.reminder();
        reminder.step_completed();
        turn.set_reminder(reminder);
    }

    /// The finished record of an attempt.
    fn finished_attempt(
        &self,
        mut attempt: RequestAttemptRecord,
        facts: &StreamFacts,
        result: &Result<StepOutcome, BundleError>,
    ) -> RequestAttemptRecord {
        let outcome = match result {
            Ok(StepOutcome::Interrupted) => AttemptOutcome::Cancelled,
            Ok(_) if facts.assistant_seq.is_some() => AttemptOutcome::Completed,
            Ok(_) | Err(_) => AttemptOutcome::Failed,
        };
        attempt.phase = AttemptPhase::Finished;
        attempt.outcome = Some(outcome);
        attempt.usage = facts.usage.as_ref().map(UsageObservation::from_usage);
        attempt.assistant_seq = facts
            .assistant_seq
            .filter(|_| outcome == AttemptOutcome::Completed);
        attempt.finished_at_ms = Some(self.clock.now_ms());
        attempt.timings_ms = facts.timings;
        attempt
    }

    /// Validates a staged proposal again, after its step settled, and builds its revision.
    fn evaluate(
        &self,
        session: &Session,
        turn: &ManagedTurn<'_>,
        staged: &Staged,
    ) -> Result<ProjectionRevision, ErrorCode> {
        let snapshot = Self::snapshot(session)
            .map_err(|error| error.managed_code().unwrap_or(ErrorCode::SourceCorrupt))?;
        let profile = turn
            .step
            .borrow()
            .profile
            .clone()
            .ok_or(ErrorCode::StaleBase)?;
        let lookup = artifact_facts(session);
        let check = proposal::Snapshot {
            session,
            state: &snapshot.state,
            fragments: &snapshot.fragments,
            protected: &snapshot.protected,
            profile_digest: &profile,
            goal_revision: snapshot.goal.as_ref().map(nanus_domain::Goal::revision),
            artifacts: &lookup,
        };
        proposal::recheck(&check, &staged.staged)?;
        let revision = proposal::revision_from(&staged.staged, staged.decision_id.clone())?;
        let record = SessionEvent::ContextRevision {
            payload: Box::new(revision.clone()),
        };
        nanus_domain::content::serialized_size(&record, limits::REVISION_RECORD_BYTES_MAX)
            .map_err(|_| ErrorCode::StorageCapacity)?;
        self.fits_next(session, turn, &snapshot, &revision)?;
        Self::check_capacity(session, limits::CLOSING_RESERVE_BYTES)
            .map_err(|_| ErrorCode::StorageCapacity)?;
        Ok(revision)
    }

    /// Whether the request after this revision would fit, by a dry preparation.
    ///
    /// An oversized proposal is rejected as it is; nothing is hidden on its behalf.
    fn fits_next(
        &self,
        session: &Session,
        turn: &ManagedTurn<'_>,
        snapshot: &super::prepare::Snapshot,
        revision: &ProjectionRevision,
    ) -> Result<(), ErrorCode> {
        let caps = self.llm().capabilities(&self.model());
        let allowance = self.input_allowance(caps, turn.policy());
        let facts = NoticeFacts {
            input_allowance: allowance,
            recovery_available: true,
            ..NoticeFacts::default()
        };
        let selection = Selection {
            revision: revision.revision,
            hidden: &revision.hidden,
            notes: &revision.notes,
            notes_goal_revision: revision.goal_revision,
        };
        let request = self.compose_for(session, snapshot, selection, &facts, turn.policy())?;
        let estimate = self.prepare_dry(request.clone())?.estimate();
        if estimate.fits(caps, &request) && estimate.input_tokens <= allowance {
            Ok(())
        } else {
            Err(ErrorCode::CandidateTooLarge)
        }
    }

    /// Changes the recorded policy, persisting the record before it is acknowledged.
    pub(in crate::agent_loop) async fn change_policy(
        &self,
        session: &mut Session,
        policy: ContextPolicy,
        actor: ModeActor,
        runtime: TurnRuntime<'_>,
    ) -> Result<Option<PersistenceState>, BundleError> {
        policy.validate().map_err(refusal)?;
        let recorded = session
            .log()
            .events()
            .iter()
            .any(|event| matches!(event, SessionEvent::ContextMode { .. }));
        let current = super::recorded_policy(session);
        if current == policy && (recorded || policy.mode == ContextMode::Legacy) {
            return Ok(None);
        }
        let checkpoint = runtime
            .checkpoint
            .ok_or_else(|| refusal(ErrorCode::UnsupportedMode))?;
        if policy.mode == ContextMode::Managed {
            self.check_activation(policy, runtime)?;
        }
        let reason = match (current.mode, policy.mode) {
            (ContextMode::Legacy, ContextMode::Managed) => ModeReason::Enable,
            (ContextMode::Managed, ContextMode::Legacy) => ModeReason::Disable,
            _ => ModeReason::ConfigurationChange,
        };
        let previous = ManagedState::fold(session.log()).map_or_else(
            |_| ManagedState::highest_revision(session.log()),
            |state| state.revision(),
        );
        let mut candidate = session.clone();
        candidate.upgrade_to_managed_body();
        candidate.append(SessionEvent::ContextMode {
            payload: Box::new(ContextModeRecord {
                policy,
                actor,
                reason,
                previous_revision: previous,
            }),
        });
        let turn = ManagedTurn::new(checkpoint, runtime.context, policy, 0);
        self.commit(&turn, &candidate, CheckpointReason::Activate, &mut Silent)
            .await?;
        *session = candidate;
        Ok(turn.persistence())
    }

    /// Refuses managed activation that this runner, model or host cannot support.
    fn check_activation(
        &self,
        policy: ContextPolicy,
        runtime: TurnRuntime<'_>,
    ) -> Result<(), BundleError> {
        if let Some(code) = self.unready(policy) {
            return Err(refusal(code));
        }
        let Some(context) = runtime.context else {
            return Err(refusal(ErrorCode::UnsupportedMode));
        };
        if policy.capture_shell && (context.archive().is_none() || !self.can_capture()) {
            return Err(BundleError::managed(
                ErrorCode::CaptureUnavailable,
                "shell capture needs an archive and a shell that can capture",
            ));
        }
        Ok(())
    }

    /// Appends an empty host revision and selects legacy replay, as one checkpoint.
    pub(in crate::agent_loop) async fn reset(
        &self,
        session: &mut Session,
        runtime: TurnRuntime<'_>,
    ) -> Result<PersistenceState, BundleError> {
        if !session.is_managed_body() {
            return Err(BundleError::managed(
                ErrorCode::UnsupportedMode,
                "this session has never managed its context; there is nothing to reset",
            ));
        }
        let checkpoint = runtime
            .checkpoint
            .ok_or_else(|| refusal(ErrorCode::UnsupportedMode))?;
        // Reset needs only a raw history that validates; an invalid selection is never run.
        session
            .try_to_jsonl()
            .map_err(|_| refusal(ErrorCode::SourceCorrupt))?;
        let highest = ManagedState::highest_revision(session.log());
        let base = ManagedState::fold(session.log()).map_or(highest, |state| state.revision());
        let frontier = state::frontier(session, base).map_err(refusal)?;
        let decision_id = format!("r-{}", frontier.event_count);
        let revision = proposal::reset_revision(
            highest,
            base,
            frontier,
            Digest::empty(),
            decision_id.clone(),
        )
        .map_err(refusal)?;
        let policy = super::recorded_policy(session).disabled();
        let mut candidate = session.clone();
        let accepted = ContextDecision {
            decision_id,
            outcome: DecisionOutcome::Accepted,
            revision: Some(revision.revision),
            error_code: None,
        };
        candidate.append(SessionEvent::ContextRevision {
            payload: Box::new(revision),
        });
        candidate.append(SessionEvent::ContextDecision {
            payload: Box::new(accepted),
        });
        candidate.append(SessionEvent::ContextMode {
            payload: Box::new(ContextModeRecord {
                policy,
                actor: ModeActor::Human,
                reason: ModeReason::Reset,
                previous_revision: base,
            }),
        });
        let turn = ManagedTurn::new(checkpoint, runtime.context, policy, 0);
        self.commit(&turn, &candidate, CheckpointReason::Reset, &mut Silent)
            .await?;
        *session = candidate;
        turn.persistence()
            .ok_or_else(|| refusal(ErrorCode::CheckpointNotCommitted))
    }

    /// Closes an open turn a checkpoint left behind, under the writer claim.
    pub(in crate::agent_loop) async fn recover(
        &self,
        session: &mut Session,
        runtime: TurnRuntime<'_>,
    ) -> Result<Option<PersistenceState>, BundleError> {
        if !session.is_managed_body() {
            return Ok(None);
        }
        let state = ManagedState::fold(session.log()).unwrap_or_default();
        let Some(plan) =
            state::recovery_plan(session, &state, self.clock.now_ms()).map_err(refusal)?
        else {
            return Ok(None);
        };
        let checkpoint = runtime
            .checkpoint
            .ok_or_else(|| refusal(ErrorCode::UnsupportedMode))?;
        let mut candidate = session.clone();
        for event in plan.events {
            candidate.append(event);
        }
        let policy = super::recorded_policy(session);
        let turn = ManagedTurn::new(checkpoint, runtime.context, policy, 0);
        self.commit(&turn, &candidate, CheckpointReason::Recovery, &mut Silent)
            .await?;
        *session = candidate;
        Ok(turn.persistence())
    }
}

/// A decision record for a staged proposal.
fn decided(
    staged: &Staged,
    outcome: DecisionOutcome,
    revision: Option<u64>,
    error_code: Option<ErrorCode>,
) -> ContextDecision {
    ContextDecision {
        decision_id: staged.decision_id.clone(),
        outcome,
        revision,
        error_code,
    }
}
