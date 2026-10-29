//! A configuration's `home` and `credential_service` reach every command that keeps state.
//!
//! The real binary, a configuration file of the test's own, and two scratch homes: one named by
//! `NANUS_HOME`, standing in for the home a terminal agent uses, and one named by the
//! configuration. Every command has to agree with the composition about which of the two it is
//! in — a listing that read one home while a run recorded into the other would show a reader
//! conversations that are not the ones they had. Nothing here writes a credential, so the
//! platform store is only ever read, and under a service name no real item has.

// A panic in a test *is* the assertion. The workspace denies the lint for production code.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::process::{Command, Output};

use nanus_adapter_store::JsonlStore;
use nanus_domain::{Session, SessionId};
use nanus_ports::StorePort as _;

/// The service name the configuration files credentials under: never the shipped `nanus`.
const HOST_SERVICE: &str = "nanus-test-example-host";

/// Saves an empty session called `id` in `home`, as the store the binary opens there would.
fn save(home: &Path, id: &str) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let store = JsonlStore::new(home).await.unwrap();
        store
            .save(&Session::new(SessionId::new(id), 0, "/w"))
            .await
            .unwrap();
    });
}

/// Runs `nanus --config <config> <args>` with `NANUS_HOME` set to `environment_home`, in `cwd`.
fn nanus(config: &Path, environment_home: &Path, cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_nanus"))
        .arg("--config")
        .arg(config)
        .args(args)
        .current_dir(cwd)
        .env("NANUS_HOME", environment_home)
        .env_remove("NANUS_CONFIG")
        .output()
        .unwrap()
}

/// Writes a configuration file with `body` in `dir`, and returns its path.
fn configuration(dir: &Path, body: &str) -> std::path::PathBuf {
    let path = dir.join("config.toml");
    std::fs::write(&path, body).unwrap();
    path
}

#[test]
fn sessions_and_the_report_follow_the_configured_home_rather_than_nanus_home() {
    let scratch = tempfile::tempdir().unwrap();
    let environment_home = tempfile::tempdir().unwrap();
    let chosen = tempfile::tempdir().unwrap();
    save(chosen.path(), "kept-in-the-chosen-home");
    let config = configuration(
        scratch.path(),
        &format!(
            "home = {:?}\ncredential_service = {HOST_SERVICE:?}\n",
            chosen.path().display().to_string()
        ),
    );

    let listed = nanus(
        &config,
        environment_home.path(),
        scratch.path(),
        &["sessions"],
    );
    let stdout = String::from_utf8_lossy(&listed.stdout);
    assert!(
        listed.status.success(),
        "{}",
        String::from_utf8_lossy(&listed.stderr)
    );
    assert!(stdout.contains("kept-in-the-chosen-home"), "{stdout}");
    assert!(
        !environment_home.path().join("sessions").exists(),
        "NANUS_HOME was never opened"
    );

    let report = nanus(
        &config,
        environment_home.path(),
        scratch.path(),
        &["config"],
    );
    let stdout = String::from_utf8_lossy(&report.stdout);
    assert!(
        report.status.success(),
        "{}",
        String::from_utf8_lossy(&report.stderr)
    );
    assert!(
        stdout.contains(&format!("nanus home: {}", chosen.path().display())),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!("credential service: {HOST_SERVICE}\n")),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!(
            "selection file: {}",
            chosen.path().join("selection.toml").display()
        )),
        "the selection's default place follows the home: {stdout}"
    );

    // The other direction: a configuration that names no home is in NANUS_HOME, exactly as
    // before the field existed. It still names the test's service, so the report's credential
    // line asks the platform store about an item no developer has, rather than their own key.
    let plain = configuration(
        scratch.path(),
        &format!("credential_service = {HOST_SERVICE:?}\n"),
    );
    let listed = nanus(
        &plain,
        environment_home.path(),
        scratch.path(),
        &["sessions"],
    );
    let stdout = String::from_utf8_lossy(&listed.stdout);
    assert!(listed.status.success());
    assert!(!stdout.contains("kept-in-the-chosen-home"), "{stdout}");
    let report = nanus(&plain, environment_home.path(), scratch.path(), &["config"]);
    let stdout = String::from_utf8_lossy(&report.stdout);
    assert!(
        stdout.contains(&format!(
            "nanus home: {}",
            environment_home.path().display()
        )),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!(
            "selection file: {}",
            environment_home.path().join("selection.toml").display()
        )),
        "{stdout}"
    );
}

#[test]
fn a_relative_home_or_an_empty_service_is_refused_by_every_command() {
    let scratch = tempfile::tempdir().unwrap();
    let environment_home = tempfile::tempdir().unwrap();
    let relative = configuration(scratch.path(), "home = \"example-host\"\n");
    for args in [&["sessions"][..], &["config"], &["auth", "status"]] {
        let refused = nanus(&relative, environment_home.path(), scratch.path(), args);
        let stderr = String::from_utf8_lossy(&refused.stderr);
        assert!(!refused.status.success(), "{args:?} was refused");
        assert!(stderr.contains("`home`"), "{args:?}: {stderr}");
    }
    assert!(
        !scratch.path().join("example-host").exists(),
        "nothing was created under the working directory"
    );
    assert!(
        !environment_home.path().join("sessions").exists(),
        "nor in NANUS_HOME"
    );

    for service in ["", "Example-Host", "-w"] {
        let unusable = configuration(
            scratch.path(),
            &format!("credential_service = {service:?}\n"),
        );
        for args in [&["config"][..], &["auth", "status"]] {
            let refused = nanus(&unusable, environment_home.path(), scratch.path(), args);
            let stderr = String::from_utf8_lossy(&refused.stderr);
            assert!(
                !refused.status.success(),
                "{service:?} {args:?} was refused"
            );
            assert!(
                stderr.contains("credential_service"),
                "{service:?} {args:?}: {stderr}"
            );
        }
    }
}
