//! Integration tests for what the store does for managed context: checkpoints, the shell
//! archive, deletion that retires an id, and garbage collection.
//!
//! An integration-test crate is entirely test code, where a panic *is* the assertion, so the
//! workspace's panic-family exemption is restated here.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use nanus_adapter_store::{ArchiveQuota, JsonlStore};
use nanus_domain::context::managed::{
    ArtifactId, ArtifactReceipt, CaptureReason, CaptureStatus, CaptureStream, Digest, Durability,
    ErrorCode, RawEncoding, limits,
};
use nanus_domain::{Session, SessionEvent, SessionId, ToolCallId, TurnEndReason};
use nanus_ports::{
    ArtifactError, ArtifactStore, CaptureFailure, CaptureLease, CaptureLimits, CheckpointError,
    CheckpointReason, CheckpointView, ExpectedCheckpoint, FinalizedArtifact, StoreError, StorePort,
};

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

/// Builds a session with one completed turn containing `messages`.
fn session(id: &str, messages: &[&str]) -> Session {
    let mut session = Session::new(SessionId::new(id), 1_700_000_000_000, "/work");
    session.append(SessionEvent::TurnStart { turn: 0 });
    for text in messages {
        session.append(SessionEvent::UserMessage {
            text: (*text).to_owned(),
        });
    }
    session.append(SessionEvent::TurnEnd {
        turn: 0,
        reason: TurnEndReason::Completed,
    });
    session
}

/// Builds a managed (version 3) session.
fn managed(id: &str, messages: &[&str]) -> Session {
    let mut session = session(id, messages);
    session.upgrade_to_managed_body();
    session
}

/// Opens a store over a fresh temporary home.
async fn store() -> (tempfile::TempDir, JsonlStore) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = JsonlStore::new(dir.path()).await.expect("store");
    (dir, store)
}

/// Commits `candidate` over `expected`, publishing `artifacts`.
async fn commit(
    store: &JsonlStore,
    candidate: &Session,
    expected: &ExpectedCheckpoint,
    artifacts: &[FinalizedArtifact],
) -> Result<nanus_domain::context::managed::CheckpointReceipt, CheckpointError> {
    store
        .checkpoint(CheckpointView {
            candidate,
            expected,
            reason: CheckpointReason::SettledStep,
            artifacts,
        })
        .await
}

/// Reads the bytes of a session's file.
fn file_bytes(store: &JsonlStore, id: &SessionId) -> Vec<u8> {
    std::fs::read(store.session_file(id).expect("path")).expect("the file reads")
}

/// Reserves a capture with the default limits.
async fn reserve(store: &JsonlStore, id: &SessionId, call: &str) -> CaptureLease {
    store
        .reserve_capture(id, &ToolCallId::new(call), CaptureLimits::default())
        .await
        .expect("a reservation")
}

/// Captures `bytes` on standard output through to end of file, returning the finalized proof.
async fn capture(
    store: &JsonlStore,
    id: &SessionId,
    call: &str,
    bytes: &[u8],
) -> FinalizedArtifact {
    let mut lease = reserve(store, id, call).await;
    let mut sink = lease.take_stdout().expect("a sink");
    sink.write(bytes).await.expect("a write");
    let observed = u64::try_from(bytes.len()).expect("a length");
    let done = sink.finalize(observed, CaptureReason::Eof).await;
    assert_eq!(done.receipt.status, CaptureStatus::Complete);
    done.artifact.expect("a finalized object")
}

/// A recognisable byte pattern of `length` bytes.
fn pattern(length: usize) -> Vec<u8> {
    (0..length)
        .map(|index| u8::try_from(index.wrapping_rem(251)).expect("a byte"))
        .collect()
}

/// The archive directory of one session.
fn archive_dir(store: &JsonlStore, id: &SessionId) -> std::path::PathBuf {
    store.session_dir(id).expect("dir").join("artifacts")
}

/// The object file of one finalized artifact.
fn object_file(store: &JsonlStore, artifact: &FinalizedArtifact) -> std::path::PathBuf {
    let id = artifact.receipt().artifact_id.clone().expect("an id");
    archive_dir(store, artifact.session()).join(format!("{}.raw", id.uuid()))
}

/// The files in one session's archive, by extension.
fn archive_files(store: &JsonlStore, id: &SessionId, ext: &str) -> usize {
    std::fs::read_dir(archive_dir(store, id)).map_or(0, |entries| {
        entries
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension().and_then(|e| e.to_str()) == Some(ext))
            .count()
    })
}

/// A receipt for an object that was never captured.
fn fabricated_receipt() -> ArtifactReceipt {
    ArtifactReceipt {
        artifact_id: ArtifactId::parse("a:0190a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b"),
        call_id: String::from("call-1"),
        stream: CaptureStream::Stdout,
        retained_bytes: 3,
        observed_bytes: 3,
        retained_sha256: Some(Digest::of(b"abc")),
        status: CaptureStatus::Complete,
        reason: CaptureReason::Eof,
        encoding: RawEncoding::Raw,
        chunk_sha256: vec![Digest::of(b"abc")],
    }
}

/// Appends a publication of `receipt` to a managed session.
fn publish(session: &mut Session, receipt: &ArtifactReceipt) {
    session.append(SessionEvent::ArtifactPublished {
        payload: Box::new(receipt.clone()),
    });
}

// ---------------------------------------------------------------------------
// Stored identity.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_session_that_is_not_stored_has_an_absent_identity() {
    let (_dir, store) = store().await;
    let identity = store.stored_identity(&SessionId::new("nobody")).await;
    assert_eq!(identity, Ok(ExpectedCheckpoint::Absent));
}

/// The identity is of the bytes on disk — blank lines and all — never of a re-encoding, and an
/// old body reports the version its header declares.
#[tokio::test]
async fn a_stored_identity_is_the_files_own_bytes_and_version() {
    let (_dir, store) = store().await;
    let saved = session("legacy", &["one", "two"]);
    store.save(&saved).await.expect("save");
    let body = saved
        .to_jsonl()
        .replacen("\"version\":2", "\"version\":1", 1)
        + "\n";
    std::fs::write(store.session_file(saved.id()).expect("path"), &body).expect("write");

    let identity = store
        .stored_identity(saved.id())
        .await
        .expect("an identity");
    assert_eq!(
        identity,
        ExpectedCheckpoint::Stored {
            body_version: 1,
            file_sha256: Digest::of(body.as_bytes()),
            event_count: 4,
        }
    );
    // A re-encoding would have been a version-2 file with another digest.
    assert_ne!(
        Digest::of(body.as_bytes()),
        Digest::of(saved.to_jsonl().as_bytes())
    );
}

#[tokio::test]
async fn a_file_with_no_readable_header_has_no_identity() {
    let (_dir, store) = store().await;
    let saved = session("damaged", &["one"]);
    store.save(&saved).await.expect("save");
    std::fs::write(store.session_file(saved.id()).expect("path"), "not json\n").expect("write");
    assert!(matches!(
        store.stored_identity(saved.id()).await,
        Err(StoreError::Corrupt { .. })
    ));
}

// ---------------------------------------------------------------------------
// Checkpoints.
// ---------------------------------------------------------------------------

/// A new session commits over `Absent`, and its receipt is the identity the next commit expects.
#[tokio::test]
async fn a_checkpoint_round_trips_from_absent_to_stored_and_on() {
    let (_dir, store) = store().await;
    let mut candidate = managed("fresh", &["hello"]);
    store
        .lock(candidate.id(), "a writer")
        .await
        .expect("claimed");

    let receipt = commit(&store, &candidate, &ExpectedCheckpoint::Absent, &[])
        .await
        .expect("committed");
    let bytes = file_bytes(&store, candidate.id());
    assert_eq!(receipt.frontier.prefix_sha256, Digest::of(&bytes));
    assert_eq!(receipt.frontier.event_count, 3);
    assert_eq!(receipt.frontier.projection_revision, 0);
    assert_eq!(receipt.body_digest, candidate.body_digest());
    assert_eq!(receipt.durability, Durability::ProcessCrash);
    let next = ExpectedCheckpoint::after(&receipt, candidate.body_version());
    assert_eq!(
        store.stored_identity(candidate.id()).await,
        Ok(next.clone())
    );
    assert_eq!(store.load(candidate.id()).await.expect("load"), candidate);

    candidate.append(SessionEvent::UserMessage {
        text: String::from("again"),
    });
    let second = commit(&store, &candidate, &next, &[])
        .await
        .expect("committed");
    assert_eq!(second.frontier.event_count, 4);
    assert_eq!(
        second.frontier.prefix_sha256,
        Digest::of(&file_bytes(&store, candidate.id()))
    );
    store.release_lock(candidate.id());
}

/// Another writer saved in between: the commit is refused and what that writer wrote stays.
#[tokio::test]
async fn a_checkpoint_over_a_stale_identity_is_not_committed_and_changes_nothing() {
    let (dir, store) = store().await;
    let candidate = managed("contested", &["mine"]);
    store
        .lock(candidate.id(), "a writer")
        .await
        .expect("claimed");
    let receipt = commit(&store, &candidate, &ExpectedCheckpoint::Absent, &[])
        .await
        .expect("committed");
    let expected = ExpectedCheckpoint::after(&receipt, candidate.body_version());

    let other = JsonlStore::new(dir.path()).await.expect("a second store");
    other
        .save(&session("contested", &["theirs"]))
        .await
        .expect("an unclaimed save");
    let theirs = file_bytes(&store, candidate.id());

    let mut next = candidate.clone();
    next.append(SessionEvent::UserMessage {
        text: String::from("more"),
    });
    assert_eq!(
        commit(&store, &next, &expected, &[]).await,
        Err(CheckpointError::NotCommitted(ErrorCode::StaleBase))
    );
    assert_eq!(
        file_bytes(&store, candidate.id()),
        theirs,
        "the other write survives"
    );

    // Expecting nothing over a stored file is the same mistake.
    assert_eq!(
        commit(&store, &next, &ExpectedCheckpoint::Absent, &[]).await,
        Err(CheckpointError::NotCommitted(ErrorCode::StaleBase))
    );
    store.release_lock(candidate.id());
}

/// A checkpoint is written under this process's claim or not at all.
#[tokio::test]
async fn a_checkpoint_without_the_claim_is_refused() {
    let (dir, store) = store().await;
    let candidate = managed("unclaimed", &["hello"]);
    assert_eq!(
        commit(&store, &candidate, &ExpectedCheckpoint::Absent, &[]).await,
        Err(CheckpointError::NotCommitted(ErrorCode::PolicyDenied))
    );
    assert!(!store.session_file(candidate.id()).expect("path").exists());

    // Held by somebody else is the same answer.
    let holder = JsonlStore::new(dir.path()).await.expect("a second store");
    holder
        .lock(candidate.id(), "the holder")
        .await
        .expect("claimed");
    assert_eq!(
        commit(&store, &candidate, &ExpectedCheckpoint::Absent, &[]).await,
        Err(CheckpointError::NotCommitted(ErrorCode::PolicyDenied))
    );
    holder.release_lock(candidate.id());

    // And once it is ours, the same request commits.
    store
        .lock(candidate.id(), "a writer")
        .await
        .expect("claimed");
    assert!(
        commit(&store, &candidate, &ExpectedCheckpoint::Absent, &[])
            .await
            .is_ok()
    );
    store.release_lock(candidate.id());
}

/// A message whose escaped length is exactly the record bound fits; one byte more is refused
/// before anything is written, and the original file survives (T15).
#[tokio::test]
async fn a_record_one_byte_over_its_bound_is_not_committed_and_the_original_survives() {
    let (_dir, store) = store().await;
    let original = managed("bounded", &["original"]);
    store
        .lock(original.id(), "a writer")
        .await
        .expect("claimed");
    let receipt = commit(&store, &original, &ExpectedCheckpoint::Absent, &[])
        .await
        .expect("committed");
    let expected = ExpectedCheckpoint::after(&receipt, original.body_version());
    let before = file_bytes(&store, original.id());

    let at_limit = with_escaped_record(&original, nanus_domain::content::RECORD_BYTES_MAX, 0);
    let over = with_escaped_record(&original, nanus_domain::content::RECORD_BYTES_MAX, 1);
    assert_eq!(
        commit(&store, &over, &expected, &[]).await,
        Err(CheckpointError::NotCommitted(ErrorCode::StorageCapacity))
    );
    assert_eq!(
        file_bytes(&store, original.id()),
        before,
        "the original survives"
    );

    let committed = commit(&store, &at_limit, &expected, &[]).await;
    assert!(
        committed.is_ok(),
        "exactly at the bound fits: {committed:?}"
    );
    store.release_lock(original.id());
}

/// Appends one user message whose encoded line is `bound + extra` bytes, mostly `\u0001`
/// escapes: six bytes on the wire for every byte in memory, so only the escaped length can
/// tell the two candidates apart.
fn with_escaped_record(base: &Session, bound: usize, extra: usize) -> Session {
    let mut probe = base.clone();
    probe.append(SessionEvent::UserMessage {
        text: String::new(),
    });
    let framing = probe
        .to_jsonl()
        .len()
        .checked_sub(base.to_jsonl().len())
        .and_then(|line| line.checked_sub(1))
        .expect("a line and its newline");
    let wanted = bound
        .checked_add(extra)
        .and_then(|total| total.checked_sub(framing))
        .expect("room for text");
    let escapes = wanted.checked_div(6).expect("six");
    let plain = wanted.checked_rem(6).expect("six");
    let text = "\u{1}".repeat(escapes) + &"a".repeat(plain);
    let mut session = base.clone();
    session.append(SessionEvent::UserMessage { text });
    let line = session
        .to_jsonl()
        .len()
        .checked_sub(base.to_jsonl().len())
        .and_then(|line| line.checked_sub(1));
    assert_eq!(
        line,
        bound.checked_add(extra),
        "the line is exactly the size asked for"
    );
    session
}

/// The whole-session bound is counted the same way: a candidate whose file would be exactly
/// 64 MiB commits, and one byte more — an escaped one — is refused with the original intact.
#[tokio::test]
async fn a_session_one_byte_over_its_bound_is_not_committed_and_the_original_survives() {
    let (_dir, store) = store().await;
    let original = managed("whole", &["original"]);
    store
        .lock(original.id(), "a writer")
        .await
        .expect("claimed");
    let receipt = commit(&store, &original, &ExpectedCheckpoint::Absent, &[])
        .await
        .expect("committed");
    let expected = ExpectedCheckpoint::after(&receipt, original.body_version());
    let before = file_bytes(&store, original.id());

    // Fill with plain records of about the record bound, estimating the length rather than
    // re-encoding 64 MiB each time round; a growing sequence number adds a digit now and then,
    // so the estimate is only trusted to stop the loop, and the exact length is read after it.
    let record = nanus_domain::content::RECORD_BYTES_MAX;
    let filler = record.checked_sub(64).expect("room");
    let mut big = original.clone();
    let mut estimate = original.to_jsonl().len();
    while nanus_domain::content::SESSION_BYTES_MAX.saturating_sub(estimate)
        > record.saturating_mul(2)
    {
        big.append(SessionEvent::UserMessage {
            text: "a".repeat(filler),
        });
        estimate = estimate.checked_add(record).expect("a length");
    }
    // Two last lines take exactly what is left, each with its newline and each within the
    // record bound; the second is the one the escapes tip over.
    let left = nanus_domain::content::SESSION_BYTES_MAX
        .checked_sub(big.to_jsonl().len())
        .and_then(|left| left.checked_sub(2))
        .expect("room for two lines");
    let line = left.checked_div(2).expect("two");
    let big = with_escaped_record(&big, left.checked_sub(line).expect("a line"), 0);
    let at_limit = with_escaped_record(&big, line, 0);
    let over = with_escaped_record(&big, line, 1);
    assert_eq!(
        at_limit.to_jsonl().len(),
        nanus_domain::content::SESSION_BYTES_MAX
    );

    assert_eq!(
        commit(&store, &over, &expected, &[]).await,
        Err(CheckpointError::NotCommitted(ErrorCode::StorageCapacity))
    );
    assert_eq!(
        file_bytes(&store, original.id()),
        before,
        "the original survives"
    );
    let committed = commit(&store, &at_limit, &expected, &[]).await;
    assert!(committed.is_ok(), "exactly at the bound fits");
    assert_eq!(
        store.stored_identity(original.id()).await,
        Ok(ExpectedCheckpoint::after(
            &committed.expect("a receipt"),
            at_limit.body_version()
        ))
    );
    store.release_lock(original.id());
}

/// A legacy body never carries a managed record, and asking it to is a corrupt candidate.
#[tokio::test]
async fn a_managed_record_in_a_legacy_body_is_not_committed() {
    let (_dir, store) = store().await;
    let mut candidate = session("legacy-body", &["hello"]);
    publish(&mut candidate, &fabricated_receipt());
    store
        .lock(candidate.id(), "a writer")
        .await
        .expect("claimed");
    assert_eq!(
        commit(&store, &candidate, &ExpectedCheckpoint::Absent, &[]).await,
        Err(CheckpointError::NotCommitted(ErrorCode::SourceCorrupt))
    );
    store.release_lock(candidate.id());
}

/// A receipt published for the first time must come with the archive's own proof, and the proof
/// must match storage: no acknowledged dangling reference (T09).
#[tokio::test]
async fn a_checkpoint_referencing_an_unfinalized_artifact_is_refused() {
    let (_dir, store) = store().await;
    let base = managed("evidence", &["run it"]);
    store.lock(base.id(), "a writer").await.expect("claimed");

    // A receipt with no proof at all.
    let mut candidate = base.clone();
    publish(&mut candidate, &fabricated_receipt());
    assert_eq!(
        commit(&store, &candidate, &ExpectedCheckpoint::Absent, &[]).await,
        Err(CheckpointError::NotCommitted(ErrorCode::InvalidReference))
    );
    // A proof whose object was never finalized.
    let proof = FinalizedArtifact::new(base.id().clone(), fabricated_receipt());
    assert_eq!(
        commit(&store, &candidate, &ExpectedCheckpoint::Absent, &[proof]).await,
        Err(CheckpointError::NotCommitted(ErrorCode::SourceUnavailable))
    );
    assert!(!store.session_file(base.id()).expect("path").exists());

    // A real capture commits.
    let artifact = capture(&store, base.id(), "call-1", b"evidence").await;
    let mut candidate = base.clone();
    publish(&mut candidate, artifact.receipt());
    let committed = commit(&store, &candidate, &ExpectedCheckpoint::Absent, &[artifact]).await;
    assert!(committed.is_ok(), "{committed:?}");
    store.release_lock(base.id());
}

/// Another session's proof is not this session's evidence.
#[tokio::test]
async fn a_checkpoint_with_another_sessions_proof_is_refused() {
    let (_dir, store) = store().await;
    let theirs = managed("theirs", &["one"]);
    let ours = managed("ours", &["two"]);
    store.lock(theirs.id(), "a writer").await.expect("claimed");
    store.lock(ours.id(), "a writer").await.expect("claimed");
    let artifact = capture(&store, theirs.id(), "call-1", b"theirs").await;
    let mut candidate = ours.clone();
    publish(&mut candidate, artifact.receipt());
    assert_eq!(
        commit(&store, &candidate, &ExpectedCheckpoint::Absent, &[artifact]).await,
        Err(CheckpointError::NotCommitted(ErrorCode::InvalidReference))
    );
    store.release_lock(theirs.id());
    store.release_lock(ours.id());
}

// ---------------------------------------------------------------------------
// Capture.
// ---------------------------------------------------------------------------

/// A whole stream finalizes complete, verifies, and reads back across a chunk boundary.
#[tokio::test]
async fn a_capture_finalizes_complete_and_reads_back_verified() {
    let (_dir, store) = store().await;
    let id = SessionId::new("captured");
    store.lock(&id, "a writer").await.expect("claimed");
    let bytes = pattern(200_000);
    let artifact = capture(&store, &id, "call-1", &bytes).await;
    let receipt = artifact.receipt();
    assert!(receipt.validate().is_ok());
    assert_eq!(receipt.retained_bytes, 200_000);
    assert_eq!(receipt.chunk_sha256.len(), 4);
    assert_eq!(receipt.retained_sha256, Some(Digest::of(&bytes)));
    assert_eq!(store.verify(&artifact).await, Ok(()));
    assert_eq!(
        archive_files(&store, &id, "partial"),
        0,
        "the object was renamed"
    );

    let read = store
        .read_range(&id, receipt, 65_000, 1_000)
        .await
        .expect("a range");
    assert_eq!(read.as_slice(), bytes.get(65_000..66_000).expect("a slice"));
    // A range past the end is clamped to the object.
    let tail = store
        .read_range(&id, receipt, 199_990, 100)
        .await
        .expect("a range");
    assert_eq!(tail.as_slice(), bytes.get(199_990..).expect("a slice"));
    store.release_lock(&id);
}

/// Past the per-stream cap the stream keeps draining and the receipt says partial.
#[tokio::test]
async fn a_stream_past_its_cap_is_partial_and_says_so() {
    let (_dir, store) = store().await;
    let id = SessionId::new("capped");
    store.lock(&id, "a writer").await.expect("claimed");
    let limits = CaptureLimits {
        stream_bytes: 100,
        ..CaptureLimits::default()
    };
    let mut lease = store
        .reserve_capture(&id, &ToolCallId::new("call-1"), limits)
        .await
        .expect("a reservation");
    let mut sink = lease.take_stdout().expect("a sink");
    sink.write(&pattern(150)).await.expect("a write");
    let done = sink.finalize(150, CaptureReason::Eof).await;
    assert_eq!(done.receipt.status, CaptureStatus::Partial);
    assert_eq!(done.receipt.reason, CaptureReason::Quota);
    assert_eq!(done.receipt.retained_bytes, 100);
    assert_eq!(done.receipt.observed_bytes, 150);
    assert!(done.receipt.validate().is_ok());
    let artifact = done.artifact.expect("a partial object is still an object");
    assert_eq!(store.verify(&artifact).await, Ok(()));

    // A cancelled stream is partial too, even with every byte it saw retained.
    let mut sink = lease.take_stderr().expect("a sink");
    sink.write(b"bye").await.expect("a write");
    let done = sink.finalize(3, CaptureReason::Cancelled).await;
    assert_eq!(done.receipt.status, CaptureStatus::Partial);
    assert_eq!(done.receipt.reason, CaptureReason::Cancelled);
    store.release_lock(&id);
}

/// An empty stream at end of file is a complete, empty object; a stream with bytes of which
/// nothing was kept is unavailable and leaves nothing behind.
#[tokio::test]
async fn nothing_retained_is_complete_only_when_nothing_was_observed() {
    let (_dir, store) = store().await;
    let id = SessionId::new("empty");
    store.lock(&id, "a writer").await.expect("claimed");
    let mut lease = reserve(&store, &id, "call-1").await;
    let done = lease
        .take_stdout()
        .expect("a sink")
        .finalize(0, CaptureReason::Eof)
        .await;
    assert_eq!(done.receipt.status, CaptureStatus::Complete);
    assert_eq!(done.receipt.retained_sha256, Some(Digest::empty()));
    assert!(done.receipt.chunk_sha256.is_empty());

    let done = lease
        .take_stderr()
        .expect("a sink")
        .finalize(42, CaptureReason::ReadError)
        .await;
    assert_eq!(done.receipt.status, CaptureStatus::Unavailable);
    assert!(done.receipt.artifact_id.is_none() && done.artifact.is_none());
    assert!(done.receipt.validate().is_ok());
    drop(lease);
    assert_eq!(archive_files(&store, &id, "partial"), 0);
    assert_eq!(
        archive_files(&store, &id, "lease"),
        0,
        "the reservation is returned"
    );
    store.release_lock(&id);
}

#[tokio::test]
async fn reserving_capture_requires_the_sessions_claim() {
    let (_dir, store) = store().await;
    let id = SessionId::new("unclaimed");
    let refused = store
        .reserve_capture(&id, &ToolCallId::new("call-1"), CaptureLimits::default())
        .await;
    assert!(matches!(refused, Err(CaptureFailure::Io(_))));
    store.lock(&id, "a writer").await.expect("claimed");
    assert!(
        store
            .reserve_capture(&id, &ToolCallId::new("call-1"), CaptureLimits::default())
            .await
            .is_ok()
    );
    store.release_lock(&id);
}

// ---------------------------------------------------------------------------
// Reading evidence back (T27).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_missing_object_is_unavailable() {
    let (_dir, store) = store().await;
    let id = SessionId::new("lost");
    store.lock(&id, "a writer").await.expect("claimed");
    let artifact = capture(&store, &id, "call-1", b"gone soon").await;
    std::fs::remove_file(object_file(&store, &artifact)).expect("remove");
    assert_eq!(
        store.verify(&artifact).await,
        Err(ArtifactError::Unavailable)
    );
    assert_eq!(
        store.read_range(&id, artifact.receipt(), 0, 4).await,
        Err(ArtifactError::Unavailable)
    );
    store.release_lock(&id);
}

#[tokio::test]
async fn a_flipped_byte_is_corrupt() {
    let (_dir, store) = store().await;
    let id = SessionId::new("flipped");
    store.lock(&id, "a writer").await.expect("claimed");
    let artifact = capture(&store, &id, "call-1", &pattern(70_000)).await;
    let path = object_file(&store, &artifact);
    let mut bytes = std::fs::read(&path).expect("read");
    let byte = bytes.get_mut(66_000).expect("a byte");
    *byte = !*byte;
    std::fs::write(&path, &bytes).expect("write");

    assert_eq!(store.verify(&artifact).await, Err(ArtifactError::Corrupt));
    // A range in the damaged chunk is refused; one in the intact chunk still reads.
    assert_eq!(
        store.read_range(&id, artifact.receipt(), 66_000, 10).await,
        Err(ArtifactError::Corrupt)
    );
    assert!(
        store
            .read_range(&id, artifact.receipt(), 0, 10)
            .await
            .is_ok()
    );
    // A truncated object fails its length check before any chunk is read.
    std::fs::write(&path, bytes.get(..100).expect("a prefix")).expect("write");
    assert_eq!(
        store.read_range(&id, artifact.receipt(), 0, 10).await,
        Err(ArtifactError::Corrupt)
    );
    store.release_lock(&id);
}

#[tokio::test]
async fn another_sessions_artifact_is_denied() {
    let (_dir, store) = store().await;
    let theirs = SessionId::new("theirs");
    let ours = SessionId::new("ours");
    store.lock(&theirs, "a writer").await.expect("claimed");
    store.lock(&ours, "a writer").await.expect("claimed");
    let artifact = capture(&store, &theirs, "call-1", b"private").await;
    assert_eq!(
        store.read_range(&ours, artifact.receipt(), 0, 3).await,
        Err(ArtifactError::Denied)
    );
    let borrowed = FinalizedArtifact::new(ours.clone(), artifact.receipt().clone());
    assert_eq!(store.verify(&borrowed).await, Err(ArtifactError::Denied));
    // Its own session reads it.
    assert!(
        store
            .read_range(&theirs, artifact.receipt(), 0, 3)
            .await
            .is_ok()
    );
    store.release_lock(&theirs);
    store.release_lock(&ours);
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlinked_archive_is_refused() {
    let (_dir, store) = store().await;
    let id = SessionId::new("linked");
    store.lock(&id, "a writer").await.expect("claimed");
    let artifact = capture(&store, &id, "call-1", b"real").await;
    let outside = tempfile::tempdir().expect("outside");
    let archive = archive_dir(&store, &id);
    std::fs::rename(&archive, outside.path().join("moved")).expect("move");
    std::os::unix::fs::symlink(outside.path().join("moved"), &archive).expect("symlink");
    assert_eq!(
        store.read_range(&id, artifact.receipt(), 0, 4).await,
        Err(ArtifactError::Denied)
    );
    assert_eq!(store.verify(&artifact).await, Err(ArtifactError::Denied));
    let refused = store
        .reserve_capture(&id, &ToolCallId::new("call-2"), CaptureLimits::default())
        .await;
    assert!(
        refused.is_err(),
        "nothing is written through a linked archive"
    );

    // A linked object is refused the same way.
    std::fs::remove_file(&archive).expect("unlink");
    std::fs::create_dir(&archive).expect("a real archive");
    std::os::unix::fs::symlink(
        outside
            .path()
            .join("moved")
            .join(object_file(&store, &artifact).file_name().unwrap()),
        object_file(&store, &artifact),
    )
    .expect("symlink");
    assert_eq!(store.verify(&artifact).await, Err(ArtifactError::Denied));
    store.release_lock(&id);
}

/// One call reads a bounded amount: a range whose chunks pass the work bound is refused.
#[tokio::test]
async fn a_range_past_the_work_bound_is_refused() {
    let (_dir, store) = store().await;
    let id = SessionId::new("big");
    store.lock(&id, "a writer").await.expect("claimed");
    let artifact = capture(&store, &id, "call-1", &pattern(400_000)).await;
    let receipt = artifact.receipt();
    assert_eq!(
        store.read_range(&id, receipt, 0, 300_000).await,
        Err(ArtifactError::Denied)
    );
    // Four whole chunks are the bound, so a range that straddles five is refused even when short.
    assert_eq!(
        store.read_range(&id, receipt, 65_000, 200_000).await,
        Err(ArtifactError::Denied)
    );
    assert_eq!(
        store
            .read_range(&id, receipt, 0, 200_000)
            .await
            .map(|bytes| bytes.len()),
        Ok(200_000)
    );
    store.release_lock(&id);
}

// ---------------------------------------------------------------------------
// Quota (T29).
// ---------------------------------------------------------------------------

/// Reservations count against the session's quota while they are held, and a refusal leaves
/// the earlier ones standing.
#[tokio::test]
async fn reservations_past_the_session_quota_are_refused_while_earlier_ones_hold() {
    let (_dir, store) = store().await;
    let id = SessionId::new("busy");
    store.lock(&id, "a writer").await.expect("claimed");
    // 128 MiB a session, 16 MiB a call: eight fit.
    let mut held = Vec::new();
    for call in 0..8 {
        held.push(reserve(&store, &id, &format!("call-{call}")).await);
    }
    let refused = store
        .reserve_capture(&id, &ToolCallId::new("call-8"), CaptureLimits::default())
        .await;
    assert_eq!(refused.err(), Some(CaptureFailure::Quota));
    assert_eq!(
        archive_files(&store, &id, "lease"),
        8,
        "the earlier ones still hold"
    );

    drop(held.pop());
    assert!(
        store
            .reserve_capture(&id, &ToolCallId::new("call-8"), CaptureLimits::default())
            .await
            .is_ok(),
        "a returned reservation is room again"
    );
    store.release_lock(&id);
}

/// The store's quota counts every session's archive.
#[tokio::test]
async fn the_store_quota_counts_every_session() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = JsonlStore::new(dir.path())
        .await
        .expect("store")
        .with_archive_quota(ArchiveQuota {
            session_bytes: limits::CAPTURE_SESSION_BYTES_MAX,
            store_bytes: limits::CAPTURE_STREAM_BYTES_MAX.saturating_mul(4),
        });
    let first = SessionId::new("first");
    let second = SessionId::new("second");
    store.lock(&first, "a writer").await.expect("claimed");
    store.lock(&second, "a writer").await.expect("claimed");
    let one = reserve(&store, &first, "call-1").await;
    let two = reserve(&store, &first, "call-2").await;
    let refused = store
        .reserve_capture(
            &second,
            &ToolCallId::new("call-3"),
            CaptureLimits::default(),
        )
        .await;
    assert_eq!(refused.err(), Some(CaptureFailure::Quota));
    drop(one);
    assert!(
        store
            .reserve_capture(
                &second,
                &ToolCallId::new("call-3"),
                CaptureLimits::default()
            )
            .await
            .is_ok()
    );
    drop(two);
    store.release_lock(&first);
    store.release_lock(&second);
}

/// Dropping a lease returns only what was not used: finalized bytes stay charged.
#[tokio::test]
async fn finalized_bytes_stay_charged_after_their_lease_is_dropped() {
    let dir = tempfile::tempdir().expect("tempdir");
    let call = limits::CAPTURE_STREAM_BYTES_MAX.saturating_mul(2);
    let store = JsonlStore::new(dir.path())
        .await
        .expect("store")
        .with_archive_quota(ArchiveQuota {
            session_bytes: call.saturating_add(10),
            store_bytes: limits::CAPTURE_STORE_BYTES_MAX,
        });
    let id = SessionId::new("charged");
    store.lock(&id, "a writer").await.expect("claimed");
    let artifact = capture(&store, &id, "call-1", &pattern(11)).await;
    assert_eq!(archive_files(&store, &id, "lease"), 0, "the lease is gone");
    // Eleven bytes remain charged, and ten are all the room a whole call has beside them.
    let refused = store
        .reserve_capture(&id, &ToolCallId::new("call-2"), CaptureLimits::default())
        .await;
    assert_eq!(refused.err(), Some(CaptureFailure::Quota));

    std::fs::remove_file(object_file(&store, &artifact)).expect("remove");
    assert!(
        store
            .reserve_capture(&id, &ToolCallId::new("call-2"), CaptureLimits::default())
            .await
            .is_ok(),
        "removed bytes are reclaimed"
    );
    store.release_lock(&id);
}

// ---------------------------------------------------------------------------
// Deletion (T29).
// ---------------------------------------------------------------------------

/// The retirement marker of an id whose encoding is itself.
fn marker(store: &JsonlStore, id: &str) -> std::path::PathBuf {
    store.home_path().join("retired").join(id)
}

#[tokio::test]
async fn deleting_a_session_another_writer_holds_is_refused() {
    let (dir, store) = store().await;
    let saved = session("held", &["hello"]);
    store.save(&saved).await.expect("save");
    let holder = JsonlStore::new(dir.path()).await.expect("a second store");
    holder
        .lock(saved.id(), "the holder")
        .await
        .expect("claimed");

    match store.delete(saved.id()).await {
        Err(StoreError::Locked { owner, .. }) => assert_eq!(owner, "the holder"),
        other => panic!("a held session must not be deleted: {other:?}"),
    }
    assert_eq!(store.load(saved.id()).await.expect("still there"), saved);
    assert!(!marker(&store, "held").exists(), "nothing was retired");

    holder.release_lock(saved.id());
    store
        .delete(saved.id())
        .await
        .expect("deleted once it is free");
    assert!(marker(&store, "held").exists());
}

/// The holder deletes its own conversation, and the claim ends with it.
#[tokio::test]
async fn the_holder_may_delete_its_own_session() {
    let (dir, store) = store().await;
    let saved = session("mine", &["hello"]);
    store.save(&saved).await.expect("save");
    store.lock(saved.id(), "a writer").await.expect("claimed");
    store
        .delete(saved.id())
        .await
        .expect("deleted by its holder");
    assert!(!store.session_dir(saved.id()).expect("dir").exists());
    assert!(matches!(
        store.lock(saved.id(), "a writer").await,
        Err(StoreError::Retired { .. })
    ));
    let other = JsonlStore::new(dir.path()).await.expect("a second store");
    assert!(matches!(
        other.lock(saved.id(), "another writer").await,
        Err(StoreError::Retired { .. })
    ));
}

/// A stale handle saving the old conversation is refused, and nothing comes back.
#[tokio::test]
async fn a_stale_save_after_deletion_is_retired_and_recreates_nothing() {
    let (_dir, store) = store().await;
    let saved = session("stale", &["hello"]);
    store.save(&saved).await.expect("save");
    store.delete(saved.id()).await.expect("delete");

    assert!(matches!(
        store.save(&saved).await,
        Err(StoreError::Retired { .. })
    ));
    assert!(!store.session_dir(saved.id()).expect("dir").exists());
    assert!(store.list().await.expect("list").is_empty());
    assert!(matches!(
        store.load(saved.id()).await,
        Err(StoreError::NotFound { .. })
    ));
    assert!(matches!(
        store.name(saved.id(), "revived").await,
        Err(StoreError::Retired { .. })
    ));
    assert_eq!(
        commit(&store, &saved, &ExpectedCheckpoint::Absent, &[]).await,
        Err(CheckpointError::NotCommitted(ErrorCode::PolicyDenied))
    );
    assert!(!store.session_dir(saved.id()).expect("dir").exists());
}

/// An id that never was a session is not burned by deleting it.
#[tokio::test]
async fn deleting_an_absent_id_does_not_retire_it() {
    let (_dir, store) = store().await;
    let id = SessionId::new("never");
    store.delete(&id).await.expect("absent is gone already");
    assert!(!marker(&store, "never").exists());
    store
        .save(&session("never", &["first"]))
        .await
        .expect("the id is still usable");
}

/// A version-1 session is deleted and retired like any other.
#[tokio::test]
async fn a_legacy_session_is_retired_too() {
    let (_dir, store) = store().await;
    let saved = session("old", &["hello"]);
    store.save(&saved).await.expect("save");
    let body = saved
        .to_jsonl()
        .replacen("\"version\":2", "\"version\":1", 1);
    std::fs::write(store.session_file(saved.id()).expect("path"), body).expect("write");
    assert!(store.load(saved.id()).await.is_ok(), "a v1 log reads");
    store.delete(saved.id()).await.expect("delete");
    assert!(matches!(
        store.save(&saved).await,
        Err(StoreError::Retired { .. })
    ));
}

/// A deletion a crash interrupted is finished by the next open — forwards, never back.
#[tokio::test]
async fn an_interrupted_deletion_is_finished_on_open_and_never_reversed() {
    let (dir, store) = store().await;
    let retired = session("half-deleted", &["hello"]);
    store.save(&retired).await.expect("save");
    // Retired, but the crash came before the move.
    std::fs::create_dir_all(store.home_path().join("retired")).expect("retired dir");
    std::fs::write(marker(&store, "half-deleted"), "half-deleted\n").expect("marker");
    // Moved, but the crash came before the removal.
    let trashed = store.home_path().join("trash").join("gone.0190a1b2");
    std::fs::create_dir_all(&trashed).expect("trash entry");
    std::fs::write(trashed.join("session.jsonl"), "{}\n").expect("a file");
    drop(store);

    let reopened = JsonlStore::new(dir.path()).await.expect("reopened");
    assert!(!reopened.session_dir(retired.id()).expect("dir").exists());
    assert!(!trashed.exists(), "the trash is emptied");
    assert!(reopened.list().await.expect("list").is_empty());
    assert!(
        !reopened
            .session_dir(&SessionId::new("gone"))
            .expect("dir")
            .exists()
    );
    assert!(matches!(
        reopened.save(&retired).await,
        Err(StoreError::Retired { .. })
    ));
}

/// Deleting a session returns its archive bytes to the store's quota.
#[tokio::test]
async fn deletion_reclaims_archive_quota() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = JsonlStore::new(dir.path())
        .await
        .expect("store")
        .with_archive_quota(ArchiveQuota {
            session_bytes: limits::CAPTURE_SESSION_BYTES_MAX,
            store_bytes: limits::CAPTURE_STREAM_BYTES_MAX.saturating_mul(2),
        });
    let doomed = managed("doomed", &["one"]);
    let next = SessionId::new("next");
    store.lock(doomed.id(), "a writer").await.expect("claimed");
    store.lock(&next, "a writer").await.expect("claimed");
    capture(&store, doomed.id(), "call-1", &pattern(10)).await;
    let refused = store
        .reserve_capture(&next, &ToolCallId::new("call-2"), CaptureLimits::default())
        .await;
    assert_eq!(refused.err(), Some(CaptureFailure::Quota));

    store
        .delete(doomed.id())
        .await
        .expect("deleted by its holder");
    assert!(
        store
            .reserve_capture(&next, &ToolCallId::new("call-2"), CaptureLimits::default())
            .await
            .is_ok()
    );
    store.release_lock(&next);
}

// ---------------------------------------------------------------------------
// Garbage collection (T09, T29).
// ---------------------------------------------------------------------------

/// Unreferenced objects and orphans go; a referenced object stays and still reads.
#[tokio::test]
async fn collection_removes_unreferenced_objects_and_keeps_referenced_ones() {
    let (_dir, store) = store().await;
    let base = managed("collected", &["run it"]);
    store.lock(base.id(), "a writer").await.expect("claimed");
    let kept = capture(&store, base.id(), "call-1", b"referenced").await;
    let dropped = capture(&store, base.id(), "call-2", b"never published").await;
    // A crash after a write and before finalize leaves a partial that no receipt names.
    let mut lease = store
        .reserve_capture(
            base.id(),
            &ToolCallId::new("call-3"),
            CaptureLimits {
                staging_bytes: 2,
                ..CaptureLimits::default()
            },
        )
        .await
        .expect("a reservation");
    let mut sink = lease.take_stdout().expect("a sink");
    sink.write(b"interrupted").await.expect("a write");
    drop(sink);
    drop(lease);
    assert_eq!(archive_files(&store, base.id(), "partial"), 1, "an orphan");

    let mut candidate = base.clone();
    publish(&mut candidate, kept.receipt());
    commit(
        &store,
        &candidate,
        &ExpectedCheckpoint::Absent,
        std::slice::from_ref(&kept),
    )
    .await
    .expect("committed");
    store.release_lock(base.id());

    let report = store.collect_garbage(base.id()).await.expect("collected");
    assert_eq!(report.removed_objects, 2);
    assert_eq!(report.kept_objects, 1);
    assert_eq!(store.verify(&kept).await, Ok(()));
    assert_eq!(
        store.verify(&dropped).await,
        Err(ArtifactError::Unavailable)
    );
    assert_eq!(archive_files(&store, base.id(), "partial"), 0);
}

#[tokio::test]
async fn collection_refuses_a_held_session() {
    let (dir, store) = store().await;
    let saved = managed("held", &["one"]);
    store.save(&saved).await.expect("save");
    store.lock(saved.id(), "a writer").await.expect("claimed");
    assert!(matches!(
        store.collect_garbage(saved.id()).await,
        Err(StoreError::Locked { .. })
    ));
    let other = JsonlStore::new(dir.path()).await.expect("a second store");
    assert!(matches!(
        other.collect_garbage(saved.id()).await,
        Err(StoreError::Locked { .. })
    ));
    store.release_lock(saved.id());
    assert!(other.collect_garbage(saved.id()).await.is_ok());
}

/// A log that does not validate is never swept against.
#[tokio::test]
async fn a_corrupt_log_prevents_collection() {
    let (_dir, store) = store().await;
    let base = managed("damaged", &["one"]);
    store.lock(base.id(), "a writer").await.expect("claimed");
    capture(&store, base.id(), "call-1", b"keep me").await;
    store.save(&base).await.expect("save");
    store.release_lock(base.id());
    std::fs::write(store.session_file(base.id()).expect("path"), "{broken\n").expect("write");
    assert!(matches!(
        store.collect_garbage(base.id()).await,
        Err(StoreError::Corrupt { .. })
    ));
    assert_eq!(
        archive_files(&store, base.id(), "raw"),
        1,
        "nothing was removed"
    );
}

/// A lease whose holder is alive protects its objects; one whose holder is gone does not.
#[tokio::test]
async fn collection_leaves_a_live_lease_alone_and_clears_a_dead_one() {
    let (_dir, store) = store().await;
    let base = managed("leased", &["one"]);
    store.lock(base.id(), "a writer").await.expect("claimed");
    store.save(&base).await.expect("save");
    let mut live = reserve(&store, base.id(), "call-1").await;
    let mut sink = live.take_stdout().expect("a sink");
    store.release_lock(base.id());

    // A crashed holder's lease: a marker nobody has locked, covering a partial.
    let archive = archive_dir(&store, base.id());
    let orphan = "0190a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b";
    std::fs::write(archive.join(format!("{orphan}.partial")), b"lost").expect("partial");
    let other = "0190a1b2-0000-7000-8000-000000000000";
    std::fs::write(archive.join(format!("16_{orphan}_{other}.lease")), b"").expect("a dead lease");

    let report = store.collect_garbage(base.id()).await.expect("collected");
    assert_eq!(report.live_leases, 1);
    assert_eq!(report.dead_leases, 1);
    assert_eq!(report.removed_objects, 1, "only the dead lease's orphan");
    assert_eq!(
        archive_files(&store, base.id(), "partial"),
        2,
        "the live lease's two"
    );

    // The live sink still works after the collection.
    sink.write(b"still here").await.expect("a write");
    assert_eq!(
        sink.finalize(10, CaptureReason::Eof).await.receipt.status,
        CaptureStatus::Complete
    );
    drop(live);
}
