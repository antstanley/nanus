//! Progress reporting for a headless run.
//!
//! The rule the whole binary is built around lives here: **stdout is the answer and
//! nothing else**. Reasoning and tool activity go to stderr, and they are written
//! incrementally so a long turn shows progress rather than appearing to hang.

use std::cell::Cell;
use std::io::Write as _;
use std::rc::Rc;

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
    /// A flag the process sets when it is interrupted, which the turn reads.
    stop: Option<Rc<Cell<bool>>>,
}

impl StderrProgress {
    /// Builds a reporter.
    #[must_use]
    pub const fn new(reasoning: bool, tools: bool) -> Self {
        Self {
            reasoning,
            tools,
            heading_written: false,
            stop: None,
        }
    }

    /// Returns the reporter, asking the turn to stop when `stop` is set.
    ///
    /// A headless run has no interface to press a key in. Without this, Ctrl-C killed the
    /// process outright: no answer, and no recorded session to resume from — which is the worst
    /// of both, since the turn's work is what a resume would build on. With it, the turn stops
    /// at its next checkpoint and is recorded as interrupted.
    #[must_use]
    pub fn stopping_when(mut self, stop: Rc<Cell<bool>>) -> Self {
        self.stop = Some(stop);
        self
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
    fn cancelled(&self) -> bool {
        self.stop.as_ref().is_some_and(|stop| stop.get())
    }

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

    fn elided(&mut self, elision: &nanus_domain::Elision) {
        // Printed whatever else is off: a reader who asked for no progress at all still needs to
        // know that the model is answering from part of the conversation, because an answer that
        // contradicts a dropped turn is not the model being wrong.
        Self::note(&format!(
            "nanus: the conversation was trimmed to fit the prompt budget: {} messages across {} turns dropped, {} estimated tokens sent",
            elision.dropped_messages, elision.dropped_turns, elision.kept_tokens
        ));
    }

    fn step_started(&mut self, step: u32) {
        if self.reasoning || self.tools {
            Self::note(&format!("nanus: step {step}"));
        }
        self.heading_written = false;
    }

    fn tool_started(
        &mut self,
        _call_id: &nanus_domain::ToolCallId,
        name: &ToolName,
        _arguments: &serde_json::Value,
    ) {
        // The arguments and the call's id are available and deliberately unused: this
        // reporter names the tool for someone watching a run, and the interface is where a
        // call is followed from start to finish.
        if self.tools {
            Self::note(&format!("nanus: running {name}"));
        }
    }

    fn tool_finished(
        &mut self,
        _call_id: &nanus_domain::ToolCallId,
        name: &ToolName,
        is_error: bool,
    ) {
        if self.tools {
            let status = if is_error { "failed" } else { "finished" };
            Self::note(&format!("nanus: {name} {status}"));
        }
    }

    fn goal_changed(&mut self, goal: Option<&nanus_domain::Goal>) {
        // With the tool lines, because it is what a goal tool call did: a reader following the
        // calls sees the objective move where the call that moved it ran.
        if self.tools {
            let line = goal.map_or_else(
                || String::from("no goal is set"),
                |goal| {
                    format!(
                        "{} (rev {}): {}",
                        goal.phase(),
                        goal.revision(),
                        goal.objective()
                    )
                },
            );
            Self::note(&format!("nanus: goal {line}"));
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
        reporter.tool_started(&call_id(), &tool(), &serde_json::Value::Null);
        reporter.tool_finished(&call_id(), &tool(), false);
        reporter.usage(&Usage::default());
    }

    #[test]
    fn an_enabled_reporter_is_callable_for_every_event() {
        let mut reporter = StderrProgress::new(true, true);
        reporter.step_started(1);
        reporter.reasoning("a");
        reporter.reasoning("b");
        reporter.tool_started(&call_id(), &tool(), &serde_json::Value::Null);
        reporter.tool_finished(&call_id(), &tool(), true);
        reporter.usage(&Usage::default());
        reporter.text("ignored");
    }

    fn call_id() -> nanus_domain::ToolCallId {
        nanus_domain::ToolCallId::new("call-1")
    }

    fn tool() -> ToolName {
        ToolName::new("read").unwrap_or_else(|_| unreachable!("read is a valid tool name"))
    }

    /// The interrupt the binary watches for reaches the loop through this flag; without the
    /// wiring the turn would run to its budget and the session would never be recorded.
    #[test]
    fn the_reporter_asks_the_turn_to_stop_when_the_flag_is_set() {
        let stop = Rc::new(Cell::new(false));
        let reporter = StderrProgress::new(false, false).stopping_when(Rc::clone(&stop));
        assert!(!reporter.cancelled(), "nothing has asked it to stop");

        stop.set(true);
        assert!(reporter.cancelled(), "the interrupt reaches the loop");

        // The other direction: a reporter with no flag — every other caller — never asks.
        assert!(!StderrProgress::new(false, false).cancelled());
    }
}
