//! # nanus-adapter-secret
//!
//! Where a provider credential is kept: the platform keychain, a private file, and
//! the environment, tried in that order.
//!
//! ## What a store is, and why there is more than one
//!
//! The port ([`nanus_ports::SecretPort`]) says *what* a secret store must do. This
//! crate says *where* the value is, and it says so as an ordered chain of
//! [`SecretBackend`]s rather than as one place, because no single place is right
//! for every way nanus runs:
//!
//! | Store | Right for | Wrong for |
//! |---|---|---|
//! | [`KeychainBackend`] (macOS) | an interactive user; the value leaves the environment and survives a reboot | a detached service with no unlocked keychain, or no graphical session at all |
//! | [`FileBackend`] | that service or a container: a `0600` file under `<nanus home>/secrets`, mode checked | a machine where more than one user shares a home |
//! | [`EnvBackend`] | CI, where the secret is injected into the process | a person's shell, where it is in every subprocess's environment and in `ps` output |
//!
//! The default chain is keychain → file → environment. A **read** takes the first
//! value any store can produce, and a store that fails does not stop the walk: a
//! locked keychain must not hide a value that the environment does hold, which is
//! exactly the case a service runs in. A **write** goes to the first store that
//! will accept it, so `nanus auth set` prefers the keychain and falls back to the
//! file without the caller choosing. When nothing produced a value, the failure
//! reported is the most actionable one available — a store that *answered and
//! declined* rather than one that was not there.
//!
//! ## Adding another store
//!
//! Implement [`SecretBackend`] and put it in the chain with
//! [`Secrets::with_backends`]. Nothing else changes: the port, the configuration,
//! and the command line all speak account names, and a new store is a new way to
//! answer the same question. `Secrets::new` names the three shipped ones; a build
//! for a platform with a different store adds one there.
//!
//! ## The secret is not in the configuration, and not in a log
//!
//! The value never enters `NanusConfig`, is wrapped in [`nanus_ports::Secret`]
//! (which redacts its own `Debug`), and is read out only where a request is built
//! — see [`nanus_ports::Secret::expose`]. The file backend narrows its file and
//! directory to owner-only access, and the keychain is the platform's own store.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
// A `pub` item inside a private module is reachable only through this crate's own
// re-exports. The lint cannot tell that from an orphaned item, and every such item
// here is deliberate.
#![allow(unreachable_pub)]
#![allow(
    clippy::unwrap_or_default,
    clippy::manual_unwrap_or,
    clippy::manual_unwrap_or_default
)]

mod backend;
#[cfg(target_os = "macos")]
mod keychain;

use std::path::Path;
use std::rc::Rc;

pub use backend::{ENV_SUFFIX, EnvBackend, FileBackend, SecretBackend, valid_account};
#[cfg(target_os = "macos")]
pub use keychain::{DEFAULT_PROGRAM as KEYCHAIN_PROGRAM, KeychainBackend};

use nanus_ports::{LocalBoxFuture, Secret, SecretError, SecretHandle, SecretPort, SecretResult};

/// The service name a secret is filed under.
///
/// One name for every account, so `security find-generic-password -s nanus -a
/// openai` is the whole of what a user needs to inspect or remove an item by hand
/// — which is the point of using the platform's own store rather than one only
/// nanus understands.
pub const DEFAULT_SERVICE: &str = "nanus";

/// An ordered chain of secret stores.
pub struct Secrets {
    service: String,
    backends: Vec<Box<dyn SecretBackend>>,
    label: String,
}

impl core::fmt::Debug for Secrets {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // The stores hold no secrets themselves, but the type is `Debug` only to
        // be useful in a log line, and the values it would reach are not here.
        f.debug_struct("Secrets")
            .field("service", &self.service)
            .field("backends", &self.label)
            .finish()
    }
}

impl Secrets {
    /// Builds the shipped chain: the platform keychain, a private file, and the
    /// environment.
    #[must_use]
    pub fn new(home: &Path) -> Self {
        // The platform store is first when there is one, so the shipped order is
        // "the best store this platform has, then the private file, then the
        // environment".
        #[cfg(target_os = "macos")]
        let backends: Vec<Box<dyn SecretBackend>> = vec![
            Box::new(KeychainBackend::new()),
            Box::new(FileBackend::new(home)),
            Box::new(EnvBackend::new()),
        ];
        #[cfg(not(target_os = "macos"))]
        let backends: Vec<Box<dyn SecretBackend>> = vec![
            Box::new(FileBackend::new(home)),
            Box::new(EnvBackend::new()),
        ];
        Self::with_backends(DEFAULT_SERVICE, backends)
    }

    /// Builds a chain from an explicit set of stores, in precedence order.
    #[must_use]
    pub fn with_backends(
        service: impl Into<String>,
        backends: Vec<Box<dyn SecretBackend>>,
    ) -> Self {
        let label = if backends.is_empty() {
            String::from("none")
        } else {
            backends
                .iter()
                .map(|backend| backend.name())
                .collect::<Vec<&str>>()
                .join("+")
        };
        Self {
            service: service.into(),
            backends,
            label,
        }
    }

    /// Wraps the chain as the shared handle the harness passes around.
    #[must_use]
    pub fn handle(self) -> SecretHandle {
        Rc::new(Box::new(self))
    }

    /// Returns the service name secrets are filed under.
    #[must_use]
    pub fn service(&self) -> &str {
        &self.service
    }

    /// Chooses which failure to report when no store answered.
    ///
    /// A refusal is preferred to an unavailability: "the keychain refused to read
    /// openai" names something the user can act on, while "the keychain is not
    /// there" is a fact about the platform. The first failure of each kind is kept,
    /// so the message is the first store's rather than the last one's.
    fn prefer(current: &mut Option<SecretError>, candidate: SecretError) {
        let replace = current
            .as_ref()
            .is_none_or(|existing| existing.is_unavailable() && !candidate.is_unavailable());
        if replace {
            *current = Some(candidate);
        }
    }
}

impl SecretPort for Secrets {
    fn get<'a>(&'a self, account: &'a str) -> LocalBoxFuture<'a, SecretResult<Option<Secret>>> {
        Box::pin(async move {
            if !valid_account(account) {
                return Err(SecretError::invalid(format!(
                    "{account:?} is not a usable account name"
                )));
            }
            let mut failure: Option<SecretError> = None;
            for backend in &self.backends {
                match backend.get(&self.service, account).await {
                    Ok(Some(secret)) if !secret.is_blank() => {
                        tracing::debug!(store = backend.name(), "found a stored secret");
                        return Ok(Some(secret));
                    }
                    Ok(_) => {}
                    Err(error) => {
                        tracing::debug!(
                            store = backend.name(),
                            "a secret store could not answer: {error}"
                        );
                        Self::prefer(&mut failure, error);
                    }
                }
            }
            failure.map_or(Ok(None), Err)
        })
    }

    fn set<'a>(
        &'a self,
        account: &'a str,
        secret: &'a str,
    ) -> LocalBoxFuture<'a, SecretResult<()>> {
        Box::pin(async move {
            if !valid_account(account) {
                return Err(SecretError::invalid(format!(
                    "{account:?} is not a usable account name"
                )));
            }
            if secret.trim().is_empty() {
                return Err(SecretError::invalid(String::from(
                    "refusing to store an empty secret",
                )));
            }
            let mut failure: Option<SecretError> = None;
            for backend in &self.backends {
                match backend.set(&self.service, account, secret).await {
                    Ok(()) => {
                        tracing::debug!(store = backend.name(), "stored a secret");
                        return Ok(());
                    }
                    Err(error) => Self::prefer(&mut failure, error),
                }
            }
            Err(failure.unwrap_or_else(|| {
                SecretError::unavailable("none", "no writable secret store is configured")
            }))
        })
    }

    fn clear<'a>(&'a self, account: &'a str) -> LocalBoxFuture<'a, SecretResult<bool>> {
        Box::pin(async move {
            if !valid_account(account) {
                return Err(SecretError::invalid(format!(
                    "{account:?} is not a usable account name"
                )));
            }
            let mut removed = false;
            let mut failure: Option<SecretError> = None;
            for backend in &self.backends {
                match backend.clear(&self.service, account).await {
                    Ok(yes) => removed = removed || yes,
                    Err(error) => {
                        Self::prefer(&mut failure, error);
                    }
                }
            }
            if removed {
                return Ok(true);
            }
            // Nothing was removed. A store that *answered* and declined is a failure
            // worth reporting; a store that could not be written at all is not, because
            // it proves nothing about whether a value was there — which is exactly the
            // case where the only source is the environment, and the caller is better
            // served by "nothing was removable" than by an error about the environment.
            match failure {
                Some(error) if !error.is_unavailable() => Err(error),
                _ => Ok(false),
            }
        })
    }

    fn backend(&self) -> &str {
        &self.label
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A store whose every answer is scripted, so the chain's rules can be pinned
    /// without a platform store being involved.
    struct Fake {
        name: &'static str,
        value: RefCell<Option<String>>,
        failure: Option<SecretError>,
    }

    impl Fake {
        fn holding(name: &'static str, value: &str) -> Self {
            Self {
                name,
                value: RefCell::new(Some(value.to_owned())),
                failure: None,
            }
        }

        fn empty(name: &'static str) -> Self {
            Self {
                name,
                value: RefCell::new(None),
                failure: None,
            }
        }

        fn failing(name: &'static str, error: SecretError) -> Self {
            Self {
                name,
                value: RefCell::new(None),
                failure: Some(error),
            }
        }
    }

    impl SecretBackend for Fake {
        fn name(&self) -> &'static str {
            self.name
        }

        fn get<'a>(
            &'a self,
            _service: &'a str,
            _account: &'a str,
        ) -> LocalBoxFuture<'a, SecretResult<Option<Secret>>> {
            Box::pin(async move {
                if let Some(error) = &self.failure {
                    return Err(error.clone());
                }
                Ok(self.value.borrow().clone().map(Secret::new))
            })
        }

        fn set<'a>(
            &'a self,
            _service: &'a str,
            _account: &'a str,
            secret: &'a str,
        ) -> LocalBoxFuture<'a, SecretResult<()>> {
            Box::pin(async move {
                if let Some(error) = &self.failure {
                    return Err(error.clone());
                }
                *self.value.borrow_mut() = Some(secret.to_owned());
                Ok(())
            })
        }

        fn clear<'a>(
            &'a self,
            _service: &'a str,
            _account: &'a str,
        ) -> LocalBoxFuture<'a, SecretResult<bool>> {
            Box::pin(async move {
                // A scripted failure is a failure for every operation, which is what
                // lets the chain's rules about *which* failure to report be pinned.
                if let Some(error) = &self.failure {
                    return Err(error.clone());
                }
                let mut slot = self.value.borrow_mut();
                let had = slot.is_some();
                *slot = None;
                Ok(had)
            })
        }
    }

    fn chain(backends: Vec<Box<dyn SecretBackend>>) -> Secrets {
        Secrets::with_backends("nanus", backends)
    }

    fn value(found: SecretResult<Option<Secret>>) -> Option<String> {
        found
            .ok()
            .flatten()
            .map(|secret| secret.expose().to_owned())
    }

    /// The first store holding a value answers, so precedence is the chain's order.
    #[test]
    fn the_first_store_holding_a_value_wins() {
        let secrets = chain(vec![
            Box::new(Fake::holding("keychain", "sk-first")),
            Box::new(Fake::holding("file", "sk-second")),
        ]);
        assert_eq!(
            value(futures::executor::block_on(secrets.get("openai"))),
            Some(String::from("sk-first"))
        );
    }

    /// A store that cannot answer does not hide a value another store holds — the
    /// case a service with a locked keychain runs in.
    #[test]
    fn a_failing_store_does_not_hide_a_later_value() {
        let secrets = chain(vec![
            Box::new(Fake::failing(
                "keychain",
                SecretError::refused("keychain", "read", "openai", "locked"),
            )),
            Box::new(Fake::holding("environment", "sk-from-env")),
        ]);
        assert_eq!(
            value(futures::executor::block_on(secrets.get("openai"))),
            Some(String::from("sk-from-env"))
        );
    }

    /// When nothing holds a value, the reported failure is the actionable one: a
    /// store that answered and declined, not one that was not there.
    #[test]
    fn the_reported_failure_is_the_most_actionable_one() {
        let secrets = chain(vec![
            Box::new(Fake::failing(
                "keychain",
                SecretError::unavailable("keychain", "no such program"),
            )),
            Box::new(Fake::failing(
                "file",
                SecretError::refused("file", "read", "openai", "permission denied"),
            )),
        ]);
        let error = futures::executor::block_on(secrets.get("openai"));
        let message = error
            .err()
            .map(|error| error.to_string())
            .unwrap_or_default();
        assert!(message.contains("permission denied"), "{message}");
        assert!(!message.contains("no such program"), "{message}");
    }

    /// Nothing anywhere, and no failure either: absence is an answer.
    #[test]
    fn an_empty_chain_reports_absence_rather_than_failure() {
        let secrets = chain(vec![Box::new(Fake::empty("file"))]);
        assert_eq!(
            futures::executor::block_on(secrets.get("openai")).ok(),
            Some(None)
        );
    }

    /// A write goes to the first store that will take it, which is what lets the
    /// environment sit in the chain without `auth set` trying to write to it.
    #[test]
    fn a_write_takes_the_first_store_that_accepts_it() {
        let secrets = chain(vec![
            Box::new(Fake::failing(
                "keychain",
                SecretError::unavailable("keychain", "locked"),
            )),
            Box::new(Fake::empty("file")),
            Box::new(Fake::empty("environment")),
        ]);
        assert!(futures::executor::block_on(secrets.set("openai", "sk-new")).is_ok());
        // The keychain could not answer, so the next store in the chain took it.
        assert_eq!(
            value(futures::executor::block_on(secrets.get("openai"))),
            Some(String::from("sk-new"))
        );
    }

    /// Every store refusing is a failure, and an empty value is refused before any
    /// store sees it.
    #[test]
    fn a_write_reports_when_no_store_will_take_it() {
        let secrets = chain(vec![Box::new(Fake::failing(
            "keychain",
            SecretError::refused("keychain", "write", "openai", "user cancelled"),
        ))]);
        let written = futures::executor::block_on(secrets.set("openai", "sk-x"));
        assert!(written.is_err());

        let secrets = chain(vec![Box::new(Fake::empty("file"))]);
        let blank = futures::executor::block_on(secrets.set("openai", "   "));
        assert!(matches!(blank, Err(SecretError::Invalid { .. })));
    }

    /// Clearing when only an unwritable store could have held a value reports that
    /// nothing was removed rather than failing: the environment cannot be changed, but
    /// it also cannot hold what was asked about, so an error would name the wrong
    /// problem.
    #[test]
    fn a_clear_reports_nothing_removed_when_only_the_environment_could_hold_it() {
        let secrets = chain(vec![Box::new(Fake::failing(
            "environment",
            SecretError::unavailable("environment", "the environment cannot be changed"),
        ))]);
        assert_eq!(
            futures::executor::block_on(secrets.clear("openai")).ok(),
            Some(false)
        );
    }

    /// A store that answered and declined is still a failure, because it means the
    /// value may be there and could not be removed.
    #[test]
    fn a_clear_reports_a_store_that_declined() {
        let secrets = chain(vec![Box::new(Fake::failing(
            "keychain",
            SecretError::refused("keychain", "delete", "openai", "the item is locked"),
        ))]);
        let outcome = futures::executor::block_on(secrets.clear("openai"));
        assert!(outcome.is_err());
    }

    /// A clear asks every store, so a value duplicated across two of them is gone
    /// from both.
    #[test]
    fn a_clear_removes_the_value_from_every_store() {
        let secrets = chain(vec![
            Box::new(Fake::holding("keychain", "sk-first")),
            Box::new(Fake::holding("file", "sk-second")),
        ]);
        assert_eq!(
            futures::executor::block_on(secrets.clear("openai")).ok(),
            Some(true)
        );
        assert_eq!(
            futures::executor::block_on(secrets.get("openai")).ok(),
            Some(None),
            "nothing is left in any store"
        );
    }

    /// A name that is not usable is refused, and the chain is never consulted.
    #[test]
    fn an_unusable_account_is_refused() {
        let secrets = chain(vec![Box::new(Fake::holding("file", "sk-x"))]);
        for account in ["", "..", "a/b"] {
            let read = futures::executor::block_on(secrets.get(account));
            assert!(
                matches!(read, Err(SecretError::Invalid { .. })),
                "{account}"
            );
            let written = futures::executor::block_on(secrets.set(account, "sk-x"));
            assert!(
                matches!(written, Err(SecretError::Invalid { .. })),
                "{account}"
            );
        }
    }

    /// The label names the chain, because `nanus config` reports it and a reader
    /// needs to know whether a fallback is in use.
    #[test]
    fn the_label_names_every_store_in_order() {
        let secrets = Secrets::with_backends(
            "nanus",
            vec![
                Box::new(Fake::empty("keychain")),
                Box::new(Fake::empty("file")),
                Box::new(Fake::empty("environment")),
            ],
        );
        assert_eq!(secrets.backend(), "keychain+file+environment");
        assert_eq!(
            Secrets::with_backends("nanus", Vec::new()).backend(),
            "none"
        );
        assert_eq!(secrets.service(), "nanus");
    }

    /// The shipped chain is usable without a keychain, a file, or a variable: it is
    /// a chain, not a requirement that one of them works.
    #[test]
    fn the_shipped_chain_is_readable_with_nothing_stored() {
        let home = tempfile::tempdir().expect("temp dir");
        let secrets = Secrets::new(home.path());
        let found = futures::executor::block_on(secrets.get("nanus-test-absent-account"));
        assert_eq!(found.ok(), Some(None));
        assert!(secrets.backend().contains("file"));
        assert!(secrets.backend().contains("environment"));
    }
}
