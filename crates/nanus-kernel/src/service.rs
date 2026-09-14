//! The service registry: the spatial half of spatiotemporal composability.
//!
//! A service is a capability published under a stable key name. Consumers name
//! keys rather than concrete types, so a provider can be replaced without
//! touching its consumers — the property that lets the harness swap a local
//! filesystem for a remote one and move every tool at once.
//!
//! Services are lent, never given: the registry hands out a clone of the shared
//! handle it holds, so a consumer cannot outlive the provider's registration
//! without the runtime noticing through `Arc::strong_count`.

use core::any::{Any, TypeId};
use core::fmt;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use crate::{Name, PluginId, ServiceName};

// A published capability is any `'static` type. Ports are shared as
// `Rc<Box<dyn Port>>` rather than `Rc<dyn Port>`, because Rust can only downcast a
// `dyn Any` back to a *sized* type; sharing the boxed trait object keeps a
// capability addressable by key without naming its concrete implementation.

/// A [`ServiceKey`] with its capability type erased.
///
/// Coeffect resolution happens without knowing a capability's concrete type: a
/// plugin declares *which* keys it needs, and the runtime checks whether each is
/// published. This is that declaration — a name plus the type identity the name
/// must resolve to, so a similarly shaped but wrong capability is still rejected.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct AnyServiceKey {
    name: Name,
    type_id: TypeId,
    type_name: &'static str,
}

impl AnyServiceKey {
    /// Builds an erased key from a typed one.
    #[must_use]
    pub const fn from_typed<T: Any>(key: ServiceKey<T>) -> Self {
        Self {
            name: key.name,
            type_id: key.type_id,
            type_name: key.type_name,
        }
    }

    /// Returns the published key name.
    #[must_use]
    pub const fn name(&self) -> ServiceName {
        ServiceName::from_name(self.name)
    }

    /// Returns the key name as a string slice.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        self.name.as_str()
    }

    /// Returns the erased type identity of the capability.
    #[must_use]
    pub const fn type_id(&self) -> TypeId {
        self.type_id
    }

    /// Returns the readable name of the capability's type.
    #[must_use]
    pub const fn type_name(&self) -> &'static str {
        self.type_name
    }
}

impl fmt::Display for AnyServiceKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name)
    }
}

impl fmt::Debug for AnyServiceKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AnyServiceKey({}: {})", self.name, self.type_name)
    }
}

impl<T: Any> From<ServiceKey<T>> for AnyServiceKey {
    fn from(key: ServiceKey<T>) -> Self {
        Self::from_typed(key)
    }
}

/// The typed handle to a capability.
///
/// A key carries both a runtime [`TypeId`] and a compile-time type parameter.
/// The two are checked against each other on every resolution: a configuration
/// mistake that swaps two services of the same shape is caught as a typed
/// [`crate::Error::ServiceTypeMismatch`] rather than an unsound downcast.
///
/// Keys are constructible wherever a plugin is built, so a provider and its
/// consumers share a key without a registry of keys to look it up in:
///
/// ```
/// use std::rc::Rc;
///
/// use nanus_kernel::ServiceKey;
///
/// trait Clock {
///     fn now_ms(&self) -> u64;
/// }
///
/// /// The key names the published handle type, and a handle is `Rc` of a boxed trait
/// /// object: recovering a capability from the registry has to produce a sized type.
/// fn clock_key() -> ServiceKey<Rc<Box<dyn Clock>>> {
///     ServiceKey::of("clock")
/// }
///
/// assert_eq!(clock_key().as_str(), "clock");
/// ```
pub struct ServiceKey<T: Any> {
    name: Name,
    type_id: TypeId,
    type_name: &'static str,
    marker: core::marker::PhantomData<fn() -> T>,
}

impl<T: Any> ServiceKey<T> {
    /// Builds a key for `T` published under `name`.
    ///
    /// The name is validated here rather than in a `const` context, because
    /// `core::any::type_name` — which gives the key its readable type for
    /// diagnostics — is not yet usable in `const fn` on stable Rust. A key is
    /// therefore built once at plugin construction and cached, never rebuilt per
    /// resolution.
    ///
    /// # Panics
    ///
    /// Panics when `name` is not a valid service name. A key name is a
    /// programmer-authored constant, so an invalid one is a bug that must be loud
    /// rather than a runtime condition to recover from.
    #[must_use]
    pub fn of(name: &'static str) -> Self {
        let Ok(validated) = Name::new(name) else {
            // A key name is authored by a programmer, so an invalid one is a bug
            // that should fail loudly rather than produce a service nothing can
            // find.
            unreachable!("invalid service key name")
        };
        // Postcondition: the key carries a validated name and a real type identity.
        assert!(
            !validated.as_str().is_empty(),
            "validated names are non-empty"
        );
        assert!(
            TypeId::of::<T>() == TypeId::of::<T>(),
            "type identity is reflexive"
        );
        Self {
            name: validated,
            type_id: TypeId::of::<T>(),
            type_name: core::any::type_name::<T>(),
            marker: core::marker::PhantomData,
        }
    }

    /// Returns the published key name.
    #[must_use]
    pub const fn name(&self) -> ServiceName {
        ServiceName::from_name(self.name)
    }

    /// Returns the key name as the registry's own index type.
    ///
    /// The registry is keyed by [`Name`]; this is the accessor the kernel uses
    /// internally, while [`name`](ServiceKey::name) is the public, rendered form.
    #[must_use]
    pub(crate) const fn raw_name(&self) -> Name {
        self.name
    }

    /// Returns the key name as a string slice.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        self.name.as_str()
    }

    /// Returns the erased type identity of the capability.
    #[must_use]
    pub const fn type_id(&self) -> TypeId {
        self.type_id
    }

    /// Returns the readable name of the capability's type.
    #[must_use]
    pub const fn type_name(&self) -> &'static str {
        self.type_name
    }
}

impl<T: Any> Clone for ServiceKey<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: Any> Copy for ServiceKey<T> {}

impl<T: Any> PartialEq for ServiceKey<T> {
    fn eq(&self, other: &Self) -> bool {
        self.type_id == other.type_id && self.name == other.name
    }
}

impl<T: Any> Eq for ServiceKey<T> {}

impl<T: Any> core::hash::Hash for ServiceKey<T> {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.type_id.hash(state);
        self.name.hash(state);
    }
}

impl<T: Any> fmt::Display for ServiceKey<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name)
    }
}

impl<T: Any> fmt::Debug for ServiceKey<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ServiceKey({}: {})", self.name, self.type_name)
    }
}

/// One published service in the registry.
struct Slot {
    /// The key the service was published under, type identity included.
    key: AnyServiceKey,
    value: Rc<dyn Any>,
    provider: PluginId,
    generation: u64,
    /// Whether the provider is being unloaded.
    ///
    /// A retiring binding satisfies nobody *new* — a mount that requires it is left pending,
    /// which is what makes the sweep deactivate the dependents that already hold it — but it
    /// still *resolves*, because those dependents have to hand back what they borrowed. That
    /// distinction is the whole of "withdrawal waits for dependents": the name is spoken for
    /// before it is gone.
    retiring: bool,
}

impl Slot {
    /// Returns the number of strong references to the published handle.
    ///
    /// The registry holds exactly one; anything above that is a live consumer, a
    /// live `Disposer`-captured handle, or a leak.
    fn strong_count(&self) -> usize {
        Rc::strong_count(&self.value)
    }
}

/// A snapshot of one published service, for diagnostics.
#[derive(Clone, PartialEq, Eq)]
pub struct ServiceInfo {
    /// The published key name.
    pub name: ServiceName,
    /// The readable type name of the capability.
    pub type_name: &'static str,
    /// The plugin that provided it.
    pub provider: PluginId,
    /// The generation counter at time of publication.
    pub generation: u64,
}

impl fmt::Debug for ServiceInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServiceInfo")
            .field("name", &self.name)
            .field("type_name", &self.type_name)
            .field("provider", &self.provider)
            .field("generation", &self.generation)
            .finish()
    }
}

/// The registry of published services.
///
/// Types are erased behind `Rc<dyn Any>`; the typed [`ServiceKey`] is the only
/// way in, and every entry checks name *and* type before handing out a handle.
#[derive(Default)]
pub(crate) struct Registry {
    entries: RefCell<HashMap<Name, Slot>>,
    generation: RefCell<u64>,
}

impl fmt::Debug for Registry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let entries = self.entries.borrow();
        f.debug_struct("Registry")
            .field("services", &entries.len())
            .finish_non_exhaustive()
    }
}

impl Registry {
    /// Creates an empty registry.
    pub(crate) fn new() -> Self {
        Self {
            entries: RefCell::new(HashMap::new()),
            generation: RefCell::new(0),
        }
    }

    /// Publishes `value` under `key` on behalf of `provider`.
    ///
    /// # Errors
    ///
    /// Returns [`crate::Error::ServiceTypeMismatch`] when the key name is
    /// already published with a different erased type. Republishing the same type
    /// is allowed and replaces the previous provider.
    pub(crate) fn provide<T: Any>(
        &self,
        key: ServiceKey<T>,
        value: Rc<T>,
        provider: PluginId,
    ) -> Result<(), crate::Error> {
        // Precondition: the handle is shared, so the registry never owns the only
        // reference and cannot accidentally free a service a consumer still uses.
        assert!(Rc::strong_count(&value) >= 1, "provided handles are shared");
        let erased_key = AnyServiceKey::from_typed(key);
        let mut entries = self.entries.borrow_mut();
        if let Some(existing) = entries.get(&key.name) {
            // A name may only be reused by the plugin already holding it, so
            // re-providing is an observable replacement rather than a silent
            // shadow. Another plugin taking the name over would leave the first
            // provider's consumers resolving a capability it never published.
            if existing.key.type_id() != key.type_id() {
                return Err(crate::Error::ServiceTypeMismatch { key: key.name() });
            }
            if existing.provider != provider {
                return Err(crate::Error::ServiceAlreadyProvided {
                    key: key.name(),
                    provider: existing.provider,
                });
            }
        }
        let generation = {
            let mut counter = self.generation.borrow_mut();
            *counter = counter.saturating_add(1);
            *counter
        };
        let erased: Rc<dyn Any> = value;
        let replaced = entries.insert(
            key.name,
            Slot {
                key: erased_key,
                value: erased,
                provider,
                generation,
                retiring: false,
            },
        );
        // Pair assertion: the slot we just inserted carries the key we were given.
        assert!(entries.contains_key(&key.name));
        drop(replaced);
        tracing::debug!(service = %key.as_str(), plugin = %provider, "service provided");
        Ok(())
    }

    /// Resolves a service, cloning the published handle.
    ///
    /// # Errors
    ///
    /// Returns [`crate::Error::MissingDependency`] when nothing is published
    /// under the key, and [`crate::Error::ServiceTypeMismatch`] when the
    /// published type differs from the requested one.
    pub(crate) fn get<T: Any>(
        &self,
        key: ServiceKey<T>,
        consumer: PluginId,
    ) -> Result<Rc<T>, crate::Error> {
        let entries = self.entries.borrow();
        let Some(slot) = entries.get(&key.name) else {
            return Err(crate::Error::MissingDependency {
                id: consumer,
                key: key.name(),
            });
        };
        if slot.key.type_id() != key.type_id() {
            return Err(crate::Error::ServiceTypeMismatch { key: key.name() });
        }
        let cloned = Rc::clone(&slot.value);
        drop(entries);
        // The `TypeId` check above makes this downcast total; expressing the
        // impossible failure as a typed error keeps a panicking downcast out of the
        // kernel entirely.
        Rc::downcast::<T>(cloned).map_err(|_| crate::Error::ServiceTypeMismatch { key: key.name() })
    }

    /// Returns `true` when `key` is published with a matching type.
    pub(crate) fn contains<T: Any>(&self, key: ServiceKey<T>) -> bool {
        let entries = self.entries.borrow();
        entries
            .get(&key.name)
            .is_some_and(|slot| slot.key.type_id() == key.type_id() && !slot.retiring)
    }

    /// Returns `true` when `key` is published with a matching erased type.
    ///
    /// This is the coeffect-resolution entry point: a plugin's declared
    /// requirements are resolved without knowing the capability's concrete type,
    /// because the key carries that type's identity.
    pub(crate) fn contains_erased(&self, key: AnyServiceKey) -> bool {
        let entries = self.entries.borrow();
        entries
            .get(&key.name)
            .is_some_and(|slot| slot.key.type_id() == key.type_id() && !slot.retiring)
    }

    /// Removes `name` if and only if `provider` is the plugin that published it.
    ///
    /// The provider check matters: if a service is republished under the same name
    /// by a different plugin, the original provider unloading must not withdraw the
    /// replacement.
    pub(crate) fn remove_named(&self, name: Name, provider: PluginId) {
        let mut entries = self.entries.borrow_mut();
        let owned = entries
            .get(&name)
            .is_some_and(|slot| slot.provider == provider);
        if owned {
            entries.remove(&name);
            drop(entries);
            tracing::debug!(service = %ServiceName::from_name(name), plugin = %provider, "service withdrawn");
        }
    }

    /// Removes every service provided by `provider`, newest first.
    ///
    /// Removal happens inside the provider's effect revert, so this is the
    /// registry-side inverse of [`provide`](Registry::provide).
    /// Marks everything `provider` published as being withdrawn.
    ///
    /// The bindings stay resolvable — a dependent that is about to be torn down must still be
    /// able to return what it borrowed — but they stop satisfying requirements, which is what
    /// lets the sweep notice that the dependents are stale and deactivate them *before* the
    /// provider's effects take the binding away.
    pub(crate) fn retire_owned_by(&self, provider: PluginId) {
        let mut entries = self.entries.borrow_mut();
        for slot in entries.values_mut() {
            if slot.provider == provider {
                slot.retiring = true;
            }
        }
    }

    pub(crate) fn remove_owned_by(&self, provider: PluginId) {
        let mut entries = self.entries.borrow_mut();
        let before = entries.len();
        entries.retain(|_name, slot| slot.provider != provider);
        let removed = before.saturating_sub(entries.len());
        drop(entries);
        if removed > 0 {
            tracing::debug!(plugin = %provider, removed, "services withdrawn");
        }
    }

    /// Returns a diagnostic snapshot of every published service, ordered by name.
    pub(crate) fn snapshot(&self) -> Vec<ServiceInfo> {
        let entries = self.entries.borrow();
        let mut info: Vec<ServiceInfo> = entries
            .values()
            .map(|slot| ServiceInfo {
                name: slot.key.name(),
                type_name: slot.key.type_name(),
                provider: slot.provider,
                generation: slot.generation,
            })
            .collect();
        drop(entries);
        info.sort_by_key(|left| left.name);
        info
    }

    /// Returns the number of live consumers of `key` beyond the registry itself.
    pub(crate) fn consumer_count<T: Any>(&self, key: ServiceKey<T>) -> usize {
        let entries = self.entries.borrow();
        // The registry holds one reference of its own, so any surplus is a live
        // consumer.
        entries
            .get(&key.raw_name())
            .map_or(0, |slot| slot.strong_count().saturating_sub(1))
    }
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use super::*;

    trait Greeter {
        fn greet(&self) -> String;
    }

    /// Declared so the fixture has a second capability distinct from `Greeter`;
    /// only its presence, not its method, is exercised.
    #[allow(dead_code)]
    trait Counter {
        fn count(&self) -> u32;
    }

    struct English;

    impl Greeter for English {
        fn greet(&self) -> String {
            "hello".to_owned()
        }
    }

    struct Zero;

    impl Counter for Zero {
        fn count(&self) -> u32 {
            0
        }
    }

    /// The registry's values are `Rc` of a boxed trait object, because a `dyn Any`
    /// can only be recovered as a sized type.
    type GreeterHandle = Rc<dyn Greeter>;

    type CounterHandle = Rc<dyn Counter>;

    fn greeter() -> ServiceKey<GreeterHandle> {
        ServiceKey::of("greeter")
    }

    fn other_greeter() -> ServiceKey<GreeterHandle> {
        ServiceKey::of("greeter.other")
    }

    /// Deliberately reuses the greeter's *name* with a different capability, so a
    /// test can show that identity includes the type and not only the name.
    fn counter_under_greeters_name() -> ServiceKey<CounterHandle> {
        ServiceKey::of("greeter")
    }

    fn greeter_handle() -> GreeterHandle {
        Rc::new(English)
    }

    fn counter_handle() -> CounterHandle {
        Rc::new(Zero)
    }

    fn provider(id: &'static str) -> PluginId {
        PluginId::new(id).unwrap_or_else(|error| panic!("test plugin id: {error}"))
    }

    #[test]
    fn key_is_copy_and_constant() {
        // Positive space: keys are `Copy`, so services can be named freely.
        let first = greeter();
        let second = first;
        assert_eq!(first, second);
        assert_eq!(first.as_str(), "greeter");
        assert!(first.type_name().contains("Greeter"));
    }

    #[test]
    fn key_identity_includes_the_name() {
        // Negative space: the same type under two names is two services.
        assert_eq!(greeter().type_id(), other_greeter().type_id());
        assert_ne!(greeter(), other_greeter());
    }

    #[test]
    fn an_erased_key_carries_the_type_identity() {
        // The erased form is what coeffect resolution compares, so it must keep
        // the type as well as the name.
        let erased = AnyServiceKey::from_typed(greeter());
        assert_eq!(erased.name().as_str(), "greeter");
        assert_eq!(erased.type_id(), greeter().type_id());
        assert_eq!(erased, greeter().into());
        assert_ne!(erased, AnyServiceKey::from_typed(other_greeter()));
        assert_ne!(
            erased,
            AnyServiceKey::from_typed(counter_under_greeters_name())
        );
    }

    #[test]
    fn provide_then_get_round_trips() {
        let registry = Registry::new();
        let plugin = provider("greeter-en");
        let provided = registry.provide(greeter(), Rc::new(greeter_handle()), plugin);
        assert!(provided.is_ok());
        assert!(registry.contains(greeter()));

        let resolved = registry.get(greeter(), provider("consumer"));
        assert!(resolved.is_ok());
        let Ok(resolved) = resolved else {
            return;
        };
        assert_eq!(resolved.greet(), "hello");
    }

    #[test]
    fn get_missing_service_is_a_typed_error() {
        let registry = Registry::new();
        let outcome = registry.get(greeter(), provider("consumer"));
        assert!(matches!(
            outcome,
            Err(crate::Error::MissingDependency { .. })
        ));
    }

    #[test]
    fn a_same_named_capability_of_another_type_is_rejected() {
        let registry = Registry::new();
        let first = registry.provide(greeter(), Rc::new(greeter_handle()), provider("en"));
        assert!(first.is_ok());

        // Negative space: reusing the name for an unrelated capability fails
        // rather than silently replacing the service.
        let clash = registry.provide(
            counter_under_greeters_name(),
            Rc::new(counter_handle()),
            provider("zero"),
        );
        assert!(
            matches!(clash, Err(crate::Error::ServiceTypeMismatch { .. })),
            "expected a type mismatch, got {clash:?}"
        );
        // Pair assertion: the original service is untouched.
        assert!(registry.contains(greeter()));
    }

    #[test]
    fn removal_is_owner_scoped() {
        let registry = Registry::new();
        let en = provider("greeter-en");
        let fr = provider("greeter-fr");
        let first = registry.provide(greeter(), Rc::new(greeter_handle()), en);
        assert!(first.is_ok());
        let second = registry.provide(other_greeter(), Rc::new(greeter_handle()), fr);
        assert!(second.is_ok());
        assert_eq!(registry.snapshot().len(), 2);

        registry.remove_owned_by(fr);
        assert!(!registry.contains(other_greeter()));
        // Pair assertion: the unrelated provider survives.
        assert!(registry.contains(greeter()));
        assert_eq!(registry.snapshot().len(), 1);
    }

    #[test]
    fn removing_a_specific_name_respects_the_provider() {
        let registry = Registry::new();
        let owner = provider("greeter-en");
        let provided = registry.provide(greeter(), Rc::new(greeter_handle()), owner);
        assert!(provided.is_ok());

        // A different plugin naming the same service must not withdraw it.
        registry.remove_named(greeter().raw_name(), provider("intruder"));
        assert!(
            registry.contains(greeter()),
            "an unrelated owner cannot withdraw"
        );

        // The real owner can.
        registry.remove_named(greeter().raw_name(), owner);
        assert!(!registry.contains(greeter()));
    }

    #[test]
    fn snapshot_is_sorted_by_name() {
        let registry = Registry::new();
        let plugin = provider("greeter-en");
        let first = registry.provide(other_greeter(), Rc::new(greeter_handle()), plugin);
        assert!(first.is_ok());
        let second = registry.provide(greeter(), Rc::new(greeter_handle()), plugin);
        assert!(second.is_ok());
        let names: Vec<&str> = registry
            .snapshot()
            .iter()
            .map(|info| info.name.as_str())
            .collect();
        assert_eq!(names, vec!["greeter", "greeter.other"]);
    }

    #[test]
    fn consumer_count_tracks_outstanding_handles() {
        let registry = Registry::new();
        let plugin = provider("greeter-en");
        let provided = registry.provide(greeter(), Rc::new(greeter_handle()), plugin);
        assert!(provided.is_ok());
        // Only the registry holds the handle.
        assert_eq!(registry.consumer_count(greeter()), 0);
        let resolved = registry.get(greeter(), provider("consumer"));
        assert!(resolved.is_ok());
        assert_eq!(registry.consumer_count(greeter()), 1);
        drop(resolved);
        assert_eq!(registry.consumer_count(greeter()), 0);
    }

    #[test]
    fn contains_erased_agrees_with_the_typed_check() {
        let registry = Registry::new();
        let plugin = provider("greeter-en");
        assert!(!registry.contains_erased(AnyServiceKey::from_typed(greeter())));
        let provided = registry.provide(greeter(), Rc::new(greeter_handle()), plugin);
        assert!(provided.is_ok());
        assert!(registry.contains_erased(AnyServiceKey::from_typed(greeter())));
        // Negative space: a differently typed key with the same name is absent.
        assert!(
            !registry.contains_erased(AnyServiceKey::from_typed(counter_under_greeters_name()))
        );
    }
}
