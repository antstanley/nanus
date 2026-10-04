//! Bounded consistency receipts for an original source and its dispatched fitted request.
use serde::{Deserialize, Serialize};

/// Fitting evidence retained beside an opaque Responses observation; no source or I/O authority.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayContext {
    /// Original fitting budget in estimated tokens.
    pub budget: u32,
    /// Complete oldest human turns omitted by that request.
    pub dropped_turns: u32,
    /// Original model-visible messages those turns contained.
    pub dropped_messages: u32,
    /// Digest of the original unelided neutral/replay prefix.
    pub source_digest: String,
    /// Digest of the exact body admitted for HTTP dispatch.
    pub wire_digest: String,
}

fn digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

impl ReplayContext {
    /// Checks shape only; the provider adapter verifies source/control consistency.
    /// # Errors
    /// Refuses impossible fitting counts, zero budget or malformed digests.
    pub fn validate(&self) -> Result<(), crate::content::ContentError> {
        if self.budget == 0
            || self.dropped_turns > self.dropped_messages
            || (self.dropped_turns == 0) != (self.dropped_messages == 0)
            || !digest(&self.source_digest)
            || !digest(&self.wire_digest)
        {
            return Err(crate::content::ContentError::new(
                "invalid replay context receipt",
            ));
        }
        Ok(())
    }
}
