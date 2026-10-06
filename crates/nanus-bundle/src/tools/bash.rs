//! The `bash` tool.
//!
//! Running a program is the most consequential thing the harness can do, so this
//! tool is deliberate about three things.
//!
//! **A non-zero exit code is not an error.** The command ran and reported what it
//! thought. The tool returns a successful outcome carrying the exit code, which is
//! what lets a model see `grep` finding nothing (exit 1) and react sensibly instead
//! of being told the call failed.
//!
//! **Output is bounded, and the model is told when it was cut.** A command that
//! writes a gigabyte must not enter the transcript, and a model given a silent
//! truncation will reason about output it never saw — so the tail of the rendering
//! states what was omitted.
//!
//! **Every failure is model-visible.** "The command never ran" is information, not a
//! harness error: the model needs it in order to try something else.
//!
//! **What the cap cut can be kept.** Built with a [`CaptureBroker`], the tool looks for a
//! capture lease the runner filed for the call. When there is one, the command runs through
//! [`ShellPort::run_with_capture`](nanus_ports::ShellPort::run_with_capture), each stream's exact
//! bytes are archived beside the preview, what the sinks finalized to is filed back with the
//! broker, and the rendering gains one bounded line per archived stream saying where the whole
//! of it is. Capture never changes what runs or how it ends: a shell that cannot capture runs the
//! command as usual and files nothing, and with no lease the rendering is byte-for-byte what it
//! was before capture existed.

use core::fmt::Write as _;
use core::time::Duration;

use nanus_domain::context::managed::{
    ArtifactReceipt, CaptureReason, CaptureStatus, CaptureStream, RECALL_TOOL, RawEncoding,
};
use nanus_domain::{
    ContentBlock, ToolAccess, ToolCall, ToolCallId, ToolDefinition, ToolExecutor, ToolFuture,
    ToolName, ToolOutcome, ToolResult, ToolSchema,
};
use nanus_ports::{
    CaptureFinalization, CaptureLease, Captured, CapturedOutcome, ShellCapture, ShellError,
    ShellHandle, ShellOutcome, ShellRequest,
};
use serde_json::json;

use crate::args::Arguments;
use crate::capture::CaptureBroker;
use crate::tools::port_error_result;

/// The default per-command time limit.
pub const DEFAULT_TIMEOUT_MS: u64 = 120_000;

/// The largest per-stream output the tool will return.
pub const MAX_OUTPUT_BYTES: usize = 65_536;

/// Builds the `bash` tool over `shell`.
pub fn bash_tool(shell: ShellHandle) -> ToolDefinition {
    definition(shell, None)
}

/// Builds the `bash` tool over `shell`, archiving a call's streams when `broker` holds a capture
/// lease for that call.
///
/// The schema is the same as [`bash_tool`]'s: capture is the host's decision, not an argument the
/// model can pass. A call with no lease filed runs and renders exactly as `bash_tool` would.
pub fn bash_tool_with_capture(shell: ShellHandle, broker: CaptureBroker) -> ToolDefinition {
    definition(shell, Some(broker))
}

/// Builds the definition both constructors share.
fn definition(shell: ShellHandle, capture: Option<CaptureBroker>) -> ToolDefinition {
    let schema = ToolSchema {
        name: ToolName::new("bash").unwrap_or_else(|_| unreachable!("bash is a valid tool name")),
        description: "Run a shell command in the workspace and return its output. A non-zero \
                      exit code is reported as part of the result, not as a failure of the \
                      call: check `exit_code` to see whether the command succeeded."
            .to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The shell command to run."
                },
                "workdir": {
                    "type": "string",
                    "description": "Directory to run in, relative to the workspace root. \
                                    Defaults to the workspace root."
                },
                "timeout_ms": {
                    "type": "integer",
                    "description": format!(
                        "How long to allow, in milliseconds. Defaults to {DEFAULT_TIMEOUT_MS}."
                    )
                }
            },
            "required": ["command"],
            "additionalProperties": false
        }),
    };
    ToolDefinition::new(schema, BashExecutor { shell, capture }).with_access(ToolAccess::Execute)
}

/// Executes `bash`.
struct BashExecutor {
    shell: ShellHandle,
    /// Where the runner files capture leases, when this tool was built to archive.
    capture: Option<CaptureBroker>,
}

impl ToolExecutor for BashExecutor {
    fn execute(&self, call: ToolCall) -> ToolFuture {
        let shell = std::rc::Rc::clone(&self.shell);
        let capture = self.capture.clone();
        Box::pin(async move { bash_outcome(shell, capture, call).await })
    }
}

/// Runs a command and renders it for the model.
async fn bash_outcome(
    shell: ShellHandle,
    capture: Option<CaptureBroker>,
    call: ToolCall,
) -> ToolResult {
    let id = call.id.clone();
    let arguments = Arguments::new("bash", &call.arguments);
    let command = match arguments.required_str("command") {
        Ok(command) => command,
        Err(failure) => return ToolResult::new(id, failure),
    };
    if command.trim().is_empty() {
        return ToolResult::new(id, ToolOutcome::failure("bash: the command is empty"));
    }
    // A field that is present and wrongly typed is a correction the model can act on, so it
    // is reported rather than folded into the default: `"timeout_ms": "fast"` silently
    // meaning two minutes is how a model learns nothing from its own mistake.
    let workdir = match arguments.optional_str("workdir") {
        Ok(workdir) => workdir,
        Err(failure) => return ToolResult::new(id, failure),
    };
    let timeout_ms = match arguments.optional_u32("timeout_ms") {
        Ok(timeout) => timeout.map_or(DEFAULT_TIMEOUT_MS, u64::from),
        Err(failure) => return ToolResult::new(id, failure),
    };
    // The schema documents the default as the workspace root, and the process's own
    // directory is not it: a run started elsewhere, or a service, would otherwise execute
    // the command somewhere the model was told it would not. The root comes from the port,
    // so the directory the command runs in and the directory the sandbox reasons about are
    // the same one.
    let workspace_root = shell.sandbox().workspace_root;
    let cwd = workdir.map_or(workspace_root, std::path::PathBuf::from);

    let request = ShellRequest {
        timeout: Some(Duration::from_millis(timeout_ms)),
        max_output_bytes: MAX_OUTPUT_BYTES,
        ..ShellRequest::shell(command, Some(cwd))
    };
    let lease = capture.as_ref().and_then(|broker| broker.take_lease(&id));
    let ran = match (lease, capture.as_ref()) {
        (Some(lease), Some(broker)) => run_captured(&shell, request, lease, broker).await,
        _ => shell
            .run(request)
            .await
            .map(|outcome| (outcome, Vec::new())),
    };
    let (outcome, receipts) = match ran {
        Ok(ran) => ran,
        Err(error) => return port_error_result(id, "bash", &error),
    };

    let rendered = render_archived(&outcome, &receipts);
    let value = json!({
        "exit_code": outcome.exit_code,
        "signal": outcome.signal,
        "timed_out": outcome.timed_out,
        "duration_ms": outcome.duration_ms,
        "stdout_bytes": outcome.stdout.total_bytes,
        "stderr_bytes": outcome.stderr.total_bytes,
    });
    ToolResult::new(
        call_id_of(&call),
        ToolOutcome::success_with(value, vec![ContentBlock::Text(rendered)]),
    )
}

/// Returns the call's id, cloned for the result.
fn call_id_of(call: &ToolCall) -> ToolCallId {
    call.id.clone()
}

/// Runs a command through the lease's sinks, files what they finalized to, and returns the
/// outcome with the receipts to render.
///
/// The lease is held until the shell has returned, so the reservation outlives every sink it
/// handed out. A shell that cannot capture says so before it runs anything, so the command then
/// runs once, uncaptured, and nothing is filed: an archive that never existed has no receipt, and
/// the runner, finding none, reports the archive unavailable for the call.
async fn run_captured(
    shell: &ShellHandle,
    request: ShellRequest,
    mut lease: CaptureLease,
    broker: &CaptureBroker,
) -> Result<(ShellOutcome, Vec<ArtifactReceipt>), ShellError> {
    let call = lease.call_id().clone();
    let capture = ShellCapture {
        stdout: lease.take_stdout(),
        stderr: lease.take_stderr(),
    };
    let given = [capture.stdout.is_some(), capture.stderr.is_some()];
    let captured = match shell.run_with_capture(request.clone(), capture).await {
        Ok(captured) => captured,
        Err(ShellError::CaptureUnsupported) => {
            return shell
                .run(request)
                .await
                .map(|outcome| (outcome, Vec::new()));
        }
        Err(error) => return Err(error),
    };
    let (outcome, finalizations) = settle(&call, given, captured);
    let receipts = finalizations
        .iter()
        .map(|finalization| finalization.receipt.clone())
        .collect();
    broker.record(&call, finalizations);
    drop(lease);
    Ok((outcome, receipts))
}

/// Pairs each stream that was given a sink with what it finalized to.
///
/// A stream whose sink was given but whose finalization did not arrive is *uncertain*: an
/// overdue write or finalize was still running when the shell stopped waiting. It is filed as
/// unavailable, because nothing may be published from a sink whose outcome nobody saw; the store
/// keeps whatever that sink leaves as an unreferenced orphan.
fn settle(
    call: &ToolCallId,
    given: [bool; 2],
    captured: CapturedOutcome,
) -> (ShellOutcome, Vec<CaptureFinalization>) {
    let CapturedOutcome {
        outcome,
        stdout,
        stderr,
    } = captured;
    let streams = [
        (CaptureStream::Stdout, stdout, outcome.stdout.total_bytes),
        (CaptureStream::Stderr, stderr, outcome.stderr.total_bytes),
    ];
    let finalizations = given
        .into_iter()
        .zip(streams)
        .filter(|(given, _)| *given)
        .map(|(_, (stream, finalized, observed))| {
            finalized.unwrap_or_else(|| uncertain(call, stream, observed))
        })
        .collect();
    (outcome, finalizations)
}

/// The receipt of a stream whose archive's outcome is not known.
fn uncertain(call: &ToolCallId, stream: CaptureStream, observed: u64) -> CaptureFinalization {
    let receipt = ArtifactReceipt {
        artifact_id: None,
        call_id: call.as_str().to_owned(),
        stream,
        retained_bytes: 0,
        observed_bytes: observed,
        retained_blake3: None,
        status: CaptureStatus::Unavailable,
        reason: CaptureReason::Timeout,
        encoding: RawEncoding::Raw,
        chunk_blake3: Vec::new(),
    };
    CaptureFinalization {
        receipt,
        artifact: None,
    }
}

/// Renders a shell outcome the way a terminal would, plus the facts a terminal omits.
///
/// The order is deliberately stdout, then stderr, then the exit status: a model
/// reading the tail learns how the command ended, which is the thing it most often
/// needs. Each archived stream gains one line after its own text and truncation notice, so
/// the exit status is still the last thing said; with no receipts the rendering is exactly what
/// it was before the archive existed.
pub fn render_archived(outcome: &ShellOutcome, receipts: &[ArtifactReceipt]) -> String {
    let receipt_for = |stream: CaptureStream| receipts.iter().find(|r| r.stream == stream);
    let mut rendered = String::new();
    let stdout = receipt_for(CaptureStream::Stdout);
    let stderr = receipt_for(CaptureStream::Stderr);
    append_stream(&mut rendered, "stdout", &outcome.stdout, stdout);
    append_stream(&mut rendered, "stderr", &outcome.stderr, stderr);
    if rendered.is_empty() {
        rendered.push_str("(no output)\n");
    }
    if outcome.timed_out {
        rendered.push_str("[timed out]\n");
    }
    if let Some(signal) = outcome.signal {
        let _ = writeln!(rendered, "[killed by signal {signal}]");
    }
    match outcome.exit_code {
        // The exit code is stated even when it is zero, because "the command said
        // nothing and succeeded" and "the command said nothing and failed" are
        // different facts.
        Some(code) => {
            let _ = writeln!(rendered, "[exit code: {code}]");
        }
        None => rendered.push_str("[exit code: none; the process was killed]\n"),
    }
    rendered
}

/// Appends one captured stream, labelled, when it has content, and its archive line when it was
/// archived.
///
/// A stream the archive saw bytes of is drawn even when the preview kept none — a pipe whose
/// drain was abandoned has an empty preview and may still have an archive.
fn append_stream(
    rendered: &mut String,
    label: &str,
    captured: &Captured,
    receipt: Option<&ArtifactReceipt>,
) {
    let archived = receipt.filter(|receipt| receipt.observed_bytes > 0);
    if captured.text.is_empty() && captured.total_bytes == 0 && archived.is_none() {
        return;
    }
    let _ = writeln!(rendered, "[{label}]");
    rendered.push_str(&captured.text);
    if !captured.text.ends_with('\n') {
        rendered.push('\n');
    }
    if captured.truncated {
        // The model must know it is reading a prefix, or it will reason about output
        // that was cut.
        let _ = writeln!(
            rendered,
            "[{label} truncated: {} bytes total, showing the first {}]",
            captured.total_bytes,
            captured.text.len()
        );
    }
    if let Some(receipt) = archived {
        append_archive_line(rendered, label, receipt);
    }
}

/// Appends the one line that tells the model where a stream's whole output is, or why it is
/// not anywhere.
///
/// The line is bounded: an artifact id has a fixed length, and the rest is two counts and a
/// fixed vocabulary.
fn append_archive_line(rendered: &mut String, label: &str, receipt: &ArtifactReceipt) {
    let start = rendered.len();
    let retained = receipt.retained_bytes;
    let observed = receipt.observed_bytes;
    match (&receipt.artifact_id, receipt.status) {
        (Some(id), CaptureStatus::Complete) => {
            let _ = writeln!(
                rendered,
                "[{label} archived as {id} — {retained} of {observed} bytes, complete; \
                 read it with {RECALL_TOOL}]"
            );
        }
        (Some(id), CaptureStatus::Partial) => {
            let _ = writeln!(
                rendered,
                "[{label} archived as {id} — {retained} of {observed} bytes, partial: {}; \
                 read it with {RECALL_TOOL}]",
                reason_text(receipt.reason)
            );
        }
        _ => {
            let _ = writeln!(
                rendered,
                "[{label} not archived: {}]",
                reason_text(receipt.reason)
            );
        }
    }
    assert!(
        rendered.len().saturating_sub(start) <= ARCHIVE_LINE_MAX,
        "an archive line is bounded"
    );
}

/// The longest an archive line can be, in bytes.
const ARCHIVE_LINE_MAX: usize = 256;

/// Says why a capture ended, in words a model can act on.
const fn reason_text(reason: CaptureReason) -> &'static str {
    match reason {
        CaptureReason::Eof => "end of output",
        CaptureReason::Quota => "the archive quota was reached",
        CaptureReason::WriteError => "the archive could not be written",
        CaptureReason::ReadError => "the output could not be read",
        CaptureReason::Timeout => "the archive did not keep up",
        CaptureReason::Cancelled => "the command was cut off",
        CaptureReason::DrainExpired => "the output did not end",
        CaptureReason::Unsupported => "this shell cannot archive",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rendering of a call that archived nothing.
    fn render_outcome(outcome: &ShellOutcome) -> String {
        render_archived(outcome, &[])
    }

    /// Builds an outcome with the given streams and status.
    fn outcome(
        stdout: &str,
        stderr: &str,
        exit_code: Option<i32>,
        timed_out: bool,
        truncated: bool,
    ) -> ShellOutcome {
        let make = |text: &str| Captured {
            text: text.to_owned(),
            truncated,
            total_bytes: u64::try_from(text.len().saturating_mul(3)).unwrap_or(0),
        };
        ShellOutcome {
            exit_code,
            signal: None,
            timed_out,
            duration_ms: 5,
            stdout: make(stdout),
            stderr: make(stderr),
        }
    }

    #[test]
    fn a_successful_command_states_its_exit_code() {
        let rendered = render_outcome(&outcome("hello\n", "", Some(0), false, false));
        assert!(rendered.contains("[stdout]"));
        assert!(rendered.contains("hello"));
        // A zero exit code is stated rather than implied.
        assert!(rendered.contains("[exit code: 0]"), "{rendered}");
    }

    #[test]
    fn a_failing_command_is_still_a_rendered_result() {
        let rendered = render_outcome(&outcome("", "boom\n", Some(1), false, false));
        assert!(rendered.contains("[stderr]"));
        assert!(rendered.contains("boom"));
        assert!(rendered.contains("[exit code: 1]"), "{rendered}");
    }

    #[test]
    fn a_silent_command_says_so() {
        let rendered = render_outcome(&outcome("", "", Some(0), false, false));
        // An empty rendering would look like a tool failure.
        assert!(rendered.contains("(no output)"), "{rendered}");
        assert!(rendered.contains("[exit code: 0]"));
    }

    #[test]
    fn a_timeout_and_a_signal_are_reported() {
        let timed = render_outcome(&outcome("partial", "", None, true, false));
        assert!(timed.contains("[timed out]"), "{timed}");
        assert!(timed.contains("partial"));
        // A killed process has no exit code, and saying so is clearer than omitting it.
        assert!(timed.contains("none"), "{timed}");

        let mut signalled = outcome("", "", None, false, false);
        signalled.signal = Some(9);
        let rendered = render_outcome(&signalled);
        assert!(rendered.contains("signal 9"), "{rendered}");
    }

    #[test]
    fn a_truncated_stream_is_labelled_with_its_real_size() {
        let rendered = render_outcome(&outcome("abcdef", "", Some(0), false, true));
        assert!(rendered.contains("truncated"), "{rendered}");
        assert!(rendered.contains("bytes total"), "{rendered}");
    }

    #[test]
    fn an_unterminated_line_still_ends_with_a_newline() {
        let rendered = render_outcome(&outcome("no trailing newline", "", Some(0), false, false));
        // The status line must not run into the output.
        assert!(rendered.contains("newline\n[exit code: 0]"), "{rendered}");
    }

    /// A wrongly typed optional argument is refused, not folded into its default.
    ///
    /// `Arguments` exists to turn a model's malformed call into a correction it can read;
    /// every tool used to discard that correction with `unwrap_or(None)`, so `"timeout_ms":
    /// "fast"` quietly meant two minutes and the model learned nothing.
    #[tokio::test]
    async fn a_wrongly_typed_timeout_is_reported_rather_than_defaulted() {
        let port: Box<dyn nanus_ports::ShellPort> = Box::new(crate::tests_support::UnusedShell);
        let shell: ShellHandle = std::rc::Rc::new(port);
        let tool = bash_tool(shell);
        let result = tool
            .execute(ToolCall::new(
                ToolCallId::new("c1"),
                ToolName::new("bash").unwrap_or_else(|_| unreachable!("bash is valid")),
                json!({ "command": "true", "timeout_ms": "fast" }),
            ))
            .await;
        let ToolOutcome::Failure { message, .. } = &result.outcome else {
            panic!("a wrong type is a failure: {:?}", result.outcome);
        };
        assert!(message.contains("timeout_ms"), "{message}");
        assert!(
            !message.contains("never run"),
            "the call is refused before the port is reached: {message}"
        );

        // Pair assertion: the same call without the field reaches the port, so the refusal
        // above is the argument's type rather than the call.
        let reached = tool
            .execute(ToolCall::new(
                ToolCallId::new("c2"),
                ToolName::new("bash").unwrap_or_else(|_| unreachable!("bash is valid")),
                json!({ "command": "true" }),
            ))
            .await;
        let ToolOutcome::Failure { message, .. } = &reached.outcome else {
            panic!("the stub port cannot run anything");
        };
        assert!(message.contains("never run"), "{message}");
        assert!(!message.contains("timeout_ms"), "{message}");
    }

    #[test]
    fn the_default_timeout_is_stated_in_the_schema() {
        // The schema text is what the model reads, so the default it mentions must be
        // the default the code uses.
        assert_eq!(DEFAULT_TIMEOUT_MS, 120_000);
        let port: Box<dyn nanus_ports::ShellPort> = Box::new(crate::tests_support::UnusedShell);
        let shell: ShellHandle = std::rc::Rc::new(port);
        let definition = bash_tool(shell);
        let parameters = definition.schema().parameters.to_string();
        assert!(parameters.contains("120000"), "{parameters}");
    }

    /// The archive path: a broker, a lease, and a shell that can or cannot capture.
    mod archiving {
        use super::*;
        use core::cell::Cell;
        use nanus_domain::context::managed::{ArtifactId, Digest};
        use nanus_ports::{
            CaptureFailure, LocalBoxFuture, RawCaptureSink, SandboxPolicy, SendBoxFuture,
            ShellPort, ShellResult, ShellStream,
        };
        use std::rc::Rc;

        /// What every scripted shell reports: output on both streams and a failing exit.
        fn scripted() -> ShellOutcome {
            let mut ran = outcome("built\n", "warning: unused\n", Some(2), false, false);
            ran.stdout.total_bytes = 6;
            ran.stderr.total_bytes = 16;
            ran
        }

        /// The exact text the tool rendered for the scripted outcome before capture existed.
        const LEGACY: &str = "[stdout]\nbuilt\n[stderr]\nwarning: unused\n[exit code: 2]\n";

        /// A sink that keeps its bytes and finalizes to a well-formed complete receipt.
        struct Keeping {
            stream: CaptureStream,
            bytes: Vec<u8>,
        }

        impl RawCaptureSink for Keeping {
            fn write<'a>(
                &'a mut self,
                bytes: &'a [u8],
            ) -> SendBoxFuture<'a, Result<(), CaptureFailure>> {
                self.bytes.extend_from_slice(bytes);
                Box::pin(async { Ok(()) })
            }

            fn finalize(
                self: Box<Self>,
                observed: u64,
                reason: CaptureReason,
            ) -> SendBoxFuture<'static, CaptureFinalization> {
                let id = match self.stream {
                    CaptureStream::Stdout => "a:00000000-0000-4000-8000-000000000001",
                    CaptureStream::Stderr => "a:00000000-0000-4000-8000-000000000002",
                };
                let retained = self.bytes.len() as u64;
                let receipt = ArtifactReceipt {
                    artifact_id: ArtifactId::parse(id),
                    call_id: "c1".to_owned(),
                    stream: self.stream,
                    retained_bytes: retained,
                    observed_bytes: observed,
                    retained_blake3: Some(Digest::of(&self.bytes)),
                    status: if reason == CaptureReason::Eof && retained == observed {
                        CaptureStatus::Complete
                    } else {
                        CaptureStatus::Partial
                    },
                    reason,
                    encoding: RawEncoding::Raw,
                    chunk_blake3: self.bytes.chunks(65_536).map(Digest::of).collect(),
                };
                assert!(
                    receipt.validate().is_ok(),
                    "the test sink writes valid receipts"
                );
                Box::pin(async move {
                    CaptureFinalization {
                        receipt,
                        artifact: None,
                    }
                })
            }
        }

        /// A lease for `c1` whose release sets `released`.
        fn lease(released: &Rc<Cell<bool>>) -> CaptureLease {
            let flag = Rc::clone(released);
            let sink = |stream| -> Box<dyn RawCaptureSink> {
                Box::new(Keeping {
                    stream,
                    bytes: Vec::new(),
                })
            };
            CaptureLease::new(
                ToolCallId::new("c1"),
                sink(CaptureStream::Stdout),
                sink(CaptureStream::Stderr),
                Box::new(move || flag.set(true)),
            )
        }

        /// How the scripted shell answers a captured run.
        #[derive(Clone, Copy)]
        enum Archiving {
            /// It cannot capture: the port's default.
            Unsupported,
            /// It archives both streams.
            Both,
            /// Standard output's archive never reports back.
            LosesStdout,
        }

        /// A shell that runs nothing and reports [`scripted`], counting how often it ran.
        struct Scripted {
            archiving: Archiving,
            runs: Rc<Cell<usize>>,
        }

        impl Scripted {
            fn ran(&self) -> ShellOutcome {
                self.runs.set(self.runs.get().saturating_add(1));
                scripted()
            }
        }

        /// Feeds `text` to a sink and finalizes it at end of file.
        async fn archive(sink: Option<Box<dyn RawCaptureSink>>, text: &str) -> CaptureFinalization {
            let mut sink = sink.expect("the tool hands over both sinks");
            sink.write(text.as_bytes()).await.expect("kept");
            sink.finalize(text.len() as u64, CaptureReason::Eof).await
        }

        impl ShellPort for Scripted {
            fn run(&self, _: ShellRequest) -> LocalBoxFuture<'_, ShellResult<ShellOutcome>> {
                Box::pin(async move { Ok(self.ran()) })
            }

            fn spawn(&self, _: ShellRequest) -> LocalBoxFuture<'_, ShellResult<ShellStream>> {
                Box::pin(async { Err(ShellError::CaptureUnsupported) })
            }

            fn kill_all(&self) -> LocalBoxFuture<'_, ShellResult<usize>> {
                Box::pin(async { Ok(0) })
            }

            fn sandbox(&self) -> SandboxPolicy {
                SandboxPolicy::danger_full_access("/work")
            }

            fn run_with_capture(
                &self,
                _: ShellRequest,
                capture: ShellCapture,
            ) -> LocalBoxFuture<'_, ShellResult<CapturedOutcome>> {
                Box::pin(async move {
                    if matches!(self.archiving, Archiving::Unsupported) {
                        return Err(ShellError::CaptureUnsupported);
                    }
                    let outcome = self.ran();
                    let stderr = Some(archive(capture.stderr, "warning: unused\n").await);
                    let stdout = archive(capture.stdout, "built\n").await;
                    let stdout = matches!(self.archiving, Archiving::Both).then_some(stdout);
                    Ok(CapturedOutcome {
                        outcome,
                        stdout,
                        stderr,
                    })
                })
            }
        }

        /// Runs `true` as call `c1` through a bash tool built over `archiving`, with or without a
        /// broker, returning the rendered text and how often the shell ran.
        async fn call(archiving: Archiving, broker: Option<&CaptureBroker>) -> (String, usize) {
            let runs = Rc::new(Cell::new(0));
            let port: Box<dyn ShellPort> = Box::new(Scripted {
                archiving,
                runs: Rc::clone(&runs),
            });
            let shell: ShellHandle = Rc::new(port);
            let tool = match broker {
                Some(broker) => bash_tool_with_capture(shell, broker.clone()),
                None => bash_tool(shell),
            };
            let name = ToolName::new("bash").unwrap_or_else(|_| unreachable!("bash is valid"));
            let call = ToolCall::new(ToolCallId::new("c1"), name, json!({ "command": "true" }));
            let result = tool.execute(call).await;
            let Some(ContentBlock::Text(text)) = result.outcome.content().first() else {
                panic!("bash renders one text block: {:?}", result.outcome);
            };
            (text.clone(), runs.get())
        }

        /// The legacy invariant: without a broker, or with one holding no lease for the call, the
        /// model sees byte-for-byte what it saw before capture existed.
        #[tokio::test]
        async fn a_call_with_no_lease_renders_exactly_as_before() {
            assert_eq!(render_outcome(&scripted()), LEGACY);
            assert_eq!(call(Archiving::Both, None).await, (LEGACY.to_owned(), 1));
            let broker = CaptureBroker::new();
            assert_eq!(
                call(Archiving::Both, Some(&broker)).await,
                (LEGACY.to_owned(), 1)
            );
            assert!(
                broker.is_empty(),
                "nothing was filed for a call that had no lease"
            );
        }

        /// A leased call archives both streams, files their finalizations under the call, and tells
        /// the model where each stream is — before the exit status, which stays the last line.
        #[tokio::test]
        async fn a_leased_call_files_its_archives_and_names_them_to_the_model() {
            let broker = CaptureBroker::new();
            let released = Rc::new(Cell::new(false));
            broker.insert(lease(&released));
            let (text, runs) = call(Archiving::Both, Some(&broker)).await;
            assert_eq!(runs, 1);
            assert_eq!(
                text,
                "[stdout]\nbuilt\n[stdout archived as a:00000000-0000-4000-8000-000000000001 — \
                 6 of 6 bytes, complete; read it with context_recall]\n[stderr]\nwarning: unused\n\
                 [stderr archived as a:00000000-0000-4000-8000-000000000002 — 16 of 16 bytes, \
                 complete; read it with context_recall]\n[exit code: 2]\n"
            );
            assert!(
                !broker.has_lease(&ToolCallId::new("c1")),
                "the lease was used"
            );
            assert!(released.get(), "and released once the shell returned");
            let filed = broker.take_finalizations(&ToolCallId::new("c1"));
            let streams: Vec<_> = filed.iter().map(|done| done.receipt.stream).collect();
            assert_eq!(streams, vec![CaptureStream::Stdout, CaptureStream::Stderr]);
            assert!(
                filed
                    .iter()
                    .all(|done| done.receipt.status == CaptureStatus::Complete)
            );
        }

        /// A shell that cannot capture runs the command once, uncaptured, and files nothing: the
        /// runner, not the tool, reports the archive as unavailable.
        #[tokio::test]
        async fn a_shell_that_cannot_capture_runs_once_and_files_nothing() {
            let broker = CaptureBroker::new();
            let released = Rc::new(Cell::new(false));
            broker.insert(lease(&released));
            let (text, runs) = call(Archiving::Unsupported, Some(&broker)).await;
            assert_eq!((text.as_str(), runs), (LEGACY, 1));
            assert!(broker.is_empty());
            assert!(released.get(), "the unused reservation is released");
        }

        /// A stream whose archive never reported back is filed as unavailable, not left out and not
        /// guessed complete, and the model is told it was not archived.
        #[tokio::test]
        async fn an_archive_that_never_reports_back_is_filed_as_unavailable() {
            let broker = CaptureBroker::new();
            broker.insert(lease(&Rc::new(Cell::new(false))));
            let (text, _) = call(Archiving::LosesStdout, Some(&broker)).await;
            assert!(
                text.contains("[stdout not archived: the archive did not keep up]\n"),
                "{text}"
            );
            assert!(text.contains("[stderr archived as a:"), "{text}");
            assert!(text.ends_with("[exit code: 2]\n"), "{text}");
            let filed = broker.take_finalizations(&ToolCallId::new("c1"));
            let stdout = filed.first().expect("standard output is filed");
            assert_eq!(stdout.receipt.stream, CaptureStream::Stdout);
            assert_eq!(stdout.receipt.status, CaptureStatus::Unavailable);
            assert_eq!(stdout.receipt.reason, CaptureReason::Timeout);
            assert_eq!(stdout.receipt.call_id, "c1");
            assert_eq!(stdout.receipt.observed_bytes, 6);
            assert!(stdout.receipt.validate().is_ok());
            assert_eq!(filed.len(), 2);
        }

        /// A partial archive of a cut preview keeps every indicator: the truncation notice, the
        /// partial line with its reason, and the failing exit status as the last line.
        #[test]
        fn a_partial_archive_keeps_the_truncation_and_exit_indicators() {
            let mut ran = outcome("xxxx", "", Some(3), false, true);
            ran.stdout.total_bytes = 70_000;
            let receipt = ArtifactReceipt {
                artifact_id: ArtifactId::parse("a:00000000-0000-4000-8000-000000000001"),
                call_id: "c1".to_owned(),
                stream: CaptureStream::Stdout,
                retained_bytes: 65_536,
                observed_bytes: 70_000,
                retained_blake3: Some(Digest::empty()),
                status: CaptureStatus::Partial,
                reason: CaptureReason::Quota,
                encoding: RawEncoding::Raw,
                chunk_blake3: vec![Digest::empty()],
            };
            let rendered = render_archived(&ran, &[receipt]);
            assert!(
                rendered.contains("[stdout truncated: 70000 bytes total"),
                "{rendered}"
            );
            assert!(
                rendered.contains("65536 of 70000 bytes, partial: the archive quota was reached"),
                "{rendered}"
            );
            assert!(rendered.ends_with("[exit code: 3]\n"), "{rendered}");
            // An empty stream gains no archive line: there is nothing in it to read back.
            assert!(!rendered.contains("stderr"), "{rendered}");
        }
    }
}
