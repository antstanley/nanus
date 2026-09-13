//! An end-to-end run: a turn against the real tools and the real filesystem.
//!
//! The loop's unit tests use a stub filesystem, which proves the loop's own logic but
//! not that the shipped toolset and the local adapters agree with it. This test closes
//! that gap: it composes the seven real tools over a temporary workspace, scripts a
//! model that reads and then writes, and asserts the *file on disk* changed.
//!
//! That is the difference between "the loop asked a tool to write" and "the file was
//! written", and only the second one is the product working.

// A panic in a test *is* the assertion, and a fixture with no sane default has nowhere
// else to put the failure. The workspace denies the lint for production code.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::cell::RefCell;
use std::rc::Rc;

use nanus_bundle::{AgentRunner, Progress, Silent};
use nanus_domain::{AgentConfig, Session, SessionId, ToolCallId, ToolName, ToolRegistry, Usage};
use nanus_ports::{ChatRequest, FinishReason, LlmEvent, LlmPort, LlmStream, SandboxPolicy};

/// A model that replays a script of event batches, one per request.
struct ScriptedModel {
    batches: RefCell<Vec<Vec<LlmEvent>>>,
    requests: RefCell<Vec<ChatRequest>>,
}

impl ScriptedModel {
    fn new(batches: Vec<Vec<LlmEvent>>) -> Rc<Self> {
        Rc::new(Self {
            batches: RefCell::new(batches),
            requests: RefCell::new(Vec::new()),
        })
    }

    /// Returns the requests the model was sent.
    ///
    /// A test that wants to assert on wire content needs the concrete type, not the
    /// erased port, which is why this is unaffected by the coercion at the call site.
    fn requests(&self) -> Vec<ChatRequest> {
        self.requests.borrow().clone()
    }
}

/// Coerces a concrete model handle into the port the runner takes.
///
/// `Rc<ScriptedModel>` cannot coerce to `Rc<Box<dyn LlmPort>>` implicitly, so the box
/// is built here. Keeping the concrete handle alive alongside it is what lets a test
/// read back what the model was sent.
fn as_port(model: &Rc<ScriptedModel>) -> Rc<Box<dyn LlmPort>> {
    Rc::new(Box::new(SharedModel {
        inner: Rc::clone(model),
    }))
}

/// Forwards port calls to a shared concrete model.
struct SharedModel {
    inner: Rc<ScriptedModel>,
}

impl LlmPort for SharedModel {
    fn model(&self) -> &str {
        LlmPort::model(&*self.inner)
    }

    fn stream_chat(&self, request: ChatRequest) -> LlmStream {
        self.inner.stream_chat(request)
    }
}

impl LlmPort for ScriptedModel {
    fn model(&self) -> &'static str {
        "scripted"
    }

    fn stream_chat(&self, request: ChatRequest) -> LlmStream {
        self.requests.borrow_mut().push(request);
        let events = {
            let mut batches = self.batches.borrow_mut();
            if batches.is_empty() {
                vec![LlmEvent::Finished {
                    reason: FinishReason::Stop,
                }]
            } else {
                batches.remove(0)
            }
        };
        Box::pin(futures::stream::iter(events))
    }
}

/// Builds the tool call delta pair for one call.
fn call(id: &str, name: &str, arguments: &str) -> Vec<LlmEvent> {
    vec![
        LlmEvent::ToolCallDelta {
            index: 0,
            id: Some(ToolCallId::new(id)),
            name: Some(
                ToolName::new(name).unwrap_or_else(|error| panic!("test tool {name}: {error}")),
            ),
            arguments_delta: arguments.to_owned(),
        },
        LlmEvent::Finished {
            reason: FinishReason::ToolCalls,
        },
    ]
}

/// Builds a settled text response.
fn answer(text: &str) -> Vec<LlmEvent> {
    vec![
        LlmEvent::TextDelta(text.to_owned()),
        LlmEvent::Usage(Usage::default()),
        LlmEvent::Finished {
            reason: FinishReason::Stop,
        },
    ]
}

/// Composes the shipped toolset over a temporary workspace.
fn workspace_tools(root: &std::path::Path) -> (Rc<ToolRegistry>, nanus_ports::ShellHandle) {
    let fs = nanus_adapter_local::LocalFs::new(root)
        .unwrap_or_else(|error| panic!("a temporary workspace is readable: {error}"))
        .handle();
    let shell = nanus_adapter_local::LocalShell::new(SandboxPolicy::new(
        nanus_domain::SandboxMode::WorkspaceWrite,
        root,
    ))
    .handle();
    let registry = nanus_bundle::build_toolset(&fs, &shell)
        .unwrap_or_else(|error| panic!("the shipped toolset builds: {error}"));
    (Rc::new(registry), shell)
}

fn config() -> AgentConfig {
    AgentConfig::new(8, 4, "scripted", 16_384)
        .unwrap_or_else(|error| panic!("the test configuration is valid: {error}"))
}

/// Runs one turn and returns the outcome.
async fn run(
    model: Rc<Box<dyn LlmPort>>,
    tools: Rc<ToolRegistry>,
    prompt: &str,
) -> nanus_bundle::RunOutcome {
    let runner = AgentRunner::new(model, tools, "you are a test", config())
        .unwrap_or_else(|error| panic!("the runner builds: {error}"));
    let mut session = Session::new(SessionId::new("e2e"), 0, "/tmp");
    runner
        .run_turn(&mut session, prompt, &mut Silent)
        .await
        .unwrap_or_else(|error| panic!("the turn completes: {error}"))
}

#[tokio::test]
async fn a_write_tool_call_changes_the_file_on_disk() {
    let dir = tempfile::tempdir().unwrap_or_else(|error| panic!("temp dir: {error}"));
    let root = dir.path();
    let (tools, _shell) = workspace_tools(root);

    let model = ScriptedModel::new(vec![
        call(
            "c1",
            "write",
            r#"{"file_path":"out.txt","content":"written\n","mode":"create"}"#,
        ),
        answer("done"),
    ]);
    let outcome = run(as_port(&model), tools, "write a file").await;
    assert_eq!(outcome.answer, "done");
    assert_eq!(outcome.steps, 2);

    // The assertion that matters: the bytes are on disk.
    let written = std::fs::read_to_string(root.join("out.txt"));
    assert!(written.is_ok(), "the tool created the file: {written:?}");
    let Ok(written) = written else {
        return;
    };
    assert_eq!(written, "written\n");
}

#[tokio::test]
async fn a_read_after_a_write_sees_the_new_content() {
    let dir = tempfile::tempdir().unwrap_or_else(|error| panic!("temp dir: {error}"));
    let root = dir.path();
    let (tools, _shell) = workspace_tools(root);

    let model = ScriptedModel::new(vec![
        call(
            "c1",
            "write",
            r#"{"file_path":"a.txt","content":"first","mode":"create"}"#,
        ),
        call("c2", "read", r#"{"file_path":"a.txt"}"#),
        answer("I read it"),
    ]);
    let outcome = run(as_port(&model), tools, "create then read").await;
    assert_eq!(outcome.answer, "I read it");
    assert_eq!(outcome.steps, 3);
    assert_eq!(
        std::fs::read_to_string(root.join("a.txt")).unwrap_or_default(),
        "first"
    );
}

#[tokio::test]
async fn an_edit_replaces_exactly_once() {
    let dir = tempfile::tempdir().unwrap_or_else(|error| panic!("temp dir: {error}"));
    let root = dir.path();
    std::fs::write(root.join("e.txt"), "alpha beta gamma\n")
        .unwrap_or_else(|error| panic!("seeding the file: {error}"));
    let (tools, _shell) = workspace_tools(root);

    let model = ScriptedModel::new(vec![
        call(
            "c1",
            "edit",
            r#"{"file_path":"e.txt","old_string":"beta","new_string":"BETA"}"#,
        ),
        answer("edited"),
    ]);
    let outcome = run(as_port(&model), tools, "edit the file").await;
    assert_eq!(outcome.answer, "edited");
    assert_eq!(
        std::fs::read_to_string(root.join("e.txt")).unwrap_or_default(),
        "alpha BETA gamma\n"
    );
}

#[tokio::test]
async fn an_ambiguous_edit_is_refused_and_the_file_is_untouched() {
    let dir = tempfile::tempdir().unwrap_or_else(|error| panic!("temp dir: {error}"));
    let root = dir.path();
    // The pattern occurs twice, so the exactly-once rule must refuse it.
    std::fs::write(root.join("dup.txt"), "same\nsame\n")
        .unwrap_or_else(|error| panic!("seeding the file: {error}"));
    let (tools, _shell) = workspace_tools(root);

    let model = ScriptedModel::new(vec![
        call(
            "c1",
            "edit",
            r#"{"file_path":"dup.txt","old_string":"same","new_string":"other"}"#,
        ),
        answer("refused"),
    ]);
    let outcome = run(as_port(&model), tools, "edit ambiguously").await;
    // The model is told, and the loop continues rather than failing.
    assert_eq!(outcome.answer, "refused");
    assert_eq!(
        std::fs::read_to_string(root.join("dup.txt")).unwrap_or_default(),
        "same\nsame\n",
        "an ambiguous edit must not change anything"
    );
}

#[tokio::test]
async fn a_shell_command_runs_and_a_non_zero_exit_is_not_a_failure() {
    let dir = tempfile::tempdir().unwrap_or_else(|error| panic!("temp dir: {error}"));
    let root = dir.path();
    let (tools, shell) = workspace_tools(root);

    let model = ScriptedModel::new(vec![
        // Exits 3 on purpose: the tool must report it, not fail the step.
        call("c1", "bash", r#"{"command":"exit 3"}"#),
        answer("it exited three"),
    ]);
    let outcome = run(as_port(&model), tools.clone(), "run a failing command").await;
    assert_eq!(outcome.answer, "it exited three");
    assert_eq!(outcome.steps, 2);

    // Pair assertion: a succeeding command also runs.
    let model = ScriptedModel::new(vec![
        call("c1", "bash", r#"{"command":"echo hello-from-shell"}"#),
        answer("done"),
    ]);
    let outcome = run(as_port(&model), tools, "run a passing command").await;
    assert_eq!(outcome.answer, "done");
    assert_eq!(outcome.steps, 2);
    // Nothing is left running, which is what `kill_all` is for.
    let reaped = shell.kill_all().await;
    assert!(reaped.is_ok());
}

#[tokio::test]
async fn a_search_finds_a_file_that_was_just_written() {
    let dir = tempfile::tempdir().unwrap_or_else(|error| panic!("temp dir: {error}"));
    let root = dir.path();
    let (tools, _shell) = workspace_tools(root);

    let model = ScriptedModel::new(vec![
        call(
            "c1",
            "write",
            r#"{"file_path":"needle.txt","content":"haystack","mode":"create"}"#,
        ),
        call("c2", "glob", r#"{"pattern":"**/*.txt"}"#),
        answer("found it"),
    ]);
    let outcome = run(as_port(&model), tools, "search").await;
    assert_eq!(outcome.answer, "found it");
    assert_eq!(outcome.steps, 3);
}

#[tokio::test]
async fn a_write_outside_the_workspace_is_refused() {
    let dir = tempfile::tempdir().unwrap_or_else(|error| panic!("temp dir: {error}"));
    let root = dir.path();
    let (tools, _shell) = workspace_tools(root);

    let model = ScriptedModel::new(vec![
        // An absolute path outside the root: the adapter must refuse it.
        call(
            "c1",
            "write",
            r#"{"file_path":"/tmp/nanus-escape-probe.txt","content":"x","mode":"create"}"#,
        ),
        answer("refused"),
    ]);
    let outcome = run(as_port(&model), tools, "escape the workspace").await;
    assert_eq!(outcome.answer, "refused");
    assert_eq!(outcome.steps, 2);
    // The file must not exist anywhere.
    assert!(!std::path::Path::new("/tmp/nanus-escape-probe.txt").exists());
}

#[tokio::test]
async fn the_request_carries_the_prompt_and_the_tool_schemas() {
    let dir = tempfile::tempdir().unwrap_or_else(|error| panic!("temp dir: {error}"));
    let (tools, _shell) = workspace_tools(dir.path());

    let model = ScriptedModel::new(vec![answer("ok")]);
    let outcome = run(as_port(&model), tools, "a question").await;
    assert_eq!(outcome.answer, "ok");

    let requests = model.requests();
    assert_eq!(requests.len(), 1, "one step means one request");
    let request = &requests[0];
    // The prompt is the first message, and the seven schemas ride along.
    assert_eq!(
        request.messages.first().map(nanus_domain::Message::role),
        Some(nanus_domain::Role::System)
    );
    assert_eq!(
        request.tools.len(),
        7,
        "the shipped toolset reaches the model"
    );
    let mut names: Vec<String> = request
        .tools
        .iter()
        .map(|schema| schema.name.as_str().to_owned())
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            "bash",
            "edit",
            "glob",
            "grep",
            "read",
            "read_image",
            "write"
        ]
    );
}

#[tokio::test]
async fn progress_reports_every_step_and_tool() {
    /// Records what the loop reported.
    #[derive(Default)]
    struct Recorder {
        steps: Vec<u32>,
        tools: Vec<String>,
        text: String,
    }

    impl Progress for Recorder {
        fn text(&mut self, delta: &str) {
            self.text.push_str(delta);
        }

        fn step_started(&mut self, step: u32) {
            self.steps.push(step);
        }

        fn tool_started(&mut self, name: &ToolName, arguments: &serde_json::Value) {
            // The path is what makes a tool call readable, so it is part of what the loop
            // is expected to report rather than something a listener has to look up.
            self.tools.push(format!(
                "{} {}",
                name.as_str(),
                arguments
                    .get("file_path")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
            ));
        }
    }

    let dir = tempfile::tempdir().unwrap_or_else(|error| panic!("temp dir: {error}"));
    let (tools, _shell) = workspace_tools(dir.path());
    let model = ScriptedModel::new(vec![
        call(
            "c1",
            "write",
            r#"{"file_path":"p.txt","content":"x","mode":"create"}"#,
        ),
        answer("finished"),
    ]);
    let runner = AgentRunner::new(as_port(&model), tools, "prompt", config())
        .unwrap_or_else(|error| panic!("the runner builds: {error}"));
    let mut session = Session::new(SessionId::new("progress"), 0, "/tmp");
    let mut recorder = Recorder::default();
    let outcome = runner.run_turn(&mut session, "go", &mut recorder).await;
    assert!(outcome.is_ok());
    assert_eq!(recorder.steps, vec![1, 2]);
    // The reported call names the file it wrote, not only the tool, because that is what
    // an interface has to show to be worth reading.
    assert_eq!(recorder.tools, vec!["write p.txt".to_owned()]);
    assert_eq!(recorder.text, "finished");
}

#[tokio::test]
async fn the_glob_tool_anchors_a_bare_pattern_to_the_workspace_root() {
    // The model-facing contract for `glob`. The adapter has its own anchoring tests; this
    // one pins what the *tool* does, because a model's complaint ("the pattern is not
    // anchored to one directory level") is about the tool it called.
    let dir = tempfile::tempdir().unwrap_or_else(|error| panic!("temp dir: {error}"));
    let root = dir.path();
    std::fs::create_dir_all(root.join("crates/one/src"))
        .unwrap_or_else(|error| panic!("mkdir: {error}"));
    std::fs::write(root.join("top.rs"), "// top").unwrap_or_else(|error| panic!("seed: {error}"));
    std::fs::write(root.join("crates/one/src/lib.rs"), "// deep")
        .unwrap_or_else(|error| panic!("seed: {error}"));
    let (tools, _shell) = workspace_tools(root);

    // A bare pattern means the root level and nothing deeper.
    let call = nanus_domain::ToolCall::new(
        ToolCallId::new("c1"),
        ToolName::new("glob").unwrap_or_else(|error| panic!("glob is valid: {error}")),
        serde_json::json!({ "pattern": "*.rs" }),
    );
    let result = tools.execute(call).await;
    let text = result
        .outcome
        .content()
        .iter()
        .filter_map(|block| match block {
            nanus_domain::ContentBlock::Text(text) => Some(text.clone()),
            nanus_domain::ContentBlock::Image { .. } => None,
        })
        .collect::<String>();
    assert!(
        text.contains("top.rs"),
        "a bare pattern finds the root file: {text}"
    );
    assert!(
        !text.contains("lib.rs"),
        "and does not reach into subdirectories: {text}"
    );

    // The recursive form is how a caller asks for every depth.
    let call = nanus_domain::ToolCall::new(
        ToolCallId::new("c2"),
        ToolName::new("glob").unwrap_or_else(|error| panic!("glob is valid: {error}")),
        serde_json::json!({ "pattern": "**/*.rs" }),
    );
    let result = tools.execute(call).await;
    let text = result
        .outcome
        .content()
        .iter()
        .filter_map(|block| match block {
            nanus_domain::ContentBlock::Text(text) => Some(text.clone()),
            nanus_domain::ContentBlock::Image { .. } => None,
        })
        .collect::<String>();
    assert!(
        text.contains("top.rs"),
        "the recursive pattern finds the root: {text}"
    );
    assert!(
        text.contains("lib.rs"),
        "and reaches the nested file: {text}"
    );
}
