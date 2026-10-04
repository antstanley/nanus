//! Fixture-only logical reservations; no application capacity or effect authority is inferred.
use super::*;
use nanus_domain::SessionEvent;
use nanus_ports::{
    AdmissionError, ModelRecordProjection, RecordAdmission, RecordEndProjection,
    StepRecordProjection, StepRecordReservation, TurnRecordProjection, TurnRecordReservation,
};
use std::cell::RefCell;

pub type Trace = Rc<RefCell<Vec<&'static str>>>;
pub const SUCCESS_TRACE: &[&str] = &[
    "turn",
    "step",
    "model",
    "batch",
    "commit",
    "drop-batch",
    "step-end",
    "drop-step",
    "step",
    "model",
    "step-end",
    "drop-step",
    "turn-end",
    "drop-turn",
    "turn",
    "step",
    "model",
    "step-end",
    "drop-step",
    "turn-end",
    "drop-turn",
];

pub fn install(runner: AgentRunner, trace: &Trace, enabled: bool, refuse: bool) -> AgentRunner {
    if enabled {
        runner.with_record_admission(Rc::new(Plan {
            trace: trace.clone(),
            refuse,
        }))
    } else {
        runner
    }
}

struct Plan {
    trace: Trace,
    refuse: bool,
}
impl RecordAdmission for Plan {
    fn reserve_turn(
        &self,
        p: &TurnRecordProjection<'_>,
    ) -> Result<Box<dyn TurnRecordReservation>, AdmissionError> {
        assert!(!p.message.is_empty());
        assert_eq!(
            p.next_sequence.value(),
            u64::try_from(p.events.len()).unwrap()
        );
        self.trace.borrow_mut().push("turn");
        Ok(Box::new(Turn {
            trace: self.trace.clone(),
            refuse: self.refuse,
        }))
    }
}
struct Turn {
    trace: Trace,
    refuse: bool,
}
impl TurnRecordReservation for Turn {
    fn reserve_step(
        &self,
        p: &StepRecordProjection<'_>,
    ) -> Result<Box<dyn StepRecordReservation>, AdmissionError> {
        assert_eq!(
            p.next_sequence.value(),
            u64::try_from(p.events.len()).unwrap()
        );
        assert_eq!(
            p.fitted_request.source_history.as_deref(),
            Some(&*p.request.messages)
        );
        assert!(
            (p.estimate)(p.fitted_request)
                .unwrap()
                .fits(p.capabilities, p.fitted_request)
        );
        assert!(!matches!(
            p.events.last(),
            Some(SessionEvent::StepStart { .. })
        ));
        self.trace.borrow_mut().push("step");
        Ok(Box::new(Step {
            trace: self.trace.clone(),
            refuse: self.refuse,
            epoch: p.selection_epoch,
            position: p.position,
            next: p.next_sequence.value(),
        }))
    }
    fn validate_end(&self, p: &RecordEndProjection<'_>) -> Result<(), AdmissionError> {
        assert!(matches!(p.event, SessionEvent::TurnEnd { .. }));
        assert_eq!(
            p.next_sequence.value(),
            u64::try_from(p.events.len()).unwrap()
        );
        self.trace.borrow_mut().push("turn-end");
        Ok(())
    }
}
impl Drop for Turn {
    fn drop(&mut self) {
        self.trace.borrow_mut().push("drop-turn");
    }
}
struct Step {
    trace: Trace,
    refuse: bool,
    epoch: u64,
    position: (u32, u32),
    next: u64,
}
impl StepRecordReservation for Step {
    fn validate_model(&self, p: &ModelRecordProjection<'_>) -> Result<(), AdmissionError> {
        assert_eq!(p.position, self.position);
        assert_eq!(p.selection_epoch, self.epoch);
        assert_eq!(p.next_sequence.value(), self.next.checked_add(1).unwrap());
        assert_eq!(
            p.next_sequence.value(),
            u64::try_from(p.events.len()).unwrap()
        );
        let SessionEvent::AssistantMessage {
            replay: Some(replay),
            model,
            ..
        } = p.assistant
        else {
            panic!("actual original Responses observation");
        };
        assert_eq!(model.as_deref(), Some("gpt-6-astra"));
        assert_eq!(replay.protocol, "openai.responses");
        assert!(replay.context_receipt.is_some());
        self.trace.borrow_mut().push("model");
        if self.refuse {
            Err(AdmissionError {
                message: "private fixture refusal".into(),
            })
        } else {
            Ok(())
        }
    }
    fn validate_end(&self, p: &RecordEndProjection<'_>) -> Result<(), AdmissionError> {
        assert!(matches!(p.event, SessionEvent::StepEnd { turn, step }
            if (*turn, *step) == self.position));
        assert_eq!(
            p.next_sequence.value(),
            u64::try_from(p.events.len()).unwrap()
        );
        self.trace.borrow_mut().push("step-end");
        Ok(())
    }
}
impl Drop for Step {
    fn drop(&mut self) {
        self.trace.borrow_mut().push("drop-step");
    }
}

#[tokio::test]
async fn actual_completed_response_refused_by_record_host_reaches_no_batch_or_executor() {
    let (endpoint, worker) = server(vec![frames(&original_items())]);
    let executed = Rc::new(Cell::new(0));
    let adapter = Rc::new(adapter());
    let batches = Rc::new(Cell::new(0));
    let trace = Trace::default();
    let runner = install(
        runner(
            FixtureModel {
                adapter: adapter.clone(),
                endpoint,
            },
            executed.clone(),
            64_000,
        )
        .with_tool_admission(Rc::new(Admit {
            adapter,
            count: batches.clone(),
            trace: Some(trace.clone()),
        })),
        &trace,
        true,
        true,
    );
    let mut session = Session::new(SessionId::new("fictional-refused"), 123, "/fictional");
    let error = tokio::time::timeout(
        Duration::from_secs(5),
        runner.run_turn(&mut session, "Fictional question", &mut Silent, None),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(error.to_string().contains("host record admission refused"));
    assert!(!error.to_string().contains("private fixture refusal"));
    assert_eq!(executed.get(), 0);
    assert_eq!(batches.get(), 0);
    assert_eq!(worker.join().unwrap().len(), 1);
    assert!(!session.log().events().iter().any(|event| matches!(
        event,
        SessionEvent::AssistantMessage { .. }
            | SessionEvent::ToolCall { .. }
            | SessionEvent::ToolResult { .. }
    )));
    assert_eq!(
        *trace.borrow(),
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
    assert_eq!(
        Session::from_jsonl(&session.try_to_jsonl().unwrap()).unwrap(),
        session
    );
}
