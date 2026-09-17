//! The append-only session event log.
//!
//! A session is the only history a model is ever shown. There is no separate
//! transcript, no in-memory message list that could drift from what was
//! persisted, and no way to mutate a past event: the log is append-only, and
//! [`SessionLog::derive_messages`] is a pure fold over it.
//!
//! That single-source rule is what makes a session resumable, replayable, and
//! auditable. It is also what makes the *surface fold* worth stating precisely:
//! which events become model-visible messages is a decision, not an accident.
//!
//! ## The surface fold
//!
//! - [`SessionEvent::UserMessage`] becomes a user message, verbatim.
//! - [`SessionEvent::AssistantMessage`] becomes an assistant message when it has
//!   non-empty text or at least one tool call. An assistant turn with neither is
//!   skipped: it carries no model-visible content, and replaying it would spend
//!   tokens on nothing.
//! - [`SessionEvent::ToolResult`] becomes a tool message, verbatim.
//! - Everything else — turn and step boundaries, and the tool-call audit record —
//!   is harness bookkeeping and never reaches a model.
//!
//! ## JSONL framing
//!
//! A session file is a header line followed by one envelope per event:
//!
//! ```text
//! {"format":"nanus.session","version":1,"id":"...","created_at_ms":0,"cwd":"/work",
//!  "origin":{"model":"deepseek-flash","effort":"medium"}}
//! {"seq":0,"event":{"type":"turn_start","turn":0}}
//! {"seq":1,"event":{"type":"user_message","text":"hello"}}
//! ```
//!
//! The sequence number is explicit in the file so a hole in it is detectable.
//! Without it, a truncated write and a complete one are indistinguishable.
//!
//! ## What a header may gain
//!
//! `origin` is written on one line in the file; it is folded above to keep the
//! example readable. A field added to the header is not a change to the *body*, so
//! it does not move [`SESSION_FORMAT_VERSION`]: an older build ignores a header key
//! it does not know, and a newer build reads one that is missing as the absence it
//! is. The version moves when the shape of an *event* changes, which is what it
//! promises to guard.

use core::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::message::{Message, ToolCallId, Usage};
use crate::tool::{ToolCall, ToolName};

/// The format tag every session header carries.
pub const SESSION_FORMAT_TAG: &str = "nanus.session";

/// The session file format version this crate writes and accepts.
pub const SESSION_FORMAT_VERSION: u32 = 1;

/// Maximum number of characters in a derived session title.
const TITLE_MAX_CHARS: usize = 72;

/// The identity of a session.
///
/// Opaque to the domain: it is a store key, and the store decides what shape a
/// key takes. It is not validated here because a rejected id is a store
/// concern, and [`Session::from_jsonl`] reports an empty one as a typed header
/// error rather than constructing a session around it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(String);

impl SessionId {
    /// Wraps a session key.
    #[must_use]
    pub fn new(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    /// Returns the id as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consumes the id, returning the underlying string.
    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }

    /// Returns `true` when the id is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The position of an event in a session log.
///
/// Sequence numbers start at zero and are contiguous. They are `u64` rather than
/// `usize` because they are written to a file and must mean the same thing on
/// every platform that reads it back.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct SessionSeq(u64);

impl SessionSeq {
    /// Wraps a sequence number.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the number.
    #[must_use]
    pub const fn value(self) -> u64 {
        self.0
    }

    /// Returns the next sequence number, saturating at the ceiling.
    #[must_use]
    pub const fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

impl fmt::Display for SessionSeq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Why a turn ended.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TurnEndReason {
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
    /// The model hit its token ceiling.
    MaxTokens,
    /// The turn hit its step budget.
    MaxSteps,
    /// The user interrupted the turn.
    Interrupted,
}

impl TurnEndReason {
    /// Returns a short label for a transcript or a status line.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Aborted { .. } => "aborted",
            Self::Blocked => "blocked",
            Self::Error { .. } => "error",
            Self::MaxTokens => "max_tokens",
            Self::MaxSteps => "max_steps",
            Self::Interrupted => "interrupted",
        }
    }

    /// Returns `true` only for [`TurnEndReason::Completed`].
    ///
    /// Every other reason is a turn that stopped for a reason the caller may
    /// need to surface, so the predicate is deliberately narrow.
    #[must_use]
    pub const fn is_success(&self) -> bool {
        matches!(self, Self::Completed)
    }
}

impl fmt::Display for TurnEndReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// One immutable fact about a session.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEvent {
    /// A turn began.
    TurnStart {
        /// The turn index, from zero.
        turn: u32,
    },
    /// A turn ended.
    TurnEnd {
        /// The turn index.
        turn: u32,
        /// Why it ended.
        reason: TurnEndReason,
    },
    /// A step began: one model request plus the tool calls it produced.
    StepStart {
        /// The turn the step belongs to.
        turn: u32,
        /// The step index within the turn, from zero.
        step: u32,
    },
    /// A step ended.
    StepEnd {
        /// The turn the step belongs to.
        turn: u32,
        /// The step index within the turn.
        step: u32,
    },
    /// A human turn.
    UserMessage {
        /// The text the human wrote.
        text: String,
    },
    /// A model turn.
    AssistantMessage {
        /// The model's visible text, when it produced any.
        #[serde(default)]
        text: Option<String>,
        /// The model's reasoning trace, which must be replayed when tools are
        /// present.
        #[serde(default)]
        reasoning: Option<String>,
        /// The tool calls the model asked for.
        #[serde(default)]
        tool_calls: Vec<ToolCall>,
        /// Token accounting, when the provider reported it.
        #[serde(default)]
        usage: Option<Usage>,
        /// Whether the turn was cut short mid-stream.
        #[serde(default)]
        interrupted: bool,
        /// The model id the request named, when it was recorded.
        ///
        /// Per message rather than only in the header, because a session can be resumed
        /// against a different model and the header would then describe the first request
        /// as though it described all of them. It sits beside `usage` for the same reason
        /// `usage` is here: the two together are what a cost is attributed from, and a
        /// reader should not have to join across events to pair them.
        #[serde(default)]
        model: Option<String>,
        /// The reasoning effort the request carried, when the adapter reported one.
        #[serde(default)]
        effort: Option<String>,
    },
    /// Audit record of one tool call the model asked for.
    ///
    /// This is not a model-visible message: the call already appears inside the
    /// assistant message it came from. It exists so the log alone answers "what
    /// was run, with what arguments" without re-deriving it from the fold.
    ToolCall {
        /// The provider-assigned call id.
        call_id: ToolCallId,
        /// The tool that was asked for.
        name: ToolName,
        /// The arguments as parsed.
        arguments: Value,
    },
    /// The result of one tool call, already rendered for the model.
    ToolResult {
        /// The call this answers.
        call_id: ToolCallId,
        /// The rendered result.
        content: String,
        /// Whether the tool failed.
        is_error: bool,
    },
}

/// An append-only sequence of [`SessionEvent`]s.
///
/// The sequence number of an event is its position. That is what keeps the log
/// contiguous by construction: there is no separate counter to fall out of step,
/// and [`assert_contiguous`](SessionLog::assert_contiguous) states the property
/// that the file writer relies on.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionLog {
    /// The events, in the order they happened.
    events: Vec<SessionEvent>,
}

impl SessionLog {
    /// Creates an empty log.
    #[must_use]
    pub const fn new() -> Self {
        Self { events: Vec::new() }
    }

    /// Appends an event.
    pub fn append(&mut self, event: SessionEvent) {
        let before = self.events.len();
        self.events.push(event);
        // Postcondition: an append adds exactly one event, which is what makes
        // the position-as-sequence-number scheme hold.
        assert!(
            self.events.len() == before.saturating_add(1),
            "an append adds exactly one event"
        );
    }

    /// Returns the events, in order.
    #[must_use]
    pub fn events(&self) -> &[SessionEvent] {
        &self.events
    }

    /// Returns the number of events.
    #[must_use]
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// Returns `true` when the log has no events.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Returns the sequence number the next appended event will carry.
    #[must_use]
    pub fn next_seq(&self) -> SessionSeq {
        SessionSeq::new(u64::try_from(self.events.len()).unwrap_or(u64::MAX))
    }

    /// Returns the sequence number of every event, in order.
    #[must_use]
    pub fn seqs(&self) -> Vec<SessionSeq> {
        (0..self.events.len())
            .map(|index| SessionSeq::new(u64::try_from(index).unwrap_or(u64::MAX)))
            .collect()
    }

    /// Asserts that the log's sequence numbers are exactly `0..len`.
    ///
    /// A hole in a session file makes the tail unrecoverable and an off-by-one
    /// silently drops an event, so this is checked rather than assumed.
    pub fn assert_contiguous(&self) {
        let seqs = self.seqs();
        for (position, seq) in seqs.iter().enumerate() {
            let expected = SessionSeq::new(u64::try_from(position).unwrap_or(u64::MAX));
            assert!(
                *seq == expected,
                "session sequence numbers are contiguous from zero"
            );
        }
        assert!(
            seqs.first().is_none_or(|first| first.value() == 0),
            "the first sequence number is zero"
        );
    }

    /// Folds the log into the message list a model would be shown.
    #[must_use]
    pub fn derive_messages(&self) -> Vec<Message> {
        // Which calls some result answers. Collected first because a call can only be judged
        // against results that follow it, and the fold below is a single pass.
        let answered: Vec<&ToolCallId> = self
            .events
            .iter()
            .filter_map(|event| match event {
                SessionEvent::ToolResult { call_id, .. } => Some(call_id),
                _ => None,
            })
            .collect();
        let mut messages: Vec<Message> = Vec::new();
        for event in &self.events {
            match event {
                SessionEvent::UserMessage { text } => {
                    messages.push(Message::user(text.clone()));
                }
                SessionEvent::AssistantMessage {
                    text,
                    reasoning,
                    tool_calls,
                    ..
                } => {
                    let has_text = text.as_ref().is_some_and(|value| !value.is_empty());
                    let has_reasoning = reasoning.as_ref().is_some_and(|value| !value.is_empty());
                    // A call that no result answers cannot travel: the provider refuses a request
                    // whose assistant message names a call with nothing answering it. A log can
                    // hold one — a step records its calls and runs them, so anything that stopped
                    // the process between the two left the call behind — and a resumed session
                    // would then send a request no provider accepts, failing every turn from a log
                    // it can never repair. So the unanswered calls are dropped.
                    //
                    // The *message* stays when it has something to say: its text, a call that
                    // survived, or the reasoning of a turn that had calls at all. That last clause
                    // is why the condition is not simply "text or calls": the provider wants a
                    // tool-using turn's reasoning back, and dropping the calls must not take the
                    // reasoning with them. A message with nothing but reasoning and no calls is
                    // still skipped, because it carries nothing a model reads.
                    let had_calls = !tool_calls.is_empty();
                    let calls: Vec<ToolCall> = tool_calls
                        .iter()
                        .filter(|call| answered.contains(&&call.id))
                        .cloned()
                        .collect();
                    if has_text || !calls.is_empty() || (had_calls && has_reasoning) {
                        messages.push(Message::assistant(text.clone(), reasoning.clone(), calls));
                    }
                }
                SessionEvent::ToolResult {
                    call_id,
                    content,
                    is_error,
                } => {
                    messages.push(Message::tool(call_id.clone(), content.clone(), *is_error));
                }
                SessionEvent::TurnStart { .. }
                | SessionEvent::TurnEnd { .. }
                | SessionEvent::StepStart { .. }
                | SessionEvent::StepEnd { .. }
                | SessionEvent::ToolCall { .. } => {}
            }
        }
        // Postcondition: the fold only removes events, never invents messages.
        assert!(messages.len() <= self.events.len());
        messages
    }

    /// Sums every usage record the log carries.
    #[must_use]
    pub fn usage_totals(&self) -> Usage {
        let mut total = Usage::default();
        for event in &self.events {
            if let SessionEvent::AssistantMessage {
                usage: Some(usage), ..
            } = event
            {
                total.accumulate(usage);
            }
        }
        total
    }

    /// Sums usage per model, in first-seen order.
    ///
    /// A session can be resumed against a different model, so "what did this cost" has an
    /// answer per model and not only overall. The key is the model the assistant message
    /// recorded, which is `None` for a message from before that was recorded — kept as its
    /// own bucket rather than folded into a named model, because a total attributed to the
    /// wrong model is worse than one attributed to none.
    #[must_use]
    pub fn usage_by_model(&self) -> Vec<(Option<String>, Usage)> {
        let mut totals: Vec<(Option<String>, Usage)> = Vec::new();
        for event in &self.events {
            let SessionEvent::AssistantMessage {
                usage: Some(usage),
                model,
                ..
            } = event
            else {
                continue;
            };
            match totals.iter_mut().find(|(seen, _)| seen == model) {
                Some((_, total)) => total.accumulate(usage),
                None => totals.push((model.clone(), *usage)),
            }
        }
        totals
    }

    /// Returns how many turns have started.
    #[must_use]
    pub fn turn_count(&self) -> u32 {
        self.count_matching(|event| matches!(event, SessionEvent::TurnStart { .. }))
    }

    /// Returns how many steps have started, across every turn.
    #[must_use]
    pub fn step_count(&self) -> u32 {
        self.count_matching(|event| matches!(event, SessionEvent::StepStart { .. }))
    }

    /// Returns how many model turns the log recorded.
    ///
    /// Not the same figure as [`SessionLog::step_count`]: a request that failed before it
    /// answered produces a step and no assistant message, and the difference between the
    /// two is exactly how many requests were not answered.
    #[must_use]
    pub fn request_count(&self) -> u32 {
        self.count_matching(|event| matches!(event, SessionEvent::AssistantMessage { .. }))
    }

    /// Counts events a predicate accepts, saturating rather than wrapping.
    fn count_matching(&self, accept: impl Fn(&SessionEvent) -> bool) -> u32 {
        let count = self.events.iter().filter(|event| accept(event)).count();
        u32::try_from(count).unwrap_or(u32::MAX)
    }

    /// Returns the index of the turn currently in progress.
    ///
    /// Zero when no turn has started, which is the same value the first turn
    /// carries: a harness that has not started a turn is at the beginning of
    /// turn zero rather than in a state with no answer.
    #[must_use]
    pub fn current_turn(&self) -> u32 {
        self.events
            .iter()
            .rev()
            .find_map(|event| match event {
                SessionEvent::TurnStart { turn } => Some(*turn),
                _ => None,
            })
            .unwrap_or(0)
    }

    /// Returns how many steps have started in `turn`.
    #[must_use]
    pub fn steps_in_turn(&self, turn: u32) -> u32 {
        let steps = self
            .events
            .iter()
            .filter(|event| {
                matches!(event, SessionEvent::StepStart { turn: started, .. } if *started == turn)
            })
            .count();
        u32::try_from(steps).unwrap_or(u32::MAX)
    }

    /// Returns the tool calls that have no result yet, in the order they were
    /// requested.
    ///
    /// This is the harness's "what is still owed" query. A turn may not close
    /// while it is non-empty.
    #[must_use]
    pub fn open_tool_calls(&self) -> Vec<ToolCallId> {
        let mut open: Vec<ToolCallId> = Vec::new();
        for event in &self.events {
            if let SessionEvent::ToolCall { call_id, .. } = event {
                if open.contains(call_id) {
                    continue;
                }
                open.push(call_id.clone());
            } else if let SessionEvent::ToolResult { call_id, .. } = event {
                open.retain(|id| id != call_id);
            }
        }
        open
    }

    /// Returns the most recent turn-end reason, if any turn has ended.
    #[must_use]
    pub fn last_turn_end(&self) -> Option<&TurnEndReason> {
        self.events.iter().rev().find_map(|event| match event {
            SessionEvent::TurnEnd { reason, .. } => Some(reason),
            _ => None,
        })
    }
}

/// The header line of a session file.
///
/// Field order is the order they are written in, which is why the identity comes first:
/// a reader scanning a header by eye sees *which* session before *what* ran it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct SessionHeader {
    /// The format tag, which distinguishes a session file from any other JSONL.
    format: String,
    /// The format version, which decides how the body is read.
    version: u32,
    /// The session's store key.
    id: String,
    /// Creation time, in milliseconds since the Unix epoch.
    created_at_ms: u64,
    /// The working directory the session ran in.
    cwd: String,
    /// The harness configuration, absent for a session recorded before there was one.
    ///
    /// Defaulted rather than required, so a header written by an older build still reads:
    /// adding a field to the header is not a change to how the *body* is read, which is
    /// what `version` guards, so an old log is not a version this build cannot accept.
    #[serde(default)]
    origin: Option<Origin>,
}

/// One event line, borrowing its event.
#[derive(Serialize)]
struct SessionLineRef<'a> {
    /// The sequence number.
    seq: SessionSeq,
    /// The event.
    event: &'a SessionEvent,
}

/// One event line, owning its event.
#[derive(Deserialize)]
struct SessionLine {
    /// The sequence number.
    seq: SessionSeq,
    /// The event.
    event: SessionEvent,
}

/// Why a session could not be read or written.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SessionError {
    /// The input had no header line at all.
    #[error("session input has no header line")]
    MissingHeader,

    /// The header line did not parse, or did not describe a session.
    #[error("session header on line {line} is invalid: {reason}")]
    BadHeader {
        /// The 1-based line the header was read from.
        line: u64,
        /// What is wrong with it.
        reason: String,
    },

    /// The header names a format version this crate cannot read.
    #[error("session format version {found} is not supported (expected {expected})")]
    UnsupportedVersion {
        /// The version found in the file.
        found: u32,
        /// The version this crate reads and writes.
        expected: u32,
    },

    /// An event line did not parse, which a truncated tail also produces.
    #[error("session event on line {line} is malformed: {detail}")]
    MalformedEvent {
        /// The 1-based line the event was read from.
        line: u64,
        /// The underlying parse failure, rendered.
        detail: String,
    },

    /// The event lines are not numbered contiguously from zero.
    #[error("session event on line {line} has sequence {found}, expected {expected}")]
    NonContiguousSequence {
        /// The 1-based line the event was read from.
        line: u64,
        /// The sequence number that was expected.
        expected: u64,
        /// The sequence number that was found.
        found: u64,
    },
}

/// The harness configuration a session was run under.
///
/// A transcript on its own does not say what produced it, and the readings a person
/// compares runs by are exactly the ones a configuration changes: the same task under
/// `deepseek-flash` at `minimal` and under `deepseek-v4-pro` at `high` produces two
/// transcripts that look alike and cost different amounts. Recording the configuration
/// is what makes a session comparable with another, and what lets a later reader tell
/// which of the two they are looking at.
///
/// Every field is optional, and deliberately so. A session recorded before this existed
/// has none of them, and "absent" means *not recorded* rather than a default: a default
/// here would be a fabricated fact about a run nobody can now verify, which is worse than
/// an admitted gap. Fields are added to this struct as they are needed, and the session
/// format version does not move for it — a version decides how the *body* is read, and an
/// older build ignores a header field it does not know.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Origin {
    /// The model id the agent's requests named.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The reasoning effort the requests carried, when the adapter reports one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// The sandbox mode in force, by its config name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<String>,
    /// The approval policy in force, by its config name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<String>,
    /// The release that recorded the session, as `nanus/<version>`.
    ///
    /// A release rather than a build: nothing at runtime knows which commit produced the
    /// binary, so claiming a commit here would be a guess dressed as a fact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
}

impl Origin {
    /// Returns `true` when nothing at all is known about the run.
    ///
    /// A session whose origin is empty is one recorded before any of this existed, so the
    /// accessors that read a field can say "not recorded" without inventing a value.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

/// A session: its identity, its environment, and its append-only log.
#[derive(Clone, Debug, PartialEq)]
pub struct Session {
    /// The store key.
    id: SessionId,
    /// Creation time, in milliseconds since the Unix epoch.
    created_at_ms: u64,
    /// The working directory the session ran in.
    cwd: String,
    /// The harness configuration it was created under, when one was known.
    origin: Option<Origin>,
    /// The event log.
    log: SessionLog,
}

impl Session {
    /// Creates an empty session.
    #[must_use]
    pub fn new(id: SessionId, created_at_ms: u64, cwd: impl Into<String>) -> Self {
        Self {
            id,
            created_at_ms,
            cwd: cwd.into(),
            origin: None,
            log: SessionLog::new(),
        }
    }

    /// Records the harness configuration this session is being run under.
    ///
    /// A builder rather than a fourth parameter of [`Session::new`], because most callers
    /// have nothing to say: a test that is about the shape of a log does not care which
    /// model would have produced it, and a required argument would make every one of them
    /// pass `None`. The caller that *does* know — the composition, which holds the whole
    /// configuration — opts in.
    #[must_use]
    pub fn with_origin(mut self, origin: Origin) -> Self {
        // Nothing known is the same fact as nothing recorded, and storing an empty origin
        // would write a header line full of absences for a session whose reader then has
        // two spellings of the same gap to handle.
        self.origin = if origin.is_empty() {
            None
        } else {
            Some(origin)
        };
        self
    }

    /// Returns the harness configuration, when one was recorded.
    #[must_use]
    pub const fn origin(&self) -> Option<&Origin> {
        self.origin.as_ref()
    }

    /// Returns the session's store key.
    #[must_use]
    pub const fn id(&self) -> &SessionId {
        &self.id
    }

    /// Returns the creation time in milliseconds since the Unix epoch.
    #[must_use]
    pub const fn created_at_ms(&self) -> u64 {
        self.created_at_ms
    }

    /// Returns the creation time as an RFC 3339 string in UTC.
    ///
    /// Returns `None` when the stored millisecond value is outside the range a
    /// calendar timestamp can represent; a session with an unrepresentable
    /// start time is not an error, it is simply undated.
    #[must_use]
    pub fn created_at_rfc3339(&self) -> Option<String> {
        let nanos = i128::from(self.created_at_ms).checked_mul(1_000_000)?;
        let datetime = time::OffsetDateTime::from_unix_timestamp_nanos(nanos).ok()?;
        datetime
            .format(&time::format_description::well_known::Rfc3339)
            .ok()
    }

    /// Returns the working directory the session ran in.
    #[must_use]
    pub fn cwd(&self) -> &str {
        &self.cwd
    }

    /// Returns the event log.
    #[must_use]
    pub const fn log(&self) -> &SessionLog {
        &self.log
    }

    /// Returns the event log for in-place appends.
    ///
    /// The sequence stays contiguous because the log numbers events by position;
    /// a caller cannot introduce a hole through this handle.
    pub const fn log_mut(&mut self) -> &mut SessionLog {
        &mut self.log
    }

    /// Appends an event, keeping the sequence contiguous.
    pub fn append(&mut self, event: SessionEvent) {
        let before = self.log.next_seq();
        self.log.append(event);
        // Postcondition: the log grew by exactly one sequence number.
        assert!(
            self.log.next_seq() == before.next(),
            "the session sequence stays contiguous"
        );
    }

    /// Returns the number of events.
    #[must_use]
    pub fn event_count(&self) -> usize {
        self.log.len()
    }

    /// Folds the log into the message list a model would be shown.
    #[must_use]
    pub fn derive_messages(&self) -> Vec<Message> {
        self.log.derive_messages()
    }

    /// Sums every usage record the session carries.
    #[must_use]
    pub fn usage_totals(&self) -> Usage {
        self.log.usage_totals()
    }

    /// Sums usage per model, in first-seen order.
    #[must_use]
    pub fn usage_by_model(&self) -> Vec<(Option<String>, Usage)> {
        self.log.usage_by_model()
    }

    /// Returns how many turns the session has started.
    #[must_use]
    pub fn turn_count(&self) -> u32 {
        self.log.turn_count()
    }

    /// Returns how many steps the session has started, across every turn.
    #[must_use]
    pub fn step_count(&self) -> u32 {
        self.log.step_count()
    }

    /// Returns how many model turns the session recorded.
    #[must_use]
    pub fn request_count(&self) -> u32 {
        self.log.request_count()
    }

    /// Derives a short title from the first human turn.
    ///
    /// Returns `None` until the session has a user message, so a listing never
    /// shows a fabricated title.
    #[must_use]
    pub fn title(&self) -> Option<String> {
        let first = self.log.events().iter().find_map(|event| match event {
            SessionEvent::UserMessage { text } => Some(text.as_str()),
            _ => None,
        })?;
        let line = first.lines().next().unwrap_or_default().trim();
        if line.is_empty() {
            return None;
        }
        let mut title: String = line.chars().take(TITLE_MAX_CHARS).collect();
        if line.chars().count() > TITLE_MAX_CHARS {
            title.push('…');
        }
        Some(title)
    }

    /// Encodes the session as JSONL: a header line, then one envelope per event.
    ///
    /// The function is infallible by construction. Every type this crate stores
    /// in a session is JSON-representable — there are no maps with non-string
    /// keys and no floating-point values — so the only way an encode could fail
    /// is a bug in the encoder, which the assertion below would surface rather
    /// than write a corrupt file.
    #[must_use]
    pub fn to_jsonl(&self) -> String {
        let header = SessionHeader {
            format: SESSION_FORMAT_TAG.to_owned(),
            version: SESSION_FORMAT_VERSION,
            id: self.id.as_str().to_owned(),
            created_at_ms: self.created_at_ms,
            cwd: self.cwd.clone(),
            origin: self.origin.clone(),
        };
        let mut out = encode(&header);
        assert!(!out.is_empty(), "the session header encodes");
        out.push('\n');
        for (index, event) in self.log.events().iter().enumerate() {
            let seq = SessionSeq::new(u64::try_from(index).unwrap_or(u64::MAX));
            let line = encode(&SessionLineRef { seq, event });
            assert!(!line.is_empty(), "a session event encodes");
            out.push_str(&line);
            out.push('\n');
        }
        out
    }

    /// Decodes a session from JSONL.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::MissingHeader`] for input with no header,
    /// [`SessionError::BadHeader`] for a header that does not describe a
    /// session, [`SessionError::UnsupportedVersion`] for a version this crate
    /// cannot read, [`SessionError::MalformedEvent`] for a line that does not
    /// parse — which is what a truncated write produces — and
    /// [`SessionError::NonContiguousSequence`] for a hole in the numbering.
    /// Blank lines are ignored so a trailing newline is not an error.
    pub fn from_jsonl(raw: &str) -> Result<Self, SessionError> {
        let mut number: u64 = 0;
        let mut header: Option<SessionHeader> = None;
        let mut log = SessionLog::new();
        let mut expected: u64 = 0;
        for line in raw.lines() {
            number = number.saturating_add(1);
            if line.trim().is_empty() {
                continue;
            }
            if header.is_none() {
                header = Some(parse_header(line, number)?);
                continue;
            }
            let parsed: SessionLine =
                serde_json::from_str(line).map_err(|error| SessionError::MalformedEvent {
                    line: number,
                    detail: error.to_string(),
                })?;
            if parsed.seq.value() != expected {
                return Err(SessionError::NonContiguousSequence {
                    line: number,
                    expected,
                    found: parsed.seq.value(),
                });
            }
            log.append(parsed.event);
            expected = expected.saturating_add(1);
        }
        let header = header.ok_or(SessionError::MissingHeader)?;
        log.assert_contiguous();
        let session = Self {
            id: SessionId::new(header.id),
            created_at_ms: header.created_at_ms,
            cwd: header.cwd,
            origin: header.origin,
            log,
        };
        // Postcondition: the decoded log has exactly the events the file numbered.
        assert_eq!(
            session.event_count(),
            usize::try_from(expected).unwrap_or(usize::MAX)
        );
        Ok(session)
    }
}

/// Parses and checks a session header line.
fn parse_header(line: &str, number: u64) -> Result<SessionHeader, SessionError> {
    let header: SessionHeader =
        serde_json::from_str(line).map_err(|error| SessionError::BadHeader {
            line: number,
            reason: error.to_string(),
        })?;
    if header.format != SESSION_FORMAT_TAG {
        return Err(SessionError::BadHeader {
            line: number,
            reason: format!(
                "expected format {SESSION_FORMAT_TAG:?}, found {:?}",
                header.format
            ),
        });
    }
    if header.version != SESSION_FORMAT_VERSION {
        return Err(SessionError::UnsupportedVersion {
            found: header.version,
            expected: SESSION_FORMAT_VERSION,
        });
    }
    if header.id.is_empty() {
        // An empty id would become an unnamed file in the store, so it is a
        // header error rather than a session with a blank key.
        return Err(SessionError::BadHeader {
            line: number,
            reason: String::from("the session id is empty"),
        });
    }
    Ok(header)
}

/// Encodes a value as compact JSON.
///
/// `unwrap_or_default` is total, so the encoder cannot panic; the callers assert
/// that the result is non-empty, which is the only realistic failure mode.
fn encode<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn session() -> Session {
        Session::new(SessionId::new("s-1"), 1_700_000_000_000, "/work")
    }

    fn tool_name(raw: &str) -> ToolName {
        ToolName::new(raw).unwrap_or_else(|error| panic!("test tool name {raw}: {error}"))
    }

    fn record_read_turn(session: &mut Session) {
        session.append(SessionEvent::TurnStart { turn: 0 });
        session.append(SessionEvent::StepStart { turn: 0, step: 0 });
        session.append(SessionEvent::UserMessage {
            text: "read the file".to_owned(),
        });
        session.append(SessionEvent::AssistantMessage {
            text: None,
            reasoning: Some("I should read it".to_owned()),
            tool_calls: vec![ToolCall::new(
                ToolCallId::new("c-1"),
                tool_name("read"),
                json!({ "path": "src/lib.rs" }),
            )],
            usage: Some(Usage::new(10, 5, 2, 0, 10)),
            interrupted: false,
            model: None,
            effort: None,
        });
        session.append(SessionEvent::ToolCall {
            call_id: ToolCallId::new("c-1"),
            name: tool_name("read"),
            arguments: json!({ "path": "src/lib.rs" }),
        });
        session.append(SessionEvent::ToolResult {
            call_id: ToolCallId::new("c-1"),
            content: "fn main() {}".to_owned(),
            is_error: false,
        });
        session.append(SessionEvent::StepEnd { turn: 0, step: 0 });
        session.append(SessionEvent::TurnEnd {
            turn: 0,
            reason: TurnEndReason::Completed,
        });
    }

    /// Builds a session header line from its parts.
    fn header_line(format: &str, version: u32, id: &str) -> String {
        serde_json::json!({
            "format": format,
            "version": version,
            "id": id,
            "created_at_ms": 0,
            "cwd": "/w",
        })
        .to_string()
    }

    /// Builds a valid session header line.
    fn header() -> String {
        header_line(SESSION_FORMAT_TAG, SESSION_FORMAT_VERSION, "s")
    }

    /// Builds a session file from a header and one envelope per `(seq, event)`.
    fn framed(lines: &[(u64, Value)]) -> String {
        let mut out = header();
        out.push('\n');
        for (seq, event) in lines {
            let envelope = serde_json::json!({ "seq": seq, "event": event });
            out.push_str(&envelope.to_string());
            out.push('\n');
        }
        out
    }

    #[test]
    fn an_append_only_log_numbers_events_by_position() {
        let mut log = SessionLog::new();
        assert!(log.is_empty());
        assert_eq!(log.next_seq().value(), 0);
        log.append(SessionEvent::TurnStart { turn: 0 });
        log.append(SessionEvent::TurnStart { turn: 1 });
        log.assert_contiguous();
        assert_eq!(log.len(), 2);
        assert_eq!(log.next_seq().value(), 2);
        assert_eq!(
            log.seqs()
                .iter()
                .map(|seq| seq.value())
                .collect::<Vec<u64>>(),
            vec![0, 1]
        );
    }

    #[test]
    fn the_surface_fold_projects_only_model_visible_events() {
        let mut session = session();
        record_read_turn(&mut session);
        let messages = session.derive_messages();
        // user, assistant (tool call), tool result: boundaries and the audit
        // record never reach a model.
        assert_eq!(messages.len(), 3);
        assert_eq!(messages.first().map(Message::role), Some(crate::Role::User));
        assert_eq!(
            messages.get(1).map(Message::role),
            Some(crate::Role::Assistant)
        );
        assert_eq!(messages.get(2).map(Message::role), Some(crate::Role::Tool));
    }

    #[test]
    fn an_assistant_turn_with_nothing_to_say_is_skipped() {
        let mut session = session();
        session.append(SessionEvent::AssistantMessage {
            text: Some(String::new()),
            reasoning: Some("thinking".to_owned()),
            tool_calls: Vec::new(),
            usage: None,
            interrupted: false,
            model: None,
            effort: None,
        });
        session.append(SessionEvent::AssistantMessage {
            text: None,
            reasoning: None,
            tool_calls: Vec::new(),
            usage: None,
            interrupted: true,
            model: None,
            effort: None,
        });
        assert!(
            session.derive_messages().is_empty(),
            "an empty assistant turn carries no model-visible content"
        );
    }

    #[test]
    fn a_tool_result_keeps_its_call_id_and_error_flag() {
        let mut session = session();
        session.append(SessionEvent::ToolResult {
            call_id: ToolCallId::new("c-7"),
            content: "no such file".to_owned(),
            is_error: true,
        });
        let messages = session.derive_messages();
        let Some(Message::Tool {
            call_id,
            content,
            is_error,
        }) = messages.first()
        else {
            panic!("the fold produces one tool message");
        };
        assert_eq!(call_id.as_str(), "c-7");
        assert_eq!(content, "no such file");
        assert!(*is_error);
    }

    /// A call nothing answered is dropped from the replay, and what the turn said is kept.
    ///
    /// The provider refuses an assistant message whose calls have no results, so a log holding
    /// one — the calls are recorded before they run, so anything that stopped the process
    /// between the two left it behind — would make every request of the resumed session invalid.
    /// Dropping the call is the fix; dropping the *message* would take the turn's reasoning with
    /// it, which the provider wants back.
    #[test]
    fn a_tool_call_with_no_result_is_not_replayed() {
        let call = |id: &str| {
            ToolCall::new(
                ToolCallId::new(id),
                ToolName::new("read").unwrap_or_else(|_| unreachable!("valid")),
                serde_json::json!({}),
            )
        };
        let unanswered = call("c1");
        let answered = call("c2");
        let mut log = SessionLog::new();
        log.append(SessionEvent::UserMessage {
            text: String::from("hi"),
        });
        log.append(SessionEvent::AssistantMessage {
            text: Some(String::from("looking")),
            reasoning: Some(String::from("I should read it")),
            tool_calls: vec![unanswered, answered.clone()],
            usage: None,
            interrupted: false,
            model: None,
            effort: None,
        });
        log.append(SessionEvent::ToolResult {
            call_id: answered.id.clone(),
            content: String::from("done"),
            is_error: false,
        });

        let messages = log.derive_messages();
        let assistant = messages
            .iter()
            .find(|message| message.text() == Some("looking"));
        assert!(
            assistant.is_some_and(|message| {
                message.tool_calls().len() == 1 && message.tool_calls()[0].id == answered.id
            }),
            "only the answered call is replayed: {messages:?}"
        );
        assert!(
            assistant.is_some_and(|message| message.reasoning() == Some("I should read it")),
            "and the reasoning of the tool-using turn survives the dropped call"
        );
    }

    /// The other direction, and the reason the rule is not "keep anything that had a call": a
    /// turn that only thought, with no calls at all, still carries nothing a model reads.
    #[test]
    fn an_assistant_turn_that_only_thought_is_still_skipped() {
        let mut log = SessionLog::new();
        log.append(SessionEvent::AssistantMessage {
            text: None,
            reasoning: Some(String::from("thinking")),
            tool_calls: Vec::new(),
            usage: None,
            interrupted: false,
            model: None,
            effort: None,
        });
        assert!(
            log.derive_messages().is_empty(),
            "reasoning without a call is not model-visible on its own"
        );
    }

    #[test]
    fn usage_totals_sum_every_assistant_record() {
        let mut session = session();
        session.append(SessionEvent::AssistantMessage {
            text: Some("a".to_owned()),
            reasoning: None,
            tool_calls: Vec::new(),
            usage: Some(Usage::new(10, 5, 0, 0, 10)),
            interrupted: false,
            model: None,
            effort: None,
        });
        session.append(SessionEvent::AssistantMessage {
            text: Some("b".to_owned()),
            reasoning: None,
            tool_calls: Vec::new(),
            usage: Some(Usage::new(20, 7, 3, 0, 20)),
            interrupted: false,
            model: None,
            effort: None,
        });
        let total = session.usage_totals();
        assert_eq!(total.prompt_tokens, 30);
        assert_eq!(total.completion_tokens, 12);
        assert_eq!(total.total_tokens(), 42);
    }

    #[test]
    fn open_tool_calls_track_what_is_owed() {
        let mut session = session();
        session.append(SessionEvent::ToolCall {
            call_id: ToolCallId::new("c-1"),
            name: tool_name("read"),
            arguments: json!({}),
        });
        session.append(SessionEvent::ToolCall {
            call_id: ToolCallId::new("c-2"),
            name: tool_name("write"),
            arguments: json!({}),
        });
        assert_eq!(session.log().open_tool_calls().len(), 2);
        session.append(SessionEvent::ToolResult {
            call_id: ToolCallId::new("c-1"),
            content: "ok".to_owned(),
            is_error: false,
        });
        let open = session.log().open_tool_calls();
        assert_eq!(open.len(), 1);
        assert_eq!(open.first().map(ToolCallId::as_str), Some("c-2"));
    }

    #[test]
    fn the_current_turn_and_its_step_count_are_derived_from_the_log() {
        let mut session = session();
        assert_eq!(session.log().current_turn(), 0, "no turn started yet");
        session.append(SessionEvent::TurnStart { turn: 0 });
        session.append(SessionEvent::StepStart { turn: 0, step: 0 });
        session.append(SessionEvent::StepStart { turn: 0, step: 1 });
        session.append(SessionEvent::TurnEnd {
            turn: 0,
            reason: TurnEndReason::Completed,
        });
        session.append(SessionEvent::TurnStart { turn: 1 });
        session.append(SessionEvent::StepStart { turn: 1, step: 0 });
        assert_eq!(session.log().current_turn(), 1);
        assert_eq!(session.log().steps_in_turn(0), 2);
        assert_eq!(session.log().steps_in_turn(1), 1);
        assert_eq!(
            session.log().last_turn_end(),
            Some(&TurnEndReason::Completed)
        );
    }

    #[test]
    fn turn_end_reasons_are_narrow_about_success() {
        assert!(TurnEndReason::Completed.is_success());
        assert!(!TurnEndReason::Blocked.is_success());
        assert!(!TurnEndReason::MaxTokens.is_success());
        assert!(!TurnEndReason::MaxSteps.is_success());
        assert!(!TurnEndReason::Interrupted.is_success());
        assert!(
            !TurnEndReason::Aborted {
                reason: "human".to_owned()
            }
            .is_success()
        );
        assert!(
            !TurnEndReason::Error {
                message: "boom".to_owned()
            }
            .is_success()
        );
        assert_eq!(
            TurnEndReason::MaxTokens.label(),
            "max_tokens",
            "labels are stable identifiers"
        );
    }

    #[test]
    fn a_session_round_trips_through_jsonl() {
        let mut original = session();
        record_read_turn(&mut original);
        original.append(SessionEvent::UserMessage {
            text: "multi\nline \"quoted\" text".to_owned(),
        });
        let encoded = original.to_jsonl();
        let decoded = Session::from_jsonl(&encoded);
        assert!(decoded.is_ok(), "the session round-trips");
        assert_eq!(decoded.ok(), Some(original));
    }

    #[test]
    fn a_session_remembers_what_it_ran_under() {
        // The header carries the configuration, so a transcript read back months later can
        // still say which model produced it.
        let original = session().with_origin(Origin {
            model: Some("deepseek-flash".to_owned()),
            effort: Some("high".to_owned()),
            sandbox: Some("read_only".to_owned()),
            approval: Some("per_call".to_owned()),
            harness: Some("nanus/0.1.0".to_owned()),
        });
        let decoded = Session::from_jsonl(&original.to_jsonl());
        assert_eq!(decoded.ok(), Some(original.clone()));
        let origin = original.origin().expect("the origin survives");
        assert_eq!(origin.model.as_deref(), Some("deepseek-flash"));
        assert_eq!(origin.effort.as_deref(), Some("high"));
    }

    #[test]
    fn a_header_with_no_origin_still_reads() {
        // The compatibility rule, stated as a test rather than as a comment: a session
        // recorded before the header had a configuration in it is not a version this build
        // cannot read, because the field is an addition to the *header* and `version` is
        // what decides how the *body* is read.
        let raw = framed(&[]);
        assert!(
            !raw.contains("origin"),
            "the fixture is an old-shaped header: {raw}"
        );
        let decoded = Session::from_jsonl(&raw);
        assert!(decoded.is_ok(), "an old header still parses: {decoded:?}");
        let Ok(decoded) = decoded else { return };
        assert_eq!(
            decoded.origin(),
            None,
            "nothing recorded is reported as nothing, not as a default"
        );
        assert_eq!(
            SESSION_FORMAT_VERSION, 1,
            "adding a header field is not a body change, so the version stays where it was"
        );
    }

    #[test]
    fn a_session_that_knows_nothing_records_nothing() {
        // An empty origin and no origin are the same fact, and the header must not grow a
        // row of absences for it.
        let bare = session();
        let annotated = session().with_origin(Origin::default());
        assert_eq!(annotated.origin(), None);
        assert_eq!(bare.to_jsonl(), annotated.to_jsonl());
    }

    #[test]
    fn usage_is_split_by_the_model_that_produced_it() {
        // A session resumed against a different model has a cost per model, and the bucket
        // for a message that recorded no model stays its own rather than being attributed
        // to a named one.
        let mut session = session();
        for (model, usage) in [
            (Some("deepseek-flash"), Usage::new(100, 10, 0, 60, 40)),
            (Some("deepseek-v4-pro"), Usage::new(50, 5, 0, 25, 25)),
            (Some("deepseek-flash"), Usage::new(10, 1, 0, 10, 0)),
            (None, Usage::new(7, 1, 0, 0, 7)),
        ] {
            session.append(SessionEvent::AssistantMessage {
                text: Some(String::from("a")),
                reasoning: None,
                tool_calls: Vec::new(),
                usage: Some(usage),
                interrupted: false,
                model: model.map(str::to_owned),
                effort: None,
            });
        }
        let by_model = session.usage_by_model();
        let model_of = |index: usize| by_model.get(index).map(|(model, _)| model.clone());
        assert_eq!(model_of(0), Some(Some(String::from("deepseek-flash"))));
        assert_eq!(model_of(1), Some(Some(String::from("deepseek-v4-pro"))));
        assert_eq!(model_of(2), Some(None), "the unrecorded bucket comes last");
        assert_eq!(by_model.len(), 3, "flash is one bucket, not two");
        assert_eq!(
            by_model.first().map(|(_, usage)| usage.prompt_tokens),
            Some(110)
        );
        assert_eq!(session.usage_totals().prompt_tokens, 167);
    }

    #[test]
    fn the_counts_report_what_the_log_recorded() {
        let mut session = session();
        assert_eq!(session.turn_count(), 0);
        assert_eq!(session.step_count(), 0);
        assert_eq!(session.request_count(), 0);
        record_read_turn(&mut session);
        assert_eq!(session.turn_count(), 1);
        assert_eq!(session.step_count(), 1);
        assert_eq!(session.request_count(), 1);
    }

    #[test]
    fn jsonl_is_a_header_then_one_line_per_event() {
        let mut original = session();
        record_read_turn(&mut original);
        let encoded = original.to_jsonl();
        let lines: Vec<&str> = encoded.lines().collect();
        assert_eq!(lines.len(), original.event_count().saturating_add(1));
        assert!(
            lines
                .first()
                .is_some_and(|line| line.contains("nanus.session"))
        );
        assert!(lines.get(1).is_some_and(|line| line.contains("\"seq\":0")));
    }

    #[test]
    fn an_empty_session_round_trips_with_only_a_header() {
        let original = session();
        let decoded = Session::from_jsonl(&original.to_jsonl());
        assert!(decoded.is_ok());
        let Ok(decoded) = decoded else { return };
        assert!(decoded.log().is_empty());
        assert_eq!(decoded.id().as_str(), "s-1");
        assert_eq!(decoded.cwd(), "/work");
    }

    #[test]
    fn an_empty_input_has_no_header() {
        assert_eq!(Session::from_jsonl(""), Err(SessionError::MissingHeader));
        assert_eq!(
            Session::from_jsonl("\n  \n"),
            Err(SessionError::MissingHeader)
        );
    }

    #[test]
    fn a_foreign_format_tag_is_rejected() {
        let raw = header_line("other.tool", SESSION_FORMAT_VERSION, "s");
        let decoded = Session::from_jsonl(&raw);
        assert!(matches!(decoded, Err(SessionError::BadHeader { .. })));
    }

    #[test]
    fn a_bad_version_is_rejected_as_its_own_error() {
        let raw = header_line(SESSION_FORMAT_TAG, 99, "s");
        let decoded = Session::from_jsonl(&raw);
        assert_eq!(
            decoded,
            Err(SessionError::UnsupportedVersion {
                found: 99,
                expected: 1
            })
        );
    }

    #[test]
    fn an_empty_session_id_is_a_header_error_not_a_panic() {
        let raw = header_line(SESSION_FORMAT_TAG, SESSION_FORMAT_VERSION, "");
        let decoded = Session::from_jsonl(&raw);
        assert!(matches!(decoded, Err(SessionError::BadHeader { .. })));
    }

    #[test]
    fn a_truncated_tail_is_a_typed_error() {
        let mut original = session();
        record_read_turn(&mut original);
        let encoded = original.to_jsonl();
        // Cut the last event line in half, which is what a crash mid-write does.
        let truncated = encoded
            .get(..encoded.len().saturating_sub(24))
            .unwrap_or_default();
        let decoded = Session::from_jsonl(truncated);
        assert!(
            matches!(decoded, Err(SessionError::MalformedEvent { .. })),
            "a half-written line is malformed, got {decoded:?}"
        );
        // Pair assertion: the untruncated text still decodes, so the failure is
        // the truncation and not the fixture.
        assert!(Session::from_jsonl(&encoded).is_ok());
    }

    #[test]
    fn a_hole_in_the_sequence_is_rejected() {
        let raw = framed(&[
            (0, json!({ "type": "turn_start", "turn": 0 })),
            (
                2,
                json!({
                    "type": "turn_end",
                    "turn": 0,
                    "reason": { "kind": "completed" },
                }),
            ),
        ]);
        let decoded = Session::from_jsonl(&raw);
        assert_eq!(
            decoded,
            Err(SessionError::NonContiguousSequence {
                line: 3,
                expected: 1,
                found: 2
            })
        );
    }

    #[test]
    fn a_repeated_sequence_is_rejected() {
        let raw = framed(&[
            (0, json!({ "type": "turn_start", "turn": 0 })),
            (0, json!({ "type": "turn_start", "turn": 1 })),
        ]);
        let decoded = Session::from_jsonl(&raw);
        assert!(matches!(
            decoded,
            Err(SessionError::NonContiguousSequence { found: 0, .. })
        ));
    }

    #[test]
    fn a_blank_line_in_the_body_is_ignored() {
        // A leading blank line, a trailing newline, and a blank line inside the
        // body are all harmless.
        let body = framed(&[(0, json!({ "type": "turn_start", "turn": 0 }))]);
        let raw = format!("\n{body}\n");
        let decoded = Session::from_jsonl(&raw);
        assert!(decoded.is_ok(), "a blank line is not a corruption");
        let Ok(decoded) = decoded else { return };
        assert_eq!(decoded.event_count(), 1);
    }

    #[test]
    fn a_derived_title_comes_from_the_first_human_turn() {
        let mut session = session();
        assert_eq!(session.title(), None, "no human turn yet");
        session.append(SessionEvent::UserMessage {
            text: "first line\nsecond line".to_owned(),
        });
        assert_eq!(session.title().as_deref(), Some("first line"));
    }

    #[test]
    fn a_long_title_is_truncated_on_a_character_boundary() {
        let mut session = session();
        session.append(SessionEvent::UserMessage {
            text: "é".repeat(200),
        });
        let title = session.title();
        assert!(title.is_some());
        let Some(title) = title else { return };
        assert!(title.ends_with('…'));
        assert_eq!(title.chars().count(), TITLE_MAX_CHARS.saturating_add(1));
    }

    #[test]
    fn a_blank_first_turn_produces_no_title() {
        let mut session = session();
        session.append(SessionEvent::UserMessage {
            text: "   \n  ".to_owned(),
        });
        assert_eq!(session.title(), None);
    }

    #[test]
    fn the_creation_time_renders_as_rfc3339() {
        let session = session();
        let rendered = session.created_at_rfc3339();
        assert!(rendered.is_some());
        let Some(rendered) = rendered else { return };
        assert!(
            rendered.starts_with("2023-11-14"),
            "the epoch milliseconds decode: {rendered}"
        );
    }

    #[test]
    fn session_ids_and_sequences_display() {
        assert_eq!(SessionId::new("s-1").to_string(), "s-1");
        assert_eq!(SessionSeq::new(4).to_string(), "4");
        assert_eq!(SessionSeq::new(4).next().value(), 5);
        assert_eq!(SessionSeq::new(u64::MAX).next().value(), u64::MAX);
        assert!(SessionId::new("").is_empty());
    }

    #[test]
    fn appending_through_the_log_handle_keeps_the_sequence_contiguous() {
        let mut session = session();
        session
            .log_mut()
            .append(SessionEvent::TurnStart { turn: 0 });
        session.log_mut().append(SessionEvent::TurnEnd {
            turn: 0,
            reason: TurnEndReason::Blocked,
        });
        session.log().assert_contiguous();
        assert_eq!(session.event_count(), 2);
    }

    #[test]
    fn a_malformed_event_body_is_reported_with_its_line() {
        let raw = framed(&[(0, json!({ "type": "no_such_event" }))]);
        let decoded = Session::from_jsonl(&raw);
        assert!(
            matches!(decoded, Err(SessionError::MalformedEvent { line: 2, .. })),
            "an unknown event type is malformed at line 2, got {decoded:?}"
        );
    }
}
