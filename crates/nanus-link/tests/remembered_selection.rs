//! The remembered selection through a served agent: where a change is written, and what the next
//! start reads.
//!
//! The record has two halves in two crates — the composition reads it at a start, the link server
//! writes it when a client changes the model — and the configuration decides where both look. The
//! property worth pinning is that they agree, so every test here composes a real agent, changes
//! its model the way an interface does, and then composes a fresh one from the same configuration.
//! A test of either half alone could not see a writer and a reader that disagree about the file.
//!
//! Composing reads the nanus home from `NANUS_HOME`, so each test **re-executes itself** with a
//! scratch home: `std::env::set_var` is `unsafe` in edition 2024, this workspace forbids `unsafe`
//! even in tests, and the developer's real home is the one place these tests must never write. No
//! request is made: the model is changed, never asked anything, and the key is a placeholder.
//!
//! The parent *returns* once its child has passed rather than exiting. CI runs this crate with
//! `cargo test`, where every test in the file is a thread of one process, and a parent that called
//! `std::process::exit` would end its siblings before they reported — passing a run in which they
//! had not.

#![cfg(feature = "server")]
// A panic in a test is the assertion.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::rc::Rc;

use nanus_adapter_config::NanusConfig;
use nanus_bundle::compose::{compose, store_home};
use nanus_bundle::{Harness, LastSelection};
use nanus_link::Client;
use nanus_link::protocol::{Frame, Request};
use nanus_link::server::Agent;

/// The variable the adapter reads a key from, as the last store in the chain.
const KEY_ENV: &str = "DEEPSEEK_API_KEY";

/// The marker the re-executed child looks for, so it never forks again.
const CHILD_MARKER: &str = "NANUS_REMEMBERED_SELECTION_CHILD";

/// The variable the nanus home is resolved from.
const HOME_ENV: &str = "NANUS_HOME";

/// The placeholder key. It is never sent anywhere.
const PLACEHOLDER_KEY: &str = "remembered-selection-test-key";

/// The model the configuration names, which a start with no record begins from.
const CONFIGURED: &str = "deepseek-flash";

/// The model a client switches to.
const CHOSEN: &str = "deepseek-v4-pro";

/// Returns whether the calling test's body should run here: in the child, yes; in the parent, no,
/// after running the child with a scratch home and a key and asserting that it passed.
///
/// The child is this same binary filtered to `name`, so the body runs exactly once, in the child.
/// The home is named for the test as well as the parent, because the tests here run beside each
/// other and each parent deletes its own child's home when it returns.
fn in_child_process(name: &str) -> bool {
    if std::env::var(CHILD_MARKER).is_ok() {
        return true;
    }
    let home = std::env::temp_dir().join(format!("nanus-remembered-{}-{name}", std::process::id()));
    let exe = std::env::current_exe().expect("the test binary has a path");
    let status = std::process::Command::new(exe)
        .arg("--exact")
        .arg(name)
        .arg("--nocapture")
        .env(KEY_ENV, PLACEHOLDER_KEY)
        .env(CHILD_MARKER, "1")
        .env(HOME_ENV, &home)
        .status()
        .expect("the child test binary runs");
    let _cleaned = std::fs::remove_dir_all(&home);
    assert!(status.success(), "the child run of {name} passed");
    false
}

/// Where the agent in `dir` listens: a socket in it on Unix, a pipe of this test's own on Windows,
/// where a pipe name is global and the tests here run beside each other.
fn endpoint(dir: &Path) -> PathBuf {
    #[cfg(unix)]
    {
        dir.join("agent.sock")
    }
    #[cfg(windows)]
    {
        let base = nanus_link::paths::attached_endpoint(dir, std::process::id()).expect("SID");
        PathBuf::from(format!("{}-{}", base.display(), uuid::Uuid::new_v4()))
    }
}

/// A configuration that names its provider and model, rooted at `workspace`.
///
/// The model is named so that a start which read no record is recognisable: anything else the
/// agent begins from came from a file.
fn config(workspace: &Path) -> NanusConfig {
    NanusConfig {
        provider: Some(String::from("deepseek")),
        model: Some(String::from(CONFIGURED)),
        workspace_root: Some(workspace.to_path_buf()),
        ..NanusConfig::default()
    }
}

/// The record a terminal session left in the home: a model and an effort no test here chooses.
///
/// `max` is the effort because a model switch puts the effort back to the adapter's default, so an
/// agent that reports `max` afterwards can only have read this file.
fn the_homes_record(home: &Path) -> Vec<u8> {
    let record = LastSelection {
        provider: Some(String::from("deepseek")),
        plan: Some(String::from("api")),
        model: Some(String::from(CHOSEN)),
        effort: Some(String::from("max")),
    };
    let path = record.save(home).expect("the home's record is written");
    std::fs::read(path).expect("and reads back")
}

/// Composes and mounts an agent from `config`, the way the binaries do.
fn start(config: &NanusConfig) -> Harness {
    let pending = nanus_kernel::runtime::block_on(compose(config)).expect("the agent composes");
    pending.start().expect("the agent mounts")
}

/// Serves `harness` and asks it, as an interface would, to switch to `model`.
///
/// Returns once the agent has answered a later request on the same connection. A connection's
/// requests are handled in order, so by then the switch — and whatever it writes — has finished,
/// which is what lets the caller look at the files without a race.
fn switch_model(harness: &Harness, workspace: &Path, model: &str) {
    let agent = Agent::new(harness, workspace);
    let socket = endpoint(workspace);
    let model = model.to_owned();
    nanus_kernel::runtime::block_on_local(async move {
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let listener = nanus_link::bind(&socket).await.expect("the socket binds");
        let serving =
            tokio::task::spawn_local(nanus_link::serve(listener, Rc::new(agent), async move {
                let _ = stop_rx.await;
            }));
        let mut client = Client::connect(&socket).await.expect("the agent answers");
        client
            .send(&Request::SetModel { model })
            .await
            .expect("the switch is sent");
        client
            .send(&Request::Sessions)
            .await
            .expect("the listing is sent");
        loop {
            match client.next().await.expect("frames are readable") {
                Some(Frame::Sessions { .. }) => break,
                Some(Frame::Refused { message, .. }) => panic!("the switch was refused: {message}"),
                Some(_) => {}
                None => panic!("the agent hung up before answering"),
            }
        }
        drop(client);
        let _ = stop_tx.send(());
        serving
            .await
            .expect("the server task is joined")
            .expect("serving ends cleanly");
    });
}

/// A scratch workspace inside `dir`.
fn workspace(dir: &Path) -> PathBuf {
    let workspace = dir.join("work");
    std::fs::create_dir_all(&workspace).expect("a workspace");
    workspace
}

/// With no option set, a change is remembered in the home and the next start begins there.
///
/// The behaviour every terminal session has always had, pinned so that moving the knob into the
/// configuration did not move the default.
#[test]
fn without_the_option_a_change_is_remembered_in_the_home() {
    if !in_child_process("without_the_option_a_change_is_remembered_in_the_home") {
        return;
    }
    let dir = tempfile::tempdir().expect("temp dir");
    let workspace = workspace(dir.path());
    let home = store_home(&config(&workspace)).expect("the child has a home");
    let settings = config(&workspace);

    let harness = start(&settings);
    let expected = LastSelection::path(&home);
    assert_eq!(harness.switch.selection_file(), Some(expected.as_path()));
    assert_eq!(
        harness.switch.model(),
        CONFIGURED,
        "nothing was recorded yet"
    );
    assert!(
        LastSelection::load(&home).is_none(),
        "a start writes nothing"
    );

    switch_model(&harness, &workspace, CHOSEN);
    assert!(harness.shutdown().is_ok(), "teardown reverts cleanly");
    let written = LastSelection::load(&home).expect("the change is recorded in the home");
    assert_eq!(written.model.as_deref(), Some(CHOSEN));

    let again = start(&settings);
    assert_eq!(
        again.switch.model(),
        CHOSEN,
        "the next start begins from it"
    );
    assert!(again.shutdown().is_ok(), "teardown reverts cleanly");
}

/// A named file takes the change, the home's record is untouched, and a fresh start reads the file.
///
/// What an embedding host needs: a default of its own that a terminal session does not change and
/// that does not change the terminal's, with the sessions still in the shared home.
#[test]
fn a_named_selection_file_takes_the_change_and_the_homes_record_is_untouched() {
    if !in_child_process(
        "a_named_selection_file_takes_the_change_and_the_homes_record_is_untouched",
    ) {
        return;
    }
    let dir = tempfile::tempdir().expect("temp dir");
    let workspace = workspace(dir.path());
    let home = store_home(&config(&workspace)).expect("the child has a home");
    let before = the_homes_record(&home);
    let file = dir.path().join("host").join("selection.toml");
    let settings = NanusConfig {
        selection_file: Some(file.clone()),
        ..config(&workspace)
    };

    let harness = start(&settings);
    assert_eq!(harness.switch.selection_file(), Some(file.as_path()));
    assert_eq!(
        harness.switch.model(),
        CONFIGURED,
        "the home's record is not what this agent starts from"
    );

    switch_model(&harness, &workspace, CHOSEN);
    assert!(harness.shutdown().is_ok(), "teardown reverts cleanly");
    let written = LastSelection::load_from(&file).expect("the change is recorded in the file");
    assert_eq!(written.model.as_deref(), Some(CHOSEN));
    let after = std::fs::read(LastSelection::path(&home)).expect("the home's record is there");
    assert_eq!(
        after, before,
        "the home's record is byte for byte what it was"
    );

    let again = start(&settings);
    assert_eq!(
        again.switch.model(),
        CHOSEN,
        "a fresh start begins from the file"
    );
    assert_eq!(
        again.runner.effort(),
        written.effort(),
        "with the file's effort, not the home's"
    );
    assert_ne!(
        again.runner.effort(),
        Some(nanus_ports::ReasoningEffort::Max),
        "the home's effort did not leak into the start"
    );
    assert!(again.shutdown().is_ok(), "teardown reverts cleanly");
}

/// With remembering off, nothing is read at a start and nothing is written on a change.
///
/// The configuration is the whole answer: the home's record is ignored even though it exists, and
/// a file named beside the switch is not created, because off wins over a location.
#[test]
fn with_remembering_off_no_selection_file_is_read_or_written() {
    if !in_child_process("with_remembering_off_no_selection_file_is_read_or_written") {
        return;
    }
    let dir = tempfile::tempdir().expect("temp dir");
    let workspace = workspace(dir.path());
    let home = store_home(&config(&workspace)).expect("the child has a home");
    let before = the_homes_record(&home);
    let unused = dir.path().join("host").join("selection.toml");
    let settings = NanusConfig {
        remember_selection: false,
        selection_file: Some(unused.clone()),
        ..config(&workspace)
    };

    let harness = start(&settings);
    assert_eq!(harness.switch.selection_file(), None);
    assert_eq!(
        harness.switch.model(),
        CONFIGURED,
        "the home's record exists and is not read"
    );

    switch_model(&harness, &workspace, CHOSEN);
    assert_eq!(
        harness.runner.model(),
        CHOSEN,
        "the change is in force for this agent"
    );
    assert!(harness.shutdown().is_ok(), "teardown reverts cleanly");
    assert!(!unused.exists(), "the named file is not written while off");
    let after = std::fs::read(LastSelection::path(&home)).expect("the home's record is there");
    assert_eq!(after, before, "the home's record is not written either");

    let again = start(&settings);
    assert_eq!(
        again.switch.model(),
        CONFIGURED,
        "the next start is the configuration again"
    );
    assert!(again.shutdown().is_ok(), "teardown reverts cleanly");
}
