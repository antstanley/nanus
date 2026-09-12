use core::fmt;

/// Errors produced by the kernel.
///
/// Every variant is a caller-visible failure of the composition model, not an
/// internal invariant violation: an internal invariant violation is an assertion.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A name or version failed validation.
    #[error("invalid name {name:?}: {reason}")]
    InvalidName {
        /// The rejected text.
        name: &'static str,
        /// Why it was rejected.
        reason: &'static str,
    },

    /// The same plugin id was mounted twice.
    #[error("plugin {id} is already mounted")]
    DuplicatePlugin {
        /// The conflicting plugin id.
        id: crate::PluginId,
    },

    /// A service was republished under a name that already holds another type.
    #[error("service {key} is already published with a different type")]
    ServiceTypeMismatch {
        /// The service key whose published type did not match.
        key: crate::ServiceName,
    },

    /// A plugin declared a dependency that no mounted plugin provides.
    #[error("plugin {id} requires service {key}, which is not provided")]
    MissingDependency {
        /// The plugin that declared the dependency.
        id: crate::PluginId,
        /// The missing service.
        key: crate::ServiceName,
    },

    /// A service name already held by another plugin.
    #[error("service {key} is already provided by plugin {provider}")]
    ServiceAlreadyProvided {
        /// The contested service name.
        key: crate::ServiceName,
        /// The plugin that already publishes it.
        provider: crate::PluginId,
    },

    /// Plugin initialization failed.
    #[error("plugin {id} failed to initialize: {source}")]
    PluginInit {
        /// The failing plugin.
        id: crate::PluginId,
        /// The underlying failure.
        #[source]
        source: Box<dyn core::error::Error + Send + Sync>,
    },

    /// Plugin mounting failed.
    #[error("plugin {id} failed to mount: {source}")]
    PluginMount {
        /// The failing plugin.
        id: crate::PluginId,
        /// The underlying failure.
        #[source]
        source: Box<dyn core::error::Error + Send + Sync>,
    },

    /// Reverting one or more effects failed.
    #[error("failed to revert {failed} of {total} effects; first failure: {first}")]
    Revert {
        /// How many effects failed to revert.
        failed: usize,
        /// How many effects were reverted in total.
        total: usize,
        /// The first failure, rendered for logs.
        first: String,
    },

    /// A configuration document was rejected.
    #[error("invalid configuration: {0}")]
    Config(String),
}

impl Error {
    /// Wraps a boxed error as a [`Error::PluginInit`].
    ///
    /// Takes the box rather than a generic error so a lifecycle hook's already-erased
    /// source can be moved in without a second allocation.
    pub(crate) const fn plugin_init(id: crate::PluginId, source: BoxError) -> Self {
        Self::PluginInit { id, source }
    }
}

/// A boxed error used at composition boundaries where the concrete type is
/// irrelevant to the caller.
pub type BoxError = Box<dyn core::error::Error + Send + Sync + 'static>;

/// A rendered, cloneable description of a failure.
///
/// Used where errors cross a boundary that cannot carry a source chain, such as a
/// session event log or a TUI status line.
#[derive(Clone, PartialEq, Eq)]
pub struct ErrorReport(String);

impl ErrorReport {
    /// Renders any displayable error into a report.
    pub fn new(error: &impl fmt::Display) -> Self {
        Self(error.to_string())
    }

    /// Returns the rendered message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ErrorReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for ErrorReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ErrorReport({})", self.0)
    }
}
