//! Combine original model-record ownership with the existing generic batch callbacks.
use super::*;
use nanus_ports::{ToolAdmission, ToolBatchProjection, ToolBatchReservation};

struct Batch(Rc<RefCell<Vec<String>>>);
impl ToolAdmission for Batch {
    fn reserve(
        &self,
        view: &ToolBatchProjection<'_>,
    ) -> Result<Box<dyn ToolBatchReservation>, AdmissionError> {
        assert_eq!(self.0.borrow().last().map(String::as_str), Some("model"));
        let assistant = view
            .events
            .iter()
            .rev()
            .find_map(|e| match e {
                SessionEvent::AssistantMessage { tool_calls, .. } => Some(tool_calls),
                _ => None,
            })
            .unwrap();
        assert_eq!(assistant, view.calls);
        assert_eq!(
            view.events
                .iter()
                .rev()
                .take_while(|e| matches!(e, SessionEvent::ToolCall { .. }))
                .count(),
            view.calls.len()
        );
        assert!(!self.0.borrow().iter().any(|s| s == "drop-step"));
        self.0.borrow_mut().push("batch".into());
        Ok(Box::new(Self(self.0.clone())))
    }
}
impl ToolBatchReservation for Batch {
    fn admit(&self, _: &ToolCall) -> Result<(), AdmissionError> {
        self.0.borrow_mut().push("admit".into());
        Ok(())
    }
    fn before_dispatch(&self, _: &ToolCall) -> Result<(), AdmissionError> {
        self.0.borrow_mut().push("dispatch".into());
        Ok(())
    }
    fn validate_result(
        &self,
        call: &ToolCall,
        raw: &ToolResult,
        retained: &ToolResult,
    ) -> Result<(), AdmissionError> {
        assert_eq!(raw.call_id, call.id);
        assert_eq!(retained.call_id, call.id);
        self.0.borrow_mut().push("result".into());
        Ok(())
    }
    fn commit(&self, _: &ChatRequest) -> Result<(), AdmissionError> {
        self.0.borrow_mut().push("commit".into());
        Ok(())
    }
}
impl Drop for Batch {
    fn drop(&mut self) {
        self.0.borrow_mut().push("drop-batch".into());
    }
}

#[tokio::test]
async fn original_model_prefix_reaches_batch_and_record_lease_outlives_ordered_commit() {
    let plan = Rc::new(Plan::default());
    let effects = Rc::new(Effects::default());
    let runner = runner(&effects, ToolAccess::Read, false)
        .with_record_admission(plan.clone())
        .with_tool_admission(Rc::new(Batch(plan.trace.clone())));
    let mut session = session();
    runner
        .run_turn(&mut session, INPUT, &mut Silent, None)
        .await
        .unwrap();
    let trace = plan.trace.borrow();
    let at = |name: &str| trace.iter().position(|s| s == name).unwrap();
    assert!(at("model") < at("batch"));
    assert!(at("batch") < at("dispatch"));
    assert!(at("commit") < at("drop-batch"));
    assert!(at("drop-batch") < at("step-end"));
    assert!(at("step-end") < at("drop-step"));
    assert_eq!(effects.started.borrow().len(), 3);
    assert_eq!(trace.iter().filter(|s| s.as_str() == "turn").count(), 1);
}

#[tokio::test]
async fn record_admission_only_holds_overlapping_steps_until_the_last_future_drops() {
    let plan = Rc::new(Plan::default());
    let effects = Rc::new(Effects::default());
    let runner = hosted(&plan, &effects, true);
    let mut first = session();
    let mut second = session();
    let mut first_progress = Silent;
    let mut second_progress = Silent;
    let mut a = Box::pin(runner.run_turn(&mut first, INPUT, &mut first_progress, None));
    let mut b = Box::pin(runner.run_turn(&mut second, INPUT, &mut second_progress, None));
    assert!(matches!(futures::poll!(a.as_mut()), Poll::Pending));
    assert!(matches!(futures::poll!(b.as_mut()), Poll::Pending));
    runner.set_model("latest-model");
    runner.set_effort(Some(nanus_ports::ReasoningEffort::Max));
    assert_eq!(runner.model(), "host-model");
    drop(a);
    assert_eq!(runner.model(), "host-model");
    drop(b);
    assert_eq!(runner.model(), "latest-model");
    assert_eq!(runner.effort(), Some(nanus_ports::ReasoningEffort::Max));
    assert_eq!(
        plan.trace
            .borrow()
            .iter()
            .filter(|s| s.as_str() == "drop-step")
            .count(),
        2
    );
    assert_eq!(
        plan.trace
            .borrow()
            .iter()
            .filter(|s| s.as_str() == "drop-turn")
            .count(),
        2
    );
}

#[tokio::test]
async fn next_explicit_turn_uses_original_retained_history_without_replaying_native_calls() {
    let plan = Rc::new(Plan::default());
    let effects = Rc::new(Effects::default());
    let runner = hosted(&plan, &effects, false);
    let mut session = session();
    runner
        .run_turn(&mut session, INPUT, &mut Silent, None)
        .await
        .unwrap();
    let original = session.log().events().to_vec();
    runner
        .run_turn(&mut session, INPUT, &mut Silent, None)
        .await
        .unwrap();
    assert_eq!(&session.log().events()[..original.len()], original);
    assert_eq!(effects.started.borrow().len(), 3);
    assert_eq!(session.log().current_turn(), 2);
    assert_eq!(
        plan.trace
            .borrow()
            .iter()
            .filter(|s| s.as_str() == "turn")
            .count(),
        2
    );
    assert_eq!(
        plan.trace
            .borrow()
            .iter()
            .filter(|s| s.as_str() == "drop-turn")
            .count(),
        2
    );
}
