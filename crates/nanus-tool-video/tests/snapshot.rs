//! External caller-owned snapshots and physical ownership, without credentials or decoders.

#![allow(clippy::expect_used)]

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use nanus_domain::{ContentBlock, ToolCall, ToolCallId, ToolName, ToolOutcome};
use nanus_ports::LocalBoxFuture;
use nanus_tool_video::{
    MediaInfo, Sample, SampledFrame, Snapshot, VideoAnalyzer, VideoDecoder, VideoError,
    VideoRouting, VideoServices, VideoSource, Window, read_video_tool,
};
use sha2::{Digest as _, Sha256};

struct Lease {
    _directory: tempfile::TempDir,
    drops: Arc<AtomicUsize>,
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

fn owned() -> (PathBuf, Arc<AtomicUsize>, Arc<Lease>) {
    let directory = tempfile::tempdir().expect("private fixture directory");
    let path = directory.path().join("source");
    std::fs::write(&path, b"fictional source").expect("fixture bytes");
    let drops = Arc::new(AtomicUsize::new(0));
    let owner = Arc::new(Lease {
        _directory: directory,
        drops: Arc::clone(&drops),
    });
    (path, drops, owner)
}

fn snapshot(path: PathBuf, owner: Arc<Lease>) -> Snapshot {
    let digest = format!("{:x}", Sha256::digest(b"fictional source"));
    Snapshot::from_owned_file(path, digest, 16, owner).expect("caller receipt")
}

#[test]
fn external_source_lease_survives_a_dropped_call_until_the_physical_worker_releases() {
    let (path, drops, owner) = owned();
    let snapshot = snapshot(path.clone(), owner);
    let retained = snapshot.retain_owner();
    let (release, wait) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        wait.recv_timeout(Duration::from_secs(2))
            .expect("bounded worker release");
        drop(retained);
    });
    drop(snapshot);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    assert!(path.is_file());
    release.send(()).expect("release worker");
    worker.join().expect("physical join");
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert!(!path.exists());
}

#[test]
fn malformed_receipts_refuse_and_release_only_the_transferred_owner_reference() {
    for case in 0..6 {
        let (path, drops, owner) = owned();
        let supplied = match case {
            0 => PathBuf::from("source"),
            1 => path.with_file_name("model-supplied-name.mp4"),
            2 => path.parent().expect("parent").join("..").join("source"),
            _ => path.clone(),
        };
        let digest = match case {
            3 => "a".repeat(63),
            4 => "g".repeat(64),
            _ => "a".repeat(64),
        };
        let length = if case == 5 {
            nanus_tool_video::source::SNAPSHOT_BYTES_MAX + 1
        } else {
            16
        };
        let retained = owner.clone();
        assert!(Snapshot::from_owned_file(supplied, digest, length, owner).is_err());
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        assert!(path.is_file());
        drop(retained);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(!path.exists());
    }
}

#[test]
fn receipt_constructor_is_io_free_and_accepts_the_inclusive_claimed_size_bound() {
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("source");
    for length in [
        0,
        nanus_tool_video::source::SNAPSHOT_BYTES_MAX - 1,
        nanus_tool_video::source::SNAPSHOT_BYTES_MAX,
    ] {
        let snapshot = Snapshot::from_owned_file(
            path.clone(),
            "a".repeat(64),
            length,
            Arc::new("opaque-fictional-host-owner"),
        )
        .expect("receipt shape only");
        assert_eq!(snapshot.byte_len, length);
        assert!(!path.exists());
        assert!(!format!("{snapshot:?}").contains("opaque-fictional-host-owner"));
    }
}

struct Source {
    drops: Arc<AtomicUsize>,
    path: std::sync::Mutex<Option<PathBuf>>,
}

impl VideoSource for Source {
    fn snapshot<'a>(&'a self, file: &'a str) -> LocalBoxFuture<'a, Result<Snapshot, VideoError>> {
        Box::pin(async move {
            assert_eq!(file, "fictional-video");
            let directory = tempfile::tempdir().expect("directory");
            let path = directory.path().join("source");
            std::fs::write(&path, b"fictional source").expect("fixture bytes");
            *self.path.lock().expect("path") = Some(path.clone());
            Ok(snapshot(
                path,
                Arc::new(Lease {
                    _directory: directory,
                    drops: Arc::clone(&self.drops),
                }),
            ))
        })
    }
}

struct Decoder;
impl VideoDecoder for Decoder {
    fn probe<'a>(
        &'a self,
        snapshot: &'a Snapshot,
    ) -> LocalBoxFuture<'a, Result<MediaInfo, VideoError>> {
        Box::pin(async move {
            assert!(snapshot.path.is_file());
            Ok(MediaInfo {
                container: "fictional".into(),
                video_codec: "fixture".into(),
                video_stream: 0,
                duration_ms: 1000,
                start_offset_ms: 0,
                width: 16,
                height: 16,
                decoder_version: "fictional-decoder".into(),
            })
        })
    }
    fn sample<'a>(
        &'a self,
        snapshot: &'a Snapshot,
        _: &'a MediaInfo,
        _: Window,
        count: u32,
    ) -> LocalBoxFuture<'a, Result<Sample, VideoError>> {
        Box::pin(async move {
            assert!(snapshot.path.is_file());
            assert_eq!(count, 1);
            let image = image::RgbImage::from_pixel(16, 16, image::Rgb([255, 0, 0]));
            let mut jpeg = Vec::new();
            image::codecs::jpeg::JpegEncoder::new(&mut jpeg)
                .encode_image(&image)
                .expect("real JPEG fixture");
            Ok(Sample {
                frames: vec![SampledFrame {
                    timestamp_ms: 500,
                    width: 16,
                    height: 16,
                    jpeg,
                }],
                warnings: Vec::new(),
            })
        })
    }
}

struct Routing;
impl VideoRouting for Routing {
    fn main_model_sees_images(&self) -> bool {
        true
    }
    fn analyzer(&self) -> LocalBoxFuture<'_, Result<Rc<dyn VideoAnalyzer>, VideoError>> {
        Box::pin(async { Err(VideoError::Unavailable("no analysis fixture".into())) })
    }
}

#[tokio::test]
async fn external_caller_source_feeds_the_real_tool_and_releases_its_copy_after_delivery() {
    let drops = Arc::new(AtomicUsize::new(0));
    let source = Rc::new(Source {
        drops: Arc::clone(&drops),
        path: std::sync::Mutex::new(None),
    });
    let services = VideoServices {
        source: source.clone(),
        decoder: Rc::new(Decoder),
        routing: Rc::new(Routing),
    };
    let tool = read_video_tool(services).expect("ordinary tool");
    let call = ToolCall::new(
        ToolCallId::new("inspect"),
        ToolName::new("read_video").expect("name"),
        serde_json::json!({"file_path":"fictional-video","mode":"frames","max_frames":1}),
    );
    let outcome = tool.execute(call).await.outcome;
    assert!(
        matches!(outcome, ToolOutcome::Success { .. }),
        "{outcome:?}"
    );
    assert_eq!(outcome.value().expect("manifest")["mode"], "frames");
    assert!(outcome.content().iter().any(|block| matches!(block,
        ContentBlock::Image {media_type, data_base64} if media_type == "image/jpeg"
        && !data_base64.is_empty())));
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    let path = source
        .path
        .lock()
        .expect("path")
        .clone()
        .expect("snapshot path");
    assert!(!path.exists());
}
