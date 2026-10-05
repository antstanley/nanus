//! Real Responses accumulator: original completed items precede executable calls and Finished.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
use nanus_adapter_openai::responses::StreamAccumulator;
use nanus_domain::message::AssistantReplay;
use nanus_ports::{LlmEvent, ResponseLimits};
use serde_json::{Value, json};

const BYTES: usize = 1024 * 1024;

fn limits(bytes: usize, frames: usize, slots: usize) -> ResponseLimits {
    ResponseLimits::new(bytes, bytes, bytes, frames, slots, bytes).unwrap()
}

fn items() -> Vec<Value> {
    vec![
        json!({"type":"reasoning","id":"reason-1","summary":[],
            "encrypted_content":"opaque+/=fixture","status":"completed"}),
        json!({"type":"function_call","id":"item-1","call_id":"call-1","name":"read",
            "arguments":"{\"path\":\"first\"}","status":"completed"}),
        json!({"type":"function_call","id":"item-2","call_id":"call-2","name":"read",
            "arguments":"{\"path\":\"second\"}","status":"completed"}),
        json!({"type":"message","id":"message-1","role":"assistant","phase":"commentary",
            "status":"completed","content":[{"type":"output_text","text":"Reading both.",
            "annotations":[{"type":"url_citation","start_index":0,"end_index":7,
                "url":"https://example.test/fictional","title":"Fictional"}]}]}),
    ]
}

fn frames_for(items: &[Value]) -> Vec<Value> {
    let mut frames = vec![json!({"type":"response.created", "response":{
        "id":"response-1","status":"in_progress"}})];
    for (index, item) in items.iter().enumerate() {
        let mut added = item.clone();
        added["status"] = json!("in_progress");
        match item["type"].as_str().unwrap() {
            "reasoning" => {
                added["encrypted_content"] = Value::Null;
                added["summary"] = json!([]);
            }
            "function_call" => added["arguments"] = json!(""),
            "message" => added["content"] = json!([]),
            _ => panic!("fixture item kind"),
        }
        frames.push(json!({"type":"response.output_item.added","output_index":index,"item":added}));
        if item["type"] == "function_call" {
            frames.push(
                json!({"type":"response.function_call_arguments.delta","output_index":index,
                "item_id":item["id"],"delta":item["arguments"]}),
            );
        }
        if let Some(content) = item["content"].as_array() {
            for (part, text) in content.iter().enumerate() {
                let refusal = text["type"] == "refusal";
                frames.push(json!({"type":if refusal {"response.refusal.delta"}
                    else {"response.output_text.delta"},"output_index":index,
                    "content_index":part,"item_id":item["id"],
                    "delta":text[if refusal {"refusal"} else {"text"}]}));
            }
        }
        frames.push(json!({"type":"response.output_item.done","output_index":index,"item":item}));
    }
    frames.push(
        json!({"type":"response.completed","response":{"id":"response-1",
        "status":"completed","output":items,"usage":{"input_tokens":100,"output_tokens":20,"total_tokens":120}}}),
    );
    frames
}

fn drain(accumulator: &mut StreamAccumulator) -> Vec<LlmEvent> {
    let mut events = Vec::new();
    while let Some(event) = accumulator.take_ready() {
        events.push(event);
    }
    events
}

fn observe(frames: &[Value], limits: ResponseLimits) -> Vec<LlmEvent> {
    let mut accumulator = StreamAccumulator::with_prefix("a".repeat(64), limits).unwrap();
    for frame in frames {
        accumulator.observe_line(&serde_json::to_string(frame).unwrap());
    }
    accumulator.close();
    drain(&mut accumulator)
}

fn refused(events: &[LlmEvent]) {
    assert!(
        events
            .iter()
            .any(|event| matches!(event, LlmEvent::Error(_))),
        "{events:?}"
    );
    assert!(
        !events.iter().any(|event| matches!(
            event,
            LlmEvent::ToolCallDelta { .. }
                | LlmEvent::AssistantReplay(_)
                | LlmEvent::Usage(_)
                | LlmEvent::Finished { .. }
        )),
        "{events:?}"
    );
}

#[test]
fn completed_original_ciphertext_phase_annotations_and_sibling_calls_survive_before_finished() {
    let items = items();
    let frames = frames_for(&items);
    let mut accumulator =
        StreamAccumulator::with_prefix("A".repeat(64), limits(BYTES, 100, 256)).unwrap();
    for frame in &frames {
        accumulator.observe_frame(frame);
    }
    let before = drain(&mut accumulator);
    assert!(matches!(&before[..], [LlmEvent::TextDelta(text)] if text == "Reading both."));
    accumulator.close();
    let after = drain(&mut accumulator);
    let LlmEvent::AssistantReplay(replay) = &after[0] else {
        panic!("original replay first");
    };
    assert_eq!(replay.blocks, items);
    assert_eq!(replay.prefix_digest, "A".repeat(64));
    let loaded: AssistantReplay =
        serde_json::from_str(&serde_json::to_string(replay).unwrap()).unwrap();
    assert_eq!(loaded, *replay);
    assert!(
        matches!(&after[1], LlmEvent::ToolCallDelta { id:Some(id), .. } if id.as_str()=="call-1")
    );
    assert!(
        matches!(&after[2], LlmEvent::ToolCallDelta { id:Some(id), .. } if id.as_str()=="call-2")
    );
    assert!(matches!(after[3], LlmEvent::Usage(_)));
    assert!(matches!(after[4], LlmEvent::Finished { .. }));
    accumulator.close();
    assert!(drain(&mut accumulator).is_empty());
}

#[test]
fn changed_missing_duplicate_or_reordered_observations_never_release_calls_or_success() {
    let valid = frames_for(&items());
    let call_delta = valid
        .iter()
        .position(|f| f["type"] == "response.function_call_arguments.delta")
        .unwrap();
    let call_done = valid
        .iter()
        .position(|f| {
            f["type"] == "response.output_item.done" && f["item"]["type"] == "function_call"
        })
        .unwrap();
    let message_delta = valid
        .iter()
        .position(|f| f["type"] == "response.output_text.delta")
        .unwrap();
    let terminal = valid.len().checked_sub(1).unwrap();
    for (index, field, bad) in [
        (call_delta, "item_id", json!("crossed")),
        (call_delta, "output_index", json!(99)),
        (message_delta, "item_id", json!("crossed")),
        (message_delta, "content_index", json!(256)),
        (message_delta, "delta", json!("changed")),
    ] {
        let mut frames = valid.clone();
        frames[index][field] = bad;
        refused(&observe(&frames, limits(BYTES, 100, 256)));
    }
    for (field, bad) in [
        ("id", json!("other")),
        ("call_id", json!("other")),
        ("name", json!("write")),
        ("arguments", json!("{ \"path\": \"first\" }")),
        ("status", json!("incomplete")),
    ] {
        let mut frames = valid.clone();
        frames[call_done]["item"][field] = bad;
        refused(&observe(&frames, limits(BYTES, 100, 256)));
    }
    for index in [0, 1, call_delta, call_done, terminal] {
        let mut frames = valid.clone();
        frames.remove(index);
        refused(&observe(&frames, limits(BYTES, 100, 256)));
    }
    for index in [1, call_done, terminal] {
        let mut frames = valid.clone();
        frames.insert(index, frames[index].clone());
        refused(&observe(&frames, limits(BYTES, 100, 256)));
    }
    let mut crossed = valid.clone();
    crossed[terminal]["response"]["id"] = json!("other");
    refused(&observe(&crossed, limits(BYTES, 100, 256)));
    let mut reordered = valid;
    reordered[terminal]["response"]["output"]
        .as_array_mut()
        .unwrap()
        .swap(1, 2);
    refused(&observe(&reordered, limits(BYTES, 100, 256)));
}

#[test]
fn incomplete_failed_truncated_cancelled_and_hosted_outputs_have_no_executable_replay() {
    let valid = frames_for(&items());
    for prefix in 0..valid.len() {
        refused(&observe(&valid[..prefix], limits(BYTES, 100, 256)));
    }
    for kind in ["response.incomplete", "response.failed", "error"] {
        let mut frames = valid.clone();
        frames.pop();
        frames.push(json!({"type":kind}));
        refused(&observe(&frames, limits(BYTES, 100, 256)));
    }
    let mut frames = valid.clone();
    frames[1]["item"]["type"] = json!("web_search_call");
    refused(&observe(&frames, limits(BYTES, 100, 256)));
    let mut frames = valid;
    frames[2]["item"]["encrypted_content"] = json!("");
    refused(&observe(&frames, limits(BYTES, 100, 256)));
}

#[test]
fn frame_item_and_cumulative_byte_boundaries_refuse_the_next_unit_and_cannot_be_disabled() {
    let frames = frames_for(&items());
    let bytes: usize = frames
        .iter()
        .map(|frame| serde_json::to_vec(frame).unwrap().len())
        .sum();
    let accepted = observe(&frames, limits(bytes, frames.len(), 4));
    assert!(matches!(accepted.last(), Some(LlmEvent::Finished { .. })));
    refused(&observe(
        &frames,
        limits(bytes.checked_sub(1).unwrap(), frames.len(), 4),
    ));
    refused(&observe(
        &frames,
        limits(bytes, frames.len().checked_sub(1).unwrap(), 4),
    ));
    refused(&observe(&frames, limits(bytes, frames.len(), 3)));
    let mut accumulator =
        StreamAccumulator::with_prefix("a".repeat(64), limits(bytes, frames.len(), 3)).unwrap();
    accumulator.set_response_limits(None);
    for frame in &frames {
        accumulator.observe_frame(frame);
    }
    accumulator.close();
    refused(&drain(&mut accumulator));
    for digest in [
        String::new(),
        "a".repeat(63),
        "g".repeat(64),
        "a".repeat(65),
    ] {
        assert!(StreamAccumulator::with_prefix(digest, limits(BYTES, 100, 256)).is_err());
    }
}

#[test]
fn refusal_text_is_retained_and_contradictory_done_arguments_or_text_refuse() {
    let mut items = items();
    items[3]["content"] = json!([{"type":"refusal","refusal":"Fictional refusal."}]);
    items[3]["phase"] = json!("final_answer");
    let frames = frames_for(&items);
    let mut stock = StreamAccumulator::default();
    stock.observe_frame(&json!({"type":"response.refusal.delta","delta":"stock unchanged"}));
    assert!(drain(&mut stock).is_empty());
    let accepted = observe(&frames, limits(BYTES, 100, 256));
    assert!(matches!(&accepted[0],LlmEvent::TextDelta(text) if text=="Fictional refusal."));
    assert!(matches!(accepted.last(), Some(LlmEvent::Finished { .. })));
    for frame in [
        json!({"type":"response.function_call_arguments.done","output_index":1,
            "item_id":"item-1","arguments":"{}"}),
        json!({"type":"response.refusal.done","output_index":3,
            "content_index":0,"item_id":"message-1","refusal":"changed"}),
    ] {
        let mut bad = frames.clone();
        let index = bad
            .iter()
            .position(|f| {
                f["type"] == "response.output_item.done"
                    && f["output_index"] == frame["output_index"]
            })
            .unwrap();
        bad.insert(index, frame);
        refused(&observe(&bad, limits(BYTES, 100, 256)));
    }
}

#[test]
fn contradictory_initial_phase_or_terminal_usage_cannot_become_a_success_receipt() {
    let valid = frames_for(&items());
    let terminal = valid.len().checked_sub(1).unwrap();
    for usage in [
        json!({}),
        json!({"input_tokens":100,"output_tokens":20,"total_tokens":121}),
        json!({"input_tokens":-1,"output_tokens":20,"total_tokens":19}),
        json!({"input_tokens":100,"output_tokens":20,"total_tokens":120,
            "output_tokens_details":{"reasoning_tokens":21}}),
        json!({"input_tokens":100,"output_tokens":20,"total_tokens":120,
            "input_tokens_details":{"cached_tokens":101}}),
    ] {
        let mut bad = valid.clone();
        bad[terminal]["response"]["usage"] = usage;
        refused(&observe(&bad, limits(BYTES, 100, 256)));
    }
    for field in ["error", "incomplete_details"] {
        let mut bad = valid.clone();
        bad[terminal]["response"][field] = json!("malformed error");
        refused(&observe(&bad, limits(BYTES, 100, 256)));
    }
    let index = valid
        .iter()
        .position(|frame| {
            frame["type"] == "response.output_item.added" && frame["item"]["type"] == "message"
        })
        .unwrap();
    for (field, value) in [
        ("phase", json!("final_answer")),
        ("role", json!("user")),
        ("extra", json!(true)),
        (
            "content",
            json!([{"type":"output_text","text":"unobserved"}]),
        ),
    ] {
        let mut bad = valid.clone();
        bad[index]["item"][field] = value;
        refused(&observe(&bad, limits(BYTES, 100, 256)));
    }
    let mut no_usage = valid;
    no_usage[terminal]["response"]
        .as_object_mut()
        .unwrap()
        .remove("usage");
    let accepted = observe(&no_usage, limits(BYTES, 100, 256));
    assert!(matches!(accepted.last(), Some(LlmEvent::Finished { .. })));
    assert!(
        !accepted
            .iter()
            .any(|event| matches!(event, LlmEvent::Usage(_)))
    );
}

#[test]
fn complete_replay_envelope_and_escaping_are_counted_before_retaining_the_last_item() {
    let item = json!({"id":"r","type":"reasoning","summary":[],
        "encrypted_content":"quote\"\\\n","status":"completed"});
    let mut frames = frames_for(std::slice::from_ref(&item));
    frames.last_mut().unwrap()["response"]
        .as_object_mut()
        .unwrap()
        .remove("usage");
    let envelope = AssistantReplay {
        protocol: "openai.responses".into(),
        prefix_digest: "a".repeat(64),
        context_receipt: None,
        blocks: vec![item],
    };
    let bytes = serde_json::to_vec(&envelope).unwrap().len();
    let limits = |bytes| ResponseLimits::new(bytes, bytes, BYTES, 100, 256, bytes).unwrap();
    let accepted = observe(&frames, limits(bytes));
    assert!(matches!(accepted.last(), Some(LlmEvent::Finished { .. })));
    let small = limits(bytes.checked_sub(1).unwrap());
    let mut accumulator = StreamAccumulator::with_prefix("a".repeat(64), small).unwrap();
    // The done item fits as an SSE payload, but retaining its full replay envelope would not.
    for frame in &frames[..3] {
        accumulator.observe_frame(frame);
    }
    assert!(accumulator.is_closed());
    let failed = drain(&mut accumulator);
    assert!(
        matches!(&failed[..],[LlmEvent::Error(message)] if message.contains("assistant replay bytes"))
    );
}

#[test]
fn admitted_context_receipts_survive_original_items_and_count_before_each_clone() {
    use nanus_domain::message::ReplayContext;
    let receipt = ReplayContext {
        budget: 64000,
        dropped_turns: 1,
        dropped_messages: 4,
        source_digest: "b".repeat(64),
        wire_digest: "c".repeat(64),
    };
    let item = json!({"id":"r","type":"reasoning","summary":[],
        "encrypted_content":"opaque-fixture","status":"completed"});
    let frames = frames_for(std::slice::from_ref(&item));
    let expected = AssistantReplay {
        protocol: "openai.responses".into(),
        prefix_digest: "a".repeat(64),
        context_receipt: Some(Box::new(receipt.clone())),
        blocks: vec![item],
    };
    let bytes = serde_json::to_vec(&expected).unwrap().len();
    let bounded = |bytes| ResponseLimits::new(bytes, bytes, BYTES, 100, 256, bytes).unwrap();
    let mut accumulator =
        StreamAccumulator::with_context("a".repeat(64), receipt.clone(), bounded(bytes)).unwrap();
    for frame in &frames {
        accumulator.observe_frame(frame);
    }
    accumulator.close();
    let events = drain(&mut accumulator);
    assert!(matches!(&events[0],LlmEvent::AssistantReplay(replay) if replay==&expected));
    assert!(matches!(events.last(), Some(LlmEvent::Finished { .. })));
    let mut small = StreamAccumulator::with_context(
        "a".repeat(64),
        receipt.clone(),
        bounded(bytes.checked_sub(1).unwrap()),
    )
    .unwrap();
    for frame in &frames[..3] {
        small.observe_frame(frame);
    }
    assert!(small.is_closed());
    refused(&drain(&mut small));
    let mut invalid = receipt;
    invalid.dropped_turns = 0;
    assert!(StreamAccumulator::with_context("a".repeat(64), invalid, bounded(bytes)).is_err());
}
