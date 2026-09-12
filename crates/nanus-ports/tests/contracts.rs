//! Integration tests for the port contracts.
//!
//! A port is a trait, so these tests implement it. The implementations are
//! deliberately built *on the shared helpers* — [`occurrence_count`],
//! [`check_edit_count`], [`ensure_within`], [`SandboxPolicy::confine`] — because
//! that is how a real adapter is expected to satisfy the contract, and because a
//! fixture that reimplements the rule would only test itself.
//!
//! The three rules these pin down are the ones most easily implemented wrong:
//!
//! 1. an edit matches exactly once, or it is a typed error;
//! 2. a non-zero exit code is an answer, not a failure;
//! 3. a path outside the workspace root is refused.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use futures::StreamExt;
use nanus_domain::{ToolCallId, ToolName};
use nanus_ports::{
    Captured, ChatRequest, FsError, FsHandle, FsPort, FsResult, LlmEvent, LlmHandle, LlmPort,
    LlmStream, LocalBoxFuture, SandboxPolicy, ShellError, ShellEvent, ShellHandle, ShellOutcome,
    ShellPort, ShellRequest, ShellResult, ShellStream, ToolCallAssembler, WriteMode,
    check_edit_count, ensure_within, occurrence_count,
};

// ---------------------------------------------------------------------------
// An in-memory filesystem that enforces the port contract
// ---------------------------------------------------------------------------

/// A filesystem held entirely in memory.
///
/// It exists so the contract can be exercised without touching a real disk, and
/// so the tests can prove that a *correct* adapter is expressible: every rule it
/// enforces comes from a helper this crate exports rather than from the fixture.
struct MemoryFs {
    /// The workspace root every path is confined to.
    root: PathBuf,
    /// The files, keyed by their normalised absolute path.
    files: RefCell<BTreeMap<PathBuf, String>>,
}

impl MemoryFs {
    /// Builds an empty filesystem rooted at `/work`.
    fn new() -> Self {
        Self {
            root: PathBuf::from("/work"),
            files: RefCell::new(BTreeMap::new()),
        }
    }

    /// Inserts a file without going through the port.
    fn seed(&self, path: &str, text: &str) {
        self.files
            .borrow_mut()
            .insert(PathBuf::from(path), text.to_owned());
    }

    /// Reports an operation this fixture does not model.
    fn unsupported(path: &Path) -> FsError {
        FsError::Io {
            path: path.to_path_buf(),
            message: String::from("the in-memory fixture does not implement this operation"),
        }
    }
}

impl FsPort for MemoryFs {
    fn read<'a>(&'a self, path: &'a Path) -> LocalBoxFuture<'a, FsResult<nanus_ports::FileRead>> {
        Box::pin(async move {
            let resolved = ensure_within(&self.root, path)?;
            let files = self.files.borrow();
            let Some(text) = files.get(&resolved) else {
                return Err(FsError::NotFound { path: resolved });
            };
            Ok(nanus_ports::FileRead {
                path: resolved,
                total_lines: text.lines().count(),
                text: text.clone(),
            })
        })
    }

    fn write<'a>(
        &'a self,
        path: &'a Path,
        contents: &'a str,
        mode: WriteMode,
    ) -> LocalBoxFuture<'a, FsResult<nanus_ports::WriteOutcome>> {
        Box::pin(async move {
            let resolved = ensure_within(&self.root, path)?;
            let mut files = self.files.borrow_mut();
            let existed = files.contains_key(&resolved);
            if existed && mode == WriteMode::Create {
                return Err(FsError::AlreadyExists { path: resolved });
            }
            files.insert(resolved.clone(), contents.to_owned());
            Ok(nanus_ports::WriteOutcome {
                path: resolved,
                bytes_written: u64::try_from(contents.len()).unwrap_or(u64::MAX),
                created: !existed,
            })
        })
    }

    fn edit<'a>(
        &'a self,
        path: &'a Path,
        old: &'a str,
        new: &'a str,
        replace_all: bool,
    ) -> LocalBoxFuture<'a, FsResult<nanus_ports::EditOutcome>> {
        Box::pin(async move {
            let resolved = ensure_within(&self.root, path)?;
            let mut files = self.files.borrow_mut();
            let Some(before) = files.get(&resolved).cloned() else {
                return Err(FsError::NotFound { path: resolved });
            };
            // The rule, from the shared helper rather than from this fixture.
            let replacements =
                check_edit_count(&resolved, occurrence_count(&before, old), replace_all)?;
            let after = if replace_all {
                before.replace(old, new)
            } else {
                before.replacen(old, new, 1)
            };
            files.insert(resolved.clone(), after.clone());
            Ok(nanus_ports::EditOutcome {
                path: resolved,
                replacements,
                before,
                after,
            })
        })
    }

    fn exists<'a>(&'a self, path: &'a Path) -> LocalBoxFuture<'a, bool> {
        Box::pin(async move {
            let Ok(resolved) = ensure_within(&self.root, path) else {
                return false;
            };
            self.files.borrow().contains_key(&resolved)
        })
    }

    fn metadata<'a>(
        &'a self,
        path: &'a Path,
    ) -> LocalBoxFuture<'a, FsResult<nanus_ports::FileMeta>> {
        Box::pin(async move {
            let resolved = ensure_within(&self.root, path)?;
            let files = self.files.borrow();
            let Some(text) = files.get(&resolved) else {
                return Err(FsError::NotFound { path: resolved });
            };
            Ok(nanus_ports::FileMeta {
                path: resolved,
                is_file: true,
                is_dir: false,
                byte_len: u64::try_from(text.len()).unwrap_or(u64::MAX),
            })
        })
    }

    fn list<'a>(
        &'a self,
        dir: &'a Path,
    ) -> LocalBoxFuture<'a, FsResult<Vec<nanus_ports::DirEntry>>> {
        Box::pin(async move { Err(Self::unsupported(dir)) })
    }

    fn search<'a>(
        &'a self,
        query: &'a nanus_ports::SearchQuery,
    ) -> LocalBoxFuture<'a, FsResult<nanus_ports::SearchOutcome>> {
        Box::pin(async move { Err(Self::unsupported(&query.root)) })
    }

    fn canonicalize<'a>(&'a self, path: &'a Path) -> LocalBoxFuture<'a, FsResult<PathBuf>> {
        Box::pin(async move { ensure_within(&self.root, path) })
    }
}

// ---------------------------------------------------------------------------
// A shell that replays queued outcomes
// ---------------------------------------------------------------------------

/// A shell that answers from a queue, so an exit code can be tested in isolation.
struct ReplayShell {
    /// The sandbox this shell enforces.
    policy: SandboxPolicy,
    /// The outcomes to hand out, in order.
    queued: RefCell<Vec<ShellOutcome>>,
    /// How many process groups `kill_all` has been asked to reap.
    reaped: Cell<usize>,
}

impl ReplayShell {
    /// Builds a shell that will return `outcome` for the next run.
    fn new(outcome: ShellOutcome) -> Self {
        Self {
            policy: SandboxPolicy::workspace_write("/work"),
            queued: RefCell::new(vec![outcome]),
            reaped: Cell::new(0),
        }
    }

    /// Removes and returns the next queued outcome.
    fn next_outcome(&self) -> Option<ShellOutcome> {
        self.queued.borrow_mut().pop()
    }
}

impl ShellPort for ReplayShell {
    fn run(&self, request: ShellRequest) -> LocalBoxFuture<'_, ShellResult<ShellOutcome>> {
        Box::pin(async move {
            // The sandbox is consulted before anything runs, so a confined path
            // never reaches a process.
            if let Some(cwd) = &request.cwd {
                self.policy.confine(cwd)?;
            }
            match self.next_outcome() {
                Some(outcome) => Ok(outcome),
                None => Err(ShellError::Spawn {
                    program: request.program,
                    message: String::from("the replay queue is empty"),
                }),
            }
        })
    }

    fn spawn(&self, request: ShellRequest) -> LocalBoxFuture<'_, ShellResult<ShellStream>> {
        Box::pin(async move {
            let outcome = self.run(request).await?;
            let mut events: Vec<ShellEvent> = Vec::new();
            if !outcome.stdout.text.is_empty() {
                events.push(ShellEvent::Stdout {
                    chunk: outcome.stdout.text.clone(),
                });
            }
            if !outcome.stderr.text.is_empty() {
                events.push(ShellEvent::Stderr {
                    chunk: outcome.stderr.text.clone(),
                });
            }
            // Exactly one terminator, so a consumer always learns how it ended.
            events.push(ShellEvent::Exited {
                exit_code: outcome.exit_code,
                signal: outcome.signal,
                duration_ms: outcome.duration_ms,
                timed_out: outcome.timed_out,
            });
            let stream: ShellStream = Box::pin(futures::stream::iter(events));
            Ok(stream)
        })
    }

    fn kill_all(&self) -> LocalBoxFuture<'_, ShellResult<usize>> {
        Box::pin(async move {
            let count = self.reaped.get();
            self.reaped.set(count.saturating_add(1));
            Ok(count)
        })
    }

    fn sandbox(&self) -> SandboxPolicy {
        self.policy.clone()
    }
}

// ---------------------------------------------------------------------------
// A model adapter that emits a fixed script
// ---------------------------------------------------------------------------

/// A model adapter that emits a canned event script and nothing else.
struct ScriptedLlm {
    /// The events the stream will yield, in order.
    events: Vec<LlmEvent>,
}

impl LlmPort for ScriptedLlm {
    // `&'static str` rather than `&str`: the model name is a literal, so tying
    // it to the borrow of `self` would claim a dependency that is not there.
    fn model(&self) -> &'static str {
        "scripted-model"
    }

    fn stream_chat(&self, _request: ChatRequest) -> LlmStream {
        Box::pin(futures::stream::iter(self.events.clone()))
    }
}

// ---------------------------------------------------------------------------
// Rule 1: an edit matches exactly once
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_edit_with_no_match_is_a_typed_error_and_changes_nothing() {
    let fs = MemoryFs::new();
    fs.seed("/work/a.rs", "fn main() {}\n");
    let outcome = fs.edit(Path::new("a.rs"), "not present", "x", false).await;
    assert!(
        matches!(outcome, Err(FsError::EditNoMatch { .. })),
        "a silent no-op would tell the model its edit landed: {outcome:?}"
    );
    // Pair assertion: the file is untouched, so the failure cannot have been a
    // partial rewrite.
    let text = fs.read(Path::new("a.rs")).await;
    assert_eq!(
        text.map(|read| read.text).ok(),
        Some("fn main() {}\n".to_owned())
    );
}

#[tokio::test]
async fn an_ambiguous_edit_is_a_typed_error_unless_replace_all() {
    let fs = MemoryFs::new();
    fs.seed("/work/a.rs", "let x = 1;\nlet y = 1;\n");
    let ambiguous = fs.edit(Path::new("a.rs"), "= 1;", "= 2;", false).await;
    assert!(
        matches!(
            ambiguous,
            Err(FsError::EditMultipleMatches { count: 2, .. })
        ),
        "expected an ambiguous-match error, got {ambiguous:?}"
    );
    // Pair assertion: still untouched.
    let text = fs.read(Path::new("a.rs")).await;
    assert_eq!(
        text.map(|read| read.text).ok(),
        Some("let x = 1;\nlet y = 1;\n".to_owned())
    );

    let all = fs.edit(Path::new("a.rs"), "= 1;", "= 2;", true).await;
    assert!(all.is_ok(), "replace_all is the escape hatch: {all:?}");
    let Ok(all) = all else { return };
    assert_eq!(all.replacements, 2);
    assert_eq!(all.after, "let x = 2;\nlet y = 2;\n");
    assert!(all.unified_diff().contains("@@"));
}

#[tokio::test]
async fn an_edit_with_exactly_one_match_lands() {
    let fs = MemoryFs::new();
    fs.seed("/work/a.rs", "fn main() {}\n");
    let outcome = fs.edit(Path::new("a.rs"), "main", "entry", false).await;
    assert!(outcome.is_ok());
    let Ok(outcome) = outcome else { return };
    assert_eq!(outcome.replacements, 1);
    assert_eq!(outcome.before, "fn main() {}\n");
    assert_eq!(outcome.after, "fn entry() {}\n");
    assert!(!outcome.is_empty());
    assert!(outcome.unified_diff().contains("+fn entry() {}"));
}

#[tokio::test]
async fn an_empty_pattern_is_refused_rather_than_matching_everywhere() {
    let fs = MemoryFs::new();
    fs.seed("/work/a.rs", "abc");
    let outcome = fs.edit(Path::new("a.rs"), "", "x", true).await;
    assert!(matches!(outcome, Err(FsError::EditNoMatch { .. })));
}

// ---------------------------------------------------------------------------
// Rule 2: a non-zero exit code is not an error
// ---------------------------------------------------------------------------

/// Builds an outcome for a process that exited with `exit_code`.
fn exited(exit_code: Option<i32>) -> ShellOutcome {
    ShellOutcome {
        exit_code,
        signal: None,
        timed_out: false,
        duration_ms: 7,
        stdout: Captured {
            text: String::new(),
            truncated: false,
            total_bytes: 0,
        },
        stderr: Captured {
            text: String::new(),
            truncated: false,
            total_bytes: 0,
        },
    }
}

#[tokio::test]
async fn a_non_zero_exit_is_a_successful_call_with_an_unsuccessful_outcome() {
    let shell: ShellHandle = Rc::new(Box::new(ReplayShell::new(exited(Some(1)))));
    let request = ShellRequest::shell("grep -q nothing file", None);
    assert!(
        request.is_shell_wrapped(),
        "the bash tool goes through sh -c"
    );

    let result = shell.run(request).await;
    // The call itself succeeded: there was an answer.
    assert!(
        result.is_ok(),
        "exit 1 is an answer, not a transport failure"
    );
    let Ok(outcome) = result else { return };
    assert_eq!(outcome.exit_code, Some(1));
    assert!(
        !outcome.is_success(),
        "the command did not succeed, and the outcome says so"
    );
}

#[tokio::test]
async fn a_failure_to_start_is_the_only_kind_of_error() {
    // An empty queue models "the program never ran".
    let shell: ShellHandle = Rc::new(Box::new(ReplayShell {
        policy: SandboxPolicy::workspace_write("/work"),
        queued: RefCell::new(Vec::new()),
        reaped: Cell::new(0),
    }));
    let result = shell
        .run(ShellRequest::direct("does-not-exist", Vec::new()))
        .await;
    assert!(matches!(result, Err(ShellError::Spawn { .. })));
}

#[tokio::test]
async fn a_confined_cwd_is_refused_before_anything_runs() {
    let shell: ShellHandle = Rc::new(Box::new(ReplayShell::new(exited(Some(0)))));
    let request = ShellRequest::shell("pwd", Some(PathBuf::from("/etc")));
    let result = shell.run(request).await;
    assert!(
        matches!(result, Err(ShellError::OutsideWorkspace { .. })),
        "the sandbox is consulted first: {result:?}"
    );
}

#[tokio::test]
async fn a_spawned_stream_ends_with_exactly_one_exit_event() {
    let shell: ShellHandle = Rc::new(Box::new(ReplayShell::new(exited(Some(3)))));
    let spawned = shell.spawn(ShellRequest::shell("false", None)).await;
    assert!(spawned.is_ok());
    let Ok(mut stream) = spawned else { return };
    let mut exits = 0_u32;
    while let Some(event) = stream.next().await {
        if let ShellEvent::Exited { exit_code, .. } = event {
            assert_eq!(exit_code, Some(3), "a non-zero code reaches the consumer");
            exits = exits.saturating_add(1);
        }
    }
    assert_eq!(exits, 1, "the stream terminator is unambiguous");
}

#[tokio::test]
async fn reap_reports_what_it_stopped() {
    let shell: ShellHandle = Rc::new(Box::new(ReplayShell::new(exited(Some(0)))));
    let first = shell.kill_all().await;
    assert_eq!(first.ok(), Some(0), "nothing was running");
    let second = shell.kill_all().await;
    assert_eq!(second.ok(), Some(1), "the counter advances");
    assert_eq!(
        shell.sandbox().mode,
        nanus_ports::SandboxMode::WorkspaceWrite
    );
}

// ---------------------------------------------------------------------------
// Rule 3: paths outside the workspace are refused
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_filesystem_port_refuses_every_escape() {
    let memory = MemoryFs::new();
    memory.seed("/work/a.rs", "safe");
    let fs: FsHandle = Rc::new(Box::new(memory));
    for escape in [
        "../secret",
        "src/../../secret",
        "/etc/passwd",
        "/work/../etc/passwd",
        "/workshop/secret",
    ] {
        let outcome = fs.read(Path::new(escape)).await;
        assert!(
            matches!(outcome, Err(FsError::OutsideWorkspace { .. })),
            "{escape} must be refused, got {outcome:?}"
        );
        // A write is refused for the same reason, not only a read.
        let written = fs.write(Path::new(escape), "x", WriteMode::Overwrite).await;
        assert!(matches!(written, Err(FsError::OutsideWorkspace { .. })));
        assert!(!fs.exists(Path::new(escape)).await, "nothing escaped");
    }
    // Negative space: the same operations inside the root succeed, so the
    // refusals above are about the boundary and not about the fixture.
    assert!(fs.read(Path::new("a.rs")).await.is_ok());
    assert!(
        fs.write(Path::new("b.rs"), "x", WriteMode::Create)
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn a_write_in_create_mode_refuses_to_replace() {
    let fs = MemoryFs::new();
    let first = fs.write(Path::new("a.rs"), "one", WriteMode::Create).await;
    assert_eq!(first.map(|outcome| outcome.created).ok(), Some(true));
    let second = fs.write(Path::new("a.rs"), "two", WriteMode::Create).await;
    assert!(matches!(second, Err(FsError::AlreadyExists { .. })));
    let overwritten = fs
        .write(Path::new("a.rs"), "two", WriteMode::Overwrite)
        .await;
    assert_eq!(overwritten.map(|outcome| outcome.created).ok(), Some(false));
}

#[test]
fn the_boundary_helpers_are_usable_without_a_port() {
    // An adapter that owns its own path handling still gets the same rule.
    assert!(ensure_within(Path::new("/work"), Path::new("../x")).is_err());
    assert!(ensure_within(Path::new("/work"), Path::new("x")).is_ok());
    assert_eq!(occurrence_count("aXbXc", "X"), 2);
    assert!(check_edit_count(Path::new("/work/a"), 2, false).is_err());
    assert!(check_edit_count(Path::new("/work/a"), 2, true).is_ok());
    let confined = SandboxPolicy::workspace_write("/work");
    assert!(confined.confine(Path::new("src/lib.rs")).is_ok());
    assert!(confined.confine(Path::new("../../etc/passwd")).is_err());
}

// ---------------------------------------------------------------------------
// The ports are usable as trait objects, driven by a runtime
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_streamed_response_assembles_into_tool_calls() {
    let tool = ToolName::new("read");
    assert!(tool.is_ok());
    let Ok(tool) = tool else { return };
    let llm: LlmHandle = Rc::new(Box::new(ScriptedLlm {
        events: vec![
            LlmEvent::ReasoningDelta("I should ".to_owned()),
            LlmEvent::ReasoningDelta("read it".to_owned()),
            LlmEvent::ToolCallDelta {
                index: 0,
                id: Some(ToolCallId::new("call-1")),
                name: Some(tool.clone()),
                arguments_delta: "{\"pa".to_owned(),
            },
            LlmEvent::ToolCallDelta {
                index: 0,
                id: None,
                name: None,
                arguments_delta: "th\":\"a.rs\"}".to_owned(),
            },
            LlmEvent::Finished {
                reason: nanus_ports::FinishReason::ToolCalls,
            },
        ],
    }));
    assert_eq!(llm.model(), "scripted-model");

    let mut stream = llm.stream_chat(ChatRequest::new("scripted-model", Vec::new()));
    let mut assembler = ToolCallAssembler::new();
    let mut reasoning = String::new();
    let mut finish = None;
    while let Some(event) = stream.next().await {
        if let LlmEvent::ReasoningDelta(delta) = &event {
            reasoning.push_str(delta);
        }
        if let LlmEvent::Finished { reason } = &event {
            finish = Some(reason.clone());
        }
        let applied = assembler.apply_event(&event);
        assert!(applied.is_ok(), "the script is well formed: {applied:?}");
    }
    assert_eq!(reasoning, "I should read it");
    assert_eq!(
        finish
            .as_ref()
            .map(nanus_ports::FinishReason::expects_tool_calls),
        Some(true)
    );
    let calls = assembler.finish();
    assert!(calls.is_ok());
    let Ok(calls) = calls else { return };
    assert_eq!(calls.len(), 1);
    let Some(call) = calls.first() else { return };
    assert_eq!(call.id.as_str(), "call-1");
    assert_eq!(call.name, tool);
    assert_eq!(
        call.arguments.get("path"),
        Some(&serde_json::Value::from("a.rs"))
    );
}

#[tokio::test]
async fn a_malformed_script_is_reported_as_a_stream_failure() {
    let llm: LlmHandle = Rc::new(Box::new(ScriptedLlm {
        events: vec![
            LlmEvent::ToolCallDelta {
                index: 0,
                id: Some(ToolCallId::new("call-1")),
                name: ToolName::new("read").ok(),
                arguments_delta: "{\"path\":".to_owned(),
            },
            LlmEvent::Error("upstream closed the connection".to_owned()),
        ],
    }));
    let mut stream = llm.stream_chat(ChatRequest::new("m", Vec::new()));
    let mut assembler = ToolCallAssembler::new();
    let mut saw_error = false;
    while let Some(event) = stream.next().await {
        if matches!(event, LlmEvent::Error(_)) {
            saw_error = true;
        }
        let applied = assembler.apply_event(&event);
        assert!(applied.is_ok(), "the delta itself is well formed");
    }
    assert!(saw_error, "the transport failure reached the consumer");
    let calls = assembler.finish();
    assert!(
        calls.is_err(),
        "half-written arguments never reach a tool: {calls:?}"
    );
}
