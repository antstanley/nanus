//! What a request carries under each context mode, as a session grows past the budget.
//!
//! Not a timing: the counts and sizes of the request a stock composition would send on the
//! next turn of a 10-, 100- and 500-turn session, under the default 64,000-token budget. Each
//! mode runs one real turn through the runner; requests are prepared and measured by the real
//! `DeepSeek` adapter (`deepseek-flash` at its official endpoint), which performs no I/O until a
//! prepared call is streamed — and none is: the model's answer comes from a script.
//!
//!     cargo run --release -p nanus-bench --example context_footprint

// A report's output is the point; the workspace denies printing in libraries, not here.
#![allow(clippy::print_stdout)]

use core::cell::RefCell;
use std::rc::Rc;

use nanus_adapter_deepseek::{DeepSeekConfig, DeepSeekLlm};
use nanus_bench::fixtures;
use nanus_bundle::{AgentRunner, SessionContext, Silent, ToolRegistryHandle, TurnHost};
use nanus_domain::context::managed::{
    CheckpointReceipt, ContextFrontier, ContextModeRecord, ContextPolicy, Digest, Durability,
    ModeActor, ModeReason, SelectionIdentity,
};
use nanus_domain::{AgentConfig, Message, Session, SessionEvent, ToolRegistry};
use nanus_ports::{
    ChatRequest, CheckpointError, CheckpointView, ClockPort, ExpectedCheckpoint, FinishReason,
    LlmEvent, LlmPort, LlmResult, LlmStream, LocalBoxFuture, ManagedRequest, ManagedSupport,
    ModelCapabilities, PreparedModelCall, Reconciled, RequestEstimate, SessionCheckpoint,
    TurnRuntime,
};

const MODEL: &str = "deepseek-flash";

struct Clock;
impl ClockPort for Clock {
    fn now_ms(&self) -> u64 {
        1_767_225_600_000
    }
}

/// The real adapter for preparation and estimates; a scripted answer for the stream.
struct Recorder {
    inner: DeepSeekLlm,
    sent: Rc<RefCell<Vec<ChatRequest>>>,
}

fn answer() -> LlmStream {
    Box::pin(futures::stream::iter([
        LlmEvent::TextDelta("done".into()),
        LlmEvent::Finished {
            reason: FinishReason::Stop,
        },
    ]))
}

impl LlmPort for Recorder {
    fn model(&self) -> &'static str {
        MODEL
    }
    fn capabilities(&self, model: &str) -> ModelCapabilities {
        self.inner.capabilities(model)
    }
    fn estimate_request(&self, request: &ChatRequest) -> LlmResult<RequestEstimate> {
        self.inner.estimate_request(request)
    }
    fn stream_chat(&self, request: ChatRequest) -> LlmStream {
        self.sent.borrow_mut().push(request);
        answer()
    }
    fn managed_support(&self, model: &str) -> ManagedSupport {
        self.inner.managed_support(model)
    }
    fn prepare_managed(&self, request: ManagedRequest) -> LlmResult<Box<dyn PreparedModelCall>> {
        let copy = request.request.clone();
        let inner = self.inner.prepare_managed(request)?;
        Ok(Box::new(Recorded {
            estimate: inner.estimate(),
            digest: inner.request_digest().clone(),
            selection: inner.selection().clone(),
            request: copy,
            sent: Rc::clone(&self.sent),
        }))
    }
}

struct Recorded {
    estimate: RequestEstimate,
    digest: Digest,
    selection: SelectionIdentity,
    request: ChatRequest,
    sent: Rc<RefCell<Vec<ChatRequest>>>,
}

impl PreparedModelCall for Recorded {
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
        self.sent.borrow_mut().push(self.request);
        answer()
    }
}

/// A checkpoint that keeps nothing but the expected identity.
struct Memory(RefCell<ExpectedCheckpoint>);

impl SessionCheckpoint for Memory {
    fn expected(&self) -> ExpectedCheckpoint {
        self.0.borrow().clone()
    }
    fn commit<'a>(
        &'a self,
        view: CheckpointView<'a>,
    ) -> LocalBoxFuture<'a, Result<CheckpointReceipt, CheckpointError>> {
        Box::pin(async move {
            let body = view.candidate.try_to_jsonl().unwrap_or_default();
            let receipt = CheckpointReceipt {
                frontier: ContextFrontier {
                    session_id: view.candidate.id().as_str().to_owned(),
                    event_count: u64::try_from(view.candidate.event_count()).unwrap_or(0),
                    prefix_blake3: Digest::of(body.as_bytes()),
                    projection_revision: 0,
                },
                body_digest: view.candidate.body_digest(),
                durability: Durability::ProcessCrash,
            };
            *self.0.borrow_mut() =
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

fn runner(sent: &Rc<RefCell<Vec<ChatRequest>>>) -> AgentRunner {
    let inner = DeepSeekLlm::new(DeepSeekConfig::new(MODEL, "not-used"))
        .unwrap_or_else(|error| unreachable!("{error}"));
    let model = Recorder {
        inner,
        sent: Rc::clone(sent),
    };
    let config = AgentConfig::for_model(MODEL);
    AgentRunner::new(
        Rc::new(Box::new(model)),
        ToolRegistryHandle::new(ToolRegistry::new()),
        "You are measured.",
        config,
        Rc::new(Box::new(Clock)),
    )
    .unwrap_or_else(|error| unreachable!("{error}"))
}

/// One row of the report.
struct Row {
    users: (usize, usize),
    work: (usize, usize),
    body_bytes: usize,
    estimate: u32,
    note: String,
}

fn measure(request: &ChatRequest, history: &Session, llm: &DeepSeekLlm, note: String) -> Row {
    let count = |messages: &[Message], user: bool| {
        messages
            .iter()
            .filter(|message| matches!(message, Message::User { .. }) == user)
            .filter(|message| !matches!(message, Message::System { .. }))
            .count()
    };
    let all = history.derive_messages();
    let users_total = count(&all, true).saturating_add(1);
    let mut probe = request.clone();
    probe.max_tokens = probe.max_tokens.or(Some(4_096));
    let body_bytes = llm
        .estimate_request(&probe)
        .map_or(0, |estimate| estimate.request_bytes);
    Row {
        users: (count(&request.messages, true), users_total),
        work: (count(&request.messages, false), count(&all, false)),
        body_bytes,
        estimate: nanus_domain::estimate(&request.messages),
        note,
    }
}

fn legacy(history: &Session, llm: &DeepSeekLlm) -> Row {
    let sent = Rc::new(RefCell::new(Vec::new()));
    let runner = runner(&sent);
    let mut session = history.clone();
    let outcome =
        futures::executor::block_on(runner.run_turn(&mut session, "next", &mut Silent, None));
    let request = sent.borrow().last().cloned();
    match (outcome, request) {
        (Ok(_), Some(request)) => {
            let dropped = count_users(history).saturating_add(1).saturating_sub(
                request
                    .messages
                    .iter()
                    .filter(|m| matches!(m, Message::User { .. }))
                    .count(),
            );
            measure(
                &request,
                history,
                llm,
                format!("{dropped} whole turns dropped"),
            )
        }
        (Err(error), _) => refused(&error.to_string()),
        (Ok(_), None) => refused("nothing was sent"),
    }
}

fn managed(history: &Session, llm: &DeepSeekLlm) -> Row {
    let sent = Rc::new(RefCell::new(Vec::new()));
    let runner = runner(&sent);
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
    let disk = Memory(RefCell::new(ExpectedCheckpoint::Absent));
    let context = SessionContext::with_key(None, [1; 32]);
    let host = TurnHost {
        approver: None,
        control: None,
        runtime: TurnRuntime {
            context: Some(&context),
            checkpoint: Some(&disk),
        },
    };
    let run = futures::executor::block_on(runner.run_turn_with_runtime(
        &mut session,
        "next",
        &mut Silent,
        host,
    ));
    let hidden = session
        .log()
        .events()
        .iter()
        .rev()
        .find_map(|event| match event {
            SessionEvent::ContextRevision { payload } => Some(payload.hidden.len()),
            _ => None,
        })
        .unwrap_or(0);
    let request = sent.borrow().last().cloned();
    match (run.outcome, request) {
        (Ok(_), Some(request)) => {
            measure(&request, history, llm, format!("{hidden} fragments hidden"))
        }
        (Err(error), _) => refused(&error.to_string()),
        (Ok(_), None) => refused("nothing was sent"),
    }
}

fn count_users(session: &Session) -> usize {
    session
        .log()
        .events()
        .iter()
        .filter(|event| matches!(event, SessionEvent::UserMessage { .. }))
        .count()
}

fn refused(reason: &str) -> Row {
    Row {
        users: (0, 0),
        work: (0, 0),
        body_bytes: 0,
        estimate: 0,
        note: format!("refused: {reason}"),
    }
}

/// A copy of `session` whose user messages are `chars` characters long, as real prompts are.
fn with_prompts(session: &Session, chars: usize) -> Session {
    let mut copy = Session::new(session.id().clone(), session.created_at_ms(), session.cwd());
    for event in session.log().events() {
        let event = match event {
            SessionEvent::UserMessage {
                text,
                content_blocks,
            } => SessionEvent::UserMessage {
                text: format!("{text} {}", "context ".repeat(chars.saturating_div(8))),
                content_blocks: content_blocks.clone(),
            },
            other => other.clone(),
        };
        copy.append(event);
    }
    copy
}

fn main() {
    let llm = DeepSeekLlm::new(DeepSeekConfig::new(MODEL, "not-used"))
        .unwrap_or_else(|error| unreachable!("{error}"));
    println!(
        "| History | Mode | User messages sent | Assistant/tool messages sent | Body bytes | chars/4 estimate | |"
    );
    println!("|---:|---|---:|---:|---:|---:|---|");
    let cases = [10_u32, 100, 500, 1_000, 2_000]
        .map(|turns| (turns, 0_usize))
        .into_iter()
        .chain([20_u32, 30, 40, 50, 100, 200].map(|turns| (turns, 1_000_usize)));
    for (turns, prompt) in cases {
        let history = if prompt == 0 {
            fixtures::session(turns)
        } else {
            with_prompts(&fixtures::session(turns), prompt)
        };
        let turns = if prompt == 0 {
            turns.to_string()
        } else {
            format!("{turns} (1 KB prompts)")
        };
        for (mode, row) in [
            ("legacy", legacy(&history, &llm)),
            ("managed", managed(&history, &llm)),
        ] {
            println!(
                "| {turns} | {mode} | {}/{} | {}/{} | {} | {} | {} |",
                row.users.0,
                row.users.1,
                row.work.0,
                row.work.1,
                row.body_bytes,
                row.estimate,
                row.note
            );
        }
    }
}
