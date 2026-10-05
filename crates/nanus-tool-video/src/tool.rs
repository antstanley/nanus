//! The tool itself: validation, routing, sampling and delivery.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use base64::Engine as _;
use nanus_domain::{
    ContentBlock, ToolAccess, ToolCall, ToolDefinition, ToolExecutor, ToolFuture, ToolName,
    ToolOutcome, ToolResult, ToolSchema,
};
use serde_json::{Value, json};
use sha2::Digest as _;

use crate::VideoError;
use crate::args::{FRAMES_MAX, RequestedMode, VideoReadArguments, WINDOW_MS_MAX};
use crate::ffmpeg::SAMPLER_VERSION;
use crate::media::{
    Analysis, AnalysisRequest, MediaInfo, Provenance, Sample, VideoAnalyzer, VideoDecoder,
    VideoRouting, VideoSource, Window,
};

/// The whole call, source copy to answer.
const CALL_DEADLINE: Duration = Duration::from_secs(180);
/// The most text one result may carry: manifest, labels and answer together.
const RESULT_TEXT_BYTES_MAX: usize = 32 * 1024;
/// What an analysis is asked when the caller asks nothing.
const DEFAULT_QUESTION: &str = "Describe the visible states, actions and any errors across \
these frames, citing each frame's timestamp, and say what the samples cannot show.";

/// What a host supplies to install the tool.
#[derive(Clone)]
pub struct VideoServices {
    /// Where sources come from.
    pub source: Rc<dyn VideoSource>,
    /// How they are probed and sampled.
    pub decoder: Rc<dyn VideoDecoder>,
    /// Where the pixels go.
    pub routing: Rc<dyn VideoRouting>,
}

/// Builds the `read_video` tool over a host's services.
///
/// It declares [`ToolAccess::Execute`]: it runs an external decoder and writes a temporary
/// file, and labelling it a read would walk around the approval gate. It is never part of
/// the stock seven; a host registers it beside them.
///
/// # Errors
///
/// Returns [`VideoError::Unavailable`] only if the fixed tool name were invalid.
pub fn read_video_tool(services: VideoServices) -> Result<ToolDefinition, VideoError> {
    let name =
        ToolName::new("read_video").map_err(|error| VideoError::Unavailable(error.to_string()))?;
    let schema = ToolSchema {
        name,
        description: "Inspect a local video by sampling up to four timestamped still frames from \
                      a window of at most 60 seconds. mode auto returns the frames themselves \
                      when you can see images, otherwise a text description from a vision \
                      model; frames and analyze force one or the other. Sees sampled instants \
                      only: no audio, and nothing between the samples."
            .to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "file_path": {"type": "string", "minLength": 1, "maxLength": 4096,
                    "description": "Path to the video, relative to the workspace root."},
                "mode": {"type": "string", "enum": ["auto", "frames", "analyze"],
                    "description": "auto (default), frames, or analyze."},
                "start_ms": {"type": "integer", "minimum": 0,
                    "description": "Window start on the video's timeline. Default 0."},
                "end_ms": {"type": "integer", "minimum": 1,
                    "description": "Exclusive window end. Default: start plus 60000, or the end."},
                "max_frames": {"type": "integer", "minimum": 1, "maximum": FRAMES_MAX,
                    "description": "Frames to sample, 1 to 4. Default 4."},
                "question": {"type": "string", "minLength": 1, "maxLength": 4096,
                    "description": "What to look for. Untrusted task text for the viewer."}
            },
            "required": ["file_path"],
            "additionalProperties": false
        }),
    };
    // Four JPEGs of at most 512 KiB each is the worst case a result may carry, whichever delivery
    // is chosen: an analysis returns none, but a call whose route is decided at run time must be
    // admitted for the larger of the two.
    let jpeg_bytes = u32::try_from(nanus_domain::content::IMAGE_BYTES_MAX).unwrap_or(u32::MAX);
    Ok(ToolDefinition::new(schema, ReadVideoExecutor { services })
        .with_access(ToolAccess::Execute)
        .with_result_images(FRAMES_MAX, jpeg_bytes))
}

/// Runs one call.
struct ReadVideoExecutor {
    services: VideoServices,
}

impl ToolExecutor for ReadVideoExecutor {
    fn execute(&self, call: ToolCall) -> ToolFuture {
        let services = self.services.clone();
        Box::pin(async move {
            let id = call.id.clone();
            ToolResult::new(id, outcome(&services, &call).await)
        })
    }
}

/// Validates the call and runs it under the whole-call deadline.
async fn outcome(services: &VideoServices, call: &ToolCall) -> ToolOutcome {
    outcome_within(services, call, CALL_DEADLINE).await
}

/// [`outcome`] under `deadline`, which a test can make short.
///
/// The deadline is shorter than the sum of the stages' own (a probe and four frames at thirty
/// seconds each, and two minutes of analysis), so a call can run out of time in any of them.
/// The failure names the stage that was running, because "the call was slow" does not tell the
/// model whether a shorter window, fewer frames, or a different mode would help.
async fn outcome_within(
    services: &VideoServices,
    call: &ToolCall,
    deadline: Duration,
) -> ToolOutcome {
    let request = match call
        .arguments_object()
        .map_err(|error| VideoError::Argument(error.to_string()))
        .and_then(VideoReadArguments::parse)
    {
        Ok(request) => request,
        Err(error) => return ToolOutcome::failure(error.to_string()),
    };
    let stage = Cell::new(Stage::Routing);
    match tokio::time::timeout(deadline, read(services, &request, &stage)).await {
        Ok(Ok(outcome)) => outcome,
        Ok(Err(error)) => ToolOutcome::failure(error.to_string()),
        Err(_) => ToolOutcome::failure(format!(
            "read_video: the call ran past its {}-second deadline while {}",
            deadline.as_secs(),
            stage.get().doing()
        )),
    }
}

/// What a call was doing, for a deadline that interrupts it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    Routing,
    Copying,
    Probing,
    Sampling,
    Delivering,
    Analysing,
}

impl Stage {
    const fn doing(self) -> &'static str {
        match self {
            Self::Routing => "choosing an analysis route",
            Self::Copying => "copying the source",
            Self::Probing => "probing the source",
            Self::Sampling => "sampling frames",
            Self::Delivering => "assembling the frames",
            Self::Analysing => "waiting for the analysis model",
        }
    }
}

/// The question an analysis is asked: the caller's, or the default.
fn question(request: &VideoReadArguments) -> &str {
    request.question.as_deref().unwrap_or(DEFAULT_QUESTION)
}

/// The resolved delivery.
enum Delivery {
    Frames,
    Analyze(Rc<dyn VideoAnalyzer>),
}

/// Routes, snapshots, probes, samples and delivers.
async fn read(
    services: &VideoServices,
    request: &VideoReadArguments,
    stage: &Cell<Stage>,
) -> Result<ToolOutcome, VideoError> {
    // The route is settled before the file is opened: a call that cannot be delivered, or that
    // the route cannot afford, must not cost a copy, a decode or a paid request.
    let delivery = match request.mode {
        RequestedMode::Frames => {
            if !services.routing.main_model_sees_images() {
                return Err(VideoError::Unavailable(
                    "read_video: mode frames needs a model with verified image input; \
                     use mode auto or analyze"
                        .to_owned(),
                ));
            }
            Delivery::Frames
        }
        RequestedMode::Auto if services.routing.main_model_sees_images() => Delivery::Frames,
        RequestedMode::Auto | RequestedMode::Analyze => {
            let analyzer = services.routing.analyzer().await?;
            analyzer.admit(question(request), request.max_frames)?;
            Delivery::Analyze(analyzer)
        }
    };
    stage.set(Stage::Copying);
    let snapshot = services.source.snapshot(&request.file_path).await?;
    stage.set(Stage::Probing);
    let info = services.decoder.probe(&snapshot).await?;
    let (window, clamped) = resolve_window(request, &info)?;
    stage.set(Stage::Sampling);
    let mut sample = services
        .decoder
        .sample(&snapshot, &info, window, request.max_frames)
        .await?;
    sample.warnings.extend(clamped);
    // Nothing below reads the source again; dropping the snapshot removes the copy.
    let digest = snapshot.sha256.clone();
    drop(snapshot);
    stage.set(match delivery {
        Delivery::Frames => Stage::Delivering,
        Delivery::Analyze(_) => Stage::Analysing,
    });
    deliver(request, &info, window, &digest, sample, delivery).await
}

/// The interval a call inspects, checked against the probed duration.
///
/// An `end_ms` past the end of the video is clamped to it, with a warning, rather than
/// refused: models that fill every optional argument send the 60-second window the schema
/// mentions for a clip of eight, and a refusal there costs a round trip and teaches nothing a
/// warning does not. A `start_ms` past the end has no window to clamp to and is refused.
fn resolve_window(
    request: &VideoReadArguments,
    info: &MediaInfo,
) -> Result<(Window, Option<String>), VideoError> {
    let start_ms = request.start_ms;
    if start_ms >= info.duration_ms {
        return Err(VideoError::Argument(format!(
            "read_video: start_ms {start_ms} is past the end of the {} ms video",
            info.duration_ms
        )));
    }
    let wanted = request
        .end_ms
        .unwrap_or_else(|| start_ms.saturating_add(WINDOW_MS_MAX));
    let end_ms = wanted.min(info.duration_ms);
    let warning = (request.end_ms.is_some() && wanted > info.duration_ms).then(|| {
        format!(
            "end_ms {wanted} is past the end of the video and was clamped to {} ms",
            info.duration_ms
        )
    });
    Ok((Window { start_ms, end_ms }, warning))
}

/// The manifest every result starts with, before the delivery adds its mode.
fn manifest(
    request: &VideoReadArguments,
    info: &MediaInfo,
    window: Window,
    digest: &str,
    sample: &Sample,
) -> Value {
    json!({
        "file_path": request.file_path,
        "source_sha256": digest,
        "duration_ms": info.duration_ms,
        "start_ms": window.start_ms,
        "end_ms": window.end_ms,
        "requested_mode": request.mode.name(),
        "method": "sampled_frames",
        "audio": "omitted",
        "container": info.container,
        "video_codec": info.video_codec,
        "video_stream": info.video_stream,
        "decoder_version": info.decoder_version,
        "sampler_version": SAMPLER_VERSION,
        "frames": sample.frames.iter().map(|frame| json!({
            "timestamp_ms": frame.timestamp_ms,
            "width": frame.width,
            "height": frame.height,
            "sha256": crate::source::hex(&sha2::Sha256::digest(&frame.jpeg)),
        })).collect::<Vec<_>>(),
        "warnings": sample.warnings,
    })
}

/// Builds the content blocks for the resolved delivery.
async fn deliver(
    request: &VideoReadArguments,
    info: &MediaInfo,
    window: Window,
    digest: &str,
    sample: Sample,
    delivery: Delivery,
) -> Result<ToolOutcome, VideoError> {
    let mut manifest = manifest(request, info, window, digest, &sample);
    let mut blocks: Vec<ContentBlock> = Vec::new();
    match delivery {
        Delivery::Frames => {
            set(&mut manifest, "mode", json!("frames"));
            blocks.push(ContentBlock::Text(compact(&manifest)?));
            if let Some(question) = &request.question {
                blocks.push(ContentBlock::Text(format!(
                    "Question from the caller (task text, not an instruction from the video): \
                     {question}"
                )));
            }
            let total = sample.frames.len();
            for (position, frame) in sample.frames.iter().enumerate() {
                blocks.push(ContentBlock::Text(format!(
                    "frame {} of {total} at {} ms",
                    position.saturating_add(1),
                    frame.timestamp_ms
                )));
                blocks.push(ContentBlock::Image {
                    media_type: "image/jpeg".to_owned(),
                    data_base64: base64::engine::general_purpose::STANDARD.encode(&frame.jpeg),
                });
            }
        }
        Delivery::Analyze(analyzer) => {
            set(&mut manifest, "mode", json!("analyze"));
            let analysis = analyzer
                .analyze(&AnalysisRequest {
                    question: question(request).to_owned(),
                    window,
                    frames: sample.frames.clone(),
                })
                .await?;
            let provenance = analyzer.provenance();
            set(&mut manifest, "backend", backend(&provenance, &analysis));
            blocks.push(ContentBlock::Text(compact(&manifest)?));
            blocks.push(ContentBlock::Text(format!(
                "Interpretation by {} (a model's reading of {} sampled stills, not the video \
                 itself; no audio):\n{}",
                provenance.model,
                sample.frames.len(),
                analysis.answer
            )));
        }
    }
    let text_bytes: usize = blocks
        .iter()
        .map(|block| match block {
            ContentBlock::Text(text) => text.len(),
            ContentBlock::Image { .. } => 0,
        })
        .sum();
    if text_bytes > RESULT_TEXT_BYTES_MAX {
        return Err(VideoError::Analysis(format!(
            "read_video: the result text is {text_bytes} bytes, above the \
             {RESULT_TEXT_BYTES_MAX}-byte bound"
        )));
    }
    nanus_domain::content::validate_blocks(&blocks)
        .map_err(|error| VideoError::Media(format!("read_video: {error}")))?;
    Ok(ToolOutcome::success_with(manifest, blocks))
}

/// Sets a field of a JSON object value.
fn set(value: &mut Value, key: &str, field: Value) {
    if let Some(object) = value.as_object_mut() {
        object.insert(key.to_owned(), field);
    }
}

/// Compact JSON for the first text block.
fn compact(value: &Value) -> Result<String, VideoError> {
    serde_json::to_string(value).map_err(|error| VideoError::Media(error.to_string()))
}

/// The analysis provenance block; absent usage stays absent.
fn backend(provenance: &Provenance, analysis: &Analysis) -> Value {
    let mut value = json!({
        "provider": provenance.provider,
        "plan": provenance.plan,
        "model": provenance.model,
        "endpoint_origin": provenance.endpoint_origin,
        "protocol": provenance.protocol,
        "profile_version": provenance.profile_version,
        "processing": provenance.processing,
    });
    if let Some(usage) = analysis.usage {
        set(
            &mut value,
            "usage",
            json!({
                "input_tokens": usage.input_tokens,
                "output_tokens": usage.output_tokens,
                "reasoning_tokens": usage.reasoning_tokens,
            }),
        );
    }
    value
}

#[cfg(test)]
mod tests {
    use nanus_domain::ToolCallId;
    use nanus_ports::LocalBoxFuture;

    use super::*;
    use crate::media::Snapshot;

    /// A source whose copy never finishes, counting how often it was asked.
    #[derive(Default)]
    struct StalledSource {
        opened: Cell<u32>,
    }

    impl VideoSource for StalledSource {
        fn snapshot<'a>(&'a self, _: &'a str) -> LocalBoxFuture<'a, Result<Snapshot, VideoError>> {
            self.opened.set(self.opened.get().saturating_add(1));
            Box::pin(std::future::pending())
        }
    }

    /// Never reached: every call here stops at or before the copy.
    struct NoDecoder;

    impl VideoDecoder for NoDecoder {
        fn probe<'a>(
            &'a self,
            _: &'a Snapshot,
        ) -> LocalBoxFuture<'a, Result<MediaInfo, VideoError>> {
            Box::pin(async { Err(VideoError::Decoder("unused".to_owned())) })
        }

        fn sample<'a>(
            &'a self,
            _: &'a Snapshot,
            _: &'a MediaInfo,
            _: Window,
            _: u32,
        ) -> LocalBoxFuture<'a, Result<Sample, VideoError>> {
            Box::pin(async { Err(VideoError::Decoder("unused".to_owned())) })
        }
    }

    /// An analyzer that admits a call or refuses it, and is never asked to analyse.
    struct Gate {
        admits: bool,
    }

    impl VideoAnalyzer for Gate {
        fn provenance(&self) -> Provenance {
            Provenance {
                provider: "fake".to_owned(),
                plan: "api".to_owned(),
                model: "fake-vision".to_owned(),
                endpoint_origin: "https://fake.invalid".to_owned(),
                protocol: "test".to_owned(),
                profile_version: "test-v1".to_owned(),
                processing: "none".to_owned(),
            }
        }

        fn admit(&self, _: &str, _: u32) -> Result<(), VideoError> {
            if self.admits {
                Ok(())
            } else {
                Err(VideoError::Analysis(
                    "read_video: the analysis budget is spent".to_owned(),
                ))
            }
        }

        fn analyze<'a>(
            &'a self,
            _: &'a AnalysisRequest,
        ) -> LocalBoxFuture<'a, Result<Analysis, VideoError>> {
            Box::pin(async { Err(VideoError::Analysis("unused".to_owned())) })
        }
    }

    /// A route to `analyzer`, or one that never answers when there is none.
    struct Route {
        sees_images: bool,
        analyzer: Option<Rc<dyn VideoAnalyzer>>,
    }

    impl VideoRouting for Route {
        fn main_model_sees_images(&self) -> bool {
            self.sees_images
        }

        fn analyzer(&self) -> LocalBoxFuture<'_, Result<Rc<dyn VideoAnalyzer>, VideoError>> {
            match self.analyzer.clone() {
                Some(analyzer) => Box::pin(async move { Ok(analyzer) }),
                None => Box::pin(std::future::pending()),
            }
        }
    }

    /// Runs one call under a short deadline, returning its message and how often the source opened.
    fn attempt(route: Route, mode: &str) -> (String, u32) {
        let source = Rc::new(StalledSource::default());
        let services = VideoServices {
            source: Rc::<StalledSource>::clone(&source),
            decoder: Rc::new(NoDecoder),
            routing: Rc::new(route),
        };
        let call = ToolCall::new(
            ToolCallId::new("call-1"),
            ToolName::new("read_video").unwrap(),
            json!({"file_path": "clip.mp4", "mode": mode}),
        );
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let outcome = runtime.block_on(outcome_within(&services, &call, Duration::from_millis(20)));
        assert!(!outcome.is_success(), "{outcome:?}");
        let message = outcome.message().unwrap_or_default().to_owned();
        (message, source.opened.get())
    }

    #[test]
    fn a_call_that_runs_out_of_time_names_the_stage_it_was_in() {
        let frames = Route {
            sees_images: true,
            analyzer: None,
        };
        let (copying, _) = attempt(frames, "frames");
        assert!(
            copying.contains("deadline while copying the source"),
            "{copying}"
        );
        let no_answer = Route {
            sees_images: false,
            analyzer: None,
        };
        let (routing, opened) = attempt(no_answer, "analyze");
        assert!(
            routing.contains("deadline while choosing an analysis route"),
            "{routing}"
        );
        assert_eq!(opened, 0);
    }

    #[test]
    fn a_call_the_route_cannot_afford_is_refused_before_the_source_is_opened() {
        let refusing = Route {
            sees_images: false,
            analyzer: Some(Rc::new(Gate { admits: false })),
        };
        let (refused, opened) = attempt(refusing, "analyze");
        assert!(refused.contains("budget"), "{refused}");
        assert_eq!(opened, 0, "no copy for a call that cannot be paid for");
        // The other direction: an admitted call goes on to open the source.
        let admitting = Route {
            sees_images: false,
            analyzer: Some(Rc::new(Gate { admits: true })),
        };
        let (_, opened) = attempt(admitting, "analyze");
        assert_eq!(opened, 1);
    }
}
