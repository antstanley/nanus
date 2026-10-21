//! Handoff records in a managed session: the seed a new session starts from, the back-link an old
//! session records, and the managed fold that orders them. Each claim is tested in both directions.
#![allow(clippy::unwrap_used, clippy::panic)]

use nanus_domain::context::managed::HANDOFF_SEED_LABEL;
use nanus_domain::context::managed::limits::HANDOFF_NARRATIVE_BYTES_MAX;
use nanus_domain::context::managed::{
    AttemptOutcome, AttemptPhase, ContextFrontier, ContextModeRecord, ContextPolicy, Digest,
    ErrorCode, HandoffRecord, HandoffSeed, MaintenanceAttemptKind, ManagedState, ModeActor,
    ModeReason, RequestAttemptRecord, SelectionIdentity, UsageObservation,
};
use nanus_domain::{Message, Session, SessionError, SessionEvent, SessionId, TurnEndReason, Usage};

fn managed_session(id: &str) -> Session {
    let mut session = Session::new(SessionId::new(id), 0, "/w");
    session.upgrade_to_managed_body();
    session.append(mode());
    session
}

fn mode() -> SessionEvent {
    SessionEvent::ContextMode {
        payload: Box::new(ContextModeRecord {
            policy: ContextPolicy::managed(),
            actor: ModeActor::Host,
            reason: ModeReason::Enable,
            previous_revision: 0,
        }),
    }
}

fn frontier(event_count: u64) -> ContextFrontier {
    ContextFrontier {
        session_id: "old".into(),
        event_count,
        prefix_blake3: Digest::of(b"prefix"),
        projection_revision: 0,
    }
}

/// A handoff document as the harness writes one: the seven header lines, a blank line, `narrative`.
fn document(narrative: &str) -> String {
    format!(
        "source_session: old\nsource_frontier: events=9\ngoal: none\ncontext_mode: managed\n\
         model: m effort: none\nworkspace: /w\ndate: 2026-10-21\n\n{narrative}"
    )
}

fn seed_of(narrative: &str) -> SessionEvent {
    seed_text(&document(narrative))
}

/// A seed whose whole text is `text`, header or not.
fn seed_text(text: &str) -> SessionEvent {
    SessionEvent::ContextSeed {
        payload: Box::new(HandoffSeed {
            text: text.into(),
            document_blake3: Digest::of(text.as_bytes()),
            source_session_id: "old".into(),
            source_frontier: frontier(9),
        }),
    }
}

fn handoff(new_session_id: &str, event_count: u64) -> SessionEvent {
    SessionEvent::ContextHandoff {
        payload: Box::new(HandoffRecord {
            source_frontier: frontier(event_count),
            new_session_id: new_session_id.into(),
            document_blake3: Digest::of(b"document"),
            confirmed: true,
        }),
    }
}

fn user(text: &str) -> SessionEvent {
    SessionEvent::UserMessage {
        text: text.into(),
        content_blocks: None,
    }
}

fn refused_with(result: &Result<String, SessionError>, code: ErrorCode) -> bool {
    matches!(result, Err(SessionError::InvalidManagedRecord { code: found, .. }) if *found == code)
}

#[test]
fn a_seed_as_the_second_event_is_accepted_and_is_never_a_user_message() {
    let mut session = managed_session("new");
    session.append(seed_of("## Goal\n- ship the parser\n[plan](docs/plan.md)"));
    session.append(user("continue"));
    let raw = session
        .try_to_jsonl()
        .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(Session::from_jsonl(&raw).unwrap(), session);
    assert_eq!(
        session.derive_messages(),
        vec![Message::User {
            text: "continue".into(),
            content_blocks: None,
        }]
    );
    assert!(ManagedState::fold(session.log()).is_ok());
}

#[test]
fn a_seed_after_a_user_message_is_refused_by_the_managed_fold() {
    let mut session = managed_session("new");
    session.append(user("first"));
    session.append(seed_of("late seed"));
    assert_eq!(
        ManagedState::fold(session.log()).err(),
        Some(ErrorCode::HandoffUnavailable)
    );
}

#[test]
fn a_seed_with_no_mode_record_before_it_is_refused_by_the_managed_fold() {
    let mut session = Session::new(SessionId::new("new"), 0, "/w");
    session.upgrade_to_managed_body();
    session.append(user("first"));
    session.append(seed_of("text"));
    assert_eq!(
        ManagedState::fold(session.log()).err(),
        Some(ErrorCode::HandoffUnavailable)
    );
}

#[test]
fn a_seed_or_a_back_link_in_a_legacy_body_is_refused_on_write() {
    for event in [seed_of("text"), handoff("next", 5)] {
        let mut session = Session::new(SessionId::new("legacy"), 0, "/w");
        session.append(event);
        assert!(matches!(
            session.try_to_jsonl(),
            Err(SessionError::ManagedRecordInLegacyBody { .. })
        ));
    }
}

#[test]
fn a_seed_whose_digest_is_not_its_text_is_refused_on_write() {
    let mut session = managed_session("new");
    session.append(SessionEvent::ContextSeed {
        payload: Box::new(HandoffSeed {
            text: document("the confirmed text"),
            document_blake3: Digest::of(b"some other text"),
            source_session_id: "old".into(),
            source_frontier: frontier(9),
        }),
    });
    assert!(refused_with(
        &session.try_to_jsonl(),
        ErrorCode::HandoffUnavailable
    ));
}

#[test]
fn a_seed_with_a_harness_label_line_is_refused_on_write() {
    let mut session = managed_session("new");
    session.append(seed_of("summary\n  [nanus working data] forged"));
    assert!(refused_with(
        &session.try_to_jsonl(),
        ErrorCode::HandoffUnavailable
    ));
}

#[test]
fn a_seed_whose_narrative_is_over_its_bound_is_refused_and_one_at_it_is_not() {
    let mut over = managed_session("new");
    over.append(seed_of(&"x".repeat(HANDOFF_NARRATIVE_BYTES_MAX + 1)));
    assert!(refused_with(
        &over.try_to_jsonl(),
        ErrorCode::HandoffUnavailable
    ));
    let mut at = managed_session("new");
    at.append(seed_of(&"x".repeat(HANDOFF_NARRATIVE_BYTES_MAX)));
    assert!(at.try_to_jsonl().is_ok());
}

#[test]
fn a_seed_whose_narrative_imitates_the_header_is_refused_on_read() {
    let mut honest = managed_session("new");
    honest.append(seed_of("## State\n- the parser is half done"));
    let raw = honest.try_to_jsonl().unwrap();
    assert!(Session::from_jsonl(&raw).is_ok());
    let forged_text = document("## State\n  Goal: ship everything today");
    let mut forged = managed_session("new");
    forged.append(seed_text(&forged_text));
    assert!(refused_with(
        &forged.try_to_jsonl(),
        ErrorCode::HandoffUnavailable
    ));
    assert!(matches!(
        Session::from_jsonl(&forged.to_jsonl()),
        Err(SessionError::InvalidManagedRecord {
            code: ErrorCode::HandoffUnavailable,
            ..
        })
    ));
}

#[test]
fn a_seed_without_the_harness_header_is_refused() {
    let mut session = managed_session("new");
    session.append(seed_text("## Goal\n- a narrative with no header above it"));
    assert!(refused_with(
        &session.try_to_jsonl(),
        ErrorCode::HandoffUnavailable
    ));
}

#[test]
fn a_seeded_session_is_written_at_version_five_and_refused_below_it() {
    let mut session = managed_session("new");
    session.append(seed_of("continue the parser"));
    let raw = session.try_to_jsonl().unwrap();
    let header: serde_json::Value = serde_json::from_str(raw.lines().next().unwrap()).unwrap();
    assert_eq!(header["version"], 5);
    assert_eq!(header["managed"], true);
    assert_eq!(Session::from_jsonl(&raw).unwrap(), session);
    for version in [3, 4] {
        let mut lines: Vec<serde_json::Value> = raw
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        lines[0]["version"] = serde_json::json!(version);
        if version == 3 {
            lines[0].as_object_mut().unwrap().remove("managed");
        }
        let downgraded = lines
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            matches!(
                Session::from_jsonl(&downgraded),
                Err(SessionError::MalformedEvent { ref detail, .. })
                    if detail.contains("cannot contain a maintenance record")
            ),
            "a version-{version} body must not carry a seed"
        );
    }
}

#[test]
fn a_seed_or_a_back_link_in_a_legacy_body_is_refused_on_read() {
    for event in [seed_of("continue"), handoff("next", 5)] {
        let mut managed = managed_session("old");
        managed.append(event);
        let raw = managed.to_jsonl();
        let mut lines: Vec<serde_json::Value> = raw
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        lines[0]["version"] = serde_json::json!(2);
        lines[0].as_object_mut().unwrap().remove("managed");
        let legacy = lines
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(matches!(
            Session::from_jsonl(&legacy),
            Err(SessionError::ManagedRecordInLegacyBody { .. })
        ));
        assert!(Session::from_jsonl(&raw).is_ok());
    }
}

#[test]
fn two_handoffs_to_distinct_sessions_with_increasing_frontiers_are_accepted() {
    let mut session = managed_session("old");
    session.append(handoff("next-a", 5));
    session.append(handoff("next-b", 9));
    assert!(session.try_to_jsonl().is_ok());
    assert!(ManagedState::fold(session.log()).is_ok());
}

#[test]
fn a_handoff_to_a_session_already_named_is_refused() {
    let mut session = managed_session("old");
    session.append(handoff("next", 5));
    session.append(handoff("next", 9));
    assert_eq!(
        ManagedState::fold(session.log()).err(),
        Some(ErrorCode::HandoffUnavailable)
    );
}

#[test]
fn a_handoff_whose_frontier_does_not_move_forward_is_refused() {
    let mut equal = managed_session("old");
    equal.append(handoff("next-a", 9));
    equal.append(handoff("next-b", 9));
    assert_eq!(
        ManagedState::fold(equal.log()).err(),
        Some(ErrorCode::HandoffUnavailable)
    );
    let mut backwards = managed_session("old");
    backwards.append(handoff("next-a", 9));
    backwards.append(handoff("next-b", 5));
    assert_eq!(
        ManagedState::fold(backwards.log()).err(),
        Some(ErrorCode::HandoffUnavailable)
    );
}

#[test]
fn an_unconfirmed_handoff_is_refused_on_write() {
    let mut session = managed_session("old");
    session.append(SessionEvent::ContextHandoff {
        payload: Box::new(HandoffRecord {
            source_frontier: frontier(5),
            new_session_id: "next".into(),
            document_blake3: Digest::of(b"document"),
            confirmed: false,
        }),
    });
    assert!(refused_with(
        &session.try_to_jsonl(),
        ErrorCode::HandoffUnavailable
    ));
}

#[test]
fn a_handoff_with_an_empty_new_session_id_is_refused_on_write() {
    let mut session = managed_session("old");
    session.append(handoff("", 5));
    assert!(refused_with(
        &session.try_to_jsonl(),
        ErrorCode::HandoffUnavailable
    ));
}

#[test]
fn a_compacted_session_cannot_enable_managed_context() {
    let mut session = Session::new(SessionId::new("legacy"), 0, "/w");
    session.append(SessionEvent::TurnStart { turn: 0 });
    session.append(user("question"));
    session.append(SessionEvent::TurnEnd {
        turn: 0,
        reason: TurnEndReason::Completed,
    });
    let event = session
        .compaction_event("the question was answered".into(), "m".into(), None)
        .unwrap_or_else(|code| panic!("{code}"));
    session.append(event);
    assert!(session.is_compacted());
    session.append(mode());
    assert_eq!(
        ManagedState::fold(session.log()).err(),
        Some(ErrorCode::CompactedSession)
    );
}

#[test]
fn a_session_that_was_never_compacted_may_enable_managed_context() {
    let session = managed_session("fresh");
    assert!(!session.is_compacted());
    assert!(ManagedState::fold(session.log()).is_ok());
}

/// A handoff draft's finished attempt record, with `usage` as the provider reported it.
fn handoff_attempt(phase: AttemptPhase, kind: Option<MaintenanceAttemptKind>) -> SessionEvent {
    let finished = phase == AttemptPhase::Finished;
    SessionEvent::RequestAttempt {
        payload: Box::new(RequestAttemptRecord {
            attempt_id: "t0s0e1".into(),
            retry_of: None,
            turn: 0,
            step: 0,
            selection: SelectionIdentity {
                provider: "deepseek".into(),
                endpoint_digest: Digest::of(b"endpoint"),
                protocol: "chat".into(),
                model: "drafter".into(),
                effort: None,
                epoch: 1,
            },
            projection_revision: 0,
            request_digest: Digest::of(b"request"),
            phase,
            outcome: finished.then_some(AttemptOutcome::Completed),
            usage: finished.then(|| UsageObservation::from_usage(&Usage::new(40, 7, 0, 0, 40))),
            assistant_seq: None,
            included_management_fragments: Vec::new(),
            kind,
            started_at_ms: 0,
            finished_at_ms: finished.then_some(1),
            timings_ms: None,
        }),
    }
}

#[test]
fn a_handoff_drafts_cost_is_in_the_sessions_totals_and_a_turn_steps_is_not_counted_twice() {
    let mut session = managed_session("old");
    session.append(handoff_attempt(
        AttemptPhase::Started,
        Some(MaintenanceAttemptKind::Handoff),
    ));
    session.append(handoff_attempt(
        AttemptPhase::Finished,
        Some(MaintenanceAttemptKind::Handoff),
    ));
    assert_eq!(session.usage_totals().prompt_tokens, 40);
    assert_eq!(session.usage_totals().completion_tokens, 7);
    assert_eq!(
        session.usage_by_model(),
        vec![(Some("drafter".to_owned()), Usage::new(40, 7, 0, 0, 40))]
    );
    let mut step = managed_session("old");
    step.append(handoff_attempt(AttemptPhase::Finished, None));
    assert_eq!(
        step.usage_totals(),
        Usage::default(),
        "a turn step's usage is its assistant message's, not its attempt's"
    );
}

#[test]
fn an_attempt_with_a_kind_needs_a_version_five_body() {
    let mut session = managed_session("old");
    session.append(handoff_attempt(AttemptPhase::Started, None));
    assert_eq!(
        session.body_version(),
        3,
        "a turn step's attempt keeps version 3"
    );
    session.append(handoff_attempt(
        AttemptPhase::Started,
        Some(MaintenanceAttemptKind::Handoff),
    ));
    assert_eq!(session.body_version(), 5);
    let raw = session.try_to_jsonl().unwrap();
    let mut lines: Vec<serde_json::Value> = raw
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    lines[0]["version"] = serde_json::json!(4);
    let downgraded = lines
        .iter()
        .map(serde_json::Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(matches!(
        Session::from_jsonl(&downgraded),
        Err(SessionError::MalformedEvent { ref detail, .. })
            if detail.contains("cannot contain a maintenance record")
    ));
    assert!(Session::from_jsonl(&raw).is_ok());
}

/// A file whose header and lines are `session`'s, re-encoded line by line so a test can read back
/// what the write would have refused.
fn raw_of(session: &Session) -> String {
    session.to_jsonl()
}

fn legacy_mode() -> SessionEvent {
    SessionEvent::ContextMode {
        payload: Box::new(ContextModeRecord {
            policy: ContextPolicy::default(),
            actor: ModeActor::Human,
            reason: ModeReason::Disable,
            previous_revision: 0,
        }),
    }
}

fn completed_turn(session: &mut Session, turn: u32, question: &str) {
    session.append(SessionEvent::TurnStart { turn });
    session.append(user(question));
    session.append(SessionEvent::AssistantMessage {
        text: Some(format!("answer to {question}")),
        reasoning: None,
        replay: None,
        tool_calls: Vec::new(),
        usage: None,
        interrupted: false,
        model: None,
        effort: None,
    });
    session.append(SessionEvent::TurnEnd {
        turn,
        reason: TurnEndReason::Completed,
    });
}

#[test]
fn a_seed_misplaced_in_the_log_is_refused_on_write_and_on_read() {
    let mut late = managed_session("new");
    late.append(user("first"));
    late.append(seed_of("late seed"));
    assert!(refused_with(
        &late.try_to_jsonl(),
        ErrorCode::HandoffUnavailable
    ));
    assert!(matches!(
        Session::from_jsonl(&raw_of(&late)),
        Err(SessionError::InvalidManagedRecord {
            code: ErrorCode::HandoffUnavailable,
            ..
        })
    ));
    let mut placed = managed_session("new");
    placed.append(seed_of("in its place"));
    let raw = placed.try_to_jsonl().unwrap();
    assert!(Session::from_jsonl(&raw).is_ok());
}

#[test]
fn a_seed_after_a_legacy_mode_record_is_refused_on_write_on_read_and_by_the_fold() {
    let mut session = Session::new(SessionId::new("new"), 0, "/w");
    session.upgrade_to_managed_body();
    session.append(legacy_mode());
    session.append(seed_of("text"));
    assert!(refused_with(
        &session.try_to_jsonl(),
        ErrorCode::HandoffUnavailable
    ));
    assert!(Session::from_jsonl(&raw_of(&session)).is_err());
    assert_eq!(
        ManagedState::fold(session.log()).err(),
        Some(ErrorCode::HandoffUnavailable)
    );
}

#[test]
fn a_back_link_must_name_the_session_that_holds_it() {
    let mut foreign = managed_session("elsewhere");
    foreign.append(handoff("next", 5));
    assert!(refused_with(
        &foreign.try_to_jsonl(),
        ErrorCode::HandoffUnavailable
    ));
    assert!(matches!(
        Session::from_jsonl(&raw_of(&foreign)),
        Err(SessionError::InvalidManagedRecord {
            code: ErrorCode::HandoffUnavailable,
            ..
        })
    ));
    let mut own = managed_session("old");
    own.append(handoff("next", 5));
    let raw = own.try_to_jsonl().unwrap();
    assert!(Session::from_jsonl(&raw).is_ok());
}

#[test]
fn a_repeated_back_link_is_refused_on_write_not_only_by_the_fold() {
    let mut session = managed_session("old");
    session.append(handoff("next", 5));
    session.append(handoff("next", 9));
    assert!(refused_with(
        &session.try_to_jsonl(),
        ErrorCode::HandoffUnavailable
    ));
}

/// A seeded session switched to legacy, with a turn after the switch.
fn seeded_then_legacy() -> Session {
    let mut session = managed_session("new");
    session.append(seed_of("## State\n- the parser is half done"));
    session.append(legacy_mode());
    completed_turn(&mut session, 0, "carry on");
    session
}

#[test]
fn a_seeded_session_switched_to_legacy_still_leads_its_prompt_with_the_seed() {
    let session = seeded_then_legacy();
    let messages = session.derive_prompt_messages();
    let Some(Message::System { text, .. }) = messages.first() else {
        panic!("the seed leads: {messages:?}");
    };
    assert!(text.starts_with(HANDOFF_SEED_LABEL));
    assert!(text.ends_with("- the parser is half done"));
    assert_eq!(&messages[1..], session.derive_messages().as_slice());
    let fitted = nanus_domain::fit(messages.clone(), 10_000).unwrap();
    assert_eq!(
        fitted.messages.first(),
        messages.first(),
        "the fit keeps it"
    );
}

#[test]
fn a_seeded_session_still_managed_leaves_the_seed_to_the_managed_compiler() {
    let mut session = managed_session("new");
    session.append(seed_of("continue the parser"));
    completed_turn(&mut session, 0, "carry on");
    assert_eq!(session.derive_prompt_messages(), session.derive_messages());
}

#[test]
fn after_a_cut_the_summary_stands_in_for_the_seed() {
    let mut session = seeded_then_legacy();
    assert!(session.can_compact());
    let event = session
        .compaction_event("the parser was finished".into(), "m".into(), None)
        .unwrap_or_else(|code| panic!("{code}"));
    session.append(event);
    completed_turn(&mut session, 1, "next");
    let messages = session.derive_prompt_messages();
    let seeds = messages
        .iter()
        .filter(|message| {
            matches!(message, Message::System { text, .. } if text.starts_with(HANDOFF_SEED_LABEL))
        })
        .count();
    assert_eq!(
        seeds, 0,
        "the seed is not sent beside the summary: {messages:?}"
    );
    assert!(matches!(
        messages.first(),
        Some(Message::System { text, .. }) if text.ends_with("the parser was finished")
    ));
    let raw = session.try_to_jsonl().unwrap();
    assert_eq!(Session::from_jsonl(&raw).unwrap(), session);
}
