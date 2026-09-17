//! Turning a recorded session into something a human can read.
//!
//! A session log is written for a model: it is the exact sequence of facts a request is
//! derived from, including audit records like `tool/call` that never reach the wire. A
//! transcript is what a person reads instead — the same events, but with the model's
//! reasoning distinguished from its answer, tool calls paired with their results, and
//! step boundaries folded away.
//!
//! The two are deliberately different views of one source. Nothing here invents content:
//! every entry comes from an event, so a transcript cannot drift from the log it was
//! built from.

use std::collections::BTreeMap;

use nanus_domain::{Session, SessionEvent, ToolCallId};

use crate::notice::{self, Ending};

use crate::transcript::{Entry, Role, Transcript};

/// Builds a transcript from a session's event log.
///
/// This is the conversation and nothing else: no introduction is added, because a live
/// conversation has no need of a banner announcing what the reader is already in the
/// middle of.
#[must_use]
pub fn transcript_of(session: &Session) -> Transcript {
    build(session, None)
}

/// Builds a transcript from a *recorded* session, introduced by a one-line header.
///
/// Someone reading a session they were not present for needs to know what they are
/// looking at, and the header — title, directory, event count — is that. It belongs to
/// reading a recording rather than to the conversation, which is why the live interface
/// does not use it: a conversation that had just started would otherwise open by
/// announcing itself as `recorded session · <untitled> · 0 events`, which is simply
/// untrue.
#[must_use]
pub fn recording_of(session: &Session) -> Transcript {
    build(session, Some(header(session)))
}

/// Folds the event log into a transcript, optionally introduced by `banner`.
fn build(session: &Session, banner: Option<String>) -> Transcript {
    let mut transcript = Transcript::new();
    if let Some(banner) = banner {
        transcript.push(Entry::notice(banner));
    }
    // Whether the log closed its turns at all, which decides who reports a stop. A log that
    // ends mid-write has no ending to render, and then a step cut short has to say so itself;
    // a complete one says it once, from the ending.
    let events = session.log().events();
    let closed = events
        .iter()
        .any(|event| matches!(event, SessionEvent::TurnEnd { .. }));
    let mut steps = 0_u32;
    // Which tool each call id named, so a result can be paired with the call it answers.
    // The pairing is by id and not by position because the log does not interleave them: a
    // step writes every call it made and *then* every result, so the entry before a result
    // is the last call of the batch rather than the one that result answers.
    let mut calls: BTreeMap<ToolCallId, String> = BTreeMap::new();
    for event in events {
        apply(&mut transcript, event, closed, &mut steps, &mut calls);
    }
    // The last entry may still be marked streaming, which would draw a cursor on a
    // finished conversation.
    transcript.settle_tail();
    transcript
}

/// Renders the one-line header a session starts with.
fn header(session: &Session) -> String {
    let title = session
        .title()
        .unwrap_or_else(|| String::from("<untitled>"));
    // One line, prefixed so it reads as a banner rather than as something the model said.
    format!(
        "recorded session · {title} · {} · {} events",
        session.cwd(),
        session.event_count()
    )
}

/// Folds one event into the transcript.
///
/// `TurnStart`, `StepStart` and `StepEnd` are deliberately skipped as structural: a reader
/// cares what the model did, not where the loop drew its step boundaries, and printing them
/// would triple the length of every transcript to say nothing the entries themselves do not
/// already show.
///
/// `TurnEnd` is *not* skipped, though it was. Why a turn stopped is exactly what a reader
/// needs when it stopped early, and the live view has always said so — so a recording that
/// stayed silent made a turn cut off at its budget read as a finished one, which is the same
/// defect the live view was fixed for, one layer down. `steps` counts the steps of the turn
/// being folded, for the notice that names them, and `calls` is the id-to-name index the
/// results are paired through.
fn apply(
    transcript: &mut Transcript,
    event: &SessionEvent,
    closed: bool,
    steps: &mut u32,
    calls: &mut BTreeMap<ToolCallId, String>,
) {
    match event {
        SessionEvent::UserMessage { text } => {
            transcript.push(Entry::prose(Role::User, text.clone()));
        }
        SessionEvent::AssistantMessage {
            text,
            reasoning,
            interrupted,
            ..
        } => {
            // Reasoning first, because that is the order it was produced in and the order
            // a reader wants it: what the model thought, then what it concluded.
            if let Some(reasoning) = reasoning.as_deref().filter(|text| !text.is_empty()) {
                transcript.push(Entry::prose(Role::Reasoning, reasoning));
            }
            // The `interrupted` flag on the message is not rendered: the turn it belongs to
            // ends with a `TurnEnd` carrying the same fact, and saying it twice would be two
            // notices for one stop. What the flag is for is the case below, where there is no
            // text to show and the reader would otherwise see nothing at all for the step.
            if let Some(text) = text.as_deref().filter(|text| !text.is_empty()) {
                // A recorded message is settled: it is not still arriving.
                transcript.push(Entry::prose(Role::Assistant, text));
            } else if *interrupted && !closed {
                // A truncated log has no `TurnEnd` to render, and a step cut short mid-stream
                // is still worth a line: rendering nothing would look like the model simply
                // stopped. A complete log says it once, at the turn's end.
                transcript.push(Entry::notice("(the response was cut short)"));
            }
        }
        SessionEvent::ToolCall {
            call_id,
            name,
            arguments,
        } => {
            // Remembered under the id the result will name, which is the only thing that
            // pairs the two: a step's calls are all written before any of its results, so
            // position cannot.
            calls.insert(call_id.clone(), name.as_str().to_owned());
            transcript.push(Entry::tool_call(
                name.as_str().to_owned(),
                render_arguments(arguments),
            ));
        }
        SessionEvent::ToolResult {
            call_id,
            content,
            is_error,
        } => {
            // The name comes from the call this result *names*. Reaching for the last call
            // in the transcript instead gave every result of a multi-call step the last
            // call's name — which then made the interface draw the first call as still
            // running, its output under the next tool, and the last result twice.
            let name = calls
                .get(call_id)
                .cloned()
                .unwrap_or_else(|| String::from("tool"));
            transcript.push(Entry::tool_result(name, *is_error, summarise(content)));
        }
        SessionEvent::TurnStart { .. } => *steps = 0,
        SessionEvent::StepStart { .. } => *steps = steps.saturating_add(1),
        SessionEvent::TurnEnd { reason, .. } => {
            if let Some(notice) = notice::stopping(&Ending::from(reason), *steps) {
                transcript.push(Entry::notice(notice));
            }
        }
        SessionEvent::StepEnd { .. } => {}
    }
}

/// Renders tool arguments as compact JSON.
fn render_arguments(arguments: &serde_json::Value) -> String {
    // Single-line and unwrapped: an entry's own rendering decides how to fit it, and a
    // pretty-printed object would occupy a screen per call.
    arguments.to_string()
}

/// Shortens a tool result to something worth showing in a transcript.
///
/// Tool output is unbounded — a `read` returns a whole file — so a transcript shows its
/// shape rather than its contents: the first few lines, and how much was left. The full
/// text is in the session log, which is the source of truth for details.
#[must_use]
pub fn summarise(content: &str) -> String {
    /// How many lines a transcript shows before summarising.
    const MAX_LINES: usize = 8;
    /// How many characters a single line may occupy.
    const MAX_LINE: usize = 120;

    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return String::from("(no output)");
    }
    let mut rendered: Vec<String> = lines
        .iter()
        .take(MAX_LINES)
        .map(|line| truncate(line, MAX_LINE))
        .collect();
    let shown = rendered.len();
    if lines.len() > shown {
        rendered.push(format!(
            "… {} more lines",
            lines.len().saturating_sub(shown)
        ));
    }
    rendered.join("\n")
}

/// Truncates `text` to at most `max` bytes, on a character boundary.
fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    format!("{}…", text.get(..end).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use nanus_domain::{Session, SessionEvent, SessionId, ToolCallId, ToolName, TurnEndReason};
    use serde_json::json;

    use super::*;

    fn session_with(events: Vec<SessionEvent>) -> Session {
        let mut session = Session::new(SessionId::new("replay"), 0, "/tmp/workspace");
        for event in events {
            session.append(event);
        }
        session
    }

    fn name(raw: &str) -> ToolName {
        ToolName::new(raw).unwrap_or_else(|error| panic!("test tool {raw}: {error}"))
    }

    #[test]
    fn an_empty_recording_still_has_a_header() {
        let session = session_with(Vec::new());
        let transcript = recording_of(&session);
        // A blank screen would look like a failure to load.
        assert_eq!(transcript.len(), 1);
        assert_eq!(transcript.entries()[0].role(), Role::Harness);
        assert!(transcript.entries()[0].text().contains("/tmp/workspace"));
    }

    #[test]
    fn a_live_transcript_has_no_header() {
        // The live interface uses this one, so a conversation that had just started must
        // not open by claiming to be a recording of itself.
        let session = session_with(Vec::new());
        assert!(transcript_of(&session).is_empty());
    }

    #[test]
    fn reasoning_is_shown_before_the_answer() {
        let session = session_with(vec![SessionEvent::AssistantMessage {
            text: Some("the answer".to_owned()),
            reasoning: Some("the thinking".to_owned()),
            tool_calls: Vec::new(),
            usage: None,
            interrupted: false,
        }]);
        let transcript = transcript_of(&session);
        let roles: Vec<Role> = transcript.entries().iter().map(Entry::role).collect();
        // Reasoning first, then the answer: a reader needs to tell the two apart.
        assert_eq!(roles, vec![Role::Reasoning, Role::Assistant]);
    }

    #[test]
    fn an_empty_reasoning_or_text_field_produces_no_entry() {
        let session = session_with(vec![SessionEvent::AssistantMessage {
            text: None,
            reasoning: Some(String::new()),
            tool_calls: Vec::new(),
            usage: None,
            interrupted: false,
        }]);
        let transcript = transcript_of(&session);
        // An absent field is not an empty entry.
        assert!(transcript.is_empty());
    }

    #[test]
    fn a_tool_call_and_its_result_are_paired() {
        let session = session_with(vec![
            SessionEvent::ToolCall {
                call_id: ToolCallId::new("c1"),
                name: name("glob"),
                arguments: json!({ "pattern": "**/*.rs" }),
            },
            SessionEvent::ToolResult {
                call_id: ToolCallId::new("c1"),
                content: "a.rs\nb.rs".to_owned(),
                is_error: false,
            },
        ]);
        let transcript = transcript_of(&session);
        assert_eq!(transcript.len(), 2);
        // The call carries its name and arguments.
        assert!(matches!(
            transcript.entries()[0].kind(),
            crate::transcript::EntryKind::ToolCall { name, .. } if name == "glob"
        ));
        // And the result carries the same name, so a reader can tell which call it
        // answers without tracking ids.
        assert!(matches!(
            transcript.entries()[1].kind(),
            crate::transcript::EntryKind::ToolResult { name, is_error: false, .. } if name == "glob"
        ));
    }

    /// Every result of a multi-call step carries the name of the call it answers.
    ///
    /// A step writes all of its calls and then all of its results, in call order — so the
    /// entry before a result is the *last* call of the batch, not the one that result
    /// answers. Pairing by adjacency therefore gave the first call's output the second
    /// call's name, which in the interface made the first call draw as still running
    /// forever and the last result draw under a heading of its own.
    #[test]
    fn a_step_of_several_calls_pairs_every_result_with_its_own_call() {
        let session = session_with(vec![
            SessionEvent::ToolCall {
                call_id: ToolCallId::new("c-read"),
                name: name("read"),
                arguments: json!({ "file_path": "a.rs" }),
            },
            SessionEvent::ToolCall {
                call_id: ToolCallId::new("c-grep"),
                name: name("grep"),
                arguments: json!({ "pattern": "fn" }),
            },
            SessionEvent::ToolResult {
                call_id: ToolCallId::new("c-read"),
                content: "the read output".to_owned(),
                is_error: false,
            },
            SessionEvent::ToolResult {
                call_id: ToolCallId::new("c-grep"),
                content: "the grep output".to_owned(),
                is_error: false,
            },
        ]);
        let transcript = transcript_of(&session);
        let pairs: Vec<(String, String)> = transcript
            .entries()
            .iter()
            .filter_map(|entry| match entry.kind() {
                crate::transcript::EntryKind::ToolResult { name, content, .. } => {
                    Some((name.clone(), content.clone()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            pairs,
            vec![
                (String::from("read"), String::from("the read output")),
                (String::from("grep"), String::from("the grep output")),
            ],
            "each result is named by the call whose id it carries"
        );
    }

    /// The other direction, and the reason the pairing cannot be positional: a result may
    /// name a call that is not the one before it, including one from an earlier step.
    #[test]
    fn a_result_naming_an_earlier_call_is_named_by_that_call() {
        let session = session_with(vec![
            SessionEvent::ToolCall {
                call_id: ToolCallId::new("c-first"),
                name: name("glob"),
                arguments: json!({ "pattern": "*.rs" }),
            },
            SessionEvent::ToolCall {
                call_id: ToolCallId::new("c-second"),
                name: name("read"),
                arguments: json!({ "file_path": "a.rs" }),
            },
            SessionEvent::ToolResult {
                call_id: ToolCallId::new("c-second"),
                content: "read first".to_owned(),
                is_error: false,
            },
            SessionEvent::ToolResult {
                call_id: ToolCallId::new("c-first"),
                content: "glob second".to_owned(),
                is_error: false,
            },
        ]);
        let transcript = transcript_of(&session);
        let names: Vec<String> = transcript
            .entries()
            .iter()
            .filter_map(|entry| match entry.kind() {
                crate::transcript::EntryKind::ToolResult { name, .. } => Some(name.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            names,
            vec![String::from("read"), String::from("glob")],
            "a result is named by the call it names, whatever order they were written in"
        );
    }

    #[test]
    fn a_failed_tool_result_is_marked() {
        let session = session_with(vec![
            SessionEvent::ToolCall {
                call_id: ToolCallId::new("c1"),
                name: name("read"),
                arguments: json!({ "file_path": "missing.txt" }),
            },
            SessionEvent::ToolResult {
                call_id: ToolCallId::new("c1"),
                content: "read: missing.txt does not exist".to_owned(),
                is_error: true,
            },
        ]);
        let transcript = transcript_of(&session);
        assert!(matches!(
            transcript.entries()[1].kind(),
            crate::transcript::EntryKind::ToolResult { is_error: true, .. }
        ));
    }

    #[test]
    fn a_result_with_no_preceding_call_still_renders() {
        // A log truncated to start mid-call is malformed but must not panic: the result
        // is rendered under a generic name.
        let session = session_with(vec![SessionEvent::ToolResult {
            call_id: ToolCallId::new("orphan"),
            content: "dangling".to_owned(),
            is_error: false,
        }]);
        let transcript = transcript_of(&session);
        assert_eq!(transcript.len(), 1);
        assert!(matches!(
            transcript.entries()[0].kind(),
            crate::transcript::EntryKind::ToolResult { name, .. } if name == "tool"
        ));
    }

    #[test]
    fn structural_events_do_not_become_entries() {
        let session = session_with(vec![
            SessionEvent::TurnStart { turn: 1 },
            SessionEvent::StepStart { turn: 1, step: 1 },
            SessionEvent::StepEnd { turn: 1, step: 1 },
            SessionEvent::TurnEnd {
                turn: 1,
                reason: TurnEndReason::Completed,
            },
        ]);
        // Structural events say nothing the entries themselves do not already show.
        assert!(transcript_of(&session).is_empty());
    }

    /// A truncated log — one that stops mid-write — has no ending to render, so a step cut
    /// short mid-stream has to say so itself; otherwise nothing in the transcript says the
    /// response was incomplete.
    #[test]
    fn an_interrupted_step_in_a_truncated_log_says_so_itself() {
        let session = session_with(vec![SessionEvent::AssistantMessage {
            text: None,
            reasoning: None,
            tool_calls: Vec::new(),
            usage: None,
            interrupted: true,
        }]);
        let transcript = transcript_of(&session);
        assert!(
            transcript
                .entries()
                .iter()
                .any(|entry| entry.text().contains("cut short"))
        );
    }

    /// The defect this closes: a turn that stopped at its step budget, or failed, or was
    /// interrupted, read as a finished conversation — `nanus tui --session` on the very log
    /// the live view had just explained said nothing at all.
    #[test]
    fn a_turn_that_stopped_early_says_so_in_a_recording() {
        for (reason, expected) in [
            (TurnEndReason::MaxSteps, "step budget"),
            (TurnEndReason::MaxTokens, "token ceiling"),
            (TurnEndReason::Interrupted, "interrupted"),
            (TurnEndReason::Blocked, "policy"),
            (
                TurnEndReason::Error {
                    message: String::from("the model failed"),
                },
                "the model failed",
            ),
        ] {
            let session = session_with(vec![
                SessionEvent::TurnStart { turn: 1 },
                SessionEvent::StepStart { turn: 1, step: 1 },
                SessionEvent::AssistantMessage {
                    text: Some(String::from("half an answer")),
                    reasoning: None,
                    tool_calls: Vec::new(),
                    usage: None,
                    interrupted: false,
                },
                SessionEvent::StepEnd { turn: 1, step: 1 },
                SessionEvent::TurnEnd {
                    turn: 1,
                    reason: reason.clone(),
                },
            ]);
            let transcript = transcript_of(&session);
            let notices: Vec<&str> = transcript
                .entries()
                .iter()
                .filter(|entry| entry.role() == Role::Harness)
                .map(Entry::text)
                .collect();
            assert!(
                notices.iter().any(|notice| notice.contains(expected)),
                "{reason:?} reads as {notices:?}, which does not mention {expected:?}"
            );
        }

        // And the other direction: a completed turn adds no notice at all.
        let session = session_with(vec![
            SessionEvent::TurnStart { turn: 1 },
            SessionEvent::StepStart { turn: 1, step: 1 },
            SessionEvent::AssistantMessage {
                text: Some(String::from("the answer")),
                reasoning: None,
                tool_calls: Vec::new(),
                usage: None,
                interrupted: false,
            },
            SessionEvent::StepEnd { turn: 1, step: 1 },
            SessionEvent::TurnEnd {
                turn: 1,
                reason: TurnEndReason::Completed,
            },
        ]);
        assert!(
            transcript_of(&session)
                .entries()
                .iter()
                .all(|entry| entry.role() != Role::Harness),
            "a conversation that finished says nothing about finishing"
        );
    }

    #[test]
    fn a_long_tool_result_is_summarised_with_a_count() {
        let content: String = (0..50).fold(String::new(), |mut acc, i| {
            let _ = core::fmt::Write::write_fmt(&mut acc, format_args!("line {i}\n"));
            acc
        });
        let rendered = summarise(&content);
        assert!(rendered.contains("line 0"), "{rendered}");
        assert!(rendered.contains("42 more lines"), "{rendered}");
        assert!(!rendered.contains("line 49"), "the tail is not shown");
    }

    #[test]
    fn a_short_result_is_shown_whole() {
        assert_eq!(summarise("one\ntwo"), "one\ntwo");
        assert_eq!(summarise(""), "(no output)");
    }

    #[test]
    fn a_long_line_is_truncated_on_a_character_boundary() {
        let content = "é".repeat(200);
        let rendered = summarise(&content);
        assert!(rendered.ends_with('…'));
        // The retained prefix must be a real prefix, which a byte-wise cut would not
        // guarantee for multi-byte text.
        assert!(content.starts_with(rendered.trim_end_matches('…')));
    }

    #[test]
    fn arguments_are_rendered_on_one_line() {
        let rendered = render_arguments(&json!({ "a": 1, "b": [2, 3] }));
        assert!(!rendered.contains('\n'));
        assert!(rendered.contains("\"a\":1"));
    }

    #[test]
    fn a_replayed_transcript_has_no_streaming_tail() {
        let session = session_with(vec![SessionEvent::AssistantMessage {
            text: Some("done".to_owned()),
            reasoning: None,
            tool_calls: Vec::new(),
            usage: None,
            interrupted: false,
        }]);
        // A cursor on a finished conversation would suggest it was still arriving.
        assert!(!transcript_of(&session).is_streaming());
    }
}
