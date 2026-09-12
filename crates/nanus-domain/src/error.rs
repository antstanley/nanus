//! The crate's error boundary.
//!
//! Each module owns the failures it can produce, and [`DomainError`] is the
//! single type a caller at the crate boundary handles. The module errors stay
//! concrete rather than being flattened into one enum, because a caller that
//! renders a prompt does not want to match on filesystem-shaped variants — but
//! the `From` impls below mean a caller that only cares about "it failed" can
//! take a `DomainError` from anything without a conversion at the call site.
//!
//! **The domain never sees a third-party error type.** Every `serde_json`
//! failure is rendered to a string at the module that caught it, so no public
//! signature names `serde_json::Error`, and an adapter cannot accidentally make
//! the domain depend on a particular serialiser.

use crate::prompt::PromptError;
use crate::session::SessionError;
use crate::tool::ToolError;

/// A failure at the domain's boundary.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum DomainError {
    /// A tool interaction failed before or during execution.
    #[error(transparent)]
    Tool(#[from] ToolError),

    /// A session could not be read or written.
    #[error(transparent)]
    Session(#[from] SessionError),

    /// A prompt could not be assembled.
    #[error(transparent)]
    Prompt(#[from] PromptError),

    /// A configured or caller-supplied value failed validation.
    #[error("invalid value for {field}: {reason}")]
    Validation {
        /// The field or parameter that was rejected.
        field: &'static str,
        /// Why it was rejected.
        reason: String,
    },
}

impl DomainError {
    /// Builds a validation failure for `field`.
    #[must_use]
    pub fn validation(field: &'static str, reason: impl Into<String>) -> Self {
        Self::Validation {
            field,
            reason: reason.into(),
        }
    }

    /// Returns the field name for a validation failure.
    #[must_use]
    pub const fn field(&self) -> Option<&'static str> {
        match self {
            Self::Validation { field, .. } => Some(field),
            Self::Tool(_) | Self::Session(_) | Self::Prompt(_) => None,
        }
    }
}

/// The result type the domain's cross-module entry points use.
pub type DomainResult<T> = Result<T, DomainError>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolName;

    #[test]
    fn a_tool_failure_converts_at_the_boundary() {
        let invalid = ToolName::new("Not Valid");
        assert!(invalid.is_err());
        let Err(tool_error) = invalid else { return };
        let domain: DomainError = tool_error.into();
        assert!(matches!(domain, DomainError::Tool(_)));
        // The message is preserved through the transparent conversion, so a
        // caller that only renders the error still says something useful.
        assert!(domain.to_string().contains("invalid tool name"));
    }

    #[test]
    fn a_validation_failure_names_its_field() {
        let error = DomainError::validation("model", "a turn needs a model");
        assert_eq!(error.field(), Some("model"));
        assert!(error.to_string().contains("model"));
        assert!(error.to_string().contains("a turn needs a model"));
    }

    #[test]
    fn a_session_failure_converts_at_the_boundary() {
        let decoded = crate::Session::from_jsonl("");
        assert!(decoded.is_err());
        let Err(session_error) = decoded else { return };
        let domain: DomainError = session_error.into();
        assert!(matches!(domain, DomainError::Session(_)));
        assert_eq!(domain.field(), None);
    }

    #[test]
    fn the_result_alias_carries_the_crate_error() {
        let ok: DomainResult<u32> = Ok(1);
        assert_eq!(ok.ok(), Some(1));
        let err: DomainResult<u32> = Err(DomainError::validation("x", "y"));
        assert!(err.is_err());
    }
}
