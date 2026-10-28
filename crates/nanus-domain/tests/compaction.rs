//! A legacy session's compaction marker: where it may sit, what it carries, and what the prompt
//! folds after it. Each claim is tested in both directions.
#![allow(clippy::unwrap_used, clippy::panic)]

use nanus_domain::context::managed::{Digest, ErrorCode};
use nanus_domain::context::{COMPACTION_LABEL, COMPACTION_SUMMARY_BYTES_MAX, CompactionRecord};
use nanus_domain::{
    Message, Session, SessionError, SessionEvent, SessionId, ToolCall, ToolCallId, ToolName,
    TurnEndReason, Usage,
};
use serde_json::{Value, json};

fn usage(prompt: u32) -> Usage {
    Usage {
        prompt_tokens: prompt,
        completion_tokens: 5,
        reasoning_tokens: 0,
        cache_hit_tokens: 0,
        cache_miss_tokens: prompt,
    }
}

/// One turn that ends cleanly: a human prompt, a model answer, and the turn's ending.
fn answered_turn(session: &mut Session, turn: u32, prompt: &str, answer: &str) {
    session.append(SessionEvent::TurnStart { turn });
    session.append(SessionEvent::UserMessage {
        text: prompt.into(),
        content_blocks: None,
    });
    session.append(SessionEvent::AssistantMessage {
        replay: None,
        text: Some(answer.into()),
        reasoning: None,
        tool_calls: Vec::new(),
        usage: Some(usage(10)),
        interrupted: false,
        model: Some("deepseek-flash".into()),
        effort: None,
    });
    session.append(SessionEvent::TurnEnd {
        turn,
        reason: TurnEndReason::Completed,
    });
}

fn two_turn_session() -> Session {
    let mut session = Session::new(SessionId::new("legacy"), 0, "/w");
    answered_turn(&mut session, 0, "first question", "first answer");
    answered_turn(&mut session, 1, "second question", "second answer");
    session
}

/// Cuts the session at its current end, with `summary` as the model's text.
fn compact(session: &mut Session, summary: &str) {
    let event = session
        .compaction_event(
            summary.to_owned(),
            "deepseek-flash".to_owned(),
            Some(usage(3)),
        )
        .unwrap_or_else(|code| panic!("{code}"));
    session.append(event);
}

/// A copy of `session` with a marker at its end, edited after the honest one is built.
fn tampered(session: &Session, change: impl FnOnce(&mut CompactionRecord)) -> Session {
    let SessionEvent::ContextCompact { mut payload } = session
        .compaction_event("summary".into(), "deepseek-flash".into(), None)
        .unwrap_or_else(|code| panic!("{code}"))
    else {
        panic!("compaction_event builds a marker");
    };
    change(&mut payload);
    let mut forged = session.clone();
    forged.append(SessionEvent::ContextCompact { payload });
    forged
}

/// The raw session with the first line's version replaced.
fn set_version(raw: &str, version: u32) -> String {
    let mut lines: Vec<Value> = raw
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    lines[0]["version"] = json!(version);
    join(&lines)
}

/// The raw session with the marker's payload edited, as a hand-edited file would be.
fn edit_marker(raw: &str, edit: impl FnOnce(&mut Value)) -> String {
    let mut lines: Vec<Value> = raw
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let marker = lines
        .iter_mut()
        .find(|line| line["event"]["type"] == "context/compact")
        .unwrap_or_else(|| panic!("the raw session holds a marker"));
    edit(&mut marker["event"]["payload"]);
    join(&lines)
}

fn join(lines: &[Value]) -> String {
    lines
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

fn header_version(raw: &str) -> u64 {
    let header: Value = serde_json::from_str(raw.lines().next().unwrap()).unwrap();
    header["version"].as_u64().unwrap()
}

fn is_marker_refusal(result: &Result<String, SessionError>, code: ErrorCode) -> bool {
    matches!(result, Err(SessionError::InvalidManagedRecord { code: found, .. }) if *found == code)
}

fn the_marker_is_refused(session: &Session) -> bool {
    is_marker_refusal(&session.try_to_jsonl(), ErrorCode::InvalidCompact)
}

#[test]
fn a_compacted_session_is_written_at_version_five_and_reads_back_identically() {
    let mut session = two_turn_session();
    compact(&mut session, "The user asked two questions.");
    let raw = session
        .try_to_jsonl()
        .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(header_version(&raw), 5);
    assert_eq!(Session::from_jsonl(&raw).unwrap(), session);
}

#[test]
fn a_session_that_never_compacts_stays_at_version_two_with_no_marker_bytes() {
    let session = two_turn_session();
    let raw = session.try_to_jsonl().unwrap();
    assert_eq!(header_version(&raw), 2);
    assert!(!raw.contains("context/compact"));
    assert_eq!(session.to_jsonl(), raw);
}

#[test]
fn a_marker_in_a_body_below_version_five_is_refused_on_read() {
    let mut session = two_turn_session();
    compact(&mut session, "summary");
    let raw = session.try_to_jsonl().unwrap();
    for version in [2, 3, 4] {
        let downgraded = set_version(&raw, version);
        assert!(
            matches!(
                Session::from_jsonl(&downgraded),
                Err(SessionError::MalformedEvent { ref detail, .. })
                    if detail.contains("cannot contain a maintenance record")
            ),
            "version {version} must not carry a marker"
        );
    }
    assert!(Session::from_jsonl(&set_version(&raw, 5)).is_ok());
}

#[test]
fn a_marker_whose_boundary_is_not_its_sequence_is_refused() {
    let honest = two_turn_session();
    let forged = tampered(&honest, |record| record.boundary_event_count = 99);
    assert!(the_marker_is_refused(&forged));
    let mut compacted = two_turn_session();
    compact(&mut compacted, "summary");
    let raw = compacted.try_to_jsonl().unwrap();
    let edited = edit_marker(&raw, |payload| payload["boundary_event_count"] = json!(3));
    assert!(matches!(
        Session::from_jsonl(&edited),
        Err(SessionError::InvalidManagedRecord {
            code: ErrorCode::InvalidCompact,
            ..
        })
    ));
}

#[test]
fn a_marker_whose_boundary_is_its_sequence_is_accepted() {
    let mut session = two_turn_session();
    compact(&mut session, "summary");
    assert_eq!(session.compaction_boundary(), Some(8));
    assert!(session.try_to_jsonl().is_ok());
}

#[test]
fn a_marker_with_a_wrong_prefix_digest_is_refused_on_write_and_on_read() {
    let forged = tampered(&two_turn_session(), |record| {
        record.prefix_blake3 = Digest::of(b"some other prefix");
    });
    assert!(the_marker_is_refused(&forged));
    let mut compacted = two_turn_session();
    compact(&mut compacted, "summary");
    let raw = compacted.try_to_jsonl().unwrap();
    let other = Digest::of(b"some other prefix");
    let edited = edit_marker(&raw, |payload| {
        payload["prefix_blake3"] = json!(other.as_str());
    });
    assert!(matches!(
        Session::from_jsonl(&edited),
        Err(SessionError::InvalidManagedRecord {
            code: ErrorCode::InvalidCompact,
            ..
        })
    ));
}

#[test]
fn a_marker_whose_supersedes_does_not_name_the_latest_marker_is_refused() {
    let mut session = two_turn_session();
    compact(&mut session, "first summary");
    answered_turn(&mut session, 2, "third question", "third answer");
    let forged = tampered(&session, |record| record.supersedes = None);
    assert!(the_marker_is_refused(&forged));
    let wrong = tampered(&session, |record| record.supersedes = Some(3));
    assert!(the_marker_is_refused(&wrong));
}

#[test]
fn a_second_marker_supersedes_the_first_and_the_newest_summary_leads() {
    let mut session = two_turn_session();
    compact(&mut session, "first summary");
    answered_turn(&mut session, 2, "third question", "third answer");
    compact(&mut session, "second summary");
    assert_eq!(session.compaction_boundary(), Some(13));
    let raw = session
        .try_to_jsonl()
        .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(Session::from_jsonl(&raw).unwrap(), session);
    let messages = session.derive_prompt_messages();
    assert_eq!(
        messages[0],
        Message::system(format!("{COMPACTION_LABEL}\nsecond summary"))
    );
    assert!(!messages.iter().any(|message| matches!(
        message,
        Message::User { text, .. } if text == "first question"
    )));
}

#[test]
fn a_marker_cut_inside_a_turn_is_refused() {
    let mut session = Session::new(SessionId::new("legacy"), 0, "/w");
    answered_turn(&mut session, 0, "first question", "first answer");
    session.append(SessionEvent::TurnStart { turn: 1 });
    session.append(SessionEvent::UserMessage {
        text: "second question".into(),
        content_blocks: None,
    });
    assert_eq!(
        session
            .compaction_event("cut mid-turn".into(), "m".into(), None)
            .err(),
        Some(ErrorCode::CompactUnavailable),
        "a cut is never made inside an open turn"
    );
    assert!(session.turn_open());
}

/// A turn that asked for a tool and ended without its result, as an interrupted turn leaves it.
fn turn_with_an_unanswered_call(session: &mut Session, turn: u32) {
    session.append(SessionEvent::TurnStart { turn });
    session.append(SessionEvent::UserMessage {
        text: "read it".into(),
        content_blocks: None,
    });
    session.append(SessionEvent::AssistantMessage {
        replay: None,
        text: None,
        reasoning: None,
        tool_calls: vec![ToolCall::new(
            ToolCallId::new("call-1"),
            ToolName::new("read").unwrap(),
            json!({}),
        )],
        usage: None,
        interrupted: false,
        model: None,
        effort: None,
    });
    session.append(SessionEvent::TurnEnd {
        turn,
        reason: TurnEndReason::Interrupted,
    });
}

#[test]
fn a_call_a_closed_turn_left_unanswered_does_not_stop_a_cut() {
    let mut session = Session::new(SessionId::new("legacy"), 0, "/w");
    turn_with_an_unanswered_call(&mut session, 0);
    answered_turn(&mut session, 1, "second question", "second answer");
    assert!(
        session.can_compact(),
        "the call's turn is over, so no result can follow it"
    );
    compact(
        &mut session,
        "an interrupted read, then a question answered",
    );
    let raw = session
        .try_to_jsonl()
        .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(Session::from_jsonl(&raw).unwrap(), session);
}

#[test]
fn a_result_after_the_cut_for_a_call_before_it_is_refused_on_write_and_on_read() {
    let mut session = Session::new(SessionId::new("legacy"), 0, "/w");
    turn_with_an_unanswered_call(&mut session, 0);
    answered_turn(&mut session, 1, "second question", "second answer");
    compact(&mut session, "summary");
    let mut severed = session.clone();
    severed.append(SessionEvent::ToolResult {
        call_id: ToolCallId::new("call-1"),
        content: "late".into(),
        content_blocks: None,
        is_error: false,
    });
    assert!(the_marker_is_refused(&severed));
    assert!(matches!(
        Session::from_jsonl(&severed.to_jsonl()),
        Err(SessionError::InvalidManagedRecord {
            code: ErrorCode::InvalidCompact,
            ..
        })
    ));
}

#[test]
fn a_call_after_the_cut_that_reuses_a_severed_id_is_answered_like_any_other() {
    let mut session = Session::new(SessionId::new("legacy"), 0, "/w");
    turn_with_an_unanswered_call(&mut session, 0);
    answered_turn(&mut session, 1, "second question", "second answer");
    compact(&mut session, "summary");
    // A provider that numbers its calls per request uses `call-1` again after the cut.
    turn_with_an_unanswered_call(&mut session, 2);
    let mut answered = session.clone();
    answered.append(SessionEvent::ToolResult {
        call_id: ToolCallId::new("call-1"),
        content: "the new call's result".into(),
        content_blocks: None,
        is_error: false,
    });
    let raw = answered
        .try_to_jsonl()
        .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(Session::from_jsonl(&raw).unwrap(), answered);
}

#[test]
fn a_cut_after_a_switch_back_to_legacy_is_admitted_by_the_check_and_the_write_alike() {
    let mut session = Session::new(SessionId::new("switched"), 0, "/w");
    session.upgrade_to_managed_body();
    session.append(mode_record(
        nanus_domain::context::managed::ContextMode::Managed,
    ));
    answered_turn(&mut session, 0, "question", "answer");
    session.append(mode_record(
        nanus_domain::context::managed::ContextMode::Legacy,
    ));
    assert!(
        session.can_compact(),
        "the switch is the last event, after a turn"
    );
    compact(&mut session, "summary right after switching back");
    let raw = session
        .try_to_jsonl()
        .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(Session::from_jsonl(&raw).unwrap(), session);
}

#[test]
fn a_goal_change_between_turns_does_not_stop_a_cut() {
    let mut session = two_turn_session();
    session.append(SessionEvent::GoalChange { goal: None });
    assert!(session.can_compact());
    compact(&mut session, "summary after a goal change");
    assert!(session.try_to_jsonl().is_ok());
}

#[test]
fn what_the_check_refuses_the_write_refuses_too() {
    let mut open = two_turn_session();
    open.append(SessionEvent::TurnStart { turn: 2 });
    // Nothing completed since the last cut: a second marker straight after the first.
    let mut nothing_new = two_turn_session();
    compact(&mut nothing_new, "first summary");
    nothing_new.append(SessionEvent::GoalChange { goal: None });
    for refused in [open, nothing_new] {
        assert!(!refused.can_compact());
        assert_eq!(
            refused
                .compaction_event("summary".into(), "m".into(), None)
                .err(),
            Some(ErrorCode::CompactUnavailable)
        );
        let forged = forged_marker(&refused);
        assert!(the_marker_is_refused(&forged));
        assert!(matches!(
            Session::from_jsonl(&forged.to_jsonl()),
            Err(SessionError::InvalidManagedRecord {
                code: ErrorCode::InvalidCompact,
                ..
            })
        ));
    }
    // And what the check admits, the write and the read admit.
    let admitted = two_turn_session();
    assert!(admitted.can_compact());
    let forged = forged_marker(&admitted);
    let raw = forged.try_to_jsonl().unwrap();
    assert!(Session::from_jsonl(&raw).is_ok());
}

/// A marker at the end of `session`, built by hand so that nothing refuses it before the write.
fn forged_marker(session: &Session) -> Session {
    let boundary = u64::try_from(session.event_count()).unwrap();
    let summary = "summary".to_owned();
    let record = CompactionRecord {
        boundary_event_count: boundary,
        prefix_blake3: session.compaction_prefix_digest(boundary).unwrap(),
        summary_blake3: Digest::of(summary.as_bytes()),
        summary,
        supersedes: session
            .log()
            .events()
            .iter()
            .rposition(|event| matches!(event, SessionEvent::ContextCompact { .. }))
            .map(|index| u64::try_from(index).unwrap()),
        method: nanus_domain::context::CompactionMethod::ModelGenerated,
        model: "m".into(),
        usage: None,
    };
    let mut forged = session.clone();
    forged.append(SessionEvent::ContextCompact {
        payload: Box::new(record),
    });
    forged
}

#[test]
fn a_marker_inside_a_turn_is_refused_on_read() {
    let mut session = two_turn_session();
    session.append(SessionEvent::TurnStart { turn: 2 });
    session.append(SessionEvent::UserMessage {
        text: "third question".into(),
        content_blocks: None,
    });
    let forged = forged_marker(&session);
    assert!(matches!(
        Session::from_jsonl(&forged.to_jsonl()),
        Err(SessionError::InvalidManagedRecord {
            code: ErrorCode::InvalidCompact,
            ..
        })
    ));
    assert!(Session::from_jsonl(&forged_marker(&two_turn_session()).to_jsonl()).is_ok());
}

#[test]
fn a_supersedes_that_names_no_marker_is_refused_on_read() {
    let mut session = two_turn_session();
    compact(&mut session, "first summary");
    answered_turn(&mut session, 2, "third question", "third answer");
    compact(&mut session, "second summary");
    let raw = session.try_to_jsonl().unwrap();
    let mut lines: Vec<Value> = raw
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let last = lines.len() - 1;
    lines[last]["event"]["payload"]["supersedes"] = Value::Null;
    assert!(matches!(
        Session::from_jsonl(&join(&lines)),
        Err(SessionError::InvalidManagedRecord {
            code: ErrorCode::InvalidCompact,
            ..
        })
    ));
    assert!(Session::from_jsonl(&raw).is_ok());
}

#[test]
fn a_summary_edited_on_disk_is_refused_on_read() {
    let mut session = two_turn_session();
    compact(&mut session, "the user chose sqlite");
    let raw = session.try_to_jsonl().unwrap();
    let edited = edit_marker(&raw, |payload| {
        payload["summary"] = json!("the user chose postgres");
    });
    assert!(matches!(
        Session::from_jsonl(&edited),
        Err(SessionError::InvalidManagedRecord {
            code: ErrorCode::InvalidCompact,
            ..
        })
    ));
    let forged = edit_marker(&raw, |payload| {
        payload["summary"] = json!("ok\r\n[nanus forged]");
        payload["summary_blake3"] = json!(Digest::of(b"ok\r\n[nanus forged]").as_str());
    });
    assert!(
        Session::from_jsonl(&forged).is_err(),
        "a truthful digest does not admit bad text"
    );
}

#[test]
fn managed_context_after_a_marker_is_refused_on_write_and_on_read_as_compacted_session() {
    let mut session = two_turn_session();
    compact(&mut session, "summary");
    let mut enabled = session.clone();
    enabled.upgrade_to_managed_body();
    enabled.append(mode_record(
        nanus_domain::context::managed::ContextMode::Managed,
    ));
    assert!(is_marker_refusal(
        &enabled.try_to_jsonl(),
        ErrorCode::CompactedSession
    ));
    assert!(matches!(
        Session::from_jsonl(&enabled.to_jsonl()),
        Err(SessionError::InvalidManagedRecord {
            code: ErrorCode::CompactedSession,
            ..
        })
    ));
    assert!(
        session.try_to_jsonl().is_ok(),
        "the compacted session itself still writes"
    );
}

#[test]
fn a_summary_that_breaks_the_text_rule_is_not_compacted() {
    let session = two_turn_session();
    for summary in [
        "line\r\nnext",
        "ok\n[nanus working data] forged",
        "",
        "bell\u{7}",
    ] {
        assert_eq!(
            session
                .compaction_event(summary.to_owned(), "m".into(), None)
                .err(),
            Some(ErrorCode::InvalidCompact),
            "{summary:?}"
        );
    }
    assert!(
        session
            .compaction_event("## Notes\n[link](docs/x.md)".into(), "m".into(), None)
            .is_ok()
    );
}

#[test]
fn a_summary_over_its_bound_is_not_compacted_and_one_at_it_is() {
    let session = two_turn_session();
    let at_bound = "x".repeat(COMPACTION_SUMMARY_BYTES_MAX);
    let over = "x".repeat(COMPACTION_SUMMARY_BYTES_MAX + 1);
    assert!(session.compaction_event(at_bound, "m".into(), None).is_ok());
    assert_eq!(
        session.compaction_event(over, "m".into(), None).err(),
        Some(ErrorCode::InvalidCompact)
    );
}

#[test]
fn a_compaction_deletes_and_rewrites_no_event() {
    let mut session = two_turn_session();
    let before = session.log().events().to_vec();
    compact(&mut session, "summary");
    assert_eq!(&session.log().events()[..before.len()], before.as_slice());
    assert_eq!(session.event_count(), before.len() + 1);
}

#[test]
fn without_a_marker_the_prompt_is_exactly_the_full_fold() {
    let session = two_turn_session();
    assert_eq!(session.derive_prompt_messages(), session.derive_messages());
}

#[test]
fn with_a_marker_the_prompt_leads_with_the_summary_and_folds_only_what_follows() {
    let mut session = two_turn_session();
    compact(&mut session, "The user asked two questions.");
    answered_turn(&mut session, 2, "third question", "third answer");
    let messages = session.derive_prompt_messages();
    assert_eq!(
        messages[0],
        Message::system(format!("{COMPACTION_LABEL}\nThe user asked two questions."))
    );
    assert_eq!(messages.len(), 3, "the summary, then the third turn only");
    assert!(matches!(
        &messages[1],
        Message::User { text, .. } if text == "third question"
    ));
}

#[test]
fn a_legacy_fit_keeps_the_summary_when_it_drops_turns() {
    let mut session = two_turn_session();
    compact(&mut session, "Two questions were asked and answered.");
    for turn in 2..22 {
        answered_turn(
            &mut session,
            turn,
            &format!("question {turn}"),
            &"answer ".repeat(200),
        );
    }
    let fitted = nanus_domain::fit(session.derive_prompt_messages(), 600)
        .unwrap_or_else(|error| panic!("{error}"));
    assert!(fitted.elision.is_some(), "the budget must force a drop");
    assert!(
        matches!(&fitted.messages[0], Message::System { text }
            if text.starts_with(COMPACTION_LABEL)),
        "the summary is the leading prompt and is never dropped"
    );
    // The notice is the request's tail, so the oldest kept turn follows the summary directly.
    assert!(matches!(&fitted.messages[1], Message::User { .. }));
    assert!(
        fitted
            .elision
            .is_some_and(|elision| elision.notice().contains("not shown"))
    );
}

#[test]
fn usage_totals_include_the_usage_of_a_compaction() {
    let mut session = two_turn_session();
    assert_eq!(session.usage_totals().prompt_tokens, 20);
    compact(&mut session, "summary");
    assert_eq!(session.usage_totals().prompt_tokens, 23);
}

#[test]
fn usage_by_model_attributes_a_compaction_to_the_model_that_wrote_it() {
    let mut session = two_turn_session();
    let event = session
        .compaction_event("summary".into(), "summariser".into(), Some(usage(3)))
        .unwrap();
    session.append(event);
    let by_model = session.usage_by_model();
    assert_eq!(by_model.len(), 2);
    assert_eq!(by_model[0].0.as_deref(), Some("deepseek-flash"));
    assert_eq!(by_model[0].1.prompt_tokens, 20);
    assert_eq!(by_model[1].0.as_deref(), Some("summariser"));
    assert_eq!(by_model[1].1.prompt_tokens, 3);
    let sum: u32 = by_model.iter().map(|(_, usage)| usage.prompt_tokens).sum();
    assert_eq!(sum, session.usage_totals().prompt_tokens);
}

#[test]
fn a_compaction_without_usage_adds_nothing_to_the_totals() {
    let mut session = two_turn_session();
    let event = session
        .compaction_event("summary".into(), "m".into(), None)
        .unwrap();
    session.append(event);
    assert_eq!(session.usage_totals().prompt_tokens, 20);
}

fn mode_record(mode: nanus_domain::context::managed::ContextMode) -> SessionEvent {
    use nanus_domain::context::managed::{ContextModeRecord, ContextPolicy, ModeActor, ModeReason};
    let policy = if mode == nanus_domain::context::managed::ContextMode::Managed {
        ContextPolicy::managed()
    } else {
        ContextPolicy::default()
    };
    SessionEvent::ContextMode {
        payload: Box::new(ContextModeRecord {
            policy,
            actor: ModeActor::Human,
            reason: ModeReason::ConfigurationChange,
            previous_revision: 0,
        }),
    }
}

#[test]
fn a_session_in_managed_mode_is_not_compacted() {
    let mut session = Session::new(SessionId::new("managed"), 0, "/w");
    session.upgrade_to_managed_body();
    session.append(mode_record(
        nanus_domain::context::managed::ContextMode::Managed,
    ));
    answered_turn(&mut session, 0, "question", "answer");
    assert_eq!(
        session
            .compaction_event("summary".into(), "m".into(), None)
            .err(),
        Some(ErrorCode::CompactUnavailable)
    );
}

#[test]
fn a_managed_session_switched_back_to_legacy_may_be_compacted() {
    let mut session = Session::new(SessionId::new("switched"), 0, "/w");
    session.upgrade_to_managed_body();
    session.append(mode_record(
        nanus_domain::context::managed::ContextMode::Managed,
    ));
    answered_turn(&mut session, 0, "question", "answer");
    session.append(mode_record(
        nanus_domain::context::managed::ContextMode::Legacy,
    ));
    answered_turn(&mut session, 1, "second", "reply");
    compact(&mut session, "summary after switching back");
    assert!(session.try_to_jsonl().is_ok());
}

#[test]
fn a_marker_written_under_managed_mode_is_refused_on_replay() {
    // Built by hand, because `compaction_event` will not build one: the replay check is the second
    // line of defence and must refuse a marker a writer could otherwise have appended.
    let mut session = Session::new(SessionId::new("forged"), 0, "/w");
    session.upgrade_to_managed_body();
    session.append(mode_record(
        nanus_domain::context::managed::ContextMode::Managed,
    ));
    answered_turn(&mut session, 0, "question", "answer");
    let boundary = u64::try_from(session.event_count()).unwrap();
    let summary = "summary".to_owned();
    let record = CompactionRecord {
        boundary_event_count: boundary,
        prefix_blake3: session.compaction_prefix_digest(boundary).unwrap(),
        summary_blake3: Digest::of(summary.as_bytes()),
        summary,
        supersedes: None,
        method: nanus_domain::context::CompactionMethod::ModelGenerated,
        model: "m".into(),
        usage: None,
    };
    session.append(SessionEvent::ContextCompact {
        payload: Box::new(record),
    });
    assert!(the_marker_is_refused(&session));
}

#[test]
fn a_cut_needs_a_completed_turn_after_the_previous_one() {
    let empty = Session::new(SessionId::new("empty"), 0, "/w");
    assert_eq!(
        empty
            .compaction_event("summary".into(), "m".into(), None)
            .err(),
        Some(ErrorCode::CompactUnavailable)
    );
    let mut session = two_turn_session();
    compact(&mut session, "first summary");
    assert_eq!(
        session
            .compaction_event("again".into(), "m".into(), None)
            .err(),
        Some(ErrorCode::CompactUnavailable),
        "nothing completed after the marker"
    );
    answered_turn(&mut session, 2, "third", "reply");
    assert!(
        session
            .compaction_event("again".into(), "m".into(), None)
            .is_ok()
    );
}
