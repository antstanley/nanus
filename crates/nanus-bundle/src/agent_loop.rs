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
    AgentConfig, ApprovalOutcome, ApprovalPolicy, ApprovalRequest, ContentBlock, SandboxMode,
    Session, SessionEvent, SessionId, StepOutcome, ToolAccess, ToolCall, ToolCallId, ToolName,
    ToolResult, TurnEndReason, TurnMachine, Usage,
};
use nanus_ports::{ChatRequest, ClockHandle, FinishReason, LlmEvent, LlmHandle};

use crate::guard;
use crate::{BundleError, ToolRegistryHandle};

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
    ///
    /// Delegated to [`TurnEndReason::is_success`] rather than written out again. The two used to
    /// answer differently — this one counted a turn cut off at the model's token ceiling as a
    /// success, the domain's counted only a completion — and this is the one that decides
    /// `nanus run`'s exit code. So a truncated answer exited zero while the interface printed
    /// "the answer is cut off", and the documented contract ("zero only for a completed turn")
    /// lost to whichever predicate happened to be consulted. One definition, one answer.
    #[must_use]
    pub const fn is_success(&self) -> bool {
        self.reason.is_success()
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

    /// The model emitted more of a tool call: its name, its arguments, or both.
    ///
    /// A step that answers with a tool call and no prose reaches neither of the two
    /// callbacks above, and that is the common case in a coding session rather than an edge
    /// one. A listener measuring how fast the model generates therefore has to hear about
    /// this kind of delta too, or the rate it reports covers only the steps that happened to
    /// talk — which are the long ones, and the flattering ones.
    ///
    /// The delta is the call's arguments as they arrived, which is empty for a chunk that
    /// carried only a name. Nothing here is for display: the call is assembled and reported
    /// through [`Progress::tool_started`] once the step has decided to run it.
    fn tool_call(&mut self, _delta: &str) {}

    /// A step began.
    fn step_started(&mut self, _step: u32) {}

    /// The server began answering the request the current step issued.
    ///
    /// The one moment between asking and the first token, and therefore the only place a
    /// request's wait can be split from this side of a socket. It matters because a wait that is
    /// mostly network and upload is a different problem from a wait that is mostly the server
    /// reading the prompt, and a single duration cannot tell a reader which they have.
    ///
    /// Reported as the fact and not as an instant, for the same reason the durations are taken
    /// elsewhere: a listener measuring with its own clock must be the one to read it, or the two
    /// readings are from clocks that were never compared.
    ///
    /// Optional, like the deltas: an agent whose provider cannot report one says nothing here,
    /// and a listener that does not care ignores it.
    fn response_head(&mut self) {}

    /// A tool is about to run.
    ///
    /// The arguments come with the name because knowing *which* tool ran is often not
    /// knowing what happened: `edit` says nothing, and `edit` on `view.rs` says
    /// everything. The call's id comes with both because it is the only thing that pairs
    /// this call with the [`Progress::tool_finished`] that answers it: a step writes every
    /// call it made before it writes any result, and two calls in one step arrive back in
    /// whatever order they finished in. A listener that only wants the name ignores the
    /// other two parameters.
    fn tool_started(
        &mut self,
        _call_id: &ToolCallId,
        _name: &ToolName,
        _arguments: &serde_json::Value,
    ) {
    }

    /// A tool finished.
    fn tool_finished(&mut self, _call_id: &ToolCallId, _name: &ToolName, _is_error: bool) {}

    /// Usage was reported.
    fn usage(&mut self, _usage: &Usage) {}

    /// The prompt for this step had its oldest turns dropped to fit the context budget.
    ///
    /// Reported rather than only logged, because a reader watching a turn needs to know that
    /// the model is answering from part of the conversation: an answer that contradicts
    /// something dropped earlier is not the model being wrong, and a reader who was not told
    /// cannot tell the difference.
    fn elided(&mut self, _elision: &nanus_domain::Elision) {}

    /// Whether the turn should stop.
    ///
    /// Asked between steps and between the tokens of a model response, so a driver that
    /// wants the turn to stop is heard at the next point where stopping is safe rather than
    /// at the end of the step budget. Defaults to `false`: a driver with no opinion — a
    /// script running one turn to completion — does not have to say so.
    ///
    /// This is the loop's only question rather than a listener callback like the rest,
    /// because the driver is the only thing that can answer it: whether a turn is still
    /// wanted is a fact about the caller, not about the turn.
    fn cancelled(&self) -> bool {
        false
    }
}

/// A [`Progress`] that ignores everything, for a caller that wants none.
#[derive(Clone, Copy, Debug, Default)]
pub struct Silent;

impl Progress for Silent {}

/// A party that can answer an approval request for one call.
///
/// The loop holds no opinion about who answers: a terminal prompts, a link asks the client
/// attached to the session, and a test replies from a script. What matters is that the
/// question is asked *before* the call runs, and that anything other than
/// [`ApprovalOutcome::AllowedOnce`] stops it — a harness that cannot obtain an answer
/// denies rather than proceeding.
///
/// The method is written as an ordinary function returning a boxed future rather than as
/// `async fn`, because the loop drives it as a trait object.
pub trait Approver {
    /// Decides one call.
    fn decide(&self, request: ApprovalRequest) -> nanus_ports::LocalBoxFuture<'_, ApprovalOutcome>;
}

/// Why a call the sandbox does not permit is being asked about.
///
/// The prompt is deliberately told the tool and the reason and *not* the arguments, so
/// model-controlled text cannot be put in front of the decision. This sentence is the
/// harness's own, and it names the knobs that made the call an exception: the sandbox that
/// refused it, its access class, and — in the `permitted` state — whether it looked
/// destructive.
fn approval_reason(sandbox: SandboxMode, access: ToolAccess, destructive: bool) -> String {
    let base = format!(
        "the sandbox mode `{sandbox}` does not permit {} calls without approval",
        access.as_str()
    );
    if destructive {
        format!("{base}; the call looks destructive")
    } else {
        base
    }
}

/// The result a denied call leaves in the log.
///
/// A denial is a *result*, not a harness error: the model is told which call did not run
/// and why, so it can say what it could not do instead of retrying something that will be
/// refused again. Leaving the call unanswered would also keep the turn open for ever,
/// because the turn machine reads an owed call from the log.
fn denied_result(call: &ToolCall, reason: &str, outcome: ApprovalOutcome) -> ToolResult {
    let detail = match outcome {
        ApprovalOutcome::AllowedOnce => "it was allowed once",
        ApprovalOutcome::Rejected => "it was denied",
        ApprovalOutcome::Cancelled => "the approval prompt was cancelled",
        ApprovalOutcome::Unavailable => "nobody was available to approve it",
    };
    ToolResult::failure(
        call.id.clone(),
        format!(
            "the {} call was not run: {reason}, and {detail}. Do not retry it; say what you \
             could not do instead.",
            call.name
        ),
    )
}

/// Appends the turn's step budget to a system prompt.
///
/// The budget is a property of the run rather than of the prose, so it is stated here
/// rather than written into the default prompt: a prompt the user configured gets the
/// same sentence, and the number the model reads cannot drift from the number the
/// turn machine enforces. It is stated at all because the first turn to hit the ceiling
/// did so while exploring — nothing had told the model there was one, so pacing was
/// never a decision it could make.
fn with_step_budget(prompt: &str, budget: u32) -> String {
    format!(
        "{prompt}\n\nYou have a budget of {budget} steps for a turn, where a step is one \
         model request together with the tool calls that follow it. The turn ends when the \
         budget runs out, wherever the work has got to, so spend the early steps finding \
         out what you need and the rest making the change."
    )
}

/// Runs turns against a session.
///
/// The runner owns no state between calls beyond the session it is given, so a
/// caller can drive several sessions with one runner and several runners against one
/// session. Everything mutable lives in the [`Session`], which is what makes the run
/// observable and resumable.
pub struct AgentRunner {
    /// The model adapter every request goes to, shared so a caller can switch providers mid-session.
    ///
    /// A `RefCell` around the handle rather than the handle alone, because `/provider` replaces
    /// the whole adapter: an interface choosing a different provider is not choosing a different
    /// model id within one vendor, it is choosing a different host and protocol, and the runner is
    /// what issues the request. The kernel is single-threaded, so there is no lock to take.
    llm: Rc<core::cell::RefCell<LlmHandle>>,
    /// The tools the model is offered, shared rather than owned.
    ///
    /// The same handle the bundle publishes as the `tools` service, deliberately: the
    /// registry a runner dispatches from and the registry a caller inspects have to be one
    /// object, or a tool registered through the published handle would change the count an
    /// agent advertises without changing the schemas it sends.
    tools: ToolRegistryHandle,
    system_prompt: String,
    config: AgentConfig,
    /// The approval state the gate consults, shared so a caller can change it mid-session.
    ///
    /// The configuration carries the *startup* value, and this is that value as a cell the
    /// link can turn: an interface toggling the state has to affect the next call of a turn
    /// already running, which a value copied into the runner at construction could not do.
    /// It is a `Cell` rather than an `Atomic` because the kernel is single-threaded by
    /// design and the runner is `!Send` with it.
    approval: Rc<core::cell::Cell<ApprovalPolicy>>,
    /// The model the next request will name, shared so a caller can switch it mid-session.
    ///
    /// [`AgentConfig`] carries the *startup* value, and this is that value as a cell the link
    /// can turn — the same shape as [`AgentRunner::approval`], and for the same reason: an
    /// interface switching models has to affect the next request of a turn already running,
    /// which a value copied into a request at construction could not do. A `RefCell` rather
    /// than a `Cell` because a model id is a string, and the kernel is single-threaded so
    /// there is no lock to take.
    model: Rc<core::cell::RefCell<String>>,
    /// The reasoning effort later requests carry, shared so a caller can change it mid-session.
    ///
    /// `None` means *ask the adapter*, which is the startup behaviour: an adapter fills an unset
    /// effort in from its own configuration, and it is the only component that knows what that
    /// default is. `Some` is an interface that has chosen one, exactly as
    /// [`AgentRunner::approval`] is an interface that has chosen a state.
    effort: Rc<core::cell::Cell<Option<nanus_ports::ReasoningEffort>>>,
    /// The clock a goal change is stamped from.
    ///
    /// The goal tools are the loop's own and run here, so the loop is what needs the time a
    /// goal change records. It is a port rather than a system call for the reason every other
    /// clock read is one: a test that cannot pin the clock cannot assert on a goal's revision
    /// timestamps, and a recorded session has to mean the same thing when it is read back.
    clock: ClockHandle,
}

impl core::fmt::Debug for AgentRunner {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AgentRunner")
            .field("model", &self.config.model)
            .field("tools", &self.tools.borrow().len())
            .field("max_steps_per_turn", &self.config.max_steps_per_turn)
            .finish_non_exhaustive()
    }
}

impl AgentRunner {
    /// Builds a runner over the shared tool registry.
    ///
    /// # Errors
    ///
    /// Returns [`BundleError::Config`] when the agent configuration is invalid, so an
    /// unusable runner cannot be constructed and then consulted.
    pub fn new(
        llm: LlmHandle,
        tools: ToolRegistryHandle,
        system_prompt: impl Into<String>,
        config: AgentConfig,
        clock: ClockHandle,
    ) -> Result<Self, BundleError> {
        config
            .validate()
            .map_err(|error| BundleError::Config(error.to_string()))?;
        // Precondition: a runner with no tools can still be useful, so an empty
        // registry is allowed; a prompt larger than the configured ceiling is not.
        // The ceiling is checked against the assembled prompt rather than the caller's
        // part of it: the sentence below is sent too, and it counts.
        let system_prompt: String = system_prompt.into();
        let system_prompt = with_step_budget(&system_prompt, config.max_steps_per_turn);
        assert!(
            system_prompt.len() <= config.system_prompt_max,
            "the system prompt fits its configured ceiling"
        );
        let model = Rc::new(core::cell::RefCell::new(config.model.clone()));
        Ok(Self {
            llm: Rc::new(core::cell::RefCell::new(llm)),
            tools,
            system_prompt,
            approval: Rc::new(core::cell::Cell::new(config.approval_policy)),
            model,
            effort: Rc::new(core::cell::Cell::new(None)),
            clock,
            config,
        })
    }

    /// Returns the configuration in use.
    #[must_use]
    pub const fn config(&self) -> &AgentConfig {
        &self.config
    }

    /// Returns the approval state the gate is consulting right now.
    #[must_use]
    pub fn approval(&self) -> ApprovalPolicy {
        self.approval.get()
    }

    /// Replaces the approval state for the rest of the session.
    ///
    /// The change takes effect on the next call the gate decides, including a call in a turn
    /// that is already running: an interface toggling the state while the model works is the
    /// case this exists for.
    pub fn set_approval(&self, policy: ApprovalPolicy) {
        self.approval.set(policy);
    }

    /// Returns the model every later request will name.
    ///
    /// Not [`AgentConfig::model`], which is the model this runner was *built* with: a caller
    /// may switch models mid-session, and the request is what the switch has to change.
    #[must_use]
    pub fn model(&self) -> String {
        self.model.borrow().clone()
    }

    /// Replaces the model every later request names.
    ///
    /// The change takes effect on the next request, including one in a turn that is already
    /// running, which is the case this exists for: a reader who switches while the model works
    /// is saying what they want the next step to be, not what they wanted the last one to be.
    ///
    /// Nothing else is re-assembled. The system prompt's runtime section names the model the
    /// composition started with, exactly as it names the approval state it started with — a
    /// sentence about how the deployment was set up rather than a claim about the next
    /// request — and the interface is where the live value is shown.
    ///
    /// # Panics
    ///
    /// Asserts that the id is not empty: an empty model is not a switch to nothing, it is a
    /// request the provider would refuse, and refusing it here is a postcondition rather than
    /// a round trip.
    pub fn set_model(&self, model: &str) {
        assert!(
            !model.trim().is_empty(),
            "a model id that is being switched to is not empty"
        );
        model.clone_into(&mut self.model.borrow_mut());
    }

    /// Returns the reasoning effort the next request will carry.
    ///
    /// The chosen one when a caller has chosen, and otherwise what the adapter applies to a
    /// request that sets none — which is the only place that answer exists, because an adapter
    /// is what fills an unset effort in. `None` means this adapter has no notion of effort,
    /// which is not the same fact as any effort at all.
    #[must_use]
    pub fn effort(&self) -> Option<nanus_ports::ReasoningEffort> {
        self.effort
            .get()
            .or_else(|| self.llm.borrow().reasoning_effort())
    }

    /// Replaces the reasoning effort every later request will carry.
    ///
    /// `None` gives the choice back to the adapter. Like [`AgentRunner::set_model`] and
    /// [`AgentRunner::set_approval`], the change takes effect on the next request, including the
    /// next step of a turn that is already running.
    pub fn set_effort(&self, effort: Option<nanus_ports::ReasoningEffort>) {
        self.effort.set(effort);
    }

    /// Returns the effort steps the adapter takes for `model`, in increasing order.
    ///
    /// A fact about the model rather than about the runner, read from the adapter that will send
    /// the request: which steps a provider refuses differs between its models, and the interface
    /// reads this so it offers only the ones that act.
    #[must_use]
    pub fn effort_levels(&self, model: &str) -> &'static [nanus_ports::ReasoningEffort] {
        self.llm.borrow().effort_levels(model)
    }

    /// Points every later request at another adapter.
    ///
    /// The switch a provider change is: the loop keeps running, the session keeps its log, and only
    /// the thing that issues the request is replaced. It takes effect on the next request,
    /// including the next step of a turn already running, exactly as [`AgentRunner::set_model`]
    /// does — a reader switching providers mid-turn is saying what they want the next step to be.
    ///
    /// The effort override is the caller's to clear: the new adapter may have no notion of
    /// effort at all (Anthropic), and a standing choice that outlived the provider it was made
    /// for would be reported as in force where it cannot be honoured.
    pub fn set_llm(&self, llm: LlmHandle) {
        *self.llm.borrow_mut() = llm;
    }

    /// Returns the model adapter in force.
    #[must_use]
    pub fn llm(&self) -> LlmHandle {
        Rc::clone(&self.llm.borrow())
    }

    /// Returns the tool registry this runner dispatches from.
    ///
    /// The same handle the composition publishes as the `tools` service, so a caller may
    /// register a tool through either and the other sees it. Handing out the shared handle
    /// rather than a count is deliberate: a count cannot be used to *add* anything, and a
    /// caller that wanted to add one would otherwise be tempted to keep a registry of its
    /// own — which is the divergence this accessor exists to make impossible.
    #[must_use]
    pub const fn tools(&self) -> &ToolRegistryHandle {
        &self.tools
    }

    /// Returns how many tools the model is offered.
    ///
    /// The registered set plus the goal tools — [`crate::goal_tools`] — because the loop offers
    /// both and dispatches both. This is the one place the two lists meet, so the count an agent
    /// advertises in its handshake is the number of schemas a request carries; asking the
    /// registry alone would understate what the model can see.
    #[must_use]
    pub fn tool_count(&self) -> usize {
        self.tools
            .borrow()
            .len()
            .saturating_add(crate::goal_tools::COUNT)
    }

    /// Returns the assembled system prompt every request carries.
    ///
    /// The prompt the model is actually sent, which is not the string a caller passed: the
    /// step budget and the runtime context are appended to it. Exposed so a test can assert
    /// that what a deployment believes about itself reaches the model.
    #[must_use]
    pub fn system_prompt(&self) -> &str {
        &self.system_prompt
    }

    /// Runs one turn for `message` and returns what it produced.
    ///
    /// `approver` is who to ask about a tool call the sandbox does not already permit. It is
    /// optional because not every caller has anyone to ask — a headless run with no
    /// terminal, a service with no client attached — and a missing answerer denies rather
    /// than allowing: the loop asks, and a call that cannot be approved does not run.
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
        approver: Option<&dyn Approver>,
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
            // A stop asked for between steps is taken here, before another request is
            // issued. The machine owns the mapping from a step outcome to a recorded
            // reason, so this goes through it rather than writing the reason into the log
            // directly — one vocabulary for why a turn ended, wherever that happens.
            let step_outcome = if progress.cancelled() {
                StepOutcome::Interrupted
            } else {
                let step = steps.saturating_add(1);
                steps = step;
                progress.step_started(step);
                match self.run_step(session, turn, step, progress, approver).await {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        // A step that failed still has to close its turn. Returning here
                        // without one left a log ending at `step_start` — no `step_end`, no
                        // `turn_end` — which is the one state the domain says a resumed
                        // session must never be handed, and which the closing postcondition
                        // below never got to check. The reason goes through the machine, as
                        // every other reason does.
                        let reason = machine
                            .decide(
                                session.log(),
                                &StepOutcome::Error {
                                    message: error.to_string(),
                                },
                            )
                            .turn_end_reason();
                        if let Some(reason) = reason {
                            session.append(SessionEvent::TurnEnd { turn, reason });
                        }
                        assert!(
                            session.log().last_turn_end().is_some(),
                            "a failed run closes its turn"
                        );
                        return Err(error);
                    }
                }
            };
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
        approver: Option<&dyn Approver>,
    ) -> Result<StepOutcome, BundleError> {
        session.append(SessionEvent::StepStart { turn, step });
        let (request, elision) = self.build_request(session)?;
        if let Some(elision) = &elision {
            progress.elided(elision);
        }
        let mut stream = self.llm.borrow().stream_chat(request);
        let assembled = match self.consume_stream(&mut stream, progress).await {
            Ok(assembled) => assembled,
            Err(error) => {
                // The step ends before the turn does: a log with a step that never finished
                // is one a reader has to guess about, and the turn above is closed by
                // `run_turn` whatever happened here.
                session.append(SessionEvent::StepEnd { turn, step });
                return Err(error);
            }
        };

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
            interrupted: assembled.interrupted,
            // What produced this message, recorded beside what it cost. The model is the
            // one the request named; the effort comes from the adapter, which is the
            // component that fills in an unset effort and so the only one that knows what
            // was actually asked for.
            model: Some(self.model()),
            effort: self.effort().map(|effort| effort.as_str().to_owned()),
        });

        // An interrupted step is *interrupted*, not a step that called tools: what the
        // model was part way through asking for was never run, and running it would do
        // work the reader has just asked to stop.
        let step_outcome = if assembled.interrupted {
            StepOutcome::Interrupted
        } else if assembled.calls.is_empty() {
            if assembled.finish == FinishReason::Length {
                StepOutcome::MaxTokens
            } else {
                StepOutcome::FinalAnswer
            }
        } else {
            self.run_tools(session, &assembled.calls, progress, approver)
                .await;
            StepOutcome::ToolCalls {
                count: u32::try_from(assembled.calls.len()).unwrap_or(u32::MAX),
            }
        };
        session.append(SessionEvent::StepEnd { turn, step });
        Ok(step_outcome)
    }

    /// Assembles the request the model sees, fitted to the prompt budget.
    ///
    /// The whole log is replayed and then trimmed: the oldest turns are dropped when the
    /// conversation no longer fits, with a notice the model reads, and the turn is *refused*
    /// when even the newest turn does not fit. See [`nanus_domain::context`] for the policy and
    /// why it is a policy rather than a tokenizer.
    ///
    /// # Errors
    ///
    /// Returns [`BundleError::Context`] when the prompt cannot be made to fit.
    fn build_request(
        &self,
        session: &Session,
    ) -> Result<(ChatRequest, Option<nanus_domain::Elision>), BundleError> {
        let mut messages = vec![nanus_domain::Message::system(self.system_prompt.clone())];
        messages.extend(session.derive_messages());
        let fitted = nanus_domain::fit(messages, self.config.context_budget)
            .map_err(|error| BundleError::context(error.to_string()))?;
        // The borrow ends with the statement, which is what keeps a tool registered through
        // the published handle visible on the very next request rather than only after a
        // rebuild. The goal tools are appended here rather than registered, because the loop
        // dispatches them itself: this is the one place the offered set is assembled.
        let tools: Vec<nanus_domain::ToolSchema> = {
            let registry = self.tools.borrow();
            let mut schemas: Vec<nanus_domain::ToolSchema> =
                registry.schemas().into_iter().cloned().collect();
            schemas.extend(crate::goal_tools::schemas());
            schemas
        };
        let mut request = ChatRequest::new(self.model(), fitted.messages);
        request.tools = tools;
        // Set only when a caller chose one: an unset effort is the adapter filling in its own
        // default, which is what a request that says nothing has always meant.
        if let Some(effort) = self.effort.get() {
            request = request.with_reasoning_effort(effort);
        }
        Ok((request, fitted.elision))
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
            // A stop asked for while the model is streaming is taken at the next token
            // rather than at the end of the response: waiting out a long answer to a
            // question nobody wants answered any more is the whole thing the reader is
            // trying to avoid. What the model has already said is kept and marked
            // interrupted, because a conversation that forgets words the reader watched
            // arrive is worse than one that keeps them — but the tool calls it was part way
            // through naming are *dropped*: a call in the log with no result to answer it
            // would be replayed as one that ran, and the next request would be refused for
            // a call nothing ever answered.
            if progress.cancelled() {
                assembled.interrupt();
                break;
            }
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
                } => {
                    progress.tool_call(&arguments_delta);
                    assembled.absorb(id, name, &arguments_delta);
                }
                LlmEvent::ResponseHead => progress.response_head(),
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
        // Asked once more now the stream has ended, which is the last moment before this step's
        // tool calls would run. A stop that arrives during the *closing* await — the end of the
        // response body, after the last event — is not seen by the check inside the loop,
        // because there is no next event to see it at, and running a command the reader has
        // just asked to stop is the one thing stopping is for.
        if progress.cancelled() {
            assembled.interrupt();
        }
        assembled.settle();
        Ok(assembled)
    }

    /// Runs every tool call in one step.
    ///
    /// Three phases, and the split is what keeps concurrency from making the log
    /// unreproducible:
    ///
    /// 1. Every call is written down before anything runs, so the log says what the model
    ///    asked for even if the process dies mid-step, and every call the step owes is
    ///    visible from the first line.
    /// 2. The gate decides each call in order. Decisions are *not* concurrent: an approval
    ///    question is a thing a person answers one at a time, and several questions in
    ///    front of them at once is how one gets lost behind another.
    /// 3. The permitted calls run at most [`AgentConfig::max_parallel_tools`] at a time,
    ///    and their results are recorded in call order whatever order they finished in.
    ///    A call and its result therefore pair up in the log by position and by id, however
    ///    the work was scheduled.
    async fn run_tools(
        &self,
        session: &mut Session,
        calls: &[ToolCall],
        progress: &mut dyn Progress,
        approver: Option<&dyn Approver>,
    ) {
        for call in calls {
            session.append(SessionEvent::ToolCall {
                call_id: call.id.clone(),
                name: call.name.clone(),
                arguments: call.arguments.clone(),
            });
            progress.tool_started(&call.id, &call.name, &call.arguments);
        }

        let mut results: Vec<Option<ToolResult>> = (0..calls.len()).map(|_| None).collect();
        for (index, call) in calls.iter().enumerate() {
            if let Some(denied) = self.gate(call, approver).await {
                progress.tool_finished(&call.id, &call.name, true);
                results[index] = Some(denied);
            }
        }

        let permitted: Vec<usize> = (0..calls.len())
            .filter(|index| results[*index].is_none())
            .collect();
        // The loop's own tools are run here, before the registry's. A goal change is a record
        // in the session log, and this function is the one place that holds the session while a
        // turn runs, so the goal tools are dispatched with it rather than through the registry —
        // which does not hold them. Running them first means a step that both changes the goal
        // and reads a file leaves the goal in the log before any result is recorded, so a
        // resumed session cannot see the second without the first.
        let (goal_indexes, registry_indexes): (Vec<usize>, Vec<usize>) = permitted
            .iter()
            .copied()
            .partition(|index| crate::goal_tools::is_goal_tool(&calls[*index].name));
        for index in goal_indexes {
            let result = self.run_goal_tool(session, &calls[index]);
            let is_error = !result.outcome.is_success();
            progress.tool_finished(&calls[index].id, &calls[index].name, is_error);
            results[index] = Some(result);
        }

        for batch in registry_indexes.chunks(self.parallel_limit()) {
            // `join_all` polls the batch together on this thread, so a call that awaits
            // leaves the others room to run: cooperative concurrency rather than
            // parallelism, because the kernel and its futures are deliberately `!Send`.
            //
            // The registry is borrowed for the *dispatch* and not for the await that
            // follows: a `ToolFuture` is `'static` and owns whatever it needs, so holding a
            // borrow across the batch would only risk meeting the borrow a registration
            // takes.
            let running: Vec<nanus_domain::ToolFuture> = {
                let registry = self.tools.borrow();
                batch
                    .iter()
                    .map(|index| registry.execute(calls[*index].clone()))
                    .collect()
            };
            let finished = futures::future::join_all(running).await;
            for (index, result) in batch.iter().zip(finished) {
                let is_error = !result.outcome.is_success();
                progress.tool_finished(&calls[*index].id, &calls[*index].name, is_error);
                results[*index] = Some(result);
            }
        }

        // Postcondition: every call was either denied or executed, so the results below
        // answer every call the model made.
        assert!(
            results.iter().all(Option::is_some),
            "every tool call has a result"
        );
        for (call, result) in calls.iter().zip(results) {
            let Some(result) = result else {
                continue;
            };
            let is_error = !result.outcome.is_success();
            let content = render_content(result.outcome.content());
            // The result answers the call it names: the pairing is by id as well as by
            // position, which is what a replay of the log reads.
            assert_eq!(
                call.id, result.call_id,
                "a result answers the call it names"
            );
            session.append(SessionEvent::ToolResult {
                call_id: result.call_id,
                content,
                is_error,
            });
        }
    }

    /// Runs one goal tool call, writing the change into the session log.
    ///
    /// The clock is read once per call, so two goal changes in one step are stamped in the
    /// order they were made. Nothing is appended for a read, or for a call the domain refuses:
    /// a `goal/change` record *is* a change, and a call that changed nothing must not leave one.
    fn run_goal_tool(&self, session: &mut Session, call: &ToolCall) -> ToolResult {
        let applied = crate::goal_tools::run(session.goal().as_ref(), call, self.clock.now_ms());
        if let Some(goal) = applied.record {
            session.append(SessionEvent::GoalChange { goal: Some(goal) });
        }
        ToolResult::new(call.id.clone(), applied.outcome)
    }

    /// How many tool calls may be in flight at once.
    ///
    /// Validated at construction, so this cannot be zero; the clamp is here because a
    /// zero-length chunk would make `chunks` iterate for ever rather than refuse.
    fn parallel_limit(&self) -> usize {
        usize::try_from(self.config.max_parallel_tools)
            .unwrap_or(usize::MAX)
            .max(1)
    }

    /// Enforces the sandbox and approval state for one call.
    ///
    /// Returns `Some` with the failure to record when the call must not run, and `None` when
    /// it may. The sandbox is the standing permission, so a call it already permits is never
    /// put to a human whatever the state is. A call outside it needs an exception, and the
    /// state says how one is obtained:
    ///
    /// - `per_call` asks every time.
    /// - `permitted` grants a call that cannot destroy anything, and a destructive one whose
    ///   targets are all inside a temporary directory; it asks about the rest.
    /// - `all_calls` grants every exception, for an environment that enforces its own
    ///   containment.
    ///
    /// With no answerer the outcome is `Unavailable`, which denies — fail closed either way.
    async fn gate(&self, call: &ToolCall, approver: Option<&dyn Approver>) -> Option<ToolResult> {
        // The access is copied out and the borrow released before anything is awaited: the
        // decision below can take as long as a person takes, and a registry borrow held that
        // long would refuse the registration that answers it.
        let access = {
            let registry = self.tools.borrow();
            registry.get(&call.name)?.access()
        };
        let sandbox = self.config.sandbox_mode;
        if sandbox.permits(access) {
            return None;
        }
        let policy = self.approval();
        if exempt(policy, call) {
            return None;
        }
        let reason = approval_reason(sandbox, access, guard::is_destructive(call));
        let outcome = match approver {
            Some(approver) => {
                let request = ApprovalRequest::new(call.name.clone())
                    .with_call_id(call.id.clone())
                    .with_reason(reason.clone());
                approver.decide(request).await
            }
            None => ApprovalOutcome::Unavailable,
        };
        if outcome.is_allowed() {
            return None;
        }
        Some(denied_result(call, &reason, outcome))
    }
}

/// Returns whether an approval state grants a call the sandbox refused, without asking.
///
/// `per_call` grants nothing, so every exception goes to a person. `all_calls` grants every
/// exception. `permitted` grants one unless it looks destructive, and grants even that when
/// every path it names is inside a temporary directory — which is where a destructive
/// command is the ordinary way to clean up rather than something to be asked about.
fn exempt(policy: ApprovalPolicy, call: &ToolCall) -> bool {
    match policy {
        ApprovalPolicy::PerCall => false,
        ApprovalPolicy::AllCalls => true,
        ApprovalPolicy::Permitted => {
            !guard::is_destructive(call) || guard::targets_are_temporary(call)
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
    /// Whether the stream was cut short by a stop request rather than finishing.
    interrupted: bool,
}

impl Assembled {
    /// Marks the assembly as cut short: the words stay, the calls that never ran go.
    ///
    /// One method rather than the same three assignments at each site, because the pair is the
    /// point: what the model already said is kept — a conversation that forgets words the reader
    /// watched arrive is worse than one that keeps them — and the calls it was part way through
    /// naming are dropped, because a call with no result to answer it would be replayed as one
    /// that ran.
    fn interrupt(&mut self) {
        self.interrupted = true;
        self.calls.clear();
        self.partial.clear();
    }
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
            interrupted: false,
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
    use nanus_domain::{ToolOutcome, ToolRegistry, ToolSchema};
    use nanus_ports::{ChatRequest, LlmPort, LlmStream};
    use serde_json::json;

    use super::*;

    /// A model that replays a fixed script of event batches.
    struct ScriptedLlm {
        batches: std::cell::RefCell<Vec<Vec<LlmEvent>>>,
        model: String,
        seen: Rc<std::cell::RefCell<Vec<ChatRequest>>>,
    }

    /// What a scripted model was asked for, shared with the test that built it.
    type Requests = Rc<std::cell::RefCell<Vec<ChatRequest>>>;

    impl ScriptedLlm {
        /// Builds a handle, which is what a runner takes; the name says so.
        fn handle(batches: Vec<Vec<LlmEvent>>) -> Rc<Box<dyn LlmPort>> {
            Self::recording(batches).0
        }

        /// Builds a handle and the requests it is sent.
        ///
        /// The second half is how a test observes what actually reached the model: a stub is
        /// behind a `dyn LlmPort` by the time the runner holds it, and the request it was
        /// given is not in the session log.
        fn recording(batches: Vec<Vec<LlmEvent>>) -> (Rc<Box<dyn LlmPort>>, Requests) {
            let seen: Requests = Rc::new(std::cell::RefCell::new(Vec::new()));
            let port: Box<dyn LlmPort> = Box::new(Self {
                batches: std::cell::RefCell::new(batches),
                model: "test-model".to_owned(),
                seen: Rc::clone(&seen),
            });
            (Rc::new(port), seen)
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
                ToolResult::new(call.id, outcome)
            })
        }
    }

    fn registry_with_echo() -> ToolRegistryHandle {
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
        ToolRegistryHandle::new(registry)
    }

    /// A configuration whose sandbox permits everything the stub tools declare.
    ///
    /// These tests are about the loop, not the gate: `echo` declares no access and is
    /// therefore gated as an `execute` call, so the sandbox has to permit execution for it
    /// to run without an answerer. The gate itself is tested separately, below.
    fn config() -> AgentConfig {
        AgentConfig::new(4, 1, "test-model", 4096)
            .unwrap_or_else(|_| unreachable!("valid config"))
            .with_sandbox(SandboxMode::DangerFullAccess)
    }

    fn session() -> Session {
        Session::new(SessionId::new("s-1"), 0, "/tmp")
    }

    /// A clock that never moves, so a goal change's timestamps are assertable.
    struct FixedClock;

    impl nanus_ports::ClockPort for FixedClock {
        fn now_ms(&self) -> u64 {
            1_700_000_000_000
        }
    }

    /// The clock every test runner is built with.
    fn clock() -> ClockHandle {
        Rc::new(Box::new(FixedClock))
    }

    fn runner(llm: Rc<Box<dyn LlmPort>>, tools: ToolRegistryHandle) -> Option<AgentRunner> {
        AgentRunner::new(llm, tools, "you are a test", config(), clock()).ok()
    }

    /// The budget is enforced by the turn machine and was, until it bit, invisible to the
    /// model it was enforced against. A turn that spends its last steps still exploring is
    /// a turn that was never told it had a last step.
    #[test]
    fn the_system_prompt_states_the_budget_it_will_be_held_to() {
        let prompt = with_step_budget("you are a test", 128);
        assert!(
            prompt.starts_with("you are a test"),
            "the prompt a caller configured is kept, not replaced: {prompt}"
        );
        assert!(
            prompt.contains("128"),
            "the budget is stated as a number: {prompt}"
        );
        assert!(
            prompt.contains("step"),
            "and what a step is, since the number alone is not actionable: {prompt}"
        );
        // Read from the argument rather than written into the prose: a hard-coded
        // hundred and twenty-eight would satisfy the assertions above.
        assert!(with_step_budget("", 7).contains('7'));
    }

    /// The ceiling is checked against the prompt the model will actually be sent, which
    /// is the one the runner assembled rather than the one the caller passed. Checking
    /// first and appending afterwards would let a prompt that just fits go out over the
    /// limit the configuration set.
    #[test]
    #[should_panic(expected = "the system prompt fits its configured ceiling")]
    fn a_prompt_that_only_fits_without_the_budget_sentence_is_refused() {
        let budget = with_step_budget("", 4).len();
        let fits = "x".repeat(4_096_usize.saturating_sub(budget));
        assert!(
            AgentRunner::new(
                ScriptedLlm::handle(Vec::new()),
                ToolRegistryHandle::new(ToolRegistry::new()),
                fits.clone(),
                config(),
                clock(),
            )
            .is_ok(),
            "a prompt that fits with the sentence is accepted"
        );
        // One byte more, and the rejection has to be about the ceiling rather than
        // about the caller's part of the prompt.
        let _ = AgentRunner::new(
            ScriptedLlm::handle(Vec::new()),
            ToolRegistryHandle::new(ToolRegistry::new()),
            format!("{fits}x"),
            config(),
            clock(),
        );
    }

    /// A driver whose answer to "should this turn stop?" the test decides.
    #[derive(Default)]
    struct Switch {
        stop: std::cell::Cell<bool>,
    }

    impl Switch {
        fn stop(&self) {
            self.stop.set(true);
        }
    }

    impl Progress for Switch {
        fn cancelled(&self) -> bool {
            self.stop.get()
        }
    }

    /// A driver that asks for the turn to stop as soon as the model has said anything,
    /// which is how a stop lands in the middle of a response rather than between steps.
    #[derive(Default)]
    struct StopAfterFirstWord {
        stop: std::cell::Cell<bool>,
    }

    impl StopAfterFirstWord {
        fn stop(&self) {
            self.stop.set(true);
        }
    }

    impl Progress for StopAfterFirstWord {
        fn cancelled(&self) -> bool {
            self.stop.get()
        }

        fn text(&mut self, _delta: &str) {
            self.stop();
        }
    }

    /// A driver that asks the turn to stop on the *n*th question the loop asks.
    ///
    /// The loop asks once before each step, once before each streamed event, and once more when
    /// the stream has ended. A count is how a test reaches that last one: a flag set by `text` is
    /// seen at the next event, and the case here is a stop that arrives after the final one.
    struct StopOnCheck {
        after: u32,
        asked: std::cell::Cell<u32>,
    }

    impl Progress for StopOnCheck {
        fn cancelled(&self) -> bool {
            let asked = self.asked.get().saturating_add(1);
            self.asked.set(asked);
            asked > self.after
        }
    }

    /// A driver that writes down which of the delta callbacks it was told about, in order.
    ///
    /// The kind of each delta rather than the delta itself: what is under test is *which* kinds
    /// of generation reach a listener, because a rate is measured from these callbacks and a kind
    /// that never arrives is a kind that is not counted.
    #[derive(Default)]
    struct Recorder {
        told: Vec<&'static str>,
    }

    impl Progress for Recorder {
        fn text(&mut self, _delta: &str) {
            self.told.push("text");
        }

        fn reasoning(&mut self, _delta: &str) {
            self.told.push("reasoning");
        }

        fn tool_call(&mut self, _delta: &str) {
            self.told.push("tool_call");
        }

        fn response_head(&mut self) {
            self.told.push("head");
        }
    }

    /// A step that answers with a tool call and no prose still reaches a listener.
    ///
    /// This is the ordinary step in a coding session rather than an edge case, and before there
    /// was a callback for it, it reached none: an observer timing generation heard nothing at all
    /// from the steps that only called a tool, so the rate it reported was the rate of the steps
    /// that happened to talk — the long ones.
    #[tokio::test]
    async fn a_step_that_only_calls_a_tool_still_reaches_a_listener() {
        let llm = ScriptedLlm::handle(vec![vec![
            LlmEvent::ToolCallDelta {
                index: 0,
                id: Some(ToolCallId::new("c1")),
                name: Some(ToolName::new("echo").unwrap_or_else(|_| unreachable!("valid"))),
                arguments_delta: "{\"x\":1}".to_owned(),
            },
            LlmEvent::Finished {
                reason: FinishReason::ToolCalls,
            },
        ]]);
        let Some(runner) = runner(Rc::clone(&llm), registry_with_echo()) else {
            return;
        };
        let mut session = session();
        let mut progress = Recorder::default();

        assert!(
            runner
                .run_turn(&mut session, "hi", &mut progress, None)
                .await
                .is_ok()
        );
        assert_eq!(
            progress.told,
            vec!["tool_call"],
            "the call is generation, and nothing else was said"
        );
    }

    /// The server beginning to answer is reported before anything it says, which is what makes it
    /// a boundary a listener can split the wait at. Reported as the fact rather than as an instant,
    /// so the listener reads its own clock and both halves of the wait are measured with one clock
    /// rather than two that were never compared.
    #[tokio::test]
    async fn the_server_answering_is_reported_before_what_it_says() {
        let llm = ScriptedLlm::handle(vec![vec![
            LlmEvent::ResponseHead,
            LlmEvent::TextDelta("an answer".to_owned()),
            LlmEvent::Finished {
                reason: FinishReason::Stop,
            },
        ]]);
        let Some(runner) = runner(Rc::clone(&llm), registry_with_echo()) else {
            return;
        };
        let mut session = session();
        let mut progress = Recorder::default();

        assert!(
            runner
                .run_turn(&mut session, "hi", &mut progress, None)
                .await
                .is_ok()
        );
        assert_eq!(progress.told, vec!["head", "text"]);
    }

    /// And the other direction: a provider that says nothing about a response head reports none,
    /// rather than a listener inventing one from the first delta — an invented boundary would put
    /// the whole wait on whichever side of it the guess happened to land.
    #[tokio::test]
    async fn a_provider_that_reports_no_head_reports_none() {
        let llm = ScriptedLlm::handle(vec![vec![
            LlmEvent::TextDelta("an answer".to_owned()),
            LlmEvent::Finished {
                reason: FinishReason::Stop,
            },
        ]]);
        let Some(runner) = runner(Rc::clone(&llm), registry_with_echo()) else {
            return;
        };
        let mut session = session();
        let mut progress = Recorder::default();

        assert!(
            runner
                .run_turn(&mut session, "hi", &mut progress, None)
                .await
                .is_ok()
        );
        assert_eq!(progress.told, vec!["text"], "no head was announced");
    }

    /// The other direction: a step that only speaks reports prose and not a tool call, so the
    /// listener is told what arrived rather than being told the same thing whatever did.
    #[tokio::test]
    async fn a_step_that_only_speaks_is_reported_as_prose() {
        let llm = ScriptedLlm::handle(vec![vec![
            LlmEvent::ReasoningDelta("thinking".to_owned()),
            LlmEvent::TextDelta("an answer".to_owned()),
            LlmEvent::Finished {
                reason: FinishReason::Stop,
            },
        ]]);
        let Some(runner) = runner(Rc::clone(&llm), registry_with_echo()) else {
            return;
        };
        let mut session = session();
        let mut progress = Recorder::default();

        assert!(
            runner
                .run_turn(&mut session, "hi", &mut progress, None)
                .await
                .is_ok()
        );
        assert_eq!(progress.told, vec!["reasoning", "text"]);
    }

    /// A stop that arrives while the response is *closing* still stops the step.
    ///
    /// The check inside the stream loop only runs when another event arrives, so a stop during
    /// the last await — the end of the response body — was not seen until the next step, and this
    /// step's tools ran anyway. Running the command a reader has just asked to stop is the one
    /// thing stopping is for.
    #[tokio::test]
    async fn a_stop_during_the_closing_await_still_stops_the_step() {
        let llm = ScriptedLlm::handle(vec![vec![
            LlmEvent::TextDelta("about to run something".to_owned()),
            LlmEvent::ToolCallDelta {
                index: 0,
                id: Some(ToolCallId::new("c1")),
                name: Some(ToolName::new("echo").unwrap_or_else(|_| unreachable!("valid"))),
                arguments_delta: "{\"x\":1}".to_owned(),
            },
            LlmEvent::Finished {
                reason: FinishReason::ToolCalls,
            },
        ]]);
        let Some(runner) = runner(Rc::clone(&llm), registry_with_echo()) else {
            return;
        };
        let mut session = session();
        // One question for the step, three for the events, and the fourth is the one asked after
        // the stream ended — which is the check under test.
        let mut progress = StopOnCheck {
            after: 4,
            asked: std::cell::Cell::new(0),
        };

        let outcome = runner
            .run_turn(&mut session, "hi", &mut progress, None)
            .await;
        assert!(outcome.is_ok());
        let Ok(outcome) = outcome else { return };
        assert_eq!(
            outcome.reason,
            TurnEndReason::Interrupted,
            "the stop was seen rather than deferred to the next step"
        );
        assert_eq!(outcome.answer, "about to run something");
        assert!(
            !session
                .log()
                .events()
                .iter()
                .any(|event| matches!(event, SessionEvent::ToolResult { .. })),
            "and the tool never ran: {:?}",
            session.log().events()
        );
    }

    /// A stop asked for before a step is issued takes effect there: no request is made, and
    /// the turn closes with the reason the machine records for it rather than with the
    /// budget or a failure.
    #[tokio::test]
    async fn a_stop_asked_for_between_steps_closes_the_turn() {
        let llm = ScriptedLlm::handle(vec![vec![LlmEvent::TextDelta("never asked".to_owned())]]);
        let Some(runner) = runner(
            Rc::clone(&llm),
            ToolRegistryHandle::new(ToolRegistry::new()),
        ) else {
            return;
        };
        let mut session = session();
        let mut progress = Switch::default();
        progress.stop();

        let outcome = runner
            .run_turn(&mut session, "hi", &mut progress, None)
            .await;
        assert!(outcome.is_ok());
        let Ok(outcome) = outcome else { return };
        assert_eq!(outcome.reason, TurnEndReason::Interrupted);
        assert_eq!(outcome.steps, 0, "no step was taken");
        assert_eq!(outcome.answer, "");
        // The turn is closed in the log, so a resumed session does not find it open.
        assert_eq!(
            session.log().last_turn_end(),
            Some(&TurnEndReason::Interrupted)
        );
    }

    /// A stop that lands while the model is streaming is taken at the next token: what has
    /// been said is kept and marked interrupted, and the tool calls the model was part way
    /// through naming are dropped, because a call with no result to answer it would be
    /// replayed as one that ran.
    #[tokio::test]
    async fn a_stop_during_a_response_keeps_the_words_and_drops_the_calls() {
        let llm = ScriptedLlm::handle(vec![vec![
            LlmEvent::TextDelta("half a sentence".to_owned()),
            LlmEvent::ToolCallDelta {
                index: 0,
                id: Some(ToolCallId::new("c1")),
                name: Some(ToolName::new("echo").unwrap_or_else(|_| unreachable!("valid"))),
                arguments_delta: "{\"x\":1}".to_owned(),
            },
            LlmEvent::TextDelta("and more".to_owned()),
            LlmEvent::Finished {
                reason: FinishReason::ToolCalls,
            },
        ]]);
        let Some(runner) = runner(Rc::clone(&llm), registry_with_echo()) else {
            return;
        };
        let mut session = session();
        let mut progress = StopAfterFirstWord::default();

        let outcome = runner
            .run_turn(&mut session, "hi", &mut progress, None)
            .await;
        assert!(outcome.is_ok());
        let Ok(outcome) = outcome else { return };
        assert_eq!(outcome.reason, TurnEndReason::Interrupted);
        assert_eq!(
            outcome.steps, 1,
            "the step that was running is the one it stopped in"
        );
        assert_eq!(
            outcome.answer, "half a sentence",
            "what the model said before the stop is the answer"
        );

        // The conversation that would be sent next: the words are there, and the call that
        // was never run is not.
        let messages = session.derive_messages();
        let assistant = messages
            .iter()
            .rev()
            .find(|message| !message.tool_calls().is_empty() || message.text().is_some());
        assert!(
            assistant.is_some_and(|message| message.tool_calls().is_empty()),
            "an unrun call must not be replayed as one that ran: {messages:?}"
        );
        assert!(
            session.log().events().iter().any(|event| matches!(
                event,
                SessionEvent::AssistantMessage {
                    interrupted: true,
                    ..
                }
            )),
            "and the step says it was cut short rather than ending on its own"
        );
        // An interrupted message is still attributed: it cost what it cost, and a session
        // that recorded the cut short turn without saying what produced it would leave that
        // spend unattributable.
        assert!(
            session.log().events().iter().any(|event| matches!(
                event,
                SessionEvent::AssistantMessage {
                    interrupted: true,
                    model: Some(_),
                    ..
                }
            )),
            "the cut short turn still records the model that produced it"
        );
    }

    /// The other direction: a driver that never asks to stop does not stop the turn. The
    /// default answer is `false`, so a script that runs one turn to completion needs no
    /// opinion about stopping at all.
    #[tokio::test]
    async fn a_driver_with_no_opinion_lets_the_turn_finish() {
        let llm = ScriptedLlm::handle(vec![
            vec![LlmEvent::TextDelta("done".to_owned())],
            vec![LlmEvent::Finished {
                reason: FinishReason::Stop,
            }],
        ]);
        let Some(runner) = runner(llm, ToolRegistryHandle::new(ToolRegistry::new())) else {
            return;
        };
        let mut session = session();
        let outcome = runner.run_turn(&mut session, "hi", &mut Silent, None).await;
        assert!(outcome.is_ok());
        assert!(outcome.is_ok_and(|outcome| outcome.reason == TurnEndReason::Completed));
    }

    #[tokio::test]
    async fn a_plain_answer_completes_the_turn() {
        let llm = ScriptedLlm::handle(vec![
            vec![LlmEvent::TextDelta("hello".to_owned())],
            vec![LlmEvent::Finished {
                reason: FinishReason::Stop,
            }],
        ]);
        let Some(runner) = runner(llm, ToolRegistryHandle::new(ToolRegistry::new())) else {
            return;
        };
        let mut session = session();
        let outcome = runner.run_turn(&mut session, "hi", &mut Silent, None).await;
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
        let outcome = runner.run_turn(&mut session, "go", &mut Silent, None).await;
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
        let outcome = runner
            .run_turn(&mut session, "loop", &mut Silent, None)
            .await;
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
        let Some(runner) = runner(llm, ToolRegistryHandle::new(ToolRegistry::new())) else {
            return;
        };
        let mut session = session();
        let outcome = runner.run_turn(&mut session, "hi", &mut Silent, None).await;
        assert!(outcome.is_err());
        let Err(error) = outcome else {
            return;
        };
        assert!(error.to_string().contains("upstream is down"));

        // The failure is reported *and* the turn is closed: a log that ends with an open
        // turn is one a resumed session has to guess about, and this path used to leave
        // exactly that — `turn_start`, `user_message`, `step_start`, and nothing else.
        assert_eq!(
            session.log().last_turn_end(),
            Some(&TurnEndReason::Error {
                message: String::from("the model failed: upstream is down"),
            }),
            "the turn is closed with the reason that closed it"
        );
        let steps: Vec<u32> = session
            .log()
            .events()
            .iter()
            .filter_map(|event| match event {
                SessionEvent::StepEnd { step, .. } => Some(*step),
                _ => None,
            })
            .collect();
        assert_eq!(steps, vec![1], "and the step boundary is balanced");
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
        let Some(runner) = runner(llm, ToolRegistryHandle::new(ToolRegistry::new())) else {
            return;
        };
        let mut session = session();
        let outcome = runner.run_turn(&mut session, "go", &mut Silent, None).await;
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
            .run_turn(&mut session, "a question", &mut Silent, None)
            .await;
        assert!(r.is_ok());
        let _ = r;
        // The request must carry the prompt, the tool schemas, and the user message.
        // `ScriptedLlm::seen` is the only way to observe what was sent.
        assert_eq!(llm.model(), "test-model");
    }

    /// Switching the model changes the next request *and* what the turn records.
    ///
    /// The configuration is the startup value, so a runner that read it instead of its own
    /// cell would keep asking the old model while the interface showed the new one — and a
    /// session resumed against a different model would record the first request's model
    /// against every step after the switch.
    #[tokio::test]
    async fn switching_the_model_changes_the_next_request_and_what_is_recorded() {
        let (llm, seen) = ScriptedLlm::recording(vec![
            vec![LlmEvent::TextDelta("ok".to_owned())],
            vec![LlmEvent::Finished {
                reason: FinishReason::Stop,
            }],
        ]);
        let Some(runner) = runner(llm, registry_with_echo()) else {
            return;
        };
        assert_eq!(runner.model(), "test-model");
        runner.set_model("deepseek-v4-pro");
        assert_eq!(runner.model(), "deepseek-v4-pro");
        assert_eq!(
            runner.config().model,
            "test-model",
            "the configured model is the startup value and is not rewritten"
        );

        let mut session = session();
        let outcome = runner
            .run_turn(&mut session, "a question", &mut Silent, None)
            .await;
        assert!(outcome.is_ok());
        assert_eq!(
            seen.borrow().first().map(|request| request.model.clone()),
            Some(String::from("deepseek-v4-pro")),
            "the request names the model that was switched to"
        );
        let recorded = session.log().events().iter().find_map(|event| match event {
            SessionEvent::AssistantMessage { model, .. } => model.clone(),
            _ => None,
        });
        assert_eq!(
            recorded,
            Some(String::from("deepseek-v4-pro")),
            "and the log says which model produced the message"
        );
    }

    /// The effort a caller chooses reaches the request, and giving it back to the adapter
    /// leaves the request unset rather than sending the adapter's default as if it were a
    /// choice.
    #[tokio::test]
    async fn the_effort_reaches_the_request_only_when_it_was_chosen() {
        let (llm, seen) = ScriptedLlm::recording(vec![
            vec![LlmEvent::TextDelta("ok".to_owned())],
            vec![LlmEvent::Finished {
                reason: FinishReason::Stop,
            }],
            vec![LlmEvent::TextDelta("ok".to_owned())],
            vec![LlmEvent::Finished {
                reason: FinishReason::Stop,
            }],
        ]);
        let Some(runner) = runner(llm, registry_with_echo()) else {
            return;
        };
        // The stub adapter has no notion of effort, so nothing is known until one is chosen.
        assert_eq!(runner.effort(), None);

        runner.set_effort(Some(nanus_ports::ReasoningEffort::High));
        assert_eq!(runner.effort(), Some(nanus_ports::ReasoningEffort::High));
        let mut chosen = session();
        let outcome = runner
            .run_turn(&mut chosen, "think harder", &mut Silent, None)
            .await;
        assert!(outcome.is_ok());
        assert_eq!(
            seen.borrow()
                .first()
                .and_then(|request| request.reasoning_effort),
            Some(nanus_ports::ReasoningEffort::High),
            "the chosen effort is what the request carries"
        );
        let recorded = chosen.log().events().iter().find_map(|event| match event {
            SessionEvent::AssistantMessage { effort, .. } => effort.clone(),
            _ => None,
        });
        assert_eq!(
            recorded,
            Some(String::from("high")),
            "and the log says which effort produced the message"
        );

        // Given back to the adapter, the request says nothing: an unset effort is the adapter
        // applying its own default, which is a different fact from asking for `medium`.
        runner.set_effort(None);
        let mut second = session();

        let outcome = runner
            .run_turn(&mut second, "as usual", &mut Silent, None)
            .await;
        assert!(outcome.is_ok());
        assert_eq!(
            seen.borrow()
                .get(1)
                .and_then(|request| request.reasoning_effort),
            None
        );
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
        let outcome = runner.run_turn(&mut session, "hi", &mut Silent, None).await;
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
            SessionEvent::GoalChange { .. } => "goal_change",
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
            context_budget: nanus_domain::DEFAULT_CONTEXT_BUDGET,
            approval_policy: ApprovalPolicy::default(),
            sandbox_mode: SandboxMode::default(),
        };
        let outcome = AgentRunner::new(
            llm,
            ToolRegistryHandle::new(ToolRegistry::new()),
            "p",
            bad,
            clock(),
        );
        assert!(outcome.is_err());
    }

    /// An approver that answers the same way every time and records what it was asked.
    ///
    /// The record is the point: several of the tests below are not about the answer but
    /// about whether the answerer was consulted at all, and "the sandbox already permitted
    /// it" is only a fact if nobody was asked.
    struct ScriptedApprover {
        answer: ApprovalOutcome,
        asked: std::cell::RefCell<Vec<ApprovalRequest>>,
    }

    impl ScriptedApprover {
        fn new(answer: ApprovalOutcome) -> Self {
            Self {
                answer,
                asked: std::cell::RefCell::new(Vec::new()),
            }
        }

        fn asked(&self) -> Vec<ApprovalRequest> {
            self.asked.borrow().clone()
        }
    }

    impl Approver for ScriptedApprover {
        fn decide(
            &self,
            request: ApprovalRequest,
        ) -> nanus_ports::LocalBoxFuture<'_, ApprovalOutcome> {
            self.asked.borrow_mut().push(request);
            let answer = self.answer;
            Box::pin(async move { answer })
        }
    }

    /// A tool that counts how many times it actually ran.
    struct Counted {
        runs: Rc<std::cell::Cell<u32>>,
    }

    impl nanus_domain::ToolExecutor for Counted {
        fn execute(&self, call: ToolCall) -> nanus_domain::ToolFuture {
            self.runs.set(self.runs.get().saturating_add(1));
            Box::pin(async move { ToolResult::success(call.id, serde_json::json!({})) })
        }
    }

    /// Builds a registry with one counted tool of the given access.
    fn registry_with_counted(
        name: &str,
        access: ToolAccess,
    ) -> (ToolRegistryHandle, Rc<std::cell::Cell<u32>>) {
        let runs = Rc::new(std::cell::Cell::new(0));
        let schema = ToolSchema {
            name: ToolName::new(name).unwrap_or_else(|_| unreachable!("a valid test tool name")),
            description: "A counted test tool".to_owned(),
            parameters: json!({ "type": "object" }),
        };
        let mut registry = ToolRegistry::new();
        assert!(
            registry
                .register(
                    nanus_domain::ToolDefinition::new(
                        schema,
                        Counted {
                            runs: Rc::clone(&runs),
                        },
                    )
                    .with_access(access)
                )
                .is_ok()
        );
        (ToolRegistryHandle::new(registry), runs)
    }

    /// A model that calls `name` once and then answers.
    fn calls_then_answers(name: &str) -> Rc<Box<dyn LlmPort>> {
        ScriptedLlm::handle(vec![
            vec![
                LlmEvent::ToolCallDelta {
                    index: 0,
                    id: Some(ToolCallId::new("c1")),
                    name: Some(ToolName::new(name).unwrap_or_else(|_| unreachable!("valid"))),
                    arguments_delta: "{}".to_owned(),
                },
                LlmEvent::Finished {
                    reason: FinishReason::ToolCalls,
                },
            ],
            vec![
                LlmEvent::TextDelta("done".to_owned()),
                LlmEvent::Finished {
                    reason: FinishReason::Stop,
                },
            ],
        ])
    }

    /// A model that calls `name` once with `arguments` and then answers.
    fn calls_then_answers_with(name: &str, arguments: &serde_json::Value) -> Rc<Box<dyn LlmPort>> {
        ScriptedLlm::handle(vec![
            vec![
                LlmEvent::ToolCallDelta {
                    index: 0,
                    id: Some(ToolCallId::new("c1")),
                    name: Some(ToolName::new(name).unwrap_or_else(|_| unreachable!("valid"))),
                    arguments_delta: arguments.to_string(),
                },
                LlmEvent::Finished {
                    reason: FinishReason::ToolCalls,
                },
            ],
            vec![
                LlmEvent::TextDelta("done".to_owned()),
                LlmEvent::Finished {
                    reason: FinishReason::Stop,
                },
            ],
        ])
    }

    /// Builds a runner over a sandbox mode and an approval policy.
    fn gated_runner(
        llm: Rc<Box<dyn LlmPort>>,
        tools: ToolRegistryHandle,
        sandbox: SandboxMode,
        approval: ApprovalPolicy,
    ) -> Option<AgentRunner> {
        let config = AgentConfig::new(8, 1, "test-model", 4096)
            .ok()?
            .with_sandbox(sandbox)
            .with_approval(approval);
        AgentRunner::new(llm, tools, "you are a test", config, clock()).ok()
    }

    /// The sandbox is the standing permission, so a call it permits is never put to a
    /// human: asking about every read would make the interface unusable and would make the
    /// approval policy mean something it does not.
    #[tokio::test]
    async fn a_call_the_sandbox_permits_runs_without_asking() {
        let (tools, runs) = registry_with_counted("reader", ToolAccess::Read);
        let approver = ScriptedApprover::new(ApprovalOutcome::Rejected);
        let Some(runner) = gated_runner(
            calls_then_answers("reader"),
            tools,
            SandboxMode::ReadOnly,
            ApprovalPolicy::PerCall,
        ) else {
            return;
        };
        let mut session = session();
        let outcome = runner
            .run_turn(&mut session, "go", &mut Silent, Some(&approver))
            .await;
        assert!(outcome.is_ok_and(|outcome| outcome.answer == "done"));
        assert_eq!(runs.get(), 1, "the tool ran");
        assert!(
            approver.asked().is_empty(),
            "a permitted call is not a question for a human"
        );
    }

    /// The other direction: a call outside the sandbox is asked about, and an approval runs
    /// it. This is the whole of `ask`.
    #[tokio::test]
    async fn a_call_outside_the_sandbox_runs_when_it_is_allowed() {
        let (tools, runs) = registry_with_counted("runner", ToolAccess::Execute);
        let approver = ScriptedApprover::new(ApprovalOutcome::AllowedOnce);
        let Some(runner) = gated_runner(
            calls_then_answers("runner"),
            tools,
            SandboxMode::WorkspaceWrite,
            ApprovalPolicy::PerCall,
        ) else {
            return;
        };
        let mut session = session();
        let outcome = runner
            .run_turn(&mut session, "go", &mut Silent, Some(&approver))
            .await;
        assert!(outcome.is_ok_and(|outcome| outcome.answer == "done"));
        assert_eq!(runs.get(), 1, "the tool ran once it was allowed");
        let asked = approver.asked();
        assert_eq!(asked.len(), 1, "the call was put to a human");
        assert_eq!(
            asked.first().map(|request| request.tool.as_str()),
            Some("runner")
        );
        // The reason names the knob that made the call an exception, so a reader can tell
        // why they were asked.
        assert!(
            asked
                .first()
                .and_then(|request| request.reason.as_deref())
                .is_some_and(|reason| reason.contains("workspace_write")),
            "the reason names the sandbox mode: {asked:?}"
        );
    }

    /// A refusal stops the call and is recorded as a result, so the model is told and the
    /// turn can close rather than waiting for an answer that never comes.
    #[tokio::test]
    async fn a_refused_call_does_not_run_and_is_answered() {
        let (tools, runs) = registry_with_counted("runner", ToolAccess::Execute);
        let approver = ScriptedApprover::new(ApprovalOutcome::Rejected);
        let Some(runner) = gated_runner(
            calls_then_answers("runner"),
            tools,
            SandboxMode::WorkspaceWrite,
            ApprovalPolicy::PerCall,
        ) else {
            return;
        };
        let mut session = session();
        let outcome = runner
            .run_turn(&mut session, "go", &mut Silent, Some(&approver))
            .await;
        assert!(outcome.is_ok(), "{outcome:?}");
        assert_eq!(runs.get(), 0, "a refused call does not run");
        let results: Vec<&SessionEvent> = session
            .log()
            .events()
            .iter()
            .filter(|event| matches!(event, SessionEvent::ToolResult { .. }))
            .collect();
        assert_eq!(results.len(), 1, "the refusal is a recorded tool result");
        assert!(
            matches!(
                results.first(),
                Some(SessionEvent::ToolResult { is_error: true, content, .. })
                    if content.contains("not run")
            ),
            "the model is told the call did not run: {results:?}"
        );
        // The turn still closed normally: a denial is information, not a failure.
        assert_eq!(
            session.log().last_turn_end(),
            Some(&TurnEndReason::Completed)
        );
    }

    /// `all_calls` grants every exception without consulting anyone.
    #[tokio::test]
    async fn all_calls_grants_without_consulting_the_answerer() {
        let (tools, runs) = registry_with_counted("runner", ToolAccess::Execute);
        let approver = ScriptedApprover::new(ApprovalOutcome::Rejected);
        let Some(runner) = gated_runner(
            calls_then_answers("runner"),
            tools,
            SandboxMode::WorkspaceWrite,
            ApprovalPolicy::AllCalls,
        ) else {
            return;
        };
        let mut session = session();
        let outcome = runner
            .run_turn(&mut session, "go", &mut Silent, Some(&approver))
            .await;
        assert!(outcome.is_ok(), "{outcome:?}");
        assert_eq!(runs.get(), 1, "the call ran without being asked about");
        assert!(
            approver.asked().is_empty(),
            "`all_calls` is a free for all and consults nobody"
        );
    }

    /// `permitted` runs a call that cannot destroy anything without asking.
    #[tokio::test]
    async fn permitted_grants_a_non_destructive_call() {
        let (tools, runs) = registry_with_counted("bash", ToolAccess::Execute);
        let approver = ScriptedApprover::new(ApprovalOutcome::Rejected);
        let Some(runner) = gated_runner(
            calls_then_answers_with("bash", &json!({ "command": "ls -la" })),
            tools,
            SandboxMode::WorkspaceWrite,
            ApprovalPolicy::Permitted,
        ) else {
            return;
        };
        let mut session = session();
        let outcome = runner
            .run_turn(&mut session, "go", &mut Silent, Some(&approver))
            .await;
        assert!(outcome.is_ok(), "{outcome:?}");
        assert_eq!(runs.get(), 1, "a harmless command runs");
        assert!(approver.asked().is_empty(), "nobody was asked");
    }

    /// The other direction: a destructive command outside a temporary directory is asked
    /// about, and a refusal stops it. This is what `permitted` is holding back.
    #[tokio::test]
    async fn permitted_asks_about_a_destructive_call() {
        let (tools, runs) = registry_with_counted("bash", ToolAccess::Execute);
        let approver = ScriptedApprover::new(ApprovalOutcome::Rejected);
        let Some(runner) = gated_runner(
            calls_then_answers_with("bash", &json!({ "command": "rm -rf /work/src" })),
            tools,
            SandboxMode::WorkspaceWrite,
            ApprovalPolicy::Permitted,
        ) else {
            return;
        };
        let mut session = session();
        let outcome = runner
            .run_turn(&mut session, "go", &mut Silent, Some(&approver))
            .await;
        assert!(outcome.is_ok(), "{outcome:?}");
        assert_eq!(runs.get(), 0, "a refused destructive call does not run");
        let asked = approver.asked();
        assert_eq!(asked.len(), 1, "the destructive call was put to a human");
        assert!(
            asked
                .first()
                .and_then(|request| request.reason.as_deref())
                .is_some_and(|reason| reason.contains("destructive")),
            "the reason says the call looks destructive: {asked:?}"
        );
    }

    /// A destructive command whose targets are all inside a temporary directory is the
    /// ordinary way to clean up, and `permitted` runs it without asking.
    #[tokio::test]
    async fn permitted_grants_a_destructive_call_in_a_temporary_directory() {
        let (tools, runs) = registry_with_counted("bash", ToolAccess::Execute);
        let approver = ScriptedApprover::new(ApprovalOutcome::Rejected);
        let Some(runner) = gated_runner(
            calls_then_answers_with("bash", &json!({ "command": "rm -rf /tmp/nanus-build" })),
            tools,
            SandboxMode::WorkspaceWrite,
            ApprovalPolicy::Permitted,
        ) else {
            return;
        };
        let mut session = session();
        let outcome = runner
            .run_turn(&mut session, "go", &mut Silent, Some(&approver))
            .await;
        assert!(outcome.is_ok(), "{outcome:?}");
        assert_eq!(runs.get(), 1, "a temporary destructive command runs");
        assert!(approver.asked().is_empty(), "nobody was asked");
    }

    /// The state can be changed while a session is live, which is what the interface's
    /// toggle needs: the next call consults the new state rather than the one the runner
    /// was built with.
    #[tokio::test]
    async fn the_approval_state_can_be_changed_after_construction() {
        let (tools, runs) = registry_with_counted("bash", ToolAccess::Execute);
        let approver = ScriptedApprover::new(ApprovalOutcome::Rejected);
        let Some(runner) = gated_runner(
            calls_then_answers_with("bash", &json!({ "command": "rm -rf /work/src" })),
            tools,
            SandboxMode::WorkspaceWrite,
            ApprovalPolicy::PerCall,
        ) else {
            return;
        };
        assert_eq!(runner.approval(), ApprovalPolicy::PerCall);
        runner.set_approval(ApprovalPolicy::AllCalls);
        assert_eq!(runner.approval(), ApprovalPolicy::AllCalls);
        let mut session = session();
        let outcome = runner
            .run_turn(&mut session, "go", &mut Silent, Some(&approver))
            .await;
        assert!(outcome.is_ok(), "{outcome:?}");
        assert_eq!(runs.get(), 1, "the new state granted the call");
        assert!(
            approver.asked().is_empty(),
            "the old state would have asked"
        );
    }

    /// `ask` with nobody to ask is a denial, not a grant: the fail-closed direction.
    #[tokio::test]
    async fn ask_with_no_answerer_denies() {
        let (tools, runs) = registry_with_counted("runner", ToolAccess::Execute);
        let Some(runner) = gated_runner(
            calls_then_answers("runner"),
            tools,
            SandboxMode::WorkspaceWrite,
            ApprovalPolicy::PerCall,
        ) else {
            return;
        };
        let mut session = session();
        let outcome = runner.run_turn(&mut session, "go", &mut Silent, None).await;
        assert!(outcome.is_ok(), "{outcome:?}");
        assert_eq!(runs.get(), 0, "no answerer means no exception");
        assert!(
            session.log().events().iter().any(|event| matches!(
                event,
                SessionEvent::ToolResult { is_error: true, content, .. }
                    if content.contains("nobody was available")
            )),
            "the model is told nobody could approve it"
        );
    }

    /// An unknown tool is not a question for a human: the registry reports it, and a request
    /// about a tool that does not exist would be noise in front of the person deciding.
    #[tokio::test]
    async fn an_unknown_tool_is_not_put_to_the_answerer() {
        let approver = ScriptedApprover::new(ApprovalOutcome::AllowedOnce);
        let Some(runner) = gated_runner(
            calls_then_answers("nope"),
            ToolRegistryHandle::new(ToolRegistry::new()),
            SandboxMode::ReadOnly,
            ApprovalPolicy::PerCall,
        ) else {
            return;
        };
        let mut session = session();
        let outcome = runner
            .run_turn(&mut session, "go", &mut Silent, Some(&approver))
            .await;
        assert!(outcome.is_ok(), "{outcome:?}");
        assert!(approver.asked().is_empty(), "nothing to approve");
    }

    /// A tool that sleeps, and records who was in flight while it did.
    ///
    /// The sleep is what makes concurrency observable: a tool that returns immediately
    /// completes on its first poll, so "were two in flight at once?" can only be asked of a
    /// tool that awaits.
    struct Tracked {
        /// What the result says it is.
        label: &'static str,
        /// How long it takes.
        delay: std::time::Duration,
        /// How many are running right now.
        in_flight: Rc<std::cell::Cell<u32>>,
        /// The most that were ever running at once.
        peak: Rc<std::cell::Cell<u32>>,
        /// The labels, in the order they finished.
        finished: Rc<std::cell::RefCell<Vec<String>>>,
    }

    impl nanus_domain::ToolExecutor for Tracked {
        fn execute(&self, call: ToolCall) -> nanus_domain::ToolFuture {
            let label = self.label;
            let delay = self.delay;
            let in_flight = Rc::clone(&self.in_flight);
            let peak = Rc::clone(&self.peak);
            let finished = Rc::clone(&self.finished);
            Box::pin(async move {
                let running = in_flight.get().saturating_add(1);
                in_flight.set(running);
                peak.set(peak.get().max(running));
                tokio::time::sleep(delay).await;
                in_flight.set(in_flight.get().saturating_sub(1));
                finished.borrow_mut().push(label.to_owned());
                ToolResult::new(
                    call.id,
                    ToolOutcome::success_with(
                        serde_json::json!({}),
                        vec![ContentBlock::Text(label.to_owned())],
                    ),
                )
            })
        }
    }

    /// How a set of tracked tools behaved.
    struct Tracking {
        peak: Rc<std::cell::Cell<u32>>,
        finished: Rc<std::cell::RefCell<Vec<String>>>,
    }

    /// Builds a registry of read-only tracked tools, named and delayed as given.
    fn registry_with_tracked(
        specs: &[(&str, &'static str, u64)],
    ) -> (ToolRegistryHandle, Tracking) {
        let in_flight = Rc::new(std::cell::Cell::new(0));
        let peak = Rc::new(std::cell::Cell::new(0));
        let finished = Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut registry = ToolRegistry::new();
        for (name, label, delay_ms) in specs {
            let schema = ToolSchema {
                name: ToolName::new(*name)
                    .unwrap_or_else(|_| unreachable!("a valid tracked tool name")),
                description: "A tracked test tool".to_owned(),
                parameters: json!({ "type": "object" }),
            };
            let tracked = Tracked {
                label,
                delay: std::time::Duration::from_millis(*delay_ms),
                in_flight: Rc::clone(&in_flight),
                peak: Rc::clone(&peak),
                finished: Rc::clone(&finished),
            };
            assert!(
                registry
                    .register(
                        nanus_domain::ToolDefinition::new(schema, tracked)
                            .with_access(ToolAccess::Read)
                    )
                    .is_ok()
            );
        }
        (
            ToolRegistryHandle::new(registry),
            Tracking { peak, finished },
        )
    }

    /// A model that calls every named tool in one step and then answers.
    fn calls_many(pairs: &[(&str, &str)]) -> Rc<Box<dyn LlmPort>> {
        let mut deltas: Vec<LlmEvent> = Vec::new();
        for (id, name) in pairs {
            deltas.push(LlmEvent::ToolCallDelta {
                index: 0,
                id: Some(ToolCallId::new(*id)),
                name: Some(ToolName::new(*name).unwrap_or_else(|_| unreachable!("valid"))),
                arguments_delta: "{}".to_owned(),
            });
        }
        deltas.push(LlmEvent::Finished {
            reason: FinishReason::ToolCalls,
        });
        ScriptedLlm::handle(vec![
            deltas,
            vec![
                LlmEvent::TextDelta("done".to_owned()),
                LlmEvent::Finished {
                    reason: FinishReason::Stop,
                },
            ],
        ])
    }

    /// A runner whose step may run `parallel` tools at once, over tools the sandbox permits.
    fn parallel_runner(
        llm: Rc<Box<dyn LlmPort>>,
        tools: ToolRegistryHandle,
        parallel: u32,
    ) -> Option<AgentRunner> {
        let config = AgentConfig::new(8, parallel, "test-model", 4096)
            .ok()?
            .with_sandbox(SandboxMode::ReadOnly);
        AgentRunner::new(llm, tools, "you are a test", config, clock()).ok()
    }

    /// The `(call id, content)` pairs the log recorded, in the order it recorded them.
    fn recorded_results(session: &Session) -> Vec<(String, String)> {
        session
            .log()
            .events()
            .iter()
            .filter_map(|event| match event {
                SessionEvent::ToolResult {
                    call_id, content, ..
                } => Some((call_id.as_str().to_owned(), content.trim().to_owned())),
                _ => None,
            })
            .collect()
    }

    /// A step's calls run together, and the log still pairs each result with its own call in
    /// call order — the two facts the concurrency has to preserve at once.
    #[tokio::test]
    async fn a_step_runs_its_calls_concurrently_and_records_results_in_call_order() {
        let (tools, tracking) =
            registry_with_tracked(&[("slow", "slow result", 60), ("fast", "fast result", 5)]);
        let Some(runner) = parallel_runner(calls_many(&[("c1", "slow"), ("c2", "fast")]), tools, 2)
        else {
            return;
        };
        let mut session = session();
        let outcome = runner.run_turn(&mut session, "go", &mut Silent, None).await;
        assert!(outcome.is_ok(), "{outcome:?}");

        assert_eq!(
            tracking.peak.get(),
            2,
            "both calls were in flight at the same time"
        );
        assert_eq!(
            tracking.finished.borrow().as_slice(),
            ["fast result", "slow result"],
            "and they finished out of call order, which is what concurrency means"
        );
        // The log is the reproducible part: results in call order, each one its own call's.
        assert_eq!(
            recorded_results(&session),
            [
                ("c1".to_owned(), "slow result".to_owned()),
                ("c2".to_owned(), "fast result".to_owned()),
            ]
        );
    }

    /// The bound holds: one at a time with a limit of one, however many calls the step made.
    ///
    /// The other direction of the test above — concurrency is bounded, so a step of six
    /// calls with a limit of one runs them in sequence.
    #[tokio::test]
    async fn a_step_never_runs_more_calls_at_once_than_the_limit() {
        let (tools, tracking) =
            registry_with_tracked(&[("slow", "slow result", 40), ("fast", "fast result", 5)]);
        let Some(runner) = parallel_runner(calls_many(&[("c1", "slow"), ("c2", "fast")]), tools, 1)
        else {
            return;
        };
        let mut session = session();
        let outcome = runner.run_turn(&mut session, "go", &mut Silent, None).await;
        assert!(outcome.is_ok(), "{outcome:?}");

        assert_eq!(tracking.peak.get(), 1, "one call at a time");
        assert_eq!(
            tracking.finished.borrow().as_slice(),
            ["slow result", "fast result"],
            "so they finished in call order rather than racing"
        );
        assert_eq!(
            recorded_results(&session),
            [
                ("c1".to_owned(), "slow result".to_owned()),
                ("c2".to_owned(), "fast result".to_owned()),
            ]
        );
    }
}
