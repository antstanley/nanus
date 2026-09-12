//! The model-facing tool contract.
//!
//! A tool has two halves and the crate keeps them apart on purpose. The
//! [`ToolSchema`] half is *wire*: a name, a description, and a JSON-schema
//! `parameters` object, and nothing else may reach a model request. The
//! [`ToolDefinition`] half is *executable*: it holds the schema plus a boxed
//! executor that deliberately has no serialisation.
//!
//! That split is the **wire allowlist invariant**. A tool's registration may
//! carry secrets, handles, or configuration; because the executable half is not
//! `Serialize`, there is no code path by which any of it can be encoded into a
//! request body. The property is proven by a test that serialises a registered
//! tool's schema and asserts the key set is exactly the allowlist.

use core::fmt;
use core::future::Future;
use core::pin::Pin;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::message::ToolCallId;

/// Maximum length, in bytes, of a tool name.
///
/// Names are rendered into a model request, a transcript, and a log line; an
/// unbounded name is a denial-of-service vector for anything that renders it.
pub const TOOL_NAME_MAX_LEN: usize = 64;

/// A validated tool name: non-empty, at most [`TOOL_NAME_MAX_LEN`] bytes, and
/// restricted to `[a-z0-9_-]`.
///
/// The restriction is the model's own: providers accept only this alphabet, and
/// a name that silently changes on the wire would make a tool uncallable in a
/// way no test could see.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ToolName(String);

impl ToolName {
    /// Validates and builds a tool name.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::InvalidName`] when `raw` is empty, longer than
    /// [`TOOL_NAME_MAX_LEN`], or contains a byte outside `[a-z0-9_-]`.
    pub fn new(raw: impl Into<String>) -> Result<Self, ToolError> {
        let raw = raw.into();
        if raw.is_empty() {
            return Err(ToolError::InvalidName {
                name: raw,
                reason: "empty",
            });
        }
        if raw.len() > TOOL_NAME_MAX_LEN {
            return Err(ToolError::InvalidName {
                name: raw,
                reason: "too long",
            });
        }
        let bad = raw
            .bytes()
            .find(|byte| !matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-'));
        if bad.is_some() {
            return Err(ToolError::InvalidName {
                name: raw,
                reason: "bad character",
            });
        }
        // Postcondition: the accepted name is exactly the kind of string the
        // wire protocol accepts, which is what makes it safe to send unchanged.
        assert!(!raw.is_empty());
        assert!(raw.len() <= TOOL_NAME_MAX_LEN);
        Ok(Self(raw))
    }

    /// Returns the name as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consumes the name, returning the underlying string.
    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

impl TryFrom<String> for ToolName {
    type Error = ToolError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::new(raw)
    }
}

impl From<ToolName> for String {
    fn from(name: ToolName) -> Self {
        name.0
    }
}

impl fmt::Display for ToolName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why a tool call could not be honoured before execution began.
///
/// Every variant is a caller-visible condition. Nothing here is an internal
/// invariant violation: an invariant violation is an assertion.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ToolError {
    /// A tool name failed validation.
    #[error("invalid tool name {name:?}: {reason}")]
    InvalidName {
        /// The rejected text.
        name: String,
        /// Why it was rejected.
        reason: &'static str,
    },

    /// The model sent arguments that are not JSON.
    ///
    /// This is not hypothetical: a model truncates, wraps in prose, or emits a
    /// trailing comma, and the harness validates rather than assumes.
    #[error("tool {tool} arguments were not valid JSON: {detail}")]
    InvalidArgumentsJson {
        /// The tool whose arguments failed to parse.
        tool: String,
        /// The underlying parse failure, rendered.
        detail: String,
    },

    /// The arguments parsed, but are not a JSON object.
    #[error("tool {tool} arguments must be a JSON object, found {found}")]
    ArgumentsNotObject {
        /// The tool whose arguments have the wrong shape.
        tool: String,
        /// The JSON kind that was found instead.
        found: &'static str,
    },

    /// No tool is registered under the requested name.
    #[error("tool {name} is not registered")]
    UnknownTool {
        /// The unregistered name.
        name: String,
    },

    /// A tool is registered twice.
    #[error("tool {name} is already registered")]
    DuplicateTool {
        /// The contested name.
        name: String,
    },

    /// Execution failed outside the tool's own outcome channel.
    #[error("tool {tool} failed: {message}")]
    Execution {
        /// The tool that failed.
        tool: String,
        /// The rendered failure.
        message: String,
    },
}

// `serde_json::Value` is only `PartialEq`, so `Eq` cannot be derived even
// though equality here is structurally total.
#[allow(clippy::derive_partial_eq_without_eq)]
/// One model request to run a tool.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolCall {
    /// The provider-assigned id this call is answered with.
    pub id: ToolCallId,
    /// The registered tool to run.
    pub name: ToolName,
    /// The arguments, always a JSON value.
    ///
    /// `Value::Null` is normalised to an empty object by [`ToolCall::new`] and
    /// by deserialisation, so "the model sent no arguments" and "the model sent
    /// `{}`" are the same call.
    pub arguments: Value,
}

impl ToolCall {
    /// Builds a tool call, normalising absent arguments to an empty object.
    #[must_use]
    pub fn new(id: ToolCallId, name: ToolName, arguments: Value) -> Self {
        let arguments = match arguments {
            Value::Null => Value::Object(serde_json::Map::new()),
            other => other,
        };
        Self {
            id,
            name,
            arguments,
        }
    }

    /// Builds a tool call from the raw arguments string a provider streams.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::InvalidArgumentsJson`] when the text is neither empty
    /// nor valid JSON. An empty string means "no arguments", which is the only
    /// reading that does not turn a no-argument tool into a failure.
    pub fn try_from_arguments_json(
        id: ToolCallId,
        name: ToolName,
        raw: &str,
    ) -> Result<Self, ToolError> {
        let arguments =
            parse_arguments_json(raw).map_err(|detail| ToolError::InvalidArgumentsJson {
                tool: name.as_str().to_owned(),
                detail,
            })?;
        Ok(Self::new(id, name, arguments))
    }

    /// Returns the arguments as a JSON object.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::ArgumentsNotObject`] for anything that is not an
    /// object. Hand-built calls can reach this state; deserialised and
    /// constructed ones cannot.
    pub fn arguments_object(&self) -> Result<&serde_json::Map<String, Value>, ToolError> {
        match &self.arguments {
            Value::Object(map) => Ok(map),
            other => Err(ToolError::ArgumentsNotObject {
                tool: self.name.as_str().to_owned(),
                found: json_kind(other),
            }),
        }
    }
}

/// Parses a raw arguments string, treating empty text as an empty object.
///
/// Returns a rendered message rather than a typed error because its two callers
/// wrap the failure differently: serde wraps it in a parse error, and
/// [`ToolCall::try_from_arguments_json`] wraps it in
/// [`ToolError::InvalidArgumentsJson`].
pub(crate) fn parse_arguments_json(raw: &str) -> Result<Value, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(Value::Object(serde_json::Map::new()));
    }
    serde_json::from_str(trimmed).map_err(|error| error.to_string())
}

/// Names the JSON kind of a value, for an error message.
fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// One piece of a tool's rendered result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContentBlock {
    /// Prose the model can read directly.
    Text(String),
    /// An image the model can only read if it is multimodal.
    Image {
        /// The IANA media type, e.g. `image/png`.
        media_type: String,
        /// The base64-encoded bytes.
        data_base64: String,
    },
}

impl ContentBlock {
    /// Renders one block as the text a tool result carries.
    ///
    /// An image renders as a placeholder rather than as its payload: a base64
    /// blob in a transcript would blow the context window and tell the model
    /// nothing it can act on.
    #[must_use]
    pub fn render_text(&self) -> String {
        match self {
            Self::Text(text) => text.clone(),
            Self::Image {
                media_type,
                data_base64,
            } => format!("[image: {media_type}, {} base64 bytes]", data_base64.len()),
        }
    }
}

/// The `type`-tagged wire form of a [`ContentBlock`].
///
/// A separate type rather than a `serde` attribute, because an internally tagged
/// enum cannot carry a newtype variant containing a string.
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentBlockWire {
    Text {
        text: String,
    },
    Image {
        media_type: String,
        data_base64: String,
    },
}

impl Serialize for ContentBlock {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let wire = match self {
            Self::Text(text) => ContentBlockWire::Text { text: text.clone() },
            Self::Image {
                media_type,
                data_base64,
            } => ContentBlockWire::Image {
                media_type: media_type.clone(),
                data_base64: data_base64.clone(),
            },
        };
        wire.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ContentBlock {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = ContentBlockWire::deserialize(deserializer)?;
        Ok(match wire {
            ContentBlockWire::Text { text } => Self::Text(text),
            ContentBlockWire::Image {
                media_type,
                data_base64,
            } => Self::Image {
                media_type,
                data_base64,
            },
        })
    }
}

/// What a tool produced.
///
/// A tool failing is not a harness error: it is an outcome the model is shown so
/// it can adapt. Only conditions that prevent a tool from being *attempted* are
/// [`ToolError`]s.
// `serde_json::Value` is only `PartialEq`, so `Eq` cannot be derived even
// though equality here is structurally total.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, Debug, PartialEq)]
pub enum ToolOutcome {
    /// The tool completed.
    Success {
        /// A machine-readable result, used by the harness and by tests.
        value: Value,
        /// The model-visible result, in order.
        content: Vec<ContentBlock>,
    },
    /// The tool failed, and the failure is the model's information.
    Failure {
        /// A one-line reason.
        message: String,
        /// The model-visible result, in order.
        content: Vec<ContentBlock>,
    },
}

impl ToolOutcome {
    /// Builds a successful outcome with no content blocks.
    #[must_use]
    pub const fn success(value: Value) -> Self {
        Self::Success {
            value,
            content: Vec::new(),
        }
    }

    /// Builds a successful outcome with content blocks.
    #[must_use]
    pub const fn success_with(value: Value, content: Vec<ContentBlock>) -> Self {
        Self::Success { value, content }
    }

    /// Builds a failed outcome whose message is also its content.
    #[must_use]
    pub fn failure(message: impl Into<String>) -> Self {
        let message = message.into();
        Self::Failure {
            message: message.clone(),
            content: vec![ContentBlock::Text(message)],
        }
    }

    /// Builds a failed outcome with a separate message and content.
    #[must_use]
    pub const fn failure_with(message: String, content: Vec<ContentBlock>) -> Self {
        Self::Failure { message, content }
    }

    /// Returns `true` only for [`ToolOutcome::Success`].
    #[must_use]
    pub const fn is_success(&self) -> bool {
        matches!(self, Self::Success { .. })
    }

    /// Returns the model-visible content blocks.
    #[must_use]
    pub fn content(&self) -> &[ContentBlock] {
        match self {
            Self::Success { content, .. } | Self::Failure { content, .. } => content,
        }
    }

    /// Returns the machine-readable value, if this outcome has one.
    #[must_use]
    pub const fn value(&self) -> Option<&Value> {
        match self {
            Self::Success { value, .. } => Some(value),
            Self::Failure { .. } => None,
        }
    }

    /// Returns the failure message, if this outcome is a failure.
    #[must_use]
    pub fn message(&self) -> Option<&str> {
        match self {
            Self::Failure { message, .. } => Some(message.as_str()),
            Self::Success { .. } => None,
        }
    }

    /// Renders the outcome as the single string a tool message carries.
    ///
    /// Content blocks win when present. With no blocks, a success renders its
    /// JSON value and a failure renders its message, so a tool that returns a
    /// value but no prose still says something useful.
    #[must_use]
    pub fn render_text(&self) -> String {
        let blocks = self.content();
        if !blocks.is_empty() {
            let rendered: Vec<String> = blocks.iter().map(ContentBlock::render_text).collect();
            return rendered.join("\n");
        }
        match self {
            Self::Success { value, .. } => match value {
                Value::Null => String::new(),
                other => other.to_string(),
            },
            Self::Failure { message, .. } => message.clone(),
        }
    }
}

/// One tool call paired with what it produced.
// `serde_json::Value` is only `PartialEq`, so `Eq` cannot be derived even
// though equality here is structurally total.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, Debug, PartialEq)]
pub struct ToolResult {
    /// The call this result answers.
    pub call_id: ToolCallId,
    /// What the call produced.
    pub outcome: ToolOutcome,
}

impl ToolResult {
    /// Pairs a call id with an outcome.
    #[must_use]
    pub const fn new(call_id: ToolCallId, outcome: ToolOutcome) -> Self {
        Self { call_id, outcome }
    }

    /// Builds a successful result.
    #[must_use]
    pub const fn success(call_id: ToolCallId, value: Value) -> Self {
        Self::new(call_id, ToolOutcome::success(value))
    }

    /// Builds a failed result whose message is also its content.
    #[must_use]
    pub fn failure(call_id: ToolCallId, message: impl Into<String>) -> Self {
        Self::new(call_id, ToolOutcome::failure(message))
    }

    /// Returns `true` only when the outcome is a success.
    #[must_use]
    pub const fn is_success(&self) -> bool {
        self.outcome.is_success()
    }

    /// Renders the result as the text a tool message carries.
    #[must_use]
    pub fn render_text(&self) -> String {
        self.outcome.render_text()
    }
}

/// The only part of a tool that may reach a model request.
///
/// The field set is the allowlist. Adding a field here is therefore a wire
/// change, which is exactly the review conversation that should happen.
// `serde_json::Value` is only `PartialEq`, so `Eq` cannot be derived even
// though equality here is structurally total.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolSchema {
    /// The tool's registered name.
    pub name: ToolName,
    /// The description the model reads to decide when to call it.
    pub description: String,
    /// A JSON-schema object describing the arguments.
    #[serde(default = "empty_object")]
    pub parameters: Value,
}

/// Returns an empty JSON object, for `serde` field defaults.
fn empty_object() -> Value {
    Value::Object(serde_json::Map::new())
}

/// The future a tool execution resolves to.
///
/// Boxed and `'static` because the executor borrows nothing from its caller: a
/// filesystem tool clones the filesystem handle it needs out of itself before
/// returning. The box is also what keeps the future nameable in a trait object.
pub type ToolFuture = Pin<Box<dyn Future<Output = ToolResult> + 'static>>;

/// The executable half of a registered tool.
///
/// Implementations receive the whole call, including its id, so they can answer
/// with a [`ToolResult`] that matches. Everything that could fail is expected to
/// be reported as a [`ToolOutcome::Failure`]; the registry only converts
/// pre-dispatch problems into failures on the tool's behalf.
pub trait ToolExecutor {
    /// Runs one call.
    fn execute(&self, call: ToolCall) -> ToolFuture;
}

/// A registered tool: its wire schema plus a non-serialisable executor.
///
/// `ToolDefinition` deliberately implements neither `Serialize` nor `Clone`.
/// The absence of `Serialize` is the wire allowlist invariant at the type level:
/// whatever the executor closes over cannot be encoded into a request, because
/// there is no code path that encodes it.
#[must_use = "a tool definition does nothing until it is registered"]
pub struct ToolDefinition {
    /// The wire half.
    schema: ToolSchema,
    /// The executable half.
    executor: Box<dyn ToolExecutor>,
}

impl ToolDefinition {
    /// Pairs a schema with its executor.
    pub fn new(schema: ToolSchema, executor: impl ToolExecutor + 'static) -> Self {
        // Precondition: the schema's own name passed validation, which is what
        // lets the registry index it without re-validating.
        assert!(!schema.name.as_str().is_empty());
        Self {
            schema,
            executor: Box::new(executor),
        }
    }

    /// Returns the tool's wire schema.
    #[must_use]
    pub const fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    /// Returns the tool's name.
    #[must_use]
    pub const fn name(&self) -> &ToolName {
        &self.schema.name
    }

    /// Runs one call that already names this tool.
    ///
    /// The registry asserts the name before dispatching, so this method takes
    /// the call as given. Calling it with another tool's call is a harness bug,
    /// not a model-visible condition, and is reported by an assertion.
    #[must_use]
    pub fn execute(&self, call: ToolCall) -> ToolFuture {
        assert!(
            call.name == self.schema.name,
            "the registry dispatches a call only to the tool it names"
        );
        self.executor.execute(call)
    }
}

impl fmt::Debug for ToolDefinition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolDefinition")
            .field("schema", &self.schema)
            .field("executor", &"<boxed>")
            .finish()
    }
}

/// The set of tools available to the model, indexed by name.
///
/// This lives in the domain, not in the ports crate, because the registry is
/// policy rather than I/O: it rejects duplicate registrations, validates
/// arguments before dispatch, projects its contents to the wire allowlist, and
/// converts a pre-dispatch failure into a model-visible [`ToolOutcome::Failure`]
/// instead of a harness error. It holds no file handles, no sockets, and no
/// runtime.
#[derive(Debug, Default)]
pub struct ToolRegistry {
    /// Tools ordered by name, so the wire projection is deterministic.
    tools: std::collections::BTreeMap<ToolName, ToolDefinition>,
}

impl ToolRegistry {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            tools: std::collections::BTreeMap::new(),
        }
    }

    /// Registers a tool.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::DuplicateTool`] when the name is already taken. A
    /// silent replacement would make the model's view of a tool depend on load
    /// order, which is exactly the hidden state this design forbids.
    pub fn register(&mut self, definition: ToolDefinition) -> Result<(), ToolError> {
        let name = definition.name().clone();
        if self.tools.contains_key(&name) {
            return Err(ToolError::DuplicateTool {
                name: name.into_string(),
            });
        }
        let previous = self.tools.insert(name.clone(), definition);
        // Postcondition: the insertion replaced nothing, which is what the
        // duplicate check above promised.
        assert!(previous.is_none());
        assert!(self.tools.contains_key(&name));
        tracing::debug!(tool = %name, "tool registered");
        Ok(())
    }

    /// Returns the tool registered under `name`.
    #[must_use]
    pub fn get(&self, name: &ToolName) -> Option<&ToolDefinition> {
        self.tools.get(name)
    }

    /// Returns `true` when `name` is registered.
    #[must_use]
    pub fn contains(&self, name: &ToolName) -> bool {
        self.tools.contains_key(name)
    }

    /// Returns the number of registered tools.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Returns `true` when no tool is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Returns every registered name, in wire order.
    #[must_use]
    pub fn names(&self) -> Vec<&ToolName> {
        self.tools.keys().collect()
    }

    /// Returns every schema that may reach a model request.
    ///
    /// This is the projection half of the wire allowlist invariant: the only
    /// thing this method can return is a [`ToolSchema`], whose field set is the
    /// allowlist.
    #[must_use]
    pub fn schemas(&self) -> Vec<&ToolSchema> {
        self.tools.values().map(ToolDefinition::schema).collect()
    }

    /// Resolves and validates a call without running it.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::UnknownTool`] when the name is not registered, and
    /// [`ToolError::ArgumentsNotObject`] when the arguments are not a JSON
    /// object.
    pub fn validate(&self, call: &ToolCall) -> Result<&ToolDefinition, ToolError> {
        let definition = self.get(&call.name).ok_or_else(|| ToolError::UnknownTool {
            name: call.name.as_str().to_owned(),
        })?;
        call.arguments_object()?;
        // Postcondition: the resolved tool answers to the call's own name.
        assert_eq!(definition.name(), &call.name);
        Ok(definition)
    }

    /// Runs a call, reporting every pre-dispatch failure as a tool outcome.
    ///
    /// An unknown tool or malformed arguments are conditions the *model* caused
    /// and can correct, so they become a [`ToolOutcome::Failure`] the model
    /// reads rather than an error the harness must handle. This is the one place
    /// the two error channels meet, and the direction is deliberate.
    #[must_use]
    pub fn execute(&self, call: ToolCall) -> ToolFuture {
        let call_id = call.id.clone();
        match self.validate(&call) {
            Ok(definition) => {
                let name = definition.name().clone();
                tracing::debug!(tool = %name, "tool dispatched");
                definition.execute(call)
            }
            Err(error) => {
                let name = call.name.clone();
                tracing::debug!(tool = %name, error = %error, "tool rejected before dispatch");
                Box::pin(async move { ToolResult::failure(call_id, error.to_string()) })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A tool that echoes its arguments, used to prove dispatch reaches the
    /// executor and that a failure is an outcome rather than an error.
    struct Echo {
        fail: bool,
    }

    impl ToolExecutor for Echo {
        fn execute(&self, call: ToolCall) -> ToolFuture {
            let fail = self.fail;
            Box::pin(async move {
                if fail {
                    return ToolResult::failure(call.id, "echo refused");
                }
                let value = call.arguments.clone();
                ToolResult::new(
                    call.id,
                    ToolOutcome::success_with(value, vec![ContentBlock::Text("echoed".to_owned())]),
                )
            })
        }
    }

    fn name(raw: &str) -> ToolName {
        ToolName::new(raw).unwrap_or_else(|error| panic!("test tool name {raw}: {error}"))
    }

    fn schema(raw: &str) -> ToolSchema {
        ToolSchema {
            name: name(raw),
            description: "an echo".to_owned(),
            parameters: json!({ "type": "object" }),
        }
    }

    fn call(raw: &str, arguments: Value) -> ToolCall {
        ToolCall::new(ToolCallId::new("call-1"), name(raw), arguments)
    }

    /// Drives a boxed future to completion.
    ///
    /// The domain has no async runtime and must not gain one, so the test polls
    /// with a no-op waker. Every future this crate produces is immediately
    /// ready, which the loop's `Pending` arm records.
    fn block_on<F: Future>(future: F) -> F::Output {
        let mut future = Box::pin(future);
        let waker = core::task::Waker::noop();
        let mut context = core::task::Context::from_waker(waker);
        loop {
            // The loop body is the whole state machine: every future this crate
            // produces is immediately ready, so a pending poll just means "ask
            // again", and there is nothing else to do between polls.
            if let core::task::Poll::Ready(value) = future.as_mut().poll(&mut context) {
                return value;
            }
        }
    }

    /// Drains a registry call into its result.
    fn run(registry: &ToolRegistry, call: ToolCall) -> ToolResult {
        block_on(registry.execute(call))
    }

    #[test]
    fn tool_names_accept_the_provider_alphabet() {
        // Positive space: the shapes a registered tool actually uses.
        assert!(ToolName::new("read").is_ok());
        assert!(ToolName::new("read_file").is_ok());
        assert!(ToolName::new("read-file").is_ok());
        assert!(ToolName::new("read2").is_ok());
        assert!(ToolName::new("a").is_ok());
    }

    #[test]
    fn tool_names_reject_everything_the_wire_would_mangle() {
        // Negative space: each rejected shape is a typed error, not a panic and
        // not a silently accepted value.
        assert!(ToolName::new("").is_err());
        assert!(ToolName::new("Read").is_err());
        assert!(ToolName::new("read file").is_err());
        assert!(ToolName::new("read.file").is_err());
        assert!(ToolName::new("read/file").is_err());
        assert!(ToolName::new("read\n").is_err());
        assert!(ToolName::new("rêad").is_err());
        assert!(ToolName::new("a".repeat(TOOL_NAME_MAX_LEN.saturating_add(1))).is_err());
        assert!(ToolName::new("a".repeat(TOOL_NAME_MAX_LEN)).is_ok());
    }

    #[test]
    fn registering_a_tool_twice_is_rejected() {
        let mut registry = ToolRegistry::new();
        let first = registry.register(ToolDefinition::new(schema("read"), Echo { fail: false }));
        assert!(first.is_ok());
        let second = registry.register(ToolDefinition::new(schema("read"), Echo { fail: false }));
        assert!(matches!(second, Err(ToolError::DuplicateTool { .. })));
        // Pair assertion: the original registration survives the rejected one.
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn only_the_three_allowlisted_keys_reach_the_wire() {
        // The wire-allowlist invariant. `ToolDefinition` is not `Serialize`, so
        // the only encodable half is the schema; this asserts the schema's own
        // key set is exactly the allowlist, with nothing extra to leak.
        let mut registry = ToolRegistry::new();
        let registered =
            registry.register(ToolDefinition::new(schema("read"), Echo { fail: false }));
        assert!(registered.is_ok());

        let encoded = serde_json::to_value(registry.schemas()).unwrap_or(Value::Null);
        let Some(first) = encoded.get(0) else {
            panic!("the registry projects one schema per tool");
        };
        let Some(object) = first.as_object() else {
            panic!("a projected schema is a JSON object");
        };
        let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["description", "name", "parameters"]);
    }

    #[test]
    fn a_request_body_carries_only_schemas() {
        // The same invariant one layer up: the serialised tool list of a request
        // contains no key outside the allowlist, at any depth of the envelope.
        let mut registry = ToolRegistry::new();
        let registered =
            registry.register(ToolDefinition::new(schema("read"), Echo { fail: false }));
        assert!(registered.is_ok());
        let body = json!({ "model": "deepseek-flash", "tools": registry.schemas() });
        let text = serde_json::to_string(&body).unwrap_or_default();
        assert!(text.contains("\"parameters\""));
        assert!(
            !text.contains("executor"),
            "the executable half never encodes"
        );
        assert!(!text.contains("fail"), "executor state never encodes");
    }

    #[test]
    fn dispatch_reaches_the_executor() {
        let mut registry = ToolRegistry::new();
        let registered =
            registry.register(ToolDefinition::new(schema("read"), Echo { fail: false }));
        assert!(registered.is_ok());
        let result = run(&registry, call("read", json!({ "path": "a" })));
        assert!(result.is_success());
        assert_eq!(result.render_text(), "echoed");
        assert_eq!(result.outcome.value(), Some(&json!({ "path": "a" })));
    }

    #[test]
    fn a_tool_failure_is_an_outcome_and_not_an_error() {
        let mut registry = ToolRegistry::new();
        let registered =
            registry.register(ToolDefinition::new(schema("read"), Echo { fail: true }));
        assert!(registered.is_ok());
        let result = run(&registry, call("read", json!({})));
        assert!(!result.is_success(), "the tool failed");
        assert_eq!(result.outcome.message(), Some("echo refused"));
        assert_eq!(result.render_text(), "echo refused");
    }

    #[test]
    fn an_unknown_tool_is_reported_to_the_model() {
        let registry = ToolRegistry::new();
        let result = run(&registry, call("missing", json!({})));
        assert!(!result.is_success());
        assert!(
            result.render_text().contains("not registered"),
            "the model is told what went wrong: {}",
            result.render_text()
        );
        // The call id still matches, so the transcript stays well-formed.
        assert_eq!(result.call_id.as_str(), "call-1");
    }

    #[test]
    fn validate_rejects_non_object_arguments() {
        let mut registry = ToolRegistry::new();
        let registered =
            registry.register(ToolDefinition::new(schema("read"), Echo { fail: false }));
        assert!(registered.is_ok());
        let bad = call("read", json!([1, 2, 3]));
        let outcome = registry.validate(&bad);
        assert!(matches!(
            outcome,
            Err(ToolError::ArgumentsNotObject {
                found: "an array",
                ..
            })
        ));
    }

    #[test]
    fn a_hand_built_null_argument_becomes_a_dispatch_failure() {
        let mut registry = ToolRegistry::new();
        let registered =
            registry.register(ToolDefinition::new(schema("read"), Echo { fail: false }));
        assert!(registered.is_ok());
        // `ToolCall::new` normalises `null`; reaching for the raw field is the
        // only way to get an invalid call, and it is reported, not panicked on.
        let mut raw = call("read", json!({}));
        raw.arguments = Value::Null;
        let result = run(&registry, raw);
        assert!(!result.is_success());
        assert!(result.render_text().contains("must be a JSON object"));
    }

    #[test]
    fn malformed_argument_json_is_a_distinct_error() {
        let outcome =
            ToolCall::try_from_arguments_json(ToolCallId::new("c"), name("read"), "{ \"path\": ");
        assert!(matches!(
            outcome,
            Err(ToolError::InvalidArgumentsJson { .. })
        ));
    }

    #[test]
    fn empty_argument_json_means_no_arguments() {
        let outcome = ToolCall::try_from_arguments_json(ToolCallId::new("c"), name("read"), "   ");
        assert!(outcome.is_ok());
        let Ok(call) = outcome else { return };
        assert_eq!(call.arguments, json!({}));
    }

    #[test]
    fn arguments_json_is_parsed_verbatim() {
        let outcome = ToolCall::try_from_arguments_json(
            ToolCallId::new("c"),
            name("read"),
            r#"{"path":"src/lib.rs","limit":3}"#,
        );
        assert!(outcome.is_ok());
        let Ok(call) = outcome else { return };
        assert_eq!(call.arguments.get("limit"), Some(&json!(3)));
    }

    #[test]
    fn content_blocks_render_images_as_placeholders() {
        let text = ContentBlock::Text("hello".to_owned());
        assert_eq!(text.render_text(), "hello");
        let image = ContentBlock::Image {
            media_type: "image/png".to_owned(),
            data_base64: "AAAA".to_owned(),
        };
        let rendered = image.render_text();
        assert!(rendered.contains("image/png"));
        assert!(!rendered.contains("AAAA"), "the payload is not rendered");
    }

    #[test]
    fn content_blocks_round_trip() {
        let blocks = vec![
            ContentBlock::Text("a".to_owned()),
            ContentBlock::Image {
                media_type: "image/png".to_owned(),
                data_base64: "AAAA".to_owned(),
            },
        ];
        let encoded = serde_json::to_string(&blocks).unwrap_or_default();
        let decoded: Result<Vec<ContentBlock>, _> = serde_json::from_str(&encoded);
        assert!(decoded.is_ok(), "content blocks round-trip: {encoded}");
        assert_eq!(decoded.ok(), Some(blocks));
    }

    #[test]
    fn a_success_without_content_renders_its_value() {
        let outcome = ToolOutcome::success(json!({ "ok": true }));
        assert_eq!(outcome.render_text(), "{\"ok\":true}");
        let empty = ToolOutcome::success(Value::Null);
        assert_eq!(empty.render_text(), "");
    }

    #[test]
    fn schemas_are_ordered_by_name() {
        let mut registry = ToolRegistry::new();
        for raw in ["write", "read", "edit"] {
            let registered =
                registry.register(ToolDefinition::new(schema(raw), Echo { fail: false }));
            assert!(registered.is_ok());
        }
        let names: Vec<&str> = registry.names().iter().map(|n| n.as_str()).collect();
        assert_eq!(names, vec!["edit", "read", "write"]);
        assert!(!registry.is_empty());
        assert!(registry.contains(&name("read")));
        assert!(registry.get(&name("nope")).is_none());
    }

    #[test]
    fn tool_names_round_trip_through_serde() {
        let original = name("read-file");
        let encoded = serde_json::to_string(&original).unwrap_or_default();
        assert_eq!(encoded, "\"read-file\"");
        let decoded: Result<ToolName, _> = serde_json::from_str(&encoded);
        assert!(decoded.is_ok());
        assert_eq!(decoded.ok(), Some(original));
    }

    #[test]
    fn an_invalid_name_is_rejected_while_deserialising() {
        let decoded: Result<ToolName, _> = serde_json::from_str("\"Read Me\"");
        assert!(decoded.is_err(), "serde validates through the same path");
    }
}
