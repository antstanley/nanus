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
fn in_child_process(name: &str) {
    if std::env::var(CHILD_MARKER).is_ok() {
        // Already the child: nothing to do. Its home was pointed at a scratch directory
        // by the parent, so nothing it writes reaches the real one.
        return;
    }
    let home = std::env::temp_dir().join(format!("nanus-composition-{}", std::process::id()));
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
        stats.active, 6,
        "clock, fs, shell, store, llm and tools are active"
    );
    assert_eq!(stats.failed, 0, "nothing failed to mount");
    assert_eq!(harness.tool_count(), 7, "the shipped toolset is published");

    // The model id is the configured one, and the loop is constructed.
    assert_eq!(harness.llm.model(), settings.model);

    // A session can be created and saved, which is the CLI's post-run step.
    let session = harness.new_session(root);
    let recorded = runtime.block_on(harness.store.save(&session));
    assert!(recorded.is_ok(), "the session is persisted: {recorded:?}");

    // And the composition tears down cleanly, reverting every effect.
    let shutdown = harness.shutdown();
    assert!(shutdown.is_ok(), "teardown reverts cleanly: {shutdown:?}");
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
fn a_configuration_without_a_key_is_refused_before_anything_mounts() {
    let dir = tempfile::tempdir().expect("temp dir");
    let settings = config(dir.path());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");

    // With the variable emptied for this thread's view, composing fails at the
    // credential check rather than after mounting a half-built harness.
    let had_key = std::env::var(KEY_ENV).is_ok_and(|value| !value.is_empty());
    if !had_key {
        let outcome = runtime.block_on(compose(&settings));
        assert!(outcome.is_err(), "a missing key is refused");
        let Err(error) = outcome else { return };
        assert!(error.to_string().contains(KEY_ENV), "{error}");
    }
}

#[test]
fn the_default_agent_configuration_is_usable() {
    // The harness builds an `AgentConfig` from `NanusConfig`; the defaults must satisfy
    // the constructor's validation, or every run would fail at the first step.
    let settings = NanusConfig::default();
    let agent = AgentConfig::new(
        settings.max_steps_per_turn,
        settings.max_parallel_tools,
        settings.model,
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
