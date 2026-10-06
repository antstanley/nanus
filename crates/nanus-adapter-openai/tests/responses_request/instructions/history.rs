use super::*;
use nanus_domain::{Session, SessionEvent, SessionId, TurnEndReason};

fn answer(prepared: PreparedResponses, id: &str) -> Message {
    let mut decoder = StreamAccumulator::with_context(
        prepared.prefix_digest,
        prepared.context_receipt,
        config().response_limits().unwrap(),
    )
    .unwrap();
    let mut items = original_items()[..2].to_vec();
    items[0]["id"] = json!(format!("rs_{id}"));
    items[1]["id"] = json!(format!("msg_{id}"));
    items[1]["phase"] = json!("final_answer");
    decoder.observe_line(
        &json!({"type":"response.created","response":{
        "id":id,"status":"in_progress"}})
        .to_string(),
    );
    for (index, item) in items.iter().enumerate() {
        observe_item(&mut decoder, index, item);
    }
    decoder.observe_line(
        &json!({"type":"response.completed","response":{
        "id":id,"status":"completed","output":items}})
        .to_string(),
    );
    decoder.close();
    let mut replay = None;
    let mut finished = false;
    while let Some(event) = decoder.take_ready() {
        match event {
            LlmEvent::AssistantReplay(original) => replay = Some(original),
            LlmEvent::Finished { .. } => finished = true,
            LlmEvent::Error(error) => panic!("{error}"),
            _ => {}
        }
    }
    assert!(finished);
    assert!(replay.is_some());
    Message::Assistant {
        text: Some("Reading both.".into()),
        reasoning: None,
        replay,
        tool_calls: vec![],
    }
}

fn save_turn(session: &mut Session, turn: u32, user: &str, answer: &Message) {
    let Message::Assistant {
        text,
        reasoning,
        replay,
        tool_calls,
    } = answer
    else {
        panic!("original response");
    };
    session.append(SessionEvent::TurnStart { turn });
    session.append(SessionEvent::UserMessage {
        content_blocks: None,
        text: user.into(),
    });
    session.append(SessionEvent::StepStart { turn, step: 0 });
    session.append(SessionEvent::AssistantMessage {
        text: text.clone(),
        reasoning: reasoning.clone(),
        replay: replay.clone(),
        tool_calls: tool_calls.clone(),
        usage: None,
        interrupted: false,
        model: Some("gpt-6-astra".into()),
        effort: None,
    });
    session.append(SessionEvent::StepEnd { turn, step: 0 });
    session.append(SessionEvent::TurnEnd {
        turn,
        reason: TurnEndReason::Completed,
    });
}

#[test]
fn successive_revisions_reload_original_completed_sessions_and_fit_whole_turns() {
    let adapter = OpenAiLlm::new(revision_config()).unwrap();
    let mut session = Session::new(SessionId::new("fictional-revisions"), 123, "/fictional");
    let first = request(vec![Message::system("A"), Message::user("First")]);
    let original = answer(adapter.prepare_responses(&first).unwrap(), "first");
    save_turn(&mut session, 0, "First", &original);
    let loaded = Session::from_jsonl(&session.try_to_jsonl().unwrap()).unwrap();
    let mut second = request(vec![
        Message::system(""),
        Message::system("B"),
        Message::system("B"),
    ]);
    second.messages.extend(loaded.derive_messages());
    second.messages.push(Message::user("Second"));
    let prepared = adapter.prepare_responses(&second).unwrap();
    assert_eq!(
        prepared
            .context_receipt
            .instructions
            .as_ref()
            .unwrap()
            .messages,
        ["", "B", "B"]
    );
    assert_eq!(second.messages[4], original);
    let next = answer(prepared, "second");
    save_turn(&mut session, 1, "Second", &next);
    let jsonl = session.try_to_jsonl().unwrap();
    let reloaded = Session::from_jsonl(&jsonl).unwrap();
    assert_eq!(reloaded.try_to_jsonl().unwrap(), jsonl);
    let mut third = request(reloaded.derive_messages());
    third
        .messages
        .push(Message::user("Third without System instructions"));
    let prepared = adapter.prepare_responses(&third).unwrap();
    assert!(
        prepared
            .context_receipt
            .instructions
            .unwrap()
            .messages
            .is_empty()
    );
    assert_eq!(third.messages[1], original);
    assert_eq!(third.messages[3], next);
    let source: Arc<[Message]> = third.messages.clone().into();
    third.messages = nanus_domain::context::fit_with_source(&source, 64_000, |messages| {
        if messages.len() > 2 { 100_000 } else { 100 }
    })
    .unwrap()
    .messages;
    third.source_history = Some(Arc::clone(&source));
    let fitted = adapter.prepare_responses(&third).unwrap();
    assert_eq!(fitted.context_receipt.dropped_turns, 2);
    assert_eq!(fitted.context_receipt.dropped_messages, 4);
    assert_eq!(fitted.body["input"].as_array().unwrap().len(), 1);
    assert_eq!(source[1], original);
    assert_eq!(source[3], next);
    let mut changed = source.to_vec();
    changed[0] = Message::user("Rewritten elided human record");
    third.source_history = Some(changed.into());
    assert!(adapter.prepare_responses(&third).is_err());
}

#[test]
fn an_observed_followup_binds_historical_tool_results_even_after_a_prompt_revision() {
    let adapter = OpenAiLlm::new(revision_config()).unwrap();
    let mut request = continuation(&adapter);
    let final_answer = answer(adapter.prepare_responses(&request).unwrap(), "reviewed");
    request.messages.push(final_answer);
    request.messages[0] = Message::system("Next prompt");
    request.messages.push(Message::user("Next turn"));
    assert!(adapter.prepare_responses(&request).is_ok());
    request.messages[3] = Message::tool(ToolCallId::new("call_1"), "forged old result", false);
    assert!(adapter.prepare_responses(&request).is_err());
    assert!(adapter.estimate_request(&request).is_err());
}

#[test]
fn direct_user_pixels_reload_with_original_responses_and_changed_pixels_refuse() {
    use base64::Engine as _;
    let adapter = OpenAiLlm::new(revision_config()).unwrap();
    let pixels = base64::engine::general_purpose::STANDARD.encode(include_bytes!(
        "../../../../nanus-domain/tests/data/tiny-green-triangle.png"
    ));
    let blocks = vec![
        nanus_domain::ContentBlock::Text("Inspect".into()),
        nanus_domain::ContentBlock::Image {
            media_type: "image/png".into(),
            data_base64: pixels,
        },
    ];
    let user = Message::user_with_content(blocks).unwrap();
    let mut first = request(vec![Message::system("A"), user.clone()]);
    first.tools.clear();
    let prepared = adapter.prepare_responses(&first).unwrap();
    assert_eq!(prepared.body["input"].as_array().unwrap().len(), 1);
    assert_eq!(
        prepared.body["input"][0]["content"][1]["type"],
        "input_image"
    );
    assert_eq!(adapter.estimate_request(&first).unwrap().images, 1);
    let original = answer(prepared, "direct");
    let loaded = reload_direct(&user, &original);
    let mut second = request(vec![Message::system("B")]);
    second.tools.clear();
    second.messages.extend(loaded.derive_messages());
    second.messages.push(Message::user("Next"));
    let prepared = adapter.prepare_responses(&second).unwrap();
    assert_eq!(second.messages[1], user);
    assert_eq!(second.messages[2], original);
    assert_eq!(prepared.body["input"][1]["id"], "rs_direct");
    if let Message::User {
        content_blocks: Some(blocks),
        ..
    } = &mut second.messages[1]
    {
        blocks[1] = nanus_domain::ContentBlock::Image {
            media_type: "image/jpeg".into(),
            data_base64: base64::engine::general_purpose::STANDARD.encode(include_bytes!(
                "../../../../nanus-domain/tests/data/tiny-green-triangle.jpg"
            )),
        };
    }
    assert!(adapter.prepare_responses(&second).is_err());
}

fn reload_direct(user: &Message, original: &Message) -> Session {
    let Message::Assistant {
        text,
        reasoning,
        replay,
        tool_calls,
    } = original
    else {
        panic!("original assistant");
    };
    let mut session = Session::new(SessionId::new("direct-original"), 123, "/fictional");
    session.append(SessionEvent::UserMessage {
        text: user.text().unwrap().into(),
        content_blocks: user.content_blocks().map(<[_]>::to_vec),
    });
    session.append(SessionEvent::AssistantMessage {
        text: text.clone(),
        reasoning: reasoning.clone(),
        replay: replay.clone(),
        tool_calls: tool_calls.clone(),
        usage: None,
        interrupted: false,
        model: Some("gpt-6-astra".into()),
        effort: None,
    });
    Session::from_jsonl(&session.try_to_jsonl().unwrap()).unwrap()
}
