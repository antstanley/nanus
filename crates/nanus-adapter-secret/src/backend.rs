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
///
/// The service name keeps credentials apart here as it does in the platform store.
/// [`crate::DEFAULT_SERVICE`] files under `<home>/secrets`, where every install before
/// service names existed already has its files; any other name files under
/// `<home>/service-secrets/<service>`, a directory no default account can collide with,
/// so two agents that share a home but not a service name never read each other's keys.
#[derive(Clone, Debug)]
pub struct FileBackend {
    root: PathBuf,
    others: PathBuf,
}

/// The directory, under a home, that holds a directory per non-default service name.
const SERVICE_SECRETS: &str = "service-secrets";

impl FileBackend {
    /// Creates a backend that files secrets under `<home>/secrets`, or under
    /// `<home>/service-secrets/<service>` for a service name other than the default.
    #[must_use]
    pub fn new(home: &Path) -> Self {
        Self {
            root: home.join("secrets"),
            others: home.join(SERVICE_SECRETS),
        }
    }

    /// Returns the directory the default service's secrets are filed under.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Returns the directory secrets filed under `service` are kept in.
    ///
    /// An unusable service name never becomes a directory outside the home: like an
    /// unusable account in [`FileBackend::path`], it is refused before a store is asked
    /// for it, and this fallback is the second line of defence.
    #[must_use]
    pub fn directory(&self, service: &str) -> PathBuf {
        if service == crate::DEFAULT_SERVICE {
            return self.root.clone();
        }
        if crate::valid_service(service) {
            return self.others.join(service);
        }
        self.others.join("invalid")
    }

    /// Returns the file an account filed under `service` is stored in.
    #[must_use]
    pub fn path(&self, service: &str, account: &str) -> PathBuf {
        // An unusable name never becomes a path outside the secrets directory; the
        // name is validated before a store is asked for it, so this fallback is a
        // second line of defence rather than the first.
        let directory = self.directory(service);
        if valid_account(account) {
            return directory.join(account);
        }
        directory.join("invalid")
    }

    /// Refuses a service name the file store cannot make a directory of.
    fn usable(service: &str) -> Result<(), SecretError> {
        if crate::valid_service(service) {
            return Ok(());
        }
        Err(SecretError::invalid(format!(
            "{service:?} is not a usable secret service name"
        )))
    }

    /// Creates the directory `service` files under, with owner-only permissions.
    fn ensure_directory(&self, service: &str) -> Result<PathBuf, SecretError> {
        let directory = self.directory(service);
        std::fs::create_dir_all(&directory).map_err(|error| {
            SecretError::refused(
                "file",
                "write",
                "<store>",
                format!("{}: {error}", directory.display()),
            )
        })?;
        restrict(&directory, 0o700)
            .map_err(|error| SecretError::refused("file", "write", "<store>", error.to_string()))?;
        Ok(directory)
    }
}

impl SecretBackend for FileBackend {
    fn name(&self) -> &'static str {
        "file"
    }

    fn get<'a>(
        &'a self,
        service: &'a str,
        account: &'a str,
    ) -> LocalBoxFuture<'a, SecretResult<Option<Secret>>> {
        Box::pin(async move {
            Self::usable(service)?;
            let path = self.path(service, account);
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
        service: &'a str,
        account: &'a str,
        secret: &'a str,
    ) -> LocalBoxFuture<'a, SecretResult<()>> {
        Box::pin(async move {
            Self::usable(service)?;
            let directory = self.ensure_directory(service)?;
            let path = self.path(service, account);
            assert!(
                path.starts_with(&directory),
                "a secret is filed in its service's directory"
            );
            write_private(&path, secret).map_err(|error| {
                SecretError::refused(self.name(), "write", account, error.to_string())
            })?;
            tracing::debug!(store = "file", "stored a secret");
            Ok(())
        })
    }

    fn clear<'a>(
        &'a self,
        service: &'a str,
        account: &'a str,
    ) -> LocalBoxFuture<'a, SecretResult<bool>> {
        Box::pin(async move {
            Self::usable(service)?;
            let path = self.path(service, account);
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
/// On Windows, access follows the containing directory's ACL. This backend does
/// not translate Unix mode bits into an explicit Windows descriptor.
#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)] // Keep the fallible Unix contract at shared call sites.
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
        assert!(
            backend
                .path("nanus", "../elsewhere")
                .starts_with(backend.root())
        );
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

    /// Two service names sharing a home share no file: a key filed under one is not
    /// read, replaced, or cleared under the other, and the shipped name keeps the
    /// layout every install before service names already has.
    #[test]
    fn a_file_secret_filed_under_one_service_is_not_read_under_another() {
        let home = tempfile::tempdir().expect("temp dir");
        let backend = FileBackend::new(home.path());
        let host = "nanus-test-example-host";
        let stored = futures::executor::block_on(backend.set(host, "openai", "sk-host"));
        assert!(stored.is_ok(), "{stored:?}");
        assert_eq!(
            backend.path(host, "openai"),
            home.path()
                .join("service-secrets")
                .join(host)
                .join("openai")
        );
        assert_eq!(
            futures::executor::block_on(backend.get("nanus", "openai")).ok(),
            Some(None),
            "the shipped name does not see the host's key"
        );
        assert_eq!(
            futures::executor::block_on(backend.clear("nanus", "openai")).ok(),
            Some(false),
            "nor clear it"
        );

        assert!(futures::executor::block_on(backend.set("nanus", "openai", "sk-own")).is_ok());
        assert_eq!(
            backend.path("nanus", "openai"),
            home.path().join("secrets").join("openai")
        );
        let read = |service| {
            futures::executor::block_on(backend.get(service, "openai"))
                .ok()
                .flatten()
                .map(|secret| secret.expose().to_owned())
        };
        assert_eq!(read(host), Some(String::from("sk-host")));
        assert_eq!(read("nanus"), Some(String::from("sk-own")));
    }

    /// A service name the file store cannot make a directory of is refused by every
    /// operation, and even the path it would have had stays inside the home.
    #[test]
    fn an_unusable_service_name_is_refused_by_the_file_store() {
        let home = tempfile::tempdir().expect("temp dir");
        let backend = FileBackend::new(home.path());
        for service in ["", "..", "../elsewhere", "a/b", "Host", "-w"] {
            assert!(backend.path(service, "openai").starts_with(home.path()));
            let written = futures::executor::block_on(backend.set(service, "openai", "sk-x"));
            assert!(
                matches!(written, Err(SecretError::Invalid { .. })),
                "{service:?}: {written:?}"
            );
            assert!(futures::executor::block_on(backend.get(service, "openai")).is_err());
            assert!(futures::executor::block_on(backend.clear(service, "openai")).is_err());
        }
        assert!(
            std::fs::read_dir(home.path()).is_ok_and(|mut entries| entries.next().is_none()),
            "nothing was created for a refused name"
        );
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
        let mode = std::fs::metadata(backend.path("nanus", "openai"))
            .map(|meta| meta.permissions().mode());
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
        let host = "nanus-test-example-host";
        assert!(futures::executor::block_on(backend.set(host, "openai", "sk-perm")).is_ok());
        let dir = std::fs::metadata(backend.directory(host)).map(|meta| meta.permissions().mode());
        assert_eq!(
            dir.ok().map(|mode| mode & 0o777),
            Some(0o700),
            "so is another service name's"
        );
    }

    /// A blank value is not a credential, so a file containing whitespace reads as
    /// absence rather than as an empty key.
    #[test]
    fn a_blank_file_reads_as_absence() {
        let home = tempfile::tempdir().expect("temp dir");
        let backend = FileBackend::new(home.path());
        assert!(futures::executor::block_on(backend.set("nanus", "openai", "sk-x")).is_ok());
        assert!(std::fs::write(backend.path("nanus", "openai"), "\n").is_ok());
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
        assert!(std::fs::write(backend.path("nanus", "openai"), [0xff, 0xfe]).is_ok());
        assert!(matches!(
            futures::executor::block_on(backend.get("nanus", "openai")),
            Err(SecretError::NotText { .. })
        ));
    }
}
