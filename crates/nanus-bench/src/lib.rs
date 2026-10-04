//! # nanus-bench
//!
//! Criterion benchmarks for the harness's hot paths, measured three ways: wall time, heap
//! allocations, and bytes allocated. The benchmarks live in `benches/`; this library is the
//! part they share — the instrumented allocator, the two measurements that read it, and the
//! fixtures that make one benchmark's "a session of a hundred turns" the same session as
//! another's.
//!
//! ## Why allocations are measured beside time
//!
//! Wall time on a laptop moves with the thermal state, the other processes, and the power
//! source; an allocation count does not. A change that adds a clone to a per-token path shows
//! up in the count on any machine, run after run, long before it is visible through the noise
//! in a timing. Bytes allocated is the same argument for memory pressure: it is what a long
//! session or a fast stream costs the allocator, which is the resource an agent harness runs
//! out of first.
//!
//! ## How the measurement works
//!
//! The library installs [`stats_alloc`]'s instrumented system allocator as the global
//! allocator of every benchmark binary that links it, which is all of them. The
//! measurements read its counters before and after a batch of iterations, and criterion
//! divides by the iteration count. The counters are process-wide atomics, so two
//! consequences follow:
//!
//! - **Timings include the counting.** Every allocation pays a few uncontended atomic
//!   increments. That is a constant cost across a baseline and a comparison, so a regression
//!   is still a regression; it does mean an absolute nanosecond figure here is slightly
//!   pessimistic against an uninstrumented build.
//! - **Only the benchmark may allocate during a measurement.** A routine that spawns a thread
//!   or drives a multi-threaded runtime would have other threads' allocations counted
//!   against it. Benchmarks use a current-thread runtime for that reason.
//!
//! ## Running
//!
//! ```sh
//! cargo bench -p nanus-bench                              # everything, every measurement
//! cargo bench -p nanus-bench --bench session -- 'allocs/'  # one file, one measurement
//! cargo bench -p nanus-bench -- --save-baseline main       # record a named baseline
//! cargo bench -p nanus-bench -- --baseline main            # compare against it
//! ```

#![forbid(unsafe_code)]

pub mod fixtures;

use core::fmt::Write as _;
use std::alloc::System;

pub use criterion;
use criterion::Throughput;
use criterion::measurement::{Measurement, ValueFormatter, WallTime};
use stats_alloc::{INSTRUMENTED_SYSTEM, Stats, StatsAlloc};

/// The instrumented allocator every benchmark binary runs on.
///
/// It is declared here rather than in each benchmark so that the measurements below are
/// guaranteed to read the allocator that is actually installed: a benchmark cannot forget it,
/// and cannot install a different one, because a binary has exactly one.
#[global_allocator]
static GLOBAL: &StatsAlloc<System> = &INSTRUMENTED_SYSTEM;

/// A measurement with a name, so each one's results land in a directory of their own.
///
/// Criterion keys stored results by benchmark id alone. Without a prefix, the allocation run
/// of `session/encode` would overwrite the timing run of the same id, and a saved baseline
/// would hold whichever ran last.
pub trait Metric: Measurement {
    /// The id prefix: `time`, `allocs`, or `bytes`.
    const NAME: &'static str;

    /// The id of a benchmark group under this measurement.
    fn group(name: &str) -> String {
        let mut id = String::with_capacity(Self::NAME.len().saturating_add(name.len()));
        id.push_str(Self::NAME);
        id.push('/');
        id.push_str(name);
        id
    }
}

impl Metric for WallTime {
    const NAME: &'static str = "time";
}

/// Heap allocations per iteration: calls to `alloc` plus calls to `realloc`.
///
/// A `realloc` is counted because it is a trip to the allocator like any other — a `Vec` that
/// grows by doubling makes one `alloc` and then a `realloc` per doubling, and both are what a
/// pre-sized buffer would save.
#[derive(Clone, Copy, Debug, Default)]
pub struct Allocations;

/// Bytes requested from the allocator per iteration, counting a `realloc`'s growth.
///
/// This is traffic, not residency: a buffer allocated and freed inside the routine counts in
/// full. It is the figure that tracks allocator pressure, which is what a per-token or
/// per-event path costs a long-running agent.
#[derive(Clone, Copy, Debug, Default)]
pub struct AllocatedBytes;

impl Metric for Allocations {
    const NAME: &'static str = "allocs";
}

impl Metric for AllocatedBytes {
    const NAME: &'static str = "bytes";
}

fn snapshot() -> Stats {
    GLOBAL.stats()
}

/// How many parts of a unit a reading is kept in: readings are thousandths of an allocation,
/// or of a byte, so the floor below can be smaller than anything the reports display.
const PARTS: u64 = 1000;

/// One thousandth of a unit added to every batch's reading, so a routine that allocates
/// nothing still records.
///
/// Criterion refuses a sample whose value is zero — it was written for time, where zero means
/// the measurement failed — and stores no result at all for that benchmark. For a count, zero
/// is the best possible answer and the one most worth keeping in a baseline, since the change
/// that matters is a zero becoming a one.
///
/// The floor is paid once per *batch*, and how many batches a sample holds depends on the
/// routine: `iter` runs a whole sample as one batch, while `iter_batched` with a large input
/// may run one iteration per batch. A whole unit per batch would therefore read as up to one
/// phantom allocation per iteration — which it did, in the first baseline — so the floor is a
/// thousandth instead: at most `0.001` per iteration, below the two decimals any report shows.
const BATCH_FLOOR: u64 = 1;

/// A batch's count as the reading criterion sums: in thousandths, with the floor added.
const fn reading(count: u64) -> u64 {
    count.saturating_mul(PARTS).saturating_add(BATCH_FLOOR)
}

/// A counter's movement between two snapshots, as the `u64` criterion's values are summed in.
fn moved(before: usize, after: usize) -> u64 {
    // The counters only grow; a decrease would mean the snapshots were taken out of order.
    assert!(after >= before, "allocator counters are monotonic");
    u64::try_from(after.saturating_sub(before)).unwrap_or(u64::MAX)
}

impl Measurement for Allocations {
    type Intermediate = Stats;
    type Value = u64;

    fn start(&self) -> Stats {
        snapshot()
    }

    fn end(&self, before: Stats) -> u64 {
        let after = snapshot();
        let count = moved(before.allocations, after.allocations)
            .saturating_add(moved(before.reallocations, after.reallocations));
        reading(count)
    }

    fn add(&self, first: &u64, second: &u64) -> u64 {
        first.saturating_add(*second)
    }

    fn zero(&self) -> u64 {
        0
    }

    fn to_f64(&self, value: &u64) -> f64 {
        to_f64(*value) / to_f64(PARTS)
    }

    fn formatter(&self) -> &dyn ValueFormatter {
        &Units::COUNT
    }
}

impl Measurement for AllocatedBytes {
    type Intermediate = Stats;
    type Value = u64;

    fn start(&self) -> Stats {
        snapshot()
    }

    fn end(&self, before: Stats) -> u64 {
        reading(moved(before.bytes_allocated, snapshot().bytes_allocated))
    }

    fn add(&self, first: &u64, second: &u64) -> u64 {
        first.saturating_add(*second)
    }

    fn zero(&self) -> u64 {
        0
    }

    fn to_f64(&self, value: &u64) -> f64 {
        to_f64(*value) / to_f64(PARTS)
    }

    fn formatter(&self) -> &dyn ValueFormatter {
        &Units::BYTES
    }
}

/// A count as criterion's statistics want it.
#[allow(
    clippy::cast_precision_loss,
    reason = "a count above 2^53 per sample is not a benchmark this crate runs"
)]
const fn to_f64(value: u64) -> f64 {
    value as f64
}

/// How a quantity is scaled for display: a ladder of units, each `step` times the last.
struct Units {
    /// The unit names, smallest first.
    names: [&'static str; 4],
    /// The ratio between adjacent units.
    step: f64,
    /// The name of the per-input ratio a throughput is reported as.
    per_byte: &'static str,
    /// The same, for a throughput counted in elements.
    per_element: &'static str,
}

impl Units {
    const COUNT: Self = Self {
        names: ["", "K", "M", "G"],
        step: 1000.0,
        per_byte: "allocs/B",
        per_element: "allocs/elem",
    };

    const BYTES: Self = Self {
        names: ["B", "KiB", "MiB", "GiB"],
        step: 1024.0,
        per_byte: "B/B",
        per_element: "B/elem",
    };

    /// The rung of the ladder `typical` reads best on, and the divisor that reaches it.
    fn rung(&self, typical: f64) -> (usize, f64) {
        let mut rung = 0;
        let mut divisor = 1.0;
        while rung < self.names.len().saturating_sub(1) && typical.abs() >= divisor * self.step {
            divisor *= self.step;
            rung = rung.saturating_add(1);
        }
        (rung, divisor)
    }
}

impl ValueFormatter for Units {
    /// The same scaling the plots use, so a figure on the terminal and on a chart agree.
    fn format_value(&self, value: f64) -> String {
        let mut values = [value];
        let unit = self.scale_values(value, &mut values);
        let mut out = String::new();
        // Writing to a `String` cannot fail.
        let _ = write!(out, "{:>8.2} {unit}", values[0]);
        out
    }

    fn scale_values(&self, typical: f64, values: &mut [f64]) -> &'static str {
        let (rung, divisor) = self.rung(typical);
        for value in values.iter_mut() {
            *value /= divisor;
        }
        match self.names[rung] {
            "" => "allocs",
            "K" => "K allocs",
            "M" => "M allocs",
            "G" => "G allocs",
            unit => unit,
        }
    }

    /// A throughput here is a ratio to the input — allocations per input byte, say — rather
    /// than a rate, because a count has no time in it to divide by.
    fn scale_throughputs(
        &self,
        _typical: f64,
        throughput: &Throughput,
        values: &mut [f64],
    ) -> &'static str {
        let (size, unit) = match *throughput {
            Throughput::Bytes(bytes) | Throughput::BytesDecimal(bytes) => (bytes, self.per_byte),
            Throughput::Elements(elements) | Throughput::Bits(elements) => {
                (elements, self.per_element)
            }
            Throughput::ElementsAndBytes { elements, .. } => (elements, self.per_element),
        };
        let size = to_f64(size.max(1));
        for value in values.iter_mut() {
            *value /= size;
        }
        unit
    }

    fn scale_for_machines(&self, _values: &mut [f64]) -> &'static str {
        if self.names[0].is_empty() {
            "allocs"
        } else {
            "B"
        }
    }
}

/// Expands to a benchmark binary that runs every target under all three measurements.
///
/// Each target is a function generic over the [`Metric`], so one body describes a benchmark
/// and the macro decides what it is measured by. Allocation counts are deterministic for the
/// routines here, so their runs use the minimum sample count and short windows: repeating an
/// exact count a hundred times would only make the suite slower.
///
/// ```ignore
/// fn encode<M: nanus_bench::Metric>(c: &mut criterion::Criterion<M>) { /* ... */ }
/// nanus_bench::benches!(encode);
/// ```
#[macro_export]
macro_rules! benches {
    ($($target:ident),+ $(,)?) => {
        // A module of its own, so a benchmark file is free to name its own functions `time`
        // or `bytes` without colliding with the three runners.
        mod __measured {
            pub(super) fn time() {
                let mut criterion =
                    $crate::criterion::Criterion::default().configure_from_args();
                $( super::$target::<$crate::criterion::measurement::WallTime>(&mut criterion); )+
            }

            pub(super) fn allocs() {
                let mut criterion = $crate::counting($crate::Allocations);
                $( super::$target::<$crate::Allocations>(&mut criterion); )+
            }

            pub(super) fn bytes() {
                let mut criterion = $crate::counting($crate::AllocatedBytes);
                $( super::$target::<$crate::AllocatedBytes>(&mut criterion); )+
            }
        }

        $crate::criterion::criterion_main!(__measured::time, __measured::allocs, __measured::bytes);
    };
}

/// The configuration of a counting run: few samples and short windows, since every sample
/// of a deterministic count is the same number.
#[must_use]
pub fn counting<M: Measurement>(measurement: M) -> criterion::Criterion<M> {
    criterion::Criterion::default()
        .with_measurement(measurement)
        .sample_size(10)
        .warm_up_time(core::time::Duration::from_millis(100))
        .measurement_time(core::time::Duration::from_millis(500))
        // A count that moves at all has moved for a reason, so the noise band that hides
        // timing jitter would only hide real changes here.
        .noise_threshold(0.0)
        .configure_from_args()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::hint::black_box;

    /// Runs `routine` between a measurement's `start` and `end`.
    fn measure<M: Measurement<Value = u64>>(measurement: &M, routine: impl Fn()) -> u64 {
        let before = measurement.start();
        routine();
        let reading = measurement.end(before);
        // Every reading carries the batch floor; take it off to see what the routine did.
        assert!(
            reading % PARTS == BATCH_FLOOR,
            "every reading is whole units plus the floor"
        );
        reading
            .saturating_sub(BATCH_FLOOR)
            .checked_div(PARTS)
            .unwrap_or(0)
    }

    /// The smallest of several readings, so a test harness thread that allocates while this
    /// one is measuring cannot make a routine that allocates nothing look as if it did.
    fn quietest<M: Measurement<Value = u64>>(measurement: &M, routine: impl Fn()) -> u64 {
        (0..8)
            .map(|_| measure(measurement, &routine))
            .min()
            .unwrap_or(u64::MAX)
    }

    #[test]
    fn every_allocation_a_routine_makes_is_counted() {
        let count = measure(&Allocations, || {
            for _ in 0..100 {
                black_box(Vec::<u8>::with_capacity(64));
            }
        });
        assert!(
            count >= 100,
            "a hundred allocations were made, {count} were counted"
        );
    }

    #[test]
    fn a_routine_that_allocates_nothing_counts_nothing() {
        let count = quietest(&Allocations, || {
            black_box(black_box(2_u64).saturating_mul(3));
        });
        assert_eq!(count, 0);
        let bytes = quietest(&AllocatedBytes, || {
            black_box(black_box(2_u64).saturating_mul(3));
        });
        assert_eq!(bytes, 0);
    }

    #[test]
    fn a_reallocation_counts_as_a_trip_and_its_growth_as_bytes() {
        let count = quietest(&Allocations, || {
            let mut buffer = Vec::<u8>::with_capacity(16);
            buffer.reserve_exact(4096);
            black_box(buffer);
        });
        assert_eq!(count, 2, "one alloc and one realloc");
        let bytes = quietest(&AllocatedBytes, || {
            let mut buffer = Vec::<u8>::with_capacity(16);
            buffer.reserve_exact(4096);
            black_box(buffer);
        });
        assert!(bytes >= 4096, "the growth is counted: {bytes}");
    }

    #[test]
    fn a_value_is_shown_on_the_rung_it_reads_best_on() {
        assert_eq!(Units::BYTES.format_value(1536.0).trim(), "1.50 KiB");
        assert_eq!(Units::BYTES.format_value(512.0).trim(), "512.00 B");
        assert_eq!(Units::COUNT.format_value(42.0).trim(), "42.00 allocs");
        assert_eq!(Units::COUNT.format_value(8170.0).trim(), "8.17 K allocs");
        let mut values = [2_000_000.0];
        assert_eq!(
            Units::COUNT.scale_values(2_000_000.0, &mut values),
            "M allocs"
        );
        assert!((values[0] - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn a_throughput_is_a_ratio_to_the_input_not_a_rate() {
        let mut values = [1024.0];
        let unit = Units::BYTES.scale_throughputs(1024.0, &Throughput::Bytes(512), &mut values);
        assert_eq!(unit, "B/B");
        assert!((values[0] - 2.0).abs() < f64::EPSILON);
        // An empty input divides by one rather than producing an infinity criterion rejects.
        let mut values = [7.0];
        let _ = Units::COUNT.scale_throughputs(7.0, &Throughput::Elements(0), &mut values);
        assert!(values[0].is_finite());
    }

    #[test]
    fn each_measurement_files_its_results_under_its_own_prefix() {
        assert_eq!(WallTime::group("session/encode"), "time/session/encode");
        assert_eq!(
            Allocations::group("session/encode"),
            "allocs/session/encode"
        );
        assert_eq!(
            AllocatedBytes::group("session/encode"),
            "bytes/session/encode"
        );
    }
}
