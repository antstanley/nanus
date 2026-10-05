use super::*;

fn revision_config() -> OpenAiConfig {
    let mut config = config();
    config.set_stateless_responses(true);
    config.set_instruction_revisions(true);
    config
}

#[test]
fn trusted_instruction_revision_preserves_original_items_after_a_new_user_turn() {
    let adapter = OpenAiLlm::new(revision_config()).unwrap();
    let mut request = continuation(&adapter);
    let original = request.messages[2].clone();
    request.messages[0] = Message::system("Revised fictional prompt");
    request.messages.push(Message::user("Next fictional turn"));
    let prepared = adapter
        .prepare_responses(&request)
        .expect("authorized host revision");
    assert_eq!(prepared.body["instructions"], "Revised fictional prompt");
    assert_eq!(
        &prepared.body["input"].as_array().unwrap()[1..5],
        original_items()
    );
    assert_eq!(request.messages[2], original);
}

#[path = "instructions/history.rs"]
mod history;
#[path = "instructions/limits.rs"]
mod limits;

#[test]
fn revisions_are_explicit_and_never_reinterpret_legacy_receipts() {
    assert!(!config().instruction_revisions());
    let mut invalid = config();
    invalid.set_instruction_revisions(true);
    assert!(OpenAiLlm::new(invalid).is_err());
    let legacy = OpenAiLlm::new(config()).unwrap();
    let revised = OpenAiLlm::new(revision_config()).unwrap();
    let old = continuation(&legacy);
    let new = continuation(&revised);
    assert!(legacy.prepare_responses(&old).is_ok());
    assert!(revised.prepare_responses(&new).is_ok());
    assert!(legacy.prepare_responses(&new).is_err());
    assert!(revised.prepare_responses(&old).is_err());
    let mut erased = new;
    if let Message::Assistant {
        replay: Some(replay),
        ..
    } = &mut erased.messages[2]
    {
        replay.context_receipt.as_mut().unwrap().instructions = None;
    }
    assert!(revised.prepare_responses(&erased).is_err());
    assert!(legacy.prepare_responses(&erased).is_err());
}

#[test]
fn current_instructions_cannot_change_during_the_original_tool_turn() {
    let adapter = OpenAiLlm::new(revision_config()).unwrap();
    let mut request = continuation(&adapter);
    request.messages[0] = Message::system("Changed during tool processing");
    assert!(adapter.prepare_responses(&request).is_err());
    assert!(adapter.estimate_request(&request).is_err());
    request
        .messages
        .push(Message::user("A new host-authorized turn"));
    assert!(adapter.prepare_responses(&request).is_ok());
    // Changing only the fitted header cannot rewrite the host's retained source.
    request.source_history = Some(request.messages.clone().into());
    request.messages[0] = Message::system("Not the original current source");
    assert!(adapter.prepare_responses(&request).is_err());
}

#[test]
fn authorized_revisions_do_not_relax_nonprompt_controls_or_historical_evidence() {
    let adapter = OpenAiLlm::new(revision_config()).unwrap();
    let mut good = continuation(&adapter);
    good.messages[0] = Message::system("Revised prompt");
    good.messages.push(Message::user("Next turn"));
    assert!(adapter.prepare_responses(&good).is_ok());
    for mutation in 0..12 {
        let mut bad = good.clone();
        match mutation {
            0 => bad.messages[1] = Message::user("Rewritten original user"),
            1 => bad.tools[0].description.push('!'),
            2 => bad.reasoning_effort = Some(ReasoningEffort::High),
            3 => bad.max_tokens = Some(8191),
            4 => bad.model = "gpt-6.1-sol".into(),
            5 => bad.temperature = Some(0.3),
            6 => bad.messages.swap(3, 4),
            7 => snapshot(&mut bad).revision = "f".repeat(64),
            8 => snapshot(&mut bad).messages[0].push('!'),
            9 => {
                let original = snapshot(&mut bad);
                original.messages[0].push('!');
                original.revision = format!(
                    "{:x}",
                    Sha256::digest(serde_json::to_vec(&original.messages).unwrap())
                );
            }
            10 => snapshot(&mut bad).version = 2,
            11 => {
                if let Message::Assistant { text, .. } = &mut bad.messages[2] {
                    *text = Some("Changed original answer".into());
                }
            }
            _ => unreachable!(),
        }
        assert!(
            adapter.prepare_responses(&bad).is_err(),
            "mutation {mutation}"
        );
        assert!(
            adapter.estimate_request(&bad).is_err(),
            "estimate {mutation}"
        );
    }
    let mut strict = revision_config();
    strict.set_function_strictness(None);
    assert!(
        OpenAiLlm::new(strict)
            .unwrap()
            .prepare_responses(&good)
            .is_err()
    );
}

fn snapshot(request: &mut ChatRequest) -> &mut nanus_domain::message::ReplayInstructions {
    let Message::Assistant {
        replay: Some(replay),
        ..
    } = &mut request.messages[2]
    else {
        panic!("original reply");
    };
    replay
        .context_receipt
        .as_mut()
        .unwrap()
        .instructions
        .as_mut()
        .unwrap()
}
