//! The session log: building it, folding it into a request, and its JSONL round trip.
//!
//! These are the costs that grow with a conversation. `derive_messages` runs before every
//! model step, and `try_to_jsonl` before every save, so both are paid once per step over the
//! *whole* log; a session that is slow at a thousand turns is slow on every step of it.

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput};
use nanus_bench::{Metric, fixtures};
use nanus_domain::Session;
use std::hint::black_box;

/// Session lengths, in turns. Thirteen events each, so the largest is 6,500 events.
const TURNS: [u32; 3] = [10, 100, 500];

fn events(turns: u32) -> u64 {
    u64::from(turns).saturating_mul(fixtures::EVENTS_PER_TURN as u64)
}

/// Appending a whole session's events, one at a time, to an empty log.
fn build<M: Metric>(c: &mut Criterion<M>) {
    let mut group = c.benchmark_group(M::group("session/build"));
    for turns in TURNS {
        group.throughput(Throughput::Elements(events(turns)));
        group.bench_with_input(BenchmarkId::from_parameter(turns), &turns, |b, &turns| {
            b.iter(|| fixtures::session(black_box(turns)));
        });
    }
    group.finish();
}

/// Folding the log into the messages a model step sends.
fn derive_messages<M: Metric>(c: &mut Criterion<M>) {
    let mut group = c.benchmark_group(M::group("session/derive_messages"));
    for turns in TURNS {
        let session = fixtures::session(turns);
        group.throughput(Throughput::Elements(events(turns)));
        group.bench_with_input(
            BenchmarkId::from_parameter(turns),
            &session,
            |b, session| {
                b.iter(|| black_box(session).derive_messages());
            },
        );
    }
    group.finish();
}

/// Encoding for the store: the validated form every save takes, and the bare encoder.
fn encode<M: Metric>(c: &mut Criterion<M>) {
    let mut group = c.benchmark_group(M::group("session/encode"));
    for turns in TURNS {
        let session = fixtures::session(turns);
        let size = session.to_jsonl().len() as u64;
        group.throughput(Throughput::Bytes(size));
        group.bench_with_input(
            BenchmarkId::new("try_to_jsonl", turns),
            &session,
            |b, session| b.iter(|| black_box(session).try_to_jsonl()),
        );
        group.bench_with_input(
            BenchmarkId::new("to_jsonl", turns),
            &session,
            |b, session| b.iter(|| black_box(session).to_jsonl()),
        );
    }
    group.finish();
}

/// Decoding a stored session: what resuming or replaying one costs before anything is drawn.
fn decode<M: Metric>(c: &mut Criterion<M>) {
    let mut group = c.benchmark_group(M::group("session/decode"));
    for turns in TURNS {
        let raw = fixtures::session(turns).to_jsonl();
        group.throughput(Throughput::Bytes(raw.len() as u64));
        group.bench_with_input(BenchmarkId::from_parameter(turns), &raw, |b, raw| {
            b.iter(|| Session::from_jsonl(black_box(raw)));
        });
    }
    group.finish();
}

/// One more event on a long session: the per-event cost of the log itself, which should not
/// grow with the session.
fn append<M: Metric>(c: &mut Criterion<M>) {
    let mut group = c.benchmark_group(M::group("session/append"));
    let event = nanus_domain::SessionEvent::UserMessage {
        text: fixtures::prose(200),
    };
    for turns in TURNS {
        group.bench_with_input(BenchmarkId::from_parameter(turns), &turns, |b, &turns| {
            b.iter_batched(
                || (fixtures::session(turns), event.clone()),
                |(mut session, event)| {
                    session.append(event);
                    session
                },
                BatchSize::LargeInput,
            );
        });
    }
    group.finish();
}

nanus_bench::benches!(build, derive_messages, encode, decode, append);
