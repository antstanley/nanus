//! The context: one repository of services, shared by every component.
//!
//! Cordis unifies the effect context and the coeffect context into a single type,
//! and so does the kernel. A [`Context`] holds the service registry that effects
//! mutate and the dependency declarations that coeffects are resolved against, so
//! a component that publishes a service defines a coeffect for its consumers in
//! the same act.
//!
//! ## Ownership
//!
//! The context is `Rc`-shared and single-threaded; every plugin gets a clone.
//! [`Context::shutdown`] reverts every plugin's effects, so a plugin that never
//! explicitly unloads still releases what it registered.

use core::cell::{Ref, RefCell, RefMut};
use core::future::Future;
use std::rc::Rc;

use crate::effect::{Disposer, Effect};
use crate::event::{self, EventGuard, EventKey, Next, Priority};
use crate::plugin::{LifecycleError, Migration, Plugin, PluginInfo, PluginState, PluginStats};
use crate::service::Registry as ServiceRegistry;
use crate::{BoxError, Error, PluginId, ServiceInfo, ServiceKey, ServiceName};

/// The shared interior of a context.
pub(crate) struct ContextInner {
    /// Published services.
    pub(crate) services: ServiceRegistry,
    /// Registered event listeners.
    pub(crate) events: event::Registry,
    /// Plugins staged by the kernel, in declaration order.
    pub(crate) staged: Vec<StagedPlugin>,
    /// Mounted plugins, in declaration order.
    pub(crate) plugins: Vec<PluginRecord>,
    /// Registered startup migrations.
    pub(crate) migrations: Vec<Migration>,
    /// Whether `start` has run.
    pub(crate) started: bool,
    /// Guards against re-entrant activation sweeps.
    pub(crate) refreshing: bool,
}

/// A plugin staged on a kernel but not yet mounted.
pub(crate) type StagedPlugin = (PluginId, Rc<RefCell<dyn Plugin>>);

/// The kernel's record of one mounted plugin.
pub(crate) struct PluginRecord {
    pub(crate) id: PluginId,
    pub(crate) description: &'static str,
    pub(crate) version: crate::Version,
    pub(crate) state: PluginState,
    /// The plugin itself, kept so the context can run its hooks.
    pub(crate) plugin: Rc<RefCell<dyn Plugin>>,
    /// The requirements captured at mount time.
    pub(crate) requirements: Vec<crate::AnyServiceKey>,
    /// The effects this plugin's activation recorded.
    pub(crate) disposer: Disposer,
    /// The event registrations this plugin's mount context created, retained so
    /// teardown can unregister them eagerly instead of waiting on the effects.
    pub(crate) guards: RefCell<Vec<Rc<EventGuard>>>,
    /// Requirements that were unsatisfied at the last sweep.
    pub(crate) missing: Vec<ServiceName>,
}

impl core::fmt::Debug for PluginRecord {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PluginRecord")
            .field("id", &self.id)
            .field("state", &self.state)
            .field("effects", &self.disposer.len())
            .field("missing", &self.missing)
            .finish_non_exhaustive()
    }
}

/// The kernel's own participant identity.
const KERNEL_ID: PluginId = PluginId::from_validated(crate::Name::new_unchecked("kernel"));

/// A running composition of plugins.
///
/// Cloning a context is cheap and yields another handle to the *same* composition:
/// services and listeners are shared, never copied.
#[derive(Clone)]
pub struct Context {
    inner: Rc<RefCell<ContextInner>>,
}

impl core::fmt::Debug for Context {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let inner = self.inner.borrow();
        f.debug_struct("Context")
            .field("plugins", &inner.plugins.len())
            .field("staged", &inner.staged.len())
            .field("started", &inner.started)
            .finish_non_exhaustive()
    }
}

impl Context {
    /// Creates an empty context.
    #[allow(clippy::missing_const_for_fn)]
    pub(crate) fn new() -> Self {
        Self {
            inner: Rc::new(RefCell::new(ContextInner {
                services: ServiceRegistry::new(),
                events: event::Registry::new(),
                staged: Vec::new(),
                plugins: Vec::new(),
                migrations: Vec::new(),
                started: false,
                refreshing: false,
            })),
        }
    }

    /// Returns the identity the kernel uses for its own actions.
    #[must_use]
    pub const fn kernel_id() -> PluginId {
        KERNEL_ID
    }

    /// Borrows the shared interior.
    pub(crate) fn inner(&self) -> Ref<'_, ContextInner> {
        self.inner.borrow()
    }

    /// Borrows the shared interior mutably.
    pub(crate) fn inner_mut(&self) -> RefMut<'_, ContextInner> {
        self.inner.borrow_mut()
    }

    /// Returns a view of the published services that attributes errors to `owner`.
    #[must_use]
    pub const fn services_for(&self, owner: PluginId) -> Services<'_> {
        Services {
            owner,
            context: self,
        }
    }

    /// Resolves a required service.
    ///
    /// # Errors
    ///
    /// Returns [`Error::MissingDependency`] when nothing is published under `key`.
    pub fn get<T: core::any::Any>(&self, key: ServiceKey<T>) -> Result<Rc<T>, Error> {
        self.inner.borrow().services.get(key, KERNEL_ID)
    }

    /// Resolves an optional service.
    #[must_use]
    pub fn try_get<T: core::any::Any>(&self, key: ServiceKey<T>) -> Option<Rc<T>> {
        self.get(key).ok()
    }

    /// Returns `true` when `key` is published with a matching type.
    #[must_use]
    pub fn has<T: core::any::Any>(&self, key: ServiceKey<T>) -> bool {
        self.inner.borrow().services.contains(key)
    }

    /// Resolves a service, reporting a mount failure when it is absent.
    ///
    /// # Errors
    ///
    /// Returns a mount-phase [`LifecycleError`] wrapping
    /// [`Error::MissingDependency`].
    pub fn require<T: core::any::Any>(&self, key: ServiceKey<T>) -> Result<Rc<T>, LifecycleError> {
        self.get(key).map_err(LifecycleError::mount)
    }

    /// Dispatches an event to every listener through `emit`.
    ///
    /// `emit` is fire-and-forget: listeners observe and cannot affect the caller.
    pub fn emit<K: 'static>(&self, key: EventKey<K>, payload: &K) {
        self.inner.borrow().events.dispatch_emit(key, payload);
    }

    /// Dispatches an event through `waterfall`, returning the final payload.
    ///
    /// A listener wraps the payload and calls [`Next::run`] to delegate. A listener
    /// that returns without delegating short-circuits the chain, and the caller
    /// observes `None`.
    #[must_use]
    pub fn waterfall<K: 'static>(&self, key: EventKey<K>, payload: K) -> Option<K> {
        self.inner.borrow().events.dispatch_waterfall(key, payload)
    }

    /// Dispatches an event through `waterfall`, recording `dispatcher` as its
    /// source so a listener can tell who raised it.
    #[must_use]
    pub fn waterfall_from<K: 'static>(
        &self,
        key: EventKey<K>,
        dispatcher: PluginId,
        payload: K,
    ) -> Option<K> {
        self.inner
            .borrow()
            .events
            .dispatch_waterfall_from(key, dispatcher, payload)
    }

    /// Dispatches an event through `parallel`: every listener runs concurrently.
    pub fn parallel<K: 'static>(&self, key: EventKey<K>, payload: &K) {
        self.inner.borrow().events.dispatch_parallel(key, payload);
    }

    /// Dispatches an event through `serial`, returning the first decision.
    ///
    /// Every listener gets a turn until one returns `Some`.
    #[must_use]
    pub fn serial<K: 'static, R: 'static>(&self, key: EventKey<K>, payload: &K) -> Option<R> {
        self.inner.borrow().events.dispatch_serial(key, payload)
    }

    /// Dispatches an event through `bail`, returning the first decision.
    ///
    /// The chain stops at the first listener that returns `Some`, so a policy
    /// listener can own a decision outright.
    #[must_use]
    pub fn bail<K: 'static, R: 'static>(&self, key: EventKey<K>, payload: &K) -> Option<R> {
        self.inner.borrow().events.dispatch_bail(key, payload)
    }

    /// Mounts every staged plugin and activates those whose requirements are met.
    ///
    /// # Errors
    ///
    /// Returns the first initialization or mount failure. Plugins that mounted
    /// before it remain mounted: partial composition is inspectable and revertible,
    /// and the caller decides whether to keep or unwind it.
    pub fn start(&self) -> Result<(), Error> {
        {
            let mut inner = self.inner_mut();
            // Negative space: starting twice would double every effect.
            assert!(!inner.started, "a context starts once");
            inner.started = true;
        }
        self.mount_staged()?;
        self.refresh();
        Ok(())
    }

    /// Mounts the staged plugins, in declaration order.
    fn mount_staged(&self) -> Result<(), Error> {
        let staged: Vec<StagedPlugin> = core::mem::take(&mut self.inner_mut().staged);
        // Postcondition: staging is drained, so a plugin cannot mount twice.
        assert!(
            self.inner().staged.is_empty(),
            "mounting drains the staging area"
        );
        for (id, plugin) in staged {
            self.mount_one(id, plugin)?;
        }
        Ok(())
    }

    /// Mounts exactly one plugin, running its `init` hook.
    fn mount_one(&self, id: PluginId, plugin: Rc<RefCell<dyn Plugin>>) -> Result<(), Error> {
        // Precondition: the id is unique, so ownership of effects is unambiguous.
        if self.inner().plugins.iter().any(|record| record.id == id) {
            return Err(Error::DuplicatePlugin { id });
        }
        let (description, version, requirements) = {
            let borrowed = plugin.borrow();
            (
                borrowed.description(),
                borrowed.version(),
                borrowed.requirements(),
            )
        };
        {
            let mut borrowed = plugin.borrow_mut();
            let prepared = crate::runtime::block_on(borrowed.init(self));
            prepared.map_err(|error| match error {
                LifecycleError::Init(source)
                | LifecycleError::Mount(source)
                | LifecycleError::Unmount(source) => Error::plugin_init(id, source),
            })?;
        }
        let mut disposer = Disposer::new();
        disposer.set_owner(id);
        let missing = requirements
            .iter()
            .filter(|key| !self.inner().services.contains_erased(**key))
            .map(super::service::AnyServiceKey::name)
            .collect();
        self.inner_mut().plugins.push(PluginRecord {
            id,
            description,
            version,
            state: PluginState::Pending,
            plugin,
            requirements,
            disposer,
            guards: RefCell::new(Vec::new()),
            missing,
        });
        tracing::debug!(plugin = %id, "plugin mounted, awaiting activation");
        Ok(())
    }

    /// Reconciles every plugin against the services currently published.
    ///
    /// This is coeffect resolution, and it runs in both directions:
    ///
    /// - a `Pending` plugin whose requirements became satisfied **activates**;
    /// - an `Active` plugin whose requirements went away **deactivates**.
    ///
    /// The deactivating half is what makes removal of a component sound: a
    /// provider unloading must not leave a consumer holding a capability that no
    /// longer exists. It is idempotent and bounded, because each sweep can only
    /// move a plugin between `Pending` and `Active`, never oscillate.
    pub fn refresh(&self) {
        {
            let mut inner = self.inner_mut();
            if inner.refreshing {
                // A nested sweep (activation published a service) is completed by
                // the sweep already in progress.
                return;
            }
            inner.refreshing = true;
        }
        let sweeps_max = self.inner().plugins.len().saturating_add(1);
        for _ in 0..sweeps_max {
            let activated = self.activate_ready();
            let deactivated = self.deactivate_stale();
            if !activated && !deactivated {
                break;
            }
        }
        let mut inner = self.inner_mut();
        inner.refreshing = false;
        drop(inner);
        self.report_pending();
    }

    /// Records which requirements are currently unmet, for diagnostics.
    fn report_pending(&self) {
        let mut inner = self.inner_mut();
        let pending: Vec<(PluginId, Vec<ServiceName>)> = inner
            .plugins
            .iter()
            .filter(|record| record.state == PluginState::Pending)
            .map(|record| {
                let missing = record
                    .requirements
                    .iter()
                    .filter(|key| !inner.services.contains_erased(**key))
                    .map(super::service::AnyServiceKey::name)
                    .collect();
                (record.id, missing)
            })
            .collect();
        for (id, missing) in pending {
            if let Some(record) = inner.plugins.iter_mut().find(|record| record.id == id) {
                record.missing = missing;
            }
        }
    }

    /// Unloads every active plugin whose requirements are no longer satisfied.
    ///
    /// Returns `true` when at least one plugin deactivated, which means another
    /// sweep is worthwhile: a plugin that lost a service may itself have provided
    /// one that others depended on.
    ///
    /// Deactivation runs through [`Context::unload`], so it reverts the plugin's
    /// effects with the same discipline as an explicit unload. The unloads are
    /// deferred until the sweep loop has finished so a plugin removed here cannot
    /// invalidate the iteration in progress.
    fn deactivate_stale(&self) -> bool {
        let stale: Vec<PluginId> = {
            let inner = self.inner();
            inner
                .plugins
                .iter()
                .filter(|record| record.state.is_active())
                .filter(|record| {
                    // A plugin that provides nothing and requires nothing is never
                    // stale; it has no coeffect to lose.
                    !record.requirements.is_empty()
                        && !record
                            .requirements
                            .iter()
                            .all(|key| inner.services.contains_erased(*key))
                })
                .map(|record| record.id)
                .collect()
        };
        let deactivated = !stale.is_empty();
        for id in stale {
            tracing::debug!(plugin = %id, "requirements withdrawn, deactivating");
            // The plugin is removed from the active set either way, so a revert
            // failure is reported and the sweep continues rather than aborting.
            if let Err(error) = self.unload_deferred(id) {
                tracing::warn!(plugin = %id, error = %error, "deactivation reverted incompletely");
            }
        }
        deactivated
    }

    /// Unloads a plugin without triggering a nested sweep.
    ///
    /// [`refresh`](Context::refresh) is already sweeping, and its own
    /// re-entrancy guard would swallow the nested call; doing the work inline
    /// keeps the outer loop's bookkeeping correct.
    fn unload_deferred(&self, id: PluginId) -> Result<(), Error> {
        let mut record = {
            let mut inner = self.inner_mut();
            match inner.plugins.iter().position(|record| record.id == id) {
                Some(index) => inner.plugins.remove(index),
                None => return Ok(()),
            }
        };
        let outcome = {
            let mut borrowed = record.plugin.borrow_mut();
            crate::runtime::block_on(borrowed.unmount(self))
        };
        if let Err(error) = outcome {
            tracing::warn!(plugin = %id, error = %error, "unmount hook failed");
        }
        let registered: Vec<ServiceName> = record
            .guards
            .borrow()
            .iter()
            .map(|guard| guard.key())
            .collect();
        self.inner_mut().services.remove_owned_by(id);
        {
            let inner = self.inner();
            for name in registered {
                inner.events.remove_named(name, id);
            }
        }
        record.guards.borrow_mut().clear();
        let revert = record.disposer.revert(self);
        record.state = PluginState::Pending;
        self.inner_mut().plugins.push(record);
        match revert {
            Ok(()) => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Activates every pending plugin whose requirements resolve.
    ///
    /// Returns `true` when at least one plugin activated, which means another sweep
    /// may activate plugins that depend on it.
    fn activate_ready(&self) -> bool {
        let candidates: Vec<PluginId> = {
            let inner = self.inner();
            inner
                .plugins
                .iter()
                .filter(|record| record.state == PluginState::Pending)
                .filter(|record| {
                    record
                        .requirements
                        .iter()
                        .all(|key| inner.services.contains_erased(*key))
                })
                .map(|record| record.id)
                .collect()
        };
        let mut progressed = false;
        for id in candidates {
            self.activate(id);
            progressed = true;
        }
        progressed
    }

    /// Runs the mount hook for `id` and applies its effects.
    fn activate(&self, id: PluginId) {
        let plugin = {
            let inner = self.inner();
            match inner.plugins.iter().find(|record| record.id == id) {
                Some(record) => Rc::clone(&record.plugin),
                None => return,
            }
        };
        // The disposer is built outside the record so the plugin's effects are
        // recorded on it during the mount and stored only on success. A failed
        // mount reverts immediately, so a partial activation leaves no trace.
        let mut disposer = Disposer::new();
        disposer.set_owner(id);
        let outcome = {
            let mut mount_cx = MountContext {
                id,
                context: self,
                disposer: &mut disposer,
            };
            let mut borrowed = plugin.borrow_mut();
            crate::runtime::block_on(borrowed.mount(&mut mount_cx))
        };
        match outcome {
            Ok(()) => {
                let effects = disposer.len();
                let mut inner = self.inner_mut();
                if let Some(record) = inner.plugins.iter_mut().find(|record| record.id == id) {
                    record.disposer = disposer;
                    record.state = PluginState::Active;
                    record.missing.clear();
                }
                drop(inner);
                tracing::debug!(plugin = %id, effects, "plugin activated");
            }
            Err(error) => {
                let revert = disposer.revert(self);
                if let Err(failure) = revert {
                    tracing::error!(
                        plugin = %id,
                        error = %failure,
                        "reverting a failed mount left effects behind"
                    );
                }
                let mut inner = self.inner_mut();
                if let Some(record) = inner.plugins.iter_mut().find(|record| record.id == id) {
                    record.state = PluginState::Failed;
                }
                drop(inner);
                tracing::error!(plugin = %id, error = %error, "plugin failed to activate");
            }
        }
    }

    /// Unloads a plugin: withdraws its registrations and reverts its effects.
    ///
    /// The plugin's record is retained in [`PluginState::Unloaded`] so a diagnostic
    /// listing can show that it existed and was torn down.
    ///
    /// # Errors
    ///
    /// Returns an error when an effect fails to revert. The plugin is unloaded
    /// either way, because a half-reverted plugin is worse than a fully reverted one.
    pub fn unload(&self, id: PluginId) -> Result<(), Error> {
        let mut record = {
            let mut inner = self.inner_mut();
            match inner.plugins.iter().position(|record| record.id == id) {
                Some(index) => inner.plugins.remove(index),
                None => return Ok(()),
            }
        };
        let outcome = {
            let mut borrowed = record.plugin.borrow_mut();
            crate::runtime::block_on(borrowed.unmount(self))
        };
        if let Err(error) = outcome {
            tracing::warn!(plugin = %id, error = %error, "unmount hook failed");
        }
        // Registrations are withdrawn before the effects revert, so a plugin's
        // effects can still resolve the services it published while unwinding.
        // Both withdrawals are name-scoped and owner-checked, so a replacement
        // another plugin published is left untouched.
        let registered: Vec<ServiceName> = record
            .guards
            .borrow()
            .iter()
            .map(|guard| guard.key())
            .collect();
        self.inner_mut().services.remove_owned_by(id);
        {
            let inner = self.inner();
            for name in registered {
                inner.events.remove_named(name, id);
            }
        }
        record.guards.borrow_mut().clear();
        let revert = record.disposer.revert(self);
        record.state = PluginState::Unloaded;
        self.inner_mut().plugins.push(record);
        self.refresh();
        match revert {
            Ok(()) => Ok(()),
            Err(error) => {
                tracing::warn!(plugin = %id, error = %error, "effect revert failed");
                Err(error)
            }
        }
    }

    /// Returns a diagnostic snapshot of every mounted plugin, ordered by id.
    #[must_use]
    pub fn plugins(&self) -> Vec<PluginInfo> {
        let inner = self.inner();
        let service_info = inner.services.snapshot();
        let listener_info = inner.events.snapshot_all();
        let mut info: Vec<PluginInfo> = inner
            .plugins
            .iter()
            .map(|record| PluginInfo {
                id: record.id,
                description: record.description,
                version: record.version,
                state: record.state,
                effects: record.disposer.len(),
                services: provider_count(&service_info, record.id),
                listeners: owner_count(&listener_info, record.id),
                missing: record.missing.clone(),
            })
            .collect();
        drop(inner);
        info.sort_by_key(|info| info.id);
        info
    }

    /// Returns the diagnostic record of the plugin registered under `id`.
    #[must_use]
    pub fn plugin(&self, id: PluginId) -> Option<PluginInfo> {
        self.plugins().into_iter().find(|info| info.id == id)
    }

    /// Returns a diagnostic snapshot of every published service.
    #[must_use]
    pub fn service_listing(&self) -> Vec<ServiceInfo> {
        self.inner().services.snapshot()
    }

    /// Returns a diagnostic snapshot of every registered event listener.
    #[must_use]
    pub fn listener_listing(&self) -> Vec<crate::ListenerInfo> {
        self.inner().events.snapshot_all()
    }

    /// Returns aggregate lifecycle counts.
    #[must_use]
    pub fn stats(&self) -> PluginStats {
        let inner = self.inner();
        let mut stats = PluginStats::default();
        for record in &inner.plugins {
            match record.state {
                PluginState::Staged => stats.staged = stats.staged.saturating_add(1),
                PluginState::Pending => stats.pending = stats.pending.saturating_add(1),
                PluginState::Active => stats.active = stats.active.saturating_add(1),
                PluginState::Failed => stats.failed = stats.failed.saturating_add(1),
                PluginState::Unloaded => stats.unloaded = stats.unloaded.saturating_add(1),
            }
        }
        stats
    }

    /// Reverts every plugin's effects, newest first.
    ///
    /// Shutdown is the composite inverse of startup: services withdraw, listeners
    /// unregister, and the context returns to its pre-mount state.
    ///
    /// # Errors
    ///
    /// Returns the first revert failure; remaining plugins still revert.
    pub fn shutdown(&self) -> Result<(), Error> {
        let ids: Vec<PluginId> = {
            let inner = self.inner();
            inner
                .plugins
                .iter()
                .filter(|record| record.state.is_active())
                .map(|record| record.id)
                .collect()
        };
        let mut first_failure: Option<Error> = None;
        for id in ids.into_iter().rev() {
            if let Err(error) = self.unload(id)
                && first_failure.is_none()
            {
                first_failure = Some(error);
            }
        }
        {
            let mut inner = self.inner_mut();
            inner.plugins.clear();
            inner.started = false;
        }
        first_failure.map_or(Ok(()), Err)
    }

    /// Returns the number of plugins holding a live handle to `key`.
    #[must_use]
    pub fn consumers<T: core::any::Any>(&self, key: ServiceKey<T>) -> usize {
        self.inner().services.consumer_count(key)
    }
}

/// Counts how many services `provider` publishes.
fn provider_count(services: &[ServiceInfo], provider: PluginId) -> usize {
    services
        .iter()
        .filter(|service| service.provider == provider)
        .count()
}

/// Counts how many listeners `owner` registered.
fn owner_count(listeners: &[crate::ListenerInfo], owner: PluginId) -> usize {
    listeners
        .iter()
        .filter(|listener| listener.owner == owner)
        .count()
}

/// A view of the services published on a context.
pub struct Services<'a> {
    owner: PluginId,
    context: &'a Context,
}

impl Services<'_> {
    /// Resolves a required service, attributing failure to this view's owner.
    ///
    /// # Errors
    ///
    /// Returns [`Error::MissingDependency`] naming the owner when nothing is
    /// published under `key`.
    pub fn get<T: core::any::Any>(&self, key: ServiceKey<T>) -> Result<Rc<T>, Error> {
        self.context.inner().services.get(key, self.owner)
    }

    /// Resolves an optional service.
    #[must_use]
    pub fn try_get<T: core::any::Any>(&self, key: ServiceKey<T>) -> Option<Rc<T>> {
        self.get(key).ok()
    }

    /// Resolves a service, reporting a mount failure when it is absent.
    ///
    /// # Errors
    ///
    /// Returns a mount-phase [`LifecycleError`] when the service is absent.
    pub fn require<T: core::any::Any>(&self, key: ServiceKey<T>) -> Result<Rc<T>, LifecycleError> {
        self.get(key).map_err(LifecycleError::mount)
    }

    /// Returns `true` when `key` is published with a matching type.
    #[must_use]
    pub fn has<T: core::any::Any>(&self, key: ServiceKey<T>) -> bool {
        self.context.has(key)
    }

    /// Returns the plugin that published `key`, if any.
    #[must_use]
    pub fn provider<T: core::any::Any>(&self, key: ServiceKey<T>) -> Option<PluginId> {
        self.context
            .service_listing()
            .into_iter()
            .find(|service| service.name == key.name())
            .map(|service| service.provider)
    }
}

impl core::fmt::Debug for Services<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Services")
            .field("owner", &self.owner)
            .finish_non_exhaustive()
    }
}

/// The view of the context a plugin receives while mounting.
///
/// This is where the two halves of the framework meet: every method records an
/// effect (so the runtime can revert it) and publishes a coeffect (so consumers
/// react to it).
pub struct MountContext<'a> {
    id: PluginId,
    context: &'a Context,
    disposer: &'a mut Disposer,
}

impl<'a> MountContext<'a> {
    /// Returns the mounting plugin's identity.
    #[must_use]
    pub const fn id(&self) -> PluginId {
        self.id
    }

    /// Returns the effects recorded so far.
    #[must_use]
    pub const fn effect_count(&self) -> usize {
        self.disposer.len()
    }

    /// Returns the services the mounting plugin may resolve.
    #[must_use]
    pub const fn services(&self) -> Services<'_> {
        self.context.services_for(self.id)
    }

    /// Returns a view of the context's dispatch methods.
    #[must_use]
    pub const fn dispatch(&self) -> Dispatch<'a> {
        Dispatch {
            context: self.context,
        }
    }

    /// Publishes `port` under `key` on behalf of the mounting plugin.
    ///
    /// The publication is an effect: when the plugin unloads, the service is
    /// withdrawn and every plugin that declared it as a requirement loses it.
    ///
    /// # Errors
    ///
    /// Returns a mount-phase [`LifecycleError`] when `key` is already published
    /// with a different type.
    pub fn provide<T: core::any::Any>(
        &mut self,
        key: ServiceKey<T>,
        port: Rc<T>,
    ) -> Result<(), LifecycleError> {
        // Precondition: the handle is shared, so withdrawal cannot dangle.
        assert!(Rc::strong_count(&port) >= 1, "provided handles are shared");
        let id = self.id;
        {
            let inner = self.context.inner();
            inner
                .services
                .provide(key, port, id)
                .map_err(LifecycleError::mount)?;
        }
        // The inverse is recorded only after a successful publication, so a failed
        // publication leaves no effect behind.
        let name = key.raw_name();
        self.disposer.record("service.withdraw", move |context| {
            // Withdrawing a service is the coeffect trigger for its consumers:
            // the sweep that follows deactivates anyone who required it.
            context.inner().services.remove_named(name, id);
            context.refresh();
            Ok(())
        });
        Ok(())
    }

    /// Alias for [`provide`](MountContext::provide), matching Cordis's `ctx.set`.
    ///
    /// # Errors
    ///
    /// Returns a mount-phase [`LifecycleError`] under the same conditions as
    /// [`provide`](MountContext::provide).
    pub fn set<T: core::any::Any>(
        &mut self,
        key: ServiceKey<T>,
        port: Rc<T>,
    ) -> Result<(), LifecycleError> {
        self.provide(key, port)
    }

    /// Registers an observer for an `emit` event.
    ///
    /// The registration is an effect: it unregisters when the plugin unloads.
    pub fn on<K, F>(&mut self, key: EventKey<K>, callback: F)
    where
        K: 'static,
        F: Fn(&crate::Event<K>, &K) + 'static,
    {
        self.on_with(key, Priority::Last, callback);
    }

    /// Registers an observer that runs before existing listeners.
    pub fn prepend<K, F>(&mut self, key: EventKey<K>, callback: F)
    where
        K: 'static,
        F: Fn(&crate::Event<K>, &K) + 'static,
    {
        self.on_with(key, Priority::First, callback);
    }

    /// Registers an observer with an explicit priority.
    pub fn on_with<K, F>(&mut self, key: EventKey<K>, priority: Priority, callback: F)
    where
        K: 'static,
        F: Fn(&crate::Event<K>, &K) + 'static,
    {
        let id = self.id;
        let guard = self.context.inner().events.on(key, id, priority, callback);
        self.record_listener(guard);
    }

    /// Registers an around-middleware listener for a `waterfall` event.
    pub fn waterfall<K, F>(&mut self, key: EventKey<K>, callback: F)
    where
        K: 'static,
        F: Fn(&crate::Event<K>, K, Next) -> Option<K> + 'static,
    {
        self.waterfall_with(key, Priority::Last, callback);
    }

    /// Registers an around-middleware listener that runs before existing ones.
    pub fn prepend_waterfall<K, F>(&mut self, key: EventKey<K>, callback: F)
    where
        K: 'static,
        F: Fn(&crate::Event<K>, K, Next) -> Option<K> + 'static,
    {
        self.waterfall_with(key, Priority::First, callback);
    }

    /// Registers an around-middleware listener with an explicit priority.
    pub fn waterfall_with<K, F>(&mut self, key: EventKey<K>, priority: Priority, callback: F)
    where
        K: 'static,
        F: Fn(&crate::Event<K>, K, Next) -> Option<K> + 'static,
    {
        let id = self.id;
        let guard = self
            .context
            .inner()
            .events
            .waterfall(key, id, priority, callback);
        self.record_listener(guard);
    }

    /// Registers an asynchronous listener for a `parallel` event.
    pub fn parallel<K, F, Fut>(&mut self, key: EventKey<K>, callback: F)
    where
        K: 'static,
        F: Fn(&crate::Event<K>, &K) -> Fut + 'static,
        Fut: Future<Output = ()> + 'static,
    {
        let id = self.id;
        let guard = self
            .context
            .inner()
            .events
            .parallel(key, id, Priority::Last, callback);
        self.record_listener(guard);
    }

    /// Registers a decision listener for a `serial` event.
    pub fn serial<K, R, F>(&mut self, key: EventKey<K>, callback: F)
    where
        K: 'static,
        R: 'static,
        F: Fn(&crate::Event<K>, &K) -> Option<R> + 'static,
    {
        let id = self.id;
        let guard = self
            .context
            .inner()
            .events
            .serial(key, id, Priority::Last, callback);
        self.record_listener(guard);
    }

    /// Registers a decision listener for a `bail` event.
    pub fn bail<K, R, F>(&mut self, key: EventKey<K>, callback: F)
    where
        K: 'static,
        R: 'static,
        F: Fn(&crate::Event<K>, &K) -> Option<R> + 'static,
    {
        let id = self.id;
        let guard = self
            .context
            .inner()
            .events
            .serial(key, id, Priority::Last, callback);
        self.record_listener(guard);
    }

    /// Records listener teardown as an effect.
    ///
    /// Unregistration is therefore LIFO with every other effect, which is what
    /// makes teardown order deterministic rather than incidental.
    fn record_listener(&mut self, guard: Rc<EventGuard>) {
        // The guard is stored on the plugin record as well as wrapped in an
        // effect: the effect guarantees teardown even if a plugin never unloads
        // explicitly, and the record lets `unload` withdraw the registration
        // eagerly, before any service it covers disappears.
        // The same guard is retained on the record *and* wrapped in an effect.
        // The effect guarantees teardown even if the plugin never unloads
        // explicitly; the record lets `unload` withdraw the registration eagerly,
        // before any service it covers disappears. `cancel` is idempotent, so
        // both paths running is harmless.
        if let Some(record) = self
            .context
            .inner()
            .plugins
            .iter()
            .find(|record| record.id == self.id)
        {
            record.guards.borrow_mut().push(Rc::clone(&guard));
        }
        let guard = RefCell::new(Some(guard));
        self.disposer.record("event.unlisten", move |_context| {
            let taken = {
                let mut borrowed = guard.borrow_mut();
                borrowed.take()
            };
            if let Some(guard) = taken {
                guard.cancel();
            }
            Ok(())
        });
    }

    /// Records a synchronous effect explicitly.
    ///
    /// Prefer the typed helpers on this type; `effect` is the escape hatch for a
    /// registration the kernel does not model.
    pub fn effect<F>(&mut self, label: &'static str, revert: F)
    where
        F: Fn(&Context) -> Result<(), BoxError> + 'static,
    {
        self.disposer.record(label, revert);
    }

    /// Records an asynchronous effect explicitly.
    pub fn effect_async<F, Fut>(&mut self, label: &'static str, revert: F)
    where
        F: Fn(&Context) -> Fut + 'static,
        Fut: Future<Output = Result<(), BoxError>> + 'static,
    {
        self.disposer.record_async(label, revert);
    }

    /// Records an asynchronous effect from an already-boxed body.
    ///
    /// This is the seam a dynamic loader uses, where the closure's concrete type is
    /// not known to the caller.
    pub fn effect_boxed_async(&mut self, label: &'static str, body: crate::AsyncEffectBody) {
        self.disposer
            .record_boxed(Box::new(BoxedAsyncEffect { label, body }));
    }
}

impl core::fmt::Debug for MountContext<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MountContext")
            .field("id", &self.id)
            .field("effects", &self.disposer.len())
            .finish_non_exhaustive()
    }
}

/// An effect built from a boxed asynchronous body.
struct BoxedAsyncEffect {
    label: &'static str,
    body: crate::AsyncEffectBody,
}

impl Effect for BoxedAsyncEffect {
    fn revert(&mut self, cx: &Context) -> Result<(), BoxError> {
        let future = (self.body)(cx);
        crate::runtime::block_on(future)
    }

    fn describe(&self) -> crate::EffectDescription {
        crate::EffectDescription::new(self.label)
    }
}

/// A dispatch-only view of a context.
///
/// A plugin that only produces events does not need the full context; handing it
/// this view keeps the surface it can reach explicit.
#[derive(Clone, Copy)]
pub struct Dispatch<'a> {
    context: &'a Context,
}

impl Dispatch<'_> {
    /// Dispatches an event to every listener through `emit`.
    pub fn emit<K: 'static>(&self, key: EventKey<K>, payload: &K) {
        self.context.emit(key, payload);
    }

    /// Dispatches an event through `waterfall`.
    #[must_use]
    pub fn waterfall<K: 'static>(&self, key: EventKey<K>, payload: K) -> Option<K> {
        self.context.waterfall(key, payload)
    }

    /// Dispatches an event through `parallel`.
    pub fn parallel<K: 'static>(&self, key: EventKey<K>, payload: &K) {
        self.context.parallel(key, payload);
    }

    /// Dispatches an event through `serial`.
    #[must_use]
    pub fn serial<K: 'static, R: 'static>(&self, key: EventKey<K>, payload: &K) -> Option<R> {
        self.context.serial(key, payload)
    }

    /// Dispatches an event through `bail`.
    #[must_use]
    pub fn bail<K: 'static, R: 'static>(&self, key: EventKey<K>, payload: &K) -> Option<R> {
        self.context.bail(key, payload)
    }
}

impl core::fmt::Debug for Dispatch<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Dispatch").finish_non_exhaustive()
    }
}
