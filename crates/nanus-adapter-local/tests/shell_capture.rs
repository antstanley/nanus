//! Integration tests for archive capture through [`nanus_ports::ShellPort::run_with_capture`].
//!
//! The sinks here are scripted rather than the store's: one that keeps everything, one that
//! runs out of quota, one whose disk fails, and one that is slower than its deadline. What is
//! asserted is the shell's half of the contract — exact bytes before the preview cap, separate
//! streams, truthful reasons, pipes that never block on the archive, and process groups that are
//! reaped exactly as `run` reaps them.
//!
//! An integration-test crate is entirely test code, where a panic *is* the assertion, so the
//! workspace's panic-family exemption is restated here.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nanus_adapter_local::{LocalShell, is_alive};
use nanus_domain::context::managed::{
    ArtifactReceipt, CaptureReason, CaptureStatus, CaptureStream, RawEncoding,
};
use nanus_ports::{
    CaptureFailure, CaptureFinalization, CaptureLimits, CapturedOutcome, RawCaptureSink,
    SandboxPolicy, SendBoxFuture, ShellCapture, ShellPort, ShellRequest,
};

/// How a scripted sink answers a write.
#[derive(Clone, Copy, Debug)]
enum Behaviour {
    /// Keeps every byte.
    Accept,
    /// Keeps bytes until this many are held, then refuses.
    Quota(usize),
    /// Fails every write.
    Fail,
    /// Takes this long over every write, then keeps the bytes.
    Slow(Duration),
}

/// What a finalize was given — observed bytes and reason — and how many writes had finished.
type Finalized = (u64, CaptureReason, usize);

/// What a scripted sink saw, shared with the test that made it.
#[derive(Clone, Default)]
struct Record {
    bytes: Arc<Mutex<Vec<u8>>>,
    finalized: Arc<Mutex<Option<Finalized>>>,
    writes_done: Arc<AtomicUsize>,
}

impl Record {
    fn bytes(&self) -> Vec<u8> {
        self.bytes.lock().unwrap().clone()
    }

    fn finalized(&self) -> Option<Finalized> {
        *self.finalized.lock().unwrap()
    }

    /// Waits for the sink to be finalized, however the run that owned it ended.
    async fn wait_for_finalize(&self, limit: Duration) -> Option<Finalized> {
        let started = Instant::now();
        while started.elapsed() < limit {
            if let Some(finalized) = self.finalized() {
                return Some(finalized);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        self.finalized()
    }
}

/// A sink that does what its [`Behaviour`] says and writes a receipt the store would.
struct ScriptedSink {
    record: Record,
    behaviour: Behaviour,
    stream: CaptureStream,
}

impl RawCaptureSink for ScriptedSink {
    fn write<'a>(&'a mut self, bytes: &'a [u8]) -> SendBoxFuture<'a, Result<(), CaptureFailure>> {
        let record = self.record.clone();
        let behaviour = self.behaviour;
        Box::pin(async move {
            match behaviour {
                Behaviour::Accept => record.bytes.lock().unwrap().extend_from_slice(bytes),
                Behaviour::Quota(limit) => {
                    let mut kept = record.bytes.lock().unwrap();
                    if kept.len().saturating_add(bytes.len()) > limit {
                        return Err(CaptureFailure::Quota);
                    }
                    kept.extend_from_slice(bytes);
                }
                Behaviour::Fail => return Err(CaptureFailure::Io("the disk is full".into())),
                Behaviour::Slow(delay) => {
                    tokio::time::sleep(delay).await;
                    record.bytes.lock().unwrap().extend_from_slice(bytes);
                }
            }
            record.writes_done.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }

    fn finalize(
        self: Box<Self>,
        observed: u64,
        reason: CaptureReason,
    ) -> SendBoxFuture<'static, CaptureFinalization> {
        Box::pin(async move {
            let retained = self.record.bytes().len() as u64;
            let done = self.record.writes_done.load(Ordering::SeqCst);
            *self.record.finalized.lock().unwrap() = Some((observed, reason, done));
            let status = if reason == CaptureReason::Eof && retained == observed {
                CaptureStatus::Complete
            } else if retained > 0 {
                CaptureStatus::Partial
            } else {
                CaptureStatus::Unavailable
            };
            let receipt = ArtifactReceipt {
                artifact_id: None,
                call_id: "call-1".into(),
                stream: self.stream,
                retained_bytes: retained,
                observed_bytes: observed,
                retained_sha256: None,
                status,
                reason,
                encoding: RawEncoding::Raw,
                chunk_sha256: Vec::new(),
            };
            CaptureFinalization {
                receipt,
                artifact: None,
            }
        })
    }
}

/// Builds the two sinks of one call, returning what each will record.
fn sinks(out: Behaviour, err: Behaviour) -> (ShellCapture, Record, Record) {
    let (out_record, err_record) = (Record::default(), Record::default());
    let capture = ShellCapture {
        stdout: Some(Box::new(ScriptedSink {
            record: out_record.clone(),
            behaviour: out,
            stream: CaptureStream::Stdout,
        })),
        stderr: Some(Box::new(ScriptedSink {
            record: err_record.clone(),
            behaviour: err,
            stream: CaptureStream::Stderr,
        })),
    };
    (capture, out_record, err_record)
}

/// An unconfined adapter over a temporary root, with tight capture limits so a deadline test
/// does not wait five seconds.
fn shell(deadline: Duration) -> (tempfile::TempDir, LocalShell) {
    let dir = tempfile::tempdir().expect("tempdir");
    let limits = CaptureLimits {
        deadline,
        ..CaptureLimits::default()
    };
    let shell =
        LocalShell::new(SandboxPolicy::danger_full_access(dir.path())).with_capture_limits(limits);
    (dir, shell)
}

async fn captured(
    shell: &LocalShell,
    request: ShellRequest,
    capture: ShellCapture,
) -> CapturedOutcome {
    shell
        .run_with_capture(request, capture)
        .await
        .expect("the command ran")
}

/// The reason standard output's receipt records, when it was finalized.
fn stdout_reason(ran: &CapturedOutcome) -> Option<CaptureReason> {
    ran.stdout
        .as_ref()
        .map(|finalization| finalization.receipt.reason)
}

/// The reason standard error's receipt records, when it was finalized.
fn stderr_reason(ran: &CapturedOutcome) -> Option<CaptureReason> {
    ran.stderr
        .as_ref()
        .map(|finalization| finalization.receipt.reason)
}

/// Quotes `text` for `/bin/sh`.
fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// Reads the grandchild pid a script recorded, or zero.
fn recorded_pid(path: &Path) -> i32 {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| text.trim().parse::<i32>().ok())
        .unwrap_or(0)
}

/// Waits for `pid` to disappear.
async fn wait_for_death(pid: i32) -> bool {
    for _ in 0..250 {
        if !is_alive(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    !is_alive(pid)
}

/// T25: what the preview cap cut is in the archive, byte for byte — the marker past 64 KiB, the
/// bytes that are not UTF-8 — while the preview stays bounded and the failure is still stated.
#[tokio::test]
async fn a_marker_past_the_preview_cap_and_binary_bytes_are_archived_exactly() {
    let (_root, shell) = shell(Duration::from_secs(5));
    let script = "head -c 65536 /dev/zero | tr '\\0' 'x'; printf 'MARKER-PAST-THE-CAP'; \
                  printf '\\377\\376\\000\\001'; printf 'oops\\n' >&2; exit 3";
    let request = || ShellRequest::shell(script, None).with_max_output_bytes(65_536);
    let (capture, out, err) = sinks(Behaviour::Accept, Behaviour::Accept);
    let ran = captured(&shell, request(), capture).await;

    let mut expected = vec![b'x'; 65_536];
    expected.extend_from_slice(b"MARKER-PAST-THE-CAP");
    expected.extend_from_slice(&[0xff, 0xfe, 0x00, 0x01]);
    assert_eq!(
        out.bytes(),
        expected,
        "the archive holds the exact pipe bytes"
    );
    assert_eq!(
        err.bytes(),
        b"oops\n",
        "standard error is archived on its own"
    );
    assert_eq!(ran.outcome.exit_code, Some(3), "the failure is truthful");
    assert!(ran.outcome.stdout.truncated, "the preview says it was cut");
    assert_eq!(
        ran.outcome.stdout.text.len(),
        65_536,
        "the preview is bounded"
    );
    assert!(!ran.outcome.stdout.text.contains("MARKER"));
    assert_eq!(ran.outcome.stdout.total_bytes, expected.len() as u64);
    let stdout = ran.stdout.clone().expect("standard output finalized");
    assert_eq!(stdout.receipt.reason, CaptureReason::Eof);
    assert_eq!(stdout.receipt.status, CaptureStatus::Complete);
    assert_eq!(stdout.receipt.observed_bytes, expected.len() as u64);
    assert_eq!(stderr_reason(&ran), Some(CaptureReason::Eof));

    // Pair: without the archive, the same command's marker is simply gone.
    let plain = shell.run(request()).await.expect("run");
    assert!(!plain.stdout.text.contains("MARKER"));
    assert_eq!(plain.exit_code, Some(3));
}

/// A run given no sinks is a plain run: no finalization is invented for a stream nobody asked
/// to keep.
#[tokio::test]
async fn a_run_with_no_sinks_finalizes_nothing() {
    let (_root, shell) = shell(Duration::from_secs(5));
    let request = ShellRequest::shell("printf hello", None);
    let ran = captured(&shell, request, ShellCapture::default()).await;
    assert_eq!(ran.outcome.stdout.text, "hello");
    assert!(ran.stdout.is_none() && ran.stderr.is_none());
}

/// An archive that keeps pace with a fast producer is complete, which is the case the staging
/// budget's wait exists for: a pump that dropped capture whenever the budget was briefly full
/// would make every large output partial.
#[tokio::test]
async fn an_archive_that_keeps_pace_with_a_fast_producer_is_complete() {
    let (_root, shell) = shell(Duration::from_secs(5));
    let request = ShellRequest::shell("head -c 3000000 /dev/zero", None)
        .with_timeout(Duration::from_secs(30))
        .with_max_output_bytes(100);
    let (capture, out, _err) = sinks(Behaviour::Accept, Behaviour::Accept);
    let ran = captured(&shell, request, capture).await;
    assert_eq!(out.bytes().len(), 3_000_000);
    assert_eq!(stdout_reason(&ran), Some(CaptureReason::Eof));
    assert_eq!(
        ran.outcome.stdout.text.len(),
        100,
        "the preview cap is unchanged"
    );
}

/// T26, quota: a refusal stops the archive, never the command, and the pipe drains to the end.
#[tokio::test]
async fn a_quota_refusal_stops_the_archive_but_not_the_command() {
    let (_root, shell) = shell(Duration::from_secs(5));
    let request = ShellRequest::shell("head -c 1000000 /dev/zero", None)
        .with_timeout(Duration::from_secs(30));
    let (capture, out, _err) = sinks(Behaviour::Quota(20_000), Behaviour::Accept);
    let ran = captured(&shell, request, capture).await;
    assert!(ran.outcome.is_success(), "the command was not denied");
    assert_eq!(
        ran.outcome.stdout.total_bytes, 1_000_000,
        "every byte was drained"
    );
    let stdout = ran.stdout.expect("finalized");
    assert_eq!(stdout.receipt.reason, CaptureReason::Quota);
    assert_eq!(stdout.receipt.status, CaptureStatus::Partial);
    assert_eq!(stdout.receipt.observed_bytes, 1_000_000);
    let kept = out.bytes();
    assert!(
        !kept.is_empty() && kept.len() <= 20_000,
        "a prefix was kept: {}",
        kept.len()
    );
}

/// T26, disk error: a failing write leaves the stream unavailable and the command untouched.
#[tokio::test]
async fn a_failing_archive_write_is_reported_and_the_pipe_still_drains() {
    let (_root, shell) = shell(Duration::from_secs(5));
    let request = ShellRequest::shell("head -c 500000 /dev/zero; printf done >&2", None)
        .with_timeout(Duration::from_secs(30));
    let (capture, _out, err) = sinks(Behaviour::Fail, Behaviour::Accept);
    let ran = captured(&shell, request, capture).await;
    assert!(ran.outcome.is_success());
    assert_eq!(ran.outcome.stdout.total_bytes, 500_000);
    let stdout = ran.stdout.clone().expect("finalized");
    assert_eq!(stdout.receipt.reason, CaptureReason::WriteError);
    assert_eq!(stdout.receipt.status, CaptureStatus::Unavailable);
    // The other stream's archive is its own, and is unaffected.
    assert_eq!(err.bytes(), b"done");
    assert_eq!(stderr_reason(&ran), Some(CaptureReason::Eof));
}

/// T26, sink deadline: a sink slower than its deadline neither holds the pipe back nor has its
/// in-flight write dropped. The run returns long before the write would finish, reports the
/// stream as uncertain, and the sink is still finalized afterwards — once its write is done.
#[tokio::test]
async fn a_sink_past_its_deadline_neither_stalls_the_command_nor_loses_its_write() {
    let (_root, shell) = shell(Duration::from_millis(100));
    let request = ShellRequest::shell("head -c 2000000 /dev/zero", None)
        .with_timeout(Duration::from_secs(30));
    let slow = Behaviour::Slow(Duration::from_millis(1_500));
    let (capture, out, _err) = sinks(slow, Behaviour::Accept);
    let started = Instant::now();
    let ran = captured(&shell, request, capture).await;
    assert!(
        started.elapsed() < Duration::from_millis(1_400),
        "the run did not wait for the slow write: {:?}",
        started.elapsed()
    );
    assert!(ran.outcome.is_success());
    assert_eq!(
        ran.outcome.stdout.total_bytes, 2_000_000,
        "the pipe drained in full"
    );
    assert!(
        ran.stdout.is_none(),
        "an unfinished archive is uncertain, not complete"
    );
    assert!(
        out.finalized().is_none(),
        "nothing was finalized under a running write"
    );

    let (observed, reason, writes) = out
        .wait_for_finalize(Duration::from_secs(5))
        .await
        .expect("the sink is finalized once its write completes");
    assert_eq!(reason, CaptureReason::Timeout);
    assert_eq!(observed, 2_000_000);
    assert_eq!(writes, 1, "the overdue write finished before the finalize");
}

/// T26, command timeout: output cut off by the port's own timeout is archived as partial, and
/// the group — grandchild included — is reaped exactly as an uncaptured run reaps it.
#[tokio::test]
async fn a_timeout_mid_output_leaves_a_partial_archive_and_reaps_the_group() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pid_file = dir.path().join("grandchild.pid");
    let (_root, shell) = shell(Duration::from_secs(5));
    let script = format!(
        "head -c 100000 /dev/zero; sleep 300 & echo $! > {}; wait",
        quote(&pid_file.to_string_lossy())
    );
    let request = ShellRequest::shell(script, None).with_timeout(Duration::from_millis(700));
    let (capture, out, _err) = sinks(Behaviour::Accept, Behaviour::Accept);
    let ran = captured(&shell, request, capture).await;
    assert!(ran.outcome.timed_out);
    let stdout = ran.stdout.clone().expect("finalized");
    assert_eq!(stdout.receipt.reason, CaptureReason::Cancelled);
    assert_eq!(stdout.receipt.status, CaptureStatus::Partial);
    assert_eq!(
        out.bytes().len(),
        100_000,
        "everything read before the kill was kept"
    );
    let grandchild = recorded_pid(&pid_file);
    assert!(grandchild > 0, "the grandchild recorded its pid");
    assert!(
        wait_for_death(grandchild).await,
        "the grandchild did not survive"
    );
    assert_eq!(shell.live_groups(), 0);
}

/// A shutdown is a cut-off too: output ended by `kill_all` is not claimed as complete.
#[tokio::test]
async fn a_shutdown_mid_output_leaves_a_partial_archive() {
    let (_root, shell) = shell(Duration::from_secs(5));
    let request =
        ShellRequest::shell("printf before; sleep 300", None).with_timeout(Duration::from_secs(60));
    let (capture, out, _err) = sinks(Behaviour::Accept, Behaviour::Accept);
    let run = shell.run_with_capture(request, capture);
    let killer = async {
        tokio::time::sleep(Duration::from_millis(400)).await;
        shell.kill_all().await.expect("kill_all")
    };
    let (ran, signalled) = futures::join!(run, killer);
    let ran = ran.expect("the command ran");
    assert_eq!(signalled, 1);
    assert!(
        !ran.outcome.timed_out,
        "the shutdown, not the timeout, ended it"
    );
    assert_eq!(stdout_reason(&ran), Some(CaptureReason::Cancelled));
    assert_eq!(out.bytes(), b"before");
    assert_eq!(shell.live_groups(), 0);
}

/// Cancellation of the call is logical; the archive's physical work is not tied to it. A run
/// dropped mid-command still has its sink finalized once the pipe ends, and as cancelled.
#[tokio::test]
async fn a_dropped_run_still_finalizes_its_archive_as_cancelled() {
    let (_root, shell) = shell(Duration::from_secs(5));
    let request = ShellRequest::shell("printf abc; sleep 1", None);
    let (capture, out, _err) = sinks(Behaviour::Accept, Behaviour::Accept);
    let run = shell.run_with_capture(request, capture);
    let abandoned = tokio::time::timeout(Duration::from_millis(300), run).await;
    assert!(
        abandoned.is_err(),
        "the run was dropped before the command ended"
    );

    let (observed, reason, _writes) = out
        .wait_for_finalize(Duration::from_secs(5))
        .await
        .expect("the sink is finalized without its caller");
    assert_eq!(reason, CaptureReason::Cancelled);
    assert_eq!(observed, 3);
    assert_eq!(out.bytes(), b"abc");
    assert_eq!(shell.live_groups(), 0);
}
