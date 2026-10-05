//! The interface's view layer: replaying a session, streaming into a transcript, and drawing.
//!
//! The interface redraws after every wake-up rather than on a timer, and a wake-up is a
//! keystroke or a frame from the agent — so while a model streams, the whole frame is drawn
//! once per delta. What a frame costs is therefore the interface's throughput ceiling: a draw
//! that takes longer than the gap between two tokens is a terminal that falls behind the
//! model. These run the view with no terminal, link, or store, against ratatui's
//! `TestBackend`, which is the same configuration the view's own tests use.

use criterion::measurement::Measurement;
use criterion::{BatchSize, BenchmarkGroup, BenchmarkId, Criterion, SamplingMode, Throughput};
use nanus_bench::{Metric, fixtures};
use nanus_tui::{Detail, InputBuffer, Role, Transcript, ViewState, transcript_of};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use std::hint::black_box;

/// Transcript lengths, in turns of the shared fixture session.
const TURNS: [u32; 2] = [10, 100];

/// Deltas in one streamed answer for the streaming benchmarks: about 12 KB at six bytes each.
const STREAM_DELTAS: usize = 2_000;

/// Deltas in one answer for the per-delta frame, after which the answer settles and the next
/// begins. A streamed tail that grew for the whole measurement would make the last iteration
/// draw a far longer answer than the first; capping it at a realistic length keeps every
/// iteration drawing the same kind of frame.
const ANSWER_DELTAS: usize = 512;

/// The bytes in one delta, roughly what a provider streams per token.
const DELTA_BYTES: usize = 6;

/// How a frame is drawn: the detail level, whether answers are markdown, and the terminal.
#[derive(Clone, Copy)]
struct Shape {
    detail: Detail,
    markdown: bool,
    width: u16,
    height: u16,
}

/// The default interface on an ordinary terminal.
const COMPACT: Shape = Shape {
    detail: Detail::Compact,
    markdown: true,
    width: 120,
    height: 40,
};

/// Which role the delta at an index streams as.
type RoleOf = fn(usize) -> Role;

fn events(turns: u32) -> u64 {
    u64::from(turns).saturating_mul(fixtures::EVENTS_PER_TURN as u64)
}

/// A streamed answer cut into `count` deltas of [`DELTA_BYTES`], on character boundaries.
fn deltas(count: usize) -> Vec<String> {
    let mut text = String::new();
    while text.len() < count.saturating_mul(DELTA_BYTES) {
        text.push_str(&fixtures::markdown_answer(4));
    }
    let chars: Vec<char> = text.chars().collect();
    let pieces: Vec<String> = chars
        .chunks(DELTA_BYTES)
        .take(count)
        .map(|piece| piece.iter().collect())
        .collect();
    assert_eq!(pieces.len(), count, "the answer is long enough to cut");
    pieces
}

/// Configures a group whose iterations take milliseconds.
///
/// Criterion's default linear sampling grows the iteration count by one per sample, so a
/// hundred samples of a 25 ms frame need over five thousand iterations — two minutes for one
/// benchmark. Flat sampling with fewer samples keeps a frame benchmark inside its window.
fn slow<M: Measurement>(group: &mut BenchmarkGroup<'_, M>) {
    group.sampling_mode(SamplingMode::Flat).sample_size(20);
}

fn draw(terminal: &mut Terminal<TestBackend>, state: &mut ViewState) {
    let drawn = terminal.draw(|frame| state.render(frame));
    assert!(drawn.is_ok(), "drawing into a test backend cannot fail");
}

/// A view of a replayed `turns`-turn session, drawn once and following the newest output,
/// which is where a live conversation sits.
fn view(turns: u32, shape: Shape) -> (ViewState, Terminal<TestBackend>) {
    let mut state = ViewState::new();
    state.transcript = transcript_of(&fixtures::session(turns));
    state.detail = shape.detail;
    state.markdown = shape.markdown;
    let mut terminal = Terminal::new(TestBackend::new(shape.width, shape.height))
        .unwrap_or_else(|error| unreachable!("a test terminal always builds: {error}"));
    // The first draw records the viewport, without which "the bottom" is undefined.
    draw(&mut terminal, &mut state);
    state.scroll_to_bottom();
    (state, terminal)
}

/// Folding a recorded session into a transcript: what opening or attaching to one costs
/// before the first frame. `recording_of` is the same fold plus one header line, so it is not
/// measured separately.
fn replay<M: Metric>(c: &mut Criterion<M>) {
    let mut group = c.benchmark_group(M::group("tui/replay"));
    for turns in TURNS {
        let session = fixtures::session(turns);
        group.throughput(Throughput::Elements(events(turns)));
        group.bench_with_input(
            BenchmarkId::from_parameter(turns),
            &session,
            |b, session| {
                b.iter(|| transcript_of(black_box(session)));
            },
        );
    }
    group.finish();
}

/// Appending a whole streamed answer to the transcript, delta by delta, with no drawing: the
/// transcript's own share of the per-token cost. `interleaved` is a reasoning segment followed
/// by an answer, which is the shape a thinking model streams and the case that settles one
/// entry to start the next.
fn stream<M: Metric>(c: &mut Criterion<M>) {
    let mut group = c.benchmark_group(M::group("tui/stream"));
    let deltas = deltas(STREAM_DELTAS);
    let half = STREAM_DELTAS.checked_div(2).unwrap_or(0);
    group.throughput(Throughput::Elements(STREAM_DELTAS as u64));
    let cases: [(&str, RoleOf); 3] = [
        ("answer", |_| Role::Assistant),
        ("reasoning", |_| Role::Reasoning),
        ("interleaved", |index| {
            if index < STREAM_DELTAS.checked_div(2).unwrap_or(0) {
                Role::Reasoning
            } else {
                Role::Assistant
            }
        }),
    ];
    assert!(half > 0, "both halves of the interleaved case are streamed");
    for (name, role_of) in cases {
        group.bench_function(name, |b| {
            b.iter(|| {
                let mut transcript = Transcript::new();
                for (index, delta) in deltas.iter().enumerate() {
                    let last = index.saturating_add(1) == deltas.len();
                    transcript.append_stream(role_of(index), delta, last);
                }
                transcript
            });
        });
    }
    group.finish();
}

/// Drawing one frame of a settled conversation, scrolled to the bottom: the redraw a keystroke
/// costs. Compact and full detail, markdown on and off, an ordinary and a large terminal.
fn frame<M: Metric>(c: &mut Criterion<M>) {
    let mut group = c.benchmark_group(M::group("tui/draw"));
    slow(&mut group);
    let cases = [
        ("compact", COMPACT),
        (
            "full",
            Shape {
                detail: Detail::Full,
                ..COMPACT
            },
        ),
        (
            "plain",
            Shape {
                markdown: false,
                ..COMPACT
            },
        ),
    ];
    for (name, shape) in cases {
        for turns in TURNS {
            let (mut state, mut terminal) = view(turns, shape);
            group.bench_function(BenchmarkId::new(name, turns), |b| {
                b.iter(|| draw(&mut terminal, &mut state));
            });
        }
    }
    let large = Shape {
        width: 200,
        height: 60,
        ..COMPACT
    };
    let (mut state, mut terminal) = view(100, large);
    group.bench_function(BenchmarkId::new("large", 100), |b| {
        b.iter(|| draw(&mut terminal, &mut state));
    });
    // The first frame of a replayed session, with nothing measured yet: every block is rendered
    // once. It is what opening a session costs, and what a resize or a change of display
    // setting costs, since either re-renders everything at the new conditions.
    for turns in TURNS {
        group.bench_function(BenchmarkId::new("first", turns), |b| {
            b.iter_batched(
                || {
                    let mut state = ViewState::new();
                    state.transcript = transcript_of(&fixtures::session(turns));
                    let terminal = Terminal::new(TestBackend::new(COMPACT.width, COMPACT.height))
                        .unwrap_or_else(|error| unreachable!("a terminal always builds: {error}"));
                    (state, terminal)
                },
                |(mut state, mut terminal)| {
                    draw(&mut terminal, &mut state);
                    (state, terminal)
                },
                BatchSize::PerIteration,
            );
        });
    }
    // No conversation at all: the floor every frame pays for the title, the composer, the
    // status line and the terminal itself, which the cases above are read against.
    let (mut state, mut terminal) = view(0, COMPACT);
    group.bench_function(BenchmarkId::new("empty", 0), |b| {
        b.iter(|| draw(&mut terminal, &mut state));
    });
    group.finish();
}

/// Drawing one frame scrolled to the middle of a long conversation, with following off: a
/// reader looking back while nothing streams. It should cost what the bottom costs.
fn scrolled<M: Metric>(c: &mut Criterion<M>) {
    let mut group = c.benchmark_group(M::group("tui/draw"));
    slow(&mut group);
    let (mut state, mut terminal) = view(100, COMPACT);
    state.scroll_offset = state.max_scroll().checked_div(2).unwrap_or(0);
    state.following = false;
    assert!(
        state.scroll_offset > 0,
        "a hundred turns overflow the viewport"
    );
    group.bench_function(BenchmarkId::new("scrolled", 100), |b| {
        b.iter(|| draw(&mut terminal, &mut state));
    });
    group.finish();
}

/// One streamed delta as the runtime handles it: appended to the tail, the view told to
/// follow, and the frame redrawn. This is the per-token cost of the interface, and the
/// comparison between ten and a hundred turns says whether it grows with the conversation.
fn delta<M: Metric>(c: &mut Criterion<M>) {
    let mut group = c.benchmark_group(M::group("tui/delta"));
    slow(&mut group);
    let deltas = deltas(ANSWER_DELTAS);
    group.throughput(Throughput::Elements(1));
    for turns in TURNS {
        let (mut state, mut terminal) = view(turns, COMPACT);
        let mut next = 0_usize;
        group.bench_function(BenchmarkId::from_parameter(turns), |b| {
            b.iter(|| {
                let fragment = deltas.get(next).map_or("", String::as_str);
                next = next.saturating_add(1);
                // The answer settles at its last delta and the next delta starts a new one.
                let last = next == deltas.len();
                if last {
                    next = 0;
                }
                state
                    .transcript
                    .append_stream(Role::Assistant, fragment, last);
                state.follow();
                draw(&mut terminal, &mut state);
            });
        });
    }
    group.finish();
}

/// Answer lengths for [`answer`], in deltas: about 3 KB and about 24 KB of markdown.
const LONG_ANSWERS: [usize; 2] = [500, 4_000];

/// How many deltas arrive between two redraws in [`answer`].
const BURST: usize = 32;

/// A whole answer streamed as the runtime receives it: every delta appended and followed, and
/// the frame redrawn once per burst, since the runtime drains the frames that queued up before
/// it draws.
///
/// The question is how the cost of a delta grows with the answer it lands in. Measured per delta
/// over answers of two lengths, a cost that rose with the answer would make the longer answer
/// dearer per delta, not merely longer; a flat one makes the two the same.
fn answer<M: Metric>(c: &mut Criterion<M>) {
    let mut group = c.benchmark_group(M::group("tui/answer"));
    group.sampling_mode(SamplingMode::Flat).sample_size(10);
    for count in LONG_ANSWERS {
        let deltas = deltas(count);
        group.throughput(Throughput::Elements(count as u64));
        group.bench_function(BenchmarkId::from_parameter(count), |b| {
            b.iter_batched(
                || view(10, COMPACT),
                |(mut state, mut terminal)| {
                    for (index, fragment) in deltas.iter().enumerate() {
                        state
                            .transcript
                            .append_stream(Role::Assistant, fragment, false);
                        state.follow();
                        if index % BURST == BURST.saturating_sub(1) {
                            draw(&mut terminal, &mut state);
                        }
                    }
                    draw(&mut terminal, &mut state);
                    (state, terminal)
                },
                BatchSize::PerIteration,
            );
        });
    }
    group.finish();
}

/// What the interface holds for a session of a hundred turns, measured as residency rather than
/// traffic: the transcript a replay builds, and what a view adds to it by drawing.
///
/// The view is given a transcript built outside the measurement and draws one frame into a
/// terminal also built outside it, so the second figure is exactly what the view keeps between
/// frames — its layout, the lines it holds for the screen, and the screen it captured — and not
/// the conversation itself, which the first figure is.
fn resident<M: Metric>(c: &mut Criterion<M>) {
    let mut group = c.benchmark_group(M::group("tui/resident"));
    let session = fixtures::session(100);
    group.bench_function(BenchmarkId::new("transcript", 100), |b| {
        b.iter_batched(|| (), |()| transcript_of(&session), BatchSize::PerIteration);
    });
    group.bench_function(BenchmarkId::new("view", 100), |b| {
        b.iter_batched(
            || {
                let terminal = Terminal::new(TestBackend::new(COMPACT.width, COMPACT.height))
                    .unwrap_or_else(|error| unreachable!("a test terminal always builds: {error}"));
                (transcript_of(&session), terminal)
            },
            |(transcript, mut terminal)| {
                let mut state = ViewState::new();
                state.transcript = transcript;
                draw(&mut terminal, &mut state);
                state.scroll_to_bottom();
                draw(&mut terminal, &mut state);
                (state, terminal)
            },
            BatchSize::PerIteration,
        );
    });
    // Every line the conversation renders to, held at once: what drawing a frame used to build
    // and hold before the layout kept measurements instead, so the figure the view's own
    // residency is read against.
    let mut state = ViewState::new();
    state.transcript = transcript_of(&session);
    group.bench_function(BenchmarkId::new("lines", 100), |b| {
        b.iter_batched(
            || (),
            |()| state.transcript_lines(COMPACT.width),
            BatchSize::PerIteration,
        );
    });
    group.finish();
}

/// A keystroke in the middle of a long prompt: one character typed and taken back, so the
/// buffer is the same before every iteration. The composer holds characters, so an edit in
/// the middle moves everything after it.
fn input<M: Metric>(c: &mut Criterion<M>) {
    let mut group = c.benchmark_group(M::group("tui/input"));
    let prompt = fixtures::prose(2_000);
    let mut buffer = InputBuffer::with_text(&prompt);
    buffer.place_cursor(0, prompt.chars().count().checked_div(2).unwrap_or(0));
    group.bench_function("insert_backspace/2000", |b| {
        b.iter(|| {
            buffer.insert(black_box('x'));
            buffer.backspace()
        });
    });
    assert_eq!(buffer.text(), prompt, "every keystroke was taken back");
    group.finish();
}

nanus_bench::benches!(replay, stream, frame, scrolled, delta, answer, input; retained: resident);
