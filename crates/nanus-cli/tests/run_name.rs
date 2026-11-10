//! `nanus run --name`: a name the store could never record is refused before the turn, not
//! after it.
//!
//! The real binary in a scratch home, its provider at a closed local port so a run that gets as
//! far as a request fails fast and sends nothing anywhere. The check used to ask only whether
//! the name was *taken*, and a name that cannot be recorded is held by nobody — so the turn ran,
//! the session was saved without the name, and the naming error was all the caller was shown.

// A panic in a test *is* the assertion. The workspace denies the lint for production code.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::process::{Command, Output};

/// Runs `nanus run --name <name> hello` in a scratch home.
fn run_named(home: &Path, name: &str) -> Output {
    let config = home.join("config.toml");
    std::fs::write(
        &config,
        format!(
            "provider = \"deepseek\"\nbase_url = \"http://127.0.0.1:9\"\nworkspace_root = {:?}\n",
            home.display().to_string()
        ),
    )
    .unwrap();
    Command::new(env!("CARGO_BIN_EXE_nanus"))
        .args(["run", "--name", name, "hello"])
        .env("NANUS_HOME", home)
        .env("NANUS_CONFIG", &config)
        .env("DEEPSEEK_API_KEY", "run-name-test-key")
        .output()
        .unwrap()
}

/// The session directories under `home`.
fn sessions(home: &Path) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(home.join("sessions")).map_or_else(
        |_| Vec::new(),
        |entries| {
            entries
                .map(|entry| entry.unwrap().path())
                .filter(|path| path.is_dir())
                .collect()
        },
    )
}

#[test]
fn an_unusable_name_is_refused_before_a_turn_runs_or_a_session_exists() {
    let home = tempfile::tempdir().unwrap();
    let output = run_named(home.path(), &"x".repeat(200));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(stderr.contains("not a usable session name"), "{stderr}");
    assert!(output.stdout.is_empty(), "there is no answer to print");
    assert!(
        sessions(home.path()).is_empty(),
        "no turn ran, so no session was left behind: {:?}",
        sessions(home.path())
    );
}

#[test]
fn a_usable_name_gets_as_far_as_the_turn_and_names_what_it_recorded() {
    // The provider is unreachable, so the turn fails — as itself, not as a naming error — and
    // the failed turn is still recorded under the name that was asked for.
    let home = tempfile::tempdir().unwrap();
    let output = run_named(home.path(), "nightly");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(!stderr.contains("not a usable session name"), "{stderr}");
    assert!(!stderr.contains("could not be named"), "{stderr}");
    let recorded = sessions(home.path());
    assert_eq!(recorded.len(), 1, "{recorded:?}");
    let name = std::fs::read_to_string(recorded[0].join("name")).unwrap();
    assert_eq!(name.trim(), "nightly");
}
