//! Local tool-capability metadata, independent of image support and token ceilings.

use nanus_domain::Message;

use crate::{ChatRequest, LlmError, LlmResult};

/// Whether the exact model/wire/endpoint/effective-effort combination can call tools.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ToolCallSupport {
    /// Exact provider contracts and actual wire fixtures establish ordinary function tools.
    Supported,
    /// This exact combination explicitly cannot accept ordinary function tools.
    Unsupported,
    /// No verified entry exists; stock behavior is retained, without claiming support.
    #[default]
    Unknown,
}

/// Detects definitions and retained calls/results, even when the current tool list is empty.
#[must_use]
pub fn has_tool_context(request: &ChatRequest) -> bool {
    !request.tools.is_empty()
        || request.messages.iter().any(|message| match message {
            Message::Assistant { tool_calls, .. } => !tool_calls.is_empty(),
            Message::Tool { .. } => true,
            Message::System { .. } | Message::User { .. } => false,
        })
}

/// Rejects known unsupported tool-bearing requests before encoding or HTTP.
///
/// Unknown is not promoted to Supported; stricter embedding hosts refuse it themselves.
/// Text-only requests retain their existing behavior.
/// # Errors
/// Returns an unsupported error for definitions or replay on a known unsupported combination.
pub fn validate_input(support: ToolCallSupport, request: &ChatRequest) -> LlmResult<()> {
    if support == ToolCallSupport::Unsupported && has_tool_context(request) {
        return Err(LlmError::Unsupported {
            feature: "tool calling is unsupported for the selected model, wire and effort".into(),
        });
    }
    Ok(())
}

#[cfg(test)]
#[path = "tool_support_tests.rs"]
mod tests;
