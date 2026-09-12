//! The crate's error boundary.
//!
//! Each port owns the failures it can produce, because an adapter implementing
//! one port should not have to render variants belonging to another. Gating them
//! is [`PortError`], which every port error converts into, so a consumer that
//! only cares about "the boundary failed" writes one `?` and moves on.
//!
//! **The core never sees a third-party error type.** `reqwest::Error`,
//! `std::io::Error`, and every other foreign failure is rendered into a string
//! at the adapter that caught it. That is what keeps the port surface stable
//! when an adapter swaps its HTTP client, and it is why [`LlmError::HttpStatus`]
//! carries a status code and a body snippet rather than a transport error.

use crate::fs::FsError;
use crate::llm::LlmError;
use crate::shell::ShellError;
use crate::store::StoreError;

/// Any failure at the hexagon's boundary.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum PortError {
    /// The model adapter failed.
    #[error(transparent)]
    Llm(#[from] LlmError),

    /// A filesystem operation failed.
    #[error(transparent)]
    Fs(#[from] FsError),

    /// A shell operation failed.
    #[error(transparent)]
    Shell(#[from] ShellError),

    /// Session storage failed.
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// The result type for consumers that handle the whole boundary uniformly.
pub type PortResult<T> = Result<T, PortError>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn a_port_error_converts_transparently_at_the_boundary() {
        let fs = FsError::NotFound {
            path: PathBuf::from("/missing"),
        };
        let boundary: PortError = fs.into();
        assert!(matches!(boundary, PortError::Fs(_)));
        // The message survives, so a caller that only renders the error still
        // says which file was missing.
        assert!(boundary.to_string().contains("/missing"));

        let llm = LlmError::MissingCredentials {
            provider: "deepseek".to_owned(),
        };
        let boundary: PortError = llm.into();
        assert!(matches!(boundary, PortError::Llm(_)));

        let shell = ShellError::SandboxUnavailable {
            platform: "unknown".to_owned(),
        };
        let boundary: PortError = shell.into();
        assert!(matches!(boundary, PortError::Shell(_)));

        let store = StoreError::NotFound {
            id: "s-1".to_owned(),
        };
        let boundary: PortError = store.into();
        assert!(matches!(boundary, PortError::Store(_)));
    }

    #[test]
    fn the_result_alias_uses_the_boundary_error() {
        let ok: PortResult<u32> = Ok(3);
        assert_eq!(ok.ok(), Some(3));
        let err: PortResult<u32> = Err(PortError::Store(StoreError::NotFound {
            id: "x".to_owned(),
        }));
        assert!(err.is_err());
    }
}
