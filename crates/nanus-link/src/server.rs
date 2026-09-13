//! The serving half of the link: an agent answering on a local socket.
//!
//! ## One session per connection
//!
//! A connection *is* a conversation. The session is created when the client connects,
//! lives in the task serving that client, and is recorded when the turn it belongs to
//! ends. Nothing is keyed, nothing is looked up, and nothing outlives the client — which
//! is what makes "the agent exits with the interface" and "the service outlives the
//! shell" the same code with two different lifetimes.
//!
//! ## Why the writer is a task and the progress callbacks are not
//!
//! [`Progress`] is synchronous, because the agent loop must not await a client in the
//! middle of assembling a step. Writing a frame to a socket is asynchronous. So the
//! callbacks queue frames on a bounded channel and one task drains it into the socket.
//! The channel is bounded and the callbacks drop rather than block: an interface that
//! cannot keep up must not slow the work down, and the session on disk still holds
//! everything the transcript might be missing.
//!
//! Ordering survives that split for one reason worth stating: every frame of a turn is
//! queued by the task that runs the turn, and a channel delivers in the order it was
//! written to. The ending therefore cannot overtake the deltas that produced it.

use std::future::Future;
use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use nanus_bundle::compose::new_session;
use nanus_bundle::{AgentRunner, Harness, Progress};
use nanus_domain::{Session, ToolName, Usage};
use nanus_ports::{ClockHandle, StoreHandle};
use tokio::io::BufReader;
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinSet;

use crate::error::{LinkError, LinkResult};
use crate::protocol::{AgentInfo, Frame, Request};
use crate::wire::{read_request, write_frame};

/// How many frames may be queued to one client before progress is dropped.
const FRAME_BUFFER: usize = 256;

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
    /// Where finished turns are recorded.
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

    /// Starts a session for one connection.
    fn start_session(&self) -> Session {
        new_session(&self.clock, &self.workspace)
    }

    /// Describes the agent and the session it is serving.
    fn info(&self, session: &Session) -> AgentInfo {
        AgentInfo {
            session: session.id().as_str().to_owned(),
            workspace: self.workspace.display().to_string(),
            model: self.model.clone(),
            tools: self.tools,
        }
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
/// Must be driven inside a local task set: each connection becomes a local task, because
/// the agent's state is `Rc`-shared and its futures are therefore not `Send`.
///
/// # Errors
///
/// Returns [`LinkError::Io`] when accepting fails.
pub async fn serve(
    listener: UnixListener,
    agent: Rc<Agent>,
    stop: impl Future<Output = ()>,
) -> LinkResult<()> {
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
                let agent = Rc::clone(&agent);
                let shutdown = Rc::clone(&shutdown);
                connections.spawn_local(async move {
                    if let Err(error) = serve_connection(stream, agent, &shutdown).await {
                        // A client that hung up mid-frame is ordinary; anything else is
                        // worth a line, and neither is worth stopping the agent for.
                        tracing::debug!(%error, "a link connection ended");
                    }
                });
            }
        }
    }
    // Aborted rather than awaited: a connection part way through a turn would otherwise
    // hold the shutdown open for as long as a model takes to answer.
    connections.shutdown().await;
    Ok(())
}

/// Serves one client for as long as its connection lasts.
///
/// # Errors
///
/// Returns an error when the connection cannot be read or its frames cannot be written.
async fn serve_connection(
    stream: UnixStream,
    agent: Rc<Agent>,
    shutdown: &Rc<Notify>,
) -> LinkResult<()> {
    let (read_half, write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let (frames, queued) = mpsc::channel::<Frame>(FRAME_BUFFER);
    let writer = tokio::task::spawn_local(write_frames(write_half, queued));

    let mut session = agent.start_session();
    send(&frames, Frame::Ready(agent.info(&session))).await;

    while let Some(request) = read_request(&mut reader).await? {
        match request {
            Request::Prompt { text } => {
                run_turn(&agent, &mut session, text, &frames).await;
            }
            Request::Status => {
                send(&frames, Frame::Status(agent.info(&session))).await;
            }
            Request::Shutdown => {
                shutdown.notify_one();
                send(&frames, Frame::Bye).await;
                break;
            }
        }
    }

    // Dropping the last sender is what ends the writer, and awaiting it is what makes
    // "the client saw every frame" true rather than likely.
    drop(frames);
    match writer.await {
        Ok(outcome) => outcome,
        Err(error) => Err(LinkError::agent(format!(
            "the link writer did not finish: {error}"
        ))),
    }
}

/// Runs one turn, streaming its progress and recording the result.
///
/// The session is recorded **before** the ending is sent, which is the same contract
/// `nanus run` keeps with its own stdout: a client that has seen `Done` is holding an
/// answer whose transcript is already on disk. The cost is that the answer waits for a
/// local write; the benefit is that a client which exits the moment it is answered
/// cannot lose the session it just had.
async fn run_turn(
    agent: &Agent,
    session: &mut Session,
    text: String,
    frames: &mpsc::Sender<Frame>,
) {
    let mut progress = LinkProgress {
        frames: frames.clone(),
    };
    let outcome = agent.runner().run_turn(session, &text, &mut progress).await;
    // Dropped before the ending is queued, so the ending cannot be overtaken by a
    // progress frame that was still being written.
    drop(progress);
    match outcome {
        Ok(result) => match agent.record(session).await {
            Ok(()) => {
                send(
                    frames,
                    Frame::Done {
                        answer: result.answer,
                    },
                )
                .await;
            }
            Err(error) => {
                let message = format!("the session could not be recorded: {error}");
                send(frames, Frame::Failed { message }).await;
            }
        },
        Err(error) => {
            send(
                frames,
                Frame::Failed {
                    message: error.to_string(),
                },
            )
            .await;
        }
    }
}

/// Queues one frame, waiting rather than dropping when the queue is full.
///
/// Used for control frames and the end of a turn, which must arrive; progress uses
/// [`LinkProgress`], which drops instead.
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

/// Bridges the agent loop's synchronous progress callbacks onto a connection.
struct LinkProgress {
    frames: mpsc::Sender<Frame>,
}

impl LinkProgress {
    /// Queues one frame, dropping it when the client is behind.
    fn push(&self, frame: Frame) {
        if self.frames.try_send(frame).is_err() {
            tracing::trace!("the client is behind; dropping a progress frame");
        }
    }
}

impl Progress for LinkProgress {
    fn text(&mut self, delta: &str) {
        self.push(Frame::Text {
            delta: delta.to_owned(),
        });
    }

    fn reasoning(&mut self, delta: &str) {
        self.push(Frame::Reasoning {
            delta: delta.to_owned(),
        });
    }

    fn step_started(&mut self, step: u32) {
        self.push(Frame::Step { step });
    }

    fn tool_started(&mut self, name: &ToolName) {
        self.push(Frame::Tool {
            name: name.as_str().to_owned(),
        });
    }

    fn tool_finished(&mut self, name: &ToolName, is_error: bool) {
        self.push(Frame::ToolDone {
            name: name.as_str().to_owned(),
            error: is_error,
        });
    }

    fn usage(&mut self, usage: &Usage) {
        self.push(Frame::Usage {
            tokens: usage.total_tokens(),
        });
    }
}
