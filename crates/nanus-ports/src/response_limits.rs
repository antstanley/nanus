//! Caller-selected response budgets. Stock callers opt out by leaving limits absent.
use crate::{LlmError, LlmResult};

/// Largest indexed tool-call namespace the API accumulators support.
pub const TOOL_CALL_SLOTS_MAX: usize = 256;

/// Immutable logical byte/count budgets, selected before a provider request begins.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResponseLimits {
    line_bytes: usize,
    event_bytes: usize,
    response_bytes: usize,
    events: usize,
    tool_slots: usize,
    error_body_bytes: usize,
}
impl ResponseLimits {
    /// All budgets are positive; line/event/error bytes cannot exceed the whole response.
    /// Limits count logical bytes, not TLS, allocator or HTTP-client memory overhead.
    /// # Errors
    /// Refuses inconsistent budgets and tool namespaces larger than 256 slots.
    pub fn new(
        line_bytes: usize,
        event_bytes: usize,
        response_bytes: usize,
        events: usize,
        tool_slots: usize,
        error_body_bytes: usize,
    ) -> LlmResult<Self> {
        if [
            line_bytes,
            event_bytes,
            response_bytes,
            events,
            tool_slots,
            error_body_bytes,
        ]
        .contains(&0)
            || line_bytes > response_bytes
            || event_bytes > response_bytes
            || error_body_bytes > response_bytes
            || tool_slots > TOOL_CALL_SLOTS_MAX
        {
            return Err(LlmError::Unsupported {
                feature: "invalid response limits".into(),
            });
        }
        Ok(Self {
            line_bytes,
            event_bytes,
            response_bytes,
            events,
            tool_slots,
            error_body_bytes,
        })
    }
    /// Largest partial SSE line, excluding its newline.
    #[must_use]
    pub const fn line_bytes(self) -> usize {
        self.line_bytes
    }
    /// Largest JSON payload or assembled tool-call content.
    #[must_use]
    pub const fn event_bytes(self) -> usize {
        self.event_bytes
    }
    /// Total received response bytes, including comments and framing.
    #[must_use]
    pub const fn response_bytes(self) -> usize {
        self.response_bytes
    }
    /// Maximum data payloads, including unknown protocol payloads, excluding the sentinel.
    #[must_use]
    pub const fn events(self) -> usize {
        self.events
    }
    /// Maximum indexed namespace and count of retained tool calls/blocks.
    #[must_use]
    pub const fn tool_slots(self) -> usize {
        self.tool_slots
    }
    /// Maximum non-success body bytes retained before UTF-8 decoding.
    #[must_use]
    pub const fn error_body_bytes(self) -> usize {
        self.error_body_bytes
    }

    /// Checks a prospective buffer length before any copy or extension.
    /// # Errors
    /// Returns a typed response-limit error on arithmetic overflow or budget exhaustion.
    pub fn add(
        resource: &'static str,
        used: usize,
        added: usize,
        limit: usize,
    ) -> LlmResult<usize> {
        used.checked_add(added)
            .filter(|size| *size <= limit)
            .ok_or(LlmError::ResponseLimit { resource, limit })
    }
    /// Rejects an indexed call before it can resize an accumulator vector.
    /// # Errors
    /// Returns a response-limit error for an index outside the configured namespace.
    pub fn index(self, index: u64) -> LlmResult<usize> {
        usize::try_from(index)
            .ok()
            .filter(|index| *index < self.tool_slots)
            .ok_or(LlmError::ResponseLimit {
                resource: "tool-call slots",
                limit: self.tool_slots,
            })
    }
    /// Bounds a tool call's combined id, name and prospective arguments before copying them.
    /// # Errors
    /// Returns a response-limit error for oversized content or arithmetic overflow.
    pub fn call_bytes(self, id: &str, name: &str, arguments: usize) -> LlmResult<()> {
        let bytes = Self::add("tool-call bytes", id.len(), name.len(), self.event_bytes)?;
        Self::add("tool-call bytes", bytes, arguments, self.event_bytes)?;
        Ok(())
    }
    /// Counts a parsed frame without allocating a second encoding.
    /// # Errors
    /// Refuses an oversized frame before the accumulator clones any of its fields.
    pub fn frame(self, value: &serde_json::Value) -> LlmResult<()> {
        nanus_domain::content::serialized_size(value, self.event_bytes)
            .map(|_| ())
            .map_err(|_| LlmError::ResponseLimit {
                resource: "decoded event bytes",
                limit: self.event_bytes,
            })
    }
}
