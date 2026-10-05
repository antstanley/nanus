//! Additional actual-runner traces for framing, replay and model failure.
use super::*;
use nanus_domain::message::AssistantReplay;
use nanus_ports::{LlmError, LlmResult, RequestEstimate};

struct Script {
    events: RefCell<Vec<LlmEvent>>,
    requests: Rc<Cell<u32>>,
    estimate_error: bool,
}
impl LlmPort for Script {
    fn model(&self) -> &'static str {
        "host-model"
    }
    fn estimate_request(&self, request: &ChatRequest) -> LlmResult<RequestEstimate> {
        if self.estimate_error {
            return Err(LlmError::Unsupported {
                feature: "fixture budget".into(),
            });
        }
        Ok(RequestEstimate {
            input_tokens: 1,
            request_bytes: 1,
            images: 0,
            reservation: request.max_tokens.unwrap_or(0),
        })
    }
    fn stream_chat(&self, _: ChatRequest) -> LlmStream {
        self.requests
            .set(self.requests.get().checked_add(1).unwrap());
        let events = std::mem::take(&mut *self.events.borrow_mut());
        Box::pin(futures::stream::iter(events))
    }
}
fn scripted(runner: &AgentRunner, events: Vec<LlmEvent>, estimate_error: bool) -> Rc<Cell<u32>> {
    let requests = Rc::new(Cell::new(0));
    runner.set_llm(Rc::new(Box::new(Script {
        events: RefCell::new(events),
        requests: requests.clone(),
        estimate_error,
    })));
    requests
}

#[tokio::test]
async fn a_new_turn_cancelled_before_model_contact_does_not_return_a_previous_turn_answer() {
    let effects = Rc::new(Effects::default());
    let mut session = session();
    let first = hosted(&Rc::new(Plan::default()), &effects, false);
    scripted(&first, finished("Previous completed answer"), false);
    assert_eq!(
        first
            .run_turn(&mut session, INPUT, &mut Silent, None)
            .await
            .unwrap()
            .answer,
        "Previous completed answer"
    );
    let raw = session.try_to_jsonl().unwrap();
    let mut loaded = Session::from_jsonl(&raw).unwrap();
    let control = Rc::new(Control::default());
    let plan = Rc::new(Plan {
        refusal: "cancel-step",
        cancel: Some(control.clone()),
        ..Plan::default()
    });
    let second = hosted(&plan, &effects, false);
    let requests = scripted(&second, finished("Must not be requested"), false);
    let outcome = second
        .run_turn_with_control(&mut loaded, INPUT, &mut Silent, None, control.as_ref())
        .await
        .unwrap();
    assert_eq!(outcome.reason, TurnEndReason::Interrupted);
    assert!(outcome.answer.is_empty());
    assert_eq!(requests.get(), 0);
    assert!(effects.started.borrow().is_empty());
    assert!(
        loaded
            .try_to_jsonl()
            .unwrap()
            .contains("Previous completed answer")
    );
}
fn finished(text: &str) -> Vec<LlmEvent> {
    vec![
        LlmEvent::TextDelta(text.into()),
        LlmEvent::Finished {
            reason: FinishReason::Stop,
        },
    ]
}

#[tokio::test]
async fn a_stock_turn_cancelled_before_model_contact_does_not_return_a_previous_turn_answer() {
    let effects = Rc::new(Effects::default());
    let runner = runner(&effects, ToolAccess::Read, false);
    scripted(&runner, finished("Previous stock answer"), false);
    let mut session = session();
    assert_eq!(
        runner
            .run_turn(&mut session, INPUT, &mut Silent, None)
            .await
            .unwrap()
            .answer,
        "Previous stock answer"
    );
    let control = Control::default();
    control.cancel();
    let requests = scripted(&runner, finished("Must not be requested"), false);
    let outcome = runner
        .run_turn_with_control(&mut session, INPUT, &mut Silent, None, &control)
        .await
        .unwrap();
    assert_eq!(outcome.reason, TurnEndReason::Interrupted);
    assert!(outcome.answer.is_empty());
    assert_eq!(requests.get(), 0);
    assert!(
        session
            .try_to_jsonl()
            .unwrap()
            .contains("Previous stock answer")
    );
}

fn replay(text: &str) -> AssistantReplay {
    AssistantReplay {
        protocol: "anthropic.messages".into(),
        prefix_digest: "a".repeat(64),
        context_receipt: None,
        blocks: vec![
            json!({"type":"thinking","thinking":"original","signature":"fixture-signature"}),
            json!({"type":"text","text":text}),
        ],
    }
}

#[tokio::test]
async fn exact_escaped_assistant_record_byte_edges_accept_at_limit_and_refuse_next_byte() {
    let text = "\0\"\\💠".repeat(64);
    let expected = SessionEvent::AssistantMessage {
        replay: None,
        text: Some(text.clone()),
        reasoning: None,
        tool_calls: vec![],
        usage: None,
        interrupted: false,
        model: Some("host-model".into()),
        effort: None,
    };
    let bytes = serde_json::to_vec(&expected).unwrap().len();
    for (limit, admitted) in [
        (bytes.checked_sub(1).unwrap(), false),
        (bytes, true),
        (bytes.checked_add(1).unwrap(), true),
    ] {
        let plan = Rc::new(Plan {
            step_limit: Some(limit),
            ..Plan::default()
        });
        let effects = Rc::new(Effects::default());
        let runner = hosted(&plan, &effects, false);
        let requests = scripted(&runner, finished(&text), false);
        let mut session = session();
        let result = runner
            .run_turn(&mut session, INPUT, &mut Silent, None)
            .await;
        assert_eq!(result.is_ok(), admitted);
        assert_eq!(requests.get(), 1);
        if admitted {
            assert_eq!(session.log().events()[3], expected);
        } else {
            no_calls(&session);
        }
        assert!(effects.started.borrow().is_empty());
    }
}

#[tokio::test]
async fn original_signed_replay_text_reasoning_and_usage_survive_actual_callback_and_reload() {
    let original = replay("Original text");
    let mut events = finished("Original text");
    events.insert(1, LlmEvent::ReasoningDelta("Original reasoning".into()));
    events.insert(2, LlmEvent::AssistantReplay(original.clone()));
    events.insert(
        3,
        LlmEvent::Usage(nanus_domain::Usage::new(20, 4, 2, 0, 20)),
    );
    let plan = Rc::new(Plan::default());
    let effects = Rc::new(Effects::default());
    let runner = hosted(&plan, &effects, false);
    let requests = scripted(&runner, events, false);
    let mut session = session();
    let result = runner
        .run_turn(&mut session, INPUT, &mut Silent, None)
        .await
        .unwrap();
    assert_eq!(requests.get(), 1);
    assert_eq!(result.answer, "Original text");
    let saved = session.try_to_jsonl().unwrap();
    let restored = Session::from_jsonl(&saved).unwrap();
    let SessionEvent::AssistantMessage {
        replay,
        reasoning,
        usage,
        ..
    } = &restored.log().events()[3]
    else {
        panic!("original assistant record");
    };
    assert_eq!(replay.as_ref(), Some(&original));
    assert_eq!(reasoning.as_deref(), Some("Original reasoning"));
    assert_eq!(usage.unwrap().prompt_tokens, 20);
    assert_eq!(
        plan.trace
            .borrow()
            .iter()
            .filter(|s| s.as_str() == "model")
            .count(),
        1
    );
}

#[tokio::test]
async fn host_size_refusal_precedes_replay_validation_and_invalid_replay_never_appends() {
    for refuse in [true, false] {
        let plan = Rc::new(Plan {
            refusal: if refuse { "model" } else { "" },
            ..Plan::default()
        });
        let effects = Rc::new(Effects::default());
        let runner = hosted(&plan, &effects, false);
        let mut events = finished("Neutral text");
        events.insert(1, LlmEvent::AssistantReplay(replay("Different text")));
        scripted(&runner, events, false);
        let mut session = session();
        let error = runner
            .run_turn(&mut session, INPUT, &mut Silent, None)
            .await
            .unwrap_err();
        if refuse {
            assert!(error.to_string().contains("host record admission refused"));
        } else {
            assert!(
                error
                    .to_string()
                    .contains("assistant replay differs from response")
            );
        }
        no_calls(&session);
        assert!(plan.trace.borrow().iter().any(|s| s == "model"));
        assert!(effects.started.borrow().is_empty());
    }
}

#[tokio::test]
async fn host_record_admission_precedes_responses_argument_parsing_and_shape_validation() {
    for refuse in [true, false] {
        let plan = Rc::new(Plan {
            refusal: if refuse { "model" } else { "" },
            ..Plan::default()
        });
        let effects = Rc::new(Effects::default());
        let runner = hosted(&plan, &effects, false);
        let original = AssistantReplay {
            protocol: "openai.responses".into(),
            prefix_digest: "a".repeat(64),
            context_receipt: None,
            blocks: vec![json!({"type":"function_call","id":"fc1","call_id":"c1",
                "name":"read","arguments":"{","status":"completed"})],
        };
        scripted(&runner, vec![LlmEvent::AssistantReplay(original)], false);
        let mut session = session();
        let error = runner
            .run_turn(&mut session, INPUT, &mut Silent, None)
            .await
            .unwrap_err();
        let expected = if refuse {
            "host record admission refused"
        } else {
            "invalid Responses replay item"
        };
        assert!(error.to_string().contains(expected));
        assert!(plan.trace.borrow().iter().any(|phase| phase == "model"));
        no_calls(&session);
        assert!(effects.started.borrow().is_empty());
    }
}

#[tokio::test]
async fn model_error_closes_reserved_step_and_turn_without_retaining_an_observation() {
    let plan = Rc::new(Plan::default());
    let effects = Rc::new(Effects::default());
    let runner = hosted(&plan, &effects, false);
    let requests = scripted(
        &runner,
        vec![
            LlmEvent::TextDelta("partial".into()),
            LlmEvent::Error("fixture error".into()),
        ],
        false,
    );
    let mut session = session();
    assert!(
        runner
            .run_turn(&mut session, INPUT, &mut Silent, None)
            .await
            .is_err()
    );
    assert_eq!(requests.get(), 1);
    no_calls(&session);
    assert_eq!(
        *plan.trace.borrow(),
        [
            "turn",
            "step",
            "step-end",
            "drop-step",
            "turn-end",
            "drop-turn"
        ]
    );
    assert!(session.log().last_turn_end().is_some());
}

#[tokio::test]
async fn request_construction_failure_after_turn_reservation_has_no_step_or_model_contact() {
    let plan = Rc::new(Plan::default());
    let effects = Rc::new(Effects::default());
    let runner = runner(&effects, ToolAccess::Read, false)
        .with_record_admission(plan.clone())
        .with_request_budget(8192, 0);
    let requests = scripted(&runner, finished("must not contact"), true);
    let mut session = session();
    assert!(
        runner
            .run_turn(&mut session, INPUT, &mut Silent, None)
            .await
            .is_err()
    );
    assert_eq!(requests.get(), 0);
    assert_eq!(*plan.trace.borrow(), ["turn", "turn-end", "drop-turn"]);
    assert!(!session.log().events().iter().any(|e| matches!(
        e,
        SessionEvent::StepStart { .. } | SessionEvent::StepEnd { .. }
    )));
    assert!(session.log().last_turn_end().is_some());
}

#[tokio::test]
async fn quiet_stream_cancellation_records_no_calls_and_releases_reserved_closings_once() {
    let plan = Rc::new(Plan::default());
    let effects = Rc::new(Effects::default());
    let runner = hosted(&plan, &effects, true);
    let control = Control::default();
    let mut session = session();
    let mut progress = Silent;
    let mut future =
        Box::pin(runner.run_turn_with_control(&mut session, INPUT, &mut progress, None, &control));
    assert!(matches!(futures::poll!(future.as_mut()), Poll::Pending));
    control.cancel();
    assert_eq!(future.await.unwrap().reason, TurnEndReason::Interrupted);
    assert!(effects.started.borrow().is_empty());
    assert_eq!(
        *plan.trace.borrow(),
        [
            "turn",
            "step",
            "model",
            "step-end",
            "drop-step",
            "turn-end",
            "drop-turn"
        ]
    );
    assert!(session.log().events().iter().any(|e|matches!(e,SessionEvent::AssistantMessage {interrupted:true,tool_calls,..} if tool_calls.is_empty())));
}

#[tokio::test]
async fn dropped_native_work_keeps_logical_guards_through_dispatch_then_releases_each_once() {
    let plan = Rc::new(Plan::default());
    let effects = Rc::new(Effects {
        pending: true,
        ..Effects::default()
    });
    let runner = hosted(&plan, &effects, false);
    let mut session = session();
    let mut progress = Silent;
    let mut future = Box::pin(runner.run_turn(&mut session, INPUT, &mut progress, None));
    assert!(matches!(futures::poll!(future.as_mut()), Poll::Pending));
    assert_eq!(effects.started.borrow().len(), 2);
    assert_eq!(*plan.trace.borrow(), ["turn", "step", "model"]);
    drop(future);
    assert_eq!(effects.dropped.get(), 2);
    assert_eq!(
        *plan.trace.borrow(),
        ["turn", "step", "model", "drop-step", "drop-turn"]
    );
}
