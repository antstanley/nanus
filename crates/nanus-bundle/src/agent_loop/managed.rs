//! The managed turn: selection, staged context edits and checkpoints around the ordinary loop.
//!
//! A managed step extends the loop rather than replacing it. The order is the whole design:
//!
//! ```text
//! admit the human turn -> append TurnStart/UserMessage
//! hold the exact selection
//! prepare the effective request -> optional automatic revision
//! reserve record capacity
//! checkpoint settled prefix + automatic revision + RequestAttempt(started)
//! append StepStart -> HTTP/stream -> append the assembled observation
//! classify mixed proposals -> policy/approval -> admission -> execute -> ordered append
//! append StepEnd -> evaluate a staged proposal
//! checkpoint the candidate revision or the unchanged one + RequestAttempt(finished)
//! install what was acknowledged -> honour cancellation -> release the selection
//! next step, or append and checkpoint TurnEnd before the answer is reported
//! ```
//!
//! Every checkpoint commits a *candidate* — a copy of the session with the new records — and only
//! an acknowledged commit installs it. A refused commit leaves the previous file intact and the
//! projection uninstalled; the turn stops new effects and makes one reserved terminal attempt. An
//! unknown outcome freezes the turn until the disk is read back under the same claim, and never
//! permits an alternate overwrite.

use core::cell::{Cell, RefCell};

use nanus_domain::context::managed::{
    CheckpointReceipt, ContextMode, ContextPolicy, ContextStatus, Durability, ErrorCode, ModeActor,
    state,
};
use nanus_domain::{Session, SessionEvent, StepOutcome, TurnMachine};
use nanus_ports::{
    CheckpointError, CheckpointReason, CheckpointView, ContextRuntime, FinalizedArtifact,
    PersistenceState, Reconciled, SessionCheckpoint, TurnControl, TurnRuntime,
};

use super::{AgentRunner, Approver, Progress, RunOutcome, dispatch, is_cancelled};
use crate::BundleError;

#[path = "managed/prepare.rs"]
mod prepare;
#[path = "managed/settle.rs"]
mod settle;
#[path = "managed/step.rs"]
mod step;
#[path = "managed/tools.rs"]
mod tools;

pub(super) use prepare::{Prepared, refusal};
pub(super) use settle::Staged;

/// What a managed run produced, and whether its session is durable.
///
/// The persistence state is returned on failure too: a host decides from it — and only from
/// it — whether any further save is permitted. `None` means no checkpoint was attempted, which is
/// a legacy session the host saves exactly as before.
#[derive(Debug)]
pub struct ManagedRun {
    /// The ordinary turn result.
    pub outcome: Result<RunOutcome, BundleError>,
    /// Where the session's durability stands.
    pub persistence: Option<PersistenceState>,
}

/// Everything a host lends one turn: who answers approvals, how it is stopped, and the
/// session-scoped runtime it saves through.
#[derive(Clone, Copy, Default)]
pub struct TurnHost<'a> {
    /// Who answers an approval question; `None` denies what the sandbox does not permit.
    pub approver: Option<&'a dyn Approver>,
    /// Wakeable cancellation, observed beside [`Progress::cancelled`].
    pub control: Option<&'a dyn TurnControl>,
    /// The session-scoped checkpoint and context runtime.
    pub runtime: TurnRuntime<'a>,
}

impl core::fmt::Debug for TurnHost<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TurnHost")
            .field("approver", &self.approver.is_some())
            .field("control", &self.control.is_some())
            .field("runtime", &self.runtime)
            .finish()
    }
}

/// Whether the turn may still write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Halt {
    /// Checkpoints are being acknowledged.
    Running,
    /// A commit was refused; one reserved terminal attempt remains, or has been used.
    Unsaved {
        /// Whether the terminal attempt has been made.
        terminal_used: bool,
    },
    /// A commit's outcome is unknown and reconciliation could not settle it.
    Frozen,
}

/// The state one managed turn carries between its steps.
pub(super) struct ManagedTurn<'a> {
    checkpoint: &'a dyn SessionCheckpoint,
    context: Option<&'a dyn ContextRuntime>,
    policy: ContextPolicy,
    halt: Cell<Halt>,
    persistence: RefCell<Option<PersistenceState>>,
    /// Events below this count are on disk.
    durable: Cell<u64>,
    /// Finalized archive objects the next checkpoint will newly reference.
    artifacts: RefCell<Vec<FinalizedArtifact>>,
    /// Receipts of streams that could not be reserved, waiting for their call's result.
    unpublished: RefCell<Vec<nanus_domain::context::managed::ArtifactReceipt>>,
    /// The current step's preparation, staged proposal and attempt.
    step: RefCell<step::StepState>,
    /// The reminder state when the host lends no context runtime.
    reminder: Cell<nanus_domain::context::managed::Reminder>,
}

impl<'a> ManagedTurn<'a> {
    fn new(
        checkpoint: &'a dyn SessionCheckpoint,
        context: Option<&'a dyn ContextRuntime>,
        policy: ContextPolicy,
        durable: u64,
    ) -> Self {
        Self {
            checkpoint,
            context,
            policy,
            halt: Cell::new(Halt::Running),
            persistence: RefCell::new(None),
            durable: Cell::new(durable),
            artifacts: RefCell::new(Vec::new()),
            unpublished: RefCell::new(Vec::new()),
            step: RefCell::new(step::StepState::default()),
            reminder: Cell::new(context.map_or_else(Default::default, ContextRuntime::reminder)),
        }
    }

    /// The accepted revision of `session`, zero when its state does not fold.
    pub(super) fn revision(session: &Session) -> u64 {
        state::ManagedState::fold(session.log()).map_or(0, |state| state.revision())
    }

    /// The policy this turn runs under.
    pub(super) const fn policy(&self) -> ContextPolicy {
        self.policy
    }

    /// The reminder, from the runtime when there is one.
    fn reminder(&self) -> nanus_domain::context::managed::Reminder {
        self.context
            .map_or_else(|| self.reminder.get(), ContextRuntime::reminder)
    }

    fn set_reminder(&self, reminder: nanus_domain::context::managed::Reminder) {
        self.reminder.set(reminder);
        if let Some(context) = self.context {
            context.set_reminder(reminder);
        }
    }

    fn persistence(&self) -> Option<PersistenceState> {
        self.persistence.borrow().clone()
    }

    fn stopped(&self) -> bool {
        self.halt.get() != Halt::Running
    }
}

/// The policy a session's last mode record set; the default when it has none.
fn recorded_policy(session: &Session) -> ContextPolicy {
    session
        .log()
        .events()
        .iter()
        .rev()
        .find_map(|event| match event {
            SessionEvent::ContextMode { payload } => Some(payload.policy),
            _ => None,
        })
        .unwrap_or_default()
}

/// The unknown-outcome failure, as the runner reports it.
fn unknown() -> BundleError {
    BundleError::managed(
        ErrorCode::CheckpointUnknown,
        "whether the session was saved is unknown; reconcile before continuing",
    )
}

/// The refused-commit failure, as the runner reports it.
fn not_committed() -> BundleError {
    BundleError::managed(
        ErrorCode::CheckpointNotCommitted,
        "the session could not be saved; the previous saved copy is intact",
    )
}

impl AgentRunner {
    /// Runs one turn of a session through its host-bound runtime.
    ///
    /// A session whose body is not version 3 runs the legacy loop and is saved by its host as
    /// before (`persistence` is `None`). A version-3 session is saved only through
    /// `host.runtime.checkpoint`: in legacy mode once, at the turn's end; in managed mode at
    /// every point the transaction above names. The host must not save it itself afterwards
    /// except as the returned [`PersistenceState`] permits.
    pub async fn run_turn_with_runtime(
        &self,
        session: &mut Session,
        message: &str,
        progress: &mut dyn Progress,
        host: TurnHost<'_>,
    ) -> ManagedRun {
        if !session.is_managed_body() {
            let outcome = self
                .run_controlled(session, message, progress, host.approver, host.control)
                .await;
            return ManagedRun {
                outcome,
                persistence: None,
            };
        }
        let Some(checkpoint) = host.runtime.checkpoint else {
            return ManagedRun {
                outcome: Err(BundleError::managed(
                    ErrorCode::UnsupportedMode,
                    "a version-3 session is saved only through a bound checkpoint",
                )),
                persistence: None,
            };
        };
        if let Err(error) = self.recover_session(session, host.runtime).await {
            return ManagedRun {
                outcome: Err(error),
                persistence: Some(PersistenceState::Unsaved {
                    last: checkpoint.expected(),
                }),
            };
        }
        let policy = recorded_policy(session);
        let durable = u64::try_from(session.event_count()).unwrap_or(u64::MAX);
        let turn = ManagedTurn::new(checkpoint, host.runtime.context, policy, durable);
        let outcome = match policy.mode {
            ContextMode::Legacy => {
                self.run_legacy_v3(session, message, progress, host, &turn)
                    .await
            }
            ContextMode::Managed => {
                self.run_managed(session, message, progress, host, &turn)
                    .await
            }
        };
        ManagedRun {
            outcome,
            persistence: turn.persistence(),
        }
    }

    /// A version-3 session in legacy mode: the legacy loop, then one terminal checkpoint.
    async fn run_legacy_v3(
        &self,
        session: &mut Session,
        message: &str,
        progress: &mut dyn Progress,
        host: TurnHost<'_>,
        turn: &ManagedTurn<'_>,
    ) -> Result<RunOutcome, BundleError> {
        let outcome = self
            .run_controlled(session, message, progress, host.approver, host.control)
            .await;
        let saved = self
            .commit(turn, session, CheckpointReason::TurnEnd, progress)
            .await;
        match (outcome, saved) {
            (Ok(outcome), Ok(_)) => Ok(outcome),
            (Err(error), _) | (Ok(_), Err(error)) => Err(error),
        }
    }

    /// Runs one managed turn to its end, checkpointing the ending before it returns.
    async fn run_managed(
        &self,
        session: &mut Session,
        message: &str,
        progress: &mut dyn Progress,
        host: TurnHost<'_>,
        turn: &ManagedTurn<'_>,
    ) -> Result<RunOutcome, BundleError> {
        let machine = TurnMachine::new(self.config.clone())
            .map_err(|error| BundleError::Config(error.to_string()))?;
        self.check_managed_ready(session, turn)?;
        let index = session.log().current_turn().saturating_add(1);
        let reservation =
            self.reserve_turn_records(session, index, message, progress, host.control)?;
        session.append(SessionEvent::TurnStart { turn: index });
        session.append(SessionEvent::UserMessage {
            text: message.to_owned(),
        });
        let phase = dispatch::Phase {
            position: (index, 0),
            approver: host.approver,
            control: host.control,
            managed: Some(turn),
        };
        let driven = self
            .drive_managed(session, &machine, progress, phase, reservation.as_deref())
            .await;
        let saved = self
            .commit(turn, session, CheckpointReason::TurnEnd, progress)
            .await;
        let steps = driven?;
        saved?;
        assert!(
            session.log().last_turn_end().is_some(),
            "a managed run has a turn end"
        );
        Ok(RunOutcome {
            session_id: session.id().clone(),
            answer: super::last_assistant_text(session),
            reason: session
                .log()
                .last_turn_end()
                .cloned()
                .unwrap_or(nanus_domain::TurnEndReason::Blocked),
            steps,
            usage: session.usage_totals(),
        })
    }

    /// The managed counterpart of `drive_turn`: the same step budget and closing rules.
    async fn drive_managed(
        &self,
        session: &mut Session,
        machine: &TurnMachine,
        progress: &mut dyn Progress,
        phase: dispatch::Phase<'_>,
        reservation: Option<&dyn nanus_ports::TurnRecordReservation>,
    ) -> Result<u32, BundleError> {
        let turn = phase.position.0;
        let mut steps = 0_u32;
        loop {
            let stopped = phase.managed.is_some_and(ManagedTurn::stopped);
            let step_outcome = if is_cancelled(progress, phase.control) || stopped {
                StepOutcome::Interrupted
            } else {
                steps = steps.saturating_add(1);
                progress.step_started(steps);
                let step_phase = dispatch::Phase {
                    position: (turn, steps),
                    ..phase
                };
                match self
                    .run_managed_step(session, progress, step_phase, reservation)
                    .await
                {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        let ended = StepOutcome::Error {
                            message: error.to_string(),
                        };
                        if let Some(reason) =
                            machine.decide(session.log(), &ended).turn_end_reason()
                        {
                            Self::end_turn_records(session, turn, reason, reservation)?;
                        }
                        return Err(error);
                    }
                }
            };
            let Some(reason) = machine
                .decide(session.log(), &step_outcome)
                .turn_end_reason()
            else {
                continue;
            };
            Self::end_turn_records(session, turn, reason, reservation)?;
            if stopped {
                return Err(phase.managed.map_or_else(not_committed, |turn| {
                    match turn.halt.get() {
                        Halt::Frozen => unknown(),
                        _ => not_committed(),
                    }
                }));
            }
            return Ok(steps);
        }
    }

    /// Commits `candidate` through the bound checkpoint and reports what happened.
    ///
    /// On success the durable frontier moves and watchers are told. A refusal halts the turn with
    /// one terminal attempt reserved; an unknown outcome is reconciled at once against the disk,
    /// and freezes the turn when reconciliation cannot settle it.
    async fn commit(
        &self,
        turn: &ManagedTurn<'_>,
        candidate: &Session,
        reason: CheckpointReason,
        progress: &mut dyn Progress,
    ) -> Result<CheckpointReceipt, BundleError> {
        match turn.halt.get() {
            Halt::Running => {}
            Halt::Unsaved {
                terminal_used: false,
            } if reason == CheckpointReason::TurnEnd => {
                turn.halt.set(Halt::Unsaved {
                    terminal_used: true,
                });
            }
            Halt::Unsaved { .. } => return Err(not_committed()),
            Halt::Frozen => return Err(unknown()),
        }
        let expected = turn.checkpoint.expected();
        let artifacts = turn.artifacts.borrow().clone();
        let view = CheckpointView {
            candidate,
            expected: &expected,
            reason,
            artifacts: &artifacts,
        };
        match turn.checkpoint.commit(view).await {
            Ok(receipt) => Ok(Self::acknowledged(turn, receipt, progress)),
            Err(CheckpointError::NotCommitted(_)) => {
                Self::refused(turn, expected);
                Err(not_committed())
            }
            Err(CheckpointError::CommitOutcomeUnknown(_)) => {
                self.reconcile(turn, candidate, expected, progress).await
            }
        }
    }

    fn acknowledged(
        turn: &ManagedTurn<'_>,
        receipt: CheckpointReceipt,
        progress: &mut dyn Progress,
    ) -> CheckpointReceipt {
        turn.artifacts.borrow_mut().clear();
        turn.durable.set(receipt.frontier.event_count);
        *turn.persistence.borrow_mut() = Some(PersistenceState::Acknowledged(receipt.clone()));
        progress.checkpointed(&receipt);
        receipt
    }

    fn refused(turn: &ManagedTurn<'_>, expected: nanus_ports::ExpectedCheckpoint) {
        if turn.halt.get() == Halt::Running {
            turn.halt.set(Halt::Unsaved {
                terminal_used: false,
            });
        }
        *turn.persistence.borrow_mut() = Some(PersistenceState::Unsaved { last: expected });
    }

    /// Reads the disk back after an unknown outcome: the candidate installs, the previous file
    /// keeps the old projection, and anything else quarantines the session.
    async fn reconcile(
        &self,
        turn: &ManagedTurn<'_>,
        candidate: &Session,
        expected: nanus_ports::ExpectedCheckpoint,
        progress: &mut dyn Progress,
    ) -> Result<CheckpointReceipt, BundleError> {
        let count = u64::try_from(candidate.event_count()).unwrap_or(u64::MAX);
        let digest = candidate.prefix_digest(count).map_err(|_| unknown())?;
        match turn.checkpoint.reconcile(&digest).await {
            Ok(Reconciled::Installed(_)) => {
                let revision = ManagedTurn::revision(candidate);
                let frontier = state::frontier(candidate, revision).map_err(|_| unknown())?;
                let receipt = CheckpointReceipt {
                    frontier,
                    body_digest: candidate.body_digest(),
                    durability: Durability::ProcessCrash,
                };
                Ok(Self::acknowledged(turn, receipt, progress))
            }
            Ok(Reconciled::Kept) => {
                Self::refused(turn, expected);
                Err(not_committed())
            }
            Ok(Reconciled::Quarantined) | Err(_) => {
                turn.halt.set(Halt::Frozen);
                *turn.persistence.borrow_mut() = Some(PersistenceState::Unknown {
                    previous: expected,
                    candidate_sha256: digest,
                });
                Err(unknown())
            }
        }
    }

    /// The context policy a session recorded last; the legacy default when it recorded none.
    #[must_use]
    pub fn context_policy(session: &Session) -> ContextPolicy {
        recorded_policy(session)
    }

    /// Reports the session's context status without preparing a model call that is sent.
    ///
    /// The estimate comes from a dry preparation of the accepted selection, so it is the cost the
    /// next request would have before any automatic fit.
    ///
    /// # Errors
    ///
    /// Returns [`BundleError::Managed`] for a session whose managed state does not validate.
    pub fn context_status(
        &self,
        session: &Session,
        runtime: TurnRuntime<'_>,
    ) -> Result<ContextStatus, BundleError> {
        self.idle_status(session, runtime)
    }

    /// Changes the session's context policy while it is idle, persisting the change first.
    ///
    /// Enabling upgrades the body to version 3 through the checkpoint; disabling never
    /// downgrades it. `Ok(None)` when the policy is already in force and nothing was written.
    ///
    /// # Errors
    ///
    /// Returns [`BundleError::Managed`] when the policy cannot be supported here, without
    /// changing the session.
    pub async fn set_context_policy(
        &self,
        session: &mut Session,
        policy: ContextPolicy,
        actor: ModeActor,
        runtime: TurnRuntime<'_>,
    ) -> Result<Option<PersistenceState>, BundleError> {
        self.change_policy(session, policy, actor, runtime).await
    }

    /// Resets the session's context to an empty selection and selects legacy replay.
    ///
    /// # Errors
    ///
    /// Returns [`BundleError::Managed`] when the raw session does not validate or the reset
    /// cannot be checkpointed.
    pub async fn reset_context(
        &self,
        session: &mut Session,
        runtime: TurnRuntime<'_>,
    ) -> Result<PersistenceState, BundleError> {
        self.reset(session, runtime).await
    }

    /// Closes what a crash left open in a managed session, before new work is admitted.
    ///
    /// `Ok(None)` when nothing needed recovery. Nothing is rerun: an open turn is closed as
    /// interrupted and an intent with no outcome is recorded as an unknown dispatch.
    ///
    /// # Errors
    ///
    /// Returns [`BundleError::Managed`] when recovery cannot be checkpointed.
    pub async fn recover_session(
        &self,
        session: &mut Session,
        runtime: TurnRuntime<'_>,
    ) -> Result<Option<PersistenceState>, BundleError> {
        self.recover(session, runtime).await
    }
}
