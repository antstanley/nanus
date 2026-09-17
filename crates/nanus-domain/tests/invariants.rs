//! Integration tests for the cross-cutting invariants of `nanus-domain`.
//!
//! These exercise the crate through its public API only. Where a unit test asks
//! "does this function do what it says", these ask "can the invariant be broken
//! from outside" — which is the question that matters for a harness whose inputs
//! are model-authored.

use std::future::Future;

use nanus_domain::{
    AgentConfig, ApprovalOutcome, ApprovalPolicy, ApprovalRequest, Message, PermissionPreset,
    PromptBuilder, PromptError, Role, SandboxMode, Session, SessionError, SessionEvent, SessionId,
    StepOutcome, ToolCall, ToolCallId, ToolDefinition, ToolExecutor, ToolFuture, ToolName,
    ToolOutcome, ToolRegistry, ToolResult, ToolSchema, TurnEndReason, TurnMachine, TurnOutcome,
    Usage,
};
use proptest::prelude::*;
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Builds a tool name from a literal the test author has already checked.
#[allow(clippy::panic)]
fn name(raw: &str) -> ToolName {
    ToolName::new(raw).unwrap_or_else(|error| panic!("test tool name {raw}: {error}"))
}

/// A tool that answers every call with a fixed outcome, so the registry can be
/// tested without any I/O.
struct Stub {
    outcome: ToolOutcome,
}

impl ToolExecutor for Stub {
    fn execute(&self, call: ToolCall) -> ToolFuture {
        let outcome = self.outcome.clone();
        Box::pin(async move { ToolResult::new(call.id, outcome) })
    }
}

fn stub_registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    let schema = ToolSchema {
        name: name("read"),
        description: "read a file".to_owned(),
        parameters: json!({ "type": "object", "properties": { "path": { "type": "string" } } }),
    };
    let registered = registry.register(ToolDefinition::new(
        schema,
        Stub {
            outcome: ToolOutcome::success(json!({ "ok": true })),
        },
    ));
    assert!(registered.is_ok());
    registry
}

/// Drives a boxed future to completion without a runtime.
///
/// The domain may not grow an async runtime, so the test polls with a no-op
/// waker. Every future this crate produces is immediately ready.
fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = Box::pin(future);
    let waker = std::task::Waker::noop();
    let mut context = std::task::Context::from_waker(waker);
    loop {
        if let std::task::Poll::Ready(value) = future.as_mut().poll(&mut context) {
            return value;
        }
    }
}

// ---------------------------------------------------------------------------
// The wire allowlist
// ---------------------------------------------------------------------------

#[test]
fn a_registered_tools_schema_is_exactly_the_allowlist() {
    // The invariant: a tool's registration may hold anything — secrets, handles,
    // configuration — and none of it can be encoded into a model request. The
    // executable half is not `Serialize`, so the only encodable half is the
    // schema, and the schema's key set is the allowlist.
    let registry = stub_registry();
    let encoded = serde_json::to_value(registry.schemas());
    assert!(encoded.is_ok());
    let Ok(encoded) = encoded else { return };
    let Some(entries) = encoded.as_array() else {
        panic!("the schema projection is a list");
    };
    assert_eq!(entries.len(), 1, "one registered tool, one schema");
    let Some(entry) = entries.first() else {
        return;
    };
    let Some(object) = entry.as_object() else {
        panic!("a schema is a JSON object");
    };
    let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec!["description", "name", "parameters"],
        "adding a fourth field here is a wire change and must be a deliberate one"
    );
    assert!(
        object
            .values()
            .all(|value| !value.is_object() || value.get("executor").is_none()),
        "no executor state appears anywhere in the projection"
    );
}

#[test]
fn the_tool_schema_type_cannot_carry_an_executor() {
    // The type-level half of the invariant: `ToolDefinition` has no `Serialize`
    // impl, so this does not compile if the executable half is ever moved into
    // the schema. The test exists to document that the compile failure is the
    // assertion.
    let schema = stub_registry().schemas().into_iter().next().cloned();
    assert!(schema.is_some(), "the fixture registers one tool");
    let Some(schema) = schema else { return };
    let text = serde_json::to_string(&schema).unwrap_or_default();
    assert!(!text.contains("Stub"), "no concrete executor type is named");
}

// ---------------------------------------------------------------------------
// The DeepSeek empty-content rule
// ---------------------------------------------------------------------------

#[test]
fn an_assistant_turn_with_no_text_never_sends_null_content() {
    // Every combination that could plausibly produce an empty assistant turn.
    let cases = [
        Message::assistant(None, None, Vec::new()),
        Message::assistant(None, Some("reasoning".to_owned()), Vec::new()),
        Message::assistant(
            None,
            None,
            vec![ToolCall::new(ToolCallId::new("c"), name("read"), json!({}))],
        ),
        Message::assistant(Some(String::new()), None, Vec::new()),
    ];
    for message in &cases {
        let encoded = serde_json::to_value(message);
        assert!(encoded.is_ok());
        let Ok(encoded) = encoded else { continue };
        assert!(
            encoded.get("content").is_some(),
            "content is always present on an assistant turn: {encoded}"
        );
        assert_ne!(
            encoded.get("content"),
            Some(&Value::Null),
            "DeepSeek rejects a null content with HTTP 400: {encoded}"
        );
    }
}

#[test]
fn reasoning_is_replayed_when_a_turn_carries_tool_calls() {
    // DeepSeek requires prior reasoning_content back when tools are present, so
    // the fold must not drop it.
    let mut session = Session::new(SessionId::new("s"), 0, "/work");
    session.append(SessionEvent::AssistantMessage {
        text: None,
        reasoning: Some("I need to read the file".to_owned()),
        tool_calls: vec![ToolCall::new(
            ToolCallId::new("c"),
            name("read"),
            json!({ "path": "a" }),
        )],
        usage: None,
        interrupted: false,
        model: None,
        effort: None,
    });
    let messages = session.derive_messages();
    let Some(assistant) = messages.first() else {
        panic!("the assistant turn is model-visible");
    };
    assert_eq!(assistant.role(), Role::Assistant);
    assert_eq!(assistant.reasoning(), Some("I need to read the file"));
    assert_eq!(assistant.text(), None);
    let encoded = serde_json::to_value(assistant).unwrap_or(Value::Null);
    assert_eq!(
        encoded.get("content"),
        Some(&Value::String(String::new())),
        "text is absent but content is an empty string"
    );
    assert_eq!(
        encoded.get("reasoning_content"),
        Some(&Value::String("I need to read the file".to_owned()))
    );
}

// ---------------------------------------------------------------------------
// The surface fold
// ---------------------------------------------------------------------------

#[test]
fn the_fold_skips_empty_assistant_turns_and_keeps_tool_results() {
    let mut session = Session::new(SessionId::new("s"), 0, "/work");
    session.append(SessionEvent::TurnStart { turn: 0 });
    session.append(SessionEvent::StepStart { turn: 0, step: 0 });
    session.append(SessionEvent::UserMessage {
        text: "hello".to_owned(),
    });
    // Nothing a model can read: skipped.
    session.append(SessionEvent::AssistantMessage {
        text: None,
        reasoning: None,
        tool_calls: Vec::new(),
        usage: Some(Usage::new(1, 0, 0, 0, 1)),
        interrupted: false,
        model: None,
        effort: None,
    });
    session.append(SessionEvent::AssistantMessage {
        text: Some("calling a tool".to_owned()),
        reasoning: None,
        tool_calls: vec![ToolCall::new(ToolCallId::new("c"), name("read"), json!({}))],
        usage: None,
        interrupted: false,
        model: None,
        effort: None,
    });
    session.append(SessionEvent::ToolResult {
        call_id: ToolCallId::new("c"),
        content: "contents".to_owned(),
        is_error: false,
    });
    session.append(SessionEvent::TurnEnd {
        turn: 0,
        reason: TurnEndReason::Completed,
    });

    let roles: Vec<Role> = session
        .derive_messages()
        .iter()
        .map(Message::role)
        .collect();
    assert_eq!(roles, vec![Role::User, Role::Assistant, Role::Tool]);
    assert_eq!(
        session.usage_totals().total_tokens(),
        1,
        "usage is summed from every assistant record, including skipped ones"
    );
}

// ---------------------------------------------------------------------------
// Sessions: round trip and its negative space
// ---------------------------------------------------------------------------

/// Builds a session whose log is a valid, fully populated transcript.
fn populated_session() -> Session {
    let mut session = Session::new(SessionId::new("s-42"), 1_700_000_000_000, "/work");
    session.append(SessionEvent::TurnStart { turn: 0 });
    session.append(SessionEvent::StepStart { turn: 0, step: 0 });
    session.append(SessionEvent::UserMessage {
        text: "line one\nline \"two\"\ttabbed".to_owned(),
    });
    session.append(SessionEvent::AssistantMessage {
        text: Some("answer".to_owned()),
        reasoning: Some("because".to_owned()),
        tool_calls: vec![ToolCall::new(
            ToolCallId::new("c-1"),
            name("read"),
            json!({ "path": "src/lib.rs", "nested": { "a": [1, 2, 3] } }),
        )],
        usage: Some(Usage::new(10, 5, 2, 1, 9)),
        interrupted: false,
        model: None,
        effort: None,
    });
    session.append(SessionEvent::ToolCall {
        call_id: ToolCallId::new("c-1"),
        name: name("read"),
        arguments: json!({ "path": "src/lib.rs" }),
    });
    session.append(SessionEvent::ToolResult {
        call_id: ToolCallId::new("c-1"),
        content: "fn main() {}".to_owned(),
        is_error: true,
    });
    session.append(SessionEvent::StepEnd { turn: 0, step: 0 });
    session.append(SessionEvent::TurnEnd {
        turn: 0,
        reason: TurnEndReason::Aborted {
            reason: "human said stop".to_owned(),
        },
    });
    session
}

#[test]
fn a_session_survives_its_file_format() {
    let original = populated_session();
    let decoded = Session::from_jsonl(&original.to_jsonl());
    assert!(decoded.is_ok(), "the whole session round-trips");
    let Ok(decoded) = decoded else { return };
    assert_eq!(decoded, original);
    // Pair assertion: the fold is stable across the round trip, which is what
    // makes a resumed session send the same request as the original run.
    assert_eq!(decoded.derive_messages(), original.derive_messages());
}

#[test]
fn a_foreign_format_is_rejected() {
    let raw = r#"{"format":"someone.else","version":1,"id":"s","created_at_ms":0,"cwd":"/w"}"#;
    assert!(matches!(
        Session::from_jsonl(raw),
        Err(SessionError::BadHeader { .. })
    ));
}

#[test]
fn an_unsupported_version_is_rejected() {
    let raw = r#"{"format":"nanus.session","version":2,"id":"s","created_at_ms":0,"cwd":"/w"}"#;
    assert!(matches!(
        Session::from_jsonl(raw),
        Err(SessionError::UnsupportedVersion {
            found: 2,
            expected: 1
        })
    ));
}

#[test]
fn a_truncated_tail_is_rejected_rather_than_silently_dropped() {
    // A crash mid-write is the realistic failure: the model must not silently
    // lose the last thing it was told.
    let encoded = populated_session().to_jsonl();
    let truncated = encoded
        .get(..encoded.len().saturating_sub(30))
        .unwrap_or_default();
    let decoded = Session::from_jsonl(truncated);
    assert!(
        matches!(decoded, Err(SessionError::MalformedEvent { .. })),
        "a half-written line is a typed error, got {decoded:?}"
    );
}

#[test]
fn a_sequence_hole_is_rejected() {
    let header = json!({
        "format": "nanus.session",
        "version": 1,
        "id": "s",
        "created_at_ms": 0,
        "cwd": "/w",
    });
    let first = json!({ "seq": 0, "event": { "type": "turn_start", "turn": 0 } });
    let skipped = json!({ "seq": 5, "event": { "type": "turn_start", "turn": 1 } });
    let raw = format!("{header}\n{first}\n{skipped}\n");
    let decoded = Session::from_jsonl(&raw);
    assert_eq!(
        decoded,
        Err(SessionError::NonContiguousSequence {
            line: 3,
            expected: 1,
            found: 5
        })
    );
}

// ---------------------------------------------------------------------------
// The turn machine
// ---------------------------------------------------------------------------

#[allow(clippy::panic)]
fn machine(max_steps: u32) -> TurnMachine {
    let config = AgentConfig::new(max_steps, 4, "deepseek-flash", 4096)
        .unwrap_or_else(|error| panic!("test config: {error}"));
    TurnMachine::new(config).unwrap_or_else(|error| panic!("test machine: {error}"))
}

#[test]
fn a_turn_closes_only_when_nothing_is_owed() {
    let mut session = Session::new(SessionId::new("s"), 0, "/work");
    session.append(SessionEvent::TurnStart { turn: 0 });
    session.append(SessionEvent::StepStart { turn: 0, step: 0 });
    session.append(SessionEvent::ToolCall {
        call_id: ToolCallId::new("c-1"),
        name: name("read"),
        arguments: json!({}),
    });
    // The model claims to be finished, but a tool result is outstanding, so the
    // log — not the caller's word — keeps the turn open.
    let open = machine(4).decide(session.log(), &StepOutcome::FinalAnswer);
    assert_eq!(open, TurnOutcome::Continue { step: 1 });

    session.append(SessionEvent::ToolResult {
        call_id: ToolCallId::new("c-1"),
        content: "done".to_owned(),
        is_error: false,
    });
    let closed = machine(4).decide(session.log(), &StepOutcome::FinalAnswer);
    assert_eq!(closed, TurnOutcome::Completed);
    assert!(
        closed
            .turn_end_reason()
            .is_some_and(|reason| reason.is_success())
    );
}

#[test]
fn an_unbounded_tool_loop_is_stopped_by_the_budget() {
    // The divergence from dsh, stated as a test: without a budget this loop runs
    // until a human notices.
    let machine = machine(3);
    let mut session = Session::new(SessionId::new("s"), 0, "/work");
    session.append(SessionEvent::TurnStart { turn: 0 });
    let mut decisions = Vec::new();
    for step in 0..8_u32 {
        session.append(SessionEvent::StepStart { turn: 0, step });
        let decision = machine.decide(session.log(), &StepOutcome::ToolCalls { count: 1 });
        decisions.push(decision.clone());
        if decision.is_closed() {
            break;
        }
        session.append(SessionEvent::StepEnd { turn: 0, step });
    }
    assert_eq!(
        decisions.last(),
        Some(&TurnOutcome::MaxSteps),
        "the loop is stopped at the budget"
    );
    assert_eq!(
        decisions.last().and_then(TurnOutcome::turn_end_reason),
        Some(TurnEndReason::MaxSteps)
    );
}

// ---------------------------------------------------------------------------
// Permissions
// ---------------------------------------------------------------------------

#[test]
fn permission_defaults_are_fail_closed() {
    assert_eq!(
        PermissionPreset::default().approval,
        ApprovalPolicy::PerCall
    );
    assert_eq!(PermissionPreset::default().sandbox, SandboxMode::ReadOnly);
    assert!(!ApprovalOutcome::Cancelled.is_allowed());
    assert!(!ApprovalOutcome::Unavailable.is_allowed());
    assert!(!ApprovalOutcome::Rejected.is_allowed());
    assert!(ApprovalOutcome::AllowedOnce.is_allowed());
}

#[test]
fn a_tool_that_fails_is_an_outcome_and_not_a_domain_error() {
    // The two error channels meet in the registry, and the direction is
    // deliberate: a failure the *model* caused is information for the model.
    let mut registry = ToolRegistry::new();
    let schema = ToolSchema {
        name: name("write"),
        description: "write a file".to_owned(),
        parameters: json!({ "type": "object" }),
    };
    let registered = registry.register(ToolDefinition::new(
        schema,
        Stub {
            outcome: ToolOutcome::failure("disk is full"),
        },
    ));
    assert!(registered.is_ok());

    let call = ToolCall::new(
        ToolCallId::new("c-9"),
        name("write"),
        json!({ "path": "a" }),
    );
    let result = block_on(registry.execute(call));
    assert!(!result.is_success());
    assert_eq!(result.render_text(), "disk is full");
    assert_eq!(
        result.call_id.as_str(),
        "c-9",
        "the result matches the call"
    );
}

#[test]
fn an_unknown_tool_is_reported_to_the_model_rather_than_panicking() {
    let registry = stub_registry();
    let call = ToolCall::new(ToolCallId::new("c-1"), name("nope"), json!({}));
    let result = block_on(registry.execute(call));
    assert!(!result.is_success());
    assert!(
        result.render_text().contains("not registered"),
        "the model can correct its mistake: {}",
        result.render_text()
    );
}

#[test]
fn an_approval_request_cannot_carry_arguments() {
    // The prompt shows a tool name and the harness's reason. Model-controlled
    // argument text must not be able to reach the human's decision.
    let request = ApprovalRequest::new(name("write")).with_reason("outside the workspace");
    let encoded = serde_json::to_string(&request).unwrap_or_default();
    assert!(!encoded.contains("arguments"));
    assert!(encoded.contains("write"));
}

// ---------------------------------------------------------------------------
// Prompt assembly
// ---------------------------------------------------------------------------

#[test]
fn an_unresolved_reference_is_never_a_silent_empty_string() {
    let prompt = PromptBuilder::new().section("body", 0, "cwd={{cwd}}");
    let rendered = prompt.render();
    assert!(matches!(
        rendered,
        Err(PromptError::UnresolvedVariable { .. })
    ));
    assert!(
        rendered.is_err(),
        "the caller cannot mistake this for success"
    );
}

#[test]
fn a_malformed_complete_group_is_an_error() {
    let unclosed = PromptBuilder::new()
        .section("body", 0, "{{#tools}}a")
        .variable("tools", "read");
    assert!(matches!(
        unclosed.render(),
        Err(PromptError::MalformedGroup { .. })
    ));
}

// ---------------------------------------------------------------------------
// Property tests
// ---------------------------------------------------------------------------

/// A strategy for text that includes the characters most likely to break the
/// JSONL framing: quotes, backslashes, newlines, and braces.
fn text() -> impl Strategy<Value = String> {
    prop_oneof![Just(String::new()), "[ -~\\n\\t\"\\\\{}]{0,48}",]
}

/// A strategy for a non-empty session id.
///
/// The empty id is a header error by design, so the round-trip property uses a
/// strategy that cannot generate one; the negative case has its own test.
fn session_id_text() -> impl Strategy<Value = String> {
    "[ -~]{1,24}"
}

/// A strategy for a syntactically valid tool name.
fn tool_name_text() -> impl Strategy<Value = String> {
    "[a-z0-9_-]{1,16}"
}

/// A strategy for a turn-end reason, covering every variant.
fn turn_end_reason() -> impl Strategy<Value = TurnEndReason> {
    prop_oneof![
        Just(TurnEndReason::Completed),
        text().prop_map(|reason| TurnEndReason::Aborted { reason }),
        Just(TurnEndReason::Blocked),
        text().prop_map(|message| TurnEndReason::Error { message }),
        Just(TurnEndReason::MaxTokens),
        Just(TurnEndReason::MaxSteps),
        Just(TurnEndReason::Interrupted),
    ]
}

/// A strategy for token accounting.
fn usage() -> impl Strategy<Value = Usage> {
    (
        any::<u32>(),
        any::<u32>(),
        any::<u32>(),
        any::<u32>(),
        any::<u32>(),
    )
        .prop_map(|(prompt, completion, reasoning, hit, miss)| {
            Usage::new(prompt, completion, reasoning, hit, miss)
        })
}

/// A strategy for one session event.
fn event() -> impl Strategy<Value = SessionEvent> {
    prop_oneof![
        any::<u32>().prop_map(|turn| SessionEvent::TurnStart { turn }),
        (any::<u32>(), turn_end_reason())
            .prop_map(|(turn, reason)| SessionEvent::TurnEnd { turn, reason }),
        (any::<u32>(), any::<u32>())
            .prop_map(|(turn, step)| SessionEvent::StepStart { turn, step }),
        (any::<u32>(), any::<u32>()).prop_map(|(turn, step)| SessionEvent::StepEnd { turn, step }),
        text().prop_map(|text| SessionEvent::UserMessage { text }),
        (
            prop::option::of(text()),
            prop::option::of(text()),
            prop::collection::vec((tool_name_text(), any::<bool>()), 0..3),
            prop::option::of(usage()),
            any::<bool>(),
            // The provenance fields are generated too, so the round-trip property covers a
            // recorded model and a recorded gap rather than only the absent case.
            prop::option::of(text()),
            prop::option::of(text()),
        )
            .prop_map(
                |(text, reasoning, calls, usage, interrupted, model, effort)| {
                    let tool_calls = calls
                        .into_iter()
                        .enumerate()
                        .map(|(index, (tool, object))| {
                            let arguments = if object {
                                json!({ "index": index })
                            } else {
                                json!({})
                            };
                            ToolCall::new(
                                ToolCallId::new(format!("c-{index}")),
                                name(&tool),
                                arguments,
                            )
                        })
                        .collect();
                    SessionEvent::AssistantMessage {
                        text,
                        reasoning,
                        tool_calls,
                        usage,
                        interrupted,
                        model,
                        effort,
                    }
                },
            ),
        (text(), tool_name_text(), any::<bool>()).prop_map(|(call_id, tool, object)| {
            SessionEvent::ToolCall {
                call_id: ToolCallId::new(call_id),
                name: name(&tool),
                arguments: if object { json!({ "a": 1 }) } else { json!({}) },
            }
        }),
        (text(), text(), any::<bool>()).prop_map(|(call_id, content, is_error)| {
            SessionEvent::ToolResult {
                call_id: ToolCallId::new(call_id),
                content,
                is_error,
            }
        }),
    ]
}

proptest! {
    /// The round trip is the whole reason the file format exists: a resumed
    /// session must be byte-for-byte the session that was saved.
    #[test]
    fn session_jsonl_round_trips(
        id in session_id_text(),
        created_at_ms in any::<u64>(),
        cwd in text(),
        events in prop::collection::vec(event(), 0..12),
    ) {
        let mut session = Session::new(SessionId::new(id), created_at_ms, cwd);
        for event in events {
            session.append(event);
        }
        let encoded = session.to_jsonl();
        let decoded = Session::from_jsonl(&encoded);
        prop_assert!(decoded.is_ok());
        prop_assert_eq!(decoded.ok(), Some(session));
    }

    /// The encoded form is always one header line plus one line per event, so a
    /// reader can stream it.
    #[test]
    fn session_jsonl_has_one_line_per_event(
        events in prop::collection::vec(event(), 0..12),
    ) {
        let mut session = Session::new(SessionId::new("s"), 0, "/w");
        for event in events {
            session.append(event);
        }
        let encoded = session.to_jsonl();
        prop_assert_eq!(encoded.lines().count(), session.event_count().saturating_add(1));
        prop_assert!(encoded.ends_with('\n'));
    }

    /// Interpolating a template built from references to defined variables
    /// produces exactly the concatenation of their values.
    #[test]
    fn interpolation_of_defined_variables_is_exact(
        values in prop::collection::vec(text(), 3),
    ) {
        let names = ["alpha", "beta", "gamma"];
        let mut builder = PromptBuilder::new();
        let mut template = String::new();
        let mut expected = String::new();
        for (index, name) in names.iter().enumerate() {
            let value = values.get(index).cloned().unwrap_or_default();
            builder = builder.variable(*name, value.clone());
            template.push_str("{{");
            template.push_str(name);
            template.push_str("}}|");
            expected.push_str(&value);
            expected.push('|');
        }
        let rendered = builder.section("body", 0, template).render();
        prop_assert!(rendered.is_ok());
        prop_assert_eq!(rendered.ok(), Some(expected));
    }

    /// A reference to a name that was never defined is always an error, never an
    /// empty substitution. No variable is defined at all, so every generated name
    /// is genuinely unresolved.
    #[test]
    fn interpolation_of_undefined_variables_always_fails(
        missing in "[a-z]{1,8}",
    ) {
        let prompt = PromptBuilder::new()
            .section("body", 0, format!("before {{{{{missing}}}}} after"));
        let rendered = prompt.render();
        // Bound rather than matched inline: `prop_assert!` stringifies its
        // condition, and a struct pattern in the message is not re-parseable.
        let unresolved = matches!(rendered, Err(PromptError::UnresolvedVariable { .. }));
        prop_assert!(unresolved);
    }

    /// A section that references nothing is returned unchanged, so a prompt
    /// without variables cannot be damaged by assembly.
    ///
    /// The alphabet excludes braces on purpose: a lone `{{` is an unterminated
    /// reference and therefore an error by design, which the negative test above
    /// covers. Both braces individually are ordinary text, which this range
    /// keeps.
    #[test]
    fn interpolation_leaves_plain_text_alone(body in "[ -z|~\\n]{0,64}") {
        let prompt = PromptBuilder::new().section("body", 0, body.clone());
        let rendered = prompt.render();
        prop_assert!(rendered.is_ok());
        // A section that is blank is dropped by design, so the property is
        // "unchanged unless it was blank".
        let expected = if body.trim().is_empty() {
            String::new()
        } else {
            body
        };
        prop_assert_eq!(rendered.ok(), Some(expected));
    }
}
