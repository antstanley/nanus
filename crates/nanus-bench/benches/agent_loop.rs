//! The agent loop's own cost: a turn with a scripted model and in-memory tools.
//!
//! Everything a real turn pays apart from the network and the tools themselves is here —
//! folding the log into a request, accumulating the stream, reassembling tool calls, gating
//! and dispatching them, and appending every event to the session. None of it is visible
//! behind a model that takes seconds to answer, which is exactly why it is measured on its
//! own: it is the part that grows with the session while the model's latency does not.
//!
//! The model replays a fixed script and the tools answer from memory, so the numbers are the
//! loop and nothing else. The script's events are cloned as they are streamed, one owned
//! `String` per delta — the same allocation a real adapter's decoder makes for each one — so
//! that cost is counted here as it would be in production.

use std::rc::Rc;

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput};
use nanus_bench::{Metric, fixtures};
use nanus_bundle::{AgentRunner, Silent, ToolRegistryHandle};
use nanus_domain::{
    AgentConfig, Message, Session, SessionEvent, SessionId, ToolAccess, ToolCall, ToolCallId,
    ToolDefinition, ToolExecutor, ToolFuture, ToolName, ToolRegistry, ToolResult, ToolSchema,
    TurnEndReason, Usage,
};
use nanus_ports::{
    ChatRequest, ClockHandle, ClockPort, FinishReason, LlmEvent, LlmPort, LlmStream,
};
use serde_json::json;

/// Reasoning deltas in the tool-calling step.
const REASONING_DELTAS: usize = 50;
/// Text deltas in the answer of a standard turn.
const ANSWER_DELTAS: usize = 500;
/// History lengths, in fixture turns, that a turn is appended to.
const HISTORY_TURNS: [u32; 2] = [10, 100];
/// Answer lengths, in deltas, for the streaming benchmark.
const STREAM_DELTAS: [usize; 2] = [500, 2_000];
/// The prompt every benchmarked turn starts with.
const PROMPT: &str = "Explain how the session log stays contiguous.";

/// A clock that never moves: the loop reads it, and a moving one would only add noise.
struct FixedClock;

impl ClockPort for FixedClock {
    fn now_ms(&self) -> u64 {
        1_767_225_600_000
    }
}

/// A model that answers from two fixed scripts, chosen by where the conversation stands.
///
/// A request that ends in the user's message gets the tool step, and one that ends in a tool
/// result gets the answer. Keying on the request rather than a counter is what lets one model
/// serve every iteration of a benchmark without being reset between them.
struct ScriptedModel {
    /// The tool-calling step, or `None` for a turn that answers straight away.
    tool_step: Option<Rc<[LlmEvent]>>,
    answer: Rc<[LlmEvent]>,
}

impl ScriptedModel {
    fn new(with_tools: bool, answer_deltas: usize) -> Self {
        Self {
            tool_step: with_tools.then(tool_step),
            answer: answer(answer_deltas),
        }
    }
}

impl LlmPort for ScriptedModel {
    fn model(&self) -> &'static str {
        "bench-model"
    }

    fn stream_chat(&self, request: ChatRequest) -> LlmStream {
        let after_user = matches!(request.messages.last(), Some(Message::User { .. }));
        let script = match (&self.tool_step, after_user) {
            (Some(step), true) => Rc::clone(step),
            _ => Rc::clone(&self.answer),
        };
        let len = script.len();
        // Streamed lazily, one clone per event, as a decoder would produce them.
        Box::pin(futures::stream::iter(
            (0..len).filter_map(move |index| script.get(index).cloned()),
        ))
    }
}

fn tool_name(raw: &str) -> ToolName {
    ToolName::new(raw).unwrap_or_else(|error| unreachable!("bench tool name {raw}: {error}"))
}

/// Reasoning, then two tool calls, the way a provider streams a step that reads and searches.
fn tool_step() -> Rc<[LlmEvent]> {
    let mut events: Vec<LlmEvent> = (0..REASONING_DELTAS)
        .map(|_| LlmEvent::ReasoningDelta("I should look at the log first. ".to_owned()))
        .collect();
    for (index, name) in [(0_u32, "lookup"), (1_u32, "search")] {
        events.push(LlmEvent::ToolCallDelta {
            index,
            id: Some(ToolCallId::new(format!("call-{name}"))),
            name: Some(tool_name(name)),
            arguments_delta: r#"{"query":"#.to_owned(),
        });
        events.push(LlmEvent::ToolCallDelta {
            index,
            id: None,
            name: None,
            arguments_delta: r#""SessionLog::append"}"#.to_owned(),
        });
    }
    events.push(LlmEvent::Usage(Usage::new(12_000, 180, 120, 11_000, 1_000)));
    events.push(LlmEvent::Finished {
        reason: FinishReason::ToolCalls,
    });
    events.into()
}

/// An answer of `deltas` text deltas, a few words each.
fn answer(deltas: usize) -> Rc<[LlmEvent]> {
    let mut events: Vec<LlmEvent> = (0..deltas)
        .map(|_| LlmEvent::TextDelta("the log numbers events ".to_owned()))
        .collect();
    events.push(LlmEvent::Usage(Usage::new(14_500, 420, 90, 13_800, 700)));
    events.push(LlmEvent::Finished {
        reason: FinishReason::Stop,
    });
    events.into()
}

/// A tool that answers from memory, so dispatch is measured and not the work behind it.
struct MemoryTool {
    reply: Rc<str>,
}

impl ToolExecutor for MemoryTool {
    fn execute(&self, call: ToolCall) -> ToolFuture {
        let reply = Rc::clone(&self.reply);
        Box::pin(async move {
            ToolResult::new(
                call.id,
                nanus_bundle::tools::text_success(json!({ "bytes": reply.len() }), &*reply),
            )
        })
    }
}

/// Two read-only in-memory tools.
///
/// Declared [`ToolAccess::Read`], which every sandbox mode permits without asking, so the
/// turn runs under the default approval policy with no one to answer — the benchmark does
/// not loosen a single default to get its tools called.
fn tools() -> ToolRegistryHandle {
    let mut registry = ToolRegistry::new();
    for (name, reply) in [
        ("lookup", fixtures::source_file(80)),
        (
            "search",
            "crates/nanus-domain/src/session.rs:334: pub fn append\n".to_owned(),
        ),
    ] {
        let schema = ToolSchema {
            name: tool_name(name),
            description: format!("The benchmark's in-memory {name} tool."),
            parameters: json!({
                "type": "object",
                "properties": { "query": { "type": "string" } },
            }),
        };
        let reply: Rc<str> = reply.into();
        let definition =
            ToolDefinition::new(schema, MemoryTool { reply }).with_access(ToolAccess::Read);
        registry
            .register(definition)
            .unwrap_or_else(|error| unreachable!("registering {name}: {error}"));
    }
    ToolRegistryHandle::new(registry)
}

/// A runner over `model`.
///
/// The context budget is raised past every session here, so a long history is sent whole:
/// the benchmark measures what folding and encoding a long log costs, not where trimming
/// starts. Trimming is a policy with its own cost, and it is not the one under test.
fn runner(model: ScriptedModel) -> AgentRunner {
    let clock: ClockHandle = Rc::new(Box::new(FixedClock));
    let config = AgentConfig::new(8, 4, "bench-model", 16_384)
        .and_then(|config| config.with_context_budget(u32::MAX))
        .unwrap_or_else(|error| unreachable!("the benchmark configuration: {error}"));
    AgentRunner::new(
        Rc::new(Box::new(model)),
        tools(),
        "You are benchmarked.",
        config,
        clock,
    )
    .unwrap_or_else(|error| unreachable!("the benchmark runner: {error}"))
}

/// Runs one turn on `session` and hands the session back, so its drop is not measured.
fn turn(runner: &AgentRunner, mut session: Session) -> Session {
    let outcome =
        futures::executor::block_on(runner.run_turn(&mut session, PROMPT, &mut Silent, None))
            .unwrap_or_else(|error| unreachable!("a scripted turn completes: {error}"));
    assert_eq!(outcome.reason, TurnEndReason::Completed);
    session
}

/// Runs one turn to check the script takes the steps it is meant to, and that its tool calls
/// ran rather than being refused, before any of it is measured.
fn check(runner: &AgentRunner, steps: u32, tool_results: usize) {
    let mut session = Session::new(SessionId::new("bench-check"), 0, fixtures::CWD);
    let outcome =
        futures::executor::block_on(runner.run_turn(&mut session, PROMPT, &mut Silent, None))
            .unwrap_or_else(|error| unreachable!("a scripted turn completes: {error}"));
    assert_eq!(outcome.reason, TurnEndReason::Completed);
    assert_eq!(outcome.steps, steps, "the script takes its steps");
    let succeeded = session
        .log()
        .events()
        .iter()
        .filter(|event| {
            matches!(
                event,
                SessionEvent::ToolResult {
                    is_error: false,
                    ..
                }
            )
        })
        .count();
    assert_eq!(succeeded, tool_results, "every scripted tool call ran");
}

/// One standard turn — a tool step and a 500-delta answer — on an empty session, and on
/// sessions that already hold history.
///
/// The fresh case is the loop's floor. The history cases are what a long conversation pays:
/// each step folds the whole log into a request, so their difference from the floor is the
/// per-turn price of the history, and how it scales between ten and a hundred turns says
/// whether that price is linear.
fn turn_cost<M: Metric>(c: &mut Criterion<M>) {
    let runner = runner(ScriptedModel::new(true, ANSWER_DELTAS));
    check(&runner, 2, 2);
    let mut group = c.benchmark_group(M::group("agent_loop/turn"));
    group.bench_function("fresh", |b| {
        b.iter_batched(
            || Session::new(SessionId::new("bench-fresh"), 0, fixtures::CWD),
            |session| turn(&runner, session),
            BatchSize::SmallInput,
        );
    });
    for turns in HISTORY_TURNS {
        let history = fixtures::session(turns);
        group.bench_with_input(
            BenchmarkId::new("history", turns),
            &history,
            |b, history| {
                b.iter_batched(
                    || history.clone(),
                    |session| turn(&runner, session),
                    BatchSize::LargeInput,
                );
            },
        );
    }
    group.finish();
}

/// A turn that only answers, measured per streamed delta: the loop's per-token cost, which is
/// paid for every token of every answer.
fn stream<M: Metric>(c: &mut Criterion<M>) {
    let mut group = c.benchmark_group(M::group("agent_loop/stream"));
    for deltas in STREAM_DELTAS {
        let runner = runner(ScriptedModel::new(false, deltas));
        check(&runner, 1, 0);
        group.throughput(Throughput::Elements(deltas as u64));
        group.bench_function(BenchmarkId::from_parameter(deltas), |b| {
            b.iter_batched(
                || Session::new(SessionId::new("bench-stream"), 0, fixtures::CWD),
                |session| turn(&runner, session),
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

nanus_bench::benches!(turn_cost, stream);
