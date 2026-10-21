//! A reservation that follows the model: the policy's arithmetic, the field's serde rules, and
//! body version 6, which is the lowest version that holds a following policy.

use nanus_domain::context::managed::{
    ContextMode, ContextModeRecord, ContextPolicy, ModeActor, ModeReason, ReserveBudget,
};
use nanus_domain::{Session, SessionEvent, SessionId};

/// The record that enables managed context with `policy`.
fn enable(policy: ContextPolicy) -> SessionEvent {
    SessionEvent::ContextMode {
        payload: Box::new(ContextModeRecord {
            policy,
            actor: ModeActor::Human,
            reason: ModeReason::Enable,
            previous_revision: 0,
        }),
    }
}

/// A managed session enabled with `policy`.
fn managed_with(policy: ContextPolicy) -> Session {
    let mut session = Session::new(SessionId::new("s"), 0, "/w");
    session.upgrade_to_managed_body();
    session.append(enable(policy));
    session
}

fn explicit(tokens: u32) -> ContextPolicy {
    ContextPolicy {
        output_reserve_tokens: tokens,
        output_reserve_follows_model: false,
        ..ContextPolicy::managed()
    }
}

fn header_of(raw: &str) -> serde_json::Value {
    // A header that does not parse reads as null, so the assertions on it fail rather than panic here.
    serde_json::from_str(raw.lines().next().unwrap_or_default()).unwrap_or(serde_json::Value::Null)
}

#[test]
fn an_explicit_policy_returns_its_number_whatever_the_arguments() {
    let policy = explicit(4_096);
    for budget in [
        ReserveBudget::ModelWindow,
        ReserveBudget::Configured(64_000),
    ] {
        assert_eq!(
            policy.effective_reservation(Some(393_216), budget),
            Some(4_096)
        );
        assert_eq!(policy.effective_reservation(None, budget), Some(4_096));
    }
}

#[test]
fn a_following_policy_takes_the_ceiling_in_the_window_and_a_quarter_of_a_smaller_budget() {
    let following = ContextPolicy::managed();
    let window = ReserveBudget::ModelWindow;
    assert_eq!(
        following.effective_reservation(Some(393_216), window),
        Some(393_216)
    );
    // A configured budget of 64,000 leaves three quarters of it for input.
    let small = ReserveBudget::Configured(64_000);
    assert_eq!(
        following.effective_reservation(Some(393_216), small),
        Some(16_000)
    );
    let large = ReserveBudget::Configured(200_000);
    assert_eq!(
        following.effective_reservation(Some(393_216), large),
        Some(50_000)
    );
    // A ceiling already below a quarter of the budget is the ceiling.
    let huge = ReserveBudget::Configured(2_000_000);
    assert_eq!(
        following.effective_reservation(Some(50_000), huge),
        Some(50_000)
    );
    // A budget too small for a quarter to be a token is still one token.
    assert_eq!(
        following.effective_reservation(Some(393_216), ReserveBudget::Configured(2)),
        Some(1)
    );
}

#[test]
fn a_following_policy_on_a_model_with_no_ceiling_resolves_to_nothing() {
    let following = ContextPolicy::managed();
    assert_eq!(
        following.effective_reservation(None, ReserveBudget::ModelWindow),
        None
    );
    assert_eq!(
        following.effective_reservation(None, ReserveBudget::Configured(64_000)),
        None
    );
}

#[test]
fn a_following_snapshot_is_the_same_setting_but_an_explicit_number_is_not() {
    let snapshot_a = ContextPolicy {
        output_reserve_tokens: 16_000,
        ..ContextPolicy::managed()
    };
    let snapshot_b = ContextPolicy {
        output_reserve_tokens: 50_000,
        ..ContextPolicy::managed()
    };
    assert!(snapshot_a.same_settings(&snapshot_b));
    assert!(
        !snapshot_a.same_settings(&explicit(4_096)),
        "the flag is a setting"
    );
    assert!(!explicit(4_096).same_settings(&explicit(8_000)));
    assert!(explicit(4_096).same_settings(&explicit(4_096)));
}

#[test]
fn a_record_that_does_not_name_the_flag_is_explicit_and_only_a_present_true_is_read() {
    let base = r#"{"mode":"managed","output_reserve_tokens":4096,{FLAG}"capture_shell":false,"policy_version":1}"#;
    let parse = |flag: &str| serde_json::from_str::<ContextPolicy>(&base.replace("{FLAG}", flag));

    let absent = parse("").unwrap();
    assert!(!absent.output_reserve_follows_model);
    let present = parse(r#""output_reserve_follows_model":true,"#).unwrap();
    assert!(present.output_reserve_follows_model);
    assert!(
        parse(r#""output_reserve_follows_model":false,"#).is_err(),
        "a present false is a body this build did not write"
    );
    assert!(parse(r#""output_reserve_follows_model":1,"#).is_err());
}

#[test]
fn the_writer_omits_the_flag_when_the_reservation_is_explicit() {
    let explicit_json = serde_json::to_value(explicit(4_096)).unwrap();
    assert!(explicit_json.get("output_reserve_follows_model").is_none());
    let following_json = serde_json::to_value(ContextPolicy::managed()).unwrap();
    assert_eq!(following_json["output_reserve_follows_model"], true);
}

#[test]
fn a_following_policy_is_written_at_version_six_and_read_back() {
    let session = managed_with(ContextPolicy {
        output_reserve_tokens: 16_000,
        ..ContextPolicy::managed()
    });
    assert_eq!(session.body_version(), 6);
    let raw = session.try_to_jsonl().unwrap();
    let header = header_of(&raw);
    assert_eq!(header["version"], 6);
    assert_eq!(header["managed"], true);
    assert_eq!(Session::from_jsonl(&raw).unwrap(), session);
}

#[test]
fn an_explicit_policy_keeps_the_lower_version_it_had() {
    let session = managed_with(explicit(4_096));
    assert_eq!(session.body_version(), 3);
    assert_eq!(
        ContextPolicy::default().mode,
        ContextMode::Legacy,
        "the default is legacy, following"
    );
}

#[test]
fn a_follows_field_below_version_six_is_refused_on_read_whatever_its_value() {
    let raw = managed_with(ContextPolicy::managed())
        .try_to_jsonl()
        .unwrap();
    let as_five = raw.replacen("\"version\":6", "\"version\":5", 1);
    assert!(
        Session::from_jsonl(&as_five).is_err(),
        "a present true in version 5"
    );
    let as_false = as_five.replacen(
        "\"output_reserve_follows_model\":true",
        "\"output_reserve_follows_model\":false",
        1,
    );
    assert!(
        Session::from_jsonl(&as_false).is_err(),
        "a present false in version 5"
    );
}

#[test]
fn a_version_newer_than_six_is_refused_at_the_header() {
    let raw = managed_with(ContextPolicy::managed())
        .try_to_jsonl()
        .unwrap();
    let as_seven = raw.replacen("\"version\":6", "\"version\":7", 1);
    assert!(Session::from_jsonl(&as_seven).is_err());
}

/// One turn that ends cleanly: a human prompt, a model answer, and the turn's ending.
fn answered_turn(session: &mut Session, turn: u32, prompt: &str, answer: &str) {
    use nanus_domain::{TurnEndReason, Usage};
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
        usage: Some(Usage {
            prompt_tokens: 10,
            completion_tokens: 5,
            reasoning_tokens: 0,
            cache_hit_tokens: 0,
            cache_miss_tokens: 10,
        }),
        interrupted: false,
        model: Some("deepseek-flash".into()),
        effort: None,
    });
    session.append(SessionEvent::TurnEnd {
        turn,
        reason: TurnEndReason::Completed,
    });
}

/// Case 12: a session managed once, switched to legacy, compacted, and then given a following
/// record is written at version 6, and its compaction marker still validates on read.
#[test]
fn a_compacted_session_that_gains_a_following_record_is_version_six_and_still_validates() {
    let mut session = Session::new(SessionId::new("s"), 0, "/w");
    answered_turn(&mut session, 0, "first question", "first answer");
    answered_turn(&mut session, 1, "second question", "second answer");
    // Managed once, with an explicit reservation, then disabled: the body is managed and legacy.
    session.upgrade_to_managed_body();
    session.append(enable(explicit(4_096)));
    session.append(SessionEvent::ContextMode {
        payload: Box::new(ContextModeRecord {
            policy: ContextPolicy {
                mode: ContextMode::Legacy,
                ..explicit(4_096)
            },
            actor: ModeActor::Human,
            reason: ModeReason::Disable,
            previous_revision: 0,
        }),
    });
    let event = session
        .compaction_event("the summary".into(), "deepseek-flash".into(), None)
        .unwrap();
    session.append(event);
    assert_eq!(
        session.body_version(),
        5,
        "a marker is version five, and nothing follows yet"
    );
    let before = session.try_to_jsonl().unwrap();

    // The following record a `--context-output-reserve model` resume writes.
    session.append(SessionEvent::ContextMode {
        payload: Box::new(ContextModeRecord {
            policy: ContextPolicy {
                mode: ContextMode::Legacy,
                ..ContextPolicy::managed()
            },
            actor: ModeActor::Human,
            reason: ModeReason::ConfigurationChange,
            previous_revision: 0,
        }),
    });
    assert_eq!(session.body_version(), 6);
    let raw = session.try_to_jsonl().unwrap();
    assert_eq!(header_of(&raw)["version"], 6);
    let reread = Session::from_jsonl(&raw).expect("the marker still validates on read");
    assert_eq!(reread, session);
    assert!(before.lines().count() < raw.lines().count());
}
