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
    /// Original trusted instructions in explicit revision mode; absent for legacy receipts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<Box<ReplayInstructions>>,
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
        if let Some(instructions) = &self.instructions {
            instructions.validate()?;
        }
        Ok(())
    }
}

/// Bounded original System texts, captured by request preparation rather than model output.
/// The adapter checks the revision digest and original control binding before replay.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayInstructions {
    /// Snapshot/source-hash format. Only version 1 is supported.
    pub version: u8,
    /// Opaque BLAKE3 identity of the ordered text array, including repeated/empty texts.
    pub revision: String,
    /// Original leading System texts in their original order.
    pub messages: Vec<String>,
}

impl ReplayInstructions {
    /// Largest number of leading System texts in a revision-aware request.
    pub const MESSAGES_MAX: usize = 64;
    /// Largest complete serialized snapshot, including JSON escaping and metadata.
    pub const BYTES_MAX: usize = 256 * 1024;

    /// Checks bounded shape only; source/control/revision digests belong to the adapter.
    /// # Errors
    /// Refuses unknown versions, malformed digests and count/serialized-byte overflow.
    pub fn validate(&self) -> Result<(), crate::content::ContentError> {
        if self.version != 1 || !digest(&self.revision) || self.messages.len() > Self::MESSAGES_MAX
        {
            return Err(crate::content::ContentError::new(
                "invalid instruction snapshot",
            ));
        }
        crate::content::serialized_size(self, Self::BYTES_MAX)?;
        Ok(())
    }
}
