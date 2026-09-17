//! Adapter errors.
//!
//! The core never sees a `reqwest::Error`: every vendor failure is translated here
//! so the domain depends on vocabulary it owns.

use crate::config::Vendor;

/// Failures this adapter can report.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum OpenAiError {
    /// The API key is unset or empty.
    #[error("no {provider} API key: set {env} or run `nanus auth set {provider}`")]
    MissingCredential {
        /// The provider whose key is missing.
        provider: &'static str,
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
        /// The host the request was sent to, as the client was configured.
        host: String,
        /// The underlying failure, rendered.
        message: String,
    },

    /// The service answered with a non-success status.
    #[error("{provider} returned HTTP {status}: {body}")]
    Status {
        /// The provider that answered.
        provider: &'static str,
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

impl OpenAiError {
    /// Builds an [`OpenAiError::InvalidConfig`].
    pub(crate) const fn invalid_config(field: &'static str, reason: &'static str) -> Self {
        Self::InvalidConfig { field, reason }
    }

    /// Builds an [`OpenAiError::MissingCredential`].
    #[must_use]
    pub const fn missing_credential(vendor: Vendor) -> Self {
        Self::MissingCredential {
            provider: vendor.as_str(),
            env: vendor.env_var(),
        }
    }

    /// Builds an [`OpenAiError::Client`].
    pub(crate) fn client(source: &reqwest::Error) -> Self {
        Self::Client {
            message: source.to_string(),
        }
    }

    /// Builds an [`OpenAiError::Transport`].
    ///
    /// The host is the one the client was configured with rather than the default,
    /// because "the request to the vendor failed" is misleading advice when the
    /// request went to a local proxy.
    pub(crate) fn transport(source: &reqwest::Error, host: &str) -> Self {
        Self::Transport {
            host: host.to_owned(),
            message: source.to_string(),
        }
    }

    /// Builds an [`OpenAiError::Status`].
    pub(crate) const fn status(vendor: Vendor, status: u16, body: String) -> Self {
        Self::Status {
            provider: vendor.as_str(),
            status,
            body,
        }
    }

    /// Returns `true` when retrying the same request could plausibly succeed.
    ///
    /// A malformed frame or a bad key fails identically on a retry; a rate limit or
    /// a server fault may not.
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retryability_classifies_by_cause() {
        assert!(OpenAiError::status(Vendor::OpenAi, 429, String::new()).is_retryable());
        assert!(OpenAiError::status(Vendor::Zai, 503, String::new()).is_retryable());
        assert!(!OpenAiError::status(Vendor::OpenAi, 400, String::new()).is_retryable());
        assert!(!OpenAiError::status(Vendor::OpenAi, 401, String::new()).is_retryable());
        assert!(!OpenAiError::missing_credential(Vendor::Zai).is_retryable());
    }

    /// A missing credential names both ways to supply one, because either may be
    /// the one a reader can act on.
    #[test]
    fn a_missing_credential_names_the_variable_and_the_command() {
        let error = OpenAiError::missing_credential(Vendor::OpenAi);
        let rendered = error.to_string();
        assert!(rendered.contains("OPENAI_API_KEY"), "{rendered}");
        assert!(rendered.contains("nanus auth set openai"), "{rendered}");

        let zai = OpenAiError::missing_credential(Vendor::Zai).to_string();
        assert!(zai.contains("ZAI_API_KEY"), "{zai}");
    }

    #[test]
    fn a_status_error_names_the_provider_that_answered() {
        let error = OpenAiError::status(Vendor::Zai, 429, String::from("slow down"));
        let rendered = error.to_string();
        assert!(rendered.contains("zai"), "{rendered}");
        assert!(rendered.contains("429"), "{rendered}");
        assert!(rendered.contains("slow down"), "{rendered}");
    }
}
