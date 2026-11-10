//! A credential that is asked for when a request is sent, rather than fixed when the adapter is
//! built.
//!
//! An API key is good until somebody revokes it, so the adapter holds one for its whole life. A
//! subscription's access token is not: it runs out, and an agent that serves for longer than
//! one lasts — a detached service, a long session in the interface — would go on sending the
//! token it was built with and be refused on every request until it was restarted. So the
//! adapter can be given a [`BearerSource`] instead, and asks it for the bearer at the last
//! moment before each request leaves. Renewing is the source's business; the adapter neither
//! knows how a grant is renewed nor where one is stored.

use nanus_ports::LocalBoxFuture;

/// What one request authenticates with.
///
/// `Debug` is written by hand, as the configuration's is: the token is the credential.
#[derive(Clone, PartialEq, Eq)]
pub struct Bearer {
    token: String,
    account_id: Option<String>,
}

impl Bearer {
    /// A bearer token, and the `ChatGPT` account a subscription request names beside it.
    #[must_use]
    pub fn new(token: impl Into<String>, account_id: Option<String>) -> Self {
        Self {
            token: token.into(),
            account_id,
        }
    }

    /// The token, by explicit request — named as [`nanus_ports::Secret::expose`] is, so the
    /// sites that handle a raw credential stay greppable.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.token
    }

    /// The account the request names in its own header, when it names one.
    #[must_use]
    pub fn account_id(&self) -> Option<&str> {
        self.account_id.as_deref()
    }
}

impl core::fmt::Debug for Bearer {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Bearer")
            .field("token", &"<redacted>")
            .field("account_id", &self.account_id)
            .finish()
    }
}

/// Where a request's bearer comes from, when it can change while the adapter lives.
pub trait BearerSource {
    /// Returns the bearer to send now, renewing it first if it has run out.
    ///
    /// The error is a sentence for the reader: it becomes the turn's failure, and nothing is
    /// sent, because a request that is known to be refused costs a round trip to say so.
    fn bearer(&self) -> LocalBoxFuture<'_, Result<Bearer, String>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_debug_of_a_bearer_keeps_the_token_out() {
        let bearer = Bearer::new("access-token-value", Some(String::from("acct-1")));
        let rendered = format!("{bearer:?}");
        assert!(!rendered.contains("access-token-value"), "{rendered}");
        // The pair: the account is a name, not a secret, and is still worth seeing.
        assert!(rendered.contains("acct-1"), "{rendered}");
        assert_eq!(bearer.expose(), "access-token-value");
        assert_eq!(bearer.account_id(), Some("acct-1"));
    }
}
