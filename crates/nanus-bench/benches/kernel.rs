//! The Cordis kernel: composing a context, finding a service in it, and dispatching an event.
//!
//! Composition is paid at every start — `nanus run`, every agent a service starts, every
//! interface session — and a withdrawal is paid whenever a provider is switched. Lookup and
//! dispatch are the per-call costs everything mounted on the kernel pays, so they are the
//! ones that would turn a cheap composition model into an expensive one.
//!
//! None of this runs inside a tokio runtime: the kernel drives plugin hooks with its own
//! `block_on`, which panics inside one (see the staging gotcha in `AGENTS.md`).

use std::hint::black_box;
use std::sync::OnceLock;

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput};
use nanus_bench::Metric;
use nanus_kernel::{
    AnyServiceKey, Context, EventKey, Hook, Kernel, PluginId, Provider, ServiceKey,
};

/// Chain lengths: a single provider, a realistic composition, and a deep one.
const CHAIN: [usize; 3] = [1, 8, 64];

/// Listener counts for a dispatch.
const LISTENERS: [usize; 3] = [1, 16, 128];

/// The longest chain any benchmark builds, which is how many names are minted.
const NAMES_MAX: usize = 64;

/// The payload of the benchmark's event: small, like the step and tool events the loop emits.
#[derive(Clone, Copy, Debug)]
struct Tick {
    step: u64,
}

const TICK: EventKey<Tick> = EventKey::of("bench.tick");

/// The `&'static` names the kernel's identifiers require, minted once per process.
///
/// Plugin ids and service keys hold `&'static str` because in production they are literals.
/// A chain of 64 needs 64 of each, so they are leaked here, once: a few kilobytes for the life
/// of a benchmark binary, rather than a per-iteration cost that would be measured.
fn names() -> &'static [(&'static str, &'static str)] {
    static NAMES: OnceLock<Vec<(&'static str, &'static str)>> = OnceLock::new();
    NAMES.get_or_init(|| {
        (0..NAMES_MAX)
            .map(|index| {
                let plugin: &'static str = Box::leak(format!("bench-plugin-{index}").into());
                let service: &'static str = Box::leak(format!("bench.service-{index}").into());
                (plugin, service)
            })
            .collect()
    })
}

fn plugin_id(index: usize) -> PluginId {
    PluginId::new(names()[index].0).unwrap_or_else(|error| unreachable!("plugin id: {error}"))
}

fn service_key(index: usize) -> ServiceKey<u64> {
    ServiceKey::of(names()[index].1)
}

/// A kernel staging a chain of `length` providers, each requiring the one before it.
///
/// They are staged leaf-first, so no plugin's requirement exists when it is mounted and every
/// activation after the root's is a reaction to the one before: the cascade is the whole
/// cost, not something declaration order quietly avoided.
fn chain(length: usize) -> Kernel {
    assert!(
        (1..=NAMES_MAX).contains(&length),
        "a chain has a root and fits the names"
    );
    let mut kernel = Kernel::new();
    for index in (0..length).rev() {
        let mut provider = Provider::new(
            plugin_id(index),
            "a benchmark provider",
            service_key(index),
            u64::try_from(index).unwrap_or(u64::MAX),
        );
        if let Some(previous) = index.checked_sub(1) {
            provider = provider.requiring(vec![AnyServiceKey::from(service_key(previous))]);
        }
        kernel = kernel.with_plugin(plugin_id(index), provider);
    }
    kernel
}

fn started(length: usize) -> Context {
    let context = chain(length)
        .start()
        .unwrap_or_else(|error| unreachable!("the chain starts: {error}"));
    // Precondition of every benchmark built on it: the far end of the chain is reachable.
    assert!(
        context.has(service_key(length.saturating_sub(1))),
        "the chain activated"
    );
    context
}

/// Building and starting a context whose plugins activate in a cascade.
fn start<M: Metric>(c: &mut Criterion<M>) {
    let mut group = c.benchmark_group(M::group("kernel/start_chain"));
    for length in CHAIN {
        group.throughput(Throughput::Elements(length as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(length),
            &length,
            |b, &length| {
                b.iter(|| started(black_box(length)));
            },
        );
    }
    group.finish();
}

/// Unloading the root of a started chain, which retires and sweeps every dependent in
/// reverse before the bindings are withdrawn.
fn unload_root<M: Metric>(c: &mut Criterion<M>) {
    let mut group = c.benchmark_group(M::group("kernel/unload_root"));
    for length in CHAIN {
        group.throughput(Throughput::Elements(length as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(length),
            &length,
            |b, &length| {
                b.iter_batched(
                    || started(length),
                    |context| {
                        context
                            .unload(plugin_id(0))
                            .unwrap_or_else(|error| unreachable!("the root unloads: {error}"));
                        // Returned so the context's own teardown is not part of the measurement.
                        context
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }
    group.finish();
}

/// Resolving the most recently activated service of a composition of `count` services.
///
/// The deepest service is the one asked for, so a registry that scanned in registration order
/// would show its worst case here.
fn get<M: Metric>(c: &mut Criterion<M>) {
    let mut group = c.benchmark_group(M::group("kernel/get"));
    for count in CHAIN {
        let context = started(count);
        let key = service_key(count.saturating_sub(1));
        group.bench_with_input(
            BenchmarkId::from_parameter(count),
            &context,
            |b, context| {
                b.iter(|| black_box(context).get(black_box(key)));
            },
        );
    }
    group.finish();
}

/// A context with `count` observers of [`TICK`], registered by one hook plugin.
fn listening(count: usize) -> Context {
    let hook = Hook::new(plugin_id(0), "benchmark listeners", move |cx| {
        for _ in 0..count {
            cx.on(TICK, |_event, tick| {
                black_box(tick.step);
            });
        }
        Ok(())
    });
    let context = Kernel::new()
        .with_plugin(plugin_id(0), hook)
        .start()
        .unwrap_or_else(|error| unreachable!("the listeners mount: {error}"));
    assert_eq!(context.stats().active, 1, "the hook is active");
    context
}

/// Emitting one event to every listener: the cost of an observation point in the loop.
fn emit<M: Metric>(c: &mut Criterion<M>) {
    let mut group = c.benchmark_group(M::group("kernel/emit"));
    for count in LISTENERS {
        let context = listening(count);
        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(count),
            &context,
            |b, context| {
                b.iter(|| context.emit(TICK, black_box(&Tick { step: 7 })));
            },
        );
    }
    group.finish();
}

nanus_bench::benches!(start, unload_root, get, emit);
