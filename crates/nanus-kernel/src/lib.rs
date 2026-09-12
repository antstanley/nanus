//! # nanus-kernel
//!
//! A safe-Rust implementation of the [Cordis](https://github.com/cordiverse/cordis)
//! meta-framework: *a meta-framework of spatiotemporal composability*.
//!
//! The kernel is the whole of nanus that is not the agent. It provides four
//! mechanisms, and every part of the harness — the model adapter, the tool
//! registry, the session log, the agent loop, and the terminal UI — is a
//! [plugin](Plugin) mounted on a shared [`Context`]:
//!
//! - **Revertible effects** ([`Effect`], [`Disposer`]) give *temporal*
//!   composability: every mutation a plugin performs on the context is recorded
//!   with its inverse, so unloading a plugin returns the context to its prior
//!   state.
//! - **Reactive coeffects** ([`ServiceKey`], [`Plugin::requirements`]) give
//!   *spatial* composability: a plugin declares the services it needs and the
//!   runtime activates it when they appear and deactivates it when they vanish,
//!   so load order is expressed as a dependency rather than as boot sequencing.
//! - **A service registry** ([`Context::provide`], [`Context::get`]) is the
//!   mechanism both halves share: capabilities are found by key, never by
//!   importing a concrete implementation.
//! - **Typed events** ([`EventKey`], [`Dispatch`]) are the extension points:
//!   `emit` observes, `waterfall` intercepts and rewrites, `parallel` fans out,
//!   `serial` orders decisions, and `bail` stops at the first one.
//!
//! ## Composing an application
//!
//! ```
//! use std::rc::Rc;
//!
//! use nanus_kernel::{Context, MountContext, Plugin, PluginId, PluginFuture, ServiceKey};
//!
//! trait Clock {
//!     fn now_ms(&self) -> u64;
//! }
//!
//! struct FixedClock;
//!
//! impl Clock for FixedClock {
//!     fn now_ms(&self) -> u64 {
//!         0
//!     }
//! }
//!
//! /// A capability is published as a shared handle, so the key names the handle type:
//! /// Rust can only recover a *sized* type from the registry.
//! fn clock_key() -> ServiceKey<Rc<Box<dyn Clock>>> {
//!     ServiceKey::of("clock")
//! }
//!
//! struct ClockPlugin;
//!
//! impl Plugin for ClockPlugin {
//!     fn id(&self) -> PluginId {
//!         PluginId::new("clock").expect("a valid id")
//!     }
//!
//!     fn init(&mut self, _cx: &Context) -> PluginFuture {
//!         Box::pin(async { Ok(()) })
//!     }
//!
//!     fn mount(&mut self, cx: &mut MountContext<'_>) -> PluginFuture {
//!         // The registry publishes an `Rc` of the handle, and the handle is itself
//!         // an `Rc` of the boxed capability.
//!         let port: Rc<Box<dyn Clock>> = Rc::new(Box::new(FixedClock));
//!         let outcome = cx.provide(clock_key(), Rc::new(port));
//!         Box::pin(async move { outcome })
//!     }
//!
//!     fn unmount(&mut self, _cx: &Context) -> PluginFuture {
//!         Box::pin(async { Ok(()) })
//!     }
//! }
//!
//! let context = nanus_kernel::Kernel::new()
//!     .with_plugin(PluginId::new("clock").expect("a valid id"), ClockPlugin)
//!     .start()
//!     .expect("the kernel starts");
//! let clock = context.get(clock_key()).expect("the service resolves");
//! assert_eq!(clock.now_ms(), 0);
//! ```
//!
//! ## Determinism
//!
//! The kernel is single-threaded, like Cordis. Components are not required to be
//! `Send`, futures are local, and dispatch order is registration order. That is
//! what makes teardown order — and therefore temporal composability — a property
//! the runtime can guarantee rather than a convention plugins must respect.
//!
//! ## Style
//!
//! This crate follows Tiger Style: no `unsafe`, no panicking accessors in
//! production paths, two or more assertions per function where a function has a
//! meaningful invariant, and a hard limit of 70 lines and 100 columns.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
// `unwrap_used` and `expect_used` are denied crate-wide. Their *total* relatives —
// `unwrap_or`, `unwrap_or_else`, `unwrap_or_default` — cannot panic, and are how
// this crate states a fallback, so they are allowed by name rather than by
// weakening the blanket lint.
#![allow(
    clippy::unwrap_or_default,
    clippy::manual_unwrap_or,
    clippy::manual_unwrap_or_default
)]
// The internal registry and store types are `pub` so they can be named in the
// crate's own signatures, but live in private modules and are never re-exported,
// which `unreachable_pub` cannot see.
#![allow(clippy::redundant_pub_crate)]
// The registry types are `pub` so they can appear in the crate's own signatures, but
// they live in private modules and are never re-exported. `unreachable_pub` cannot see
// the difference between that and a genuinely orphaned item.
#![allow(unreachable_pub)]

mod context;
mod effect;
mod error;
mod event;
mod ident;
mod plugin;
mod service;

pub mod runtime;

pub use context::{Context, Dispatch, MountContext, Services};
pub use effect::{AsyncEffectBody, Disposer, Effect, EffectDescription};
pub use error::{BoxError, Error, ErrorReport};
pub use event::{Event, EventGuard, EventKey, ListenerInfo, LocalBoxFuture, Next, Priority};
pub use ident::{NAME_MAX_LEN, Name, PluginId, ServiceName, VERSION_MAX_LEN, Version};
pub use plugin::{
    Hook, LifecycleError, Migration, Plugin, PluginFuture, PluginInfo, PluginState, PluginStats,
    Provider, describe_plugin, run_startup_migrations,
};
pub use service::{AnyServiceKey, ServiceInfo, ServiceKey};

use std::cell::RefCell;
use std::rc::Rc;

/// The builder that assembles a kernel from plugins.
///
/// A `Kernel` is a staging area, not a running context: plugins accumulate in
/// declaration order, and [`start`](Kernel::start) mounts them all, activating
/// each one as soon as its declared services exist.
///
/// ```
/// use nanus_kernel::{Kernel, PluginId};
///
/// let kernel = Kernel::new();
/// let _context = kernel.into_context();
/// ```
pub struct Kernel {
    /// Plugins staged for mounting, in declaration order.
    staged: Vec<StagedPlugin>,
    /// Startup migrations, applied in `version` order before any plugin mounts.
    migrations: Vec<Migration>,
    /// Whether `start` has already run.
    started: bool,
}

/// A plugin staged for mounting.
struct StagedPlugin {
    id: PluginId,
    plugin: Rc<RefCell<dyn Plugin>>,
}

impl core::fmt::Debug for Kernel {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Kernel")
            .field("staged", &self.staged.len())
            .field("migrations", &self.migrations.len())
            .field("started", &self.started)
            .finish()
    }
}

impl Kernel {
    /// Creates an empty kernel.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            staged: Vec::new(),
            migrations: Vec::new(),
            started: false,
        }
    }

    /// Stages `plugin` under `id`.
    ///
    /// Staging preserves order but does not mount: mounting happens in
    /// [`start`](Kernel::start), which is what lets a plugin be declared before
    /// the service it requires.
    ///
    /// A duplicate id is **not** rejected here. It is reported by
    /// [`start`](Kernel::start) as [`Error::DuplicatePlugin`], because a duplicate
    /// is a property of the whole composition — two plugins may be staged from
    /// different layers, and only the assembled kernel knows they collide. Staging
    /// is therefore infallible and start is where composition errors surface.
    #[must_use]
    pub fn with_plugin<P: Plugin + 'static>(mut self, id: PluginId, plugin: P) -> Self {
        self.staged.push(StagedPlugin {
            id,
            plugin: Rc::new(RefCell::new(plugin)),
        });
        self
    }

    /// Stages an already-shared plugin.
    #[must_use]
    pub fn with_shared_plugin(mut self, id: PluginId, plugin: Rc<RefCell<dyn Plugin>>) -> Self {
        self.staged.push(StagedPlugin { id, plugin });
        self
    }

    /// Registers a startup migration.
    #[must_use]
    pub fn with_migration(mut self, migration: Migration) -> Self {
        self.migrations.push(migration);
        self
    }

    /// Mounts every staged plugin and returns the running context.
    ///
    /// # Errors
    ///
    /// Returns the first plugin initialization or mount failure. Plugins staged
    /// before the failure remain mounted, because partially applied composition is
    /// inspectable and revertible; a caller that wants atomic startup reverts the
    /// context.
    pub fn start(self) -> Result<Context, Error> {
        let context = self.into_context();
        context.start()?;
        Ok(context)
    }

    /// Builds a context holding the staged plugins without mounting them.
    #[must_use]
    pub fn into_context(self) -> Context {
        let context = Context::new();
        {
            let mut inner = context.inner_mut();
            inner.migrations = self.migrations;
            for staged in self.staged {
                inner.staged.push((staged.id, staged.plugin));
            }
        }
        context
    }
}

impl Default for Kernel {
    fn default() -> Self {
        Self::new()
    }
}
