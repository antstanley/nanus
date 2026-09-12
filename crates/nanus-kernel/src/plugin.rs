//! Plugin lifecycle: the component model of dynamic composition.
//!
//! A plugin is the unit of composition. It declares the services it needs, and
//! the runtime decides *when* it runs: a plugin with unmet requirements sits
//! `Pending` and is activated the moment its dependencies appear. That is the
//! coeffect half of the framework, and it is why a harness can mount its tool
//! registry before its model adapter without a boot script.

use core::fmt;
use core::future::Future;
use core::pin::Pin;
use std::rc::Rc;

use crate::{AnyServiceKey, Context, Error, MountContext, PluginId, ServiceKey};

/// A boxed lifecycle future.
///
/// The kernel is single-threaded, so the futures a plugin returns are not
/// required to be `Send`; they are only required to be `'static`, which is what
/// lets the kernel hold `dyn Plugin`.
pub type PluginFuture = Pin<Box<dyn Future<Output = Result<(), LifecycleError>> + 'static>>;

/// The lifecycle position of a plugin.
///
/// The states are ordered by progress, not by health: `Failed` is terminal for
/// one activation attempt and is superseded only by a fresh mount.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub enum PluginState {
    /// Staged on a kernel but not yet mounted.
    #[default]
    Staged,
    /// Mounted, but at least one required service is missing, so its effects have
    /// not been applied.
    Pending,
    /// Mounted and active.
    Active,
    /// Initialization or activation failed.
    Failed,
    /// Mounted and then deliberately unloaded; its effects have reverted.
    Unloaded,
}

impl PluginState {
    /// Returns `true` when the plugin's effects are applied to the context.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        matches!(self, Self::Active)
    }

    /// Returns `true` when the plugin could still become active without being
    /// remounted.
    #[must_use]
    pub const fn is_resumable(&self) -> bool {
        matches!(self, Self::Pending)
    }

    /// Returns a short human-readable label.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Staged => "staged",
            Self::Pending => "pending",
            Self::Active => "active",
            Self::Failed => "failed",
            Self::Unloaded => "unloaded",
        }
    }
}

impl fmt::Display for PluginState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Errors raised by the lifecycle hooks.
///
/// A hook returns this rather than a bare [`crate::BoxError`] so that a plugin can
/// tell the runtime *which* phase failed without string matching.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LifecycleError {
    /// Initialization failed.
    #[error("initialization failed: {0}")]
    Init(#[source] crate::BoxError),
    /// Mounting failed.
    #[error("mount failed: {0}")]
    Mount(#[source] crate::BoxError),
    /// Unmounting failed.
    #[error("unmount failed: {0}")]
    Unmount(#[source] crate::BoxError),
}

impl LifecycleError {
    /// Wraps an error as an initialization failure.
    pub fn init(error: impl Into<crate::BoxError>) -> Self {
        Self::Init(error.into())
    }

    /// Wraps an error as a mount failure.
    pub fn mount(error: impl Into<crate::BoxError>) -> Self {
        Self::Mount(error.into())
    }

    /// Wraps an error as an unmount failure.
    pub fn unmount(error: impl Into<crate::BoxError>) -> Self {
        Self::Unmount(error.into())
    }
}

/// A component of the system.
///
/// The three hooks run in a fixed order and have distinct jobs:
///
/// 1. [`init`](Plugin::init) runs once, before anything is published. It is where
///    a plugin parses its configuration and fails fast. It receives no
///    [`MountContext`], so it cannot create effects that nothing can revert.
/// 2. [`mount`](Plugin::mount) runs whenever the plugin's requirements are met. It
///    publishes services, registers listeners, and claims events, recording every
///    mutation on the supplied disposer.
/// 3. [`unmount`](Plugin::unmount) runs before the disposer reverts and before the
///    plugin's services disappear, so it can still resolve what it published.
///    Everything it recorded is reverted after it returns.
///
/// # Dyn compatibility
///
/// The hooks return boxed futures rather than being `async fn`s, so the kernel can
/// hold a `dyn Plugin`. That is what makes plugin composition dynamic rather than
/// generic — the harness can add a plugin it did not know about at compile time.
#[allow(clippy::type_complexity)]
pub trait Plugin {
    /// Returns the plugin's stable identity.
    fn id(&self) -> PluginId;

    /// Returns a human-readable one-line description, shown by `nanus plugins`.
    fn description(&self) -> &'static str {
        "a plugin"
    }

    /// Returns the plugin's version.
    fn version(&self) -> crate::Version {
        default_version()
    }

    /// Returns the services this plugin cannot run without.
    ///
    /// The runtime keeps the plugin `Pending` until all of these resolve.
    /// Returning an empty vector means the plugin activates immediately.
    fn requirements(&self) -> Vec<AnyServiceKey> {
        Vec::new()
    }

    /// Prepares the plugin. Runs once, before any effects are recorded.
    ///
    /// # Errors
    ///
    /// Return [`LifecycleError::Init`] when the plugin cannot be prepared. The
    /// plugin is then left [`PluginState::Failed`] and its effects are never
    /// applied.
    fn init(&mut self, cx: &Context) -> PluginFuture;

    /// Applies the plugin's effects. Runs once per activation.
    ///
    /// # Errors
    ///
    /// Return [`LifecycleError::Mount`] when a registration fails. The disposer is
    /// reverted, so a partially mounted plugin leaves no trace.
    fn mount(&mut self, cx: &mut MountContext<'_>) -> PluginFuture;

    /// Withdraws the plugin's behaviour. Runs before the disposer reverts.
    ///
    /// # Errors
    ///
    /// Return [`LifecycleError::Unmount`] when teardown fails. The disposer is
    /// reverted regardless.
    fn unmount(&mut self, cx: &Context) -> PluginFuture;
}

/// A completed lifecycle hook.
fn ready() -> PluginFuture {
    Box::pin(async { Ok(()) })
}

/// The version reported by a plugin that does not declare one.
///
/// Built by a function rather than a `const` because `Version::new` validates, and
/// validation is not usable in a `const` on stable Rust.
fn default_version() -> crate::Version {
    // `Version::new` cannot fail for this input; writing that as a `let else`
    // states the fact rather than leaving the reader to trust a fallback.
    let Ok(version) = crate::Version::new("0.0.0") else {
        unreachable!("the default version is valid by construction")
    };
    version
}

/// A plugin that publishes exactly one service and does nothing else.
///
/// Most adapters are this shape — an `LlmPort`, an `FsPort`, a `ShellPort` — and
/// writing them as a type rather than an ad-hoc struct gives the service a stable
/// key and the plugin a stable id.
pub struct Provider<P: 'static> {
    id: PluginId,
    description: &'static str,
    key: ServiceKey<P>,
    port: Rc<P>,
    requirements: Vec<AnyServiceKey>,
}

impl<P: 'static> Provider<P> {
    /// Builds a provider for `key` around `port`.
    #[must_use]
    pub fn new(id: PluginId, description: &'static str, key: ServiceKey<P>, port: P) -> Self {
        Self {
            id,
            description,
            key,
            port: Rc::new(port),
            requirements: Vec::new(),
        }
    }

    /// Builds a provider around an already-shared port handle.
    #[must_use]
    pub const fn from_handle(
        id: PluginId,
        description: &'static str,
        key: ServiceKey<P>,
        port: Rc<P>,
    ) -> Self {
        Self {
            id,
            description,
            key,
            port,
            requirements: Vec::new(),
        }
    }

    /// Declares the services this provider needs before it can publish its own.
    ///
    /// This is how a chain of adapters states its order: a sandboxed filesystem
    /// provider requires the sandbox, a remote model provider requires the HTTP
    /// client, and neither appears in a boot script.
    #[must_use]
    pub fn requiring(mut self, requirements: Vec<AnyServiceKey>) -> Self {
        self.requirements = requirements;
        self
    }

    /// Returns a shared handle to the wrapped port.
    #[must_use]
    pub fn handle(&self) -> Rc<P> {
        Rc::clone(&self.port)
    }

    /// Returns the key this provider publishes under.
    #[must_use]
    pub const fn key(&self) -> ServiceKey<P> {
        self.key
    }
}

impl<P: 'static> Plugin for Provider<P> {
    fn id(&self) -> PluginId {
        self.id
    }

    fn description(&self) -> &'static str {
        self.description
    }

    fn requirements(&self) -> Vec<AnyServiceKey> {
        self.requirements.clone()
    }

    fn init(&mut self, cx: &Context) -> PluginFuture {
        let _ = cx;
        ready()
    }

    fn mount(&mut self, cx: &mut MountContext<'_>) -> PluginFuture {
        // Precondition: the provider holds its own handle, so withdrawing the
        // service cannot free the port while `unmount` still needs it.
        assert!(
            Rc::strong_count(&self.port) >= 1,
            "the provider holds its port"
        );
        let key = self.key;
        let port = Rc::clone(&self.port);
        let outcome = cx.provide(key, port);
        Box::pin(async move { outcome })
    }

    fn unmount(&mut self, cx: &Context) -> PluginFuture {
        let _ = cx;
        ready()
    }
}

/// The mount body of a [`Hook`] plugin.
type HookBody = Rc<dyn Fn(&mut MountContext<'_>) -> Result<(), LifecycleError>>;

/// A plugin that runs side effects at mount and publishes no service.
///
/// This is the shape of policy plugins: an approval policy, an audit listener, a
/// telemetry sink.
pub struct Hook {
    id: PluginId,
    description: &'static str,
    requirements: Vec<AnyServiceKey>,
    body: HookBody,
}

impl Hook {
    /// Builds a hook plugin from a mount body.
    #[must_use]
    pub fn new<F>(id: PluginId, description: &'static str, body: F) -> Self
    where
        F: Fn(&mut MountContext<'_>) -> Result<(), LifecycleError> + 'static,
    {
        Self {
            id,
            description,
            requirements: Vec::new(),
            body: Rc::new(body),
        }
    }

    /// Declares the services the hook requires before it activates.
    #[must_use]
    pub fn requiring(mut self, requirements: Vec<AnyServiceKey>) -> Self {
        self.requirements = requirements;
        self
    }
}

impl Plugin for Hook {
    fn id(&self) -> PluginId {
        self.id
    }

    fn description(&self) -> &'static str {
        self.description
    }

    fn requirements(&self) -> Vec<AnyServiceKey> {
        self.requirements.clone()
    }

    fn init(&mut self, cx: &Context) -> PluginFuture {
        let _ = cx;
        ready()
    }

    fn mount(&mut self, cx: &mut MountContext<'_>) -> PluginFuture {
        let body = Rc::clone(&self.body);
        let outcome = body(cx);
        Box::pin(async move { outcome })
    }

    fn unmount(&mut self, cx: &Context) -> PluginFuture {
        let _ = cx;
        ready()
    }
}

/// A snapshot of one plugin's lifecycle, for diagnostics.
#[derive(Clone, PartialEq, Eq)]
pub struct PluginInfo {
    /// The plugin identity.
    pub id: PluginId,
    /// The one-line description.
    pub description: &'static str,
    /// The plugin version.
    pub version: crate::Version,
    /// The current lifecycle state.
    pub state: PluginState,
    /// How many effects the plugin currently holds.
    pub effects: usize,
    /// How many services the plugin currently provides.
    pub services: usize,
    /// How many event listeners the plugin currently owns.
    pub listeners: usize,
    /// Requirements that are not currently satisfied.
    pub missing: Vec<crate::ServiceName>,
}

impl fmt::Debug for PluginInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PluginInfo")
            .field("id", &self.id)
            .field("description", &self.description)
            .field("version", &self.version)
            .field("state", &self.state)
            .field("effects", &self.effects)
            .field("services", &self.services)
            .field("listeners", &self.listeners)
            .field("missing", &self.missing)
            .finish()
    }
}

/// Aggregate counts describing a running context.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct PluginStats {
    /// Plugins in [`PluginState::Staged`].
    pub staged: usize,
    /// Plugins in [`PluginState::Pending`].
    pub pending: usize,
    /// Plugins in [`PluginState::Active`].
    pub active: usize,
    /// Plugins in [`PluginState::Failed`].
    pub failed: usize,
    /// Plugins in [`PluginState::Unloaded`].
    pub unloaded: usize,
}

impl PluginStats {
    /// Returns the total number of plugins known to the context.
    #[must_use]
    pub const fn total(&self) -> usize {
        self.staged
            .saturating_add(self.pending)
            .saturating_add(self.active)
            .saturating_add(self.failed)
            .saturating_add(self.unloaded)
    }
}

/// Renders a plugin's identity, version, and description for a listing.
#[must_use]
pub fn describe_plugin(plugin: &dyn Plugin) -> String {
    let id = plugin.id();
    let version = plugin.version();
    let description = plugin.description();
    // Postcondition: every plugin listing names the plugin and its version, so a
    // reader can always tell what is mounted.
    assert!(
        !id.as_str().is_empty(),
        "plugin ids are non-empty by construction"
    );
    format!("{id} {version} — {description}")
}

/// A one-step schema migration applied at startup, before any plugin mounts.
///
/// Migrations exist because plugin configuration is durable state owned by the
/// user: changing a config field must not brick a home directory. Each migration
/// upgrades exactly one version, and the loader applies them in ascending order,
/// so a chain of migrations composes without any step knowing about the others.
#[derive(Clone, Copy)]
pub struct Migration {
    /// The config version this migration upgrades *from*.
    pub from: u32,
    /// The config version this migration upgrades *to*. Must be `from + 1`.
    pub to: u32,
    /// A short description, shown when the migration runs.
    pub description: &'static str,
    /// The transformation.
    pub apply: fn(&mut serde_json::Value) -> Result<(), Error>,
}

impl Migration {
    /// Builds a migration.
    ///
    /// # Panics
    ///
    /// Panics when `to` is not exactly `from + 1`. A migration that skipped a
    /// version would leave the intermediate state unrepresentable.
    #[must_use]
    pub const fn new(
        from: u32,
        to: u32,
        description: &'static str,
        apply: fn(&mut serde_json::Value) -> Result<(), Error>,
    ) -> Self {
        assert!(
            to == from.saturating_add(1),
            "a migration upgrades exactly one version"
        );
        assert!(!description.is_empty(), "a migration is described");
        Self {
            from,
            to,
            description,
            apply,
        }
    }
}

impl fmt::Debug for Migration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Migration")
            .field("from", &self.from)
            .field("to", &self.to)
            .field("description", &self.description)
            .finish_non_exhaustive()
    }
}

/// Applies every migration needed to bring `document` from `current` to `target`.
///
/// # Errors
///
/// Returns [`Error::Config`] when a migration is missing for a version step, or
/// when a migration fails. Never downgrades: the older schema cannot represent the
/// newer document, so that is an error rather than silent data loss.
pub fn run_startup_migrations(
    document: &mut serde_json::Value,
    current: u32,
    target: u32,
    migrations: &[Migration],
) -> Result<u32, Error> {
    // Precondition: a downgrade is never attempted.
    if current > target {
        return Err(Error::Config(format!(
            "config version {current} is newer than this build's {target}"
        )));
    }
    let mut version = current;
    while version < target {
        let step = migrations
            .iter()
            .find(|migration| migration.from == version);
        let Some(step) = step else {
            return Err(Error::Config(format!(
                "no migration from config version {version}"
            )));
        };
        (step.apply)(document)?;
        tracing::info!(
            from = step.from,
            to = step.to,
            description = step.description,
            "applied config migration"
        );
        version = step.to;
        // Invariant: each step advances exactly one version, so the loop is
        // bounded by `target - current` iterations.
        assert!(version <= target, "migrations cannot overshoot the target");
    }
    Ok(version)
}
