// A host's own tool, written and registered from outside the nanus workspace.
//
// This is the worked example `docs/host-tools.md` walks through, kept as a test so the guide
// cannot drift from what compiles. It depends on `nanus-bundle` with default features off, so
// it is the narrowest host there is: its own model, clock, registry and state.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use nanus_bundle::args::Arguments;
use nanus_bundle::{AgentRunner, Silent, ToolRegistryHandle, host_tools};
use nanus_domain::{
    AgentConfig, Destructive, SandboxMode, Session, SessionEvent, SessionId, ToolAccess, ToolCall,
    ToolCallId, ToolDefinition, ToolExecutor, ToolFuture, ToolName, ToolOutcome, ToolRegistry,
    ToolResult, ToolSchema,
};
use nanus_ports::{ChatRequest, ClockPort, FinishReason, LlmEvent, LlmPort, LlmStream};
use serde_json::json;

/// The longest note the tool keeps. A bound the model can read in the schema and hit in a
/// result, rather than one it discovers by filling the host's memory.
const NOTE_MAX_CHARS: usize = 200;

/// The host's state: a list the tool appends to. A real host would hold a document, a browser,
/// a connection — anything `'static` the executor can own or share.
type Notes = Rc<RefCell<Vec<String>>>;

/// Builds the `host_note` tool over the host's notes.
fn note_tool(notes: Notes) -> ToolDefinition {
    let schema = ToolSchema {
        name: ToolName::new("host_note").expect("a valid tool name"),
        description: format!(
            "Pin a short note to the host's notebook. Notes are kept for the user to read later; \
             at most {NOTE_MAX_CHARS} characters each."
        ),
        parameters: json!({
            "type": "object",
            "properties": {
                "text": { "type": "string", "description": "The note to pin." }
            },
            "required": ["text"],
            "additionalProperties": false
        }),
    };
    ToolDefinition::new(schema, NoteExecutor { notes })
        // It changes host state, so a read-only sandbox asks before it runs.
        .with_access(ToolAccess::Write)
        // It only appends, so the `permitted` state may run it without asking.
        .with_destructive(Destructive::Never)
}

struct NoteExecutor {
    notes: Notes,
}

impl ToolExecutor for NoteExecutor {
    fn execute(&self, call: ToolCall) -> ToolFuture {
        // Everything the future needs is moved in: a `ToolFuture` is `'static`.
        let notes = Rc::clone(&self.notes);
        Box::pin(async move {
            let args = Arguments::new(call.name.as_str(), &call.arguments);
            let text = match args.required_str("text") {
                Ok(text) => text,
                // A bad argument is the model's to fix, so it is a result, not an error.
                Err(outcome) => return ToolResult::new(call.id, outcome),
            };
            if text.chars().count() > NOTE_MAX_CHARS {
                return ToolResult::new(
                    call.id,
                    ToolOutcome::failure(format!(
                        "host_note: a note is at most {NOTE_MAX_CHARS} characters"
                    )),
                );
            }
            let count = {
                let mut notes = notes.borrow_mut();
                notes.push(text);
                notes.len()
            };
            ToolResult::success(call.id, json!({ "pinned": true, "notes": count }))
        })
    }
}

/// A model that calls `host_note` once with `arguments`, then answers.
struct Scripted {
    arguments: serde_json::Value,
    called: Cell<bool>,
    offered: RefCell<Vec<String>>,
}

impl LlmPort for Scripted {
    fn model(&self) -> &'static str {
        "host-model"
    }

    fn stream_chat(&self, request: ChatRequest) -> LlmStream {
        *self.offered.borrow_mut() = request
            .tools
            .iter()
            .map(|schema| schema.name.as_str().to_owned())
            .collect();
        if self.called.replace(true) {
            return Box::pin(futures::stream::iter([
                LlmEvent::TextDelta("done".into()),
                LlmEvent::Finished {
                    reason: FinishReason::Stop,
                },
            ]));
        }
        Box::pin(futures::stream::iter([LlmEvent::ToolCallDelta {
            index: 0,
            id: Some(ToolCallId::new("note-1")),
            name: Some(ToolName::new("host_note").expect("a valid tool name")),
            arguments_delta: self.arguments.to_string(),
        }]))
    }
}

struct Clock;
impl ClockPort for Clock {
    fn now_ms(&self) -> u64 {
        0
    }
}

/// Runs one turn in which the model calls `host_note` with `arguments`.
fn run(arguments: serde_json::Value, sandbox: SandboxMode) -> (Session, Vec<String>, Rc<Scripted>) {
    let notes: Notes = Rc::default();
    let mut registry = ToolRegistry::new();
    // The checked registration: a name the loop reserves, or one already taken, is refused.
    host_tools::register(&mut registry, note_tool(Rc::clone(&notes))).expect("registers");
    let model = Rc::new(Scripted {
        arguments,
        called: Cell::new(false),
        offered: RefCell::default(),
    });
    let llm: Rc<Box<dyn LlmPort>> = Rc::new(Box::new(Shared(Rc::clone(&model))));
    let config = AgentConfig::new(4, 1, "host-model", 16_384)
        .unwrap()
        .with_sandbox(sandbox);
    let runner = AgentRunner::new(
        llm,
        ToolRegistryHandle::new(registry),
        "You are the host's assistant.",
        config,
        Rc::new(Box::new(Clock)),
    )
    .unwrap();
    let mut session = Session::new(SessionId::new("host-tool"), 0, "host workspace");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    runtime
        .block_on(runner.run_turn(&mut session, "pin a note", &mut Silent, None))
        .unwrap();
    let kept = notes.borrow().clone();
    (session, kept, model)
}

/// Lends the scripted model to the runner while the test keeps a handle on it.
struct Shared(Rc<Scripted>);
impl LlmPort for Shared {
    fn model(&self) -> &str {
        self.0.model()
    }
    fn stream_chat(&self, request: ChatRequest) -> LlmStream {
        self.0.stream_chat(request)
    }
}

/// The result the session recorded for the one call.
fn recorded_result(session: &Session) -> (String, bool) {
    session
        .log()
        .events()
        .iter()
        .find_map(|event| match event {
            SessionEvent::ToolResult {
                content, is_error, ..
            } => Some((content.clone(), *is_error)),
            _ => None,
        })
        .expect("the call was answered")
}

#[test]
fn a_host_tool_is_offered_called_and_its_effect_lands_in_host_state() {
    let (session, notes, model) = run(
        json!({ "text": "check the build on Windows" }),
        SandboxMode::WorkspaceWrite,
    );
    assert!(
        model
            .offered
            .borrow()
            .iter()
            .any(|name| name == "host_note"),
        "the model was offered the tool: {:?}",
        model.offered.borrow()
    );
    assert_eq!(notes, vec!["check the build on Windows".to_owned()]);
    let (content, is_error) = recorded_result(&session);
    assert!(!is_error, "{content}");
    assert!(content.contains("\"pinned\":true"), "{content}");
}

#[test]
fn a_bad_argument_is_a_result_the_model_reads_and_nothing_changes() {
    let (session, notes, _) = run(json!({ "text": 7 }), SandboxMode::WorkspaceWrite);
    assert!(notes.is_empty(), "nothing was pinned");
    let (content, is_error) = recorded_result(&session);
    assert!(is_error);
    assert!(
        content.contains("text"),
        "the failure names the field: {content}"
    );
}

#[test]
fn a_write_tool_under_a_read_only_sandbox_is_denied_when_nobody_can_approve() {
    // `read_only` does not permit a `Write` call, and with no approver the gate fails closed.
    let (session, notes, _) = run(json!({ "text": "never pinned" }), SandboxMode::ReadOnly);
    assert!(notes.is_empty(), "a denied call does not run");
    let (content, is_error) = recorded_result(&session);
    assert!(is_error);
    assert!(content.contains("was not run"), "{content}");
}

#[test]
fn a_name_the_loop_reserves_cannot_be_registered() {
    let mut registry = ToolRegistry::new();
    let schema = ToolSchema {
        name: ToolName::new("create_goal").unwrap(),
        description: "shadows a goal tool".into(),
        parameters: json!({ "type": "object" }),
    };
    let refused = host_tools::register(
        &mut registry,
        ToolDefinition::new(
            schema,
            NoteExecutor {
                notes: Rc::default(),
            },
        ),
    );
    assert!(refused.is_err());
    assert!(registry.is_empty());
}
