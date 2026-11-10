//! `nanus service start` beside a service that is already running: refused by name, in the
//! foreground as much as detached, and never at the running service's expense.
//!
//! The real binary in a scratch home. A second start used to get as far as a bind that could
//! only fail, and then removed the socket on its way out — the socket of the service that was
//! using it, which went on running where no `status` or `stop` could reach it.

// A panic in a test *is* the assertion. The workspace denies the lint for production code.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]
#![cfg(unix)]

use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

/// A `nanus` command in a scratch home, pointed at a closed local port so nothing leaves the
/// machine.
fn nanus(home: &Path) -> Command {
    let config = home.join("config.toml");
    if !config.exists() {
        std::fs::write(
            &config,
            format!(
                "provider = \"deepseek\"\nbase_url = \"http://127.0.0.1:9\"\n\
                 workspace_root = {:?}\n",
                home.display().to_string()
            ),
        )
        .unwrap();
    }
    let mut command = Command::new(env!("CARGO_BIN_EXE_nanus"));
    command
        .env("NANUS_HOME", home)
        .env("NANUS_CONFIG", &config)
        .env("DEEPSEEK_API_KEY", "service-start-test-key")
        .stdin(Stdio::null());
    command
}

fn status(home: &Path) -> Output {
    nanus(home).args(["service", "status"]).output().unwrap()
}

/// Starts a foreground service and waits until it answers.
fn serving(home: &Path) -> Child {
    let child = nanus(home)
        .args(["service", "start", "--foreground"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now().checked_add(Duration::from_secs(30)).unwrap();
    while !status(home).status.success() {
        assert!(Instant::now() < deadline, "the service never answered");
        std::thread::sleep(Duration::from_millis(50));
    }
    child
}

/// Stops the service through the link and reaps it.
fn stop(home: &Path, mut child: Child) {
    let stopped = nanus(home).args(["service", "stop"]).output().unwrap();
    assert!(stopped.status.success(), "{stopped:?}");
    let exit = child.wait().unwrap();
    assert!(exit.success(), "a stopped service exits cleanly: {exit}");
}

#[test]
fn a_second_foreground_start_is_refused_and_leaves_the_first_reachable() {
    let home = tempfile::tempdir().unwrap();
    let socket = home.path().join("run").join("agent.sock");
    let first = serving(home.path());
    assert!(socket.exists(), "the first service bound its socket");

    let second = nanus(home.path())
        .args(["service", "start", "--foreground"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(!second.status.success(), "{stderr}");
    assert!(stderr.contains("already listening"), "{stderr}");

    // The first service is untouched: its socket is on disk, it answers, and it can be stopped
    // the documented way.
    assert!(socket.exists(), "the running service kept its socket");
    let answered = status(home.path());
    assert!(answered.status.success(), "{answered:?}");
    stop(home.path(), first);
    assert!(!socket.exists(), "a stopped service removes its own socket");
}

#[test]
fn a_second_detached_start_is_refused_too() {
    let home = tempfile::tempdir().unwrap();
    let first = serving(home.path());
    let second = nanus(home.path())
        .args(["service", "start"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(!second.status.success(), "{stderr}");
    assert!(stderr.contains("already listening"), "{stderr}");
    assert!(status(home.path()).status.success());
    stop(home.path(), first);
}

#[test]
fn a_start_with_nothing_listening_serves_and_stops_cleanly() {
    let home = tempfile::tempdir().unwrap();
    assert!(
        !status(home.path()).status.success(),
        "nothing is listening before the start"
    );
    let first = serving(home.path());
    stop(home.path(), first);
    assert!(
        !status(home.path()).status.success(),
        "nothing is listening after the stop"
    );
}
