//! The tool against a real FFmpeg, over real encoded files in a temporary workspace.
//!
//! Each clip is eight seconds of four solid colours, two seconds each: red, green, blue,
//! yellow. Four samples fall on the centres of the quarters, so a decoder that returns the
//! right frames in the right order is visible in the pixels, not only in a count.

#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]
// Test arithmetic is over small literals and counters, `FFmpeg` is a name, and the two
// `let shared` bindings are what make an `Rc` unsize to a trait object.
#![allow(
    clippy::arithmetic_side_effects,
    clippy::integer_division,
    clippy::doc_markdown,
    clippy::let_and_return
)]

use std::cell::Cell;
use std::path::Path;
use std::process::Command;
use std::rc::Rc;

use nanus_adapter_local::LocalFs;
use nanus_domain::{ContentBlock, ToolCall, ToolCallId, ToolName, ToolOutcome};
use nanus_ports::LocalBoxFuture;
use nanus_tool_video::{
    Analysis, AnalysisRequest, FfmpegDecoder, FsSource, Provenance, Snapshot, VideoAnalyzer,
    VideoError, VideoRouting, VideoServices, VideoSource, Window, read_video_tool,
};
use serde_json::{Value, json};

const PALETTE: [(&str, [i32; 3]); 4] = [
    ("red", [255, 0, 0]),
    ("green", [0, 255, 0]),
    ("blue", [0, 0, 255]),
    ("yellow", [255, 255, 0]),
];

/// Encodes the four-colour clip with `encoder_args` into `dir/name`.
fn make_clip(dir: &Path, name: &str, encoder_args: &[&str], filter_tail: &str) {
    let mut command = Command::new("ffmpeg");
    command.args(["-hide_banner", "-loglevel", "error", "-y"]);
    for color in ["red", "green", "blue", "yellow"] {
        command.args([
            "-f",
            "lavfi",
            "-i",
            &format!("color=c={color}:s=320x240:r=25:d=2"),
        ]);
    }
    let graph = format!("[0][1][2][3]concat=n=4:v=1:a=0{filter_tail}[v]");
    command.args(["-filter_complex", &graph, "-map", "[v]"]);
    command.args(encoder_args);
    command.arg(dir.join(name));
    let status = command.status().expect("ffmpeg runs");
    assert!(status.success(), "ffmpeg could not make {name}");
}

struct Routing {
    sees_images: Cell<bool>,
    analyzer: Option<Rc<dyn VideoAnalyzer>>,
    asked: Cell<u32>,
}

impl VideoRouting for Routing {
    fn main_model_sees_images(&self) -> bool {
        self.sees_images.get()
    }
    fn analyzer(&self) -> LocalBoxFuture<'_, Result<Rc<dyn VideoAnalyzer>, VideoError>> {
        self.asked.set(self.asked.get() + 1);
        Box::pin(async move {
            self.analyzer
                .clone()
                .ok_or_else(|| VideoError::Unavailable("read_video: no analysis route".into()))
        })
    }
}

struct Recorder {
    seen: std::cell::RefCell<Vec<AnalysisRequest>>,
}

impl VideoAnalyzer for Recorder {
    fn provenance(&self) -> Provenance {
        Provenance {
            provider: "fake".into(),
            plan: "api".into(),
            model: "fake-vision".into(),
            endpoint_origin: "https://fake.invalid".into(),
            protocol: "test".into(),
            profile_version: "test-v1".into(),
            processing: "recorded".into(),
        }
    }
    fn analyze<'a>(
        &'a self,
        request: &'a AnalysisRequest,
    ) -> LocalBoxFuture<'a, Result<Analysis, VideoError>> {
        self.seen.borrow_mut().push(request.clone());
        Box::pin(async {
            Ok(Analysis {
                answer: "it changes colour".into(),
                usage: None,
            })
        })
    }
}

/// Counts how often the source is opened, to prove a refusal happened before any I/O.
struct CountingSource {
    inner: FsSource,
    opened: Rc<Cell<u32>>,
}

impl VideoSource for CountingSource {
    fn snapshot<'a>(&'a self, path: &'a str) -> LocalBoxFuture<'a, Result<Snapshot, VideoError>> {
        self.opened.set(self.opened.get() + 1);
        self.inner.snapshot(path)
    }
}

fn analyzer_handle(recorder: &Rc<Recorder>) -> Rc<dyn VideoAnalyzer> {
    let shared = Rc::clone(recorder);
    shared
}

fn routing_handle(routing: &Rc<Routing>) -> Rc<dyn VideoRouting> {
    let shared = Rc::clone(routing);
    shared
}

struct Harness {
    workspace: tempfile::TempDir,
    scratch: tempfile::TempDir,
    routing: Rc<Routing>,
    recorder: Rc<Recorder>,
    opened: Rc<Cell<u32>>,
    tool: nanus_domain::ToolDefinition,
}

fn harness(sees_images: bool, with_analyzer: bool) -> Harness {
    let workspace = tempfile::tempdir().expect("workspace");
    let root = workspace.path().canonicalize().expect("canonical");
    let fs = LocalFs::new(root).expect("fs").handle();
    let recorder = Rc::new(Recorder {
        seen: std::cell::RefCell::new(Vec::new()),
    });
    let routing = Rc::new(Routing {
        sees_images: Cell::new(sees_images),
        analyzer: with_analyzer.then(|| analyzer_handle(&recorder)),
        asked: Cell::new(0),
    });
    let opened = Rc::new(Cell::new(0));
    let scratch = tempfile::tempdir().expect("scratch");
    let decoder = FfmpegDecoder::prepare(None).expect("a full FFmpeg build is installed");
    let tool = read_video_tool(VideoServices {
        source: Rc::new(CountingSource {
            inner: FsSource::new(fs).with_temp_root(scratch.path().to_path_buf()),
            opened: Rc::clone(&opened),
        }),
        decoder: Rc::new(decoder),
        routing: routing_handle(&routing),
    })
    .expect("tool");
    Harness {
        workspace,
        scratch,
        routing,
        recorder,
        opened,
        tool,
    }
}

fn call(arguments: Value) -> ToolCall {
    ToolCall::new(
        ToolCallId::new("call-1"),
        ToolName::new("read_video").expect("name"),
        arguments,
    )
}

fn run(harness: &Harness, arguments: Value) -> ToolOutcome {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let result = runtime.block_on(harness.tool.execute(call(arguments)));
    assert_eq!(result.call_id.as_str(), "call-1", "the original call id");
    result.outcome
}

fn images(outcome: &ToolOutcome) -> Vec<Vec<u8>> {
    use base64::Engine as _;
    outcome
        .content()
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Image { data_base64, .. } => Some(
                base64::engine::general_purpose::STANDARD
                    .decode(data_base64)
                    .expect("base64"),
            ),
            ContentBlock::Text(_) => None,
        })
        .collect()
}

/// The palette entry nearest the middle pixel of a JPEG.
fn colour_of(jpeg: &[u8]) -> &'static str {
    let decoded = image::load_from_memory(jpeg).expect("a JPEG").to_rgb8();
    let pixel = decoded
        .get_pixel(decoded.width() / 2, decoded.height() / 2)
        .0;
    PALETTE
        .iter()
        .min_by_key(|(_, rgb)| {
            (0..3)
                .map(|channel| (rgb[channel] - i32::from(pixel[channel])).pow(2))
                .sum::<i32>()
        })
        .expect("a palette")
        .0
}

fn manifest(outcome: &ToolOutcome) -> Value {
    let Some(ContentBlock::Text(text)) = outcome.content().first() else {
        panic!("the first block is the manifest: {outcome:?}");
    };
    let parsed: Value = serde_json::from_str(text).expect("the manifest is JSON");
    assert_eq!(Some(&parsed), outcome.value(), "block and value agree");
    parsed
}

/// Every mandatory container and codec yields the four colours, in order, at the centres.
#[test]
fn every_mandatory_codec_and_container_yields_the_right_frames_in_order() {
    let clips: [(&str, &[&str]); 9] = [
        ("h264.mp4", &["-c:v", "libx264", "-pix_fmt", "yuv420p"]),
        (
            "hevc.mp4",
            &["-c:v", "libx265", "-pix_fmt", "yuv420p", "-tag:v", "hvc1"],
        ),
        ("vp8.webm", &["-c:v", "libvpx", "-pix_fmt", "yuv420p"]),
        ("vp9.webm", &["-c:v", "libvpx-vp9", "-pix_fmt", "yuv420p"]),
        ("av1.webm", &["-c:v", "libsvtav1", "-pix_fmt", "yuv420p"]),
        ("av1.mp4", &["-c:v", "libsvtav1", "-pix_fmt", "yuv420p"]),
        ("mpeg4.avi", &["-c:v", "mpeg4", "-q:v", "3"]),
        ("mpeg2.mpg", &["-c:v", "mpeg2video", "-q:v", "3"]),
        (
            "mjpeg.mkv",
            &["-c:v", "mjpeg", "-q:v", "3", "-pix_fmt", "yuvj420p"],
        ),
    ];
    let harness = harness(true, false);
    for (name, args) in clips {
        make_clip(harness.workspace.path(), name, args, "");
        let outcome = run(&harness, json!({"file_path": name, "mode": "frames"}));
        assert!(outcome.is_success(), "{name}: {outcome:?}");
        let colours: Vec<_> = images(&outcome)
            .iter()
            .map(|jpeg| colour_of(jpeg))
            .collect();
        assert_eq!(colours, ["red", "green", "blue", "yellow"], "{name}");
        let manifest = manifest(&outcome);
        let stamps: Vec<i64> = manifest["frames"]
            .as_array()
            .expect("frames")
            .iter()
            .map(|frame| frame["timestamp_ms"].as_i64().expect("ms"))
            .collect();
        for (stamp, centre) in stamps.iter().zip([1000, 3000, 5000, 7000]) {
            assert!((stamp - centre).abs() <= 150, "{name}: {stamps:?}");
        }
        assert_eq!(manifest["mode"], "frames", "{name}");
        assert_eq!(manifest["method"], "sampled_frames", "{name}");
        assert_eq!(manifest["audio"], "omitted", "{name}");
        assert!(manifest.get("backend").is_none(), "{name}");
    }
}

#[test]
fn a_nonzero_source_start_is_normalised_to_the_start_of_the_video() {
    let harness = harness(true, false);
    make_clip(
        harness.workspace.path(),
        "offset.ts",
        &["-c:v", "libx264", "-pix_fmt", "yuv420p"],
        "",
    );
    let outcome = run(
        &harness,
        json!({"file_path": "offset.ts", "mode": "frames", "start_ms": 4000, "end_ms": 8000}),
    );
    assert!(outcome.is_success(), "{outcome:?}");
    let colours: Vec<_> = images(&outcome)
        .iter()
        .map(|jpeg| colour_of(jpeg))
        .collect();
    assert_eq!(colours, ["blue", "blue", "yellow", "yellow"]);
    let stamps: Vec<i64> = manifest(&outcome)["frames"]
        .as_array()
        .expect("frames")
        .iter()
        .map(|frame| frame["timestamp_ms"].as_i64().expect("ms"))
        .collect();
    assert!(
        stamps.iter().all(|stamp| (4000..8000).contains(stamp)),
        "{stamps:?}"
    );
}

#[test]
fn pixel_aspect_ratio_is_applied_and_the_long_edge_is_bounded() {
    let harness = harness(true, false);
    make_clip(
        harness.workspace.path(),
        "anamorphic.mp4",
        &["-c:v", "libx264", "-pix_fmt", "yuv420p"],
        ",scale=2000:500,setsar=2",
    );
    let outcome = run(
        &harness,
        json!({"file_path": "anamorphic.mp4", "mode": "frames", "max_frames": 1}),
    );
    assert!(outcome.is_success(), "{outcome:?}");
    let frame = &manifest(&outcome)["frames"][0];
    // 2000x500 at sample aspect 2:1 displays as 4000x500, so the long edge is scaled to 1024.
    assert_eq!(frame["width"], 1024);
    assert!(frame["height"].as_u64().expect("height") <= 130, "{frame}");
}

#[test]
fn analyze_returns_text_only_and_sends_exactly_the_sampled_frames() {
    let harness = harness(false, true);
    make_clip(
        harness.workspace.path(),
        "clip.mp4",
        &["-c:v", "libx264", "-pix_fmt", "yuv420p"],
        "",
    );
    // Auto on a model that cannot see images resolves to analysis.
    let outcome = run(
        &harness,
        json!({"file_path": "clip.mp4", "question": "what changes?", "max_frames": 3}),
    );
    assert!(outcome.is_success(), "{outcome:?}");
    assert!(
        images(&outcome).is_empty(),
        "no pixels reach a model that cannot see them"
    );
    let manifest = manifest(&outcome);
    assert_eq!(manifest["mode"], "analyze");
    assert_eq!(manifest["requested_mode"], "auto");
    assert_eq!(manifest["backend"]["model"], "fake-vision");
    assert!(
        manifest["backend"].get("usage").is_none(),
        "absent usage stays absent"
    );
    assert!(
        matches!(outcome.content().get(1), Some(ContentBlock::Text(text))
        if text.contains("it changes colour") && text.contains("not the video itself"))
    );
    let seen = harness.recorder.seen.borrow();
    assert_eq!(seen.len(), 1, "one request, no retry");
    assert_eq!(seen[0].question, "what changes?");
    assert_eq!(seen[0].frames.len(), 3);
    assert_eq!(
        seen[0].window,
        Window {
            start_ms: 0,
            end_ms: 8000
        }
    );
}

#[test]
fn a_video_that_cannot_be_delivered_is_refused_before_the_file_is_opened() {
    // Explicit frames on a model that cannot see images, and analysis with no route.
    let harness = harness(false, false);
    make_clip(
        harness.workspace.path(),
        "clip.mp4",
        &["-c:v", "libx264", "-pix_fmt", "yuv420p"],
        "",
    );
    for mode in ["frames", "analyze", "auto"] {
        let outcome = run(&harness, json!({"file_path": "clip.mp4", "mode": mode}));
        assert!(!outcome.is_success(), "{mode}: {outcome:?}");
    }
    assert_eq!(
        harness.opened.get(),
        0,
        "no source I/O for an undeliverable call"
    );
    assert_eq!(
        harness.routing.asked.get(),
        2,
        "analyze and auto asked for a route"
    );
}

#[test]
fn bad_sources_and_windows_are_failures_the_model_can_read() {
    let harness = harness(true, false);
    let workspace = harness.workspace.path();
    make_clip(
        workspace,
        "clip.mp4",
        &["-c:v", "libx264", "-pix_fmt", "yuv420p"],
        "",
    );
    std::fs::write(workspace.join("notes.txt"), "not a video").expect("write");
    std::fs::write(
        workspace.join("list.ffconcat"),
        "ffconcat version 1.0\nfile /etc/hosts\n",
    )
    .expect("write");
    std::fs::create_dir(workspace.join("folder")).expect("dir");
    let cases = [
        (json!({"file_path": "missing.mp4"}), "missing"),
        (json!({"file_path": "notes.txt"}), "not readable media"),
        (json!({"file_path": "list.ffconcat"}), "not readable media"),
        (json!({"file_path": "folder"}), "not a regular file"),
        (json!({"file_path": "../outside.mp4"}), "outside"),
        (
            json!({"file_path": "clip.mp4", "start_ms": 9000}),
            "past the end",
        ),
        (
            json!({"file_path": "clip.mp4", "max_frames": 9}),
            "max_frames",
        ),
    ];
    for (arguments, reason) in cases {
        let outcome = run(&harness, arguments.clone());
        let message = outcome.message().map(str::to_lowercase);
        assert!(
            message.as_deref().is_some_and(|text| text.contains(reason)),
            "{arguments}: expected `{reason}` in {message:?}"
        );
    }
}

#[test]
fn an_end_past_the_video_is_clamped_and_said_so_but_a_start_past_it_is_refused() {
    let harness = harness(true, false);
    make_clip(
        harness.workspace.path(),
        "clip.mp4",
        &["-c:v", "libx264", "-pix_fmt", "yuv420p"],
        "",
    );
    let outcome = run(
        &harness,
        json!({"file_path": "clip.mp4", "mode": "frames", "end_ms": 60000}),
    );
    assert!(outcome.is_success(), "{outcome:?}");
    let manifest = manifest(&outcome);
    assert_eq!(manifest["end_ms"], 8000);
    assert!(
        manifest["warnings"][0]
            .as_str()
            .is_some_and(|text| text.contains("clamped")),
        "{manifest}"
    );
    let refused = run(&harness, json!({"file_path": "clip.mp4", "start_ms": 8000}));
    assert!(
        refused
            .message()
            .is_some_and(|text| text.contains("past the end"))
    );
}

#[test]
fn no_snapshot_survives_a_call_that_succeeds_or_fails() {
    let harness = harness(true, false);
    make_clip(
        harness.workspace.path(),
        "clip.mp4",
        &["-c:v", "libx264", "-pix_fmt", "yuv420p"],
        "",
    );
    std::fs::write(harness.workspace.path().join("bad.bin"), "nope").expect("write");
    let leftovers = || {
        std::fs::read_dir(harness.scratch.path())
            .expect("scratch")
            .count()
    };
    assert!(run(&harness, json!({"file_path": "clip.mp4", "mode": "frames"})).is_success());
    assert_eq!(leftovers(), 0, "a success removes its copy");
    assert!(!run(&harness, json!({"file_path": "bad.bin"})).is_success());
    assert_eq!(leftovers(), 0, "a failure removes its copy");
    assert_eq!(harness.opened.get(), 2, "both calls did open the source");
}

#[test]
fn the_decoder_is_certified_and_a_missing_one_is_named() {
    let decoder = FfmpegDecoder::prepare(None).expect("certified");
    drop(decoder);
    let error = FfmpegDecoder::prepare(Some(Path::new("/nonexistent-ffmpeg-dir"))).unwrap_err();
    assert!(error.to_string().contains("not found"), "{error}");
}

/// The runner admits a call against what the tool declares, so the declaration is part of the
/// contract: four JPEGs of at most 512 KiB, and `Execute` access.
#[test]
fn the_tool_declares_the_images_it_may_return() {
    let harness = harness(true, false);
    let envelope = harness.tool.result_images().expect("a declared envelope");
    assert_eq!(envelope.max_images, 4);
    assert_eq!(envelope.max_bytes_each, 512 * 1024);
    assert_eq!(harness.tool.access(), nanus_domain::ToolAccess::Execute);
}
