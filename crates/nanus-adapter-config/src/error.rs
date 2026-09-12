//! Failures the configuration adapter can produce.
//!
//! Every variant is a *caller-visible* configuration failure. The underlying
//! vendor errors — `std::io`, `toml`, `serde_json`, and the kernel's migration
//! error — are translated here, at the boundary, so nothing above this crate
//! names one.

use std::path::PathBuf;

/// A configuration could not be located, read, validated, or written.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ConfigError {
    /// An operating-system failure, tagged with the path it concerns.
    #[error("i/o error at {path}: {source}")]
    Io {
        /// The path the operation was about.
        path: PathBuf,
        /// The operating-system failure.
        #[source]
        source: std::io::Error,
    },

    /// The file was not valid TOML.
    #[error("{path} is not valid TOML: {source}")]
    Malformed {
        /// The offending file.
        path: PathBuf,
        /// The parse failure.
        #[source]
        source: toml::de::Error,
    },

    /// The document parsed as TOML but did not fit the schema.
    #[error("{path} does not match the configuration schema: {source}")]
    Invalid {
        /// The offending file.
        path: PathBuf,
        /// The schema failure.
        #[source]
        source: serde_json::Error,
    },

    /// The configuration could not be encoded.
    #[error("failed to encode the configuration: {source}")]
    Serialize {
        /// The encoding failure.
        #[source]
        source: toml::ser::Error,
    },

    /// The file was written by a newer build.
    #[error("configuration version {found} is newer than this build's version {supported}")]
    UnsupportedVersion {
        /// The version found on disk.
        found: u32,
        /// The version this build understands.
        supported: u32,
    },

    /// A startup migration failed or was missing.
    #[error("configuration migration failed: {source}")]
    Migration {
        /// The kernel's migration failure.
        #[source]
        source: nanus_kernel::Error,
    },

    /// No configuration directory could be determined.
    #[error("no configuration directory is available: {reason}")]
    NoConfigDirectory {
        /// Why the platform lookup failed.
        reason: String,
    },
}

impl From<nanus_kernel::Error> for ConfigError {
    fn from(source: nanus_kernel::Error) -> Self {
        Self::Migration { source }
    }
}
