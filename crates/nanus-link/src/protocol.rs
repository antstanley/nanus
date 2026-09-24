//! The frames an interface and an agent exchange.
//!
//! ## Why a vocabulary this small
//!
//! An interface needs three things from an agent: what it is, which session it is
//! talking about, and what happened while a turn ran. So the protocol has two shapes —
//! a request the client sends and a frame the agent sends back — and the frames are a
//! one-to-one transcription of the agent loop's [`nanus_bundle::Progress`] callbacks
//! plus the handshake, the attachment, and the ending. Nothing an interface might
//! *want* is here; anything an interface can learn about the conversation, the session
//! log already holds.
//!
//! ## Why attaching is explicit
//!
//! A connection used to be a conversation: connect, and you had a session. That is a
//! pleasant default and it made the lifetimes work, but it left no way to say *which*
//! conversation you wanted — so a session could be resumed only by reading it, and a
//! session an agent was holding open could not be reached at all. Now the handshake
//! describes the agent, `Request::New` and `Request::Attach` choose a session, and
//! everything else happens in the session the connection is attached to.
//!
//! The cost is one extra round trip and one more state for a client to be in. The
//! benefit is that `status` and `shutdown` no longer create a session just to ask a
//! question, and a session is something with an identity rather than something a
//! connection happens to be holding.
//!
//! ## Why lines of JSON
//!
//! One frame per line, newline-terminated. A streamed delta can contain a newline, and
//! `serde_json` escapes it, so a frame never contains a raw newline and the framing
//! cannot be confused with the payload — which a length-prefixed binary frame would also
//! manage, at the cost of a decoder. The transport is a local socket and the volume is a
//! model's output, so readability wins.
//!
//! ## Why the enum is internally tagged
//!
//! `{"frame":"text","delta":"…"}` is self-describing, so an unrecognised frame is a
//! named error rather than a silent misparse, and a log of the exchange can be read by
//! a person. The cost is a few bytes per frame on a socket that is not the bottleneck.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::error::{LinkError, LinkResult};

/// The reason a `Done` frame carries when the sender did not say one.
///
/// A serde default rather than an `Option` because every turn has a reason, and an agent
/// too old to send one only ever ended turns the way a completed turn ends.
fn completed_reason() -> TurnEnd {
    TurnEnd::Completed
}

/// Why a turn ended.
///
/// This crate's own vocabulary rather than the domain's turn-end reason, even though the
/// two say the same thing. A bare client does not link the domain — the two halves of the
/// link are split at a feature boundary so that an interface gets the frames and the
/// socket and nothing else — and the protocol is what such a client compiles against. The
/// server translates, and the translation is an exhaustive match, so a reason the domain
/// grows cannot quietly fail to cross the link.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TurnEnd {
    /// The model finished and nothing was owed.
    Completed,
    /// A human stopped the turn.
    Aborted {
        /// The recorded reason.
        reason: String,
    },
    /// A policy refused to continue.
    Blocked,
    /// The harness failed.
    Error {
        /// The rendered failure.
        message: String,
    },
    /// The model hit its output ceiling.
    MaxTokens,
    /// The turn hit its step budget.
    MaxSteps,
    /// The turn was interrupted.
    Interrupted,
}

/// How much an agent asks before running a call the sandbox refused.
///
/// The link's own vocabulary rather than the domain's approval policy, for the same reason
/// [`TurnEnd`] is its own: a client that only draws and answers links the protocol and not
/// the domain. The server translates, and the translation is an exhaustive match, so a
/// state the domain grows cannot quietly fail to cross.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalState {
    /// Ask about every exception.
    PerCall,
    /// Grant a non-destructive exception without asking; ask about the rest.
    Permitted,
    /// Grant every exception without asking.
    AllCalls,
}

/// How much reasoning effort an agent asks the model to spend.
///
/// The link's own vocabulary rather than the ports enum, for the same reason [`ApprovalState`]
/// is: a client that only draws links the protocol, and the ports crate is not something the
/// interface half of the link may name. The server translates, and the translation is an
/// exhaustive match, so a step the scale grows cannot quietly fail to cross.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffortState {
    /// Spend nothing: the provider's way of turning thinking off.
    None,
    /// Spend as little as possible while still thinking.
    Minimal,
    /// Spend a little.
    Low,
    /// Spend the provider's default amount.
    Medium,
    /// Spend more than the default.
    High,
    /// Spend still more, for long-horizon work.
    XHigh,
    /// Spend as much as the provider allows.
    Max,
}

/// Where a goal is in its lifecycle, in the link's own vocabulary.
///
/// This crate's own rather than the domain's, for the same reason [`TurnEnd`] and
/// [`ApprovalState`] are: a client that only draws links the protocol and not the
/// domain. The server translates, and the translation is an exhaustive match, so a
/// phase the domain grows cannot quietly fail to cross.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalState {
    /// Being pursued.
    Active,
    /// Suspended by a person.
    Paused,
    /// The objective is achieved.
    Complete,
    /// Given up on without achieving the objective.
    Abandoned,
}

/// A durable objective, as an interface draws it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalInfo {
    /// What is to be done.
    pub objective: String,
    /// Where it is in its lifecycle.
    pub state: GoalState,
    /// How many changes the goal has had, starting at one.
    pub revision: u64,
    /// When it was created, in milliseconds since the Unix epoch.
    pub created_at_ms: u64,
    /// When it was last changed, in milliseconds since the Unix epoch.
    pub updated_at_ms: u64,
    /// Why it is in its current phase, when a reason was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// What a client asks to do to a session's goal.
///
/// One request with an action rather than a variant per transition, because the
/// actions are one lifecycle: a client that speaks one has to speak the status read
/// that tells it what the others mean, and a single shape keeps them together. The
/// variant names are the words a person types after `/goal`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum GoalAction {
    /// Report the goal without changing it.
    Status,
    /// Create a goal, or replace the objective of the one that exists.
    Set {
        /// The new objective.
        objective: String,
    },
    /// Suspend a goal so it is not worked on.
    Pause,
    /// Resume a suspended goal.
    Resume,
    /// Mark the objective achieved, with the reason it was given.
    Complete {
        /// Why it is complete, when a reason was given.
        #[serde(default)]
        note: Option<String>,
    },
    /// Give up on the objective without achieving it, with the reason it was given.
    Abandon {
        /// Why the pursuit stopped, when a reason was given.
        #[serde(default)]
        note: Option<String>,
    },
    /// Remove the goal.
    Clear,
}

/// What a client asks an agent to do.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(tag = "request", rename_all = "snake_case")]
pub enum Request {
    ///
    /// Replaces whatever the connection was attached to. Naming it is optional, and a
    /// name that is already taken is refused rather than moved: an alias silently
    /// reassigned would make a resume open somebody else's conversation.
    New {
        /// The name to record it under, if any.
        name: Option<String>,
    },

    /// Attach to a session that already exists.
    ///
    /// The reference is a name or an id. The agent prefers a session it is already
    /// holding — so attaching to a running conversation joins it rather than loading a
    /// stale copy — and falls back to the store, which is what makes a session resumed
    /// after the agent restarted the same session.
    Attach {
        /// The name or id to attach to.
        session: String,
    },

    /// Run one turn and stream its progress.
    ///
    /// The turn joins the session this connection is attached to, and every client
    /// attached to that session sees it: a session is the conversation, and a
    /// connection is one view of it.
    Prompt {
        /// The prompt text.
        text: String,
    },

    /// Stop the turn running in the session this connection is attached to.
    ///
    /// A *request* rather than a keystroke because the turn is not the client's: it runs
    /// in a task the agent owns, in a session the agent holds, so that it outlives the
    /// terminal that asked for it. A client closing its window does not stop the turn, and
    /// neither should a client that merely stops wanting it — this is how it says so.
    ///
    /// Nothing is sent back. A turn that stops ends with the ending frame it always ends
    /// with, carrying [`TurnEnd::Interrupted`], and there is nothing an acknowledgement
    /// could add: a client that asked to stop a session which was not busy has asked for
    /// something that is already true.
    Interrupt,

    /// List the sessions the agent is holding open.
    Sessions,

    /// Answer an approval question about one tool call.
    ///
    /// The agent asks with [`Frame::Approval`] and waits; this is the answer. It is a
    /// *request* rather than a keystroke for the same reason [`Request::Interrupt`] is:
    /// the turn belongs to the session, not to the connection, and any client watching may
    /// be the one to answer. The first answer wins, and one is enough.
    Approve {
        /// The call the question was about, exactly as the approval frame named it.
        call_id: String,
        /// Whether the call may run.
        allow: bool,
        /// Whether the answer is a standing permission for this session.
        ///
        /// `true` records the tool so the same question is not asked again in this session;
        /// `false` grants the one call. Defaulted on the way in, so a client that predates
        /// the option sends an allow-once answer, which is what it always meant.
        #[serde(default)]
        always: bool,
    },

    /// Replace the agent's approval state for the rest of the session.
    ///
    /// The interface's own toggle, sent when a reader cycles the state. It is a request
    /// rather than part of a prompt because the *agent* owns the gate: the state decides how
    /// a call in a turn that is already running is treated, so it has to reach the loop
    /// rather than ride along with the next thing the model is asked.
    SetApproval {
        /// The state the agent should use from now on.
        state: ApprovalState,
    },

    /// Replace the model every later request will name.
    ///
    /// The interface's runtime switch, sent when a reader cycles or names a model. A request
    /// rather than part of a prompt because the *agent* issues the request to the provider: the
    /// choice has to reach the loop, so that a model switched while a turn is running affects
    /// the next step of that turn rather than the next conversation.
    ///
    /// An id the agent does not offer is refused with a sentence naming the ones it does, the
    /// same way the handshake refuses a protocol it does not speak: a request that quietly
    /// named a retired model would be answered by the provider, not by the harness.
    SetModel {
        /// The model id to use from now on.
        model: String,
    },

    /// Replace the reasoning effort every later request will carry.
    ///
    /// The same shape as [`Request::SetModel`] and for the same reason: the effort travels with
    /// the request the agent issues, so a choice has to reach the loop rather than ride along
    /// with the next prompt. The *link* does not validate it — the scale is the protocol's own —
    /// and the steps a model actually takes cross in [`AgentInfo::model_efforts`], so it is the
    /// interface that offers only those; a provider with no notion of effort ignores the choice.
    SetEffort {
        /// The effort to ask for from now on.
        state: EffortState,
    },

    /// Replace the provider (and plan) every later request goes to.
    ///
    /// A request rather than part of a prompt because the *agent* owns the adapter: a provider
    /// change rebuilds it in place, and every client attached to the agent is told, so two views
    /// cannot disagree about where the next request goes. An unknown provider, a refused plan, or
    /// a provider with no credential is answered rather than applied.
    SetProvider {
        /// The provider's name, as the provider table spells it.
        provider: String,
        /// The plan to use, when one is named. Absent means the provider's default plan.
        #[serde(default)]
        plan: Option<String>,
    },

    /// File a credential for a provider.
    ///
    /// The interface's way to store a key it has just asked for: the agent owns the credential
    /// store, so the key travels the local socket to the one process that can file it rather than
    /// the interface reaching for a store it does not link. The key is never echoed back.
    SetCredential {
        /// The provider's name.
        provider: String,
        /// The plan the credential is for, when one is named.
        ///
        /// A plan may take a credential of its own — z.ai's coding subscription does — so the key is
        /// filed against the account the plan names, not the provider's. Absent means the provider's
        /// default plan.
        #[serde(default)]
        plan: Option<String>,
        /// The secret to file under that account.
        key: String,
    },

    /// Change or report the session's goal.
    ///
    /// The interface's command for a durable objective, and a *request* rather than
    /// something a client does for itself because a goal is session state and the session
    /// belongs to the agent: a client cannot mutate a conversation it does not own, and two
    /// clients editing one goal have to meet somewhere the log does. The agent answers with
    /// [`Frame::Goal`], and the command's own output is not a prompt — it never reaches the
    /// model, exactly as the goal record it writes does not.
    Goal {
        /// What to do to the goal.
        action: GoalAction,
    },

    /// Describe the agent without changing anything.
    Status,

    /// Ask the agent to stop serving.
    Shutdown,
}

/// What an agent tells a client.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(tag = "frame", rename_all = "snake_case")]
pub enum Frame {
    /// The first frame on every connection, sent before anything is asked.
    ///
    /// It describes the *agent*, not a session: a client that gets no `Ready` knows it
    /// is not talking to an agent, and a client that only wants to ask a question does
    /// not have to create a conversation to do it.
    Ready(AgentInfo),

    /// This connection is attached to a session.
    ///
    /// Sent in reply to `New` and `Attach`, and it is the point at which a client knows
    /// which conversation it is in.
    Attached(SessionInfo),

    /// A reply to [`Request::Sessions`].
    ///
    /// A named field rather than a newtype, because an internally tagged enum cannot
    /// carry a bare sequence: the tag and the payload have to be one JSON object.
    Sessions {
        /// The sessions the agent is holding, most recently used first.
        held: Vec<SessionInfo>,
    },

    /// Somebody asked something in this session.
    ///
    /// Sent to every client attached to the session except the one that asked, which has
    /// already put its own words on screen. Without it a watcher would see answers to
    /// questions it never saw, which is precisely the conversation being unreadable.
    User {
        /// The prompt text.
        text: String,
    },

    /// More of the model's answer.
    Text {
        /// The delta, exactly as the model emitted it.
        delta: String,
    },

    /// More of the model's reasoning.
    Reasoning {
        /// The delta, exactly as the model emitted it.
        delta: String,
    },

    /// A step began.
    Step {
        /// The step number within the turn, counting from one.
        step: u32,
    },

    /// A tool is about to run.
    Tool {
        /// The provider's identity for this call, when the agent has one.
        ///
        /// What pairs this frame with the [`Frame::ToolDone`] that answers it. A step sends
        /// every call it made and then every result, and two calls in one step finish in
        /// whatever order they finish in, so neither position nor name can pair them —
        /// only this can, and only for an agent that has it. Absent from an agent too old to
        /// send one, which a client reads as "not identified" and pairs by name and order
        /// instead.
        ///
        /// Adding a field does not change what an older peer reads: it ignores the key, and
        /// its own frames decode without it. So this does not bump
        /// [`PROTOCOL_VERSION`].
        #[serde(default)]
        call_id: Option<String>,
        /// The tool's name.
        name: String,
        /// The arguments the model sent, as JSON.
        ///
        /// Carried so an interface can say what the call *is* — which file it reads,
        /// which command it runs — rather than only which tool it is, which is the part a
        /// reader cannot get anywhere else: the session log has the arguments, but a
        /// client that is watching a turn is not reading the log as it is written.
        ///
        /// Defaulted to null on the way in, so a frame from an agent that predates the
        /// field still decodes and renders as the name alone.
        #[serde(default)]
        arguments: serde_json::Value,
    },

    /// A tool finished.
    ToolDone {
        /// The identity of the call this answers, when the agent has one.
        ///
        /// See [`Frame::Tool`] for why it is here and why its absence is not an error.
        #[serde(default)]
        call_id: Option<String>,
        /// The tool's name.
        name: String,
        /// Whether it reported a failure, which is a result rather than a broken link.
        error: bool,
    },

    /// The agent is asking whether one tool call may run.
    ///
    /// Sent when the loop reaches a call the sandbox does not already permit and the state
    /// is not granting it, and sent to every client attached to the session — so a watcher
    /// sees both the question and, in the transcript that follows, what was decided. The
    /// turn waits for [`Request::Approve`], and a question nobody answers leaves the call
    /// denied.
    ///
    /// Only the tool and the harness's own reason cross the link. The arguments do not:
    /// the domain's approval request deliberately carries none, so model-controlled text
    /// cannot be placed in front of the person deciding.
    Approval {
        /// The call the question is about, and what an answer must name.
        call_id: String,
        /// The tool the model wants to run.
        tool: String,
        /// Why the harness is asking, in the harness's own words.
        ///
        /// Defaulted on the way in, so a reason-less question still decodes.
        #[serde(default)]
        reason: Option<String>,
    },

    /// The model the agent's next request will name.
    ///
    /// Sent once when a client attaches, so the interface knows what it is talking to before
    /// it draws, and again whenever any client changes it, so two views of one agent cannot
    /// disagree about which model is answering. A switch that reaches the agent therefore
    /// reaches every watcher, which is precisely what a shared session needs.
    ModelChanged {
        /// The model id the agent is using now.
        model: String,
    },

    /// The reasoning effort the agent's next request will carry.
    ///
    /// Sent whenever the choice changes, so every client attached to the agent agrees about how
    /// hard the model is being asked to think. The value in the handshake is the one in force at
    /// the moment of attaching; this is what keeps a client that attached earlier up to date.
    EffortChanged {
        /// The effort the agent is asking for now.
        state: EffortState,
    },

    /// The provider (and plan) the agent is talking to now.
    ///
    /// Sent whenever a client changes it, so every viewer agrees about where the next request goes.
    /// The model list and the models' effort steps travel with it because a provider change
    /// replaces both, and a client that kept the old list would offer models the agent no longer
    /// has.
    ProviderChanged {
        /// The provider's name.
        provider: String,
        /// The plan in force.
        plan: String,
        /// The model ids the new provider offers, the one in use first.
        models: Vec<String>,
        /// The effort steps each offered model takes.
        #[serde(default)]
        model_efforts: Vec<ModelEfforts>,
    },

    /// A provider switch could not go ahead because no credential is configured.
    ///
    /// A separate frame rather than a [`Frame::Failed`] because it is the prompt that leads to the
    /// interface asking for a key: matching the text of a refusal to decide whether to offer that
    /// would be guessing at a sentence.
    NoCredential {
        /// The provider whose credential is missing.
        provider: String,
        /// The plan whose credential is missing, when the provider has more than one account.
        #[serde(default)]
        plan: Option<String>,
        /// The environment variable a credential would otherwise be read from, so the interface can
        /// say where a key is normally kept.
        env: String,
    },

    /// The user must authorize a plan in a browser, and here is where.
    ///
    /// Sent when a plan is reached with an OAuth authorization and none is stored: the interface
    /// shows the page and the code, and the agent polls the service until the user finishes. A
    /// [`Frame::ProviderChanged`] follows on success, a [`Frame::Failed`] on failure — there is no
    /// reply the client sends, because the authorization is the service's to confirm.
    AuthPrompt {
        /// The provider being authorized.
        provider: String,
        /// The plan being authorized.
        #[serde(default)]
        plan: Option<String>,
        /// The page the user visits.
        url: String,
        /// The code they enter there.
        code: String,
    },

    /// The agent's approval state, for an interface to draw and cycle from.
    ///
    /// Sent once when a client attaches, so the interface knows the state before it draws
    /// anything, and again whenever any client changes it, so two views of one session do
    /// not disagree about what the next call will do.
    ApprovalChanged {
        /// The state the agent is using now.
        state: ApprovalState,
    },

    /// Usage was reported for the request that just completed.
    ///
    /// Everything here describes *one* model request rather than the session: an
    /// interface accumulates what it needs, and a session's own totals are already in
    /// the log. It carries the prompt-cache counters, the generated count, and the
    /// request's active time because the two rates an interface shows — tokens per
    /// second for the request, and the share of the prompt that was cached — are about
    /// this request's accounting and this request's clock.
    ///
    /// The request's time is reported as a decomposition rather than as one figure,
    /// because the parts answer different questions and only one of them is the model's
    /// generation speed. Issuing the request, waiting for the first token, generating, and
    /// closing the stream are four different costs; a rate that divides generated tokens by
    /// their sum charges the three that are not generation to the model, and understates it
    /// most for exactly the steps a coding session is made of — a tool call is a short
    /// generation behind a long wait.
    Usage {
        /// Tokens the request used, prompt and completion, which an interface
        /// accumulates rather than replaces.
        tokens: u32,
        /// Tokens the model generated, which is what a rate is measured against.
        ///
        /// Sent rather than left to be derived from `tokens` minus the cache counters,
        /// because a provider that reports no cache accounting would make that
        /// subtraction return the whole request as generation.
        #[serde(default)]
        completion_tokens: u32,
        /// The share of the request's prompt served from the provider's cache.
        ///
        /// Defaulted on the way in so a frame from an agent that predates these fields
        /// still decodes: a missing counter is zero rather than a protocol failure.
        #[serde(default)]
        cache_hit_tokens: u32,
        /// The share of the request's prompt that missed the provider's cache.
        #[serde(default)]
        cache_miss_tokens: u32,
        /// How long the request was in flight, in active milliseconds.
        ///
        /// Active rather than elapsed: it starts when the request is issued and ends
        /// when its stream does, so a rate computed from it excludes the idle time
        /// between turns and the tool time *within* one. It is the whole request, so it is
        /// `ttft_ms` plus the generation plus however long the stream took to close — which
        /// is why it is no longer what a rate is divided by.
        #[serde(default)]
        duration_ms: u64,
        /// The share of `completion_tokens` the model spent thinking.
        ///
        /// A subset of the generated count rather than a part of it, so it must not be added
        /// to `completion_tokens` — the provider bills the same tokens once. It is carried
        /// because thinking is generated at the model's speed but is not visible in the
        /// answer, so a single rate over both reports a number a reader watching the
        /// transcript cannot account for.
        #[serde(default)]
        reasoning_tokens: u32,
        /// How long the request took to reach the server and be answered at all, in
        /// milliseconds.
        ///
        /// The response head — the moment the server began replying, before any of the body —
        /// measured from the request being issued. Everything before it is connecting, uploading,
        /// and waiting to be answered; everything between it and the first token is the server's
        /// own work. Without both ends those are one number, and whether a slow wait is the
        /// network or the prompt is a guess.
        ///
        /// This is the nearer end of the split, and the only end the agent can measure: the far
        /// end is `ttft_ms`, so the server's own share is the difference. Zero when the
        /// provider announced no head, which is a blank rather than a measurement of no time.
        #[serde(default)]
        head_ms: u64,
        /// How long the request waited before its first generated token, in milliseconds.
        ///
        /// This is the wait a reader feels, and the part a rate that divides by the whole
        /// request silently charges to generation. It spans everything between issuing the
        /// request and the first token arriving — the provider's queue, the connection, and
        /// the prompt being read — so it bounds any of those rather than measuring prefill
        /// alone. Nothing at this layer can separate them, and the frame does not pretend to.
        #[serde(default)]
        ttft_ms: u64,
        /// How long the request spent generating: its first token to its last, in
        /// milliseconds.
        ///
        /// This is the only stretch during which the model was generating anything, so it is
        /// what a generation rate divides by. Zero when the stream carried fewer than two
        /// chunks — there is no interval between an instant and itself — and a reader that
        /// divides by it has to decide for itself what to do instead, because dividing by
        /// zero is not an answer.
        #[serde(default)]
        decode_ms: u64,
    },

    /// The session's goal, or its absence.
    ///
    /// Sent in answer to [`Request::Goal`], when a client attaches — only when a goal exists,
    /// so attaching to a session with none does not announce the absence of something nobody
    /// asked about — and to every client attached to the session whenever the goal changes, so
    /// two views of one conversation agree about its objective. Unlike the model and the
    /// approval state this is *session* state rather than the agent's, so only the session's
    /// own viewers are told.
    Goal {
        /// The goal, when there is one.
        goal: Option<GoalInfo>,
    },

    /// The turn ended, whatever the outcome.
    ///
    /// One frame rather than a success variant and a failure variant, because "the model
    /// ran out of steps with the work half done" is neither: the interface has to show
    /// what the model said *and* say why it stopped, and the reason is what tells it
    /// which of those it is looking at.
    ///
    /// Carrying the reason is the point of the field. This frame used to mean "the turn
    /// finished", so a turn that closed at its step budget arrived looking exactly like a
    /// completed one: the last thing the model had said was drawn as the final answer,
    /// the status line went back to ready, and nothing said the work had been cut off.
    /// `nanus run` had always called that a failed run; the link had no way to.
    Done {
        /// The model's final answer for this turn, empty when it produced none.
        answer: String,
        /// Why the turn ended.
        ///
        /// Defaulted to [`TurnEnd::Completed`] on the way in, so a frame from an agent
        /// that predates the field decodes as it always did.
        #[serde(default = "completed_reason")]
        reason: TurnEnd,
    },

    /// The turn failed.
    Failed {
        /// The rendered reason, already user-facing.
        message: String,
    },

    /// A reply to [`Request::Status`].
    Status(AgentInfo),

    /// The turn that was already running when this client attached, so far.
    ///
    /// Sent once, immediately after [`Frame::Attached`] and before any live frame of that
    /// turn, and only when there is a turn in flight. The frames inside are the ones a
    /// client that had been attached all along would already have seen — in the same order,
    /// with adjacent deltas folded together — so the client replays them and its transcript
    /// is whole rather than beginning mid-turn.
    ///
    /// It is *not* history. A turn that has ended is in the session log, which is where a
    /// client reads it from; this carries only the part of a running turn that the log does
    /// not have yet, because the log is written when the turn ends. Empty for an attachment
    /// to an idle session, which needs nothing.
    ///
    /// Nesting frames rather than repeating them as ordinary ones is what keeps the batch
    /// atomic: it is one item on the client's queue, so no live frame can slip between the
    /// frames it carries and reorder the turn.
    Backlog {
        /// The turn so far, oldest first.
        frames: Vec<Self>,
    },

    /// The prompt for the step that is starting had its oldest turns dropped.
    ///
    /// Sent before the step's own frames, once per step that was trimmed, because a reader
    /// following a conversation needs to know that the model is answering from part of it: the
    /// gap explains an answer that contradicts something said earlier, which otherwise looks
    /// like the model being wrong.
    Elided {
        /// How many messages were left out.
        dropped_messages: u32,
        /// How many turns those messages made up.
        dropped_turns: u32,
    },

    /// The agent is stopping.
    Bye,
}

impl Frame {
    /// Returns `true` when the frame ends a turn.
    ///
    /// A stream that has seen one of these is waiting for the next request rather than
    /// for more of this one, which is the property a client reader loops on.
    #[must_use]
    pub const fn is_end_of_turn(&self) -> bool {
        matches!(self, Self::Done { .. } | Self::Failed { .. })
    }
}

/// The version of this protocol, exchanged in the handshake.
///
/// The two halves ship together — `nanus tui` runs the interface binary from beside the
/// core, and never looks one up on `PATH` — so a mismatch means two builds from different
/// sources rather than two releases a user deliberately paired. Nothing stops that from
/// happening (a stale `NANUS_TUI`, a partial rebuild), and without a version the first
/// frame that changed shape is a decode error naming a field rather than a sentence naming
/// the mismatch. Bump this when a frame's meaning changes in a way an older peer would
/// misread — including when a variant is *added*, because an unknown variant is a decode
/// error rather than something an older peer can ignore.
///
/// The field is optional on the wire and defaults to zero, which is what a build that
/// predates versioning sends. Zero is therefore "too old to say", and a client refuses it
/// rather than assuming compatibility.
///
/// Version 8 added the goal surface — [`Request::Goal`], [`Frame::Goal`],
/// [`GoalAction`], and [`GoalState`] — so a client or agent from before that cannot
/// read the other's goal frames: an older peer meets an unknown variant as a decode
/// error naming a field, which is exactly the misread this moves for.
pub const PROTOCOL_VERSION: u32 = 8;

/// The version a handshake that carries none is read as.
///
/// A serde default rather than [`PROTOCOL_VERSION`], deliberately: an agent too old to send
/// a version is not one that speaks this version, and quietly treating it as one is the
/// silence the field exists to remove.
fn unversioned() -> u32 {
    0
}

/// What an agent says about itself.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct AgentInfo {
    /// The workspace the agent's tools are confined to.
    pub workspace: String,
    /// The model id the agent will call.
    pub model: String,
    /// The model ids a client may switch this agent to, the current one first.
    ///
    /// Carried so a client does not have to know the names: which models exist is a decision
    /// of the composition, and an interface that cycled a list of its own would offer models
    /// the agent would refuse. Defaulted on the way in, so a handshake from an agent that
    /// predates the field decodes — a client then has nothing to offer but knows the model in
    /// use, which is what it draws.
    #[serde(default)]
    pub models: Vec<String>,
    /// The reasoning effort the agent applies to a request that chooses none.
    ///
    /// `None` means the adapter has no notion of effort, which is not the same fact as any
    /// effort at all and is reported as the absence it is. Defaulted on the way in for the same
    /// reason `models` is.
    #[serde(default)]
    pub effort: Option<EffortState>,
    /// The effort steps each offered model takes, one entry per model in [`AgentInfo::models`].
    ///
    /// Which steps a model accepts is a provider fact that differs between models — one family
    /// takes `none` through `max`, another stops at `high` — so a client that offered the whole
    /// scale would offer steps the provider refuses. Defaulted on the way in, so a handshake from
    /// an agent that predates the field still decodes; a client then offers nothing and draws only
    /// the effort in force.
    #[serde(default)]
    pub model_efforts: Vec<ModelEfforts>,
    /// The provider the agent is talking to.
    ///
    /// Defaulted on the way in so a handshake from an agent that predates providers decodes; a
    /// client then draws nothing for it.
    #[serde(default)]
    pub provider: String,
    /// The plan in force for that provider.
    #[serde(default)]
    pub plan: String,
    /// The providers a client may switch this agent to, with the plans each offers.
    ///
    /// Carried for the same reason the models are: which providers exist, and which plans each has,
    /// is a decision of the composition, and an interface that offered a list of its own would
    /// offer a provider the agent refuses. Defaulted on the way in.
    #[serde(default)]
    pub providers: Vec<ProviderInfo>,
    /// How many tools the agent exposes.
    pub tools: usize,
    /// The link protocol version the agent speaks.
    ///
    /// Defaulted on the way in so a handshake from a build that predates the field still
    /// decodes; the client then refuses it by name rather than misreading it.
    #[serde(default = "unversioned")]
    pub version: u32,
}

/// The effort steps one model takes.
///
/// Carried beside [`AgentInfo::models`] because which steps a model accepts is a provider fact
/// that differs between models, and an interface that offered the whole scale would offer steps
/// the provider refuses.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct ModelEfforts {
    /// The model id.
    pub model: String,
    /// The effort steps it takes, in increasing order.
    pub efforts: Vec<EffortState>,
}

/// One provider a client may switch to, and the plans it offers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct ProviderInfo {
    /// The provider's name, as it is written in a configuration.
    pub name: String,
    /// The environment variable its credential is read from, for a prompt that names it.
    pub credential_env: String,
    /// The plans it offers, the default first.
    pub plans: Vec<PlanInfo>,
}

/// One plan of a provider.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct PlanInfo {
    /// The plan's name, as it is written in a configuration.
    pub name: String,
    /// The model the plan resolves to when the configuration names none.
    pub model: Option<String>,
    /// Why this build cannot use the plan, when it cannot.
    ///
    /// A plan that cannot be used is listed and refused by name rather than absent, so a reader is
    /// told what would have to change rather than that nothing exists.
    pub refused: Option<String>,
}

/// What an agent says about one session.
///
/// Deliberately a summary rather than the session: a client that wants the conversation
/// reads it from the store, where it is already durable. Sending a log down the link
/// would make the socket a second source of truth for something that already has one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct SessionInfo {
    /// The session's store key.
    pub session: String,
    /// The name a user gave it, if any.
    pub name: Option<String>,
    /// A title derived from its first human turn, if it has one yet.
    pub title: Option<String>,
    /// How many events it holds.
    pub events: u64,
    /// Whether a turn is running in it.
    ///
    /// A client that attaches to a busy session sees the rest of the turn rather than
    /// all of it, because the frames have already gone out. Its transcript is still
    /// whole: the agent records the turn, and the store is where a client reads history.
    pub busy: bool,
    /// How many clients are attached.
    pub viewers: usize,
}

/// Encodes a value as one line of JSON, **without** the terminating newline.
///
/// The caller writes the newline, so a writer cannot accidentally emit a frame and a
/// separator as two operations that a reader could observe separately.
///
/// # Errors
///
/// Returns [`LinkError::Protocol`] when the value cannot be encoded, which for these
/// types means a bug in the protocol rather than bad input.
pub fn encode<T: Serialize>(value: &T) -> LinkResult<String> {
    serde_json::to_string(value).map_err(|error| LinkError::protocol(error.to_string()))
}

/// Decodes one line, tolerating a trailing newline.
///
/// # Errors
///
/// Returns [`LinkError::Protocol`] when the line is not a frame or request of this
/// protocol, including when it carries an unknown tag.
pub fn decode<T: DeserializeOwned>(line: &str) -> LinkResult<T> {
    serde_json::from_str(line.trim_end_matches(['\n', '\r']))
        .map_err(|error| LinkError::protocol(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> AgentInfo {
        AgentInfo {
            workspace: "/work".to_owned(),
            model: "deepseek-flash".to_owned(),
            models: vec!["deepseek-flash".to_owned(), "deepseek-v4-pro".to_owned()],
            effort: Some(EffortState::Medium),
            model_efforts: Vec::new(),
            provider: "deepseek".to_owned(),
            plan: "api".to_owned(),
            providers: vec![ProviderInfo {
                name: "deepseek".to_owned(),
                credential_env: "DEEPSEEK_API_KEY".to_owned(),
                plans: vec![PlanInfo {
                    name: "api".to_owned(),
                    model: Some("deepseek-flash".to_owned()),
                    refused: None,
                }],
            }],
            tools: 7,
            version: PROTOCOL_VERSION,
        }
    }

    fn session_info() -> SessionInfo {
        SessionInfo {
            session: "01a09558".to_owned(),
            name: Some("the-glob-bug".to_owned()),
            title: Some("fix the glob bug".to_owned()),
            events: 12,
            busy: false,
            viewers: 2,
        }
    }

    fn goal_info() -> GoalInfo {
        GoalInfo {
            objective: "reduce p95 latency below 120 ms".to_owned(),
            state: GoalState::Active,
            revision: 1,
            created_at_ms: 1_700_000_000_000,
            updated_at_ms: 1_700_000_000_000,
            note: None,
        }
    }

    #[test]
    fn a_round_trip_preserves_every_frame() {
        let frames = [
            Frame::Ready(info()),
            Frame::Attached(session_info()),
            Frame::Sessions {
                held: vec![session_info()],
            },
            Frame::Sessions { held: Vec::new() },
            Frame::User {
                text: "what is this".to_owned(),
            },
            Frame::Text {
                delta: "hello".to_owned(),
            },
            Frame::Reasoning {
                delta: "thinking".to_owned(),
            },
            Frame::Step { step: 2 },
            Frame::Tool {
                call_id: Some("call-1".to_owned()),
                name: "read".to_owned(),
                arguments: serde_json::json!({"file_path": "src/main.rs"}),
            },
            Frame::Tool {
                call_id: None,
                name: "read".to_owned(),
                arguments: serde_json::json!({"file_path": "src/main.rs"}),
            },
            Frame::ToolDone {
                call_id: Some("call-1".to_owned()),
                name: "read".to_owned(),
                error: true,
            },
            Frame::ToolDone {
                call_id: None,
                name: "read".to_owned(),
                error: false,
            },
            Frame::Approval {
                call_id: "call-1".to_owned(),
                tool: "bash".to_owned(),
                reason: Some("the sandbox mode `read_only` does not permit execute".to_owned()),
            },
            Frame::ApprovalChanged {
                state: ApprovalState::PerCall,
            },
            Frame::ApprovalChanged {
                state: ApprovalState::Permitted,
            },
            Frame::ApprovalChanged {
                state: ApprovalState::AllCalls,
            },
            Frame::ModelChanged {
                model: "deepseek-v4-pro".to_owned(),
            },
            Frame::EffortChanged {
                state: EffortState::Minimal,
            },
            Frame::EffortChanged {
                state: EffortState::High,
            },
            Frame::Usage {
                tokens: 1234,
                completion_tokens: 90,
                cache_hit_tokens: 900,
                cache_miss_tokens: 100,
                duration_ms: 2500,
                reasoning_tokens: 40,
                ttft_ms: 700,
                decode_ms: 1_500,
                head_ms: 250,
            },
            Frame::Done {
                answer: "done".to_owned(),
                reason: TurnEnd::Completed,
            },
            Frame::Done {
                answer: String::new(),
                reason: TurnEnd::MaxSteps,
            },
            Frame::Failed {
                message: "boom".to_owned(),
            },
            Frame::Status(info()),
            Frame::Elided {
                dropped_messages: 12,
                dropped_turns: 2,
            },
            Frame::Elided {
                dropped_messages: 0,
                dropped_turns: 0,
            },
            Frame::Bye,
        ];
        for frame in frames {
            let encoded = encode(&frame);
            assert!(encoded.is_ok(), "encodes: {encoded:?}");
            let Ok(encoded) = encoded else { return };
            let decoded = decode::<Frame>(&encoded);
            assert_eq!(decoded.ok(), Some(frame.clone()), "round trip of {frame:?}");
        }
    }

    /// The goal frame carries an optional goal and an optional note — the two shapes a session
    /// with a goal and one that cleared it produce — so each gets a round trip.
    #[test]
    fn the_goal_frames_round_trip() {
        for frame in [
            Frame::Goal {
                goal: Some(goal_info()),
            },
            Frame::Goal {
                goal: Some(GoalInfo {
                    objective: "a paused objective".to_owned(),
                    state: GoalState::Paused,
                    revision: 4,
                    created_at_ms: 1,
                    updated_at_ms: 9,
                    note: Some("waiting on the release window".to_owned()),
                }),
            },
            Frame::Goal {
                goal: Some(GoalInfo {
                    objective: "an achieved objective".to_owned(),
                    state: GoalState::Complete,
                    revision: 7,
                    created_at_ms: 1,
                    updated_at_ms: 20,
                    note: None,
                }),
            },
            Frame::Goal {
                goal: Some(GoalInfo {
                    objective: "a given-up-on objective".to_owned(),
                    state: GoalState::Abandoned,
                    revision: 2,
                    created_at_ms: 1,
                    updated_at_ms: 30,
                    note: Some("this cannot be made deterministic".to_owned()),
                }),
            },
            Frame::Goal { goal: None },
        ] {
            let encoded = encode(&frame);
            assert!(encoded.is_ok(), "encodes: {encoded:?}");
            let Ok(encoded) = encoded else { return };
            let decoded = decode::<Frame>(&encoded);
            assert_eq!(decoded.ok(), Some(frame.clone()), "round trip of {frame:?}");
        }
    }

    /// The provider frames carry a model list with per-model effort steps — the one nesting here
    /// besides a backlog — so they get their own round trip.
    #[test]
    fn the_provider_frames_round_trip() {
        for frame in [
            Frame::ProviderChanged {
                provider: "zai".to_owned(),
                plan: "coding".to_owned(),
                models: vec!["glm-5.3-flashx".to_owned()],
                model_efforts: vec![ModelEfforts {
                    model: "glm-5.3-flashx".to_owned(),
                    efforts: vec![EffortState::High, EffortState::Max],
                }],
            },
            Frame::ProviderChanged {
                provider: "deepseek".to_owned(),
                plan: "api".to_owned(),
                models: Vec::new(),
                model_efforts: Vec::new(),
            },
            Frame::NoCredential {
                provider: "zai".to_owned(),
                plan: Some("coding".to_owned()),
                env: "ZAI_CODING_API_KEY".to_owned(),
            },
            Frame::NoCredential {
                provider: "openai".to_owned(),
                plan: None,
                env: "OPENAI_API_KEY".to_owned(),
            },
            Frame::AuthPrompt {
                provider: "openai".to_owned(),
                plan: Some("subscription".to_owned()),
                url: "https://auth.openai.com/codex/device".to_owned(),
                code: "ABCD-EFGH".to_owned(),
            },
        ] {
            let encoded = encode(&frame);
            assert!(encoded.is_ok(), "encodes: {encoded:?}");
            let Ok(encoded) = encoded else { return };
            let decoded = decode::<Frame>(&encoded);
            assert_eq!(decoded.ok(), Some(frame.clone()), "round trip of {frame:?}");
        }
    }

    /// A backlog is the one recursive shape here — a frame that carries frames — so it gets
    /// its own round trip: empty, for an attachment to an idle session, and carrying a turn,
    /// which is what a late client is actually sent.
    #[test]
    fn a_backlog_round_trips_the_frames_it_carries() {
        for frame in [
            Frame::Backlog { frames: Vec::new() },
            Frame::Backlog {
                frames: vec![
                    Frame::User {
                        text: "what is this".to_owned(),
                    },
                    Frame::Step { step: 1 },
                    Frame::Text {
                        delta: "hello".to_owned(),
                    },
                ],
            },
        ] {
            let encoded = encode(&frame);
            assert!(encoded.is_ok(), "encodes: {encoded:?}");
            let Ok(encoded) = encoded else { return };
            let decoded = decode::<Frame>(&encoded);
            assert_eq!(decoded.ok(), Some(frame.clone()), "round trip of {frame:?}");
        }
    }

    #[test]
    fn a_round_trip_preserves_every_request() {
        let requests = [
            Request::New {
                name: Some("the-glob-bug".to_owned()),
            },
            Request::New { name: None },
            Request::Attach {
                session: "the-glob-bug".to_owned(),
            },
            Request::Prompt {
                text: "do the thing".to_owned(),
            },
            Request::Approve {
                call_id: "call-1".to_owned(),
                allow: true,
                always: false,
            },
            Request::Approve {
                call_id: "call-1".to_owned(),
                allow: true,
                always: true,
            },
            Request::Approve {
                call_id: "call-1".to_owned(),
                allow: false,
                always: false,
            },
            Request::SetApproval {
                state: ApprovalState::AllCalls,
            },
            Request::SetModel {
                model: "deepseek-v4-pro".to_owned(),
            },
            Request::SetEffort {
                state: EffortState::Minimal,
            },
            Request::SetProvider {
                provider: "zai".to_owned(),
                plan: Some("coding".to_owned()),
            },
            Request::SetProvider {
                provider: "deepseek".to_owned(),
                plan: None,
            },
            Request::SetCredential {
                provider: "zai".to_owned(),
                plan: Some("coding".to_owned()),
                key: "zai-coding-secret".to_owned(),
            },
            Request::SetCredential {
                provider: "openai".to_owned(),
                plan: None,
                key: "sk-secret".to_owned(),
            },
            Request::Sessions,
            Request::Status,
            Request::Shutdown,
        ];
        for request in requests {
            let encoded = encode(&request);
            assert!(encoded.is_ok(), "encodes: {encoded:?}");
            let Ok(encoded) = encoded else { return };
            let decoded = decode::<Request>(&encoded);
            assert_eq!(
                decoded.ok(),
                Some(request.clone()),
                "round trip of {request:?}"
            );
        }
    }

    /// Every goal request, one per action, because the actions are what `/goal` and the model
    /// tools speak and each has to survive the wire unchanged.
    #[test]
    fn the_goal_requests_round_trip() {
        for action in [
            GoalAction::Status,
            GoalAction::Set {
                objective: "reduce p95 latency below 120 ms".to_owned(),
            },
            GoalAction::Pause,
            GoalAction::Resume,
            GoalAction::Complete {
                note: Some("the benchmark run is green".to_owned()),
            },
            GoalAction::Complete { note: None },
            GoalAction::Abandon {
                note: Some("the benchmark cannot be made deterministic".to_owned()),
            },
            GoalAction::Abandon { note: None },
            GoalAction::Clear,
        ] {
            let request = Request::Goal {
                action: action.clone(),
            };
            let encoded = encode(&request);
            assert!(encoded.is_ok(), "encodes: {encoded:?}");
            let Ok(encoded) = encoded else { return };
            let decoded = decode::<Request>(&encoded);
            assert_eq!(decoded.ok(), Some(request), "round trip of {action:?}");
        }
    }

    #[test]
    fn a_delta_containing_a_newline_still_encodes_to_one_line() {
        // The whole framing argument rests on this: the payload may contain anything at
        // all, and the frame must remain one line.
        let frame = Frame::Text {
            delta: "first\nsecond\r\nthird".to_owned(),
        };
        let Ok(encoded) = encode(&frame) else {
            panic!("a text frame encodes");
        };
        assert_eq!(encoded.lines().count(), 1, "one line: {encoded}");
        assert!(!encoded.contains('\n'), "no raw newline: {encoded}");
        assert_eq!(decode::<Frame>(&encoded).ok(), Some(frame));
    }

    /// A handshake that carries no version is not one that speaks this version.
    ///
    /// It decodes — a build that predates the field must not become an unreadable frame —
    /// and it reads as zero, which is "too old to say". The client refuses it rather than
    /// assuming the version it happens to speak, which is the whole point of the field.
    #[test]
    fn a_handshake_from_a_build_without_a_version_decodes_as_unversioned() {
        let decoded =
            decode::<Frame>(r#"{"frame":"ready","workspace":"/w","model":"m","tools":7}"#);
        assert_eq!(
            decoded.ok(),
            Some(Frame::Ready(AgentInfo {
                workspace: "/w".to_owned(),
                model: "m".to_owned(),
                models: Vec::new(),
                effort: None,
                model_efforts: Vec::new(),
                provider: String::new(),
                plan: String::new(),
                providers: Vec::new(),
                tools: 7,
                version: 0,
            }))
        );
        // And zero is not the current version, so a client cannot read it as compatible.
        assert_ne!(0, PROTOCOL_VERSION);
    }

    #[test]
    fn an_unknown_tag_is_a_named_failure_rather_than_a_silent_misparse() {
        let decoded = decode::<Frame>(r#"{"frame":"wibble"}"#);
        assert!(
            matches!(decoded, Err(LinkError::Protocol(_))),
            "{decoded:?}"
        );
        let Err(error) = decoded else { return };
        assert!(error.to_string().contains("wibble"), "{error}");
    }

    #[test]
    fn a_line_that_is_not_json_is_refused() {
        assert!(decode::<Frame>("not json at all").is_err());
        assert!(decode::<Request>("").is_err());
    }

    /// The cache counters, the generation window, and the active time were added to a frame
    /// that used to carry only a token count. A client talking to an agent that predates them
    /// must read the frame rather than fail on it, and an absent counter is zero.
    ///
    /// Zero is also what an older agent's frame *means* for the two durations: a reader that
    /// divides by the generation window has to treat zero as "not measured" rather than as an
    /// instantaneous request, which is why the interface falls back rather than dividing.
    #[test]
    fn a_usage_frame_from_an_older_agent_still_decodes() {
        let decoded = decode::<Frame>(r#"{"frame":"usage","tokens":42}"#);
        assert_eq!(
            decoded.ok(),
            Some(Frame::Usage {
                tokens: 42,
                completion_tokens: 0,
                cache_hit_tokens: 0,
                cache_miss_tokens: 0,
                duration_ms: 0,
                reasoning_tokens: 0,
                ttft_ms: 0,
                decode_ms: 0,
                head_ms: 0,
            })
        );
    }

    /// The arguments were added to a frame that used to carry only a tool's name. A
    /// client talking to an agent that predates them must read the frame rather than fail
    /// on it, and an absent call is a call that says nothing about what it is doing — the
    /// name alone — rather than a protocol error.
    #[test]
    fn a_tool_frame_from_an_older_agent_still_decodes() {
        let decoded = decode::<Frame>(r#"{"frame":"tool","name":"read"}"#);
        assert_eq!(
            decoded.ok(),
            Some(Frame::Tool {
                call_id: None,
                name: "read".to_owned(),
                arguments: serde_json::Value::Null,
            })
        );
    }

    /// The ending used to say only that the turn had finished, so an agent too old to
    /// carry a reason can only have meant the one reason that used to be unsayable
    /// because it was the only possibility. Reading it as anything else would turn an
    /// old agent's success into a failure notice.
    #[test]
    fn a_done_frame_from_an_older_agent_ends_a_completed_turn() {
        let decoded = decode::<Frame>(r#"{"frame":"done","answer":"hi"}"#);
        assert_eq!(
            decoded.ok(),
            Some(Frame::Done {
                answer: "hi".to_owned(),
                reason: TurnEnd::Completed,
            })
        );
    }

    /// The reason is the whole point of the field, so a frame that carries one must
    /// survive the round trip with it intact: a `max_steps` ending that decoded as
    /// `completed` would put the interface back where it started.
    #[test]
    fn an_ending_keeps_the_reason_it_was_sent_with() {
        let reasons = [
            TurnEnd::Completed,
            TurnEnd::MaxSteps,
            TurnEnd::MaxTokens,
            TurnEnd::Interrupted,
            TurnEnd::Blocked,
            TurnEnd::Aborted {
                reason: "the human stopped it".to_owned(),
            },
            TurnEnd::Error {
                message: "the model call failed".to_owned(),
            },
        ];
        for reason in reasons {
            let frame = Frame::Done {
                answer: "half an answer".to_owned(),
                reason: reason.clone(),
            };
            let encoded = encode(&frame);
            assert!(encoded.is_ok(), "a frame with a reason encodes: {reason:?}");
            let Ok(encoded) = encoded else { return };
            let decoded = decode::<Frame>(&encoded);
            assert_eq!(decoded.ok(), Some(frame), "the reason survives: {reason:?}");
        }
    }

    #[test]
    fn only_done_and_failed_end_a_turn() {
        assert!(
            Frame::Done {
                answer: String::new(),
                reason: TurnEnd::Completed,
            }
            .is_end_of_turn()
        );
        assert!(
            Frame::Failed {
                message: String::new()
            }
            .is_end_of_turn()
        );
        assert!(!Frame::Bye.is_end_of_turn());
        assert!(!Frame::Step { step: 1 }.is_end_of_turn());
    }
}
