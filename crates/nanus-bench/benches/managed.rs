//! Managed context: what selecting a request costs, per step, as a session grows.
//!
//! Every managed step derives the fragments, folds the accepted state, computes what is
//! protected, hashes the prefix a frontier names, and compiles the effective conversation — all
//! over the *whole* log, before the adapter prepares anything. These are measured at the same
//! lengths as the session benchmarks so the two can be read side by side.

use criterion::{BenchmarkId, Criterion, Throughput};
use nanus_bench::{Metric, fixtures};
use nanus_domain::Session;
use nanus_domain::context::managed::{
    FragmentId, ManagedState, NoticeFacts, Selection, derive_effective_context, fragments, state,
};
use std::hint::black_box;

/// Session lengths, in turns.
const TURNS: [u32; 3] = [10, 100, 500];

fn session(turns: u32) -> Session {
    let mut session = fixtures::session(turns);
    session.upgrade_to_managed_body();
    session
}

fn events(turns: u32) -> u64 {
    u64::from(turns).saturating_mul(fixtures::EVENTS_PER_TURN as u64)
}

/// Deriving fragments and the protected set: the selection's starting point.
fn derive<M: Metric>(c: &mut Criterion<M>) {
    let mut group = c.benchmark_group(M::group("managed/derive"));
    for turns in TURNS {
        let session = session(turns);
        group.throughput(Throughput::Elements(events(turns)));
        group.bench_with_input(BenchmarkId::from_parameter(turns), &session, |b, session| {
            b.iter(|| {
                let fragments = fragments::derive(black_box(session).log());
                fragments.map(|fragments| state::protected(session.log(), &fragments))
            });
        });
    }
    group.finish();
}

/// Folding the accepted state, which a reload and every step both pay.
fn fold<M: Metric>(c: &mut Criterion<M>) {
    let mut group = c.benchmark_group(M::group("managed/fold"));
    for turns in TURNS {
        let session = session(turns);
        group.throughput(Throughput::Elements(events(turns)));
        group.bench_with_input(BenchmarkId::from_parameter(turns), &session, |b, session| {
            b.iter(|| ManagedState::fold(black_box(session).log()));
        });
    }
    group.finish();
}

/// Hashing the whole stored prefix, which a frontier check and every receipt pay.
fn frontier<M: Metric>(c: &mut Criterion<M>) {
    let mut group = c.benchmark_group(M::group("managed/frontier"));
    for turns in TURNS {
        let session = session(turns);
        group.throughput(Throughput::Bytes(session.to_jsonl().len() as u64));
        group.bench_with_input(BenchmarkId::from_parameter(turns), &session, |b, session| {
            b.iter(|| state::frontier(black_box(session), 0));
        });
    }
    group.finish();
}

/// Compiling the effective conversation with half the eligible fragments hidden.
fn compile<M: Metric>(c: &mut Criterion<M>) {
    let mut group = c.benchmark_group(M::group("managed/compile"));
    for turns in TURNS {
        let session = session(turns);
        let Ok(fragments) = fragments::derive(session.log()) else {
            continue;
        };
        let protected = state::protected(session.log(), &fragments);
        let eligible: Vec<FragmentId> = fragments
            .all()
            .iter()
            .filter(|fragment| fragment.settled && !protected.contains(&fragment.id))
            .map(|fragment| fragment.id)
            .collect();
        let hidden: Vec<FragmentId> = eligible.iter().copied().step_by(2).collect();
        let facts = NoticeFacts {
            estimate_input_tokens: Some(10_000),
            estimator: "bench".into(),
            input_allowance: 60_000,
            budget_hint: false,
            recovery_available: true,
        };
        group.throughput(Throughput::Elements(events(turns)));
        group.bench_with_input(BenchmarkId::from_parameter(turns), &session, |b, session| {
            b.iter(|| {
                let selection = Selection {
                    revision: 1,
                    hidden: &hidden,
                    notes: &[],
                    notes_goal_revision: None,
                };
                derive_effective_context(
                    black_box(session),
                    &fragments,
                    &protected,
                    selection,
                    None,
                    &facts,
                )
            });
        });
    }
    group.finish();
}

nanus_bench::benches!(derive, fold, frontier, compile);
