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
//! ## The evidence archive
//!
//! [`ShellPort::run_with_capture`] is `run` with a second reader of the same bytes. It goes
//! through the same engine — the same spawn, the same group, the same wait, timeout and
//! drain — and differs only in that each pump hands the exact bytes it read to an archive
//! *before* the preview cap cuts them. Two rules shape that hand-off, both about never letting
//! the archive reach back into the process:
//!
//! - **A pump never waits on a sink.** The sink is owned by a task of its own, and a pump only
//!   *stages* a copy of each chunk for it. Staged bytes hold permits of one budget shared by
//!   both streams ([`CaptureLimits::staging_bytes`]), so memory is bounded by the permits rather
//!   than by the channel. When the budget is full a pump waits for room, but only for a total of
//!   one sink deadline per stream, and never longer than half the drain grace; after that the
//!   stream's capture is disabled and the pump goes back to draining for the preview alone. A
//!   slow archive can delay a command by at most that much, can never stop it, and can never be
//!   the reason a drain is abandoned and its preview lost.
//! - **An in-flight write is never abandoned.** A write that passes its deadline disables the
//!   stream and the task keeps polling it to completion before it finalizes, so no physical
//!   write is dropped and then published. The engine waits for the finalization for a bounded
//!   time; if it does not arrive, the run reports the stream as uncertain and the task finishes
//!   on its own, its object an unreferenced orphan the store accounts for.
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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use nanus_domain::context::managed::CaptureReason;
use nanus_ports::{
    CaptureFailure, CaptureFinalization, CaptureLimits, Captured, CapturedOutcome, LocalBoxFuture,
    RawCaptureSink, SandboxPolicy, SendBoxFuture, ShellCapture, ShellError, ShellEvent,
    ShellOutcome, ShellPort, ShellRequest, ShellResult, ShellStream, ensure_within,
};
#[cfg(windows)]
use nanus_sys_windows::Job;
#[cfg(unix)]
use nix::sys::signal::{Signal, kill, killpg};
#[cfg(unix)]
use nix::unistd::Pid;
#[cfg(windows)]
use std::collections::BTreeMap;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt as _};
#[cfg(unix)]
use tokio::process::Child;
use tokio::process::Command;
#[cfg(windows)]
type Child = Arc<Job>;
use tokio::sync::oneshot::error::TryRecvError;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::Instant;

/// Bytes read from a pipe in one syscall.
const READ_CHUNK: usize = 8 * 1024;

/// How long to wait for the pipe pumps to reach EOF after the group is killed.
///
/// Without this bound a grandchild that escaped the group by calling `setsid`
/// would hold the pipe open forever and the run would never return.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// The longest, in total, a pump may wait for the archive's staging room.
///
/// Kept under [`DRAIN_GRACE`] so that a pump held back by a slow archive after the process has
/// exited still reaches EOF before the engine gives up on it: an abandoned drain loses its
/// preview, and capture must never change what a run returns.
const STALL_MAX: Duration = Duration::from_secs(1);

const _: () = assert!(
    STALL_MAX.as_millis() < DRAIN_GRACE.as_millis(),
    "a pump's archive stall ends before its drain grace does"
);

/// The wall-clock budget applied when a request does not set one.
pub const DEFAULT_TIMEOUT_MS: u64 = 120_000;

/// Depth of the bounded live-output channel.
const STREAM_DEPTH: usize = 256;

/// Reports whether `pid` is alive, using the null signal.
///
/// `kill(pid, 0)` succeeds while the process exists and reports `ESRCH` once it is
/// gone, which is the portable liveness probe the tests use.
#[must_use]
#[cfg(unix)]
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
    #[cfg(windows)]
    jobs: BTreeMap<i32, Arc<Job>>,
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
        #[cfg(windows)]
        self.jobs.remove(&pgid);
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
    #[cfg(unix)]
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
/// a full pipe and hang the run, which is the `take(n)` trap. An archive, when there is
/// one, is handed each chunk exactly as it was read and *before* the cap, so what scrolls
/// past the preview is still kept; it is told how the pipe ended once it has.
async fn drain<R>(
    mut reader: R,
    cap: usize,
    is_stderr: bool,
    live: Option<mpsc::Sender<ShellEvent>>,
    mut archive: Option<ArchiveFeed>,
) -> Captured
where
    R: AsyncRead + Unpin,
{
    assert!(cap > 0, "an output cap is at least one byte");
    let mut stored: Vec<u8> = Vec::new();
    let mut total: u64 = 0;
    let mut chunk = [0u8; READ_CHUNK];
    let ended = loop {
        let read = match reader.read(&mut chunk).await {
            Ok(0) => break CaptureReason::Eof,
            Ok(n) => n,
            Err(error) => {
                tracing::debug!(%error, "child stream ended in a read error");
                break CaptureReason::ReadError;
            }
        };
        total = total.saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
        let bytes = chunk.get(..read).unwrap_or_default();
        if let Some(feed) = archive.as_mut() {
            feed.stage(bytes).await;
        }
        append_capped(&mut stored, bytes, cap);
        if let Some(sender) = live.as_ref() {
            offer(sender, is_stderr, bytes);
        }
    };
    if let Some(feed) = archive {
        feed.end(ended);
    }
    captured(&stored, total)
}

/// What a pump hands the task that owns one stream's archive sink.
enum Staged {
    /// Exact pipe bytes, holding their share of the call's staging budget until they are
    /// written or dropped.
    Bytes(Vec<u8>, OwnedSemaphorePermit),
    /// The pump stopped staging, and why.
    Stopped(CaptureReason),
    /// The pump reached the end of its pipe, and how.
    Ended(CaptureReason),
}

/// The pump's half of one stream's archive.
///
/// The channel is unbounded in messages and bounded in bytes: every [`Staged::Bytes`] holds
/// permits of the call's staging semaphore for exactly its length, and the two other messages
/// are sent at most once each. So the memory a stalled sink can pin is the staging budget, which
/// is the bound the live channel's `try_send` gives that channel, stated in bytes rather than in
/// events.
struct ArchiveFeed {
    /// Where staged bytes go.
    staged: mpsc::UnboundedSender<Staged>,
    /// The call's staging budget, shared by both streams.
    staging: Arc<Semaphore>,
    /// Set once this stream's capture has stopped, by either side.
    disabled: Arc<AtomicBool>,
    /// Every byte this pump read, archived or not.
    observed: Arc<AtomicU64>,
    /// How much longer, in total, this pump may wait for staging room.
    stall_left: Duration,
}

impl ArchiveFeed {
    /// Counts `bytes` as observed and stages a copy of them, unless capture has stopped.
    async fn stage(&mut self, bytes: &[u8]) {
        assert!(!bytes.is_empty(), "a pump stages only what it read");
        let length = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        let before = self.observed.fetch_add(length, Ordering::AcqRel);
        assert!(
            before.checked_add(length).is_some(),
            "observed bytes fit in a u64"
        );
        if self.disabled.load(Ordering::Acquire) {
            return;
        }
        let Ok(wanted) = u32::try_from(bytes.len()) else {
            self.stop(CaptureReason::WriteError);
            return;
        };
        let permit = match Arc::clone(&self.staging).try_acquire_many_owned(wanted) {
            Ok(permit) => permit,
            Err(_) => match self.wait_for_room(wanted).await {
                Some(permit) => permit,
                None => return,
            },
        };
        // The sink may have stopped while this pump waited; the permit then simply returns.
        if self.disabled.load(Ordering::Acquire) {
            return;
        }
        if self
            .staged
            .send(Staged::Bytes(bytes.to_vec(), permit))
            .is_err()
        {
            self.disabled.store(true, Ordering::Release);
        }
    }

    /// Waits for staging room out of what is left of this pump's stall allowance.
    ///
    /// The allowance is spent, not reset, so a sink that is merely slow cannot hold the pipe
    /// back for one deadline per chunk: it holds it back for one allowance in all, and then the
    /// stream's capture stops with [`CaptureReason::Timeout`].
    async fn wait_for_room(&mut self, wanted: u32) -> Option<OwnedSemaphorePermit> {
        let started = Instant::now();
        let room = Arc::clone(&self.staging).acquire_many_owned(wanted);
        let granted = tokio::time::timeout(self.stall_left, room).await;
        self.stall_left = self.stall_left.saturating_sub(started.elapsed());
        if let Ok(Ok(permit)) = granted {
            return Some(permit);
        }
        self.stop(CaptureReason::Timeout);
        None
    }

    /// Stops staging and tells the sink's task why, once.
    fn stop(&self, reason: CaptureReason) {
        if self.disabled.swap(true, Ordering::AcqRel) {
            return;
        }
        if self.staged.send(Staged::Stopped(reason)).is_err() {
            tracing::debug!("the archive task was gone before capture stopped");
        }
    }

    /// Tells the sink's task how the pipe ended. Dropping the feed then closes the channel.
    fn end(self, reason: CaptureReason) {
        if self.staged.send(Staged::Ended(reason)).is_err() {
            tracing::debug!("the archive task was gone before the pipe ended");
        }
    }
}

/// The engine's word on how the command ended, sent once its pumps are done.
#[derive(Clone, Copy, Debug)]
struct Verdict {
    /// [`CaptureReason::Cancelled`] when a timeout or a shutdown cut the command off.
    reason: Option<CaptureReason>,
    /// When writing the backlog must stop, so that finalizing can still be awaited.
    settle_by: Instant,
}

impl Verdict {
    /// The verdict of an engine that went away without giving one: its caller was cancelled.
    fn abandoned(deadline: Duration) -> Self {
        let now = Instant::now();
        Self {
            reason: Some(CaptureReason::Cancelled),
            settle_by: now.checked_add(deadline).unwrap_or(now),
        }
    }
}

/// The engine's half of one stream's archive.
struct ArchiveControl {
    /// How the command ended.
    verdict: oneshot::Sender<Verdict>,
    /// What the sink finalized to.
    finalized: oneshot::Receiver<CaptureFinalization>,
}

/// Everything an archive task owns except the sink itself.
///
/// Kept apart from the sink so that an in-flight write, which borrows the sink, can run while
/// the backlog is still being read and released.
struct Backlog {
    /// What the pump staged.
    staged: mpsc::UnboundedReceiver<Staged>,
    /// How the command ended, once the engine says.
    verdict: oneshot::Receiver<Verdict>,
    /// The verdict, once it has been received.
    known: Option<Verdict>,
    /// Shared with the pump, so a failure here stops its staging.
    disabled: Arc<AtomicBool>,
    /// The bounds of this call.
    limits: CaptureLimits,
    /// Bytes the sink accepted.
    retained: u64,
    /// The first reason capture stopped early.
    failure: Option<CaptureReason>,
    /// How the pipe ended, when the pump said.
    ended: Option<CaptureReason>,
}

impl Backlog {
    /// Records the first reason capture stopped, and stops the pump staging.
    fn fail(&mut self, reason: CaptureReason) {
        if self.failure.is_none() {
            self.failure = Some(reason);
        }
        self.disabled.store(true, Ordering::Release);
    }

    /// Takes in one non-write message, or drops staged bytes, returning their permit.
    fn note(&mut self, staged: Staged) {
        match staged {
            // Discarded, which returns their share of the budget.
            Staged::Bytes(_bytes, permit) => drop(permit),
            Staged::Stopped(reason) => self.fail(reason),
            Staged::Ended(reason) => self.ended = Some(reason),
        }
    }

    /// Picks up the verdict if it has arrived, without waiting for it.
    fn poll_verdict(&mut self) {
        if self.known.is_some() {
            return;
        }
        match self.verdict.try_recv() {
            Ok(verdict) => self.known = Some(verdict),
            Err(TryRecvError::Closed) => {
                self.known = Some(Verdict::abandoned(self.limits.deadline));
            }
            Err(TryRecvError::Empty) => {}
        }
    }

    /// Waits for the verdict, returning the reason it gives.
    async fn settle(&mut self) -> Option<CaptureReason> {
        if let Some(verdict) = self.known {
            return verdict.reason;
        }
        match (&mut self.verdict).await {
            Ok(verdict) => verdict.reason,
            Err(_closed) => Some(CaptureReason::Cancelled),
        }
    }

    /// How long the next write may take: a deadline, and less once the backlog must settle.
    fn write_limit(&self) -> Option<Duration> {
        let Some(verdict) = self.known else {
            return Some(self.limits.deadline);
        };
        let left = verdict.settle_by.saturating_duration_since(Instant::now());
        (!left.is_zero()).then(|| left.min(self.limits.deadline))
    }

    /// Writes one staged chunk, keeping at most the stream's retention limit.
    async fn write(&mut self, sink: &mut dyn RawCaptureSink, bytes: &[u8]) {
        if self.failure.is_some() {
            return;
        }
        let room = self.limits.stream_bytes.saturating_sub(self.retained);
        let take = usize::try_from(room).unwrap_or(usize::MAX).min(bytes.len());
        let kept = bytes.get(..take).unwrap_or_default();
        if !kept.is_empty() {
            let Some(limit) = self.write_limit() else {
                self.fail(CaptureReason::Timeout);
                return;
            };
            let mut pending = sink.write(kept);
            match tokio::time::timeout(limit, &mut pending).await {
                Ok(Ok(())) => {
                    let length = u64::try_from(kept.len()).unwrap_or(u64::MAX);
                    self.retained = self.retained.saturating_add(length);
                }
                Ok(Err(failure)) => {
                    self.fail(failure.reason());
                    return;
                }
                Err(_elapsed) => {
                    self.fail(CaptureReason::Timeout);
                    self.outlast(pending).await;
                    return;
                }
            }
        }
        if take < bytes.len() {
            self.fail(CaptureReason::Quota);
        }
        assert!(
            self.retained <= self.limits.stream_bytes,
            "retention stays within its limit"
        );
    }

    /// Polls an overdue write to completion, releasing the backlog meanwhile.
    ///
    /// Dropping the write would abandon a physical write the sink may still be making, and a
    /// sink finalized after that could publish bytes nobody waited for. So the write is kept;
    /// what is released is the staged bytes behind it, which free their budget for the other
    /// stream.
    async fn outlast(&mut self, mut pending: SendBoxFuture<'_, Result<(), CaptureFailure>>) {
        loop {
            tokio::select! {
                written = &mut pending => {
                    tracing::debug!(late = written.is_ok(), "an overdue archive write finished");
                    return;
                }
                staged = self.staged.recv() => {
                    let Some(staged) = staged else { break };
                    self.note(staged);
                }
            }
        }
        let written = pending.await;
        tracing::debug!(late = written.is_ok(), "an overdue archive write finished");
    }

    /// The reason the receipt records: the archive's own failure first, then the pipe's, then
    /// the command's, and end of file only when nothing else went wrong.
    fn reason(&self, verdict: Option<CaptureReason>) -> CaptureReason {
        let ended = self.ended.unwrap_or(CaptureReason::DrainExpired);
        let pipe = (ended != CaptureReason::Eof).then_some(ended);
        self.failure
            .or(pipe)
            .or(verdict)
            .unwrap_or(CaptureReason::Eof)
    }
}

/// Owns one stream's sink: writes what the pump staged, then finalizes it, whatever the caller
/// is doing by then.
///
/// The task runs to the end even when the run that started it has been dropped, because the
/// sink's physical work has to finish regardless: cleanup does not depend on the logical
/// cancellation of the call. A finalization nobody is waiting for any more is dropped, and the
/// object it describes is an orphan the store still accounts for.
async fn archive(
    mut sink: Box<dyn RawCaptureSink>,
    mut backlog: Backlog,
    observed: Arc<AtomicU64>,
    finalized: oneshot::Sender<CaptureFinalization>,
) {
    while let Some(staged) = backlog.staged.recv().await {
        backlog.poll_verdict();
        match staged {
            Staged::Bytes(bytes, permit) => {
                backlog.write(sink.as_mut(), &bytes).await;
                drop(permit);
            }
            other => backlog.note(other),
        }
    }
    let verdict = backlog.settle().await;
    let reason = backlog.reason(verdict);
    let observed = observed.load(Ordering::Acquire);
    assert!(
        backlog.retained <= observed,
        "nothing is retained that was not observed"
    );
    let finalization = sink.finalize(observed, reason).await;
    if finalized.send(finalization).is_err() {
        tracing::debug!(
            ?reason,
            "an archive finalized after its caller stopped waiting"
        );
    }
}

/// Starts the task that owns `sink`, returning the pump's and the engine's halves of it.
///
/// This happens before the process is spawned, so the first byte the process writes already
/// has somewhere to go.
fn start_archive(
    sink: Box<dyn RawCaptureSink>,
    staging: &Arc<Semaphore>,
    limits: CaptureLimits,
) -> (ArchiveFeed, ArchiveControl) {
    let (staged_tx, staged_rx) = mpsc::unbounded_channel();
    let (verdict_tx, verdict_rx) = oneshot::channel();
    let (finalized_tx, finalized_rx) = oneshot::channel();
    let disabled = Arc::new(AtomicBool::new(false));
    let observed = Arc::new(AtomicU64::new(0));
    tokio::spawn(archive(
        sink,
        Backlog {
            staged: staged_rx,
            verdict: verdict_rx,
            known: None,
            disabled: Arc::clone(&disabled),
            limits,
            retained: 0,
            failure: None,
            ended: None,
        },
        Arc::clone(&observed),
        finalized_tx,
    ));
    let feed = ArchiveFeed {
        staged: staged_tx,
        staging: Arc::clone(staging),
        disabled,
        observed,
        stall_left: limits.deadline.min(STALL_MAX),
    };
    let control = ArchiveControl {
        verdict: verdict_tx,
        finalized: finalized_rx,
    };
    (feed, control)
}

/// Gives one archive its verdict and waits, for a bounded time, for what it finalized to.
///
/// The bound is two deadlines: one in which the backlog may still be written and one in which
/// the sink may finalize. `None` means the outcome is uncertain — an overdue write or finalize
/// is still running — and the caller must treat the stream as unavailable.
async fn collect(
    control: Option<ArchiveControl>,
    reason: Option<CaptureReason>,
    deadline: Duration,
) -> Option<CaptureFinalization> {
    let control = control?;
    let now = Instant::now();
    let settle_by = now.checked_add(deadline).unwrap_or(now);
    if control.verdict.send(Verdict { reason, settle_by }).is_err() {
        tracing::debug!("an archive task ended before its verdict");
    }
    match tokio::time::timeout(deadline.saturating_mul(2), control.finalized).await {
        Ok(Ok(finalization)) => Some(finalization),
        Ok(Err(_dropped)) => None,
        Err(_elapsed) => {
            tracing::warn!("an archive did not finalize in time; its stream is unavailable");
            None
        }
    }
}

/// The pump halves a run feeds, one per stream that is being captured.
#[derive(Default)]
struct Feeds {
    stdout: Option<ArchiveFeed>,
    stderr: Option<ArchiveFeed>,
}

/// What the engine learned beyond the outcome itself.
struct Ran {
    /// The outcome every caller sees.
    outcome: ShellOutcome,
    /// Whether the port, rather than the command, ended it: a timeout or a shutdown.
    cut_off: bool,
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
#[cfg(unix)]
fn signal_group(pgid: i32, signal: Signal) -> ShellResult<()> {
    assert!(pgid > 0, "a process group id is positive");
    killpg(Pid::from_raw(pgid), signal).map_err(|source| ShellError::Spawn {
        program: format!("process group {pgid}"),
        message: format!("could not be signalled: {source}"),
    })
}

/// Spawns `command` in its own process group.
#[cfg(unix)]
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
#[cfg(unix)]
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

/// Passes the request's arguments to the program, exactly as the request holds them.
#[cfg(unix)]
fn apply_args(command: &mut Command, request: &ShellRequest) {
    command.args(&request.args);
}

/// Passes the request's arguments to the program; a `cmd /C` script goes through verbatim.
///
/// `cmd` does not parse its command line with the rules the standard library quotes for: those
/// turn every `"` inside an argument into `\"`, and `cmd /C` strips only the outermost pair of
/// quotes, so `git commit -m "fix bug"` reached git as the two arguments `\"fix` and `bug\"`. A
/// shell-wrapped request therefore becomes `cmd /S /C "<script>"`, appended raw: `/S` is what
/// makes `cmd` remove exactly the first and the last quote and run what is between them as
/// written. Every other request — including a direct one to some other program — is quoted
/// normally, because that program is the one that will parse it.
#[cfg(windows)]
fn apply_args(command: &mut Command, request: &ShellRequest) {
    match request.args.as_slice() {
        [_flag, script] if request.is_shell_wrapped() => {
            command
                .raw_arg("/S")
                .raw_arg("/C")
                .raw_arg(format!("\"{script}\""));
        }
        _ => {
            command.args(&request.args);
        }
    }
}

/// Splits an exit status into a code and a signal, one of which is present.
#[cfg(unix)]
fn split_status(status: ExitStatus) -> (Option<i32>, Option<i32>) {
    use std::os::unix::process::ExitStatusExt as _;
    (status.code(), status.signal())
}

/// Writes the request's standard input from a task of its own, then closes the pipe.
fn feed_stdin(child: &mut Child, stdin: Option<String>) {
    if let Some(text) = stdin
        && let Some(mut pipe) = take_stdin(child)
    {
        tokio::spawn(async move {
            if let Err(error) = pipe.write_all(text.as_bytes()).await {
                tracing::debug!(%error, "failed to write the child's standard input");
            }
            // Dropping the pipe closes it, which is what unblocks the child.
        });
    }
}

/// Awaits a pump that may never have been started.
async fn join_pump(handle: Option<JoinHandle<Captured>>) -> Captured {
    match handle {
        Some(handle) => join_drain(handle).await,
        None => empty_captured(),
    }
}

/// Runs one command to completion under the process-group discipline.
///
/// `live` is the live-output channel, when the caller wants one. Chunks are
/// offered with `try_send` and dropped when the channel is full, so a slow
/// consumer can never grow the producer's memory without limit. `archive` holds the
/// pump halves of the streams being captured; it changes what the pumps hand on, never how
/// the process is spawned, waited for, timed out or killed.
async fn run_engine(
    request: ShellRequest,
    registry: Arc<Mutex<GroupRegistry>>,
    live: Option<mpsc::Sender<ShellEvent>>,
    archive: Feeds,
) -> ShellResult<Ran> {
    assert!(!request.program.is_empty(), "a program to run is named");
    let cap = request.max_output_bytes;
    if cap == 0 {
        return Err(ShellError::InvalidOutputCap { limit: cap });
    }
    let started = Instant::now();
    let mut command = Command::new(&request.program);
    apply_args(&mut command, &request);
    if let Some(cwd) = request.cwd.as_ref() {
        command.current_dir(cwd);
    }
    for (key, value) in &request.env {
        command.env(key, value);
    }
    let (mut child, guard, pgid) = spawn_group(&mut command, &request.program, &registry)?;
    feed_stdin(&mut child, request.stdin.clone());
    let out_pipe = take_stdout(&mut child);
    let err_pipe = take_stderr(&mut child);
    let (out_live, err_live) = (live.clone(), live.clone());
    let Feeds { stdout, stderr } = archive;
    let out_handle = out_pipe.map(|pipe| tokio::spawn(drain(pipe, cap, false, out_live, stdout)));
    let err_handle = err_pipe.map(|pipe| tokio::spawn(drain(pipe, cap, true, err_live, stderr)));

    let (status, timed_out) = wait_group(&mut child, pgid, request.timeout).await?;
    let stdout = join_pump(out_handle).await;
    let stderr = join_pump(err_handle).await;
    let elapsed = started.elapsed();
    // `kill_all` retires the groups it signals, so a group already gone from the registry
    // was ended by a shutdown rather than by the command.
    let shut_down = !lock(&registry).contains(pgid);
    drop(guard);
    assert!(
        !lock(&registry).contains(pgid),
        "a completed run leaves no live process group"
    );
    // Close the live channel only after the group is retired, so a consumer that
    // observes the end of the stream is guaranteed to see a clean registry.
    drop(live);
    let (exit_code, signal) = split_status(status);
    let outcome = ShellOutcome {
        exit_code,
        signal,
        timed_out,
        duration_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
        stdout,
        stderr,
    };
    Ok(Ran {
        outcome,
        cut_off: timed_out || shut_down,
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
    /// The staging budget and sink deadline a captured run works within.
    capture_limits: CaptureLimits,
}

impl LocalShell {
    /// Creates an adapter that enforces `policy`.
    #[must_use]
    pub fn new(policy: SandboxPolicy) -> Self {
        Self {
            registry: Arc::new(Mutex::new(GroupRegistry::default())),
            policy,
            timeout_ms: DEFAULT_TIMEOUT_MS,
            capture_limits: CaptureLimits::default(),
        }
    }

    /// Replaces the bounds a captured run works within.
    ///
    /// The defaults are the specification's: 8 MiB retained per stream, 128 KiB staged per
    /// call across both streams, and five seconds for one sink write or finalize. A host — or a
    /// test that cannot wait five seconds — may tighten them.
    ///
    /// # Panics
    ///
    /// When the staging budget cannot hold one pipe read, or is too large for a semaphore, or
    /// the deadline is zero: each would make capture fail on its first byte rather than bound it.
    #[must_use]
    pub fn with_capture_limits(mut self, limits: CaptureLimits) -> Self {
        assert!(
            limits.staging_bytes >= READ_CHUNK,
            "the staging budget holds at least one pipe read"
        );
        assert!(
            limits.staging_bytes <= Semaphore::MAX_PERMITS,
            "the staging budget fits a semaphore"
        );
        assert!(!limits.deadline.is_zero(), "a sink deadline is positive");
        self.capture_limits = limits;
        self
    }

    /// Returns the bounds a captured run works within.
    #[must_use]
    pub const fn capture_limits(&self) -> CaptureLimits {
        self.capture_limits
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
    #[cfg(unix)]
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
            let ran = run_engine(request, Arc::clone(&self.registry), None, Feeds::default());
            ran.await.map(|ran| ran.outcome)
        })
    }

    /// Runs as [`ShellPort::run`] does, archiving each stream's exact bytes as it goes.
    ///
    /// Both sinks are handed to their tasks before the process is spawned. A stream's
    /// finalization is `None` when no sink was given for it *or* when its outcome is uncertain:
    /// a write or finalize was still running two deadlines after the command ended. An
    /// uncertain stream must be reported as unavailable; its task finishes on its own and the
    /// object it leaves is never published.
    fn run_with_capture(
        &self,
        request: ShellRequest,
        capture: ShellCapture,
    ) -> LocalBoxFuture<'_, ShellResult<CapturedOutcome>> {
        Box::pin(async move {
            let request = self.resolve(request)?;
            let request = self.confine_cwd(request)?;
            let limits = self.capture_limits;
            let staging = Arc::new(Semaphore::new(limits.staging_bytes));
            let start = |sink| start_archive(sink, &staging, limits);
            let (out_feed, out_control) = capture.stdout.map(start).unzip();
            let (err_feed, err_control) = capture.stderr.map(start).unzip();
            let registry = Arc::clone(&self.registry);
            let ran = run_engine(
                request,
                registry,
                None,
                Feeds {
                    stdout: out_feed,
                    stderr: err_feed,
                },
            );
            let ran = ran.await?;
            let verdict = ran.cut_off.then_some(CaptureReason::Cancelled);
            let (stdout, stderr) = tokio::join!(
                collect(out_control, verdict, limits.deadline),
                collect(err_control, verdict, limits.deadline),
            );
            Ok(CapturedOutcome {
                outcome: ran.outcome,
                stdout,
                stderr,
            })
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
                let ran = run_engine(request, registry, Some(sender), Feeds::default()).await;
                let event = match ran.map(|ran| ran.outcome) {
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

#[cfg(unix)]
fn take_stdin(child: &mut Child) -> Option<tokio::process::ChildStdin> {
    child.stdin.take()
}
#[cfg(unix)]
fn take_stdout(child: &mut Child) -> Option<tokio::process::ChildStdout> {
    child.stdout.take()
}
#[cfg(unix)]
fn take_stderr(child: &mut Child) -> Option<tokio::process::ChildStderr> {
    child.stderr.take()
}
#[cfg(windows)]
// The Unix side needs a mutable child; keep the shared engine's call shape identical.
#[allow(clippy::needless_pass_by_ref_mut)]
fn take_stdin(child: &mut Child) -> Option<tokio::process::ChildStdin> {
    child.take_stdin()
}
#[cfg(windows)]
// The Unix side needs a mutable child; keep the shared engine's call shape identical.
#[allow(clippy::needless_pass_by_ref_mut)]
fn take_stdout(child: &mut Child) -> Option<tokio::process::ChildStdout> {
    child.take_stdout()
}
#[cfg(windows)]
// The Unix side needs a mutable child; keep the shared engine's call shape identical.
#[allow(clippy::needless_pass_by_ref_mut)]
fn take_stderr(child: &mut Child) -> Option<tokio::process::ChildStderr> {
    child.take_stderr()
}

#[cfg(windows)]
fn job_error(error: &nanus_sys_windows::JobError) -> ShellError {
    ShellError::Spawn {
        program: String::from("the Windows job"),
        message: error.to_string(),
    }
}

#[cfg(windows)]
fn spawn_group(
    command: &mut Command,
    program: &str,
    registry: &Arc<Mutex<GroupRegistry>>,
) -> ShellResult<(Child, LiveGroup, i32)> {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let command = std::mem::replace(command, Command::new(program));
    let job = Arc::new(Job::spawn(command).map_err(|error| job_error(&error))?);
    let pid = i32::try_from(job.pid()).map_err(|_| ShellError::Spawn {
        program: program.to_owned(),
        message: String::from("the pid does not fit in an i32"),
    })?;
    let guard = LiveGroup::register(registry, pid);
    let previous = lock(registry).jobs.insert(pid, Arc::clone(&job));
    assert!(previous.is_none(), "a live job is registered exactly once");
    Ok((job, guard, pid))
}

/// Waits for the job's leader, ending the whole job when the leader exits or the budget elapses.
///
/// This is where Windows deliberately differs from Unix. A Unix group is signalled only on a
/// timeout, so `nohup server &` outlives the call. Here the job is the call's: when the leader
/// exits, everything it started goes with it — `start /b server` included, and a `nanus service
/// start` run as a tool, whose service is started inside the job. Ending the tree is also what
/// closes the pipes a leftover descendant would hold open, which the drain would otherwise wait out.
/// The last owner closing the job kills the tree regardless, so terminating here only makes it
/// prompt; a failure to do so is reported and the leader's real status is still returned.
#[cfg(windows)]
// Waiting mutates the Unix child; the Windows job puts that state behind its mutex.
#[allow(clippy::needless_pass_by_ref_mut)]
async fn wait_group(
    child: &mut Child,
    pgid: i32,
    timeout: Option<Duration>,
) -> ShellResult<(ExitStatus, bool)> {
    assert!(pgid > 0, "a spawned job has a positive pid");
    let started = Instant::now();
    let mut timed_out = false;
    loop {
        if let Some(status) = child.try_wait().map_err(|error| job_error(&error))? {
            if let Err(error) = child.terminate() {
                tracing::warn!(%error, pgid, "the job outlived its leader and could not be ended");
            }
            return Ok((status, timed_out));
        }
        if !timed_out && timeout.is_some_and(|limit| started.elapsed() >= limit) {
            child.terminate().map_err(|error| job_error(&error))?;
            timed_out = true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[cfg(windows)]
fn split_status(status: ExitStatus) -> (Option<i32>, Option<i32>) {
    (status.code(), None)
}

#[cfg(windows)]
impl LocalShell {
    fn kill_all_blocking(&self) -> usize {
        let jobs = {
            let mut registry = lock(&self.registry);
            registry.live.clear();
            std::mem::take(&mut registry.jobs)
        };
        let mut killed = 0usize;
        for (pid, job) in jobs {
            if job.terminate().is_ok() {
                killed = killed.saturating_add(1);
            } else {
                tracing::warn!(pid, "failed to terminate a job during shutdown");
            }
        }
        assert_eq!(lock(&self.registry).len(), 0, "shutdown retires every job");
        killed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A feed over a fresh channel, with the receiving end handed back to play the sink task.
    fn feed(staging: usize, stall: Duration) -> (ArchiveFeed, mpsc::UnboundedReceiver<Staged>) {
        let (staged, received) = mpsc::unbounded_channel();
        let feed = ArchiveFeed {
            staged,
            staging: Arc::new(Semaphore::new(staging)),
            disabled: Arc::new(AtomicBool::new(false)),
            observed: Arc::new(AtomicU64::new(0)),
            stall_left: stall,
        };
        (feed, received)
    }

    /// Counts what is waiting in the channel, by kind.
    fn tally(received: &mut mpsc::UnboundedReceiver<Staged>) -> (usize, Vec<CaptureReason>) {
        let mut bytes = 0_usize;
        let mut stops = Vec::new();
        while let Ok(staged) = received.try_recv() {
            match staged {
                Staged::Bytes(chunk, _permit) => bytes = bytes.saturating_add(chunk.len()),
                Staged::Stopped(reason) | Staged::Ended(reason) => stops.push(reason),
            }
        }
        (bytes, stops)
    }

    /// A sink that never takes anything pins the staging budget and no more: the pump waits out
    /// one stall, stops capture, and goes on counting what it reads without staging it.
    #[tokio::test]
    async fn a_full_staging_budget_stops_capture_after_one_stall_and_bounds_memory() {
        let (mut feed, mut received) = feed(2 * READ_CHUNK, Duration::from_millis(50));
        let chunk = [7_u8; READ_CHUNK];
        let started = Instant::now();
        for _ in 0..4 {
            feed.stage(&chunk).await;
        }
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the pump was not held back"
        );
        assert_eq!(
            feed.staging.available_permits(),
            0,
            "the budget is exactly full"
        );
        assert!(feed.disabled.load(Ordering::Acquire));
        assert_eq!(feed.observed.load(Ordering::Acquire), 4 * 8192);
        let (staged, stops) = tally(&mut received);
        assert_eq!(staged, 2 * READ_CHUNK, "nothing past the budget was staged");
        assert_eq!(stops, vec![CaptureReason::Timeout]);
        // Releasing the staged bytes returns the budget, which is what lets the other stream on.
        assert_eq!(feed.staging.available_permits(), 2 * READ_CHUNK);
    }

    /// The other direction: a sink that frees room within the stall is waited for, not dropped.
    #[tokio::test]
    async fn a_pump_waits_for_room_a_working_sink_frees() {
        let (mut feed, mut received) = feed(READ_CHUNK, Duration::from_secs(5));
        let chunk = [1_u8; READ_CHUNK];
        feed.stage(&chunk).await;
        let sink = tokio::spawn(async move {
            let mut taken = 0_usize;
            while let Some(staged) = received.recv().await {
                tokio::time::sleep(Duration::from_millis(10)).await;
                if let Staged::Bytes(bytes, _permit) = staged {
                    taken = taken.saturating_add(bytes.len());
                }
            }
            taken
        });
        for _ in 0..3 {
            feed.stage(&chunk).await;
        }
        assert!(
            !feed.disabled.load(Ordering::Acquire),
            "a sink that keeps up keeps capture on"
        );
        drop(feed);
        assert_eq!(sink.await.ok(), Some(4 * READ_CHUNK));
    }

    /// Builds a backlog whose only interesting state is how capture ended.
    fn backlog(failure: Option<CaptureReason>, ended: Option<CaptureReason>) -> Backlog {
        let (_staged, staged_rx) = mpsc::unbounded_channel();
        let (_verdict, verdict_rx) = oneshot::channel();
        Backlog {
            staged: staged_rx,
            verdict: verdict_rx,
            known: None,
            disabled: Arc::new(AtomicBool::new(false)),
            limits: CaptureLimits::default(),
            retained: 0,
            failure,
            ended,
        }
    }

    /// End of file is claimed only when the archive, the pipe and the command all finished well.
    #[test]
    fn a_receipt_says_end_of_file_only_when_nothing_went_wrong() {
        let eof = Some(CaptureReason::Eof);
        assert_eq!(backlog(None, eof).reason(None), CaptureReason::Eof);
        let cancelled = Some(CaptureReason::Cancelled);
        assert_eq!(
            backlog(None, eof).reason(cancelled),
            CaptureReason::Cancelled
        );
        let read_error = Some(CaptureReason::ReadError);
        assert_eq!(
            backlog(None, read_error).reason(cancelled),
            CaptureReason::ReadError
        );
        assert_eq!(
            backlog(None, None).reason(None),
            CaptureReason::DrainExpired
        );
        let quota = Some(CaptureReason::Quota);
        assert_eq!(
            backlog(quota, read_error).reason(cancelled),
            CaptureReason::Quota
        );
    }

    /// Counts what it is given and never finalizes; enough to watch the retention limit.
    struct Counting(usize);

    impl RawCaptureSink for Counting {
        fn write<'a>(
            &'a mut self,
            bytes: &'a [u8],
        ) -> SendBoxFuture<'a, Result<(), CaptureFailure>> {
            self.0 = self.0.saturating_add(bytes.len());
            Box::pin(async { Ok(()) })
        }

        fn finalize(
            self: Box<Self>,
            _observed: u64,
            _reason: CaptureReason,
        ) -> SendBoxFuture<'static, CaptureFinalization> {
            Box::pin(std::future::pending())
        }
    }

    /// A stream at its retention limit takes nothing more, and says why.
    #[tokio::test]
    async fn retention_stops_at_the_stream_limit_with_a_quota_reason() {
        let mut backlog = backlog(None, None);
        backlog.limits.stream_bytes = 10;
        let mut sink = Counting(0);
        backlog.write(&mut sink, b"0123456").await;
        assert_eq!(
            (sink.0, backlog.failure),
            (7, None),
            "under the limit, all is kept"
        );
        backlog.write(&mut sink, b"789abc").await;
        assert_eq!(sink.0, 10, "exactly the limit is kept");
        assert_eq!(backlog.retained, 10);
        assert_eq!(backlog.failure, Some(CaptureReason::Quota));
        assert!(
            backlog.disabled.load(Ordering::Acquire),
            "the pump is told to stop staging"
        );
        drop(backlog);
    }
}
