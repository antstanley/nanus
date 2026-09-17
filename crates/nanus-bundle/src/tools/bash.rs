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

use core::fmt::Write as _;
use core::time::Duration;

use nanus_domain::{
    ContentBlock, ToolAccess, ToolCall, ToolCallId, ToolDefinition, ToolExecutor, ToolFuture,
    ToolName, ToolOutcome, ToolResult, ToolSchema,
};
use nanus_ports::{Captured, ShellHandle, ShellRequest};
use serde_json::json;

use crate::args::Arguments;
use crate::tools::port_error_result;

/// The default per-command time limit.
pub const DEFAULT_TIMEOUT_MS: u64 = 120_000;

/// The largest per-stream output the tool will return.
pub const MAX_OUTPUT_BYTES: usize = 65_536;

/// Builds the `bash` tool over `shell`.
pub fn bash_tool(shell: ShellHandle) -> ToolDefinition {
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
    ToolDefinition::new(schema, BashExecutor { shell }).with_access(ToolAccess::Execute)
}

/// Executes `bash`.
struct BashExecutor {
    shell: ShellHandle,
}

impl ToolExecutor for BashExecutor {
    fn execute(&self, call: ToolCall) -> ToolFuture {
        let shell = std::rc::Rc::clone(&self.shell);
        Box::pin(async move { bash_outcome(shell, call).await })
    }
}

/// Runs a command and renders it for the model.
async fn bash_outcome(shell: ShellHandle, call: ToolCall) -> ToolResult {
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
    let outcome = shell.run(request).await;
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(error) => return port_error_result(id, "bash", &error),
    };

    let rendered = render_outcome(&outcome);
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

/// Renders a shell outcome the way a terminal would, plus the facts a terminal omits.
///
/// The order is deliberately stdout, then stderr, then the exit status: a model
/// reading the tail learns how the command ended, which is the thing it most often
/// needs.
pub fn render_outcome(outcome: &nanus_ports::ShellOutcome) -> String {
    let mut rendered = String::new();
    append_stream(&mut rendered, "stdout", &outcome.stdout);
    append_stream(&mut rendered, "stderr", &outcome.stderr);
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

/// Appends one captured stream, labelled, when it has content.
fn append_stream(rendered: &mut String, label: &str, captured: &Captured) {
    if captured.text.is_empty() && captured.total_bytes == 0 {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use nanus_ports::ShellOutcome;

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
}
