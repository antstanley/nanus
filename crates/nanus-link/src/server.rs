//! The serving half of the link: an agent answering on a local socket.
//!
//! ## What the agent holds
//!
//! An agent owns *sessions*, and a connection is a view of one. That is the whole of the
//! model, and it is what makes a conversation outlive the terminal it was started in:
//!
//! - A session is opened once — created by `Request::New`, or loaded by
//!   `Request::Attach` — and the agent keeps holding it after the client that opened it
//!   goes away.
//! - Every client attached to a session sees the same frames. A turn runs **once**, in
//!   its own task, and broadcasts to whoever is watching, so a second terminal can watch
//!   a long turn or pick it up where the first left off.
//! - A turn outlives the client that asked for it, because the session is the thing
//!   doing the work. Closing a terminal mid-turn no longer abandons the turn.
//!
//! ## The borrow, and why it is safe
//!
//! A turn needs `&mut Session` for its whole duration, which is many awaits long. The
//! session therefore lives in a `RefCell` that exactly one turn borrows at a time, and
//! `Held::busy` is what enforces "one at a time": a prompt to a busy session is refused
//! rather than queued. Everything a *listing* or an *attachment* needs — the title, the
//! event count, the name, whether it is busy, how many are watching — is cached beside
//! the session rather than read out of it, precisely so that no other path ever has to
//! borrow the session while a turn holds it.
//!
//! ## Why the writer is a task
//!
//! [`Progress`] is synchronous, because the agent loop must not await a client in the
//! middle of assembling a step. Writing a frame to a socket is asynchronous. So each
//! connection has a bounded queue and one task draining it into its socket. Progress
//! frames are dropped when a client falls behind — a transcript missing a delta can be
//! re-read from the store, and an interface that cannot keep up must not slow the work
//! down — while the *ending* waits for room, because a client that never learns a turn
//! finished would wait forever.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use nanus_bundle::compose::new_session;
use nanus_bundle::{AgentRunner, Approver, Harness, Progress};
use nanus_domain::{
    ApprovalOutcome, ApprovalPolicy, ApprovalRequest, Session, SessionId, ToolCallId, ToolName,
    TurnEndReason, Usage,
};
use nanus_ports::{ClockHandle, StoreHandle};
use tokio::io::BufReader;
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Notify, mpsc, oneshot};
use tokio::task::JoinSet;

use crate::error::{LinkError, LinkResult};
use crate::protocol::{
    AgentInfo, ApprovalState, EffortState, Frame, Request, SessionInfo, TurnEnd,
};
use crate::wire::{read_request, write_frame};

/// How many frames may be queued to one client before progress is dropped.
const FRAME_BUFFER: usize = 256;

/// How long the end of a turn waits for room in a client's queue.
///
/// Long enough for a client that is merely busy drawing, short enough that a client
/// which has stopped reading cannot hold a session busy for ever.
const ENDING_TIMEOUT: Duration = Duration::from_secs(2);

/// How many sessions an agent holds open before it lets an idle one go.
///
/// A service runs for weeks, and every session it holds is a conversation in memory.
/// The bound yields to the work rather than the other way round: a session that is busy
/// or has a client attached is never let go, even if that means holding more than this.
pub const MAX_HELD_SESSIONS: usize = 32;

/// An agent a link can serve.
///
/// The three things a served turn needs are a runner, somewhere to record what it did,
/// and a workspace to record it against; the models it offers and its tool count are what
/// the agent says about itself when a client asks. The model *in use* is not here: it lives
/// in the runner, which is the object that names it in a request, and [`Agent::model`] reads
/// it there rather than from a copy that could drift.
pub struct Agent {
    runner: Rc<AgentRunner>,
    store: StoreHandle,
    clock: ClockHandle,
    workspace: PathBuf,
    models: Vec<String>,
    tools: usize,
}

/// The parts an [`Agent`] is built from.
///
/// A named struct rather than a long argument list, because every field is a distinct
/// decision and a call site that gets two of them the wrong way round should be a
/// compile error rather than a silently mismatched link.
pub struct Parts {
    /// The loop that runs turns.
    pub runner: Rc<AgentRunner>,
    /// Where finished turns are recorded, and where names live.
    pub store: StoreHandle,
    /// The clock a new session is stamped from.
    pub clock: ClockHandle,
    /// The workspace the tools are confined to.
    pub workspace: PathBuf,
    /// The model ids a client may switch the runner between.
    ///
    /// The composition's list, not the adapter's: which models are offered is a decision of
    /// the deployment, and the agent is what refuses an id nobody offers. The model actually
    /// in use is read from the runner rather than carried here, because the runner is what
    /// issues the request — two copies of that string would be two answers to one question.
    pub models: Vec<String>,
    /// How many tools the runner exposes.
    pub tools: usize,
}

impl Agent {
    /// Wraps a composed harness.
    #[must_use]
    pub fn new(harness: &Harness, workspace: impl Into<PathBuf>) -> Self {
        Self::from_parts(Parts {
            runner: Rc::clone(&harness.runner),
            store: harness.store.clone(),
            clock: harness.clock.clone(),
            workspace: workspace.into(),
            models: harness.models().to_vec(),
            tools: harness.tool_count(),
        })
    }

    /// Builds an agent from its parts.
    ///
    /// The ordinary path is [`Agent::new`], which takes them from a harness. This exists
    /// because an agent *is* those parts and nothing more, and a test that scripts its
    /// model has no adapters to compose a harness from.
    #[must_use]
    pub fn from_parts(parts: Parts) -> Self {
        // A composition that named no models still offers the one it is using, so the first
        // switch is never one-way. Enforced here rather than at the call sites, which is what
        // makes it true of every agent however it was built.
        let mut models = parts.models;
        let current = parts.runner.model();
        if !models.contains(&current) {
            models.insert(0, current);
        }
        Self {
            runner: parts.runner,
            store: parts.store,
            clock: parts.clock,
            workspace: parts.workspace,
            models,
            tools: parts.tools,
        }
    }

    /// Returns the workspace sessions are created against.
    #[must_use]
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// Returns the runner that executes turns.
    #[must_use]
    pub const fn runner(&self) -> &Rc<AgentRunner> {
        &self.runner
    }

    /// Returns the model id the agent's next request will name.
    ///
    /// Read from the runner rather than kept beside it: a client may switch the model, and the
    /// runner is the object that issues the request, so a copy here would be a second answer to
    /// a question that has one.
    #[must_use]
    pub fn model(&self) -> String {
        self.runner.model()
    }

    /// Returns the model ids a client may switch to, the one in use first.
    #[must_use]
    pub fn models(&self) -> &[String] {
        &self.models
    }

    /// Returns the reasoning effort the agent's next request will carry.
    ///
    /// Read from the runner, which is what fills a request in, and translated into the link's
    /// vocabulary here rather than on the wire: `None` means the adapter has no notion of
    /// effort, which is reported as the absence it is.
    #[must_use]
    pub fn effort(&self) -> Option<EffortState> {
        self.runner.effort().map(wire_effort)
    }

    /// Describes the agent itself.
    fn info(&self) -> AgentInfo {
        AgentInfo {
            workspace: self.workspace.display().to_string(),
            model: self.model(),
            models: self.models.clone(),
            effort: self.effort(),
            tools: self.tools,
            version: crate::protocol::PROTOCOL_VERSION,
        }
    }

    /// Starts a session for a new conversation.
    fn start_session(&self) -> Session {
        new_session(&self.clock, &self.workspace)
    }

    /// Records a session.
    ///
    /// # Errors
    ///
    /// Returns [`LinkError::Agent`] when the store refuses the write.
    async fn record(&self, session: &Session) -> LinkResult<()> {
        self.store
            .save(session)
            .await
            .map_err(|error| LinkError::agent(error.to_string()))
    }
}

impl core::fmt::Debug for Agent {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Agent")
            .field("model", &self.model())
            .field("models", &self.models)
            .field("tools", &self.tools)
            .field("workspace", &self.workspace)
            .finish_non_exhaustive()
    }
}

/// The parts of a session a listing shows.
///
/// Cached beside the session rather than read from it, because a turn borrows the session
/// for its whole duration and nothing else may borrow it while that is true.
#[derive(Clone, Debug, Default)]
struct Headline {
    /// A title derived from the first human turn.
    title: Option<String>,
    /// How many events the session holds.
    events: u64,
}

/// An approval question this session is waiting on.
///
/// The tool is kept beside the sender because an answer may be *standing*: a client that
/// says "always" is granting that tool for the session, and which tool it was is not
/// recoverable from the question's id.
struct Question {
    /// The tool the question is about.
    tool: ToolName,
    /// Why the harness is asking, in its own words.
    ///
    /// Kept because the question may be *re-sent*: a client that attaches while the turn
    /// waits on this question is shown it, and a question replayed without its reason would
    /// ask a person to decide on less than the first client was told.
    reason: Option<String>,
    /// How the answer reaches the turn.
    sender: oneshot::Sender<ApprovalOutcome>,
}

/// A session claimed for writing, released when the agent lets the session go.
///
/// Dropping it releases the claim, which is why it is a field of [`Held`] rather than something
/// a caller is expected to remember: every path that lets a session go — an idle eviction, a
/// failed hold, a shutdown — drops the entry, and a claim that outlived its holder would refuse
/// the next writer for as long as this process runs.
struct Claim {
    /// The store the claim lives in.
    store: StoreHandle,
    /// The session it is about.
    id: SessionId,
    /// Whether this claim is the one to release.
    ///
    /// A claim that lost a race to hold the same session is *disarmed* rather than released,
    /// because a claim is one file per session and the loser's release would remove the
    /// winner's. A claim records a process, not a claimant.
    armed: Cell<bool>,
}

impl Claim {
    /// Marks the claim as one this process no longer needs to release.
    fn disarm(&self) {
        self.armed.set(false);
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        if self.armed.get() {
            self.store.release_lock(&self.id);
        }
    }
}

/// A session an agent is holding open.
struct Held {
    /// The store key, kept here so a listing never has to borrow the session for it.
    id: SessionId,
    /// The conversation.
    session: RefCell<Session>,
    /// The name a user gave it, if any.
    ///
    /// A cache of what the store says, not a fact: a rename is written straight to the store
    /// by a command that knows nothing about the agent holding the session, so every report
    /// the agent makes about a session refreshes this first — see [`Registry::refresh_name`].
    name: RefCell<Option<String>>,
    /// What a listing shows about it.
    headline: RefCell<Headline>,
    /// One queue per attached client, with the id that connection unsubscribes by.
    viewers: RefCell<Vec<(u64, mpsc::Sender<Frame>)>>,
    /// Whether a turn is running. One at a time, because a turn owns the session.
    busy: Cell<bool>,
    /// Whether the running turn has been asked to stop.
    ///
    /// A flag the turn reads rather than a notification it waits on, because there is
    /// nothing for a queued signal to say that the flag does not: the turn looks between
    /// steps and between the tokens of a response, and either it has been asked to stop or
    /// it has not. Cleared when a turn starts, so a request that arrived a moment after the
    /// last turn ended cannot stop the next one before it begins.
    stop: Cell<bool>,
    /// The claim this agent holds on the session, for as long as it holds the session.
    ///
    /// `None` only where no store is involved at all: the unit tests build a bare `Held` to
    /// exercise the bookkeeping of who is watching, and a `Registry` — which is the only thing
    /// that makes one — always claims first.
    claim: Option<Claim>,
    /// The frames of the turn in progress, so a client that attaches in the middle of one
    /// can be shown the whole turn rather than its tail.
    ///
    /// This is exactly the part of the turn the store does not have yet, and no more: the
    /// log is written when a turn ends, so a turn that is running is the one thing a client
    /// cannot read anywhere else. Cleared the moment the turn is saved. Adjacent deltas are
    /// folded together as they arrive, which bounds this by the number of *segments* a turn
    /// has rather than by the number of tokens it streamed.
    turn: RefCell<Vec<Frame>>,
    /// Approval questions this session is waiting on, keyed by the id sent to clients.
    ///
    /// The sender is how a client's answer reaches the turn: `Request::Approve` looks the
    /// id up here and delivers the decision. Dropping the map — which happens when the
    /// session is let go, and when a turn ends with a question still open — is what makes
    /// a question nobody answers a *denial* rather than a hang: the approver's receiver
    /// ends and it reports `Unavailable`.
    approvals: RefCell<BTreeMap<String, Question>>,
    /// Tools a client has granted for this session with an *always* answer.
    ///
    /// Session state rather than agent state, because the grant was given about this
    /// conversation: a second session on the same agent is a different person's trust. It
    /// lives as long as the held session, which is what "for this session" means.
    approved: RefCell<BTreeSet<ToolName>>,
    /// The next approval question's id, so ids stay unique even across turns.
    approval_seq: Cell<u64>,
    /// When it was last used, for letting an idle session go.
    touched: Cell<u64>,
}

impl Held {
    /// Describes the session for a client.
    fn info(&self) -> SessionInfo {
        let headline = self.headline.borrow();
        SessionInfo {
            session: self.id.as_str().to_owned(),
            name: self.name.borrow().clone(),
            title: headline.title.clone(),
            events: headline.events,
            busy: self.busy.get(),
            viewers: self.viewers.borrow().len(),
        }
    }

    /// Replaces the cached name.
    ///
    /// The store is the authority on what a session is called, so this is called with what
    /// the store answered rather than with what a caller believes: a name read from disk that
    /// has since been removed clears the cache, which is what makes a rename visible here.
    fn set_name(&self, name: Option<String>) {
        *self.name.borrow_mut() = name;
    }

    /// Returns whether `viewer` is still being sent this session's frames.
    fn is_watching(&self, viewer: u64) -> bool {
        self.viewers.borrow().iter().any(|(id, _)| *id == viewer)
    }

    /// Detaches a client.
    ///
    /// When the last one goes, any open approval question goes with it: nobody is left to
    /// answer it, and the waiting turn is denied rather than left waiting for a client that
    /// has closed the window.
    fn unview(&self, viewer: u64) {
        self.viewers.borrow_mut().retain(|(id, _)| *id != viewer);
        if self.viewers.borrow().is_empty() {
            self.abandon_approvals();
        }
    }

    /// Refreshes the cached headline from the session.
    fn refresh(&self, session: &Session) {
        *self.headline.borrow_mut() = Headline {
            title: session.title(),
            events: u64::try_from(session.event_count()).unwrap_or(u64::MAX),
        };
    }

    /// Records one frame of the turn in progress.
    ///
    /// Two adjacent deltas of the same kind are one piece of text however they were
    /// divided, so they are folded here: a client replays the same answer either way, and
    /// this keeps a long turn from being thousands of frames in memory. Everything else is
    /// pushed as it came, because the order of a turn is the turn.
    fn seen(&self, frame: &Frame) {
        let mut turn = self.turn.borrow_mut();
        match (turn.last_mut(), frame) {
            (Some(Frame::Text { delta: held }), Frame::Text { delta: more }) => held.push_str(more),
            (Some(Frame::Reasoning { delta: held }), Frame::Reasoning { delta: more }) => {
                held.push_str(more);
            }
            _ => turn.push(frame.clone()),
        }
    }

    /// Returns the frames of the turn in progress, oldest first.
    fn turn_frames(&self) -> Vec<Frame> {
        self.turn.borrow().clone()
    }

    /// Notes that the session has been written down.
    ///
    /// Called immediately after a *successful* save and never after a failed one: what this
    /// holds is what the store does not have, so a turn the store failed to record is
    /// exactly what a later client still needs to be caught up with.
    fn saved(&self) {
        self.turn.borrow_mut().clear();
    }

    /// Returns a question frame for every approval this session is still waiting on.
    ///
    /// Questions are state rather than history. One that has been answered is over and must
    /// not be asked again; one that is still open is blocking the turn, so a client arriving
    /// now is shown it — and can answer it, because the answer is looked up by id here.
    fn open_questions(&self) -> Vec<Frame> {
        self.approvals
            .borrow()
            .iter()
            .map(|(call_id, question)| Frame::Approval {
                call_id: call_id.clone(),
                tool: question.tool.as_str().to_owned(),
                reason: question.reason.clone(),
            })
            .collect()
    }

    /// Opens an approval question and returns the id a client answers with.
    fn ask(
        &self,
        tool: ToolName,
        reason: Option<String>,
        sender: oneshot::Sender<ApprovalOutcome>,
    ) -> String {
        let next = self.approval_seq.get().saturating_add(1);
        self.approval_seq.set(next);
        let call_id = format!("a{next}");
        self.approvals.borrow_mut().insert(
            call_id.clone(),
            Question {
                tool,
                reason,
                sender,
            },
        );
        call_id
    }

    /// Returns whether a tool was granted for this session with an *always* answer.
    fn is_approved(&self, tool: &ToolName) -> bool {
        self.approved.borrow().contains(tool)
    }

    /// Delivers a client's answer, if the question is still open.
    ///
    /// Returns whether it was delivered: the first answer wins, and an answer to a question
    /// another client already settled — or one the turn abandoned — is dropped rather than
    /// reported, because by then there is nothing left to decide.
    ///
    /// An `always` answer also records the tool, which is what makes the next call to it run
    /// without asking for the rest of the session. Recording happens even if the receiver is
    /// gone: the grant is about the session, not about the one call the question named, and
    /// a turn that ended a moment before the answer arrived should still leave the session
    /// remembering what a person granted.
    fn answer(&self, call_id: &str, allow: bool, always: bool) -> bool {
        let Some(question) = self.approvals.borrow_mut().remove(call_id) else {
            return false;
        };
        if allow && always {
            self.approved.borrow_mut().insert(question.tool);
        }
        let outcome = if allow {
            ApprovalOutcome::AllowedOnce
        } else {
            ApprovalOutcome::Rejected
        };
        question.sender.send(outcome).is_ok()
    }

    /// Abandons every question still open.
    ///
    /// Called when a turn ends, so a late answer cannot be mistaken for a decision about the
    /// next turn's call: the sender is dropped, the waiting approver sees `Unavailable` and
    /// the call it was about is denied.
    fn abandon_approvals(&self) {
        self.approvals.borrow_mut().clear();
    }
}

/// Every session an agent is holding, and the turns running in them.
struct Registry {
    /// The agent every session belongs to.
    agent: Rc<Agent>,
    /// What this agent calls itself in a session claim.
    ///
    /// A word for a person rather than for a program — the socket it is reachable on — because
    /// the sentence a refused writer reads has to tell them what to do next, and a pid does not.
    owner: String,
    /// The held sessions, by store key, so a listing is ordered by creation.
    held: RefCell<BTreeMap<SessionId, Rc<Held>>>,
    /// Turns still running, so a shutdown can stop them.
    turns: RefCell<JoinSet<()>>,
    /// A monotonic stamp, for least-recently-used order.
    tick: Cell<u64>,
    /// The next viewer id, which is how a connection unsubscribes.
    viewer: Cell<u64>,
}

impl Registry {
    /// Wraps an agent, which will claim the sessions it holds as `owner`.
    fn new(agent: Rc<Agent>, owner: String) -> Self {
        Self {
            agent,
            owner,
            held: RefCell::new(BTreeMap::new()),
            turns: RefCell::new(JoinSet::new()),
            tick: Cell::new(0),
            viewer: Cell::new(0),
        }
    }

    /// Returns the next monotonic stamp.
    fn stamp(&self) -> u64 {
        let next = self.tick.get().saturating_add(1);
        self.tick.set(next);
        next
    }

    /// Returns the next viewer id.
    fn next_viewer(&self) -> u64 {
        let next = self.viewer.get().saturating_add(1);
        self.viewer.set(next);
        next
    }

    /// Holds a session open, letting idle ones go if the agent holds too many.
    ///
    /// Claiming is what makes a second agent refuse to hold the same conversation rather than
    /// overwrite it, and it happens *before* anything else here: a session this agent cannot
    /// claim must not evict an idle one on its way to failing.
    ///
    /// # Errors
    ///
    /// Returns a message when another live process is writing the session.
    async fn hold(&self, session: Session, name: Option<String>) -> Result<Rc<Held>, String> {
        let id = session.id().clone();
        // Already held: the session is this agent's, and re-claiming it would be claiming it
        // from itself. Answered before the map is touched, so the common case costs nothing.
        if let Some(existing) = self.get(&id) {
            return Ok(existing);
        }
        self.agent
            .store
            .lock(&id, &self.owner)
            .await
            .map_err(|error| error.to_string())?;
        // Room is made *before* the newcomer is in the map, so that the session being
        // opened can never be the one let go. A client would otherwise be handed a
        // conversation the agent no longer held: nothing else could attach to it, a
        // second client would load a second copy of it, and the two would overwrite each
        // other's log.
        self.evict_idle(1);
        let entry = Rc::new(Held {
            id: id.clone(),
            claim: Some(Claim {
                store: Rc::clone(&self.agent.store),
                id: id.clone(),
                armed: Cell::new(true),
            }),
            headline: RefCell::new(Headline::default()),
            session: RefCell::new(session),
            name: RefCell::new(name),
            turn: RefCell::new(Vec::new()),
            viewers: RefCell::new(Vec::new()),
            busy: Cell::new(false),
            stop: Cell::new(false),
            approvals: RefCell::new(BTreeMap::new()),
            approved: RefCell::new(BTreeSet::new()),
            approval_seq: Cell::new(0),
            touched: Cell::new(self.stamp()),
        });
        entry.refresh(&entry.session.borrow());
        let mut held = self.held.borrow_mut();
        // The map is the authority, not this call. Two connections can reach here with the
        // same session — `open_reference` awaits the store between its check and its insert,
        // and both connections run on the same set — and inserting over the first would
        // leave two holders for one session: the loser's client subscribed to a session
        // nothing else can see, `busy` no longer gating either copy, and both recording the
        // same store key over each other.
        if let Some(existing) = held.get(&id) {
            // Another connection won the race to hold it: both waited on the store between the
            // check above and here. This entry is dropped rather than kept, and its claim is
            // disarmed first — the file it would remove is the *winner's* claim on the very same
            // session, because a claim records a process rather than a holder.
            if let Some(claim) = &entry.claim {
                claim.disarm();
            }
            return Ok(Rc::clone(existing));
        }
        held.insert(id, Rc::clone(&entry));
        drop(held);
        Ok(entry)
    }

    /// Returns the held session with `id`, if it is held.
    fn get(&self, id: &SessionId) -> Option<Rc<Held>> {
        self.held.borrow().get(id).map(Rc::clone)
    }

    /// Re-reads a held session's name from the store.
    ///
    /// A name is a file beside the log, and `nanus sessions name` writes it without telling
    /// the agent — there is nothing to tell it through: the command does not connect to the
    /// link. So the cached name is a *probably* rather than a fact, and everything the agent
    /// *reports* about a held session refreshes it first: a listing, and the attachment that
    /// tells a client which conversation it is in.
    ///
    /// A name that cannot be read leaves the cache alone rather than clearing it: an
    /// unreadable alias is worth a line in the log, not a session that appears nameless until
    /// the next successful read.
    async fn refresh_name(&self, held: &Rc<Held>) {
        match self.agent.store.name_of(&held.id).await {
            Ok(name) => held.set_name(name),
            Err(error) => tracing::warn!(%error, "a session's name could not be read"),
        }
    }

    /// Finds the session a reference names, loading it from the store if it is not held.
    ///
    /// A name is resolved by the store and never by matching the agent's cached copy of one:
    /// the store is where a name lives, and a cache that a rename has not reached yet would
    /// otherwise *join the wrong session* — `--resume` on a name that has moved on would open
    /// the conversation it used to belong to. An id the agent is holding is answered without
    /// the store, because an id is a store key rather than an alias.
    ///
    /// # Errors
    ///
    /// Returns a message when the store cannot be read, or nothing answers to the
    /// reference.
    async fn open_reference(&self, reference: &str) -> Result<Rc<Held>, String> {
        if let Some(found) = self.get(&SessionId::new(reference)) {
            return Ok(found);
        }
        let named = match self.agent.store.resolve(reference).await {
            Ok(named) => named,
            Err(error) => return Err(error.to_string()),
        };
        let id = named.unwrap_or_else(|| SessionId::new(reference));
        if let Some(found) = self.get(&id) {
            return Ok(found);
        }
        let session = self
            .agent
            .store
            .load(&id)
            .await
            .map_err(|error| format!("{error}"))?;
        // What a person calls it, so a session resumed by id still shows the name it was
        // given. A name is a convenience, so an unreadable one is worth a line rather than
        // refusing to open the conversation at all.
        let name = match self.agent.store.name_of(&id).await {
            Ok(name) => name,
            Err(error) => {
                tracing::warn!(%error, "a session's name could not be read");
                None
            }
        };
        // A session this agent is not already holding has to be claimed, and a refusal here is
        // another *agent* writing the same conversation: the reader's answer is to attach to
        // that one rather than to start a second, so the message says so.
        self.hold(session, name).await.map_err(|error| {
            format!(
                "{error}; attach to it with `nanus tui --connect`, or stop the agent that holds it"
            )
        })
    }

    /// Attaches a client to a session and returns the id it unsubscribes by.
    fn view(&self, held: &Rc<Held>, frames: &mpsc::Sender<Frame>) -> u64 {
        let viewer = self.next_viewer();
        held.viewers.borrow_mut().push((viewer, frames.clone()));
        held.touched.set(self.stamp());
        viewer
    }

    /// Describes every held session, most recently used first.
    ///
    /// Names are refreshed from the store first, so a listing shows what a session is called
    /// *now* rather than what it was called when the agent opened it.
    async fn listing(&self) -> Vec<SessionInfo> {
        let sessions: Vec<Rc<Held>> = self.held.borrow().values().map(Rc::clone).collect();
        for held in &sessions {
            self.refresh_name(held).await;
        }
        let mut described: Vec<(u64, SessionInfo)> = sessions
            .iter()
            .map(|entry| (entry.touched.get(), entry.info()))
            .collect();
        described.sort_by_key(|described| std::cmp::Reverse(described.0));
        described.into_iter().map(|(_, info)| info).collect()
    }

    /// Spawns a turn, keeping its handle so a shutdown can stop it.
    fn spawn_turn<F>(&self, task: F)
    where
        F: Future<Output = ()> + 'static,
    {
        let mut turns = self.turns.borrow_mut();
        // Reaped before spawning: a service that has been up for a week has run thousands of
        // turns, and a completed task nobody collects is a slot the set never gives back.
        while turns.try_join_next().is_some() {}
        turns.spawn_local(task);
    }

    /// Tells every attached client what the agent's approval state is now.
    ///
    /// The state is the agent's rather than a session's, so every session's viewers are told;
    /// a client that toggled it and a client watching another conversation then agree about
    /// what the next call will do.
    async fn broadcast_approval(&self, state: ApprovalState) {
        let held: Vec<Rc<Held>> = self.held.borrow().values().map(Rc::clone).collect();
        for entry in held {
            broadcast_awaited(&entry, Frame::ApprovalChanged { state }, None).await;
        }
    }

    /// Tells every attached client which model the agent is using now.
    ///
    /// Sent to every session's viewers for the same reason the approval state is: the model is
    /// the agent's, so a client that switched it and a client watching another conversation
    /// have to agree about it.
    async fn broadcast_model(&self, model: &str) {
        let held: Vec<Rc<Held>> = self.held.borrow().values().map(Rc::clone).collect();
        for entry in held {
            broadcast_awaited(
                &entry,
                Frame::ModelChanged {
                    model: model.to_owned(),
                },
                None,
            )
            .await;
        }
    }

    /// Tells every attached client which reasoning effort the agent is asking for now.
    ///
    /// Sent to every session's viewers for the same reason the approval state and the model are:
    /// it is the agent's, so two clients watching one conversation have to agree about it.
    async fn broadcast_effort(&self, state: EffortState) {
        let held: Vec<Rc<Held>> = self.held.borrow().values().map(Rc::clone).collect();
        for entry in held {
            broadcast_awaited(&entry, Frame::EffortChanged { state }, None).await;
        }
    }

    /// Lets idle sessions go until the agent is holding no more than it should.
    ///
    /// `incoming` is how many sessions the caller is about to add. Counting them before
    /// they exist is what keeps the newcomer out of the candidate set — an eviction
    /// policy that can evict the thing it was called to make room for is worse than one
    /// that overshoots its bound.
    fn evict_idle(&self, incoming: usize) {
        loop {
            if self.held.borrow().len().saturating_add(incoming) <= MAX_HELD_SESSIONS {
                return;
            }
            let candidate = {
                let held = self.held.borrow();
                held.values()
                    .filter(|entry| !entry.busy.get() && entry.viewers.borrow().is_empty())
                    .min_by_key(|entry| entry.touched.get())
                    .map(|entry| entry.id.clone())
            };
            let Some(id) = candidate else {
                // Everything is in use. The bound yields to the work rather than
                // interrupting a turn or dropping a client's conversation.
                return;
            };
            tracing::debug!(session = %id.as_str(), "letting an idle session go");
            self.held.borrow_mut().remove(&id);
        }
    }
}

/// Translates the domain's turn-end reason into the link's own vocabulary.
///
/// The match is exhaustive on purpose: a reason added to the domain has to be a compile
/// error here, because the alternative is a turn that ends for a reason the interface
/// cannot be told about, and an interface that cannot be told draws it as a completed
/// turn. That failure is the one this field exists to prevent.
impl From<&TurnEndReason> for TurnEnd {
    fn from(reason: &TurnEndReason) -> Self {
        match reason {
            TurnEndReason::Completed => Self::Completed,
            TurnEndReason::Aborted { reason } => Self::Aborted {
                reason: reason.clone(),
            },
            TurnEndReason::Blocked => Self::Blocked,
            TurnEndReason::Error { message } => Self::Error {
                message: message.clone(),
            },
            TurnEndReason::MaxTokens => Self::MaxTokens,
            TurnEndReason::MaxSteps => Self::MaxSteps,
            TurnEndReason::Interrupted => Self::Interrupted,
        }
    }
}

/// Renders the domain's approval policy in the link's vocabulary.
///
/// An exhaustive match, for the same reason [`From<&TurnEndReason> for TurnEnd`] is one: a
/// state the domain grows has to be a compile error here rather than a state the interface
/// silently never shows.
const fn wire_state(policy: ApprovalPolicy) -> ApprovalState {
    match policy {
        ApprovalPolicy::PerCall => ApprovalState::PerCall,
        ApprovalPolicy::Permitted => ApprovalState::Permitted,
        ApprovalPolicy::AllCalls => ApprovalState::AllCalls,
    }
}

/// Renders a reasoning effort in the link's vocabulary.
///
/// An exhaustive match, so a step the ports scale grows cannot quietly fail to cross.
const fn wire_effort(effort: nanus_ports::ReasoningEffort) -> EffortState {
    match effort {
        nanus_ports::ReasoningEffort::Minimal => EffortState::Minimal,
        nanus_ports::ReasoningEffort::Low => EffortState::Low,
        nanus_ports::ReasoningEffort::Medium => EffortState::Medium,
        nanus_ports::ReasoningEffort::High => EffortState::High,
    }
}

/// Reads the link's effort into the ports vocabulary.
const fn domain_effort(state: EffortState) -> nanus_ports::ReasoningEffort {
    match state {
        EffortState::Minimal => nanus_ports::ReasoningEffort::Minimal,
        EffortState::Low => nanus_ports::ReasoningEffort::Low,
        EffortState::Medium => nanus_ports::ReasoningEffort::Medium,
        EffortState::High => nanus_ports::ReasoningEffort::High,
    }
}

/// Reads the link's approval state into the domain's vocabulary.
///
/// An exhaustive match, for the same reason [`wire_state`] is one: a state the link grows has
/// to be a compile error here rather than a state the agent silently never applies.
const fn domain_policy(state: ApprovalState) -> ApprovalPolicy {
    match state {
        ApprovalState::PerCall => ApprovalPolicy::PerCall,
        ApprovalState::Permitted => ApprovalPolicy::Permitted,
        ApprovalState::AllCalls => ApprovalPolicy::AllCalls,
    }
}

/// Asks the clients attached to a session to approve one call.
///
/// The link's answerer, and the reason the approval gate means something when a person is
/// watching: the question goes to every client attached to the session as a
/// [`Frame::Approval`], and the turn waits for the first [`Request::Approve`] that names it.
///
/// Nobody attached is [`ApprovalOutcome::Unavailable`], which the loop treats as a denial —
/// a service with no client, or a turn nobody is watching, fails closed rather than
/// proceeding on an answer that never came.
struct LinkApprover<'a> {
    /// The session whose viewers are asked.
    held: &'a Held,
}

impl Approver for LinkApprover<'_> {
    fn decide(&self, request: ApprovalRequest) -> nanus_ports::LocalBoxFuture<'_, ApprovalOutcome> {
        Box::pin(async move {
            // A tool already granted for this session is not a question any more: an
            // "always" answer recorded it, and asking again would be asking a person the
            // same thing they have already answered once.
            if self.held.is_approved(&request.tool) {
                return ApprovalOutcome::AllowedOnce;
            }
            // Asked before the question is queued, so a turn with nobody watching does not
            // put a frame nowhere and wait for an answer that cannot arrive.
            if self.held.viewers.borrow().is_empty() {
                return ApprovalOutcome::Unavailable;
            }
            let (sender, receiver) = oneshot::channel();
            let call_id = self
                .held
                .ask(request.tool.clone(), request.reason.clone(), sender);
            broadcast_awaited(
                self.held,
                Frame::Approval {
                    call_id: call_id.clone(),
                    tool: request.tool.as_str().to_owned(),
                    reason: request.reason.clone(),
                },
                None,
            )
            .await;
            // A question that reached nobody is one that cannot be answered, and without
            // this check the turn would wait for a client the broadcast just detached.
            if self.held.viewers.borrow().is_empty() {
                self.held.approvals.borrow_mut().remove(&call_id);
                return ApprovalOutcome::Unavailable;
            }
            // A closed receiver means the sender was dropped: the session was let go, or
            // the turn abandoned its questions. Either way nobody decided.
            let outcome = receiver.await.unwrap_or(ApprovalOutcome::Unavailable);
            self.held.approvals.borrow_mut().remove(&call_id);
            outcome
        })
    }
}

/// Runs one turn in a held session and tells everyone watching.
///
/// The session is borrowed for the whole turn, which is why exactly one turn may run at a
/// time in it and why everything a listing shows is cached outside.
// The borrow is the design rather than an oversight: a turn needs `&mut`, and `Held::busy`
// is what keeps a second one from starting while it holds. Everything a listing or an
// attachment needs is cached outside the borrow, so no other path can reach it.
#[allow(clippy::await_holding_refcell_ref)]
async fn run_turn(agent: &Agent, held: &Rc<Held>, text: String) {
    let mut session = held.session.borrow_mut();
    let outcome = {
        // Scoped rather than dropped: the bridge borrows the session only for as long as
        // the turn runs, so a frame it queues cannot overtake the ending.
        let mut progress = Broadcast {
            held,
            started: None,
            head: None,
            first_token: None,
            last_token: None,
        };
        // The approver borrows the session too — for its viewers rather than its log — so it
        // is built here and lives exactly as long as the turn that may ask through it.
        let approver = LinkApprover { held };
        agent
            .runner()
            .run_turn(&mut session, &text, &mut progress, Some(&approver))
            .await
    };
    // A question still open when the turn ended belongs to a turn that is over. Abandoning
    // it drops the sender, so a late answer cannot be read as a decision about the next
    // turn's call, and a waiting approver — none can be waiting here — would be denied.
    held.abandon_approvals();

    let ending = match outcome {
        // The reason travels with the ending. A turn that closed at its step budget is
        // not a completed turn, and only the reason says so: without it the interface
        // draws the last thing the model happened to say as though it were an answer.
        Ok(result) => match agent.record(&session).await {
            // Cleared with no await in between, so the backlog and the log cannot both be
            // behind or ahead: a client attaching at any instant sees the turn either in the
            // store or in the backlog, and never in both and never in neither.
            Ok(()) => {
                held.saved();
                Frame::Done {
                    answer: result.answer,
                    reason: TurnEnd::from(&result.reason),
                }
            }
            Err(error) => Frame::Failed {
                message: format!("the session could not be recorded: {error}"),
            },
        },
        // A turn that failed is still worth recording: what the model said before it
        // failed is what the next attempt has to work from, and a conversation that
        // silently forgot its own failure would repeat it.
        Err(error) => {
            match agent.record(&session).await {
                // Cleared here for the same reason and in the same place: a turn that failed
                // is still written down, and once it is, the log has it.
                Ok(()) => held.saved(),
                Err(recorded) => tracing::warn!(%recorded, "a failed turn could not be recorded"),
            }
            Frame::Failed {
                message: error.to_string(),
            }
        }
    };

    // Settled before the ending goes out, so a client that sees the ending and then asks
    // what is running is told the truth.
    held.refresh(&session);
    drop(session);
    held.busy.set(false);
    broadcast_end(held, ending).await;
}

/// Forwards the loop's progress to every client attached to a session.
struct Broadcast<'a> {
    /// The session whose viewers are watching.
    held: &'a Held,
    /// When the step now running issued its request, if one is in flight.
    ///
    /// The loop reports a step starting and then a usage record when the request
    /// finishes, and the difference is the request's *active* time: tool calls run after
    /// usage is reported, and the idle between turns never enters it. The instant is
    /// taken here rather than in the loop because the frame that carries the duration is
    /// produced here, which keeps the loop's [`Progress`] contract about what happened
    /// rather than about how long it took.
    started: Option<Instant>,
    /// When the server began answering this step's request, if it announced that it had.
    ///
    /// The far side of the split a reader's wait can be divided at: everything before this is
    /// reaching the server and being answered at all, and everything after it is the server's own
    /// work. `None` from a provider that does not report it, which is a blank rather than a
    /// measurement of no time.
    head: Option<Instant>,
    /// When this step's first generated delta arrived, if any did.
    ///
    /// Taken from every delta callback rather than only from the two that carry something a
    /// reader sees, because a step that answers with a tool call and no prose generates
    /// without calling either of them — and in a coding session that is the ordinary step,
    /// not the exceptional one. Timing only the visible kinds would report the speed of the
    /// steps that happened to talk, which are the long ones.
    first_token: Option<Instant>,
    /// When this step's most recent generated delta arrived.
    ///
    /// The other end of the generation window, and the reason a rate is no longer divided by
    /// the whole request: between the first token and this one the model was generating, and
    /// outside them it was not.
    last_token: Option<Instant>,
}

/// Milliseconds between two instants, saturating rather than failing.
///
/// `as_millis` is a `u128`; a request that ran for longer than a `u64` of milliseconds did
/// not happen, so the conversion saturates rather than failing the frame. The subtraction
/// saturates too, because these instants are taken in an order the callbacks imply but that
/// no signature enforces, and a clock read backwards should report a duration of zero rather
/// than a number near `u64::MAX`.
fn millis_between(from: Instant, to: Instant) -> u64 {
    u64::try_from(to.saturating_duration_since(from).as_millis()).unwrap_or(u64::MAX)
}

impl Broadcast<'_> {
    /// Queues one frame for every attached client.
    ///
    /// A client that has gone is dropped here, and one that has fallen behind loses the
    /// frame: the store holds the conversation, so a missing delta costs a re-read rather
    /// than a fact.
    fn push(&self, frame: &Frame) {
        // Recorded before it is queued, so the backlog holds every frame any viewer was
        // sent: a client that attaches after this one goes out is caught up with it.
        self.held.seen(frame);
        self.held.viewers.borrow_mut().retain(|(_, sender)| {
            !matches!(
                sender.try_send(frame.clone()),
                Err(mpsc::error::TrySendError::Closed(_))
            )
        });
    }

    /// Records that the model generated something, whatever it was.
    ///
    /// The three delta callbacks are one event to a clock: what arrived changes what a reader
    /// sees and nothing about when it arrived. Both ends of the window are written here so
    /// that a step either has a measurable generation window or has none, and never half of
    /// one.
    fn note_token(&mut self) {
        let now = Instant::now();
        if self.first_token.is_none() {
            self.first_token = Some(now);
        }
        self.last_token = Some(now);
    }

    /// Starts the clock for the request a step is about to issue.
    ///
    /// All three instants are cleared together rather than only the one the last usage record
    /// consumed. A request that generated nothing leaves its first and last token unread, and
    /// clearing only `started` would let the next step inherit them and report a generation
    /// window belonging to the step before it — which is a wrong number presented as a
    /// measurement rather than as an absence.
    fn start_request(&mut self) {
        self.started = Some(Instant::now());
        self.head = None;
        self.first_token = None;
        self.last_token = None;
    }
}

impl Progress for Broadcast<'_> {
    fn text(&mut self, delta: &str) {
        self.note_token();
        self.push(&Frame::Text {
            delta: delta.to_owned(),
        });
    }

    fn reasoning(&mut self, delta: &str) {
        self.note_token();
        self.push(&Frame::Reasoning {
            delta: delta.to_owned(),
        });
    }

    fn tool_call(&mut self, _delta: &str) {
        // The delta is the loop's to assemble and nothing here forwards it: a reader is told
        // about a call as a call, once the step has decided to run it. What this observer
        // wants is only that the model generated, which for a tool-call step is the only
        // evidence there is.
        self.note_token();
    }

    fn step_started(&mut self, step: u32) {
        self.start_request();
        self.push(&Frame::Step { step });
    }

    fn response_head(&mut self) {
        // Taken once. A provider that repeated the announcement would otherwise move the
        // boundary later and quietly shorten the part of the wait it is meant to measure.
        if self.head.is_none() {
            self.head = Some(Instant::now());
        }
    }

    fn tool_started(
        &mut self,
        call_id: &ToolCallId,
        name: &ToolName,
        arguments: &serde_json::Value,
    ) {
        // The id travels with the frame so a watcher can pair the call with the `ToolDone`
        // that answers it: the frames of a step's calls all go out before its results, and
        // the results go out in the order the tools finished rather than the order they
        // were asked for.
        self.push(&Frame::Tool {
            call_id: Some(call_id.as_str().to_owned()),
            name: name.as_str().to_owned(),
            arguments: arguments.clone(),
        });
    }

    fn tool_finished(&mut self, call_id: &ToolCallId, name: &ToolName, is_error: bool) {
        self.push(&Frame::ToolDone {
            call_id: Some(call_id.as_str().to_owned()),
            name: name.as_str().to_owned(),
            error: is_error,
        });
    }

    fn cancelled(&self) -> bool {
        self.held.stop.get()
    }

    /// Reports that this step's prompt had part of the conversation dropped.
    ///
    /// Pushed like progress rather than queued like a control frame, and recorded in the backlog
    /// with it: a client that attached after the trim was made is watching the same partial
    /// conversation, so the notice belongs to the turn it happened in.
    fn elided(&mut self, elision: &nanus_domain::Elision) {
        self.push(&Frame::Elided {
            dropped_messages: elision.dropped_messages,
            dropped_turns: elision.dropped_turns,
        });
    }

    fn usage(&mut self, usage: &Usage) {
        let now = Instant::now();
        let started = self.started.take();
        let head = self.head.take();
        let first = self.first_token.take();
        let last = self.last_token.take();
        // Both ends of the window are written together or not at all, so one present without
        // the other would mean a callback took a clock reading this code does not take.
        assert!(
            first.is_some() == last.is_some(),
            "a generation window with only one end is not one this observer records"
        );
        // The request ends at its usage record — except when a generated delta arrived after
        // it, which a provider is free to do and which must not make the request end before
        // its own last token. Taking the later of the two is what keeps the three durations
        // addable.
        let end = last.map_or(now, |last| last.max(now));
        // A usage record with no step before it cannot be timed, and reports zero rather than
        // a duration measured from some other request's start.
        let duration_ms = started.map_or(0, |at| millis_between(at, end));
        // Measured from the step's own start, not from the request: the wait is part of what
        // the reader waits for even though it is not part of generation.
        let (ttft_ms, decode_ms) = match (started, first, last) {
            (Some(at), Some(first), Some(last)) => {
                (millis_between(at, first), millis_between(first, last))
            }
            _ => (0, 0),
        };
        // The part of the wait spent reaching the server and being answered at all, before its
        // own work started. Zero when it was not reported, which is the same "not measured" the
        // other durations use: a response head takes longer than a millisecond to cross a
        // network, so a measured zero is not a thing that happens.
        let head_ms = started
            .zip(head)
            .map_or(0, |(at, head)| millis_between(at, head));
        // A step that generated nothing can still have been answered, so the head is not
        // required to sit inside a first-token wait that never happened — but where both exist,
        // the head precedes the token it is measured against.
        assert!(
            first.is_none() || head_ms <= ttft_ms,
            "a response head {head_ms}ms cannot follow the first token at {ttft_ms}ms"
        );
        // The wait, the generation, and whatever followed the last token are disjoint and
        // cover the request, so they cannot exceed it. Asserted rather than clamped because a
        // rate now divides by one of the parts, and parts that could add up to more than the
        // whole would make the rate a claim about arithmetic instead of about the model.
        assert!(
            ttft_ms.saturating_add(decode_ms) <= duration_ms,
            "a request's {ttft_ms}ms wait plus {decode_ms}ms generation exceeds its {duration_ms}ms"
        );
        self.push(&Frame::Usage {
            tokens: usage.total_tokens(),
            completion_tokens: usage.completion_tokens,
            cache_hit_tokens: usage.cache_hit_tokens,
            cache_miss_tokens: usage.cache_miss_tokens,
            duration_ms,
            reasoning_tokens: usage.reasoning_tokens,
            ttft_ms,
            decode_ms,
            head_ms,
        });
    }
}

/// Sends the end of a turn to every attached client, waiting for room.
///
/// The one frame that is not allowed to be dropped: a client that never learns a turn
/// finished would show it running for ever. A client that does not make room within
/// [`ENDING_TIMEOUT`] is detached instead, because a queue that is not being read is not
/// a client any more.
async fn broadcast_end(held: &Held, frame: Frame) {
    broadcast_awaited(held, frame, None).await;
}

/// Sends a frame every attached client must see, waiting for room.
///
/// The two frames with this treatment are the ending and the prompt that opened the turn.
/// Both are facts a watcher cannot infer from anything else it receives: a client that
/// never learns a turn finished shows it running for ever, and one that never sees the
/// prompt watches an answer to a question it never heard. Progress — deltas, steps, tool
/// names — stays on the droppable path, where a missed frame costs a re-read.
///
/// A client that does not make room within [`ENDING_TIMEOUT`] is detached, and the next
/// request it makes is refused rather than acted on.
async fn broadcast_awaited(held: &Held, frame: Frame, except: Option<u64>) {
    let viewers: Vec<(u64, mpsc::Sender<Frame>)> = held.viewers.borrow().clone();
    let mut stale: Vec<u64> = Vec::new();
    for (viewer, sender) in &viewers {
        if Some(*viewer) == except {
            continue;
        }
        match tokio::time::timeout(ENDING_TIMEOUT, sender.send(frame.clone())).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => stale.push(*viewer),
            Err(_) => {
                tracing::warn!(viewer, "a client did not make room for the end of a turn");
                stale.push(*viewer);
            }
        }
    }
    if !stale.is_empty() {
        held.viewers
            .borrow_mut()
            .retain(|(viewer, _)| !stale.contains(viewer));
    }
}

/// Binds a listener at `path`, creating the run directory if it is missing.
///
/// A socket file left behind by a process that died cannot be bound over, and cannot be
/// told from a live one by looking at it, so the only honest test is to ask it. The
/// permissions are narrowed to the owner: a socket that any local user can connect to is
/// a socket that any local user can drive an agent through.
///
/// # Errors
///
/// Returns [`LinkError::Io`] when the directory cannot be created, the bind fails, or
/// the permissions cannot be set.
pub async fn bind(path: &Path) -> LinkResult<UnixListener> {
    let Some(parent) = path.parent() else {
        return Err(LinkError::protocol(format!(
            "{} has no directory to bind in",
            path.display()
        )));
    };
    create_run_dir(parent)?;
    if path.exists() && UnixStream::connect(path).await.is_err() {
        let removed = tokio::fs::remove_file(path).await;
        if let Err(error) = removed {
            tracing::debug!(%error, "a stale socket could not be cleared");
        }
    }
    let listener = UnixListener::bind(path)?;
    restrict(path)?;
    Ok(listener)
}

/// Creates the run directory with owner-only permissions.
fn create_run_dir(path: &Path) -> LinkResult<()> {
    if path.is_dir() {
        return Ok(());
    }
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    builder.mode(0o700);
    builder.create(path)?;
    Ok(())
}

/// Narrows a socket's permissions to its owner.
fn restrict(path: &Path) -> LinkResult<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

/// Serves connections until `stop` resolves or a client asks the agent to stop.
///
/// Must be driven inside a local task set: the sessions, the turns, and the connections
/// are all local tasks, because the agent's state is `Rc`-shared and its futures are
/// therefore not `Send`.
///
/// # Errors
///
/// Returns [`LinkError::Io`] when accepting fails.
pub async fn serve(
    listener: UnixListener,
    agent: Rc<Agent>,
    stop: impl Future<Output = ()>,
) -> LinkResult<()> {
    // What this agent calls itself in the claim it takes on each session it holds: the socket it
    // answers on, because the sentence a refused writer reads should say *where* the conversation
    // is being written and what to do about it, and a pid does not.
    let owner = listener
        .local_addr()
        .ok()
        .and_then(|socket| socket.as_pathname().map(Path::to_path_buf))
        .map_or_else(
            || format!("a nanus agent (pid {})", std::process::id()),
            |socket| format!("nanus at {}", socket.display()),
        );
    let registry = Rc::new(Registry::new(agent, owner));
    let shutdown = Rc::new(Notify::new());
    let mut connections: JoinSet<()> = JoinSet::new();
    tokio::pin!(stop);
    loop {
        tokio::select! {
            () = &mut stop => break,
            () = shutdown.notified() => break,
            joined = connections.join_next(), if !connections.is_empty() => {
                if let Some(Err(error)) = joined {
                    tracing::debug!(%error, "a link connection task ended abnormally");
                }
            }
            accepted = listener.accept() => {
                let (stream, _address) = accepted?;
                let registry = Rc::clone(&registry);
                let shutdown = Rc::clone(&shutdown);
                connections.spawn_local(async move {
                    if let Err(error) = serve_connection(stream, registry, &shutdown).await {
                        // A client that hung up mid-frame is ordinary; anything else is
                        // worth a line, and neither is worth stopping the agent for.
                        tracing::debug!(%error, "a link connection ended");
                    }
                });
            }
        }
    }
    // Aborted rather than awaited: a connection part way through a request would
    // otherwise hold the shutdown open.
    connections.shutdown().await;
    // Turns are the agent's own work rather than a client's, so they are stopped by the
    // agent going away, and stopping means the session they were writing is not recorded.
    let mut turns = std::mem::take(&mut *registry.turns.borrow_mut());
    turns.shutdown().await;
    Ok(())
}

/// Subscribes this connection to a session it asked to open, or says why it could not.
///
/// The two ways a connection acquires a session — `new` and `attach` — differ only in how
/// the session is found, and everything after that is the same: send the attachment, or
/// send the reason it did not happen.
async fn watch_opened(
    registry: &Rc<Registry>,
    frames: &mpsc::Sender<Frame>,
    opened: Result<Rc<Held>, String>,
) -> Option<(u64, Rc<Held>)> {
    let attached = match opened {
        Ok(held) => attach(registry, &held, frames).await,
        Err(message) => Err(message),
    };
    match attached {
        Ok(watching) => Some(watching),
        Err(message) => {
            // Queued on the awaiting path rather than the droppable one: a refusal is the
            // answer to the request, and the client is reading for it.
            send(frames, Frame::Failed { message }).await;
            None
        }
    }
}

/// Tells a connection the session stopped serving that it is no longer following it.
///
/// A connection detached for falling behind is no longer sent frames, so a request from it
/// must not act on the session: a prompt whose every answer goes nowhere, or an interrupt
/// aimed at a turn it cannot see. Saying so is the honest answer, and the connection stays
/// open so that re-attaching is what recovers.
///
/// Returns whether the refusal was sent, which is what the caller acts on.
async fn refuse_if_detached(
    frames: &mpsc::Sender<Frame>,
    watching: Option<&(u64, Rc<Held>)>,
) -> bool {
    let Some((viewer, held)) = watching else {
        return false;
    };
    if held.is_watching(*viewer) {
        return false;
    }
    let message = String::from(
        "this connection fell behind and is no longer following the session; attach again",
    );
    send(frames, Frame::Failed { message }).await;
    true
}

/// Serves one client for as long as its connection lasts.
///
/// # Errors
///
/// Returns an error when the connection cannot be read or its frames cannot be written.
async fn serve_connection(
    stream: UnixStream,
    registry: Rc<Registry>,
    shutdown: &Rc<Notify>,
) -> LinkResult<()> {
    let (read_half, write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let (frames, queued) = mpsc::channel::<Frame>(FRAME_BUFFER);
    let writer = tokio::task::spawn_local(write_frames(write_half, queued));

    send(&frames, Frame::Ready(registry.agent.info())).await;

    // Which session this connection is watching, and the id it watches under.
    let mut watching: Option<(u64, Rc<Held>)> = None;
    while let Some(request) = read_request(&mut reader).await? {
        if refuse_if_detached(&frames, watching.as_ref()).await {
            continue;
        }
        match request {
            Request::New { name } => {
                release(&mut watching);
                let opened = new_session_of(&registry, name).await;
                watching = watch_opened(&registry, &frames, opened).await;
            }
            Request::Attach { session } => {
                release(&mut watching);
                let opened = registry.open_reference(&session).await;
                watching = watch_opened(&registry, &frames, opened).await;
            }
            Request::Prompt { text } => {
                if let Some((viewer, held)) = &watching {
                    start_turn(&registry, held, text, &frames, *viewer).await;
                } else {
                    refuse_unattached(&frames).await;
                }
            }
            Request::Interrupt => {
                if let Some((_, held)) = &watching {
                    // Only the turn that is running is asked, and only if one is: an
                    // interrupt with nothing to interrupt is not an error, it is a client
                    // that pressed the key a moment after the turn ended.
                    if held.busy.get() {
                        held.stop.set(true);
                    }
                } else {
                    refuse_unattached(&frames).await;
                }
            }
            Request::Sessions => list_sessions(&registry, &frames).await,
            Request::Approve {
                call_id,
                allow,
                always,
            } => answer_approval(watching.as_ref(), &call_id, allow, always),
            Request::SetApproval { state } => {
                // The state belongs to the agent rather than to the session: the runner the
                // gate consults is one object shared by every session the agent holds, and a
                // per-session state would need the gate to be handed one on every call.
                // Every viewer is told, not only the connection that changed it, so two
                // views of the same agent cannot disagree about what the next call will do.
                registry.agent.runner().set_approval(domain_policy(state));
                registry.broadcast_approval(state).await;
            }
            Request::SetModel { model } => set_model(&registry, &frames, model).await,
            Request::SetEffort { state } => {
                registry
                    .agent
                    .runner()
                    .set_effort(Some(domain_effort(state)));
                registry.broadcast_effort(state).await;
            }
            Request::Status => send(&frames, Frame::Status(registry.agent.info())).await,
            Request::Shutdown => {
                shutdown.notify_one();
                send(&frames, Frame::Bye).await;
                break;
            }
        }
    }

    // Unsubscribed before the queue is dropped, so the session stops holding a sender
    // that can no longer be read.
    release(&mut watching);
    drop(frames);
    match writer.await {
        Ok(outcome) => outcome,
        Err(error) => Err(LinkError::agent(format!(
            "the link writer did not finish: {error}"
        ))),
    }
}

/// Delivers an approval answer to the question it names.
///
/// Only against the session this connection is watching: an answer is about a question that
/// session asked, and a connection that is not attached has not been asked anything. An id
/// nobody is waiting on is dropped rather than refused — the first answer has already settled
/// it, or the turn ended — and saying so would only be noise on a client's screen.
fn answer_approval(watching: Option<&(u64, Rc<Held>)>, call_id: &str, allow: bool, always: bool) {
    let delivered = watching.is_some_and(|(_, held)| held.answer(call_id, allow, always));
    if !delivered {
        tracing::debug!(call = %call_id, "an approval answer matched no open question");
    }
}

/// Answers a listing of the sessions the agent is holding.
///
/// A function rather than a few lines in the request loop, because reading the names back from
/// the store is an await and the loop is already at its complexity ceiling: the refreshing
/// belongs to the listing anyway, and a listing is one thing to answer.
async fn list_sessions(registry: &Rc<Registry>, frames: &mpsc::Sender<Frame>) {
    let held = registry.listing().await;
    send(frames, Frame::Sessions { held }).await;
}

/// Refuses a request that needs a session on a connection that has not attached to one.
///
/// Both requests that need a session say the same sentence, because it is the same mistake:
/// a client that prompts or interrupts before `new` or `attach` has asked about a conversation
/// it has not chosen yet.
async fn refuse_unattached(frames: &mpsc::Sender<Frame>) {
    let message =
        String::from("this connection is not attached to a session; send `new` or `attach` first");
    send(frames, Frame::Failed { message }).await;
}

/// Replaces the agent's model, or refuses an id it does not offer.
///
/// Validated here rather than accepted and forwarded: a retired or mistyped id would reach the
/// provider as a request rather than as a diagnosis, and the client would have been told
/// nothing it can act on. The refusal names the ids that do exist, the way an unknown slash
/// command names the commands that do.
async fn set_model(registry: &Rc<Registry>, frames: &mpsc::Sender<Frame>, model: String) {
    if !registry.agent.models().contains(&model) {
        let message = format!(
            "no such model: {model} — this agent offers {}",
            registry.agent.models().join(", ")
        );
        send(frames, Frame::Failed { message }).await;
        return;
    }
    // The model belongs to the agent rather than to the session, exactly as the approval state
    // does: one runner serves every session the agent holds. Every viewer is told, not only the
    // connection that asked, so two views of one agent cannot disagree about which model is
    // answering.
    registry.agent.runner().set_model(&model);
    registry.broadcast_model(&model).await;
}

/// Detaches a connection from the session it was watching.
///
/// Called both when the connection ends and *before* it attaches to another session, and
/// the second call site is the one that is easy to forget. Letting a stale viewer stand
/// does two wrong things at once: the session being left keeps queueing its frames into a
/// client that is now watching something else — which would put one conversation's words
/// in another's transcript — and it can never be let go while that viewer counts as
/// attached, so an idle session is pinned in memory for the life of the agent.
fn release(watching: &mut Option<(u64, Rc<Held>)>) {
    if let Some((viewer, held)) = watching.take() {
        held.unview(viewer);
    }
}

/// Attaches a connection to a session and tells it which one it got.
///
/// The order matters twice over, and both halves rest on the same fact — finding a
/// session the agent is already holding does not await, so the executor cannot run a turn
/// between these statements:
///
/// 1. The viewer is registered *before* the reply is queued, so a turn that is already
///    running cannot have a frame queued past a viewer that is not there yet.
/// 2. The reply is queued before the connection task can yield, so no frame of that turn
///    can arrive ahead of the attachment that explains it.
///
/// # Errors
///
/// Returns a message when the connection cannot be told what it was attached to. The two
/// frames of an attachment are queued without waiting, because a wait inside the region would
/// let a live frame in ahead of them — but a client whose queue is already full has not been
/// reading, and the registration is taken back rather than left standing: a viewer that is
/// registered and unanswered would receive the turn while waiting for a reply that was
/// dropped, which is a hang with no diagnosis. The refusal travels the awaiting path, which
/// the client's own reading makes room for.
async fn attach(
    registry: &Rc<Registry>,
    held: &Rc<Held>,
    frames: &mpsc::Sender<Frame>,
) -> Result<(u64, Rc<Held>), String> {
    // A name another process may have changed since this session was opened, read before the
    // region below rather than inside it. Awaiting *here* is safe for catching up — a frame
    // produced during it is recorded in the backlog and this viewer is not registered yet, so
    // it is caught up with rather than missed — but an await inside the region would not be.
    registry.refresh_name(held).await;
    // One synchronous region: the backlog is snapshotted, the reply that explains it and the
    // batch itself are queued, and the viewer is registered — with no await anywhere in
    // between. That is what makes catching up exact rather than nearly exact:
    //
    //   * every frame the turn produced before this instant is in the backlog,
    //   * every frame it produces after goes to the registered viewer,
    //   * and the batch is queued before any live frame can be, so the turn is drawn in
    //     order rather than from its middle.
    //
    // An await in here would open a window where a live frame could be queued ahead of the
    // backlog — the newest delta drawn before the text it continues.
    let backlog = held.turn_frames();
    let viewer = registry.view(held, frames);
    if !queue_now(frames, Frame::Attached(held.info())) {
        held.unview(viewer);
        return Err(String::from(
            "this connection has fallen too far behind to be attached; reconnect and attach again",
        ));
    }
    if !backlog.is_empty() && !queue_now(frames, Frame::Backlog { frames: backlog }) {
        held.unview(viewer);
        return Err(String::from(
            "this connection has fallen too far behind to catch up with the running turn; \
             reconnect and attach again",
        ));
    }
    // A question the turn is waiting on is state rather than history: a client that arrived
    // after it went out is shown it here, and can answer it, because the answer is matched
    // by id against the session's open questions.
    for question in held.open_questions() {
        send(frames, question).await;
    }
    // The state follows the attachment, so an interface knows what the toggle is showing
    // before a reader can press the key. Sent after `Attached` and not before, because the
    // client reads frames until the attachment and would discard one that arrived first.
    let state = wire_state(registry.agent.runner().approval());
    send(frames, Frame::ApprovalChanged { state }).await;
    // The model follows for the same reason: it is the agent's rather than the session's, it
    // can be switched by any client, and a reader should see which one is answering before
    // they type rather than after the first answer.
    let model = registry.agent.model();
    send(frames, Frame::ModelChanged { model }).await;
    // The effort follows, and only when the adapter has one: a frame carrying "no notion of
    // effort" would need a second shape to say so, and the handshake already said it.
    if let Some(state) = registry.agent.effort() {
        send(frames, Frame::EffortChanged { state }).await;
    }
    Ok((viewer, Rc::clone(held)))
}

/// Starts a session for a client, named if it was asked for by name.
///
/// The caller attaches; this only creates, and it creates *nothing* when the name is
/// taken, so a refused name cannot leave an orphan conversation behind.
async fn new_session_of(registry: &Rc<Registry>, name: Option<String>) -> Result<Rc<Held>, String> {
    // A name that is taken is refused *before* a session exists, so a refused name
    // cannot leave an orphan conversation behind.
    if let Some(asked) = &name
        && let Some(existing) = registry
            .agent
            .store
            .resolve(asked)
            .await
            .map_err(|error| error.to_string())?
    {
        return Err(format!(
            "the name {asked:?} already belongs to session {}",
            existing.as_str()
        ));
    }
    let session = registry.agent.start_session();
    let id = session.id().clone();
    if name.is_some() {
        // A named session is written down immediately, and *before* it is held, because
        // the name is a promise about a session that has to exist for the promise to mean
        // anything — and because saving the session a turn will later borrow is simpler
        // than saving it through that borrow.
        if let Err(error) = registry.agent.record(&session).await {
            return Err(error.to_string());
        }
    }
    let held = registry.hold(session, name.clone()).await?;
    if let Some(asked) = name
        && let Err(error) = registry.agent.store.name(&id, &asked).await
    {
        registry.held.borrow_mut().remove(&id);
        return Err(error.to_string());
    }
    Ok(held)
}

/// Starts a turn in a session, refusing a second one at the same time.
///
/// The turn runs as its own task so that it belongs to the session rather than to the
/// client that asked: a conversation the agent is holding should not stop halfway
/// because a terminal closed.
async fn start_turn(
    registry: &Rc<Registry>,
    held: &Rc<Held>,
    text: String,
    frames: &mpsc::Sender<Frame>,
    viewer: u64,
) {
    if held.busy.replace(true) {
        let message = String::from(
            "a turn is already running in this session; wait for it to finish or start another session",
        );
        send(frames, Frame::Failed { message }).await;
        return;
    }
    // Cleared only once this call is the one running the turn, and after the refusal above
    // has returned. Clearing it first — which is what this did — meant that a prompt to a
    // busy session *cancelled the interrupt aimed at the turn it was refused by*: press
    // Esc and then Enter in the same terminal and the turn carries on regardless.
    held.stop.set(false);
    // Everyone else is told what was asked, before the turn can queue anything. The
    // client that asked already has its own words on screen and is skipped, which is
    // what keeps the prompt from appearing twice in its transcript.
    // Waited for rather than dropped on a full queue: a watcher that misses the prompt
    // reads an answer to a question it never saw, which the protocol promises cannot
    // happen.
    let asked = Frame::User { text: text.clone() };
    // The prompt is part of the turn, so a client that arrives after it goes out is shown
    // it too: an answer to a question a reader never saw is the conversation unreadable.
    held.seen(&asked);
    broadcast_awaited(held, asked, Some(viewer)).await;
    held.touched.set(registry.stamp());
    // Two handles: one for the task to own, one to hand the task to. Building the future
    // before borrowing the task set is what keeps the move of the first out of the
    // borrow of the second.
    let owner = Rc::clone(registry);
    let held = Rc::clone(held);
    let task = async move { run_turn(&owner.agent, &held, text).await };
    registry.spawn_turn(task);
}

/// Queues one frame without waiting.
///
/// For the two frames of an attachment, which must be queued in a region that cannot yield:
/// the reply that explains the attachment and the batch that follows it. A full queue means a
/// client that has stopped reading, which is not a client any more — and the caller takes the
/// attachment back rather than leaving it half delivered, so the reply is a refusal the client
/// can read rather than a stream it cannot be caught up with.
///
/// Returns whether the frame was queued.
fn queue_now(frames: &mpsc::Sender<Frame>, frame: Frame) -> bool {
    match frames.try_send(frame) {
        Ok(()) => true,
        Err(error) => {
            tracing::debug!(%error, "a frame could not be queued without waiting");
            false
        }
    }
}

/// Queues one frame, waiting rather than dropping when the queue is full.
///
/// Used for control frames and the end of a turn, which must arrive; progress uses
/// [`Broadcast`], which drops instead.
async fn send(frames: &mpsc::Sender<Frame>, frame: Frame) {
    if frames.send(frame).await.is_err() {
        // The writer is gone, which means the client left.
        tracing::trace!("dropping a frame for a client that has gone");
    }
}

/// Drains a connection's frames into its socket.
async fn write_frames(
    mut writer: OwnedWriteHalf,
    mut queued: mpsc::Receiver<Frame>,
) -> LinkResult<()> {
    while let Some(frame) = queued.recv().await {
        write_frame(&mut writer, &frame).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nanus_adapter_local::SystemClock;
    use nanus_adapter_store::JsonlStore;
    use nanus_bundle::AgentRunner;
    use nanus_domain::{AgentConfig, ToolRegistry};
    use nanus_ports::{ChatRequest, LlmPort, LlmStream};

    /// A model that is never asked: these tests drive the server's own bookkeeping.
    struct SilentLlm;

    impl LlmPort for SilentLlm {
        fn model(&self) -> &'static str {
            "silent"
        }

        fn stream_chat(&self, _request: ChatRequest) -> LlmStream {
            Box::pin(futures::stream::empty())
        }
    }

    /// A held session with nobody attached.
    ///
    /// Built here rather than through `Registry::hold` because none of these tests needs a
    /// store: what is under test is who a session is talking to, which is `Held` alone.
    fn held(id: &str) -> Rc<Held> {
        let session = Session::new(SessionId::new(id), 0, "/work");
        Rc::new(Held {
            id: session.id().clone(),
            // No claim: these tests exercise who a session is talking to, and a `Registry` — the
            // only thing that produces a `Held` in service — always claims first.
            claim: None,
            session: RefCell::new(session),
            name: RefCell::new(None),
            headline: RefCell::new(Headline::default()),
            turn: RefCell::new(Vec::new()),
            viewers: RefCell::new(Vec::new()),
            busy: Cell::new(false),
            stop: Cell::new(false),
            approvals: RefCell::new(BTreeMap::new()),
            approved: RefCell::new(BTreeSet::new()),
            approval_seq: Cell::new(0),
            touched: Cell::new(0),
        })
    }

    /// Builds a registry over a store in `dir`, with a model no turn ever reaches.
    async fn registry_over(dir: &Path) -> Rc<Registry> {
        let store = JsonlStore::new(dir.to_path_buf())
            .await
            .expect("the store opens")
            .handle();
        let config = AgentConfig::new(4, 1, "silent", 4096).expect("a valid config");
        let runner = AgentRunner::new(
            Rc::new(Box::new(SilentLlm)),
            nanus_bundle::ToolRegistryHandle::new(ToolRegistry::new()),
            "a test",
            config,
        )
        .expect("a valid runner");
        Rc::new(Registry::new(
            Rc::new(Agent::from_parts(Parts {
                runner: Rc::new(runner),
                store,
                clock: SystemClock::new().handle(),
                workspace: dir.to_path_buf(),
                models: vec![String::from("silent")],
                tools: 0,
            })),
            String::from("a test agent"),
        ))
    }

    /// The backlog is the turn in progress: everything a late client needs, in order, folded
    /// where folding changes nothing, and emptied when the store has the turn instead.
    #[test]
    fn the_turn_in_progress_is_kept_for_a_client_that_has_not_arrived_yet() {
        let session = held("catch-up");
        session.seen(&Frame::User {
            text: "do it".to_owned(),
        });
        session.seen(&Frame::Step { step: 1 });
        session.seen(&Frame::Text {
            delta: "Hel".to_owned(),
        });
        session.seen(&Frame::Text {
            delta: "lo".to_owned(),
        });
        session.seen(&Frame::Reasoning {
            delta: "hmm".to_owned(),
        });
        session.seen(&Frame::Reasoning {
            delta: " more".to_owned(),
        });
        session.seen(&Frame::Text {
            delta: "!".to_owned(),
        });

        let frames = session.turn_frames();
        // Five, not seven: two adjacent deltas of one kind are one piece of text either way,
        // and a non-delta frame between them keeps them apart.
        assert_eq!(frames.len(), 5, "{frames:?}");
        assert!(
            matches!(frames.first(), Some(Frame::User { text }) if text == "do it"),
            "the prompt comes first: {frames:?}"
        );
        assert!(matches!(frames.get(1), Some(Frame::Step { step: 1 })));
        assert!(matches!(frames.get(2), Some(Frame::Text { delta }) if delta == "Hello"));
        assert!(matches!(frames.get(3), Some(Frame::Reasoning { delta }) if delta == "hmm more"));
        assert!(matches!(frames.get(4), Some(Frame::Text { delta }) if delta == "!"));

        // A question the turn is waiting on is state, so a client arriving now is shown it —
        // with the words the first client was given — and can answer it.
        let (sender, _receiver) = oneshot::channel();
        let bash = ToolName::new("bash").unwrap_or_else(|_| panic!("a valid tool name"));
        let reason = String::from("the sandbox mode `read_only` does not permit execute");
        let call_id = session.ask(bash, Some(reason.clone()), sender);
        let questions = session.open_questions();
        assert_eq!(questions.len(), 1, "{questions:?}");
        assert!(
            matches!(
                questions.first(),
                Some(Frame::Approval { call_id: asked, tool, reason: Some(why) })
                    if asked == &call_id && tool == "bash" && why == &reason
            ),
            "{questions:?}"
        );
        // Answered, it is over, and it must not be asked a second time.
        assert!(session.answer(&call_id, true, false));
        assert!(session.open_questions().is_empty());

        // Written down, there is nothing left that the store does not have.
        session.saved();
        assert!(session.turn_frames().is_empty());
    }

    /// A step whose prompt was trimmed tells every client watching, and the notice is part of
    /// the turn they would be caught up with.
    #[tokio::test]
    async fn a_trimmed_prompt_reaches_every_viewer_and_the_backlog() {
        let session = held("trimmed");
        let (frames, mut queued) = mpsc::channel(FRAME_BUFFER);
        session.viewers.borrow_mut().push((1, frames));

        let elision = nanus_domain::Elision {
            dropped_messages: 12,
            dropped_turns: 2,
            kept_tokens: 100,
            budget: 200,
        };
        Broadcast {
            held: &session,
            started: None,
            head: None,
            first_token: None,
            last_token: None,
        }
        .elided(&elision);

        match queued.try_recv() {
            Ok(Frame::Elided {
                dropped_messages,
                dropped_turns,
            }) => {
                assert_eq!(dropped_messages, 12);
                assert_eq!(dropped_turns, 2);
            }
            other => panic!("expected a notice, got {other:?}"),
        }
        // And a client that attaches after the trim is caught up with it: the notice belongs to
        // the turn it happened in.
        assert!(
            matches!(session.turn_frames().first(), Some(Frame::Elided { .. })),
            "{:?}",
            session.turn_frames()
        );
    }

    /// A connection with no room for the reply is refused rather than half attached.
    ///
    /// The attachment's own two frames are queued without waiting, because a wait there would
    /// let a live frame in ahead of them. A client that cannot take them has stopped reading,
    /// and it is *unregistered* rather than left subscribed: a viewer registered and unanswered
    /// would receive the turn while waiting for a reply that was dropped, which is a hang with
    /// nothing to read. The refusal goes out on the awaiting path instead, which the client's
    /// own reading makes room for.
    #[test]
    fn a_connection_with_no_room_for_the_reply_is_refused_rather_than_attached() {
        let dir = tempfile::tempdir().expect("temp dir");
        nanus_kernel::runtime::block_on_local(async move {
            let registry = registry_over(dir.path()).await;
            let session = held("no-room");
            // Room for one frame, already taken: what a client that has fallen behind looks
            // like. The receiver is held rather than drained, so the queue is still full when
            // the attachment tries to answer.
            let (frames, _queued) = mpsc::channel(1);
            frames.try_send(Frame::Bye).expect("the one slot");

            // The refusal must not wait for the room it cannot have: it is returned here, and the
            // reply it becomes travels the awaiting path in `watch_opened`, which the client's
            // own reading makes room for — the path a refused claim already takes, asserted end
            // to end in `nanus-link`'s socket tests.
            let Err(message) = attach(&registry, &session, &frames).await else {
                panic!("a connection with no room cannot be attached");
            };
            assert!(message.contains("too far behind"), "{message}");
            assert!(
                session.viewers.borrow().is_empty(),
                "the registration is taken back rather than left standing"
            );
        });
    }

    #[tokio::test]
    async fn a_request_from_a_viewer_that_was_detached_is_refused() {
        let session = held("refused");
        let (frames, mut queued) = mpsc::channel(FRAME_BUFFER);
        let viewer = 7;
        session.viewers.borrow_mut().push((viewer, frames.clone()));

        // While the viewer is attached a request is acted on, and nothing is said to it.
        assert!(
            !refuse_if_detached(&frames, Some(&(viewer, Rc::clone(&session)))).await,
            "an attached viewer is not refused"
        );
        assert!(
            queued.try_recv().is_err(),
            "an accepted request says nothing"
        );

        // Detached, which is what falling behind the ending does to a connection: the same
        // request must not act on the session, and the client has to be told why rather
        // than watching nothing happen.
        session.unview(viewer);
        assert!(
            refuse_if_detached(&frames, Some(&(viewer, Rc::clone(&session)))).await,
            "a detached viewer is refused"
        );
        match queued.try_recv() {
            Ok(Frame::Failed { message }) => assert!(
                message.contains("attach again"),
                "the refusal says how to recover: {message}"
            ),
            other => panic!("expected a refusal, got {other:?}"),
        }

        // A connection that never attached is not detached, it is unattached: prompting it
        // is a different mistake with a different message.
        assert!(
            !refuse_if_detached(&frames, None).await,
            "a connection with no session is left to the caller"
        );
    }

    /// The ending is the one frame a client cannot be allowed to miss, so a viewer that
    /// never makes room is dropped rather than waited for.
    // Paused time so the test does not wait the real timeout out; the timeout is still the
    // production constant, and it is still what ends the send.
    #[tokio::test(start_paused = true)]
    async fn a_viewer_that_cannot_make_room_for_an_ending_is_detached() {
        let session = held("ending");
        let ending = Frame::Done {
            answer: "done".to_owned(),
            reason: TurnEnd::Completed,
        };

        let (roomy, mut roomy_queue) = mpsc::channel(FRAME_BUFFER);
        session.viewers.borrow_mut().push((1, roomy));

        // A viewer whose connection, and therefore whose writer task, has gone.
        let (gone, receiver) = mpsc::channel(FRAME_BUFFER);
        session.viewers.borrow_mut().push((2, gone));
        drop(receiver);

        // A viewer that is still connected but not reading, with its queue already full.
        let (stalled, _unread) = mpsc::channel(1);
        stalled
            .try_send(Frame::Bye)
            .expect("one frame fits in a queue of one");
        session.viewers.borrow_mut().push((3, stalled));

        broadcast_end(&session, ending).await;

        assert!(
            session.is_watching(1),
            "a viewer that made room stays attached"
        );
        assert!(
            matches!(roomy_queue.try_recv(), Ok(Frame::Done { .. })),
            "and it received the ending"
        );
        assert!(
            !session.is_watching(2),
            "a viewer whose client left is gone"
        );
        assert!(
            !session.is_watching(3),
            "a viewer that never made room is detached"
        );
    }

    /// A prompt is the other frame a watcher cannot be allowed to miss: an answer to a
    /// question it never saw is a conversation that cannot be read. It waits for room, and
    /// the client that asked is skipped because its own words are already on its screen.
    // Paused time, as above: the timeout is the production constant and still fires.
    #[tokio::test(start_paused = true)]
    async fn a_prompt_reaches_every_watcher_or_that_watcher_is_detached() {
        let session = held("prompt");
        let (asker, mut asker_queue) = mpsc::channel(FRAME_BUFFER);
        let (watcher, mut watcher_queue) = mpsc::channel(FRAME_BUFFER);
        let (stalled, _unread) = mpsc::channel(1);
        stalled
            .try_send(Frame::Bye)
            .expect("one frame fits in a queue of one");
        session.viewers.borrow_mut().push((1, asker));
        session.viewers.borrow_mut().push((2, watcher));
        session.viewers.borrow_mut().push((3, stalled));

        broadcast_awaited(
            &session,
            Frame::User {
                text: "hello".to_owned(),
            },
            Some(1),
        )
        .await;

        assert!(
            asker_queue.try_recv().is_err(),
            "the asker is not sent its own prompt back"
        );
        assert!(
            matches!(watcher_queue.try_recv(), Ok(Frame::User { text }) if text == "hello"),
            "a watcher is told what was asked"
        );
        assert!(
            !session.is_watching(3),
            "a watcher that could not be told is detached rather than left guessing"
        );
    }

    /// A question reaches every client attached to the session, and the first answer settles
    /// it: the second client is not asked again, and a later answer changes nothing.
    #[tokio::test]
    async fn a_question_reaches_the_viewers_and_the_first_answer_settles_it() {
        let session = held("approval");
        let (first, mut first_queue) = mpsc::channel(FRAME_BUFFER);
        let (second, mut second_queue) = mpsc::channel(FRAME_BUFFER);
        session.viewers.borrow_mut().push((1, first));
        session.viewers.borrow_mut().push((2, second));

        let approver = LinkApprover { held: &session };
        let name = ToolName::new("bash").unwrap_or_else(|_| panic!("a valid tool name"));
        let request = ApprovalRequest::new(name).with_reason("a reason");
        let answering = async {
            let question = first_queue.try_recv();
            assert!(
                matches!(
                    question,
                    Ok(Frame::Approval { ref tool, ref reason, .. })
                        if tool == "bash" && reason.as_deref() == Some("a reason")
                ),
                "the first viewer is asked which tool: {question:?}"
            );
            let Ok(Frame::Approval { call_id, .. }) = question else {
                return;
            };
            assert!(
                matches!(second_queue.try_recv(), Ok(Frame::Approval { .. })),
                "and so is the second: every client sees the question"
            );
            assert!(
                session.answer(&call_id, true, false),
                "the first answer settles the question"
            );
            // The first answer wins: a second one finds nothing to settle.
            assert!(
                !session.answer(&call_id, false, false),
                "the question is already settled"
            );
        };
        let (outcome, ()) = tokio::join!(approver.decide(request), answering);
        assert_eq!(outcome, ApprovalOutcome::AllowedOnce);
        assert!(
            session.approvals.borrow().is_empty(),
            "no answered question is left waiting"
        );
    }

    /// An `always` answer grants the tool for the session: the next question about the same
    /// tool is not asked, and a different tool still is.
    #[tokio::test]
    async fn an_always_answer_records_the_tool_for_the_session() {
        let session = held("always");
        let (sender, _receiver) = oneshot::channel();
        let bash = ToolName::new("bash").unwrap_or_else(|_| panic!("a valid tool name"));
        let call_id = session.ask(bash.clone(), None, sender);
        assert!(
            session.answer(&call_id, true, true),
            "the answer settles the question"
        );
        assert!(session.is_approved(&bash), "the tool is granted");
        let read = ToolName::new("read").unwrap_or_else(|_| panic!("a valid tool name"));
        assert!(
            !session.is_approved(&read),
            "a different tool was not granted by answering about bash"
        );

        // A subsequent question about the granted tool is answered without asking anyone.
        let approver = LinkApprover { held: &session };
        let outcome = approver.decide(ApprovalRequest::new(bash)).await;
        assert_eq!(outcome, ApprovalOutcome::AllowedOnce);
        assert!(
            session.approvals.borrow().is_empty(),
            "a granted tool opens no question"
        );
    }

    /// `always` on a denial grants nothing: the toggle to remember is only on an allow.
    #[tokio::test]
    async fn an_always_answer_on_a_refusal_records_nothing() {
        let session = held("refused-always");
        let (sender, _receiver) = oneshot::channel();
        let bash = ToolName::new("bash").unwrap_or_else(|_| panic!("a valid tool name"));
        let call_id = session.ask(bash.clone(), None, sender);
        assert!(session.answer(&call_id, false, true));
        assert!(!session.is_approved(&bash), "a refusal grants nothing");
    }

    /// Nobody attached means nobody to answer, and the turn is told so rather than left
    /// waiting for an answer that cannot come. This is the fail-closed direction over the
    /// link.
    #[tokio::test]
    async fn a_question_with_nobody_attached_is_unavailable() {
        let session = held("unattended");
        let approver = LinkApprover { held: &session };
        let name = ToolName::new("bash").unwrap_or_else(|_| panic!("a valid tool name"));
        let outcome = approver.decide(ApprovalRequest::new(name)).await;
        assert_eq!(outcome, ApprovalOutcome::Unavailable);
        assert!(
            session.approvals.borrow().is_empty(),
            "no question is left open for a client that is not there"
        );
    }

    /// An abandoned question cannot be answered afterwards, which is what keeps a late
    /// answer from deciding the next turn's call.
    #[tokio::test]
    async fn an_abandoned_question_cannot_be_answered() {
        let session = held("abandoned");
        let (sender, receiver) = oneshot::channel();
        let name = ToolName::new("bash").unwrap_or_else(|_| panic!("a valid tool name"));
        let call_id = session.ask(name, Some(String::from("a test reason")), sender);
        assert_eq!(session.approvals.borrow().len(), 1, "the question is open");
        session.abandon_approvals();
        assert!(
            !session.answer(&call_id, true, false),
            "the abandoned question is gone"
        );
        assert!(
            receiver.await.is_err(),
            "the waiting approver sees the question go away"
        );
    }

    #[test]
    fn a_finished_turn_is_reaped_before_the_next_one_is_spawned() {
        let dir = tempfile::tempdir().expect("temp dir");
        nanus_kernel::runtime::block_on_local(async move {
            let registry = registry_over(dir.path()).await;

            let (finished, waiter) = oneshot::channel();
            registry.spawn_turn(async move {
                let _ = finished.send(());
            });
            waiter.await.expect("the first turn ran to completion");

            // The spawn happens after the reap, so the task set holds one turn however
            // many have finished. Without the reap this is two, and a service that has
            // been up for a week has one uncollected slot per turn it ever ran.
            let (again, second) = oneshot::channel();
            registry.spawn_turn(async move {
                let _ = again.send(());
            });
            assert_eq!(
                registry.turns.borrow().len(),
                1,
                "the finished turn was reaped before the new one was spawned"
            );
            second.await.expect("the second turn ran to completion");
            assert_eq!(registry.turns.borrow().len(), 1);
        });
    }
}
