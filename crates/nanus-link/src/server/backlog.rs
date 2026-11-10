//! Cutting a running turn's backlog into frames a client will read.
//!
//! A client refuses a line longer than [`MAX_FRAME_BYTES`], and nothing bounded what the agent
//! sent: the backlog of a long turn — every tool call with its arguments, every folded answer —
//! went out as one frame however large it had grown. A legacy session's backlog is the whole
//! running turn, so the turns most worth attaching to were the ones whose attachment a client
//! could not read.
//!
//! So the backlog is sent as as many [`Frame::Backlog`] frames as it takes, each under a budget
//! well inside the cap. Nothing about the frame changes: a client applies the segments of each
//! in order, which is what it did with one, and the frames are queued back to back in the region
//! that queues the attachment, so no live frame comes between them. A client that predates the
//! split reads them as it always read a backlog.
//!
//! Pure, and separate from the server, so the cutting is tested without a socket.

use crate::protocol::{BacklogSegment, Frame, encode};
use crate::wire::MAX_FRAME_BYTES;

/// The most a backlog frame's segments may come to, encoded, in bytes.
///
/// A quarter of the cap rather than all of it: the frame carries its stream mark and its own
/// punctuation beside the segments, and a budget with room to spare does not have to count
/// either exactly.
pub(super) const BACKLOG_FRAME_BYTES: usize = 1024 * 1024;

const _: () = assert!(
    BACKLOG_FRAME_BYTES.saturating_mul(2) <= MAX_FRAME_BYTES,
    "a backlog frame's budget leaves room inside the frame cap"
);

/// How many bytes of text one piece of a divided delta holds, for a frame budget of `budget`.
///
/// An eighth of the budget, because encoding can lengthen text sixfold — a control character
/// is six bytes of JSON — and a piece has to fit its frame once it has.
const fn piece_bytes(budget: usize) -> usize {
    let piece = budget >> 3;
    // Room for one character whatever the budget, so dividing always makes progress.
    if piece < 4 { 4 } else { piece }
}

/// Cuts `segments` into batches, each of which encodes to about `budget` bytes or fewer.
///
/// Order is kept, within a batch and across them. A segment that is too large by itself is
/// divided when it is a delta — a folded delta is one piece of text however it is divided, so
/// unfolding it changes nothing a client draws — and is otherwise sent alone, as the one thing
/// that cannot be made smaller without changing what it says.
pub(super) fn batches(segments: Vec<BacklogSegment>, budget: usize) -> Vec<Vec<BacklogSegment>> {
    assert!(budget > 0, "a backlog frame has room for something");
    let mut cut: Vec<Vec<BacklogSegment>> = Vec::new();
    let mut current: Vec<BacklogSegment> = Vec::new();
    let mut used: usize = 0;
    for segment in segments
        .into_iter()
        .flat_map(|segment| divided(segment, budget))
    {
        let size = encoded_len(&segment, budget);
        if !current.is_empty() && used.saturating_add(size) > budget {
            cut.push(core::mem::take(&mut current));
            used = 0;
        }
        // One more for the comma that separates it from the next.
        used = used.saturating_add(size).saturating_add(1);
        current.push(segment);
    }
    if !current.is_empty() {
        cut.push(current);
    }
    cut
}

/// The encoded length of a segment, or `budget` for one that cannot be encoded: such a segment
/// is sent alone, where the writer reports the failure the encoding is.
fn encoded_len(segment: &BacklogSegment, budget: usize) -> usize {
    encode(segment).map_or(budget, |line| line.len())
}

/// Divides a delta too large for one frame into pieces that fit; anything else is itself.
fn divided(segment: BacklogSegment, budget: usize) -> Vec<BacklogSegment> {
    if encoded_len(&segment, budget) <= budget {
        return vec![segment];
    }
    let BacklogSegment {
        frame_id,
        turn,
        step,
        frame,
    } = segment;
    let piece = |frame| BacklogSegment {
        frame_id,
        turn,
        step,
        frame,
    };
    match frame {
        Frame::Text { delta } => pieces(&delta, piece_bytes(budget))
            .into_iter()
            .map(|delta| piece(Frame::Text { delta }))
            .collect(),
        Frame::Reasoning { delta } => pieces(&delta, piece_bytes(budget))
            .into_iter()
            .map(|delta| piece(Frame::Reasoning { delta }))
            .collect(),
        other => {
            tracing::warn!(
                frame_id,
                "a backlog segment is larger than a frame and cannot be divided"
            );
            vec![piece(other)]
        }
    }
}

/// Divides `text` into pieces of at most `max` bytes, on character boundaries.
fn pieces(text: &str, max: usize) -> Vec<String> {
    assert!(max >= 4, "a piece has room for any one character");
    let mut cut = Vec::new();
    let mut current = String::new();
    for character in text.chars() {
        if current.len().saturating_add(character.len_utf8()) > max {
            cut.push(core::mem::take(&mut current));
        }
        current.push(character);
    }
    if !current.is_empty() {
        cut.push(current);
    }
    cut
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{FrontierInfo, StreamMark, decode};

    fn mark() -> StreamMark {
        StreamMark {
            stream_epoch: "p1-t2-h3".to_owned(),
            stream_watermark: 60,
            frontier: FrontierInfo {
                session_id: "s".to_owned(),
                event_count: 0,
                prefix_blake3: "ab".repeat(32),
                projection_revision: 0,
            },
            compaction_boundary: None,
        }
    }

    fn segment(frame_id: u64, frame: Frame) -> BacklogSegment {
        BacklogSegment {
            frame_id,
            turn: Some(0),
            step: Some(1),
            frame,
        }
    }

    fn tool(frame_id: u64, bytes: usize) -> BacklogSegment {
        segment(
            frame_id,
            Frame::Tool {
                call_id: Some(format!("call-{frame_id}")),
                name: "write".to_owned(),
                arguments: serde_json::json!({ "content": "x".repeat(bytes) }),
            },
        )
    }

    /// The text a run of segments draws, deltas joined as a client joins them.
    fn text_of(segments: &[BacklogSegment]) -> String {
        segments
            .iter()
            .filter_map(|segment| match &segment.frame {
                Frame::Text { delta } => Some(delta.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_backlog_that_fits_one_frame_is_one_batch_unchanged() {
        let segments = vec![
            segment(
                1,
                Frame::User {
                    text: "do it".to_owned(),
                },
            ),
            segment(2, Frame::Step { step: 1 }),
            tool(3, 100),
        ];
        let cut = batches(segments.clone(), BACKLOG_FRAME_BYTES);
        assert_eq!(cut, vec![segments]);
        assert!(batches(Vec::new(), BACKLOG_FRAME_BYTES).is_empty());
    }

    /// The case the cap used to turn into an attachment nobody could read: a long turn's
    /// backlog is cut into frames that each fit, with every segment kept, in order.
    #[test]
    fn a_backlog_larger_than_a_frame_is_cut_into_frames_a_client_reads() {
        // Sixty writes of a hundred kilobytes: six megabytes, half as much again as the cap.
        let segments: Vec<BacklogSegment> = (1..=60).map(|id| tool(id, 100_000)).collect();
        let whole = Frame::Backlog {
            stream: mark(),
            segments: segments.clone(),
        };
        assert!(
            encode(&whole).is_ok_and(|line| line.len() > MAX_FRAME_BYTES),
            "the control: as one frame it is past what a client reads"
        );

        let cut = batches(segments.clone(), BACKLOG_FRAME_BYTES);
        assert!(cut.len() > 1, "it takes more than one frame");
        for batch in &cut {
            let frame = Frame::Backlog {
                stream: mark(),
                segments: batch.clone(),
            };
            let line = encode(&frame).unwrap_or_default();
            assert!(
                !line.is_empty() && line.len() <= MAX_FRAME_BYTES,
                "each frame is inside the cap: {}",
                line.len()
            );
            assert_eq!(decode::<Frame>(&line).ok(), Some(frame), "and reads back");
        }
        let rejoined: Vec<BacklogSegment> = cut.into_iter().flatten().collect();
        assert_eq!(rejoined, segments, "nothing is lost or reordered");
    }

    /// A folded delta larger than a frame is unfolded: the same text, the same id, in pieces
    /// that fit — including text that grows sixfold when it is encoded.
    #[test]
    fn a_delta_larger_than_a_frame_is_divided_without_changing_its_text() {
        let budget = 4096;
        for text in [
            "plain text ".repeat(2000),
            "\u{1}".repeat(3000),
            "héllo wörld — ünïcode ".repeat(900),
        ] {
            let folded = segment(
                9,
                Frame::Text {
                    delta: text.clone(),
                },
            );
            let cut = batches(vec![folded], budget);
            assert!(cut.len() > 1, "one frame does not hold it");
            for batch in &cut {
                let size: usize = batch
                    .iter()
                    .map(|piece| encoded_len(piece, usize::MAX))
                    .sum();
                assert!(size <= budget, "a batch of {size} bytes is past {budget}");
                assert!(batch.iter().all(|piece| piece.frame_id == 9));
            }
            let rejoined: Vec<BacklogSegment> = cut.into_iter().flatten().collect();
            assert_eq!(text_of(&rejoined), text);
        }
    }

    /// The limit of it, stated: a call whose arguments alone are past the budget cannot be
    /// divided without changing what the call is, so it is sent whole and alone, and what
    /// surrounds it is still cut to fit.
    #[test]
    fn a_segment_that_cannot_be_divided_is_sent_alone() {
        let budget = 4096;
        let segments = vec![tool(1, 100), tool(2, 10_000), tool(3, 100)];
        let cut = batches(segments.clone(), budget);
        assert_eq!(
            cut,
            vec![
                vec![segments[0].clone()],
                vec![segments[1].clone()],
                vec![segments[2].clone()]
            ]
        );
    }
}
