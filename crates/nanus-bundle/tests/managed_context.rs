//! Managed context through the real runner: activation, fitting, proposals and checkpoints.
//!
//! Every case drives `AgentRunner::run_turn_with_runtime` with a scripted model that implements
//! managed preparation the way an adapter must — one encoded body, estimated and digested once,
//! sent as it was prepared — and an in-memory checkpoint that can be told to refuse or to lose
//! track of a commit. What is asserted is what reached the model and what reached the disk.

// A panic in a test *is* the assertion. The workspace denies the lint for production code.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

#[path = "managed_context/harness.rs"]
mod harness;

use harness::*;
use nanus_bundle::TurnHost;
use nanus_domain::context::managed::{
    AttemptOutcome, AttemptPhase, ContextMode, ContextPolicy, DecisionOutcome, ErrorCode,
    MEMORY_LABEL, ModeActor, RevisionAuthor,
};
use nanus_domain::{Message, SessionEvent};
use nanus_ports::PersistenceState;

/// T01: a session that never enabled managed context runs, encodes and saves exactly as before.
#[test]
fn a_legacy_session_is_untouched_by_the_managed_entry_point() {
    let run = |managed_entry: bool| {
        let model = <Model as ModelExt>::new(vec![text("hello")]);
        let runner = runner(&model, 64_000);
        let mut session = session();
        let disk = Disk::new();
        let context = context();
        if managed_entry {
            let outcome = block(runner.run_turn_with_runtime(
                &mut session,
                "hi",
                &mut nanus_bundle::Silent,
                host(&disk, &context),
            ));
            assert!(outcome.outcome.is_ok());
            assert!(
                outcome.persistence.is_none(),
                "a legacy session is the host's to save"
            );
        } else {
            block(runner.run_turn(&mut session, "hi", &mut nanus_bundle::Silent, None)).unwrap();
        }
        assert_eq!(disk.commits(), 0);
        (model.sent(), session.to_jsonl())
    };
    let (legacy, legacy_log) = run(false);
    let (entry, entry_log) = run(true);
    assert_eq!(legacy, entry, "the same request reaches the model");
    assert_eq!(legacy_log, entry_log, "and the same log is recorded");
    assert_eq!(
        legacy[0].tools.len(),
        6,
        "one registered tool and five goal tools; no context tools"
    );
    assert!(legacy_log.starts_with(r#"{"format":"nanus.session","version":2,"#));
}

/// T33: activation needs a model path that supports it, and a refusal changes nothing.
#[test]
fn activation_refuses_an_unsupported_model_without_changing_the_session() {
    let model = <Model as ModelExt>::new(Vec::new());
    model.set_supported(false);
    let runner = runner(&model, 64_000);
    let mut session = session();
    let disk = Disk::new();
    let context = context();
    let before = session.clone();
    let refused = block(runner.set_context_policy(
        &mut session,
        ContextPolicy::managed(),
        ModeActor::Human,
        host(&disk, &context).runtime,
    ));
    assert_eq!(
        refused.err().and_then(|error| error.managed_code()),
        Some(ErrorCode::ProtocolIncompatible)
    );
    assert_eq!(session, before);
    assert_eq!(disk.commits(), 0);

    model.set_supported(true);
    let enabled = block(runner.set_context_policy(
        &mut session,
        ContextPolicy::managed(),
        ModeActor::Human,
        host(&disk, &context).runtime,
    ));
    assert!(
        matches!(enabled, Ok(Some(PersistenceState::Acknowledged(_)))),
        "{enabled:?}"
    );
    assert!(session.is_managed_body());
    assert_eq!(disk.commits(), 1);
    assert!(
        disk.last()
            .to_jsonl()
            .starts_with(r#"{"format":"nanus.session","version":3,"#)
    );
}

/// T02/T03/T31: a long turn is fitted by hiding whole old fragments, every user message stays,
/// and what was dispatched is exactly what the attempt record names.
#[test]
fn automatic_fitting_hides_old_fragments_and_keeps_every_user_message() {
    let mut script = Vec::new();
    for index in 0..8 {
        script.push(call(&format!("c{index}"), "echo", r#"{"size": 3000}"#));
    }
    script.push(text("done"));
    let model = <Model as ModelExt>::new(script);
    let runner = runner(&model, 12_000);
    let (mut session, disk, context) = managed(&runner);
    let outcome = block(runner.run_turn_with_runtime(
        &mut session,
        "constraint: keep the API stable. Now do the long task.",
        &mut nanus_bundle::Silent,
        host(&disk, &context),
    ));
    assert!(outcome.outcome.is_ok(), "{:?}", outcome.outcome);
    assert!(matches!(
        outcome.persistence,
        Some(PersistenceState::Acknowledged(_))
    ));
    let sent = model.sent();
    let last = sent.last().unwrap();
    assert!(
        last.messages.iter().any(|message| message.text()
            == Some("constraint: keep the API stable. Now do the long task.")),
        "the user message survives fitting"
    );
    let calls: usize = last
        .messages
        .iter()
        .map(|message| message.tool_calls().len())
        .sum();
    let results = last
        .messages
        .iter()
        .filter(|message| matches!(message, Message::Tool { .. }))
        .count();
    assert_eq!(calls, results, "no call without its result");
    assert!(calls < 8, "old fragments were hidden: {calls}");
    let automatic = session
        .log()
        .events()
        .iter()
        .filter(|event| {
            matches!(event,
        SessionEvent::ContextRevision { payload } if payload.author == RevisionAuthor::Automatic)
        })
        .count();
    assert!(automatic > 0, "fitting recorded its revision");
    // Every dispatched body is named by exactly one started and one finished attempt record.
    let started: Vec<_> = attempts(&session, AttemptPhase::Started);
    let finished: Vec<_> = attempts(&session, AttemptPhase::Finished);
    assert_eq!(started.len(), sent.len());
    assert_eq!(finished.len(), sent.len());
    assert_eq!(
        model.digests(),
        started
            .iter()
            .map(|a| a.request_digest.clone())
            .collect::<Vec<_>>()
    );
    assert!(
        finished
            .iter()
            .all(|attempt| attempt.outcome == Some(AttemptOutcome::Completed))
    );
    assert_eq!(&disk.last(), &session, "the disk holds the whole turn");
}

/// T04: when the protected floor alone does not fit, nothing is sent and nothing is installed.
#[test]
fn a_protected_floor_over_the_budget_refuses_before_any_request() {
    let model = <Model as ModelExt>::new(vec![text("never")]);
    let runner = runner(&model, 5_000);
    let (mut session, disk, context) = managed(&runner);
    let huge = "x".repeat(40_000);
    let outcome = block(runner.run_turn_with_runtime(
        &mut session,
        &huge,
        &mut nanus_bundle::Silent,
        host(&disk, &context),
    ));
    assert_eq!(
        outcome.outcome.err().and_then(|error| error.managed_code()),
        Some(ErrorCode::ProtectedFloorTooLarge)
    );
    assert!(model.sent().is_empty(), "no request was sent");
    assert!(
        !session
            .log()
            .events()
            .iter()
            .any(|event| matches!(event, SessionEvent::ContextRevision { .. }))
    );
}

/// T16: inspect, then a proposal alone in its step: staged, the step settles, then accepted.
#[test]
fn a_valid_proposal_is_staged_then_accepted_after_its_step_settles() {
    let model = <Model as ModelExt>::new(Vec::new());
    let runner = runner(&model, 64_000);
    let (mut session, disk, context) = managed(&runner);
    // Three earlier fragments to choose from.
    model.push(call("w1", "echo", r#"{"size": 10}"#));
    model.push(call("w2", "echo", r#"{"size": 10}"#));
    model.push(call("w3", "echo", r#"{"size": 10}"#));
    model.push(call("i1", "context_manage", INSPECT));
    model.push_with(propose_oldest("i2"));
    model.push(text("tidied"));
    let outcome = block(runner.run_turn_with_runtime(
        &mut session,
        "work, then tidy up",
        &mut nanus_bundle::Silent,
        host(&disk, &context),
    ));
    assert!(outcome.outcome.is_ok(), "{:?}", outcome.outcome);
    let proposal = tool_result_json(&session, "i2");
    assert_eq!(proposal["status"], "staged", "{proposal}");
    let accepted = session.log().events().iter().find_map(|event| match event {
        SessionEvent::ContextDecision { payload }
            if payload.outcome == DecisionOutcome::Accepted =>
        {
            Some(payload.clone())
        }
        _ => None,
    });
    assert!(
        accepted.is_some(),
        "the proposal was accepted after its step"
    );
    let last = model.sent().pop().unwrap();
    assert!(
        !last
            .messages
            .iter()
            .any(|message| message.tool_calls().iter().any(|c| c.id.as_str() == "w1")),
        "the hidden fragment is not in the next request"
    );
    assert!(
        last.messages
            .iter()
            .any(|message| matches!(message, Message::User { .. }))
    );
    assert!(matches!(last.messages.get(2), Some(Message::User { .. })));
    assert!(
        last.messages
            .get(3)
            .and_then(Message::text)
            .is_some_and(|text| text.starts_with(MEMORY_LABEL)),
        "generated data follows the first user message"
    );
    assert_eq!(&disk.last(), &session);
}

/// T17: a proposal sharing its message with any other call refuses every call before effects.
#[test]
fn a_mixed_proposal_batch_refuses_every_call_before_any_effect() {
    let model = <Model as ModelExt>::new(Vec::new());
    let runner = runner(&model, 64_000);
    let (mut session, disk, context) = managed(&runner);
    model.push(calls(&[
        ("g", "create_goal", r#"{"objective": "should not be set"}"#),
        ("p", "context_manage", r#"{"action": "propose"}"#),
        ("e", "echo", r#"{"size": 1}"#),
    ]));
    model.push(text("ok"));
    let outcome = block(runner.run_turn_with_runtime(
        &mut session,
        "go",
        &mut nanus_bundle::Silent,
        host(&disk, &context),
    ));
    assert!(outcome.outcome.is_ok());
    assert!(session.goal().is_none(), "the goal tool did not run");
    assert_eq!(model.echoes(), 0, "the registered tool did not run");
    let refused: Vec<bool> = ["g", "p", "e"]
        .iter()
        .map(|id| tool_result(&session, id).1)
        .collect();
    assert_eq!(refused, vec![true, true, true]);
    assert!(
        tool_result(&session, "p")
            .0
            .contains("mixed_mutation_batch")
    );
}

/// Enabling, disabling and resetting keep the revision numbers rising and never restore a hide.
#[test]
fn reset_selects_legacy_and_a_re_enable_starts_from_an_empty_selection() {
    let model = <Model as ModelExt>::new(Vec::new());
    let runner = runner(&model, 64_000);
    let (mut session, disk, context) = managed(&runner);
    model.push(call("w1", "echo", r#"{"size": 10}"#));
    model.push(call("w2", "echo", r#"{"size": 10}"#));
    model.push(call("w3", "echo", r#"{"size": 10}"#));
    model.push(call("i1", "context_manage", INSPECT));
    model.push_with(propose_oldest("i2"));
    model.push(text("tidied"));
    block(runner.run_turn_with_runtime(
        &mut session,
        "go",
        &mut nanus_bundle::Silent,
        host(&disk, &context),
    ))
    .outcome
    .unwrap();
    let status = runner
        .context_status(&session, host(&disk, &context).runtime)
        .unwrap();
    assert_eq!(status.hidden_fragments, 1);
    let reset = block(runner.reset_context(&mut session, host(&disk, &context).runtime));
    assert!(matches!(reset, Ok(PersistenceState::Acknowledged(_))));
    let after = runner
        .context_status(&session, host(&disk, &context).runtime)
        .unwrap();
    assert_eq!(after.mode, ContextMode::Legacy);
    assert_eq!(after.hidden_fragments, 0);
    assert!(after.revision > status.revision, "numbers never restart");
    block(runner.set_context_policy(
        &mut session,
        ContextPolicy::managed(),
        ModeActor::Human,
        host(&disk, &context).runtime,
    ))
    .unwrap();
    let again = runner
        .context_status(&session, host(&disk, &context).runtime)
        .unwrap();
    assert_eq!(again.mode, ContextMode::Managed);
    assert_eq!(again.hidden_fragments, 0, "a reset hide is not resurrected");
    assert!(again.managed_ready);
}

/// Silences an unused import on platforms where a helper is not used.
#[allow(dead_code)]
fn _uses(_: TurnHost<'_>) {}
