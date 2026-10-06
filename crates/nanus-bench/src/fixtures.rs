//! Deterministic inputs shared by the benchmarks.
//!
//! Every fixture is a pure function of its size argument, so a baseline recorded today and a
//! comparison run next month measure the same bytes. The shapes imitate what a coding agent
//! actually produces: a turn is a prompt, a step that reads and searches, and a markdown
//! answer, because that is the session a store saves and an interface replays.

use core::fmt::Write as _;

use nanus_domain::{
    Session, SessionEvent, SessionId, ToolCall, ToolCallId, ToolName, TurnEndReason, Usage,
};
use serde_json::json;

/// The working directory every fixture session claims.
pub const CWD: &str = "/workspace/nanus";

/// A paragraph of plain prose, roughly `bytes` long, cut at a word boundary.
#[must_use]
pub fn prose(bytes: usize) -> String {
    const WORDS: [&str; 16] = [
        "the",
        "session",
        "log",
        "is",
        "append-only",
        "and",
        "every",
        "event",
        "carries",
        "its",
        "sequence",
        "number",
        "so",
        "a",
        "truncated",
        "tail",
    ];
    let mut out = String::with_capacity(bytes.saturating_add(16));
    for word in WORDS.iter().cycle() {
        if out.len() >= bytes {
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
}

/// A Rust source file of `lines` lines, as a `read` tool result would carry it.
#[must_use]
pub fn source_file(lines: usize) -> String {
    let mut out = String::with_capacity(lines.saturating_mul(48));
    for line in 0..lines {
        // Writing to a `String` cannot fail.
        let _ = match line % 6 {
            0 => writeln!(out, "/// Returns the value at position {line}."),
            1 => writeln!(out, "pub fn item_{line}(input: &[u8]) -> Option<u8> {{"),
            2 => writeln!(out, "    let index = input.len().checked_sub({line})?;"),
            3 => writeln!(out, "    input.get(index).copied()"),
            4 => writeln!(out, "}}"),
            _ => writeln!(out),
        };
    }
    out
}

/// A model answer in markdown: headings, a list, inline code, a fenced block, and a table.
///
/// `sections` repeats the body, so the answer grows without changing its mix of elements.
#[must_use]
pub fn markdown_answer(sections: usize) -> String {
    let mut out = String::from("# Summary\n\n");
    for section in 0..sections {
        let _ = write!(
            out,
            "## Change {section}\n\n\
             The `SessionLog` now records **every** step, and the store writes it \
             atomically. {}\n\n\
             - `append` asserts the sequence stays contiguous\n\
             - `from_jsonl` refuses a hole with *the line number*\n\
             - a truncated tail is reported, not silently dropped\n\n\
             ```rust\n\
             fn append(&mut self, event: SessionEvent) {{\n    \
             let before = self.events.len();\n    \
             self.events.push(event);\n\
             }}\n\
             ```\n\n\
             | Crate | Lines | Tests |\n\
             |---|---:|---:|\n\
             | nanus-domain | 4120 | 212 |\n\
             | nanus-ports | 1890 | 96 |\n\n",
            prose(240)
        );
    }
    out
}

fn name(raw: &str) -> ToolName {
    // The names here are literals that satisfy the tool-name grammar; a failure is a bug in
    // this file, and a benchmark built on a wrong fixture should stop rather than measure it.
    ToolName::new(raw).unwrap_or_else(|error| unreachable!("fixture tool name: {error}"))
}

/// Appends one complete turn: a prompt, a step that reads and searches, and an answer.
fn turn(session: &mut Session, index: u32) {
    let read = ToolCall::new(
        ToolCallId::new(format!("call-{index}-read")),
        name("read"),
        json!({ "file_path": "crates/nanus-domain/src/session.rs", "offset": 1, "limit": 80 }),
    );
    let grep = ToolCall::new(
        ToolCallId::new(format!("call-{index}-grep")),
        name("grep"),
        json!({ "pattern": "fn append", "path": "crates", "output_mode": "content" }),
    );
    session.append(SessionEvent::TurnStart { turn: index });
    session.append(SessionEvent::UserMessage {
        text: format!("Turn {index}: explain how the session log stays contiguous."),
        content_blocks: None,
    });
    session.append(SessionEvent::StepStart {
        turn: index,
        step: 0,
    });
    session.append(SessionEvent::AssistantMessage {
        text: None,
        reasoning: Some(prose(600)),
        replay: None,
        tool_calls: vec![read.clone(), grep.clone()],
        usage: Some(Usage::new(12_000, 180, 120, 11_000, 1_000)),
        interrupted: false,
        model: Some("deepseek-flash".to_owned()),
        effort: Some("high".to_owned()),
    });
    for call in [&read, &grep] {
        session.append(SessionEvent::ToolCall {
            call_id: call.id.clone(),
            name: call.name.clone(),
            arguments: call.arguments.clone(),
        });
    }
    session.append(SessionEvent::ToolResult {
        call_id: read.id,
        content: source_file(80),
        content_blocks: None,
        is_error: false,
    });
    session.append(SessionEvent::ToolResult {
        call_id: grep.id,
        content: "crates/nanus-domain/src/session.rs:334:    pub fn append(&mut self, \
                  event: SessionEvent) {\ncrates/nanus-domain/src/session.rs:871:    pub fn \
                  append(&mut self, event: SessionEvent) {\n"
            .to_owned(),
        content_blocks: None,
        is_error: false,
    });
    session.append(SessionEvent::StepEnd {
        turn: index,
        step: 0,
    });
    session.append(SessionEvent::StepStart {
        turn: index,
        step: 1,
    });
    session.append(SessionEvent::AssistantMessage {
        text: Some(markdown_answer(1)),
        reasoning: Some(prose(300)),
        replay: None,
        tool_calls: Vec::new(),
        usage: Some(Usage::new(14_500, 420, 90, 13_800, 700)),
        interrupted: false,
        model: Some("deepseek-flash".to_owned()),
        effort: Some("high".to_owned()),
    });
    session.append(SessionEvent::StepEnd {
        turn: index,
        step: 1,
    });
    session.append(SessionEvent::TurnEnd {
        turn: index,
        reason: TurnEndReason::Completed,
    });
}

/// The number of events [`session`] appends per turn.
pub const EVENTS_PER_TURN: usize = 13;

/// A session of `turns` complete turns.
#[must_use]
pub fn session(turns: u32) -> Session {
    let mut session = Session::new(SessionId::new("bench-session"), 1_767_225_600_000, CWD);
    for index in 0..turns {
        turn(&mut session, index);
    }
    // Postcondition: the shape the benchmarks size their throughput by.
    assert_eq!(
        session.event_count(),
        usize::try_from(turns)
            .unwrap_or(usize::MAX)
            .saturating_mul(EVENTS_PER_TURN)
    );
    session
}
