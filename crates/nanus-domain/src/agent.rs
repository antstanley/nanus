//! The turn and step state machine, pure and synchronous.
//!
//! The vocabulary is the `DeepSeek` Harness's, because the harness loop is the
//! part of `dsh` nanus most wants to keep:
//!
//! - A **turn** begins when a human submits something and ends when the model
//!   has nothing left owing.
//! - A **step** is one model request plus the tool calls it produced. A turn is
//!   zero or more steps: a turn that the model answers immediately has exactly
//!   one; a turn that keeps calling tools has more.
//! - The turn closes when *nothing is owed* — no tool call lacks a result and
//!   the model did not ask for more.
//!
//! The machine is a pure function of the log and the last step outcome. It holds
//! no state of its own beyond configuration, which is what makes the loop
//! testable without a runtime and resumable from a session file.
//!
//! ## Divergence from `dsh`: the step budget
//!
//! `dsh` has no bound on how many steps a turn may take. A model that keeps
//! calling tools, or two tools that call each other, therefore runs until a
//! human notices. nanus adds [`AgentConfig::max_steps_per_turn`], defaulting to
//! [`DEFAULT_MAX_STEPS_PER_TURN`], and closes the turn as
//! [`TurnOutcome::MaxSteps`] when it is reached. The budget is a deliberate
//! divergence, not a compatibility gap: an unbounded loop inside a tool-using
//! agent is a hazard with no upside, and the closing reason is reported so a
//! caller can tell "the model finished" from "we stopped it".

use serde::{Deserialize, Serialize};

use crate::error::DomainError;
use crate::message::ToolCallId;
use crate::session::{SessionLog, TurnEndReason};

/// Default number of steps a single turn may take.
///
/// Sixteen is chosen to be far above any genuine tool-using turn and far below
/// the point at which a runaway loop becomes expensive.
pub const DEFAULT_MAX_STEPS_PER_TURN: u32 = 16;

/// Default number of tool calls a harness may have in flight at once.
pub const DEFAULT_MAX_PARALLEL_TOOLS: u32 = 4;

/// Default ceiling, in bytes, on an assembled system prompt.
pub const DEFAULT_SYSTEM_PROMPT_MAX: usize = 32_768;

/// The knobs the turn machine and its callers share.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentConfig {
    /// Maximum steps in one turn. At least one.
    pub max_steps_per_turn: u32,
    /// Maximum tool calls in flight at once. At least one.
    pub max_parallel_tools: u32,
    /// The model a turn is sent to.
    pub model: String,
    /// Maximum size, in bytes, of an assembled system prompt.
    pub system_prompt_max: usize,
}

impl AgentConfig {
    /// Validates and builds a configuration.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::Validation`] when the model is empty, when either
    /// budget is zero, or when the prompt ceiling is zero. A zero budget is
    /// rejected rather than reinterpreted: `max_steps_per_turn: 0` would mean
    /// "no turn can ever run", which is never what a caller means.
    pub fn new(
        max_steps_per_turn: u32,
        max_parallel_tools: u32,
        model: impl Into<String>,
        system_prompt_max: usize,
    ) -> Result<Self, DomainError> {
        let model = model.into();
        let config = Self {
            max_steps_per_turn,
            max_parallel_tools,
            model,
            system_prompt_max,
        };
        config.validate()?;
        Ok(config)
    }

    /// Builds a configuration for `model` with every default budget.
    #[must_use]
    pub fn for_model(model: impl Into<String>) -> Self {
        Self {
            max_steps_per_turn: DEFAULT_MAX_STEPS_PER_TURN,
            max_parallel_tools: DEFAULT_MAX_PARALLEL_TOOLS,
            model: model.into(),
            system_prompt_max: DEFAULT_SYSTEM_PROMPT_MAX,
        }
    }

    /// Checks the configuration's invariants.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::Validation`] under the same conditions as
    /// [`AgentConfig::new`].
    pub fn validate(&self) -> Result<(), DomainError> {
        if self.model.trim().is_empty() {
            return Err(DomainError::Validation {
                field: "model",
                reason: String::from("a turn cannot be sent to an unnamed model"),
            });
        }
        if self.max_steps_per_turn == 0 {
            return Err(DomainError::Validation {
                field: "max_steps_per_turn",
                reason: String::from("a turn needs at least one step"),
            });
        }
        if self.max_parallel_tools == 0 {
            return Err(DomainError::Validation {
                field: "max_parallel_tools",
                reason: String::from("at least one tool may run at a time"),
            });
        }
        if self.system_prompt_max == 0 {
            return Err(DomainError::Validation {
                field: "system_prompt_max",
                reason: String::from("a prompt has a non-zero ceiling"),
            });
        }
        // Postcondition: an accepted configuration never carries an empty model.
        assert!(!self.model.trim().is_empty());
        Ok(())
    }
}

/// What one step produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StepOutcome {
    /// The model ended its step without requesting any tool call.
    ///
    /// The machine still consults the log before closing the turn, because the
    /// log is the only source of truth about what is owed; a step reported as
    /// final while a tool result is outstanding keeps the turn open.
    FinalAnswer,
    /// The model requested tool calls.
    ToolCalls {
        /// How many it requested.
        count: u32,
    },
    /// The model hit its token ceiling.
    MaxTokens,
    /// The request failed.
    Error {
        /// The rendered failure.
        message: String,
    },
    /// The user interrupted the turn.
    Interrupted,
}

/// What the turn machine decided.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TurnOutcome {
    /// The turn stays open; run the step at the given index.
    Continue {
        /// The step index the next step will carry.
        step: u32,
    },
    /// The turn closed normally, with nothing owed.
    Completed,
    /// The turn closed at its step budget.
    MaxSteps,
    /// The turn closed because the model ran out of tokens.
    MaxTokens,
    /// The turn closed because a request failed.
    Error {
        /// The rendered failure.
        message: String,
    },
    /// The turn closed because the user interrupted it.
    Interrupted,
}

impl TurnOutcome {
    /// Returns `true` when the turn is over.
    #[must_use]
    pub const fn is_closed(&self) -> bool {
        !matches!(self, Self::Continue { .. })
    }

    /// Returns the reason to record when the turn closes, or `None` to continue.
    ///
    /// This is the one place a decision becomes a durable
    /// [`TurnEndReason`], so the two vocabularies cannot drift.
    #[must_use]
    pub fn turn_end_reason(&self) -> Option<TurnEndReason> {
        match self {
            Self::Continue { .. } => None,
            Self::Completed => Some(TurnEndReason::Completed),
            Self::MaxSteps => Some(TurnEndReason::MaxSteps),
            Self::MaxTokens => Some(TurnEndReason::MaxTokens),
            Self::Error { message } => Some(TurnEndReason::Error {
                message: message.clone(),
            }),
            Self::Interrupted => Some(TurnEndReason::Interrupted),
        }
    }

    /// Returns a short label for a status line.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Continue { .. } => "continue",
            Self::Completed => "completed",
            Self::MaxSteps => "max_steps",
            Self::MaxTokens => "max_tokens",
            Self::Error { .. } => "error",
            Self::Interrupted => "interrupted",
        }
    }
}

/// Decides whether a turn continues or closes.
///
/// The machine is cheap to clone and holds only configuration, so a caller may
/// keep one per turn or one for the process. It never mutates: every decision is
/// a function of the configuration, the log, and the last step outcome.
#[derive(Clone, Debug, PartialEq, Eq)]
#[must_use = "a turn machine that is never consulted decides nothing"]
pub struct TurnMachine {
    /// The budgets and the model this machine decides against.
    config: AgentConfig,
}

impl TurnMachine {
    /// Builds a machine, validating its configuration.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::Validation`] when the configuration is invalid, so
    /// an unusable machine cannot be constructed and then consulted.
    pub fn new(config: AgentConfig) -> Result<Self, DomainError> {
        config.validate()?;
        Ok(Self { config })
    }

    /// Returns the configuration this machine decides against.
    #[must_use]
    pub const fn config(&self) -> &AgentConfig {
        &self.config
    }

    /// Returns the highest step index the turn may reach.
    #[must_use]
    pub const fn step_budget(&self) -> u32 {
        self.config.max_steps_per_turn
    }

    /// Returns how many steps the current turn has started.
    #[must_use]
    pub fn steps_taken(&self, log: &SessionLog) -> u32 {
        log.steps_in_turn(log.current_turn())
    }

    /// Returns the tool calls the current turn still owes results for.
    #[must_use]
    pub fn owed_tool_calls(&self, log: &SessionLog) -> Vec<ToolCallId> {
        log.open_tool_calls()
    }

    /// Decides what happens after one step.
    ///
    /// A terminal step outcome closes the turn with the matching reason. A
    /// non-terminal one closes the turn only when it is final *and* nothing is
    /// owed; otherwise another step runs, unless the step budget is exhausted,
    /// in which case the turn closes as [`TurnOutcome::MaxSteps`].
    #[must_use]
    pub fn decide(&self, log: &SessionLog, outcome: &StepOutcome) -> TurnOutcome {
        if let Some(closed) = terminal_outcome(outcome) {
            return closed;
        }
        let steps_taken = self.steps_taken(log);
        if steps_taken >= self.config.max_steps_per_turn {
            return TurnOutcome::MaxSteps;
        }
        let decision = if matches!(outcome, StepOutcome::FinalAnswer)
            && self.owed_tool_calls(log).is_empty()
        {
            TurnOutcome::Completed
        } else {
            TurnOutcome::Continue { step: steps_taken }
        };
        // Postcondition: a turn that continues has budget left, which is the
        // whole reason the budget exists.
        assert!(
            decision.is_closed() || steps_taken < self.config.max_steps_per_turn,
            "a continuing turn has budget left"
        );
        decision
    }
}

/// Maps a step outcome that always closes the turn.
fn terminal_outcome(outcome: &StepOutcome) -> Option<TurnOutcome> {
    match outcome {
        StepOutcome::Interrupted => Some(TurnOutcome::Interrupted),
        StepOutcome::Error { message } => Some(TurnOutcome::Error {
            message: message.clone(),
        }),
        StepOutcome::MaxTokens => Some(TurnOutcome::MaxTokens),
        StepOutcome::FinalAnswer | StepOutcome::ToolCalls { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::ToolCallId;
    use crate::session::{SessionEvent, SessionId, SessionLog};
    use crate::tool::ToolName;
    use serde_json::json;

    fn machine() -> TurnMachine {
        let config = AgentConfig::for_model("deepseek-flash");
        TurnMachine::new(config).unwrap_or_else(|error| panic!("test config: {error}"))
    }

    fn tool_name(raw: &str) -> ToolName {
        ToolName::new(raw).unwrap_or_else(|error| panic!("test tool name {raw}: {error}"))
    }

    /// A log with one started step in turn zero.
    fn log_with_one_step() -> SessionLog {
        let mut log = SessionLog::new();
        log.append(SessionEvent::TurnStart { turn: 0 });
        log.append(SessionEvent::StepStart { turn: 0, step: 0 });
        log
    }

    #[test]
    fn a_final_answer_with_nothing_owed_closes_the_turn() {
        let log = log_with_one_step();
        let decision = machine().decide(&log, &StepOutcome::FinalAnswer);
        assert_eq!(decision, TurnOutcome::Completed);
        assert!(decision.is_closed());
        assert_eq!(decision.turn_end_reason(), Some(TurnEndReason::Completed));
        assert!(decision.turn_end_reason().is_some_and(|r| r.is_success()));
    }

    #[test]
    fn a_requested_tool_call_keeps_the_turn_open() {
        let log = log_with_one_step();
        let decision = machine().decide(&log, &StepOutcome::ToolCalls { count: 2 });
        assert_eq!(decision, TurnOutcome::Continue { step: 1 });
        assert!(!decision.is_closed());
        assert_eq!(decision.turn_end_reason(), None);
    }

    #[test]
    fn a_final_answer_with_an_owed_tool_call_keeps_the_turn_open() {
        // The log, not the caller's word, decides what is owed: a step cannot
        // close a turn while a tool result is outstanding.
        let mut log = log_with_one_step();
        log.append(SessionEvent::ToolCall {
            call_id: ToolCallId::new("c-1"),
            name: tool_name("read"),
            arguments: json!({}),
        });
        let decision = machine().decide(&log, &StepOutcome::FinalAnswer);
        assert_eq!(
            decision,
            TurnOutcome::Continue { step: 1 },
            "an outstanding tool call is still owed"
        );
        assert_eq!(machine().owed_tool_calls(&log).len(), 1);
    }

    #[test]
    fn a_settled_tool_call_lets_the_final_answer_close_the_turn() {
        let mut log = log_with_one_step();
        log.append(SessionEvent::ToolCall {
            call_id: ToolCallId::new("c-1"),
            name: tool_name("read"),
            arguments: json!({}),
        });
        log.append(SessionEvent::ToolResult {
            call_id: ToolCallId::new("c-1"),
            content: "ok".to_owned(),
            is_error: false,
        });
        assert!(machine().owed_tool_calls(&log).is_empty());
        assert_eq!(
            machine().decide(&log, &StepOutcome::FinalAnswer),
            TurnOutcome::Completed
        );
    }

    #[test]
    fn the_step_budget_closes_a_runaway_turn() {
        let config = AgentConfig::new(2, 4, "deepseek-flash", 1024);
        assert!(config.is_ok());
        let Ok(config) = config else { return };
        let machine = TurnMachine::new(config);
        assert!(machine.is_ok());
        let Ok(machine) = machine else { return };

        let mut log = SessionLog::new();
        log.append(SessionEvent::TurnStart { turn: 0 });
        log.append(SessionEvent::StepStart { turn: 0, step: 0 });
        assert_eq!(
            machine.decide(&log, &StepOutcome::ToolCalls { count: 1 }),
            TurnOutcome::Continue { step: 1 }
        );
        log.append(SessionEvent::StepEnd { turn: 0, step: 0 });
        log.append(SessionEvent::StepStart { turn: 0, step: 1 });
        let closed = machine.decide(&log, &StepOutcome::ToolCalls { count: 1 });
        assert_eq!(closed, TurnOutcome::MaxSteps);
        assert_eq!(closed.turn_end_reason(), Some(TurnEndReason::MaxSteps));
        assert!(!closed.turn_end_reason().is_some_and(|r| r.is_success()));
    }

    #[test]
    fn the_default_budget_is_the_documented_sixteen() {
        let machine = machine();
        assert_eq!(machine.step_budget(), DEFAULT_MAX_STEPS_PER_TURN);
        assert_eq!(machine.step_budget(), 16);
    }

    #[test]
    fn terminal_outcomes_close_the_turn_regardless_of_the_log() {
        let log = log_with_one_step();
        let machine = machine();
        assert_eq!(
            machine.decide(&log, &StepOutcome::Interrupted),
            TurnOutcome::Interrupted
        );
        assert_eq!(
            machine.decide(
                &log,
                &StepOutcome::Error {
                    message: "connection reset".to_owned()
                }
            ),
            TurnOutcome::Error {
                message: "connection reset".to_owned()
            }
        );
        assert_eq!(
            machine.decide(&log, &StepOutcome::MaxTokens),
            TurnOutcome::MaxTokens
        );
    }

    #[test]
    fn every_closing_decision_maps_to_a_turn_end_reason() {
        let decisions = [
            TurnOutcome::Completed,
            TurnOutcome::MaxSteps,
            TurnOutcome::MaxTokens,
            TurnOutcome::Interrupted,
            TurnOutcome::Error {
                message: "boom".to_owned(),
            },
        ];
        for decision in &decisions {
            assert!(decision.is_closed(), "{decision:?} closes the turn");
            assert!(
                decision.turn_end_reason().is_some(),
                "{decision:?} has a durable reason"
            );
        }
        assert!(
            TurnOutcome::Continue { step: 3 }
                .turn_end_reason()
                .is_none()
        );
        assert_eq!(TurnOutcome::Continue { step: 0 }.label(), "continue");
    }

    #[test]
    fn a_zero_step_budget_cannot_be_configured() {
        // The divergence from `dsh` is worth stating, but zero is not a budget:
        // it is a configuration that could never run a turn.
        assert!(AgentConfig::new(0, 4, "deepseek-flash", 1024).is_err());
        assert!(AgentConfig::new(1, 0, "deepseek-flash", 1024).is_err());
        assert!(AgentConfig::new(1, 4, "", 1024).is_err());
        assert!(AgentConfig::new(1, 4, "deepseek-flash", 0).is_err());
        assert!(AgentConfig::new(1, 4, "deepseek-flash", 1024).is_ok());
    }

    #[test]
    fn a_machine_cannot_be_built_from_an_invalid_config() {
        let mut config = AgentConfig::for_model("deepseek-flash");
        config.max_steps_per_turn = 0;
        assert!(config.validate().is_err());
        assert!(TurnMachine::new(config).is_err());
    }

    #[test]
    fn the_step_count_comes_from_the_log() {
        let mut log = SessionLog::new();
        log.append(SessionEvent::TurnStart { turn: 0 });
        assert_eq!(
            machine().steps_taken(&log),
            0,
            "a turn may have no steps yet"
        );
        log.append(SessionEvent::StepStart { turn: 0, step: 0 });
        assert_eq!(machine().steps_taken(&log), 1);
        log.append(SessionEvent::TurnStart { turn: 1 });
        assert_eq!(machine().steps_taken(&log), 0, "turn one has not stepped");
    }

    #[test]
    fn the_machine_exposes_the_config_it_was_given() {
        // The model id is opaque here: the domain neither knows nor cares which
        // provider it names.
        let config = AgentConfig::for_model("a-model-id");
        let machine = TurnMachine::new(config.clone());
        assert!(machine.is_ok());
        let Ok(machine) = machine else { return };
        assert_eq!(machine.config(), &config);
        assert_eq!(machine.config().model, "a-model-id");
        assert_eq!(
            machine.config().max_parallel_tools,
            DEFAULT_MAX_PARALLEL_TOOLS
        );
    }

    #[test]
    fn a_log_with_an_unknown_session_id_still_decides() {
        // The machine reads the log, not the session, so it works on a log that
        // was recovered without its header.
        let session = crate::Session::new(SessionId::new("s"), 0, "/w");
        let decision = machine().decide(session.log(), &StepOutcome::FinalAnswer);
        assert_eq!(decision, TurnOutcome::Completed);
    }
}
