//! The secret port: where a provider credential is kept.
//!
//! A secret is the one value in the harness that must never reach a log, a
//! transcript, or a `Debug` rendering, so the port hands it over in a wrapper
//! that redacts itself ([`Secret`]) rather than as a `String`. The guarantee is
//! carried by the type rather than by a convention: `Debug` is implemented by
//! hand to print nothing, and `Display` and `Serialize` are deliberately absent
//! from that wrapper, so an adapter that wanted to print a key would have to call
//! [`Secret::expose`] by name, which is the one thing a reviewer greps for.
//!
//! ## Accounts, not providers
//!
//! The port is keyed by an *account* — the name a value is filed under — rather
//! than by a provider, because a store that can hold an API key can hold a
//! bearer token for anything. The convention the harness follows is that a
//! provider's account is its name (`deepseek`, `openai`, `anthropic`, `zai`),
//! and the environment fallback derives its variable from the same word
//! (`OPENAI_API_KEY`), so one spelling names a credential everywhere.
//!
//! ## Why a failure is not the same as an absence
//!
//! `Ok(None)` and `Err(_)` are different answers and the caller must be able to
//! tell them apart. A keychain that reports "no such item" has told the truth —
//! nothing is stored — while a keychain that is locked, or a platform store that
//! is not there at all, has failed to answer. Collapsing the two would make a
//! locked keychain look like an unset credential, and the fix for those two
//! problems is not the same sentence.

use core::fmt;
use std::rc::Rc;

use crate::LocalBoxFuture;

/// A shared, key-addressable secret store.
pub type SecretHandle = Rc<Box<dyn SecretPort>>;

/// The result type for secret operations.
pub type SecretResult<T> = Result<T, SecretError>;

/// The secret-store port.
pub trait SecretPort {
    /// Returns the secret filed under `account`, or `None` when none is stored.
    fn get<'a>(&'a self, account: &'a str) -> LocalBoxFuture<'a, SecretResult<Option<Secret>>>;

    /// Files `secret` under `account`, replacing any value already there.
    fn set<'a>(&'a self, account: &'a str, secret: &'a str)
    -> LocalBoxFuture<'a, SecretResult<()>>;

    /// Removes the secret filed under `account`.
    ///
    /// Returns `true` when something was removed and `false` when there was
    /// nothing there: removing an absent secret is a success, not a failure, so a
    /// caller can clear an account without first asking whether it is set.
    fn clear<'a>(&'a self, account: &'a str) -> LocalBoxFuture<'a, SecretResult<bool>>;

    /// Returns a short name for where secrets are kept.
    ///
    /// Reported by `nanus config`, so a reader can tell which store answered — and
    /// so a fallback that is in use (a file rather than a keychain) is visible
    /// rather than silent.
    fn backend(&self) -> &str;
}

/// A stored secret.
///
/// The inner string is private and the only way out is [`Secret::expose`], so a
/// key cannot reach a log line by accident: `Debug` prints nothing useful, and
/// there is no `Display` or `Serialize` to render it with. `Clone` exists
/// because a caller that needs the value twice should not have to ask the store
/// twice.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// Wraps a secret value.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Returns the value, by explicit request.
    ///
    /// Named `expose` rather than `as_str` or `value` so that every site that
    /// handles a raw credential is greppable — a reviewer can list the places a
    /// secret is unwrapped, and a new one stands out.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Returns `true` when the stored value is empty or only whitespace.
    ///
    /// An empty value is treated as an absent one by the harness, because a
    /// provider rejects it identically and a configured-but-blank key is a
    /// mistake worth reporting as "not set" rather than as an authentication
    /// failure later.
    #[must_use]
    pub fn is_blank(&self) -> bool {
        self.0.trim().is_empty()
    }
}

impl fmt::Debug for Secret {
    /// Renders the shape without the value.
    ///
    /// Implemented rather than derived so the `Debug` of a struct that *holds* a
    /// secret stays safe too, which is the rendering that actually reaches a log.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// Why a secret-store operation failed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SecretError {
    /// The backing store could not answer at all.
    ///
    /// A locked keychain, a missing `security` binary, or a platform with no
    /// store: the store exists as a concept but this process cannot use it. A
    /// caller should move to the next store in its chain, or tell the user which
    /// fallback to configure.
    #[error("the {backend} store is unavailable: {message}")]
    Unavailable {
        /// The store that could not answer.
        backend: String,
        /// The platform failure, rendered.
        message: String,
    },

    /// The store answered by refusing.
    #[error("the {backend} store refused to {operation} {account}: {message}")]
    Refused {
        /// The store that refused.
        backend: String,
        /// What was being attempted: `read`, `write`, or `delete`.
        operation: &'static str,
        /// The account involved.
        account: String,
        /// The store's own message.
        message: String,
    },

    /// The account or value is not usable.
    #[error("invalid secret request: {reason}")]
    Invalid {
        /// Why the request was refused.
        reason: String,
    },

    /// The value read back is not valid UTF-8.
    ///
    /// Only the file fallback can produce this, and it means the file was written
    /// by something that was not this harness.
    #[error("the secret stored for {account} is not valid text")]
    NotText {
        /// The account whose value could not be read.
        account: String,
    },
}

impl SecretError {
    /// Builds an [`SecretError::Unavailable`].
    #[must_use]
    pub fn unavailable(backend: &str, message: impl Into<String>) -> Self {
        Self::Unavailable {
            backend: backend.to_owned(),
            message: message.into(),
        }
    }

    /// Builds a [`SecretError::Refused`].
    #[must_use]
    pub fn refused(
        backend: &str,
        operation: &'static str,
        account: &str,
        message: impl Into<String>,
    ) -> Self {
        Self::Refused {
            backend: backend.to_owned(),
            operation,
            account: account.to_owned(),
            message: message.into(),
        }
    }

    /// Builds a [`SecretError::Invalid`].
    #[must_use]
    pub fn invalid(reason: impl Into<String>) -> Self {
        Self::Invalid {
            reason: reason.into(),
        }
    }

    /// Returns `true` when the failure means "try another store".
    ///
    /// The distinction a chain of stores needs: an unavailable store is skipped,
    /// while a refusal is the store's answer and is reported rather than papered
    /// over by a fallback that might hold a stale value.
    #[must_use]
    pub const fn is_unavailable(&self) -> bool {
        matches!(self, Self::Unavailable { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wrapper's whole purpose: a secret cannot be rendered by accident.
    #[test]
    fn a_secret_redacts_itself_in_debug() {
        let secret = Secret::new("sk-not-a-real-key");
        let rendered = format!("{secret:?}");
        assert!(!rendered.contains("sk-not-a-real-key"), "{rendered}");
        assert!(rendered.contains("redacted"));
        // And the value is still reachable on purpose.
        assert_eq!(secret.expose(), "sk-not-a-real-key");
    }

    /// A blank value is reported as blank rather than as a usable credential, so a
    /// whitespace-only key is refused before a request is made with it.
    #[test]
    fn a_blank_secret_is_not_a_credential() {
        assert!(Secret::new("").is_blank());
        assert!(Secret::new("   \n").is_blank());
        assert!(!Secret::new("sk-x").is_blank());
    }

    /// A struct holding a secret keeps the redaction, because that rendering is the
    /// one that reaches a log line.
    #[test]
    fn a_secret_nested_in_a_struct_stays_redacted() {
        #[derive(Debug)]
        struct Holder {
            #[allow(dead_code)]
            key: Secret,
        }
        let rendered = format!(
            "{:?}",
            Holder {
                key: Secret::new("sk-secret")
            }
        );
        assert!(!rendered.contains("sk-secret"), "{rendered}");
    }

    #[test]
    fn only_an_unavailable_store_invites_a_fallback() {
        assert!(SecretError::unavailable("keychain", "locked").is_unavailable());
        assert!(!SecretError::refused("keychain", "read", "openai", "denied").is_unavailable());
        assert!(!SecretError::invalid("no account").is_unavailable());
        assert!(
            !SecretError::NotText {
                account: String::from("openai")
            }
            .is_unavailable()
        );
    }

    #[test]
    fn a_failure_names_the_store_and_the_account() {
        let refused = SecretError::refused("keychain", "write", "openai", "user cancelled");
        let rendered = refused.to_string();
        assert!(rendered.contains("keychain"));
        assert!(rendered.contains("openai"));
        assert!(rendered.contains("write"));
        // The error carries no secret value: there is nothing here to leak.
        assert!(!rendered.contains("sk-"));
    }
}
