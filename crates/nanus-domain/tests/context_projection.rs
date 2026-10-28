//! Original source and fitted notice prove whole-turn projection, without modifying the source.
//!
//! The notice is the candidate's tail, never one of its messages.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
use nanus_domain::context::{fit_with, fit_with_source, identify_projection};
use nanus_domain::{Message, ToolCall, ToolCallId, ToolName};
use serde_json::json;

fn source() -> Vec<Message> {
    let call = ToolCall::new(
        ToolCallId::new("old-call"),
        ToolName::new("read").unwrap(),
        json!({}),
    );
    vec![
        Message::system("original instructions"),
        Message::user("older question"),
        Message::assistant(None, None, vec![call]),
        Message::tool(ToolCallId::new("old-call"), "older result", false),
        Message::assistant(Some("older answer".into()), None, vec![]),
        Message::user("latest question"),
    ]
}
/// The fitted messages and the tail they carry.
fn fitted_with_tail(source: &[Message]) -> (Vec<Message>, Option<String>) {
    let fitted = fit_with_source(source, 100, |messages, _| {
        if messages
            .iter()
            .any(|message| matches!(message,Message::User { text, .. } if text=="older question"))
        {
            101
        } else {
            100
        }
    })
    .unwrap();
    (
        fitted.messages,
        fitted.elision.map(|elision| elision.notice()),
    )
}

fn fitted(source: &[Message]) -> Vec<Message> {
    fitted_with_tail(source).0
}

fn notice(source: &[Message]) -> String {
    fitted_with_tail(source).1.unwrap()
}

#[test]
fn borrowed_and_owned_fitting_preserve_source_and_identify_exact_complete_omission() {
    let source = source();
    let before = source.clone();
    let (candidate, tail) = fitted_with_tail(&source);
    let receipt = identify_projection(&source, &candidate, tail.as_deref(), 100).unwrap();
    assert_eq!((receipt.dropped_turns, receipt.dropped_messages), (1, 4));
    assert_eq!(
        candidate.len(),
        2,
        "the prompt and the newest turn, no notice"
    );
    assert_eq!(source, before);
    let owned = fit_with(source.clone(), 100, |messages, _| {
        if messages.len() > 2 { 101 } else { 100 }
    })
    .unwrap();
    assert_eq!(owned.messages, candidate);
    let full = identify_projection(&source, &source, None, 100).unwrap();
    assert_eq!((full.dropped_turns, full.dropped_messages), (0, 0));
    // A tail with nothing omitted, or an omission with no tail, is no projection.
    assert!(identify_projection(&source, &source, tail.as_deref(), 100).is_err());
    assert!(identify_projection(&source, &candidate, None, 100).is_err());
    assert_eq!(
        fit_with_source(&source, 100, |_, _| 0).unwrap().messages,
        source
    );
}

#[test]
fn changed_prompt_notice_budget_user_order_or_partial_old_turn_cannot_be_admitted_as_fitting() {
    let source = source();
    let (valid, tail) = fitted_with_tail(&source);
    let tail = tail.unwrap();
    for index in 0..valid.len() {
        let mut candidate = valid.clone();
        candidate[index] = if index == 1 {
            Message::user("changed question")
        } else {
            Message::system("changed")
        };
        assert!(identify_projection(&source, &candidate, Some(&tail), 100).is_err());
    }
    assert!(identify_projection(&source, &valid, Some(&tail), 100).is_ok());
    let forged = tail.replace("4 messages", "3 messages");
    assert_ne!(forged, tail);
    assert!(identify_projection(&source, &valid, Some(&forged), 100).is_err());
    assert!(identify_projection(&source, &valid, Some(&tail), 99).is_err());
    assert!(identify_projection(&source, &valid, Some(&tail), 0).is_err());
    for keep in 2..source.len() {
        let mut candidate = valid[..1].to_vec();
        candidate.extend_from_slice(&source[keep..]);
        if keep != 5 {
            assert!(identify_projection(&source, &candidate, Some(&tail), 100).is_err());
        }
    }
    for candidate in [Vec::new(), valid[..1].to_vec(), source[1..].to_vec()] {
        assert!(identify_projection(&source, &candidate, Some(&tail), 100).is_err());
    }
}

#[test]
fn one_message_old_turn_and_short_or_oversized_candidates_are_checked_without_index_panics() {
    let source = vec![Message::user("old"), Message::user("new")];
    let fitted = fit_with_source(&source, 1, |messages, _| {
        if messages.iter().any(|m| m == &Message::user("old")) {
            2
        } else {
            1
        }
    })
    .unwrap();
    let tail = fitted.elision.map(|elision| elision.notice());
    assert_eq!(fitted.messages, source[1..]);
    assert_eq!(
        identify_projection(&source, &fitted.messages, tail.as_deref(), 1)
            .unwrap()
            .dropped_messages,
        1
    );
    let mut oversized = source.clone();
    oversized.push(Message::user("extra"));
    for tail in [None, tail.as_deref()] {
        assert!(identify_projection(&source, &oversized, tail, 1).is_err());
        for prefix in 0..source.len() {
            assert!(identify_projection(&source, &source[..prefix], tail, 1).is_err());
        }
        let systems = vec![Message::system("first"), Message::system("second")];
        assert!(identify_projection(&systems, &systems[..1], tail, 1).is_err());
    }
}

#[test]
fn prospective_complete_results_can_fit_after_whole_turn_elision_with_exact_calls_and_notice() {
    use nanus_domain::context::identify_tool_result_projection;
    let mut source = source();
    let calls = ["a", "b"]
        .into_iter()
        .map(|id| {
            ToolCall::new(
                ToolCallId::new(id),
                ToolName::new("read").unwrap(),
                json!({}),
            )
        })
        .collect();
    source.push(Message::assistant(None, None, calls));
    source.push(Message::tool(ToolCallId::new("a"), "base a", true));
    source.push(Message::tool(ToolCallId::new("b"), "base b", true));
    let mut candidate = fitted(&source);
    let tail = notice(&source);
    let tail = Some(tail.as_str());
    let start = candidate.len().checked_sub(2).unwrap();
    candidate[start] = Message::tool(ToolCallId::new("a"), "proposed success", false);
    candidate[start.checked_add(1).unwrap()] =
        Message::tool(ToolCallId::new("b"), "proposed failure", true);
    assert!(identify_projection(&source, &candidate, tail, 100).is_err());
    assert_eq!(
        identify_tool_result_projection(&source, &candidate, tail, 100)
            .unwrap()
            .dropped_messages,
        4
    );
    assert!(identify_tool_result_projection(&source, &candidate, tail, 99).is_err());
    candidate.swap(start, start.checked_add(1).unwrap());
    assert!(identify_tool_result_projection(&source, &candidate, tail, 100).is_err());
    candidate.swap(start, start.checked_add(1).unwrap());
    candidate[start.checked_sub(1).unwrap()] = Message::assistant(None, None, vec![]);
    assert!(identify_tool_result_projection(&source, &candidate, tail, 100).is_err());
}

#[test]
fn incomplete_batches_and_batches_before_a_new_user_have_no_prospective_value_exemption() {
    use nanus_domain::context::identify_tool_result_projection;
    let calls = ["a", "b"]
        .into_iter()
        .map(|id| {
            ToolCall::new(
                ToolCallId::new(id),
                ToolName::new("read").unwrap(),
                json!({}),
            )
        })
        .collect();
    let mut source = vec![
        Message::user("question"),
        Message::assistant(None, None, calls),
        Message::tool(ToolCallId::new("a"), "base", true),
    ];
    let mut changed = source.clone();
    changed[2] = Message::tool(ToolCallId::new("a"), "proposed", false);
    assert!(identify_tool_result_projection(&source, &changed, None, 100).is_err());
    source.push(Message::tool(ToolCallId::new("b"), "base", true));
    source.push(Message::user("next question"));
    changed = source.clone();
    changed[2] = Message::tool(ToolCallId::new("a"), "proposed", false);
    assert!(identify_tool_result_projection(&source, &changed, None, 100).is_err());
}
