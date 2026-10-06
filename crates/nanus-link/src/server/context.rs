//! Managed context over the link: the bound checkpoint, the frontier, and the two requests.
//!
//! ## Why the host does not save a managed session
//!
//! A legacy turn is recorded once, when it ends, and the agent is what writes it. A managed turn
//! saves *while it runs* — a settled prefix before every request, every settled step, every
//! accepted revision — through a checkpoint bound to the session under the agent's claim, and the
//! checkpoint remembers which stored file the next commit must replace. A host that saved the
//! session again at the end would replace a file the checkpoint did not expect, and a host that
//! saved after a commit whose outcome is *unknown* could overwrite exactly the write it was unsure
//! about. So the runner returns how far its checkpoints got, and this module decides the ending
//! from that and from nothing else: an acknowledged checkpoint is final, a refused one is not
//! retried, and an unknown one is settled by reading the disk — never by writing it.
//!
//! ## Why the frontier lives beside the backlog
//!
//! An attaching client reads the store and is sent the backlog, and the two must add up to the
//! conversation exactly once. The frontier is where one ends and the other begins, and every
//! change to it — a terminal save, a checkpoint, an idle change — moves it and retires the backlog
//! it covers in one step with no await between, so an attachment can never see one without the
//! other.

use std::rc::Rc;

use nanus_bundle::{ManagedRun, SessionContext, StoreCheckpoint};
use nanus_domain::context::managed::{
    CheckpointReceipt, ContextDecision, ContextFrontier, ContextMode, ContextStatus,
    DecisionOutcome, Digest, Durability, ManagedState,
};
use nanus_domain::{Session, SessionId};
use nanus_ports::{
    CheckpointError, CheckpointReason, CheckpointView, ExpectedCheckpoint, PersistenceState,
    Reconciled, SessionCheckpoint, StoreHandle, TurnRuntime,
};
use tokio::sync::mpsc;

use super::{Held, Registry, Reserved, TurnEnd, broadcast_awaited, send};
use crate::protocol::{
    CheckpointInfo, ContextAction, ContextDecisionInfo, ContextModeState, ContextStatusInfo,
    DecisionState, DurabilityState, Frame, FrontierInfo,
};

/// What a turn of a managed session is lent: the checkpoint bound to the session's store under
/// this agent's claim, and the session's context runtime.
pub(super) struct Binding {
    /// The bound checkpoint, which remembers the stored identity the next commit replaces.
    checkpoint: StoreCheckpoint,
    /// The archive and the process-held cursor key.
    context: SessionContext,
}

impl Binding {
    /// Binds a held session to its store.
    ///
    /// Called only once the claim is taken, because the checkpoint reads the stored identity it
    /// will compare every later commit against, and a writer without the claim could change it.
    ///
    /// # Errors
    ///
    /// Returns the sentence to show when the store cannot report the session's stored identity —
    /// a store that cannot checkpoint cannot run a managed turn — or there is no random source.
    async fn bind(store: &StoreHandle, id: &SessionId) -> Result<Self, String> {
        let checkpoint = StoreCheckpoint::bind(Rc::clone(store), id.clone())
            .await
            .map_err(|error| {
                format!("this session's managed context cannot be bound to its store: {error}")
            })?;
        let context = SessionContext::new(Some(Rc::clone(store))).map_err(|error| {
            format!("this session's managed context cannot be bound to its store: {error}")
        })?;
        Ok(Self {
            checkpoint,
            context,
        })
    }

    /// The runtime one turn, read or reset borrows.
    pub(super) fn runtime(&self) -> TurnRuntime<'_> {
        TurnRuntime {
            context: Some(&self.context),
            checkpoint: Some(&self.checkpoint),
        }
    }
}

/// A context status the session published, with where in the session it was published.
#[derive(Clone)]
pub(super) struct Published {
    /// The status.
    pub(super) status: ContextStatus,
    /// The turn it describes, or `None` for one read while idle.
    pub(super) turn: Option<u64>,
    /// The step it describes.
    pub(super) step: Option<u64>,
}

/// The sentence a session that may not continue is refused with.
const QUARANTINED: &str = "this session cannot take another turn: the store holds neither the \
     checkpoint the agent last wrote nor the one before it; `/context reset` or a repaired store \
     is needed before it continues";

/// How many events `session` holds, as the frontier counts them.
fn count(session: &Session) -> u64 {
    u64::try_from(session.event_count()).unwrap_or(u64::MAX)
}

/// The accepted context revision of `session`; zero for a legacy body.
///
/// A body whose managed records do not fold still has a highest revision, which is what a
/// frontier names: the selection is not executed here, only counted.
fn revision_of(session: &Session) -> u64 {
    if !session.is_managed_body() {
        return 0;
    }
    ManagedState::fold(session.log()).map_or_else(
        |_| ManagedState::highest_revision(session.log()),
        |state| state.revision(),
    )
}

/// The frontier of the whole of `session`: what the store holds once it holds this.
pub(super) fn frontier_of(session: &Session) -> ContextFrontier {
    let event_count = count(session);
    // The count is the log's own length, which a prefix digest cannot refuse; the empty digest is
    // the answer to a failure that cannot happen rather than a panic waiting for one.
    let prefix_sha256 = session
        .prefix_digest(event_count)
        .unwrap_or_else(|_| Digest::empty());
    ContextFrontier {
        session_id: session.id().as_str().to_owned(),
        event_count,
        prefix_sha256,
        projection_revision: revision_of(session),
    }
}

/// Renders a frontier in the link's vocabulary.
pub(super) fn wire_frontier(frontier: &ContextFrontier) -> FrontierInfo {
    FrontierInfo {
        session_id: frontier.session_id.clone(),
        event_count: frontier.event_count,
        prefix_sha256: frontier.prefix_sha256.as_str().to_owned(),
        projection_revision: frontier.projection_revision,
    }
}

/// Renders a context mode in the link's vocabulary.
///
/// An exhaustive match, so a mode the domain grows is a compile error here rather than a mode
/// the interface silently draws as another.
const fn wire_mode(mode: ContextMode) -> ContextModeState {
    match mode {
        ContextMode::Legacy => ContextModeState::Legacy,
        ContextMode::Managed => ContextModeState::Managed,
    }
}

/// Renders a decision outcome in the link's vocabulary; exhaustive for the same reason.
const fn wire_outcome(outcome: DecisionOutcome) -> DecisionState {
    match outcome {
        DecisionOutcome::Staged => DecisionState::Staged,
        DecisionOutcome::Accepted => DecisionState::Accepted,
        DecisionOutcome::Rejected => DecisionState::Rejected,
        DecisionOutcome::Cancelled => DecisionState::Cancelled,
    }
}

/// Renders a durability grade in the link's vocabulary; exhaustive for the same reason.
const fn wire_durability(durability: Durability) -> DurabilityState {
    match durability {
        Durability::ProcessCrash => DurabilityState::ProcessCrash,
        Durability::PowerLoss => DurabilityState::PowerLoss,
    }
}

/// Renders a context decision in the link's vocabulary.
pub(super) fn wire_decision(decision: &ContextDecision) -> ContextDecisionInfo {
    ContextDecisionInfo {
        decision_id: decision.decision_id.clone(),
        outcome: wire_outcome(decision.outcome),
        revision: decision.revision,
        error_code: decision.error_code.map(|code| code.as_str().to_owned()),
    }
}

/// Renders a context status in the link's vocabulary.
pub(super) fn wire_status(status: &ContextStatus) -> ContextStatusInfo {
    ContextStatusInfo {
        mode: wire_mode(status.mode),
        revision: status.revision,
        frontier: wire_frontier(&status.frontier),
        estimate_input_tokens: status.estimate_input_tokens,
        estimate_protected_tokens: status.estimate_protected_tokens,
        output_reserve_tokens: status.output_reserve_tokens,
        estimator: status.estimator.clone(),
        hidden_fragments: status.hidden_fragments,
        protected_fragments: status.protected_fragments,
        goal_revision: status.goal_revision,
        goal_data_available: status.goal_data_available,
        recall_available: status.recall_available,
        archive_available: status.archive_available,
        last_decision: status.last_decision.as_ref().map(wire_decision),
        profile_digest: status.profile_digest.as_str().to_owned(),
        managed_ready: status.managed_ready,
        unavailable_reason: status
            .unavailable_reason
            .map(|code| code.as_str().to_owned()),
    }
}

/// Renders a checkpoint receipt in the link's vocabulary.
pub(super) fn wire_receipt(receipt: &CheckpointReceipt) -> CheckpointInfo {
    CheckpointInfo {
        frontier: wire_frontier(&receipt.frontier),
        body_digest: receipt.body_digest.as_str().to_owned(),
        durability: wire_durability(receipt.durability),
    }
}

/// Binds a held managed session to its store and closes what a crash left open, once.
///
/// Nothing to do for a legacy body, a session already bound, or one already refused. The
/// session is reserved while this runs, so no turn can start on a log that is about to gain a
/// recovery record; a binding that cannot be made quarantines the session rather than letting a
/// managed body run without its checkpoint.
pub(super) async fn prepare(registry: &Registry, held: &Rc<Held>) {
    let unbound = held.managed.borrow().is_none() && held.quarantine.borrow().is_none();
    // Reserved before the session is read: a turn that is running holds it, and a session that
    // is busy is either being prepared already or was prepared before its turn began.
    if !unbound || held.busy.replace(true) {
        return;
    }
    let _reserved = Reserved(&held.busy);
    if !held.session.borrow().is_managed_body() {
        return;
    }
    let binding = match Binding::bind(&registry.agent.store, &held.id).await {
        Ok(binding) => Rc::new(binding),
        Err(reason) => {
            held.quarantine(reason);
            return;
        }
    };
    *held.managed.borrow_mut() = Some(Rc::clone(&binding));
    recover(registry, held, &binding).await;
}

/// Closes an interrupted turn through the bound checkpoint, before new work is admitted.
///
/// The runner works on a copy, and the copy replaces the held session only once the store holds
/// it: a recovery that was not saved leaves the held session exactly as it was loaded and
/// refuses further turns, because a new turn on top of an unclosed one is the thing recovery
/// exists to prevent.
async fn recover(registry: &Registry, held: &Held, binding: &Binding) {
    let mut candidate = held.session.borrow().clone();
    let recovered = registry
        .agent
        .runner()
        .recover_session(&mut candidate, binding.runtime())
        .await;
    let state = match recovered {
        Ok(None) => return,
        Ok(Some(state)) => state,
        Err(error) => {
            held.quarantine(format!(
                "this session cannot take another turn: an interrupted turn could not be closed \
                 ({error}); `/context reset` selects legacy replay"
            ));
            return;
        }
    };
    match settle(binding, state, &candidate).await {
        Settled::Durable(receipt) if receipt.frontier.event_count == count(&candidate) => {
            held.refresh(&candidate);
            *held.session.borrow_mut() = candidate;
            held.saved(receipt.frontier);
        }
        Settled::Durable(_) | Settled::Kept => held.quarantine(String::from(
            "this session cannot take another turn: the record closing an interrupted turn could \
             not be saved; the stored copy is unchanged",
        )),
        Settled::Quarantined(reason) => held.quarantine(reason),
    }
}

/// Where a managed write ended up.
enum Settled {
    /// The store holds the candidate, as this receipt describes.
    Durable(CheckpointReceipt),
    /// The previous file is intact and the candidate is not on disk.
    Kept,
    /// Neither can be established; nothing may be built on the session until it is repaired.
    Quarantined(String),
}

/// Decides where a managed write ended up, reconciling an unknown outcome by reading the disk.
///
/// The only thing an unknown outcome permits is a read: writing again — the old terminal save —
/// could replace the very file whose fate is in question.
async fn settle(binding: &Binding, state: PersistenceState, session: &Session) -> Settled {
    match state {
        PersistenceState::Acknowledged(receipt) => Settled::Durable(receipt),
        // The runner has already made the one reserved attempt it is allowed; a second here is
        // what the contract forbids.
        PersistenceState::Unsaved { .. } => Settled::Kept,
        PersistenceState::Unknown {
            candidate_sha256, ..
        } => match binding.checkpoint.reconcile(&candidate_sha256).await {
            Ok(Reconciled::Installed(stored)) => receipt_from(&stored, session).map_or_else(
                || Settled::Quarantined(String::from(QUARANTINED)),
                Settled::Durable,
            ),
            Ok(Reconciled::Kept) => Settled::Kept,
            Ok(Reconciled::Quarantined) => Settled::Quarantined(String::from(QUARANTINED)),
            Err(error) => Settled::Quarantined(format!(
                "this session cannot take another turn: whether its last checkpoint was saved \
                 could not be read back ({error})"
            )),
        },
    }
}

/// The receipt a reconciliation that found the candidate on disk amounts to.
///
/// `None` when the stored file is not, byte for byte, a prefix of `session` — the caller treats
/// that as quarantine. An equal event count is not enough: after an uncertain commit the file on
/// disk may hold the candidate the runner tried to write while the held session has since
/// diverged from it, and a receipt built from the held copy would call a session saved that the
/// disk does not hold, and let the next commit overwrite what the disk does.
fn receipt_from(stored: &ExpectedCheckpoint, session: &Session) -> Option<CheckpointReceipt> {
    let ExpectedCheckpoint::Stored {
        file_sha256,
        event_count,
        ..
    } = stored
    else {
        return None;
    };
    if session.prefix_digest(*event_count).ok()? != *file_sha256 {
        return None;
    }
    let prefix = session.prefix(*event_count).ok()?;
    Some(CheckpointReceipt {
        frontier: ContextFrontier {
            session_id: session.id().as_str().to_owned(),
            event_count: *event_count,
            prefix_sha256: file_sha256.clone(),
            projection_revision: revision_of(&prefix),
        },
        body_digest: prefix.body_digest(),
        durability: Durability::ProcessCrash,
    })
}

/// Decides how a managed turn ends, from how far its checkpoints got.
///
/// `Done` is sent only for a turn whose whole session is on disk, which is the promise the frame
/// makes to every client; anything less is a `Failed` that says the stored copy ends at the last
/// checkpoint. Nothing here writes the session.
pub(super) async fn managed_ending(
    held: &Held,
    session: &Session,
    run: ManagedRun,
    binding: &Binding,
) -> Frame {
    let durable = match run.persistence {
        // No checkpoint was attempted: if the turn appended nothing, the store already holds it
        // all; if it did, the store does not, and this host may not save it.
        None => held.frontier.borrow().event_count == count(session),
        Some(state) => match settle(binding, state, session).await {
            Settled::Durable(receipt) => {
                let whole = receipt.frontier.event_count == count(session);
                if whole {
                    held.saved(receipt.frontier);
                } else {
                    held.advance(receipt.frontier);
                }
                whole
            }
            Settled::Kept => false,
            Settled::Quarantined(reason) => {
                held.quarantine(reason);
                false
            }
        },
    };
    match (run.outcome, durable) {
        (Ok(result), true) => Frame::Done {
            answer: result.answer,
            reason: TurnEnd::from(&result.reason),
        },
        (Ok(_), false) => Frame::Failed {
            message: String::from(
                "the turn ended, but the session is not saved past its last checkpoint; the \
                 stored copy ends there",
            ),
        },
        (Err(error), true) => Frame::Failed {
            message: error.to_string(),
        },
        (Err(error), false) => Frame::Failed {
            message: format!(
                "{error}; the session is not saved past its last checkpoint either, and the \
                 stored copy ends there"
            ),
        },
    }
}

/// Records an idle change to a held session — a goal change — and moves the frontier.
///
/// A legacy body is recorded as it always was. A managed body goes through its checkpoint, so
/// the stored identity the next commit expects is the one this write leaves; an unknown outcome
/// is settled by reading, as a turn's is. Every viewer is told of a checkpoint.
///
/// # Errors
///
/// Returns the sentence to show when the change is not on disk; the held session is then left
/// as it was by the caller.
pub(super) async fn record_idle(
    registry: &Registry,
    held: &Held,
    updated: &Session,
) -> Result<(), String> {
    if !updated.is_managed_body() {
        registry
            .agent
            .record(updated)
            .await
            .map_err(|error| error.to_string())?;
        held.saved(frontier_of(updated));
        return Ok(());
    }
    let bound = held.managed.borrow().clone();
    let Some(binding) = bound else {
        return Err(String::from(
            "this session's managed context is not bound to its store",
        ));
    };
    let expected = binding.checkpoint.expected();
    let view = CheckpointView {
        candidate: updated,
        expected: &expected,
        // An idle session is between turns, every one of them ended: the point a turn's own
        // final checkpoint is taken at.
        reason: CheckpointReason::TurnEnd,
        artifacts: &[],
    };
    let committed = binding.checkpoint.commit(view).await;
    let receipt = match committed {
        Ok(receipt) => receipt,
        Err(CheckpointError::NotCommitted(code)) => {
            return Err(format!(
                "the checkpoint was refused ({code}); the stored copy is unchanged"
            ));
        }
        Err(CheckpointError::CommitOutcomeUnknown(_)) => {
            let candidate_sha256 = updated
                .prefix_digest(count(updated))
                .map_err(|error| error.to_string())?;
            let unknown = PersistenceState::Unknown {
                previous: expected,
                candidate_sha256,
            };
            match settle(&binding, unknown, updated).await {
                Settled::Durable(receipt) => receipt,
                Settled::Kept => {
                    return Err(String::from(
                        "the checkpoint was not saved; the stored copy is unchanged",
                    ));
                }
                Settled::Quarantined(reason) => {
                    held.quarantine(reason.clone());
                    return Err(reason);
                }
            }
        }
    };
    held.saved(receipt.frontier.clone());
    announce_checkpoint(held, &receipt).await;
    Ok(())
}

/// Tells every viewer of an idle session that a checkpoint moved its frontier.
async fn announce_checkpoint(held: &Held, receipt: &CheckpointReceipt) {
    let id = held.next_frame_id();
    let frame = Frame::Checkpoint {
        envelope: held.envelope(None, None, id),
        payload: wire_receipt(receipt),
    };
    broadcast_awaited(held, frame, None).await;
}

/// Answers a context request from the session this connection is watching.
pub(super) async fn answer(
    registry: &Rc<Registry>,
    frames: &mpsc::Sender<Frame>,
    held: &Rc<Held>,
    action: ContextAction,
) {
    match action {
        ContextAction::Status => status(registry, frames, held).await,
        ContextAction::Reset => reset(registry, frames, held).await,
    }
}

/// Answers a status read: from the published snapshot while a turn runs, fresh while idle.
///
/// Never by borrowing a running turn's session — the turn holds it, and a reader asking how its
/// context stands has asked nothing the turn needs to refuse. A status the runner cannot give is
/// a refusal with its reason rather than a guess.
async fn status(registry: &Rc<Registry>, frames: &mpsc::Sender<Frame>, held: &Held) {
    let reply = if held.busy.get() {
        published(held)
    } else {
        fresh(registry, held)
    };
    match reply {
        Ok(frame) => send(frames, frame).await,
        Err(message) => send(frames, Frame::Refused { message }).await,
    }
}

/// A status frame from what the running turn last published.
fn published(held: &Held) -> Result<Frame, String> {
    let snapshot = held.context.borrow().clone();
    let Some(snapshot) = snapshot else {
        return Err(String::from(
            "a turn is running and has not published a context status yet; ask again when it \
             has, or when the session is idle",
        ));
    };
    let id = held.next_frame_id();
    Ok(Frame::ContextStatus {
        envelope: held.envelope(snapshot.turn, snapshot.step, id),
        payload: wire_status(&snapshot.status),
    })
}

/// A status frame read now from an idle session, published as the session's snapshot.
fn fresh(registry: &Registry, held: &Held) -> Result<Frame, String> {
    let computed = {
        let session = held.session.borrow();
        let binding = held.managed.borrow().clone();
        let runtime = binding
            .as_deref()
            .map_or_else(TurnRuntime::default, Binding::runtime);
        registry.agent.runner().context_status(&session, runtime)
    };
    let status =
        computed.map_err(|error| format!("the context status is not available: {error}"))?;
    let id = held.next_frame_id();
    let frame = Frame::ContextStatus {
        envelope: held.envelope(None, None, id),
        payload: wire_status(&status),
    };
    *held.context.borrow_mut() = Some(Published {
        status,
        turn: None,
        step: None,
    });
    Ok(frame)
}

/// Resets the session's context while it is idle, and tells every viewer once it is saved.
///
/// A busy session is refused rather than queued: the reset writes the log, and the turn holds
/// it. The session is reserved for the reset's own awaits, so a prompt from another viewer
/// cannot start a turn on a log that is about to change.
async fn reset(registry: &Rc<Registry>, frames: &mpsc::Sender<Frame>, held: &Rc<Held>) {
    if held.busy.replace(true) {
        let message = String::from(
            "a turn is running in this session; the context can be reset when it is idle",
        );
        send(frames, Frame::Refused { message }).await;
        return;
    }
    let outcome = {
        let _reserved = Reserved(&held.busy);
        reset_reserved(registry, held).await
    };
    match outcome {
        Ok(receipt) => {
            announce_checkpoint(held, &receipt).await;
            match fresh(registry, held) {
                Ok(frame) => broadcast_awaited(held, frame, None).await,
                Err(message) => tracing::debug!(%message, "no status follows the reset"),
            }
        }
        Err(message) => send(frames, Frame::Refused { message }).await,
    }
}

/// Runs a reset in a session the caller has reserved.
///
/// The runner works on a copy, and the copy replaces the held session only once the store holds
/// all of it — the rule a goal change follows — so a reset that was not saved changes nothing.
async fn reset_reserved(registry: &Registry, held: &Held) -> Result<CheckpointReceipt, String> {
    assert!(held.busy.get(), "a reset runs in a reserved session");
    let bound = held.managed.borrow().clone();
    let binding = match bound {
        Some(binding) => binding,
        None => Rc::new(Binding::bind(&registry.agent.store, &held.id).await?),
    };
    let mut candidate = held.session.borrow().clone();
    let state = registry
        .agent
        .runner()
        .reset_context(&mut candidate, binding.runtime())
        .await
        .map_err(|error| format!("the context could not be reset: {error}"))?;
    let receipt = match settle(&binding, state, &candidate).await {
        Settled::Durable(receipt) if receipt.frontier.event_count == count(&candidate) => receipt,
        Settled::Durable(_) | Settled::Kept => {
            return Err(String::from(
                "the reset could not be saved; the session is unchanged",
            ));
        }
        Settled::Quarantined(reason) => {
            held.quarantine(reason.clone());
            return Err(reason);
        }
    };
    held.refresh(&candidate);
    let managed = candidate.is_managed_body();
    *held.session.borrow_mut() = candidate;
    held.saved(receipt.frontier.clone());
    if managed {
        *held.managed.borrow_mut() = Some(binding);
    }
    // A reset that is saved is the explicit recovery a quarantine waits for.
    *held.quarantine.borrow_mut() = None;
    Ok(receipt)
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::path::PathBuf;

    use nanus_bundle::RunOutcome;
    use nanus_domain::context::managed::ErrorCode;
    use nanus_domain::{SessionEvent, TurnEndReason, Usage};
    use nanus_ports::{LocalBoxFuture, SessionSummary, StoreError, StorePort, StoreResult};

    use super::*;

    /// A store that answers the stored identity it is told to, and counts every write.
    ///
    /// What these tests are about is what the *host* does with a managed run's persistence state,
    /// so the store's only job is to say what is on disk and to notice being written to.
    struct ScriptedStore {
        identity: Rc<RefCell<ExpectedCheckpoint>>,
        saves: Rc<Cell<u32>>,
        commits: Rc<Cell<u32>>,
    }

    impl StorePort for ScriptedStore {
        fn save<'a>(&'a self, _session: &'a Session) -> LocalBoxFuture<'a, StoreResult<()>> {
            self.saves.set(self.saves.get().saturating_add(1));
            Box::pin(async { Ok(()) })
        }

        fn load<'a>(&'a self, id: &'a SessionId) -> LocalBoxFuture<'a, StoreResult<Session>> {
            Box::pin(async move {
                Err(StoreError::NotFound {
                    id: id.as_str().to_owned(),
                })
            })
        }

        fn list(&self) -> LocalBoxFuture<'_, StoreResult<Vec<SessionSummary>>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn delete<'a>(&'a self, _id: &'a SessionId) -> LocalBoxFuture<'a, StoreResult<()>> {
            Box::pin(async { Ok(()) })
        }

        fn name<'a>(
            &'a self,
            _id: &'a SessionId,
            _name: &'a str,
        ) -> LocalBoxFuture<'a, StoreResult<()>> {
            Box::pin(async { Ok(()) })
        }

        fn resolve<'a>(
            &'a self,
            _name: &'a str,
        ) -> LocalBoxFuture<'a, StoreResult<Option<SessionId>>> {
            Box::pin(async { Ok(None) })
        }

        fn name_of<'a>(
            &'a self,
            _id: &'a SessionId,
        ) -> LocalBoxFuture<'a, StoreResult<Option<String>>> {
            Box::pin(async { Ok(None) })
        }

        fn home(&self) -> LocalBoxFuture<'_, StoreResult<PathBuf>> {
            Box::pin(async { Ok(PathBuf::from("/nowhere")) })
        }

        fn lock<'a>(
            &'a self,
            _id: &'a SessionId,
            _owner: &'a str,
        ) -> LocalBoxFuture<'a, StoreResult<()>> {
            Box::pin(async { Ok(()) })
        }

        fn release_lock(&self, _id: &SessionId) {}

        fn stored_identity<'a>(
            &'a self,
            _id: &'a SessionId,
        ) -> LocalBoxFuture<'a, StoreResult<ExpectedCheckpoint>> {
            let identity = self.identity.borrow().clone();
            Box::pin(async move { Ok(identity) })
        }

        fn checkpoint<'a>(
            &'a self,
            _view: CheckpointView<'a>,
        ) -> LocalBoxFuture<'a, Result<CheckpointReceipt, CheckpointError>> {
            self.commits.set(self.commits.get().saturating_add(1));
            Box::pin(async { Err(CheckpointError::NotCommitted(ErrorCode::StorageCapacity)) })
        }
    }

    /// The identity a store reports for `session` written whole.
    fn stored(session: &Session) -> ExpectedCheckpoint {
        ExpectedCheckpoint::Stored {
            body_version: session.body_version(),
            file_sha256: frontier_of(session).prefix_sha256,
            event_count: count(session),
        }
    }

    /// A managed session two events long, as it was loaded, and the same session after a turn.
    fn sessions() -> (Session, Session) {
        let mut before = Session::new(SessionId::new("managed"), 1, "/work");
        before.upgrade_to_managed_body();
        before.append(SessionEvent::UserMessage {
            text: "an earlier question".to_owned(),
        });
        before.append(SessionEvent::UserMessage {
            text: "and another".to_owned(),
        });
        let mut after = before.clone();
        after.append(SessionEvent::UserMessage {
            text: "this turn".to_owned(),
        });
        (before, after)
    }

    /// What a test reads back from a [`ScriptedStore`] it has handed to a binding.
    struct Probe {
        identity: Rc<RefCell<ExpectedCheckpoint>>,
        saves: Rc<Cell<u32>>,
        commits: Rc<Cell<u32>>,
    }

    /// The host's side of a managed session: the store, the binding, and the held session.
    struct Fixture {
        store: Probe,
        held: Held,
        binding: Binding,
    }

    fn fixture(loaded: &Session) -> Fixture {
        let store = Probe {
            identity: Rc::new(RefCell::new(stored(loaded))),
            saves: Rc::new(Cell::new(0)),
            commits: Rc::new(Cell::new(0)),
        };
        let handle: StoreHandle = Rc::new(Box::new(ScriptedStore {
            identity: Rc::clone(&store.identity),
            saves: Rc::clone(&store.saves),
            commits: Rc::clone(&store.commits),
        }));
        let binding = Binding {
            checkpoint: StoreCheckpoint::new(
                Rc::clone(&handle),
                loaded.id().clone(),
                stored(loaded),
            ),
            context: SessionContext::with_key(Some(handle), [7; 32]),
        };
        let held = Held::new(loaded.clone(), None, None, String::from("e"), 0);
        Fixture {
            store,
            held,
            binding,
        }
    }

    fn completed(session: &Session) -> RunOutcome {
        RunOutcome {
            session_id: session.id().clone(),
            answer: String::from("the answer"),
            reason: TurnEndReason::Completed,
            steps: 1,
            usage: Usage::default(),
        }
    }

    fn receipt_for(session: &Session) -> CheckpointReceipt {
        CheckpointReceipt {
            frontier: frontier_of(session),
            body_digest: session.body_digest(),
            durability: Durability::ProcessCrash,
        }
    }

    fn run(outcome: RunOutcome, state: Option<PersistenceState>) -> ManagedRun {
        ManagedRun {
            outcome: Ok(outcome),
            persistence: state,
        }
    }

    /// An acknowledged checkpoint covering the whole turn ends it with `Done`, moves the frontier,
    /// empties the backlog — and is not saved a second time, by either path.
    #[test]
    fn an_acknowledged_turn_ends_done_and_is_not_saved_again() {
        let (before, after) = sessions();
        let fixture = fixture(&before);
        fixture.held.keep(
            1,
            Some(0),
            None,
            &Frame::User {
                text: "this turn".to_owned(),
            },
        );
        let state = PersistenceState::Acknowledged(receipt_for(&after));
        let ending = nanus_kernel::runtime::block_on(managed_ending(
            &fixture.held,
            &after,
            run(completed(&after), Some(state)),
            &fixture.binding,
        ));
        assert!(
            matches!(ending, Frame::Done { ref answer, .. } if answer == "the answer"),
            "{ending:?}"
        );
        assert_eq!(fixture.held.frontier.borrow().event_count, 3);
        assert!(
            fixture.held.turn_frames().is_empty(),
            "the store has the turn"
        );
        assert_eq!(fixture.store.saves.get(), 0, "no terminal save");
        assert_eq!(fixture.store.commits.get(), 0, "no second commit");
    }

    /// A refused checkpoint is final: the ending says the session is not saved, the backlog is
    /// kept for a later client, and nothing here tries again.
    #[test]
    fn an_unsaved_turn_ends_failed_and_is_not_retried() {
        let (before, after) = sessions();
        let fixture = fixture(&before);
        fixture.held.keep(
            1,
            Some(0),
            None,
            &Frame::User {
                text: "this turn".to_owned(),
            },
        );
        let state = PersistenceState::Unsaved {
            last: stored(&before),
        };
        let ending = nanus_kernel::runtime::block_on(managed_ending(
            &fixture.held,
            &after,
            run(completed(&after), Some(state)),
            &fixture.binding,
        ));
        assert!(
            matches!(ending, Frame::Failed { ref message } if message.contains("not saved")),
            "{ending:?}"
        );
        assert_eq!(
            fixture.held.frontier.borrow().event_count,
            2,
            "the frontier stays"
        );
        assert_eq!(
            fixture.held.turn_frames().len(),
            1,
            "the backlog is what the store lacks"
        );
        assert_eq!(fixture.store.saves.get(), 0);
        assert_eq!(fixture.store.commits.get(), 0);
        assert!(
            fixture.held.quarantine.borrow().is_none(),
            "the old file is intact"
        );
    }

    /// An unknown outcome is settled by reading the disk, three ways: the candidate is there, the
    /// previous file is there, or neither — and in no case is anything written.
    #[test]
    fn an_unknown_outcome_is_settled_by_reading_and_never_by_writing() {
        let (before, after) = sessions();
        let unknown = || PersistenceState::Unknown {
            previous: stored(&before),
            candidate_sha256: frontier_of(&after).prefix_sha256,
        };

        // The candidate is on disk: the turn is durable, and ends as it would have.
        let installed = fixture(&before);
        *installed.store.identity.borrow_mut() = stored(&after);
        let ending = nanus_kernel::runtime::block_on(managed_ending(
            &installed.held,
            &after,
            run(completed(&after), Some(unknown())),
            &installed.binding,
        ));
        assert!(matches!(ending, Frame::Done { .. }), "{ending:?}");
        assert_eq!(installed.held.frontier.borrow().event_count, 3);
        assert_eq!(
            installed.held.frontier.borrow().prefix_sha256,
            frontier_of(&after).prefix_sha256
        );

        // The previous file is on disk: not saved, not quarantined.
        let kept = fixture(&before);
        let ending = nanus_kernel::runtime::block_on(managed_ending(
            &kept.held,
            &after,
            run(completed(&after), Some(unknown())),
            &kept.binding,
        ));
        assert!(matches!(ending, Frame::Failed { .. }), "{ending:?}");
        assert_eq!(kept.held.frontier.borrow().event_count, 2);
        assert!(kept.held.quarantine.borrow().is_none());

        // Neither: nothing may be built on it until it is repaired or reset.
        let lost = fixture(&before);
        *lost.store.identity.borrow_mut() = ExpectedCheckpoint::Absent;
        let ending = nanus_kernel::runtime::block_on(managed_ending(
            &lost.held,
            &after,
            run(completed(&after), Some(unknown())),
            &lost.binding,
        ));
        assert!(matches!(ending, Frame::Failed { .. }), "{ending:?}");
        assert!(
            lost.held.quarantine.borrow().is_some(),
            "continuation is refused"
        );

        for fixture in [&installed, &kept, &lost] {
            assert_eq!(
                fixture.store.saves.get(),
                0,
                "an unknown outcome is never overwritten"
            );
            assert_eq!(fixture.store.commits.get(), 0);
        }
    }

    /// A run that attempted no checkpoint and appended nothing is durable as it stands; one that
    /// appended something is not, and the host may not save it to make it so.
    #[test]
    fn a_run_with_no_checkpoint_is_durable_only_if_nothing_changed() {
        let (before, after) = sessions();
        let unchanged = fixture(&before);
        let ending = nanus_kernel::runtime::block_on(managed_ending(
            &unchanged.held,
            &before,
            run(completed(&before), None),
            &unchanged.binding,
        ));
        assert!(matches!(ending, Frame::Done { .. }), "{ending:?}");

        let changed = fixture(&before);
        let ending = nanus_kernel::runtime::block_on(managed_ending(
            &changed.held,
            &after,
            run(completed(&after), None),
            &changed.binding,
        ));
        assert!(matches!(ending, Frame::Failed { .. }), "{ending:?}");
        assert_eq!(
            changed.store.saves.get(),
            0,
            "no fallback to the legacy save"
        );
    }

    /// The status a client is shown is the status the runner reported, field for field, in the
    /// link's vocabulary — codes as their stable names, digests as their hex.
    #[test]
    fn a_status_crosses_in_the_links_vocabulary() {
        let (before, _) = sessions();
        let status = ContextStatus {
            mode: ContextMode::Managed,
            revision: 4,
            frontier: frontier_of(&before),
            estimate_input_tokens: Some(9_000),
            estimate_protected_tokens: Some(1_000),
            output_reserve_tokens: 8_192,
            estimator: String::from("bytes/4"),
            hidden_fragments: 3,
            protected_fragments: 2,
            goal_revision: None,
            goal_data_available: false,
            recall_available: true,
            archive_available: false,
            last_decision: Some(ContextDecision {
                decision_id: String::from("d9"),
                outcome: DecisionOutcome::Rejected,
                revision: None,
                error_code: Some(ErrorCode::StaleBase),
            }),
            profile_digest: Digest::of(b"profile"),
            managed_ready: false,
            unavailable_reason: Some(ErrorCode::ProtocolIncompatible),
        };
        let wire = wire_status(&status);
        assert_eq!(wire.mode, ContextModeState::Managed);
        assert_eq!(wire.revision, 4);
        assert_eq!(wire.frontier.event_count, 2);
        assert_eq!(
            wire.frontier.prefix_sha256,
            status.frontier.prefix_sha256.as_str()
        );
        assert_eq!(wire.hidden_fragments, 3);
        assert_eq!(wire.profile_digest, Digest::of(b"profile").as_str());
        assert_eq!(
            wire.unavailable_reason.as_deref(),
            Some("protocol_incompatible")
        );
        let decision = wire.last_decision.unwrap_or_else(|| panic!("a decision"));
        assert_eq!(decision.outcome, DecisionState::Rejected);
        assert_eq!(decision.error_code.as_deref(), Some("stale_base"));
        let legacy = wire_status(&ContextStatus {
            mode: ContextMode::Legacy,
            ..status
        });
        assert_eq!(legacy.mode, ContextModeState::Legacy);
    }

    /// F4: a reconciliation is trusted only for a file that is byte for byte a prefix of the
    /// held session; an equal event count over different records is quarantine, not durable.
    #[test]
    fn a_reconciled_file_must_be_a_prefix_of_the_held_session() {
        let mut held = Session::new(SessionId::new("s"), 0, "/w");
        held.upgrade_to_managed_body();
        held.append(SessionEvent::UserMessage { text: "a".into() });
        held.append(SessionEvent::TurnEnd {
            turn: 1,
            reason: TurnEndReason::Interrupted,
        });
        let mut disk = Session::new(SessionId::new("s"), 0, "/w");
        disk.upgrade_to_managed_body();
        disk.append(SessionEvent::UserMessage { text: "a".into() });
        disk.append(SessionEvent::UserMessage { text: "b".into() });
        let stored = |session: &Session| ExpectedCheckpoint::Stored {
            body_version: 3,
            file_sha256: session.prefix_digest(2).expect("encodes"),
            event_count: 2,
        };
        assert!(
            receipt_from(&stored(&disk), &held).is_none(),
            "same count, other records"
        );
        let receipt = receipt_from(&stored(&held), &held).expect("the held prefix is on disk");
        assert_eq!(receipt.frontier.event_count, 2);
    }
}
