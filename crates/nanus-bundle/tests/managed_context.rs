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
use nanus_bundle::{Silent, TurnHost};
use nanus_domain::context::managed::{
    AttemptOutcome, AttemptPhase, ContextMode, ContextPolicy, DecisionOutcome, ErrorCode,
    MANAGED_REQUEST_GUIDE_V1, MANUAL_POLICY_V1, MEMORY_LABEL, ModeActor, RevisionAuthor, limits,
};
use nanus_domain::{Message, SessionEvent};
use nanus_ports::{CheckpointReason, PersistenceState};
use std::rc::Rc;

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
                &mut Silent,
                host(&disk, &context),
            ));
            assert!(outcome.outcome.is_ok());
            assert!(
                outcome.persistence.is_none(),
                "a legacy session is the host's to save"
            );
        } else {
            block(runner.run_turn(&mut session, "hi", &mut Silent, None)).unwrap();
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
        explicit_managed(),
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
        explicit_managed(),
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

/// C13: an adapter built for the earlier layout, which would drop the tail, cannot carry a
/// managed session: enabling is refused and a session already managed is unready, both with
/// `unsupported_mode`.
#[test]
fn an_adapter_declaring_the_earlier_layout_is_unsupported() {
    let model = <Model as ModelExt>::new(Vec::new());
    let runner = runner(&model, 64_000);
    let disk = Disk::new();
    let context = context();
    model.set_layout(1);
    let mut session = session();
    let refused = block(runner.set_context_policy(
        &mut session,
        explicit_managed(),
        ModeActor::Human,
        host(&disk, &context).runtime,
    ));
    assert_eq!(
        refused.err().and_then(|error| error.managed_code()),
        Some(ErrorCode::UnsupportedMode)
    );
    model.set_layout(2);
    let (managed, _, _) = managed(&runner);
    model.set_layout(1);
    let status = runner
        .context_status(&managed, host(&disk, &context).runtime)
        .unwrap();
    assert!(!status.managed_ready);
    assert_eq!(status.unavailable_reason, Some(ErrorCode::UnsupportedMode));
}

/// C13/C11: the managed system prompt ends with the request guide after the unchanged policy, the
/// notice and catalog travel as the tail, and the tail is never recorded in the session.
#[test]
fn the_guide_closes_the_prompt_and_the_tail_is_never_recorded() {
    const { assert!(limits::POLICY_VERSION == 1) };
    assert!(MANUAL_POLICY_V1.starts_with("Context management (policy 1)."));
    let recall = |id: &str, query: &str| {
        let arguments = format!(
            r#"{{"action": "search", "query": "{query}", "target": null, "cursor": null,
                "limit": 5, "max_bytes": 8192, "encoding": "text"}}"#
        );
        calls(&[(id, "context_recall", arguments.as_str())])
    };
    let model = <Model as ModelExt>::new(vec![
        calls(&[("e1", "echo", "{}")]),
        recall("r1", "mode=managed"),
        recall("r2", "go"),
        text("done"),
    ]);
    let runner = runner(&model, 64_000);
    let (mut session, disk, context) = managed(&runner);
    block(runner.run_turn_with_runtime(&mut session, "go", &mut Silent, host(&disk, &context)))
        .outcome
        .unwrap();
    for request in model.sent() {
        let Some(Message::System { text }) = request.messages.first() else {
            panic!("the system prompt leads")
        };
        assert!(
            text.ends_with(&format!("{MANUAL_POLICY_V1}\n\n{MANAGED_REQUEST_GUIDE_V1}")),
            "{text}"
        );
        assert_eq!(
            request
                .messages
                .iter()
                .filter(|message| matches!(message, Message::System { .. }))
                .count(),
            1,
            "no notice among the messages"
        );
        assert!(
            request
                .context_tail
                .as_deref()
                .is_some_and(|tail| tail.starts_with("[nanus context] mode=managed")),
            "{:?}",
            request.context_tail
        );
    }
    // Recall searches the log, where no request's tail was ever written: a search for the tail's
    // first field finds nothing, while a search for the user's own prompt finds it.
    let hits = |id: &str| {
        let result = tool_result_json(&session, id);
        result["hits"]
            .as_array()
            .unwrap_or_else(|| panic!("a search result: {result}"))
            .len()
    };
    assert_eq!(hits("r1"), 0, "the tail is never recalled");
    assert!(hits("r2") > 0, "the search itself finds what the log holds");
    let recorded = session.to_jsonl();
    assert!(!recorded.contains("[nanus context]"), "{recorded}");
    assert!(!disk.last().to_jsonl().contains("[nanus context]"));
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
        &mut Silent,
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
        &mut Silent,
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

/// The phases of every request attempt in a session, in log order.
fn attempt_phases(session: &nanus_domain::Session) -> Vec<AttemptPhase> {
    session
        .log()
        .events()
        .iter()
        .filter_map(|event| match event {
            SessionEvent::RequestAttempt { payload } => Some(payload.phase),
            _ => None,
        })
        .collect()
}

/// A step with nothing to decide is not checkpointed on its own: its finished attempt rides with
/// the next step's intent, and the last step's with the turn's end — `n + 1` commits, not `2n + 1`.
#[test]
fn a_step_with_nothing_to_decide_rides_with_the_next_checkpoint() {
    let model = <Model as ModelExt>::new(vec![call("e1", "echo", r#"{"size": 10}"#)]);
    model.push(text("done"));
    let runner = runner(&model, 64_000);
    let (mut session, disk, context) = managed(&runner);
    let before = disk.commits();
    let run =
        block(runner.run_turn_with_runtime(&mut session, "go", &mut Silent, host(&disk, &context)));
    assert!(run.outcome.is_ok(), "{:?}", run.outcome);
    assert_eq!(
        disk.reasons_since(before),
        vec![
            CheckpointReason::RequestIntent,
            CheckpointReason::RequestIntent,
            CheckpointReason::TurnEnd,
        ]
    );
    // Every outcome is on disk, each before the next intent.
    assert_eq!(&disk.last(), &session);
    assert_eq!(
        attempt_phases(&session),
        vec![
            AttemptPhase::Started,
            AttemptPhase::Finished,
            AttemptPhase::Started,
            AttemptPhase::Finished,
        ]
    );
}

/// Pair: the commit that carries a settled step is refused. Nothing more is sent, and the one
/// terminal attempt saves the step's outcome — what a refused settlement of its own would leave.
#[test]
fn a_refused_commit_carrying_a_settled_step_sends_nothing_and_keeps_its_outcome() {
    let model = <Model as ModelExt>::new(vec![call("e1", "echo", r#"{"size": 10}"#)]);
    model.push(text("never"));
    let runner = runner(&model, 64_000);
    let (mut session, disk, context) = managed(&runner);
    let before = disk.commits();
    // The first intent lands; the second, carrying the first step's settlement, is refused.
    disk.fail(1, Failure::Refuse, false);
    let run =
        block(runner.run_turn_with_runtime(&mut session, "go", &mut Silent, host(&disk, &context)));
    assert_eq!(
        run.outcome.err().and_then(|error| error.managed_code()),
        Some(ErrorCode::CheckpointNotCommitted)
    );
    assert_eq!(model.sent().len(), 1, "the second request was never sent");
    assert_eq!(
        disk.reasons_since(before),
        vec![
            CheckpointReason::RequestIntent,
            CheckpointReason::RequestIntent,
            CheckpointReason::TurnEnd,
        ],
        "the refused intent, then one terminal attempt"
    );
    let stored = disk.last();
    assert_eq!(
        attempt_phases(&stored),
        vec![AttemptPhase::Started, AttemptPhase::Finished],
        "the first step's outcome is saved and the refused intent is not"
    );
    assert!(matches!(
        stored.log().last_turn_end(),
        Some(nanus_domain::TurnEndReason::Error { .. })
    ));
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
        &mut Silent,
        host(&disk, &context),
    ));
    assert!(outcome.outcome.is_ok(), "{:?}", outcome.outcome);
    // A step that staged a proposal is still settled by a checkpoint of its own, because its
    // revision has to be acknowledged before the next request is prepared under it. Every other
    // step rides with the next intent.
    let reasons = disk.reasons_since(0);
    assert_eq!(
        reasons
            .iter()
            .filter(|reason| **reason == CheckpointReason::ContextRevision)
            .count(),
        1,
        "{reasons:?}"
    );
    assert!(
        !reasons.contains(&CheckpointReason::SettledStep),
        "{reasons:?}"
    );
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
    assert!(matches!(last.messages.get(1), Some(Message::User { .. })));
    // A hide-only revision with no notes and no goal has no working data: what is hidden is
    // named by the catalog in the tail, after the newest message.
    assert!(
        !last.messages.iter().any(|message| message
            .text()
            .is_some_and(|text| text.starts_with(MEMORY_LABEL))),
        "no generated data without notes or a goal"
    );
    assert!(
        last.context_tail
            .as_deref()
            .is_some_and(|tail| tail.contains("\nHidden history: 1 of")),
        "{:?}",
        last.context_tail
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
    let outcome =
        block(runner.run_turn_with_runtime(&mut session, "go", &mut Silent, host(&disk, &context)));
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
    block(runner.run_turn_with_runtime(&mut session, "go", &mut Silent, host(&disk, &context)))
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

/// F7: a legacy policy never upgrades a session that has not enabled managed context.
#[test]
fn a_legacy_policy_change_leaves_a_version_two_body_alone() {
    let model = <Model as ModelExt>::new(Vec::new());
    let runner = runner(&model, 64_000);
    let mut session = session();
    let disk = Disk::new();
    let context = context();
    let legacy = ContextPolicy {
        output_reserve_tokens: 9_000,
        ..ContextPolicy::default()
    };
    let changed = block(runner.set_context_policy(
        &mut session,
        legacy,
        ModeActor::Human,
        host(&disk, &context).runtime,
    ));
    assert!(matches!(changed, Ok(None)), "{changed:?}");
    assert!(!session.is_managed_body());
    assert_eq!(disk.commits(), 0);
}

/// The managed policy with the explicit 4,096 reservation this file was written against. The
/// default policy follows the model, which is a version-6 body and has its own tests.
fn explicit_managed() -> ContextPolicy {
    ContextPolicy {
        output_reserve_follows_model: false,
        ..ContextPolicy::managed()
    }
}

/// T19 and T18: enabling a following policy records the reservation in force, bounded by the
/// configured budget, and recording the same setting again writes nothing.
#[test]
fn enabling_a_following_policy_records_its_resolved_reservation_once() {
    let model = <Model as ModelExt>::new(Vec::new());
    let runner = runner(&model, 64_000);
    let mut session = session();
    let disk = Disk::new();
    let context = context();
    let enable = |session: &mut nanus_domain::Session| {
        block(runner.set_context_policy(
            session,
            ContextPolicy::managed(),
            ModeActor::Human,
            host(&disk, &context).runtime,
        ))
        .unwrap()
    };
    enable(&mut session);
    // The harness model declares an 8,192 ceiling; a 64,000 budget leaves it whole.
    let recorded = nanus_bundle::AgentRunner::context_policy(&session);
    assert!(recorded.output_reserve_follows_model);
    assert_eq!(recorded.output_reserve_tokens, 8_192);
    assert_eq!(session.body_version(), 6);
    let commits = disk.commits();
    enable(&mut session);
    assert_eq!(disk.commits(), commits, "the same setting writes no record");
}

/// T18: a configured budget smaller than the window bounds a following reservation to a quarter
/// of it, and an explicit reservation is never touched by the budget.
#[test]
fn a_configured_budget_bounds_a_following_reservation_and_an_explicit_one_is_kept() {
    let model = <Model as ModelExt>::new(Vec::new());
    let runner = runner(&model, 16_000);
    let mut session = session();
    let disk = Disk::new();
    let context = context();
    block(runner.set_context_policy(
        &mut session,
        ContextPolicy::managed(),
        ModeActor::Human,
        host(&disk, &context).runtime,
    ))
    .unwrap();
    let recorded = nanus_bundle::AgentRunner::context_policy(&session);
    assert_eq!(recorded.output_reserve_tokens, 4_000, "a quarter of 16,000");

    let status = runner
        .context_status(&session, host(&disk, &context).runtime)
        .unwrap();
    assert_eq!(status.output_reserve_tokens, 4_000);
    assert!(status.managed_ready);

    let mut explicit_session = self::session();
    block(runner.set_context_policy(
        &mut explicit_session,
        ContextPolicy {
            output_reserve_tokens: 4_096,
            output_reserve_follows_model: false,
            ..ContextPolicy::managed()
        },
        ModeActor::Human,
        host(&disk, &context).runtime,
    ))
    .unwrap();
    let explicit = nanus_bundle::AgentRunner::context_policy(&explicit_session);
    assert_eq!(explicit.output_reserve_tokens, 4_096);
    assert!(!explicit.output_reserve_follows_model);
}

/// Case 17, the step transaction: a selection change requested while a proposal is staged waits for
/// the step to settle. The step that staged the proposal is judged under the profile it was prepared
/// under, so the proposal is accepted; the switch lands after that settle, and the attempts before and
/// after it record the two models.
#[test]
fn a_selection_change_while_a_proposal_is_staged_lands_after_the_step_settles() {
    let model = <Model as ModelExt>::new(Vec::new());
    let runner = Rc::new(runner(&model, 64_000));
    let (mut session, disk, context) = managed(&runner);
    model.push(call("w1", "echo", r#"{"size": 10}"#));
    model.push(call("w2", "echo", r#"{"size": 10}"#));
    model.push(call("w3", "echo", r#"{"size": 10}"#));
    model.push(call("i1", "context_manage", INSPECT));
    // The model's answer proposes, and asks for a different model while the step is still open.
    let switcher = Rc::clone(&runner);
    let propose = propose_oldest("i2");
    model.push_with(move |request| {
        switcher.set_model("m-after");
        propose(request)
    });
    model.push(text("tidied"));
    let outcome = block(runner.run_turn_with_runtime(
        &mut session,
        "work, then tidy up",
        &mut Silent,
        host(&disk, &context),
    ));
    assert!(outcome.outcome.is_ok(), "{:?}", outcome.outcome);

    let accepted = session.log().events().iter().any(|event| {
        matches!(event, SessionEvent::ContextDecision { payload }
            if payload.outcome == DecisionOutcome::Accepted)
    });
    assert!(
        accepted,
        "the proposal was judged under its own step's profile, and accepted"
    );
    assert!(
        disk.reasons_since(0)
            .contains(&CheckpointReason::ContextRevision),
        "the step settled with the revision"
    );

    let models: Vec<String> = session
        .log()
        .events()
        .iter()
        .filter_map(|event| match event {
            SessionEvent::RequestAttempt { payload } => Some(payload.selection.model.clone()),
            _ => None,
        })
        .collect();
    let first_after = models.iter().position(|model| model == "m-after");
    assert!(first_after.is_some(), "the switch took effect: {models:?}");
    assert!(
        models[..first_after.unwrap_or(0)]
            .iter()
            .all(|model| model == "m"),
        "every step before the settle ran under the model it was prepared with: {models:?}"
    );
    assert_eq!(runner.model(), "m-after");
}

/// Case 22, the record: a resumed session with an explicit reservation that asks for `model`
/// records a following policy, its number resolved on the model in force, at version 6.
#[test]
fn a_resumed_explicit_session_that_asks_for_model_records_a_following_policy_at_version_six() {
    let model = <Model as ModelExt>::new(Vec::new());
    let runner = runner(&model, 64_000);
    let mut session = session();
    let disk = Disk::new();
    let context = context();
    let explicit = ContextPolicy {
        output_reserve_tokens: 4_096,
        output_reserve_follows_model: false,
        ..ContextPolicy::managed()
    };
    block(runner.set_context_policy(
        &mut session,
        explicit,
        ModeActor::Human,
        host(&disk, &context).runtime,
    ))
    .unwrap();
    assert_eq!(
        session.body_version(),
        3,
        "an explicit reservation is version three"
    );

    // What `--context-output-reserve model` on the resume asks for: the same policy, following.
    let mut asked = nanus_bundle::AgentRunner::context_policy(&session);
    asked.output_reserve_follows_model = true;
    block(runner.set_context_policy(
        &mut session,
        asked,
        ModeActor::Human,
        host(&disk, &context).runtime,
    ))
    .unwrap();
    let recorded = nanus_bundle::AgentRunner::context_policy(&session);
    assert!(recorded.output_reserve_follows_model);
    assert_eq!(
        recorded.output_reserve_tokens, 8_192,
        "resolved on the model in force"
    );
    assert_eq!(session.body_version(), 6);
}
