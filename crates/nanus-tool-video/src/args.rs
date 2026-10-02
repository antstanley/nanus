//! The request contract: strict, bounded, and checked before any I/O.

use serde_json::{Map, Value};

use crate::VideoError;

/// Longest window one call may inspect.
pub const WINDOW_MS_MAX: u64 = 60_000;
/// Most frames one call may return or analyse.
pub const FRAMES_MAX: u32 = 4;
/// Longest accepted path or question, in characters.
pub const TEXT_CHARS_MAX: usize = 4096;
/// The longest source the extension will inspect, in milliseconds (one hour).
pub const SOURCE_MS_MAX: u64 = 3_600_000;

/// What the caller asked the tool to return.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestedMode {
    /// Pixels for a verified vision model, text for any other.
    Auto,
    /// Pixels, refused unless the active model has verified image input.
    Frames,
    /// A same-provider vision model's text.
    Analyze,
}

impl RequestedMode {
    /// The name the manifest records.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Frames => "frames",
            Self::Analyze => "analyze",
        }
    }
}

/// A validated `read_video` request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoReadArguments {
    /// The workspace-relative path.
    pub file_path: String,
    /// The requested delivery.
    pub mode: RequestedMode,
    /// Window start on the source timeline.
    pub start_ms: u64,
    /// Exclusive window end, when given.
    pub end_ms: Option<u64>,
    /// How many frames to sample.
    pub max_frames: u32,
    /// What the caller wants to know.
    pub question: Option<String>,
}

impl VideoReadArguments {
    /// Validates the model's argument object.
    ///
    /// # Errors
    ///
    /// Returns [`VideoError::Argument`] naming the field, so the model can correct the call.
    pub fn parse(arguments: &Map<String, Value>) -> Result<Self, VideoError> {
        const KNOWN: [&str; 6] = [
            "file_path",
            "mode",
            "start_ms",
            "end_ms",
            "max_frames",
            "question",
        ];
        if let Some(unknown) = arguments.keys().find(|key| !KNOWN.contains(&key.as_str())) {
            return Err(bad(format!("read_video: unknown argument `{unknown}`")));
        }
        let file_path = text(arguments, "file_path")?
            .ok_or_else(|| bad("read_video: file_path is required".to_owned()))?;
        if file_path.contains("://") || file_path.contains('\0') {
            return Err(bad(
                "read_video: file_path is a workspace path, not a URL".to_owned()
            ));
        }
        let mode = match text(arguments, "mode")?.as_deref() {
            None | Some("auto") => RequestedMode::Auto,
            Some("frames") => RequestedMode::Frames,
            Some("analyze") => RequestedMode::Analyze,
            Some(other) => {
                return Err(bad(format!(
                    "read_video: mode `{other}` is not auto, frames or analyze"
                )));
            }
        };
        let start_ms = integer(arguments, "start_ms", SOURCE_MS_MAX)?.unwrap_or(0);
        let end_ms = integer(arguments, "end_ms", SOURCE_MS_MAX)?;
        if let Some(end) = end_ms {
            if end <= start_ms {
                return Err(bad("read_video: end_ms must be after start_ms".to_owned()));
            }
            if end.saturating_sub(start_ms) > WINDOW_MS_MAX {
                return Err(bad(format!(
                    "read_video: a window is at most {WINDOW_MS_MAX} ms; read a longer video in parts"
                )));
            }
        }
        let max_frames = integer(arguments, "max_frames", u64::from(FRAMES_MAX))?
            .map_or(FRAMES_MAX, |value| {
                u32::try_from(value).unwrap_or(FRAMES_MAX)
            });
        if max_frames == 0 {
            return Err(bad(format!(
                "read_video: max_frames is between 1 and {FRAMES_MAX}"
            )));
        }
        Ok(Self {
            file_path,
            mode,
            start_ms,
            end_ms,
            max_frames,
            question: text(arguments, "question")?,
        })
    }
}

fn bad(message: String) -> VideoError {
    VideoError::Argument(message)
}

/// A present string, nonblank and within the character bound.
fn text(arguments: &Map<String, Value>, field: &str) -> Result<Option<String>, VideoError> {
    match arguments.get(field) {
        None => Ok(None),
        Some(Value::String(value)) => {
            if value.trim().is_empty() || value.chars().count() > TEXT_CHARS_MAX {
                return Err(bad(format!(
                    "read_video: {field} is 1 to {TEXT_CHARS_MAX} characters"
                )));
            }
            Ok(Some(value.clone()))
        }
        Some(_) => Err(bad(format!("read_video: {field} must be a string"))),
    }
}

/// A present nonnegative integer no greater than `max`.
fn integer(
    arguments: &Map<String, Value>,
    field: &str,
    max: u64,
) -> Result<Option<u64>, VideoError> {
    arguments
        .get(field)
        .map_or(Ok(None), |value| match value.as_u64() {
            Some(number) if number <= max => Ok(Some(number)),
            _ => Err(bad(format!(
                "read_video: {field} must be an integer from 0 to {max}"
            ))),
        })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn parse(value: &Value) -> Result<VideoReadArguments, VideoError> {
        VideoReadArguments::parse(value.as_object().unwrap())
    }

    #[test]
    fn defaults_apply_to_a_bare_path() {
        let parsed = parse(&json!({"file_path": "a.webm"})).unwrap();
        assert_eq!(parsed.mode, RequestedMode::Auto);
        assert_eq!(
            (parsed.start_ms, parsed.end_ms, parsed.max_frames),
            (0, None, 4)
        );
    }

    #[test]
    fn every_bound_refuses_the_call_that_breaks_it() {
        for bad in [
            json!({}),
            json!({"file_path": ""}),
            json!({"file_path": "  "}),
            json!({"file_path": "https://x/y.mp4"}),
            json!({"file_path": "a", "mode": "movie"}),
            json!({"file_path": "a", "max_frames": 0}),
            json!({"file_path": "a", "max_frames": 5}),
            json!({"file_path": "a", "start_ms": 5, "end_ms": 5}),
            json!({"file_path": "a", "start_ms": 0, "end_ms": 60_001}),
            json!({"file_path": "a", "start_ms": -1}),
            json!({"file_path": "a", "question": 3}),
            json!({"file_path": "a", "codec": "h264"}),
        ] {
            assert!(parse(&bad).is_err(), "{bad} should be refused");
        }
        let edge = parse(&json!({"file_path": "a", "start_ms": 10, "end_ms": 60_010})).unwrap();
        assert_eq!(edge.end_ms, Some(60_010));
    }
}
