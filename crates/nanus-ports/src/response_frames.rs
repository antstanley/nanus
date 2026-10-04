//! Optional bounded SSE framing. Legacy framing stays available to stock callers.
use crate::{LlmError, LlmResult, ResponseLimits, SseFrames};

/// The API adapters' common framing boundary, selected once before the stream starts.
#[derive(Debug)]
pub struct ResponseFrames {
    reader: Reader,
}
#[derive(Debug)]
enum Reader {
    Legacy(SseFrames),
    Limited(Limited),
}
#[derive(Debug)]
struct Limited {
    limits: ResponseLimits,
    pending: Vec<u8>,
    received: usize,
    events: usize,
    done: bool,
    failed: bool,
}
impl ResponseFrames {
    /// Selects legacy framing or the caller's immutable budgets.
    #[must_use]
    pub fn new(limits: Option<ResponseLimits>) -> Self {
        let reader = limits.map_or_else(
            || Reader::Legacy(SseFrames::new()),
            |limits| {
                Reader::Limited(Limited {
                    limits,
                    pending: Vec::new(),
                    received: 0,
                    events: 0,
                    done: false,
                    failed: false,
                })
            },
        );
        Self { reader }
    }
    /// Whether the transport sentinel has been consumed.
    #[must_use]
    pub const fn is_done(&self) -> bool {
        match &self.reader {
            Reader::Legacy(reader) => reader.is_done(),
            Reader::Limited(reader) => reader.done,
        }
    }
    /// Checks a chunk before copying it. Failure discards pending bytes and is sticky.
    /// # Errors
    /// Refuses excessive raw/line/payload bytes, payload counts, allocation failure or invalid UTF-8.
    pub fn push(&mut self, chunk: &[u8]) -> LlmResult<Vec<String>> {
        match &mut self.reader {
            Reader::Legacy(reader) => Ok(reader.push(chunk)),
            Reader::Limited(reader) => {
                let result = reader.push(chunk);
                if result.is_err() {
                    reader.fail();
                }
                result
            }
        }
    }
    /// Decodes an unterminated final line once; protocol termination is the adapter's decision.
    /// # Errors
    /// Uses the same payload/count/UTF-8 checks as complete lines.
    pub fn finish(&mut self) -> LlmResult<Option<String>> {
        match &mut self.reader {
            Reader::Legacy(reader) => Ok(reader.finish()),
            Reader::Limited(reader) => {
                let result = reader.line();
                if result.is_err() {
                    reader.fail();
                }
                result
            }
        }
    }
}
impl Limited {
    fn push(&mut self, chunk: &[u8]) -> LlmResult<Vec<String>> {
        if self.failed {
            return Err(malformed("response framing already failed"));
        }
        self.received = ResponseLimits::add(
            "response bytes",
            self.received,
            chunk.len(),
            self.limits.response_bytes(),
        )?;
        let mut payloads = Vec::new();
        for piece in chunk.split_inclusive(|byte| *byte == b'\n') {
            if self.done {
                break;
            }
            let complete = piece.last() == Some(&b'\n');
            let content = if complete {
                piece
                    .get(..piece.len().saturating_sub(1))
                    .unwrap_or_default()
            } else {
                piece
            };
            ResponseLimits::add(
                "SSE line bytes",
                self.pending.len(),
                content.len(),
                self.limits.line_bytes(),
            )?;
            self.pending
                .try_reserve_exact(content.len())
                .map_err(|_| malformed("could not allocate bounded SSE line"))?;
            self.pending.extend_from_slice(content);
            if complete && let Some(payload) = self.line()? {
                payloads.push(payload);
            }
        }
        assert!(self.pending.len() <= self.limits.line_bytes());
        Ok(payloads)
    }
    fn line(&mut self) -> LlmResult<Option<String>> {
        if self.failed {
            return Err(malformed("response framing already failed"));
        }
        if self.done || self.pending.is_empty() {
            return Ok(None);
        }
        let text = core::str::from_utf8(&self.pending)
            .map_err(|_| malformed("SSE line is not valid UTF-8"))?;
        let data = text
            .trim_end_matches('\r')
            .strip_prefix("data:")
            .map(str::trim_start);
        let result = match data {
            Some("[DONE]") => {
                self.done = true;
                None
            }
            Some(data) => {
                ResponseLimits::add(
                    "decoded event bytes",
                    0,
                    data.len(),
                    self.limits.event_bytes(),
                )?;
                self.events =
                    ResponseLimits::add("response events", self.events, 1, self.limits.events())?;
                Some(data.to_owned())
            }
            None => None,
        };
        self.pending.clear();
        Ok(result)
    }
    fn fail(&mut self) {
        self.pending.clear();
        self.failed = true;
    }
}
fn malformed(message: &str) -> LlmError {
    LlmError::MalformedStream {
        message: message.into(),
    }
}

#[cfg(test)]
#[path = "response_frames_tests.rs"]
mod tests;
