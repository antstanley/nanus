//! Maintenance over a held session: a compaction and a handoff, each drafted, previewed and
//! committed.
//!
//! A draft is a model call a person asked for, and it writes nothing a reader would see. It runs in
//! a task of its own, as a turn does, so the connection that asked keeps reading: an interrupt from
//! that client stops the model call the way it stops a turn, and the session's reservation is
//! released when the draft ends. The draft is held on the connection that asked, under an id, until
//! that connection commits it or lets go of its session; a draft that finishes after its connection
//! let go is dropped. A commit is the only thing here that changes a session, and it runs under the
//! same `busy` reservation a turn takes, so a turn and a draft cannot race for the log.

use std::cell::RefCell;
use std::rc::Rc;

use nanus_bundle::{AgentRunner, BundleError, CompactionDraft, HandoffDraft};
use nanus_domain::context::managed::ErrorCode;
use nanus_domain::context::managed::limits::{
    COMPACT_DRAFTS_PER_CONNECTION, HANDOFF_DRAFTS_PER_CONNECTION,
};
use nanus_domain::{Session, SessionId};
use nanus_ports::{LocalBoxFuture, TurnControl, TurnRuntime};
use tokio::sync::mpsc;

use super::context::{self, Binding, Settled};
use super::{Held, Registry, broadcast_awaited, send};
use crate::protocol::{DraftUsage, Frame};

/// A draft a connection holds: the id it was given, the draft, and for a handoff the name asked
/// for.
pub(super) struct Pending<T> {
    /// The id the client names when it commits.
    id: String,
    /// What the commit will write.
    draft: T,
    /// The name the new session will be recorded under, when the handoff asked for one.
    name: Option<String>,
}

// One slot of each kind is what the limits allow, so an `Option` is the whole store: a second
// draft of a kind replaces the first rather than queueing beside it.
const _: () = assert!(HANDOFF_DRAFTS_PER_CONNECTION == 1 && COMPACT_DRAFTS_PER_CONNECTION == 1);

/// The drafts one connection holds, at most one of each kind
/// ([`HANDOFF_DRAFTS_PER_CONNECTION`], [`COMPACT_DRAFTS_PER_CONNECTION`]).
#[derive(Default)]
pub(super) struct Drafts {
    /// The compaction draft, if one is held.
    compact: Option<Pending<CompactionDraft>>,
    /// The handoff draft, if one is held.
    handoff: Option<Pending<HandoffDraft>>,
    /// Moved on every [`Drafts::clear`], so a draft still running when its connection let go of
    /// the session it was drafted from is recognised as stale when it finishes.
    generation: u64,
}

impl Drafts {
    /// Discards every draft: the connection has let go of its session, or is starting over.
    pub(super) fn clear(&mut self) {
        self.compact = None;
        self.handoff = None;
        self.generation = self.generation.wrapping_add(1);
    }

    /// Holds a handoff draft under `id`, as a finished draft does; for tests that commit one
    /// without a model.
    #[cfg(test)]
    pub(super) fn hold_handoff(&mut self, id: &str, draft: HandoffDraft) {
        self.handoff = Some(Pending {
            id: id.to_owned(),
            draft,
            name: None,
        });
    }
}

/// A draft request as the connection read it: who asked, about which session, for what.
pub(super) struct Asked<'a> {
    /// Where the answer is sent.
    pub(super) frames: &'a mpsc::Sender<Frame>,
    /// The session the connection is watching.
    pub(super) held: &'a Rc<Held>,
    /// The viewer id the connection watches under, which is whose interrupt stops the draft.
    pub(super) viewer: u64,
    /// The drafts the connection holds.
    pub(super) drafts: &'a SharedDrafts,
    /// The draft asked for.
    pub(super) request: DraftRequest,
}

/// A session's `busy` reservation, owned rather than borrowed, so it can travel into a task.
///
/// Released when dropped, however the holder ends — its future dropped at shutdown included — so
/// a draft or a commit can never leave a session refusing every turn as though one were running.
pub(super) struct Reservation(Rc<Held>);

impl Reservation {
    /// Reserves `held`, or answers `None` when something else already holds it.
    fn take(held: &Rc<Held>) -> Option<Self> {
        if held.busy.replace(true) {
            None
        } else {
            Some(Self(Rc::clone(held)))
        }
    }

    /// The session reserved.
    pub(super) const fn held(&self) -> &Rc<Held> {
        &self.0
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.0.drafting.set(None);
        self.0.busy.set(false);
    }
}

/// The refusal a request gets when the session is reserved by something else, saying what.
fn busy_refusal(held: &Held, code: ErrorCode, then: &str) -> Frame {
    let running = if held.drafting.get().is_some() {
        "a draft is being made in this session"
    } else {
        "a turn or another change is running in this session"
    };
    refuse_with(code, &format!("{running}; {then} when it is idle"))
}

/// The drafts of one connection, shared with the drafts it has running.
pub(super) type SharedDrafts = Rc<RefCell<Drafts>>;

/// What a draft request asks for.
pub(super) enum DraftRequest {
    /// A compaction of the held legacy session.
    Compact,
    /// A handoff from the held managed session, naming the new session when `name` is given.
    Handoff {
        /// The one-word name the new session will take.
        name: Option<String>,
    },
}

/// The stop a draft obeys: the session's own, which [`super::Request::Interrupt`] sets.
struct StopControl<'a>(&'a Held);

impl TurnControl for StopControl<'_> {
    fn is_cancelled(&self) -> bool {
        self.0.stop.get()
    }

    fn cancelled(&self) -> LocalBoxFuture<'_, ()> {
        Box::pin(async move {
            while !self.0.stop.get() {
                self.0.stop_wake.notified().await;
            }
        })
    }
}

/// A refusal carrying the stable code a maintenance request was refused with.
fn refuse_with(code: ErrorCode, message: &str) -> Frame {
    Frame::Refused {
        message: message.to_owned(),
        code: Some(code.as_str().to_owned()),
    }
}

/// The refusal of a runner error: its code, when it has one, beside its bare sentence.
///
/// The sentence never repeats the code, so a client shows the two side by side and an older client
/// that reads only the sentence reads a sentence.
fn refused(error: &BundleError) -> Frame {
    Frame::Refused {
        message: error.reason(),
        code: error.code().map(|code| code.as_str().to_owned()),
    }
}

/// A refusal with no stable code of its own, such as a name that is taken.
const fn plain(message: String) -> Frame {
    Frame::Refused {
        message,
        code: None,
    }
}

/// The refusal a maintenance request gets on a connection that is not attached to a session.
pub(super) fn unattached(handoff: bool) -> Frame {
    let code = if handoff {
        ErrorCode::HandoffUnavailable
    } else {
        ErrorCode::CompactUnavailable
    };
    refuse_with(
        code,
        "this connection is not attached to a session; send `new` or `attach` first",
    )
}

/// Takes the draft held under `id`, or leaves the slot alone when the id is not the one held.
fn take_matching<T>(slot: &mut Option<Pending<T>>, id: &str) -> Option<Pending<T>> {
    if slot.as_ref().is_some_and(|pending| pending.id == id) {
        slot.take()
    } else {
        None
    }
}

/// The wire shape of a summary request's usage.
const fn draft_usage(usage: nanus_domain::Usage) -> DraftUsage {
    DraftUsage {
        prompt_tokens: usage.prompt_tokens,
        completion_tokens: usage.completion_tokens,
        reasoning_tokens: usage.reasoning_tokens,
        cache_hit_tokens: usage.cache_hit_tokens,
        cache_miss_tokens: usage.cache_miss_tokens,
    }
}

/// Makes the held copy what the store holds of a maintenance write's `candidate`, and tells every
/// viewer the frontier moved, as an idle checkpoint does.
///
/// The binding is what knows how far the write got: a draft whose last record was refused leaves
/// the store at an earlier prefix, and the held copy follows the store rather than the candidate,
/// so the frontier never names an event the disk does not have. When the binding knows of no
/// stored prefix of the candidate, nothing of it is on disk and the held copy is left alone.
/// `except` is a viewer that is leaving the session and is not told.
async fn settle_written(held: &Held, binding: &Binding, candidate: &Session, except: Option<u64>) {
    let Some(receipt) = context::stored_receipt(binding, candidate) else {
        return;
    };
    let Ok(durable) = candidate.prefix(receipt.frontier.event_count) else {
        return;
    };
    held.refresh(&durable);
    *held.session.borrow_mut() = durable;
    held.saved(receipt.frontier.clone());
    context::announce_checkpoint(held, &receipt, except).await;
}

/// Refuses a handoff's name before anything is done for it: the store's name rules, one word, and
/// free.
async fn check_name(registry: &Registry, asked: &str) -> Result<(), Box<Frame>> {
    if let Some(reason) = nanus_ports::session_name_problem(asked) {
        return Err(Box::new(plain(format!(
            "the name {asked:?} cannot name a session: {reason}"
        ))));
    }
    if asked.chars().any(char::is_whitespace) {
        return Err(Box::new(plain(format!(
            "the name {asked:?} is more than one word; a handoff names its session with one"
        ))));
    }
    match registry.agent.store.resolve(asked).await {
        Ok(None) => Ok(()),
        Ok(Some(existing)) => Err(Box::new(plain(format!(
            "the name {asked:?} already belongs to session {}",
            existing.as_str()
        )))),
        Err(error) => Err(Box::new(plain(error.to_string()))),
    }
}

/// Starts a draft in its own task, so the connection that asked keeps reading while it runs.
///
/// The draft of the same kind the connection holds is discarded now, whatever happens to this one.
/// The session is reserved here, before the task exists, and the reservation travels into it: an
/// interrupt read straight after the request then finds the draft it is aimed at rather than an
/// idle session, and no turn can start in between. The answer — the draft, or its refusal — is
/// sent when the task ends; nothing at all is sent for a draft whose connection let go of the
/// session in the meantime, because the connection is now showing a different conversation.
pub(super) async fn spawn_draft(registry: &Rc<Registry>, asked: Asked<'_>) {
    let Asked {
        frames,
        held,
        viewer,
        drafts,
        request,
    } = asked;
    let generation = {
        let mut drafts = drafts.borrow_mut();
        match request {
            DraftRequest::Compact => drafts.compact = None,
            DraftRequest::Handoff { .. } => drafts.handoff = None,
        }
        drafts.generation
    };
    let Some(reservation) = Reservation::take(held) else {
        let refusal = match request {
            DraftRequest::Compact => {
                busy_refusal(held, ErrorCode::CompactUnavailable, "compact it")
            }
            DraftRequest::Handoff { .. } => {
                busy_refusal(held, ErrorCode::HandoffUnavailable, "hand it off")
            }
        };
        send(frames, refusal).await;
        return;
    };
    // Cleared once this draft holds the session, as a turn clears it, so an interrupt aimed at
    // whatever ran before cannot stop this one; set from here on only by this viewer's interrupt.
    held.stop.set(false);
    held.drafting.set(Some(viewer));
    let owner = Rc::clone(registry);
    let frames = frames.clone();
    let drafts = Rc::clone(drafts);
    let task = async move {
        let answer = match request {
            DraftRequest::Compact => compact_draft(&owner, &reservation, &drafts, generation).await,
            DraftRequest::Handoff { name } => {
                handoff_draft(&owner, &reservation, &drafts, generation, name).await
            }
        };
        // The reservation ends before the answer is sent, so a client that acts on the answer
        // at once finds the session free.
        drop(reservation);
        if drafts.borrow().generation != generation {
            return;
        }
        if let Some(frame) = answer {
            send(&frames, frame).await;
        }
    };
    registry.spawn_turn(task);
}

/// The refusal of a draft that was interrupted before its model call began.
fn stopped_before(code: ErrorCode) -> Frame {
    refuse_with(
        code,
        "the draft was interrupted before it began; nothing was drafted",
    )
}

/// Drafts a compaction of the held legacy session, within the reservation the request took.
async fn compact_draft(
    registry: &Registry,
    reservation: &Reservation,
    drafts: &SharedDrafts,
    generation: u64,
) -> Option<Frame> {
    let held = reservation.held();
    if held.stop.get() {
        return Some(stopped_before(ErrorCode::CompactUnavailable));
    }
    let candidate = held.session.borrow().clone();
    let control = StopControl(held);
    let drafted = registry
        .agent
        .runner()
        .draft_compaction_controlled(&candidate, Some(&control))
        .await;
    let draft = match drafted {
        Ok(draft) => draft,
        Err(error) => return Some(refused(&error)),
    };
    let mut drafts = drafts.borrow_mut();
    if drafts.generation != generation {
        return None;
    }
    let draft_id = registry.next_draft_id();
    let frame = Frame::CompactDraft {
        draft_id: draft_id.clone(),
        text: draft.summary.clone(),
        dropped_turns: draft.dropped_turns,
        usage: draft.usage.map(draft_usage),
        boundary_event_count: draft.base_event_count,
    };
    drafts.compact = Some(Pending {
        id: draft_id,
        draft,
        name: None,
    });
    Some(frame)
}

/// Commits the held compaction draft: the cut is saved, then it is shown to every viewer.
///
/// Saved the way an idle change is (`context::record_idle`): a legacy body by the store's save, and
/// a body that still holds managed records — a session managed once and switched back to legacy —
/// through its checkpoint, so the identity the next checkpoint expects is the one this write
/// leaves and an unknown outcome is only ever reconciled. The held session is replaced only once
/// the store holds the cut. The draft is discarded whenever its id matched, whatever the outcome.
pub(super) async fn compact_commit(
    registry: &Rc<Registry>,
    frames: &mpsc::Sender<Frame>,
    held: &Rc<Held>,
    drafts: &SharedDrafts,
    draft_id: &str,
) {
    let taken = take_matching(&mut drafts.borrow_mut().compact, draft_id);
    let Some(pending) = taken else {
        let refusal = refuse_with(
            ErrorCode::StaleDraft,
            "no compaction draft is held under that id",
        );
        send(frames, refusal).await;
        return;
    };
    // A body that holds managed records is bound before it is written, as a turn's is.
    context::prepare(registry, held).await;
    let Some(_reserved) = Reservation::take(held) else {
        let refusal = busy_refusal(held, ErrorCode::CompactUnavailable, "compact it");
        drafts.borrow_mut().compact = Some(pending);
        send(frames, refusal).await;
        return;
    };
    let quarantined = held.quarantine.borrow().clone();
    if let Some(reason) = quarantined {
        send(frames, refuse_with(ErrorCode::CompactUnavailable, &reason)).await;
        return;
    }
    let current = held.session.borrow().clone();
    let next = match registry
        .agent
        .runner()
        .commit_compaction(&current, &pending.draft)
    {
        Ok(next) => next,
        Err(error) => {
            send(frames, refused(&error)).await;
            return;
        }
    };
    if let Err(message) = context::record_idle(registry, held, &next).await {
        // The cut is not on disk. Whether that is known — the store refused the write and the file
        // is intact — or could not be established, which quarantines the session, is the code.
        let code = if held.quarantine.borrow().is_some() {
            ErrorCode::CheckpointUnknown
        } else {
            ErrorCode::CheckpointNotCommitted
        };
        send(frames, refuse_with(code, &message)).await;
        return;
    }
    let boundary_event_count = next.compaction_boundary().unwrap_or(0);
    held.refresh(&next);
    *held.session.borrow_mut() = next;
    let frame = Frame::Compacted {
        boundary_event_count,
        summary: pending.draft.summary,
    };
    broadcast_awaited(held, frame, None).await;
}

/// Drafts a handoff from the held managed session, with the name the new session will take.
///
/// A name that breaks the name rules, is more than one word or is taken is refused before the model
/// is called, inside the reservation the request took, so the name is checked against a session
/// nothing else can move. The attempt records the request writes are the only records a draft
/// leaves behind. A checkpoint whose outcome is unknown quarantines the session, as a mode change's
/// does: the held copy and the disk can no longer be assumed to agree.
async fn handoff_draft(
    registry: &Registry,
    reservation: &Reservation,
    drafts: &SharedDrafts,
    generation: u64,
    name: Option<String>,
) -> Option<Frame> {
    let held = reservation.held();
    if let Some(asked) = &name
        && let Err(refusal) = check_name(registry, asked).await
    {
        return Some(*refusal);
    }
    // A managed body that is not yet bound is bound before a request is admitted, as a turn is.
    context::prepare_reserved(registry, held).await;
    let quarantined = held.quarantine.borrow().clone();
    if let Some(reason) = quarantined {
        return Some(refuse_with(ErrorCode::HandoffUnavailable, &reason));
    }
    if held.stop.get() {
        return Some(stopped_before(ErrorCode::HandoffUnavailable));
    }
    let binding = held.managed.borrow().clone();
    let mut candidate = held.session.borrow().clone();
    let control = StopControl(held);
    let outcome = {
        let runtime = binding
            .as_ref()
            .map_or_else(TurnRuntime::default, |binding| binding.runtime());
        registry
            .agent
            .runner()
            .draft_handoff_controlled(&mut candidate, runtime, Some(&control))
            .await
    };
    if outcome.as_ref().err().and_then(BundleError::code) == Some(ErrorCode::CheckpointUnknown) {
        held.quarantine(String::from(context::QUARANTINED));
    } else if let Some(binding) = &binding {
        // The attempt records a draft writes are kept whatever the outcome, so the held copy
        // follows as far as the store holds them.
        settle_written(held, binding, &candidate, None).await;
    }
    let draft = match outcome {
        Ok(draft) => draft,
        Err(error) => return Some(refused(&error)),
    };
    let mut drafts = drafts.borrow_mut();
    if drafts.generation != generation {
        return None;
    }
    let draft_id = registry.next_draft_id();
    let frame = Frame::HandoffDraft {
        draft_id: draft_id.clone(),
        text: draft.text.clone(),
    };
    drafts.handoff = Some(Pending {
        id: draft_id,
        draft,
        name,
    });
    Some(frame)
}

/// Commits the held handoff draft, in the order the change spec gives, and returns the new session,
/// still reserved so that it cannot be let go before the caller is watching it.
///
/// `viewer` is the connection committing, which leaves the source for the new session. The source
/// is reserved for the whole commit. The new session is created and saved before the
/// back-link is written, so a failure of the back-link leaves a complete session whose seed names
/// where it came from. A failure before the new session is saved creates nothing. A checkpoint
/// whose outcome is unknown is reconciled against the disk, never written again.
///
/// # Errors
///
/// Returns the refusal for the client: a `stale_draft` when the id is not the one held or the
/// source moved, `handoff_unavailable` when the source is busy or quarantined, a name refusal when
/// the name was taken, and the refusal of the seed's checkpoint otherwise.
pub(super) async fn handoff_commit(
    registry: &Rc<Registry>,
    held: &Rc<Held>,
    viewer: u64,
    drafts: &SharedDrafts,
    draft_id: &str,
) -> Result<Reservation, Box<Frame>> {
    let taken = take_matching(&mut drafts.borrow_mut().handoff, draft_id);
    let Some(pending) = taken else {
        return Err(Box::new(refuse_with(
            ErrorCode::StaleDraft,
            "no handoff draft is held under that id",
        )));
    };
    let Some(_reserved) = Reservation::take(held) else {
        let refusal = busy_refusal(held, ErrorCode::HandoffUnavailable, "hand it off");
        drafts.borrow_mut().handoff = Some(pending);
        return Err(Box::new(refusal));
    };
    // A source whose state on disk could not be established is not built on: neither a seed drawn
    // from it nor a back-link written into it could be trusted to describe what the store holds.
    let quarantined = held.quarantine.borrow().clone();
    if let Some(reason) = quarantined {
        return Err(Box::new(refuse_with(
            ErrorCode::HandoffUnavailable,
            &reason,
        )));
    }
    // Step 1: the source still holds the draft's base, and the name is still free.
    let source = held.session.borrow().clone();
    AgentRunner::check_handoff_source(&source, &pending.draft)
        .map_err(|error| Box::new(refused(&error)))?;
    if let Some(asked) = &pending.name {
        check_name(registry, asked).await?;
    }
    // Step 2: the new session, seeded, claimed and saved. Nothing exists if this fails.
    let seeded = create_seeded(registry, &source, &pending.draft).await?;
    let opened = seeded.held();
    let new_id = opened.id.clone();
    // Step 3: the name, when one was asked for. The session exists either way, so a name that
    // cannot be written is reported in the log and by the seed frame, and the session is still
    // handed over.
    if let Some(asked) = &pending.name {
        match registry.agent.store.name(&new_id, asked).await {
            Ok(()) => opened.set_name(Some(asked.clone())),
            Err(error) => tracing::warn!(%error, "a handed-off session could not be named"),
        }
    }
    // Step 4: the back-link, in the source, through the source's own checkpoint.
    let recorded = back_link(registry, held, viewer, &source, &pending.draft, &new_id).await;
    opened.set_back_link(recorded);
    // Step 5: the new session is held, and reserved until the caller has attached to it.
    Ok(seeded)
}

/// Creates, holds and saves the seeded session a handoff commit makes.
///
/// # Errors
///
/// Returns the refusal for the client when the session cannot be seeded or saved; the held entry is
/// let go then, unless whether it was saved could not be established, which quarantines it.
async fn create_seeded(
    registry: &Rc<Registry>,
    source: &Session,
    draft: &HandoffDraft,
) -> Result<Reservation, Box<Frame>> {
    let fresh = registry.agent.start_session();
    let seeded = registry
        .agent
        .runner()
        .seed_session(fresh, source, draft)
        .map_err(|error| Box::new(refused(&error)))?;
    let new_id: SessionId = seeded.id().clone();
    let opened = registry
        .hold(seeded.clone(), None)
        .await
        .map_err(|message| Box::new(plain(message)))?;
    // Reserved from the moment it is held: it has no viewer until the caller attaches, and an
    // idle, unwatched session is what another connection's `hold` lets go to make room.
    let Some(reservation) = Reservation::take(&opened) else {
        return Err(Box::new(refuse_with(
            ErrorCode::HandoffUnavailable,
            "the new session is already in use",
        )));
    };
    context::prepare_reserved(registry, &opened).await;
    let binding = opened.managed.borrow().clone();
    let Some(binding) = binding else {
        registry.held.borrow_mut().remove(&new_id);
        return Err(Box::new(refuse_with(
            ErrorCode::UnsupportedMode,
            "the new session cannot be bound to its store",
        )));
    };
    let saved = registry
        .agent
        .runner()
        .checkpoint_seeded(&seeded, binding.runtime())
        .await;
    let Err(error) = saved else {
        return Ok(reservation);
    };
    if error.code() != Some(ErrorCode::CheckpointUnknown) {
        registry.held.borrow_mut().remove(&new_id);
        return Err(Box::new(refused(&error)));
    }
    match context::settle_uncertain(&binding, &seeded).await {
        Settled::Durable(receipt) => {
            opened.saved(receipt.frontier);
            Ok(reservation)
        }
        Settled::Kept => {
            registry.held.borrow_mut().remove(&new_id);
            Err(Box::new(refused(&error)))
        }
        Settled::Quarantined(reason) => {
            opened.quarantine(reason.clone());
            Err(Box::new(refuse_with(
                ErrorCode::CheckpointUnknown,
                &format!("whether the new session {new_id} was saved is unknown: {reason}"),
            )))
        }
    }
}

/// Records the back-link in the source and says whether the source now holds it.
///
/// An unknown outcome is settled by reading the disk: the candidate is the source with the record,
/// and the source is quarantined when neither it nor the previous file can be established.
async fn back_link(
    registry: &Registry,
    held: &Held,
    viewer: u64,
    source: &Session,
    draft: &HandoffDraft,
    new_id: &SessionId,
) -> bool {
    let binding = held.managed.borrow().clone();
    let Some(binding) = binding else {
        tracing::warn!("a handoff's source is not bound to its store; no back-link was written");
        return false;
    };
    let mut linked = source.clone();
    let recorded = registry
        .agent
        .runner()
        .record_handoff(&mut linked, draft, new_id, binding.runtime())
        .await;
    let Err(error) = recorded else {
        settle_written(held, &binding, &linked, Some(viewer)).await;
        return true;
    };
    tracing::warn!(%error, "a handoff's back-link could not be recorded");
    if error.code() != Some(ErrorCode::CheckpointUnknown) {
        return false;
    }
    let Ok(candidate) = AgentRunner::back_linked(source, draft, new_id) else {
        held.quarantine(String::from(context::QUARANTINED));
        return false;
    };
    match context::settle_uncertain(&binding, &candidate).await {
        Settled::Durable(_) => {
            settle_written(held, &binding, &candidate, Some(viewer)).await;
            true
        }
        Settled::Kept => false,
        Settled::Quarantined(reason) => {
            held.quarantine(reason);
            false
        }
    }
}
