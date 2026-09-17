//! Server-sent-events framing, shared by the adapters that speak it.
//!
//! Every OpenAI-compatible provider streams a response the same way at the
//! framing level: `data:`-prefixed lines, blank separators, colon-prefixed
//! keep-alives, and a `[DONE]` sentinel. What differs between providers is the
//! JSON *inside* a payload, which is each adapter's business; the framing is not,
//! so it lives here where it can be tested once.
//!
//! ## Why a byte sink rather than a line reader
//!
//! A network chunk can split a frame anywhere, including in the middle of a
//! multi-byte character, so the decoder buffers bytes and only consumes whole
//! lines. A server that closes without a trailing newline still sent its last
//! frame, so [`SseFrames::finish`] flushes the tail — dropping it would lose the
//! final delta of a response, which is typically the tool call that ends a step.
//!
//! The sentinel is tested in **both** paths, because a stream that closes
//! immediately after `data: [DONE]` leaves the sentinel as the tail. Returning it
//! there made a caller try to parse `[DONE]` as JSON, failing a turn whose answer
//! had already arrived in full.

/// The `data:` field prefix carrying a payload.
const DATA_PREFIX: &str = "data:";

/// The sentinel that marks the end of a stream.
const DONE_SENTINEL: &str = "[DONE]";

/// A decoder for server-sent-events framing.
#[derive(Debug, Default)]
pub struct SseFrames {
    pending: Vec<u8>,
    done: bool,
}

impl SseFrames {
    /// Creates an empty decoder.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            pending: Vec::new(),
            done: false,
        }
    }

    /// Returns `true` once the terminal sentinel has been seen.
    #[must_use]
    pub const fn is_done(&self) -> bool {
        self.done
    }

    /// Feeds a network chunk and returns every complete payload it completed.
    ///
    /// A payload is the text after `data:` on a line, with the trailing newline
    /// removed. The `[DONE]` sentinel is consumed rather than returned.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.pending.extend_from_slice(chunk);
        let mut payloads = Vec::new();
        // Only whole lines are consumed, so a partial frame stays buffered.
        while let Some(index) = self.pending.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=index).collect();
            let Some(payload) = decode_line(&line) else {
                continue;
            };
            if payload == DONE_SENTINEL {
                self.done = true;
                continue;
            }
            payloads.push(payload);
        }
        // Postcondition: whatever remains is a partial line, never a whole one, so a
        // frame cannot be emitted twice.
        assert!(!self.pending.contains(&b'\n'), "whole lines are drained");
        payloads
    }

    /// Decodes whatever remains after the stream ends.
    ///
    /// A server that closes without a final newline still sent a complete frame, so
    /// discarding the tail would lose the last delta of a response.
    pub fn finish(&mut self) -> Option<String> {
        if self.pending.is_empty() {
            return None;
        }
        let line = std::mem::take(&mut self.pending);
        let payload = decode_line(&line)?;
        if payload == DONE_SENTINEL {
            self.done = true;
            return None;
        }
        Some(payload)
    }
}

/// Decodes one line, returning its payload when it carries one.
fn decode_line(line: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(line);
    let trimmed = text.trim_end_matches(['\r', '\n']);
    if trimmed.is_empty() {
        return None;
    }
    // Keep-alive comments start with a colon and carry no payload. An `event:`
    // line names the frame's type for protocols that need it (Anthropic does),
    // and it is not this decoder's job to interpret it: the adapter reads the
    // `type` field inside the payload, which every such protocol also sends.
    if trimmed.starts_with(':') {
        return None;
    }
    let payload = trimmed.strip_prefix(DATA_PREFIX)?.trim_start();
    Some(payload.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The case a line reader gets wrong: a frame split mid-payload.
    #[test]
    fn a_frame_split_across_chunks_is_reassembled() {
        let mut frames = SseFrames::new();
        assert!(frames.push(b"data: {\"a\":").is_empty());
        assert_eq!(frames.push(b"1}\n\n"), vec!["{\"a\":1}".to_owned()]);
    }

    #[test]
    fn comments_and_blank_lines_carry_nothing() {
        let mut frames = SseFrames::new();
        let payloads = frames.push(b": keep-alive\n\nevent: message\ndata: {\"x\":1}\n");
        assert_eq!(payloads, vec!["{\"x\":1}".to_owned()]);
        assert!(!frames.is_done());
    }

    #[test]
    fn the_sentinel_ends_the_stream_without_being_returned() {
        let mut frames = SseFrames::new();
        assert!(frames.push(b"data: [DONE]\n").is_empty());
        assert!(frames.is_done());
    }

    /// The tail path: a server that closes immediately after the sentinel leaves it
    /// with no newline, and returning it as a payload failed the turn.
    #[test]
    fn a_sentinel_with_no_trailing_newline_ends_the_stream() {
        let mut frames = SseFrames::new();
        let payloads = frames.push(b"data: {\"id\":1}\ndata: [DONE]");
        assert_eq!(payloads, vec!["{\"id\":1}".to_owned()]);
        assert!(frames.finish().is_none(), "the sentinel is consumed");
        assert!(frames.is_done());
    }

    /// The other half of the pair: a genuine tail is still a frame.
    #[test]
    fn a_real_tail_is_decoded_once() {
        let mut frames = SseFrames::new();
        assert!(frames.push(b"data: {\"z\":9}").is_empty());
        assert_eq!(frames.finish(), Some("{\"z\":9}".to_owned()));
        assert_eq!(frames.finish(), None, "the tail is consumed once");
    }

    /// A multi-byte character split across two chunks must not become mojibake.
    ///
    /// The decoder buffers bytes rather than text, so this is the property that
    /// makes it a byte sink: a chunk boundary inside `é` is not a character
    /// boundary, and a lossy decode of half a character would corrupt the payload.
    #[test]
    fn a_multibyte_character_split_across_chunks_survives() {
        let mut frames = SseFrames::new();
        let payload = "data: \"café\"\n";
        let bytes = payload.as_bytes();
        let split = bytes.len().saturating_sub(2);
        let (head, tail) = bytes.split_at(split);
        assert!(frames.push(head).is_empty(), "the frame is incomplete");
        let payloads = frames.push(tail);
        assert_eq!(payloads, vec!["\"café\"".to_owned()]);
    }
}
