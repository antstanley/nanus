//! Adapter errors.
//!
//! The core never sees a `reqwest::Error`: every vendor failure is translated here
//! so the domain depends on vocabulary it owns.

use crate::config::{API_KEY_ENV, DEFAULT_BASE_URL};

/// Failures the `DeepSeek` adapter can report.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DeepSeekError {
    /// The API key is unset or empty.
    #[error("no DeepSeek API key: set {env}")]
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

    /// The HTTP client could not be constructed.
    #[error("could not build the HTTP client: {message}")]
    Client {
        /// The platform failure, rendered.
        message: String,
    },

    /// The request did not complete.
    #[error("transport failure reaching {host}: {message}")]
    Transport {
        /// The host that was unreachable.
        host: &'static str,
        /// The underlying failure, rendered.
        message: String,
    },

    /// The service answered with a non-success status.
    #[error("DeepSeek returned HTTP {status}: {body}")]
    Status {
        /// The numeric status code.
        status: u16,
        /// A truncated body, carrying `DeepSeek`'s own message.
        body: String,
    },

    /// A server-sent-events frame could not be decoded.
    #[error("malformed stream frame: {detail}")]
    MalformedFrame {
        /// What was wrong with the frame.
        detail: String,
    },
}

impl DeepSeekError {
    /// Builds an [`DeepSeekError::InvalidConfig`].
    pub(crate) fn invalid_config(field: &'static str, reason: &'static str) -> Self {
        Self::InvalidConfig { field, reason }
    }

    /// Builds a [`DeepSeekError::Client`] from a `reqwest` construction failure.
    pub(crate) fn client(source: &reqwest::Error) -> Self {
        Self::Client {
            message: source.to_string(),
        }
    }

    /// Builds a [`DeepSeekError::Transport`] from a `reqwest` request failure.
    pub(crate) fn transport(source: &reqwest::Error) -> Self {
        Self::Transport {
            host: DEFAULT_BASE_URL,
            message: source.to_string(),
        }
    }

    /// Builds a [`DeepSeekError::Status`].
    pub(crate) fn status(status: u16, body: String) -> Self {
        Self::Status { status, body }
    }

    /// Returns `true` when retrying the same request could plausibly succeed.
    ///
    /// This is the classification the agent loop needs: a malformed frame or a bad
    /// key will fail identically on a retry, a rate limit or a server fault may not.
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        match self {
            Self::Status { status, .. } => matches!(*status, 429 | 500 | 502 | 503 | 504),
            Self::Transport { .. } => true,
            Self::MalformedFrame { .. }
            | Self::MissingCredential { .. }
            | Self::InvalidConfig { .. }
            | Self::Client { .. } => false,
        }
    }

    /// Returns the credential variable name, for a caller that wants to render
    /// setup guidance.
    #[must_use]
    pub const fn credential_variable() -> &'static str {
        API_KEY_ENV
    }

    /// Builds a [`DeepSeekError::MissingCredential`] for the configured variable.
    #[must_use]
    pub const fn missing_credential() -> Self {
        Self::MissingCredential { env: API_KEY_ENV }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retryability_classifies_by_cause() {
        // Retryable: a rate limit and a server fault may pass on a second attempt.
        assert!(DeepSeekError::status(429, String::new()).is_retryable());
        assert!(DeepSeekError::status(503, String::new()).is_retryable());
        // Not retryable: a bad request or a bad key will fail identically.
        assert!(!DeepSeekError::status(400, String::new()).is_retryable());
        assert!(!DeepSeekError::status(401, String::new()).is_retryable());
        assert!(!DeepSeekError::missing_credential().is_retryable());
        assert!(
            !DeepSeekError::MalformedFrame {
                detail: "bad json".to_owned()
            }
            .is_retryable()
        );
    }

    #[test]
    fn messages_name_the_thing_that_is_wrong() {
        let missing = DeepSeekError::MissingCredential { env: API_KEY_ENV };
        assert!(missing.to_string().contains(API_KEY_ENV));

        let invalid = DeepSeekError::invalid_config("max_tokens", "must be greater than zero");
        assert!(invalid.to_string().contains("max_tokens"));
        assert!(invalid.to_string().contains("greater than zero"));
    }

    #[test]
    fn the_credential_variable_is_stable() {
        assert_eq!(DeepSeekError::credential_variable(), "DEEPSEEK_API_KEY");
    }
}
