//! Versioned typed result persistence, including legacy isolation.
#![allow(clippy::unwrap_used)]

use base64::Engine as _;
use nanus_domain::{
    ContentBlock, Message, Session, SessionEvent, SessionId, ToolCall, ToolCallId, ToolName,
};
use serde_json::json;
use std::io::Cursor;

fn image(media: &str) -> ContentBlock {
    let format = if media == "image/png" {
        image::ImageFormat::Png
    } else {
        image::ImageFormat::Jpeg
    };
    let mut bytes = Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(2, 3)
        .write_to(&mut bytes, format)
        .unwrap();
    ContentBlock::Image {
        media_type: media.into(),
        data_base64: base64::engine::general_purpose::STANDARD.encode(bytes.into_inner()),
    }
}

fn session(blocks: Option<Vec<ContentBlock>>) -> Session {
    let mut session = Session::new(SessionId::new("pixels"), 123, "/caller");
    let call = ToolCall::new(
        ToolCallId::new("original"),
        ToolName::new("read_image").unwrap(),
        json!({ "path": "gone.png" }),
    );
    session.append(SessionEvent::AssistantMessage {
        replay: None,
        text: None,
        reasoning: None,
        tool_calls: vec![call.clone()],
        usage: None,
        interrupted: false,
        model: Some("host-model".into()),
        effort: None,
    });
    session.append(SessionEvent::ToolResult {
        call_id: call.id,
        content: "summary".into(),
        content_blocks: blocks,
        is_error: true,
    });
    session
}

#[test]
fn version_two_retains_pixel_order_call_id_and_error_through_folding_and_reload() {
    let blocks = vec![
        ContentBlock::Text("before".into()),
        image("image/png"),
        ContentBlock::Text("between".into()),
        image("image/jpeg"),
    ];
    let original = session(Some(blocks.clone()));
    let raw = original.try_to_jsonl().unwrap();
    assert!(raw.contains("\"version\":3"));
    let older = raw.replacen("\"version\":3", "\"version\":2", 1);
    assert_eq!(Session::from_jsonl(&older).unwrap(), original);
    let loaded = Session::from_jsonl(&raw).unwrap();
    assert_eq!(loaded, original);
    let messages = loaded.derive_messages();
    assert!(
        matches!(&messages[1], Message::Tool { call_id, content, content_blocks: Some(retained), is_error: true }
        if call_id.as_str() == "original" && content == "summary" && retained == &blocks)
    );
    let wire = serde_json::to_string(&messages[1]).unwrap();
    assert_eq!(serde_json::from_str::<Message>(&wire).unwrap(), messages[1]);
}

#[test]
fn version_one_remains_legacy_text_and_cannot_smuggle_typed_results() {
    let legacy = session(None);
    let raw = legacy
        .to_jsonl()
        .replacen("\"version\":3", "\"version\":1", 1);
    assert_eq!(Session::from_jsonl(&raw).unwrap(), legacy);
    let pixels = session(Some(vec![image("image/png")])).to_jsonl().replacen(
        "\"version\":3",
        "\"version\":1",
        1,
    );
    assert!(Session::from_jsonl(&pixels).is_err());
}

#[test]
fn invalid_lists_media_and_record_sizes_fail_at_read_and_write_boundaries() {
    assert!(session(Some(vec![])).try_to_jsonl().is_err());
    let malformed = session(Some(vec![ContentBlock::Image {
        media_type: "image/png".into(),
        data_base64: "AAAA".into(),
    }]));
    assert!(malformed.try_to_jsonl().is_err());
    assert!(Session::from_jsonl(&malformed.to_jsonl()).is_err());
    let oversized = session(Some(vec![ContentBlock::Text(
        "x".repeat(nanus_domain::content::RECORD_BYTES_MAX),
    )]));
    assert!(oversized.try_to_jsonl().is_err());
    assert!(Session::from_jsonl(&oversized.to_jsonl()).is_err());
    let empty = session(Some(vec![]));
    assert!(Session::from_jsonl(&empty.to_jsonl()).is_err());
    let huge = " ".repeat(nanus_domain::content::SESSION_BYTES_MAX.saturating_add(1));
    assert!(Session::from_jsonl(&huge).is_err());
}

#[test]
fn signed_replay_cannot_smuggle_pixels_or_change_the_executable_response() {
    use nanus_domain::message::AssistantReplay;
    let good = AssistantReplay {
        protocol: "anthropic.messages".into(),
        prefix_digest: "a".repeat(64),
        context_receipt: None,
        blocks: vec![
            serde_json::json!({
                "type": "thinking", "thinking": "", "signature": "opaque"
            }),
            serde_json::json!({"type":"text", "text":"answer"}),
        ],
    };
    assert!(good.validate_response(Some("answer"), &[]).is_ok());
    assert!(good.validate_response(Some("changed"), &[]).is_err());
    for block in [
        serde_json::json!({"type":"image", "source": {"data":"hidden"}}),
        serde_json::json!({"type":"thinking", "thinking":"", "signature":""}),
        serde_json::json!({"type":"text", "text":"answer", "extra":"hidden"}),
    ] {
        let bad = AssistantReplay {
            blocks: vec![block],
            ..good.clone()
        };
        assert!(bad.validate().is_err());
        assert!(
            serde_json::from_value::<AssistantReplay>(serde_json::to_value(&bad).unwrap()).is_err()
        );
    }
}

/// Newer Claude models add `caller` to a `tool_use` block; the API takes the block back with it,
/// so a replay that carries it must validate — and survive a session file — while a caller of
/// any other shape, or any other unknown field, is still refused, and the error names it.
#[test]
fn a_tool_call_naming_its_caller_replays_and_a_malformed_caller_does_not() {
    use nanus_domain::message::AssistantReplay;
    use nanus_domain::{ToolCall, ToolCallId, ToolName};
    let call = ToolCall::new(
        ToolCallId::new("toolu_01"),
        ToolName::new("read").unwrap(),
        serde_json::json!({"file_path": "src/lib.rs"}),
    );
    let with_caller = |caller: serde_json::Value| AssistantReplay {
        protocol: "anthropic.messages".into(),
        prefix_digest: "a".repeat(64),
        blocks: vec![serde_json::json!({
            "type": "tool_use", "id": "toolu_01", "name": "read",
            "input": {"file_path": "src/lib.rs"}, "caller": caller
        })],
        context_receipt: None,
    };
    for caller in [
        serde_json::json!({"type": "direct"}),
        serde_json::json!({"type": "code_execution_20260120", "tool_id": "srvtoolu_01"}),
    ] {
        let replay = with_caller(caller);
        assert!(
            replay
                .validate_response(None, std::slice::from_ref(&call))
                .is_ok()
        );
        let round =
            serde_json::from_value::<AssistantReplay>(serde_json::to_value(&replay).unwrap());
        assert_eq!(
            round.unwrap(),
            replay,
            "the caller survives a session file unchanged"
        );
    }
    for caller in [
        serde_json::json!("direct"),
        serde_json::json!({}),
        serde_json::json!({"type": ""}),
        serde_json::json!({"type": "direct", "extra": "hidden"}),
        serde_json::json!({"type": "code_execution_20260120", "tool_id": ""}),
    ] {
        assert!(with_caller(caller.clone()).validate().is_err(), "{caller}");
    }
    let unknown = AssistantReplay {
        blocks: vec![serde_json::json!({
            "type": "tool_use", "id": "toolu_01", "name": "read",
            "input": {}, "toolset": "hidden"
        })],
        ..with_caller(serde_json::json!({"type": "direct"}))
    };
    let error = unknown.validate().unwrap_err().to_string();
    assert!(
        error.contains("tool_use") && error.contains("toolset"),
        "{error}"
    );
}
