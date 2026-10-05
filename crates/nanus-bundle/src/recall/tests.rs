//! Recall: bounded work, honest coverage, sealed cursors, and evidence that is what it says.

use nanus_domain::context::managed::{
    ArtifactId, CaptureReason, CaptureStream, RawEncoding, state,
};
use nanus_domain::{SessionId, ToolCallId};
use nanus_ports::{CaptureFailure, CaptureLease, CaptureLimits, FinalizedArtifact, LocalBoxFuture};

use super::*;

/// An archive holding one object, which can be told to report it corrupt.
struct Archive {
    bytes: Vec<u8>,
    corrupt: bool,
}

impl ArtifactStore for Archive {
    fn reserve_capture<'a>(
        &'a self,
        _: &'a SessionId,
        _: &'a ToolCallId,
        _: CaptureLimits,
    ) -> LocalBoxFuture<'a, Result<CaptureLease, CaptureFailure>> {
        Box::pin(async { Err(CaptureFailure::Unsupported) })
    }

    fn read_range<'a>(
        &'a self,
        _: &'a SessionId,
        _: &'a ArtifactReceipt,
        offset: u64,
        length: u64,
    ) -> LocalBoxFuture<'a, Result<Vec<u8>, ArtifactError>> {
        Box::pin(async move {
            if self.corrupt {
                return Err(ArtifactError::Corrupt);
            }
            let start = usize::try_from(offset).unwrap_or(usize::MAX);
            let end = start
                .saturating_add(usize::try_from(length).unwrap_or(0))
                .min(self.bytes.len());
            Ok(self.bytes.get(start..end).unwrap_or_default().to_vec())
        })
    }

    fn verify<'a>(
        &'a self,
        _: &'a FinalizedArtifact,
    ) -> LocalBoxFuture<'a, Result<(), ArtifactError>> {
        Box::pin(async { Ok(()) })
    }
}

fn id() -> ArtifactId {
    ArtifactId::parse("a:0190a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b").unwrap_or_else(|| unreachable!())
}

fn receipt(bytes: &[u8]) -> ArtifactReceipt {
    let chunks = bytes
        .chunks(limits::ARTIFACT_CHUNK_BYTES)
        .map(Digest::of)
        .collect();
    ArtifactReceipt {
        artifact_id: Some(id()),
        call_id: "c1".into(),
        stream: CaptureStream::Stdout,
        retained_bytes: u64::try_from(bytes.len()).unwrap_or(0),
        observed_bytes: u64::try_from(bytes.len()).unwrap_or(0),
        retained_sha256: Some(Digest::of(bytes)),
        status: CaptureStatus::Complete,
        reason: CaptureReason::Eof,
        encoding: RawEncoding::Raw,
        chunk_sha256: chunks,
    }
}

fn session(text: &str, artifact: Option<&[u8]>) -> Session {
    let mut session = Session::new(SessionId::new("s"), 0, "/w");
    session.upgrade_to_managed_body();
    session.append(SessionEvent::UserMessage {
        text: text.to_owned(),
    });
    if let Some(bytes) = artifact {
        session.append(SessionEvent::ArtifactPublished {
            payload: Box::new(receipt(bytes)),
        });
    }
    session
}

fn scope<'a>(
    session: &'a Session,
    archive: Option<&'a dyn ArtifactStore>,
    key: &'a [u8],
) -> Scope<'a> {
    Scope {
        session,
        frontier: state::frontier(session, 0).unwrap_or_else(|_| unreachable!()),
        durable: 0,
        archive,
        key,
    }
}

fn search(query: &str, cursor: Option<String>, limit: u32) -> ContextRecallInput {
    ContextRecallInput {
        action: RecallAction::Search,
        query: Some(query.to_owned()),
        target: None,
        cursor,
        limit,
        max_bytes: 8_192,
        encoding: RecallEncoding::Text,
    }
}

fn read(target: RecallTarget, max_bytes: u32, encoding: RecallEncoding) -> ContextRecallInput {
    ContextRecallInput {
        action: RecallAction::Read,
        query: None,
        target: Some(target),
        cursor: None,
        limit: 1,
        max_bytes,
        encoding,
    }
}

fn artifact_target(offset: u64, length: u64) -> RecallTarget {
    RecallTarget {
        kind: SourceKind::Artifact,
        event_seq: None,
        block_index: None,
        artifact_id: Some(id()),
        offset,
        length,
        field: SourceField::Artifact,
    }
}

/// T24: a scan past the work bound returns partial coverage and a cursor that advances, even
/// with no hit, and a later page finds the marker past the bound.
#[test]
fn a_scan_past_the_work_bound_is_partial_and_resumes() {
    let mut text = "a".repeat(limits::RECALL_WORK_BYTES.saturating_add(10));
    text.push_str("MARKER");
    let session = session(&text, None);
    let scope = scope(&session, None, b"key");
    let first = futures::executor::block_on(recall(&scope, &search("MARKER", None, 5)));
    assert_eq!(first.status, RecallStatus::Ok);
    assert!(first.hits.is_empty());
    assert_eq!(first.coverage, Coverage::Partial);
    let cursor = first.next_cursor.unwrap_or_default();
    let second = futures::executor::block_on(recall(&scope, &search("MARKER", Some(cursor), 5)));
    assert_eq!(second.hits.len(), 1, "{second:?}");
    assert_eq!(second.coverage, Coverage::Complete);
    let offset = u64::try_from(text.len().saturating_sub(6)).unwrap_or(0);
    assert_eq!(second.hits[0].source.offset, offset);
    assert_eq!(
        second.hits[0].source.source_digest,
        Digest::of(text.as_bytes())
    );
}

/// A cursor from another key — another process — or for another query is expired.
#[test]
fn a_cursor_is_bound_to_its_key_and_query() {
    let text = format!("{}x", "b".repeat(limits::RECALL_WORK_BYTES));
    let session = session(&text, None);
    let issued = futures::executor::block_on(recall(
        &scope(&session, None, b"one"),
        &search("x", None, 5),
    ));
    let cursor = issued.next_cursor.unwrap_or_default();
    let restarted = futures::executor::block_on(recall(
        &scope(&session, None, b"two"),
        &search("x", Some(cursor.clone()), 5),
    ));
    assert_eq!(restarted.error_code, Some(ErrorCode::CursorExpired));
    let other_query = futures::executor::block_on(recall(
        &scope(&session, None, b"one"),
        &search("y", Some(cursor), 5),
    ));
    assert_eq!(other_query.error_code, Some(ErrorCode::CursorExpired));
}

/// T24: invalid UTF-8 in an artifact is reported as replaced in text, and exact in base64.
#[test]
fn invalid_bytes_are_replaced_and_reported_or_returned_exactly() {
    let bytes = [b'o', b'k', 0xff, 0xfe, b'!'];
    let session = session("go", Some(&bytes));
    let archive = Archive {
        bytes: bytes.to_vec(),
        corrupt: false,
    };
    let scope = scope(&session, Some(&archive), b"key");
    let text = futures::executor::block_on(recall(
        &scope,
        &read(artifact_target(0, 5), 100, RecallEncoding::Text),
    ));
    assert!(text.invalid_bytes_replaced);
    assert_eq!(text.actual_offset, Some(0));
    assert_eq!(text.next_offset, None);
    let raw = futures::executor::block_on(recall(
        &scope,
        &read(artifact_target(1, 3), 100, RecallEncoding::Base64),
    ));
    assert_eq!(raw.data.as_deref(), Some("a//+"));
    assert!(!raw.invalid_bytes_replaced);
    assert_eq!(raw.next_offset, Some(4));
    assert_eq!(
        raw.source.map(|source| source.source_digest),
        Some(Digest::of(&bytes))
    );
}

/// T27: a corrupt object is reported corrupt and a missing one unavailable — never replaced.
#[test]
fn missing_or_corrupt_evidence_is_never_substituted() {
    let bytes = b"evidence".to_vec();
    let session = session("go", Some(&bytes));
    let corrupt = Archive {
        bytes: b"something else".to_vec(),
        corrupt: true,
    };
    let result = futures::executor::block_on(recall(
        &scope(&session, Some(&corrupt), b"key"),
        &read(artifact_target(0, 8), 100, RecallEncoding::Text),
    ));
    assert_eq!(result.status, RecallStatus::Corrupt);
    assert!(result.data.is_none());
    let absent = futures::executor::block_on(recall(
        &scope(&session, None, b"key"),
        &read(artifact_target(0, 8), 100, RecallEncoding::Text),
    ));
    assert_eq!(absent.status, RecallStatus::Unavailable);
    let foreign = futures::executor::block_on(recall(
        &scope(&session, Some(&corrupt), b"key"),
        &read(
            RecallTarget {
                artifact_id: ArtifactId::parse("a:00000000-0000-7000-8000-000000000000"),
                ..artifact_target(0, 8)
            },
            100,
            RecallEncoding::Text,
        ),
    ));
    assert_eq!(
        foreign.status,
        RecallStatus::Unavailable,
        "an unpublished id names nothing"
    );
}

/// A read's encoded result stays within its byte bound, and says where to continue.
#[test]
fn a_read_is_bounded_by_its_encoded_size() {
    let text = "\"".repeat(1_000);
    let session = session(&text, None);
    let target = RecallTarget {
        kind: SourceKind::Event,
        event_seq: Some(0),
        block_index: None,
        artifact_id: None,
        offset: 0,
        length: 1_000,
        field: SourceField::UserText,
    };
    let result = futures::executor::block_on(recall(
        &scope(&session, None, b"key"),
        &read(target, 200, RecallEncoding::Text),
    ));
    let data = result.data.clone().unwrap_or_default();
    let encoded = nanus_domain::content::serialized_size(&data, usize::MAX).unwrap_or(usize::MAX);
    assert!(encoded <= 200, "escaped quotes count twice: {encoded}");
    assert_eq!(result.coverage, Coverage::Partial);
    assert_eq!(
        result.next_offset,
        Some(u64::try_from(data.len()).unwrap_or(0))
    );
}

/// An archive is searched a chunk at a time within one call's work bound, so a marker past the
/// first chunk is found without a cursor round trip — and a match straddling a chunk edge too.
#[test]
fn an_archive_search_reads_past_its_first_chunk_and_across_chunk_edges() {
    let mut bytes = vec![b'x'; limits::RECALL_CHUNK_BYTES.saturating_sub(3)];
    bytes.extend_from_slice(b"EDGE");
    bytes.extend(vec![b'y'; 10_000]);
    bytes.extend_from_slice(b"LATER");
    let session = session("go", Some(&bytes));
    let archive = Archive {
        bytes: bytes.clone(),
        corrupt: false,
    };
    let scope = scope(&session, Some(&archive), b"key");
    for (query, offset) in [
        ("EDGE", limits::RECALL_CHUNK_BYTES.saturating_sub(3)),
        ("LATER", bytes.len().saturating_sub(5)),
    ] {
        let found = futures::executor::block_on(recall(&scope, &search(query, None, 5)));
        assert_eq!(found.hits.len(), 1, "{query}: {found:?}");
        assert_eq!(
            found.hits[0].source.offset,
            u64::try_from(offset).unwrap_or(0)
        );
        assert_eq!(found.coverage, Coverage::Complete);
    }
}
