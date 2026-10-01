//! Native Windows shell lifecycle: output, timeout, cancellation, and grandchildren.
#![cfg(windows)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;
use std::time::Duration;

use futures::StreamExt as _;
use nanus_adapter_local::LocalShell;
use nanus_ports::{ShellEvent, ShellPort, ShellRequest};

fn powershell(script: &str) -> ShellRequest {
    ShellRequest::direct(
        "powershell.exe",
        vec![
            String::from("-NoProfile"),
            String::from("-NonInteractive"),
            String::from("-Command"),
            script.to_owned(),
        ],
    )
}

fn descendant(path: &Path) -> ShellRequest {
    powershell(&format!(
        "$p = Start-Process powershell.exe -ArgumentList '-NoProfile', '-Command', \
         'Start-Sleep 300' -PassThru; \
         [IO.File]::WriteAllText('{}', [string]$p.Id); Wait-Process -Id $p.Id",
        path.display().to_string().replace('\'', "''")
    ))
}

async fn recorded_pid(path: &Path) -> u32 {
    for _ in 0..500 {
        if let Ok(text) = std::fs::read_to_string(path)
            && let Ok(pid) = text.trim().parse()
        {
            return pid;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the grandchild pid was never recorded");
}

async fn assert_dead(pid: u32) {
    for _ in 0..200 {
        if !nanus_sys_windows::is_process_alive(pid).expect("process liveness") {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        !nanus_sys_windows::is_process_alive(pid).expect("process liveness"),
        "grandchild {pid} survived"
    );
}

#[tokio::test]
async fn the_platform_shell_returns_output_and_a_nonzero_exit_as_a_result() {
    let dir = tempfile::tempdir().expect("workspace");
    let shell = LocalShell::unconfined(dir.path());
    let output = shell
        .run(ShellRequest::shell(
            "echo hello",
            Some(dir.path().to_owned()),
        ))
        .await
        .expect("cmd");
    assert_eq!(output.stdout.text.trim(), "hello");
    assert_eq!(output.exit_code, Some(0));
    let failed = shell
        .run(ShellRequest::shell("echo failure 1>&2 & exit /b 3", None))
        .await
        .expect("a nonzero exit is a result");
    assert_eq!(failed.exit_code, Some(3));
    assert!(failed.stderr.text.contains("failure"));
    assert_eq!(shell.live_groups(), 0);
}

#[tokio::test]
async fn output_is_capped_while_the_pipe_is_drained_and_stdin_is_delivered() {
    let dir = tempfile::tempdir().expect("workspace");
    let shell = LocalShell::unconfined(dir.path());
    let output = shell
        .run(powershell("[Console]::Out.Write('x' * 200000)").with_max_output_bytes(512))
        .await
        .expect("large output");
    assert_eq!(output.stdout.text.len(), 512);
    assert_eq!(output.stdout.total_bytes, 200_000);
    assert!(output.stdout.truncated);
    let echo = shell
        .run(
            powershell("[Console]::Out.Write([Console]::In.ReadToEnd())")
                .with_stdin("supplied input"),
        )
        .await
        .expect("stdin");
    assert_eq!(echo.stdout.text, "supplied input");
}

#[tokio::test]
async fn a_timeout_terminates_the_grandchild_and_retires_the_job() {
    let dir = tempfile::tempdir().expect("workspace");
    let shell = LocalShell::unconfined(dir.path());
    let path = dir.path().join("pid");
    let request = descendant(&path).with_timeout(Duration::from_secs(8));
    let run = shell.run(request);
    let (result, pid) = tokio::join!(run, recorded_pid(&path));
    assert!(result.expect("timeout outcome").timed_out);
    assert_dead(pid).await;
    assert_eq!(shell.live_groups(), 0);
}

#[tokio::test]
async fn cancelling_a_run_closes_its_job_and_kills_the_grandchild() {
    let dir = tempfile::tempdir().expect("workspace");
    let shell = LocalShell::unconfined(dir.path());
    let path = dir.path().join("pid");
    let mut running = shell.run(descendant(&path));
    let pid = tokio::select! {
        pid = recorded_pid(&path) => pid,
        result = &mut running => panic!("the long-running job exited early: {result:?}"),
    };
    assert_eq!(shell.live_groups(), 1);
    drop(running);
    assert_dead(pid).await;
    assert_eq!(shell.live_groups(), 0);
}

#[tokio::test]
async fn shutdown_kills_a_streamed_job_and_the_stream_reports_its_exit() {
    let dir = tempfile::tempdir().expect("workspace");
    let shell = LocalShell::unconfined(dir.path());
    let path = dir.path().join("pid");
    let mut stream = shell.spawn(descendant(&path)).await.expect("stream");
    let pid = recorded_pid(&path).await;
    assert_eq!(shell.kill_all().await.expect("shutdown"), 1);
    assert_eq!(shell.kill_all().await.expect("already stopped"), 0);
    let mut exits = 0usize;
    while let Some(event) = stream.next().await {
        if matches!(event, ShellEvent::Exited { .. }) {
            exits = exits.saturating_add(1);
        }
    }
    assert_eq!(exits, 1);
    assert_dead(pid).await;
    assert_eq!(shell.live_groups(), 0);
}

#[tokio::test]
async fn a_program_that_cannot_start_is_an_error_and_leaves_no_job() {
    let dir = tempfile::tempdir().expect("workspace");
    let shell = LocalShell::unconfined(dir.path());
    assert!(
        shell
            .run(ShellRequest::direct(
                "nanus-nonexistent-test-program",
                Vec::new()
            ))
            .await
            .is_err()
    );
    assert_eq!(shell.live_groups(), 0);
}

#[tokio::test]
async fn a_quoted_script_reaches_cmd_as_written() {
    // `cmd` does not undo the standard library's argument escaping, so an escaped script ran
    // `\"fix` and `bug\"` where the model wrote `"fix bug"`.
    let dir = tempfile::tempdir().expect("workspace");
    let shell = LocalShell::unconfined(dir.path());
    let spaced = dir.path().join("with space");
    std::fs::create_dir(&spaced).expect("a directory with a space");
    std::fs::write(spaced.join("note.txt"), "x").expect("a file inside it");
    let echoed = shell
        .run(ShellRequest::shell(r#"echo "a b""#, None))
        .await
        .expect("echo");
    assert_eq!(echoed.exit_code, Some(0), "{echoed:?}");
    assert_eq!(echoed.stdout.text.trim(), r#""a b""#);
    let listed = shell
        .run(ShellRequest::shell(
            r#"dir /b "with space""#,
            Some(dir.path().to_owned()),
        ))
        .await
        .expect("dir");
    assert_eq!(listed.exit_code, Some(0), "{listed:?}");
    assert_eq!(listed.stdout.text.trim(), "note.txt");
}

#[tokio::test]
async fn a_direct_request_is_still_quoted_for_the_program_that_parses_it() {
    // The negative direction: only a `cmd /C` script is passed raw. PowerShell parses its own
    // command line, so the quotes inside this argument must arrive escaped and come back intact.
    let dir = tempfile::tempdir().expect("workspace");
    let shell = LocalShell::unconfined(dir.path());
    let output = shell
        .run(powershell(r#"[Console]::Out.Write('x "y" z')"#))
        .await
        .expect("powershell");
    assert_eq!(output.exit_code, Some(0), "{output:?}");
    assert_eq!(output.stdout.text, r#"x "y" z"#);
}

#[tokio::test]
async fn a_background_process_ends_with_the_command_that_started_it() {
    // Deliberately unlike Unix, where only a timeout signals the group: on Windows the job is the
    // call's, so what a command leaves running when it exits is ended with it. The leader's own
    // exit status is still what the call reports.
    let dir = tempfile::tempdir().expect("workspace");
    let shell = LocalShell::unconfined(dir.path());
    let path = dir.path().join("pid");
    let output = shell
        .run(powershell(&format!(
            "$p = Start-Process powershell.exe -ArgumentList '-NoProfile', '-Command', \
             'Start-Sleep 300' -PassThru; \
             [IO.File]::WriteAllText('{}', [string]$p.Id); exit 4",
            path.display().to_string().replace('\'', "''")
        )))
        .await
        .expect("the leader exits");
    assert_eq!(output.exit_code, Some(4));
    assert!(!output.timed_out);
    assert_dead(recorded_pid(&path).await).await;
    assert_eq!(shell.live_groups(), 0);
}
