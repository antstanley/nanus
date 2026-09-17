//! The `!` escape: a command the reader runs, rather than one the model asks for.
//!
//! ## Why this is not a tool call
//!
//! Everything else that runs a program here goes through the agent: the model asks, the gate
//! decides, and the result is recorded in the session as something the model said and something it
//! was told. A `!` line is the other direction — a person typing at their own machine — and routing
//! it through the model would mean recording an instruction the model never gave, in a log the model
//! is later shown.
//!
//! So the command is the *interface's*: it runs in the directory the interface was started in, with
//! the interface's environment and privileges, and its output is drawn in the transcript as a notice
//! — the colour that means "the harness said this, not the model". Nothing about it reaches the
//! session log, and the model is never shown it. `SAFETY.md` says the same thing to a reader, in the
//! place they would look for it.
//!
//! ## Why it is asynchronous
//!
//! The interface runs on one thread, in a local task set that also carries the link's transport: a
//! child process waited on synchronously would stop the agent's frames being read for as long as the
//! command ran, and a turn's stream would arrive in one lump afterwards — or not at all, since a
//! full frame queue drops progress rather than blocking.
//!
//! ## Why the output is bounded
//!
//! `!find /` prints more than the transcript should hold. What is drawn is the first rows of it and
//! a count of what was left, because the alternative is a screen a reader can no longer scroll to
//! the conversation through.

// The module is private, so `pub(crate)` and `pub` are the same reachability; the explicit
// `pub(crate)` says which surface these items are meant for, and this is the lint's counterpart —
// the same allow `paste.rs`, `mentions.rs`, `help.rs`, and `markdown/mod.rs` carry.
#![allow(clippy::redundant_pub_crate)]

use std::fmt::Write as _;
use std::process::Stdio;

use tokio::process::Command;

/// How many lines of a command's output are drawn.
const MAX_LINES: usize = 40;

/// The most characters of a command's output that are drawn.
///
/// A bound on characters as well as on lines, because one line can be a megabyte: a command that
/// printed a minified file is one line long and would otherwise be drawn in full.
const MAX_CHARS: usize = 4_000;

/// What one command did.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Outcome {
    /// The command, as it was typed, without its `!`.
    pub(crate) command: String,
    /// What the command wrote to standard output.
    pub(crate) stdout: String,
    /// What it wrote to standard error.
    pub(crate) stderr: String,
    /// Its exit code, when it reported one.
    ///
    /// `None` is a command killed by a signal, or one that never started: neither has an exit code,
    /// and drawing zero for them would say the command succeeded.
    pub(crate) status: Option<i32>,
    /// Why it could not be run at all, when it could not.
    pub(crate) failed: Option<String>,
}

impl Outcome {
    /// Records a command that could not be started.
    fn refused(command: &str, reason: String) -> Self {
        Self {
            command: command.to_owned(),
            stdout: String::new(),
            stderr: String::new(),
            status: None,
            failed: Some(reason),
        }
    }

    /// Whether the command finished successfully.
    #[must_use]
    pub(crate) fn succeeded(&self) -> bool {
        self.failed.is_none() && self.status == Some(0)
    }
}

/// Runs one command through the shell, in the directory the interface was started in.
///
/// `sh -c` rather than a split argument list, because the line is a command line: `!ls | wc -l` and
/// `!for f in *; do …; done` are things a reader types, and splitting them here would mean the
/// interface implementing a shell's grammar rather than handing the line to the shell.
///
/// The directory is inherited rather than set: `!` means "run this here", and "here" is where the
/// reader started the interface. It is *not* the workspace root, which is where the agent's tools
/// are rooted — the two are the same in the ordinary case of `nanus tui` in a project, and a reader
/// who attached to a service elsewhere should still get their own shell.
pub(crate) async fn run(command: &str) -> Outcome {
    let child = Command::new("sh")
        .arg("-c")
        .arg(command)
        // The streams are read rather than inherited, so the output can be drawn in the transcript
        // rather than appearing behind the alternate screen the interface is drawing on.
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await;
    match child {
        Ok(output) => Outcome {
            command: command.to_owned(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            status: output.status.code(),
            failed: None,
        },
        Err(error) => Outcome::refused(command, error.to_string()),
    }
}

/// Renders what a command did, as the text the transcript shows.
///
/// The command itself leads, because a transcript of several commands otherwise shows output with
/// nothing saying what produced it. A command that succeeded says nothing about that: it is the
/// shell's own convention, and a line saying `[exit 0]` under every successful command is noise a
/// reader learns to skip — which is exactly the line they would then skip when it said something
/// else.
#[must_use]
pub(crate) fn report(outcome: &Outcome) -> String {
    let mut text = format!("$ {}", outcome.command);
    if let Some(reason) = &outcome.failed {
        let _ = write!(text, "\n{reason}");
        return text;
    }
    for block in [&outcome.stdout, &outcome.stderr] {
        if block.trim().is_empty() {
            continue;
        }
        text.push('\n');
        text.push_str(&bounded(block));
    }
    match outcome.status {
        Some(0) => {}
        Some(code) => {
            let _ = write!(text, "\n[exit {code}]");
        }
        None => text.push_str("\n[killed]"),
    }
    text
}

/// The first [`MAX_LINES`] lines and [`MAX_CHARS`] characters of `text`, with a count of the rest.
///
/// Two bounds rather than one, because they catch different commands: `find /` prints more lines than
/// a screen can hold, and `cat minified.js` prints one line that is longer than all of them together.
/// A cut inside a line is marked on that line, so a reader can tell a truncated line from a short
/// one, and the lines that were dropped are counted.
fn bounded(text: &str) -> String {
    let all: Vec<&str> = text.trim_end_matches('\n').split('\n').collect();
    let mut kept: Vec<String> = Vec::new();
    let mut used = 0_usize;
    for line in &all {
        let room = MAX_CHARS.saturating_sub(used);
        if kept.len() >= MAX_LINES || room == 0 {
            break;
        }
        let piece: String = line.chars().take(room).collect();
        used = used.saturating_add(piece.chars().count());
        let cut = piece.chars().count() < line.chars().count();
        kept.push(if cut { format!("{piece}…") } else { piece });
        if cut {
            break;
        }
    }
    let dropped = all.len().saturating_sub(kept.len());
    let joined = kept.join("\n");
    if dropped == 0 {
        return joined;
    }
    format!("{joined}\n… and {dropped} more lines")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(stdout: &str, status: Option<i32>) -> Outcome {
        Outcome {
            command: String::from("ls"),
            stdout: stdout.to_owned(),
            stderr: String::new(),
            status,
            failed: None,
        }
    }

    /// The command leads the report, because output with nothing saying what produced it is a
    /// transcript a reader has to reconstruct.
    #[test]
    fn a_report_says_what_was_run_and_what_came_back() {
        assert_eq!(report(&outcome("a\nb\n", Some(0))), "$ ls\na\nb");
        // A successful command says nothing else: `[exit 0]` under every one is noise.
        assert!(!report(&outcome("a\n", Some(0))).contains("exit"));
        // Anything else is said, and said where a reader is looking.
        assert!(report(&outcome("", Some(1))).contains("[exit 1]"));
        assert!(report(&outcome("", None)).contains("[killed]"));

        // A command that printed nothing is one line: what was run, and nothing more.
        assert_eq!(report(&outcome("", Some(0))), "$ ls");

        // Standard error is shown too, because a command that failed usually explained why there.
        let mut complained = outcome("", Some(2));
        complained.stderr = String::from("ls: nope: No such file or directory\n");
        let text = report(&complained);
        assert!(text.contains("No such file or directory"), "{text}");
        assert!(text.contains("[exit 2]"), "{text}");
    }

    #[test]
    fn a_command_that_could_not_be_started_says_so() {
        let refused = Outcome::refused("ls", String::from("no such file or directory"));
        let text = report(&refused);
        assert!(text.starts_with("$ ls"), "{text}");
        assert!(text.contains("no such file or directory"), "{text}");
        assert!(!refused.succeeded());
    }

    /// Long output is bounded, and the bound is stated: a reader who cannot see the rest should know
    /// there is a rest rather than believing the command printed forty lines.
    #[test]
    fn long_output_is_bounded_and_counted() {
        let many: String = (0..200).fold(String::new(), |mut all, index| {
            let _ = writeln!(all, "line {index}");
            all
        });
        let text = report(&outcome(&many, Some(0)));
        assert!(text.contains("line 0"), "{text}");
        assert!(
            !text.contains("line 199"),
            "everything past the bound is gone"
        );
        assert!(text.contains("more lines"), "{text}");
        assert!(
            text.lines().count() <= MAX_LINES.saturating_add(2),
            "the bound is kept: {}",
            text.lines().count()
        );

        // One enormous line is bounded too: a bound on lines alone would let a minified file
        // through in full.
        let wide = "x".repeat(MAX_CHARS.saturating_mul(2));
        let cut = report(&outcome(&wide, Some(0)));
        assert!(
            cut.chars().count() <= MAX_CHARS.saturating_add(64),
            "{}",
            cut.chars().count()
        );
        assert!(
            cut.trim_end().ends_with('…'),
            "a line cut short says so: {}",
            cut.trim_end().chars().rev().take(1).collect::<String>()
        );
    }

    /// The whole path: a real shell, a real child process, and the streams back.
    #[test]
    fn a_command_runs_through_a_shell_and_reports_what_it_did() {
        let ran = nanus_kernel::runtime::block_on_local(async {
            let printed = run("printf 'hi'").await;
            let failed = run("printf 'oops' >&2; exit 3").await;
            (printed, failed)
        });
        assert!(ran.0.succeeded(), "{:?}", ran.0);
        assert_eq!(ran.0.stdout, "hi");
        assert_eq!(report(&ran.0), "$ printf 'hi'\nhi");

        assert_eq!(ran.1.status, Some(3));
        assert!(ran.1.stderr.contains("oops"), "{:?}", ran.1);
        assert!(!ran.1.succeeded());
        let text = report(&ran.1);
        assert!(text.contains("oops") && text.contains("[exit 3]"), "{text}");
    }

    /// A shell's own syntax is the shell's business: the line is handed over whole rather than split
    /// by the interface.
    #[test]
    fn the_line_is_a_command_line_rather_than_an_argument_list() {
        let piped = nanus_kernel::runtime::block_on_local(async {
            run("printf 'a\\nb\\nc\\n' | wc -l").await
        });
        assert!(piped.succeeded(), "{piped:?}");
        assert_eq!(piped.stdout.trim(), "3");
    }
}
