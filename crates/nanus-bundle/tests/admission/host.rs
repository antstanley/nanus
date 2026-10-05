//! Actual optional host lifecycle, not a duplicate reservation implementation.
use super::*;
use nanus_ports::{AdmissionError, ToolAdmission, ToolBatchProjection, ToolBatchReservation};
use nanus_ports::{LocalBoxFuture, ReasoningEffort, TurnControl};
use std::rc::Weak;

#[derive(Default)]
struct Plan {
    trace: Rc<RefCell<Vec<String>>>,
    ran: Rc<Cell<u32>>,
    allow: Cell<Option<u32>>,
    reserve_error: Cell<bool>,
    denied: Cell<usize>,
    prior: Cell<u32>,
    expected_output: Option<u32>,
    raw_value: Option<serde_json::Value>,
    dispatch_error: Cell<bool>,
    result_error: Cell<bool>,
    commit_error: Cell<bool>,
    switch: Cell<bool>,
    runner: RefCell<Weak<AgentRunner>>,
    cancel: Option<Rc<Stop>>,
}
fn error() -> AdmissionError {
    AdmissionError {
        message: "fixture refusal".into(),
    }
}
impl ToolAdmission for Plan {
    fn reserve(
        &self,
        projection: &ToolBatchProjection<'_>,
    ) -> Result<Box<dyn ToolBatchReservation>, AdmissionError> {
        assert!(projection.session_id.as_str().starts_with("admission"));
        assert_eq!(projection.calls.len(), projection.outcomes.len());
        assert_eq!(
            projection.request.max_tokens,
            Some(self.expected_output.unwrap_or(8192))
        );
        self.denied.set(
            projection
                .outcomes
                .iter()
                .filter(|outcome| outcome.is_some())
                .count(),
        );
        assert_eq!(projection.request.separate_reasoning_tokens, 0);
        assert!(
            projection
                .request
                .tools
                .iter()
                .any(|schema| schema.name.as_str() == "frames")
        );
        assert!(
            projection
                .request
                .messages
                .iter()
                .any(|message| message.tool_calls().len() == projection.calls.len())
        );
        assert!((projection.estimate)(projection.request).is_ok());
        assert_eq!(
            self.ran.get(),
            self.prior.get(),
            "projection precedes every current-batch effect"
        );
        self.trace.borrow_mut().push(format!(
            "reserve:{}:{}:{}:{}",
            projection.position.0,
            projection.position.1,
            projection.selection_epoch,
            projection.request.model
        ));
        if self.reserve_error.get() {
            return Err(error());
        }
        Ok(Box::new(Lease {
            trace: self.trace.clone(),
            ran: self.ran.clone(),
            allow: self.allow.get(),
            dispatch_error: self.dispatch_error.get(),
            result_error: self.result_error.get(),
            commit_error: self.commit_error.get(),
            switch: self.switch.get(),
            runner: self.runner.borrow().clone(),
            cancel: self.cancel.clone(),
            raw_value: self.raw_value.clone(),
        }))
    }
    fn release_unreserved(&self, calls: &[ToolCall]) {
        self.trace
            .borrow_mut()
            .push(format!("unreserved:{}", calls.len()));
    }
}
struct Lease {
    trace: Rc<RefCell<Vec<String>>>,
    ran: Rc<Cell<u32>>,
    allow: Option<u32>,
    dispatch_error: bool,
    result_error: bool,
    commit_error: bool,
    switch: bool,
    runner: Weak<AgentRunner>,
    cancel: Option<Rc<Stop>>,
    raw_value: Option<serde_json::Value>,
}
impl ToolBatchReservation for Lease {
    fn admit(&self, call: &ToolCall) -> Result<(), AdmissionError> {
        self.trace.borrow_mut().push(format!("admit:{}", call.id));
        let index: u32 = call
            .id
            .as_str()
            .strip_prefix("call-")
            .unwrap()
            .parse()
            .unwrap();
        if self.allow.is_some_and(|limit| index >= limit) {
            Err(error())
        } else {
            Ok(())
        }
    }
    fn before_dispatch(&self, call: &ToolCall) -> Result<(), AdmissionError> {
        self.trace
            .borrow_mut()
            .push(format!("dispatch:{}:{}", call.id, self.ran.get()));
        if self.switch && call.id.as_str() == "call-0" {
            let runner = self.runner.upgrade().unwrap();
            runner.set_model("replacement");
            runner.set_effort(Some(ReasoningEffort::High));
            runner.set_llm(Rc::new(Box::new(Scripted {
                responses: RefCell::new(vec![answer()]),
                profile: false,
            })));
            assert_eq!(runner.model(), "claude-sonnet-5-5");
            assert_eq!(runner.effort(), None);
        }
        if let Some(cancel) = &self.cancel {
            cancel.cancel();
        }
        if self.dispatch_error {
            Err(error())
        } else {
            Ok(())
        }
    }
    fn validate_result(
        &self,
        call: &ToolCall,
        raw: &ToolResult,
        result: &ToolResult,
    ) -> Result<(), AdmissionError> {
        if let Some(value) = &self.raw_value {
            assert_eq!(raw.outcome.value(), Some(value));
            assert_eq!(result.outcome.value(), Some(&serde_json::Value::Null));
        }
        assert_eq!(call.id, result.call_id);
        self.trace
            .borrow_mut()
            .push(format!("validate:{}:{}", call.id, self.ran.get()));
        if self.result_error {
            Err(error())
        } else {
            Ok(())
        }
    }
    fn commit(&self, request: &ChatRequest) -> Result<(), AdmissionError> {
        let observed: Vec<_> = request
            .messages
            .iter()
            .filter_map(|message| match message {
                nanus_domain::Message::Tool { call_id, .. } => Some(call_id.as_str()),
                _ => None,
            })
            .collect();
        assert!(!observed.is_empty());
        assert!(observed.windows(2).all(|pair| pair[0] < pair[1]));
        if self.switch {
            assert_eq!(request.model, "claude-sonnet-5-5");
            assert_eq!(self.runner.upgrade().unwrap().model(), request.model);
        }
        self.trace
            .borrow_mut()
            .push(format!("commit:{}", observed.len()));
        if self.commit_error {
            Err(error())
        } else {
            Ok(())
        }
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        self.trace.borrow_mut().push("drop".into());
    }
}
#[derive(Default)]
struct Stop {
    stopped: Cell<bool>,
    wake: tokio::sync::Notify,
}
impl Stop {
    fn cancel(&self) {
        self.stopped.set(true);
        self.wake.notify_waiters();
    }
}
impl TurnControl for Stop {
    fn is_cancelled(&self) -> bool {
        self.stopped.get()
    }
    fn cancelled(&self) -> LocalBoxFuture<'_, ()> {
        Box::pin(async move {
            while !self.stopped.get() {
                self.wake.notified().await;
            }
        })
    }
}
fn fixture(count: usize, plan: Plan) -> (Rc<AgentRunner>, Rc<Plan>) {
    let (runner, ran) = runner(vec![calls(count), answer()], false, None, 0);
    let plan = Rc::new(Plan { ran, ..plan });
    let runner = Rc::new(runner.with_tool_admission(plan.clone()));
    *plan.runner.borrow_mut() = Rc::downgrade(&runner);
    (runner, plan)
}

#[tokio::test]
async fn complete_projection_reserves_before_every_chunk_and_drops_after_ordered_commit() {
    let (runner, plan) = fixture(9, Plan::default());
    let mut session = session();
    runner
        .run_turn(&mut session, "look", &mut Silent, None)
        .await
        .unwrap();
    assert_eq!(plan.ran.get(), 9);
    let trace = plan.trace.borrow();
    assert_eq!(trace.first().unwrap(), "reserve:1:1:0:claude-sonnet-5-5");
    assert!(
        trace.iter().position(|x| x == "admit:call-8").unwrap()
            < trace
                .iter()
                .position(|x| x.starts_with("dispatch:"))
                .unwrap()
    );
    assert!(trace.contains(&"validate:call-0:4".into()));
    assert!(trace.contains(&"validate:call-4:8".into()));
    assert!(trace.contains(&"validate:call-8:9".into()));
    assert_eq!(&trace[trace.len() - 2..], &["commit:9", "drop"]);
    assert!(!trace.iter().any(|x| x.starts_with("unreserved:")));
}

#[tokio::test]
async fn impossible_base_refuses_every_effect_and_closes_the_step_and_turn() {
    let (runner, plan) = fixture(
        3,
        Plan {
            reserve_error: Cell::new(true),
            ..Plan::default()
        },
    );
    let mut session = session();
    let outcome = runner
        .run_turn(&mut session, "look", &mut Silent, None)
        .await;
    assert!(matches!(
        outcome,
        Err(nanus_bundle::BundleError::Context(_))
    ));
    assert_eq!(plan.ran.get(), 0);
    assert_eq!(
        results(&session).iter().map(|r| r.1).collect::<Vec<_>>(),
        [true; 3]
    );
    assert!(
        session
            .log()
            .events()
            .iter()
            .any(|e| matches!(e, SessionEvent::StepEnd { .. }))
    );
    assert!(
        session
            .log()
            .events()
            .iter()
            .any(|e| matches!(e, SessionEvent::TurnEnd { .. }))
    );
    assert_eq!(plan.trace.borrow().last().unwrap(), "unreserved:3");
}

#[tokio::test]
async fn admission_and_live_dispatch_refusals_never_invoke_an_executor() {
    for (allow, dispatch, expected) in [(Some(1), false, 1), (None, true, 0)] {
        let (runner, plan) = fixture(
            3,
            Plan {
                allow: Cell::new(allow),
                dispatch_error: Cell::new(dispatch),
                ..Plan::default()
            },
        );
        let mut session = session();
        runner
            .run_turn(&mut session, "look", &mut Silent, None)
            .await
            .unwrap();
        assert_eq!(plan.ran.get(), expected);
        assert_eq!(
            results(&session).iter().filter(|r| !r.1).count(),
            expected as usize
        );
        assert_eq!(plan.trace.borrow().last().unwrap(), "drop");
    }
}

#[tokio::test]
async fn invalid_actual_observations_are_replaced_before_progress_and_retention() {
    struct Progress(Vec<bool>);
    impl nanus_bundle::Progress for Progress {
        fn tool_finished(&mut self, _: &ToolCallId, _: &ToolName, error: bool) {
            self.0.push(error);
        }
    }
    let (runner, plan) = fixture(
        2,
        Plan {
            result_error: Cell::new(true),
            ..Plan::default()
        },
    );
    let mut progress = Progress(Vec::new());
    let mut session = session();
    runner
        .run_turn(&mut session, "look", &mut progress, None)
        .await
        .unwrap();
    assert_eq!(plan.ran.get(), 2);
    assert_eq!(progress.0, [true, true]);
    assert!(results(&session).iter().all(|r| r.1
        && r.2.trim_end() == nanus_ports::tool_admission::TOOL_ADMISSION_RESULT_REFUSAL_TEXT));
}

#[tokio::test]
async fn commit_failure_closes_context_before_a_followup_model_request() {
    let (runner, plan) = fixture(
        2,
        Plan {
            commit_error: Cell::new(true),
            ..Plan::default()
        },
    );
    let mut session = session();
    assert!(matches!(
        runner
            .run_turn(&mut session, "look", &mut Silent, None)
            .await,
        Err(nanus_bundle::BundleError::Context(_))
    ));
    assert_eq!(
        plan.ran.get(),
        2,
        "effects already performed are not fabricated away"
    );
    assert_eq!(results(&session).len(), 2);
    assert_eq!(
        &plan.trace.borrow()[plan.trace.borrow().len() - 2..],
        &["commit:2", "drop"]
    );
}

#[tokio::test]
async fn selection_changes_wait_for_ordered_commit_then_apply_to_the_next_request() {
    let (runner, plan) = fixture(
        2,
        Plan {
            switch: Cell::new(true),
            ..Plan::default()
        },
    );
    let mut session = session();
    runner
        .run_turn(&mut session, "look", &mut Silent, None)
        .await
        .unwrap();
    assert_eq!(plan.ran.get(), 2);
    assert_eq!(runner.model(), "replacement");
    assert_eq!(runner.effort(), Some(ReasoningEffort::High));
    let models: Vec<_> = session
        .log()
        .events()
        .iter()
        .filter_map(|e| match e {
            SessionEvent::AssistantMessage { model, .. } => model.as_deref(),
            _ => None,
        })
        .collect();
    assert_eq!(models, ["claude-sonnet-5-5", "replacement"]);
}

#[tokio::test]
async fn cancellation_racing_a_dispatch_check_settles_every_call_without_effects() {
    let stop = Rc::new(Stop::default());
    let (runner, plan) = fixture(
        6,
        Plan {
            cancel: Some(stop.clone()),
            ..Plan::default()
        },
    );
    let mut session = session();
    let outcome = runner
        .run_turn_with_control(&mut session, "look", &mut Silent, None, stop.as_ref())
        .await
        .unwrap();
    assert_eq!(outcome.reason, nanus_domain::TurnEndReason::Interrupted);
    assert_eq!(
        plan.ran.get(),
        0,
        "a ready cancellation after the callback still denies dispatch"
    );
    assert!(results(&session).iter().all(|r| r.1));
    assert_eq!(
        &plan.trace.borrow()[plan.trace.borrow().len() - 2..],
        &["commit:6", "drop"]
    );
}

struct PendingTool(Rc<Cell<u32>>);
impl ToolExecutor for PendingTool {
    fn execute(&self, _: ToolCall) -> ToolFuture {
        self.0.set(self.0.get().saturating_add(1));
        Box::pin(futures::future::pending())
    }
}
struct PendingPolicy;
impl nanus_ports::ToolPolicy for PendingPolicy {
    fn decide<'a>(
        &'a self,
        _: &'a ToolCall,
        _: nanus_domain::ToolAccess,
    ) -> LocalBoxFuture<'a, Result<nanus_ports::ToolPolicyDecision, nanus_ports::PolicyError>> {
        Box::pin(futures::future::pending())
    }
}
fn blocked(policy: bool) -> (Rc<AgentRunner>, Rc<Plan>) {
    let plan = Rc::new(Plan::default());
    let mut registry = ToolRegistry::new();
    registry.register(ToolDefinition::new(ToolSchema {
        name: ToolName::new("frames").unwrap(), description: "bounded pending fixture".into(),
        parameters: json!({"type":"object","properties":{},"additionalProperties":false}),
    }, PendingTool(plan.ran.clone())).with_access(nanus_domain::ToolAccess::Read)).unwrap();
    let mut runner = AgentRunner::new(
        Rc::new(Box::new(Scripted {
            responses: RefCell::new(vec![calls(1), calls(1)]),
            profile: false,
        })),
        ToolRegistryHandle::new(registry),
        "host",
        AgentConfig::new(6, 4, "claude-sonnet-5-5", 4096)
            .unwrap()
            .with_context_budget(900_000)
            .unwrap(),
        Rc::new(Box::new(Clock)),
    )
    .unwrap()
    .with_request_budget(8192, 0)
    .with_tool_admission(plan.clone());
    if policy {
        runner = runner.with_tool_policy(Rc::new(PendingPolicy));
    }
    let runner = Rc::new(runner);
    *plan.runner.borrow_mut() = Rc::downgrade(&runner);
    (runner, plan)
}

#[tokio::test]
async fn dropping_pending_tool_work_releases_capacity_and_applies_only_latest_queued_choices() {
    let (runner, plan) = blocked(false);
    let mut session = session();
    let mut progress = Silent;
    let mut work = Box::pin(runner.run_turn(&mut session, "look", &mut progress, None));
    assert!(futures::poll!(work.as_mut()).is_pending());
    assert_eq!(plan.ran.get(), 1);
    runner.set_model("first");
    runner.set_model("latest");
    runner.set_effort(Some(ReasoningEffort::High));
    runner.set_effort(None);
    assert_eq!(runner.model(), "claude-sonnet-5-5");
    drop(work);
    assert_eq!(runner.model(), "latest");
    assert_eq!(runner.effort(), None);
    let trace = plan.trace.borrow();
    assert_eq!(trace.last().unwrap(), "drop");
    assert!(
        !trace
            .iter()
            .any(|x| x.starts_with("commit:") || x.starts_with("unreserved:"))
    );
    assert!(
        results(&session).is_empty(),
        "dropping work fabricates no completed observations"
    );
}

#[tokio::test]
async fn dropping_pending_approval_retires_unreserved_handles_without_any_tool_effect() {
    let (runner, plan) = blocked(true);
    let mut session = session();
    let mut progress = Silent;
    let mut work = Box::pin(runner.run_turn(&mut session, "look", &mut progress, None));
    assert!(futures::poll!(work.as_mut()).is_pending());
    assert_eq!(plan.ran.get(), 0);
    assert!(plan.trace.borrow().is_empty());
    runner.set_model("latest");
    drop(work);
    assert_eq!(runner.model(), "latest");
    assert_eq!(&*plan.trace.borrow(), &["unreserved:1"]);
}

#[tokio::test]
async fn an_unfittable_actual_call_batch_retires_handles_before_any_executor() {
    let (base, ran) = runner(vec![calls(96), answer()], false, None, 0);
    let plan = Rc::new(Plan {
        ran,
        ..Plan::default()
    });
    let config = base.config().clone().with_context_budget(4096).unwrap();
    let runner = AgentRunner::new(
        base.llm(),
        base.tools().clone(),
        "host",
        config,
        Rc::new(Box::new(Clock)),
    )
    .unwrap()
    .with_request_budget(16, 0)
    .with_tool_admission(plan.clone());
    let mut session = session();
    let outcome = runner
        .run_turn(&mut session, "look", &mut Silent, None)
        .await;
    assert!(matches!(
        outcome,
        Err(nanus_bundle::BundleError::Context(_))
    ));
    assert!(session.log().events().iter().any(|event| matches!(event,
        SessionEvent::AssistantMessage {tool_calls,..} if tool_calls.len()==96)));
    assert_eq!(plan.ran.get(), 0);
    assert_eq!(results(&session).len(), 96);
    assert_eq!(&*plan.trace.borrow(), &["unreserved:96"]);
}

struct DenyFirst;
impl nanus_ports::ToolPolicy for DenyFirst {
    fn decide<'a>(
        &'a self,
        call: &'a ToolCall,
        _: nanus_domain::ToolAccess,
    ) -> LocalBoxFuture<'a, Result<nanus_ports::ToolPolicyDecision, nanus_ports::PolicyError>> {
        Box::pin(async move {
            Ok(if call.id.as_str() == "call-0" {
                nanus_ports::ToolPolicyDecision::Deny {
                    reason: "ordinary policy denial".into(),
                }
            } else {
                nanus_ports::ToolPolicyDecision::AllowOnce
            })
        })
    }
}

#[tokio::test]
async fn ordinary_denials_reach_projection_without_regaining_dispatch_capacity() {
    let (base, ran) = runner(vec![calls(2), answer()], false, None, 0);
    let plan = Rc::new(Plan {
        ran,
        ..Plan::default()
    });
    let runner = base
        .with_tool_policy(Rc::new(DenyFirst))
        .with_tool_admission(plan.clone());
    let mut session = session();
    runner
        .run_turn(&mut session, "look", &mut Silent, None)
        .await
        .unwrap();
    assert_eq!(plan.denied.get(), 1);
    assert_eq!(plan.ran.get(), 1);
    assert_eq!(
        results(&session).iter().map(|r| r.1).collect::<Vec<_>>(),
        [true, false]
    );
    assert!(!plan.trace.borrow().contains(&"admit:call-0".into()));
}

#[tokio::test]
async fn goal_effects_use_the_same_admission_dispatch_and_validation_lifetime() {
    for allow in [Some(0), None] {
        let mut batch = calls(2);
        if let LlmEvent::ToolCallDelta {
            name,
            arguments_delta,
            ..
        } = &mut batch[0]
        {
            *name = Some(ToolName::new("create_goal").unwrap());
            *arguments_delta = "{\"objective\":\"fictional fixture objective\"}".into();
        }
        let (base, ran) = runner(vec![batch, answer()], false, None, 0);
        let plan = Rc::new(Plan {
            ran,
            allow: Cell::new(allow),
            ..Plan::default()
        });
        let runner = base.with_tool_admission(plan.clone());
        let mut session = session();
        runner
            .run_turn(&mut session, "track this fictional goal", &mut Silent, None)
            .await
            .unwrap();
        assert_eq!(session.goal().is_some(), allow.is_none());
        assert_eq!(plan.ran.get(), u32::from(allow.is_none()));
        assert_eq!(results(&session).len(), 2);
        if allow.is_none() {
            assert!(plan.trace.borrow().contains(&"validate:call-0:0".into()));
        }
        assert_eq!(plan.trace.borrow().last().unwrap(), "drop");
    }
}

struct FullHistory(Rc<Plan>);
impl ToolAdmission for FullHistory {
    fn reserve(
        &self,
        projection: &ToolBatchProjection<'_>,
    ) -> Result<Box<dyn ToolBatchReservation>, AdmissionError> {
        let old = |request: &ChatRequest| {
            request
                .messages
                .iter()
                .any(|message| message.text().is_some_and(|text| text.starts_with("old:")))
        };
        assert!(
            old(projection.request),
            "full history is available to conservative reservation"
        );
        assert!(
            !old(projection.fitted_base),
            "ordinary whole-turn fitting still removes an old group"
        );
        assert!(projection.events.iter().any(|event| matches!(event,
            SessionEvent::UserMessage { text, .. } if text.len() > 100_000)));
        assert!(projection.events.iter().all(|event| !matches!(event,
            SessionEvent::ToolResult { call_id, .. } if projection.calls.iter()
                .any(|call| &call.id == call_id))));
        let full = (projection.estimate)(projection.request).unwrap();
        let fitted = (projection.estimate)(projection.fitted_base).unwrap();
        assert!(full.input_tokens > fitted.input_tokens);
        self.0.reserve(projection)
    }
    fn release_unreserved(&self, calls: &[ToolCall]) {
        self.0.release_unreserved(calls);
    }
}

#[tokio::test]
async fn projection_keeps_full_history_and_the_separately_fitted_failure_base() {
    let (base, ran) = runner(vec![calls(2), answer()], false, None, 0);
    let plan = Rc::new(Plan {
        ran,
        expected_output: Some(64),
        ..Plan::default()
    });
    let config = base.config().clone().with_context_budget(4096).unwrap();
    let runner = AgentRunner::new(
        base.llm(),
        base.tools().clone(),
        "host",
        config,
        Rc::new(Box::new(Clock)),
    )
    .unwrap()
    .with_request_budget(64, 0)
    .with_tool_admission(Rc::new(FullHistory(plan.clone())));
    let mut session = session();
    session.append(SessionEvent::UserMessage {
        content_blocks: None,
        text: format!("old:{}", "a".repeat(128 * 1024)),
    });
    session.append(SessionEvent::AssistantMessage {
        text: Some("old answer".into()),
        reasoning: None,
        tool_calls: Vec::new(),
        replay: None,
        usage: None,
        interrupted: false,
        model: Some("claude-sonnet-5-5".into()),
        effort: None,
    });
    runner
        .run_turn(&mut session, "look", &mut Silent, None)
        .await
        .unwrap();
    assert_eq!(plan.ran.get(), 2);
    assert_eq!(results(&session).len(), 2);
}

#[tokio::test]
async fn overlapping_steps_keep_selection_held_until_both_reservations_drop() {
    let (runner, plan) = blocked(false);
    let mut first = session();
    let mut second = Session::new(SessionId::new("admission-2"), 0, "workspace");
    let mut first_progress = Silent;
    let mut second_progress = Silent;
    let mut first_work = Box::pin(runner.run_turn(&mut first, "first", &mut first_progress, None));
    assert!(futures::poll!(first_work.as_mut()).is_pending());
    plan.prior.set(1);
    let mut second_work =
        Box::pin(runner.run_turn(&mut second, "second", &mut second_progress, None));
    assert!(futures::poll!(second_work.as_mut()).is_pending());
    assert_eq!(plan.ran.get(), 2);
    runner.set_model("latest");
    drop(first_work);
    assert_eq!(runner.model(), "claude-sonnet-5-5");
    drop(second_work);
    assert_eq!(runner.model(), "latest");
    assert_eq!(
        plan.trace
            .borrow()
            .iter()
            .filter(|entry| *entry == "drop")
            .count(),
        2
    );
    assert!(
        !plan
            .trace
            .borrow()
            .iter()
            .any(|entry| entry.starts_with("commit:"))
    );
}

#[tokio::test]
async fn validation_sees_private_raw_values_and_the_exact_normalized_model_observation() {
    let value = json!({"source_sha256":"fixture","frames":1});
    let outcome = ToolOutcome::success_with(
        value.clone(),
        vec![ContentBlock::Text("frame manifest".into())],
    );
    let (base, ran) = runner_outcome(vec![calls(1), answer()], false, None, 0, Some(outcome));
    let plan = Rc::new(Plan {
        ran,
        raw_value: Some(value),
        ..Plan::default()
    });
    let runner = base.with_tool_admission(plan.clone());
    let mut session = session();
    runner
        .run_turn(&mut session, "look", &mut Silent, None)
        .await
        .unwrap();
    assert_eq!(plan.ran.get(), 1);
    assert_eq!(results(&session)[0].2, "frame manifest\n");
    assert!(!results(&session)[0].1);
}
