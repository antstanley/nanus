//! What managed context costs a real turn, beside the legacy loop it replaces.
//!
//! Every case runs the same scripted turn — a tool step that reads and searches, then a
//! 500-delta answer — through `run_turn` (legacy) and through `run_turn_with_runtime` on a
//! managed session, so the difference is the managed machinery and nothing else:
//!
//! - **`overhead`**: no disk. The legacy case includes the session encoding its host's save
//!   performs; the managed case checkpoints through an in-memory checkpoint that does the same
//!   encoding and hashing a store does for each commit (intent and settled step per step, and
//!   the turn's end). Both "adapters" encode each request body once, as a real one does, so the
//!   managed case's extra preparations are counted at their real size.
//! - **`persisted`**: the same turn through a real `JsonlStore` on disk — one atomic save for
//!   legacy, the five atomic checkpoints of a two-step managed turn — so file writes and
//!   `fsync` are in the numbers.
//! - **`fitted`**: a long history and the default 64,000-token budget, so every request must
//!   be fitted: the legacy fitter drops whole old turns, managed fitting hides old fragments.
//!   `first` is a session fitted for the first time; `steady` is the next turn of a session
//!   whose accepted revision already fits most of it.

use core::cell::{Cell, RefCell};
use std::rc::Rc;

use criterion::{BatchSize, BenchmarkId, Criterion};
use nanus_bench::{Metric, fixtures};
use nanus_bundle::{
    AgentRunner, SessionContext, Silent, StoreCheckpoint, ToolRegistryHandle, TurnHost,
};
use nanus_domain::context::managed::{
    CheckpointReceipt, ContextModeRecord, ContextPolicy, Digest, Durability, ManagedState,
    ModeActor, ModeReason, SelectionIdentity,
};
use nanus_domain::{
    AgentConfig, Message, Session, SessionEvent, SessionId, ToolAccess, ToolCall, ToolCallId,
    ToolDefinition, ToolExecutor, ToolFuture, ToolName, ToolRegistry, ToolResult, ToolSchema,
    TurnEndReason, Usage,
};
use nanus_ports::{
    ChatRequest, CheckpointError, CheckpointView, ClockHandle, ClockPort, ExpectedCheckpoint,
    FinishReason, LlmEvent, LlmPort, LlmResult, LlmStream, LocalBoxFuture, ManagedRequest,
    ManagedSupport, ModelCapabilities, PreparedModelCall, Reconciled, RequestEstimate,
    SessionCheckpoint, TurnRuntime,
};
use serde_json::json;

/// The prompt every benchmarked turn starts with.
const PROMPT: &str = "Explain how the session log stays contiguous.";
/// Text deltas in the answer.
const ANSWER_DELTAS: usize = 500;
/// The stock context budget.
const STOCK_BUDGET: u32 = nanus_domain::DEFAULT_CONTEXT_BUDGET;

/// A clock that never moves.
struct FixedClock;

impl ClockPort for FixedClock {
    fn now_ms(&self) -> u64 {
        1_767_225_600_000
    }
}

fn tool_name(raw: &str) -> ToolName {
    ToolName::new(raw).unwrap_or_else(|error| unreachable!("bench tool name {raw}: {error}"))
}

/// The tool step and the answer, cloned per event as a decoder would produce them.
struct Script {
    tool_step: Rc<[LlmEvent]>,
    answer: Rc<[LlmEvent]>,
    /// How many tool steps have been streamed, so each one's call ids are new: a managed
    /// session refuses a call id that an earlier turn already used.
    steps: Cell<u64>,
}

impl Script {
    fn new() -> Rc<Self> {
        let mut step: Vec<LlmEvent> = (0..50)
            .map(|_| LlmEvent::ReasoningDelta("I should look at the log first. ".to_owned()))
            .collect();
        for (index, name) in [(0_u32, "lookup"), (1_u32, "search")] {
            step.push(LlmEvent::ToolCallDelta {
                index,
                id: Some(ToolCallId::new(format!("bench-{name}"))),
                name: Some(tool_name(name)),
                arguments_delta: r#"{"query":"SessionLog::append"}"#.to_owned(),
            });
        }
        step.push(LlmEvent::Usage(Usage::new(12_000, 180, 120, 11_000, 1_000)));
        step.push(LlmEvent::Finished {
            reason: FinishReason::ToolCalls,
        });
        let mut answer: Vec<LlmEvent> = (0..ANSWER_DELTAS)
            .map(|_| LlmEvent::TextDelta("the log numbers events ".to_owned()))
            .collect();
        answer.push(LlmEvent::Usage(Usage::new(14_500, 420, 90, 13_800, 700)));
        answer.push(LlmEvent::Finished {
            reason: FinishReason::Stop,
        });
        Rc::new(Self {
            tool_step: step.into(),
            answer: answer.into(),
            steps: Cell::new(0),
        })
    }

    /// The tool step after the user's message, the answer after a tool result.
    fn respond(&self, request: &ChatRequest) -> LlmStream {
        let (script, step) = match request.messages.last() {
            Some(Message::User { .. }) => {
                let step = self.steps.get();
                self.steps.set(step.saturating_add(1));
                (Rc::clone(&self.tool_step), Some(step))
            }
            _ => (Rc::clone(&self.answer), None),
        };
        let len = script.len();
        Box::pin(futures::stream::iter((0..len).filter_map(move |index| {
            let event = script.get(index).cloned()?;
            Some(match (event, step) {
                (
                    LlmEvent::ToolCallDelta {
                        index,
                        id: Some(id),
                        name,
                        arguments_delta,
                    },
                    Some(step),
                ) => LlmEvent::ToolCallDelta {
                    index,
                    id: Some(ToolCallId::new(format!("{}-{step}", id.as_str()))),
                    name,
                    arguments_delta,
                },
                (event, _) => event,
            })
        })))
    }
}

/// Encodes a request body the way an adapter does, once per request it prepares or sends.
fn encode(request: &ChatRequest) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "model": request.model,
        "messages": request.messages,
        "tools": request.tools,
        "max_tokens": request.max_tokens,
        "stream": true,
    }))
    .unwrap_or_default()
}

/// A model that encodes every body it is given, answers from the script, and prepares
/// managed calls the way the stock adapters do: `MANAGED_BYTES_PER_TOKEN` bytes of body are one
/// estimated token, and a candidate that does not fit is refused.
struct Model {
    script: Rc<Script>,
}

impl LlmPort for Model {
    fn model(&self) -> &'static str {
        "bench-model"
    }
    fn capabilities(&self, _: &str) -> ModelCapabilities {
        ModelCapabilities {
            max_output_tokens: Some(32_768),
            ..ModelCapabilities::default()
        }
    }
    fn stream_chat(&self, request: ChatRequest) -> LlmStream {
        let body = encode(&request);
        assert!(!body.is_empty(), "the body encodes");
        self.script.respond(&request)
    }
    fn managed_support(&self, _: &str) -> ManagedSupport {
        ManagedSupport::Supported { policy_version: 1 }
    }
    fn prepare_managed(&self, request: ManagedRequest) -> LlmResult<Box<dyn PreparedModelCall>> {
        let body = encode(&request.request);
        let estimate = RequestEstimate {
            input_tokens: u32::try_from(
                body.len()
                    .div_ceil(nanus_ports::capabilities::MANAGED_BYTES_PER_TOKEN),
            )
            .unwrap_or(u32::MAX),
            request_bytes: body.len(),
            images: 0,
            reservation: request.request.max_tokens.unwrap_or(0),
        };
        if !estimate.fits(self.capabilities(&request.request.model), &request.request) {
            return Err(nanus_ports::LlmError::Unsupported {
                feature: "candidate_too_large: over the budget".into(),
            });
        }
        Ok(Box::new(Call {
            script: Rc::clone(&self.script),
            digest: Digest::of(&body),
            selection: SelectionIdentity {
                provider: "bench".into(),
                endpoint_digest: Digest::of(b"bench"),
                protocol: "bench.chat".into(),
                model: request.request.model.clone(),
                effort: None,
                epoch: request.selection_epoch,
            },
            request: request.request,
            estimate,
        }))
    }
}

struct Call {
    script: Rc<Script>,
    request: ChatRequest,
    digest: Digest,
    selection: SelectionIdentity,
    estimate: RequestEstimate,
}

impl PreparedModelCall for Call {
    fn estimate(&self) -> RequestEstimate {
        self.estimate
    }
    fn request_digest(&self) -> &Digest {
        &self.digest
    }
    fn selection(&self) -> &SelectionIdentity {
        &self.selection
    }
    fn stream(self: Box<Self>) -> LlmStream {
        self.script.respond(&self.request)
    }
}

/// A tool that answers from memory.
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
            parameters: json!({"type": "object", "properties": {"query": {"type": "string"}}}),
        };
        let definition = ToolDefinition::new(
            schema,
            MemoryTool {
                reply: reply.into(),
            },
        )
        .with_access(ToolAccess::Read);
        registry
            .register(definition)
            .unwrap_or_else(|error| unreachable!("registering {name}: {error}"));
    }
    ToolRegistryHandle::new(registry)
}

fn runner(budget: u32) -> AgentRunner {
    let clock: ClockHandle = Rc::new(Box::new(FixedClock));
    let config = AgentConfig::new(8, 4, "bench-model", 16_384)
        .and_then(|config| config.with_context_budget(budget))
        .unwrap_or_else(|error| unreachable!("the benchmark configuration: {error}"));
    AgentRunner::new(
        Rc::new(Box::new(Model {
            script: Script::new(),
        })),
        tools(),
        "You are benchmarked.",
        config,
        clock,
    )
    .unwrap_or_else(|error| unreachable!("the benchmark runner: {error}"))
}

/// A checkpoint that does a store's CPU work for each commit — encoding what it would write,
/// checking the candidate extends what is stored, folding the revision and digesting the
/// receipt — without the disk.
struct Memory {
    expected: RefCell<ExpectedCheckpoint>,
}

impl Memory {
    fn new() -> Self {
        Self {
            expected: RefCell::new(ExpectedCheckpoint::Absent),
        }
    }
}

impl SessionCheckpoint for Memory {
    fn expected(&self) -> ExpectedCheckpoint {
        self.expected.borrow().clone()
    }
    fn commit<'a>(
        &'a self,
        view: CheckpointView<'a>,
    ) -> LocalBoxFuture<'a, Result<CheckpointReceipt, CheckpointError>> {
        Box::pin(async move {
            let candidate = view.candidate;
            // What the store writes: the whole file the first time, then only the new lines,
            // after checking that the candidate extends the stored prefix.
            let written = match &*self.expected.borrow() {
                ExpectedCheckpoint::Stored {
                    event_count,
                    file_blake3,
                    ..
                } => {
                    let stored = candidate
                        .prefix_digest(*event_count)
                        .unwrap_or_else(|error| unreachable!("a stored prefix: {error}"));
                    assert_eq!(&stored, file_blake3, "the candidate extends what is stored");
                    candidate
                        .encoded_lines_from(*event_count)
                        .unwrap_or_else(|error| unreachable!("new lines encode: {error}"))
                }
                ExpectedCheckpoint::Absent => candidate
                    .try_to_jsonl()
                    .unwrap_or_else(|error| unreachable!("a session encodes: {error}")),
            };
            assert!(!written.is_empty(), "a checkpoint writes something");
            let revision = ManagedState::fold(candidate.log()).map_or(0, |s| s.revision());
            let count = u64::try_from(candidate.event_count()).unwrap_or(0);
            let receipt = CheckpointReceipt {
                frontier: nanus_domain::context::managed::ContextFrontier {
                    session_id: candidate.id().as_str().to_owned(),
                    event_count: count,
                    prefix_blake3: candidate
                        .prefix_digest(count)
                        .unwrap_or_else(|error| unreachable!("a whole prefix: {error}")),
                    projection_revision: revision,
                },
                body_digest: view.candidate.body_digest(),
                durability: Durability::ProcessCrash,
            };
            *self.expected.borrow_mut() =
                ExpectedCheckpoint::after(&receipt, view.candidate.body_version());
            Ok(receipt)
        })
    }
    fn reconcile<'a>(
        &'a self,
        _: &'a Digest,
    ) -> LocalBoxFuture<'a, Result<Reconciled, CheckpointError>> {
        Box::pin(async { Ok(Reconciled::Quarantined) })
    }
}

/// A copy of `history` that has enabled managed context.
fn managed(history: &Session) -> Session {
    let mut session = history.clone();
    session.upgrade_to_managed_body();
    session.append(SessionEvent::ContextMode {
        payload: Box::new(ContextModeRecord {
            policy: ContextPolicy::managed(),
            actor: ModeActor::Human,
            reason: ModeReason::Enable,
            previous_revision: 0,
        }),
    });
    session
}

fn legacy_turn(runner: &AgentRunner, mut session: Session) -> Session {
    let outcome =
        futures::executor::block_on(runner.run_turn(&mut session, PROMPT, &mut Silent, None))
            .unwrap_or_else(|error| unreachable!("a legacy turn completes: {error}"));
    assert_eq!(outcome.reason, TurnEndReason::Completed);
    // The host's save encodes the session once.
    assert!(session.try_to_jsonl().is_ok_and(|body| !body.is_empty()));
    session
}

fn managed_turn(runner: &AgentRunner, mut session: Session, runtime: TurnRuntime<'_>) -> Session {
    let host = TurnHost {
        approver: None,
        control: None,
        runtime,
    };
    let run = futures::executor::block_on(runner.run_turn_with_runtime(
        &mut session,
        PROMPT,
        &mut Silent,
        host,
    ));
    let outcome = run
        .outcome
        .unwrap_or_else(|error| unreachable!("a managed turn completes: {error}"));
    assert_eq!(outcome.reason, TurnEndReason::Completed);
    session
}

/// Legacy and managed turns with no disk, on growing histories.
fn overhead<M: Metric>(c: &mut Criterion<M>) {
    let runner = runner(u32::MAX);
    let context = SessionContext::with_key(None, [7; 32]);
    let mut group = c.benchmark_group(M::group("managed_turn/overhead"));
    for turns in [0_u32, 10, 100] {
        let history = fixtures::session(turns);
        let managed_history = managed(&history);
        group.bench_with_input(BenchmarkId::new("legacy", turns), &history, |b, history| {
            b.iter_batched(
                || history.clone(),
                |session| legacy_turn(&runner, session),
                BatchSize::LargeInput,
            );
        });
        group.bench_with_input(
            BenchmarkId::new("managed", turns),
            &managed_history,
            |b, history| {
                b.iter_batched(
                    || (history.clone(), Memory::new()),
                    |(session, disk)| {
                        let runtime = TurnRuntime {
                            context: Some(&context),
                            checkpoint: Some(&disk),
                        };
                        managed_turn(&runner, session, runtime)
                    },
                    BatchSize::LargeInput,
                );
            },
        );
    }
    group.finish();
}

/// Legacy and managed turns persisted to a real store on disk.
fn persisted<M: Metric>(c: &mut Criterion<M>) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|error| unreachable!("a runtime: {error}"));
    let home = tempfile::tempdir().unwrap_or_else(|error| unreachable!("a temp dir: {error}"));
    let store = runtime.block_on(async {
        nanus_adapter_store::JsonlStore::new(home.path())
            .await
            .unwrap_or_else(|error| unreachable!("a store: {error}"))
            .handle()
    });
    let runner = runner(u32::MAX);
    let mut group = c.benchmark_group(M::group("managed_turn/persisted"));
    for turns in [10_u32, 100] {
        let history = fixtures::session(turns);
        let legacy_id = SessionId::new(format!("legacy-{turns}"));
        let managed_id = SessionId::new(format!("managed-{turns}"));
        let rename = |session: &Session, id: &SessionId| {
            let mut copy = Session::new(id.clone(), session.created_at_ms(), fixtures::CWD);
            for event in session.log().events() {
                copy.append(event.clone());
            }
            copy
        };
        let legacy_history = rename(&history, &legacy_id);
        let managed_history = managed(&rename(&history, &managed_id));
        runtime.block_on(async {
            store
                .lock(&managed_id, "bench")
                .await
                .unwrap_or_else(|e| unreachable!("{e}"));
        });
        group.bench_with_input(
            BenchmarkId::new("legacy", turns),
            &legacy_history,
            |b, history| {
                b.iter_batched(
                    || history.clone(),
                    |mut session| {
                        runtime.block_on(async {
                            runner
                                .run_turn(&mut session, PROMPT, &mut Silent, None)
                                .await
                                .unwrap_or_else(|error| unreachable!("{error}"));
                            store
                                .save(&session)
                                .await
                                .unwrap_or_else(|e| unreachable!("{e}"));
                        });
                        session
                    },
                    BatchSize::LargeInput,
                );
            },
        );
        group.bench_with_input(
            BenchmarkId::new("managed", turns),
            &managed_history,
            |b, history| {
                b.iter_batched(
                    || {
                        runtime.block_on(async {
                            store
                                .save(history)
                                .await
                                .unwrap_or_else(|e| unreachable!("{e}"));
                            let checkpoint =
                                StoreCheckpoint::bind(store.clone(), history.id().clone())
                                    .await
                                    .unwrap_or_else(|e| unreachable!("{e}"));
                            let context = SessionContext::with_key(Some(store.clone()), [7; 32]);
                            (history.clone(), checkpoint, context)
                        })
                    },
                    |(mut session, checkpoint, context)| {
                        let host = TurnHost {
                            approver: None,
                            control: None,
                            runtime: TurnRuntime {
                                context: Some(&context),
                                checkpoint: Some(&checkpoint),
                            },
                        };
                        let run = runtime.block_on(runner.run_turn_with_runtime(
                            &mut session,
                            PROMPT,
                            &mut Silent,
                            host,
                        ));
                        assert!(run.outcome.is_ok(), "{:?}", run.outcome);
                        session
                    },
                    BatchSize::LargeInput,
                );
            },
        );
        store.release_lock(&managed_id);
    }
    group.finish();
}

/// Legacy and managed turns on histories the stock budget cannot hold whole.
fn fitted<M: Metric>(c: &mut Criterion<M>) {
    let runner = runner(STOCK_BUDGET);
    let context = SessionContext::with_key(None, [7; 32]);
    let mut group = c.benchmark_group(M::group("managed_turn/fitted"));
    for turns in [100_u32, 300] {
        let history = fixtures::session(turns);
        let managed_history = managed(&history);
        // One managed turn ahead of time, so `steady` starts from an accepted revision.
        let steady_history = {
            let disk = Memory::new();
            let runtime = TurnRuntime {
                context: Some(&context),
                checkpoint: Some(&disk),
            };
            managed_turn(&runner, managed_history.clone(), runtime)
        };
        group.bench_with_input(BenchmarkId::new("legacy", turns), &history, |b, history| {
            b.iter_batched(
                || history.clone(),
                |session| legacy_turn(&runner, session),
                BatchSize::LargeInput,
            );
        });
        for (label, start) in [
            ("managed_first", &managed_history),
            ("managed_steady", &steady_history),
        ] {
            group.bench_with_input(BenchmarkId::new(label, turns), start, |b, start| {
                b.iter_batched(
                    || (start.clone(), Memory::new()),
                    |(session, disk)| {
                        let runtime = TurnRuntime {
                            context: Some(&context),
                            checkpoint: Some(&disk),
                        };
                        managed_turn(&runner, session, runtime)
                    },
                    BatchSize::LargeInput,
                );
            });
        }
    }
    group.finish();
}

nanus_bench::benches!(overhead, persisted, fitted);
