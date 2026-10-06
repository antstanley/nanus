//! Managed context when things go wrong: refused and uncertain saves, crashes, denials,
//! selection changes, and the model's own recall of what it can no longer see.

// A panic in a test *is* the assertion. The workspace denies the lint for production code.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

#[path = "managed_context/harness.rs"]
mod harness;

use std::rc::Rc;

use harness::*;
use nanus_bundle::Silent;
use nanus_domain::context::managed::{
    AttemptOutcome, AttemptPhase, ErrorCode, RequestAttemptRecord,
};
use nanus_domain::{SessionEvent, ToolAccess, ToolCall, TurnEndReason};
use nanus_ports::{LocalBoxFuture, PersistenceState, PolicyError, ToolPolicy, ToolPolicyDecision};
use serde_json::json;

/// T10: a refused intent checkpoint sends nothing, leaves the disk at its last acknowledged
/// state, and the turn makes exactly one reserved terminal attempt.
#[test]
fn a_refused_checkpoint_stops_before_dispatch_with_one_terminal_attempt() {
    let model = <Model as ModelExt>::new(vec![text("never")]);
    let runner = runner(&model, 64_000);
    let (mut session, disk, context) = managed(&runner);
    let before = disk.last();
    disk.fail(0, Failure::Refuse, false);
    let commits = disk.commits();
    let run =
        block(runner.run_turn_with_runtime(&mut session, "go", &mut Silent, host(&disk, &context)));
    assert_eq!(
        run.outcome.err().and_then(|error| error.managed_code()),
        Some(ErrorCode::CheckpointNotCommitted)
    );
    assert!(
        model.sent().is_empty(),
        "nothing was sent after the refused intent"
    );
    assert_eq!(
        disk.commits(),
        commits.saturating_add(2),
        "the intent and one terminal attempt"
    );
    assert!(
        matches!(run.persistence, Some(PersistenceState::Acknowledged(_))),
        "the terminal attempt landed"
    );
    assert_ne!(disk.last(), before);
    assert!(matches!(
        disk.last().log().last_turn_end(),
        Some(TurnEndReason::Error { .. })
    ));
}

/// T10: an uncertain commit that did not land is quarantined: nothing more is written.
#[test]
fn an_unknown_commit_that_did_not_land_freezes_the_session() {
    let model = <Model as ModelExt>::new(vec![text("never")]);
    let runner = runner(&model, 64_000);
    let (mut session, disk, context) = managed(&runner);
    disk.fail(0, Failure::Lose, false);
    let commits = disk.commits();
    let run =
        block(runner.run_turn_with_runtime(&mut session, "go", &mut Silent, host(&disk, &context)));
    assert_eq!(
        run.outcome.err().and_then(|error| error.managed_code()),
        Some(ErrorCode::CheckpointUnknown)
    );
    assert!(matches!(
        run.persistence,
        Some(PersistenceState::Unknown { .. })
    ));
    assert_eq!(
        disk.commits(),
        commits.saturating_add(1),
        "no alternate overwrite"
    );
    assert!(model.sent().is_empty());
}

/// T10: an uncertain commit that did land is installed by reconciliation and the turn goes on.
#[test]
fn an_unknown_commit_that_landed_is_installed_by_reconciliation() {
    let model = <Model as ModelExt>::new(vec![text("answer")]);
    let runner = runner(&model, 64_000);
    let (mut session, disk, context) = managed(&runner);
    disk.fail(0, Failure::Lose, true);
    let run =
        block(runner.run_turn_with_runtime(&mut session, "go", &mut Silent, host(&disk, &context)));
    assert_eq!(
        run.outcome.map(|outcome| outcome.answer).ok(),
        Some(String::from("answer"))
    );
    assert!(matches!(
        run.persistence,
        Some(PersistenceState::Acknowledged(_))
    ));
    assert_eq!(model.sent().len(), 1);
    assert_eq!(disk.last(), session);
}

/// T12: a crash left a turn open with an intent and no outcome: recovery closes it as
/// interrupted, records the unknown dispatch, and reruns nothing.
#[test]
fn a_resumed_open_turn_is_closed_without_rerunning_anything() {
    let model = <Model as ModelExt>::new(vec![call("c1", "echo", r#"{"size": 1}"#)]);
    let runner = runner(&model, 64_000);
    let (mut session, disk, context) = managed(&runner);
    // The crash: the intent is on disk, nothing after it.
    disk.fail(1, Failure::Refuse, false);
    let _ =
        block(runner.run_turn_with_runtime(&mut session, "go", &mut Silent, host(&disk, &context)));
    let mut crashed = session.clone();
    // Rebuild the on-disk shape a crash after the intent leaves: up to the started attempt.
    let cut = crashed
        .log()
        .events()
        .iter()
        .position(|event| matches!(event, SessionEvent::StepStart { .. }))
        .unwrap();
    let mut prefix = nanus_domain::Session::new(crashed.id().clone(), 0, "/w");
    prefix.upgrade_to_managed_body();
    for event in crashed.log().events().iter().take(cut) {
        prefix.append(event.clone());
    }
    crashed = prefix;
    let recovered_disk = Disk::new();
    let model = <Model as ModelExt>::new(vec![text("fresh")]);
    let runner = harness::runner(&model, 64_000);
    let recovered =
        block(runner.recover_session(&mut crashed, host(&recovered_disk, &context).runtime));
    assert!(
        matches!(recovered, Ok(Some(PersistenceState::Acknowledged(_)))),
        "{recovered:?}"
    );
    assert!(matches!(
        crashed.log().last_turn_end(),
        Some(TurnEndReason::Interrupted)
    ));
    let unknown: Vec<RequestAttemptRecord> = attempts(&crashed, AttemptPhase::Finished);
    assert_eq!(unknown.len(), 1);
    assert_eq!(unknown[0].outcome, Some(AttemptOutcome::UnknownDispatch));
    assert!(
        unknown[0].usage.is_none(),
        "unknown dispatch is not paid usage"
    );
    assert!(
        crashed
            .log()
            .events()
            .iter()
            .any(|event| matches!(event, SessionEvent::ContextRecovery { .. }))
    );
    assert!(model.sent().is_empty(), "nothing was rerun");
    assert_eq!(model.echoes(), 0);
}

/// A host policy that denies one action of the context tools.
struct Deny(&'static str);

impl ToolPolicy for Deny {
    fn decide<'a>(
        &'a self,
        call: &'a ToolCall,
        access: ToolAccess,
    ) -> LocalBoxFuture<'a, Result<ToolPolicyDecision, PolicyError>> {
        let deny =
            call.name.as_str() == self.0 || (self.0 == "write" && access == ToolAccess::Write);
        Box::pin(async move {
            Ok(if deny {
                ToolPolicyDecision::Deny {
                    reason: String::from("the host denies this"),
                }
            } else {
                ToolPolicyDecision::UseDefault
            })
        })
    }
}

/// T18: a host policy sees every context call with its access, and its denial stands.
#[test]
fn a_host_policy_denies_context_calls_without_a_goal_tool_bypass() {
    for (denied, arguments) in [
        ("context_manage", INSPECT),
        (
            "context_recall",
            r#"{"action": "search", "query": "x", "target": null, "cursor": null,
            "limit": 5, "max_bytes": 1000, "encoding": "text"}"#,
        ),
    ] {
        let model = <Model as ModelExt>::new(vec![call("d", denied, arguments), text("ok")]);
        let runner = runner(&model, 64_000).with_tool_policy(Rc::new(Deny(denied)));
        let (mut session, disk, context) = managed(&runner);
        block(runner.run_turn_with_runtime(&mut session, "go", &mut Silent, host(&disk, &context)))
            .outcome
            .unwrap();
        let (text, failed) = tool_result(&session, "d");
        assert!(
            failed && text.contains("the host denies this"),
            "{denied}: {text}"
        );
    }
    let model = <Model as ModelExt>::new(vec![
        call("w", "echo", r#"{"size": 1}"#),
        call(
            "p",
            "context_manage",
            r#"{"action": "propose", "base_revision": 0,
            "base_frontier": null, "hide": [], "restore": [], "notes": [], "cursor": null,
            "base_profile_digest": null}"#,
        ),
        text("ok"),
    ]);
    let runner = runner(&model, 64_000).with_tool_policy(Rc::new(Deny("write")));
    let (mut session, disk, context) = managed(&runner);
    block(runner.run_turn_with_runtime(&mut session, "go", &mut Silent, host(&disk, &context)))
        .outcome
        .unwrap();
    assert!(
        tool_result(&session, "p").1,
        "a proposal is a write the host may deny"
    );
    assert!(
        !session
            .log()
            .events()
            .iter()
            .any(|event| matches!(event, SessionEvent::ContextDecision { .. }))
    );
}

/// T16: a context call whose raw arguments pass the limit is never parsed and is a bounded
/// failure; one at the limit is parsed.
#[test]
fn an_oversized_context_call_is_refused_before_parsing() {
    let over = format!(
        r#"{{"action": "search", "query": "{}"}}"#,
        "q".repeat(2_100)
    );
    let model = <Model as ModelExt>::new(vec![call("o", "context_recall", &over), text("ok")]);
    let runner = runner(&model, 64_000);
    let (mut session, disk, context) = managed(&runner);
    block(runner.run_turn_with_runtime(&mut session, "go", &mut Silent, host(&disk, &context)))
        .outcome
        .unwrap();
    let (text, failed) = tool_result(&session, "o");
    assert!(failed && text.contains("exceeded 2048 bytes"), "{text}");
    let audited = session.log().events().iter().find_map(|event| match event {
        SessionEvent::ToolCall {
            call_id, arguments, ..
        } if call_id.as_str() == "o" => Some(arguments.clone()),
        _ => None,
    });
    assert_eq!(
        audited,
        Some(json!(nanus_ports::OVERSIZED_ARGUMENTS)),
        "no oversized value is retained"
    );
}

/// T19: a model switch to an unsupported path makes the session unready; the next request is
/// refused before HTTP and the selection is not silently reverted.
#[test]
fn switching_to_an_unsupported_model_refuses_the_next_request() {
    let model = <Model as ModelExt>::new(vec![text("first"), text("never")]);
    let runner = runner(&model, 64_000);
    let (mut session, disk, context) = managed(&runner);
    block(runner.run_turn_with_runtime(&mut session, "one", &mut Silent, host(&disk, &context)))
        .outcome
        .unwrap();
    model.set_supported(false);
    runner.set_model("another");
    let status = runner
        .context_status(&session, host(&disk, &context).runtime)
        .unwrap();
    assert!(!status.managed_ready);
    assert_eq!(
        status.unavailable_reason,
        Some(ErrorCode::ProtocolIncompatible)
    );
    let run = block(runner.run_turn_with_runtime(
        &mut session,
        "two",
        &mut Silent,
        host(&disk, &context),
    ));
    assert_eq!(
        run.outcome.err().and_then(|error| error.managed_code()),
        Some(ErrorCode::ProtocolIncompatible)
    );
    assert_eq!(
        model.sent().len(),
        1,
        "nothing was sent for the second turn"
    );
    assert_eq!(
        runner.model(),
        "another",
        "the selection is the host's to change back"
    );
}

/// T23/T24: hidden evidence is still recallable, by search and by exact read with its digest.
#[test]
fn hidden_evidence_is_found_and_read_back_with_its_digest() {
    let model = <Model as ModelExt>::new(Vec::new());
    let runner = runner(&model, 64_000);
    let (mut session, disk, context) = managed(&runner);
    model.push(call("w1", "echo", r#"{"size": 10}"#));
    model.push(call("w2", "echo", r#"{"size": 10}"#));
    model.push(call("w3", "echo", r#"{"size": 10}"#));
    model.push(call("i1", "context_manage", INSPECT));
    model.push_with(propose_oldest("i2"));
    model.push(call(
        "s1",
        "context_recall",
        r#"{"action": "search", "query": "xxxxxxxxxx",
        "target": null, "cursor": null, "limit": 1, "max_bytes": 8192, "encoding": "text"}"#,
    ));
    model.push_with(|request| {
        let found = last_tool_json(request);
        let source = &found["hits"][0]["source"];
        let target = json!({"kind": "event", "event_seq": source["event_seq"], "block_index": null,
            "artifact_id": null, "offset": 0, "length": 100, "field": source["field"]});
        let arguments = json!({"action": "read", "query": null, "target": target, "cursor": null,
            "limit": 1, "max_bytes": 100, "encoding": "text"});
        call("r1", "context_recall", &arguments.to_string())
    });
    model.push(text("recalled"));
    block(runner.run_turn_with_runtime(&mut session, "go", &mut Silent, host(&disk, &context)))
        .outcome
        .unwrap();
    let search = tool_result_json(&session, "s1");
    assert_eq!(search["status"], "ok");
    assert_eq!(search["hits"].as_array().map(Vec::len), Some(1));
    assert_eq!(
        search["coverage"], "partial",
        "the hit limit stopped it: {search}"
    );
    assert!(search["next_cursor"].is_string());
    let read = tool_result_json(&session, "r1");
    // The exact stored string, rendered text and its trailing newline included.
    assert_eq!(read["data"], "xxxxxxxxxx\n");
    assert_eq!(
        read["source"]["source_digest"],
        nanus_domain::context::managed::Digest::of(b"xxxxxxxxxx\n").as_str()
    );
    assert_eq!(
        read["durable"], true,
        "the source was checkpointed before it was read"
    );
}

/// T32: management steps are model calls; they consume the step budget like any other.
#[test]
fn management_steps_are_charged_against_the_step_budget() {
    let mut script = Vec::new();
    for index in 0..40 {
        script.push(call(&format!("i{index}"), "context_manage", INSPECT));
    }
    let model = <Model as ModelExt>::new(script);
    let runner = runner(&model, 64_000);
    let (mut session, disk, context) = managed(&runner);
    let run =
        block(runner.run_turn_with_runtime(&mut session, "go", &mut Silent, host(&disk, &context)));
    let outcome = run.outcome.unwrap();
    assert_eq!(outcome.reason, TurnEndReason::MaxSteps);
    assert_eq!(model.sent().len(), 32, "no free compaction steps");
}

/// T30: a goal change after notes were accepted marks them stale in what the model reads.
#[test]
fn notes_bound_to_an_older_goal_are_marked_stale() {
    let model = <Model as ModelExt>::new(Vec::new());
    let runner = runner(&model, 64_000);
    let (mut session, disk, context) = managed(&runner);
    model.push(call("w1", "echo", r#"{"size": 10}"#));
    model.push(call("w2", "echo", r#"{"size": 10}"#));
    model.push(call("w3", "echo", r#"{"size": 10}"#));
    model.push(call("i1", "context_manage", INSPECT));
    model.push_with(|request| {
        let inspected = last_tool_json(request);
        let oldest = eligible(&inspected).into_iter().next().unwrap();
        let seq: u64 = oldest.trim_start_matches("f:").parse().unwrap();
        let note = json!({"id": "n:echo", "claim": "echo returns filler", "category": "observed",
            "sources": [{"kind": "event", "event_seq": seq + 2, "block_index": null,
            "artifact_id": null, "offset": 0, "length": 10,
            "source_digest": nanus_domain::context::managed::Digest::of(b"xxxxxxxxxx\n").as_str(),
            "field": "tool_text"}]});
        let context = &inspected["context"];
        let arguments = json!({"action": "propose", "base_revision": context["revision"],
            "base_frontier": context["frontier"], "hide": [], "restore": [], "notes": [note],
            "cursor": null, "base_profile_digest": context["profile_digest"]});
        call("i2", "context_manage", &arguments.to_string())
    });
    model.push(call(
        "g",
        "create_goal",
        r#"{"objective": "a new direction"}"#,
    ));
    model.push(text("done"));
    block(runner.run_turn_with_runtime(&mut session, "go", &mut Silent, host(&disk, &context)))
        .outcome
        .unwrap();
    assert_eq!(
        tool_result_json(&session, "i2")["status"],
        "staged",
        "{}",
        tool_result(&session, "i2").0
    );
    let last = model.sent().pop().unwrap();
    let memory = last
        .messages
        .get(3)
        .and_then(nanus_domain::Message::text)
        .unwrap_or_default()
        .to_owned();
    assert!(memory.contains("n:echo"), "{memory}");
    assert!(
        memory.contains("STALE"),
        "the goal moved after the notes: {memory}"
    );
    assert!(memory.contains("a new direction"));
}

/// What one reservation was shown: the original request's length, and the managed revision and
/// effective request's length.
type Shown = (usize, Option<(u64, usize)>);

/// A batch-admission host that admits everything and records what it was shown.
#[derive(Default)]
struct Watching {
    seen: std::cell::RefCell<Vec<Shown>>,
}

struct Lease;

impl nanus_ports::ToolBatchReservation for Lease {
    fn admit(&self, _: &ToolCall) -> Result<(), nanus_ports::AdmissionError> {
        Ok(())
    }
    fn before_dispatch(&self, _: &ToolCall) -> Result<(), nanus_ports::AdmissionError> {
        Ok(())
    }
    fn validate_result(
        &self,
        _: &ToolCall,
        _: &nanus_domain::ToolResult,
        _: &nanus_domain::ToolResult,
    ) -> Result<(), nanus_ports::AdmissionError> {
        Ok(())
    }
    fn commit(&self, _: &nanus_ports::ChatRequest) -> Result<(), nanus_ports::AdmissionError> {
        Ok(())
    }
}

impl nanus_ports::ToolAdmission for Watching {
    fn reserve(
        &self,
        projection: &nanus_ports::ToolBatchProjection<'_>,
    ) -> Result<Box<dyn nanus_ports::ToolBatchReservation>, nanus_ports::AdmissionError> {
        let managed = projection
            .managed
            .as_ref()
            .map(|managed| (managed.revision, managed.effective.messages.len()));
        self.seen
            .borrow_mut()
            .push((projection.request.messages.len(), managed));
        Ok(Box::new(Lease))
    }
}

/// T20: admission sees the full original request, unchanged, and the effective request the next
/// step would send, with the accepted revision — never one in place of the other.
#[test]
fn admission_sees_the_original_and_the_effective_request_side_by_side() {
    let model = <Model as ModelExt>::new(Vec::new());
    let watching = Rc::new(Watching::default());
    let runner = runner(&model, 64_000).with_tool_admission(watching.clone());
    let (mut session, disk, context) = managed(&runner);
    model.push(call("w1", "echo", r#"{"size": 10}"#));
    model.push(call("w2", "echo", r#"{"size": 10}"#));
    model.push(call("w3", "echo", r#"{"size": 10}"#));
    model.push(call("i1", "context_manage", INSPECT));
    model.push_with(propose_oldest("i2"));
    model.push(call("w4", "echo", r#"{"size": 10}"#));
    model.push(text("done"));
    block(runner.run_turn_with_runtime(&mut session, "go", &mut Silent, host(&disk, &context)))
        .outcome
        .unwrap();
    let seen = watching.seen.borrow();
    assert_eq!(seen.len(), 6, "every batch was admitted: {seen:?}");
    assert!(seen.iter().all(|(_, managed)| managed.is_some()));
    let (original, last) = seen.last().copied().unwrap();
    let (revision, effective) = last.unwrap();
    assert_eq!(revision, 1, "the accepted revision is named");
    // Original: system + every message; effective: system, notice, generated data, fewer
    // messages because one fragment (a call and its result) is hidden.
    assert!(
        effective < original.saturating_add(2),
        "{effective} vs {original}"
    );
    assert!(original >= 10);
}

/// F5: an inspect cursor from one step still pages the catalog in the next, though the step
/// appended records in between, and every fragment is listed exactly once.
#[test]
fn an_inspect_cursor_pages_the_whole_catalog_across_steps() {
    let model = <Model as ModelExt>::new(Vec::new());
    let runner = runner_steps(&model, 256_000, 64);
    let (mut session, disk, context) = managed(&runner);
    for index in 0..45 {
        model.push(call(&format!("w{index}"), "echo", r#"{"size": 1}"#));
    }
    model.push(call("i1", "context_manage", INSPECT));
    model.push_with(|request| {
        let first = last_tool_json(request);
        let cursor = first["next_cursor"].as_str().unwrap().to_owned();
        let arguments = serde_json::json!({"action": "inspect", "base_revision": null,
            "base_frontier": null, "hide": [], "restore": [], "notes": [], "cursor": cursor,
            "base_profile_digest": null});
        call("i2", "context_manage", &arguments.to_string())
    });
    model.push(text("listed"));
    block(runner.run_turn_with_runtime(&mut session, "go", &mut Silent, host(&disk, &context)))
        .outcome
        .unwrap();
    let first = tool_result_json(&session, "i1");
    let second = tool_result_json(&session, "i2");
    assert_eq!(second["status"], "inspected", "{second}");
    let ids = |page: &serde_json::Value| -> Vec<String> {
        page["fragments"]
            .as_array()
            .unwrap()
            .iter()
            .map(|fragment| fragment["id"].as_str().unwrap().to_owned())
            .collect()
    };
    let mut all = ids(&first);
    all.extend(ids(&second));
    let total = all.len();
    all.sort();
    all.dedup();
    assert_eq!(all.len(), total, "no fragment is listed twice");
    assert!(
        total >= 46,
        "every fragment, the first inspect's own included: {total}"
    );
}

/// A batch admission whose lease refuses to commit any batch holding a proposal.
struct RefusesProposals;

struct RefusingLease(bool);

impl nanus_ports::ToolBatchReservation for RefusingLease {
    fn admit(&self, _: &ToolCall) -> Result<(), nanus_ports::AdmissionError> {
        Ok(())
    }
    fn before_dispatch(&self, _: &ToolCall) -> Result<(), nanus_ports::AdmissionError> {
        Ok(())
    }
    fn validate_result(
        &self,
        _: &ToolCall,
        _: &nanus_domain::ToolResult,
        _: &nanus_domain::ToolResult,
    ) -> Result<(), nanus_ports::AdmissionError> {
        Ok(())
    }
    fn commit(&self, _: &nanus_ports::ChatRequest) -> Result<(), nanus_ports::AdmissionError> {
        if self.0 {
            Err(nanus_ports::AdmissionError {
                message: "the ledger is full".to_owned(),
            })
        } else {
            Ok(())
        }
    }
}

impl nanus_ports::ToolAdmission for RefusesProposals {
    fn reserve(
        &self,
        projection: &nanus_ports::ToolBatchProjection<'_>,
    ) -> Result<Box<dyn nanus_ports::ToolBatchReservation>, nanus_ports::AdmissionError> {
        let proposal = projection
            .calls
            .iter()
            .any(|call| call.arguments["action"] == "propose");
        Ok(Box::new(RefusingLease(proposal)))
    }
}

/// F11: a proposal staged in a step whose batch lease could not commit is refused, not accepted.
#[test]
fn a_proposal_in_a_failed_step_is_never_accepted() {
    let model = <Model as ModelExt>::new(Vec::new());
    let runner = runner(&model, 64_000).with_tool_admission(Rc::new(RefusesProposals));
    let (mut session, disk, context) = managed(&runner);
    model.push(call("w1", "echo", r#"{"size": 10}"#));
    model.push(call("w2", "echo", r#"{"size": 10}"#));
    model.push(call("w3", "echo", r#"{"size": 10}"#));
    model.push(call("i1", "context_manage", INSPECT));
    model.push_with(propose_oldest("i2"));
    model.push(text("never"));
    let run =
        block(runner.run_turn_with_runtime(&mut session, "go", &mut Silent, host(&disk, &context)));
    assert!(run.outcome.is_err());
    assert!(
        !session
            .log()
            .events()
            .iter()
            .any(|event| matches!(event, SessionEvent::ContextRevision { .. })),
        "no revision was accepted"
    );
    let rejected = session.log().events().iter().any(|event| {
        matches!(event,
        SessionEvent::ContextDecision { payload }
            if payload.outcome == nanus_domain::context::managed::DecisionOutcome::Rejected)
    });
    assert!(rejected);
    assert_eq!(disk.last(), session, "the failure is recorded and saved");
}
