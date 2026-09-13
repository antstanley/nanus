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
use std::collections::BTreeMap;
use std::future::Future;
use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use nanus_bundle::compose::new_session;
use nanus_bundle::{AgentRunner, Harness, Progress};
use nanus_domain::{Session, SessionId, ToolName, TurnEndReason, Usage};
use nanus_ports::{ClockHandle, StoreHandle};
use tokio::io::BufReader;
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinSet;

use crate::error::{LinkError, LinkResult};
use crate::protocol::{AgentInfo, Frame, Request, SessionInfo, TurnEnd};
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
/// and a workspace to record it against; the model and tool count are what the agent
/// says about itself when a client asks.
pub struct Agent {
    runner: Rc<AgentRunner>,
    store: StoreHandle,
    clock: ClockHandle,
    workspace: PathBuf,
    model: String,
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
    /// The model id the runner calls.
    pub model: String,
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
            model: harness.llm.model().to_owned(),
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
        Self {
            runner: parts.runner,
            store: parts.store,
            clock: parts.clock,
            workspace: parts.workspace,
            model: parts.model,
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

    /// Returns the model id the agent calls.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Describes the agent itself.
    fn info(&self) -> AgentInfo {
        AgentInfo {
            workspace: self.workspace.display().to_string(),
            model: self.model.clone(),
            tools: self.tools,
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
            .field("model", &self.model)
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

/// A session an agent is holding open.
struct Held {
    /// The store key, kept here so a listing never has to borrow the session for it.
    id: SessionId,
    /// The conversation.
    session: RefCell<Session>,
    /// The name a user gave it, if any.
    name: Option<String>,
    /// What a listing shows about it.
    headline: RefCell<Headline>,
    /// One queue per attached client, with the id that connection unsubscribes by.
    viewers: RefCell<Vec<(u64, mpsc::Sender<Frame>)>>,
    /// Whether a turn is running. One at a time, because a turn owns the session.
    busy: Cell<bool>,
    /// When it was last used, for letting an idle session go.
    touched: Cell<u64>,
}

impl Held {
    /// Describes the session for a client.
    fn info(&self) -> SessionInfo {
        let headline = self.headline.borrow();
        SessionInfo {
            session: self.id.as_str().to_owned(),
            name: self.name.clone(),
            title: headline.title.clone(),
            events: headline.events,
            busy: self.busy.get(),
            viewers: self.viewers.borrow().len(),
        }
    }

    /// Detaches a client.
    fn unview(&self, viewer: u64) {
        self.viewers.borrow_mut().retain(|(id, _)| *id != viewer);
    }

    /// Refreshes the cached headline from the session.
    fn refresh(&self, session: &Session) {
        *self.headline.borrow_mut() = Headline {
            title: session.title(),
            events: u64::try_from(session.event_count()).unwrap_or(u64::MAX),
        };
    }
}

/// Every session an agent is holding, and the turns running in them.
struct Registry {
    /// The agent every session belongs to.
    agent: Rc<Agent>,
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
    /// Wraps an agent.
    fn new(agent: Rc<Agent>) -> Self {
        Self {
            agent,
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
    fn hold(&self, session: Session, name: Option<String>) -> Rc<Held> {
        let id = session.id().clone();
        // Room is made *before* the newcomer is in the map, so that the session being
        // opened can never be the one let go. A client would otherwise be handed a
        // conversation the agent no longer held: nothing else could attach to it, a
        // second client would load a second copy of it, and the two would overwrite each
        // other's log.
        self.evict_idle(1);
        let entry = Rc::new(Held {
            id: id.clone(),
            headline: RefCell::new(Headline::default()),
            session: RefCell::new(session),
            name,
            viewers: RefCell::new(Vec::new()),
            busy: Cell::new(false),
            touched: Cell::new(self.stamp()),
        });
        entry.refresh(&entry.session.borrow());
        self.held.borrow_mut().insert(id, Rc::clone(&entry));
        entry
    }

    /// Returns the held session with `id`, if it is held.
    fn get(&self, id: &SessionId) -> Option<Rc<Held>> {
        self.held.borrow().get(id).map(Rc::clone)
    }

    /// Finds a held session by name and then by id.
    fn find(&self, reference: &str) -> Option<Rc<Held>> {
        let held = self.held.borrow();
        if let Some(found) = held
            .values()
            .find(|entry| entry.name.as_deref() == Some(reference))
        {
            return Some(Rc::clone(found));
        }
        held.get(&SessionId::new(reference)).map(Rc::clone)
    }

    /// Finds the session a reference names, loading it from the store if it is not held.
    ///
    /// A name is tried before an id, and a session the agent is already holding before one
    /// on disk: attaching to a running conversation should join it rather than load a
    /// stale copy of it.
    ///
    /// # Errors
    ///
    /// Returns a message when the store cannot be read, or nothing answers to the
    /// reference.
    async fn open_reference(&self, reference: &str) -> Result<Rc<Held>, String> {
        if let Some(found) = self.find(reference) {
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
        Ok(self.hold(session, name))
    }

    /// Attaches a client to a session and returns the id it unsubscribes by.
    fn view(&self, held: &Rc<Held>, frames: &mpsc::Sender<Frame>) -> u64 {
        let viewer = self.next_viewer();
        held.viewers.borrow_mut().push((viewer, frames.clone()));
        held.touched.set(self.stamp());
        viewer
    }

    /// Describes every held session, most recently used first.
    fn listing(&self) -> Vec<SessionInfo> {
        let mut described: Vec<(u64, SessionInfo)> = self
            .held
            .borrow()
            .values()
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
        self.turns.borrow_mut().spawn_local(task);
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
        };
        agent
            .runner()
            .run_turn(&mut session, &text, &mut progress)
            .await
    };

    let ending = match outcome {
        // The reason travels with the ending. A turn that closed at its step budget is
        // not a completed turn, and only the reason says so: without it the interface
        // draws the last thing the model happened to say as though it were an answer.
        Ok(result) => match agent.record(&session).await {
            Ok(()) => Frame::Done {
                answer: result.answer,
                reason: TurnEnd::from(&result.reason),
            },
            Err(error) => Frame::Failed {
                message: format!("the session could not be recorded: {error}"),
            },
        },
        // A turn that failed is still worth recording: what the model said before it
        // failed is what the next attempt has to work from, and a conversation that
        // silently forgot its own failure would repeat it.
        Err(error) => {
            if let Err(recorded) = agent.record(&session).await {
                tracing::warn!(%recorded, "a failed turn could not be recorded");
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
}

impl Broadcast<'_> {
    /// Queues one frame for every attached client.
    ///
    /// A client that has gone is dropped here, and one that has fallen behind loses the
    /// frame: the store holds the conversation, so a missing delta costs a re-read rather
    /// than a fact.
    fn push_except(&self, except: u64, frame: &Frame) {
        self.held.viewers.borrow_mut().retain(|(viewer, sender)| {
            if *viewer == except {
                return true;
            }
            !matches!(
                sender.try_send(frame.clone()),
                Err(mpsc::error::TrySendError::Closed(_))
            )
        });
    }

    fn push(&self, frame: &Frame) {
        self.held.viewers.borrow_mut().retain(|(_, sender)| {
            !matches!(
                sender.try_send(frame.clone()),
                Err(mpsc::error::TrySendError::Closed(_))
            )
        });
    }
}

impl Progress for Broadcast<'_> {
    fn text(&mut self, delta: &str) {
        self.push(&Frame::Text {
            delta: delta.to_owned(),
        });
    }

    fn reasoning(&mut self, delta: &str) {
        self.push(&Frame::Reasoning {
            delta: delta.to_owned(),
        });
    }

    fn step_started(&mut self, step: u32) {
        self.started = Some(Instant::now());
        self.push(&Frame::Step { step });
    }

    fn tool_started(&mut self, name: &ToolName, arguments: &serde_json::Value) {
        self.push(&Frame::Tool {
            name: name.as_str().to_owned(),
            arguments: arguments.clone(),
        });
    }

    fn tool_finished(&mut self, name: &ToolName, is_error: bool) {
        self.push(&Frame::ToolDone {
            name: name.as_str().to_owned(),
            error: is_error,
        });
    }

    fn usage(&mut self, usage: &Usage) {
        // The request's active time, taken from the step that issued it. A usage record
        // that arrives with no step before it cannot be timed, and reports zero rather
        // than a duration measured from some other request's start. `as_millis` is a
        // `u128`; a request that ran for longer than a `u64` of milliseconds did not
        // happen, so the conversion saturates rather than failing the frame.
        let duration_ms = self
            .started
            .take()
            .map_or(0, |started| started.elapsed().as_millis());
        self.push(&Frame::Usage {
            tokens: usage.total_tokens(),
            completion_tokens: usage.completion_tokens,
            cache_hit_tokens: usage.cache_hit_tokens,
            cache_miss_tokens: usage.cache_miss_tokens,
            duration_ms: u64::try_from(duration_ms).unwrap_or(u64::MAX),
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
    let viewers: Vec<(u64, mpsc::Sender<Frame>)> = held.viewers.borrow().clone();
    let mut stale: Vec<u64> = Vec::new();
    for (viewer, sender) in &viewers {
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
    let registry = Rc::new(Registry::new(agent));
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
        match request {
            Request::New { name } => {
                release(&mut watching);
                match new_session_of(&registry, name).await {
                    Ok(held) => watching = Some(attach(&registry, &held, &frames).await),
                    Err(message) => send(&frames, Frame::Failed { message }).await,
                }
            }
            Request::Attach { session } => {
                release(&mut watching);
                match registry.open_reference(&session).await {
                    Ok(held) => watching = Some(attach(&registry, &held, &frames).await),
                    Err(message) => send(&frames, Frame::Failed { message }).await,
                }
            }
            Request::Prompt { text } => {
                if let Some((viewer, held)) = &watching {
                    start_turn(&registry, held, text, &frames, *viewer).await;
                } else {
                    let message = String::from(
                        "this connection is not attached to a session; send `new` or `attach` first",
                    );
                    send(&frames, Frame::Failed { message }).await;
                }
            }
            Request::Sessions => {
                let held = registry.listing();
                send(&frames, Frame::Sessions { held }).await;
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
async fn attach(
    registry: &Rc<Registry>,
    held: &Rc<Held>,
    frames: &mpsc::Sender<Frame>,
) -> (u64, Rc<Held>) {
    let viewer = registry.view(held, frames);
    send(frames, Frame::Attached(held.info())).await;
    (viewer, Rc::clone(held))
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
    let held = registry.hold(session, name.clone());
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
    // Everyone else is told what was asked, before the turn can queue anything. The
    // client that asked already has its own words on screen and is skipped, which is
    // what keeps the prompt from appearing twice in its transcript.
    let progress = Broadcast {
        held,
        started: None,
    };
    progress.push_except(viewer, &Frame::User { text: text.clone() });
    held.touched.set(registry.stamp());
    // Two handles: one for the task to own, one to hand the task to. Building the future
    // before borrowing the task set is what keeps the move of the first out of the
    // borrow of the second.
    let owner = Rc::clone(registry);
    let held = Rc::clone(held);
    let task = async move { run_turn(&owner.agent, &held, text).await };
    registry.spawn_turn(task);
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
