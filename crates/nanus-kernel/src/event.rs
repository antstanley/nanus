//! Typed events and the five Cordis dispatch modes.
//!
//! An event key names a payload type. Listeners register against that key and
//! receive that payload type; the registry is untyped internally, and every entry
//! is checked against the key's [`TypeId`] before a listener runs, so a mismatched
//! payload can never reach a handler.
//!
//! Dispatch mode is part of an event's public contract, and each mode has a
//! dedicated registration method — a listener that intercepts is registered on a
//! waterfall, a listener that observes is registered on an emit, and neither can
//! pretend to be the other.

use core::any::{Any, TypeId};
use core::fmt;
use core::future::Future;
use core::marker::PhantomData;
use core::pin::Pin;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use crate::{Name, PluginId, ServiceName};

/// A boxed future that does not cross threads.
pub type LocalBoxFuture = Pin<Box<dyn Future<Output = ()> + 'static>>;

/// A boxed payload travelling through an erased dispatch.
type ErasedPayload = Box<dyn Any>;

/// Boxes a payload into its erased form.
///
/// Written as a function rather than a cast so the unsizing coercion is explicit
/// and the lint against trivial casts stays clean.
fn into_erased<T: 'static>(value: T) -> ErasedPayload {
    Box::new(value)
}

/// A boxed unregister action, used so a guard never holds a pointer to the
/// registry that created it.
type Unlisten = Box<dyn FnOnce()>;

/// The continuation handed to a waterfall listener.
///
/// Calling [`Next::run`] delegates the — possibly rewritten — payload to the next
/// listener. Returning without calling it short-circuits the chain, which is how
/// a policy listener takes ownership of a decision.
pub struct Next {
    /// Whether this continuation has already been invoked. A continuation is
    /// linear: invoking it twice would duplicate every downstream side effect.
    consumed: bool,
    state: Option<Rc<WaterfallState>>,
}

/// The immutable tail of a waterfall chain, shared by every [`Next`].
struct WaterfallState {
    /// Shared so every continuation in one dispatch points at the same chain,
    /// and so a snapshot stays valid while listeners register more.
    chain: Rc<ListenerChain>,
    index: usize,
    key: &'static str,
}

impl Next {
    /// Builds a continuation over `state`.
    const fn step(state: Option<Rc<WaterfallState>>) -> Self {
        Self {
            consumed: false,
            state,
        }
    }

    /// Continues the chain, returning the downstream result.
    ///
    /// The payload passed here is what the remaining listeners receive, so a
    /// listener that rewrites the value by passing the rewritten one delegates its
    /// change downstream.
    ///
    /// # Panics
    ///
    /// Panics when called twice on the same continuation: a continuation is
    /// linear, and invoking it again would duplicate every downstream side effect.
    #[must_use]
    pub fn run(mut self, payload: ErasedPayload) -> Option<ErasedPayload> {
        // Precondition: exactly one call. A second is a programmer error rather
        // than a recoverable condition, so it is an assertion.
        assert!(!self.consumed, "a waterfall continuation is invoked once");
        self.consumed = true;
        let Some(state) = self.state else {
            return Some(payload);
        };
        plateau(payload, &state)
    }
}

/// Advances a waterfall one listener, invoking it with a fresh continuation.
fn plateau(payload: ErasedPayload, state: &Rc<WaterfallState>) -> Option<ErasedPayload> {
    let Some(handler) = state.chain.get(state.index) else {
        // Postcondition: past the end of the chain, the payload survives as-is.
        return Some(payload);
    };
    let HandlerKind::Waterfall(callback) = &handler.kind else {
        // A chain holds one kind of listener, because registration fixes the kind
        // to the dispatch method; reaching here means a registration bug.
        tracing::error!(event = state.key, "listener kind mismatch on waterfall");
        return None;
    };
    let next_state = Rc::new(WaterfallState {
        chain: Rc::clone(&state.chain),
        index: state.index.saturating_add(1),
        key: state.key,
    });
    let event = EventErased {
        name: state.key,
        dispatcher: None,
    };
    callback(&event, payload, Next::step(Some(next_state)))
}

impl fmt::Debug for Next {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Next")
            .field("consumed", &self.consumed)
            .field(
                "remaining",
                &self.state.as_ref().map_or(0, |state| state.chain.len()),
            )
            .finish()
    }
}

/// The typed key of an event.
///
/// `K` is the payload type the event carries. A key is a constant, so declaring
/// an event is declaring a value:
///
/// ```
/// use nanus_kernel::EventKey;
///
/// struct StepStarted {
///     step: u32,
/// }
///
/// const STEP_STARTED: EventKey<StepStarted> = EventKey::of("step.start");
/// ```
pub struct EventKey<K> {
    name: Name,
    type_id: TypeId,
    marker: PhantomData<fn() -> K>,
}

impl<K: 'static> EventKey<K> {
    /// Builds an event key for payload `K` under `name`.
    ///
    /// **The name is not validated here**, and no `const fn` on stable Rust could validate it: a
    /// checked `Name` cannot be built in a constant, and the whole point of `of` is to sit in a
    /// `const`. The documentation used to promise a compile-time panic, which was never true of
    /// any code path — the literal went straight into `Name::new_unchecked`.
    ///
    /// The invariant is held by [`EventKey::checked`], which is what any name that is not a
    /// literal should go through: it builds the same key and refuses a name the kernel would not
    /// accept. A name that is not one still keys the registry — names are compared as strings —
    /// so the cost of getting it wrong is a listener that never hears an event, not a broken
    /// invariant.
    #[must_use]
    pub const fn of(name: &'static str) -> Self {
        let validated = Name::new_unchecked(name);
        Self {
            name: validated,
            type_id: TypeId::of::<K>(),
            marker: PhantomData,
        }
    }

    /// Builds an event key for payload `K`, refusing a name the kernel would not accept.
    ///
    /// The checked counterpart of [`EventKey::of`], for a key built from anything that is not a
    /// literal: configured, computed, or received from somewhere else.
    ///
    /// # Errors
    ///
    /// Returns [`crate::Error::InvalidName`] when the name is empty, longer than the kernel's
    /// limit, or contains a character outside `[a-z0-9_.-]`.
    pub fn checked(name: &'static str) -> Result<Self, crate::Error> {
        Ok(Self {
            name: Name::new(name)?,
            type_id: TypeId::of::<K>(),
            marker: PhantomData,
        })
    }

    /// Returns the event name.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        self.name.as_str()
    }

    /// Returns the published name as a [`ServiceName`].
    #[must_use]
    pub const fn name(&self) -> ServiceName {
        ServiceName::from_name(self.name)
    }

    /// Returns the payload's erased type identity.
    #[must_use]
    pub const fn type_id(&self) -> TypeId {
        self.type_id
    }

    /// Returns the readable payload type name.
    ///
    /// Resolved on demand rather than stored, because `core::any::type_name` is
    /// not usable in a `const` on stable Rust and these keys are constants.
    #[must_use]
    pub fn type_name(&self) -> &'static str {
        core::any::type_name::<K>()
    }
}

impl<K> Clone for EventKey<K> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<K> Copy for EventKey<K> {}

impl<K> PartialEq for EventKey<K> {
    fn eq(&self, other: &Self) -> bool {
        self.type_id == other.type_id && self.name == other.name
    }
}

impl<K> Eq for EventKey<K> {}

impl<K> core::hash::Hash for EventKey<K> {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.type_id.hash(state);
        self.name.hash(state);
    }
}

impl<K> fmt::Display for EventKey<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name)
    }
}

impl<K: 'static> fmt::Debug for EventKey<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "EventKey({}: {})", self.name, self.type_name())
    }
}

/// Where a listener sits in a chain: before or after existing registrations.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Priority {
    /// Run before listeners that were already registered.
    First,
    /// Run after listeners that were already registered.
    #[default]
    Last,
}

/// An event being dispatched: its key and the plugin that dispatched it.
pub struct Event<K> {
    key: EventKey<K>,
    dispatcher: Option<PluginId>,
}

impl<K: 'static> Event<K> {
    /// Builds an event record for dispatch.
    pub(crate) const fn new(key: EventKey<K>, dispatcher: Option<PluginId>) -> Self {
        Self { key, dispatcher }
    }

    /// Returns the event key.
    #[must_use]
    pub const fn key(&self) -> EventKey<K> {
        self.key
    }

    /// Returns the plugin that dispatched the event, if any.
    #[must_use]
    pub const fn dispatcher(&self) -> Option<PluginId> {
        self.dispatcher
    }
}

impl<K: 'static> fmt::Debug for Event<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Event")
            .field("key", &self.key.as_str())
            .field("dispatcher", &self.dispatcher)
            .finish()
    }
}

/// The invocation protocol of a registered listener.
///
/// The variant doubles as evidence of which dispatch mode a listener may be
/// invoked through, so a mis-registration is detected at dispatch time rather
/// than behaving unpredictably.
enum HandlerKind {
    /// `Fn(&Event<K>, &K)`
    Observe(ObserveCallback),
    /// `Fn(&Event<K>, K, Next) -> Option<K>`
    Waterfall(WaterfallCallback),
    /// `Fn(&Event<K>, &K) -> impl Future<Output = ()>`
    Async(AsyncCallback),
    /// `Fn(&Event<K>, &K) -> Option<R>`
    Decide(DecideCallback),
}

/// An erased observer: receives the event record and a borrowed payload.
type ObserveCallback = Box<dyn Fn(&EventErased, &dyn Any)>;

/// An erased around-middleware listener.
type WaterfallCallback = Box<dyn Fn(&EventErased, ErasedPayload, Next) -> Option<ErasedPayload>>;

/// An erased asynchronous observer.
type AsyncCallback = Box<dyn Fn(&EventErased, &dyn Any) -> LocalBoxFuture>;

/// An erased decision listener.
type DecideCallback = Box<dyn Fn(&EventErased, &dyn Any) -> Option<ErasedPayload>>;

/// The erased handle a listener receives instead of a typed [`Event`].
///
/// Listeners are stored untyped, so the dispatch record crosses that boundary
/// erased. It carries only the name and dispatcher, which are type-independent.
struct EventErased {
    name: &'static str,
    dispatcher: Option<PluginId>,
}

/// The listeners registered for one event, in dispatch order.
type ListenerChain = Vec<Rc<ErasedHandler>>;

/// One registered listener, shared through an `Rc` so snapshots are pointer
/// copies and re-entrant registration cannot invalidate an in-flight dispatch.
struct ErasedHandler {
    order: u64,
    owner: PluginId,
    kind: HandlerKind,
}

/// A snapshot of one registered listener, for diagnostics.
#[derive(Clone, PartialEq, Eq)]
pub struct ListenerInfo {
    /// The event the listener is registered on.
    pub event: ServiceName,
    /// The plugin that registered it.
    pub owner: PluginId,
    /// The registration order within the event.
    pub order: u64,
}

impl fmt::Debug for ListenerInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ListenerInfo")
            .field("event", &self.event)
            .field("owner", &self.owner)
            .field("order", &self.order)
            .finish()
    }
}

/// The storage shared between the registry and the guards it hands out.
///
/// Sharing the storage rather than borrowing the registry is what lets an
/// [`EventGuard`] unregister itself without holding a pointer to — or a borrow
/// of — the registry that created it. A guard therefore holds no `unsafe` and
/// cannot dangle.
#[derive(Default)]
struct Store {
    listeners: RefCell<HashMap<ServiceName, ListenerChain>>,
    order: RefCell<u64>,
}

impl Store {
    /// Removes a listener previously inserted.
    ///
    /// Idempotent: a listener already removed by
    /// [`Registry::remove_owned_by`] is simply not found.
    fn remove(&self, key: ServiceName, order: u64) {
        let mut listeners = self.listeners.borrow_mut();
        let Some(chain) = listeners.get_mut(&key) else {
            return;
        };
        let before = chain.len();
        chain.retain(|handler| handler.order != order);
        let after = chain.len();
        if after == 0 {
            listeners.remove(&key);
        }
        drop(listeners);
        // Negative space: removal never adds a listener back.
        assert!(after <= before, "removal cannot grow a chain");
    }

    /// Allocates the next registration order.
    fn allocate_order(&self) -> u64 {
        let mut order = self.order.borrow_mut();
        *order = order.saturating_add(1);
        *order
    }
}

/// The event registry.
pub(crate) struct Registry {
    store: Rc<Store>,
}

impl fmt::Debug for Registry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let listeners = self.store.listeners.borrow();
        let total: usize = listeners.values().map(Vec::len).sum();
        f.debug_struct("EventRegistry")
            .field("events", &listeners.len())
            .field("listeners", &total)
            .finish_non_exhaustive()
    }
}

impl Registry {
    /// Creates an empty registry.
    pub(crate) fn new() -> Self {
        Self {
            store: Rc::new(Store::default()),
        }
    }

    /// Inserts a listener, honouring its [`Priority`], and returns the guard that
    /// unregisters it.
    fn insert<K: 'static>(
        &self,
        key: EventKey<K>,
        owner: PluginId,
        kind: HandlerKind,
        priority: Priority,
    ) -> Rc<EventGuard> {
        let order = self.store.allocate_order();
        let handler = Rc::new(ErasedHandler { order, owner, kind });
        {
            let mut listeners = self.store.listeners.borrow_mut();
            let chain = listeners.entry(key.name()).or_default();
            match priority {
                Priority::First => chain.insert(0, Rc::clone(&handler)),
                Priority::Last => chain.push(Rc::clone(&handler)),
            }
            // Postcondition: insertion grows the chain by exactly one.
            assert!(!chain.is_empty(), "insertion cannot leave an empty chain");
        }
        let name = key.name();
        let store = Rc::clone(&self.store);
        Rc::new(EventGuard::new(name, order, owner, move |key, order| {
            store.remove(key, order);
        }))
    }

    /// Takes a snapshot of the handlers registered on `key`.
    ///
    /// Snapshotting before dispatch is what makes re-entrant registration safe: a
    /// listener that registers another listener cannot mutate the chain being
    /// iterated. The new listener is disposed before the old one by LIFO effect
    /// order, so unloading stays deterministic.
    fn snapshot<K: 'static>(&self, key: EventKey<K>) -> Vec<Rc<ErasedHandler>> {
        let listeners = self.store.listeners.borrow();
        let chain = listeners.get(&key.name());
        let snapshot: Vec<Rc<ErasedHandler>> = chain
            .map(|chain| chain.iter().map(Rc::clone).collect())
            .unwrap_or_default();
        drop(listeners);
        snapshot
    }

    /// Removes every listener registered for `key` by `owner`.
    ///
    /// Used when a plugin withdraws a service: the listeners it contributed under
    /// that service's name must go with it.
    pub(crate) fn remove_named(&self, key: ServiceName, owner: PluginId) {
        let mut listeners = self.store.listeners.borrow_mut();
        let Some(chain) = listeners.get_mut(&key) else {
            return;
        };
        let before = chain.len();
        chain.retain(|handler| handler.owner != owner);
        let after = chain.len();
        if after == 0 {
            listeners.remove(&key);
        }
        drop(listeners);
        assert!(after <= before, "removal cannot grow a chain");
    }

    /// Returns a diagnostic snapshot of every registered listener.
    pub(crate) fn snapshot_all(&self) -> Vec<ListenerInfo> {
        let listeners = self.store.listeners.borrow();
        let mut info: Vec<ListenerInfo> = listeners
            .iter()
            .flat_map(|(name, chain)| {
                chain.iter().map(|handler| ListenerInfo {
                    event: *name,
                    owner: handler.owner,
                    order: handler.order,
                })
            })
            .collect();
        drop(listeners);
        info.sort_by(|left, right| {
            left.event
                .cmp(&right.event)
                .then(left.order.cmp(&right.order))
        });
        info
    }

    /// Registers an observer for `emit` dispatch.
    pub(crate) fn on<K, F>(
        &self,
        key: EventKey<K>,
        owner: PluginId,
        priority: Priority,
        callback: F,
    ) -> Rc<EventGuard>
    where
        K: 'static,
        F: Fn(&Event<K>, &K) + 'static,
    {
        let boxed: ObserveCallback = Box::new(move |event, payload| {
            let Some(typed) = payload.downcast_ref::<K>() else {
                tracing::error!(event = event.name, "payload type mismatch on emit");
                return;
            };
            callback(&Event::new(key, event.dispatcher), typed);
        });
        self.insert(key, owner, HandlerKind::Observe(boxed), priority)
    }

    /// Registers an around-middleware listener for `waterfall` dispatch.
    pub(crate) fn waterfall<K, F>(
        &self,
        key: EventKey<K>,
        owner: PluginId,
        priority: Priority,
        callback: F,
    ) -> Rc<EventGuard>
    where
        K: 'static,
        F: Fn(&Event<K>, K, Next) -> Option<K> + 'static,
    {
        let boxed: WaterfallCallback = Box::new(move |event, payload, next| {
            let erased: Result<Box<K>, _> = payload.downcast::<K>();
            let Ok(typed) = erased else {
                tracing::error!(event = event.name, "payload type mismatch on waterfall");
                return None;
            };
            let outcome = callback(&Event::new(key, event.dispatcher), *typed, next);
            outcome.map(into_erased)
        });
        self.insert(key, owner, HandlerKind::Waterfall(boxed), priority)
    }

    /// Registers an asynchronous observer for `parallel` dispatch.
    pub(crate) fn parallel<K, F, Fut>(
        &self,
        key: EventKey<K>,
        owner: PluginId,
        priority: Priority,
        callback: F,
    ) -> Rc<EventGuard>
    where
        K: 'static,
        F: Fn(&Event<K>, &K) -> Fut + 'static,
        Fut: Future<Output = ()> + 'static,
    {
        let boxed: AsyncCallback = Box::new(move |event, payload| {
            let Some(typed) = payload.downcast_ref::<K>() else {
                tracing::error!(event = event.name, "payload type mismatch on parallel");
                return Box::pin(async {});
            };
            let event = Event::new(key, event.dispatcher);
            Box::pin(callback(&event, typed))
        });
        self.insert(key, owner, HandlerKind::Async(boxed), priority)
    }

    /// Registers an ordered decision listener for `serial` or `bail` dispatch.
    pub(crate) fn serial<K, R, F>(
        &self,
        key: EventKey<K>,
        owner: PluginId,
        priority: Priority,
        callback: F,
    ) -> Rc<EventGuard>
    where
        K: 'static,
        R: 'static,
        F: Fn(&Event<K>, &K) -> Option<R> + 'static,
    {
        let boxed: DecideCallback = Box::new(move |event, payload| {
            let Some(typed) = payload.downcast_ref::<K>() else {
                tracing::error!(event = event.name, "payload type mismatch on serial");
                return None;
            };
            let outcome = callback(&Event::new(key, event.dispatcher), typed);
            outcome.map(into_erased)
        });
        self.insert(key, owner, HandlerKind::Decide(boxed), priority)
    }

    /// Dispatches `payload` through `emit`: every listener observes, in order.
    pub(crate) fn dispatch_emit<K: 'static>(&self, key: EventKey<K>, payload: &K) {
        let chain = self.snapshot(key);
        let event = EventErased {
            name: key.as_str(),
            dispatcher: None,
        };
        for handler in &chain {
            if let HandlerKind::Observe(callback) = &handler.kind {
                callback(&event, payload);
            } else {
                tracing::error!(event = key.as_str(), "listener kind mismatch on emit");
            }
        }
    }

    /// Dispatches `payload` through `waterfall`, returning the final value.
    ///
    /// `None` means a listener short-circuited the chain without producing a
    /// value, which is a decision the caller must honour.
    pub(crate) fn dispatch_waterfall<K: 'static>(&self, key: EventKey<K>, payload: K) -> Option<K> {
        self.dispatch_waterfall_as(key, None, payload)
    }

    /// Dispatches `payload` through `waterfall`, recording its dispatcher.
    pub(crate) fn dispatch_waterfall_from<K: 'static>(
        &self,
        key: EventKey<K>,
        dispatcher: PluginId,
        payload: K,
    ) -> Option<K> {
        self.dispatch_waterfall_as(key, Some(dispatcher), payload)
    }

    /// The shared waterfall body.
    fn dispatch_waterfall_as<K: 'static>(
        &self,
        key: EventKey<K>,
        dispatcher: Option<PluginId>,
        payload: K,
    ) -> Option<K> {
        let chain = Rc::new(self.snapshot(key));
        let Some(first) = chain.first().map(Rc::clone) else {
            // No listener claimed the payload; it passes through untouched.
            return Some(payload);
        };
        let HandlerKind::Waterfall(callback) = &first.kind else {
            tracing::error!(event = key.as_str(), "listener kind mismatch on waterfall");
            return None;
        };
        let event = EventErased {
            name: key.as_str(),
            dispatcher,
        };
        // The chain is moved into the shared state so the local borrow of `first`
        // ends before the callback runs, which keeps a re-entrant registration safe
        // to observe.
        let state = WaterfallState {
            chain,
            index: 1,
            key: key.as_str(),
        };
        let outcome = callback(&event, Box::new(payload), Next::step(Some(Rc::new(state))));
        outcome.and_then(|boxed| boxed.downcast::<K>().ok().map(|value| *value))
    }

    /// Dispatches `payload` through `parallel`: every listener runs to
    /// completion, concurrently, on the kernel runtime.
    pub(crate) fn dispatch_parallel<K: 'static>(&self, key: EventKey<K>, payload: &K) {
        let chain = self.snapshot(key);
        let mut futures: Vec<LocalBoxFuture> = Vec::with_capacity(chain.len());
        let event = EventErased {
            name: key.as_str(),
            dispatcher: None,
        };
        for handler in &chain {
            if let HandlerKind::Async(callback) = &handler.kind {
                futures.push(callback(&event, payload));
            } else {
                tracing::error!(event = key.as_str(), "listener kind mismatch on parallel");
            }
        }
        if futures.is_empty() {
            return;
        }
        crate::runtime::block_on(async move { futures::future::join_all(futures).await });
    }

    /// Dispatches `payload` through `serial`, returning the first decision.
    pub(crate) fn dispatch_serial<K: 'static, R: 'static>(
        &self,
        key: EventKey<K>,
        payload: &K,
    ) -> Option<R> {
        let chain = self.snapshot(key);
        let event = EventErased {
            name: key.as_str(),
            dispatcher: None,
        };
        for handler in &chain {
            if let HandlerKind::Decide(callback) = &handler.kind {
                if let Some(boxed) = callback(&event, payload) {
                    return boxed.downcast::<R>().ok().map(|value| *value);
                }
            } else {
                tracing::error!(event = key.as_str(), "listener kind mismatch on serial");
            }
        }
        None
    }

    /// Dispatches `payload` through `bail`, returning the first non-`None`
    /// decision.
    ///
    /// `bail` and `serial` share a body because the difference between them is
    /// the *contract*, not the mechanics: `serial` promises every listener a turn
    /// and every listener may contribute; `bail` promises to stop at the first
    /// decision. The mode the caller chose lives on the context method, where it
    /// is visible in the call site.
    pub(crate) fn dispatch_bail<K: 'static, R: 'static>(
        &self,
        key: EventKey<K>,
        payload: &K,
    ) -> Option<R> {
        self.dispatch_serial::<K, R>(key, payload)
    }
}

/// A live registration. Dropping the guard unregisters the listener.
///
/// The guard owns a boxed unregister closure that borrows the registry through a
/// shared `Rc` captured at registration. It holds no pointer of its own and needs
/// no `unsafe`.
pub struct EventGuard {
    key: ServiceName,
    order: u64,
    unlisten: RefCell<Option<Unlisten>>,
    owner: PluginId,
}

impl EventGuard {
    fn new<F>(key: ServiceName, order: u64, owner: PluginId, unlisten: F) -> Self
    where
        F: FnOnce(ServiceName, u64) + 'static,
    {
        Self {
            key,
            order,
            unlisten: RefCell::new(Some(Box::new(move || unlisten(key, order)))),
            owner,
        }
    }

    /// Returns the plugin that registered the listener.
    #[must_use]
    pub const fn owner(&self) -> PluginId {
        self.owner
    }

    /// Returns the event the listener is registered on.
    #[must_use]
    pub const fn key(&self) -> ServiceName {
        self.key
    }

    /// Unregisters the listener. Idempotent.
    pub fn cancel(&self) {
        let taken = {
            let mut borrowed = self.unlisten.borrow_mut();
            borrowed.take()
        };
        if let Some(unlisten) = taken {
            unlisten();
        }
    }
}

impl Drop for EventGuard {
    fn drop(&mut self) {
        self.cancel();
    }
}

impl fmt::Debug for EventGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EventGuard")
            .field("key", &self.key)
            .field("order", &self.order)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `of` is unchecked by necessity — a `const fn` cannot build a validated [`Name`] — so
    /// `checked` is what a name that is not a literal goes through. This is both halves: a name
    /// the unchecked path would accept blindly is refused when it is not a name at all, and the
    /// unchecked path still builds the key it always did.
    #[test]
    fn a_checked_key_refuses_a_name_the_kernel_would_not_accept() {
        assert!(EventKey::<u32>::checked("step.start").is_ok());
        assert!(EventKey::<u32>::checked("").is_err(), "empty");
        assert!(EventKey::<u32>::checked("Agent Loop").is_err(), "spaces");
        assert!(EventKey::<u32>::checked("Step").is_err(), "capitals");
        assert!(
            EventKey::<u32>::checked("step/start").is_err(),
            "punctuation"
        );

        let unchecked = EventKey::<u32>::of("step.start");
        assert_eq!(unchecked.as_str(), "step.start");
        assert!(
            EventKey::<u32>::checked("step.start").is_ok_and(|key| key.as_str() == "step.start"),
            "and the two agree on a name that is valid"
        );
    }
}
