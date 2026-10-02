//! The one error type of the extension.

/// Why a `read_video` call produced no result.
///
/// Every variant is rendered to the model as a [`nanus_domain::ToolOutcome::Failure`]: a
/// video that cannot be read is information the model can act on, not a harness fault.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum VideoError {
    /// The arguments are malformed or ask for something outside the bounds.
    #[error("{0}")]
    Argument(String),
    /// The extension is not ready: a missing decoder, or no verified analysis route.
    #[error("{0}")]
    Unavailable(String),
    /// The source could not be snapshotted.
    #[error("{0}")]
    Source(String),
    /// The media is unreadable, unsafe or outside the bounds.
    #[error("{0}")]
    Media(String),
    /// The decoder failed or ran past its deadline.
    #[error("{0}")]
    Decoder(String),
    /// The analysis request failed or was cut off.
    #[error("{0}")]
    Analysis(String),
}
