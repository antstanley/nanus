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
/// The file is written atomically on every platform: a sibling temporary file, a sync,
/// and a rename. A half-written secret would be a credential that authenticates as
/// nothing, which is worse than a write that visibly failed.
///
/// The mode is checked on every read as well as set on every write: a secret found
/// readable by anyone but its owner is narrowed before it is used. On Windows, where
/// there is no mode and a `:` in a file name addresses a stream of another file, an
/// account is filed under a percent-encoded name instead — `zai%3Acoding` — so a plan's
/// secret is a file of its own.
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
            return directory.join(file_name(account).as_ref());
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
            narrow_if_open(&path);
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

/// Whether an account is spelled differently on disk than in the store's vocabulary.
///
/// Only on Windows, where a `:` in a file name is not a character but a separator: it names an
/// alternate data stream of the file before it. Filed as written, `openai:subscription` would be
/// a stream of the file `openai` — invisible in a listing, dropped by a copy to another file
/// system, and deleted along with the API key's file by `nanus auth clear openai`. Elsewhere the
/// name is kept as it is, so every secret already filed is still found.
const ENCODES_FILE_NAMES: bool = cfg!(windows);

/// Returns the file name `account` is stored under.
fn file_name(account: &str) -> std::borrow::Cow<'_, str> {
    if ENCODES_FILE_NAMES {
        return std::borrow::Cow::Owned(portable_file_name(account));
    }
    std::borrow::Cow::Borrowed(account)
}

/// Spells `account` with nothing a Windows file name gives a meaning to.
///
/// Every character outside letters, digits, `.`, `-` and `_` becomes `%` and its bytes in
/// hexadecimal — the `%` itself included, which is what keeps two accounts from ever sharing a
/// name: `zai:coding` is `zai%3Acoding`, and an account spelled `zai%3Acoding` is
/// `zai%253Acoding`. A name ending in a dot, which Windows would silently drop, has that dot
/// encoded too.
fn portable_file_name(account: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let last = account.len().saturating_sub(1);
    let mut name = String::with_capacity(account.len());
    for (index, byte) in account.bytes().enumerate() {
        let plain = byte.is_ascii_alphanumeric()
            || matches!(byte, b'-' | b'_')
            || (byte == b'.' && index != last);
        if plain {
            name.push(char::from(byte));
        } else {
            name.push('%');
            name.push(char::from(HEX[usize::from(byte >> 4)]));
            name.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
    name
}

/// Narrows a secret that is readable by more than its owner, before it is read.
///
/// This store's own writes are owner-only from the moment the file exists, so a wider mode
/// means the file was put there by hand — `echo key > secrets/openai` under an ordinary umask
/// — or restored by something that did not keep modes. The credential is the user's either
/// way, so it is still read; but it is narrowed first and the fact is logged, rather than
/// left open for as long as nobody rewrites it.
#[cfg(unix)]
fn narrow_if_open(path: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    let Ok(metadata) = std::fs::metadata(path) else {
        // Absent or unreadable: the read that follows reports which.
        return;
    };
    // The group's and everyone else's bits, which this store never sets.
    let open_to_others = metadata.permissions().mode() & 0o077 != 0;
    if !open_to_others {
        return;
    }
    match restrict(path, 0o600) {
        Ok(()) => tracing::warn!(
            path = %path.display(),
            "a stored secret was readable by other users; narrowed it to its owner"
        ),
        Err(error) => tracing::warn!(
            path = %path.display(),
            %error,
            "a stored secret is readable by other users and could not be narrowed"
        ),
    }
}

/// Leaves a secret's access alone where the platform has no mode bits to check.
#[cfg(not(unix))]
const fn narrow_if_open(_path: &Path) {}

/// Opens `path` for a fresh write that only its owner can read.
#[cfg(unix)]
fn create_private(path: &Path) -> Result<std::fs::File, std::io::Error> {
    use std::os::unix::fs::OpenOptionsExt as _;
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)
}

/// Opens `path` for a fresh write where access follows the directory rather than a mode.
#[cfg(not(unix))]
fn create_private(path: &Path) -> Result<std::fs::File, std::io::Error> {
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)
}

/// Returns the sibling a secret is written to before it takes `path`'s place.
///
/// Named for the process as well as the secret, so two processes storing the same account —
/// both renewing one authorization — never write through each other's half-finished file, and
/// begun with a dot so it is not mistaken for an account in a listing.
fn staging_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!(".{name}.{}.tmp", std::process::id()))
}

/// Writes `body` and a final newline to the staging file `temp`, and syncs it.
fn stage(temp: &Path, body: &str) -> Result<(), std::io::Error> {
    use std::io::Write as _;
    let mut file = create_private(temp)?;
    file.write_all(body.as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_all()
}

/// Writes `body` to `path` with owner-only access, atomically, on every platform.
///
/// A sibling file, a sync, and a rename: a reader sees the old secret or the new one, never
/// half of either.
fn write_private(path: &Path, body: &str) -> Result<(), std::io::Error> {
    let temp = staging_path(path);
    let written = stage(&temp, body).and_then(|()| std::fs::rename(&temp, path));
    if let Err(error) = written {
        // Whatever part of the secret reached the sibling does not stay behind.
        if std::fs::remove_file(&temp).is_ok() {
            tracing::debug!("discarded a temporary secret file");
        }
        return Err(error);
    }
    restrict(path, 0o600)
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

    /// A secret put there by hand under an ordinary umask is readable by everyone on the
    /// machine. It is still the user's credential, so it is read — and narrowed first, rather
    /// than left open until something rewrites it.
    #[cfg(unix)]
    #[test]
    fn a_secret_left_readable_by_others_is_narrowed_when_it_is_read() {
        use std::os::unix::fs::PermissionsExt as _;
        let home = tempfile::tempdir().expect("temp dir");
        let backend = FileBackend::new(home.path());
        let path = backend.path("nanus", "openai");
        std::fs::create_dir_all(backend.root()).expect("the directory");
        std::fs::write(&path, "sk-by-hand\n").expect("the file");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");

        let read = futures::executor::block_on(backend.get("nanus", "openai"));
        assert_eq!(
            read.ok().flatten().map(|secret| secret.expose().to_owned()),
            Some(String::from("sk-by-hand"))
        );
        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "narrowed to its owner");

        // The pair: a secret that was already owner-only is read and left exactly as it was.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).expect("chmod");
        assert!(futures::executor::block_on(backend.get("nanus", "openai")).is_ok());
        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o400, "an owner-only mode is not rewritten");
    }

    /// The spelling a Windows file name needs: a `:` there names a stream of another file, so
    /// a plan's account must not keep one. Every account still has a name of its own.
    #[test]
    fn a_portable_file_name_has_no_separator_and_no_two_accounts_share_one() {
        assert_eq!(portable_file_name("openai"), "openai");
        assert_eq!(portable_file_name("z.ai"), "z.ai");
        assert_eq!(portable_file_name("zai:coding"), "zai%3Acoding");
        assert_eq!(
            portable_file_name("openai:subscription"),
            "openai%3Asubscription"
        );
        // The escape is escaped, so an account that is spelled like an encoded one is not it.
        assert_eq!(portable_file_name("zai%3Acoding"), "zai%253Acoding");
        // A trailing dot is one Windows drops, which would file `a.` over `a`.
        assert_eq!(portable_file_name("a."), "a%2E");
        for reserved in ['<', '>', ':', '"', '|', '?', '*', ' '] {
            let name = portable_file_name(&format!("a{reserved}b"));
            assert!(!name.contains(reserved), "{reserved:?} survives in {name}");
        }
    }

    /// The other direction: where a `:` is only a character the name is kept as written, so a
    /// secret every existing install has already filed is still found under it.
    #[cfg(not(windows))]
    #[test]
    fn an_account_keeps_its_own_name_where_a_colon_is_only_a_character() {
        let backend = FileBackend::new(Path::new("/tmp/nanus-test-home"));
        assert_eq!(
            backend.path("nanus", "openai:subscription"),
            backend.root().join("openai:subscription")
        );
    }

    /// A plan's account and its provider's are two secrets: storing and clearing one leaves the
    /// other alone, which on Windows is exactly what a stream of the provider's file would not.
    #[test]
    fn clearing_a_providers_secret_leaves_its_plans_secret_alone() {
        let home = tempfile::tempdir().expect("temp dir");
        let backend = FileBackend::new(home.path());
        let set =
            |account, value| futures::executor::block_on(backend.set("nanus", account, value));
        assert!(set("openai", "sk-api").is_ok());
        assert!(set("openai:subscription", "the-grant").is_ok());
        assert_ne!(
            backend.path("nanus", "openai"),
            backend.path("nanus", "openai:subscription")
        );

        let cleared = futures::executor::block_on(backend.clear("nanus", "openai"));
        assert_eq!(cleared.ok(), Some(true));
        let kept = futures::executor::block_on(backend.get("nanus", "openai:subscription"));
        assert_eq!(
            kept.ok().flatten().map(|secret| secret.expose().to_owned()),
            Some(String::from("the-grant"))
        );
    }

    /// A write leaves the secret and nothing else: the sibling it was staged in is gone, so a
    /// listing of the directory is a listing of accounts.
    #[test]
    fn a_write_leaves_no_staging_file_behind() {
        let home = tempfile::tempdir().expect("temp dir");
        let backend = FileBackend::new(home.path());
        assert!(futures::executor::block_on(backend.set("nanus", "z.ai", "sk-1")).is_ok());
        assert!(futures::executor::block_on(backend.set("nanus", "z.ai", "sk-2")).is_ok());
        let names: Vec<String> = std::fs::read_dir(backend.root())
            .expect("the directory")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec![String::from("z.ai")]);
        let read = futures::executor::block_on(backend.get("nanus", "z.ai"));
        assert_eq!(
            read.ok().flatten().map(|secret| secret.expose().to_owned()),
            Some(String::from("sk-2")),
            "the second write replaced the first"
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
