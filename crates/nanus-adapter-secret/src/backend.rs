//! The stores a credential can live in, and the trait that makes them pluggable.
//!
//! A [`SecretBackend`] is one place a secret can be kept. [`crate::Secrets`] holds
//! an ordered list of them and tries each in turn, which is what makes the
//! platform story separable from the harness: adding a store — a different
//! keychain, a hardware token, a network vault — is a new implementation of this
//! trait and one line in the chain, and nothing above it changes.
//!
//! ## Why the chain has an order
//!
//! The order is a precedence rule a user can predict: the platform's own store
//! first, then the private file, then the environment. A read takes the first
//! value any store can produce and a write stops at the first store that will take
//! one, so a store that cannot help — an unavailable keychain, an environment that
//! cannot be written — is stepped over rather than becoming the error. The only
//! choice a failing chain makes is which failure to report when nothing produced a
//! value; [`crate::Secrets`] makes it, not a backend, because it is the thing that
//! knows what the others answered.

use std::path::{Path, PathBuf};

use nanus_ports::{LocalBoxFuture, Secret, SecretError, SecretResult};

/// The suffix an environment variable carrying a credential ends with.
pub const ENV_SUFFIX: &str = "_API_KEY";

/// One place a secret can be kept.
///
/// The methods borrow their arguments for as long as the operation runs, exactly
/// as the port does, so an implementation may hold the account name without
/// cloning it.
pub trait SecretBackend {
    /// Returns a short name for this store, used in messages and in the chain's
    /// label.
    fn name(&self) -> &'static str;

    /// Reads the secret filed under `account`.
    fn get<'a>(
        &'a self,
        service: &'a str,
        account: &'a str,
    ) -> LocalBoxFuture<'a, SecretResult<Option<Secret>>>;

    /// Writes `secret` under `account`.
    fn set<'a>(
        &'a self,
        service: &'a str,
        account: &'a str,
        secret: &'a str,
    ) -> LocalBoxFuture<'a, SecretResult<()>>;

    /// Removes the secret filed under `account`, reporting whether one was there.
    fn clear<'a>(
        &'a self,
        service: &'a str,
        account: &'a str,
    ) -> LocalBoxFuture<'a, SecretResult<bool>>;
}

/// Returns `true` when `account` is usable as a store key.
///
/// An account is a *name*, not a path: it may not be empty, may not be a path
/// component, and may not contain a separator. The file backend turns it into a
/// file name, so this is the check that keeps a name from climbing out of the
/// secrets directory — the same rule the session store applies to a session name.
#[must_use]
pub fn valid_account(account: &str) -> bool {
    !account.trim().is_empty()
        && account != "."
        && account != ".."
        && !account.contains(['/', '\\', '\0'])
}

/// Reads a variable from the process environment.
fn read_env(variable: &str) -> Option<String> {
    std::env::var(variable).ok()
}

/// The environment, as a read-only store.
///
/// It is last in the chain because it is the fallback the harness has always had:
/// a container or a CI job sets a variable and nothing else exists. Writing is
/// refused rather than attempted, so `nanus auth set` never claims to have
/// changed a process's environment.
///
/// The reader is a function pointer rather than a call to `std::env::var`, because
/// setting a variable is `unsafe` in Rust 2024 and the workspace forbids
/// `unsafe` everywhere — including in tests. Injecting the source is how the
/// present-and-absent cases are both pinned without the unsafe write.
#[derive(Clone, Copy, Debug)]
pub struct EnvBackend {
    read: fn(&str) -> Option<String>,
}

impl Default for EnvBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl EnvBackend {
    /// Creates a backend over the process environment.
    #[must_use]
    pub const fn new() -> Self {
        Self { read: read_env }
    }

    /// Creates a backend over an arbitrary source, for tests and embedders.
    #[must_use]
    pub const fn with_reader(read: fn(&str) -> Option<String>) -> Self {
        Self { read }
    }

    /// Returns the environment variable an account is read from.
    ///
    /// The name is derived rather than looked up so that adding a provider needs no
    /// table: `openai` reads `OPENAI_API_KEY`, and a name outside that convention
    /// simply has no variable.
    #[must_use]
    pub fn variable(account: &str) -> String {
        let mut name = String::with_capacity(account.len().saturating_add(ENV_SUFFIX.len()));
        for character in account.chars() {
            if character.is_ascii_alphanumeric() {
                name.push(character.to_ascii_uppercase());
            } else {
                name.push('_');
            }
        }
        name.push_str(ENV_SUFFIX);
        name
    }
}

impl SecretBackend for EnvBackend {
    fn name(&self) -> &'static str {
        "environment"
    }

    fn get<'a>(
        &'a self,
        _service: &'a str,
        account: &'a str,
    ) -> LocalBoxFuture<'a, SecretResult<Option<Secret>>> {
        Box::pin(async move {
            let variable = Self::variable(account);
            match (self.read)(&variable) {
                // An unset variable and an empty one are the same answer: neither
                // is a credential, and reporting the difference would make an
                // empty export look like a configured key.
                Some(value) if !value.trim().is_empty() => Ok(Some(Secret::new(value))),
                Some(_) | None => Ok(None),
            }
        })
    }

    fn set<'a>(
        &'a self,
        _service: &'a str,
        account: &'a str,
        _secret: &'a str,
    ) -> LocalBoxFuture<'a, SecretResult<()>> {
        Box::pin(async move {
            Err(SecretError::unavailable(
                self.name(),
                format!(
                    "the environment cannot be written; export {} instead, or store the key in a writable store",
                    Self::variable(account)
                ),
            ))
        })
    }

    fn clear<'a>(
        &'a self,
        _service: &'a str,
        account: &'a str,
    ) -> LocalBoxFuture<'a, SecretResult<bool>> {
        Box::pin(async move {
            Err(SecretError::unavailable(
                self.name(),
                format!(
                    "the environment cannot be changed; unset {} in the shell that sets it",
                    Self::variable(account)
                ),
            ))
        })
    }
}

/// A `0600` file per account, under a `0700` directory.
///
/// The fallback for a machine where the platform store is not usable — a detached
/// service with no unlocked keychain, a container, a host whose keychain is not
/// on a bus. It is deliberately the *last* store that can be written, so it is
/// used only when the platform's own store has refused.
///
/// The file is written atomically: a sibling temporary file, a sync, and a
/// rename. A half-written secret would be a credential that authenticates as
/// nothing, which is worse than a write that visibly failed.
#[derive(Clone, Debug)]
pub struct FileBackend {
    root: PathBuf,
}

impl FileBackend {
    /// Creates a backend that files secrets under `<home>/secrets`.
    #[must_use]
    pub fn new(home: &Path) -> Self {
        Self {
            root: home.join("secrets"),
        }
    }

    /// Returns the directory secrets are filed under.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Returns the file an account is stored in.
    #[must_use]
    pub fn path(&self, account: &str) -> PathBuf {
        // An unusable name never becomes a path outside the secrets directory; the
        // name is validated before a store is asked for it, so this fallback is a
        // second line of defence rather than the first.
        if valid_account(account) {
            return self.root.join(account);
        }
        self.root.join("invalid")
    }

    /// Creates the secrets directory with owner-only permissions.
    fn ensure_root(&self) -> Result<(), SecretError> {
        std::fs::create_dir_all(&self.root).map_err(|error| {
            SecretError::refused(
                "file",
                "write",
                "<store>",
                format!("{}: {error}", self.root.display()),
            )
        })?;
        restrict(&self.root, 0o700)
            .map_err(|error| SecretError::refused("file", "write", "<store>", error.to_string()))
    }
}

impl SecretBackend for FileBackend {
    fn name(&self) -> &'static str {
        "file"
    }

    fn get<'a>(
        &'a self,
        _service: &'a str,
        account: &'a str,
    ) -> LocalBoxFuture<'a, SecretResult<Option<Secret>>> {
        Box::pin(async move {
            let path = self.path(account);
            let bytes = match std::fs::read(&path) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => {
                    return Err(SecretError::refused(
                        self.name(),
                        "read",
                        account,
                        format!("{}: {error}", path.display()),
                    ));
                }
            };
            let text = String::from_utf8(bytes).map_err(|_| SecretError::NotText {
                account: account.to_owned(),
            })?;
            let trimmed = text.trim_end_matches(['\r', '\n']).to_owned();
            if trimmed.trim().is_empty() {
                return Ok(None);
            }
            Ok(Some(Secret::new(trimmed)))
        })
    }

    fn set<'a>(
        &'a self,
        _service: &'a str,
        account: &'a str,
        secret: &'a str,
    ) -> LocalBoxFuture<'a, SecretResult<()>> {
        Box::pin(async move {
            self.ensure_root()?;
            let path = self.path(account);
            write_private(&path, secret).map_err(|error| {
                SecretError::refused(self.name(), "write", account, error.to_string())
            })?;
            tracing::debug!(store = "file", "stored a secret");
            Ok(())
        })
    }

    fn clear<'a>(
        &'a self,
        _service: &'a str,
        account: &'a str,
    ) -> LocalBoxFuture<'a, SecretResult<bool>> {
        Box::pin(async move {
            let path = self.path(account);
            match std::fs::remove_file(&path) {
                Ok(()) => Ok(true),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
                Err(error) => Err(SecretError::refused(
                    self.name(),
                    "delete",
                    account,
                    format!("{}: {error}", path.display()),
                )),
            }
        })
    }
}

/// Narrows a file or directory to owner-only access.
#[cfg(unix)]
fn restrict(path: &Path, mode: u32) -> Result<(), std::io::Error> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

/// Leaves permissions alone where the platform has no mode bits.
///
/// Windows support is not a goal (the link is a Unix socket), so this exists to
/// keep the crate building rather than to promise confinement.
#[cfg(not(unix))]
fn restrict(_path: &Path, _mode: u32) -> Result<(), std::io::Error> {
    Ok(())
}

/// Writes `body` to `path` with owner-only access, atomically.
#[cfg(unix)]
fn write_private(path: &Path, body: &str) -> Result<(), std::io::Error> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let temp = path.with_extension("tmp");
    {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&temp)?;
        file.write_all(body.as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_all()?;
    }
    if let Err(error) = std::fs::rename(&temp, path) {
        if std::fs::remove_file(&temp).is_ok() {
            tracing::debug!("discarded a temporary secret file");
        }
        return Err(error);
    }
    restrict(path, 0o600)
}

/// Writes `body` to `path` where the platform has no mode bits.
#[cfg(not(unix))]
fn write_private(path: &Path, body: &str) -> Result<(), std::io::Error> {
    std::fs::write(path, body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// A source that answers with one value, so both directions of a read are
    /// pinned without touching the process environment — writing to it is
    /// `unsafe`, which this workspace forbids even in a test.
    static PRESENT: AtomicBool = AtomicBool::new(false);

    fn source(variable: &str) -> Option<String> {
        if PRESENT.load(Ordering::Relaxed) && variable == "OPENAI_API_KEY" {
            return Some(String::from("sk-from-env"));
        }
        None
    }

    #[test]
    fn an_account_maps_to_an_environment_variable() {
        assert_eq!(EnvBackend::variable("openai"), "OPENAI_API_KEY");
        assert_eq!(EnvBackend::variable("anthropic"), "ANTHROPIC_API_KEY");
        assert_eq!(EnvBackend::variable("deepseek"), "DEEPSEEK_API_KEY");
        // A name with punctuation still yields a usable variable rather than an
        // empty one.
        assert_eq!(EnvBackend::variable("z.ai"), "Z_AI_API_KEY");
        assert_eq!(EnvBackend::variable(""), "_API_KEY");
    }

    /// An account is a name, not a path: the check that keeps a store key from
    /// becoming a traversal.
    #[test]
    fn an_account_may_not_climb_out_of_the_store() {
        assert!(valid_account("openai"));
        assert!(valid_account("z.ai"));
        assert!(!valid_account(""));
        assert!(!valid_account("   "));
        assert!(!valid_account("../login"));
        assert!(!valid_account("a/b"));
        assert!(!valid_account(".."));
        assert!(!valid_account("."));
        // The file path follows: an unusable name never becomes a path outside the
        // secrets directory.
        let backend = FileBackend::new(Path::new("/tmp/nanus-test-home"));
        assert!(backend.path("../elsewhere").starts_with(backend.root()));
    }

    /// The environment answers with the value when the variable is set, and with
    /// absence when it is not — a fallback that always answered would make the
    /// chain's order meaningless.
    #[test]
    fn the_environment_reads_only_what_is_set() {
        let backend = EnvBackend::with_reader(source);
        let absent = futures::executor::block_on(backend.get("nanus", "openai"));
        assert_eq!(absent.ok(), Some(None));

        PRESENT.store(true, Ordering::Relaxed);
        let found = futures::executor::block_on(backend.get("nanus", "openai"));
        PRESENT.store(false, Ordering::Relaxed);
        assert_eq!(
            found
                .ok()
                .flatten()
                .map(|secret| secret.expose().to_owned()),
            Some(String::from("sk-from-env"))
        );
    }

    /// The environment cannot be written, and says so instead of pretending.
    #[test]
    fn the_environment_refuses_a_write() {
        let backend = EnvBackend::new();
        let written = futures::executor::block_on(backend.set("nanus", "openai", "sk-x"));
        assert!(written.is_err());
        assert_eq!(
            written.err().map(|error| error.is_unavailable()),
            Some(true)
        );
        let cleared = futures::executor::block_on(backend.clear("nanus", "openai"));
        assert_eq!(
            cleared.err().map(|error| error.is_unavailable()),
            Some(true)
        );
    }

    /// The file backend round-trips a value, and reports absence as absence.
    #[test]
    fn a_file_secret_round_trips_and_clears() {
        let home = tempfile::tempdir().expect("temp dir");
        let backend = FileBackend::new(home.path());
        let missing = futures::executor::block_on(backend.get("nanus", "openai"));
        assert_eq!(missing.ok(), Some(None), "nothing stored is not an error");

        let stored = futures::executor::block_on(backend.set("nanus", "openai", "sk-file-secret"));
        assert!(stored.is_ok(), "a write succeeds: {stored:?}");
        let read = futures::executor::block_on(backend.get("nanus", "openai"));
        assert_eq!(
            read.ok().flatten().map(|secret| secret.expose().to_owned()),
            Some(String::from("sk-file-secret"))
        );

        let cleared = futures::executor::block_on(backend.clear("nanus", "openai"));
        assert_eq!(cleared.ok(), Some(true));
        // The pair: clearing an absent secret is a success that removed nothing.
        let again = futures::executor::block_on(backend.clear("nanus", "openai"));
        assert_eq!(again.ok(), Some(false));
    }

    /// The file a secret is written to is owner-only, which is the property that
    /// makes the fallback an acceptable place for a credential at all.
    #[cfg(unix)]
    #[test]
    fn a_stored_secret_is_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let home = tempfile::tempdir().expect("temp dir");
        let backend = FileBackend::new(home.path());
        assert!(futures::executor::block_on(backend.set("nanus", "openai", "sk-perm")).is_ok());
        let mode = std::fs::metadata(backend.path("openai")).map(|meta| meta.permissions().mode());
        assert_eq!(
            mode.ok().map(|mode| mode & 0o777),
            Some(0o600),
            "the secret file is readable only by its owner"
        );
        let dir = std::fs::metadata(backend.root()).map(|meta| meta.permissions().mode());
        assert_eq!(
            dir.ok().map(|mode| mode & 0o777),
            Some(0o700),
            "the directory holding secrets is owner-only"
        );
    }

    /// A blank value is not a credential, so a file containing whitespace reads as
    /// absence rather than as an empty key.
    #[test]
    fn a_blank_file_reads_as_absence() {
        let home = tempfile::tempdir().expect("temp dir");
        let backend = FileBackend::new(home.path());
        assert!(futures::executor::block_on(backend.set("nanus", "openai", "sk-x")).is_ok());
        assert!(std::fs::write(backend.path("openai"), "\n").is_ok());
        assert_eq!(
            futures::executor::block_on(backend.get("nanus", "openai")).ok(),
            Some(None)
        );
    }

    /// Text that is not valid UTF-8 is reported as such rather than being replaced
    /// with something plausible.
    #[test]
    fn a_file_that_is_not_text_is_reported() {
        let home = tempfile::tempdir().expect("temp dir");
        let backend = FileBackend::new(home.path());
        assert!(std::fs::create_dir_all(backend.root()).is_ok());
        assert!(std::fs::write(backend.path("openai"), [0xff, 0xfe]).is_ok());
        assert!(matches!(
            futures::executor::block_on(backend.get("nanus", "openai")),
            Err(SecretError::NotText { .. })
        ));
    }
}
