use super::*;
fn bounded() -> StreamAccumulator {
    let mut accumulator = StreamAccumulator::default();
    accumulator.set_response_limits(Some(
        nanus_ports::ResponseLimits::new(1024, 512, 8192, 32, 2, 256).expect("limits"),
    ));
    accumulator
}
fn drain(accumulator: &mut StreamAccumulator) -> Vec<LlmEvent> {
    std::iter::from_fn(|| accumulator.take_ready()).collect()
}
fn call(index: &Value, fragment: &str) -> Value {
    json!({"choices":[{"delta":{"tool_calls":[{"index":index,"id":"id","function":{"name":"read","arguments":fragment}}]}}]})
}
#[test]
fn bounded_indexes_never_fold_into_slot_zero() {
    let mut accumulator = bounded();
    accumulator.observe_frame(&call(&json!(1), "{}"));
    accumulator.close();
    assert!(
        drain(&mut accumulator)
            .iter()
            .any(|event| matches!(event, LlmEvent::ToolCallDelta { index: 1, .. }))
    );
    for index in [json!(2), json!(u64::MAX), json!(-1), json!("0")] {
        let mut accumulator = bounded();
        accumulator.observe_frame(&call(&index, "{}"));
        accumulator.close();
        assert!(matches!(
            drain(&mut accumulator).as_slice(),
            [LlmEvent::Error(_)]
        ));
        assert!(accumulator.calls.is_empty());
    }
}
#[test]
fn assembled_arguments_cannot_evade_the_event_budget() {
    let mut accumulator = bounded();
    for _ in 0..2 {
        accumulator.observe_frame(&call(&json!(0), &"x".repeat(200)));
    }
    accumulator.close();
    assert!(drain(&mut accumulator).iter().any(|event| matches!(event,LlmEvent::ToolCallDelta{arguments_delta,..} if arguments_delta.len()==400)));
    let mut accumulator = bounded();
    for _ in 0..3 {
        accumulator.observe_frame(&call(&json!(0), &"x".repeat(200)));
    }
    accumulator.close();
    assert!(matches!(
        drain(&mut accumulator).as_slice(),
        [LlmEvent::Error(_)]
    ));
    assert!(accumulator.calls.is_empty());
}
#[test]
fn malformed_payload_discards_pending_calls_without_completion() {
    let mut accumulator = bounded();
    accumulator.observe_frame(&call(&json!(0), "{}"));
    accumulator.observe_line("not JSON");
    accumulator.close();
    assert!(matches!(
        drain(&mut accumulator).as_slice(),
        [LlmEvent::Error(_)]
    ));
}

#[test]
fn a_partially_named_call_cannot_finish_as_an_empty_success() {
    let mut accumulator = bounded();
    accumulator.observe_frame(&json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"id","function":{"arguments":"{}"}}]}}]}));
    accumulator.close();
    assert!(matches!(
        drain(&mut accumulator).as_slice(),
        [LlmEvent::Error(_)]
    ));
    accumulator.observe_line("still not JSON");
    assert!(drain(&mut accumulator).is_empty());
}
