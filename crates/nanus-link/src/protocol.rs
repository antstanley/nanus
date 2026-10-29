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

/// How far an *always* answer to an approval question reaches.
///
/// Read only when the answer allows the call and is `always`: a one-call answer reaches no
/// further than the call, whatever it says here.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalScope {
    /// The session the question was asked in, for as long as the agent holds it.
    ///
    /// The default, and what every *always* answer meant before the scope existed.
    #[default]
    Session,
    /// Every session the agent holds, now and later, until the agent stops.
    ///
    /// The agent tells every attached client the new set with [`Frame::ApprovedToolsChanged`],
    /// and keeps it in memory only: a host that wants it to outlive the agent records it in its
    /// own configuration's `approved_tools`.
    Agent,
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

/// What a client asks to do about a session's context.
///
/// Status never borrows a running session: it reads the last published snapshot. Every mutation
/// is refused while the session is busy and persisted before it is acknowledged. Mode changes
/// preserve the managed selection; only reset clears it. Actions are bare words on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextAction {
    /// Report the context status without changing anything.
    Status,
    /// Select whole-turn fitting, retaining the managed selection for later re-enabling.
    Legacy,
    /// Enable managed selection, retaining the reservation and leaving capture unchanged.
    Managed,
    /// Empty the selection, select legacy replay, and persist that decision.
    Reset,
}

/// A durable point in a session's log, in the link's vocabulary.
///
/// The link's own copy of the domain's frontier, for the reason [`TurnEnd`] is its own: a client
/// that only draws links the protocol and not the domain. The fields and their spelling are the
/// domain's, so the JSON is the same either way.
///
/// The event count is exclusive — sequence `N` is inside the frontier exactly when `N` is less
/// than it — and the digest covers the stored header and those events byte for byte. A reader
/// verifies a file it reads against both before it trusts any of it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrontierInfo {
    /// The session the frontier belongs to.
    pub session_id: String,
    /// How many events are durable.
    pub event_count: u64,
    /// BLAKE3 of the stored header and the first `event_count` event lines, lowercase hex.
    pub prefix_blake3: String,
    /// The context revision accepted at this point; zero for a session that has none.
    pub projection_revision: u64,
}

/// Where a session's stream stood at an attachment barrier.
///
/// What [`Frame::Attached`] and [`Frame::Backlog`] both carry, from one snapshot, so a client can
/// tell that the two describe the same instant: the durable prefix it reads from the store, and
/// the last frame id that prefix and the backlog together account for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamMark {
    /// The stream's identity: one per held session per process.
    ///
    /// A frame id is meaningful only inside its epoch. Ids are not kept across a restart or a
    /// session being let go and held again, so a frame from another epoch is refused rather than
    /// compared.
    pub stream_epoch: String,
    /// The last frame id the session assigned before the barrier.
    pub stream_watermark: u64,
    /// What the store held at the barrier.
    pub frontier: FrontierInfo,
    /// The event count of the session's active compaction marker, when one exists.
    ///
    /// For a client that has no store to read: it says where the cut is without the log. An
    /// interface that replays the log draws its divider from the marker it finds there and need
    /// not read this. Absent means no cut has been made, so the whole log is the conversation.
    /// Defaulted and omitted on the wire when absent, so an attachment to a session that was never
    /// compacted is byte-identical to before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction_boundary: Option<u64>,
}

/// What a maintenance request (a compaction or a handoff draft) cost, as the provider reported it.
///
/// Every counter is a count of tokens for that one request. A reader adds it to nothing: the log
/// already has the same figures for the turn, and this is the cost of the draft alone.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct DraftUsage {
    /// Tokens sent to the model.
    #[serde(default)]
    pub prompt_tokens: u32,
    /// Tokens the model generated, reasoning included.
    #[serde(default)]
    pub completion_tokens: u32,
    /// The share of `completion_tokens` spent reasoning.
    #[serde(default)]
    pub reasoning_tokens: u32,
    /// The share of `prompt_tokens` served from the provider's cache.
    #[serde(default)]
    pub cache_hit_tokens: u32,
    /// The share of `prompt_tokens` that missed the cache.
    #[serde(default)]
    pub cache_miss_tokens: u32,
}

/// One frame of a running turn that the store does not hold yet, tagged with where it belongs.
///
/// The tag is the step identity the frame was produced in, which is what lets the agent retire
/// exactly the segments a checkpoint covers, and the id is the frame's place in the session's
/// stream. Adjacent deltas are folded into one segment, which takes the id of the last delta it
/// holds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BacklogSegment {
    /// The stream id of the newest frame this segment holds.
    pub frame_id: u64,
    /// The turn it was produced in, counting from zero.
    pub turn: Option<u64>,
    /// The step it was produced in, counting from one; `None` before the first step.
    pub step: Option<u64>,
    /// The frame itself.
    pub frame: Frame,
}

/// The envelope every context frame carries.
///
/// The ids are assigned by the session that owns the stream, not by the connection that sends
/// them, so every viewer of one session sees the same id for the same frame and a client can
/// drop a duplicate or an older id from its attachment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextEnvelope {
    /// The session the frame is about.
    pub session_id: String,
    /// The stream's identity; see [`StreamMark::stream_epoch`].
    pub stream_epoch: String,
    /// The turn it belongs to, or `None` when the session is idle.
    pub turn: Option<u64>,
    /// The step it belongs to, or `None` when the session is idle or between steps.
    pub step: Option<u64>,
    /// This frame's id in the stream.
    pub frame_id: u64,
    /// The session's stream watermark once this frame is sent: its own id.
    pub stream_watermark: u64,
    /// What the store holds as this frame goes out.
    pub frontier: FrontierInfo,
}

/// Whether a session's requests replay the log or use the managed projection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextModeState {
    /// Whole-log replay; what every session did before managed context.
    Legacy,
    /// Fragment selection, working notes and checkpoints.
    Managed,
}

/// What became of one context decision, in the link's vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionState {
    /// Validated, waiting for its step to settle.
    Staged,
    /// A checkpoint acknowledged the revision.
    Accepted,
    /// Refused; the previous revision stays selected.
    Rejected,
    /// The turn stopped before validation began.
    Cancelled,
}

/// How strong a checkpoint's durability claim is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DurabilityState {
    /// Atomic replacement survives a process crash.
    ProcessCrash,
    /// File and parent directory are synchronized as well.
    PowerLoss,
}

/// The outcome of one context decision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextDecisionInfo {
    /// The host-generated id, which is also the id of the record the decision is written as.
    pub decision_id: String,
    /// What happened.
    pub outcome: DecisionState,
    /// The revision accepted, when one was.
    pub revision: Option<u64>,
    /// The stable code it was refused with, when it was.
    pub error_code: Option<String>,
}

/// A session's context status, in the link's vocabulary.
///
/// Counts, codes and digests only: no credential, no provider payload and no evidence crosses
/// here, which is the same rule the record it is read from keeps.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextStatusInfo {
    /// Legacy or managed.
    pub mode: ContextModeState,
    /// The accepted revision.
    pub revision: u64,
    /// The frontier the status describes.
    pub frontier: FrontierInfo,
    /// The estimated input of the last prepared request.
    pub estimate_input_tokens: Option<u64>,
    /// The estimated input of the protected floor.
    pub estimate_protected_tokens: Option<u64>,
    /// The output reservation.
    pub output_reserve_tokens: u64,
    /// Which estimator produced the estimates, and over what.
    pub estimator: String,
    /// Fragments hidden.
    pub hidden_fragments: u64,
    /// Fragments protected.
    pub protected_fragments: u64,
    /// The goal revision.
    pub goal_revision: Option<u64>,
    /// Whether goal data is in the generated memory.
    pub goal_data_available: bool,
    /// Whether recall can be used.
    pub recall_available: bool,
    /// Whether archived evidence can be used.
    pub archive_available: bool,
    /// The most recent decision.
    pub last_decision: Option<ContextDecisionInfo>,
    /// The snapshot profile's digest.
    pub profile_digest: String,
    /// Whether the next managed request can be prepared.
    pub managed_ready: bool,
    /// The stable code saying why it cannot, when it cannot.
    pub unavailable_reason: Option<String>,
    /// The prompt tokens of the newest assistant message that reported usage, in either mode.
    ///
    /// Null when none has, and exactly when [`Self::last_cache_hit_tokens`] is. Absent from an
    /// older agent's frame, which a reader treats as "no usage reported".
    #[serde(default)]
    pub last_prompt_tokens: Option<u64>,
    /// The share of those tokens the provider served from its prompt cache.
    ///
    /// Zero means the provider reported none, or reported no cache counter at all: the neutral
    /// usage record cannot tell the two apart. Null exactly when [`Self::last_prompt_tokens`] is.
    #[serde(default)]
    pub last_cache_hit_tokens: Option<u64>,
}

/// What a successful checkpoint proved.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointInfo {
    /// The frontier now on disk; its digest covers the whole stored file.
    pub frontier: FrontierInfo,
    /// BLAKE3 of the event lines alone.
    pub body_digest: String,
    /// The durability grade.
    pub durability: DurabilityState,
}

/// What a client asks an agent to do.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(tag = "request", rename_all = "snake_case")]
pub enum Request {
    /// Start a session and attach this connection to it.
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

    /// Draft a handoff from the managed session this connection is attached to.
    ///
    /// The agent reserves the session while the model writes the narrative, then answers with
    /// [`Frame::HandoffDraft`] or [`Frame::Refused`]. Nothing is created yet: the draft is held
    /// on the connection until [`Request::HandoffCommit`] or until it is discarded. `name` names
    /// the new session when it is committed: one word, under the store's name rules, and free —
    /// a name that breaks any of those is refused before any model call. The draft runs in its own
    /// task, so [`Request::Interrupt`] from this connection stops it inside its model call; it is
    /// then refused as `handoff_unavailable`. On a connection with no session, it is refused as
    /// `handoff_unavailable` too.
    HandoffDraft {
        /// The name the new session will be recorded under, if any.
        name: Option<String>,
    },

    /// Commit the handoff draft this connection holds, creating the new session.
    ///
    /// The commit runs in the order the change spec gives: the source is checked against the draft,
    /// the new session is created and saved, the back-link is recorded in the source, and then this
    /// connection is attached to the new session, which is sent [`Frame::Seeded`] after its
    /// backlog. A mismatched or absent `draft_id` is refused as `stale_draft`, and the draft held
    /// stays held.
    HandoffCommit {
        /// The id [`Frame::HandoffDraft`] named.
        draft_id: String,
    },

    /// Draft a compaction of the legacy session this connection is attached to.
    ///
    /// Answered with [`Frame::CompactDraft`] or [`Frame::Refused`]. Writes nothing: the draft is
    /// held on the connection until [`Request::CompactCommit`], or until it is discarded. The
    /// draft runs in its own task, so [`Request::Interrupt`] from this connection stops it inside
    /// its model call; it is then refused as `compact_unavailable`, as it is on a connection with
    /// no session.
    CompactDraft,

    /// Commit the compaction draft this connection holds: the session is cut, saved, and every
    /// viewer is sent [`Frame::Compacted`].
    ///
    /// A mismatched or absent `draft_id` is refused as `stale_draft`. A session that moved after
    /// the draft is refused as `stale_draft` too, and the draft is discarded. A legacy body is
    /// saved as it always is; a body that still holds managed records, from a period when the
    /// session was managed, is saved through its checkpoint.
    CompactCommit {
        /// The id [`Frame::CompactDraft`] named.
        draft_id: String,
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
    /// something that is already true. An approval the turn is waiting on is answered as
    /// unavailable, so the wait ends with the turn; a maintenance draft that is running is
    /// stopped too, and answers with its refusal.
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
        /// How far an `always` answer reaches: this session, or every session the agent holds.
        ///
        /// Ignored unless the answer allows the call and is `always`. Defaulted on the way in to
        /// [`ApprovalScope::Session`], so a client that predates the field means what it always
        /// did. An older *agent* would ignore the field and grant the session alone, which is why
        /// adding it moved [`PROTOCOL_VERSION`].
        #[serde(default)]
        scope: ApprovalScope,
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

    /// Report, switch or reset the session's context.
    ///
    /// The session belongs to the agent; a client cannot change its policy itself. Status reads
    /// use the last published snapshot during a turn. Mutations of a busy session are refused;
    /// saved changes are broadcast as a checkpoint followed by status. A no-op mode change
    /// broadcasts status without writing a checkpoint.
    Context {
        /// What to do.
        action: ContextAction,
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
    ///
    /// It also says *where* the client is joining it. The frontier is the prefix of the log the
    /// store holds at the attachment barrier, and the client reads disk only through it: a file
    /// that has grown since is clipped to that prefix and verified against its digest, because
    /// what lies past it reaches the client as the [`Frame::Backlog`] and the frames after it.
    /// The watermark is the last frame id the barrier accounts for.
    Attached {
        /// The session.
        session: SessionInfo,
        /// Where its stream stood at the barrier.
        #[serde(flatten)]
        stream: StreamMark,
    },

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
    /// The model list, the models' effort steps and their image input travel with it because a
    /// provider change replaces all three, and a client that kept the old list would offer models
    /// the agent no longer has.
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
        /// Whether each offered model reads images, as [`AgentInfo::model_inputs`] says it.
        ///
        /// Defaulted on the way in, so a frame from an agent that predates the field decodes with
        /// every model's image input unknown.
        #[serde(default)]
        model_inputs: Vec<ModelInputs>,
    },

    /// A provider switch could not go ahead because no credential is configured.
    ///
    /// A separate frame rather than a [`Frame::Refused`] because it is the prompt that leads to the
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
    /// [`Frame::ProviderChanged`] follows on success, a [`Frame::Refused`] on failure — there is no
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

    /// The tools the agent runs without asking in every session: its standing approvals.
    ///
    /// Sent to every attached client whenever the set grows — an *always* answer with
    /// [`ApprovalScope::Agent`] — so a host can record it in its own configuration, which is the
    /// durable copy: the agent never writes one. Sent when a client attaches, too, but only when
    /// the set is not empty, as the effort is only sent when there is one: a frame announcing that
    /// nothing is approved would be noise on every attachment, and an empty set is what a client
    /// that has been told nothing assumes.
    ///
    /// The whole set rather than the tool just added, so a client that missed a change is
    /// corrected by the next one rather than drifting.
    ApprovedToolsChanged {
        /// Every tool approved for every session, in name order.
        tools: Vec<String>,
    },

    /// A handoff draft is ready: the document the new session would open with, for the client to
    /// show.
    ///
    /// The draft is held on the agent under `draft_id` until the client commits it or the
    /// connection lets go. Nothing has been written to either session yet.
    HandoffDraft {
        /// The id a [`Request::HandoffCommit`] names.
        draft_id: String,
        /// The whole document: the deterministic header, then the model's narrative.
        text: String,
    },
    /// A compaction draft is ready: the summary a cut would carry, for the client to show.
    ///
    /// Nothing has been written. The warning that earlier content leaves the model's reach is the
    /// interface's to say; `dropped_turns` is non-zero when the request that wrote the summary
    /// could not fit every turn, so the summary covers only the turns the model saw.
    CompactDraft {
        /// The id a [`Request::CompactCommit`] names.
        draft_id: String,
        /// The summary, which has passed the text rule.
        text: String,
        /// Whole turns the request left out, so the summary covers only what the model saw.
        dropped_turns: u32,
        /// What the summary request cost, when the provider reported it.
        #[serde(default)]
        usage: Option<DraftUsage>,
        /// The event count the cut would be made at: the session's count when the draft was made,
        /// which is the boundary the committed marker takes. Everything before it leaves the
        /// model's prompt.
        ///
        /// `0` from an agent that predates the field, and never otherwise, because a cut needs a
        /// completed turn before it: a reader shows a zero as an unknown count, not as a cut at
        /// the start.
        #[serde(default)]
        boundary_event_count: u64,
    },
    /// This session was started from a handoff: its opening document.
    ///
    /// Sent after [`Frame::Backlog`] on every attachment to a session whose log opens with a
    /// `context/seed`, so a reader always sees what it was started with, and on the attachment that
    /// creates the session by [`Request::HandoffCommit`].
    Seeded {
        /// The seed's document, exactly as it was drafted and confirmed.
        text: String,
        /// The session the document came from.
        source_session_id: String,
        /// How many events the source held when the document was drawn, from its frontier.
        source_event_count: u64,
        /// The name the new session holds, when it holds one.
        ///
        /// `None` means the session is unnamed. A client that asked for a name and reads `None` (or
        /// a different name) knows the session was created without it, which is what a naming
        /// failure after the seed was saved looks like. Absent on the wire from an agent that
        /// predates the field, and read as `None` there — which says nothing about whether the
        /// session is named, so an interface that asked for a name should not treat an absent field
        /// as a failure of that name.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// Whether the source session's log records this handoff.
        ///
        /// Read from the source's log — a `context/handoff` naming this session — whenever a
        /// client attaches, so every attachment, from any agent, gets the same answer. `false`
        /// when the source does not record it, which is what a back-link that could not be
        /// written after the new session was saved leaves, and also when the source cannot be
        /// read: the field claims only what was seen.
        back_link_recorded: bool,
    },
    /// A compaction was committed: everything before `boundary_event_count` is out of the prompt.
    ///
    /// Sent live to every viewer of the session. A client attaching later sees the same boundary in
    /// [`StreamMark::compaction_boundary`]. The pre-cut transcript stays on screen; the summary is
    /// what the model now reads in its place.
    Compacted {
        /// The marker's boundary: the event count it cut at, which is its own sequence.
        boundary_event_count: u64,
        /// The summary the model now reads for everything before the boundary.
        summary: String,
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

    /// The turn failed — or the turn a client asked for never started, or the connection stopped
    /// following the session. Either way, whatever turn the client was showing is over.
    Failed {
        /// The rendered reason, already user-facing.
        message: String,
    },

    /// A request was refused, and nothing about any turn changed.
    ///
    /// The answer to a request that is not a prompt — a model, provider, credential, or goal
    /// change the agent will not make. Separate from [`Frame::Failed`] because a client reads
    /// `Failed` as the end of the turn in front of it: a refused model switch sent while a turn
    /// ran used to dismiss the turn's open approval and send the next queued prompt into a
    /// session that was still busy. A `Refused` is only a sentence to show.
    ///
    /// A refusal of a maintenance request (a handoff or a compaction, drafted or committed), and of
    /// a context mode change the runner refused with a code (`compacted_session` for managed
    /// context on a compacted session), carries that stable code in `code`, so an interface can
    /// tell `stale_draft` from `compact_unavailable` without parsing the sentence. The sentence in
    /// `message` never repeats the code.
    Refused {
        /// The rendered reason, already user-facing. It does not repeat `code`.
        message: String,
        /// The stable code the request was refused with, when it has one.
        ///
        /// Absent — read as `None` — from an agent that predates the field, and from every refusal
        /// that has no code of its own. An interface that needs a code and finds none treats the
        /// refusal as an ordinary sentence. Defaulted and omitted when absent, so the refusals that
        /// carry no code keep their bytes, and `PROTOCOL_VERSION` does not move for it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<String>,
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
    /// It is *not* history. What the store holds is in the session log, which is where a
    /// client reads it from; this carries only the part of a running turn past the durable
    /// frontier. A legacy session's log is written when the turn ends, so its backlog is the
    /// whole turn; a managed session checkpoints as it runs, and each checkpoint retires the
    /// segments it covers, so its backlog is only what the last checkpoint did not cover. Not
    /// sent for an attachment that needs nothing.
    ///
    /// Nesting frames rather than repeating them as ordinary ones is what keeps the batch
    /// atomic: it is one item on the client's queue, so no live frame can slip between the
    /// frames it carries and reorder the turn. It identifies the same frontier and watermark
    /// as the [`Frame::Attached`] before it, from the same snapshot.
    Backlog {
        /// Where the stream stood at the barrier.
        #[serde(flatten)]
        stream: StreamMark,
        /// The turn past the frontier, oldest first.
        segments: Vec<BacklogSegment>,
    },

    /// The session's context status.
    ///
    /// Sent in answer to a status read, after a reset, and once per managed step before its
    /// request is sent, so a watcher sees the revision, the pressure and the hidden counts the
    /// model is about to answer from. A frame of its own rather than text in a notice, so an
    /// interface can tell it from the model's output and from a goal change.
    ContextStatus {
        /// Which session, which stream, which step and where the frame stands in it.
        #[serde(flatten)]
        envelope: ContextEnvelope,
        /// The status.
        ///
        /// Boxed because it is the largest payload a frame carries, and every other frame would
        /// otherwise be as large as it is. A box is transparent on the wire.
        payload: Box<ContextStatusInfo>,
    },

    /// A context decision settled: a proposal accepted, rejected or cancelled, or a fit made.
    ContextDecision {
        /// Which session, which stream, which step and where the frame stands in it.
        #[serde(flatten)]
        envelope: ContextEnvelope,
        /// The decision.
        payload: ContextDecisionInfo,
    },

    /// A checkpoint was acknowledged, and the durable frontier is now its receipt's.
    ///
    /// Not a turn's end, and not something to draw: a viewer already shows the records it
    /// covers, so the frame moves its watermark and its idea of what the store holds, and a
    /// client attaching later reads the newer prefix from disk and only the backlog that is left.
    Checkpoint {
        /// Which session, which stream, which step and where the frame stands in it.
        #[serde(flatten)]
        envelope: ContextEnvelope,
        /// What the checkpoint proved.
        payload: CheckpointInfo,
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
///
/// Version 9 added [`Frame::Refused`], and moved the refusals of non-prompt requests onto it:
/// an older client would read it as a decode error, and an older agent sends those refusals as
/// [`Frame::Failed`], which a newer client would read as a turn ending.
///
/// Version 10 changed what an attachment *means*. [`Frame::Attached`] and [`Frame::Backlog`]
/// now identify a durable frontier, a stream epoch and a watermark, and a backlog is only what
/// lies past the frontier — which a managed session moves at every checkpoint, mid-turn. An older
/// client reads the whole stored file and then applies the backlog, which is exactly the double
/// copy the frontier exists to prevent, so it has to be refused by version rather than trusted
/// to ignore fields it does not know. The same version added [`Request::Context`] and the three
/// context frames, [`Frame::ContextStatus`], [`Frame::ContextDecision`] and
/// [`Frame::Checkpoint`].
///
/// Version 11 adds explicit legacy/managed mode changes to [`ContextAction`]. An older agent
/// cannot decode those requests, so it must be refused at the handshake rather than mid-session.
///
/// Version 12 adds the handoff and compaction surface: [`Request::HandoffDraft`],
/// [`Request::HandoffCommit`], [`Request::CompactDraft`] and [`Request::CompactCommit`], and the
/// frames [`Frame::HandoffDraft`], [`Frame::CompactDraft`], [`Frame::Seeded`] and
/// [`Frame::Compacted`]. An agent at 11 cannot parse those requests, so it is refused at the
/// handshake. The attachment's stream mark also carries the compaction boundary, which an older
/// client would discard without knowing it was there.
///
/// Version 13 adds the agent's standing approvals: [`Frame::ApprovedToolsChanged`], and
/// [`ApprovalScope`] on [`Request::Approve`]. An older client cannot decode the frame, and an
/// older agent would read an agent-wide *always* as a session's — granting less than the person
/// chose and never saying so — so both are refused at the handshake rather than misread.
pub const PROTOCOL_VERSION: u32 = 13;

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
    /// Whether each offered model reads images, one entry per model in [`AgentInfo::models`].
    ///
    /// Carried so a client can say, before a reader attaches an image, that the model in force
    /// will not be able to read it — without keeping a list of model names of its own, which
    /// would go stale the first time a provider changed a model or a composition offered another
    /// set. The answer is the adapter's, for the exact model on the endpoint and wire in force.
    ///
    /// A sibling of [`AgentInfo::model_efforts`] rather than a field of [`ModelEfforts`] because
    /// adding a defaulted list changes nothing an older peer reads, so the protocol version stands;
    /// growing the existing entry would have changed its shape. Defaulted on the way in, so a
    /// handshake from an agent that predates the field decodes, and every model's image input is
    /// then unknown — see [`ModelInputs::images`] for why that is not the same as "text only".
    #[serde(default)]
    pub model_inputs: Vec<ModelInputs>,
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

/// What one model accepts as input besides text.
///
/// Carried beside [`AgentInfo::models`] because whether a model reads images is a fact about the
/// exact model on the endpoint and wire in force, and an interface that kept its own list of
/// text-only models would warn about the wrong ones.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct ModelInputs {
    /// The model id.
    pub model: String,
    /// Whether the model reads images: `Some(true)` when the agent would put one in front of it,
    /// `Some(false)` when the model is known not to read them, and `None` when nobody has said.
    ///
    /// Three states rather than a flag because "unknown" and "text only" are different facts. A
    /// model is reported as reading images only once the adapter has evidence that it does — the
    /// harness refuses an image for anything less — and a model with no evidence either way is
    /// unknown, not text-only: a client that warned on unknown would warn about a model it knows
    /// nothing about. A client treats `None`, an entry that is missing, and a list that is missing
    /// alike, as "do not warn". An `Option<bool>` rather than an enum of its own so that a state
    /// the harness grows later cannot become a frame an older client fails to decode: the agent
    /// decides which of the three it is, and the wire only ever carries those three.
    #[serde(default)]
    pub images: Option<bool>,
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
            // All three states, so the round trip shows that "unknown" survives as itself rather
            // than collapsing into "text only" on the way through.
            model_inputs: vec![
                ModelInputs {
                    model: "deepseek-flash".to_owned(),
                    images: Some(true),
                },
                ModelInputs {
                    model: "deepseek-v4-pro".to_owned(),
                    images: Some(false),
                },
                ModelInputs {
                    model: "an-unverified-model".to_owned(),
                    images: None,
                },
            ],
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

    fn frontier() -> FrontierInfo {
        FrontierInfo {
            session_id: "01a09558".to_owned(),
            event_count: 12,
            prefix_blake3: "ab".repeat(32),
            projection_revision: 3,
        }
    }

    fn mark() -> StreamMark {
        StreamMark {
            stream_epoch: "p1-t2-h3".to_owned(),
            stream_watermark: 41,
            frontier: frontier(),
            compaction_boundary: None,
        }
    }

    fn envelope(turn: Option<u64>, step: Option<u64>, frame_id: u64) -> ContextEnvelope {
        ContextEnvelope {
            session_id: "01a09558".to_owned(),
            stream_epoch: "p1-t2-h3".to_owned(),
            turn,
            step,
            frame_id,
            stream_watermark: frame_id,
            frontier: frontier(),
        }
    }

    fn status_info() -> ContextStatusInfo {
        ContextStatusInfo {
            mode: ContextModeState::Managed,
            revision: 3,
            frontier: frontier(),
            estimate_input_tokens: Some(12_000),
            estimate_protected_tokens: None,
            output_reserve_tokens: 8_192,
            estimator: "bytes/4 over the compiled request".to_owned(),
            hidden_fragments: 2,
            protected_fragments: 5,
            goal_revision: Some(1),
            goal_data_available: true,
            recall_available: true,
            archive_available: false,
            last_decision: Some(ContextDecisionInfo {
                decision_id: "d1".to_owned(),
                outcome: DecisionState::Rejected,
                revision: None,
                error_code: Some("stale_base".to_owned()),
            }),
            profile_digest: "cd".repeat(32),
            managed_ready: false,
            unavailable_reason: Some("protocol_incompatible".to_owned()),
            last_prompt_tokens: Some(400),
            last_cache_hit_tokens: Some(300),
        }
    }

    /// Every context frame, idle and mid-step, and the attachment pair that carries a frontier:
    /// each crosses the wire with its envelope at the top level and its payload intact.
    #[test]
    fn the_context_and_attachment_frames_round_trip() {
        let frames = [
            Frame::Attached {
                session: session_info(),
                stream: mark(),
            },
            Frame::Backlog {
                stream: mark(),
                segments: Vec::new(),
            },
            Frame::Backlog {
                stream: mark(),
                segments: vec![
                    BacklogSegment {
                        frame_id: 39,
                        turn: Some(4),
                        step: None,
                        frame: Frame::User {
                            text: "what is this".to_owned(),
                        },
                    },
                    BacklogSegment {
                        frame_id: 41,
                        turn: Some(4),
                        step: Some(2),
                        frame: Frame::Text {
                            delta: "hello".to_owned(),
                        },
                    },
                ],
            },
            Frame::ContextStatus {
                envelope: envelope(None, None, 42),
                payload: Box::new(status_info()),
            },
            Frame::ContextStatus {
                envelope: envelope(Some(4), Some(2), 43),
                payload: Box::new(ContextStatusInfo {
                    mode: ContextModeState::Legacy,
                    last_decision: None,
                    unavailable_reason: None,
                    last_prompt_tokens: None,
                    last_cache_hit_tokens: None,
                    ..status_info()
                }),
            },
            Frame::ContextDecision {
                envelope: envelope(Some(4), Some(2), 44),
                payload: ContextDecisionInfo {
                    decision_id: "d2".to_owned(),
                    outcome: DecisionState::Accepted,
                    revision: Some(4),
                    error_code: None,
                },
            },
            Frame::ContextDecision {
                envelope: envelope(Some(4), Some(3), 45),
                payload: ContextDecisionInfo {
                    decision_id: "d3".to_owned(),
                    outcome: DecisionState::Cancelled,
                    revision: None,
                    error_code: Some("cancelled".to_owned()),
                },
            },
            Frame::Checkpoint {
                envelope: envelope(Some(4), Some(3), 46),
                payload: CheckpointInfo {
                    frontier: frontier(),
                    body_digest: "ef".repeat(32),
                    durability: DurabilityState::ProcessCrash,
                },
            },
            Frame::Checkpoint {
                envelope: envelope(None, None, 47),
                payload: CheckpointInfo {
                    frontier: frontier(),
                    body_digest: "ef".repeat(32),
                    durability: DurabilityState::PowerLoss,
                },
            },
        ];
        for frame in frames {
            let encoded = encode(&frame);
            assert!(encoded.is_ok(), "encodes: {encoded:?}");
            let Ok(encoded) = encoded else { return };
            let decoded = decode::<Frame>(&encoded);
            assert_eq!(decoded.ok(), Some(frame.clone()), "round trip of {frame:?}");
        }
    }

    /// C14: a status from an older agent, which carries no usage fields, decodes with both null:
    /// the fields are defaulted, so the protocol version does not move for them.
    #[test]
    fn a_status_without_the_usage_fields_decodes_with_both_null() {
        let frame = Frame::ContextStatus {
            envelope: envelope(None, None, 9),
            payload: Box::new(status_info()),
        };
        let encoded = encode(&frame).unwrap_or_else(|error| panic!("{error}"));
        let mut value: serde_json::Value =
            serde_json::from_str(&encoded).unwrap_or_else(|error| panic!("{error}"));
        let payload = value["payload"]
            .as_object_mut()
            .unwrap_or_else(|| panic!("a payload object"));
        assert!(payload.remove("last_prompt_tokens").is_some());
        assert!(payload.remove("last_cache_hit_tokens").is_some());
        let decoded = decode::<Frame>(&value.to_string()).unwrap_or_else(|error| panic!("{error}"));
        let Frame::ContextStatus { payload, .. } = decoded else {
            panic!("a status frame")
        };
        assert_eq!(payload.last_prompt_tokens, None);
        assert_eq!(payload.last_cache_hit_tokens, None);
        assert_eq!(payload.revision, status_info().revision);
    }

    /// The envelope is flat on the wire, as the contract writes it: a reader that does not know
    /// this crate finds the session, the epoch and the ids beside the tag, not inside a wrapper.
    #[test]
    fn a_context_frame_carries_its_envelope_at_the_top_level() {
        let frame = Frame::Checkpoint {
            envelope: envelope(None, None, 7),
            payload: CheckpointInfo {
                frontier: frontier(),
                body_digest: "ef".repeat(32),
                durability: DurabilityState::ProcessCrash,
            },
        };
        let encoded = encode(&frame).unwrap_or_else(|error| panic!("{error}"));
        let value: serde_json::Value =
            serde_json::from_str(&encoded).unwrap_or_else(|error| panic!("{error}"));
        let object = value.as_object().unwrap_or_else(|| panic!("an object"));
        let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "frame",
                "frame_id",
                "frontier",
                "payload",
                "session_id",
                "step",
                "stream_epoch",
                "stream_watermark",
                "turn"
            ]
        );
        assert_eq!(object.get("frame"), Some(&serde_json::json!("checkpoint")));
        assert_eq!(object.get("turn"), Some(&serde_json::Value::Null));
        // A context frame ends no turn: a checkpoint mid-turn is not a `Done`.
        assert!(!frame.is_end_of_turn());
    }

    /// Every context action survives the wire unchanged; unknown actions are refused.
    #[test]
    fn the_context_requests_round_trip() {
        for action in [
            ContextAction::Status,
            ContextAction::Legacy,
            ContextAction::Managed,
            ContextAction::Reset,
        ] {
            let request = Request::Context { action };
            let encoded = encode(&request).unwrap_or_else(|error| panic!("{error}"));
            assert_eq!(decode::<Request>(&encoded).ok(), Some(request));
        }
        assert_eq!(
            decode::<Request>(r#"{"request":"context","action":"reset"}"#).ok(),
            Some(Request::Context {
                action: ContextAction::Reset
            })
        );
        assert!(
            decode::<Request>(r#"{"request":"context","action":"enable"}"#).is_err(),
            "an action this version does not offer is refused, not guessed"
        );
    }

    /// An attachment from a build that predates the frontier is a decode error rather than an
    /// attachment with an empty frontier: an older agent's attachment means "read the whole
    /// file", and a client that read it as "read nothing" would show an empty conversation.
    #[test]
    fn an_attachment_without_a_frontier_is_refused() {
        let old = r#"{"frame":"attached","session":"s","name":null,"title":null,"events":3,"busy":false,"viewers":1}"#;
        assert!(
            decode::<Frame>(old).is_err(),
            "the old shape does not decode"
        );
        let old_backlog = r#"{"frame":"backlog","frames":[]}"#;
        assert!(decode::<Frame>(old_backlog).is_err());
    }

    #[test]
    fn a_round_trip_preserves_every_frame() {
        let frames = [
            Frame::Ready(info()),
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
                model_inputs: vec![ModelInputs {
                    model: "glm-5.3-flashx".to_owned(),
                    images: None,
                }],
            },
            Frame::ProviderChanged {
                provider: "inception".to_owned(),
                plan: "api".to_owned(),
                models: vec!["mercury-3".to_owned()],
                model_efforts: Vec::new(),
                model_inputs: vec![ModelInputs {
                    model: "mercury-3".to_owned(),
                    images: Some(false),
                }],
            },
            Frame::ProviderChanged {
                provider: "deepseek".to_owned(),
                plan: "api".to_owned(),
                models: Vec::new(),
                model_efforts: Vec::new(),
                model_inputs: Vec::new(),
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
        let segment = |frame_id: u64, step: Option<u64>, frame: Frame| BacklogSegment {
            frame_id,
            turn: Some(0),
            step,
            frame,
        };
        for frame in [
            Frame::Backlog {
                stream: mark(),
                segments: Vec::new(),
            },
            Frame::Backlog {
                stream: mark(),
                segments: vec![
                    segment(
                        1,
                        None,
                        Frame::User {
                            text: "what is this".to_owned(),
                        },
                    ),
                    segment(2, Some(1), Frame::Step { step: 1 }),
                    segment(
                        4,
                        Some(1),
                        Frame::Text {
                            delta: "hello".to_owned(),
                        },
                    ),
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
                scope: ApprovalScope::Session,
            },
            Request::Approve {
                call_id: "call-1".to_owned(),
                allow: true,
                always: true,
                scope: ApprovalScope::Session,
            },
            Request::Approve {
                call_id: "call-1".to_owned(),
                allow: true,
                always: true,
                scope: ApprovalScope::Agent,
            },
            Request::Approve {
                call_id: "call-1".to_owned(),
                allow: false,
                always: false,
                scope: ApprovalScope::Session,
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
            Request::Context {
                action: ContextAction::Status,
            },
            Request::Context {
                action: ContextAction::Reset,
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
                model_inputs: Vec::new(),
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

    /// A handshake from an agent that reports effort steps but predates image input decodes, and
    /// it decodes with every model's image input unknown rather than refused or text-only.
    ///
    /// The field was added without moving the protocol version, which is only sound if an agent
    /// at version 12 that never sends it is still read correctly: as one that said nothing.
    #[test]
    fn a_handshake_from_an_agent_without_image_input_decodes_as_unknown() {
        let line = format!(
            r#"{{"frame":"ready","workspace":"/w","model":"m","models":["m"],
                "model_efforts":[{{"model":"m","efforts":["high"]}}],
                "provider":"p","plan":"api","providers":[],"tools":7,"version":{PROTOCOL_VERSION}}}"#
        );
        let decoded = decode::<Frame>(&line);
        let Ok(Frame::Ready(info)) = decoded else {
            panic!("an older agent's handshake decodes: {decoded:?}");
        };
        assert!(info.model_inputs.is_empty(), "{info:?}");
        assert_eq!(info.version, PROTOCOL_VERSION, "and is not refused for it");

        // The same of a provider change, which carries the list for the same reason.
        let decoded = decode::<Frame>(
            r#"{"frame":"provider_changed","provider":"p","plan":"api","models":["m"]}"#,
        );
        assert!(
            matches!(decoded, Ok(Frame::ProviderChanged { ref model_inputs, .. })
                if model_inputs.is_empty()),
            "{decoded:?}"
        );

        // An entry that names the model and says nothing about images is unknown too, and an
        // explicit `null` is the same fact: neither is read as "text only".
        for entry in [r#"{"model":"m"}"#, r#"{"model":"m","images":null}"#] {
            let decoded = decode::<ModelInputs>(entry);
            assert_eq!(
                decoded.ok(),
                Some(ModelInputs {
                    model: "m".to_owned(),
                    images: None,
                }),
                "{entry}"
            );
        }
        // While a stated answer is kept as stated, in both directions.
        for (entry, images) in [
            (r#"{"model":"m","images":true}"#, Some(true)),
            (r#"{"model":"m","images":false}"#, Some(false)),
        ] {
            assert_eq!(
                decode::<ModelInputs>(entry)
                    .ok()
                    .and_then(|inputs| inputs.images),
                images,
                "{entry}"
            );
        }
    }

    /// The other direction: a client that predates image input still reads a handshake and a
    /// provider change that carry it, which is the half of leaving the version alone that the
    /// test above does not reach.
    ///
    /// An older client is this decoder without the field, so what it meets is a key it does not
    /// know. Renaming the key to one no build knows puts this decoder in exactly that position:
    /// the frame must decode, and say nothing about images, rather than be refused for the key.
    #[test]
    fn a_client_without_image_input_reads_an_agent_that_sends_it() {
        let unknown_to_the_reader = |frame: &Frame| {
            let line = encode(frame).expect("the frame encodes");
            assert!(line.contains(r#""model_inputs":[{"#), "{line}");
            line.replace(r#""model_inputs":"#, r#""a_key_from_a_later_build":"#)
        };

        let sent = info();
        let decoded = decode::<Frame>(&unknown_to_the_reader(&Frame::Ready(sent.clone())));
        let Ok(Frame::Ready(read)) = decoded else {
            panic!("a newer agent's handshake decodes: {decoded:?}");
        };
        assert!(read.model_inputs.is_empty(), "{read:?}");
        assert_eq!(
            AgentInfo {
                model_inputs: Vec::new(),
                ..sent
            },
            read,
            "and every field the reader does know is read as sent"
        );

        let changed = Frame::ProviderChanged {
            provider: "inception".to_owned(),
            plan: "api".to_owned(),
            models: vec!["mercury-3".to_owned()],
            model_efforts: Vec::new(),
            model_inputs: vec![ModelInputs {
                model: "mercury-3".to_owned(),
                images: Some(false),
            }],
        };
        let decoded = decode::<Frame>(&unknown_to_the_reader(&changed));
        assert!(
            matches!(decoded, Ok(Frame::ProviderChanged { ref models, ref model_inputs, .. })
                if model_inputs.is_empty() && models == &["mercury-3"]),
            "{decoded:?}"
        );
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

    /// The agent's standing approvals cross as the whole set, in order, and an empty set is a set
    /// rather than an absence.
    #[test]
    fn the_approved_tools_frame_round_trips_the_whole_set() {
        for tools in [vec!["bash".to_owned(), "write".to_owned()], Vec::new()] {
            let frame = Frame::ApprovedToolsChanged { tools };
            let line = encode(&frame).unwrap_or_else(|error| panic!("encodes: {error}"));
            assert_eq!(decode::<Frame>(&line).ok(), Some(frame));
        }
        let wire = r#"{"frame":"approved_tools_changed","tools":["bash"]}"#;
        assert_eq!(
            decode::<Frame>(wire).ok(),
            Some(Frame::ApprovedToolsChanged {
                tools: vec!["bash".to_owned()],
            }),
            "the wire spelling is the one the docs give"
        );
    }

    /// An answer that names no scope is a session's, which is what every `always` meant before
    /// the scope existed: a client that predates it must not grant more than it asked for.
    #[test]
    fn an_approval_answer_without_a_scope_reaches_only_its_session() {
        let decoded =
            decode::<Request>(r#"{"request":"approve","call_id":"a1","allow":true,"always":true}"#);
        assert_eq!(
            decoded.ok(),
            Some(Request::Approve {
                call_id: "a1".to_owned(),
                allow: true,
                always: true,
                scope: ApprovalScope::Session,
            })
        );
        // The other direction: the agent-wide scope is its own spelling, and an unknown one is a
        // decode error rather than a quiet session grant.
        let agent = decode::<Request>(
            r#"{"request":"approve","call_id":"a1","allow":true,"always":true,"scope":"agent"}"#,
        );
        assert!(matches!(
            agent,
            Ok(Request::Approve {
                scope: ApprovalScope::Agent,
                ..
            })
        ));
        let unknown = decode::<Request>(
            r#"{"request":"approve","call_id":"a1","allow":true,"always":true,"scope":"world"}"#,
        );
        assert!(unknown.is_err(), "{unknown:?}");
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

    /// A refusal survives the wire as a refusal, so a client cannot read it as a turn ending.
    #[test]
    fn a_refusal_round_trips_as_itself() {
        let frame = Frame::Refused {
            message: "no such model: x".to_owned(),
            code: None,
        };
        let encoded = encode(&frame).unwrap_or_else(|error| panic!("{error}"));
        let decoded = decode::<Frame>(&encoded).unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(decoded, frame);
        assert!(!decoded.is_end_of_turn());
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
        assert!(
            !Frame::Refused {
                message: String::new(),
                code: None,
            }
            .is_end_of_turn(),
            "a refused request ends no turn"
        );
    }
}

#[cfg(test)]
mod maintenance_tests {
    use super::*;

    fn frontier() -> FrontierInfo {
        FrontierInfo {
            session_id: "s".to_owned(),
            event_count: 9,
            prefix_blake3: "ab".repeat(32),
            projection_revision: 0,
        }
    }

    /// Every maintenance request survives the wire, and each names the fields it carries.
    #[test]
    fn the_maintenance_requests_round_trip() {
        let requests = [
            Request::HandoffDraft { name: None },
            Request::HandoffDraft {
                name: Some("next".to_owned()),
            },
            Request::HandoffCommit {
                draft_id: "d1".to_owned(),
            },
            Request::CompactDraft,
            Request::CompactCommit {
                draft_id: "d2".to_owned(),
            },
        ];
        for request in requests {
            let line = encode(&request).unwrap_or_else(|error| panic!("{error}"));
            let decoded: Request = decode(&line).unwrap_or_else(|error| panic!("{error}"));
            assert_eq!(decoded, request);
        }
        let commit = encode(&Request::HandoffCommit {
            draft_id: "d1".to_owned(),
        })
        .unwrap_or_else(|error| panic!("{error}"));
        assert!(commit.contains(r#""request":"handoff_commit""#), "{commit}");
    }

    /// Every maintenance frame survives the wire, with its fields destructured so a new one is a
    /// compile error here.
    #[test]
    fn the_maintenance_frames_round_trip_with_every_field() {
        let frames = [
            Frame::HandoffDraft {
                draft_id: "d1".to_owned(),
                text: "source_session: s\n\nthe narrative".to_owned(),
            },
            Frame::CompactDraft {
                draft_id: "d2".to_owned(),
                text: "the summary".to_owned(),
                dropped_turns: 3,
                usage: Some(DraftUsage {
                    prompt_tokens: 100,
                    completion_tokens: 20,
                    reasoning_tokens: 4,
                    cache_hit_tokens: 60,
                    cache_miss_tokens: 40,
                }),
                boundary_event_count: 12,
            },
            Frame::Seeded {
                text: "the seed".to_owned(),
                source_session_id: "old".to_owned(),
                source_event_count: 42,
                name: Some("next".to_owned()),
                back_link_recorded: false,
            },
            Frame::Compacted {
                boundary_event_count: 17,
                summary: "the summary".to_owned(),
            },
        ];
        for frame in frames {
            let line = encode(&frame).unwrap_or_else(|error| panic!("{error}"));
            let decoded: Frame = decode(&line).unwrap_or_else(|error| panic!("{error}"));
            assert_eq!(decoded, frame);
            match decoded {
                Frame::HandoffDraft { draft_id, text } => {
                    assert_eq!(draft_id, "d1");
                    assert!(text.starts_with("source_session:"));
                }
                Frame::CompactDraft {
                    draft_id,
                    text,
                    dropped_turns,
                    usage,
                    boundary_event_count,
                } => {
                    assert_eq!(
                        (draft_id.as_str(), text.as_str(), dropped_turns),
                        ("d2", "the summary", 3)
                    );
                    assert_eq!(boundary_event_count, 12);
                    assert_eq!(usage.map(|usage| usage.cache_hit_tokens), Some(60));
                }
                Frame::Seeded {
                    text,
                    source_session_id,
                    source_event_count,
                    name,
                    back_link_recorded,
                } => {
                    assert_eq!(text, "the seed");
                    assert_eq!(source_session_id, "old");
                    assert_eq!(source_event_count, 42);
                    assert_eq!(name.as_deref(), Some("next"));
                    assert!(
                        !back_link_recorded,
                        "a failed back-link is said, not assumed"
                    );
                }
                Frame::Compacted {
                    boundary_event_count,
                    summary,
                } => {
                    assert_eq!(boundary_event_count, 17);
                    assert_eq!(summary, "the summary");
                }
                other => panic!("{other:?}"),
            }
        }
    }

    /// A compaction draft with no usage and no cut count, from an agent that omits them, still
    /// decodes: no usage, and a count of zero that a reader shows as unknown.
    #[test]
    fn a_compaction_draft_without_usage_decodes_as_no_usage() {
        let line = r#"{"frame":"compact_draft","draft_id":"d1","text":"s","dropped_turns":0}"#;
        let decoded: Frame = decode(line).unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(
            decoded,
            Frame::CompactDraft {
                draft_id: "d1".to_owned(),
                text: "s".to_owned(),
                dropped_turns: 0,
                usage: None,
                boundary_event_count: 0,
            }
        );
    }

    /// An attachment with no cut is byte-identical to one from before the field existed, and a cut
    /// is named on the wire.
    #[test]
    fn the_stream_mark_names_a_compaction_boundary_only_when_there_is_one() {
        let uncut = StreamMark {
            stream_epoch: "e".to_owned(),
            stream_watermark: 1,
            frontier: frontier(),
            compaction_boundary: None,
        };
        let line = encode(&uncut).unwrap_or_else(|error| panic!("{error}"));
        assert!(!line.contains("compaction_boundary"), "{line}");
        let decoded: StreamMark = decode(&line).unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(decoded.compaction_boundary, None, "no cut reads as no cut");

        let cut = StreamMark {
            compaction_boundary: Some(8),
            ..uncut
        };
        let line = encode(&cut).unwrap_or_else(|error| panic!("{error}"));
        let decoded: StreamMark = decode(&line).unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(decoded.compaction_boundary, Some(8));
    }

    /// A seed's name round-trips, and an unnamed session's seed carries no name key at all.
    #[test]
    fn a_seed_names_the_session_it_holds_and_an_unnamed_one_omits_the_key() {
        let named = Frame::Seeded {
            text: "t".to_owned(),
            source_session_id: "old".to_owned(),
            source_event_count: 1,
            name: Some("next".to_owned()),
            back_link_recorded: true,
        };
        let line = encode(&named).unwrap_or_else(|error| panic!("{error}"));
        let decoded: Frame = decode(&line).unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(decoded, named);

        let unnamed = Frame::Seeded {
            text: "t".to_owned(),
            source_session_id: "old".to_owned(),
            source_event_count: 1,
            name: None,
            back_link_recorded: true,
        };
        let line = encode(&unnamed).unwrap_or_else(|error| panic!("{error}"));
        assert!(!line.contains(r#""name""#), "{line}");
        let old_agent = concat!(
            r#"{"frame":"seeded","text":"t","source_session_id":"old","#,
            r#""source_event_count":1,"back_link_recorded":true}"#
        );
        let decoded: Frame = decode(old_agent).unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(
            decoded, unnamed,
            "an agent without the field is read as unnamed"
        );
    }

    /// A refusal carries its stable code in `code`, and the sentence in `message` does not repeat
    /// it.
    #[test]
    fn a_coded_refusal_round_trips_its_code_beside_its_sentence() {
        let frame = Frame::Refused {
            message: "the session changed after this summary was drafted".to_owned(),
            code: Some("stale_draft".to_owned()),
        };
        let line = encode(&frame).unwrap_or_else(|error| panic!("{error}"));
        assert!(line.contains(r#""code":"stale_draft""#), "{line}");
        let decoded: Frame = decode(&line).unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(decoded, frame);
        let Frame::Refused { message, code } = decoded else {
            panic!("a refusal")
        };
        assert_eq!(code.as_deref(), Some("stale_draft"));
        assert!(
            !message.contains("stale_draft"),
            "the code is not repeated in the sentence"
        );
        assert!(!Frame::Refused { message, code }.is_end_of_turn());
    }

    /// A refusal with no code, from an agent that predates the field or a refusal that has none,
    /// decodes to `None`, and encodes without the key so its bytes are unchanged.
    #[test]
    fn a_refusal_without_a_code_decodes_as_no_code_and_keeps_its_bytes() {
        let uncoded = r#"{"frame":"refused","message":"no model is selected"}"#;
        let decoded: Frame = decode(uncoded).unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(
            decoded,
            Frame::Refused {
                message: "no model is selected".to_owned(),
                code: None,
            }
        );
        let encoded = encode(&decoded).unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(encoded, uncoded, "no code, no key");
    }
}
