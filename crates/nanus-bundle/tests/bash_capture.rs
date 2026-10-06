//! The `bash` tool archiving through the real local shell: T25 end to end.
//!
//! A command prints a marker after 65,536 bytes of output, then bytes that are not UTF-8, then
//! fails. What is asserted is what each side ends up with: the archive holds the exact bytes,
//! marker included; the model's text is the bounded preview with its truncation notice, one line
//! naming the archive, and the failing exit status last; and without a lease the same command
//! renders exactly as the legacy tool renders it.
//!
//! An integration-test crate is entirely test code, where a panic *is* the assertion, so the
//! workspace's panic-family exemption is restated here.
#![cfg(all(unix, feature = "stock-compose"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::{Arc, Mutex};

use nanus_adapter_local::LocalShell;
use nanus_bundle::CaptureBroker;
use nanus_bundle::tools::{bash_tool, bash_tool_with_capture};
use nanus_domain::context::managed::{
    ArtifactId, ArtifactReceipt, CaptureReason, CaptureStatus, CaptureStream, Digest, RawEncoding,
};
use nanus_domain::{ContentBlock, ToolCall, ToolCallId, ToolDefinition, ToolName};
use nanus_ports::{
    CaptureFailure, CaptureFinalization, CaptureLease, RawCaptureSink, SandboxPolicy,
    SendBoxFuture, ShellHandle,
};
use serde_json::json;

/// The command under test: a marker past the preview cap, binary bytes, a failing exit.
const SCRIPT: &str = "head -c 65536 /dev/zero | tr '\\0' 'x'; printf 'MARKER-PAST-THE-CAP'; \
                      printf '\\377\\376'; exit 3";

/// A sink that keeps everything in memory, shared with the test.
struct Memory {
    stream: CaptureStream,
    kept: Arc<Mutex<Vec<u8>>>,
}

impl RawCaptureSink for Memory {
    fn write<'a>(&'a mut self, bytes: &'a [u8]) -> SendBoxFuture<'a, Result<(), CaptureFailure>> {
        self.kept.lock().unwrap().extend_from_slice(bytes);
        Box::pin(async { Ok(()) })
    }

    fn finalize(
        self: Box<Self>,
        observed: u64,
        reason: CaptureReason,
    ) -> SendBoxFuture<'static, CaptureFinalization> {
        let bytes = self.kept.lock().unwrap().clone();
        let retained = bytes.len() as u64;
        let receipt = ArtifactReceipt {
            artifact_id: ArtifactId::parse("a:00000000-0000-4000-8000-0000000000aa"),
            call_id: "c1".to_owned(),
            stream: self.stream,
            retained_bytes: retained,
            observed_bytes: observed,
            retained_blake3: Some(Digest::of(&bytes)),
            status: if reason == CaptureReason::Eof && retained == observed {
                CaptureStatus::Complete
            } else {
                CaptureStatus::Partial
            },
            reason,
            encoding: RawEncoding::Raw,
            chunk_blake3: bytes.chunks(65_536).map(Digest::of).collect(),
        };
        Box::pin(async move {
            CaptureFinalization {
                receipt,
                artifact: None,
            }
        })
    }
}

fn shell(root: &std::path::Path) -> ShellHandle {
    LocalShell::new(SandboxPolicy::danger_full_access(root)).handle()
}

async fn text_of(tool: &ToolDefinition) -> String {
    let name = ToolName::new("bash").unwrap();
    let call = ToolCall::new(ToolCallId::new("c1"), name, json!({ "command": SCRIPT }));
    let result = tool.execute(call).await;
    let Some(ContentBlock::Text(text)) = result.outcome.content().first() else {
        panic!("bash renders text: {:?}", result.outcome);
    };
    text.clone()
}

#[tokio::test]
async fn a_marker_past_the_cap_is_archived_while_the_model_sees_a_bounded_preview() {
    let root = tempfile::tempdir().unwrap();
    let broker = CaptureBroker::new();
    let stdout = Arc::new(Mutex::new(Vec::new()));
    let sink = |stream, kept: &Arc<Mutex<Vec<u8>>>| -> Box<dyn RawCaptureSink> {
        Box::new(Memory {
            stream,
            kept: Arc::clone(kept),
        })
    };
    let lease = CaptureLease::new(
        ToolCallId::new("c1"),
        sink(CaptureStream::Stdout, &stdout),
        sink(CaptureStream::Stderr, &Arc::new(Mutex::new(Vec::new()))),
        Box::new(|| {}),
    );
    assert!(broker.insert(lease).is_none());
    let archived = text_of(&bash_tool_with_capture(shell(root.path()), broker.clone())).await;

    let mut expected = vec![b'x'; 65_536];
    expected.extend_from_slice(b"MARKER-PAST-THE-CAP\xff\xfe");
    assert_eq!(
        *stdout.lock().unwrap(),
        expected,
        "the archive is byte-exact"
    );
    assert!(
        !archived.contains("MARKER"),
        "the model's preview is cut at the cap"
    );
    assert!(
        archived.len() < 66_000,
        "the rendering is bounded: {}",
        archived.len()
    );
    assert!(
        archived.contains("[stdout truncated: 65557 bytes total"),
        "{}",
        tail(&archived)
    );
    assert!(
        archived.contains(
            "[stdout archived as a:00000000-0000-4000-8000-0000000000aa — 65557 of 65557 bytes, \
             complete; read it with context_recall]"
        ),
        "{}",
        tail(&archived)
    );
    assert!(
        archived.ends_with("[exit code: 3]\n"),
        "{}",
        tail(&archived)
    );
    let filed = broker.take_finalizations(&ToolCallId::new("c1"));
    assert_eq!(filed.len(), 2, "both streams are filed, the empty one too");
    assert!(filed.iter().all(|done| done.receipt.validate().is_ok()));
    assert!(broker.is_empty());

    // Pair: the legacy tool renders the same run without the archive line, and nothing else
    // about the text differs.
    let legacy = text_of(&bash_tool(shell(root.path()))).await;
    let without_archive: String = archived
        .split_inclusive('\n')
        .filter(|line| !line.starts_with("[stdout archived as "))
        .collect();
    assert_eq!(without_archive, legacy);
}

/// The last few lines of a long rendering, for a readable failure message.
fn tail(text: &str) -> &str {
    let start = text.len().saturating_sub(400);
    text.get(start..).unwrap_or(text)
}
