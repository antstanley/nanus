//! The macOS keychain, through the platform's own `security` tool.
//!
//! ## Why the tool and not a binding
//!
//! The interface reads the clipboard through `pbpaste`, and this adapter reads the
//! keychain through `/usr/bin/security`, for the same reason: the platform already
//! ships a program that does this well, and linking a binding would put a
//! dependency — and, in most crates, an `unsafe` block — between the harness and a
//! store it only touches twice per install. The workspace forbids `unsafe`
//! everywhere, so a binding is not an option anyway.
//!
//! ## The value is on the command line
//!
//! `security add-generic-password` accepts the secret only as an argument (`-w`)
//! or as a terminal prompt; it does not read standard input, which was verified
//! against the shipped tool rather than assumed. So for the lifetime of the
//! `security` invocation the value is visible in that process's argument list to
//! anything running as this user. That is stated in `SAFETY.md` rather than
//! hidden: the exposure is a few milliseconds, to a process the same user already
//! controls, and the alternative — a file the tool would have to read — is a worse
//! place to leave a key.
//!
//! ## A keychain of its own
//!
//! [`KeychainBackend::with_keychain`] points the backend at an explicit keychain
//! file instead of the user's default. Two reasons: a user may keep separate
//! profiles, and a test must not write to the login keychain — the suite creates a
//! throwaway keychain in a temporary directory and removes it afterwards.

use std::path::{Path, PathBuf};
use std::process::Command;

use nanus_ports::{LocalBoxFuture, Secret, SecretError, SecretResult};

use crate::backend::SecretBackend;

/// The platform tool that talks to the keychain.
pub const DEFAULT_PROGRAM: &str = "/usr/bin/security";

/// The status `security` exits with when an item is not there.
///
/// `errSecItemNotFound`, which both `find-generic-password` and
/// `delete-generic-password` report. It is the difference between "no key is
/// stored", which is a normal answer, and "the keychain could not be read", which
/// is not.
pub const NOT_FOUND_STATUS: i32 = 44;

/// The keychain, as a secret store.
#[derive(Clone, Debug)]
pub struct KeychainBackend {
    program: PathBuf,
    keychain: Option<PathBuf>,
}

impl Default for KeychainBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl KeychainBackend {
    /// Creates a backend over the user's default keychain.
    #[must_use]
    pub fn new() -> Self {
        Self {
            program: PathBuf::from(DEFAULT_PROGRAM),
            keychain: None,
        }
    }

    /// Creates a backend over an explicit keychain file.
    ///
    /// The path is handed to `security` as its keychain argument, so nothing is
    /// read from or written to the user's default keychain.
    #[must_use]
    pub fn with_keychain(keychain: impl Into<PathBuf>) -> Self {
        Self {
            program: PathBuf::from(DEFAULT_PROGRAM),
            keychain: Some(keychain.into()),
        }
    }

    /// Returns the `security` program this backend runs.
    #[must_use]
    pub fn program(&self) -> &Path {
        &self.program
    }

    /// Returns the keychain file this backend is pointed at, if any.
    #[must_use]
    pub fn keychain(&self) -> Option<&Path> {
        self.keychain.as_deref()
    }

    /// The arguments that read a secret.
    #[must_use]
    pub fn find_args(&self, service: &str, account: &str) -> Vec<String> {
        self.with_keychain_arg(vec![
            String::from("find-generic-password"),
            String::from("-s"),
            service.to_owned(),
            String::from("-a"),
            account.to_owned(),
            // `-w` last prints only the password, which is the whole reason the
            // store's output is usable rather than being an attribute dump.
            String::from("-w"),
        ])
    }

    /// The arguments that write a secret, replacing any item already there.
    #[must_use]
    pub fn add_args(&self, service: &str, account: &str, secret: &str) -> Vec<String> {
        self.with_keychain_arg(vec![
            String::from("add-generic-password"),
            String::from("-s"),
            service.to_owned(),
            String::from("-a"),
            account.to_owned(),
            String::from("-w"),
            secret.to_owned(),
            // Update rather than fail when the item exists, which makes `set` a
            // replace on every call instead of a first-time-only affordance.
            String::from("-U"),
        ])
    }

    /// The arguments that remove a secret.
    #[must_use]
    pub fn delete_args(&self, service: &str, account: &str) -> Vec<String> {
        self.with_keychain_arg(vec![
            String::from("delete-generic-password"),
            String::from("-s"),
            service.to_owned(),
            String::from("-a"),
            account.to_owned(),
        ])
    }

    /// Appends the keychain path, which the tool takes as a trailing argument.
    fn with_keychain_arg(&self, mut args: Vec<String>) -> Vec<String> {
        if let Some(keychain) = &self.keychain {
            args.push(keychain.display().to_string());
        }
        args
    }

    /// Runs the tool and returns its output, or an error that means "no store".
    fn run(&self, args: &[String]) -> SecretResult<std::process::Output> {
        Command::new(&self.program)
            .args(args)
            .output()
            .map_err(|error| {
                SecretError::unavailable(
                    "keychain",
                    format!("{} could not be run: {error}", self.program.display()),
                )
            })
    }
}

/// Decodes a successful read into a secret or an absence.
fn decode_read(account: &str, stdout: &[u8]) -> SecretResult<Option<Secret>> {
    let text = String::from_utf8(stdout.to_vec()).map_err(|_| SecretError::NotText {
        account: account.to_owned(),
    })?;
    let trimmed = text.trim_end_matches(['\r', '\n']).to_owned();
    if trimmed.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(Secret::new(trimmed)))
}

impl SecretBackend for KeychainBackend {
    fn name(&self) -> &'static str {
        "keychain"
    }

    fn get<'a>(
        &'a self,
        service: &'a str,
        account: &'a str,
    ) -> LocalBoxFuture<'a, SecretResult<Option<Secret>>> {
        Box::pin(async move {
            let output = self.run(&self.find_args(service, account))?;
            if output.status.success() {
                return decode_read(account, &output.stdout);
            }
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            if is_missing(output.status.code(), &stderr) {
                return Ok(None);
            }
            Err(SecretError::refused("keychain", "read", account, stderr))
        })
    }

    fn set<'a>(
        &'a self,
        service: &'a str,
        account: &'a str,
        secret: &'a str,
    ) -> LocalBoxFuture<'a, SecretResult<()>> {
        Box::pin(async move {
            let output = self.run(&self.add_args(service, account, secret))?;
            if output.status.success() {
                return Ok(());
            }
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            Err(SecretError::refused("keychain", "write", account, stderr))
        })
    }

    fn clear<'a>(
        &'a self,
        service: &'a str,
        account: &'a str,
    ) -> LocalBoxFuture<'a, SecretResult<bool>> {
        Box::pin(async move {
            let output = self.run(&self.delete_args(service, account))?;
            if output.status.success() {
                return Ok(true);
            }
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            if is_missing(output.status.code(), &stderr) {
                return Ok(false);
            }
            Err(SecretError::refused("keychain", "delete", account, stderr))
        })
    }
}

/// Returns `true` when a failure means "no such item".
///
/// Two signals, because they answer slightly different questions: the status is
/// the documented `errSecItemNotFound`, and the text is what an older tool prints
/// when it exits with a generic status. The text is only consulted for a non-zero
/// status, so it cannot turn a successful run into an absence.
#[must_use]
pub fn is_missing(status: Option<i32>, stderr: &str) -> bool {
    if status == Some(NOT_FOUND_STATUS) {
        return true;
    }
    if status == Some(0) {
        return false;
    }
    let lowered = stderr.to_ascii_lowercase();
    lowered.contains("could not be found") || lowered.contains("item not found")
}

/// Removes a throwaway keychain even when an assertion fails.
#[cfg(test)]
struct KeychainGuard(PathBuf);

#[cfg(test)]
impl Drop for KeychainGuard {
    fn drop(&mut self) {
        let name = self.0.display().to_string();
        let _ = Command::new(DEFAULT_PROGRAM)
            .args(["delete-keychain", &name])
            .output();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_commands_carry_the_service_and_account() {
        let backend = KeychainBackend::new();
        assert_eq!(
            backend.find_args("nanus", "openai"),
            vec!["find-generic-password", "-s", "nanus", "-a", "openai", "-w"]
        );
        assert_eq!(
            backend.delete_args("nanus", "openai"),
            vec!["delete-generic-password", "-s", "nanus", "-a", "openai"]
        );
    }

    /// A write names the secret and asks the tool to replace an existing item, so
    /// `set` means replace rather than "only the first time".
    #[test]
    fn a_write_replaces_an_existing_item() {
        let backend = KeychainBackend::new();
        let args = backend.add_args("nanus", "openai", "sk-value");
        assert_eq!(
            args.first().map(String::as_str),
            Some("add-generic-password")
        );
        assert!(args.iter().any(|arg| arg == "-U"), "{args:?}");
        // The value follows `-w`, which is what makes it the password rather than a
        // keychain to operate on.
        let value = args
            .iter()
            .position(|arg| arg == "-w")
            .and_then(|index| args.get(index.saturating_add(1)));
        assert_eq!(value.map(String::as_str), Some("sk-value"));
    }

    /// An explicit keychain is a trailing argument, which is how the tool is told
    /// to leave the user's default keychain alone.
    #[test]
    fn an_explicit_keychain_is_named_last() {
        let backend = KeychainBackend::with_keychain("/tmp/throwaway.keychain");
        let find = backend.find_args("nanus", "openai");
        assert_eq!(
            find.last().map(String::as_str),
            Some("/tmp/throwaway.keychain")
        );
        let delete = backend.delete_args("nanus", "openai");
        assert_eq!(
            delete.last().map(String::as_str),
            Some("/tmp/throwaway.keychain")
        );
        assert_eq!(
            backend.keychain(),
            Some(Path::new("/tmp/throwaway.keychain"))
        );
    }

    /// Not-found is an answer; anything else is a failure. Both directions, because
    /// a classifier that said "missing" too readily would hide a locked keychain.
    #[test]
    fn only_a_missing_item_is_an_absence() {
        assert!(is_missing(Some(NOT_FOUND_STATUS), ""));
        assert!(is_missing(Some(1), "security: item could not be found"));
        // A zero status is never "missing": the command succeeded.
        assert!(!is_missing(
            Some(0),
            "the specified item could not be found"
        ));
        assert!(!is_missing(Some(1), "User interaction is not allowed."));
        assert!(!is_missing(None, ""));
    }

    /// A read decodes the tool's output, and treats a blank value as absence.
    #[test]
    fn a_read_decodes_output_or_reports_absence() {
        let decoded = decode_read("openai", b"sk-from-keychain\n");
        assert_eq!(
            decoded
                .ok()
                .flatten()
                .map(|secret| secret.expose().to_owned()),
            Some(String::from("sk-from-keychain"))
        );
        assert_eq!(decode_read("openai", b"\n").ok(), Some(None));
    }

    /// The backend against a real keychain: a throwaway file is created for the
    /// test and removed afterwards, so the user's login keychain is never touched.
    #[test]
    fn the_keychain_backend_round_trips_a_secret() {
        let home = tempfile::tempdir().expect("temp dir");
        let keychain = home.path().join("nanus-test.keychain");
        let name = keychain.display().to_string();
        let created = Command::new(DEFAULT_PROGRAM)
            .args(["create-keychain", "-p", "test-password", &name])
            .output();
        assert!(
            created.is_ok_and(|output| output.status.success()),
            "a throwaway keychain is created"
        );
        let _guard = KeychainGuard(keychain.clone());

        let backend = KeychainBackend::with_keychain(&keychain);
        // Absence first: the keychain exists and does not hold the item.
        let missing = futures::executor::block_on(backend.get("nanus", "openai-test"));
        assert_eq!(missing.ok(), Some(None), "nothing stored is not an error");

        assert!(futures::executor::block_on(backend.set("nanus", "openai-test", "sk-1")).is_ok());
        let first = futures::executor::block_on(backend.get("nanus", "openai-test"));
        assert_eq!(
            first
                .ok()
                .flatten()
                .map(|secret| secret.expose().to_owned()),
            Some(String::from("sk-1"))
        );
        // A second write replaces rather than failing on the existing item.
        assert!(futures::executor::block_on(backend.set("nanus", "openai-test", "sk-2")).is_ok());
        let second = futures::executor::block_on(backend.get("nanus", "openai-test"));
        assert_eq!(
            second
                .ok()
                .flatten()
                .map(|secret| secret.expose().to_owned()),
            Some(String::from("sk-2"))
        );

        assert_eq!(
            futures::executor::block_on(backend.clear("nanus", "openai-test")).ok(),
            Some(true)
        );
        // The pair: a second clear removed nothing and is still a success.
        assert_eq!(
            futures::executor::block_on(backend.clear("nanus", "openai-test")).ok(),
            Some(false)
        );
    }
}
