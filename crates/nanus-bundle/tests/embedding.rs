//! Caller-owned model, policy and cancellation without stock composition.

// Integration fixture helpers use panics only as assertions.
#![allow(clippy::unwrap_used)]
#![cfg(test)]

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::task::Poll;

use nanus_bundle::{AgentRunner, Approver, Silent, ToolRegistryHandle};
use nanus_domain::{
    AgentConfig, ApprovalOutcome, ApprovalPolicy, ApprovalRequest, Session, SessionEvent,
    SessionId, ToolAccess, ToolCall, ToolCallId, ToolDefinition, ToolExecutor, ToolFuture,
    ToolName, ToolRegistry, ToolResult, ToolSchema, TurnEndReason,
};
use nanus_ports::{
    ChatRequest, ClockPort, FinishReason, LlmEvent, LlmHandle, LlmPort, LlmStream, LocalBoxFuture,
    PolicyError, ToolPolicy, ToolPolicyDecision, TurnControl,
};
use serde_json::json;

#[derive(Default)]
struct Control {
    stopped: Cell<bool>,
    wake: tokio::sync::Notify,
}

impl Control {
    fn cancel(&self) {
        self.stopped.set(true);
        self.wake.notify_waiters();
    }
}

impl TurnControl for Control {
    fn is_cancelled(&self) -> bool {
        self.stopped.get()
    }

    fn cancelled(&self) -> LocalBoxFuture<'_, ()> {
        Box::pin(async {
            let wake = self.wake.notified();
            if !self.stopped.get() {
                wake.await;
            }
        })
    }
}

struct Clock;
impl ClockPort for Clock {
    fn now_ms(&self) -> u64 {
        123
    }
}

struct Model {
    calls: Cell<bool>,
    silent: bool,
}

impl LlmPort for Model {
    fn model(&self) -> &'static str {
        "host-model"
    }

    fn stream_chat(&self, _request: ChatRequest) -> LlmStream {
        if self.silent {
            return Box::pin(futures::stream::pending());
        }
        if self.calls.replace(false) {
            let events = (0_u32..3).map(|index| LlmEvent::ToolCallDelta {
                index,
                id: Some(ToolCallId::new(format!("call-{index}"))),
                name: Some(ToolName::new("host_tool").unwrap()),
                arguments_delta: json!({ "path": format!("file-{index}") }).to_string(),
            });
            Box::pin(futures::stream::iter(events))
        } else {
            Box::pin(futures::stream::iter([
                LlmEvent::TextDelta("saved answer".into()),
                LlmEvent::Finished {
                    reason: FinishReason::Stop,
                },
            ]))
        }
    }
}

#[derive(Default)]
struct Effects {
    started: RefCell<Vec<ToolCall>>,
    dropped: Cell<u32>,
    pending: bool,
}

struct DropWork(Rc<Effects>);
impl Drop for DropWork {
    fn drop(&mut self) {
        self.0.dropped.set(self.0.dropped.get().saturating_add(1));
    }
}

struct Executor(Rc<Effects>);
impl ToolExecutor for Executor {
    fn execute(&self, call: ToolCall) -> ToolFuture {
        self.0.started.borrow_mut().push(call.clone());
        let state = Rc::clone(&self.0);
        Box::pin(async move {
            let _guard = DropWork(Rc::clone(&state));
            if state.pending {
                futures::future::pending::<()>().await;
            }
            ToolResult::success(call.id, json!({ "done": true }))
        })
    }
}

struct Policy {
    seen: RefCell<Vec<(ToolCall, ToolAccess)>>,
    answer: Result<ToolPolicyDecision, PolicyError>,
    pending: bool,
}
impl ToolPolicy for Policy {
    fn decide<'a>(
        &'a self,
        call: &'a ToolCall,
        access: ToolAccess,
    ) -> LocalBoxFuture<'a, Result<ToolPolicyDecision, PolicyError>> {
        self.seen.borrow_mut().push((call.clone(), access));
        Box::pin(async {
            if self.pending {
                futures::future::pending::<()>().await;
            }
            self.answer.clone()
        })
    }
}

fn policy(answer: Result<ToolPolicyDecision, PolicyError>, pending: bool) -> Rc<Policy> {
    Rc::new(Policy {
        seen: RefCell::new(Vec::new()),
        answer,
        pending,
    })
}

fn runner(effects: &Rc<Effects>, access: ToolAccess, silent: bool) -> AgentRunner {
    let mut tools = ToolRegistry::new();
    tools
        .register(
            ToolDefinition::new(
                ToolSchema {
                    name: ToolName::new("host_tool").unwrap(),
                    description: "A caller-owned tool".into(),
                    parameters: json!({ "type": "object" }),
                },
                Executor(Rc::clone(effects)),
            )
            .with_access(access),
        )
        .unwrap();
    let llm: LlmHandle = Rc::new(Box::new(Model {
        calls: Cell::new(true),
        silent,
    }));
    AgentRunner::new(
        llm,
        ToolRegistryHandle::new(tools),
        "Caller skill text",
        AgentConfig::new(4, 2, "host-model", 16384).unwrap(),
        Rc::new(Box::new(Clock)),
    )
    .unwrap()
}

fn session() -> Session {
    Session::new(
        SessionId::new("embedded"),
        123,
        std::env::temp_dir().to_string_lossy(),
    )
}

#[tokio::test]
async fn host_policy_sees_exact_reads_and_denial_prevents_io() {
    let effects = Rc::new(Effects::default());
    let policy = policy(
        Ok(ToolPolicyDecision::Deny {
            reason: "host scope".into(),
        }),
        false,
    );
    let runner = runner(&effects, ToolAccess::Read, false).with_tool_policy(policy.clone());
    let mut session = session();
    assert!(
        runner
            .run_turn(&mut session, "inspect", &mut Silent, None)
            .await
            .unwrap()
            .is_success()
    );
    assert!(effects.started.borrow().is_empty());
    let seen = policy.seen.borrow();
    assert_eq!(seen.len(), 3);
    for (index, (call, access)) in seen.iter().enumerate() {
        assert_eq!(call.id.as_str(), format!("call-{index}"));
        assert_eq!(call.arguments, json!({ "path": format!("file-{index}") }));
        assert_eq!(*access, ToolAccess::Read);
    }
    assert_eq!(
        session
            .log()
            .events()
            .iter()
            .filter(|event| matches!(event,
        SessionEvent::ToolResult { content, is_error: true, .. } if content.contains("host scope")))
            .count(),
        3
    );
}

#[tokio::test]
async fn one_call_grant_does_not_change_default_approval() {
    struct Exact;
    impl ToolPolicy for Exact {
        fn decide<'a>(
            &'a self,
            call: &'a ToolCall,
            _access: ToolAccess,
        ) -> LocalBoxFuture<'a, Result<ToolPolicyDecision, PolicyError>> {
            Box::pin(async move {
                Ok(if call.id.as_str() == "call-1" {
                    ToolPolicyDecision::AllowOnce
                } else {
                    ToolPolicyDecision::UseDefault
                })
            })
        }
    }
    let effects = Rc::new(Effects::default());
    let runner = runner(&effects, ToolAccess::Write, false).with_tool_policy(Rc::new(Exact));
    runner
        .run_turn(&mut session(), "write", &mut Silent, None)
        .await
        .unwrap();
    assert_eq!(effects.started.borrow().len(), 1);
    assert_eq!(effects.started.borrow()[0].id.as_str(), "call-1");
    assert_eq!(runner.approval(), ApprovalPolicy::PerCall);
}

#[tokio::test]
async fn policy_delegation_preserves_approval_modes_and_errors_refuse_reads() {
    for approval in [
        ApprovalPolicy::PerCall,
        ApprovalPolicy::Permitted,
        ApprovalPolicy::AllCalls,
    ] {
        let effects = Rc::new(Effects::default());
        let policy = policy(Ok(ToolPolicyDecision::UseDefault), false);
        let runner = runner(&effects, ToolAccess::Write, false).with_tool_policy(policy);
        runner.set_approval(approval);
        runner
            .run_turn(&mut session(), "write", &mut Silent, None)
            .await
            .unwrap();
        assert_eq!(
            effects.started.borrow().len(),
            if approval == ApprovalPolicy::PerCall {
                0
            } else {
                3
            }
        );
    }
    let effects = Rc::new(Effects::default());
    let policy = policy(
        Err(PolicyError {
            message: "UI unavailable".into(),
        }),
        false,
    );
    runner(&effects, ToolAccess::Read, false)
        .with_tool_policy(policy)
        .run_turn(&mut session(), "read", &mut Silent, None)
        .await
        .unwrap();
    assert!(effects.started.borrow().is_empty());
}

struct PendingApprover;
impl Approver for PendingApprover {
    fn decide(&self, _request: ApprovalRequest) -> LocalBoxFuture<'_, ApprovalOutcome> {
        Box::pin(futures::future::pending())
    }
}

#[tokio::test]
async fn cancellation_wakes_silent_model_policy_approval_and_tools_and_settles_calls() {
    for boundary in ["model", "policy", "approval", "tools"] {
        let effects = Rc::new(Effects {
            pending: boundary == "tools",
            ..Effects::default()
        });
        let mut runner = runner(&effects, ToolAccess::Write, boundary == "model");
        let policy = policy(Ok(ToolPolicyDecision::AllowOnce), boundary == "policy");
        if boundary != "approval" {
            runner = runner.with_tool_policy(policy.clone());
        }
        let control = Control::default();
        let mut session = session();
        let mut progress = Silent;
        let mut turn = Box::pin(runner.run_turn_with_control(
            &mut session,
            "work",
            &mut progress,
            Some(&PendingApprover),
            &control,
        ));
        assert!(
            matches!(futures::poll!(turn.as_mut()), Poll::Pending),
            "{boundary}"
        );
        control.cancel();
        assert_eq!(
            turn.await.unwrap().reason,
            TurnEndReason::Interrupted,
            "{boundary}"
        );
        assert_eq!(
            session
                .log()
                .events()
                .iter()
                .filter(|event| matches!(event, SessionEvent::TurnEnd { .. }))
                .count(),
            1
        );
        assert_eq!(
            effects.started.borrow().len(),
            if boundary == "tools" { 2 } else { 0 }
        );
        assert_eq!(
            effects.dropped.get(),
            if boundary == "tools" { 2 } else { 0 }
        );
        let messages = session.derive_messages();
        let calls: usize = messages
            .iter()
            .map(|message| message.tool_calls().len())
            .sum();
        let results = messages
            .iter()
            .filter(|message| matches!(message, nanus_domain::Message::Tool { .. }))
            .count();
        assert_eq!(calls, results, "no orphaned calls after {boundary}");
        // A new control and turn can resume without replaying the interrupted effects.
        if boundary != "model" {
            let runs = effects.started.borrow().len();
            assert!(
                runner
                    .run_turn_with_control(
                        &mut session,
                        "resume",
                        &mut Silent,
                        None,
                        &Control::default()
                    )
                    .await
                    .unwrap()
                    .is_success()
            );
            assert_eq!(effects.started.borrow().len(), runs);
        }
    }
}

#[tokio::test]
async fn approval_racing_cancellation_never_dispatches_and_cancelled_controls_stay_stopped() {
    struct CancelAndGrant<'a>(&'a Control);
    impl Approver for CancelAndGrant<'_> {
        fn decide(&self, _request: ApprovalRequest) -> LocalBoxFuture<'_, ApprovalOutcome> {
            Box::pin(async {
                self.0.cancel();
                ApprovalOutcome::AllowedOnce
            })
        }
    }
    let effects = Rc::new(Effects::default());
    let runner = runner(&effects, ToolAccess::Write, false);
    let control = Control::default();
    let mut session = session();
    let outcome = runner
        .run_turn_with_control(
            &mut session,
            "work",
            &mut Silent,
            Some(&CancelAndGrant(&control)),
            &control,
        )
        .await
        .unwrap();
    assert_eq!(outcome.reason, TurnEndReason::Interrupted);
    assert!(effects.started.borrow().is_empty());
    assert_eq!(
        runner
            .run_turn_with_control(&mut session, "still stopped", &mut Silent, None, &control)
            .await
            .unwrap()
            .steps,
        0
    );
}

#[tokio::test]
async fn host_grants_cannot_bypass_unknown_tool_or_malformed_arguments() {
    struct InvalidModel {
        name: &'static str,
        raw: &'static str,
        issued: Cell<bool>,
    }
    impl LlmPort for InvalidModel {
        fn model(&self) -> &'static str {
            "host-model"
        }
        fn stream_chat(&self, _request: ChatRequest) -> LlmStream {
            let events = if self.issued.replace(true) {
                vec![LlmEvent::TextDelta("settled".into())]
            } else {
                vec![LlmEvent::ToolCallDelta {
                    index: 0,
                    id: Some(ToolCallId::new("invalid")),
                    name: Some(ToolName::new(self.name).unwrap()),
                    arguments_delta: self.raw.into(),
                }]
            };
            Box::pin(futures::stream::iter(events))
        }
    }
    for (name, raw) in [
        ("missing", "{}"),
        ("host_tool", "[]"),
        ("host_tool", "{broken"),
    ] {
        let effects = Rc::new(Effects::default());
        let policy = policy(Ok(ToolPolicyDecision::AllowOnce), false);
        let runner = runner(&effects, ToolAccess::Write, false).with_tool_policy(policy.clone());
        runner.set_llm(Rc::new(Box::new(InvalidModel {
            name,
            raw,
            issued: Cell::new(false),
        })));
        let mut session = session();
        assert!(
            runner
                .run_turn(&mut session, "work", &mut Silent, None)
                .await
                .unwrap()
                .is_success()
        );
        assert!(policy.seen.borrow().is_empty());
        assert!(effects.started.borrow().is_empty());
        assert!(session.log().events().iter().any(|event| matches!(event,
            SessionEvent::ToolResult { call_id, is_error: true, .. } if call_id.as_str() == "invalid")));
    }
}

struct Capture(Rc<RefCell<Vec<ChatRequest>>>);
impl LlmPort for Capture {
    fn model(&self) -> &'static str {
        "claude-sonnet-5-5"
    }
    fn capabilities(&self, _: &str) -> nanus_ports::ModelCapabilities {
        nanus_ports::ModelCapabilities {
            image_input: nanus_ports::ImageInputSupport::Supported,
            image_profile: Some(nanus_ports::ImageProfile::AnthropicSonnet55HighPatch28V1),
            context_window_tokens: Some(1_000_000),
            max_input_tokens: Some(1_000_000),
            max_output_tokens: Some(128_000),
        }
    }
    fn stream_chat(&self, request: ChatRequest) -> LlmStream {
        self.0.borrow_mut().push(request);
        Box::pin(futures::stream::iter([LlmEvent::TextDelta("done".into())]))
    }
}

#[tokio::test]
async fn assembled_fit_drops_whole_image_turns_and_refuses_the_current_turn_before_streaming() {
    use base64::Engine as _;
    use nanus_domain::{ContentBlock, Message};
    let requests = Rc::new(RefCell::new(Vec::new()));
    let runner = AgentRunner::new(
        Rc::new(Box::new(Capture(Rc::clone(&requests)))),
        ToolRegistryHandle::new(ToolRegistry::new()),
        "host",
        AgentConfig::new(4, 1, "claude-sonnet-5-5", 4096)
            .unwrap()
            .with_context_budget(16384)
            .unwrap(),
        Rc::new(Box::new(Clock)),
    )
    .unwrap()
    .with_request_budget(8192, 0);
    let mut session = session();
    session.append(SessionEvent::UserMessage {
        text: "old inspection".into(),
    });
    let calls: Vec<_> = (0..3)
        .map(|index| {
            ToolCall::new(
                ToolCallId::new(format!("old-{index}")),
                ToolName::new("inspect").unwrap(),
                json!({}),
            )
        })
        .collect();
    session.append(SessionEvent::AssistantMessage {
        text: None,
        reasoning: None,
        replay: None,
        tool_calls: calls.clone(),
        usage: None,
        interrupted: false,
        model: None,
        effort: None,
    });
    let image = ContentBlock::Image {
        media_type: "image/png".into(),
        data_base64: base64::engine::general_purpose::STANDARD.encode(include_bytes!(
            "../../nanus-domain/tests/data/tiny-green-triangle.png"
        )),
    };
    for call in calls {
        session.append(SessionEvent::ToolResult {
            call_id: call.id,
            content: "summary".into(),
            content_blocks: Some(vec![image.clone(); 4]),
            is_error: false,
        });
    }
    runner
        .run_turn(&mut session, "new question", &mut Silent, None)
        .await
        .unwrap();
    {
        let sent = requests.borrow();
        assert_eq!(sent.len(), 1);
        assert!(
            !sent[0]
                .messages
                .iter()
                .any(|m| matches!(m, Message::Tool { .. }))
        );
        assert!(sent[0].messages.iter().all(|m| m.tool_calls().is_empty()));
        assert!(
            sent[0]
                .messages
                .iter()
                .any(|m| m.text().is_some_and(|s| s.contains("were left out")))
        );
        assert!(
            session
                .derive_messages()
                .iter()
                .any(|m| matches!(m, Message::Tool { .. }))
        );
    }
    assert!(
        runner
            .run_turn(&mut session, &"x".repeat(20000), &mut Silent, None)
            .await
            .is_err()
    );
    assert_eq!(requests.borrow().len(), 1);
    let events = session.log().events();
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, SessionEvent::StepStart { .. }))
            .count(),
        events
            .iter()
            .filter(|e| matches!(e, SessionEvent::StepEnd { .. }))
            .count()
    );
}
