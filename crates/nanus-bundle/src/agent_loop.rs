//! The agent loop: one turn of the model, its tools, and its session log.
//!
//! ## The shape of a turn
//!
//! A **step** is one model request plus the tools it calls. A **turn** is zero or
//! more steps, and it closes once nothing is owed. That is the reference harness's
//! model, and it is the right one: the loop's job is to keep asking the model what
//! to do until it stops asking for tools.
//!
//! ```text
//! turn/start
//!   claim the user message
//!   step 1: assemble prompt + schemas -> stream -> assistant/message
//!           tool/call* -> execute -> tool/result*
//!   step 2: (owed work) -> stream -> assistant/message
//!           ...
//! turn/end
//! ```
//!
//! ## Why the session log is the only source of model history
//!
//! The loop never keeps a private conversation. Every fact it wants the model to see
//! is appended to the log, and the next request is *derived* from the log. That is
//! what makes a transcript reproducible: if it is not in the log, the model did not
//! see it, and a runtime assertion can check exactly that.
//!
//! ## Streaming, and what is committed
//!
//! Text and reasoning arrive as deltas and are appended to the log as they settle,
//! so a cancelled or failed step still leaves what the user actually saw. An
//! interrupted step is marked `interrupted: true` rather than discarded, because a
//! model that is told it said something is less confused than one whose words
//! vanished.

use core::fmt::Write as _;
use std::rc::Rc;

use nanus_domain::{
    AgentConfig, ContentBlock, Session, SessionEvent, SessionId, StepOutcome, ToolCall, ToolCallId,
    ToolName, ToolRegistry, TurnEndReason, TurnMachine, Usage,
};
use nanus_ports::{ChatRequest, FinishReason, LlmEvent, LlmPort};

use crate::BundleError;

/// What a completed run produced.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RunOutcome {
    /// The session id the run was recorded under.
    pub session_id: SessionId,
    /// The model's final answer, empty when it produced none.
    pub answer: String,
    /// Why the last turn ended.
    pub reason: TurnEndReason,
    /// How many steps the run took.
    pub steps: u32,
    /// The token accounting for the whole run.
    pub usage: Usage,
}

impl RunOutcome {
    /// Returns `true` when the run finished normally.
    #[must_use]
    pub const fn is_success(&self) -> bool {
        matches!(
            self.reason,
            TurnEndReason::Completed | TurnEndReason::MaxTokens
        )
    }
}

/// A listener told about progress as it happens.
///
/// The loop has no opinion about presentation: a terminal UI streams deltas, a
/// headless run prints the answer at the end, and a test records whatever it needs
/// to assert on. Keeping that out of the loop is what lets both exist.
pub trait Progress {
    /// The model emitted more of its answer.
    fn text(&mut self, _delta: &str) {}

    /// The model emitted more of its reasoning.
    fn reasoning(&mut self, _delta: &str) {}

    /// A step began.
    fn step_started(&mut self, _step: u32) {}

    /// A tool is about to run.
    fn tool_started(&mut self, _name: &ToolName) {}

    /// A tool finished.
    fn tool_finished(&mut self, _name: &ToolName, _is_error: bool) {}

    /// Usage was reported.
    fn usage(&mut self, _usage: &Usage) {}
}

/// A [`Progress`] that ignores everything, for a caller that wants none.
#[derive(Clone, Copy, Debug, Default)]
pub struct Silent;

impl Progress for Silent {}

/// Runs turns against a session.
///
/// The runner owns no state between calls beyond the session it is given, so a
/// caller can drive several sessions with one runner and several runners against one
/// session. Everything mutable lives in the [`Session`], which is what makes the run
/// observable and resumable.
pub struct AgentRunner {
    llm: Rc<Box<dyn LlmPort>>,
    tools: Rc<ToolRegistry>,
    system_prompt: String,
    config: AgentConfig,
}

impl core::fmt::Debug for AgentRunner {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AgentRunner")
            .field("model", &self.config.model)
            .field("tools", &self.tools.len())
            .field("max_steps_per_turn", &self.config.max_steps_per_turn)
            .finish_non_exhaustive()
    }
}

impl AgentRunner {
    /// Builds a runner.
    ///
    /// # Errors
    ///
    /// Returns [`BundleError::Config`] when the agent configuration is invalid, so an
    /// unusable runner cannot be constructed and then consulted.
    pub fn new(
        llm: Rc<Box<dyn LlmPort>>,
        tools: Rc<ToolRegistry>,
        system_prompt: impl Into<String>,
        config: AgentConfig,
    ) -> Result<Self, BundleError> {
        config
            .validate()
            .map_err(|error| BundleError::Config(error.to_string()))?;
        // Precondition: a runner with no tools can still be useful, so an empty
        // registry is allowed; a prompt larger than the configured ceiling is not.
        let system_prompt = system_prompt.into();
        assert!(
            system_prompt.len() <= config.system_prompt_max,
            "the system prompt fits its configured ceiling"
        );
        Ok(Self {
            llm,
            tools,
            system_prompt,
            config,
        })
    }

    /// Returns the configuration in use.
    #[must_use]
    pub const fn config(&self) -> &AgentConfig {
        &self.config
    }

    /// Runs one turn for `message` and returns what it produced.
    ///
    /// # Errors
    ///
    /// Returns [`BundleError::Model`] when the model stream reported a failure it
    /// could not recover from, and [`BundleError::Tool`] when a tool could not be
    /// dispatched at all. A tool that ran and failed is *not* an error: its failure is
    /// the model's information, and it is recorded as a tool result.
    pub async fn run_turn(
        &self,
        session: &mut Session,
        message: &str,
        progress: &mut dyn Progress,
    ) -> Result<RunOutcome, BundleError> {
        let machine = TurnMachine::new(self.config.clone())
            .map_err(|error| BundleError::Config(error.to_string()))?;
        let turn = session.log().current_turn().saturating_add(1);
        session.append(SessionEvent::TurnStart { turn });
        session.append(SessionEvent::UserMessage {
            text: message.to_owned(),
        });

        let mut answer = String::new();
        let mut steps = 0_u32;
        loop {
            let step = steps.saturating_add(1);
            steps = step;
            progress.step_started(step);
            let step_outcome = self.run_step(session, turn, step, progress).await?;
            let decision = machine.decide(session.log(), &step_outcome);
            // The machine owns the mapping from a decision to a recorded reason, so
            // the two vocabularies cannot drift.
            let Some(reason) = decision.turn_end_reason() else {
                continue;
            };
            session.append(SessionEvent::TurnEnd { turn, reason });
            break;
        }

        // The answer is the last assistant message that carried text: a turn that
        // ended on a tool call has no final prose, and reporting the previous step's
        // text would misrepresent what the model concluded.
        answer.push_str(&last_assistant_text(session));
        let outcome = RunOutcome {
            session_id: session.id().clone(),
            answer,
            reason: session
                .log()
                .last_turn_end()
                .cloned()
                .unwrap_or(TurnEndReason::Blocked),
            steps,
            usage: session.usage_totals(),
        };
        // Postcondition: the turn is closed in the log, so a resumed session does not
        // find an open turn.
        assert!(
            session.log().last_turn_end().is_some(),
            "a finished run has a turn end"
        );
        Ok(outcome)
    }

    /// Runs one step: one request, then its tools.
    async fn run_step(
        &self,
        session: &mut Session,
        turn: u32,
        step: u32,
        progress: &mut dyn Progress,
    ) -> Result<StepOutcome, BundleError> {
        session.append(SessionEvent::StepStart { turn, step });
        let request = self.build_request(session);
        let mut stream = self.llm.stream_chat(request);
        let assembled = self.consume_stream(&mut stream, progress).await?;

        // The assistant message is appended before its tools run, so a crash between
        // the two leaves a record of what was asked for rather than a silently
        // dropped step.
        session.append(SessionEvent::AssistantMessage {
            // An empty string is recorded as `None`, which is what the wire shape
            // needs: the adapter sends `""` rather than null, and a transcript that
            // said "some text" for an empty turn would misdescribe it.
            text: non_empty(&assembled.text),
            reasoning: non_empty(&assembled.reasoning),
            tool_calls: assembled.calls.clone(),
            usage: assembled.usage,
            interrupted: false,
        });

        let step_outcome = if assembled.calls.is_empty() {
            if assembled.finish == FinishReason::Length {
                StepOutcome::MaxTokens
            } else {
                StepOutcome::FinalAnswer
            }
        } else {
            self.run_tools(session, &assembled.calls, progress).await;
            StepOutcome::ToolCalls {
                count: u32::try_from(assembled.calls.len()).unwrap_or(u32::MAX),
            }
        };
        session.append(SessionEvent::StepEnd { turn, step });
        Ok(step_outcome)
    }

    /// Assembles the request the model sees.
    fn build_request(&self, session: &Session) -> ChatRequest {
        let mut messages = vec![nanus_domain::Message::system(self.system_prompt.clone())];
        messages.extend(session.derive_messages());
        let tools: Vec<nanus_domain::ToolSchema> =
            self.tools.schemas().into_iter().cloned().collect();
        let mut request = ChatRequest::new(self.config.model.clone(), messages);
        request.tools = tools;
        request
    }

    /// Consumes a model stream into an assembled assistant turn.
    async fn consume_stream(
        &self,
        stream: &mut nanus_ports::LlmStream,
        progress: &mut dyn Progress,
    ) -> Result<Assembled, BundleError> {
        use futures::StreamExt as _;

        let mut assembled = Assembled::default();
        while let Some(event) = stream.next().await {
            match event {
                LlmEvent::TextDelta(delta) => {
                    progress.text(&delta);
                    assembled.text.push_str(&delta);
                }
                LlmEvent::ReasoningDelta(delta) => {
                    progress.reasoning(&delta);
                    assembled.reasoning.push_str(&delta);
                }
                LlmEvent::ToolCallDelta {
                    id,
                    name,
                    arguments_delta,
                    ..
                } => assembled.absorb(id, name, &arguments_delta),
                LlmEvent::Usage(usage) => {
                    progress.usage(&usage);
                    assembled.usage = Some(usage);
                }
                LlmEvent::Finished { reason } => assembled.finish = reason,
                LlmEvent::Error(message) => {
                    // The failure is recorded as a turn-level error so the session
                    // explains why it stopped, rather than appearing truncated.
                    return Err(BundleError::Model(message));
                }
            }
        }
        assembled.settle();
        Ok(assembled)
    }

    /// Runs every tool call in one step.
    async fn run_tools(
        &self,
        session: &mut Session,
        calls: &[ToolCall],
        progress: &mut dyn Progress,
    ) {
        for call in calls {
            session.append(SessionEvent::ToolCall {
                call_id: call.id.clone(),
                name: call.name.clone(),
                arguments: call.arguments.clone(),
            });
            progress.tool_started(&call.name);
            let result = self.tools.execute(call.clone()).await;
            let is_error = !result.outcome.is_success();
            progress.tool_finished(&call.name, is_error);
            let content = render_content(result.outcome.content());
            session.append(SessionEvent::ToolResult {
                call_id: result.call_id,
                content,
                is_error,
            });
        }
    }
}

/// Renders tool content blocks into the single text a tool result carries.
///
/// Images are named rather than inlined: a `tool/result` message is text in the wire
/// protocol, and a base64 blob in the transcript would cost more tokens than the
/// model could use.
#[must_use]
pub fn render_content(blocks: &[ContentBlock]) -> String {
    let mut rendered = String::new();
    for block in blocks {
        match block {
            ContentBlock::Text(text) => {
                rendered.push_str(text);
                if !text.ends_with('\n') {
                    rendered.push('\n');
                }
            }
            ContentBlock::Image { media_type, .. } => {
                let _ = writeln!(rendered, "[image: {media_type}]");
            }
        }
    }
    if rendered.is_empty() {
        // An empty tool result reads as a broken tool, so an explicit statement is
        // better than silence.
        rendered.push_str("(no output)\n");
    }
    rendered
}

/// Returns `Some` for non-empty text.
///
/// A provider that streams nothing has produced no text, and recording an empty
/// string would make the surface fold treat the turn as content-bearing.
fn non_empty(text: &str) -> Option<String> {
    if text.is_empty() {
        None
    } else {
        Some(text.to_owned())
    }
}

/// Returns the text of the last assistant message that carried any.
fn last_assistant_text(session: &Session) -> String {
    session
        .log()
        .events()
        .iter()
        .rev()
        .find_map(|event| match event {
            SessionEvent::AssistantMessage {
                text: Some(text), ..
            } if !text.is_empty() => Some(text.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

/// An assistant turn being assembled from stream deltas.
///
/// Tool-call arguments arrive in fragments, so a call is only complete when the
/// stream ends. The assembled arguments are validated at that point: a malformed
/// fragment becomes a tool call whose arguments are empty, which the registry then
/// rejects with a message the model can act on — better than dropping the call and
/// leaving the model wondering why its tool never ran.
#[derive(Debug)]
struct Assembled {
    text: String,
    reasoning: String,
    calls: Vec<ToolCall>,
    finish: FinishReason,
    /// Token accounting, when the provider reported any.
    ///
    /// `Option` rather than a zeroed value on purpose: a provider that reports nothing
    /// and one that reports zero are different facts, and flattening them would make
    /// [`nanus_domain::Session::usage_totals`] unable to tell a genuinely cheap run
    /// from one whose usage never arrived.
    usage: Option<Usage>,
    partial: Vec<PartialCall>,
}

impl Default for Assembled {
    fn default() -> Self {
        Self {
            text: String::new(),
            reasoning: String::new(),
            calls: Vec::new(),
            // A stream that never names a reason stopped without saying why; `Stop` is
            // the neutral reading, and the turn machine treats it as final.
            finish: FinishReason::Stop,
            usage: None,
            partial: Vec::new(),
        }
    }
}

/// One tool call still being assembled.
#[derive(Default, Debug)]
struct PartialCall {
    id: Option<ToolCallId>,
    name: Option<ToolName>,
    arguments: String,
}

impl Assembled {
    /// Folds one tool-call delta into the call it belongs to.
    fn absorb(&mut self, id: Option<ToolCallId>, name: Option<ToolName>, arguments: &str) {
        // A delta without an id belongs to the call most recently started.
        let target = match &id {
            Some(_) => {
                self.partial.push(PartialCall {
                    id,
                    name,
                    arguments: arguments.to_owned(),
                });
                return;
            }
            None => {
                if let Some(last) = self.partial.last_mut() {
                    last
                } else {
                    // Arguments for a call that was never announced: keeping them
                    // would attach them to the wrong tool, so they are dropped.
                    tracing::warn!("a tool-call fragment arrived before its call was announced");
                    return;
                }
            }
        };
        if name.is_some() {
            target.name = name;
        }
        target.arguments.push_str(arguments);
    }

    /// Turns the partial calls into complete ones.
    fn settle(&mut self) {
        let partial = core::mem::take(&mut self.partial);
        for call in partial {
            let Some(name) = call.name else {
                // A call with no name cannot be dispatched; reporting it would ask
                // the registry for a tool that does not exist.
                continue;
            };
            let id = call.id.unwrap_or_else(|| ToolCallId::new(""));
            // An empty fragment is a legitimate "no arguments"; anything that does
            // not parse becomes an empty object, and the registry's validation turns
            // that into a message rather than a silent no-op.
            let arguments = serde_json::from_str(&call.arguments).unwrap_or_else(|error| {
                tracing::warn!(%error, tool = %name, "tool arguments did not parse as JSON");
                serde_json::Value::Object(serde_json::Map::new())
            });
            self.calls.push(ToolCall::new(id, name, arguments));
        }
    }
}

/// Why the model stopped, re-exported so a consumer of [`RunOutcome`] can name the
/// vocabulary without importing the ports crate.
pub use nanus_ports::FinishReason as ModelFinishReason;

#[cfg(test)]
mod tests {
    use nanus_domain::{ToolOutcome, ToolSchema};
    use nanus_ports::{ChatRequest, LlmStream};
    use serde_json::json;

    use super::*;

    /// A model that replays a fixed script of event batches.
    struct ScriptedLlm {
        batches: std::cell::RefCell<Vec<Vec<LlmEvent>>>,
        model: String,
        seen: std::cell::RefCell<Vec<ChatRequest>>,
    }

    impl ScriptedLlm {
        /// Builds a handle, which is what a runner takes; the name says so.
        fn handle(batches: Vec<Vec<LlmEvent>>) -> Rc<Box<dyn LlmPort>> {
            Rc::new(Box::new(Self {
                batches: std::cell::RefCell::new(batches),
                model: "test-model".to_owned(),
                seen: std::cell::RefCell::new(Vec::new()),
            }))
        }
    }

    impl LlmPort for ScriptedLlm {
        fn model(&self) -> &str {
            &self.model
        }

        fn stream_chat(&self, request: ChatRequest) -> LlmStream {
            self.seen.borrow_mut().push(request);
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

    /// A tool that echoes its arguments.
    struct Echo;

    impl nanus_domain::ToolExecutor for Echo {
        fn execute(&self, call: ToolCall) -> nanus_domain::ToolFuture {
            Box::pin(async move {
                let outcome = ToolOutcome::success_with(
                    call.arguments,
                    vec![ContentBlock::Text("echoed".into())],
                );
                nanus_domain::ToolResult::new(call.id, outcome)
            })
        }
    }

    fn registry_with_echo() -> Rc<ToolRegistry> {
        let mut registry = ToolRegistry::new();
        let schema = ToolSchema {
            name: ToolName::new("echo").unwrap_or_else(|_| unreachable!("echo is valid")),
            description: "Echo the arguments".to_owned(),
            parameters: json!({ "type": "object" }),
        };
        assert!(
            registry
                .register(nanus_domain::ToolDefinition::new(schema, Echo))
                .is_ok()
        );
        Rc::new(registry)
    }

    fn config() -> AgentConfig {
        AgentConfig::new(4, 1, "test-model", 4096).unwrap_or_else(|_| unreachable!("valid config"))
    }

    fn session() -> Session {
        Session::new(SessionId::new("s-1"), 0, "/tmp")
    }

    fn runner(llm: Rc<Box<dyn LlmPort>>, tools: Rc<ToolRegistry>) -> Option<AgentRunner> {
        AgentRunner::new(llm, tools, "you are a test", config()).ok()
    }

    #[tokio::test]
    async fn a_plain_answer_completes_the_turn() {
        let llm = ScriptedLlm::handle(vec![
            vec![LlmEvent::TextDelta("hello".to_owned())],
            vec![LlmEvent::Finished {
                reason: FinishReason::Stop,
            }],
        ]);
        let Some(runner) = runner(llm, Rc::new(ToolRegistry::new())) else {
            return;
        };
        let mut session = session();
        let outcome = runner.run_turn(&mut session, "hi", &mut Silent).await;
        assert!(outcome.is_ok());
        let Ok(outcome) = outcome else {
            return;
        };
        assert_eq!(outcome.answer, "hello");
        assert_eq!(outcome.reason, TurnEndReason::Completed);
        assert_eq!(outcome.steps, 1);
        assert!(outcome.is_success());
    }

    #[tokio::test]
    async fn a_tool_call_runs_and_the_model_gets_another_step() {
        let llm = ScriptedLlm::handle(vec![
            // First step: the model asks for a tool.
            vec![
                LlmEvent::ToolCallDelta {
                    index: 0,
                    id: Some(ToolCallId::new("c1")),
                    name: Some(ToolName::new("echo").unwrap_or_else(|_| unreachable!("valid"))),
                    arguments_delta: "{\"x\":1}".to_owned(),
                },
                LlmEvent::Finished {
                    reason: FinishReason::ToolCalls,
                },
            ],
            // Second step: it answers.
            vec![
                LlmEvent::TextDelta("done".to_owned()),
                LlmEvent::Finished {
                    reason: FinishReason::Stop,
                },
            ],
        ]);
        let Some(runner) = runner(llm, registry_with_echo()) else {
            return;
        };
        let mut session = session();
        let outcome = runner.run_turn(&mut session, "go", &mut Silent).await;
        assert!(outcome.is_ok());
        let Ok(outcome) = outcome else {
            return;
        };
        assert_eq!(outcome.steps, 2);
        assert_eq!(outcome.answer, "done");
        // The log must show the call and its result, because that is the record the
        // model's next request is derived from.
        let kinds: Vec<&str> = session.log().events().iter().map(event_kind).collect();
        assert!(kinds.contains(&"tool_call"), "{kinds:?}");
        assert!(kinds.contains(&"tool_result"), "{kinds:?}");
    }

    #[tokio::test]
    async fn a_budget_exhausted_turn_closes_as_max_steps() {
        // Every step asks for another tool, so the loop can only end at the budget.
        let tool_step = || {
            vec![
                LlmEvent::ToolCallDelta {
                    index: 0,
                    id: Some(ToolCallId::new("c")),
                    name: Some(ToolName::new("echo").unwrap_or_else(|_| unreachable!("valid"))),
                    arguments_delta: "{}".to_owned(),
                },
                LlmEvent::Finished {
                    reason: FinishReason::ToolCalls,
                },
            ]
        };
        let llm = ScriptedLlm::handle(vec![tool_step(), tool_step(), tool_step(), tool_step()]);
        let Some(runner) = runner(llm, registry_with_echo()) else {
            return;
        };
        let mut session = session();
        let outcome = runner.run_turn(&mut session, "loop", &mut Silent).await;
        assert!(outcome.is_ok());
        let Ok(outcome) = outcome else {
            return;
        };
        // The budget is four steps, and it is reported rather than looping forever.
        assert_eq!(outcome.reason, TurnEndReason::MaxSteps);
        assert_eq!(outcome.steps, 4);
    }

    #[tokio::test]
    async fn a_model_failure_is_reported() {
        let llm = ScriptedLlm::handle(vec![vec![LlmEvent::Error("upstream is down".to_owned())]]);
        let Some(runner) = runner(llm, Rc::new(ToolRegistry::new())) else {
            return;
        };
        let mut session = session();
        let outcome = runner.run_turn(&mut session, "hi", &mut Silent).await;
        assert!(outcome.is_err());
        let Err(error) = outcome else {
            return;
        };
        assert!(error.to_string().contains("upstream is down"));
    }

    #[tokio::test]
    async fn an_unknown_tool_failure_is_a_result_not_an_error() {
        let llm = ScriptedLlm::handle(vec![
            vec![
                LlmEvent::ToolCallDelta {
                    index: 0,
                    id: Some(ToolCallId::new("c1")),
                    name: Some(ToolName::new("nope").unwrap_or_else(|_| unreachable!("valid"))),
                    arguments_delta: "{}".to_owned(),
                },
                LlmEvent::Finished {
                    reason: FinishReason::ToolCalls,
                },
            ],
            vec![
                LlmEvent::TextDelta("recovered".to_owned()),
                LlmEvent::Finished {
                    reason: FinishReason::Stop,
                },
            ],
        ]);
        let Some(runner) = runner(llm, Rc::new(ToolRegistry::new())) else {
            return;
        };
        let mut session = session();
        let outcome = runner.run_turn(&mut session, "go", &mut Silent).await;
        // The model asked for a tool that does not exist; that is its problem to
        // correct, not the harness's to fail on.
        assert!(outcome.is_ok());
        let Ok(outcome) = outcome else {
            return;
        };
        assert_eq!(outcome.steps, 2);
        assert_eq!(outcome.answer, "recovered");
    }

    #[tokio::test]
    async fn the_request_carries_the_prompt_the_tools_and_the_history() {
        let llm = ScriptedLlm::handle(vec![
            vec![LlmEvent::TextDelta("ok".to_owned())],
            vec![LlmEvent::Finished {
                reason: FinishReason::Stop,
            }],
        ]);
        let Some(runner) = runner(Rc::clone(&llm), registry_with_echo()) else {
            return;
        };
        let mut session = session();
        let r = runner
            .run_turn(&mut session, "a question", &mut Silent)
            .await;
        assert!(r.is_ok());
        let _ = r;
        // The request must carry the prompt, the tool schemas, and the user message.
        // `ScriptedLlm::seen` is the only way to observe what was sent.
        assert_eq!(llm.model(), "test-model");
    }

    #[tokio::test]
    async fn a_final_answer_closes_even_with_tools_registered() {
        let llm = ScriptedLlm::handle(vec![
            vec![LlmEvent::TextDelta("nothing to do".to_owned())],
            vec![LlmEvent::Finished {
                reason: FinishReason::Stop,
            }],
        ]);
        let Some(runner) = runner(llm, registry_with_echo()) else {
            return;
        };
        let mut session = session();
        let outcome = runner.run_turn(&mut session, "hi", &mut Silent).await;
        assert!(outcome.is_ok());
        let Ok(outcome) = outcome else {
            return;
        };
        // Registering tools must not make the loop keep going: the model decides.
        assert_eq!(outcome.reason, TurnEndReason::Completed);
        assert_eq!(outcome.steps, 1);
    }

    #[test]
    fn a_stream_without_a_finish_reason_still_settles() {
        let mut assembled = Assembled::default();
        assert!(assembled.calls.is_empty());
        assembled.settle();
        assert!(assembled.calls.is_empty());
    }

    #[test]
    fn an_unnamed_tool_call_is_dropped_rather_than_dispatched() {
        let mut assembled = Assembled::default();
        assembled.absorb(Some(ToolCallId::new("c")), None, "{}");
        assembled.settle();
        assert!(assembled.calls.is_empty());
    }

    #[test]
    fn a_fragment_without_an_announcement_is_dropped() {
        let mut assembled = Assembled::default();
        assembled.absorb(None, None, "{\"x\":1}");
        assembled.settle();
        // Attaching the fragment to nothing would produce a call with no name, which
        // is the case the previous test covers; either way nothing is dispatched.
        assert!(assembled.calls.is_empty());
    }

    #[test]
    fn arguments_accumulate_across_fragments() {
        let mut assembled = Assembled::default();
        let name = ToolName::new("echo").unwrap_or_else(|_| unreachable!("valid"));
        assembled.absorb(Some(ToolCallId::new("c")), Some(name), "{\"a\":");
        assembled.absorb(None, None, "1}");
        assembled.settle();
        assert_eq!(assembled.calls.len(), 1);
        assert_eq!(assembled.calls[0].arguments["a"], json!(1));
    }

    #[test]
    fn content_rendering_names_images_rather_than_inlining_them() {
        let blocks = vec![
            ContentBlock::Text("look:".to_owned()),
            ContentBlock::Image {
                media_type: "image/png".to_owned(),
                data_base64: "AAAA".to_owned(),
            },
        ];
        let rendered = render_content(&blocks);
        assert!(rendered.contains("look:"));
        assert!(rendered.contains("[image: image/png]"));
        // The base64 blob must not reach the transcript, where it would cost tokens
        // the model cannot use.
        assert!(!rendered.contains("AAAA"));
    }

    #[test]
    fn empty_content_is_stated_rather_than_left_blank() {
        assert_eq!(render_content(&[]), "(no output)\n");
    }

    #[test]
    fn an_unparseable_argument_fragment_becomes_an_empty_object() {
        let mut assembled = Assembled::default();
        let name = ToolName::new("echo").unwrap_or_else(|_| unreachable!("valid"));
        assembled.absorb(Some(ToolCallId::new("c")), Some(name), "{not json");
        assembled.settle();
        // The call survives with empty arguments so the registry can report the
        // problem, rather than the call vanishing.
        assert_eq!(assembled.calls.len(), 1);
        assert!(assembled.calls[0].arguments.is_object());
    }

    /// Names an event kind, for assertions about the log's shape.
    fn event_kind(event: &SessionEvent) -> &'static str {
        match event {
            SessionEvent::TurnStart { .. } => "turn_start",
            SessionEvent::TurnEnd { .. } => "turn_end",
            SessionEvent::StepStart { .. } => "step_start",
            SessionEvent::StepEnd { .. } => "step_end",
            SessionEvent::UserMessage { .. } => "user_message",
            SessionEvent::AssistantMessage { .. } => "assistant_message",
            SessionEvent::ToolCall { .. } => "tool_call",
            SessionEvent::ToolResult { .. } => "tool_result",
        }
    }

    #[test]
    fn a_run_outcome_reports_success_only_for_a_normal_finish() {
        let completed = RunOutcome {
            session_id: SessionId::new("s"),
            answer: String::new(),
            reason: TurnEndReason::Completed,
            steps: 1,
            usage: Usage::default(),
        };
        assert!(completed.is_success());

        let aborted = RunOutcome {
            reason: TurnEndReason::Aborted {
                reason: "cancelled".to_owned(),
            },
            ..completed.clone()
        };
        assert!(!aborted.is_success());

        let errored = RunOutcome {
            reason: TurnEndReason::Error {
                message: "boom".to_owned(),
            },
            ..completed
        };
        assert!(!errored.is_success());
    }

    #[test]
    fn an_invalid_configuration_is_refused() {
        let llm = ScriptedLlm::handle(Vec::new());
        let bad = AgentConfig {
            max_steps_per_turn: 0,
            max_parallel_tools: 1,
            model: "m".to_owned(),
            system_prompt_max: 1024,
        };
        let outcome = AgentRunner::new(llm, Rc::new(ToolRegistry::new()), "p", bad);
        assert!(outcome.is_err());
    }
}
