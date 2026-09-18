//! The composition contract: awaiting is legal inside a runtime, mounting is not.
//!
//! This test exists because of a real bug. `compose` used to be `async` and to mount
//! the kernel itself, so calling it from an async test nested the kernel's own
//! `block_on` inside the runtime and panicked with "cannot start a runtime from within
//! a runtime". Every command in the CLI hit it.
//!
//! The fix split the work into two phases: [`compose`] awaits, and
//! [`nanus_bundle::compose::Pending::start`] blocks. The property this test pins is
//! that the split holds — that the whole flow works when driven the way the binaries
//! drive it, and that mounting from *inside* a runtime still fails, so nobody
//! "simplifies" the two phases back into one.
//!
//! Composing a harness requires an API key, so these tests **re-execute themselves**
//! with one set. `std::env::set_var` is `unsafe` in edition 2024 and this workspace
//! forbids `unsafe` even in tests, and a test that depended on the caller's environment
//! would fail in CI, so the child-process re-execution is the way through. No request is
//! ever made: the key is a placeholder that never leaves the process.

// A panic is the assertion in the negative case, a fixture with no sane default has
// nowhere else to put the failure, and the parent of a re-executed test must stop rather
// than run the body a second time.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used, clippy::exit)]

use nanus_adapter_config::NanusConfig;
use nanus_bundle::Selection;
use nanus_bundle::compose::{Pending, compose};
use nanus_domain::AgentConfig;

/// The variable the adapter reads a key from.
const KEY_ENV: &str = "DEEPSEEK_API_KEY";

/// The marker the re-executed child looks for, so it never forks again.
const CHILD_MARKER: &str = "NANUS_COMPOSITION_CHILD";

/// The variable the session store resolves its home from.
const HOME_ENV: &str = "NANUS_HOME";

/// The placeholder key. It is never sent anywhere.
const PLACEHOLDER_KEY: &str = "composition-test-key";

/// Builds a configuration rooted at `root`.
fn config(root: &std::path::Path) -> NanusConfig {
    NanusConfig {
        workspace_root: Some(root.to_path_buf()),
        // A session store under the temporary directory keeps the test hermetic; the
        // default would write into the user's real home.
        ..NanusConfig::default()
    }
}

/// Runs `body` in a child process that has an API key set.
///
/// The child is this same test binary, filtered to `name`, so the test body runs
/// exactly once — in the child. Re-executing rather than mutating the environment is
/// forced by two constraints: `set_var` is `unsafe` in edition 2024, this workspace
/// forbids `unsafe`, and a test that required the caller to export a key would fail in
/// CI.
///
/// The scratch home the child writes into is named for `name` as well as the parent, because the
/// tests in this file run beside each other and each parent deletes its own child's home when it
/// returns.
fn in_child_process(name: &str) {
    if std::env::var(CHILD_MARKER).is_ok() {
        // Already the child: nothing to do. Its home was pointed at a scratch directory
        // by the parent, so nothing it writes reaches the real one.
        return;
    }
    // A harness home of its own, and one per test: the tests in this file run beside each other,
    // and a home named for the *parent* process alone is shared by every child, so the first test
    // to finish would delete the directory another child was still writing sessions into.
    let home =
        std::env::temp_dir().join(format!("nanus-composition-{}-{name}", std::process::id()));
    let exe = std::env::current_exe().expect("the test binary has a path");
    let status = std::process::Command::new(exe)
        // Exact match, so the marker does not leak into sibling tests.
        .arg("--exact")
        .arg(name)
        .arg("--nocapture")
        .env(KEY_ENV, PLACEHOLDER_KEY)
        .env(CHILD_MARKER, "1")
        // A harness home of its own. Without this the test writes sessions into the
        // developer's real store, which is a side effect a test must never have.
        .env(HOME_ENV, &home)
        .status()
        .expect("the child test binary runs");
    // The child is gone, so its home is inert.
    let _cleaned = std::fs::remove_dir_all(&home);
    // The parent asserts on the child's status and then stops; running the body here as
    // well would duplicate the work and defeat the isolation.
    assert!(status.success(), "the child run of {name} passed");
    std::process::exit(0);
}

#[test]
fn the_pending_split_mounts_and_runs_a_turn() {
    in_child_process("the_pending_split_mounts_and_runs_a_turn");
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path();
    let settings = config(root);

    // Phase 1: await the adapters. This is what the binaries do inside their runtime.
    // The test's own runtime is the one `compose` awaits on.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let pending: Pending = runtime
        .block_on(compose(&settings))
        .expect("the composition builds without a network call");

    // Phase 2: mount outside the runtime, which is where the kernel's `block_on` is
    // legal. Doing this was the bug.
    let harness = pending.start().expect("the composition mounts");

    // The kernel published everything the harness needs.
    let stats = harness.context.stats();
    assert_eq!(
        stats.active, 7,
        "clock, fs, shell, store, llm, secrets and tools are active"
    );
    assert_eq!(stats.failed, 0, "nothing failed to mount");
    assert_eq!(harness.tool_count(), 7, "the shipped toolset is published");

    // The model id is the resolved one, and the loop is constructed. Resolution is
    // what turns an absent `model` into the provider's own default, so the two are
    // asserted together: the adapter talks to what the configuration resolved to.
    let selection = Selection::resolve(&settings).expect("the configuration resolves");
    assert_eq!(harness.switch.model(), selection.model());
    assert_eq!(harness.llm().model(), selection.model());
    assert!(harness.configured(), "the key was found");

    // The prompt the model is sent describes this deployment: the workspace the tools are
    // rooted in, and the two permission knobs the gate enforces. The domain renders this
    // text; the failure this asserts against is the bundle never asking it to.
    let prompt = harness.runner.system_prompt();
    assert!(prompt.contains("Approval policy: per_call"), "{prompt}");
    assert!(prompt.contains("Sandbox: read_only"), "{prompt}");
    assert!(
        prompt.contains(&root.display().to_string()),
        "the prompt names the workspace root: {prompt}"
    );
    assert!(prompt.contains(selection.model()), "{prompt}");

    // A session can be created and saved, which is the CLI's post-run step.
    let session = harness.new_session(root);
    // And it is stamped with the configuration the harness will actually run under, taken
    // from the composition rather than restated by the caller. A session that could not say
    // which model produced it is one no two runs can be compared through.
    let origin = session
        .origin()
        .expect("a composed session records its origin");
    assert_eq!(origin.model.as_deref(), Some(selection.model()));
    assert_eq!(
        origin.approval.as_deref(),
        Some(settings.approval_policy.as_str())
    );
    assert_eq!(
        origin.sandbox.as_deref(),
        Some(settings.sandbox_mode.as_str())
    );
    assert_eq!(
        origin.effort.as_deref(),
        Some(settings.reasoning_effort.to_port().as_str()),
        "the effort comes from the adapter, which is what fills in an unset one"
    );
    assert!(
        origin
            .harness
            .as_deref()
            .is_some_and(|version| version.starts_with("nanus/")),
        "the release that wrote it is recorded: {origin:?}"
    );

    let recorded = runtime.block_on(harness.store.save(&session));
    assert!(recorded.is_ok(), "the session is persisted: {recorded:?}");
    // Read back from the store rather than from memory: the stamp has to survive the file,
    // which is the only copy there is.
    let reloaded = runtime
        .block_on(harness.store.load(session.id()))
        .expect("the session reads back");
    assert_eq!(reloaded.origin(), Some(origin));

    // And the composition tears down cleanly, reverting every effect.
    let shutdown = harness.shutdown();
    assert!(shutdown.is_ok(), "teardown reverts cleanly: {shutdown:?}");
}

/// A tool registered through one door is visible through the other.
///
/// The runner used to be built over its own `ToolRegistry` while the plugin published a
/// second one built from the same ports. Both held the same seven tools, so nothing looked
/// wrong until something registered an eighth: the count an agent advertises — which is
/// what `nanus service status` prints and what the handshake carries — went up, and the
/// schemas sent on the next request did not. There is one registry now, and this is the
/// property that says so.
#[test]
fn the_runners_registry_is_the_one_the_context_publishes() {
    in_child_process("the_runners_registry_is_the_one_the_context_publishes");
    let dir = tempfile::tempdir().expect("temp dir");
    let settings = config(dir.path());

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let pending = runtime.block_on(compose(&settings)).expect("composes");
    let harness = pending.start().expect("mounts");
    assert_eq!(harness.tool_count(), 7, "the shipped toolset");

    // Register through the *runner's* handle, then look through the published service.
    let schema = nanus_domain::ToolSchema {
        name: nanus_domain::ToolName::new("eighth")
            .unwrap_or_else(|error| panic!("a valid tool name: {error}")),
        description: "registered after composition".to_owned(),
        parameters: serde_json::json!({ "type": "object" }),
    };
    let registered = harness
        .runner
        .tools()
        .borrow_mut()
        .register(nanus_domain::ToolDefinition::new(schema, Unused));
    assert!(registered.is_ok(), "the tool registers: {registered:?}");

    // The published count follows, which it can only do if the two are one object.
    assert_eq!(
        harness.tool_count(),
        8,
        "the service the context publishes is the registry the runner dispatches from"
    );
    // And the reverse door: what was registered through the service is what the runner
    // will send.
    let published = harness
        .context
        .get(nanus_bundle::tools_key())
        .expect("the tools service resolves");
    assert!(
        published
            .borrow()
            .names()
            .iter()
            .any(|name| name.as_str() == "eighth"),
        "the runner's registry carries the name: {:?}",
        published.borrow().names()
    );

    let shutdown = harness.shutdown();
    assert!(shutdown.is_ok(), "teardown reverts cleanly: {shutdown:?}");
}

/// A tool that does nothing, for a registration that is only ever inspected.
struct Unused;

impl nanus_domain::ToolExecutor for Unused {
    fn execute(&self, call: nanus_domain::ToolCall) -> nanus_domain::ToolFuture {
        Box::pin(async move { nanus_domain::ToolResult::success(call.id, serde_json::json!({})) })
    }
}

#[test]
fn mounting_from_inside_a_runtime_fails_loudly() {
    in_child_process("mounting_from_inside_a_runtime_fails_loudly");
    // The negative space. If someone makes `Pending` mountable from an async context by
    // marking the mount `async` and awaiting it, this test stops panicking and the
    // suite says so — which is the point: the constraint must remain visible.
    let dir = tempfile::tempdir().expect("temp dir");
    let settings = config(dir.path());

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let pending = runtime.block_on(compose(&settings)).expect("composes");

    // `AssertUnwindSafe` because a panic here is the expected outcome, and nothing is
    // observed afterwards.
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        runtime.block_on(async {
            // Awaiting inside the runtime, then mounting: the nested `block_on` the
            // kernel performs must not be silently tolerated.
            let _ = pending.start();
        });
    }));
    assert!(
        outcome.is_err(),
        "mounting inside a runtime must panic rather than appear to work"
    );
}

#[test]
fn a_configuration_without_a_key_still_composes_unconfigured() {
    let dir = tempfile::tempdir().expect("temp dir");
    let settings = config(dir.path());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");

    // A missing credential is the one failure an agent may start *through*: the composition
    // substitutes a placeholder adapter that reports the sentence naming `nanus auth set` on the
    // first request, so the interface opens and the reader can configure a provider with
    // `/provider`. A machine that already holds one — exported, in the keychain, or in the file
    // store — is not the case under test, because composing then finds the real adapter and that
    // is the correct behaviour: the guard asks the stores rather than the variable alone, or a
    // developer who once ran `nanus auth set deepseek` would see this test fail for doing its job.
    let account = Selection::resolve(&settings)
        .expect("the default configuration resolves")
        .credential_account()
        .to_owned();
    let had_key = std::env::var(KEY_ENV).is_ok_and(|value| !value.is_empty())
        || runtime.block_on(stored_credential(&account));
    if !had_key {
        let pending = runtime
            .block_on(compose(&settings))
            .expect("an unconfigured agent composes");
        assert!(!pending.configured(), "no credential was found");
    }
}

/// Whether any store already holds a credential for `account`.
///
/// Asked through the same chain a run uses, so the answer is the one composition will act on.
async fn stored_credential(account: &str) -> bool {
    let Ok(secrets) = nanus_bundle::compose::open_secrets() else {
        return false;
    };
    matches!(secrets.get(account).await, Ok(Some(secret)) if !secret.is_blank())
}

#[test]
fn the_default_agent_configuration_is_usable() {
    // The harness builds an `AgentConfig` from `NanusConfig`; the defaults must satisfy
    // the constructor's validation, or every run would fail at the first step.
    let settings = NanusConfig::default();
    let selection = Selection::resolve(&settings).expect("the defaults resolve");
    let agent = AgentConfig::new(
        settings.max_steps_per_turn,
        settings.max_parallel_tools,
        selection.model().to_owned(),
        16_384,
    );
    assert!(agent.is_ok(), "the shipped defaults are valid: {agent:?}");
}

#[test]
fn an_absent_usage_report_is_not_recorded_as_zero() {
    // The provider may report no usage at all, and a turn that recorded it as zero would
    // be indistinguishable from a genuinely free one. The distinction lives in the
    // session log, so it is asserted there rather than on the wire.
    use nanus_domain::{Session, SessionEvent, SessionId};

    let mut session = Session::new(SessionId::new("usage"), 0, "/tmp");
    session.append(SessionEvent::AssistantMessage {
        text: Some("hello".to_owned()),
        reasoning: None,
        tool_calls: Vec::new(),
        usage: None,
        interrupted: false,
        model: None,
        effort: None,
    });
    // A session whose only turn reported nothing totals zero tokens, and — the point —
    // the log says the report was absent rather than empty.
    assert_eq!(session.usage_totals().total_tokens(), 0);
    let recorded = session
        .log()
        .events()
        .iter()
        .any(|event| matches!(event, SessionEvent::AssistantMessage { usage: None, .. }));
    assert!(recorded, "the absence is preserved in the log");
}
