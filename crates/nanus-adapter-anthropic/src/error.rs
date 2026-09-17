//! Adapter errors.
//!
//! The core never sees a `reqwest::Error`: every vendor failure is translated here
//! so the domain depends on vocabulary it owns.

/// Failures the `Anthropic` adapter can report.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AnthropicError {
    /// The API key is unset or empty.
    #[error("no Anthropic API key: set {env} or run `nanus auth set anthropic`")]
    MissingCredential {
        /// The environment variable that was consulted.
        env: &'static str,
    },

    /// A configuration field holds a value the adapter cannot use.
    #[error("invalid {field}: {reason}")]
    InvalidConfig {
        /// The offending field.
        field: &'static str,
        /// Why the value was rejected.
        reason: &'static str,
    },

    /// The conversation cannot be expressed in the Messages API.
    ///
    /// The two shapes are close but not identical — a system turn is top-level
    /// here, and a tool result is a *user* turn — so a conversation that cannot be
    /// translated is reported rather than sent as something that means a different
    /// thing.
    #[error("this conversation cannot be sent to Anthropic: {reason}")]
    Unsupported {
        /// What could not be expressed.
        reason: String,
    },

    /// The HTTP client could not be constructed.
    #[error("could not build the HTTP client: {message}")]
    Client {
        /// The platform failure, rendered.
        message: String,
    },

    /// The request did not complete.
    #[error("transport failure reaching {host}: {message}")]
    Transport {
        /// The host the request was sent to, as the client was configured.
        host: String,
        /// The underlying failure, rendered.
        message: String,
    },

    /// The service answered with a non-success status.
    #[error("Anthropic returned HTTP {status}: {body}")]
    Status {
        /// The numeric status code.
        status: u16,
        /// A truncated body, carrying the provider's own message.
        body: String,
    },

    /// A server-sent-events frame could not be decoded.
    #[error("malformed stream frame: {detail}")]
    MalformedFrame {
        /// What was wrong with the frame.
        detail: String,
    },
}

impl AnthropicError {
    /// Builds an [`AnthropicError::InvalidConfig`].
    pub(crate) const fn invalid_config(field: &'static str, reason: &'static str) -> Self {
        Self::InvalidConfig { field, reason }
    }

    /// Builds an [`AnthropicError::MissingCredential`].
    #[must_use]
    pub const fn missing_credential(env: &'static str) -> Self {
        Self::MissingCredential { env }
    }

    /// Builds an [`AnthropicError::Unsupported`].
    #[must_use]
    pub fn unsupported(reason: impl Into<String>) -> Self {
        Self::Unsupported {
            reason: reason.into(),
        }
    }

    /// Builds an [`AnthropicError::Client`].
    pub(crate) fn client(source: &reqwest::Error) -> Self {
        Self::Client {
            message: source.to_string(),
        }
    }

    /// Builds an [`AnthropicError::Transport`].
    pub(crate) fn transport(source: &reqwest::Error, host: &str) -> Self {
        Self::Transport {
            host: host.to_owned(),
            message: source.to_string(),
        }
    }

    /// Builds an [`AnthropicError::Status`].
    pub(crate) const fn status(status: u16, body: String) -> Self {
        Self::Status { status, body }
    }

    /// Returns `true` when retrying the same request could plausibly succeed.
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        match self {
            Self::Status { status, .. } => matches!(*status, 429 | 500 | 502 | 503 | 504 | 529),
            Self::Transport { .. } => true,
            Self::MalformedFrame { .. }
            | Self::MissingCredential { .. }
            | Self::InvalidConfig { .. }
            | Self::Unsupported { .. }
            | Self::Client { .. } => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retryability_classifies_by_cause() {
        // 529 is Anthropic's "overloaded": a second attempt may pass.
        assert!(AnthropicError::status(529, String::new()).is_retryable());
        assert!(AnthropicError::status(429, String::new()).is_retryable());
        assert!(!AnthropicError::status(400, String::new()).is_retryable());
        assert!(!AnthropicError::missing_credential("ANTHROPIC_API_KEY").is_retryable());
        // A conversation that cannot be expressed will not become expressible.
        assert!(!AnthropicError::unsupported("no messages").is_retryable());
    }

    #[test]
    fn a_missing_credential_names_the_variable_and_the_command() {
        let rendered = AnthropicError::missing_credential("ANTHROPIC_API_KEY").to_string();
        assert!(rendered.contains("ANTHROPIC_API_KEY"), "{rendered}");
        assert!(rendered.contains("nanus auth set anthropic"), "{rendered}");
    }
}
