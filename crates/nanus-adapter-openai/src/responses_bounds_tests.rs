use super::*;
use serde_json::json;
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
fn added(id: &str, call_id: &str, name: &str) -> Value {
    json!({"type":"response.output_item.added","item":{"type":"function_call","id":id,"call_id":call_id,"name":name,"arguments":""}})
}
#[test]
fn too_many_response_calls_fail_without_partial_completion() {
    let mut accumulator = bounded();
    for id in ["a", "b"] {
        accumulator.observe_frame(&added(id, id, "read"));
    }
    accumulator.close();
    assert_eq!(
        drain(&mut accumulator)
            .iter()
            .filter(|event| matches!(event, LlmEvent::ToolCallDelta { .. }))
            .count(),
        2
    );
    let mut accumulator = bounded();
    for id in ["a", "b", "c"] {
        accumulator.observe_frame(&added(id, id, "read"));
    }
    accumulator.close();
    assert!(matches!(
        drain(&mut accumulator).as_slice(),
        [LlmEvent::Error(_)]
    ));
}
#[test]
fn response_done_keeps_omitted_fields_in_its_prospective_budget() {
    let mut accumulator = bounded();
    accumulator.observe_frame(&added("a", &"i".repeat(64), &"n".repeat(64)));
    let done = json!({"type":"response.output_item.done","item":{"type":"function_call","id":"a","arguments":"x".repeat(400)}});
    assert!(nanus_domain::content::serialized_size(&done, 512).is_ok());
    accumulator.observe_frame(&done);
    accumulator.close();
    assert!(matches!(
        drain(&mut accumulator).as_slice(),
        [LlmEvent::Error(_)]
    ));
    assert!(accumulator.calls.is_empty());
}
#[test]
fn response_argument_fragments_cannot_evade_the_call_budget() {
    let mut accumulator = bounded();
    accumulator.observe_frame(&added("a", "id", "read"));
    for _ in 0..3 {
        accumulator.observe_frame(&json!({"type":"response.function_call_arguments.delta","item_id":"a","delta":"x".repeat(200)}));
    }
    accumulator.close();
    assert!(matches!(
        drain(&mut accumulator).as_slice(),
        [LlmEvent::Error(_)]
    ));
}
