//! Original runner ordering; no copied implementation of its model/tool loop.
use super::*;
use nanus_ports::{
    AdmissionError, ModelRecordProjection, RecordAdmission, RecordEndProjection,
    StepRecordProjection, StepRecordReservation, TurnRecordProjection, TurnRecordReservation,
};

#[derive(Default)]
struct Plan {
    trace: Rc<RefCell<Vec<String>>>,
    refusal: &'static str,
    step_limit: Option<usize>,
    runner: RefCell<std::rc::Weak<AgentRunner>>,
    switch: bool,
    cancel: Option<Rc<Control>>,
}
fn refused() -> AdmissionError {
    AdmissionError {
        message: "private diagnostic must never enter the log".into(),
    }
}
impl Plan {
    fn check(&self, phase: &str) -> Result<(), AdmissionError> {
        self.trace.borrow_mut().push(phase.into());
        if self.refusal == phase {
            Err(refused())
        } else {
            Ok(())
        }
    }
    fn lease(&self) -> Lease {
        Lease {
            trace: self.trace.clone(),
            refusal: self.refusal,
            limit: self.step_limit,
            runner: self.runner.borrow().clone(),
            switch: self.switch,
            cancel: self.cancel.clone(),
        }
    }
}
impl RecordAdmission for Plan {
    fn reserve_turn(
        &self,
        view: &TurnRecordProjection<'_>,
    ) -> Result<Box<dyn TurnRecordReservation>, AdmissionError> {
        assert_eq!(view.session_id.as_str(), "embedded");
        assert_eq!(
            view.next_sequence.value(),
            u64::try_from(view.events.len()).unwrap()
        );
        assert_eq!(view.message, "  Original\nmessage 💠  ");
        self.check("turn")?;
        if self.refusal == "cancel-turn"
            && let Some(control) = &self.cancel
        {
            control.cancel();
        }
        Ok(Box::new(self.lease()))
    }
}
struct Lease {
    trace: Rc<RefCell<Vec<String>>>,
    refusal: &'static str,
    limit: Option<usize>,
    runner: std::rc::Weak<AgentRunner>,
    switch: bool,
    cancel: Option<Rc<Control>>,
}
impl Lease {
    fn check(&self, phase: &str) -> Result<(), AdmissionError> {
        self.trace.borrow_mut().push(phase.into());
        if self.refusal == phase {
            Err(refused())
        } else {
            Ok(())
        }
    }
}
impl TurnRecordReservation for Lease {
    fn reserve_step(
        &self,
        view: &StepRecordProjection<'_>,
    ) -> Result<Box<dyn StepRecordReservation>, AdmissionError> {
        assert_eq!(view.session_id.as_str(), "embedded");
        assert_eq!(
            view.next_sequence.value(),
            u64::try_from(view.events.len()).unwrap()
        );
        assert!(!matches!(
            view.events.last(),
            Some(SessionEvent::StepStart { .. })
        ));
        assert_eq!(view.request.model, view.fitted_request.model);
        assert!(view.request.messages.iter().any(|m| matches!(m,
            nanus_domain::Message::User { text, .. } if text == "  Original\nmessage 💠  ")));
        self.check("step")?;
        if self.refusal == "cancel-step"
            && let Some(control) = &self.cancel
        {
            control.cancel();
        }
        let old = view.request.model.clone();
        if self.switch && view.position.1 == 1 {
            let runner = self.runner.upgrade().unwrap();
            runner.set_model("replacement");
            runner.set_effort(Some(nanus_ports::ReasoningEffort::High));
            assert_eq!(runner.model(), old);
        }
        Ok(Box::new(StepLease {
            trace: self.trace.clone(),
            refusal: self.refusal,
            limit: self.limit,
            epoch: view.selection_epoch,
            model: old,
            effort: view.effort,
            position: view.position,
        }))
    }
    fn validate_end(&self, view: &RecordEndProjection<'_>) -> Result<(), AdmissionError> {
        assert!(matches!(view.event, SessionEvent::TurnEnd { .. }));
        assert_eq!(
            view.next_sequence.value(),
            u64::try_from(view.events.len()).unwrap()
        );
        self.check("turn-end")
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        self.trace.borrow_mut().push("drop-turn".into());
    }
}
struct StepLease {
    trace: Rc<RefCell<Vec<String>>>,
    refusal: &'static str,
    limit: Option<usize>,
    epoch: u64,
    model: String,
    effort: Option<nanus_ports::ReasoningEffort>,
    position: (u32, u32),
}
impl StepRecordReservation for StepLease {
    fn validate_model(&self, view: &ModelRecordProjection<'_>) -> Result<(), AdmissionError> {
        self.trace.borrow_mut().push("model".into());
        assert_eq!(view.position, self.position);
        assert_eq!(view.selection_epoch, self.epoch);
        assert_eq!(
            view.next_sequence.value(),
            u64::try_from(view.events.len()).unwrap()
        );
        assert!(
            matches!(view.events.last(), Some(SessionEvent::StepStart { turn, step })
            if (*turn, *step) == view.position)
        );
        let SessionEvent::AssistantMessage { model, effort, .. } = view.assistant else {
            panic!("actual proposed assistant");
        };
        assert_eq!(model.as_deref(), Some(self.model.as_str()));
        assert_eq!(
            effort.as_deref(),
            self.effort.map(nanus_ports::ReasoningEffort::as_str)
        );
        let encoded = serde_json::to_vec(view.assistant).unwrap();
        if self.refusal == "model" || self.limit.is_some_and(|limit| encoded.len() > limit) {
            Err(refused())
        } else {
            Ok(())
        }
    }
    fn validate_end(&self, view: &RecordEndProjection<'_>) -> Result<(), AdmissionError> {
        self.trace.borrow_mut().push("step-end".into());
        assert!(matches!(view.event, SessionEvent::StepEnd { turn, step }
            if (*turn,*step)==self.position));
        assert_eq!(
            view.next_sequence.value(),
            u64::try_from(view.events.len()).unwrap()
        );
        if self.refusal == "step-end" {
            Err(refused())
        } else {
            Ok(())
        }
    }
}
impl Drop for StepLease {
    fn drop(&mut self) {
        self.trace.borrow_mut().push("drop-step".into());
    }
}
const INPUT: &str = "  Original\nmessage 💠  ";
fn hosted(plan: &Rc<Plan>, effects: &Rc<Effects>, silent: bool) -> Rc<AgentRunner> {
    let runner =
        Rc::new(runner(effects, ToolAccess::Read, silent).with_record_admission(plan.clone()));
    *plan.runner.borrow_mut() = Rc::downgrade(&runner);
    runner
}
fn no_calls(session: &Session) {
    assert!(!session.log().events().iter().any(|e| matches!(
        e,
        SessionEvent::AssistantMessage { .. }
            | SessionEvent::ToolCall { .. }
            | SessionEvent::ToolResult { .. }
    )));
}

#[tokio::test]
async fn turn_refusal_preserves_original_history_and_prevents_every_later_phase() {
    let plan = Rc::new(Plan {
        refusal: "turn",
        ..Plan::default()
    });
    let effects = Rc::new(Effects::default());
    let runner = hosted(&plan, &effects, false);
    let mut session = session();
    let original = session.try_to_jsonl().unwrap();
    let error = runner
        .run_turn(&mut session, INPUT, &mut Silent, None)
        .await
        .unwrap_err();
    assert_eq!(original, session.try_to_jsonl().unwrap());
    assert!(error.to_string().contains("host record admission refused"));
    assert!(!error.to_string().contains("private diagnostic"));
    assert!(effects.started.borrow().is_empty());
    assert_eq!(*plan.trace.borrow(), ["turn"]);
}

#[tokio::test]
async fn step_refusal_closes_the_admitted_turn_without_step_model_or_native_effects() {
    let plan = Rc::new(Plan {
        refusal: "step",
        ..Plan::default()
    });
    let effects = Rc::new(Effects::default());
    let runner = hosted(&plan, &effects, false);
    let mut session = session();
    assert!(
        runner
            .run_turn(&mut session, INPUT, &mut Silent, None)
            .await
            .is_err()
    );
    assert_eq!(
        *plan.trace.borrow(),
        ["turn", "step", "turn-end", "drop-turn"]
    );
    assert!(
        !session
            .log()
            .events()
            .iter()
            .any(|e| matches!(e, SessionEvent::StepStart { .. }))
    );
    assert!(session.log().last_turn_end().is_some());
    no_calls(&session);
    assert!(effects.started.borrow().is_empty());
}

#[tokio::test]
async fn model_refusal_precedes_copied_calls_policy_approval_and_executor() {
    let plan = Rc::new(Plan {
        refusal: "model",
        ..Plan::default()
    });
    let effects = Rc::new(Effects::default());
    let policy = policy(Ok(ToolPolicyDecision::AllowOnce), false);
    let base = runner(&effects, ToolAccess::Write, false).with_tool_policy(policy.clone());
    let runner = base.with_record_admission(plan.clone());
    let mut session = session();
    assert!(
        runner
            .run_turn(&mut session, INPUT, &mut Silent, None)
            .await
            .is_err()
    );
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
    assert!(policy.seen.borrow().is_empty());
    assert!(effects.started.borrow().is_empty());
    assert_eq!(effects.dropped.get(), 0);
    no_calls(&session);
    assert!(session.log().last_turn_end().is_some());
}

#[tokio::test]
async fn successful_tool_and_final_steps_hold_owned_capacity_until_each_original_end() {
    let plan = Rc::new(Plan::default());
    let effects = Rc::new(Effects::default());
    let runner = hosted(&plan, &effects, false);
    let mut session = session();
    let outcome = runner
        .run_turn(&mut session, INPUT, &mut Silent, None)
        .await
        .unwrap();
    assert_eq!(outcome.answer, "saved answer");
    assert_eq!(outcome.steps, 2);
    assert_eq!(effects.started.borrow().len(), 3);
    assert_eq!(
        *plan.trace.borrow(),
        [
            "turn",
            "step",
            "model",
            "step-end",
            "drop-step",
            "step",
            "model",
            "step-end",
            "drop-step",
            "turn-end",
            "drop-turn"
        ]
    );
    assert!(
        session
            .try_to_jsonl()
            .unwrap()
            .contains("Original\\nmessage")
    );
}

#[tokio::test]
async fn selection_switch_inside_step_reservation_waits_until_model_tools_and_closing_finish() {
    let plan = Rc::new(Plan {
        switch: true,
        ..Plan::default()
    });
    let effects = Rc::new(Effects::default());
    let runner = hosted(&plan, &effects, false);
    let mut session = session();
    runner
        .run_turn(&mut session, INPUT, &mut Silent, None)
        .await
        .unwrap();
    let models: Vec<_> = session
        .log()
        .events()
        .iter()
        .filter_map(|event| match event {
            SessionEvent::AssistantMessage { model, .. } => model.as_deref(),
            _ => None,
        })
        .collect();
    assert_eq!(models, ["host-model", "replacement"]);
    assert_eq!(runner.model(), "replacement");
    assert_eq!(effects.started.borrow().len(), 3);
}

#[tokio::test]
async fn closing_refusal_is_explicit_and_cannot_fabricate_a_completed_checkpoint() {
    for phase in ["step-end", "turn-end"] {
        let plan = Rc::new(Plan {
            refusal: phase,
            ..Plan::default()
        });
        let effects = Rc::new(Effects::default());
        let runner = hosted(&plan, &effects, false);
        let mut session = session();
        let result = runner
            .run_turn(&mut session, INPUT, &mut Silent, None)
            .await;
        assert!(result.is_err());
        assert_eq!(
            effects.started.borrow().len(),
            3,
            "earlier effects are not rolled back"
        );
        if phase == "turn-end" {
            assert!(session.log().last_turn_end().is_none());
        }
        assert_eq!(
            plan.trace
                .borrow()
                .iter()
                .filter(|s| s.as_str() == "drop-turn")
                .count(),
            1
        );
        assert!(
            !session
                .try_to_jsonl()
                .unwrap()
                .contains("private diagnostic")
        );
    }
}

#[tokio::test]
async fn pre_turn_cancellation_never_opens_an_unreserved_turn() {
    let control = Rc::new(Control::default());
    control.cancel();
    let plan = Rc::new(Plan::default());
    let effects = Rc::new(Effects::default());
    let runner = hosted(&plan, &effects, false);
    let mut session = session();
    let original = session.try_to_jsonl().unwrap();
    assert!(
        runner
            .run_turn_with_control(&mut session, INPUT, &mut Silent, None, control.as_ref())
            .await
            .is_err()
    );
    assert_eq!(original, session.try_to_jsonl().unwrap());
    assert!(plan.trace.borrow().is_empty());
    assert!(effects.started.borrow().is_empty());
}

#[tokio::test]
async fn cancellation_from_turn_or_step_reservation_rechecks_before_later_effects() {
    for phase in ["cancel-turn", "cancel-step"] {
        let control = Rc::new(Control::default());
        let plan = Rc::new(Plan {
            refusal: phase,
            cancel: Some(control.clone()),
            ..Plan::default()
        });
        let effects = Rc::new(Effects::default());
        let runner = hosted(&plan, &effects, false);
        let mut session = session();
        let result = runner
            .run_turn_with_control(&mut session, INPUT, &mut Silent, None, control.as_ref())
            .await;
        if phase == "cancel-turn" {
            assert!(result.is_err());
            assert!(session.log().events().is_empty());
        } else {
            assert_eq!(result.unwrap().reason, TurnEndReason::Interrupted);
        }
        no_calls(&session);
        assert!(effects.started.borrow().is_empty());
        assert_eq!(
            plan.trace
                .borrow()
                .iter()
                .filter(|s| s.as_str() == "drop-turn")
                .count(),
            1
        );
    }
}

#[tokio::test]
async fn dropped_quiet_model_future_releases_each_original_logical_reservation_once() {
    let plan = Rc::new(Plan::default());
    let effects = Rc::new(Effects::default());
    let runner = hosted(&plan, &effects, true);
    let mut session = session();
    let mut progress = Silent;
    let mut future = Box::pin(runner.run_turn(&mut session, INPUT, &mut progress, None));
    assert!(matches!(futures::poll!(future.as_mut()), Poll::Pending));
    assert_eq!(*plan.trace.borrow(), ["turn", "step"]);
    drop(future);
    assert_eq!(
        *plan.trace.borrow(),
        ["turn", "step", "drop-step", "drop-turn"]
    );
    assert!(effects.started.borrow().is_empty());
    no_calls(&session);
}

#[path = "records_more.rs"]
mod more;

#[path = "records_batch.rs"]
mod batch;
