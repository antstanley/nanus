//! The shell port: process execution, live output, and sandbox confinement.
//!
//! ### A non-zero exit code is not an error
//!
//! This is the rule most likely to be implemented wrong. `grep` exits 1 when it
//! finds nothing, `git diff --quiet` exits 1 when there are differences, and a
//! compiler exits 1 when it finds a type error. All three are *answers*. So
//! [`ShellPort::run`] returns a successful result holding a [`ShellOutcome`]
//! whose [`exit_code`](ShellOutcome::exit_code) is `Some(1)`. The `Err` channel
//! is reserved for the cases where there is no answer at all: the program could
//! not be started, the working directory was unusable, or the sandbox refused
//! the work.
//!
//! ### The platform shell is always available
//!
//! [`ShellRequest::shell`] runs a script through the platform shell
//! (`sh -c` on unix), which is what a `bash` tool needs: pipelines, redirection,
//! and globbing are the point. [`ShellRequest::direct`] runs an argv without a
//! shell, which is what a tool that must not interpret its arguments needs.
//! Both go through the same port so both are subject to the same sandbox and the
//! same process-group tracking.
//!
//! ### Shutdown needs to reap grandchildren
//!
//! [`ShellPort::kill_all`] exists because a shell scripts spawns children of its
//! own, and killing the direct child leaves them running. The count it returns
//! is the number of tracked process groups that were signalled, so a shutdown
//! path can report what it reaped.

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::Duration;

use futures::Stream;
use nanus_domain::SandboxMode;
use serde::{Deserialize, Serialize};

use crate::LocalBoxFuture;
use crate::fs::ensure_within;

/// A shared, key-addressable shell.
pub type ShellHandle = std::rc::Rc<Box<dyn ShellPort>>;

/// The name of the platform's command interpreter.
#[cfg(unix)]
pub const PLATFORM_SHELL: &str = "sh";

/// The name of the platform's command interpreter.
#[cfg(windows)]
pub const PLATFORM_SHELL: &str = "cmd";

/// The flag that makes [`PLATFORM_SHELL`] read a script from its argument.
#[cfg(unix)]
pub const PLATFORM_SHELL_FLAG: &str = "-c";

/// The flag that makes [`PLATFORM_SHELL`] read a script from its argument.
#[cfg(windows)]
pub const PLATFORM_SHELL_FLAG: &str = "/C";

/// The default cap on captured output per stream, in bytes.
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 1_048_576;

/// A pinned stream of live process output.
pub type ShellStream = Pin<Box<dyn Stream<Item = ShellEvent> + 'static>>;

/// The shell port.
pub trait ShellPort {
    /// Runs a command to completion, capturing its output.
    ///
    /// A non-zero exit code is a successful call with a non-zero
    /// [`ShellOutcome::exit_code`]. The `Err` channel is for "the command never
    /// ran".
    fn run(&self, request: ShellRequest) -> LocalBoxFuture<'_, ShellResult<ShellOutcome>>;

    /// Starts a command and returns its live output stream.
    ///
    /// The stream ends with exactly one [`ShellEvent::Exited`], so a consumer
    /// that reads to the end always learns how the process finished.
    fn spawn(&self, request: ShellRequest) -> LocalBoxFuture<'_, ShellResult<ShellStream>>;

    /// Signals every process group this port started, returning how many.
    ///
    /// Called at shutdown so a harness does not leave orphaned grandchildren.
    fn kill_all(&self) -> LocalBoxFuture<'_, ShellResult<usize>>;

    /// Returns the sandbox policy this port enforces.
    fn sandbox(&self) -> SandboxPolicy;
}

/// The result type of every shell operation.
pub type ShellResult<T> = Result<T, ShellError>;

/// One command to run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellRequest {
    /// The program to execute.
    pub program: String,
    /// Its arguments, unmodified.
    pub args: Vec<String>,
    /// The working directory, when it differs from the process's own.
    pub cwd: Option<PathBuf>,
    /// Extra environment variables, in a deterministic order.
    pub env: Vec<(String, String)>,
    /// How long to allow, when the caller wants a bound.
    pub timeout: Option<Duration>,
    /// Text to write to the process's standard input.
    pub stdin: Option<String>,
    /// The per-stream output cap, in bytes.
    pub max_output_bytes: usize,
}

impl ShellRequest {
    /// Builds a request that runs `script` through the platform shell.
    ///
    /// This is the form a `bash` tool uses. The script is passed as a single
    /// argument to `sh -c`, so quoting is the script author's responsibility and
    /// exactly as expressive as the shell itself.
    #[must_use]
    pub fn shell(script: impl Into<String>, cwd: Option<PathBuf>) -> Self {
        Self {
            program: PLATFORM_SHELL.to_owned(),
            args: vec![PLATFORM_SHELL_FLAG.to_owned(), script.into()],
            cwd,
            env: Vec::new(),
            timeout: None,
            stdin: None,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
        }
    }

    /// Builds a request that runs `program` with `args` and no shell.
    ///
    /// Because no shell parses the arguments, a path containing a space or a
    /// semicolon is passed through literally. Use this whenever the arguments
    /// are not authored by a human.
    #[must_use]
    pub fn direct(program: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            program: program.into(),
            args,
            cwd: None,
            env: Vec::new(),
            timeout: None,
            stdin: None,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
        }
    }

    /// Sets the working directory.
    #[must_use]
    pub fn with_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// Adds an environment variable.
    #[must_use]
    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// Sets a timeout.
    #[must_use]
    pub const fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Sets the text written to standard input.
    #[must_use]
    pub fn with_stdin(mut self, stdin: impl Into<String>) -> Self {
        self.stdin = Some(stdin.into());
        self
    }

    /// Replaces the per-stream output cap.
    #[must_use]
    pub const fn with_max_output_bytes(mut self, max_output_bytes: usize) -> Self {
        self.max_output_bytes = max_output_bytes;
        self
    }

    /// Returns `true` when this request runs through the platform shell.
    #[must_use]
    pub fn is_shell_wrapped(&self) -> bool {
        self.program == PLATFORM_SHELL
            && self
                .args
                .first()
                .is_some_and(|flag| flag.as_str() == PLATFORM_SHELL_FLAG)
    }
}

/// One increment of live process output.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ShellEvent {
    /// A chunk of standard output.
    Stdout {
        /// The chunk.
        chunk: String,
    },
    /// A chunk of standard error.
    Stderr {
        /// The chunk.
        chunk: String,
    },
    /// The process ended.
    ///
    /// Exactly one of these ends a [`ShellStream`], so a consumer never has to
    /// guess whether the process is still running.
    Exited {
        /// The exit code, absent when a signal killed the process.
        exit_code: Option<i32>,
        /// The signal that killed the process, when one did.
        signal: Option<i32>,
        /// How long the process ran.
        duration_ms: u64,
        /// Whether the port's own timeout ended it.
        timed_out: bool,
    },
}

/// One captured stream of output.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Captured {
    /// The captured text, cut to the request's cap.
    pub text: String,
    /// Whether the cap cut it.
    pub truncated: bool,
    /// How many bytes the stream produced in total, cap or no cap.
    pub total_bytes: u64,
}

/// How a command finished.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellOutcome {
    /// The exit code, absent when a signal killed the process.
    pub exit_code: Option<i32>,
    /// The signal that killed the process, when one did.
    pub signal: Option<i32>,
    /// Whether the port's timeout ended the process.
    pub timed_out: bool,
    /// How long the process ran.
    pub duration_ms: u64,
    /// What it wrote to standard output.
    pub stdout: Captured,
    /// What it wrote to standard error.
    pub stderr: Captured,
}

impl ShellOutcome {
    /// Returns `true` only for a clean exit with no signal and no timeout.
    ///
    /// This is the predicate a caller uses to decide whether a command
    /// *succeeded*. A false answer is not an error: see the module docs.
    #[must_use]
    pub fn is_success(&self) -> bool {
        self.exit_code == Some(0) && self.signal.is_none() && !self.timed_out
    }

    /// Renders both streams, standard error last.
    #[must_use]
    pub fn combined_text(&self) -> String {
        if self.stderr.text.is_empty() {
            return self.stdout.text.clone();
        }
        if self.stdout.text.is_empty() {
            return self.stderr.text.clone();
        }
        let mut out = self.stdout.text.clone();
        out.push('\n');
        out.push_str(&self.stderr.text);
        out
    }
}

/// What a port is allowed to touch.
///
/// [`SandboxMode`] is the domain's vocabulary, re-used rather than redefined so
/// a permission preset and a shell policy cannot disagree. This type adds the
/// two facts the shell needs in order to enforce a mode: *where* the workspace
/// is, and whether the network is reachable.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxPolicy {
    /// The mode to enforce.
    pub mode: SandboxMode,
    /// The root that `WorkspaceWrite` confines writes to.
    pub workspace_root: PathBuf,
    /// Whether the sandbox allows network access.
    pub allow_network: bool,
}

impl SandboxPolicy {
    /// Builds a policy for `mode` rooted at `workspace_root`.
    #[must_use]
    pub fn new(mode: SandboxMode, workspace_root: impl Into<PathBuf>) -> Self {
        Self {
            mode,
            workspace_root: workspace_root.into(),
            allow_network: false,
        }
    }

    /// Builds the read-only policy: no writes, no network.
    #[must_use]
    pub fn read_only(workspace_root: impl Into<PathBuf>) -> Self {
        Self::new(SandboxMode::ReadOnly, workspace_root)
    }

    /// Builds the workspace-write policy: writes confined to the root.
    #[must_use]
    pub fn workspace_write(workspace_root: impl Into<PathBuf>) -> Self {
        Self::new(SandboxMode::WorkspaceWrite, workspace_root)
    }

    /// Builds the danger-full-access policy.
    ///
    /// Network access is still off: "unconfined filesystem" and "reachable
    /// network" are separate decisions, and granting both at once would be a
    /// default nobody chose.
    #[must_use]
    pub fn danger_full_access(workspace_root: impl Into<PathBuf>) -> Self {
        Self::new(SandboxMode::DangerFullAccess, workspace_root)
    }

    /// Enables network access.
    #[must_use]
    pub const fn with_network(mut self) -> Self {
        self.allow_network = true;
        self
    }

    /// Returns `true` when the policy permits writing anywhere at all.
    #[must_use]
    pub const fn permits_writes(&self) -> bool {
        self.mode.permits_writes()
    }

    /// Returns `true` when the policy confines writes to a root.
    #[must_use]
    pub const fn is_confined(&self) -> bool {
        self.mode.is_confined()
    }

    /// Checks that a write is permitted at all.
    ///
    /// # Errors
    ///
    /// Returns [`ShellError::SandboxRefused`] under
    /// [`SandboxMode::ReadOnly`].
    pub fn ensure_writes_permitted(&self) -> ShellResult<()> {
        if self.permits_writes() {
            return Ok(());
        }
        Err(ShellError::SandboxRefused { mode: self.mode })
    }

    /// Normalises `path` and refuses anything outside the confined root.
    ///
    /// # Errors
    ///
    /// Returns [`ShellError::OutsideWorkspace`] when the mode is confined and
    /// the path escapes. Under [`SandboxMode::DangerFullAccess`] there is no
    /// root to escape, so the path is returned as given.
    pub fn confine(&self, path: &Path) -> ShellResult<PathBuf> {
        if !self.is_confined() {
            return Ok(path.to_path_buf());
        }
        ensure_within(&self.workspace_root, path).map_err(|_| ShellError::OutsideWorkspace {
            root: self.workspace_root.clone(),
            path: path.to_path_buf(),
        })
    }
}

/// Why a command could not be run.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ShellError {
    /// The platform has no sandbox this port can use.
    ///
    /// Reported rather than silently ignored: a policy that cannot be enforced
    /// must not look like one that was.
    #[error("no sandbox is available on {platform}")]
    SandboxUnavailable {
        /// The platform that lacks one.
        platform: String,
    },

    /// The sandbox refused the operation.
    #[error("the sandbox refused a write in {mode} mode")]
    SandboxRefused {
        /// The mode that refused.
        mode: SandboxMode,
    },

    /// The program could not be started.
    #[error("{program} could not be started: {message}")]
    Spawn {
        /// The program that failed to start.
        program: String,
        /// The rendered failure.
        message: String,
    },

    /// The working directory was unusable.
    #[error("working directory {path} is unusable: {message}")]
    Cwd {
        /// The offending path.
        path: PathBuf,
        /// The rendered failure.
        message: String,
    },

    /// A path escaped the workspace root.
    #[error("{path} is outside the workspace root {root}")]
    OutsideWorkspace {
        /// The workspace root.
        root: PathBuf,
        /// The rejected path.
        path: PathBuf,
    },

    /// A request carried an output cap the port cannot honour.
    #[error("the output cap must be a non-zero byte count, got {limit}")]
    InvalidOutputCap {
        /// The rejected cap.
        limit: usize,
    },

    /// Any other input/output failure.
    #[error("I/O failure: {message}")]
    Io {
        /// The rendered failure.
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(exit_code: Option<i32>) -> ShellOutcome {
        ShellOutcome {
            exit_code,
            signal: None,
            timed_out: false,
            duration_ms: 3,
            stdout: Captured {
                text: "out".to_owned(),
                truncated: false,
                total_bytes: 3,
            },
            stderr: Captured {
                text: String::new(),
                truncated: false,
                total_bytes: 0,
            },
        }
    }

    #[test]
    fn a_non_zero_exit_is_an_answer_and_not_a_failure() {
        // The rule, stated as an assertion: exit 1 is a successful call whose
        // outcome says the answer was "no".
        let mut ran = outcome(Some(1));
        assert!(!ran.is_success(), "grep found nothing");
        assert_eq!(ran.exit_code, Some(1));
        // The outcome itself is a perfectly good value; nothing about it is an
        // error, which is what `Ok(..)` at the call site means.
        let result: ShellResult<ShellOutcome> = Ok(ran.clone());
        assert!(result.is_ok());
        let Ok(returned) = result else { return };
        assert!(!returned.is_success());

        ran.exit_code = Some(0);
        assert!(ran.is_success());
    }

    #[test]
    fn a_signalled_or_timed_out_process_is_not_a_success() {
        let mut signalled = outcome(None);
        signalled.signal = Some(9);
        assert!(!signalled.is_success());

        let mut timed_out = outcome(Some(0));
        timed_out.timed_out = true;
        assert!(
            !timed_out.is_success(),
            "a killed process did not succeed even if it exited zero"
        );
    }

    #[test]
    fn the_shell_form_always_goes_through_the_platform_shell() {
        let request = ShellRequest::shell("echo hi | wc -l", None);
        assert_eq!(request.program, PLATFORM_SHELL);
        assert_eq!(
            request.args.first().map(String::as_str),
            Some(PLATFORM_SHELL_FLAG)
        );
        assert_eq!(
            request.args.get(1).map(String::as_str),
            Some("echo hi | wc -l")
        );
        assert!(request.is_shell_wrapped());
    }

    #[test]
    fn the_direct_form_does_not_invoke_a_shell() {
        let request = ShellRequest::direct("git", vec!["status".to_owned()]);
        assert_eq!(request.program, "git");
        assert!(!request.is_shell_wrapped(), "argv is not interpreted");
        // A shell metacharacter survives as data.
        let literal = ShellRequest::direct("echo", vec!["a; rm -rf /".to_owned()]);
        assert_eq!(
            literal.args.first().map(String::as_str),
            Some("a; rm -rf /")
        );
    }

    #[test]
    fn a_request_carries_its_limits_and_environment() {
        let request = ShellRequest::shell("true", Some(PathBuf::from("/work")))
            .with_env("A", "1")
            .with_env("B", "2")
            .with_timeout(Duration::from_millis(50))
            .with_stdin("input")
            .with_max_output_bytes(16);
        assert_eq!(request.cwd, Some(PathBuf::from("/work")));
        assert_eq!(request.env.len(), 2);
        assert_eq!(request.timeout, Some(Duration::from_millis(50)));
        assert_eq!(request.stdin.as_deref(), Some("input"));
        assert_eq!(request.max_output_bytes, 16);
    }

    #[test]
    fn combined_output_renders_both_streams() {
        let mut both = outcome(Some(0));
        both.stderr.text = "err".to_owned();
        assert_eq!(both.combined_text(), "out\nerr");
        assert_eq!(outcome(Some(0)).combined_text(), "out");
        let mut only_err = outcome(Some(0));
        only_err.stdout.text = String::new();
        only_err.stderr.text = "err".to_owned();
        assert_eq!(only_err.combined_text(), "err");
    }

    #[test]
    fn a_read_only_policy_refuses_writes() {
        let policy = SandboxPolicy::read_only("/work");
        assert!(!policy.permits_writes());
        assert!(policy.is_confined());
        assert!(matches!(
            policy.ensure_writes_permitted(),
            Err(ShellError::SandboxRefused {
                mode: SandboxMode::ReadOnly
            })
        ));
    }

    #[test]
    fn a_confined_policy_refuses_paths_that_escape() {
        let policy = SandboxPolicy::workspace_write("/work");
        assert!(policy.permits_writes());
        assert_eq!(
            policy.confine(Path::new("src/lib.rs")).ok(),
            Some(PathBuf::from("/work/src/lib.rs"))
        );
        assert!(matches!(
            policy.confine(Path::new("../../etc/passwd")),
            Err(ShellError::OutsideWorkspace { .. })
        ));
        // Negative space: the same call inside the root succeeds, so the refusal
        // is the escape and not the fixture.
        assert!(policy.confine(Path::new("src")).is_ok());
    }

    #[test]
    fn danger_full_access_is_unconfined_but_still_not_a_network_grant() {
        let policy = SandboxPolicy::danger_full_access("/work");
        assert!(!policy.is_confined());
        assert!(policy.confine(Path::new("/etc/passwd")).is_ok());
        assert!(
            !policy.allow_network,
            "unconfined filesystem access is not a network grant"
        );
        assert!(policy.ensure_writes_permitted().is_ok());
        assert!(policy.with_network().allow_network);
    }

    #[test]
    fn each_constructor_pairs_a_mode_with_its_policy() {
        assert_eq!(SandboxPolicy::read_only("/w").mode, SandboxMode::ReadOnly);
        assert_eq!(
            SandboxPolicy::workspace_write("/w").mode,
            SandboxMode::WorkspaceWrite
        );
        assert_eq!(
            SandboxPolicy::danger_full_access("/w").mode,
            SandboxMode::DangerFullAccess
        );
        assert_eq!(
            SandboxPolicy::read_only("/w").workspace_root,
            PathBuf::from("/w")
        );
    }
}
