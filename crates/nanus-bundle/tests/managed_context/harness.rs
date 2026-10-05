//! The doubles the managed-context tests share: a preparing model, a checkpoint, a tool.

#![allow(dead_code, unreachable_pub)]

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::future::Future;
use std::rc::Rc;

use nanus_bundle::{AgentRunner, SessionContext, ToolRegistryHandle, TurnHost};
use nanus_domain::context::managed::{
    AttemptPhase, CheckpointReceipt, ContextPolicy, Digest, Durability, ManagedState, ModeActor,
    RequestAttemptRecord, SelectionIdentity, state,
};
use nanus_domain::{
    AgentConfig, ContentBlock, SandboxMode, Session, SessionEvent, SessionId, ToolCall, ToolCallId,
    ToolDefinition, ToolName, ToolOutcome, ToolRegistry, ToolResult, ToolSchema,
};
use nanus_ports::{
    ChatRequest, CheckpointError, CheckpointView, ExpectedCheckpoint, FinishReason, LlmEvent,
    LlmPort, LlmResult, LlmStream, LocalBoxFuture, ManagedRequest, ManagedSupport,
    ModelCapabilities, PreparedModelCall, Reconciled, RequestEstimate, SessionCheckpoint,
    TurnRuntime,
};
use serde_json::{Value, json};

/// Drives a future to completion on this thread.
pub fn block<F: Future>(future: F) -> F::Output {
    futures::executor::block_on(future)
}

/// A response computed from the request that asked for it.
pub type Answer = Box<dyn Fn(&ChatRequest) -> Vec<LlmEvent>>;

/// One scripted response, fixed or computed from the request that asked for it.
pub enum Step {
    Fixed(Vec<LlmEvent>),
    Computed(Answer),
}

/// What the model was asked and what it is to answer.
pub struct Shared {
    script: RefCell<VecDeque<Step>>,
    sent: RefCell<Vec<ChatRequest>>,
    digests: RefCell<Vec<Digest>>,
    supported: Cell<bool>,
    echoes: Cell<usize>,
}

/// The test's handle on the model.
pub type Model = Rc<Shared>;

pub trait ModelExt {
    fn new(script: Vec<Vec<LlmEvent>>) -> Self;
    fn push(&self, events: Vec<LlmEvent>);
    fn push_with(&self, step: impl Fn(&ChatRequest) -> Vec<LlmEvent> + 'static);
    fn sent(&self) -> Vec<ChatRequest>;
    fn digests(&self) -> Vec<Digest>;
    fn set_supported(&self, supported: bool);
    fn echoes(&self) -> usize;
}

impl ModelExt for Model {
    fn new(script: Vec<Vec<LlmEvent>>) -> Self {
        Self::from(Shared {
            script: RefCell::new(script.into_iter().map(Step::Fixed).collect()),
            sent: RefCell::new(Vec::new()),
            digests: RefCell::new(Vec::new()),
            supported: Cell::new(true),
            echoes: Cell::new(0),
        })
    }
    fn push(&self, events: Vec<LlmEvent>) {
        self.script.borrow_mut().push_back(Step::Fixed(events));
    }
    fn push_with(&self, step: impl Fn(&ChatRequest) -> Vec<LlmEvent> + 'static) {
        self.script
            .borrow_mut()
            .push_back(Step::Computed(Box::new(step)));
    }
    fn sent(&self) -> Vec<ChatRequest> {
        self.sent.borrow().clone()
    }
    fn digests(&self) -> Vec<Digest> {
        self.digests.borrow().clone()
    }
    fn set_supported(&self, supported: bool) {
        self.supported.set(supported);
    }
    fn echoes(&self) -> usize {
        self.echoes.get()
    }
}

impl Shared {
    fn answer(&self, request: &ChatRequest) -> LlmStream {
        self.sent.borrow_mut().push(request.clone());
        let step = self.script.borrow_mut().pop_front();
        let events = match step {
            Some(Step::Fixed(events)) => events,
            Some(Step::Computed(step)) => step(request),
            None => text("(script exhausted)"),
        };
        Box::pin(futures::stream::iter(events))
    }
}

struct Port(Model);

impl LlmPort for Port {
    fn model(&self) -> &'static str {
        "m"
    }
    fn capabilities(&self, _: &str) -> ModelCapabilities {
        ModelCapabilities {
            max_output_tokens: Some(8_192),
            ..ModelCapabilities::default()
        }
    }
    fn stream_chat(&self, request: ChatRequest) -> LlmStream {
        self.0.answer(&request)
    }
    fn managed_support(&self, _: &str) -> ManagedSupport {
        if self.0.supported.get() {
            ManagedSupport::Supported { policy_version: 1 }
        } else {
            ManagedSupport::Unsupported
        }
    }
    fn prepare_managed(&self, request: ManagedRequest) -> LlmResult<Box<dyn PreparedModelCall>> {
        if !self.0.supported.get() {
            return Err(nanus_ports::LlmError::Unsupported {
                feature: "managed".into(),
            });
        }
        let body = serde_json::to_vec(&json!({
            "model": request.request.model,
            "messages": request.request.messages,
            "tools": request.request.tools,
            "max_tokens": request.request.max_tokens,
        }))
        .unwrap();
        let estimate = RequestEstimate {
            input_tokens: u32::try_from(body.len().div_ceil(4)).unwrap(),
            request_bytes: body.len(),
            images: 0,
            reservation: request.request.max_tokens.unwrap_or(0),
        };
        Ok(Box::new(Call {
            model: Rc::clone(&self.0),
            digest: Digest::of(&body),
            selection: SelectionIdentity {
                provider: "test".into(),
                endpoint_digest: Digest::of(b"local"),
                protocol: "test.chat".into(),
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
    model: Model,
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
        self.model.digests.borrow_mut().push(self.digest.clone());
        self.model.answer(&self.request)
    }
}

/// A clock that never moves.
struct Clock;

impl nanus_ports::ClockPort for Clock {
    fn now_ms(&self) -> u64 {
        1_700_000_000_000
    }
}

/// The `echo` tool: returns `size` bytes of filler, and counts its runs.
struct Echo(Model);

impl nanus_domain::ToolExecutor for Echo {
    fn execute(&self, call: ToolCall) -> nanus_domain::ToolFuture {
        self.0.echoes.set(self.0.echoes.get().saturating_add(1));
        let size = call.arguments["size"].as_u64().unwrap_or(1);
        Box::pin(async move {
            let filler = "x".repeat(usize::try_from(size).unwrap());
            ToolResult::new(
                call.id,
                ToolOutcome::success_with(Value::Null, vec![ContentBlock::Text(filler)]),
            )
        })
    }
}

/// A runner over the scripted model with one registered tool, `echo`.
pub fn runner(model: &Model, budget: u32) -> AgentRunner {
    let mut registry = ToolRegistry::new();
    let schema = ToolSchema {
        name: ToolName::new("echo").unwrap(),
        description: "Echo filler".into(),
        parameters: json!({"type": "object"}),
    };
    registry
        .register(ToolDefinition::new(schema, Echo(Rc::clone(model))))
        .unwrap();
    runner_with(model, ToolRegistryHandle::new(registry), budget)
}

/// A runner over the scripted model and the given tools.
pub fn runner_with(model: &Model, tools: ToolRegistryHandle, budget: u32) -> AgentRunner {
    let config = AgentConfig::new(32, 1, "m", 32_768)
        .unwrap()
        .with_context_budget(budget)
        .unwrap()
        .with_sandbox(SandboxMode::DangerFullAccess);
    AgentRunner::new(
        Rc::new(Box::new(Port(Rc::clone(model)))),
        tools,
        "you are a test",
        config,
        Rc::new(Box::new(Clock)),
    )
    .unwrap()
}

/// A fresh session.
pub fn session() -> Session {
    Session::new(SessionId::new("s"), 0, "/w")
}

/// A context runtime with a fixed cursor key and no archive.
pub fn context() -> SessionContext {
    SessionContext::with_key(None, [7; 32])
}

/// The host lent to a turn.
pub fn host<'a>(disk: &'a Disk, context: &'a SessionContext) -> TurnHost<'a> {
    TurnHost {
        approver: None,
        control: None,
        runtime: TurnRuntime {
            context: Some(context),
            checkpoint: Some(disk),
        },
    }
}

/// A session with managed context enabled, its disk and its context.
pub fn managed(runner: &AgentRunner) -> (Session, Disk, SessionContext) {
    let mut session = session();
    let disk = Disk::new();
    let context = context();
    block(runner.set_context_policy(
        &mut session,
        ContextPolicy::managed(),
        ModeActor::Human,
        host(&disk, &context).runtime,
    ))
    .unwrap();
    (session, disk, context)
}

/// How a scripted commit fails.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    Refuse,
    Lose,
}

/// An in-memory disk that checks expected identities as a store does.
pub struct Disk {
    stored: RefCell<Option<Session>>,
    expected: RefCell<ExpectedCheckpoint>,
    commits: Cell<usize>,
    fail_at: Cell<Option<(usize, Failure)>>,
    /// Whether a lost commit actually reached the disk.
    lost_landed: Cell<bool>,
}

impl Disk {
    pub fn new() -> Self {
        Self {
            stored: RefCell::new(None),
            expected: RefCell::new(ExpectedCheckpoint::Absent),
            commits: Cell::new(0),
            fail_at: Cell::new(None),
            lost_landed: Cell::new(false),
        }
    }
    /// Makes the `index`th commit from now fail.
    pub fn fail(&self, after: usize, failure: Failure, landed: bool) {
        self.fail_at
            .set(Some((self.commits.get().saturating_add(after), failure)));
        self.lost_landed.set(landed);
    }
    pub fn commits(&self) -> usize {
        self.commits.get()
    }
    pub fn last(&self) -> Session {
        self.stored.borrow().clone().unwrap()
    }
    fn write(&self, candidate: &Session) -> CheckpointReceipt {
        let revision = ManagedState::fold(candidate.log()).map_or(0, |state| state.revision());
        let receipt = CheckpointReceipt {
            frontier: state::frontier(candidate, revision).unwrap(),
            body_digest: candidate.body_digest(),
            durability: Durability::ProcessCrash,
        };
        *self.stored.borrow_mut() = Some(candidate.clone());
        *self.expected.borrow_mut() = ExpectedCheckpoint::after(&receipt, candidate.body_version());
        receipt
    }
}

impl SessionCheckpoint for Disk {
    fn expected(&self) -> ExpectedCheckpoint {
        self.expected.borrow().clone()
    }
    fn commit<'a>(
        &'a self,
        view: CheckpointView<'a>,
    ) -> LocalBoxFuture<'a, Result<CheckpointReceipt, CheckpointError>> {
        Box::pin(async move {
            assert_eq!(
                view.expected,
                &*self.expected.borrow(),
                "commits name what they replace"
            );
            let index = self.commits.get();
            self.commits.set(index.saturating_add(1));
            if let Some((at, failure)) = self.fail_at.get()
                && at == index
            {
                self.fail_at.set(None);
                return Err(match failure {
                    Failure::Refuse => CheckpointError::NotCommitted(
                        nanus_domain::context::managed::ErrorCode::StorageCapacity,
                    ),
                    Failure::Lose => {
                        if self.lost_landed.get() {
                            *self.stored.borrow_mut() = Some(view.candidate.clone());
                        }
                        CheckpointError::CommitOutcomeUnknown(
                            nanus_domain::context::managed::ErrorCode::CheckpointUnknown,
                        )
                    }
                });
            }
            Ok(self.write(view.candidate))
        })
    }
    fn reconcile<'a>(
        &'a self,
        candidate_sha256: &'a Digest,
    ) -> LocalBoxFuture<'a, Result<Reconciled, CheckpointError>> {
        Box::pin(async move {
            let stored = self.stored.borrow().clone();
            let digest = stored.as_ref().map(|session| {
                session
                    .prefix_digest(u64::try_from(session.event_count()).unwrap())
                    .unwrap()
            });
            if digest.as_ref() == Some(candidate_sha256) {
                let session = stored.unwrap();
                self.write(&session);
                return Ok(Reconciled::Installed(self.expected.borrow().clone()));
            }
            Ok(Reconciled::Quarantined)
        })
    }
}

/// A final answer.
pub fn text(answer: &str) -> Vec<LlmEvent> {
    vec![
        LlmEvent::TextDelta(answer.to_owned()),
        LlmEvent::Finished {
            reason: FinishReason::Stop,
        },
    ]
}

/// One tool call.
pub fn call(id: &str, tool: &str, arguments: &str) -> Vec<LlmEvent> {
    calls(&[(id, tool, arguments)])
}

/// Several tool calls in one message.
pub fn calls(list: &[(&str, &str, &str)]) -> Vec<LlmEvent> {
    let mut events: Vec<LlmEvent> = list
        .iter()
        .enumerate()
        .map(|(index, (id, tool, arguments))| LlmEvent::ToolCallDelta {
            index: u32::try_from(index).unwrap(),
            id: Some(ToolCallId::new(*id)),
            name: Some(ToolName::new(*tool).unwrap()),
            arguments_delta: (*arguments).to_owned(),
        })
        .collect();
    events.push(LlmEvent::Finished {
        reason: FinishReason::ToolCalls,
    });
    events
}

/// An inspect call's arguments.
pub const INSPECT: &str = r#"{"action": "inspect", "base_revision": null, "base_frontier": null,
    "hide": [], "restore": [], "notes": [], "cursor": null, "base_profile_digest": null}"#;

/// The JSON of the last tool result in a request.
pub fn last_tool_json(request: &ChatRequest) -> Value {
    let content = request
        .messages
        .iter()
        .rev()
        .find_map(|message| match message {
            nanus_domain::Message::Tool { content, .. } => Some(content.clone()),
            _ => None,
        })
        .unwrap();
    serde_json::from_str(content.trim()).unwrap()
}

/// A proposal built from an inspect result, hiding `hide`.
pub fn propose_from(inspected: &Value, id: &str, hide: &[&str]) -> Vec<LlmEvent> {
    let context = &inspected["context"];
    let arguments = json!({
        "action": "propose",
        "base_revision": context["revision"],
        "base_frontier": context["frontier"],
        "hide": hide,
        "restore": [],
        "notes": [],
        "cursor": null,
        "base_profile_digest": context["profile_digest"],
    });
    call(id, "context_manage", &arguments.to_string())
}

/// The ids of the unprotected, unhidden fragments an inspect result lists, oldest first.
pub fn eligible(inspected: &Value) -> Vec<String> {
    inspected["fragments"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|fragment| fragment["protected"] == false && fragment["hidden"] == false)
        .map(|fragment| fragment["id"].as_str().unwrap().to_owned())
        .collect()
}

/// A proposal hiding the oldest eligible fragment the latest inspect listed.
pub fn propose_oldest(id: &'static str) -> impl Fn(&ChatRequest) -> Vec<LlmEvent> {
    move |request| {
        let inspected = last_tool_json(request);
        let oldest = eligible(&inspected).into_iter().next().unwrap();
        propose_from(&inspected, id, &[oldest.as_str()])
    }
}

/// The rendered text and error flag of the result answering `id`.
pub fn tool_result(session: &Session, id: &str) -> (String, bool) {
    session
        .log()
        .events()
        .iter()
        .find_map(|event| match event {
            SessionEvent::ToolResult {
                call_id,
                content,
                is_error,
                ..
            } if call_id.as_str() == id => Some((content.clone(), *is_error)),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no result for {id}"))
}

/// The JSON of the result answering `id`.
pub fn tool_result_json(session: &Session, id: &str) -> Value {
    serde_json::from_str(tool_result(session, id).0.trim()).unwrap()
}

/// Every attempt record in a phase.
pub fn attempts(session: &Session, phase: AttemptPhase) -> Vec<RequestAttemptRecord> {
    session
        .log()
        .events()
        .iter()
        .filter_map(|event| match event {
            SessionEvent::RequestAttempt { payload } if payload.phase == phase => {
                Some((**payload).clone())
            }
            _ => None,
        })
        .collect()
}
