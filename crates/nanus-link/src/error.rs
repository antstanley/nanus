//! The link's boundary error.
//!
//! One enum, because a link fails in one of four ways: a socket operation failed, the
//! socket could not be reached at all, the peer said something that is not this
//! protocol, or the peer closed before answering. Nothing vendor-shaped escapes —
//! `io::Error` is translated here, and the server translates a `BundleError` into
//! [`LinkError::Agent`] rather than letting the agent's vocabulary reach an interface.

use std::path::PathBuf;

/// A failure on the local agent link.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LinkError {
    /// A socket read or write failed.
    #[error("the agent link failed: {0}")]
    Io(#[from] std::io::Error),

    /// Nothing is listening at the socket.
    ///
    /// Distinct from [`LinkError::Io`] because it is the *expected* outcome of asking
    /// for a service that was never started, and a caller usually wants to say so
    /// rather than report a syscall.
    #[error("no agent is listening at {path}: {source}")]
    Connect {
        /// The socket that was tried.
        path: PathBuf,
        /// Why the connection failed.
        source: std::io::Error,
    },

    /// The peer sent something that is not a frame of this protocol.
    #[error("the link sent something that is not a nanus frame: {0}")]
    Protocol(String),

    /// The connection closed before the expected frame arrived.
    #[error("the agent closed the link before answering")]
    Closed,

    /// The agent ran and failed.
    #[error("{0}")]
    Agent(String),
}

impl LinkError {
    /// Builds a [`LinkError::Protocol`].
    pub fn protocol(message: impl Into<String>) -> Self {
        Self::Protocol(message.into())
    }

    /// Builds a [`LinkError::Agent`].
    pub fn agent(message: impl Into<String>) -> Self {
        Self::Agent(message.into())
    }
}

/// The result of a link operation.
pub type LinkResult<T> = Result<T, LinkError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_connect_failure_names_the_socket() {
        let error = LinkError::Connect {
            path: PathBuf::from("/tmp/nanus.sock"),
            source: std::io::Error::from(std::io::ErrorKind::NotFound),
        };
        let rendered = error.to_string();
        assert!(rendered.contains("/tmp/nanus.sock"), "{rendered}");
    }

    #[test]
    fn a_protocol_failure_carries_what_was_seen() {
        let error = LinkError::protocol("unexpected tag `wibble`");
        assert!(error.to_string().contains("wibble"), "{error}");
    }

    #[test]
    fn an_agent_failure_renders_its_message_alone() {
        // The message is already user-facing: the agent composed it for a person, so
        // wrapping it in another sentence would only add noise.
        assert_eq!(
            LinkError::agent("the model refused").to_string(),
            "the model refused"
        );
    }
}
