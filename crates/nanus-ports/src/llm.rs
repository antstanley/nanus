//! The language-model port and the streaming vocabulary it speaks.
//!
//! ### Why the futures are boxed by hand
//!
//! `async fn` in a trait is stable, but an `async fn` trait is not
//! *dyn-compatible*, and these ports must be held as trait objects so the
//! kernel's service registry can publish them by key. Every method here is
//! therefore an ordinary function returning a [`crate::LocalBoxFuture`] — the
//! desugaring `async fn` would perform, written out so `dyn LlmPort` is legal.
//!
//! The futures are `!Send` on purpose. The kernel is single-threaded, like
//! Cordis, and requiring `Send` would force every adapter to prove something the
//! runtime never needs.
//!
//! ### The request is the port's only job
//!
//! [`LlmPort::stream_chat`] is not fallible. Every failure — a missing key, an
//! HTTP error, a truncated stream — is delivered as [`LlmEvent::Error`] on the
//! stream, because a caller that is already driving a stream should not have to
//! handle a second error channel. `stream_chat` cannot fail before it starts:
//! it performs no I/O until the stream is polled.

use core::fmt;
use std::collections::BTreeMap;
use std::rc::Rc;

use futures::Stream;
use nanus_domain::{Message, ToolCall, ToolCallId, ToolName, ToolSchema, Usage};
use serde_json::Value;

/// A shared, key-addressable model adapter.
///
/// Shared as `Rc<Box<dyn LlmPort>>` rather than `Rc<dyn LlmPort>` because the
/// kernel's registry recovers a value from `dyn Any` and can only downcast to a
/// *sized* type; boxing first is what keeps the capability addressable by key
/// without naming its concrete implementation.
pub type LlmHandle = Rc<Box<dyn LlmPort>>;

/// A pinned stream of [`LlmEvent`]s.
///
/// `'static` because the adapter owns everything the stream needs: nothing about
/// the request borrows from the caller, so a stream outlives the call that
/// produced it.
pub type LlmStream = std::pin::Pin<Box<dyn Stream<Item = LlmEvent> + 'static>>;

/// The language-model port.
pub trait LlmPort {
    /// Returns the model this adapter is configured to talk to.
    ///
    /// Used in the prompt's runtime section and in transcripts, so it reports
    /// what is configured rather than what a particular request overrode.
    fn model(&self) -> &str;

    /// Returns the reasoning effort this adapter applies to a request naming `model` that sets none.
    ///
    /// The adapter is the component that fills an unset effort in, so it is the only one
    /// that can answer what a run actually asked for. A caller records this rather than
    /// assuming the configured default, because "which effort produced this session" is
    /// what makes two runs comparable and a guess would make them silently incomparable.
    ///
    /// `model` is passed rather than assumed for the same reason [`LlmPort::effort_levels`] takes
    /// it: a runner may switch models without rebuilding the adapter, and whether a default is
    /// applied at all is a fact about the model — one that takes no effort steps has none in force.
    ///
    /// Defaults to `None`, meaning this adapter has no notion of effort for `model`. That is not
    /// the same fact as "medium", and it is reported as the absence it is.
    fn reasoning_effort(&self, model: &str) -> Option<ReasoningEffort> {
        let _ = model;
        None
    }

    /// Returns the effort steps this adapter may send for `model`, in increasing order.
    ///
    /// The scale is provider-neutral, but which steps a *model* accepts is a provider fact: one
    /// `OpenAI` model takes `none` through `max` while another has no `none`, and an Anthropic model
    /// that predates effort takes none of them. A caller that offered the whole scale would offer
    /// steps the provider refuses, so the interface reads this before it draws the list. An empty
    /// slice means this adapter has no notion of effort at all — the same fact
    /// [`LlmPort::reasoning_effort`] reports as `None` for that model.
    ///
    /// `model` is passed rather than assumed because a runner may switch models without rebuilding
    /// the adapter, so the answer is a fact about the id and not about the instance.
    fn effort_levels(&self, model: &str) -> &'static [ReasoningEffort] {
        let _ = model;
        &[]
    }

    /// Returns local capability metadata for this exact model and configured protocol.
    ///
    /// Defaults to unknown metadata; never reads credentials or performs network I/O.
    fn capabilities(&self, model: &str) -> crate::ModelCapabilities {
        let _ = model;
        crate::ModelCapabilities::default()
    }

    /// Reports tool support for this exact model, configured wire/endpoint and effective effort.
    ///
    /// `request_effort = None` inherits the adapter's configured default; it differs from
    /// `Some(ReasoningEffort::None)`, which explicitly disables reasoning. This local query
    /// never reads credentials or performs network I/O. Unknown preserves existing adapters;
    /// an embedding host may require Supported before admitting a registry or request.
    fn tool_call_support(
        &self,
        model: &str,
        request_effort: Option<ReasoningEffort>,
    ) -> crate::ToolCallSupport {
        let _ = (model, request_effort);
        crate::ToolCallSupport::Unknown
    }

    /// Estimates the fully translated request without network access.
    ///
    /// Concrete providers override this with their actual encoder. The default serves fake
    /// adapters and counts the neutral representation conservatively.
    fn estimate_request(&self, request: &ChatRequest) -> LlmResult<crate::RequestEstimate> {
        let payload = serde_json::json!({ "model": request.model, "messages": request.messages,
            "tools": request.tools, "max_tokens": request.max_tokens });
        crate::capabilities::estimate_payload(self.capabilities(&request.model), request, &payload)
    }

    /// Starts a chat completion and returns its event stream.
    ///
    /// Failure is delivered as [`LlmEvent::Error`]; a caller that has read the
    /// stream to the end has seen every failure that occurred.
    fn stream_chat(&self, request: ChatRequest) -> LlmStream;

    /// Reports whether this exact model, on this adapter's configured protocol and endpoint,
    /// can carry a managed-context projection.
    ///
    /// Unsupported by default, and an adapter turns it on only for combinations whose pure
    /// preparation, wire and reload fixtures exist. A source digest is not provider evidence.
    fn managed_support(&self, model: &str) -> ManagedSupport {
        let _ = model;
        ManagedSupport::Unsupported
    }

    /// Prepares one managed request without performing any I/O.
    ///
    /// The returned call owns the admitted body: its estimate and digest are of exactly the bytes
    /// [`PreparedModelCall::stream`] sends, so a request is never re-encoded between the moment
    /// it is measured and recorded and the moment it is dispatched.
    ///
    /// # Errors
    ///
    /// Returns [`LlmError::Unsupported`] by default, and for a candidate this path cannot carry.
    fn prepare_managed(&self, request: ManagedRequest) -> LlmResult<Box<dyn PreparedModelCall>> {
        let _ = request;
        Err(LlmError::Unsupported {
            feature: "managed context".into(),
        })
    }
}

/// Whether a model path can carry a managed projection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ManagedSupport {
    /// No pure preparation or fixtures exist for this path.
    #[default]
    Unsupported,
    /// Preparation exists for this policy version.
    Supported {
        /// The context policy version the preparation implements.
        policy_version: u32,
    },
}

impl ManagedSupport {
    /// Whether `policy_version` is supported.
    #[must_use]
    pub const fn supports(self, policy_version: u32) -> bool {
        matches!(self, Self::Supported { policy_version: supported } if supported == policy_version)
    }
}

/// One managed request to prepare.
#[derive(Clone, Debug, PartialEq)]
#[allow(clippy::derive_partial_eq_without_eq)]
pub struct ManagedRequest {
    /// The candidate, already compiled and fitted by the runner.
    pub request: ChatRequest,
    /// The runner's selection epoch, recorded in the identity.
    pub selection_epoch: u64,
}

/// An adapter-owned, admitted, not-yet-sent model call.
///
/// It owns the body and the endpoint, account and credential it will use, privately; it exposes
/// no mutation and no serializable handle, and nothing happens on the network until
/// [`PreparedModelCall::stream`] is polled.
pub trait PreparedModelCall {
    /// The estimate of exactly the prepared body.
    fn estimate(&self) -> crate::RequestEstimate;

    /// BLAKE3 of exactly the prepared body.
    fn request_digest(&self) -> &nanus_domain::context::managed::Digest;

    /// The route, without credentials.
    fn selection(&self) -> &nanus_domain::context::managed::SelectionIdentity;

    /// Sends the prepared body, consuming the call.
    fn stream(self: Box<Self>) -> LlmStream;
}

/// Raw argument byte limits for named tools, applied while a call's arguments stream in.
///
/// Decoder metadata, never a schema field: a limit is checked once the call's final name is
/// known and before the arguments are parsed, so a call that is too large never becomes a JSON
/// value — and a name that arrives late is checked when it arrives, against everything already
/// buffered. Tools with no limit here keep their existing bounds.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToolArgumentLimits {
    limits: Vec<(String, usize)>,
}

/// The argument text a call over its limit is replaced with.
///
/// Not JSON, so the tool's parser refuses it as an ordinary bounded failure the model reads.
pub const OVERSIZED_ARGUMENTS: &str = "<tool arguments exceeded their byte limit and were dropped>";

impl ToolArgumentLimits {
    /// The managed-context limits: 16 KiB for `context_manage`, 2 KiB for `context_recall`.
    #[must_use]
    pub fn managed() -> Self {
        use nanus_domain::context::managed::{MANAGE_TOOL, RECALL_TOOL, limits};
        Self {
            limits: vec![
                (MANAGE_TOOL.to_owned(), limits::MANAGE_ARGUMENT_BYTES_MAX),
                (RECALL_TOOL.to_owned(), limits::RECALL_ARGUMENT_BYTES_MAX),
            ],
        }
    }

    /// The limit for a tool, when it has one.
    #[must_use]
    pub fn limit_for(&self, name: &str) -> Option<usize> {
        self.limits
            .iter()
            .find(|(tool, _)| tool == name)
            .map(|(_, limit)| *limit)
    }

    /// Whether any limit is set.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.limits.is_empty()
    }
}

/// How much reasoning effort to ask the model to spend.
///
/// A provider-neutral scale in increasing order. Not every provider offers every step — and not
/// every *model* does — so a caller asks which steps a model supports before offering them; see
/// [`LlmPort::effort_levels`]. [`ReasoningEffort::None`] is the bottom of the scale: it asks the
/// provider to spend nothing at all, which a provider that expresses thinking as a switch writes
/// as that switch being off rather than as an effort of zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ReasoningEffort {
    /// Spend nothing: the provider's way of turning thinking off.
    None,
    /// Spend as little as possible while still thinking.
    Minimal,
    /// Spend a little.
    Low,
    /// Spend the default amount.
    Medium,
    /// Spend more than the default.
    High,
    /// Spend still more, for long-horizon work.
    XHigh,
    /// Spend as much as the provider allows.
    Max,
}

impl ReasoningEffort {
    /// Returns the wire name of the effort.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }

    /// Returns the step a wire name names, when it names one.
    ///
    /// The counterpart of [`ReasoningEffort::as_str`], and the pair exists for the same reason the
    /// finish reason has one: a caller that *draws* the name of the effort in force and then steps
    /// the scale from what it drew needs to read its own drawing back, and a scale with one
    /// direction only would have to be spelled a second time wherever it is stepped.
    ///
    /// `None` is an honest answer rather than a defaulted one: a word that is not on the scale is
    /// not a step, and the caller has its own idea of where to start — the interface steps from the
    /// middle of the scale when nothing is known — which a plausible-looking guess here would take
    /// away from it.
    ///
    /// Not `const`, unlike [`ReasoningEffort::as_str`]: matching on a string is not something a
    /// constant function may do yet, which is also why [`FinishReason::parse`] is not one.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "none" => Some(Self::None),
            "minimal" => Some(Self::Minimal),
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "xhigh" => Some(Self::XHigh),
            "max" => Some(Self::Max),
            _ => None,
        }
    }

    /// Returns the next step of the scale, wrapping round.
    ///
    /// The order is by increasing effort, so a reader stepping through it is asking for more
    /// thinking rather than for a different setting, and wrapping is what keeps a key from
    /// being stuck at the top: an interface cycling the scale needs a step that always exists.
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::None => Self::Minimal,
            Self::Minimal => Self::Low,
            Self::Low => Self::Medium,
            Self::Medium => Self::High,
            Self::High => Self::XHigh,
            Self::XHigh => Self::Max,
            Self::Max => Self::None,
        }
    }
}

impl fmt::Display for ReasoningEffort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One chat completion request.
///
/// The fields are the whole of what an adapter may send. There is no catch-all
/// `extra: Value`, because a request body is a wire contract and a field that
/// bypasses the type is a field that bypasses review.
#[derive(Clone, Debug, PartialEq)]
#[allow(clippy::derive_partial_eq_without_eq)]
pub struct ChatRequest {
    /// The model to send to.
    pub model: String,
    /// The conversation so far, in order.
    pub messages: Vec<Message>,
    /// Immutable unelided source for replay/fitting admission. Never encoded into the HTTP body.
    /// Absence means `messages` itself is the original source, preserving stock callers.
    pub source_history: Option<std::sync::Arc<[Message]>>,
    /// The tools the model may call. This is the wire allowlist: a
    /// [`ToolSchema`] has no field an executor could hide in.
    pub tools: Vec<ToolSchema>,
    /// The response token ceiling, when the caller wants one.
    pub max_tokens: Option<u32>,
    /// Separately bounded reasoning tokens, outside the output ceiling (zero when included).
    pub separate_reasoning_tokens: u32,
    /// Caller context ceiling, when preflight is explicitly requested.
    pub context_budget: Option<u32>,
    /// How much reasoning to request.
    pub reasoning_effort: Option<ReasoningEffort>,
    /// The sampling temperature.
    pub temperature: Option<f32>,
}

impl ChatRequest {
    /// Builds a request with no optional knobs set.
    #[must_use]
    pub fn new(model: impl Into<String>, messages: Vec<Message>) -> Self {
        Self {
            model: model.into(),
            messages,
            source_history: None,
            tools: Vec::new(),
            max_tokens: None,
            separate_reasoning_tokens: 0,
            context_budget: None,
            reasoning_effort: None,
            temperature: None,
        }
    }

    /// Attaches the tool schemas.
    #[must_use]
    pub fn with_tools(mut self, tools: Vec<ToolSchema>) -> Self {
        self.tools = tools;
        self
    }

    /// Sets the response token ceiling.
    #[must_use]
    pub const fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }

    /// Sets the reasoning effort.
    #[must_use]
    pub const fn with_reasoning_effort(mut self, effort: ReasoningEffort) -> Self {
        self.reasoning_effort = Some(effort);
        self
    }

    /// Sets the sampling temperature.
    #[must_use]
    pub const fn with_temperature(mut self, temperature: f32) -> Self {
        self.temperature = Some(temperature);
        self
    }
}

/// Why the model stopped producing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FinishReason {
    /// The model finished its turn.
    Stop,
    /// The model wants tools run.
    ToolCalls,
    /// The response hit its token ceiling.
    Length,
    /// A content filter stopped the response.
    ContentFilter,
    /// A reason this crate does not know, preserved verbatim.
    Unknown(String),
}

impl FinishReason {
    /// Parses a provider's finish reason.
    ///
    /// An unrecognised value becomes [`FinishReason::Unknown`] rather than an
    /// error: a provider adding a reason must not break the harness, and the
    /// original text is kept so a transcript still says what happened.
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        match raw {
            "stop" => Self::Stop,
            "tool_calls" => Self::ToolCalls,
            "length" => Self::Length,
            "content_filter" => Self::ContentFilter,
            other => Self::Unknown(other.to_owned()),
        }
    }

    /// Returns the wire name of the reason.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Stop => "stop",
            Self::ToolCalls => "tool_calls",
            Self::Length => "length",
            Self::ContentFilter => "content_filter",
            Self::Unknown(raw) => raw,
        }
    }

    /// Returns `true` when the reason means "another step is owed".
    #[must_use]
    pub const fn expects_tool_calls(&self) -> bool {
        matches!(self, Self::ToolCalls)
    }
}

impl fmt::Display for FinishReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One increment of a streamed response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LlmEvent {
    /// A chunk of the model's reasoning trace.
    ReasoningDelta(String),
    /// A chunk of the model's visible text.
    TextDelta(String),
    /// A chunk of a tool call.
    ///
    /// The first chunk for an `index` carries the id and usually the name;
    /// later chunks carry only more `arguments_delta` text. Nothing else
    /// identifies a call, so `index` is the join key within one response.
    ToolCallDelta {
        /// The position of the call within this response.
        index: u32,
        /// The provider-assigned call id, on the first chunk for `index`.
        id: Option<ToolCallId>,
        /// The tool's name, usually on the first chunk for `index`.
        name: Option<ToolName>,
        /// A fragment of the arguments JSON string.
        arguments_delta: String,
    },
    /// Ordered signed assistant blocks for persistence and protocol-specific replay.
    AssistantReplay(nanus_domain::message::AssistantReplay),
    /// The server began answering: its response head arrived, before any of the body.
    ///
    /// A fact about the transport rather than about the model, and the only one this vocabulary
    /// carries. It exists because it is the one boundary a request's wait can be split at from
    /// this side of the socket: everything before it is connecting, uploading, and waiting to be
    /// answered at all, and everything after it is the server's own work. Without the split
    /// those are a single number, and telling a slow network from a slow prompt is a guess.
    ///
    /// It carries no instant, because a listener owns its own clock — an adapter that reported
    /// the time would be reporting a reading from a clock nobody else measures with. The event
    /// says only *that* it happened, as everything else here does.
    ///
    /// Optional, like the rest: a port that cannot see a response head emits none, and a
    /// listener that has no use for it ignores it.
    ResponseHead,
    /// Token accounting, usually delivered once near the end.
    Usage(Usage),
    /// The response is complete.
    Finished {
        /// Why it stopped.
        reason: FinishReason,
    },
    /// The request failed.
    ///
    /// The string is already rendered: the core never sees a transport error.
    Error(String),
}

/// Why an adapter could not serve a request.
///
/// Delivered as [`LlmEvent::Error`] after its message has been rendered, and
/// also usable directly by an adapter's own startup checks.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum LlmError {
    /// A caller-selected provider response budget was exhausted before retention.
    #[error("response limit exceeded: {resource} (maximum {limit})")]
    ResponseLimit {
        /// The bounded resource, never provider content or a credential.
        resource: &'static str,
        /// The caller's byte or count ceiling.
        limit: usize,
    },
    /// No usable credential was found for the provider.
    #[error("no credentials for provider {provider}")]
    MissingCredentials {
        /// The provider whose credential is missing.
        provider: String,
    },

    /// The connection failed before a response started.
    #[error("transport failure: {message}")]
    Transport {
        /// The rendered transport failure.
        message: String,
    },

    /// The provider answered with a non-success status.
    #[error("provider returned HTTP {status}: {body_snippet}")]
    HttpStatus {
        /// The HTTP status code.
        status: u16,
        /// A short, sanitised excerpt of the response body.
        body_snippet: String,
    },

    /// The response stream did not conform to the protocol.
    ///
    /// This is where malformed tool-call arguments surface: the model really
    /// does emit truncated JSON, and the assembler reports it here rather than
    /// handing a half-parsed call to a tool.
    #[error("malformed stream: {message}")]
    MalformedStream {
        /// What was wrong with the stream.
        message: String,
    },

    /// The adapter was asked for something it cannot do.
    #[error("unsupported feature: {feature}")]
    Unsupported {
        /// The unsupported feature.
        feature: String,
    },
}

/// The result type for adapter internals that fail before a stream starts.
pub type LlmResult<T> = Result<T, LlmError>;

/// Renders a short, human-readable excerpt of an error response body.
///
/// Providers wrap the useful sentence in JSON, so the excerpt is the
/// `error.message` field when there is one and the raw body otherwise, cut to
/// `max_chars` characters on a character boundary. This is the only place the
/// crate parses an error body, so every adapter's failure text looks the same.
#[must_use]
pub fn error_body_snippet(body: &str, max_chars: usize) -> String {
    let message = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| body.to_owned());
    truncate_chars(&message, max_chars)
}

/// Cuts `text` to at most `max_chars` characters, marking a cut with an ellipsis.
#[must_use]
pub fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max_chars).collect();
    out.push('…');
    out
}

/// One tool call being assembled from stream deltas.
#[derive(Clone, Debug, Default)]
struct Slot {
    /// The call id, once a chunk has carried it.
    id: Option<ToolCallId>,
    /// The tool name, once a chunk has carried it.
    name: Option<ToolName>,
    /// The argument fragments, concatenated in arrival order.
    arguments: String,
    /// Whether the arguments passed their tool's limit and were dropped.
    oversized: bool,
}

/// Rebuilds whole tool calls from the deltas of one response.
///
/// The provider streams a tool call as a sequence of fragments keyed by `index`;
/// nothing else can be trusted to arrive in order or in one piece. The assembler
/// is the single place that join happens, so every adapter produces identical
/// calls from identical bytes — and so a malformed join is reported as
/// [`LlmError::MalformedStream`] rather than handed to a tool.
#[derive(Clone, Debug, Default)]
pub struct ToolCallAssembler {
    /// One slot per tool-call index, ordered so `finish` restores provider order.
    slots: BTreeMap<u32, Slot>,
    /// Raw argument limits by tool name.
    limits: ToolArgumentLimits,
}

impl ToolCallAssembler {
    /// Creates an empty assembler.
    #[must_use]
    pub fn new() -> Self {
        Self {
            slots: BTreeMap::new(),
            limits: ToolArgumentLimits::default(),
        }
    }

    /// Creates an assembler that enforces raw argument limits before parsing.
    #[must_use]
    pub fn with_limits(limits: ToolArgumentLimits) -> Self {
        Self {
            slots: BTreeMap::new(),
            limits,
        }
    }

    /// Returns the number of tool calls seen so far.
    #[must_use]
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// Returns `true` when no tool call has been seen.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Folds one delta into the call at `index`.
    ///
    /// # Errors
    ///
    /// Returns [`LlmError::MalformedStream`] when a later chunk contradicts an
    /// earlier one — a different id or a different name for the same index — or
    /// when a chunk carries an empty id.
    pub fn apply(
        &mut self,
        index: u32,
        id: Option<ToolCallId>,
        name: Option<ToolName>,
        arguments_delta: &str,
    ) -> LlmResult<()> {
        if let Some(id) = &id
            && id.is_empty()
        {
            return Err(LlmError::MalformedStream {
                message: format!("tool call {index} carries an empty id"),
            });
        }
        let slot = self.slots.entry(index).or_default();
        let merged_id = merge_id(slot, index, id)?;
        let merged_name = merge_name(slot, index, name)?;
        // Postcondition: the slot holds whatever the merge decided, so a later
        // chunk cannot see a value that was not recorded.
        assert_eq!(slot.id, merged_id);
        assert_eq!(slot.name, merged_name);
        if !slot.oversized {
            slot.arguments.push_str(arguments_delta);
        }
        // Checked whenever the name is known, so a name that arrives after its arguments is
        // judged against everything already buffered; and the buffer is released at once.
        let limit = slot
            .name
            .as_ref()
            .and_then(|name| self.limits.limit_for(name.as_str()));
        if limit.is_some_and(|limit| slot.arguments.len() > limit) {
            slot.oversized = true;
            slot.arguments = String::new();
        }
        Ok(())
    }

    /// Folds one stream event, ignoring the ones that carry no tool call.
    ///
    /// # Errors
    ///
    /// Returns [`LlmError::MalformedStream`] under the same conditions as
    /// [`apply`](ToolCallAssembler::apply).
    pub fn apply_event(&mut self, event: &LlmEvent) -> LlmResult<()> {
        if let LlmEvent::ToolCallDelta {
            index,
            id,
            name,
            arguments_delta,
        } = event
        {
            return self.apply(*index, id.clone(), name.clone(), arguments_delta);
        }
        Ok(())
    }

    /// Finishes assembly, returning the calls in provider order.
    ///
    /// # Errors
    ///
    /// Returns [`LlmError::MalformedStream`] when a call is missing its id or
    /// name, or when its accumulated arguments are not valid JSON.
    pub fn finish(self) -> LlmResult<Vec<ToolCall>> {
        let slots = self.slots;
        let expected = slots.len();
        let mut calls: Vec<ToolCall> = Vec::with_capacity(expected);
        for (index, slot) in slots {
            let id = slot.id.ok_or_else(|| LlmError::MalformedStream {
                message: format!("tool call {index} never received an id"),
            })?;
            let name = slot.name.ok_or_else(|| LlmError::MalformedStream {
                message: format!("tool call {index} never received a name"),
            })?;
            if slot.oversized {
                calls.push(ToolCall::new(
                    id,
                    name,
                    Value::String(OVERSIZED_ARGUMENTS.to_owned()),
                ));
                continue;
            }
            let call =
                ToolCall::try_from_arguments_json(id, name, &slot.arguments).map_err(|error| {
                    LlmError::MalformedStream {
                        message: format!("tool call {index} has invalid arguments: {error}"),
                    }
                })?;
            calls.push(call);
        }
        // Postcondition: one call per slot, in ascending index order, so a
        // response with no tool calls finishes with an empty list rather than an
        // error.
        assert_eq!(calls.len(), expected);
        Ok(calls)
    }
}

/// Merges a chunk's id into a slot, rejecting a contradiction.
fn merge_id(slot: &mut Slot, index: u32, id: Option<ToolCallId>) -> LlmResult<Option<ToolCallId>> {
    let Some(id) = id else {
        return Ok(slot.id.clone());
    };
    match &slot.id {
        None => {
            slot.id = Some(id);
            Ok(slot.id.clone())
        }
        Some(existing) if *existing == id => Ok(slot.id.clone()),
        Some(_) => Err(LlmError::MalformedStream {
            message: format!("tool call {index} changed id mid-stream"),
        }),
    }
}

/// Merges a chunk's name into a slot, rejecting a contradiction.
fn merge_name(slot: &mut Slot, index: u32, name: Option<ToolName>) -> LlmResult<Option<ToolName>> {
    let Some(name) = name else {
        return Ok(slot.name.clone());
    };
    match &slot.name {
        None => {
            slot.name = Some(name);
            Ok(slot.name.clone())
        }
        Some(existing) if *existing == name => Ok(slot.name.clone()),
        Some(_) => Err(LlmError::MalformedStream {
            message: format!("tool call {index} changed name mid-stream"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The scale steps upward and wraps, which is what an interface cycling it needs: a step
    /// that ends at the top would be a key that stops working.
    #[test]
    fn the_effort_scale_steps_upward_and_wraps() {
        assert_eq!(ReasoningEffort::None.next(), ReasoningEffort::Minimal);
        assert_eq!(ReasoningEffort::Minimal.next(), ReasoningEffort::Low);
        assert_eq!(ReasoningEffort::Low.next(), ReasoningEffort::Medium);
        assert_eq!(ReasoningEffort::Medium.next(), ReasoningEffort::High);
        assert_eq!(ReasoningEffort::High.next(), ReasoningEffort::XHigh);
        assert_eq!(ReasoningEffort::XHigh.next(), ReasoningEffort::Max);
        assert_eq!(ReasoningEffort::Max.next(), ReasoningEffort::None);
        // The order agrees with `Ord`, so "more effort" means the same thing however it is
        // expressed.
        assert!(ReasoningEffort::High > ReasoningEffort::Minimal);
        assert!(ReasoningEffort::Max > ReasoningEffort::None);
    }

    /// Every step survives the round trip through its own name, which is what lets a caller draw
    /// the name and step the scale from what it drew. Both directions, because a name added to
    /// one and not the other is a key that stops at a value nobody can read back.
    #[test]
    fn an_effort_is_read_back_from_its_own_name() {
        for effort in [
            ReasoningEffort::None,
            ReasoningEffort::Minimal,
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High,
            ReasoningEffort::XHigh,
            ReasoningEffort::Max,
        ] {
            assert_eq!(ReasoningEffort::parse(effort.as_str()), Some(effort));
            assert_eq!(effort.to_string(), effort.as_str());
        }
        // A word that is not on the scale is not a step, and is not guessed at.
        assert_eq!(ReasoningEffort::parse("mediumish"), None);
        assert_eq!(ReasoningEffort::parse(""), None);
    }

    fn tool_name(raw: &str) -> ToolName {
        ToolName::new(raw).unwrap_or_else(|error| panic!("test tool name {raw}: {error}"))
    }

    /// One fragment of a streamed tool call: index, id, name, argument text.
    type Chunk<'a> = (u32, Option<&'a str>, Option<&'a str>, &'a str);

    fn assemble(chunks: &[Chunk<'_>]) -> LlmResult<Vec<ToolCall>> {
        let mut assembler = ToolCallAssembler::new();
        for (index, id, name, delta) in chunks {
            let id = id.map(ToolCallId::new);
            let name = name.map(tool_name);
            assembler.apply(*index, id, name, delta)?;
        }
        // One slot needs at least one chunk, so the slot count never exceeds the
        // chunk count.
        assert!(assembler.len() <= chunks.len());
        assembler.finish()
    }

    #[test]
    fn deltas_assemble_into_one_call() {
        // The realistic fragment sequence: id and name first, arguments split
        // across chunks.
        let calls = assemble(&[
            (0, Some("call-1"), Some("read"), "{\"pa"),
            (0, None, None, "th\":\"src/"),
            (0, None, None, "lib.rs\"}"),
        ]);
        assert!(calls.is_ok());
        let Ok(calls) = calls else { return };
        assert_eq!(calls.len(), 1);
        let Some(call) = calls.first() else { return };
        assert_eq!(call.id.as_str(), "call-1");
        assert_eq!(call.name.as_str(), "read");
        assert_eq!(call.arguments.get("path"), Some(&Value::from("src/lib.rs")));
    }

    #[test]
    fn parallel_calls_are_restored_in_index_order() {
        let calls = assemble(&[
            (1, Some("b"), Some("write"), "{}"),
            (0, Some("a"), Some("read"), "{}"),
        ]);
        assert!(calls.is_ok());
        let Ok(calls) = calls else { return };
        let ids: Vec<&str> = calls.iter().map(|call| call.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b"], "index order, not arrival order");
    }

    #[test]
    fn a_repeated_id_for_one_index_is_tolerated_but_a_change_is_not() {
        let repeated = assemble(&[
            (0, Some("call-1"), Some("read"), "{}"),
            (0, Some("call-1"), None, ""),
        ]);
        assert!(repeated.is_ok(), "providers repeat the id");

        let changed = assemble(&[
            (0, Some("call-1"), Some("read"), "{}"),
            (0, Some("call-2"), None, ""),
        ]);
        assert!(matches!(changed, Err(LlmError::MalformedStream { .. })));

        let renamed = assemble(&[
            (0, Some("call-1"), Some("read"), "{}"),
            (0, None, Some("write"), ""),
        ]);
        assert!(matches!(renamed, Err(LlmError::MalformedStream { .. })));
    }

    #[test]
    fn malformed_arguments_are_reported_rather_than_handed_to_a_tool() {
        let calls = assemble(&[(0, Some("call-1"), Some("read"), "{\"path\":")]);
        assert!(matches!(calls, Err(LlmError::MalformedStream { .. })));
    }

    #[test]
    fn an_incomplete_call_is_reported() {
        let no_id = assemble(&[(0, None, Some("read"), "{}")]);
        assert!(matches!(no_id, Err(LlmError::MalformedStream { .. })));
        let no_name = assemble(&[(0, Some("call-1"), None, "{}")]);
        assert!(matches!(no_name, Err(LlmError::MalformedStream { .. })));
    }

    /// T16: the limit is applied once the name is known and before parsing; a name arriving
    /// after its arguments is judged against everything buffered so far.
    #[test]
    fn an_argument_over_its_tools_limit_is_dropped_before_parsing() {
        let manage = || Some(tool_name("context_manage"));
        let at_limit = format!("\"{}\"", "x".repeat(16 * 1024 - 2));
        let mut fits = ToolCallAssembler::with_limits(ToolArgumentLimits::managed());
        assert!(
            fits.apply(0, Some(ToolCallId::new("a")), manage(), &at_limit)
                .is_ok()
        );
        let calls = fits.finish().unwrap_or_default();
        assert_eq!(
            calls.first().map(|call| call.arguments.is_string()),
            Some(true)
        );
        assert_ne!(
            calls.first().and_then(|call| call.arguments.as_str()),
            Some(OVERSIZED_ARGUMENTS)
        );

        let over = format!("{at_limit} ");
        let mut late = ToolCallAssembler::with_limits(ToolArgumentLimits::managed());
        assert!(
            late.apply(0, Some(ToolCallId::new("b")), None, &over)
                .is_ok()
        );
        assert!(
            late.apply(0, None, manage(), "").is_ok(),
            "the name arrives last"
        );
        let calls = late.finish().unwrap_or_default();
        assert_eq!(
            calls.first().and_then(|call| call.arguments.as_str()),
            Some(OVERSIZED_ARGUMENTS)
        );

        let mut other = ToolCallAssembler::with_limits(ToolArgumentLimits::managed());
        let big = format!("{{\"text\":\"{}\"}}", "y".repeat(20_000));
        assert!(
            other
                .apply(
                    0,
                    Some(ToolCallId::new("c")),
                    Some(tool_name("write")),
                    &big
                )
                .is_ok()
        );
        assert!(
            other
                .finish()
                .is_ok_and(|calls| calls[0].arguments.is_object())
        );
    }

    #[test]
    fn an_empty_id_is_rejected_at_the_delta() {
        let mut assembler = ToolCallAssembler::new();
        let outcome = assembler.apply(0, Some(ToolCallId::new("")), None, "{}");
        assert!(matches!(outcome, Err(LlmError::MalformedStream { .. })));
    }

    #[test]
    fn only_tool_call_deltas_reach_the_assembler() {
        let mut assembler = ToolCallAssembler::new();
        let events = [
            LlmEvent::ReasoningDelta("hmm".to_owned()),
            LlmEvent::TextDelta("hi".to_owned()),
            LlmEvent::Usage(Usage::new(1, 2, 0, 0, 1)),
            LlmEvent::Finished {
                reason: FinishReason::Stop,
            },
        ];
        for event in &events {
            assert!(assembler.apply_event(event).is_ok());
        }
        assert!(assembler.is_empty());
        assert_eq!(assembler.len(), 0);
        let finished = assembler.finish();
        assert_eq!(finished.ok(), Some(Vec::new()));
    }

    #[test]
    fn a_delta_event_flows_through_apply_event() {
        let mut assembler = ToolCallAssembler::new();
        let event = LlmEvent::ToolCallDelta {
            index: 2,
            id: Some(ToolCallId::new("c")),
            name: Some(tool_name("read")),
            arguments_delta: "{}".to_owned(),
        };
        assert!(assembler.apply_event(&event).is_ok());
        let calls = assembler.finish();
        assert!(calls.is_ok());
        assert_eq!(calls.map(|calls| calls.len()).ok(), Some(1));
    }

    #[test]
    fn finish_reasons_round_trip_through_their_names() {
        for (raw, expected) in [
            ("stop", FinishReason::Stop),
            ("tool_calls", FinishReason::ToolCalls),
            ("length", FinishReason::Length),
            ("content_filter", FinishReason::ContentFilter),
        ] {
            let parsed = FinishReason::parse(raw);
            assert_eq!(parsed, expected);
            assert_eq!(parsed.as_str(), raw);
        }
        // An unknown reason is preserved rather than rejected.
        let unknown = FinishReason::parse("brand_new");
        assert_eq!(unknown, FinishReason::Unknown("brand_new".to_owned()));
        assert_eq!(unknown.as_str(), "brand_new");
        assert_eq!(unknown.to_string(), "brand_new");
        assert!(!unknown.expects_tool_calls());
        assert!(FinishReason::ToolCalls.expects_tool_calls());
    }

    #[test]
    fn an_error_body_is_unwrapped_and_truncated() {
        let wrapped = r#"{"error":{"message":"Authentication Fails","type":"auth"}}"#;
        assert_eq!(error_body_snippet(wrapped, 64), "Authentication Fails");

        // A body that is not the expected shape falls back to the raw text.
        assert_eq!(
            error_body_snippet("<html>502</html>", 64),
            "<html>502</html>"
        );

        // Truncation happens on a character boundary, so a multi-byte body
        // cannot produce invalid UTF-8.
        let long = "é".repeat(50);
        let snippet = error_body_snippet(&long, 4);
        assert_eq!(snippet, "éééé…");
        assert!(snippet.is_char_boundary(snippet.len()));
    }

    #[test]
    fn a_request_carries_only_what_the_adapter_may_send() {
        let request = ChatRequest::new("deepseek-flash", vec![Message::user("hi")])
            .with_max_tokens(64)
            .with_reasoning_effort(ReasoningEffort::High)
            .with_temperature(0.2_f32);
        assert_eq!(request.model, "deepseek-flash");
        assert_eq!(request.messages.len(), 1);
        assert!(request.tools.is_empty());
        assert_eq!(request.max_tokens, Some(64));
        assert_eq!(request.reasoning_effort, Some(ReasoningEffort::High));
        // Compared by bits: a float equality assertion is not a real invariant.
        assert_eq!(
            request.temperature.map(f32::to_bits),
            Some(0.2_f32.to_bits())
        );
        assert_eq!(ReasoningEffort::High.as_str(), "high");
    }
}
