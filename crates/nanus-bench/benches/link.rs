//! The local link: what a frame costs to put on the socket and to take off it.
//!
//! Every token the model streams crosses the link as its own `Text` or `Reasoning` frame, so
//! the per-frame encode and decode are paid once per token, on both sides, for every client
//! watching. A turn's worth of frames is measured as well as the single frames, because a
//! regression in a frame that is rare (a tool call with a large argument block) and one in a
//! frame that is constant (a delta) look the same in a single-frame benchmark and very
//! different in a turn.

use criterion::{BenchmarkId, Criterion, Throughput};
use nanus_bench::{Metric, fixtures};
use nanus_link::protocol::TurnEnd;
use nanus_link::{Frame, LinkResult, Request, decode, encode};
use serde_json::json;
use std::hint::black_box;

/// The deltas in a fixture turn: a long answer streamed a few characters at a time.
const DELTAS: usize = 2_000;

/// The reasoning deltas that precede the answer.
const REASONING_DELTAS: usize = 500;

fn tool() -> Frame {
    Frame::Tool {
        call_id: Some("call-0-read".to_owned()),
        name: "read".to_owned(),
        arguments: json!({
            "file_path": "crates/nanus-domain/src/session.rs",
            "offset": 1,
            "limit": 80
        }),
    }
}

fn tool_done() -> Frame {
    Frame::ToolDone {
        call_id: Some("call-0-read".to_owned()),
        name: "read".to_owned(),
        error: false,
    }
}

fn done() -> Frame {
    Frame::Done {
        answer: fixtures::markdown_answer(2),
        reason: TurnEnd::Completed,
    }
}

fn delta() -> Frame {
    Frame::Text {
        delta: "the log ".to_owned(),
    }
}

/// The frames of one turn, in the order the server sends them.
///
/// Reasoning first, then a step that calls two tools, then the streamed answer and its end:
/// the mix a watching client actually decodes, dominated by deltas.
fn turn() -> Vec<Frame> {
    let mut frames = Vec::with_capacity(DELTAS.saturating_add(16));
    frames.push(Frame::User {
        text: "Explain how the session log stays contiguous.".to_owned(),
    });
    frames.push(Frame::Step { step: 0 });
    for _ in 0..REASONING_DELTAS {
        frames.push(Frame::Reasoning {
            delta: "so a hole ".to_owned(),
        });
    }
    frames.push(tool());
    frames.push(tool_done());
    frames.push(Frame::Step { step: 1 });
    for _ in 0..DELTAS {
        frames.push(delta());
    }
    frames.push(Frame::Usage {
        tokens: 14_500,
        completion_tokens: 420,
        cache_hit_tokens: 13_800,
        cache_miss_tokens: 700,
        duration_ms: 9_000,
        reasoning_tokens: 90,
        head_ms: 300,
        ttft_ms: 650,
        decode_ms: 8_000,
    });
    frames.push(done());
    frames
}

fn encoded(frames: &[Frame]) -> Vec<String> {
    frames
        .iter()
        .map(|frame| encode(frame).unwrap_or_else(|error| unreachable!("fixture frame: {error}")))
        .collect()
}

fn total_bytes(lines: &[String]) -> u64 {
    let total = lines
        .iter()
        .fold(0_usize, |sum, line| sum.saturating_add(line.len()));
    u64::try_from(total).unwrap_or(u64::MAX)
}

/// Single frames, one each way: the per-token delta and the frames that carry payloads.
fn frames<M: Metric>(c: &mut Criterion<M>) {
    let mut group = c.benchmark_group(M::group("link/frame"));
    for (name, frame) in [
        ("text", delta()),
        ("tool", tool()),
        ("tool_done", tool_done()),
        ("done", done()),
    ] {
        let line = encode(&frame).unwrap_or_else(|error| unreachable!("fixture frame: {error}"));
        group.throughput(Throughput::Bytes(line.len() as u64));
        group.bench_with_input(BenchmarkId::new("encode", name), &frame, |b, frame| {
            b.iter(|| encode(black_box(frame)));
        });
        group.bench_with_input(BenchmarkId::new("decode", name), &line, |b, line| {
            b.iter(|| decode::<Frame>(black_box(line)));
        });
    }
    group.finish();
}

/// Requests, client to agent: rare compared with frames, but a prompt carries the user's
/// whole message.
fn requests<M: Metric>(c: &mut Criterion<M>) {
    let mut group = c.benchmark_group(M::group("link/request"));
    for (name, request) in [
        (
            "prompt",
            Request::Prompt {
                text: fixtures::prose(2_000),
            },
        ),
        (
            "approve",
            Request::Approve {
                call_id: "call-0-bash".to_owned(),
                allow: true,
                always: false,
            },
        ),
    ] {
        let line =
            encode(&request).unwrap_or_else(|error| unreachable!("fixture request: {error}"));
        group.throughput(Throughput::Bytes(line.len() as u64));
        group.bench_with_input(BenchmarkId::new("encode", name), &request, |b, request| {
            b.iter(|| encode(black_box(request)));
        });
        group.bench_with_input(BenchmarkId::new("decode", name), &line, |b, line| {
            b.iter(|| decode::<Request>(black_box(line)));
        });
    }
    group.finish();
}

/// A whole turn, line by line: what one watching client costs the agent and itself.
fn turn_frames<M: Metric>(c: &mut Criterion<M>) {
    let mut group = c.benchmark_group(M::group("link/turn"));
    let frames = turn();
    let lines = encoded(&frames);
    group.throughput(Throughput::ElementsAndBytes {
        elements: frames.len() as u64,
        bytes: total_bytes(&lines),
    });
    group.bench_function("encode", |b| {
        b.iter(|| {
            black_box(&frames)
                .iter()
                .map(encode)
                .collect::<LinkResult<Vec<String>>>()
        });
    });
    group.bench_function("decode", |b| {
        b.iter(|| {
            black_box(&lines)
                .iter()
                .map(|line| decode::<Frame>(line))
                .collect::<LinkResult<Vec<Frame>>>()
        });
    });
    group.finish();
}

/// The backlog a client receives on attaching to a busy session: one frame holding the turn.
///
/// It is the largest single line the link sends, so it is where a quadratic encoder or a
/// decoder that copies the line would show first.
fn backlog<M: Metric>(c: &mut Criterion<M>) {
    let mut group = c.benchmark_group(M::group("link/backlog"));
    let segments = turn()
        .into_iter()
        .zip(1_u64..)
        .map(|(frame, frame_id)| nanus_link::protocol::BacklogSegment {
            frame_id,
            turn: Some(0),
            step: Some(1),
            frame,
        })
        .collect();
    let frame = Frame::Backlog {
        stream: nanus_link::protocol::StreamMark {
            stream_epoch: String::from("p1-t0-h0"),
            stream_watermark: 1,
            frontier: nanus_link::protocol::FrontierInfo {
                session_id: String::from("s"),
                event_count: 0,
                prefix_sha256: "0".repeat(64),
                projection_revision: 0,
            },
        },
        segments,
    };
    let line = encode(&frame).unwrap_or_else(|error| unreachable!("fixture backlog: {error}"));
    group.throughput(Throughput::Bytes(line.len() as u64));
    group.bench_function("encode", |b| b.iter(|| encode(black_box(&frame))));
    group.bench_function("decode", |b| b.iter(|| decode::<Frame>(black_box(&line))));
    group.finish();
}

nanus_bench::benches!(frames, requests, turn_frames, backlog);
