//! Maintenance requests through the real runner: a compaction of a legacy session, and a handoff
//! from a managed one, with the seed a new session opens with. Each claim is tested in both
//! directions.
//!
//! The scripted model and the disk are the managed-context doubles, so what is asserted is what
//! reached the model and what reached the disk.

// A panic in a test *is* the assertion. The workspace denies the lint for production code.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

#[path = "managed_context/harness.rs"]
mod harness;

use harness::*;
use nanus_bundle::{CompactionDraft, HandoffDraft, Silent};
use nanus_domain::context::managed::{
    AttemptOutcome, AttemptPhase, ContextPolicy, ErrorCode, HANDOFF_SEED_LABEL, MEMORY_LABEL,
    MaintenanceAttemptKind, ModeActor, RecoveryReason, RequestAttemptRecord,
};
use nanus_domain::context::{COMPACTION_LABEL, COMPACTION_SUMMARY_BYTES_MAX};
use nanus_domain::{Message, Session, SessionEvent, SessionId};
use nanus_ports::{CheckpointReason, FinishReason, LlmEvent, PersistenceState};

const SYSTEM: &str = "you are a test";

/// Runs `count` legacy turns, each answered with a short reply.
fn legacy_turns(runner: &nanus_bundle::AgentRunner, session: &mut Session, count: u32) {
    for index in 0..count {
        block(runner.run_turn(session, &format!("question {index}"), &mut Silent, None)).unwrap();
    }
}

/// A legacy session with two answered turns. The model is scripted for those turns only, so a test
/// queues whatever the maintenance request should answer.
fn legacy_session() -> (Model, nanus_bundle::AgentRunner, Session) {
    let model = <Model as ModelExt>::new(vec![text("answer 0"), text("answer 1")]);
    let runner = runner(&model, 64_000);
    let mut session = session();
    legacy_turns(&runner, &mut session, 2);
    (model, runner, session)
}

fn attempt_records(events: &[SessionEvent]) -> Vec<RequestAttemptRecord> {
    events
        .iter()
        .filter_map(|event| match event {
            SessionEvent::RequestAttempt { payload } => Some((**payload).clone()),
            _ => None,
        })
        .collect()
}

fn refused_with(
    result: Result<CompactionDraft, nanus_bundle::BundleError>,
    code: ErrorCode,
) -> bool {
    result.err().and_then(|error| error.code()) == Some(code)
}

fn handoff_refused(
    result: Result<HandoffDraft, nanus_bundle::BundleError>,
    code: ErrorCode,
) -> bool {
    result.err().and_then(|error| error.code()) == Some(code)
}

/// The generated message's text in a request, when the request carries one.
fn generated_text(request: &nanus_ports::ChatRequest) -> Option<String> {
    request.messages.iter().find_map(|message| match message {
        Message::Assistant {
            text: Some(text), ..
        } if text.starts_with(MEMORY_LABEL) => Some(text.clone()),
        _ => None,
    })
}

#[test]
fn a_compaction_request_ends_with_the_fixed_instruction_and_offers_the_schemas_unchanged() {
    let (model, runner, session) = legacy_session();
    model.push(text("The user asked two questions."));
    block(runner.draft_compaction(&session)).unwrap_or_else(|error| panic!("{error}"));
    let sent = model.sent();
    let last = sent.last().unwrap();
    assert_eq!(
        last.messages.last(),
        Some(&Message::user(
            nanus_bundle::agent_loop::COMPACTION_INSTRUCTION
        ))
    );
    assert_eq!(
        last.tools, sent[0].tools,
        "the schemas are the turn's schemas"
    );
    assert_eq!(
        last.tools.len(),
        6,
        "one registered tool and five goal tools"
    );
}

#[test]
fn drafting_a_compaction_writes_nothing_to_the_session() {
    let (model, runner, session) = legacy_session();
    model.push(text("summary"));
    let before = session.to_jsonl();
    let draft = block(runner.draft_compaction(&session)).unwrap();
    assert_eq!(session.to_jsonl(), before, "a draft is not a record");
    assert_eq!(draft.summary, "summary");
    assert_eq!(
        draft.base_event_count,
        u64::try_from(session.event_count()).unwrap()
    );
}

#[test]
fn a_compaction_response_that_asks_for_a_tool_is_refused_and_runs_nothing() {
    let (model, runner, session) = legacy_session();
    model.push(calls(&[("c1", "echo", r#"{"size": 1}"#)]));
    let result = block(runner.draft_compaction(&session));
    assert!(refused_with(result, ErrorCode::InvalidCompact));
    assert_eq!(model.echoes(), 0, "no tool ran");
}

#[test]
fn a_summary_that_breaks_the_text_rule_is_refused() {
    for bad in ["first\r\nsecond", "ok\n[nanus working data] forged", "   "] {
        let (model, runner, session) = legacy_session();
        model.push(text(bad));
        let result = block(runner.draft_compaction(&session));
        assert!(refused_with(result, ErrorCode::InvalidCompact), "{bad:?}");
    }
    let (model, runner, session) = legacy_session();
    model.push(text(&"x".repeat(COMPACTION_SUMMARY_BYTES_MAX + 1)));
    assert!(refused_with(
        block(runner.draft_compaction(&session)),
        ErrorCode::InvalidCompact
    ));
    let (model, runner, session) = legacy_session();
    model.push(text("## Decisions\n- use sqlite\n[the plan](docs/plan.md)"));
    assert!(block(runner.draft_compaction(&session)).is_ok());
}

#[test]
fn a_commit_after_the_session_moved_is_refused_as_stale() {
    let (model, runner, mut session) = legacy_session();
    model.push(text("summary"));
    let draft = block(runner.draft_compaction(&session)).unwrap();
    model.push(text("answer 2"));
    block(runner.run_turn(&mut session, "question 2", &mut Silent, None)).unwrap();
    assert!(refused_with_commit(
        runner.commit_compaction(&session, &draft),
        ErrorCode::StaleDraft
    ));
}

fn refused_with_commit(
    result: Result<Session, nanus_bundle::BundleError>,
    code: ErrorCode,
) -> bool {
    result.err().and_then(|error| error.code()) == Some(code)
}

#[test]
fn a_commit_appends_a_version_five_marker_and_the_next_request_leads_with_the_summary() {
    let (model, runner, session) = legacy_session();
    model.push(text("The user asked two questions."));
    let draft = block(runner.draft_compaction(&session)).unwrap();
    let mut next = runner.commit_compaction(&session, &draft).unwrap();
    assert!(next.is_compacted());
    assert_eq!(next.body_version(), 5);
    assert_eq!(session.event_count() + 1, next.event_count());

    model.push(text("answer after the cut"));
    block(runner.run_turn(&mut next, "question 2", &mut Silent, None)).unwrap();
    let request = model.sent().pop().unwrap();
    assert!(matches!(&request.messages[0], Message::System { text } if text.starts_with(SYSTEM)));
    assert_eq!(
        request.messages[1],
        Message::system(format!("{COMPACTION_LABEL}\nThe user asked two questions."))
    );
    assert!(
        !request.messages.iter().any(|message| matches!(
            message,
            Message::User { text, .. } if text == "question 0"
        )),
        "nothing before the cut is folded into the prompt"
    );
    assert!(request.messages.iter().any(|message| matches!(
        message,
        Message::User { text, .. } if text == "question 2"
    )));
}

#[test]
fn a_second_compaction_is_refused_until_a_turn_completes_after_the_first() {
    let (model, runner, mut session) = legacy_session();
    model.push(text("first summary"));
    let draft = block(runner.draft_compaction(&session)).unwrap();
    session = runner.commit_compaction(&session, &draft).unwrap();
    assert!(refused_with(
        block(runner.draft_compaction(&session)),
        ErrorCode::CompactUnavailable
    ));
    model.push(text("answer 2"));
    block(runner.run_turn(&mut session, "question 2", &mut Silent, None)).unwrap();
    model.push(text("second summary"));
    assert!(block(runner.draft_compaction(&session)).is_ok());
}

#[test]
fn a_session_without_a_marker_sends_exactly_the_request_it_always_did() {
    // Two turns, the first with a tool round, so the prompt fold has a call, its result and an
    // earlier turn to fold: a fold that started anywhere but the beginning would change the second
    // request, which a single-turn session could never show.
    let model = <Model as ModelExt>::new(vec![
        call("c1", "echo", r#"{"size": 1}"#),
        text("hello"),
        text("again to you"),
    ]);
    let runner = runner(&model, 64_000);
    let mut session = session();
    block(runner.run_turn(&mut session, "hi", &mut Silent, None)).unwrap();
    let before_reply = {
        let mut asked = session.clone();
        asked.append(SessionEvent::TurnStart { turn: 1 });
        asked.append(SessionEvent::UserMessage {
            text: "again".into(),
            content_blocks: None,
        });
        asked
    };
    block(runner.run_turn(&mut session, "again", &mut Silent, None)).unwrap();
    let sent = model.sent();
    let second = sent.last().unwrap();
    assert!(matches!(&second.messages[0], Message::System { text } if text.starts_with(SYSTEM)));
    assert_eq!(
        second.messages[1..],
        before_reply.derive_messages()[..],
        "the system prompt, then the whole fold of the log as it stood"
    );
    assert!(
        second
            .messages
            .iter()
            .any(|message| matches!(message, Message::Tool { .. })),
        "the earlier turn's tool round is folded in"
    );
    assert_eq!(session.derive_prompt_messages(), session.derive_messages());
    assert_eq!(
        session.body_version(),
        2,
        "a legacy session stays at version two"
    );
}

#[test]
fn compaction_is_refused_on_a_managed_session() {
    let model = <Model as ModelExt>::new(Vec::new());
    let runner = runner(&model, 64_000);
    let (mut session, disk, context) = managed(&runner);
    block(runner.run_turn_with_runtime(&mut session, "first", &mut Silent, host(&disk, &context)));
    assert!(refused_with(
        block(runner.draft_compaction(&session)),
        ErrorCode::CompactUnavailable
    ));
}

#[test]
fn enabling_managed_context_on_a_compacted_session_is_refused_before_any_write() {
    let (model, runner, session) = legacy_session();
    model.push(text("summary"));
    let draft = block(runner.draft_compaction(&session)).unwrap();
    let compacted = runner.commit_compaction(&session, &draft).unwrap();
    let mut session = compacted;
    let before = session.clone();
    let disk = Disk::new();
    let context = context();
    let refused = block(runner.set_context_policy(
        &mut session,
        ContextPolicy::managed(),
        ModeActor::Human,
        host(&disk, &context).runtime,
    ));
    assert_eq!(
        refused.err().and_then(|error| error.code()),
        Some(ErrorCode::CompactedSession)
    );
    assert_eq!(session, before, "the session is unchanged");
    assert_eq!(disk.commits(), 0, "nothing was written");
    drop(model);
}

#[test]
fn a_handoff_is_refused_on_a_legacy_session_and_writes_nothing() {
    let (_model, runner, mut session) = legacy_session();
    let before = session.clone();
    let disk = Disk::new();
    let context = context();
    let result = block(runner.draft_handoff(&mut session, host(&disk, &context).runtime));
    assert!(handoff_refused(result, ErrorCode::HandoffUnavailable));
    assert_eq!(session, before);
    assert_eq!(disk.commits(), 0);
}

/// A managed session with one answered turn, and a model scripted for that turn and a handoff.
fn managed_with_history(
    handoff: Vec<LlmEventList>,
) -> (
    Model,
    nanus_bundle::AgentRunner,
    Session,
    Disk,
    nanus_bundle::SessionContext,
) {
    let model = <Model as ModelExt>::new(Vec::new());
    model.push(text("turn answer"));
    for step in handoff {
        model.push(step);
    }
    let runner = runner(&model, 64_000);
    let (mut session, disk, context) = managed(&runner);
    let outcome = block(runner.run_turn_with_runtime(
        &mut session,
        "first",
        &mut Silent,
        host(&disk, &context),
    ));
    assert!(outcome.outcome.is_ok(), "{:?}", outcome.outcome);
    (model, runner, session, disk, context)
}

type LlmEventList = Vec<LlmEvent>;

#[test]
fn a_handoff_writes_only_its_intent_and_its_finished_attempt_both_of_kind_handoff() {
    let (_model, runner, mut session, disk, context) = managed_with_history(vec![text(
        "## Goal\n- ship the parser\n[plan](docs/plan.md)",
    )]);
    let before = session.event_count();
    let commits = disk.commits();
    let draft = block(runner.draft_handoff(&mut session, host(&disk, &context).runtime))
        .unwrap_or_else(|error| panic!("{error}"));
    let added = &session.log().events()[before..];
    assert!(
        added
            .iter()
            .all(|event| matches!(event, SessionEvent::RequestAttempt { .. })),
        "only attempt records"
    );
    let records = attempt_records(added);
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].phase, AttemptPhase::Started);
    assert_eq!(records[1].phase, AttemptPhase::Finished);
    assert_eq!(records[1].outcome, Some(AttemptOutcome::Completed));
    assert!(
        records
            .iter()
            .all(|record| record.kind == Some(MaintenanceAttemptKind::Handoff))
    );
    assert_eq!(
        disk.commits(),
        commits + 2,
        "intent, then the finished record"
    );
    assert!(draft.text.contains("## Goal"));
}

#[test]
fn a_handoff_base_frontier_is_taken_after_its_own_finished_attempt() {
    let (_model, runner, mut session, disk, context) =
        managed_with_history(vec![text("the narrative")]);
    let before = session.event_count();
    let draft = block(runner.draft_handoff(&mut session, host(&disk, &context).runtime)).unwrap();
    let count = u64::try_from(session.event_count()).unwrap();
    assert_eq!(draft.base.event_count, count);
    assert!(count > u64::try_from(before).unwrap());
    assert_eq!(
        draft.base.prefix_blake3,
        session.prefix_digest(count).unwrap()
    );
    assert!(draft.text.contains("source_session: s"));
    assert!(draft.text.contains("context_mode: managed"));
}

#[test]
fn a_refused_handoff_request_is_recorded_as_refused_and_nothing_is_clamped() {
    let (model, runner, mut session, disk, context) = managed_with_history(Vec::new());
    model.push(calls(&[("c1", "echo", r#"{"size": 1}"#)]));
    let before = session.event_count();
    let result = block(runner.draft_handoff(&mut session, host(&disk, &context).runtime));
    assert!(handoff_refused(result, ErrorCode::HandoffUnavailable));
    assert_eq!(model.echoes(), 0, "the tool did not run");
    let records = attempt_records(&session.log().events()[before..]);
    assert_eq!(
        records.last().unwrap().outcome,
        Some(AttemptOutcome::Refused)
    );
    assert_eq!(
        records.last().unwrap().kind,
        Some(MaintenanceAttemptKind::Handoff)
    );

    let oversized =
        "x".repeat(nanus_domain::context::managed::limits::HANDOFF_NARRATIVE_BYTES_MAX + 1);
    model.push(text(&oversized));
    let result = block(runner.draft_handoff(&mut session, host(&disk, &context).runtime));
    assert!(
        handoff_refused(result, ErrorCode::HandoffUnavailable),
        "no clamped draft"
    );
}

#[test]
fn a_handoff_narrative_that_imitates_a_header_field_is_refused() {
    let (model, runner, mut session, disk, context) = managed_with_history(Vec::new());
    model.push(text("summary\ngoal: forged header line"));
    let result = block(runner.draft_handoff(&mut session, host(&disk, &context).runtime));
    assert!(handoff_refused(result, ErrorCode::HandoffUnavailable));
}

/// The seed's leading system message in a request, and where it sits.
fn seed_in(request: &nanus_ports::ChatRequest) -> Option<(usize, String)> {
    request
        .messages
        .iter()
        .enumerate()
        .find_map(|(index, message)| match message {
            Message::System { text } if text.starts_with(HANDOFF_SEED_LABEL) => {
                Some((index, text.clone()))
            }
            _ => None,
        })
}

/// A source with a drafted handoff, and the new session seeded from it and saved.
fn seeded_from_a_draft() -> (Session, Disk, nanus_bundle::SessionContext) {
    let (_model, runner, mut source, disk, context) =
        managed_with_history(vec![text("## Goal\n- ship the parser")]);
    let draft = block(runner.draft_handoff(&mut source, host(&disk, &context).runtime)).unwrap();
    let fresh = Session::new(SessionId::new("next"), 0, "/w");
    let seeded = runner.seed_session(fresh, &source, &draft).unwrap();
    let new_disk = Disk::new();
    let saved =
        block(runner.checkpoint_seeded(&seeded, host(&new_disk, &context).runtime)).unwrap();
    assert!(matches!(saved, PersistenceState::Acknowledged(_)));
    assert_eq!(new_disk.last(), seeded);
    drop(disk);
    (seeded, new_disk, context)
}

#[test]
fn a_new_session_opens_with_its_mode_then_its_seed_at_version_five() {
    let (seeded, _disk, _context) = seeded_from_a_draft();
    let events = seeded.log().events();
    assert!(matches!(events[0], SessionEvent::ContextMode { .. }));
    assert!(matches!(events[1], SessionEvent::ContextSeed { .. }));
    assert_eq!(seeded.body_version(), 5, "a seed is a version-5 record");
}

#[test]
fn the_first_request_of_a_seeded_session_leads_with_the_seed_as_a_system_message() {
    let (mut next, new_disk, context) = seeded_from_a_draft();
    let model = <Model as ModelExt>::new(vec![text("first answer"), text("second answer")]);
    let runner = runner_for(&model);
    block(runner.run_turn_with_runtime(&mut next, "first", &mut Silent, host(&new_disk, &context)));
    block(runner.run_turn_with_runtime(
        &mut next,
        "second",
        &mut Silent,
        host(&new_disk, &context),
    ));
    let sent = model.sent();
    for request in &sent {
        let (at, seed) = seed_in(request).expect("every request carries the seed");
        assert_eq!(at, 2, "after the system prompt and the notice");
        assert!(seed.contains("Source session: s."), "{seed}");
        assert!(seed.contains("ship the parser"));
        assert!(
            request.messages[..at]
                .iter()
                .all(|message| matches!(message, Message::System { .. })),
            "a leading system message"
        );
        assert!(
            generated_text(request).is_none_or(|text| !text.contains("ship the parser")),
            "the seed is not in the generated message"
        );
        assert!(!request.messages.iter().any(|message| matches!(
            message,
            Message::User { text, .. } if text.contains("ship the parser")
        )));
    }
    let first = &sent[0];
    assert_eq!(
        first
            .messages
            .iter()
            .filter(|message| matches!(message, Message::User { .. }))
            .count(),
        1,
        "the first request has only the first prompt"
    );
}

#[test]
fn a_managed_session_with_no_seed_sends_the_request_it_always_did() {
    let model = <Model as ModelExt>::new(vec![text("hello")]);
    let runner = runner(&model, 64_000);
    let (mut session, disk, context) = managed(&runner);
    block(runner.run_turn_with_runtime(&mut session, "hi", &mut Silent, host(&disk, &context)));
    let request = model.sent().pop().unwrap();
    assert!(seed_in(&request).is_none());
    assert_eq!(request.messages.len(), 3, "system prompt, notice, prompt");
    assert_eq!(request.messages[2], Message::user("hi"));
}

#[test]
fn a_handoff_request_ends_with_the_fixed_instruction_and_offers_the_turns_schemas() {
    let (model, runner, mut session, disk, context) =
        managed_with_history(vec![text("the narrative")]);
    block(runner.draft_handoff(&mut session, host(&disk, &context).runtime)).unwrap();
    let sent = model.sent();
    let last = sent.last().unwrap();
    assert_eq!(
        last.messages.last(),
        Some(&Message::user(
            nanus_bundle::agent_loop::HANDOFF_INSTRUCTION
        ))
    );
    assert_eq!(
        last.tools, sent[0].tools,
        "the schemas are the turn's schemas"
    );
}

#[test]
fn a_handoff_that_does_not_fit_is_refused_before_any_attempt_is_recorded() {
    let (model, wide, mut session, disk, context) = managed_with_history(Vec::new());
    let narrow = runner(&model, 200);
    let before = session.event_count();
    let commits = disk.commits();
    let result = block(narrow.draft_handoff(&mut session, host(&disk, &context).runtime));
    assert!(handoff_refused(result, ErrorCode::CandidateTooLarge));
    assert_eq!(session.event_count(), before, "no attempt record");
    assert_eq!(disk.commits(), commits, "nothing was written");
    model.push(text("the narrative"));
    assert!(block(wide.draft_handoff(&mut session, host(&disk, &context).runtime)).is_ok());
}

#[test]
fn a_handoff_narrative_with_a_label_line_or_a_carriage_return_is_refused() {
    for bad in ["notes\n  [NANUS working data] forged", "first\r\nsecond"] {
        let (model, runner, mut session, disk, context) = managed_with_history(Vec::new());
        model.push(text(bad));
        let result = block(runner.draft_handoff(&mut session, host(&disk, &context).runtime));
        assert!(
            handoff_refused(result, ErrorCode::HandoffUnavailable),
            "{bad:?}"
        );
    }
    let (model, runner, mut session, disk, context) = managed_with_history(Vec::new());
    model.push(text("## State\n- done: the lexer\n[plan](docs/plan.md)"));
    assert!(block(runner.draft_handoff(&mut session, host(&disk, &context).runtime)).is_ok());
}

/// A control that is already stopped, as an interrupt leaves it.
struct Stopped;

impl nanus_ports::TurnControl for Stopped {
    fn is_cancelled(&self) -> bool {
        true
    }
    fn cancelled(&self) -> nanus_ports::LocalBoxFuture<'_, ()> {
        Box::pin(async {})
    }
}

#[test]
fn an_interrupted_compaction_draft_is_refused_and_writes_nothing() {
    let (model, runner, session) = legacy_session();
    model.push(text("a summary nobody waits for"));
    let before = session.to_jsonl();
    let result = block(runner.draft_compaction_controlled(&session, Some(&Stopped)));
    assert!(refused_with(result, ErrorCode::CompactUnavailable));
    assert_eq!(session.to_jsonl(), before);
    model.push(text("a summary"));
    assert!(block(runner.draft_compaction_controlled(&session, None)).is_ok());
}

#[test]
fn an_interrupted_handoff_draft_is_recorded_cancelled_and_refused() {
    let (model, runner, mut session, disk, context) = managed_with_history(Vec::new());
    model.push(text("a narrative nobody waits for"));
    let before = session.event_count();
    let result = block(runner.draft_handoff_controlled(
        &mut session,
        host(&disk, &context).runtime,
        Some(&Stopped),
    ));
    assert!(handoff_refused(result, ErrorCode::HandoffUnavailable));
    let records = attempt_records(&session.log().events()[before..]);
    assert_eq!(records.len(), 2, "the intent and its outcome");
    assert_eq!(records[1].outcome, Some(AttemptOutcome::Cancelled));
}

#[test]
fn a_handoff_intent_left_without_an_outcome_is_settled_before_the_next_draft() {
    let (model, runner, mut session, disk, context) = managed_with_history(Vec::new());
    model.push(text("the first narrative"));
    // The intent is saved; the finished record is refused, so the intent has no outcome.
    disk.fail(1, Failure::Refuse, false);
    assert!(block(runner.draft_handoff(&mut session, host(&disk, &context).runtime)).is_err());
    assert_eq!(
        nanus_domain::context::managed::state::unmatched_attempts(session.log()).len(),
        1
    );
    model.push(text("the second narrative"));
    block(runner.draft_handoff(&mut session, host(&disk, &context).runtime)).unwrap();
    let recovery = session.log().events().iter().find_map(|event| match event {
        SessionEvent::ContextRecovery { payload } => Some(payload.reason),
        _ => None,
    });
    assert_eq!(recovery, Some(RecoveryReason::SettledMaintenanceAttempt));
    assert!(nanus_domain::context::managed::state::unmatched_attempts(session.log()).is_empty());
    assert!(
        !session
            .log()
            .events()
            .iter()
            .any(|event| matches!(event, SessionEvent::TurnEnd { reason, .. }
                if *reason == nanus_domain::TurnEndReason::Interrupted)),
        "no turn was closed for it"
    );
}

#[test]
fn a_second_compaction_draft_is_given_the_first_summary() {
    let (model, runner, mut session) = legacy_session();
    model.push(text("FIRST-SUMMARY: two questions"));
    let draft = block(runner.draft_compaction(&session)).unwrap();
    session = runner.commit_compaction(&session, &draft).unwrap();
    model.push(text("answer 2"));
    block(runner.run_turn(&mut session, "question 2", &mut Silent, None)).unwrap();
    model.push(text("second summary"));
    block(runner.draft_compaction(&session)).unwrap();
    let request = model.sent().pop().unwrap();
    assert!(request.messages.iter().any(|message| matches!(
        message,
        Message::System { text } if text.contains("FIRST-SUMMARY: two questions")
    )));
    assert!(!request.messages.iter().any(|message| matches!(
        message,
        Message::User { text, .. } if text == "question 0"
    )));
}

#[test]
fn a_compaction_drafts_usage_reaches_the_marker_and_the_sessions_totals() {
    let (model, runner, session) = legacy_session();
    model.push(vec![
        LlmEvent::TextDelta("summary".to_owned()),
        LlmEvent::Usage(nanus_domain::Usage::new(70, 9, 0, 0, 70)),
        LlmEvent::Finished {
            reason: FinishReason::Stop,
        },
    ]);
    let before = session.usage_totals();
    let draft = block(runner.draft_compaction(&session)).unwrap();
    assert_eq!(draft.usage.map(|usage| usage.prompt_tokens), Some(70));
    let next = runner.commit_compaction(&session, &draft).unwrap();
    assert_eq!(next.usage_totals().prompt_tokens, before.prompt_tokens + 70);
    assert!(
        next.usage_by_model()
            .iter()
            .any(|(model, usage)| model.as_deref() == Some("m") && usage.prompt_tokens >= 70)
    );
    assert_eq!(
        session.usage_totals(),
        before,
        "a draft alone records nothing"
    );
}

/// A runner over `model` with the harness's configuration.
fn runner_for(model: &Model) -> nanus_bundle::AgentRunner {
    runner(model, 64_000)
}

#[test]
fn the_back_link_is_recorded_in_the_source_when_the_source_has_not_moved() {
    let (_model, runner, mut source, disk, context) =
        managed_with_history(vec![text("the narrative")]);
    let draft = block(runner.draft_handoff(&mut source, host(&disk, &context).runtime)).unwrap();
    let new_id = SessionId::new("next");
    let commits = disk.commits();
    let recorded =
        block(runner.record_handoff(&mut source, &draft, &new_id, host(&disk, &context).runtime));
    assert!(
        matches!(recorded, Ok(PersistenceState::Acknowledged(_))),
        "{recorded:?}"
    );
    assert_eq!(disk.commits(), commits + 1);
    assert_eq!(disk.reasons_since(commits), vec![CheckpointReason::Handoff]);
    assert!(matches!(
        disk.last().log().events().last(),
        Some(SessionEvent::ContextHandoff { payload })
            if payload.new_session_id == "next" && payload.confirmed
    ));
}

#[test]
fn the_back_link_is_refused_as_stale_when_the_source_moved_after_the_draft() {
    let (model, runner, mut source, disk, context) =
        managed_with_history(vec![text("the narrative")]);
    let draft = block(runner.draft_handoff(&mut source, host(&disk, &context).runtime)).unwrap();
    model.push(text("a later answer"));
    block(runner.run_turn_with_runtime(&mut source, "later", &mut Silent, host(&disk, &context)));
    let commits = disk.commits();
    let result = block(runner.record_handoff(
        &mut source,
        &draft,
        &SessionId::new("next"),
        host(&disk, &context).runtime,
    ));
    assert_eq!(
        result.err().and_then(|error| error.code()),
        Some(ErrorCode::StaleDraft)
    );
    assert_eq!(disk.commits(), commits, "nothing was recorded");
}

#[test]
fn a_seed_needs_an_empty_session_drafted_from_the_same_source() {
    let (_model, runner, mut source, disk, context) =
        managed_with_history(vec![text("the narrative")]);
    let draft = block(runner.draft_handoff(&mut source, host(&disk, &context).runtime)).unwrap();
    let mut used = Session::new(SessionId::new("used"), 0, "/w");
    used.append(SessionEvent::TurnStart { turn: 0 });
    assert!(runner.seed_session(used, &source, &draft).is_err());
    let other = Session::new(SessionId::new("other"), 0, "/w");
    let mut elsewhere = draft;
    elsewhere.source_session_id = "someone-else".to_owned();
    assert!(runner.seed_session(other, &source, &elsewhere).is_err());
}

#[test]
fn a_handoff_base_names_the_source_session() {
    let (_model, runner, mut source, disk, context) =
        managed_with_history(vec![text("the narrative")]);
    let draft = block(runner.draft_handoff(&mut source, host(&disk, &context).runtime)).unwrap();
    assert_eq!(draft.base.session_id, "s");
    assert_eq!(draft.source_session_id, "s");
}

/// An answer that stopped at the output ceiling.
fn truncated(answer: &str) -> Vec<LlmEvent> {
    vec![
        LlmEvent::TextDelta(answer.to_owned()),
        LlmEvent::Finished {
            reason: FinishReason::Length,
        },
    ]
}

#[test]
fn a_summary_that_stopped_at_the_output_ceiling_is_refused() {
    let (model, runner, session) = legacy_session();
    model.push(truncated("the user asked and"));
    assert!(refused_with(
        block(runner.draft_compaction(&session)),
        ErrorCode::InvalidCompact
    ));
    let (model, runner, session) = legacy_session();
    model.push(text("the user asked two questions"));
    assert!(block(runner.draft_compaction(&session)).is_ok());
}

#[test]
fn a_handoff_that_stopped_at_the_output_ceiling_is_refused_and_recorded_as_refused() {
    let (_model, runner, mut session, disk, context) =
        managed_with_history(vec![truncated("partial")]);
    let before = session.event_count();
    let result = block(runner.draft_handoff(&mut session, host(&disk, &context).runtime));
    assert!(handoff_refused(result, ErrorCode::HandoffUnavailable));
    let records = attempt_records(&session.log().events()[before..]);
    assert_eq!(
        records.last().unwrap().outcome,
        Some(AttemptOutcome::Refused)
    );
}

#[test]
fn a_compaction_whose_fit_drops_every_real_turn_is_refused() {
    let wide_answer = "answer ".repeat(1_500);
    let model = <Model as ModelExt>::new(vec![text(&wide_answer)]);
    let wide = runner(&model, 64_000);
    let mut session = session();
    block(wide.run_turn(&mut session, "question 0", &mut Silent, None)).unwrap();
    let narrow = runner(&model, 1_000);
    model.push(text("summary"));
    assert!(refused_with(
        block(narrow.draft_compaction(&session)),
        ErrorCode::CompactUnavailable
    ));
}

#[test]
fn a_compaction_whose_fit_drops_an_old_turn_says_so_and_still_asks_the_model() {
    let wide_answer = "answer ".repeat(1_500);
    let model = <Model as ModelExt>::new(vec![text(&wide_answer), text("short answer")]);
    let wide = runner(&model, 64_000);
    let mut session = session();
    block(wide.run_turn(&mut session, "question 0", &mut Silent, None)).unwrap();
    block(wide.run_turn(&mut session, "question 1", &mut Silent, None)).unwrap();
    let narrow = runner(&model, 1_000);
    model.push(text("summary"));
    let draft = block(narrow.draft_compaction(&session)).unwrap_or_else(|error| panic!("{error}"));
    assert!(draft.dropped_turns >= 1, "the oldest turn was left out");
    let request = model.sent().pop().unwrap();
    assert_eq!(
        request.messages.last(),
        Some(&Message::user(
            nanus_bundle::agent_loop::COMPACTION_INSTRUCTION
        )),
        "the fitted request was sent"
    );
    let asked = |question: &str| {
        request
            .messages
            .iter()
            .any(|message| matches!(message, Message::User { text, .. } if text == question))
    };
    assert!(
        !asked("question 0"),
        "the dropped turn is not in the request"
    );
    assert!(asked("question 1"), "the newest real turn is kept");
}

#[test]
fn a_compaction_whose_fit_keeps_a_real_turn_beside_the_request_drafts() {
    let model = <Model as ModelExt>::new(vec![text("short answer")]);
    let wide = runner(&model, 64_000);
    let mut session = session();
    block(wide.run_turn(&mut session, "question 0", &mut Silent, None)).unwrap();
    let narrow = runner(&model, 1_000);
    model.push(text("summary"));
    let draft = block(narrow.draft_compaction(&session)).unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(draft.dropped_turns, 0);
}

#[test]
fn a_handoff_closes_a_turn_a_crash_left_open_and_then_drafts() {
    let (_model, runner, mut session, disk, context) =
        managed_with_history(vec![text("the narrative")]);
    // A crash between a turn's opening and its end: the log holds an open turn nobody is running.
    session.append(SessionEvent::TurnStart { turn: 1 });
    session.append(SessionEvent::UserMessage {
        text: "interrupted by a crash".into(),
        content_blocks: None,
    });
    assert!(session.turn_open());
    let draft = block(runner.draft_handoff(&mut session, host(&disk, &context).runtime))
        .unwrap_or_else(|error| panic!("{error}"));
    assert!(!session.turn_open(), "recovery closed the turn");
    let recovery = session.log().events().iter().find_map(|event| match event {
        SessionEvent::ContextRecovery { payload } => Some(payload.reason),
        _ => None,
    });
    assert_eq!(recovery, Some(RecoveryReason::SettledOpenTurn));
    assert_eq!(
        draft.base.event_count,
        u64::try_from(session.event_count()).unwrap()
    );
}

#[test]
fn a_legacy_session_with_an_open_turn_is_refused_before_anything_is_recovered() {
    let (_model, runner, mut session) = legacy_session();
    session.append(SessionEvent::TurnStart { turn: 2 });
    session.append(SessionEvent::UserMessage {
        text: "interrupted by a crash".into(),
        content_blocks: None,
    });
    let before = session.clone();
    let disk = Disk::new();
    let context = context();
    let result = block(runner.draft_handoff(&mut session, host(&disk, &context).runtime));
    assert!(handoff_refused(result, ErrorCode::HandoffUnavailable));
    assert_eq!(
        session, before,
        "nothing was recovered for a session that cannot hand off"
    );
    assert_eq!(disk.commits(), 0);
}

/// A managed session whose workspace path is long and whose goal is at its bound, so its handoff
/// header is large: with a narrative at its own bound, the document passes its bound.
fn managed_with_a_large_header(
    narrative: &str,
) -> (
    Model,
    nanus_bundle::AgentRunner,
    Session,
    Disk,
    nanus_bundle::SessionContext,
) {
    let model = <Model as ModelExt>::new(vec![text("turn answer"), text(narrative)]);
    let runner = runner(&model, 64_000);
    let mut session = Session::new(SessionId::new("s"), 0, format!("/w/{}", "d".repeat(2_000)));
    let disk = Disk::new();
    let context = context();
    block(runner.set_context_policy(
        &mut session,
        ContextPolicy::managed(),
        ModeActor::Human,
        host(&disk, &context).runtime,
    ))
    .unwrap();
    let outcome = block(runner.run_turn_with_runtime(
        &mut session,
        "first",
        &mut Silent,
        host(&disk, &context),
    ));
    assert!(outcome.outcome.is_ok(), "{:?}", outcome.outcome);
    let objective = "\u{1d11e}".repeat(nanus_domain::GOAL_OBJECTIVE_MAX_CHARS);
    session.append(SessionEvent::GoalChange {
        goal: Some(nanus_domain::Goal::new(objective, 0).unwrap()),
    });
    (model, runner, session, disk, context)
}

#[test]
fn a_handoff_whose_document_would_pass_its_bound_is_recorded_refused_not_completed() {
    let at_bound = "x".repeat(nanus_domain::context::managed::limits::HANDOFF_NARRATIVE_BYTES_MAX);
    let (_model, runner, mut session, disk, context) = managed_with_a_large_header(&at_bound);
    let before = session.event_count();
    let result = block(runner.draft_handoff(&mut session, host(&disk, &context).runtime));
    assert!(handoff_refused(result, ErrorCode::HandoffUnavailable));
    let records = attempt_records(&session.log().events()[before..]);
    assert_eq!(
        records.last().map(|record| record.outcome),
        Some(Some(AttemptOutcome::Refused)),
        "the attempt says the narrative was refused"
    );

    let (_model, runner, mut session, disk, context) = managed_with_a_large_header("short");
    let before = session.event_count();
    let draft = block(runner.draft_handoff(&mut session, host(&disk, &context).runtime))
        .unwrap_or_else(|error| panic!("{error}"));
    let records = attempt_records(&session.log().events()[before..]);
    assert_eq!(
        records.last().unwrap().outcome,
        Some(AttemptOutcome::Completed)
    );
    assert!(draft.text.len() <= nanus_domain::context::managed::limits::HANDOFF_DOCUMENT_BYTES_MAX);
}

#[test]
fn a_session_that_hid_history_still_runs_turns_and_drafts_after_a_handoff_draft() {
    let model = <Model as ModelExt>::new(Vec::new());
    let runner = runner(&model, 64_000);
    let (mut session, disk, context) = managed(&runner);
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
    assert!(
        nanus_domain::context::managed::ManagedState::fold(session.log())
            .unwrap()
            .accepted
            .is_some(),
        "history is hidden under an accepted revision"
    );
    let version = session.body_version();
    model.push(text("the narrative"));
    block(runner.draft_handoff(&mut session, host(&disk, &context).runtime))
        .unwrap_or_else(|error| panic!("{error}"));
    assert!(
        session.body_version() > version,
        "the draft moved the body to version five"
    );

    model.push(text("still working"));
    let next = block(runner.run_turn_with_runtime(
        &mut session,
        "carry on",
        &mut Silent,
        host(&disk, &context),
    ));
    assert!(
        next.outcome.is_ok(),
        "the revision survives the version move: {:?}",
        next.outcome
    );
    model.push(text("the second narrative"));
    block(runner.draft_handoff(&mut session, host(&disk, &context).runtime))
        .unwrap_or_else(|error| panic!("a second draft: {error}"));
}

#[test]
fn a_seeded_session_switched_to_legacy_still_leads_with_its_seed() {
    let (mut next, new_disk, context) = seeded_from_a_draft();
    let model = <Model as ModelExt>::new(vec![text("a legacy answer")]);
    let runner = runner_for(&model);
    block(runner.set_context_policy(
        &mut next,
        ContextPolicy::default(),
        ModeActor::Human,
        host(&new_disk, &context).runtime,
    ))
    .unwrap_or_else(|error| panic!("{error}"));
    let run = block(runner.run_turn_with_runtime(
        &mut next,
        "go on",
        &mut Silent,
        host(&new_disk, &context),
    ));
    assert!(run.outcome.is_ok(), "{:?}", run.outcome);
    let request = model.sent().pop().unwrap();
    let first_user = request
        .messages
        .iter()
        .position(|message| matches!(message, Message::User { .. }))
        .unwrap();
    let (at, seed) = seed_in(&request).expect("the legacy request carries the seed");
    assert!(
        at > 0 && at < first_user,
        "a leading system message, after the system prompt"
    );
    assert!(seed.contains("ship the parser"), "{seed}");
    assert!(!request.messages.iter().any(|message| matches!(
        message,
        Message::User { text, .. } if text.contains("ship the parser")
    )));

    // A legacy session that was never seeded sends what it always did.
    let model = <Model as ModelExt>::new(vec![text("hello")]);
    let runner = runner_for(&model);
    let mut plain = session();
    block(runner.run_turn(&mut plain, "hi", &mut Silent, None)).unwrap();
    let request = model.sent().pop().unwrap();
    assert!(seed_in(&request).is_none());
    assert_eq!(
        request.messages.len(),
        2,
        "the system prompt and the prompt"
    );
}
