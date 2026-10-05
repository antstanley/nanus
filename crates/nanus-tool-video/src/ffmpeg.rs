//! FFprobe and FFmpeg as the decoder: structured arguments, no shell, bounded everything.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use base64::Engine as _;
use nanus_domain::content::IMAGE_BYTES_MAX;
use nanus_ports::LocalBoxFuture;
use serde_json::Value;
use tokio::io::AsyncReadExt as _;
use tokio::process::Command;

use crate::VideoError;
use crate::media::{MediaInfo, Sample, SampledFrame, Snapshot, VideoDecoder, Window};

/// Longest the decoder may spend on one probe or one frame.
const PROCESS_DEADLINE: Duration = Duration::from_secs(30);
/// Most bytes a probe may print.
const PROBE_BYTES_MAX: usize = 1024 * 1024;
/// Most bytes of FFmpeg's log kept for parsing: the tail, which holds the last frame's time.
const LOG_BYTES_MAX: usize = 256 * 1024;
/// Source edge and area ceilings, checked before any pixel is allocated.
const SOURCE_EDGE_MAX: u32 = 8192;
const SOURCE_PIXELS_MAX: u64 = 33_554_432;
/// How far before the target the input seek lands, so the exact seek has frames to discard.
const SEEK_PREROLL_MS: i64 = 5000;
/// The longest edge of a sampled still.
pub const FRAME_EDGE_MAX: u32 = 1024;
/// The sampler policy recorded in every manifest; changing the sampling changes this.
pub const SAMPLER_VERSION: &str = "centres-nearest-after-v2";

/// Containers that name something other than one self-contained file.
const REFUSED_CONTAINERS: [&str; 12] = [
    "concat",
    "ffconcat",
    "hls",
    "applehttp",
    "dash",
    "sdp",
    "rtsp",
    "rtp",
    "image2",
    "image2pipe",
    "lavfi",
    "ffmetadata",
];

/// Decoders every certified build carries; AV1 is satisfied by any of three.
const MANDATORY_DECODERS: [&str; 7] =
    ["h264", "hevc", "mpeg4", "mpeg2video", "mjpeg", "vp8", "vp9"];
const AV1_DECODERS: [&str; 3] = ["libdav1d", "libaom-av1", "av1"];
/// Demuxers every certified build carries: Matroska/WebM, MP4/MOV, AVI and an MPEG family.
const MANDATORY_DEMUXERS: [&str; 4] = ["matroska", "mov", "avi", "mpeg"];

/// FFprobe and FFmpeg executables, certified once.
#[derive(Clone, Debug)]
pub struct FfmpegDecoder {
    ffmpeg: PathBuf,
    ffprobe: PathBuf,
    version: String,
}

impl FfmpegDecoder {
    /// Finds `ffmpeg` and `ffprobe` in `dir`, or on `PATH` when none is given, and certifies them.
    ///
    /// Blocking: it runs the executables to list their components, and belongs in composition
    /// rather than in a call. Nothing is installed; a missing or reduced build is a sentence
    /// naming what is missing.
    ///
    /// # Errors
    ///
    /// Returns [`VideoError::Unavailable`] when an executable is missing or lacks a mandatory
    /// demuxer or decoder.
    pub fn prepare(dir: Option<&Path>) -> Result<Self, VideoError> {
        let ffmpeg = locate("ffmpeg", dir)?;
        let ffprobe = locate("ffprobe", dir)?;
        let version = output(&ffmpeg, &["-hide_banner", "-version"])?
            .lines()
            .next()
            .unwrap_or("ffmpeg (unknown version)")
            .trim()
            .to_owned();
        let decoders = output(&ffmpeg, &["-hide_banner", "-decoders"])?;
        let demuxers = output(&ffmpeg, &["-hide_banner", "-demuxers"])?;
        let missing = missing_components(&decoders, &demuxers);
        if !missing.is_empty() {
            return Err(VideoError::Unavailable(format!(
                "read_video: {} lacks {}; install a full FFmpeg build (WebM/Matroska and AV1 \
                 decoding are required)",
                ffmpeg.display(),
                missing.join(", ")
            )));
        }
        Ok(Self {
            ffmpeg,
            ffprobe,
            version,
        })
    }
}

/// The mandatory components a build's listings do not show.
fn missing_components(decoders: &str, demuxers: &str) -> Vec<String> {
    let names = |listing: &str| -> Vec<String> {
        listing
            .lines()
            .filter_map(|line| line.split_whitespace().nth(1))
            .flat_map(|name| name.split(',').map(str::to_owned).collect::<Vec<_>>())
            .collect()
    };
    let have_decoders = names(decoders);
    let have_demuxers = names(demuxers);
    let mut missing: Vec<String> = MANDATORY_DECODERS
        .iter()
        .filter(|name| !have_decoders.iter().any(|have| have == *name))
        .map(|name| format!("the {name} decoder"))
        .collect();
    if !AV1_DECODERS
        .iter()
        .any(|name| have_decoders.iter().any(|have| have == name))
    {
        missing.push("an AV1 decoder".to_owned());
    }
    missing.extend(
        MANDATORY_DEMUXERS
            .iter()
            .filter(|name| !have_demuxers.iter().any(|have| have == *name))
            .map(|name| format!("the {name} demuxer")),
    );
    if !have_demuxers.iter().any(|have| have == "webm") {
        missing.push("the webm demuxer".to_owned());
    }
    missing
}

/// Resolves an executable in `dir` or on `PATH`.
fn locate(name: &str, dir: Option<&Path>) -> Result<PathBuf, VideoError> {
    let candidates: Vec<PathBuf> = dir.map_or_else(
        || {
            std::env::var_os("PATH")
                .map(|path| {
                    std::env::split_paths(&path)
                        .map(|dir| dir.join(name))
                        .collect()
                })
                .unwrap_or_default()
        },
        |dir| vec![dir.join(name)],
    );
    candidates
        .into_iter()
        .find(|path| path.is_file())
        .ok_or_else(|| {
            VideoError::Unavailable(format!(
                "read_video: {name} was not found; install FFmpeg or set `ffmpeg_dir` in the \
                 configuration"
            ))
        })
}

/// Runs a listing command to completion.
fn output(binary: &Path, args: &[&str]) -> Result<String, VideoError> {
    let run = std::process::Command::new(binary)
        .args(args)
        .env_clear()
        .stdin(Stdio::null())
        .output()
        .map_err(|error| VideoError::Unavailable(format!("{}: {error}", binary.display())))?;
    if !run.status.success() {
        return Err(VideoError::Unavailable(format!(
            "{} {} failed",
            binary.display(),
            args.join(" ")
        )));
    }
    Ok(String::from_utf8_lossy(&run.stdout).into_owned())
}

/// Reads a pipe to its end, keeping only the last [`LOG_BYTES_MAX`] bytes.
///
/// The pipe must be read to the end even when the bytes are not wanted: a decoder that logs
/// more than the pipe holds blocks on the write, and a reader that stopped at a cap would turn
/// a chatty codec (HEVC prints its SEI payload for every frame) into a deadlock.
async fn drain_tail<R: tokio::io::AsyncRead + Unpin>(
    mut reader: R,
    tail: &mut Vec<u8>,
) -> std::io::Result<()> {
    let mut chunk = [0_u8; 8192];
    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            return Ok(());
        }
        tail.extend_from_slice(chunk.get(..read).unwrap_or_default());
        if tail.len() > LOG_BYTES_MAX.saturating_mul(2) {
            let excess = tail.len().saturating_sub(LOG_BYTES_MAX);
            tail.drain(..excess);
        }
    }
}

/// What a bounded run produced.
struct Run {
    stdout: Vec<u8>,
    stderr: String,
    success: bool,
}

/// Why a bounded run produced nothing usable.
enum RunError {
    /// It printed more than its output bound and was killed for it.
    OutputBound,
    /// Anything else: a spawn failure, a pipe error, or the deadline.
    Failed(VideoError),
}

impl From<RunError> for VideoError {
    fn from(error: RunError) -> Self {
        match error {
            RunError::OutputBound => {
                Self::Decoder("the decoder printed more than its output bound".to_owned())
            }
            RunError::Failed(error) => error,
        }
    }
}

/// Runs `binary` with no shell, no stdin and a cleared environment, under a deadline.
///
/// Output beyond `stdout_max` kills the process: an unbounded pipe is how a hostile file would
/// turn into unbounded memory. It is killed the moment the bound is crossed, not at the deadline:
/// a process whose stdout is no longer read blocks on the write and never closes its stderr, so
/// waiting for both pipes to end would turn every overflow into a thirty-second stall. Dropping
/// the future kills the child too, so a cancelled call leaves nothing running.
async fn run(binary: &Path, args: &[String], stdout_max: usize) -> Result<Run, RunError> {
    let failed = |message: String| RunError::Failed(VideoError::Decoder(message));
    let mut child = Command::new(binary)
        .args(args)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| failed(format!("{}: {error}", binary.display())))?;
    let (Some(out), Some(err)) = (child.stdout.take(), child.stderr.take()) else {
        return Err(failed("the decoder has no pipes".to_owned()));
    };
    let cap = |limit: usize| u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1);
    let work = async {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut out = out.take(cap(stdout_max));
        let read_stdout = async {
            let read = out.read_to_end(&mut stdout).await;
            if stdout.len() > stdout_max {
                // Unblocks the stderr drain below: a dead process closes its pipes.
                let _ignored = child.start_kill();
            }
            read
        };
        let (read_out, read_err) = tokio::join!(read_stdout, drain_tail(err, &mut stderr));
        read_out.and(read_err)?;
        if stdout.len() > stdout_max {
            return Ok(None);
        }
        child
            .wait()
            .await
            .map(|status| Some((stdout, stderr, status)))
    };
    let finished = tokio::time::timeout(PROCESS_DEADLINE, work).await;
    match finished {
        Err(_) => {
            let _ignored = child.kill().await;
            Err(failed(format!(
                "the decoder ran past its {}-second deadline",
                PROCESS_DEADLINE.as_secs()
            )))
        }
        Ok(Err(error)) => Err(failed(error.to_string())),
        Ok(Ok(None)) => {
            // Reaps the child the bound already killed.
            let _ignored = child.kill().await;
            Err(RunError::OutputBound)
        }
        Ok(Ok(Some((stdout, stderr, status)))) => Ok(Run {
            stdout,
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
            success: status.success(),
        }),
    }
}

impl VideoDecoder for FfmpegDecoder {
    fn probe<'a>(
        &'a self,
        snapshot: &'a Snapshot,
    ) -> LocalBoxFuture<'a, Result<MediaInfo, VideoError>> {
        Box::pin(async move {
            let args: Vec<String> = [
                "-v",
                "error",
                "-protocol_whitelist",
                "file",
                "-print_format",
                "json",
                "-show_format",
                "-show_streams",
            ]
            .iter()
            .map(|arg| (*arg).to_owned())
            .chain([snapshot.path.display().to_string()])
            .collect();
            let probed = run(&self.ffprobe, &args, PROBE_BYTES_MAX).await?;
            if !probed.success {
                return Err(VideoError::Media(format!(
                    "read_video: the file is not readable media ({})",
                    first_line(&probed.stderr)
                )));
            }
            let document: Value = serde_json::from_slice(&probed.stdout)
                .map_err(|_| VideoError::Media("read_video: unreadable probe output".to_owned()))?;
            info_from_probe(&document, &self.version)
        })
    }

    fn sample<'a>(
        &'a self,
        snapshot: &'a Snapshot,
        info: &'a MediaInfo,
        window: Window,
        count: u32,
    ) -> LocalBoxFuture<'a, Result<Sample, VideoError>> {
        Box::pin(async move {
            let mut frames: Vec<SampledFrame> = Vec::new();
            let mut warnings = Vec::new();
            for target in centres(window, count) {
                match self.one_frame(snapshot, info, target).await? {
                    Some(frame) if frame.timestamp_ms >= window.end_ms => {
                        warnings.push(format!("no frame inside the window near {target} ms"));
                    }
                    Some(frame)
                        if frames
                            .last()
                            .is_some_and(|last| last.timestamp_ms == frame.timestamp_ms) =>
                    {
                        warnings.push(format!(
                            "samples near {target} ms landed on the same frame; one was dropped"
                        ));
                    }
                    Some(frame) => frames.push(frame),
                    None => warnings.push(format!("no frame could be decoded near {target} ms")),
                }
            }
            if frames.is_empty() {
                return Err(VideoError::Media(
                    "read_video: no frame could be decoded in the window".to_owned(),
                ));
            }
            if frames.len() < usize::try_from(count).unwrap_or(usize::MAX) {
                warnings.push(format!(
                    "{} of {count} requested frames were produced",
                    frames.len()
                ));
            }
            Ok(Sample { frames, warnings })
        })
    }
}

impl FfmpegDecoder {
    /// Decodes the first displayed frame at or after `target_ms` and encodes it as a JPEG.
    async fn one_frame(
        &self,
        snapshot: &Snapshot,
        info: &MediaInfo,
        target_ms: u64,
    ) -> Result<Option<SampledFrame>, VideoError> {
        let target = i64::try_from(target_ms).unwrap_or(i64::MAX);
        let offset = target.saturating_add(info.start_offset_ms);
        // A coarse seek into the input, then an exact one on the output: with `-copyts` the
        // second is absolute, so a container whose index lands the first one late (MPEG
        // program streams do) still yields the frame at the target rather than a later one.
        //
        // The two seeks are on different clocks. FFmpeg adds the file's start time to an input
        // `-ss` itself, so the coarse seek is relative to the start of the video and must not
        // carry the offset again: counted twice, a stream starting 20 seconds in is sought 20
        // seconds past every target, and from past the end nothing decodes at all. The stream's
        // start is never before the file's, so this lands at or before `offset - preroll`.
        let exact = format!("{:.3}", ms_to_seconds(offset.max(0)));
        let coarse = format!(
            "{:.3}",
            ms_to_seconds(target.saturating_sub(SEEK_PREROLL_MS).max(0))
        );
        for quality in ["4", "10", "20"] {
            let args = frame_arguments(snapshot, info, [&coarse, &exact], quality);
            let produced = run(&self.ffmpeg, &args, IMAGE_BYTES_MAX.saturating_mul(2)).await;
            let produced = match produced {
                Ok(produced) => produced,
                // A frame over the byte bound is retried at lower quality; anything else is final.
                Err(RunError::OutputBound) => continue,
                Err(RunError::Failed(error)) => return Err(error),
            };
            if !produced.success {
                return Err(VideoError::Media(format!(
                    "read_video: the decoder failed ({})",
                    first_line(&produced.stderr)
                )));
            }
            if produced.stdout.is_empty() {
                return Ok(None);
            }
            if produced.stdout.len() > IMAGE_BYTES_MAX {
                continue;
            }
            let Some(pts_ms) = last_pts_ms(&produced.stderr) else {
                return Ok(None);
            };
            let encoded = base64::engine::general_purpose::STANDARD.encode(&produced.stdout);
            let dimensions = nanus_domain::content::validate_image("image/jpeg", &encoded)
                .map_err(|error| VideoError::Media(format!("read_video: {error}")))?;
            let timestamp_ms =
                u64::try_from(pts_ms.saturating_sub(info.start_offset_ms)).unwrap_or(0);
            return Ok(Some(SampledFrame {
                timestamp_ms,
                width: dimensions.width,
                height: dimensions.height,
                jpeg: produced.stdout,
            }));
        }
        Err(VideoError::Media(
            "read_video: a frame does not fit the 512 KiB image bound at any quality".to_owned(),
        ))
    }
}

/// The FFmpeg arguments for one still.
fn frame_arguments(
    snapshot: &Snapshot,
    info: &MediaInfo,
    [coarse, exact]: [&str; 2],
    quality: &str,
) -> Vec<String> {
    // Sample aspect ratio is applied, then the long edge is bounded without ever upscaling.
    let fit = format!("min(1,min({FRAME_EDGE_MAX}/iw,{FRAME_EDGE_MAX}/ih))");
    let filter = format!(
        "scale=iw*sar:ih,setsar=1,scale=w='max(2,2*trunc(iw*{fit}/2))':h='max(2,2*trunc(ih*{fit}/2))',\
         format=yuvj420p,showinfo"
    );
    [
        "-hide_banner",
        "-nostdin",
        "-loglevel",
        "info",
        "-protocol_whitelist",
        "file",
        "-threads",
        "2",
        "-copyts",
        "-ss",
        coarse,
        "-i",
    ]
    .iter()
    .map(|arg| (*arg).to_owned())
    .chain([
        snapshot.path.display().to_string(),
        "-ss".to_owned(),
        exact.to_owned(),
        "-map".to_owned(),
        format!("0:{}", info.video_stream),
        "-an".to_owned(),
        "-sn".to_owned(),
        "-dn".to_owned(),
        "-frames:v".to_owned(),
        "1".to_owned(),
        "-vf".to_owned(),
        filter,
        "-c:v".to_owned(),
        "mjpeg".to_owned(),
        "-q:v".to_owned(),
        quality.to_owned(),
        "-f".to_owned(),
        "image2pipe".to_owned(),
        "-".to_owned(),
    ])
    .collect()
}

/// The sample times: the centres of `count` equal portions of the window.
fn centres(window: Window, count: u32) -> Vec<u64> {
    let span = window.end_ms.saturating_sub(window.start_ms);
    let count = u64::from(count.max(1));
    (0..count)
        .map(|index| {
            let numerator = span.saturating_mul(index.saturating_mul(2).saturating_add(1));
            window
                .start_ms
                .saturating_add(numerator.checked_div(count.saturating_mul(2)).unwrap_or(0))
        })
        .collect()
}

/// Seconds for a millisecond count; the conversion is exact for every bounded timestamp.
fn ms_to_seconds(ms: i64) -> f64 {
    #[allow(clippy::cast_precision_loss)] // timestamps are bounded far below 2^53
    let value = ms as f64;
    value / 1000.0
}

/// Milliseconds for a seconds value from FFmpeg's text.
fn seconds_to_ms(seconds: f64) -> Option<i64> {
    if !seconds.is_finite() || seconds.abs() > 1.0e9 {
        return None;
    }
    #[allow(clippy::cast_possible_truncation)] // bounded just above
    Some((seconds * 1000.0).round() as i64)
}

/// The presentation time of the last frame `showinfo` reported.
fn last_pts_ms(log: &str) -> Option<i64> {
    log.lines().rev().find_map(|line| {
        let rest = line.split("pts_time:").nth(1)?;
        let token = rest.split_whitespace().next()?;
        seconds_to_ms(token.parse::<f64>().ok()?)
    })
}

/// The first line of a log, for a one-line reason.
fn first_line(log: &str) -> String {
    log.lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("no detail")
        .chars()
        .take(200)
        .collect()
}

/// Selects the stream and checks every bound from a probe document.
fn info_from_probe(document: &Value, version: &str) -> Result<MediaInfo, VideoError> {
    let media = |reason: &str| VideoError::Media(format!("read_video: {reason}"));
    let format = document.get("format");
    let container = format
        .and_then(|format| format.get("format_name"))
        .and_then(Value::as_str)
        .ok_or_else(|| media("the container is unknown"))?;
    if container
        .split(',')
        .any(|name| REFUSED_CONTAINERS.contains(&name))
    {
        return Err(media(&format!(
            "{container} refers to other files or streams and is not read"
        )));
    }
    let streams = document
        .get("streams")
        .and_then(Value::as_array)
        .ok_or_else(|| media("the file has no streams"))?;
    let usable = |stream: &&Value| {
        stream.get("codec_type").and_then(Value::as_str) == Some("video")
            && stream
                .pointer("/disposition/attached_pic")
                .and_then(Value::as_u64)
                != Some(1)
    };
    let default = |stream: &&Value| {
        stream
            .pointer("/disposition/default")
            .and_then(Value::as_u64)
            == Some(1)
    };
    let stream = streams
        .iter()
        .filter(usable)
        .find(default)
        .or_else(|| streams.iter().find(usable))
        .ok_or_else(|| media("the file has no video stream"))?;
    let number = |value: Option<&Value>| value.and_then(Value::as_u64);
    let index = u32::try_from(number(stream.get("index")).unwrap_or(u64::MAX))
        .map_err(|_| media("the video stream index is unusable"))?;
    let width = u32::try_from(number(stream.get("width")).unwrap_or(0)).unwrap_or(0);
    let height = u32::try_from(number(stream.get("height")).unwrap_or(0)).unwrap_or(0);
    let pixels = u64::from(width).saturating_mul(u64::from(height));
    if width == 0
        || height == 0
        || width > SOURCE_EDGE_MAX
        || height > SOURCE_EDGE_MAX
        || pixels > SOURCE_PIXELS_MAX
    {
        return Err(media(&format!(
            "the video is {width}x{height}, outside the {SOURCE_EDGE_MAX}-pixel edge and \
             {SOURCE_PIXELS_MAX}-pixel bounds"
        )));
    }
    let seconds = |value: Option<&Value>| {
        value
            .and_then(Value::as_str)
            .and_then(|text| text.parse::<f64>().ok())
            .and_then(seconds_to_ms)
    };
    let duration = seconds(format.and_then(|format| format.get("duration")))
        .or_else(|| seconds(stream.get("duration")))
        .filter(|ms| *ms > 0)
        .ok_or_else(|| media("the duration is unknown, so the timeline cannot be sampled"))?;
    let duration_ms = u64::try_from(duration).unwrap_or(0);
    if duration_ms > crate::args::SOURCE_MS_MAX {
        return Err(media("the video is longer than one hour"));
    }
    let start_offset_ms = seconds(stream.get("start_time"))
        .or_else(|| seconds(format.and_then(|format| format.get("start_time"))))
        .unwrap_or(0);
    Ok(MediaInfo {
        container: container.to_owned(),
        video_codec: stream
            .get("codec_name")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned(),
        video_stream: index,
        duration_ms,
        start_offset_ms,
        width,
        height,
        decoder_version: version.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn samples_are_the_centres_of_equal_portions() {
        let window = Window {
            start_ms: 1000,
            end_ms: 9000,
        };
        assert_eq!(centres(window, 4), vec![2000, 4000, 6000, 8000]);
        assert_eq!(centres(window, 1), vec![5000]);
    }

    #[test]
    fn the_last_showinfo_time_is_the_frame_time() {
        let log = "x pts:1 pts_time:12.5 pkt_dts\nnoise\ny n:0 pts_time:13.25 duration";
        assert_eq!(last_pts_ms(log), Some(13_250));
        assert_eq!(last_pts_ms("no frames here"), None);
    }

    fn document(codec_type: &str, extra: &Value) -> Value {
        let mut stream = json!({"index": 0, "codec_type": codec_type, "codec_name": "vp9",
            "width": 640, "height": 360});
        stream
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        json!({"format": {"format_name": "matroska,webm", "duration": "2.5"},
               "streams": [stream]})
    }

    #[test]
    fn a_probe_selects_the_video_stream_and_refuses_what_it_cannot_sample() {
        let info = info_from_probe(&document("video", &json!({})), "v").unwrap();
        assert_eq!((info.video_codec.as_str(), info.duration_ms), ("vp9", 2500));
        let no_video = info_from_probe(&document("audio", &json!({})), "v").unwrap_err();
        assert!(no_video.to_string().contains("no video stream"));
        let cover = document("video", &json!({"disposition": {"attached_pic": 1}}));
        assert!(info_from_probe(&cover, "v").is_err());
        let huge = document("video", &json!({"width": 9000}));
        assert!(
            info_from_probe(&huge, "v")
                .unwrap_err()
                .to_string()
                .contains("bounds")
        );
        let mut playlist = document("video", &json!({}));
        playlist["format"]["format_name"] = json!("hls");
        assert!(
            info_from_probe(&playlist, "v")
                .unwrap_err()
                .to_string()
                .contains("other files")
        );
        let mut long = document("video", &json!({}));
        long["format"]["duration"] = json!("3601");
        assert!(
            info_from_probe(&long, "v")
                .unwrap_err()
                .to_string()
                .contains("one hour")
        );
    }

    #[test]
    fn a_reduced_build_is_named_not_accepted() {
        let decoders = " V..... h264 H.264\n V..... vp9 VP9\n";
        let demuxers = " D  matroska,webm Matroska\n";
        let missing = missing_components(decoders, demuxers);
        assert!(missing.iter().any(|m| m.contains("AV1")), "{missing:?}");
        assert!(missing.iter().any(|m| m.contains("hevc")), "{missing:?}");
        assert!(missing.iter().any(|m| m.contains("avi")), "{missing:?}");
        assert!(!missing.iter().any(|m| m.contains("webm")), "{missing:?}");
    }

    /// A process that overflows its output bound is killed at once and reported as the bound,
    /// which is what lets a frame too large at one quality be retried at a lower one. Waiting for
    /// it instead stalls on the blocked write until the deadline, and reads as a timeout.
    #[cfg(unix)]
    #[test]
    fn an_output_over_its_bound_is_cut_off_at_once_not_at_the_deadline() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let started = std::time::Instant::now();
        let flood = ["-c".to_owned(), "head -c 4000000 /dev/zero".to_owned()];
        let result = runtime.block_on(run(Path::new("/bin/sh"), &flood, 1024 * 1024));
        assert!(matches!(result, Err(RunError::OutputBound)));
        assert!(
            started.elapsed() < PROCESS_DEADLINE / 3,
            "the overflow took {:?}",
            started.elapsed()
        );
        // The other direction: output within the bound is returned whole, not cut off.
        let small = ["-c".to_owned(), "head -c 1000 /dev/zero".to_owned()];
        let Ok(within) = runtime.block_on(run(Path::new("/bin/sh"), &small, 1024 * 1024)) else {
            panic!("output within the bound is not an error");
        };
        assert_eq!((within.stdout.len(), within.success), (1000, true));
    }

    /// Dropping a call kills its decoder: a cancelled `read_video` leaves nothing running.
    #[cfg(unix)]
    #[test]
    fn a_dropped_run_kills_its_process() {
        let directory = tempfile::tempdir().unwrap();
        let pid_file = directory.path().join("pid");
        let script = format!("echo $$ > {}; exec sleep 60", pid_file.display());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let started = tokio::time::timeout(
                Duration::from_millis(500),
                run(Path::new("/bin/sh"), &["-c".to_owned(), script], 1024),
            )
            .await;
            assert!(
                started.is_err(),
                "the process outlives the call that was dropped"
            );
        });
        let pid = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .to_owned();
        // A killed child stays a zombie until the runtime's reaper runs, and `kill -0` succeeds on
        // a zombie, so "alive" is a process state that is neither gone nor zombie.
        let alive = || {
            let state = std::process::Command::new("ps")
                .args(["-o", "stat=", "-p", &pid])
                .output()
                .unwrap();
            let state = String::from_utf8_lossy(&state.stdout);
            !state.trim().is_empty() && !state.trim().starts_with('Z')
        };
        // The kill is asynchronous with the drop, so give the reaper a moment before asserting.
        let mut waited = 0;
        while alive() && waited < 50 {
            std::thread::sleep(Duration::from_millis(50));
            waited += 1;
        }
        assert!(
            !alive(),
            "process {pid} was still running after its call was dropped"
        );
    }
}
