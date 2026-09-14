//! The process-group shell adapter: an implementation of
//! [`nanus_ports::ShellPort`].
//!
//! ## The grandchild problem
//!
//! An agent harness runs tools as `sh -c "..."`, so the direct child is a *shell*
//! and every real tool — `cargo`, `make`, a pipeline — is a **grandchild**. On this
//! toolchain `Child::kill()` and `kill_on_drop(true)` kill only the direct child
//! and leave the grandchild running; that was measured, not assumed. A harness
//! with that bug finishes a build while `rustc` keeps running.
//!
//! So every spawn sets `process_group(0)`, which makes the child a group leader
//! with `pgid == pid`, and every kill signals the group with `nix`'s safe
//! `killpg`. Two further traps are handled as they were found:
//!
//! - **`AsyncReadExt::take(n)` hangs on a pipe.** It stops reading, the writer
//!   blocks on a full 64 KiB pipe, and the run never ends. This adapter reads each
//!   pipe to EOF and truncates the *stored* text afterwards, while still counting
//!   every byte.
//! - **An unbounded live channel is unbounded.** A 50 MB producer enqueued
//!   millions of events into an unbounded `mpsc`. The live channel here is a
//!   bounded `tokio::sync::mpsc` fed by `try_send`, which drops chunks rather than
//!   growing without limit.
//!
//! ## What is *not* implemented
//!
//! There is no OS-level filesystem sandbox here. [`SandboxPolicy`] is reported by
//! [`ShellPort::sandbox`] and the *working directory* is checked against it for a
//! confined mode, but a child process is not otherwise restricted. A caller that
//! needs real confinement must supply a sandboxed runner; this adapter does not
//! pretend to provide one.

use std::collections::HashSet;
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use nanus_ports::{
    Captured, LocalBoxFuture, SandboxPolicy, ShellError, ShellEvent, ShellOutcome, ShellPort,
    ShellRequest, ShellResult, ShellStream, ensure_within,
};
use nix::sys::signal::{Signal, kill, killpg};
use nix::unistd::Pid;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt as _};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;

/// Bytes read from a pipe in one syscall.
const READ_CHUNK: usize = 8 * 1024;

/// How long to wait for the pipe pumps to reach EOF after the group is killed.
///
/// Without this bound a grandchild that escaped the group by calling `setsid`
/// would hold the pipe open forever and the run would never return.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// The wall-clock budget applied when a request does not set one.
pub const DEFAULT_TIMEOUT_MS: u64 = 120_000;

/// Depth of the bounded live-output channel.
const STREAM_DEPTH: usize = 256;

/// Reports whether `pid` is alive, using the null signal.
///
/// `kill(pid, 0)` succeeds while the process exists and reports `ESRCH` once it is
/// gone, which is the portable liveness probe the tests use.
#[must_use]
pub fn is_alive(pid: i32) -> bool {
    assert!(pid > 0, "a process id is positive");
    kill(Pid::from_raw(pid), None).is_ok()
}

/// The process groups this adapter has spawned and not yet reaped.
#[derive(Debug, Default)]
struct GroupRegistry {
    /// Live group ids. A set, so insertion is idempotent and its length is the
    /// number of groups shutdown must signal.
    live: HashSet<i32>,
}

impl GroupRegistry {
    /// Records a newly spawned group.
    fn insert(&mut self, pgid: i32) {
        assert!(pgid > 0, "a process group id is positive");
        let inserted = self.live.insert(pgid);
        assert!(inserted, "a live process group is registered exactly once");
    }

    /// Forgets a group, returning whether it was registered.
    ///
    /// `false` is expected after [`LocalShell::kill_all`], which retires groups
    /// eagerly during shutdown.
    fn retire(&mut self, pgid: i32) -> bool {
        self.live.remove(&pgid)
    }

    /// Returns whether `pgid` is registered.
    fn contains(&self, pgid: i32) -> bool {
        self.live.contains(&pgid)
    }

    /// Returns how many groups are live.
    fn len(&self) -> usize {
        self.live.len()
    }

    /// Takes every live group id, leaving the registry empty.
    fn take(&mut self) -> Vec<i32> {
        let mut taken: Vec<i32> = self.live.drain().collect();
        taken.sort_unstable();
        taken
    }
}

/// Loads the registry, recovering from a poisoned lock.
fn lock(registry: &Mutex<GroupRegistry>) -> MutexGuard<'_, GroupRegistry> {
    registry.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Removes a group from the registry when the run that owns it ends.
///
/// The guard makes "live groups returns to zero" hold on every exit path,
/// including the error paths, without a hand-written cleanup call.
#[derive(Debug)]
struct LiveGroup {
    /// The registry to update.
    registry: Arc<Mutex<GroupRegistry>>,
    /// The group this guard owns.
    pgid: i32,
}

impl LiveGroup {
    /// Registers `pgid` and returns a guard that unregisters it on drop.
    fn register(registry: &Arc<Mutex<GroupRegistry>>, pgid: i32) -> Self {
        lock(registry).insert(pgid);
        Self {
            registry: Arc::clone(registry),
            pgid,
        }
    }
}

impl Drop for LiveGroup {
    fn drop(&mut self) {
        let retired = lock(&self.registry).retire(self.pgid);
        tracing::debug!(pgid = self.pgid, retired, "process group left the registry");
    }
}

/// A capture of nothing, used when a pipe was never opened or a pump was lost.
fn empty_captured() -> Captured {
    Captured {
        text: String::new(),
        truncated: false,
        total_bytes: 0,
    }
}

/// Builds a capture from the retained bytes and the total read.
fn captured(stored: &[u8], total: u64) -> Captured {
    let kept = u64::try_from(stored.len()).unwrap_or(u64::MAX);
    assert!(
        total >= kept,
        "the total read cannot be less than what was kept"
    );
    Captured {
        text: String::from_utf8_lossy(stored).into_owned(),
        truncated: total > kept,
        total_bytes: total,
    }
}

/// Appends `bytes` to `stored` without exceeding `cap`.
fn append_capped(stored: &mut Vec<u8>, bytes: &[u8], cap: usize) {
    assert!(cap > 0, "an output cap is at least one byte");
    let room = cap.saturating_sub(stored.len());
    let take = room.min(bytes.len());
    stored.extend_from_slice(bytes.get(..take).unwrap_or_default());
}

/// Offers one chunk to the live channel, dropping it when the channel is full.
fn offer(sender: &mpsc::Sender<ShellEvent>, is_stderr: bool, bytes: &[u8]) {
    let chunk = String::from_utf8_lossy(bytes).into_owned();
    let event = if is_stderr {
        ShellEvent::Stderr { chunk }
    } else {
        ShellEvent::Stdout { chunk }
    };
    if sender.try_send(event).is_err() {
        tracing::debug!(len = bytes.len(), "live output channel full; chunk dropped");
    }
}

/// Reads `reader` to EOF, keeping at most `cap` bytes and reporting the total.
///
/// Reading always continues to EOF: stopping at the cap would block the writer on
/// a full pipe and hang the run, which is the `take(n)` trap.
async fn drain<R>(
    mut reader: R,
    cap: usize,
    is_stderr: bool,
    sink: Option<mpsc::Sender<ShellEvent>>,
) -> Captured
where
    R: AsyncRead + Unpin,
{
    assert!(cap > 0, "an output cap is at least one byte");
    let mut stored: Vec<u8> = Vec::new();
    let mut total: u64 = 0;
    let mut chunk = [0u8; READ_CHUNK];
    loop {
        let read = match reader.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(error) => {
                tracing::debug!(%error, "child stream ended in a read error");
                break;
            }
        };
        total = total.saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
        let bytes = chunk.get(..read).unwrap_or_default();
        append_capped(&mut stored, bytes, cap);
        if let Some(sender) = sink.as_ref() {
            offer(sender, is_stderr, bytes);
        }
    }
    captured(&stored, total)
}

/// Awaits a pump, giving up after [`DRAIN_GRACE`] and aborting it.
async fn join_drain(mut handle: JoinHandle<Captured>) -> Captured {
    tokio::select! {
        joined = &mut handle => joined.unwrap_or_else(|_join| empty_captured()),
        () = tokio::time::sleep(DRAIN_GRACE) => {
            handle.abort();
            tracing::warn!("a pipe pump did not reach EOF; the task was aborted");
            empty_captured()
        }
    }
}

/// Signals a whole process group.
fn signal_group(pgid: i32, signal: Signal) -> ShellResult<()> {
    assert!(pgid > 0, "a process group id is positive");
    killpg(Pid::from_raw(pgid), signal).map_err(|source| ShellError::Spawn {
        program: format!("process group {pgid}"),
        message: format!("could not be signalled: {source}"),
    })
}

/// Spawns `command` in its own process group.
fn spawn_group(
    command: &mut Command,
    program: &str,
    registry: &Arc<Mutex<GroupRegistry>>,
) -> ShellResult<(Child, LiveGroup, i32)> {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // The child becomes a group leader, so `pgid == pid` and one `killpg`
        // reaches every grandchild.
        .process_group(0)
        // A backstop for the direct child only; the group kill is what matters.
        .kill_on_drop(true);
    let child = command.spawn().map_err(|source| ShellError::Spawn {
        program: program.to_owned(),
        message: source.to_string(),
    })?;
    let raw = child.id().ok_or_else(|| ShellError::Spawn {
        program: program.to_owned(),
        message: String::from("the child was reaped before its pid could be read"),
    })?;
    let pgid = i32::try_from(raw).map_err(|_| ShellError::Spawn {
        program: program.to_owned(),
        message: String::from("the child's pid does not fit in an i32"),
    })?;
    assert!(pgid > 0, "a spawned child leads a positive process group");
    let guard = LiveGroup::register(registry, pgid);
    Ok((child, guard, pgid))
}

/// Waits for the child, killing the whole group if the budget elapses.
async fn wait_group(
    child: &mut Child,
    pgid: i32,
    timeout: Option<Duration>,
) -> ShellResult<(ExitStatus, bool)> {
    assert!(pgid > 0, "a process group id is positive");
    let Some(limit) = timeout else {
        let status = child.wait().await.map_err(|source| ShellError::Spawn {
            program: String::from("the child"),
            message: format!("could not be reaped: {source}"),
        })?;
        return Ok((status, false));
    };
    match tokio::time::timeout(limit, child.wait()).await {
        Ok(Ok(status)) => Ok((status, false)),
        Ok(Err(source)) => Err(ShellError::Spawn {
            program: String::from("the child"),
            message: format!("could not be reaped: {source}"),
        }),
        Err(_elapsed) => {
            // Kill the *group*, not the child: the shell is not the process that
            // needs killing.
            signal_group(pgid, Signal::SIGKILL)?;
            // `Child::wait` is cancel-safe, so this second wait reaps the leader
            // and reports the signal death the kill produced.
            let status = child.wait().await.map_err(|source| ShellError::Spawn {
                program: String::from("the child"),
                message: format!("could not be reaped after a timeout: {source}"),
            })?;
            Ok((status, true))
        }
    }
}

/// Splits an exit status into a code and a signal, one of which is present.
fn split_status(status: ExitStatus) -> (Option<i32>, Option<i32>) {
    use std::os::unix::process::ExitStatusExt as _;
    (status.code(), status.signal())
}

/// Runs one command to completion under the process-group discipline.
///
/// `sink` is the live-output channel, when the caller wants one. Chunks are
/// offered with `try_send` and dropped when the channel is full, so a slow
/// consumer can never grow the producer's memory without limit.
async fn run_engine(
    request: ShellRequest,
    registry: Arc<Mutex<GroupRegistry>>,
    sink: Option<mpsc::Sender<ShellEvent>>,
) -> ShellResult<ShellOutcome> {
    assert!(!request.program.is_empty(), "a program to run is named");
    let cap = request.max_output_bytes;
    if cap == 0 {
        return Err(ShellError::InvalidOutputCap { limit: cap });
    }
    let started = Instant::now();
    let mut command = Command::new(&request.program);
    command.args(&request.args);
    if let Some(cwd) = request.cwd.as_ref() {
        command.current_dir(cwd);
    }
    for (key, value) in &request.env {
        command.env(key, value);
    }
    let (mut child, guard, pgid) = spawn_group(&mut command, &request.program, &registry)?;

    if let Some(text) = request.stdin.clone()
        && let Some(mut pipe) = child.stdin.take()
    {
        tokio::spawn(async move {
            if let Err(error) = pipe.write_all(text.as_bytes()).await {
                tracing::debug!(%error, "failed to write the child's standard input");
            }
            // Dropping the pipe closes it, which is what unblocks the child.
        });
    }
    let out_pipe = child.stdout.take();
    let err_pipe = child.stderr.take();
    let out_sink = sink.clone();
    let err_sink = sink.clone();
    let out_handle = out_pipe.map(|pipe| tokio::spawn(drain(pipe, cap, false, out_sink)));
    let err_handle = err_pipe.map(|pipe| tokio::spawn(drain(pipe, cap, true, err_sink)));

    let (status, timed_out) = wait_group(&mut child, pgid, request.timeout).await?;
    let stdout = match out_handle {
        Some(handle) => join_drain(handle).await,
        None => empty_captured(),
    };
    let stderr = match err_handle {
        Some(handle) => join_drain(handle).await,
        None => empty_captured(),
    };
    let elapsed = started.elapsed();
    drop(guard);
    assert!(
        !lock(&registry).contains(pgid),
        "a completed run leaves no live process group"
    );
    // Close the live channel only after the group is retired, so a consumer that
    // observes the end of the stream is guaranteed to see a clean registry.
    drop(sink);
    let (exit_code, signal) = split_status(status);
    Ok(ShellOutcome {
        exit_code,
        signal,
        timed_out,
        duration_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
        stdout,
        stderr,
    })
}

/// A local shell adapter that owns a registry of its live process groups.
///
/// The registry is shared, not owned per run, because shutdown must be able to
/// signal every in-flight group from one call. It sits behind a `Mutex` rather
/// than a `RefCell` so a spawned [`ShellStream`] can outlive the borrow that
/// started it.
#[derive(Debug, Clone)]
pub struct LocalShell {
    /// Every group this adapter has spawned and not yet reaped.
    registry: Arc<Mutex<GroupRegistry>>,
    /// The policy this adapter reports to its callers.
    policy: SandboxPolicy,
    /// The budget applied when a request does not set one.
    timeout_ms: u64,
}

impl LocalShell {
    /// Creates an adapter that enforces `policy`.
    #[must_use]
    pub fn new(policy: SandboxPolicy) -> Self {
        Self {
            registry: Arc::new(Mutex::new(GroupRegistry::default())),
            policy,
            timeout_ms: DEFAULT_TIMEOUT_MS,
        }
    }

    /// Creates an unconfined adapter rooted at `workspace_root`.
    #[must_use]
    pub fn unconfined(workspace_root: impl Into<PathBuf>) -> Self {
        Self::new(SandboxPolicy::danger_full_access(workspace_root))
    }

    /// Replaces the default timeout applied when a request sets none.
    #[must_use]
    pub const fn with_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.timeout_ms = timeout_ms;
        self
    }

    /// Returns the policy this adapter reports.
    #[must_use]
    pub fn policy(&self) -> &SandboxPolicy {
        &self.policy
    }

    /// Shares this adapter as the handle a kernel plugin publishes.
    #[must_use]
    pub fn handle(self) -> nanus_ports::ShellHandle {
        std::rc::Rc::new(Box::new(self))
    }

    /// Returns how many process groups are live right now.
    #[must_use]
    pub fn live_groups(&self) -> usize {
        lock(&self.registry).len()
    }

    /// Rewrites the request's working directory to the absolute path the policy permits.
    ///
    /// It used to *check* the path and throw the answer away, leaving the request's own value
    /// for `Command::current_dir` to resolve. A relative `workdir` — which is what this tool's
    /// schema documents, "relative to the workspace root" — was therefore validated as
    /// `<root>/src` and executed in `<process cwd>/src`, which is a different directory and may
    /// be outside the workspace entirely. Returning the resolved path is the fix: what was
    /// checked is what runs.
    fn confine_cwd(&self, request: ShellRequest) -> ShellResult<ShellRequest> {
        let Some(cwd) = request.cwd.as_ref() else {
            return Ok(request);
        };
        if !self.policy.mode.is_confined() {
            return Ok(request);
        }
        let root = self.policy.workspace_root.clone();
        let resolved = ensure_within(&root, cwd).map_err(|_| ShellError::OutsideWorkspace {
            root,
            path: cwd.clone(),
        })?;
        assert!(resolved.starts_with(&self.policy.workspace_root));
        Ok(ShellRequest {
            cwd: Some(resolved),
            ..request
        })
    }

    /// Returns the effective request, applying the adapter's timeout default.
    fn resolve(&self, mut request: ShellRequest) -> ShellResult<ShellRequest> {
        if request.max_output_bytes == 0 {
            return Err(ShellError::InvalidOutputCap {
                limit: request.max_output_bytes,
            });
        }
        if request.timeout.is_none() && self.timeout_ms > 0 {
            request.timeout = Some(Duration::from_millis(self.timeout_ms));
        }
        Ok(request)
    }

    /// Signals every live process group and returns how many were signalled.
    ///
    /// This is what shutdown calls. Groups are retired eagerly, so a run that is
    /// still unwinding will find its group already gone and simply not re-retire
    /// it.
    fn kill_all_blocking(&self) -> usize {
        let groups = lock(&self.registry).take();
        let mut signalled = 0usize;
        for pgid in groups {
            if signal_group(pgid, Signal::SIGKILL).is_ok() {
                signalled = signalled.saturating_add(1);
            } else {
                tracing::warn!(pgid, "failed to signal a process group during shutdown");
            }
        }
        assert_eq!(
            lock(&self.registry).len(),
            0,
            "kill_all leaves no live group registered"
        );
        signalled
    }
}

impl ShellPort for LocalShell {
    fn run(&self, request: ShellRequest) -> LocalBoxFuture<'_, ShellResult<ShellOutcome>> {
        Box::pin(async move {
            let request = self.resolve(request)?;
            let request = self.confine_cwd(request)?;
            run_engine(request, Arc::clone(&self.registry), None).await
        })
    }

    fn spawn(&self, request: ShellRequest) -> LocalBoxFuture<'_, ShellResult<ShellStream>> {
        Box::pin(async move {
            let request = self.resolve(request)?;
            let request = self.confine_cwd(request)?;
            let (sender, receiver) = mpsc::channel::<ShellEvent>(STREAM_DEPTH);
            let registry = Arc::clone(&self.registry);
            let finisher = sender.clone();
            tokio::spawn(async move {
                let outcome = run_engine(request, registry, Some(sender)).await;
                let event = match outcome {
                    Ok(outcome) => ShellEvent::Exited {
                        exit_code: outcome.exit_code,
                        signal: outcome.signal,
                        duration_ms: outcome.duration_ms,
                        timed_out: outcome.timed_out,
                    },
                    Err(error) => {
                        tracing::warn!(%error, "a spawned run failed before it could start");
                        // The failure is *said* before the stream ends. An `Exited` with no
                        // code and no output is indistinguishable from a process that ran and
                        // printed nothing, so a consumer reading the stream — which has no
                        // other channel for it — could not tell that the command never started.
                        if finisher
                            .send(ShellEvent::Stderr {
                                chunk: format!("{error}\n"),
                            })
                            .await
                            .is_err()
                        {
                            tracing::debug!("no consumer was left for the failure");
                        }
                        ShellEvent::Exited {
                            exit_code: None,
                            signal: None,
                            duration_ms: 0,
                            timed_out: false,
                        }
                    }
                };
                if finisher.send(event).await.is_err() {
                    tracing::debug!("no consumer was left for the exit event");
                }
            });
            let stream = futures::stream::unfold(receiver, |mut receiver| async move {
                receiver.recv().await.map(|event| (event, receiver))
            });
            let stream: ShellStream = Box::pin(stream);
            Ok(stream)
        })
    }

    fn kill_all(&self) -> LocalBoxFuture<'_, ShellResult<usize>> {
        Box::pin(async move { Ok(self.kill_all_blocking()) })
    }

    fn sandbox(&self) -> SandboxPolicy {
        self.policy.clone()
    }
}
