use super::*;
use nanus_domain::message::ReplayInstructions;

#[test]
fn snapshot_count_and_escaped_bytes_admit_exact_limits_and_refuse_next_unit() {
    let adapter = OpenAiLlm::new(revision_config()).unwrap();
    let mut messages = vec![Message::system(""); ReplayInstructions::MESSAGES_MAX];
    messages.push(Message::user("Fictional user"));
    assert!(
        adapter
            .prepare_responses(&request(messages.clone()))
            .is_ok()
    );
    messages.insert(0, Message::system(""));
    assert!(adapter.prepare_responses(&request(messages)).is_err());
    let mut request = request(vec![Message::system(""), Message::user("Fictional")]);
    let prepared = adapter.prepare_responses(&request).unwrap();
    let snapshot = prepared.context_receipt.instructions.unwrap();
    let remaining = ReplayInstructions::BYTES_MAX - serde_json::to_vec(&snapshot).unwrap().len();
    let text = "\0".repeat(remaining.checked_div(6).unwrap()) + &"x".repeat(remaining % 6);
    request.messages[0] = Message::system(text.clone());
    let cost = adapter
        .estimate_request(&request)
        .expect("bounded prospective cost");
    assert!(!cost.fits(adapter.capabilities(&request.model), &request));
    assert!(adapter.prepare_responses(&request).is_err());
    request.messages[0] = Message::system(text + "x");
    assert!(adapter.estimate_request(&request).is_err());
}

#[test]
fn malformed_or_unknown_nested_snapshot_shapes_cannot_enter_replay_history() {
    let adapter = OpenAiLlm::new(revision_config()).unwrap();
    let request = continuation(&adapter);
    let original = serde_json::to_value(&request.messages[2]).unwrap();
    assert!(original["assistant_replay"]["context_receipt"]["instructions"].is_object());
    for (field, value) in [
        ("version", json!(2)),
        ("version", json!(-1)),
        ("revision", json!("bad")),
        ("messages", json!([false])),
        (
            "messages",
            json!(["x".repeat(ReplayInstructions::BYTES_MAX)]),
        ),
        ("extra", json!(true)),
    ] {
        let mut bad = original.clone();
        bad["assistant_replay"]["context_receipt"]["instructions"][field] = value;
        assert!(serde_json::from_value::<Message>(bad).is_err(), "{field}");
    }
}

#[test]
fn instruction_receipts_consume_the_selected_decoder_capacity() {
    let mut small = revision_config();
    small.set_response_limits(ResponseLimits::new(4096, 100, 4096, 100, 16, 4096).unwrap());
    let small = OpenAiLlm::new(small).unwrap();
    let request = request(vec![
        Message::system("Bounded prompt"),
        Message::user("Fictional"),
    ]);
    assert!(small.prepare_responses(&request).is_err());
    let adapter = OpenAiLlm::new(revision_config()).unwrap();
    let prepared = adapter.prepare_responses(&request).unwrap();
    let bytes = serde_json::to_vec(&prepared.context_receipt).unwrap().len();
    let limits = ResponseLimits::new(8192, bytes + 100, 8192, 100, 16, 8192).unwrap();
    let mut decoder =
        StreamAccumulator::with_context(prepared.prefix_digest, prepared.context_receipt, limits)
            .unwrap();
    decoder.observe_line(
        &json!({"type":"response.created","response":{
        "id":"response_original","status":"in_progress"}})
        .to_string(),
    );
    observe_item(&mut decoder, 0, &original_items()[0]);
    decoder.close();
    let mut capacity = false;
    while let Some(event) = decoder.take_ready() {
        match event {
            LlmEvent::Error(error) => {
                assert!(error.contains("assistant replay bytes"), "{error}");
                capacity = true;
            }
            LlmEvent::AssistantReplay(_) | LlmEvent::Finished { .. } => panic!("refused replay"),
            _ => {}
        }
    }
    assert!(capacity);
}
