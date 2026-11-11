//! Auditing a recorded session's cache reads: sessions recorded by the real runner, saved through
//! the real store, read back and prepared again.
//!
//! The model double encodes the way every adapter does — canonically, then decorated with the tail
//! and the retention — so a request's digest covers its tail and its cache settings, and its
//! canonical encoding is the body without them. It reports whatever usage the script gives it,
//! which is how a reported zero, an unreported read and a full read reach the records. The audit
//! is given a runner built exactly as the recording one was, unless a case says otherwise.

// A panic in a test *is* the assertion. The workspace denies the lint for production code.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

use nanus_adapter_store::JsonlStore;
use nanus_bundle::cache_audit::{
    self, Attribution, AuditRoutes, Basis, CacheAudit, CacheRead, RequestKind, Verification,
};
use nanus_bundle::{
    AgentRunner, SessionContext, Silent, StoreCheckpoint, ToolRegistryHandle, TurnHost,
};
use nanus_domain::context::managed::{
    AttemptPhase, ContextPolicy, Digest, ModeActor, SelectionIdentity, limits,
};
use nanus_domain::{
    AgentConfig, ContentBlock, SandboxMode, Session, SessionEvent, SessionId, ToolCall, ToolCallId,
    ToolDefinition, ToolName, ToolOutcome, ToolRegistry, ToolResult, ToolSchema, Usage,
};
use nanus_ports::{
    CanonicalEncoding, ChatRequest, ClockPort, FinishReason, LlmEvent, LlmPort, LlmResult,
    LlmStream, ManagedRequest, ManagedSupport, ModelCapabilities, PreparedModelCall,
    PromptCacheRetention, RequestEstimate, StoreHandle, TurnRuntime,
};
use serde_json::{Value, json};

/// The instant the test clock starts at.
const EPOCH_MS: u64 = 1_700_000_000_000;

/// One scripted response, computed from the request that asks for it.
type Step = Box<dyn Fn(&ChatRequest) -> Vec<LlmEvent>>;

/// What the scripted model answers, and the canonical encoding of every request it was sent.
#[derive(Default)]
struct Script {
    steps: RefCell<VecDeque<Step>>,
    sent: RefCell<Vec<CanonicalEncoding>>,
}

type Model = Rc<Script>;

/// The body a request is sent as: its canonical part, then the tail as a trailing item and the
/// retention and cache key beside it, as decoration adds them.
fn body(request: &ChatRequest) -> Value {
    let mut messages = serde_json::to_value(&request.messages).unwrap();
    if let Some(tail) = &request.context_tail {
        messages
            .as_array_mut()
            .unwrap()
            .push(json!({ "tail": tail }));
    }
    let retention = match request.prompt_cache {
        PromptCacheRetention::None => Value::Null,
        PromptCacheRetention::Short => json!("short"),
        PromptCacheRetention::Long => json!("long"),
    };
    json!({
        "model": request.model,
        "tools": request.tools,
        "max_tokens": request.max_tokens,
        "effort": request.reasoning_effort.map(nanus_ports::ReasoningEffort::as_str),
        "retention": retention,
        "key": request.prompt_cache_key,
        "messages": messages,
    })
}

fn canonical(request: &ChatRequest) -> CanonicalEncoding {
    CanonicalEncoding::from_body(&body(&request.undecorated()), "messages")
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
        self.0.sent.borrow_mut().push(canonical(&request));
        answer(&self.0, &request)
    }
    fn managed_support(&self, _: &str) -> ManagedSupport {
        ManagedSupport::Supported {
            policy_version: limits::POLICY_VERSION,
            layout_version: limits::LAYOUT_VERSION,
        }
    }
    fn prompt_cache_lifetime_ms(&self, _: &str, retention: PromptCacheRetention) -> Option<u64> {
        match retention {
            PromptCacheRetention::None => None,
            PromptCacheRetention::Short => Some(300_000),
            PromptCacheRetention::Long => Some(3_600_000),
        }
    }
    fn canonical_encoding(&self, request: &ChatRequest) -> Option<CanonicalEncoding> {
        Some(canonical(request))
    }
    fn prepare_managed(&self, managed: ManagedRequest) -> LlmResult<Box<dyn PreparedModelCall>> {
        let encoded = serde_json::to_vec(&body(&managed.request)).unwrap();
        let estimate = RequestEstimate {
            input_tokens: u32::try_from(encoded.len().div_ceil(4)).unwrap(),
            request_bytes: encoded.len(),
            images: 0,
            reservation: managed.request.max_tokens.unwrap_or(0),
        };
        Ok(Box::new(Call {
            model: Rc::clone(&self.0),
            request: managed.request.clone(),
            canonical: canonical(&managed.request),
            digest: Digest::of(&encoded),
            selection: SelectionIdentity {
                provider: "test".into(),
                endpoint_digest: Digest::of(b"local"),
                protocol: "test.chat".into(),
                model: managed.request.model.clone(),
                effort: None,
                epoch: managed.selection_epoch,
            },
            estimate,
        }))
    }
}

fn answer(model: &Model, request: &ChatRequest) -> LlmStream {
    let step = model.steps.borrow_mut().pop_front();
    let events = step.map_or_else(|| text("(exhausted)"), |step| step(request));
    Box::pin(futures::stream::iter(events))
}

struct Call {
    model: Model,
    request: ChatRequest,
    canonical: CanonicalEncoding,
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
        self.model.sent.borrow_mut().push(self.canonical);
        answer(&self.model, &self.request)
    }
}

/// The `echo` tool: `size` bytes of filler.
struct Echo;

impl nanus_domain::ToolExecutor for Echo {
    fn execute(&self, call: ToolCall) -> nanus_domain::ToolFuture {
        let size = usize::try_from(call.arguments["size"].as_u64().unwrap_or(1)).unwrap();
        Box::pin(async move {
            let filler = "x".repeat(size);
            ToolResult::new(
                call.id,
                ToolOutcome::success_with(Value::Null, vec![ContentBlock::Text(filler)]),
            )
        })
    }
}

/// A clock the test moves.
struct Moving(Rc<Cell<u64>>);

impl ClockPort for Moving {
    fn now_ms(&self) -> u64 {
        self.0.get()
    }
}

/// A runner over the scripted model with `echo`, a 100,000-token budget and the short retention
/// `nanus run` asks for, its system prompt `prompt`.
fn runner(model: &Model, time: &Rc<Cell<u64>>, prompt: &str) -> AgentRunner {
    let mut registry = ToolRegistry::new();
    let schema = ToolSchema {
        name: ToolName::new("echo").unwrap(),
        description: "Echo filler".into(),
        parameters: json!({ "type": "object" }),
    };
    registry
        .register(ToolDefinition::new(schema, Echo))
        .unwrap();
    let config = AgentConfig::new(32, 1, "m", 32_768)
        .unwrap()
        .with_context_budget(100_000)
        .unwrap()
        .with_sandbox(SandboxMode::DangerFullAccess);
    AgentRunner::new(
        Rc::new(Box::new(Port(Rc::clone(model)))),
        ToolRegistryHandle::new(registry),
        prompt,
        config,
        Rc::new(Box::new(Moving(Rc::clone(time)))),
    )
    .unwrap()
    .with_prompt_cache(PromptCacheRetention::Short)
    .with_context_hide_break_even(false)
}

/// The routes an audit uses: a runner built as the recording one was, with `prompt`.
struct Routes(String);

impl AuditRoutes for Routes {
    fn managed(&self, _: &SelectionIdentity) -> Result<Vec<AgentRunner>, String> {
        Ok(vec![runner(
            &Model::default(),
            &Rc::new(Cell::new(EPOCH_MS)),
            &self.0,
        )])
    }
    fn legacy(&self, _: Option<&str>) -> Result<Vec<AgentRunner>, String> {
        self.managed(&identity())
    }
}

fn identity() -> SelectionIdentity {
    SelectionIdentity {
        provider: "test".into(),
        endpoint_digest: Digest::of(b"local"),
        protocol: "test.chat".into(),
        model: "m".into(),
        effort: None,
        epoch: 0,
    }
}

const PROMPT: &str = "you are a test";

/// The provider's usage report: `prompt` tokens, `read` of them from cache when it said so.
fn usage(prompt: u32, read: Option<u32>) -> LlmEvent {
    let hit = read.unwrap_or(0);
    LlmEvent::Usage(
        Usage::new(prompt, 5, 0, hit, prompt.saturating_sub(hit))
            .with_cache_hit_reported(read.is_some()),
    )
}

fn text(answer: &str) -> Vec<LlmEvent> {
    vec![
        LlmEvent::TextDelta(answer.to_owned()),
        LlmEvent::Finished {
            reason: FinishReason::Stop,
        },
    ]
}

fn call(id: &str, tool: &str, arguments: &str) -> Vec<LlmEvent> {
    vec![
        LlmEvent::ToolCallDelta {
            index: 0,
            id: Some(ToolCallId::new(id)),
            name: Some(ToolName::new(tool).unwrap()),
            arguments_delta: arguments.to_owned(),
        },
        LlmEvent::Finished {
            reason: FinishReason::ToolCalls,
        },
    ]
}

/// A scripted step with its usage report placed before its end.
fn reported(mut events: Vec<LlmEvent>, prompt: u32, read: Option<u32>) -> Vec<LlmEvent> {
    let at = events.len().saturating_sub(1);
    events.insert(at, usage(prompt, read));
    events
}

/// One recorded session in a temporary store, and the time its clock reads.
struct Recording {
    _home: tempfile::TempDir,
    store: StoreHandle,
    model: Model,
    time: Rc<Cell<u64>>,
    session: Session,
}

impl Recording {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let store = nanus_kernel::runtime::block_on(JsonlStore::new(home.path()))
            .unwrap()
            .handle();
        Self {
            _home: home,
            store,
            model: Model::default(),
            time: Rc::new(Cell::new(EPOCH_MS)),
            session: Session::new(SessionId::new("audited"), EPOCH_MS, "/w"),
        }
    }

    fn script(&self, steps: Vec<Vec<LlmEvent>>) {
        for events in steps {
            self.model
                .steps
                .borrow_mut()
                .push_back(Box::new(move |_: &ChatRequest| events.clone()));
        }
    }

    fn script_with(&self, step: impl Fn(&ChatRequest) -> Vec<LlmEvent> + 'static) {
        self.model.steps.borrow_mut().push_back(Box::new(step));
    }

    /// Runs one managed turn, checkpointed through the store, enabling managed context first.
    fn managed_turn(&mut self, prompt: &str) {
        let runner = runner(&self.model, &self.time, PROMPT);
        nanus_kernel::runtime::block_on_local(async {
            self.store.lock(self.session.id(), "test").await.unwrap();
            let checkpoint = StoreCheckpoint::bind(self.store.clone(), self.session.id().clone())
                .await
                .unwrap();
            let context = SessionContext::with_key(Some(self.store.clone()), [3; 32]);
            let runtime = TurnRuntime {
                context: Some(&context),
                checkpoint: Some(&checkpoint),
            };
            if !self.session.is_managed_body() {
                runner
                    .set_context_policy(
                        &mut self.session,
                        ContextPolicy::managed(),
                        ModeActor::Human,
                        runtime,
                    )
                    .await
                    .unwrap();
            }
            let host = TurnHost {
                approver: None,
                control: None,
                runtime,
            };
            let run = runner
                .run_turn_with_runtime(&mut self.session, prompt, &mut Silent, host)
                .await;
            assert!(run.outcome.is_ok(), "{:?}", run.outcome);
            self.store.release_lock(self.session.id());
        });
    }

    /// Runs one legacy turn and saves the session, as `nanus run` does.
    fn legacy_turn(&mut self, prompt: &str) {
        let runner = runner(&self.model, &self.time, PROMPT);
        nanus_kernel::runtime::block_on_local(async {
            runner
                .run_turn(&mut self.session, prompt, &mut Silent, None)
                .await
                .unwrap();
            self.store.save(&self.session).await.unwrap();
            // Read back, as `nanus run --resume` reads a session before it continues one.
            self.session = self.store.load(self.session.id()).await.unwrap();
        });
    }

    /// The session as the store holds it, audited with a runner whose system prompt is `prompt`.
    fn audit_with(&self, prompt: &str) -> CacheAudit {
        let stored = nanus_kernel::runtime::block_on(self.store.load(self.session.id())).unwrap();
        // The live usage's "reported" flag is not serialised, so the events are compared by count:
        // the audit reads exactly what the store holds.
        assert_eq!(
            stored.event_count(),
            self.session.event_count(),
            "the store holds every recorded event"
        );
        cache_audit::audit(&stored, &Routes(prompt.to_owned()))
    }

    fn audit(&self) -> CacheAudit {
        self.audit_with(PROMPT)
    }

    /// The canonical encodings the model was sent, in order.
    fn sent(&self) -> Vec<CanonicalEncoding> {
        self.model.sent.borrow().clone()
    }
}

fn classes(audit: &CacheAudit) -> Vec<&'static str> {
    audit
        .requests
        .iter()
        .map(|request| cache_audit::class_of(&request.attribution))
        .collect()
}

fn digest_verified(audit: &CacheAudit) -> bool {
    audit
        .requests
        .iter()
        .all(|request| matches!(request.verification, Verification::Digest { .. }))
}

#[test]
fn a_stable_three_step_turn_is_digest_verified_and_a_hit_after_the_first() {
    let mut recording = Recording::new();
    recording.script(vec![
        reported(call("c0", "echo", r#"{"size": 40}"#), 1_000, Some(0)),
        reported(call("c1", "echo", r#"{"size": 40}"#), 1_100, Some(1_000)),
        reported(text("done"), 1_200, Some(1_100)),
    ]);
    recording.managed_turn("the task");
    let audit = recording.audit();
    assert_eq!(audit.requests.len(), 3);
    assert!(digest_verified(&audit), "{audit:#?}");
    assert_eq!(classes(&audit), ["first", "hit", "hit"]);
    assert_eq!(
        audit.requests[1].attribution,
        Attribution::Hit {
            basis: Basis::Digest
        }
    );
    // The canonical encodings the audit compared are the ones that were sent.
    let sent = recording.sent();
    assert_eq!(sent[0].divergence(&sent[1]), None);
    assert_eq!(sent[1].divergence(&sent[2]), None);
    assert_eq!(audit.summary.digest_verified, 3);
    assert_eq!(audit.summary.read_percent, Some(63));
}

#[test]
fn a_request_this_configuration_cannot_prepare_again_is_unverified_not_guessed() {
    let mut recording = Recording::new();
    recording.script(vec![
        reported(call("c0", "echo", r#"{"size": 40}"#), 1_000, Some(0)),
        reported(text("done"), 1_100, Some(0)),
    ]);
    recording.managed_turn("the task");
    // A runner whose system prompt is not the recording one prepares other bytes.
    let audit = recording.audit_with("you are another test");
    assert_eq!(classes(&audit), ["unverified", "unverified"]);
    for request in &audit.requests {
        let Verification::Unverified { reason } = &request.verification else {
            panic!("claimed without a digest: {request:?}");
        };
        assert!(
            reason.contains("reproduces the recorded digest"),
            "{reason}"
        );
    }
    assert_eq!(audit.summary.digest_verified, 0);
    // The recording's own configuration verifies the same records.
    assert!(digest_verified(&recording.audit()));
}

#[test]
fn a_tampered_digest_is_unverified_and_the_request_after_it_is_not_compared() {
    let mut recording = Recording::new();
    recording.script(vec![
        reported(call("c0", "echo", r#"{"size": 40}"#), 1_000, Some(0)),
        reported(call("c1", "echo", r#"{"size": 40}"#), 1_100, Some(0)),
        reported(text("done"), 1_200, Some(1_100)),
    ]);
    recording.managed_turn("the task");
    let mut tampered = recording.session.prefix(0).unwrap();
    let mut seen = 0;
    for event in recording.session.log().events() {
        let mut event = event.clone();
        if let SessionEvent::RequestAttempt { payload } = &mut event
            && payload.phase == AttemptPhase::Started
        {
            if seen == 1 {
                payload.request_digest = Digest::of(b"another body");
            }
            seen += 1;
        }
        tampered.append(event);
    }
    let audit = cache_audit::audit(&tampered, &Routes(PROMPT.to_owned()));
    assert_eq!(classes(&audit), ["first", "unverified", "unverified"]);
    let Attribution::Unverified { reason } = &audit.requests[2].attribution else {
        panic!("{audit:#?}");
    };
    assert!(reason.contains("nothing proven precedes it"), "{reason}");
    assert!(matches!(
        audit.requests[2].verification,
        Verification::Digest { .. }
    ));
}

/// The JSON of the newest tool result in `request`.
fn last_tool_json(request: &ChatRequest) -> Value {
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

/// A proposal, from the inspect result `request` ends with, hiding the oldest eligible fragment.
fn propose_oldest(request: &ChatRequest) -> Vec<LlmEvent> {
    let inspected = last_tool_json(request);
    let oldest = inspected["fragments"]
        .as_array()
        .unwrap()
        .iter()
        .find(|fragment| fragment["protected"] == false && fragment["hidden"] == false)
        .map(|fragment| fragment["id"].clone())
        .unwrap();
    let context = &inspected["context"];
    let arguments = json!({
        "action": "propose",
        "base_revision": context["revision"],
        "base_frontier": context["frontier"],
        "hide": [oldest],
        "restore": [],
        "notes": [],
        "cursor": null,
        "base_profile_digest": context["profile_digest"],
    });
    reported(
        call("p1", "context_manage", &arguments.to_string()),
        2_400,
        Some(2_300),
    )
}

/// An inspect call's arguments.
const INSPECT: &str = r#"{"action": "inspect", "base_revision": null, "base_frontier": null,
    "hide": [], "restore": [], "notes": [], "cursor": null, "base_profile_digest": null}"#;

/// The unit of `sent` that holds the call `id`'s assistant message.
fn unit_of_call(sent: &CanonicalEncoding, id: &str) -> usize {
    sent.units()
        .iter()
        .position(|unit| unit.contains(&format!("\"{id}\"")) && unit.contains("tool_calls"))
        .unwrap()
}

#[test]
fn an_accepted_hide_is_a_prefix_change_inside_the_hidden_fragment() {
    let mut recording = Recording::new();
    recording.script(vec![
        reported(call("c0", "echo", r#"{"size": 3000}"#), 1_000, Some(0)),
        reported(call("c1", "echo", r#"{"size": 3000}"#), 2_000, Some(1_000)),
        reported(call("c2", "echo", r#"{"size": 10}"#), 2_100, Some(2_000)),
        reported(call("c3", "echo", r#"{"size": 10}"#), 2_200, Some(2_100)),
        reported(call("i1", "context_manage", INSPECT), 2_300, Some(2_200)),
    ]);
    recording.script_with(propose_oldest);
    recording.script(vec![reported(text("done"), 1_500, Some(300))]);
    recording.managed_turn("the task");
    let audit = recording.audit();
    assert!(digest_verified(&audit), "{audit:#?}");
    let found = classes(&audit);
    assert_eq!(
        found,
        ["first", "hit", "hit", "hit", "hit", "hit", "prefix_changed"]
    );
    let Attribution::PrefixChanged {
        offset,
        previous_bytes,
        causes,
        basis,
    } = &audit.requests[6].attribution
    else {
        panic!("{audit:#?}");
    };
    assert_eq!(*basis, Basis::Digest);
    assert_eq!(causes, &["revision 1: the model's proposal hid 1 fragment"]);
    let sent = recording.sent();
    let previous = &sent[5];
    assert_eq!(*previous_bytes, previous.byte_len());
    // The two part inside the hidden fragment's assistant message, the first thing it removed.
    let unit = unit_of_call(previous, "c0");
    assert!(*offset >= previous.offset_of(unit), "{offset}");
    assert!(*offset < previous.offset_of(unit + 1), "{offset}");
    assert_eq!(audit.requests[6].revision, Some(1));
}

#[test]
fn a_goal_created_extends_the_prefix_and_a_goal_changed_moves_the_working_data() {
    let mut recording = Recording::new();
    recording.script(vec![
        reported(call("c0", "echo", r#"{"size": 40}"#), 1_000, Some(0)),
        reported(
            call("g1", "create_goal", r#"{"objective": "ship the parser"}"#),
            1_100,
            Some(1_000),
        ),
        reported(call("c1", "echo", r#"{"size": 40}"#), 1_200, Some(1_100)),
        reported(
            call("g2", "update_goal", r#"{"objective": "ship the lexer"}"#),
            1_300,
            Some(1_200),
        ),
        reported(text("done"), 1_400, Some(300)),
    ]);
    recording.managed_turn("the task");
    let audit = recording.audit();
    assert!(digest_verified(&audit), "{audit:#?}");
    // Created, the working data is placed where the previous request ended: still a prefix.
    assert_eq!(
        classes(&audit),
        ["first", "hit", "hit", "hit", "prefix_changed"]
    );
    let Attribution::PrefixChanged { causes, .. } = &audit.requests[4].attribution else {
        panic!("{audit:#?}");
    };
    assert_eq!(causes, &["a goal change moved the working data"]);
}

#[test]
fn an_idle_gap_past_the_lifetime_with_a_reported_zero_is_an_expiry_and_within_it_is_not() {
    for (gap_ms, expired) in [(300_001, true), (300_000, false)] {
        let mut recording = Recording::new();
        recording.script(vec![
            reported(call("c0", "echo", r#"{"size": 40}"#), 10_000, Some(0)),
            reported(text("done"), 11_000, Some(10_000)),
        ]);
        recording.managed_turn("first");
        recording.time.set(EPOCH_MS + gap_ms);
        recording.script(vec![reported(text("again"), 12_000, Some(0))]);
        recording.managed_turn("second");
        let audit = recording.audit();
        assert!(digest_verified(&audit), "{audit:#?}");
        let resumed = &audit.requests[2];
        assert_eq!(resumed.cache_read, CacheRead::Reported { tokens: 0 });
        assert_eq!(resumed.idle_ms, Some(gap_ms));
        assert_eq!(
            resumed.lifetime_ms,
            Some(300_000),
            "the short retention's lifetime"
        );
        assert_eq!(
            resumed.attribution,
            Attribution::ProviderMiss {
                expired: Some(expired),
                basis: Basis::Digest
            },
            "a gap of {gap_ms} ms"
        );
    }
}

#[test]
fn a_reported_zero_and_an_unreported_read_are_recorded_apart() {
    let mut recording = Recording::new();
    recording.script(vec![
        reported(call("c0", "echo", r#"{"size": 40}"#), 10_000, Some(0)),
        reported(call("c1", "echo", r#"{"size": 40}"#), 11_000, Some(0)),
        reported(text("done"), 12_000, None),
    ]);
    recording.managed_turn("the task");
    let audit = recording.audit();
    assert_eq!(
        audit.requests[1].cache_read,
        CacheRead::Reported { tokens: 0 }
    );
    assert_eq!(audit.requests[2].cache_read, CacheRead::NotReported);
    // A zero the provider stated is a miss; a read it never stated cannot be compared.
    assert_eq!(
        cache_audit::class_of(&audit.requests[1].attribution),
        "provider_miss_unexplained"
    );
    assert_eq!(
        cache_audit::class_of(&audit.requests[2].attribution),
        "stable_unmeasured"
    );
}

#[test]
fn a_legacy_session_is_rebuilt_and_labelled_and_enabling_managed_is_its_cause() {
    let mut recording = Recording::new();
    recording.script(vec![
        reported(call("c0", "echo", r#"{"size": 40}"#), 1_000, Some(0)),
        reported(text("done"), 1_100, Some(1_000)),
    ]);
    recording.legacy_turn("first");
    recording.script(vec![reported(text("again"), 1_200, Some(0))]);
    recording.managed_turn("second");
    let audit = recording.audit();
    let kinds: Vec<RequestKind> = audit.requests.iter().map(|request| request.kind).collect();
    assert_eq!(
        kinds,
        [
            RequestKind::Legacy,
            RequestKind::Legacy,
            RequestKind::Managed
        ]
    );
    assert_eq!(audit.requests[0].verification, Verification::Rebuilt);
    assert_eq!(
        audit.requests[0].idle_ms, None,
        "a legacy step records no times"
    );
    assert_eq!(
        audit.requests[1].attribution,
        Attribution::Hit {
            basis: Basis::Rebuilt
        }
    );
    // A legacy reply's usage always writes the read, so a zero there is a zero or no report.
    assert_eq!(audit.requests[0].cache_read, CacheRead::ZeroOrNotReported);
    let Attribution::PrefixChanged { causes, basis, .. } = &audit.requests[2].attribution else {
        panic!("{audit:#?}");
    };
    assert_eq!(
        *basis,
        Basis::Rebuilt,
        "it rests on a rebuilt legacy request"
    );
    assert_eq!(causes, &["the context policy changed (now managed)"]);
    assert_eq!(audit.summary.rebuilt, 2);
    assert_eq!(audit.summary.digest_verified, 1);
}

#[test]
fn the_json_form_round_trips() {
    let mut recording = Recording::new();
    recording.script(vec![
        reported(call("c0", "echo", r#"{"size": 40}"#), 1_000, Some(0)),
        reported(text("done"), 1_100, None),
    ]);
    recording.managed_turn("the task");
    let audit = recording.audit();
    let encoded = serde_json::to_string(&audit).unwrap();
    let decoded: CacheAudit = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded, audit);
    let value: Value = serde_json::from_str(&encoded).unwrap();
    assert_eq!(value["requests"][0]["attribution"]["class"], "first");
    assert_eq!(value["requests"][1]["cache_read"]["state"], "not_reported");
    assert_eq!(value["requests"][1]["verification"]["basis"], "digest");
}
