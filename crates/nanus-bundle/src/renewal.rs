//! Keeping an authorization usable for as long as the agent that holds it.
//!
//! A subscription's grant is a long-lived refresh token and a short-lived access token. The
//! access token used to be checked once, when the adapter was built, so an agent that outlived
//! it — a detached service, a session left open — sent an expired bearer on every request
//! until somebody switched provider or restarted it. [`RenewingGrant`] is what the adapter asks
//! instead, as each request leaves: it answers from what it holds while that is good, and
//! renews when it is not.
//!
//! ## Another process may have renewed first
//!
//! A service and a `nanus run` share one store, and the authorization service rotates the
//! refresh token it is shown. So before renewing, the store is read again: a grant another
//! process renewed is taken as it is, and the refresh token this process holds — which that
//! renewal retired — is never presented. The store is read only then, at expiry, because on
//! macOS a read is a `security` invocation and a request should not pay for one.

use std::cell::RefCell;
use std::time::Duration;

use nanus_adapter_openai::{Bearer, BearerSource, oauth};
use nanus_ports::{LocalBoxFuture, SecretHandle};

use crate::error::BundleError;

/// How long before an access token's expiry it is renewed.
///
/// A request that leaves just before the expiry would arrive just after it, so the renewal happens
/// early rather than exactly on time.
pub const REFRESH_MARGIN: Duration = Duration::from_mins(5);

/// Renews `tokens` at `issuer` and files the new set under `account`.
///
/// The renewed set is written back before it is returned, so the next reader — this process or
/// another — does not renew again, and does not present a refresh token this renewal retired.
///
/// # Errors
///
/// Returns [`BundleError::Config`] when the service refuses the grant or cannot be reached,
/// naming the command that authorizes again, and when the store refuses the write.
pub async fn renew_and_store(
    secrets: &SecretHandle,
    account: &str,
    issuer: &str,
    tokens: &oauth::Tokens,
) -> Result<oauth::Tokens, BundleError> {
    let renewed = oauth::refresh(issuer, tokens).await.map_err(|error| {
        BundleError::config(format!(
            "{account} could not be renewed: {error}; run `nanus auth login {account}` to authorize \
             it again"
        ))
    })?;
    let encoded = renewed
        .encode()
        .map_err(|error| BundleError::config(format!("{account}: {error}")))?;
    secrets
        .set(account, &encoded)
        .await
        .map_err(|error| BundleError::config(format!("{account}: {error}")))?;
    Ok(renewed)
}

/// A subscription's grant, renewed when it runs out rather than when the adapter was built.
pub struct RenewingGrant {
    secrets: SecretHandle,
    account: &'static str,
    issuer: String,
    /// The newest set this process knows of. Never borrowed across an await.
    held: RefCell<oauth::Tokens>,
}

impl core::fmt::Debug for RenewingGrant {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // The token set redacts itself; the store handle has nothing to show.
        f.debug_struct("RenewingGrant")
            .field("account", &self.account)
            .field("held", &self.held)
            .finish_non_exhaustive()
    }
}

impl RenewingGrant {
    /// Holds `tokens`, filed under `account` in `secrets`, renewing at the real service.
    pub fn new(secrets: SecretHandle, account: &'static str, tokens: oauth::Tokens) -> Self {
        Self::at(secrets, account, tokens, oauth::ISSUER)
    }

    /// [`RenewingGrant::new`], renewing at `issuer` — which is how a test serves the renewal.
    pub fn at(
        secrets: SecretHandle,
        account: &'static str,
        tokens: oauth::Tokens,
        issuer: &str,
    ) -> Self {
        Self {
            secrets,
            account,
            issuer: issuer.to_owned(),
            held: RefCell::new(tokens),
        }
    }

    /// Returns a token set whose access token is good for at least the margin.
    async fn current(&self) -> Result<oauth::Tokens, BundleError> {
        let held = self.held.borrow().clone();
        if !held.is_expired(REFRESH_MARGIN) {
            return Ok(held);
        }
        // What the store holds now is newer than what this process holds whenever another
        // process renewed in between, and its refresh token is the one still accepted. A store
        // that cannot answer, or holds something unreadable, leaves the held set as the best
        // there is.
        let latest = match self.secrets.get(self.account).await {
            Ok(Some(stored)) => oauth::Tokens::decode(stored.expose()).unwrap_or(held),
            Ok(None) | Err(_) => held,
        };
        let fresh = if latest.is_expired(REFRESH_MARGIN) {
            renew_and_store(&self.secrets, self.account, &self.issuer, &latest).await?
        } else {
            latest
        };
        *self.held.borrow_mut() = fresh.clone();
        Ok(fresh)
    }
}

impl BearerSource for RenewingGrant {
    fn bearer(&self) -> LocalBoxFuture<'_, Result<Bearer, String>> {
        Box::pin(async move {
            let tokens = self.current().await.map_err(|error| error.to_string())?;
            Ok(Bearer::new(tokens.access_token, tokens.account_id))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nanus_ports::{Secret, SecretPort, SecretResult};
    use std::cell::Cell;
    use std::rc::Rc;

    /// The account the tests file a grant under.
    const ACCOUNT: &str = "openai:subscription";

    /// One account's worth of store, which counts how often it is read.
    #[derive(Default)]
    struct Store {
        value: RefCell<Option<String>>,
        reads: Rc<Cell<u32>>,
    }

    impl SecretPort for Store {
        fn get<'a>(
            &'a self,
            _account: &'a str,
        ) -> LocalBoxFuture<'a, SecretResult<Option<Secret>>> {
            Box::pin(async move {
                self.reads.set(self.reads.get().saturating_add(1));
                Ok(self.value.borrow().clone().map(Secret::new))
            })
        }

        fn set<'a>(
            &'a self,
            _account: &'a str,
            secret: &'a str,
        ) -> LocalBoxFuture<'a, SecretResult<()>> {
            Box::pin(async move {
                *self.value.borrow_mut() = Some(secret.to_owned());
                Ok(())
            })
        }

        fn clear<'a>(&'a self, _account: &'a str) -> LocalBoxFuture<'a, SecretResult<bool>> {
            Box::pin(async move { Ok(self.value.borrow_mut().take().is_some()) })
        }

        fn backend(&self) -> &'static str {
            "test"
        }
    }

    /// A store holding `stored`, and the count of reads made of it.
    fn store(stored: Option<&oauth::Tokens>) -> (SecretHandle, Rc<Cell<u32>>) {
        let reads = Rc::new(Cell::new(0));
        let store = Store {
            value: RefCell::new(stored.map(|tokens| tokens.encode().expect("encodes"))),
            reads: Rc::clone(&reads),
        };
        (Rc::new(Box::new(store)), reads)
    }

    /// A grant whose access token is `access` and runs out at `expires_at`.
    fn grant(access: &str, refresh: &str, expires_at: u64) -> oauth::Tokens {
        oauth::Tokens {
            access_token: access.to_owned(),
            refresh_token: refresh.to_owned(),
            id_token: String::new(),
            expires_at: Some(expires_at),
            account_id: Some(String::from("acct-1")),
        }
    }

    /// An instant an hour from now, as the store records one.
    fn in_an_hour() -> u64 {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the clock is past the epoch");
        u64::try_from(now.as_millis())
            .expect("fits")
            .saturating_add(3_600_000)
    }

    /// The requests an authorization service was sent, as raw text.
    type Requests = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

    /// An authorization service that answers every renewal with `status` and `body`, returning
    /// its issuer and the requests it was sent.
    fn issuer(status: u16, body: &'static str) -> (String, Requests) {
        use std::io::{Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("the bound address");
        let seen = Requests::default();
        let log = std::sync::Arc::clone(&seen);
        std::thread::spawn(move || {
            while let Ok((mut stream, _)) = listener.accept() {
                let mut request = [0_u8; 8192];
                let count = stream.read(&mut request).unwrap_or(0);
                let text = String::from_utf8_lossy(request.get(..count).unwrap_or_default());
                log.lock().expect("the log").push(text.into_owned());
                let head = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(body.as_bytes());
                let _ = stream.flush();
            }
        });
        (format!("http://{address}"), seen)
    }

    /// How many requests the service has been sent.
    fn count(requests: &Requests) -> usize {
        requests.lock().expect("the log").len()
    }

    fn block_on<T>(future: impl Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime")
            .block_on(future)
    }

    /// What the service answers a renewal with: a new access token, good for an hour.
    const RENEWED: &str =
        r#"{"access_token":"new-access","refresh_token":"refresh-2","expires_in":3600}"#;

    /// A grant that is still good is the answer, and nobody is asked: not the service, and not
    /// the store, which on macOS is a process to start.
    #[test]
    fn a_grant_that_is_still_good_is_sent_without_asking_anyone() {
        let (service, requests) = issuer(500, "{}");
        let (secrets, reads) = store(None);
        let held = grant("held-access", "refresh-1", in_an_hour());
        let source = RenewingGrant::at(secrets, ACCOUNT, held, &service);
        let bearer = block_on(source.bearer()).expect("a bearer");
        assert_eq!(bearer.expose(), "held-access");
        assert_eq!(bearer.account_id(), Some("acct-1"));
        assert_eq!(count(&requests), 0, "the service was not asked");
        assert_eq!(reads.get(), 0, "the store was not read");
    }

    /// The defect: an agent that outlived its access token went on sending it. The grant is
    /// renewed as the request leaves, the new set is filed, and the request after it is
    /// answered from what is now held rather than by renewing again.
    #[test]
    fn a_grant_that_ran_out_is_renewed_filed_and_then_reused() {
        let (service, requests) = issuer(200, RENEWED);
        let expired = grant("old-access", "refresh-1", 1);
        let (secrets, _reads) = store(Some(&expired));
        let source = RenewingGrant::at(Rc::clone(&secrets), ACCOUNT, expired, &service);

        let first = block_on(source.bearer()).expect("a bearer");
        assert_eq!(first.expose(), "new-access");
        assert_eq!(
            first.account_id(),
            Some("acct-1"),
            "the account survives a renewal that names none"
        );
        let sent = requests.lock().expect("the log").join("\n");
        assert!(sent.contains("refresh_token=refresh-1"), "{sent}");

        let filed = block_on(secrets.get(ACCOUNT))
            .expect("reads")
            .expect("filed");
        let filed = oauth::Tokens::decode(filed.expose()).expect("a token set");
        assert_eq!(filed.access_token, "new-access");
        assert_eq!(
            filed.refresh_token, "refresh-2",
            "the rotated token is kept"
        );

        let second = block_on(source.bearer()).expect("a bearer");
        assert_eq!(second.expose(), "new-access");
        assert_eq!(count(&requests), 1, "one renewal served both requests");
    }

    /// Another process renewed first. Its set is in the store, and it is used as it is: the
    /// refresh token this process holds was retired by that renewal, and presenting it would
    /// be refused.
    #[test]
    fn a_grant_another_process_renewed_is_taken_from_the_store() {
        let (service, requests) = issuer(400, "{}");
        let theirs = grant("their-access", "refresh-2", in_an_hour());
        let (secrets, _reads) = store(Some(&theirs));
        let ours = grant("old-access", "refresh-1", 1);
        let source = RenewingGrant::at(secrets, ACCOUNT, ours, &service);
        let bearer = block_on(source.bearer()).expect("a bearer");
        assert_eq!(bearer.expose(), "their-access");
        assert_eq!(
            count(&requests),
            0,
            "the retired refresh token was not shown"
        );
    }

    /// A grant the service will not renew is a sentence naming the way back in, and the old
    /// token is not handed out as though it might still work.
    #[test]
    fn a_grant_the_service_refuses_is_a_failure_that_names_the_way_back_in() {
        let (service, _requests) = issuer(401, "{}");
        let expired = grant("old-access", "refresh-1", 1);
        let (secrets, _reads) = store(Some(&expired));
        let source = RenewingGrant::at(secrets, ACCOUNT, expired, &service);
        let refused = block_on(source.bearer()).expect_err("no bearer");
        assert!(refused.contains("401"), "{refused}");
        assert!(
            refused.contains("nanus auth login openai:subscription"),
            "{refused}"
        );
        assert!(!refused.contains("old-access"), "{refused}");
    }
}
