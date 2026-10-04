# Benchmarking

`nanus` measures its hot paths with [criterion](https://docs.rs/criterion/latest/criterion/)
benchmarks in `crates/nanus-bench`: the session log, the provider wire, the link, the
interface's view, the kernel, the session store, the tools, and the agent loop. Every
benchmark is measured three ways — wall time, heap allocations, and bytes allocated — and
this page records how, what the current baseline is, and what it shows.

Benchmarks are not a quality gate: nothing fails because a number moved. They exist so that
a change to a hot path is *measured* against a baseline rather than argued about, and so that
a claim like "this is faster" or "this allocates less" comes with the two numbers it rests on.

## At a glance

From the [current baseline](#current-baseline), on an Apple M2:

| Path | Time | Allocations | Bytes |
|---|---:|---:|---:|
| A whole turn through the agent loop, scripted model, fresh session | 37.8 µs | 974 | 146 KiB |
| The same turn on 100 turns of history | 784 µs | 12,809 | 3.05 MiB |
| A DeepSeek request body for 100 turns of history | 536 µs | 7,131 | 1.85 MiB |
| A ~460 KB streamed response through the DeepSeek adapter | 20.9 ms | 52,793 | 6.97 MiB |
| Saving a 500-turn session | 18.1 ms | 23,549 | 9.58 MiB |
| A grep over 500 files with few hits | 13.0 ms | 85,514 | 5.27 MiB |
| One interface frame of a 100-turn conversation | 7.4 ms | 134,941 | 8.05 MiB |
| One streamed token in the interface at 100 turns (append, follow, redraw) | 12.6 ms | 202,908 | 12.10 MiB |

The harness's own overhead is microseconds against a model's seconds. The interface's
redraw is the first thing that will not keep up with a fast stream on a long session; the
[findings](#what-the-baseline-shows) say why.

## Methodology

### Three measurements

| Measurement | Id prefix | What it counts |
|---|---|---|
| Wall time | `time/` | Criterion's default: elapsed time per iteration. |
| Allocations | `allocs/` | Calls to `alloc` plus calls to `realloc`, per iteration. |
| Bytes allocated | `bytes/` | Bytes requested from the allocator per iteration, counting a `realloc`'s growth. |

Wall time on a laptop moves with the thermal state, the other processes, and the power
source; an allocation count does not. A change that adds a clone to a per-token path shows
up in the count on any machine, run after run, long before it is visible through the noise
in a timing. Bytes allocated is the same argument for memory pressure: it is what a long
session or a fast stream costs the allocator, which is the resource a long-running agent
runs out of first. It is *traffic*, not residency — a buffer allocated and freed inside the
routine counts in full — because traffic is what tracks allocator pressure.

A `realloc` counts as an allocation because it is a trip to the allocator like any other: a
`Vec` that grows by doubling makes one `alloc` and then a `realloc` per doubling, and both
are what pre-sizing it would save.

Each benchmark is written once, generic over the measurement, and the `benches!` macro in
`nanus-bench` runs it under all three. The measurement's name is the first segment of the
benchmark id, so the three runs of one benchmark are stored separately and a saved baseline
holds all of them.

### The instruments

**Time** is criterion's `WallTime`, a monotonic clock read around each batch of iterations.

**Allocations and bytes** come from [`stats_alloc`](https://docs.rs/stats_alloc)'s
instrumented system allocator, which `nanus-bench` installs as the global allocator of every
benchmark binary. Two custom criterion measurements read its counters before and after a
batch. Its `GlobalAlloc` implementation is that crate's, so installing it needs no `unsafe`
here and the workspace keeps `forbid(unsafe_code)`. Because the allocator is global, the
crate is a leaf that nothing depends on: it must never be linked into a shipped binary.

The measurement code is itself tested in both directions: a routine's allocations are all
counted, a routine that allocates nothing counts nothing, a `realloc` counts as one trip
and its growth as bytes, and each measurement files its results under its own prefix.

### Workloads

The inputs are deterministic functions of a size argument (`nanus_bench::fixtures`), so a
baseline recorded today and a comparison next month measure the same bytes. They imitate
what a coding agent produces rather than what is convenient to generate: a fixture **turn**
is a prompt, a step with reasoning and two tool calls whose results are an 80-line source
file and a set of grep hits, then a markdown answer with headings, a list, inline code, a
fenced block, and a table — thirteen events.

Sizes are chosen to show **growth**, not just a point: most paths that depend on the
conversation are measured at 10, 100, and (where it is affordable) 500 turns, because a cost
paid once per step over the whole log is the kind that is invisible in a short session and
dominant in a long one.

| File | Benchmarks | Why |
|---|---|---|
| `session` | `build`, `derive_messages`, `encode/{to_jsonl,try_to_jsonl}`, `decode`, `append`, over 10, 100, and 500 turns | `derive_messages` runs before every model step and `try_to_jsonl` before every save, each over the *whole* log. |
| `wire` | `sse/*`: SSE framing of a ~460 KB response (500 reasoning deltas, 2,000 content deltas, a fragmented tool call), in 1,460-byte segments and one frame per chunk; `encode/{value,body}/*`: each adapter's request for 10 and 100 turns of history with a 4 KB system prompt and the twelve offered tool schemas; `stream/*`: a whole `stream_chat` decode through the real DeepSeek, OpenAI and Anthropic adapters against a loopback server | Framing and decoding run per token; encoding runs per step over the whole history. |
| `link` | `frame/*` and `request/*` round trips, `turn/*` (a turn's 2,506 frames, line by line), `backlog/*` (the same turn as one frame) | Every streamed token crosses the link once in each direction. |
| `tui` | `replay`, `stream` (2,000 six-byte deltas into a transcript), `draw/*` (one frame at 120x40 or 200x60, markdown on and off, compact and full detail, scrolled to the middle), `delta` (append, follow, redraw: the runtime's per-token cycle), `input` | What opening a session, receiving a token, and pressing a key cost the interface. |
| `kernel` | `start_chain` and `unload_root` over chains of 1, 8, and 64 dependent plugins, `get` (service lookup), `emit` to 1, 16, and 128 listeners | Composition and teardown, and the per-call cost of the kernel's indirection. |
| `store` | `save` and `load` of 10-, 100-, and 500-turn sessions, `list` of a store of 50 sessions, on a tempdir | Every turn ends in a save; every resume and every listing starts with a read. |
| `tools` | `read` of a 5,000-line file, `grep` and `glob` over a 500-file tree, `edit` and `write` of a 2,000-line file, `bash` running `true` and `echo`, through `ToolDefinition::execute` on a real tempdir | The calls a model makes most often. |
| `agent_loop` | `turn/fresh`, `turn/history/{10,100}`, `stream/{500,2000}`: whole turns through `AgentRunner` with a scripted model and in-memory tools, under the default approval policy | The harness's own overhead per turn and per streamed token, with no network and no disk. |

Each workload is checked before it is measured — a tool must return success, a stream must
end without an error and with the expected number of events, a turn must take the expected
steps with every tool call run rather than refused — so a benchmark cannot quietly measure a
refusal or a failure path.

### Statistics

Timing runs use criterion's defaults unless a group says otherwise:

| Setting | Default | Meaning |
|---|---|---|
| Warm-up | 3 s | Run the routine before measuring, to settle caches, the branch predictor, and the CPU's clock. |
| Samples | 100 | Each sample is one timed batch of iterations. |
| Measurement window | 5 s | The target total time for all samples. |
| Sampling | linear | Sample *i* runs *i·d* iterations, so time can be regressed on iteration count. Criterion switches to *flat* (equal batches) when linear would take more than twice the window. |
| Estimate | slope | The regression slope of time on iterations, which cancels a constant per-batch overhead; the mean of per-iteration times under flat sampling. |
| Confidence interval | 95% | Bootstrapped from 100,000 resamples. |
| Outliers | Tukey's fences | Classified as mild (1.5 IQR) or severe (3 IQR) and reported, not removed. |
| Comparison | p < 0.05 and > 1% | A change against a baseline is reported only when a bootstrap test finds it significant *and* its interval clears a 1% noise band. |

Groups whose iterations take milliseconds override the sample count, the window, or the
sampling mode, so that one benchmark does not take minutes. A group setting outranks the
command line, so `--sample-size` and `--measurement-time` do not shorten these:

| Groups | Override | Applies to |
|---|---|---|
| `tui/draw/*`, `tui/delta/*` | flat sampling, 20 samples | all three measurements |
| `wire/sse/*`, `wire/encode/*` | 50 samples, 1 s warm-up, 2 s window | time |
| `wire/stream/*` | 10 samples, 1 s warm-up, 3 s window | time |
| `store/*/10` | 50 samples, 2 s window | time |
| `store/*/100`, `store/*/500` | 20 samples, 3 s window | time |
| `tools/bash/*` | 20 samples | all three measurements |

**Counting runs** use 10 samples, a 100 ms warm-up, a 500 ms window, and a noise threshold of
zero. A count repeats exactly for the routines here, so a hundred samples of the same number
would only make the suite slower, and the noise band that hides timing jitter would only hide
a real change: a count that moves at all has moved for a reason.

The baseline tables carry a **±** column: half the width of the 95% interval for the time, as
a percentage of the estimate. It is the honest reading of a timing — two runs of the same code
can land that far apart — and a difference smaller than it is not a finding.

### Isolation, and its limits

- **Setup is not measured.** Inputs are built outside the timed region (`iter_batched` where a
  routine consumes or mutates its input, such as appending to a session or editing a file),
  and results pass through `black_box` so the optimiser cannot delete the work.
- **Counts are exact where only the benchmark thread runs.** The allocator's counters are
  process-wide. The kernel, session, link, interface, and agent-loop benchmarks run on one
  thread and repeat their counts exactly. The `store`, `tools`, and `wire/stream` benchmarks
  use a current-thread tokio runtime, but its filesystem and process work runs on tokio's
  blocking pool, so their counts include that pool's allocations and are approximate.
- **Timings include the counting.** Every allocation pays a few uncontended atomic increments
  in the instrumented allocator. That is constant between a baseline and a comparison, so a
  regression is still a regression, but absolute times are slightly pessimistic against an
  uninstrumented build.
- **Zero is recorded as `0.00`.** Criterion refuses a sample whose value is zero, since for
  time zero means the measurement failed, and it would store nothing at all. Every batch's
  count therefore carries a floor of one thousandth of a unit — at most `0.001` per iteration,
  invisible in two decimals — so "allocates nothing" is kept in the baseline, where a zero
  becoming a one is the change most worth seeing.
- **The I/O is real.** The store and tools touch a real tempdir and the shell spawns real
  processes, so their timings carry the filesystem's and the scheduler's variance; their
  intervals are the widest in the tables. The stream benchmarks use loopback TCP with
  `TCP_NODELAY`, which removes a delayed-acknowledgement stall that otherwise dominated them.
- **Nothing leaves the machine.** No benchmark reaches the network, a credential, or the
  secret store; the store and tools are confined to tempdirs.

### Environment

Benchmarks build with the `bench` profile, which inherits the workspace's release settings:
thin LTO and one codegen unit. A baseline is recorded on AC power with nothing else running,
from a clean `target/criterion/`, and its record states the machine, the operating system,
the toolchain, and the commit. A number from a different machine is not comparable to these;
re-record a baseline there before comparing.

## Procedure

### Comparing a change

```sh
git switch main
cargo bench -p nanus-bench -- --save-baseline before    # the parent revision
git switch my-change
cargo bench -p nanus-bench -- --baseline before         # the change, compared
```

Criterion prints a verdict per benchmark: *Performance has regressed*, *has improved*, or *No
change in performance detected*, with the change and its interval. For the `allocs/` and
`bytes/` runs any reported change is real. For `time/`, read the change against the ± of the
benchmark: a verdict inside it on one run is worth a second run before it is believed.

Narrow the run while iterating; the filter is a regular expression on the benchmark id:

```sh
cargo bench -p nanus-bench --bench session                 # one file
cargo bench -p nanus-bench -- '^allocs/'                   # one measurement
cargo bench -p nanus-bench --bench tui -- 'tui/draw'       # one group
```

Criterion labels every result `time:` in its terminal output, including the counts; the units
(`allocs`, `KiB`) are what say which measurement a line is.

### Recording a new baseline

Re-record when the toolchain moves, when benchmarks are added, or when a change is meant to
move the numbers — and in the same change, so the page and the code agree.

```sh
rm -rf target/criterion                                    # no stale results
cargo bench -p nanus-bench -- --save-baseline main         # about 20 minutes
scripts/bench-baseline.py main --by-area                   # the tables below
```

Then replace the tables under [Current baseline](#current-baseline), update its record line
(date, commit, machine, toolchain), the [at-a-glance](#at-a-glance) figures, and any
[finding](#what-the-baseline-shows) the new numbers change. Criterion keeps its results under
`target/`, which is not committed, so this page is the only place a baseline outlives the
machine it was recorded on.

### Adding a benchmark

Write a function generic over `nanus_bench::Metric`, name its group with `M::group("area/…")`,
and list it in the file's `nanus_bench::benches!(…)`; a new file also needs a `[[bench]]`
entry with `harness = false` in `crates/nanus-bench/Cargo.toml`. Build inputs from
`nanus_bench::fixtures`, check the workload succeeds before measuring it, say in the doc
comment *why* the path deserves a benchmark, and keep a full run of the file under about three
minutes. The workspace lints apply to benchmarks as to everything else.

## Current baseline

Recorded on 2026-10-04 at `ee1e595`, on an Apple M2 (8 cores, 8 GB) on AC power, macOS 27.0,
Rust 1.98.0, with nothing else running. Times are criterion's point estimate with the
half-width of its 95% interval; counts are per iteration.

### `agent_loop`

| Benchmark | Time | ± | Allocations | Bytes allocated |
|---|---:|---:|---:|---:|
| `stream/2000` | 75.91 µs | 2.9% | 2,145 | 246.14 KiB |
| `stream/500` | 23.45 µs | 3.6% | 643 | 76.87 KiB |
| `turn/fresh` | 37.82 µs | 4.2% | 974 | 146.29 KiB |
| `turn/history/10` | 91.55 µs | 4.2% | 2,171 | 442.47 KiB |
| `turn/history/100` | 783.59 µs | 9.4% | 12,809 | 3.05 MiB |

### `kernel`

| Benchmark | Time | ± | Allocations | Bytes allocated |
|---|---:|---:|---:|---:|
| `emit/1` | 32.41 ns | 3.3% | 1.00 | 8.00 B |
| `emit/128` | 449.87 ns | 1.9% | 1.00 | 1.00 KiB |
| `emit/16` | 79.16 ns | 2.4% | 1.00 | 128.00 B |
| `get/1` | 21.03 ns | 2.9% | 0.00 | 0.00 B |
| `get/64` | 17.87 ns | 2.6% | 0.00 | 0.00 B |
| `get/8` | 21.08 ns | 4.4% | 0.00 | 0.00 B |
| `start_chain/1` | 723.97 ns | 3.5% | 13 | 2.14 KiB |
| `start_chain/64` | 110.84 µs | 2.1% | 660 | 77.56 KiB |
| `start_chain/8` | 6.15 µs | 3.3% | 88 | 9.45 KiB |
| `unload_root/1` | 261.95 ns | 3.3% | 1.00 | 1.00 B |
| `unload_root/64` | 134.88 µs | 1.5% | 268 | 21.88 KiB |
| `unload_root/8` | 4.62 µs | 2.6% | 35 | 2.57 KiB |

### `link`

| Benchmark | Time | ± | Allocations | Bytes allocated |
|---|---:|---:|---:|---:|
| `backlog/decode` | 432.82 µs | 1.6% | 7,550 | 1.53 MiB |
| `backlog/encode` | 89.76 µs | 1.2% | 11 | 128.00 KiB |
| `frame/decode/done` | 1.20 µs | 1.7% | 11 | 4.71 KiB |
| `frame/decode/text` | 134.34 ns | 4.6% | 2.00 | 264.00 B |
| `frame/decode/tool` | 511.21 ns | 3.2% | 9.00 | 1.18 KiB |
| `frame/decode/tool_done` | 201.58 ns | 1.7% | 3.00 | 271.00 B |
| `frame/encode/done` | 1.07 µs | 2.7% | 5.00 | 2.96 KiB |
| `frame/encode/text` | 49.21 ns | 2.7% | 1.00 | 128.00 B |
| `frame/encode/tool` | 179.93 ns | 2.3% | 2.00 | 256.00 B |
| `frame/encode/tool_done` | 79.18 ns | 1.8% | 1.00 | 128.00 B |
| `request/decode/approve` | 160.29 ns | 2.3% | 2.00 | 267.00 B |
| `request/decode/prompt` | 374.63 ns | 1.2% | 2.00 | 2.21 KiB |
| `request/encode/approve` | 73.95 ns | 1.1% | 1.00 | 128.00 B |
| `request/encode/prompt` | 899.43 ns | 3.2% | 3.00 | 3.97 KiB |
| `turn/decode` | 345.98 µs | 1.4% | 5,041 | 1.39 MiB |
| `turn/encode` | 138.56 µs | 0.9% | 2,524 | 412.46 KiB |

### `session`

| Benchmark | Time | ± | Allocations | Bytes allocated |
|---|---:|---:|---:|---:|
| `append/10` | 41.45 ns | 3.5% | 0.00 | 0.00 B |
| `append/100` | 33.53 ns | 4.2% | 0.00 | 0.00 B |
| `append/500` | 41.13 ns | 8.4% | 0.00 | 0.00 B |
| `build/10` | 41.21 µs | 2.6% | 649 | 157.94 KiB |
| `build/100` | 382.00 µs | 1.9% | 6,412 | 1.43 MiB |
| `build/500` | 1.94 ms | 2.3% | 32,014 | 6.73 MiB |
| `decode/10` | 104.68 µs | 3.6% | 981 | 239.70 KiB |
| `decode/100` | 1.00 ms | 1.5% | 9,714 | 2.23 MiB |
| `decode/500` | 5.27 ms | 1.9% | 48,516 | 10.72 MiB |
| `derive_messages/10` | 9.19 µs | 2.2% | 259 | 65.73 KiB |
| `derive_messages/100` | 124.44 µs | 2.7% | 2,515 | 639.28 KiB |
| `derive_messages/500` | 1.37 ms | 4.2% | 12,520 | 3.33 MiB |
| `encode/to_jsonl/10` | 71.81 µs | 5.8% | 383 | 189.51 KiB |
| `encode/to_jsonl/100` | 654.49 µs | 3.4% | 3,716 | 1.66 MiB |
| `encode/to_jsonl/500` | 3.04 ms | 2.4% | 18,518 | 7.50 MiB |
| `encode/try_to_jsonl/10` | 101.74 µs | 3.3% | 486 | 191.19 KiB |
| `encode/try_to_jsonl/100` | 1.11 ms | 6.5% | 4,719 | 1.67 MiB |
| `encode/try_to_jsonl/500` | 5.08 ms | 2.5% | 23,521 | 7.58 MiB |

### `store`

| Benchmark | Time | ± | Allocations | Bytes allocated |
|---|---:|---:|---:|---:|
| `list/50` | 5.17 ms | 8.7% | 2,765 | 1.26 MiB |
| `load/10` | 296.25 µs | 10.0% | 1,028 | 340.05 KiB |
| `load/100` | 1.39 ms | 12.0% | 9,773 | 3.74 MiB |
| `load/500` | 6.16 ms | 5.8% | 48,581 | 16.73 MiB |
| `save/10` | 2.85 ms | 2.6% | 512 | 254.05 KiB |
| `save/100` | 4.58 ms | 5.1% | 4,745 | 2.26 MiB |
| `save/500` | 18.12 ms | 7.1% | 23,548 | 9.58 MiB |

### `tools`

| Benchmark | Time | ± | Allocations | Bytes allocated |
|---|---:|---:|---:|---:|
| `bash/echo` | 7.62 ms | 10.2% | 54 | 20.99 KiB |
| `bash/true` | 7.60 ms | 15.1% | 52 | 20.98 KiB |
| `modify/edit` | 286.64 µs | 5.1% | 79 | 229.59 KiB |
| `modify/write_overwrite` | 93.08 µs | 12.5% | 37 | 110.67 KiB |
| `read/default_window` | 274.66 µs | 3.5% | 70 | 344.86 KiB |
| `read/whole_file` | 303.12 µs | 1.4% | 73 | 578.87 KiB |
| `read/window_100` | 201.24 µs | 6.9% | 68 | 271.25 KiB |
| `search/glob/all_rs` | 1.70 ms | 2.4% | 1,613 | 187.62 KiB |
| `search/glob/narrow` | 1.79 ms | 2.8% | 1,777 | 183.81 KiB |
| `search/grep/common_capped` | 1.80 ms | 1.9% | 3,529 | 312.73 KiB |
| `search/grep/include_rs` | 13.47 ms | 2.2% | 85,683 | 5.29 MiB |
| `search/grep/rare` | 12.95 ms | 2.0% | 85,514 | 5.27 MiB |

### `tui`

| Benchmark | Time | ± | Allocations | Bytes allocated |
|---|---:|---:|---:|---:|
| `delta/10` | 3.58 ms | 8.0% | 23,542 | 1.33 MiB |
| `delta/100` | 12.59 ms | 6.2% | 202,781 | 12.08 MiB |
| `draw/compact/10` | 859.83 µs | 6.5% | 13,787 | 821.36 KiB |
| `draw/compact/100` | 7.36 ms | 7.6% | 134,941 | 8.05 MiB |
| `draw/full/10` | 817.70 µs | 2.5% | 13,696 | 923.89 KiB |
| `draw/full/100` | 6.73 ms | 3.0% | 133,948 | 8.47 MiB |
| `draw/large/100` | 6.90 ms | 4.7% | 135,099 | 8.41 MiB |
| `draw/plain/10` | 297.63 µs | 3.2% | 3,152 | 258.10 KiB |
| `draw/plain/100` | 1.74 ms | 4.1% | 28,546 | 2.45 MiB |
| `draw/scrolled/100` | 8.12 ms | 6.9% | 134,939 | 8.14 MiB |
| `input/insert_backspace/2000` | 143.04 ns | 5.4% | 0.00 | 0.00 B |
| `replay/10` | 29.36 µs | 1.8% | 510 | 72.91 KiB |
| `replay/100` | 381.04 µs | 2.2% | 5,043 | 698.04 KiB |
| `stream/answer` | 12.13 µs | 1.3% | 13 | 16.44 KiB |
| `stream/interleaved` | 12.45 µs | 1.4% | 23 | 16.44 KiB |
| `stream/reasoning` | 12.41 µs | 2.4% | 13 | 16.44 KiB |

### `wire`

| Benchmark | Time | ± | Allocations | Bytes allocated |
|---|---:|---:|---:|---:|
| `encode/body/anthropic/10` | 108.05 µs | 7.0% | 2,121 | 395.80 KiB |
| `encode/body/anthropic/100` | 758.01 µs | 3.1% | 14,727 | 2.83 MiB |
| `encode/body/deepseek/10` | 76.62 µs | 3.2% | 1,095 | 255.85 KiB |
| `encode/body/deepseek/100` | 535.59 µs | 3.7% | 7,131 | 1.85 MiB |
| `encode/body/openai_chat/10` | 69.21 µs | 3.2% | 1,052 | 246.27 KiB |
| `encode/body/openai_chat/100` | 532.21 µs | 3.9% | 6,728 | 1.76 MiB |
| `encode/body/openai_responses/10` | 65.98 µs | 2.8% | 1,020 | 229.62 KiB |
| `encode/body/openai_responses/100` | 482.51 µs | 3.9% | 6,607 | 1.65 MiB |
| `encode/value/anthropic/10` | 66.78 µs | 5.7% | 2,111 | 331.80 KiB |
| `encode/value/anthropic/100` | 547.50 µs | 5.0% | 14,714 | 2.33 MiB |
| `encode/value/deepseek/10` | 37.95 µs | 3.8% | 1,089 | 192.63 KiB |
| `encode/value/deepseek/100` | 246.35 µs | 2.7% | 7,122 | 1.36 MiB |
| `encode/value/openai_chat/10` | 34.62 µs | 2.3% | 1,046 | 182.88 KiB |
| `encode/value/openai_chat/100` | 248.52 µs | 3.9% | 6,719 | 1.27 MiB |
| `encode/value/openai_responses/10` | 35.35 µs | 2.6% | 1,010 | 165.62 KiB |
| `encode/value/openai_responses/100` | 235.66 µs | 3.6% | 6,594 | 1.15 MiB |
| `sse/legacy/frames` | 675.24 µs | 2.8% | 10,025 | 1.11 MiB |
| `sse/legacy/segments` | 711.74 µs | 2.0% | 8,167 | 966.49 KiB |
| `sse/limited/frames` | 367.75 µs | 3.1% | 5,013 | 676.86 KiB |
| `sse/limited/segments` | 327.87 µs | 1.4% | 3,155 | 502.67 KiB |
| `sse/sse_frames/frames` | 652.29 µs | 2.2% | 10,025 | 1.11 MiB |
| `sse/sse_frames/segments` | 752.88 µs | 3.2% | 8,167 | 966.49 KiB |
| `stream/anthropic` | 13.82 ms | 12.5% | 36,344 | 34.15 MiB |
| `stream/deepseek` | 20.94 ms | 11.7% | 52,791 | 7.03 MiB |
| `stream/openai_chat` | 19.94 ms | 13.8% | 52,790 | 7.13 MiB |

## What the baseline shows

These are observations from the numbers above, recorded so that a fix can be measured
against them. None has been changed.

- **The interface redraws the whole conversation on every frame.** A frame of a 100-turn
  transcript costs nine to ten times one of a 10-turn transcript in time, allocations, and
  bytes, and being scrolled to the middle makes no difference (`draw/scrolled/100` makes the
  same allocations as the bottom). The per-token cycle is worse than one frame, because
  `follow()` rebuilds every line to find the bottom and the redraw then rebuilds them again:
  `tui/delta/100` is about 12.6 ms, 203 K allocations, and 12 MiB per token, a ceiling of
  roughly 80 tokens a second on a long session.
- **Markdown is most of a frame.** At 100 turns, turning it off takes a frame from 7.4 ms and
  135 K allocations to 1.7 ms and 29 K. Answers are re-rendered on every frame; nothing caches
  them.
- **Anthropic streaming copies its accumulated reply on every delta.** `absorb_replay_delta`
  (`crates/nanus-adapter-anthropic/src/wire.rs`) clones the block's text so far, appends the
  fragment, and stores it back, so the cost is quadratic in the reply's length:
  `wire/stream/anthropic` allocates 34 MiB for a ~240 KB body, against 7 MiB for DeepSeek
  decoding a ~460 KB one.
- **The unlimited SSE framer allocates a `Vec` per line.** `SseFrames::push` (`legacy`)
  makes 2.6 times the allocations of the limited `ResponseFrames` reader on the same body.
- **A grep that finds little is the expensive grep.** `grep/rare` and `grep/include_rs`
  allocate about 85 K times (about 171 per file) and run about seven times longer than
  `grep/common_capped`, which stops at its cap.
- **`glob` walks the whole tree whatever the pattern.** `glob/narrow`, which names one
  directory, costs the same as `glob/all_rs`.
- **Listing reads every session file to the end.** `list` counts events by reading every
  line of every session, so its cost grows with the total size of the store rather than the
  number of sessions.
- **A save is mostly fixed cost when small.** `store/save/10` is 2.9 ms against 18 ms for
  500 turns: the atomic write-and-rename dominates until the session is large.
- **Plugin cascades are superlinear in time but linear in memory.** Going from 8 to 64
  plugins multiplies `start_chain` by about 18 and `unload_root` by about 29, while
  allocations per plugin stay flat at about ten.
- **The agent loop's history cost is linear.** A turn on 100 turns of history is about
  0.8 ms and 12.8 K allocations, against 38 µs and 974 on a fresh session: each step re-folds
  and re-encodes the whole log, which is the `derive_messages` and `encode` cost above.
