//! Pure request admission, real completion receipts and exact original-item continuation.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
use std::sync::Arc;

use nanus_adapter_openai::responses::{PreparedResponses, StreamAccumulator};
use nanus_adapter_openai::{OpenAiConfig, OpenAiLlm, Protocol, ProtocolPreference, Vendor};
use nanus_domain::{Message, ToolCall, ToolCallId, ToolName, ToolSchema};
use nanus_ports::LlmPort as _;
use nanus_ports::{ChatRequest, LlmEvent, ReasoningEffort, ResponseLimits};
use serde_json::{Value, json};

#[path = "responses_request/cost.rs"]
mod cost;

fn config() -> OpenAiConfig {
    let mut config = OpenAiConfig::new(Vendor::OpenAi, "gpt-6-astra", "fictional-key");
    config
        .set_protocol_preference(ProtocolPreference::Exact(Protocol::Responses))
        .unwrap();
    config.set_response_limits(
        ResponseLimits::new(1 << 20, 1 << 20, 1 << 20, 100, 16, 1 << 20).unwrap(),
    );
    config.set_function_strictness(Some(false));
    config
}

fn request(messages: Vec<Message>) -> ChatRequest {
    let mut request = ChatRequest::new("gpt-6-astra", messages);
    request.max_tokens = Some(8192);
    request.context_budget = Some(64_000);
    request.tools = vec![ToolSchema {
        name: ToolName::new("read").unwrap(),
        description: "Read fictional text".into(),
        parameters: json!({"type":"object",
        "properties":{"path":{"type":"string"}},"required":["path"],
        "additionalProperties":false}),
    }];
    request
}

fn original_items() -> Vec<Value> {
    vec![
        json!({"type":"reasoning","id":"rs_original","summary":[],
        "encrypted_content":"opaque+/=fictional","status":"completed"}),
        json!({"type":"message","id":"msg_original","role":"assistant",
        "phase":"commentary","status":"completed","content":[{"type":"output_text",
        "text":"Reading both.","annotations":[]}]}),
        json!({"type":"function_call","id":"fc_original_1","call_id":"call_1",
        "name":"read","arguments":"{\"path\":\"a\"}","status":"completed"}),
        json!({"type":"function_call","id":"fc_original_2","call_id":"call_2",
        "name":"read","arguments":"{\"path\":\"b\"}","status":"completed"}),
    ]
}

fn observe_item(decoder: &mut StreamAccumulator, index: usize, item: &Value) {
    let mut added = item.clone();
    added["status"] = json!("in_progress");
    match item["type"].as_str().unwrap() {
        "reasoning" => added["encrypted_content"] = Value::Null,
        "function_call" => added["arguments"] = json!(""),
        "message" => added["content"] = json!([]),
        _ => unreachable!(),
    }
    decoder.observe_line(
        &json!({"type":"response.output_item.added",
            "output_index":index,"item":added})
        .to_string(),
    );
    if item["type"] == "function_call" {
        decoder.observe_line(
            &json!({"type":"response.function_call_arguments.delta",
                "output_index":index,"item_id":item["id"],"delta":item["arguments"]})
            .to_string(),
        );
    }
    if item["type"] == "message" {
        decoder.observe_line(
            &json!({"type":"response.output_text.delta",
                "output_index":index,"content_index":0,"item_id":item["id"],
                "delta":"Reading both."})
            .to_string(),
        );
    }
    decoder.observe_line(
        &json!({"type":"response.output_item.done",
            "output_index":index,"item":item})
        .to_string(),
    );
}

fn complete(prepared: PreparedResponses) -> Message {
    let mut decoder = StreamAccumulator::with_context(
        prepared.prefix_digest,
        prepared.context_receipt,
        config().response_limits().unwrap(),
    )
    .unwrap();
    let items = original_items();
    decoder.observe_line(
        &json!({"type":"response.created","response":{
        "id":"response_original","status":"in_progress"}})
        .to_string(),
    );
    for (index, item) in items.iter().enumerate() {
        observe_item(&mut decoder, index, item);
    }
    decoder.observe_line(
        &json!({"type":"response.completed","response":{
        "id":"response_original","status":"completed","output":items}})
        .to_string(),
    );
    decoder.close();
    let mut replay = None;
    let mut finished = false;
    while let Some(event) = decoder.take_ready() {
        match event {
            LlmEvent::AssistantReplay(record) => replay = Some(record),
            LlmEvent::Finished { .. } => finished = true,
            LlmEvent::Error(error) => panic!("{error}"),
            _ => {}
        }
    }
    assert!(finished);
    let calls = [("call_1", "a"), ("call_2", "b")]
        .into_iter()
        .map(|(id, path)| ToolCall {
            id: ToolCallId::new(id),
            name: ToolName::new("read").unwrap(),
            arguments: json!({"path":path}),
        })
        .collect();
    Message::Assistant {
        text: Some("Reading both.".into()),
        reasoning: None,
        replay,
        tool_calls: calls,
    }
}

fn continuation(adapter: &OpenAiLlm) -> ChatRequest {
    let mut request = request(vec![
        Message::system("Fictional prompt"),
        Message::user("Read both"),
    ]);
    let prepared = adapter.prepare_responses(&request).unwrap();
    request.messages.push(complete(prepared));
    request
        .messages
        .push(Message::tool(ToolCallId::new("call_1"), "alpha", false));
    request
        .messages
        .push(Message::tool(ToolCallId::new("call_2"), "beta", false));
    request
}

#[test]
fn original_items_and_exact_receipts_survive_completed_stream_and_json_reload() {
    let adapter = OpenAiLlm::new(config()).unwrap();
    let request = continuation(&adapter);
    let bytes = serde_json::to_vec(&request.messages).unwrap();
    let mut reloaded = request.clone();
    reloaded.messages = serde_json::from_slice(&bytes).unwrap();
    let prepared = adapter.prepare_responses(&reloaded).unwrap();
    let body = &prepared.body;
    assert_eq!(body["store"], false);
    assert_eq!(body["max_output_tokens"], 8192);
    assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    assert!(body.get("previous_response_id").is_none());
    assert_eq!(body["tools"][0]["strict"], false);
    assert_eq!(&body["input"].as_array().unwrap()[1..5], original_items());
    assert_eq!(body["input"][5]["call_id"], "call_1");
    assert_eq!(body["input"][6]["call_id"], "call_2");
    assert_eq!(
        prepared.context_receipt.wire_digest,
        blake3::hash(&serde_json::to_vec(body).unwrap())
            .to_hex()
            .to_string()
    );
    assert_eq!(
        prepared.context_receipt.source_digest,
        blake3::hash(&bytes).to_hex().to_string()
    );
    // The raw stock encoder still has its existing neutral reconstruction semantics.
    assert_ne!(adapter.encode(&request)["input"], body["input"]);
}

#[test]
fn whole_turn_fit_preserves_complete_source_and_original_prefix_receipts() {
    let adapter = OpenAiLlm::new(config()).unwrap();
    let mut request = continuation(&adapter);
    request.messages.push(Message::user("New fictional turn"));
    let source: Arc<[Message]> = request.messages.clone().into();
    request.messages = nanus_domain::context::fit_with_source(&source, 64_000, |messages| {
        if messages.len() > 3 { 100_000 } else { 100 }
    })
    .unwrap()
    .messages;
    request.source_history = Some(Arc::clone(&source));
    let prepared = adapter.prepare_responses(&request).unwrap();
    assert_eq!(prepared.context_receipt.dropped_turns, 1);
    assert_eq!(prepared.context_receipt.dropped_messages, 4);
    assert_eq!(prepared.body["input"].as_array().unwrap().len(), 1);
    assert_eq!(source.len(), 6);
    let mut rewritten = request.clone();
    rewritten.messages[1] = Message::system("Invented fitting notice");
    assert!(adapter.prepare_responses(&rewritten).is_err());
    rewritten = request.clone();
    rewritten.context_budget = Some(63_999);
    assert!(adapter.prepare_responses(&rewritten).is_err());
    let mut altered = source.to_vec();
    altered[1] = Message::user("Rewritten old input");
    rewritten = request;
    rewritten.source_history = Some(altered.into());
    assert!(adapter.prepare_responses(&rewritten).is_err());
}

#[test]
fn changed_controls_source_receipts_and_incomplete_tool_batches_refuse() {
    let adapter = OpenAiLlm::new(config()).unwrap();
    let request = continuation(&adapter);
    for mutation in 0..11 {
        let mut bad = request.clone();
        match mutation {
            0 => bad.messages[0] = Message::system("Changed prompt"),
            1 => bad.messages[1] = Message::user("Changed original input"),
            2 => bad.tools[0].description.push('!'),
            3 => bad.reasoning_effort = Some(ReasoningEffort::High),
            4 => bad.max_tokens = Some(8191),
            5 => bad.model = "gpt-6.1-sol".into(),
            6 => {
                bad.messages.pop();
            }
            7 => bad.messages.swap(3, 4),
            8 => {
                if let Message::Assistant { replay, .. } = &mut bad.messages[2] {
                    *replay = None;
                }
            }
            9 => {
                if let Message::Assistant {
                    replay: Some(replay),
                    ..
                } = &mut bad.messages[2]
                {
                    replay.context_receipt = None;
                }
            }
            10 => {
                if let Message::Assistant {
                    replay: Some(replay),
                    ..
                } = &mut bad.messages[2]
                {
                    replay.context_receipt.as_mut().unwrap().wire_digest = "f".repeat(64);
                }
            }
            _ => unreachable!(),
        }
        assert!(
            adapter.prepare_responses(&bad).is_err(),
            "mutation {mutation}"
        );
    }
    let mut changed = config();
    changed.set_function_strictness(None);
    assert!(
        OpenAiLlm::new(changed)
            .unwrap()
            .prepare_responses(&request)
            .is_err()
    );
}

#[test]
fn missing_explicit_budgets_unknown_models_and_oversized_inputs_refuse() {
    let adapter = OpenAiLlm::new(config()).unwrap();
    let good = request(vec![Message::user("fictional")]);
    assert!(adapter.prepare_responses(&good).is_ok());
    for mutation in 0..8 {
        let mut bad = good.clone();
        match mutation {
            0 => bad.context_budget = None,
            1 => bad.max_tokens = None,
            2 => bad.context_budget = Some(0),
            3 => bad.model = "unknown-model".into(),
            4 => bad.separate_reasoning_tokens = 1,
            5 => bad.messages = vec![Message::user("x".repeat(4 * 1024 * 1024))],
            6 => bad.temperature = Some(f32::NAN),
            7 => bad.temperature = Some(-1.0),
            _ => unreachable!(),
        }
        assert!(adapter.prepare_responses(&bad).is_err());
    }
    assert!(adapter.prepare_responses(&request(vec![])).is_err());
}

#[test]
fn prospective_estimation_substitutes_only_last_balanced_batch_values_and_never_admits_dispatch() {
    let mut config = config();
    config.set_stateless_responses(true);
    let adapter = OpenAiLlm::new(config).unwrap();
    let original = continuation(&adapter);
    let source: Arc<[Message]> = original.messages.clone().into();
    let mut candidate = original.clone();
    candidate.source_history = Some(Arc::clone(&source));
    candidate.messages[3] = Message::tool(
        ToolCallId::new("call_1"),
        "reserved success".repeat(100),
        false,
    );
    candidate.messages[4] = Message::tool(ToolCallId::new("call_2"), "reserved failure", true);
    let estimate = adapter.estimate_request(&candidate).unwrap();
    assert!(estimate.request_bytes > adapter.estimate_request(&original).unwrap().request_bytes);
    assert!(adapter.prepare_responses(&candidate).is_err());
    assert!(matches!(
        futures::executor::block_on(async {
            use futures::StreamExt as _;
            adapter.stream_chat(candidate.clone()).next().await
        }),
        Some(LlmEvent::Error(_))
    ));
    assert_eq!(source.as_ref(), original.messages);
    candidate.messages.swap(3, 4);
    assert!(adapter.estimate_request(&candidate).is_err());
    candidate = original.clone();
    candidate.source_history = Some(Arc::clone(&source));
    candidate.messages[2] = Message::assistant(Some("rewritten call record".into()), None, vec![]);
    assert!(adapter.estimate_request(&candidate).is_err());
    candidate = original;
    candidate.messages.push(Message::user("new question"));
    candidate.source_history = Some(candidate.messages.clone().into());
    candidate.messages[3] = Message::tool(
        ToolCallId::new("call_1"),
        "changed historical result",
        false,
    );
    assert!(adapter.estimate_request(&candidate).is_err());
}

#[test]
fn opt_in_requires_explicit_public_endpoint_limits_and_preserves_default() {
    assert!(!config().stateless_responses());
    for mutation in 0..4 {
        let mut changed = if mutation == 0 {
            OpenAiConfig::new(Vendor::OpenAi, "gpt-6-astra", "fictional")
        } else {
            config()
        };
        changed.set_stateless_responses(true);
        match mutation {
            0 => {}
            1 => changed
                .set_protocol_preference(ProtocolPreference::Automatic)
                .unwrap(),
            2 => changed.set_base_url("http://127.0.0.1:1").unwrap(),
            3 => changed
                .set_protocol_preference(ProtocolPreference::Exact(Protocol::ChatCompletions))
                .unwrap(),
            _ => unreachable!(),
        }
        assert!(OpenAiLlm::new(changed).is_err());
    }
}

#[test]
fn oversized_prospective_cost_is_measured_but_never_admitted_for_dispatch() {
    let mut config = config();
    config.set_stateless_responses(true);
    let adapter = OpenAiLlm::new(config).unwrap();
    let mut candidate = continuation(&adapter);
    candidate.source_history = Some(candidate.messages.clone().into());
    candidate.messages[3] = Message::tool(ToolCallId::new("call_1"), "\0".repeat(12_000), false);
    let measured = adapter.estimate_request(&candidate).unwrap();
    assert!(measured.input_tokens > 64_000);
    assert!(!measured.fits(adapter.capabilities(&candidate.model), &candidate));
    assert!(adapter.prepare_responses(&candidate).is_err());
    candidate.source_history = None;
    assert_eq!(adapter.estimate_request(&candidate).unwrap(), measured);
    assert!(adapter.prepare_responses(&candidate).is_err());
    assert!(matches!(
        futures::executor::block_on(async {
            use futures::StreamExt as _;
            adapter.stream_chat(candidate).next().await
        }),
        Some(LlmEvent::Error(_))
    ));
}
