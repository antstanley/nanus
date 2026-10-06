//! Typed human input is never reduced to its display projection.
#![allow(clippy::unwrap_used)]
use base64::Engine as _;
use nanus_domain::context::managed::{ContextModeRecord, ContextPolicy, ModeActor, ModeReason};
use nanus_domain::{ContentBlock, Message, Session, SessionEvent, SessionId};
use serde_json::{Value, json};

fn pixels() -> ContentBlock {
    ContentBlock::Image {
        media_type: "image/png".into(),
        data_base64: base64::engine::general_purpose::STANDARD
            .encode(include_bytes!("data/tiny-green-triangle.png")),
    }
}
fn session(blocks: Option<Vec<ContentBlock>>) -> Session {
    let mut session = Session::new(SessionId::new("direct-input"), 123, "/fictional");
    session.append(SessionEvent::UserMessage {
        text: "display only".into(),
        content_blocks: blocks,
    });
    session
}
fn version(raw: &str, version: u32) -> String {
    let mut lines: Vec<Value> = raw
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    lines[0]["version"] = json!(version);
    lines
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}
#[test]
fn user_pixels_survive_message_json_and_session_reload_in_order() {
    let blocks = vec![
        ContentBlock::Text("before".into()),
        pixels(),
        ContentBlock::Text("after".into()),
    ];
    let message = Message::user_with_content(blocks.clone()).unwrap();
    assert!(!message.is_empty());
    let wire = serde_json::to_string(&message).unwrap();
    assert_eq!(serde_json::from_str::<Message>(&wire).unwrap(), message);
    let original = session(Some(blocks.clone()));
    let raw = original.try_to_jsonl().unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(raw.lines().next().unwrap()).unwrap()["version"],
        4
    );
    let restored = Session::from_jsonl(&raw).unwrap();
    assert_eq!(restored, original);
    let messages = restored.derive_messages();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].content_blocks(), Some(blocks.as_slice()));
}
#[test]
fn old_text_bodies_still_load_but_typed_user_content_cannot_be_downgraded() {
    let typed = session(Some(vec![pixels()])).try_to_jsonl().unwrap();
    let plain = session(None).try_to_jsonl().unwrap();
    for old in [1, 2, 3] {
        assert!(Session::from_jsonl(&version(&plain, old)).is_ok());
        assert!(Session::from_jsonl(&version(&typed, old)).is_err());
        let mut lines: Vec<Value> = plain
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        lines[1]["event"]["content_blocks"] = Value::Null;
        let raw = lines
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(Session::from_jsonl(&version(&raw, old)).is_err());
    }
}
#[test]
fn a_session_is_written_at_the_lowest_version_that_holds_it() {
    let header = |raw: &str| {
        serde_json::from_str::<Value>(raw.lines().next().unwrap()).unwrap()["version"].clone()
    };
    // Typed *tool* content is a version-2 body already, so it does not move the version.
    let mut plain = session(None);
    plain.append(SessionEvent::ToolResult {
        call_id: nanus_domain::ToolCallId::new("call-1"),
        content: "a still".into(),
        content_blocks: Some(vec![pixels()]),
        is_error: false,
    });
    assert_eq!(plain.body_version(), 2);
    for raw in [plain.to_jsonl(), plain.try_to_jsonl().unwrap()] {
        assert_eq!(header(&raw), 2, "an older build can still open it");
        assert_eq!(Session::from_jsonl(&raw).unwrap(), plain);
    }
    // One typed user message anywhere moves the whole session to version 4, which an older
    // build refuses rather than reading with the pixels dropped.
    let mut typed = plain;
    typed.append(SessionEvent::UserMessage {
        text: "look".into(),
        content_blocks: Some(vec![pixels()]),
    });
    assert_eq!(typed.body_version(), 4);
    for raw in [typed.to_jsonl(), typed.try_to_jsonl().unwrap()] {
        assert_eq!(header(&raw), 4);
        assert_eq!(Session::from_jsonl(&raw).unwrap(), typed);
        assert!(Session::from_jsonl(&version(&raw, 2)).is_err());
        assert!(Session::from_jsonl(&version(&raw, 3)).is_err());
    }
}

/// The record a session enables managed context with.
fn enable() -> SessionEvent {
    SessionEvent::ContextMode {
        payload: Box::new(ContextModeRecord {
            policy: ContextPolicy::managed(),
            actor: ModeActor::Human,
            reason: ModeReason::Enable,
            previous_revision: 0,
        }),
    }
}

/// The header line of an encoded session, parsed.
fn header_of(raw: &str) -> Value {
    serde_json::from_str::<Value>(raw.lines().next().unwrap()).unwrap()
}

/// Typed user content and managed context are independent: version 4 holds either or both, and
/// its header says which, so images never make a legacy session managed and never make a managed
/// one forget that it is.
#[test]
fn version_four_says_whether_it_is_managed_and_versions_below_it_never_do() {
    let typed = || {
        let mut session = session(None);
        session.append(SessionEvent::UserMessage {
            text: "look".into(),
            content_blocks: Some(vec![pixels()]),
        });
        session
    };
    // Images alone: version 4, not managed, and the header does not mention it.
    let legacy = typed();
    let raw = legacy.try_to_jsonl().unwrap();
    assert_eq!(header_of(&raw)["version"], 4);
    assert!(header_of(&raw).get("managed").is_none());
    let reread = Session::from_jsonl(&raw).unwrap();
    assert!(!reread.is_managed_body());
    assert_eq!(reread, legacy);

    // Managed alone: version 3, which implies it, so the header does not say it.
    let mut managed = session(None);
    managed.upgrade_to_managed_body();
    managed.append(enable());
    let raw = managed.try_to_jsonl().unwrap();
    assert_eq!(header_of(&raw)["version"], 3);
    assert!(header_of(&raw).get("managed").is_none());
    assert!(Session::from_jsonl(&raw).unwrap().is_managed_body());

    // Both: version 4, and the header carries what the version no longer implies.
    let mut both = typed();
    both.upgrade_to_managed_body();
    both.append(enable());
    let raw = both.try_to_jsonl().unwrap();
    assert_eq!(header_of(&raw)["version"], 4);
    assert_eq!(header_of(&raw)["managed"], true);
    let reread = Session::from_jsonl(&raw).unwrap();
    assert!(reread.is_managed_body());
    assert_eq!(reread, both);
}

/// The other direction: a managed record in a version-4 body that does not say it is managed is
/// refused, and so is a header below version 4 that claims to be.
#[test]
fn a_managed_record_needs_a_managed_header_and_only_version_four_may_say_so() {
    let mut both = session(Some(vec![pixels()]));
    both.upgrade_to_managed_body();
    both.append(enable());
    let raw = both.try_to_jsonl().unwrap();
    let unmanaged = raw.replacen(",\"managed\":true", "", 1);
    assert_ne!(unmanaged, raw);
    assert!(matches!(
        Session::from_jsonl(&unmanaged),
        Err(nanus_domain::SessionError::ManagedRecordInLegacyBody { version: 4, .. })
    ));
    let plain = session(None).try_to_jsonl().unwrap();
    let claimed = plain.replacen("\"version\":2", "\"version\":2,\"managed\":true", 1);
    assert_ne!(claimed, plain);
    assert!(matches!(
        Session::from_jsonl(&claimed),
        Err(nanus_domain::SessionError::BadHeader { .. })
    ));
}
#[test]
fn plaintext_json_stays_exact_and_image_only_input_is_visible() {
    assert_eq!(
        serde_json::to_string(&Message::user("plain")).unwrap(),
        r#"{"role":"user","content":"plain"}"#
    );
    let image_only = Message::User {
        text: String::new(),
        content_blocks: Some(vec![pixels()]),
    };
    assert!(!image_only.is_empty());
    assert!(
        Message::user_with_content(vec![ContentBlock::Text(String::new())])
            .unwrap()
            .is_empty()
    );
    assert_eq!(session(Some(vec![pixels()])).derive_messages().len(), 1);
}
#[test]
fn invalid_and_oversized_user_blocks_refuse_at_construction_and_persistence() {
    let invalid = ContentBlock::Image {
        media_type: "image/png".into(),
        data_base64: "broken".into(),
    };
    for blocks in [
        vec![],
        vec![pixels(); 5],
        vec![ContentBlock::Text("x".into()); 33],
        vec![invalid],
        vec![ContentBlock::Text(
            "x".repeat(nanus_domain::content::RECORD_BYTES_MAX),
        )],
    ] {
        assert!(Message::user_with_content(blocks.clone()).is_err());
        assert!(session(Some(blocks.clone())).try_to_jsonl().is_err());
        let raw = json!({"role":"user", "content":"display", "content_blocks":blocks});
        assert!(serde_json::from_value::<Message>(raw).is_err());
    }
}
#[test]
fn foreign_media_arrays_refuse_instead_of_silently_losing_images() {
    for block in [
        json!({"type":"image_url", "image_url":{"url":"https://invalid.test/a.png"}}),
        json!({"type":"image", "text":"misleading"}),
        json!({"other":"ignored"}),
    ] {
        let raw = json!({"role":"user", "content":[{"type":"text","text":"question"},block]});
        assert!(serde_json::from_value::<Message>(raw).is_err());
    }
    assert_eq!(
        serde_json::from_value::<Message>(
            json!({"role":"user", "content":[{"type":"text", "text":"ok"}]})
        )
        .unwrap(),
        Message::user("ok")
    );
}
#[test]
fn context_projection_cannot_change_pixels_or_split_a_human_turn() {
    let input = Message::user_with_content(vec![pixels()]).unwrap();
    let source = vec![
        Message::system("trusted"),
        Message::user("old"),
        Message::assistant(Some("old answer".into()), None, vec![]),
        input.clone(),
    ];
    let fitted = nanus_domain::context::fit_with(source.clone(), 4, |messages| {
        u32::try_from(messages.len().saturating_mul(2)).unwrap()
    })
    .unwrap_err();
    assert!(matches!(
        fitted,
        nanus_domain::context::FitError::TooLarge { .. }
    ));
    let kept = nanus_domain::context::fit_with(source.clone(), 6, |messages| {
        u32::try_from(messages.len().saturating_mul(2)).unwrap()
    })
    .unwrap();
    assert_eq!(kept.messages.last(), Some(&input));
    assert!(nanus_domain::context::identify_projection(&source, &kept.messages, 6).is_ok());
    let mut changed = source.clone();
    changed[3] = Message::user("image omitted");
    assert!(nanus_domain::context::identify_projection(&source, &changed, 100).is_err());
    assert!(nanus_domain::context::identify_projection(&source, &source, 100).is_ok());
}
