//! Original opaque Responses items agree with the neutral response and survive v2 persistence.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use nanus_domain::message::AssistantReplay;
use nanus_domain::{Message, Session, SessionEvent, SessionId, ToolCall, ToolCallId, ToolName};
use serde_json::{Value, json};

fn reasoning() -> Value {
    json!({"type":"reasoning", "id":"rs_original", "summary":[],
        "encrypted_content":"opaque-fictional-ciphertext", "status":null, "content":[]})
}
fn message() -> Value {
    json!({"type":"message", "id":"msg_original", "role":"assistant", "status":"completed",
        "phase":"commentary", "content":[{"type":"output_text", "text":"original text",
            "annotations":[], "logprobs":[]}]})
}
fn function(item: &str, call: &str, path: &str) -> Value {
    json!({"type":"function_call", "id":item, "call_id":call, "name":"read",
        "arguments":json!({"path":path}).to_string(), "status":"completed"})
}
fn replay() -> AssistantReplay {
    AssistantReplay {
        protocol: "openai.responses".into(),
        prefix_digest: "A".repeat(64),
        context_receipt: None,
        blocks: vec![
            reasoning(),
            message(),
            function("fc_1", "original_1", "fictional-a.txt"),
            function("fc_2", "original_2", "fictional-b.txt"),
        ],
    }
}

#[test]
fn context_receipts_are_closed_bounded_optional_and_cannot_be_relabelled_as_messages() {
    use nanus_domain::message::ReplayContext;
    let mut replay = replay();
    assert!(
        serde_json::to_value(&replay)
            .unwrap()
            .get("context_receipt")
            .is_none()
    );
    let receipt = ReplayContext {
        instructions: None,
        budget: 64000,
        dropped_turns: 1,
        dropped_messages: 3,
        source_digest: "a".repeat(64),
        wire_digest: "B".repeat(64),
    };
    replay.context_receipt = Some(Box::new(receipt.clone()));
    let value = serde_json::to_value(&replay).unwrap();
    assert_eq!(
        serde_json::from_value::<AssistantReplay>(value.clone()).unwrap(),
        replay
    );
    for (field, bad) in [
        ("budget", json!(0)),
        ("budget", json!(u64::MAX)),
        ("dropped_messages", json!(0)),
        ("dropped_messages", json!(-1)),
        ("dropped_turns", json!(4)),
        ("source_digest", json!("g".repeat(64))),
        ("wire_digest", json!("a".repeat(65))),
        ("extra", json!(true)),
    ] {
        let mut bad_value = value.clone();
        bad_value["context_receipt"][field] = bad;
        assert!(serde_json::from_value::<AssistantReplay>(bad_value).is_err());
    }
    let mut crossed = value;
    crossed["protocol"] = json!("anthropic.messages");
    assert!(serde_json::from_value::<AssistantReplay>(crossed).is_err());
    let mut impossible = receipt;
    impossible.dropped_turns = 0;
    assert!(impossible.validate().is_err());
}

#[test]
fn completed_opaque_only_reasoning_survives_source_folding_and_v2_reload() {
    let mut replay = replay();
    replay.blocks = vec![reasoning()];
    let mut session = Session::new(SessionId::new("opaque-only"), 123, "/fictional");
    session.append(SessionEvent::AssistantMessage {
        replay: Some(replay.clone()),
        text: None,
        reasoning: None,
        tool_calls: vec![],
        usage: None,
        interrupted: false,
        model: Some("gpt-6-astra".into()),
        effort: None,
    });
    let loaded = Session::from_jsonl(&session.try_to_jsonl().unwrap()).unwrap();
    let messages = loaded.derive_messages();
    assert_eq!(messages.len(), 1);
    assert!(!messages[0].is_empty());
    assert!(
        matches!(&messages[0],Message::Assistant {replay:Some(original), ..} if original==&replay)
    );
}
fn calls() -> Vec<ToolCall> {
    [
        ("original_1", "fictional-a.txt"),
        ("original_2", "fictional-b.txt"),
    ]
    .into_iter()
    .map(|(id, path)| {
        ToolCall::new(
            ToolCallId::new(id),
            ToolName::new("read").unwrap(),
            json!({"path":path}),
        )
    })
    .collect()
}

#[test]
fn ordered_empty_reasoning_phase_sibling_calls_and_ciphertext_remain_original_bytes() {
    let replay = replay();
    assert!(
        replay
            .validate_response(Some("original text"), &calls())
            .is_ok()
    );
    let raw = serde_json::to_string(&replay).unwrap();
    assert_eq!(
        serde_json::from_str::<AssistantReplay>(&raw).unwrap(),
        replay
    );
    assert_eq!(replay.blocks[0]["summary"], json!([]));
    assert_eq!(
        replay.blocks[0]["encrypted_content"],
        "opaque-fictional-ciphertext"
    );
    assert_eq!(replay.blocks[1]["phase"], "commentary");
    assert_eq!(replay.blocks[2]["id"], "fc_1");
    assert_eq!(replay.blocks[2]["call_id"], "original_1");
    assert!(
        replay
            .validate_response(Some("changed text"), &calls())
            .is_err()
    );
    let mut reversed = calls();
    reversed.reverse();
    assert!(
        replay
            .validate_response(Some("original text"), &reversed)
            .is_err()
    );
    assert!(
        replay
            .validate_response(Some("original text"), &calls()[..1])
            .is_err()
    );
}

#[test]
fn wrong_protocol_empty_encryption_duplicate_ids_and_unsafe_item_shapes_refuse_deserialization() {
    let valid = serde_json::to_value(replay()).unwrap();
    for (pointer, value) in [
        ("/protocol", json!("anthropic.messages")),
        ("/protocol", json!("unknown")),
        ("/prefix_digest", json!("short")),
        ("/blocks/0/encrypted_content", json!("")),
        ("/blocks/0/encrypted_content", json!(null)),
        ("/blocks/0/summary", json!(false)),
        ("/blocks/1/status", json!("in_progress")),
        ("/blocks/1/role", json!("system")),
        ("/blocks/1/phase", json!("tool")),
        ("/blocks/2/type", json!("web_search_call")),
        ("/blocks/2/call_id", json!("")),
        ("/blocks/2/name", json!("Bad Name")),
        ("/blocks/2/arguments", json!("[]")),
        ("/blocks/2/arguments", json!("malformed")),
        ("/blocks/3/id", json!("fc_1")),
        ("/blocks/3/call_id", json!("original_1")),
    ] {
        let mut bad = valid.clone();
        *bad.pointer_mut(pointer).unwrap() = value;
        assert!(
            serde_json::from_value::<AssistantReplay>(bad).is_err(),
            "{pointer}"
        );
    }
    let mut bad = valid;
    bad["blocks"][1]["tools"] = json!(["smuggled"]);
    assert!(serde_json::from_value::<AssistantReplay>(bad).is_err());
}

#[test]
fn unknown_nested_summary_annotations_and_logprob_payloads_refuse_without_becoming_tools() {
    for (pointer, value) in [
        (
            "/summary",
            json!([{"type":"summary_text", "text":"ok", "unknown":true}]),
        ),
        (
            "/summary",
            json!([{"type":"tool_use", "name":"bash", "text":"execute"}]),
        ),
        ("/content", json!([{"type":"reasoning_text", "text":4}])),
    ] {
        let mut item = reasoning();
        *item.pointer_mut(pointer).unwrap() = value;
        assert!(AssistantReplay::validate_item("openai.responses", &item).is_err());
    }
    for part in [
        json!({"type":"input_image", "image_url":"https://fictional.invalid"}),
        json!({"type":"output_text", "text":"ok", "annotations":[{"type":"execute", "code":"x"}]}),
        json!({"type":"output_text", "text":"ok", "annotations":[], "logprobs":[{"fake":true}]}),
        json!({"type":"refusal", "refusal":false}),
    ] {
        let mut item = message();
        item["content"] = json!([part]);
        assert!(AssistantReplay::validate_item("openai.responses", &item).is_err());
    }
}

#[test]
fn valid_refusal_and_annotation_metadata_agree_with_neutral_text_without_authority() {
    let mut replay = replay();
    replay.blocks.truncate(2);
    replay.blocks[1]["phase"] = json!("final_answer");
    replay.blocks[1]["content"] = json!([{"type":"refusal", "refusal":"original refusal"}]);
    assert!(
        replay
            .validate_response(Some("original refusal"), &[])
            .is_ok()
    );
    let annotations = [
        json!({"type":"url_citation", "url":"https://fictional.invalid", "title":"data",
        "start_index":0, "end_index":3}),
        json!({"type":"file_citation", "file_id":"fictional-id",
        "filename":"fictional.txt", "index":0}),
        json!({"type":"file_path", "file_id":"fictional-id",
        "index":0}),
        json!({"type":"container_file_citation", "container_id":"fictional-container",
        "file_id":"fictional-id", "filename":"fictional.txt", "start_index":0,"end_index":3}),
    ];
    for annotation in annotations {
        replay.blocks[1]["content"] = json!([{"type":"output_text", "text":"original text",
            "annotations":[annotation.clone()]}]);
        assert!(replay.validate_response(Some("original text"), &[]).is_ok());
        let mut bad = annotation;
        bad["executor"] = json!("bash");
        replay.blocks[1]["content"][0]["annotations"] = json!([bad]);
        assert!(replay.validate().is_err());
    }
}

#[test]
fn session_reload_retains_original_items_phase_function_ids_and_refuses_legacy_smuggling() {
    let replay = replay();
    let mut session = Session::new(SessionId::new("fictional"), 123, "/fictional");
    session.append(SessionEvent::AssistantMessage {
        replay: Some(replay.clone()),
        text: Some("original text".into()),
        reasoning: None,
        tool_calls: calls(),
        usage: None,
        interrupted: false,
        model: Some("gpt-6-astra".into()),
        effort: None,
    });
    for call in calls() {
        session.append(SessionEvent::ToolResult {
            call_id: call.id,
            content: "original result".into(),
            content_blocks: None,
            is_error: false,
        });
    }
    let raw = session.try_to_jsonl().unwrap();
    let loaded = Session::from_jsonl(&raw).unwrap();
    assert_eq!(loaded, session);
    let messages = loaded.derive_messages();
    assert!(
        matches!(&messages[0], Message::Assistant { replay:Some(original), .. } if original==&replay)
    );
    let old = raw.replacen("\"version\":3", "\"version\":1", 1);
    assert!(Session::from_jsonl(&old).is_err());
    let bad = raw.replace("original text", "changed text");
    assert!(Session::from_jsonl(&bad).is_ok()); // Both neutral and original text changed consistently.
    let bad = raw.replacen("original text", "changed text", 1);
    assert!(Session::from_jsonl(&bad).is_err());
}

#[test]
fn replay_counts_and_complete_serialized_bytes_accept_the_bound_then_refuse_next_unit() {
    let mut replay = replay();
    replay.blocks = vec![reasoning()];
    for (count, accepted) in [(256, true), (257, false)] {
        replay.blocks = (0..count)
            .map(|i| {
                let mut item = reasoning();
                item["id"] = json!(format!("rs_{i}"));
                item
            })
            .collect();
        assert_eq!(replay.validate().is_ok(), accepted);
    }
    replay.blocks = vec![reasoning()];
    let limit = nanus_domain::content::RECORD_BYTES_MAX;
    let overhead = serde_json::to_vec(&replay).unwrap().len()
        - replay.blocks[0]["encrypted_content"]
            .as_str()
            .unwrap()
            .len();
    replay.blocks[0]["encrypted_content"] = json!("x".repeat(limit - overhead));
    assert_eq!(serde_json::to_vec(&replay).unwrap().len(), limit);
    assert!(replay.validate().is_ok());
    replay.blocks[0]["encrypted_content"] = json!("x".repeat(limit - overhead + 1));
    assert!(replay.validate().is_err());
}
