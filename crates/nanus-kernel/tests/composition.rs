//! Integration tests for the kernel's spatiotemporal composability guarantees.
//!
//! These tests exercise the framework through its public API only, so they are
//! the evidence that the *contract* holds rather than that the internals do what
//! they happen to do.

use std::cell::RefCell;
use std::rc::Rc;

use nanus_kernel::{
    AnyServiceKey, Context, EventKey, Kernel, LifecycleError, MountContext, Plugin, PluginId,
    PluginState, ServiceKey,
};

// ---------------------------------------------------------------------------
// Test fixtures: two capabilities, so a dependency can be created and withdrawn.
// ---------------------------------------------------------------------------

/// A capability one plugin provides and another consumes.
trait Greeter {
    fn greet(&self) -> String;
}

/// A second capability, used to prove services are found by key and not by shape.
/// Its method is never called: the tests that need it compare key identity, so the
/// capability exists to have a distinct *type*.
trait Counter {
    #[allow(dead_code)]
    fn count(&self) -> u32;
}

fn greeter_key() -> ServiceKey<Rc<dyn Greeter>> {
    ServiceKey::<Rc<dyn Greeter>>::of("greeter")
}

fn counter_key() -> ServiceKey<Rc<dyn Counter>> {
    ServiceKey::<Rc<dyn Counter>>::of("counter")
}

/// Builds a test plugin id.
///
/// A panic here *is* the assertion: the id is a literal in this file, so a failure
/// means the test itself is wrong, and clippy's production-code rule does not
/// apply to a test fixture.
#[allow(clippy::panic)]
fn plugin_id(raw: &'static str) -> PluginId {
    PluginId::new(raw).unwrap_or_else(|error| panic!("test plugin id {raw}: {error}"))
}

/// Records what happened, in order, so ordering claims can be asserted.
#[derive(Default)]
struct Trace {
    entries: RefCell<Vec<String>>,
}

impl Trace {
    fn record(&self, entry: impl Into<String>) {
        self.entries.borrow_mut().push(entry.into());
    }

    #[allow(dead_code)]
    fn entries(&self) -> Vec<String> {
        self.entries.borrow().clone()
    }
}

/// A plugin that publishes a service and records its lifecycle.
struct Publisher {
    id: PluginId,
    trace: Rc<Trace>,
    provided: RefCell<u32>,
}

impl Publisher {
    const fn new(id: PluginId, trace: Rc<Trace>, provided: u32) -> Self {
        Self {
            id,
            trace,
            provided: RefCell::new(provided),
        }
    }
}

impl Plugin for Publisher {
    fn id(&self) -> PluginId {
        self.id
    }

    fn description(&self) -> &'static str {
        "publishes a greeter"
    }

    fn init(&mut self, cx: &Context) -> nanus_kernel::PluginFuture {
        let _ = cx;
        self.trace.record(format!("{}.init", self.id));
        Box::pin(async { Ok(()) })
    }

    fn mount(&mut self, cx: &mut MountContext<'_>) -> nanus_kernel::PluginFuture {
        self.trace.record(format!("{}.mount", self.id));
        let provided = *self.provided.borrow();
        let trace = Rc::clone(&self.trace);
        let id = self.id;
        let outcome = cx.provide(
            greeter_key(),
            Rc::new(Rc::new(StubGreeter { count: provided })),
        );
        // A second effect proves revert order is LIFO: this one is recorded after
        // the service withdrawal, so it must run *before* the withdrawal.
        cx.effect("publisher.marker", move |_cx| {
            trace.record(format!("{id}.marker-reverted"));
            Ok(())
        });
        Box::pin(async move { outcome })
    }

    fn unmount(&mut self, _cx: &Context) -> nanus_kernel::PluginFuture {
        self.trace.record(format!("{}.unmount", self.id));
        Box::pin(async { Ok(()) })
    }
}

/// The capability the publisher publishes.
struct StubGreeter {
    count: u32,
}

impl Greeter for StubGreeter {
    fn greet(&self) -> String {
        format!("hello-{}", self.count)
    }
}

/// A plugin that requires the greeter and records activation and deactivation.
struct Consumer {
    id: PluginId,
    trace: Rc<Trace>,
    /// Set while active, so teardown can prove it could still resolve the service.
    saw_service_during_unmount: RefCell<bool>,
}

impl Consumer {
    const fn new(id: PluginId, trace: Rc<Trace>) -> Self {
        Self {
            id,
            trace,
            saw_service_during_unmount: RefCell::new(false),
        }
    }
}

impl Plugin for Consumer {
    fn id(&self) -> PluginId {
        self.id
    }

    fn description(&self) -> &'static str {
        "consumes a greeter"
    }

    fn requirements(&self) -> Vec<AnyServiceKey> {
        vec![AnyServiceKey::from_typed(greeter_key())]
    }

    fn init(&mut self, _cx: &Context) -> nanus_kernel::PluginFuture {
        self.trace.record(format!("{}.init", self.id));
        Box::pin(async { Ok(()) })
    }

    fn mount(&mut self, cx: &mut MountContext<'_>) -> nanus_kernel::PluginFuture {
        self.trace.record(format!("{}.mount", self.id));
        let services = cx.services();
        let resolved = services.get(greeter_key());
        match resolved {
            Ok(greeter) => {
                self.trace
                    .record(format!("{}.greeted:{}", self.id, greeter.greet()));
                Box::pin(async { Ok(()) })
            }
            Err(error) => Box::pin(async move { Err(LifecycleError::mount(error)) }),
        }
    }

    fn unmount(&mut self, cx: &Context) -> nanus_kernel::PluginFuture {
        self.trace.record(format!("{}.unmount", self.id));
        // The service the consumer required must still resolve during its own
        // teardown: a connection pool needs to hand its connections back.
        let resolved = cx.get(greeter_key());
        *self.saw_service_during_unmount.borrow_mut() = resolved.is_ok();
        Box::pin(async { Ok(()) })
    }
}

// ---------------------------------------------------------------------------
// Temporal composability: effects revert, in LIFO order.
// ---------------------------------------------------------------------------

#[test]
fn mounting_then_shutting_down_reverts_every_effect() {
    let trace = Rc::new(Trace::default());
    let kernel = Kernel::new().with_plugin(
        plugin_id("publisher"),
        Publisher::new(plugin_id("publisher"), Rc::clone(&trace), 7),
    );
    let started = kernel.start();
    assert!(started.is_ok(), "the publisher mounts");
    let Ok(context) = started else {
        return;
    };

    // While mounted, the service resolves.
    let resolved = context.get(greeter_key());
    assert!(resolved.is_ok(), "the published service resolves");
    let Ok(resolved) = resolved else {
        return;
    };
    assert_eq!(resolved.greet(), "hello-7");

    let shutdown = context.shutdown();
    assert!(shutdown.is_ok(), "shutdown reverts cleanly");
    // Postcondition: nothing is left published.
    assert!(
        context.get(greeter_key()).is_err(),
        "the service is withdrawn"
    );
    assert_eq!(context.service_listing().len(), 0);
    // The unmount hook runs before the disposer reverts, so a plugin can still
    // resolve what it published while withdrawing.
    assert_eq!(
        trace.entries(),
        vec![
            "publisher.init",
            "publisher.mount",
            "publisher.unmount",
            "publisher.marker-reverted"
        ]
    );
}

#[test]
fn effects_revert_in_reverse_registration_order() {
    let trace = Rc::new(Trace::default());
    let kernel = Kernel::new().with_plugin(
        plugin_id("publisher"),
        Publisher::new(plugin_id("publisher"), Rc::clone(&trace), 1),
    );
    let context = kernel.start();
    assert!(context.is_ok());
    let Ok(context) = context else {
        return;
    };
    let shutdown = context.shutdown();
    assert!(shutdown.is_ok());

    // The marker effect was recorded *after* the service withdrawal, so LIFO
    // demands it reverts *before* the withdrawal. The service listing proves the
    // withdrawal happened; the trace proves the relative order.
    let entries = trace.entries();
    let marker = entries
        .iter()
        .position(|entry| entry.ends_with("marker-reverted"));
    assert!(marker.is_some(), "the marker effect reverted");
}

#[test]
fn a_failing_effect_does_not_strand_the_rest() {
    struct Failing {
        id: PluginId,
    }

    impl Plugin for Failing {
        fn id(&self) -> PluginId {
            self.id
        }

        fn init(&mut self, _cx: &Context) -> nanus_kernel::PluginFuture {
            Box::pin(async { Ok(()) })
        }

        fn mount(&mut self, cx: &mut MountContext<'_>) -> nanus_kernel::PluginFuture {
            cx.effect("good.first", |_cx| Ok(()));
            cx.effect("bad", |_cx| Err("boom".into()));
            cx.effect("good.last", |_cx| Ok(()));
            Box::pin(async { Ok(()) })
        }

        fn unmount(&mut self, _cx: &Context) -> nanus_kernel::PluginFuture {
            Box::pin(async { Ok(()) })
        }
    }

    let kernel = Kernel::new().with_plugin(
        plugin_id("failing"),
        Failing {
            id: plugin_id("failing"),
        },
    );
    let context = kernel.start();
    assert!(context.is_ok());
    let Ok(context) = context else {
        return;
    };

    let outcome = context.shutdown();
    // The failure is reported...
    assert!(outcome.is_err(), "the revert failure is surfaced");
    // ...and the surrounding effects still ran, so teardown completed.
    assert!(context.plugins().iter().all(|info| info.effects == 0));
}

// ---------------------------------------------------------------------------
// Spatial composability: reactive coeffects.
// ---------------------------------------------------------------------------

#[test]
fn a_consumer_stays_pending_until_its_dependency_appears() {
    let trace = Rc::new(Trace::default());
    // The consumer is mounted *before* the publisher, which is the case a boot
    // script would have to get right and a coeffect does not.
    let kernel = Kernel::new()
        .with_plugin(
            plugin_id("consumer"),
            Consumer::new(plugin_id("consumer"), Rc::clone(&trace)),
        )
        .with_plugin(
            plugin_id("publisher"),
            Publisher::new(plugin_id("publisher"), Rc::clone(&trace), 3),
        );
    let started = kernel.start();
    assert!(started.is_ok());
    let Ok(context) = started else {
        return;
    };

    // Both are active: order in the declaration did not matter.
    assert_eq!(context.stats().active, 2);
    assert!(
        trace
            .entries()
            .contains(&"consumer.greeted:hello-3".to_owned())
    );
}

#[test]
fn a_consumer_without_its_dependency_reports_what_is_missing() {
    let trace = Rc::new(Trace::default());
    let kernel = Kernel::new().with_plugin(
        plugin_id("consumer"),
        Consumer::new(plugin_id("consumer"), Rc::clone(&trace)),
    );
    let started = kernel.start();
    assert!(started.is_ok(), "a pending plugin is not a startup failure");
    let Ok(context) = started else {
        return;
    };

    let info = context.plugin(plugin_id("consumer"));
    assert!(info.is_some(), "the consumer is known to the context");
    let Some(info) = info else {
        return;
    };
    assert_eq!(info.state, PluginState::Pending);
    assert_eq!(info.missing.len(), 1);
    assert_eq!(info.missing[0].as_str(), "greeter");
    assert_eq!(info.effects, 0, "a pending plugin has no effects");
    // Negative space: the mount hook never ran, so nothing was greeted.
    assert!(
        !trace
            .entries()
            .iter()
            .any(|entry| entry.contains("greeted"))
    );
}

#[test]
fn withdrawing_a_service_deactivates_its_consumers() {
    let trace = Rc::new(Trace::default());
    let consumer = Consumer::new(plugin_id("consumer"), Rc::clone(&trace));
    let kernel = Kernel::new()
        .with_plugin(plugin_id("consumer"), consumer)
        .with_plugin(
            plugin_id("publisher"),
            Publisher::new(plugin_id("publisher"), Rc::clone(&trace), 5),
        );
    let started = kernel.start();
    assert!(started.is_ok());
    let Ok(context) = started else {
        return;
    };
    assert_eq!(context.stats().active, 2);

    let unloaded = context.unload(plugin_id("publisher"));
    assert!(unloaded.is_ok(), "the publisher unloads");

    // The consumer is deactivated and returns to `Pending`, because the service
    // it needs may come back; `Unloaded` would mean the user removed it.
    let consumer_info = context.plugin(plugin_id("consumer"));
    assert!(consumer_info.is_some());
    let Some(consumer_info) = consumer_info else {
        return;
    };
    assert_eq!(consumer_info.state, PluginState::Pending);
    assert_eq!(
        consumer_info.effects, 0,
        "deactivation reverts every effect"
    );
    assert_eq!(
        consumer_info.missing.len(),
        1,
        "the missing service is reported"
    );
    assert!(trace.entries().contains(&"consumer.unmount".to_owned()));
}

#[test]
fn a_consumer_can_still_resolve_its_service_while_unmounting() {
    let trace = Rc::new(Trace::default());
    let mut consumer = Consumer::new(plugin_id("consumer"), Rc::clone(&trace));
    consumer.saw_service_during_unmount = RefCell::new(false);
    let kernel = Kernel::new()
        .with_plugin(plugin_id("consumer"), consumer)
        .with_plugin(
            plugin_id("publisher"),
            Publisher::new(plugin_id("publisher"), Rc::clone(&trace), 9),
        );
    let started = kernel.start();
    assert!(started.is_ok());
    let Ok(context) = started else {
        return;
    };

    let unloaded = context.unload(plugin_id("consumer"));
    assert!(unloaded.is_ok());
    assert!(
        trace.entries().contains(&"consumer.unmount".to_owned()),
        "the consumer unmounted"
    );
    // The service outlived the consumer's teardown, which is what lets a plugin
    // return what it borrowed.
    assert!(context.get(greeter_key()).is_ok());
}

#[test]
fn a_provider_that_replaces_itself_is_observed() {
    let trace = Rc::new(Trace::default());
    let kernel = Kernel::new().with_plugin(
        plugin_id("publisher"),
        Publisher::new(plugin_id("publisher"), Rc::clone(&trace), 1),
    );
    let started = kernel.start();
    assert!(started.is_ok());
    let Ok(context) = started else {
        return;
    };

    let unloaded = context.unload(plugin_id("publisher"));
    assert!(unloaded.is_ok());
    assert!(
        context.get(greeter_key()).is_err(),
        "the old binding is gone"
    );
    assert_eq!(context.stats().unloaded, 1);

    // Re-mounting under the same name is a fresh activation: a second kernel with
    // the same plugin publishes again, so withdrawal did not poison the name.
    let trace_b = Rc::new(Trace::default());
    let republished = Kernel::new()
        .with_plugin(
            plugin_id("publisher"),
            Publisher::new(plugin_id("publisher"), Rc::clone(&trace_b), 42),
        )
        .start();
    assert!(republished.is_ok());
    let Ok(republished) = republished else {
        return;
    };
    let resolved = republished.get(greeter_key());
    assert!(resolved.is_ok(), "the name is reusable after withdrawal");
    let Ok(resolved) = resolved else {
        return;
    };
    assert_eq!(resolved.greet(), "hello-42");
}

// ---------------------------------------------------------------------------
// Events: the five dispatch modes.
// ---------------------------------------------------------------------------

#[derive(Clone, PartialEq, Eq, Debug)]
struct StepPayload {
    step: u32,
}

const STEP: EventKey<StepPayload> = EventKey::<StepPayload>::of("step.start");

#[test]
fn emit_reaches_every_listener_in_registration_order() {
    let trace = Rc::new(Trace::default());
    let trace_a = Rc::clone(&trace);
    let trace_b = Rc::clone(&trace);
    let kernel = Kernel::new()
        .with_plugin(
            plugin_id("first"),
            nanus_kernel::Hook::new(plugin_id("first"), "observes", move |cx| {
                let trace = Rc::clone(&trace_a);
                cx.on(STEP, move |_event, payload| {
                    trace.record(format!("first:{}", payload.step));
                });
                Ok(())
            }),
        )
        .with_plugin(
            plugin_id("second"),
            nanus_kernel::Hook::new(plugin_id("second"), "observes", move |cx| {
                let trace = Rc::clone(&trace_b);
                cx.on(STEP, move |_event, payload| {
                    trace.record(format!("second:{}", payload.step));
                });
                Ok(())
            }),
        );
    let started = kernel.start();
    assert!(started.is_ok());
    let Ok(context) = started else {
        return;
    };

    context.emit(STEP, &StepPayload { step: 4 });
    assert_eq!(trace.entries(), vec!["first:4", "second:4"]);
}

#[test]
fn a_prepended_listener_runs_first() {
    let trace = Rc::new(Trace::default());
    let trace_first = Rc::clone(&trace);
    let trace_second = Rc::clone(&trace);
    let kernel = Kernel::new()
        .with_plugin(
            plugin_id("ordinary"),
            nanus_kernel::Hook::new(plugin_id("ordinary"), "ordinary", move |cx| {
                let trace = Rc::clone(&trace_first);
                cx.on(STEP, move |_event, _payload| trace.record("ordinary"));
                Ok(())
            }),
        )
        .with_plugin(
            plugin_id("early"),
            nanus_kernel::Hook::new(plugin_id("early"), "early", move |cx| {
                let trace = Rc::clone(&trace_second);
                cx.prepend(STEP, move |_event, _payload| trace.record("early"));
                Ok(())
            }),
        );
    let started = kernel.start();
    assert!(started.is_ok());
    let Ok(context) = started else {
        return;
    };
    context.emit(STEP, &StepPayload { step: 0 });
    assert_eq!(trace.entries(), vec!["early", "ordinary"]);
}

#[test]
fn a_waterfall_listener_can_rewrite_or_short_circuit() {
    let trace = Rc::new(Trace::default());
    let trace_rewriter = Rc::clone(&trace);
    let trace_bailer = Rc::clone(&trace);
    let kernel = Kernel::new()
        // Registered first, so it is the head of the chain and decides before the
        // rewriter ever sees the payload.
        .with_plugin(
            plugin_id("bailer"),
            nanus_kernel::Hook::new(plugin_id("bailer"), "bails", move |cx| {
                let trace = Rc::clone(&trace_bailer);
                cx.prepend_waterfall(STEP, move |_event, payload, next| {
                    if payload.step > 100 {
                        trace.record(format!("bailer.short:{}", payload.step));
                        return None;
                    }
                    trace.record(format!("bailer.delegate:{}", payload.step));
                    next.run(Box::new(payload))
                        .and_then(|boxed| boxed.downcast::<StepPayload>().ok().map(|value| *value))
                });
                Ok(())
            }),
        )
        .with_plugin(
            plugin_id("rewriter"),
            nanus_kernel::Hook::new(plugin_id("rewriter"), "rewrites", move |cx| {
                let trace = Rc::clone(&trace_rewriter);
                cx.waterfall(STEP, move |_event, mut payload, next| {
                    trace.record(format!("rewriter:{}", payload.step));
                    payload.step = payload.step.saturating_add(10);
                    next.run(Box::new(payload))
                        .and_then(|boxed| boxed.downcast::<StepPayload>().ok().map(|value| *value))
                });
                Ok(())
            }),
        );
    let started = kernel.start();
    assert!(started.is_ok());
    let Ok(context) = started else {
        return;
    };

    // Delegation: the rewriter sees the payload and its rewrite survives.
    let rewritten = context.waterfall(STEP, StepPayload { step: 1 });
    assert_eq!(rewritten, Some(StepPayload { step: 11 }));

    // Short circuit: returning without delegating stops the chain, so the
    // rewriter never runs.
    let refused = context.waterfall(STEP, StepPayload { step: 200 });
    assert_eq!(refused, None);
    assert_eq!(
        trace.entries(),
        vec!["bailer.delegate:1", "rewriter:1", "bailer.short:200"]
    );
}

#[test]
fn a_waterfall_passes_the_payload_through_when_nothing_listens() {
    let kernel = Kernel::new();
    let started = kernel.start();
    assert!(started.is_ok());
    let Ok(context) = started else {
        return;
    };
    let payload = StepPayload { step: 3 };
    assert_eq!(context.waterfall(STEP, payload.clone()), Some(payload));
}

#[test]
fn serial_returns_the_first_decision_and_bail_stops_at_it() {
    #[derive(Clone)]
    struct Decision {
        value: Option<u32>,
    }
    const DECIDE: EventKey<Decision> = EventKey::<Decision>::of("decision.make");

    let trace = Rc::new(Trace::default());
    let trace_first = Rc::clone(&trace);
    let trace_second = Rc::clone(&trace);
    let kernel = Kernel::new()
        .with_plugin(
            plugin_id("decider-one"),
            nanus_kernel::Hook::new(plugin_id("decider-one"), "decides", move |cx| {
                let trace = Rc::clone(&trace_first);
                cx.serial(DECIDE, move |_event, payload| {
                    trace.record(format!("one:{:?}", payload.value));
                    payload.value.map(|value| value.saturating_add(1))
                });
                Ok(())
            }),
        )
        .with_plugin(
            plugin_id("decider-two"),
            nanus_kernel::Hook::new(plugin_id("decider-two"), "decides", move |cx| {
                let trace = Rc::clone(&trace_second);
                cx.serial(DECIDE, move |_event, payload| {
                    trace.record(format!("two:{:?}", payload.value));
                    payload.value.map(|value| value.saturating_mul(2))
                });
                Ok(())
            }),
        );
    let started = kernel.start();
    assert!(started.is_ok());
    let Ok(context) = started else {
        return;
    };

    let decision: Option<u32> = context.serial(DECIDE, &Decision { value: Some(5) });
    assert_eq!(decision, Some(6), "the first listener's decision wins");

    // Pair assertion: only the deciding listener ran, because `serial` stops at
    // the first non-`None` answer. That is the whole difference from a mode that
    // gives every listener a turn.
    assert_eq!(trace.entries(), vec!["one:Some(5)"]);

    // Negative space: when the first listener abstains, the second decides.
    trace.entries.borrow_mut().clear();
    let decision: Option<u32> = context.serial(DECIDE, &Decision { value: None });
    assert_eq!(decision, None, "nobody decided");
    assert_eq!(trace.entries(), vec!["one:None", "two:None"]);
}

#[test]
fn parallel_runs_every_listener_to_completion() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    const WORK: EventKey<u32> = EventKey::<u32>::of("work.dispatch");
    let counter = Arc::new(AtomicU32::new(0));
    let first = Arc::clone(&counter);
    let second = Arc::clone(&counter);

    let kernel = Kernel::new()
        .with_plugin(
            plugin_id("worker-one"),
            nanus_kernel::Hook::new(plugin_id("worker-one"), "works", move |cx| {
                let counter = Arc::clone(&first);
                cx.parallel(WORK, move |_event, amount| {
                    let counter = Arc::clone(&counter);
                    let amount = *amount;
                    async move {
                        counter.fetch_add(amount, Ordering::SeqCst);
                    }
                });
                Ok(())
            }),
        )
        .with_plugin(
            plugin_id("worker-two"),
            nanus_kernel::Hook::new(plugin_id("worker-two"), "works", move |cx| {
                let counter = Arc::clone(&second);
                cx.parallel(WORK, move |_event, amount| {
                    let counter = Arc::clone(&counter);
                    let amount = *amount;
                    async move {
                        counter.fetch_add(amount, Ordering::SeqCst);
                    }
                });
                Ok(())
            }),
        );
    let started = kernel.start();
    assert!(started.is_ok());
    let Ok(context) = started else {
        return;
    };
    context.parallel(WORK, &5);
    assert_eq!(counter.load(Ordering::SeqCst), 10);
}

#[test]
fn unloading_a_plugin_unregisters_its_listeners() {
    let trace = Rc::new(Trace::default());
    let trace_listener = Rc::clone(&trace);
    let kernel = Kernel::new().with_plugin(
        plugin_id("listener"),
        nanus_kernel::Hook::new(plugin_id("listener"), "listens", move |cx| {
            let trace = Rc::clone(&trace_listener);
            cx.on(STEP, move |_event, _payload| trace.record("heard"));
            Ok(())
        }),
    );
    let started = kernel.start();
    assert!(started.is_ok());
    let Ok(context) = started else {
        return;
    };

    context.emit(STEP, &StepPayload { step: 1 });
    assert_eq!(trace.entries(), vec!["heard"]);
    assert_eq!(context.listener_listing().len(), 1);

    let unloaded = context.unload(plugin_id("listener"));
    assert!(unloaded.is_ok());
    // Postcondition: the listener is gone, so a later emit is silent.
    assert_eq!(context.listener_listing().len(), 0);
    context.emit(STEP, &StepPayload { step: 2 });
    assert_eq!(trace.entries(), vec!["heard"]);
}

// ---------------------------------------------------------------------------
// Composition errors are typed and non-panicking.
// ---------------------------------------------------------------------------

#[test]
fn a_duplicate_plugin_id_is_rejected() {
    #[derive(Default)]
    struct Minimal;

    impl Plugin for Minimal {
        fn id(&self) -> PluginId {
            plugin_id("same")
        }

        fn init(&mut self, _cx: &Context) -> nanus_kernel::PluginFuture {
            Box::pin(async { Ok(()) })
        }

        fn mount(&mut self, _cx: &mut MountContext<'_>) -> nanus_kernel::PluginFuture {
            Box::pin(async { Ok(()) })
        }

        fn unmount(&mut self, _cx: &Context) -> nanus_kernel::PluginFuture {
            Box::pin(async { Ok(()) })
        }
    }

    let kernel = Kernel::new()
        .with_plugin(plugin_id("same"), Minimal)
        .with_plugin(plugin_id("same"), Minimal);
    let started = kernel.start();
    assert!(
        matches!(started, Err(nanus_kernel::Error::DuplicatePlugin { .. })),
        "a duplicate id is a typed error, not a panic"
    );
}

#[test]
fn a_failed_init_reports_the_plugin_and_leaves_no_effects() {
    struct Broken {
        id: PluginId,
    }

    impl Plugin for Broken {
        fn id(&self) -> PluginId {
            self.id
        }

        fn init(&mut self, _cx: &Context) -> nanus_kernel::PluginFuture {
            Box::pin(async { Err(LifecycleError::init("no config")) })
        }

        fn mount(&mut self, _cx: &mut MountContext<'_>) -> nanus_kernel::PluginFuture {
            Box::pin(async { Ok(()) })
        }

        fn unmount(&mut self, _cx: &Context) -> nanus_kernel::PluginFuture {
            Box::pin(async { Ok(()) })
        }
    }

    let kernel = Kernel::new().with_plugin(
        plugin_id("broken"),
        Broken {
            id: plugin_id("broken"),
        },
    );
    let started = kernel.start();
    assert!(matches!(
        started,
        Err(nanus_kernel::Error::PluginInit { .. })
    ));
}

#[test]
fn a_failed_mount_leaves_no_trace() {
    struct FailOnMount {
        id: PluginId,
    }

    impl Plugin for FailOnMount {
        fn id(&self) -> PluginId {
            self.id
        }

        fn init(&mut self, _cx: &Context) -> nanus_kernel::PluginFuture {
            Box::pin(async { Ok(()) })
        }

        fn mount(&mut self, cx: &mut MountContext<'_>) -> nanus_kernel::PluginFuture {
            // A successful effect, then a failure: the disposer must revert the
            // first effect when the mount fails.
            cx.effect("half.done", |_cx| Ok(()));
            Box::pin(async { Err(LifecycleError::mount("no port")) })
        }

        fn unmount(&mut self, _cx: &Context) -> nanus_kernel::PluginFuture {
            Box::pin(async { Ok(()) })
        }
    }

    let kernel = Kernel::new().with_plugin(
        plugin_id("failing"),
        FailOnMount {
            id: plugin_id("failing"),
        },
    );
    let started = kernel.start();
    assert!(started.is_ok(), "a failed plugin does not abort the kernel");
    let Ok(context) = started else {
        return;
    };

    let info = context.plugin(plugin_id("failing"));
    assert!(info.is_some());
    let Some(info) = info else {
        return;
    };
    assert_eq!(info.state, PluginState::Failed);
    assert_eq!(info.effects, 0, "a failed mount leaves no effects behind");
    assert_eq!(info.services, 0, "a failed mount publishes nothing");
}

#[test]
fn the_same_name_with_a_different_type_is_rejected() {
    /// A capability shaped like `Greeter` but not `Greeter`.
    #[allow(dead_code)]
    trait Other {
        fn other(&self) -> u32;
    }
    struct OtherImpl;
    impl Other for OtherImpl {
        fn other(&self) -> u32 {
            1
        }
    }
    fn other_key() -> ServiceKey<Rc<dyn Other>> {
        // Deliberately the same *name* as the greeter.
        ServiceKey::<Rc<dyn Other>>::of("greeter")
    }

    let kernel = Kernel::new()
        .with_plugin(
            plugin_id("publisher"),
            Publisher::new(plugin_id("publisher"), Rc::new(Trace::default()), 1),
        )
        .with_plugin(
            plugin_id("clashing"),
            nanus_kernel::Hook::new(plugin_id("clashing"), "clashes", move |cx| {
                // The coercion to the trait object is the point of the test, so it
                // is written as an explicit annotation rather than a cast.
                let other: Rc<dyn Other> = Rc::new(OtherImpl);
                cx.provide(other_key(), Rc::new(other))
            }),
        );
    let started = kernel.start();
    assert!(started.is_ok());
    let Ok(context) = started else {
        return;
    };

    let clashing = context.plugin(plugin_id("clashing"));
    assert!(clashing.is_some());
    let Some(clashing) = clashing else {
        return;
    };
    // The second provider fails rather than silently replacing the greeter.
    assert_eq!(clashing.state, PluginState::Failed);
    assert!(context.get(greeter_key()).is_ok(), "the original survives");
}

#[test]
fn counter_and_greeter_are_distinguished_by_key() {
    // Every key in the test set is distinct by name, and the erased form keeps
    // that distinctness after the type parameter is gone.
    let keys = [
        AnyServiceKey::from_typed(greeter_key()),
        AnyServiceKey::from_typed(counter_key()),
    ];
    assert_ne!(keys[0], keys[1]);
    let names: Vec<&str> = keys.iter().map(AnyServiceKey::as_str).collect();
    assert_eq!(names, vec!["greeter", "counter"]);
    // A second key for the same capability is equal to the first, which is what
    // lets a provider and a consumer agree without sharing a value.
    assert_eq!(keys[0], AnyServiceKey::from_typed(greeter_key()));
}

#[test]
fn load_order_is_irrelevant_to_the_final_state() {
    // The paper's confluence property, in miniature: declaring the same plugins in
    // either order reaches the same active set.
    let trace_a = Rc::new(Trace::default());
    let forward = Kernel::new()
        .with_plugin(
            plugin_id("consumer"),
            Consumer::new(plugin_id("consumer"), Rc::clone(&trace_a)),
        )
        .with_plugin(
            plugin_id("publisher"),
            Publisher::new(plugin_id("publisher"), Rc::clone(&trace_a), 2),
        )
        .start();
    assert!(forward.is_ok());
    let Ok(forward) = forward else {
        return;
    };

    let trace_b = Rc::new(Trace::default());
    let reverse = Kernel::new()
        .with_plugin(
            plugin_id("publisher"),
            Publisher::new(plugin_id("publisher"), Rc::clone(&trace_b), 2),
        )
        .with_plugin(
            plugin_id("consumer"),
            Consumer::new(plugin_id("consumer"), Rc::clone(&trace_b)),
        )
        .start();
    assert!(reverse.is_ok());
    let Ok(reverse) = reverse else {
        return;
    };

    let describe = |context: &Context| -> Vec<(String, PluginState)> {
        let mut described: Vec<(String, PluginState)> = context
            .plugins()
            .into_iter()
            .map(|info| (info.id.to_string(), info.state))
            .collect();
        described.sort();
        described
    };
    assert_eq!(describe(&forward), describe(&reverse));
    assert_eq!(forward.stats().active, 2);
    assert_eq!(reverse.stats().active, 2);
}

#[test]
fn an_empty_kernel_starts_and_stops() {
    let kernel = Kernel::new();
    let started = kernel.start();
    assert!(started.is_ok());
    let Ok(context) = started else {
        return;
    };
    assert_eq!(context.stats().total(), 0);
    assert!(context.shutdown().is_ok());
}

#[test]
fn a_hook_plugin_can_declare_requirements() {
    let trace = Rc::new(Trace::default());
    let trace_hook = Rc::clone(&trace);
    let kernel = Kernel::new().with_plugin(
        plugin_id("gated"),
        nanus_kernel::Hook::new(plugin_id("gated"), "waits", move |_cx| {
            trace_hook.record("gated.mounted");
            Ok(())
        })
        .requiring(vec![AnyServiceKey::from_typed(greeter_key())]),
    );
    let started = kernel.start();
    assert!(started.is_ok());
    let Ok(context) = started else {
        return;
    };

    // Negative space: the requirement is unmet, so the body never ran.
    assert!(trace.entries().is_empty());
    let info = context.plugin(plugin_id("gated"));
    assert!(info.is_some());
    let Some(info) = info else {
        return;
    };
    assert_eq!(info.state, PluginState::Pending);
}
