//! # nanus-domain
//!
//! The pure domain of the nanus agent harness. Nothing here performs I/O: there
//! is no filesystem, no network, no clock, and no async runtime. That is not an
//! aesthetic preference — it is what lets the whole agent loop be tested against
//! a log, replayed from a file, and reasoned about without a mock server.
//!
//! The crate owns six things, and they are deliberately separate:
//!
//! - [`message`] — the model conversation vocabulary, in the `OpenAI` wire shape
//!   the `DeepSeek` adapter sends, including the rule that an assistant turn with
//!   no text serialises `content` as `""` rather than `null`.
//! - [`tool`] — the model-facing tool contract, and the **wire allowlist**: only
//!   a tool's `name`, `description`, and `parameters` can be encoded into a
//!   request, enforced by giving the executable half no `Serialize`.
//! - [`session`] — the append-only event log, the only source of model history,
//!   with a JSONL framing that detects truncation and sequence holes.
//! - [`prompt`] — ordered, named prompt sections with explicit variables, where
//!   an unresolved reference is an error rather than a silent empty string.
//! - [`approval`] — the two orthogonal permission knobs, fail-closed.
//! - [`agent`] — the pure turn/step state machine, with a step budget that `dsh`
//!   does not have.
//!
//! ## The shape of a session
//!
//! ```
//! use nanus_domain::{Message, Role, Session, SessionEvent, SessionId, TurnEndReason};
//!
//! let mut session = Session::new(SessionId::new("demo"), 0, "/work");
//! session.append(SessionEvent::TurnStart { turn: 0 });
//! session.append(SessionEvent::UserMessage {
//!     text: "what is in src/lib.rs?".to_owned(),
//! });
//! session.append(SessionEvent::TurnEnd {
//!     turn: 0,
//!     reason: TurnEndReason::Completed,
//! });
//!
//! // The log is the only history, so the message list is a fold over it.
//! let messages = session.derive_messages();
//! assert_eq!(messages.len(), 1);
//! assert_eq!(messages.first().map(Message::role), Some(Role::User));
//!
//! // And the whole session round-trips through its own file format.
//! let restored = Session::from_jsonl(&session.to_jsonl());
//! assert_eq!(restored.ok(), Some(session));
//! ```
//!
//! ## Style
//!
//! This crate follows Tiger Style: no `unsafe`, no panicking accessors in
//! production paths, assertions wherever a real invariant exists, a hard limit
//! of 70 lines and 100 columns per function, and explicit fixed-width integers
//! for domain values.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
// A `pub` item inside a private module is reachable only through this crate's own
// re-exports. The lint cannot tell that from an orphaned item, and every such item
// here is deliberate.
#![allow(unreachable_pub)]
// The *total* relatives of the unwrap family cannot panic, and are how this
// crate states a fallback, so they are allowed by name rather than by weakening
// the blanket lint.
#![allow(
    clippy::unwrap_or_default,
    clippy::manual_unwrap_or,
    clippy::manual_unwrap_or_default
)]

pub mod agent;
pub mod approval;
pub mod context;
pub mod error;
pub mod message;
pub mod prompt;
pub mod session;
pub mod tool;

pub use agent::{
    AgentConfig, DEFAULT_CONTEXT_BUDGET, DEFAULT_MAX_PARALLEL_TOOLS, DEFAULT_MAX_STEPS_PER_TURN,
    DEFAULT_SYSTEM_PROMPT_MAX, StepOutcome, TurnMachine, TurnOutcome,
};
pub use approval::{
    ApprovalOutcome, ApprovalPolicy, ApprovalRequest, PermissionPreset, PresetName, SandboxMode,
    ToolAccess,
};
pub use context::{Elision, FitError, Fitted, estimate, fit};
pub use error::{DomainError, DomainResult};
pub use message::{Message, Role, ToolCallId, Usage};
pub use prompt::{PromptBuilder, PromptError, PromptSection, runtime_context};
pub use session::{
    Origin, SESSION_FORMAT_TAG, SESSION_FORMAT_VERSION, Session, SessionError, SessionEvent,
    SessionId, SessionLog, SessionSeq, TurnEndReason,
};
pub use tool::{
    ContentBlock, TOOL_NAME_MAX_LEN, ToolCall, ToolDefinition, ToolError, ToolExecutor, ToolFuture,
    ToolName, ToolOutcome, ToolRegistry, ToolResult, ToolSchema,
};
