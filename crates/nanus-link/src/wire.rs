//! Moving frames over a stream.
//!
//! One frame per line, in both directions. The reader is generic over anything buffered
//! and the writer over anything writable, so the same functions serve a socket and a
//! connected pair used by a test.
//!
//! ## Why the cap is checked after the read rather than during it
//!
//! A genuinely bounded read needs its own chunk loop, and the peer here is the same user
//! on a socket only they can reach: a process that can connect already has the
//! workspace. The cap is therefore about not handing an unbounded amount of *parsing* to
//! a peer that is confused or corrupt, not about defending against a hostile one. That
//! distinction is written down rather than implied, because it is the kind of thing that
//! gets quietly upgraded into a security claim.

use serde::de::DeserializeOwned;
use tokio::io::{AsyncBufRead, AsyncBufReadExt as _, AsyncWrite, AsyncWriteExt as _};

use crate::error::{LinkError, LinkResult};
use crate::protocol::{Frame, Request, encode};

/// The largest single frame accepted, in bytes.
///
/// A frame is a model delta or a control message, and a tool result never travels as a
/// frame — a tool result is part of the session, and the session is the agent's. So this
/// is generous by two orders of magnitude.
pub const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;

/// Reads one line, or `None` at end of stream.
///
/// # Errors
///
/// Returns [`LinkError::Io`] when the read fails and [`LinkError::Protocol`] when the
/// line is longer than [`MAX_FRAME_BYTES`].
pub async fn read_line<R>(reader: &mut R) -> LinkResult<Option<String>>
where
    R: AsyncBufRead + Unpin,
{
    let mut line = String::new();
    let read = reader.read_line(&mut line).await?;
    if read == 0 {
        return Ok(None);
    }
    if line.len() > MAX_FRAME_BYTES {
        return Err(LinkError::protocol(format!(
            "a frame of {} bytes exceeds the {MAX_FRAME_BYTES}-byte limit",
            line.len()
        )));
    }
    Ok(Some(line))
}

/// Reads one message, or `None` at end of stream.
///
/// # Errors
///
/// As [`read_line`], plus [`LinkError::Protocol`] when the line is not a message of the
/// requested type.
pub async fn read_message<T, R>(reader: &mut R) -> LinkResult<Option<T>>
where
    T: DeserializeOwned,
    R: AsyncBufRead + Unpin,
{
    read_line(reader)
        .await?
        .map_or_else(|| Ok(None), |line| crate::protocol::decode(&line).map(Some))
}

/// Reads one request, or `None` at end of stream.
///
/// # Errors
///
/// As [`read_message`].
pub async fn read_request<R>(reader: &mut R) -> LinkResult<Option<Request>>
where
    R: AsyncBufRead + Unpin,
{
    read_message::<Request, R>(reader).await
}

/// Writes one frame and flushes it.
///
/// Flushed per frame rather than buffered until the turn ends: a streamed delta that
/// arrives only when the turn is over is not a stream, and the interface this exists for
/// is judged on whether it moves.
///
/// # Errors
///
/// Returns [`LinkError::Io`] when the write fails and [`LinkError::Protocol`] when the
/// frame cannot be encoded.
pub async fn write_frame<W>(writer: &mut W, frame: &Frame) -> LinkResult<()>
where
    W: AsyncWrite + Unpin,
{
    let mut line = encode(frame)?;
    line.push('\n');
    writer.write_all(line.as_bytes()).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Request;

    #[tokio::test]
    async fn a_written_frame_reads_back_identically() {
        let mut buffer: Vec<u8> = Vec::new();
        let frame = Frame::Text {
            delta: "one\ntwo".to_owned(),
        };
        let written = write_frame(&mut buffer, &frame).await;
        assert!(written.is_ok(), "{written:?}");
        // The delta's newline is escaped, so the whole frame is still one line.
        assert_eq!(String::from_utf8_lossy(&buffer).lines().count(), 1);
        assert!(buffer.ends_with(b"\n"), "the frame is newline-terminated");

        let mut reader = std::io::Cursor::new(&buffer);
        let read = read_message::<Frame, _>(&mut reader).await;
        assert_eq!(read.ok().flatten(), Some(frame));
    }

    #[tokio::test]
    async fn an_exhausted_stream_reads_as_nothing_rather_than_an_error() {
        // A client that closes its end is the ordinary way a connection ends, so it must
        // be distinguishable from a failure.
        let empty: &[u8] = b"";
        let mut reader = std::io::Cursor::new(empty);
        assert_eq!(
            read_message::<Frame, _>(&mut reader).await.ok().flatten(),
            None
        );
    }

    #[tokio::test]
    async fn a_line_that_is_not_a_frame_is_a_protocol_error() {
        let mut reader = std::io::Cursor::new(br#"{"frame":"nope"}"#.as_slice());
        let read = read_message::<Frame, _>(&mut reader).await;
        assert!(matches!(read, Err(LinkError::Protocol(_))), "{read:?}");
    }

    #[tokio::test]
    async fn the_same_line_decodes_as_the_type_that_was_asked_for() {
        // The reader is shared by both directions, so the requested type is what decides
        // how a line is read. A request must not decode as a frame.
        let mut reader = std::io::Cursor::new(br#"{"request":"shutdown"}"#.as_slice());
        let request = read_message::<Request, _>(&mut reader).await;
        assert_eq!(request.ok().flatten(), Some(Request::Shutdown));

        let mut reader = std::io::Cursor::new(br#"{"request":"shutdown"}"#.as_slice());
        assert!(read_message::<Frame, _>(&mut reader).await.is_err());
    }

    #[tokio::test]
    async fn an_oversized_line_is_refused() {
        let oversize = "x".repeat(MAX_FRAME_BYTES.saturating_add(1));
        let line = format!("{oversize}\n");
        let mut reader = std::io::Cursor::new(line.as_bytes());
        let read = read_message::<Frame, _>(&mut reader).await;
        assert!(matches!(read, Err(LinkError::Protocol(_))), "{read:?}");
    }
}
