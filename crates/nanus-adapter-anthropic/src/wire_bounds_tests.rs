use super::*;
fn bounded() -> StreamAccumulator {
    let mut accumulator = StreamAccumulator::with_prefix("a".repeat(64));
    accumulator.set_response_limits(Some(
        nanus_ports::ResponseLimits::new(1024, 512, 8192, 32, 2, 256).expect("limits"),
    ));
    accumulator
}
fn drain(accumulator: &mut StreamAccumulator) -> Vec<LlmEvent> {
    std::iter::from_fn(|| accumulator.take_ready()).collect()
}
fn thinking(index: &Value) -> Value {
    json!({"type":"content_block_start","index":index,"content_block":{"type":"thinking","thinking":"","signature":"signed"}})
}
#[test]
fn bounded_block_indexes_refuse_non_numbers_and_out_of_range_values() {
    let mut accumulator = bounded();
    accumulator.observe_frame(&thinking(&json!(1)));
    accumulator.close();
    assert!(
        drain(&mut accumulator)
            .iter()
            .any(|event| matches!(event, LlmEvent::AssistantReplay(_)))
    );
    for index in [json!(2), json!(u64::MAX), json!(-1), json!("0")] {
        let mut accumulator = bounded();
        accumulator.observe_frame(&thinking(&index));
        accumulator.close();
        assert!(matches!(
            drain(&mut accumulator).as_slice(),
            [LlmEvent::Error(_)]
        ));
        assert!(accumulator.replay_blocks.is_empty());
    }
}
#[test]
fn replay_checks_full_json_wrapper_and_escaping_before_emission() {
    let mut accumulator = bounded();
    accumulator.observe_frame(&thinking(&json!(0)));
    for _ in 0..2 {
        accumulator.observe_frame(&json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"\u{1}".repeat(40)}}));
    }
    accumulator.close();
    assert!(
        matches!(drain(&mut accumulator).as_slice(),[LlmEvent::Error(message)] if message.contains("assistant replay bytes"))
    );
}
#[test]
fn replay_append_limits_refuse_growth_across_small_frames() {
    let mut accumulator = bounded();
    accumulator.observe_frame(&thinking(&json!(0)));
    for _ in 0..3 {
        accumulator.observe_frame(&json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"x".repeat(200)}}));
    }
    accumulator.close();
    assert!(matches!(
        drain(&mut accumulator).as_slice(),
        [LlmEvent::Error(_)]
    ));
    assert!(accumulator.replay_blocks.is_empty());
}
