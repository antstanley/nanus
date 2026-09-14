//! Integration tests for the process-group shell adapter, through
//! [`nanus_ports::ShellPort`].
//!
//! An integration-test crate is entirely test code, where a panic *is* the
//! assertion, so the workspace's panic-family exemption is restated here.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;
use std::time::Duration;

use futures::StreamExt as _;
use nanus_adapter_local::{LocalShell, is_alive};
use nanus_ports::{SandboxPolicy, ShellEvent, ShellPort, ShellRequest};

/// Builds an unconfined adapter over a temporary workspace root.
fn shell() -> (tempfile::TempDir, LocalShell) {
    let dir = tempfile::tempdir().expect("tempdir");
    let shell = LocalShell::new(SandboxPolicy::danger_full_access(dir.path()));
    (dir, shell)
}

/// Reads the grandchild pid a script recorded, or zero.
fn recorded_pid(path: &Path) -> i32 {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| text.trim().parse::<i32>().ok())
        .unwrap_or(0)
}

/// Waits for `pid` to disappear, polling for at most `attempts` ticks.
async fn wait_for_death(pid: i32, attempts: u32) -> bool {
    for _ in 0..attempts {
        if !is_alive(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    !is_alive(pid)
}

/// Quotes `text` for `/bin/sh`.
fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// The classic orphan-maker: a shell that backgrounds a long sleep and waits.
fn background_sleep(pid_file: &Path) -> String {
    format!(
        "sleep 300 & echo $! > {}; wait",
        quote(&pid_file.to_string_lossy())
    )
}

/// The measurement that motivates the whole module: the grandchild must not
/// survive the timeout, because `Child::kill` alone would leave it running.
#[tokio::test]
async fn timeout_kills_the_whole_group() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pid_file = dir.path().join("grandchild.pid");
    let (_root, shell) = shell();
    let request = ShellRequest::shell(background_sleep(&pid_file), None)
        .with_timeout(Duration::from_millis(700))
        .with_max_output_bytes(8192);
    let outcome = shell.run(request).await.expect("run");
    assert!(outcome.timed_out, "the run timed out");
    assert_eq!(outcome.exit_code, None, "a killed child has no status code");
    let grandchild = recorded_pid(&pid_file);
    assert!(grandchild > 0, "the grandchild recorded its pid");
    assert!(
        wait_for_death(grandchild, 250).await,
        "the grandchild must not survive the group kill"
    );
    assert_eq!(shell.live_groups(), 0, "the group was reaped");
}

/// `kill_all` — what shutdown calls — reaches grandchildren too.
#[tokio::test]
async fn kill_all_reaps_a_running_group() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pid_file = dir.path().join("grandchild.pid");
    let (_root, shell) = shell();
    let request = ShellRequest::shell(background_sleep(&pid_file), None)
        .with_timeout(Duration::from_secs(60));
    let run = shell.run(request);
    let killer = async {
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(shell.live_groups(), 1, "the run registered one group");
        shell.kill_all().await.expect("kill_all")
    };
    let (outcome, signalled) = futures::join!(run, killer);
    let outcome = outcome.expect("run");
    assert_eq!(signalled, 1, "one group was signalled");
    assert!(
        !outcome.timed_out,
        "the kill, not the timeout, ended the run"
    );
    let grandchild = recorded_pid(&pid_file);
    assert!(grandchild > 0, "the grandchild recorded its pid");
    assert!(
        wait_for_death(grandchild, 250).await,
        "kill_all reaches grandchildren"
    );
    assert_eq!(shell.live_groups(), 0, "the registry drained");
    assert_eq!(shell.kill_all().await.expect("kill_all"), 0);
}

/// A 50 MB producer with a 100-byte cap must terminate *and* count every byte.
#[tokio::test]
async fn output_cap_truncates_without_hanging() {
    let (_root, shell) = shell();
    let request = ShellRequest::shell("head -c 50000000 /dev/zero | tr '\\0' 'a'", None)
        .with_timeout(Duration::from_secs(30))
        .with_max_output_bytes(100);
    let outcome = shell.run(request).await.expect("run");
    assert!(outcome.stdout.truncated, "the capture reports truncation");
    assert_eq!(
        outcome.stdout.text.len(),
        100,
        "exactly the cap is retained"
    );
    assert!(
        outcome.stdout.total_bytes >= 50_000_000,
        "every byte past the cap was still counted: {}",
        outcome.stdout.total_bytes
    );
    assert!(outcome.is_success(), "the producer exited cleanly");
}

/// A non-zero exit code is an outcome, not an error.
#[tokio::test]
async fn exit_codes_are_outcomes_not_errors() {
    let (_root, shell) = shell();
    let ok = shell
        .run(ShellRequest::shell("exit 0", None).with_timeout(Duration::from_secs(5)))
        .await
        .expect("run");
    assert_eq!(ok.exit_code, Some(0));
    assert!(ok.is_success());

    let bad = shell
        .run(
            ShellRequest::shell("echo boom >&2; exit 3", None).with_timeout(Duration::from_secs(5)),
        )
        .await
        .expect("a non-zero exit is an outcome");
    assert_eq!(bad.exit_code, Some(3));
    assert_eq!(bad.signal, None);
    assert_eq!(bad.stderr.text, "boom\n");
    assert!(!bad.is_success(), "a non-zero code is not success");

    let killed = shell
        .run(ShellRequest::shell("kill -TERM $$", None).with_timeout(Duration::from_secs(5)))
        .await
        .expect("a signal death is an outcome");
    assert_eq!(killed.exit_code, None);
    assert_eq!(killed.signal, Some(15));
    assert!(!killed.is_success());
}

/// A completed run leaves the registry empty.
#[tokio::test]
async fn live_groups_return_to_zero_after_a_run() {
    let (_root, shell) = shell();
    let outcome = shell
        .run(ShellRequest::shell("echo hi", None).with_timeout(Duration::from_secs(5)))
        .await
        .expect("run");
    assert_eq!(outcome.stdout.text, "hi\n");
    assert_eq!(
        shell.live_groups(),
        0,
        "a completed run leaves no live group"
    );
    assert_eq!(shell.kill_all().await.expect("kill_all"), 0);
}

/// The direct-argv path runs a program with no shell interpretation.
#[tokio::test]
async fn the_direct_path_needs_no_shell() {
    let (_root, shell) = shell();
    let request = ShellRequest::direct("printf", vec!["%s".to_owned(), "a;b".to_owned()])
        .with_timeout(Duration::from_secs(5));
    let outcome = shell.run(request).await.expect("run");
    assert_eq!(outcome.exit_code, Some(0));
    assert_eq!(
        outcome.stdout.text, "a;b",
        "the semicolon is data, not a command separator"
    );
}

/// The working directory and environment are honoured.
#[tokio::test]
async fn the_working_directory_and_environment_are_honoured() {
    let root = tempfile::tempdir().expect("tempdir");
    let shell = LocalShell::new(SandboxPolicy::danger_full_access(root.path()));
    let request = ShellRequest::shell("pwd; printf '%s' \"$NANUS_TEST\"", None)
        .with_cwd(root.path())
        .with_env("NANUS_TEST", "present")
        .with_timeout(Duration::from_secs(5));
    let outcome = shell.run(request).await.expect("run");
    let expected = root.path().canonicalize().expect("canonical");
    assert!(
        outcome
            .stdout
            .text
            .contains(&expected.to_string_lossy().to_string())
    );
    assert!(outcome.stdout.text.ends_with("present"));
}

/// Standard input reaches the child.
#[tokio::test]
async fn standard_input_reaches_the_child() {
    let (_root, shell) = shell();
    let request = ShellRequest::shell("cat", None)
        .with_stdin("piped input\n")
        .with_timeout(Duration::from_secs(5));
    let outcome = shell.run(request).await.expect("run");
    assert_eq!(outcome.stdout.text, "piped input\n");
}

/// A confined policy refuses a working directory outside its root.
#[tokio::test]
async fn a_confined_policy_refuses_an_outside_working_directory() {
    let root = tempfile::tempdir().expect("root");
    let elsewhere = tempfile::tempdir().expect("elsewhere");
    let shell = LocalShell::new(SandboxPolicy::workspace_write(root.path()));
    let request = ShellRequest::shell("pwd", None)
        .with_cwd(elsewhere.path())
        .with_timeout(Duration::from_secs(5));
    let error = shell.run(request).await.expect_err("must be refused");
    assert!(
        matches!(error, nanus_ports::ShellError::OutsideWorkspace { .. }),
        "{error}"
    );
    assert_eq!(
        shell.sandbox().mode,
        nanus_domain::SandboxMode::WorkspaceWrite
    );
}

/// An output cap of zero is refused rather than silently replaced.
#[tokio::test]
async fn a_zero_output_cap_is_refused() {
    let (_root, shell) = shell();
    let request = ShellRequest::shell("echo hi", None)
        .with_max_output_bytes(0)
        .with_timeout(Duration::from_secs(5));
    let error = shell.run(request).await.expect_err("must be refused");
    assert!(
        matches!(error, nanus_ports::ShellError::InvalidOutputCap { .. }),
        "{error}"
    );
}

/// A program that cannot be started is a spawn error, not a silent success.
#[tokio::test]
async fn an_unstartable_program_is_a_spawn_error() {
    let (_root, shell) = shell();
    let request = ShellRequest::direct("nanus-no-such-program", Vec::new())
        .with_timeout(Duration::from_secs(5));
    let error = shell.run(request).await.expect_err("must fail");
    assert!(
        matches!(error, nanus_ports::ShellError::Spawn { .. }),
        "{error}"
    );
    assert_eq!(
        shell.live_groups(),
        0,
        "a failed spawn leaves no live group"
    );
}

/// Streaming delivers live events and ends with exactly one `Exited`.
#[tokio::test]
async fn spawn_streams_live_output_and_ends_with_exited() {
    let (_root, shell) = shell();
    let request = ShellRequest::shell("echo first; echo second; echo oops >&2", None)
        .with_timeout(Duration::from_secs(5));
    let mut stream = shell.spawn(request).await.expect("spawn");
    let mut stdout = String::new();
    let mut stderr = String::new();
    let mut exits = 0usize;
    while let Some(event) = stream.next().await {
        match event {
            ShellEvent::Stdout { chunk } => stdout.push_str(&chunk),
            ShellEvent::Stderr { chunk } => stderr.push_str(&chunk),
            ShellEvent::Exited {
                exit_code,
                timed_out,
                ..
            } => {
                exits = exits.saturating_add(1);
                assert_eq!(exit_code, Some(0));
                assert!(!timed_out);
            }
        }
    }
    assert_eq!(exits, 1, "exactly one Exited ends the stream");
    assert_eq!(stdout, "first\nsecond\n");
    assert_eq!(stderr, "oops\n");
    assert_eq!(shell.live_groups(), 0);
}

/// The live channel is bounded, so a fast producer cannot grow it without limit.
#[tokio::test]
async fn the_live_channel_is_bounded() {
    let (_root, shell) = shell();
    let request = ShellRequest::shell("head -c 20000000 /dev/zero | tr '\\0' 'b'", None)
        .with_timeout(Duration::from_secs(30))
        .with_max_output_bytes(128);
    let mut stream = shell.spawn(request).await.expect("spawn");
    let consumer = async {
        let mut seen = 0usize;
        while let Some(event) = stream.next().await {
            if !matches!(event, ShellEvent::Exited { .. }) {
                seen = seen.saturating_add(1);
            }
        }
        seen
    };
    let seen = tokio::time::timeout(Duration::from_secs(60), consumer)
        .await
        .expect("the producer terminates");
    // The live channel holds 256 events; a slow consumer must not accumulate
    // millions the way an unbounded channel did.
    assert!(
        seen <= 10_000,
        "the bounded channel never grew unboundedly: {seen}"
    );
    assert_eq!(shell.live_groups(), 0);
}

/// A relative working directory runs *inside* the root it was checked against.
///
/// The confinement check resolved a relative path against the workspace root and then threw the
/// answer away, leaving the relative path for `Command::current_dir` — which resolves against the
/// *process's* working directory. So a `workdir` of `sub` was validated as `<root>/sub` and
/// executed in `<cwd>/sub`: a different directory, which need not be inside the workspace at all.
/// The tool's own schema documents `workdir` as relative to the workspace root, so the documented
/// spelling was the broken one, and it takes a root that differs from the process's directory to
/// see it — which is exactly what this test arranges.
#[tokio::test]
async fn a_relative_working_directory_runs_under_the_root_it_was_checked_against() {
    let root = tempfile::tempdir().expect("root");
    std::fs::create_dir(root.path().join("sub")).expect("create sub");
    std::fs::write(root.path().join("sub").join("marker"), "here").expect("write marker");
    let shell = LocalShell::new(SandboxPolicy::workspace_write(root.path()));

    let request = ShellRequest::shell("cat marker", None)
        .with_cwd("sub")
        .with_timeout(Duration::from_secs(5));
    let outcome = shell.run(request).await.expect("the relative cwd resolves");
    assert_eq!(
        outcome.stdout.text, "here",
        "the command ran where the policy checked, not where the process happens to be"
    );

    // And escaping through the relative path is still refused.
    let escaping = ShellRequest::shell("pwd", None)
        .with_cwd("../..")
        .with_timeout(Duration::from_secs(5));
    assert!(
        escaping_error(&shell, escaping).await,
        "a relative path that climbs out is refused"
    );
}

/// Runs a request and reports whether the policy refused the working directory.
async fn escaping_error(shell: &LocalShell, request: ShellRequest) -> bool {
    matches!(
        shell.run(request).await,
        Err(nanus_ports::ShellError::OutsideWorkspace { .. })
    )
}

/// A run that cannot start says so in the stream.
///
/// The failure was reported as an `Exited` with no code and no output, which is exactly what a
/// process that ran and printed nothing looks like: a consumer reading the stream — the only
/// channel it has — could not tell that the command never started.
#[tokio::test]
async fn a_failed_spawn_is_reported_rather_than_looking_silent() {
    let (_root, shell) = shell();
    let request = ShellRequest::direct("/definitely/not/a/program/here", Vec::new())
        .with_timeout(Duration::from_secs(5));
    let mut stream = shell.spawn(request).await.expect("the stream opens");
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event);
    }
    assert!(
        events.iter().any(|event| matches!(
            event,
            ShellEvent::Stderr { chunk } if chunk.contains("not/a/program")
        )),
        "the reason the run did not start is in the stream: {events:?}"
    );
    assert!(
        matches!(
            events.last(),
            Some(ShellEvent::Exited {
                exit_code: None,
                signal: None,
                ..
            })
        ),
        "and the stream still ends with an exit: {events:?}"
    );
}
