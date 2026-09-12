//! Revertible effects: the temporal half of spatiotemporal composability.
//!
//! Every mutation a plugin performs on the shared context is recorded as an
//! [`Effect`] whose inverse the runtime holds. When the plugin unloads, its
//! effects revert in reverse order of registration, so the context returns to
//! the state it had before the plugin mounted. A plugin that reverts cleanly
//! leaves no trace.

use core::fmt;
use core::future::Future;

use crate::{BoxError, Context};

/// A single reversible context transformation.
///
/// Implementations capture whatever they need to undo themselves; the runtime
/// never inspects them beyond calling [`revert`](Effect::revert).
pub trait Effect {
    /// Undoes this effect.
    ///
    /// Takes `&mut self` because reverting is a state transition of the effect
    /// itself: an inverse that has run once must not run again.
    ///
    /// # Errors
    ///
    /// Returns an error when the inverse transformation cannot be completed. The
    /// runtime records the failure and continues reverting the remaining effects,
    /// because a partially unwound context is more useful than one abandoned
    /// halfway.
    fn revert(&mut self, cx: &Context) -> Result<(), BoxError>;

    /// Describes this effect for diagnostics and tests.
    fn describe(&self) -> EffectDescription;
}

/// A human-readable label for an effect, used in traces and error messages.
#[derive(Clone, PartialEq, Eq)]
pub struct EffectDescription(&'static str);

impl EffectDescription {
    /// Builds a description from a static label.
    ///
    /// The label must name the effect's *kind*, not its arguments: descriptions
    /// are compared in tests and must not vary run to run.
    #[must_use]
    pub const fn new(label: &'static str) -> Self {
        Self(label)
    }

    /// Returns the label.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        self.0
    }
}

impl fmt::Display for EffectDescription {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl fmt::Debug for EffectDescription {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Effect({})", self.0)
    }
}

/// Adapts a synchronous closure into an [`Effect`].
struct SyncEffect<F> {
    label: &'static str,
    revert: F,
}

impl<F> Effect for SyncEffect<F>
where
    F: Fn(&Context) -> Result<(), BoxError>,
{
    fn revert(&mut self, cx: &Context) -> Result<(), BoxError> {
        (self.revert)(cx)
    }

    fn describe(&self) -> EffectDescription {
        EffectDescription::new(self.label)
    }
}

/// Adapts an asynchronous closure into an [`Effect`].
struct AsyncEffect<F, Fut> {
    label: &'static str,
    revert: F,
    _future: core::marker::PhantomData<fn() -> Fut>,
}

impl<F, Fut> Effect for AsyncEffect<F, Fut>
where
    F: Fn(&Context) -> Fut,
    Fut: Future<Output = Result<(), BoxError>>,
{
    fn revert(&mut self, cx: &Context) -> Result<(), BoxError> {
        // A revert may await: tearing down a provider can require flushing or
        // joining a task. The kernel drives the returned future to completion on
        // its own runtime rather than blocking the current thread.
        let future = (self.revert)(cx);
        crate::runtime::block_on(future)
    }

    fn describe(&self) -> EffectDescription {
        EffectDescription::new(self.label)
    }
}

/// One recorded effect.
///
/// Registration order is the position in the disposer's vector, so no separate
/// counter is needed; LIFO revert is literally "pop".
struct Recorded {
    effect: Box<dyn Effect>,
}

/// A boxed asynchronous revert body.
///
/// Boxed bodies exist for the dynamic-loading seam, where the closure's concrete
/// type is not nameable at the point the effect is recorded.
pub type AsyncEffectBody =
    Box<dyn Fn(&Context) -> core::pin::Pin<Box<dyn Future<Output = Result<(), BoxError>>>>>;

/// The set of effects a single plugin owns, reverted in reverse registration
/// order.
///
/// A [`Disposer`] is created empty by the kernel when a plugin mounts and is
/// discarded once its effects have reverted, so a disposer never outlives the
/// plugin that owns it.
#[must_use = "a Disposer must be reverted; dropping it loses teardown errors"]
pub struct Disposer {
    owner: Option<crate::PluginId>,
    effects: Vec<Recorded>,
}

impl fmt::Debug for Disposer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Disposer")
            .field("owner", &self.owner)
            .field("effects", &self.effects.len())
            .finish_non_exhaustive()
    }
}

impl Disposer {
    /// Creates an empty disposer.
    pub(crate) const fn new() -> Self {
        Self {
            owner: None,
            effects: Vec::new(),
        }
    }

    /// Binds this disposer to the plugin that owns its effects.
    pub(crate) fn set_owner(&mut self, owner: crate::PluginId) {
        // Invariant: a disposer is owned by exactly one plugin.
        assert!(self.owner.is_none(), "disposer ownership is assigned once");
        self.owner = Some(owner);
    }

    /// Returns the owning plugin, if one has been assigned.
    #[must_use]
    pub const fn owner(&self) -> Option<crate::PluginId> {
        self.owner
    }

    /// Returns how many effects are currently recorded.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.effects.len()
    }

    /// Returns `true` when no effects are recorded.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.effects.is_empty()
    }

    /// Returns the descriptions of the recorded effects, in registration order.
    #[must_use]
    pub fn describe(&self) -> Vec<EffectDescription> {
        self.effects
            .iter()
            .map(|recorded| recorded.effect.describe())
            .collect()
    }

    /// Records a synchronous effect.
    ///
    /// The effect reverts when this disposer is reverted, unless
    /// [`revert_one`](Disposer::revert_one) removed it first.
    pub fn record<F>(&mut self, label: &'static str, revert: F)
    where
        F: Fn(&Context) -> Result<(), BoxError> + 'static,
    {
        assert!(!label.is_empty(), "effect label must not be empty");
        self.effects.push(Recorded {
            effect: Box::new(SyncEffect { label, revert }),
        });
    }

    /// Records an asynchronous effect.
    ///
    /// The revert future is driven to completion by the kernel's runtime when
    /// the disposer is reverted.
    pub fn record_async<F, Fut>(&mut self, label: &'static str, revert: F)
    where
        F: Fn(&Context) -> Fut + 'static,
        Fut: Future<Output = Result<(), BoxError>> + 'static,
    {
        assert!(!label.is_empty(), "effect label must not be empty");
        self.effects.push(Recorded {
            effect: Box::new(AsyncEffect {
                label,
                revert,
                _future: core::marker::PhantomData,
            }),
        });
    }

    /// Records an already-boxed asynchronous effect.
    ///
    /// This is the seam a dynamic loader uses, where the closure's concrete type is
    /// not known to the recording call site.
    pub fn record_boxed(&mut self, effect: Box<dyn Effect>) {
        self.effects.push(Recorded { effect });
    }

    /// Reverts the most recently recorded effect, if any.
    ///
    /// This is the primitive that makes a *scoped* registration possible: a
    /// plugin can undo one effect without unloading.
    ///
    /// # Errors
    ///
    /// Returns the effect's own revert failure. The effect is removed from the
    /// disposer even when its revert fails, because retrying a failed inverse is
    /// not generally safe.
    pub fn revert_one(&mut self, cx: &Context) -> Result<(), BoxError> {
        let Some(mut recorded) = self.effects.pop() else {
            return Ok(());
        };
        recorded.effect.revert(cx)
    }

    /// Reverts every recorded effect, newest first, and empties the disposer.
    ///
    /// All effects are attempted even when earlier ones fail. The returned error
    /// summarises the failures; the disposer is left empty either way, so a
    /// second call is a no-op.
    ///
    /// # Errors
    ///
    /// Returns [`crate::Error::Revert`] when at least one effect failed.
    pub fn revert(&mut self, cx: &Context) -> Result<(), crate::Error> {
        // Postcondition: the disposer is empty afterwards, whatever happened.
        let total = self.effects.len();
        let mut failed = 0_usize;
        let mut first: Option<String> = None;
        while let Some(mut recorded) = self.effects.pop() {
            let outcome = recorded.effect.revert(cx);
            if let Err(error) = outcome {
                failed = failed.saturating_add(1);
                tracing::warn!(
                    effect = %recorded.effect.describe(),
                    error = %error,
                    "effect failed to revert"
                );
                if first.is_none() {
                    first = Some(error.to_string());
                }
            }
        }
        assert!(self.effects.is_empty(), "revert drains the disposer");
        if failed == 0 {
            return Ok(());
        }
        Err(crate::Error::Revert {
            failed,
            total,
            first: first.unwrap_or_else(|| "unknown".to_owned()),
        })
    }
}

impl Default for Disposer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use core::cell::RefCell;
    use std::rc::Rc;

    use super::*;
    use crate::Kernel;

    /// Collects the order in which effects reverted.
    type Log = Rc<RefCell<Vec<&'static str>>>;

    fn log_push(log: &Log, label: &'static str) {
        let mut borrowed = log.borrow_mut();
        borrowed.push(label);
    }

    #[test]
    fn reverts_in_reverse_registration_order() {
        let cx = Kernel::new().into_context();
        let log: Log = Rc::new(RefCell::new(Vec::new()));
        let mut disposer = Disposer::new();

        for label in ["a", "b", "c"] {
            let log = Rc::clone(&log);
            disposer.record(label, move |_cx| {
                log_push(&log, label);
                Ok(())
            });
        }

        // Precondition: all three are recorded.
        assert_eq!(disposer.len(), 3);
        let outcome = disposer.revert(&cx);
        assert!(outcome.is_ok());
        // LIFO: the last registered effect reverts first.
        assert_eq!(*log.borrow(), vec!["c", "b", "a"]);
        assert!(disposer.is_empty());
    }

    #[test]
    fn revert_one_is_scoped_and_lifo() {
        let cx = Kernel::new().into_context();
        let log: Log = Rc::new(RefCell::new(Vec::new()));
        let mut disposer = Disposer::new();

        for label in ["a", "b"] {
            let log = Rc::clone(&log);
            disposer.record(label, move |_cx| {
                log_push(&log, label);
                Ok(())
            });
        }

        let popped = disposer.revert_one(&cx);
        assert!(popped.is_ok());
        assert_eq!(*log.borrow(), vec!["b"]);
        // Invariant: the remaining effect survives a scoped revert.
        assert_eq!(disposer.len(), 1);

        let rest = disposer.revert(&cx);
        assert!(rest.is_ok());
        assert_eq!(*log.borrow(), vec!["b", "a"]);
    }

    #[test]
    fn reverting_an_empty_disposer_is_ok() {
        let cx = Kernel::new().into_context();
        let mut disposer = Disposer::new();
        assert!(disposer.is_empty());
        let outcome = disposer.revert(&cx);
        assert!(outcome.is_ok());
        // Pair assertion: reverting twice stays a no-op.
        let again = disposer.revert(&cx);
        assert!(again.is_ok());
    }

    #[test]
    fn a_failing_effect_does_not_strand_the_others() {
        let cx = Kernel::new().into_context();
        let log: Log = Rc::new(RefCell::new(Vec::new()));
        let mut disposer = Disposer::new();

        let log_ok = Rc::clone(&log);
        disposer.record("first", move |_cx| {
            log_push(&log_ok, "first");
            Ok(())
        });
        disposer.record("failing", |_cx| Err("boom".into()));
        let log_last = Rc::clone(&log);
        disposer.record("last", move |_cx| {
            log_push(&log_last, "last");
            Ok(())
        });

        let outcome = disposer.revert(&cx);
        // The failure is reported...
        assert!(matches!(
            outcome,
            Err(crate::Error::Revert {
                failed: 1,
                total: 3,
                ..
            })
        ));
        // ...and the surrounding effects still reverted, in order.
        assert_eq!(*log.borrow(), vec!["last", "first"]);
        // Postcondition: no effect is left recorded, so teardown cannot loop.
        assert!(disposer.is_empty());
    }

    #[test]
    fn describe_reports_registration_order() {
        let mut disposer = Disposer::new();
        disposer.record("alpha", |_cx| Ok(()));
        disposer.record("beta", |_cx| Ok(()));
        let described: Vec<&str> = disposer
            .describe()
            .iter()
            .map(EffectDescription::as_str)
            .collect();
        assert_eq!(described, vec!["alpha", "beta"]);
    }

    #[test]
    fn async_effects_revert() {
        let cx = Kernel::new().into_context();
        let log: Log = Rc::new(RefCell::new(Vec::new()));
        let mut disposer = Disposer::new();

        let log_first = Rc::clone(&log);
        disposer.record_async("sync-effect", move |_cx| {
            let log = Rc::clone(&log_first);
            async move {
                log_push(&log, "sync");
                Ok(())
            }
        });
        let log_second = Rc::clone(&log);
        disposer.record_async("async-effect", move |_cx| {
            let log = Rc::clone(&log_second);
            async move {
                log_push(&log, "async");
                Ok(())
            }
        });

        let outcome = disposer.revert(&cx);
        assert!(outcome.is_ok());
        assert_eq!(*log.borrow(), vec!["async", "sync"]);
    }
}
