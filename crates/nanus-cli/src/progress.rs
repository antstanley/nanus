//! Progress reporting for a headless run.
//!
//! The rule the whole binary is built around lives here: **stdout is the answer and
//! nothing else**. Reasoning and tool activity go to stderr, and they are written
//! incrementally so a long turn shows progress rather than appearing to hang.

use std::io::Write as _;

use nanus_bundle::Progress;
use nanus_domain::{ToolName, Usage};

/// Reports progress to stderr.
pub struct StderrProgress {
    /// Whether reasoning should be echoed as it arrives.
    reasoning: bool,
    /// Whether tool activity should be announced.
    tools: bool,
    /// Whether a reasoning heading has been printed for the current step.
    heading_written: bool,
}

impl StderrProgress {
    /// Builds a reporter.
    #[must_use]
    pub const fn new(reasoning: bool, tools: bool) -> Self {
        Self {
            reasoning,
            tools,
            heading_written: false,
        }
    }

    /// Writes a line to stderr, ignoring a failure.
    ///
    /// A closed stderr must not abort a run: the answer still has somewhere to go on
    /// stdout, and losing progress reporting is not a reason to lose the work.
    fn note(message: &str) {
        let mut stderr = std::io::stderr().lock();
        let _ = writeln!(stderr, "{message}");
    }

    /// Writes a fragment to stderr without a trailing newline.
    fn fragment(text: &str) {
        let mut stderr = std::io::stderr().lock();
        let _ = write!(stderr, "{text}");
        let _ = stderr.flush();
    }
}

impl Progress for StderrProgress {
    fn reasoning(&mut self, delta: &str) {
        if !self.reasoning {
            return;
        }
        if !self.heading_written {
            Self::note("nanus: reasoning:");
            self.heading_written = true;
        }
        Self::fragment(delta);
    }

    fn text(&mut self, _delta: &str) {
        // The answer is streamed to stdout by the caller once the turn completes, so
        // echoing it here would print it twice.
    }

    fn step_started(&mut self, step: u32) {
        if self.reasoning || self.tools {
            Self::note(&format!("nanus: step {step}"));
        }
        self.heading_written = false;
    }

    fn tool_started(&mut self, name: &ToolName, _arguments: &serde_json::Value) {
        // The arguments are available and deliberately unused: this reporter names the
        // tool for someone watching a run, and the interface is where a call is turned
        // into a sentence about what the agent is doing.
        if self.tools {
            Self::note(&format!("nanus: running {name}"));
        }
    }

    fn tool_finished(&mut self, name: &ToolName, is_error: bool) {
        if self.tools {
            let status = if is_error { "failed" } else { "finished" };
            Self::note(&format!("nanus: {name} {status}"));
        }
    }

    fn usage(&mut self, usage: &Usage) {
        if self.tools {
            Self::note(&format!(
                "nanus: {} prompt + {} completion tokens",
                usage.prompt_tokens, usage.completion_tokens
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_disabled_reporter_writes_nothing() {
        // The behaviour is that no panic and no output occurs; the assertion is that
        // every method is callable in the disabled configuration.
        let mut reporter = StderrProgress::new(false, false);
        reporter.reasoning("thinking");
        reporter.text("answer");
        reporter.step_started(1);
        reporter.tool_started(&tool(), &serde_json::Value::Null);
        reporter.tool_finished(&tool(), false);
        reporter.usage(&Usage::default());
    }

    #[test]
    fn an_enabled_reporter_is_callable_for_every_event() {
        let mut reporter = StderrProgress::new(true, true);
        reporter.step_started(1);
        reporter.reasoning("a");
        reporter.reasoning("b");
        reporter.tool_started(&tool(), &serde_json::Value::Null);
        reporter.tool_finished(&tool(), true);
        reporter.usage(&Usage::default());
        reporter.text("ignored");
    }

    fn tool() -> ToolName {
        ToolName::new("read").unwrap_or_else(|_| unreachable!("read is a valid tool name"))
    }
}
