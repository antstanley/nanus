//! The session store: saving, loading, and listing sessions on a real filesystem.
//!
//! A `Done` frame means the session is already on disk, so a save sits between the model's
//! last token and the answer every turn, and grows with the session. A load is what resuming
//! or attaching costs, and a listing is what opening the session picker costs.
//!
//! Every store here lives in a temporary directory passed to `JsonlStore::new` explicitly;
//! nothing reads `NANUS_HOME` or touches the real store.
//!
//! **The allocation figures here are approximate.** The store uses `tokio::fs`, which runs
//! file operations on tokio's blocking thread pool, and the allocator's counters are
//! process-wide: the pool thread's allocations are counted, which is correct, but so would be
//! anything else that thread did during the measurement. The timings include the hand-off to
//! that thread and the disk itself, so they are noisier than the in-memory benchmarks.

use std::hint::black_box;
use std::path::Path;
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, Throughput};
use nanus_adapter_store::JsonlStore;
use nanus_bench::{Metric, fixtures};
use nanus_domain::{Session, SessionId};
use nanus_ports::StorePort;
use tempfile::TempDir;
use tokio::runtime::Runtime;

/// Session lengths, in turns, matching the session benchmarks.
const TURNS: [u32; 3] = [10, 100, 500];

/// How many sessions the listed store holds.
const LISTED: u32 = 50;

/// The turns in each listed session: enough for a real headline, small enough to set up fast.
const LISTED_TURNS: u32 = 4;

/// A current-thread runtime, so the only other thread in play is tokio's blocking pool.
fn runtime() -> Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|error| unreachable!("a current-thread runtime builds: {error}"))
}

/// A store in a fresh temporary directory; the directory is removed when the guard drops.
fn store(runtime: &Runtime) -> (TempDir, JsonlStore) {
    let home =
        TempDir::new().unwrap_or_else(|error| unreachable!("a temporary directory: {error}"));
    let store = runtime
        .block_on(JsonlStore::new(home.path()))
        .unwrap_or_else(|error| unreachable!("the store opens in a tempdir: {error}"));
    assert!(
        store.home_path().starts_with(home.path()),
        "the store is the tempdir"
    );
    (home, store)
}

/// The fixture session under another id, so a listing has distinct sessions to read.
fn renamed(template: &Session, id: String) -> Session {
    let mut session = Session::new(SessionId::new(id), template.created_at_ms(), template.cwd());
    for event in template.log().events() {
        session.append(event.clone());
    }
    assert_eq!(session.event_count(), template.event_count());
    session
}

/// Sampling for a session size: the large sessions take milliseconds of disk per iteration,
/// so they get fewer samples and a longer window rather than a whole minute each.
///
/// Only the timing run is tuned; the counting runs keep their own short configuration.
fn sampling<M: Metric>(group: &mut criterion::BenchmarkGroup<'_, M>, turns: u32) {
    if M::NAME != "time" {
        return;
    }
    if turns >= 100 {
        group.sample_size(20);
        group.measurement_time(Duration::from_secs(3));
    } else {
        group.sample_size(50);
        group.measurement_time(Duration::from_secs(2));
    }
}

/// Saving a session over its previous copy: the atomic write every turn ends with.
///
/// The id is the same each iteration, so the store's directory holds one session however
/// long the benchmark runs.
fn save<M: Metric>(c: &mut Criterion<M>) {
    let runtime = runtime();
    let mut group = c.benchmark_group(M::group("store/save"));
    for turns in TURNS {
        let (_home, store) = store(&runtime);
        let session = fixtures::session(turns);
        sampling(&mut group, turns);
        group.throughput(Throughput::Bytes(session.to_jsonl().len() as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(turns),
            &session,
            |b, session| {
                b.iter(|| runtime.block_on(store.save(black_box(session))));
            },
        );
    }
    group.finish();
}

/// Loading a saved session: reading, decoding, and validating the whole file.
fn load<M: Metric>(c: &mut Criterion<M>) {
    let runtime = runtime();
    let mut group = c.benchmark_group(M::group("store/load"));
    for turns in TURNS {
        let (_home, store) = store(&runtime);
        let session = fixtures::session(turns);
        runtime
            .block_on(store.save(&session))
            .unwrap_or_else(|error| unreachable!("the fixture saves: {error}"));
        sampling(&mut group, turns);
        group.throughput(Throughput::Bytes(session.to_jsonl().len() as u64));
        group.bench_with_input(BenchmarkId::from_parameter(turns), session.id(), |b, id| {
            b.iter(|| runtime.block_on(store.load(black_box(id))));
        });
    }
    group.finish();
}

/// Fills `store` with [`LISTED`] small sessions under distinct ids.
fn fill(runtime: &Runtime, store: &JsonlStore, home: &Path) {
    let template = fixtures::session(LISTED_TURNS);
    for index in 0..LISTED {
        let session = renamed(&template, format!("bench-listed-{index:03}"));
        runtime
            .block_on(store.save(&session))
            .unwrap_or_else(|error| unreachable!("a listed session saves: {error}"));
    }
    assert!(
        home.join("sessions").is_dir(),
        "the sessions landed in the tempdir"
    );
}

/// Listing a store of [`LISTED`] sessions: what the session picker and `nanus sessions` pay.
fn list<M: Metric>(c: &mut Criterion<M>) {
    let runtime = runtime();
    let (home, store) = store(&runtime);
    fill(&runtime, &store, home.path());
    let listed = runtime
        .block_on(store.list())
        .unwrap_or_else(|error| unreachable!("the store lists: {error}"));
    assert_eq!(
        listed.len(),
        LISTED as usize,
        "every saved session is listed"
    );
    let mut group = c.benchmark_group(M::group("store/list"));
    sampling(&mut group, LISTED);
    group.throughput(Throughput::Elements(u64::from(LISTED)));
    group.bench_function(BenchmarkId::from_parameter(LISTED), |b| {
        b.iter(|| runtime.block_on(store.list()));
    });
    group.finish();
}

nanus_bench::benches!(save, load, list);
