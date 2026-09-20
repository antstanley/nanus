//! Authorizing a plan that is reached with OAuth rather than a typed key.
//!
//! A `ChatGPT` subscription is the case: the user authorizes in a browser, and what is filed is a
//! token set rather than a key. This module wraps the adapter's device flow and writes the result
//! into the credential store under the account the plan names, so the rest of the harness reads it
//! exactly as it reads a key — by account — and never has to know how it was obtained.
//!
//! The flow is split into [`begin`] and [`poll`] rather than run as one call, because the interface
//! has to show the user the page and the code *between* the two, and the wait is theirs to take.

use std::time::Duration;

use nanus_adapter_openai::oauth;
use nanus_ports::SecretHandle;

use crate::error::BundleError;
use crate::provider::{DEFAULT_PLAN, Provider};

/// How long an authorization is watched before it is given up on.
///
/// The service expires a device code well within this, so a longer wait would poll a code that can
/// no longer be accepted. The caller is free to stop sooner.
pub const AUTHORIZATION_TIMEOUT: Duration = Duration::from_mins(15);

/// How long to wait past the service's own interval before polling, so a poll that arrives a moment
/// early is not counted as a refusal.
pub const POLL_MARGIN: Duration = Duration::from_secs(3);

/// An authorization waiting on the user.
#[derive(Debug)]
pub struct PendingAuth {
    provider: Provider,
    plan: Option<String>,
    account: &'static str,
    inner: oauth::Pending,
}

impl PendingAuth {
    /// Returns the page the user visits.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.inner.url
    }

    /// Returns the code the user enters there.
    #[must_use]
    pub fn code(&self) -> &str {
        &self.inner.code
    }

    /// Returns how long to wait between polls.
    #[must_use]
    pub const fn interval(&self) -> Duration {
        self.inner.interval
    }

    /// Returns the provider being authorized.
    #[must_use]
    pub const fn provider(&self) -> Provider {
        self.provider
    }

    /// Returns the plan being authorized.
    #[must_use]
    pub fn plan(&self) -> Option<&str> {
        self.plan.as_deref()
    }

    /// Returns the account the token set is filed under.
    #[must_use]
    pub const fn account(&self) -> &'static str {
        self.account
    }
}

/// Starts an authorization for `provider`'s `plan`, against the provider's own service.
///
/// The issuer is not a parameter here, unlike in [`begin`]: a caller that has no reason to know
/// which service a provider authorizes against — the command line, a service — names the provider
/// and the plan and nothing else.
///
/// # Errors
///
/// Returns [`BundleError::Config`] when the plan is reached with a key rather than an
/// authorization, or when the authorization service cannot be reached.
pub async fn begin_for(provider: Provider, plan: Option<&str>) -> Result<PendingAuth, BundleError> {
    begin(provider, plan, oauth::ISSUER).await
}

/// Starts an authorization for `provider`'s `plan`, against `issuer`.
///
/// The issuer is a parameter rather than a constant so a test can point the flow at a local
/// service; production passes [`oauth::ISSUER`], which [`begin_for`] is the way to ask for.
///
/// # Errors
///
/// Returns [`BundleError::Config`] when the plan is reached with a key rather than an
/// authorization, or when the authorization service cannot be reached.
pub async fn begin(
    provider: Provider,
    plan: Option<&str>,
    issuer: &str,
) -> Result<PendingAuth, BundleError> {
    if !provider.credential_is_oauth(plan) {
        return Err(BundleError::config(format!(
            "{provider}'s {} plan is reached with a key, not an authorization",
            plan.unwrap_or(DEFAULT_PLAN)
        )));
    }
    let inner = oauth::begin(issuer)
        .await
        .map_err(|error| BundleError::config(error.to_string()))?;
    Ok(PendingAuth {
        provider,
        plan: plan.map(str::to_owned),
        account: provider.credential_account(plan),
        inner,
    })
}

/// Polls once, filing the token set and answering `true` when the user has authorized.
///
/// `false` means still waiting, which is not an error: the service answers "not yet" until the user
/// finishes, and the caller decides how long to keep asking.
///
/// # Errors
///
/// Returns [`BundleError::Config`] when the service cannot be reached or refuses the poll, and when
/// the store refuses the write — a token that could not be filed is a sentence rather than an
/// authorization that silently did not happen.
pub async fn poll(secrets: &SecretHandle, pending: &PendingAuth) -> Result<bool, BundleError> {
    let Some(tokens) = oauth::poll(&pending.inner)
        .await
        .map_err(|error| BundleError::config(error.to_string()))?
    else {
        return Ok(false);
    };
    let stored = tokens
        .encode()
        .map_err(|error| BundleError::config(error.to_string()))?;
    secrets
        .set(pending.account, &stored)
        .await
        .map_err(|error| BundleError::config(format!("{}: {error}", pending.account)))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An authorization is only started for a plan that takes one: a key plan is a mistake, not a
    /// flow to begin.
    #[test]
    fn an_authorization_is_only_started_for_a_plan_that_takes_one() {
        // The issuer is unreachable, so this also pins that the check happens *before* any request:
        // a key plan is refused without touching the network.
        let refused = nanus_kernel::runtime::block_on(begin(
            Provider::OpenAi,
            Some("api"),
            "http://127.0.0.1:1",
        ));
        let error = refused
            .expect_err("a key plan is not authorized")
            .to_string();
        assert!(error.contains("api"), "{error}");
        assert!(error.contains("key"), "{error}");
    }

    /// A plan that takes an authorization starts the flow, and an unreachable service is a sentence
    /// rather than a panic.
    #[test]
    fn an_authorization_against_an_unreachable_service_is_refused() {
        let outcome = nanus_kernel::runtime::block_on(begin(
            Provider::OpenAi,
            Some("subscription"),
            "http://127.0.0.1:1",
        ));
        assert!(outcome.is_err(), "an unreachable service is refused");
    }
}
