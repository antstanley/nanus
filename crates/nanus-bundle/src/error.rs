//! The bundle's boundary error.
//!
//! One enum, because a bundle's failures are about *composition*: a configuration it
//! cannot use, a port that failed, or a model that refused. Each variant carries
//! enough context to act, and none carries a vendor type — the adapters have already
//! translated those into the ports vocabulary.

/// A failure while assembling or running a harness.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BundleError {
    /// The configuration is unusable.
    #[error("invalid configuration: {0}")]
    Config(String),

    /// No credential is stored for the selected provider.
    ///
    /// Discrete from [`BundleError::Config`] because it is the one configuration failure an
    /// agent may *start* through: an interface that opens without a credential lets the reader
    /// configure one, so the composition substitutes a placeholder adapter and reports this to
    /// it rather than refusing to start. Every other configuration failure still refuses.
    #[error("{0}")]
    Credential(String),

    /// A required service was not published.
    ///
    /// This is a composition mistake rather than a runtime condition: the kernel
    /// keeps a plugin pending until its requirements appear, so reaching this means a
    /// caller asked for work that no mounted plugin can do.
    #[error("the required service {name:?} is not available")]
    MissingService {
        /// The key name that was not published.
        name: &'static str,
    },

    /// The model stream reported a failure.
    #[error("the model failed: {0}")]
    Model(String),

    /// A tool could not be dispatched at all.
    ///
    /// Distinct from a tool that ran and failed: that is a model-visible outcome, not
    /// a harness error.
    #[error("the tool {name} could not be dispatched: {message}")]
    Tool {
        /// The tool that could not be dispatched.
        name: String,
        /// The rendered reason.
        message: String,
    },

    /// The conversation does not fit the prompt budget.
    ///
    /// Discrete from [`BundleError::Model`]: the model was never asked. The harness refused to
    /// send a prompt that could not be made to fit, which is a statement about the session and
    /// the configured budget rather than about the provider.
    #[error("the conversation does not fit the context budget: {0}")]
    Context(String),

    /// A session could not be read or written.
    #[error("session storage failed: {0}")]
    Session(String),

    /// The kernel refused a composition step.
    #[error("composition failed: {0}")]
    Kernel(String),
}

impl BundleError {
    /// Builds a [`BundleError::Config`].
    pub fn config(message: impl Into<String>) -> Self {
        Self::Config(message.into())
    }

    /// Builds a [`BundleError::Credential`].
    pub fn credential(message: impl Into<String>) -> Self {
        Self::Credential(message.into())
    }

    /// Builds a [`BundleError::Model`].
    pub fn model(message: impl Into<String>) -> Self {
        Self::Model(message.into())
    }

    /// Builds a [`BundleError::Context`].
    #[must_use]
    pub fn context(message: impl Into<String>) -> Self {
        Self::Context(message.into())
    }

    /// Builds a [`BundleError::Session`].
    pub fn session(message: impl Into<String>) -> Self {
        Self::Session(message.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_service_names_the_key() {
        let error = BundleError::MissingService { name: "llm" };
        assert!(error.to_string().contains("llm"));
    }

    #[test]
    fn a_tool_failure_names_the_tool() {
        let error = BundleError::Tool {
            name: "bash".to_owned(),
            message: "the sandbox is unavailable".to_owned(),
        };
        let rendered = error.to_string();
        assert!(rendered.contains("bash"));
        assert!(rendered.contains("unavailable"));
    }

    #[test]
    fn constructors_render_their_message() {
        assert!(
            BundleError::config("bad model")
                .to_string()
                .contains("bad model")
        );
        assert!(
            BundleError::model("timeout")
                .to_string()
                .contains("timeout")
        );
        assert!(
            BundleError::session("no home")
                .to_string()
                .contains("no home")
        );
    }
}
