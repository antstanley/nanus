//! The `ChatGPT` subscription authorization: the flow that files a token set.
//!
//! A subscription is not a key. It is a `ChatGPT` account, authorized through `OpenAI`'s OAuth
//! service, whose grant is a refresh token plus a short-lived access token. This module runs the
//! **device** flow, which is the one a terminal can drive: the user is shown a page and a code,
//! authorizes on any machine, and the flow polls until the service hands back an authorization
//! code. The browser flow — a local callback on a fixed port — needs a browser on the machine the
//! agent runs on, which a service does not have.
//!
//! The shape follows the Codex CLI and opencode, which use the same public client id: a device
//! request, a poll that answers `403`/`404` until the user is done, and a final exchange for the
//! tokens. The account id is read out of the token's claims because the `ChatGPT` backend wants it
//! named on every request.

use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

/// The public client id the Codex CLI and opencode authorize with.
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

/// Where `OpenAI`'s authorization service lives.
pub const ISSUER: &str = "https://auth.openai.com";

/// The path the device page lives at, appended to the issuer.
const DEVICE_PATH: &str = "/codex/device";

/// How long to wait between polls when the service names no interval.
const DEFAULT_INTERVAL_SECS: u64 = 5;

/// The least the service's own interval is respected as.
const MIN_INTERVAL_SECS: u64 = 1;

/// Why the authorization could not be started or completed.
#[derive(Debug, thiserror::Error)]
pub enum OAuthError {
    /// The service could not be reached, or answered something unreadable.
    #[error("the authorization service could not be reached: {0}")]
    Transport(String),
    /// The service answered a status the flow does not expect.
    #[error("the authorization service answered HTTP {0}")]
    Status(u16),
}

/// An authorization waiting on the user.
///
/// Held between the poll that started it and the polls that watch it, because the service keys the
/// poll on the id it handed out rather than on the code the user types.
#[derive(Clone, Debug)]
pub struct Pending {
    /// The page the user visits.
    pub url: String,
    /// The code they enter there.
    pub code: String,
    /// How long to wait between polls.
    pub interval: Duration,
    device_auth_id: String,
    issuer: String,
    client: reqwest::Client,
}

/// A token set, as the service returns it and as it is stored.
///
/// `Debug` is implemented by hand rather than derived, as `Secret`'s is: a token set *is* the
/// credential, so a derived one would put the access and refresh tokens into whatever log line
/// formats it.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct Tokens {
    /// The short-lived token a request carries.
    pub access_token: String,
    /// The token a later access token is minted from.
    pub refresh_token: String,
    /// The identity token, whose claims name the account.
    #[serde(default)]
    pub id_token: String,
    /// When the access token stops being accepted, as a Unix instant in milliseconds.
    ///
    /// Absolute rather than the service's own `expires_in`, because a stored set is read back long
    /// after it was minted: a lifetime described relative to a moment nobody recorded cannot answer
    /// whether it has passed, so a set that recorded one would look freshly minted for ever.
    ///
    /// `None` when the service stated no lifetime, which [`Tokens::is_expired`] reads as expired —
    /// the grant is then renewed rather than sent and refused.
    #[serde(default)]
    pub expires_at: Option<u64>,
    /// The `ChatGPT` account the tokens belong to, read from their claims.
    #[serde(default)]
    pub account_id: Option<String>,
}

impl core::fmt::Debug for Tokens {
    /// Renders the shape without the grant.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Tokens")
            .field("access_token", &"<redacted>")
            .field("refresh_token", &"<redacted>")
            .field(
                "id_token",
                &if self.id_token.is_empty() {
                    "<absent>"
                } else {
                    "<redacted>"
                },
            )
            .field("expires_at", &self.expires_at)
            .field("account_id", &self.account_id)
            .finish()
    }
}

impl Tokens {
    /// Returns `true` when the access token is expired or within `margin` of it.
    ///
    /// A set whose expiry was never recorded — or one written before the expiry was recorded at all
    /// — is treated as expired, so it is renewed rather than sent and refused.
    #[must_use]
    pub fn is_expired(&self, margin: Duration) -> bool {
        let Some(expires_at) = self.expires_at else {
            return true;
        };
        // A clock that cannot be read is not a reason to trust a token: renewing costs a request,
        // and sending an expired one costs the turn.
        let Some(now) = now_ms() else {
            return true;
        };
        let margin = u64::try_from(margin.as_millis()).unwrap_or(u64::MAX);
        now.saturating_add(margin) >= expires_at
    }

    /// Renders the token set as the single string the credential store holds.
    ///
    /// # Errors
    ///
    /// Returns [`OAuthError::Transport`] if it cannot be rendered, which is a defect rather than
    /// bad input: every field is a string or an integer.
    pub fn encode(&self) -> Result<String, OAuthError> {
        serde_json::to_string(self).map_err(|error| {
            OAuthError::Transport(format!("the token set does not encode: {error}"))
        })
    }

    /// Reads a token set back out of the stored string.
    ///
    /// # Errors
    ///
    /// Returns [`OAuthError::Transport`] when the stored value is not a token set, so a credential
    /// filed by something else is a sentence rather than a field that silently reads as empty.
    pub fn decode(stored: &str) -> Result<Self, OAuthError> {
        serde_json::from_str(stored).map_err(|error| {
            OAuthError::Transport(format!("the stored token set is unreadable: {error}"))
        })
    }
}

/// The current Unix instant in milliseconds, when the clock can say.
///
/// A clock that cannot be read answers nothing, and every caller treats that as "renew rather than
/// trust": the cost of one extra request is smaller than the cost of a turn spent on a token the
/// provider has stopped accepting.
fn now_ms() -> Option<u64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|since| u64::try_from(since.as_millis()).ok())
}

/// The instant a stated lifetime ends, as a Unix instant in milliseconds.
///
/// This is where the service's relative `expires_in` becomes the absolute instant the store holds,
/// recorded once, at the moment the token was minted.
fn expiry_of(lifetime: Option<u64>) -> Option<u64> {
    // Saturating rather than wrapping: an expiry a very long way out must not read as one in the
    // past.
    let seconds = lifetime?;
    Some(now_ms()?.saturating_add(seconds.saturating_mul(1_000)))
}

/// What the device endpoint answers with.
#[derive(Debug, Deserialize)]
struct DeviceCode {
    device_auth_id: String,
    user_code: String,
    #[serde(default)]
    interval: Option<String>,
}

/// What the poll answers with once the user has authorized.
#[derive(Debug, Deserialize)]
struct AuthorizationCode {
    authorization_code: String,
    code_verifier: String,
}

/// What the token endpoint answers with.
#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    /// A refresh response may omit it, in which case the old one still stands.
    #[serde(default)]
    refresh_token: String,
    #[serde(default)]
    id_token: String,
    #[serde(default)]
    expires_in: Option<u64>,
}

/// Starts a device authorization, returning what to show the user.
///
/// # Errors
///
/// Returns [`OAuthError`] when the HTTP client cannot be built, the service cannot be reached, or it
/// refuses the device request.
pub async fn begin(issuer: &str) -> Result<Pending, OAuthError> {
    let client = reqwest::Client::builder()
        .build()
        .map_err(|error| OAuthError::Transport(error.to_string()))?;
    let response = client
        .post(format!("{issuer}/api/accounts/deviceauth/usercode"))
        .json(&serde_json::json!({ "client_id": CLIENT_ID }))
        .send()
        .await
        .map_err(|error| OAuthError::Transport(error.to_string()))?;
    if !response.status().is_success() {
        return Err(OAuthError::Status(response.status().as_u16()));
    }
    let device: DeviceCode = response
        .json()
        .await
        .map_err(|error| OAuthError::Transport(error.to_string()))?;
    let seconds = device
        .interval
        .as_deref()
        .and_then(|raw| raw.parse::<u64>().ok())
        .unwrap_or(DEFAULT_INTERVAL_SECS)
        .max(MIN_INTERVAL_SECS);
    Ok(Pending {
        url: format!("{issuer}{DEVICE_PATH}"),
        code: device.user_code,
        interval: Duration::from_secs(seconds),
        device_auth_id: device.device_auth_id,
        issuer: issuer.to_owned(),
        client,
    })
}

/// Polls once for the authorization, returning the tokens when the user has finished.
///
/// `Ok(None)` means still waiting: the service answers `403`/`404` until the user authorizes, and
/// that is not a failure. Any other status is.
///
/// # Errors
///
/// Returns [`OAuthError`] when the service cannot be reached or answers a status other than the
/// two that mean "not yet".
pub async fn poll(pending: &Pending) -> Result<Option<Tokens>, OAuthError> {
    let response = pending
        .client
        .post(format!("{}/api/accounts/deviceauth/token", pending.issuer))
        .json(&serde_json::json!({
            "device_auth_id": pending.device_auth_id,
            "user_code": pending.code,
        }))
        .send()
        .await
        .map_err(|error| OAuthError::Transport(error.to_string()))?;
    let status = response.status();
    if status == reqwest::StatusCode::FORBIDDEN || status == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !status.is_success() {
        return Err(OAuthError::Status(status.as_u16()));
    }
    let code: AuthorizationCode = response
        .json()
        .await
        .map_err(|error| OAuthError::Transport(error.to_string()))?;
    let tokens = exchange(
        pending,
        &code.authorization_code,
        &format!("{}/deviceauth/callback", pending.issuer),
        &code.code_verifier,
    )
    .await?;
    Ok(Some(tokens))
}

/// Exchanges an authorization code for a token set.
async fn exchange(
    pending: &Pending,
    code: &str,
    redirect: &str,
    verifier: &str,
) -> Result<Tokens, OAuthError> {
    let response = pending
        .client
        .post(format!("{}/oauth/token", pending.issuer))
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .body(form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect),
            ("client_id", CLIENT_ID),
            ("code_verifier", verifier),
        ]))
        .send()
        .await
        .map_err(|error| OAuthError::Transport(error.to_string()))?;
    if !response.status().is_success() {
        return Err(OAuthError::Status(response.status().as_u16()));
    }
    let tokens: TokenResponse = response
        .json()
        .await
        .map_err(|error| OAuthError::Transport(error.to_string()))?;
    Ok(Tokens {
        account_id: account_id(&tokens),
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        id_token: tokens.id_token,
        expires_at: expiry_of(tokens.expires_in),
    })
}

/// Encodes a form body, because the HTTP client here is built without the form feature.
///
/// Everything outside the unreserved set is percent-encoded, so a redirect URI's `:` and `/` — and
/// any character a token carries — reach the service rather than splitting the body.
fn form(fields: &[(&str, &str)]) -> String {
    let mut body = String::new();
    for (name, value) in fields {
        if !body.is_empty() {
            body.push('&');
        }
        percent_encode(name, &mut body);
        body.push('=');
        percent_encode(value, &mut body);
    }
    body
}

/// Percent-encodes `value` into `out`, leaving the unreserved characters alone.
fn percent_encode(value: &str, out: &mut String) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push('%');
            out.push(char::from(HEX[usize::from(byte >> 4)]));
            out.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
}

/// Mints a new access token from a stored refresh token.
///
/// # Errors
///
/// Returns [`OAuthError`] when the service cannot be reached or refuses the grant, which means the
/// authorization is gone and the user must complete the flow again.
pub async fn refresh(issuer: &str, tokens: &Tokens) -> Result<Tokens, OAuthError> {
    let client = reqwest::Client::builder()
        .build()
        .map_err(|error| OAuthError::Transport(error.to_string()))?;
    let response = client
        .post(format!("{issuer}/oauth/token"))
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .body(form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", &tokens.refresh_token),
            ("client_id", CLIENT_ID),
        ]))
        .send()
        .await
        .map_err(|error| OAuthError::Transport(error.to_string()))?;
    if !response.status().is_success() {
        return Err(OAuthError::Status(response.status().as_u16()));
    }
    let refreshed: TokenResponse = response
        .json()
        .await
        .map_err(|error| OAuthError::Transport(error.to_string()))?;
    Ok(Tokens {
        account_id: account_id(&refreshed).or_else(|| tokens.account_id.clone()),
        access_token: refreshed.access_token,
        // A response that rotates nothing keeps the token that was used.
        refresh_token: if refreshed.refresh_token.is_empty() {
            tokens.refresh_token.clone()
        } else {
            refreshed.refresh_token
        },
        id_token: refreshed.id_token,
        // A response that states no lifetime leaves the new expiry unknown, so the next read renews
        // again rather than trusting a token whose life nobody recorded.
        expires_at: expiry_of(refreshed.expires_in),
    })
}

/// Reads the `ChatGPT` account id out of a token set's claims.
fn account_id(tokens: &TokenResponse) -> Option<String> {
    claim(&tokens.id_token).or_else(|| claim(&tokens.access_token))
}

/// Reads the account id from one token's claims, when it carries one.
fn claim(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let claims: Value = serde_json::from_slice(&decode_base64url(payload)?).ok()?;
    claims
        .get("chatgpt_account_id")
        .and_then(Value::as_str)
        .or_else(|| {
            claims
                .get("https://api.openai.com/auth")
                .and_then(|auth| auth.get("chatgpt_account_id"))
                .and_then(Value::as_str)
        })
        .or_else(|| {
            claims
                .get("organizations")
                .and_then(|list| list.get(0))
                .and_then(|org| org.get("id"))
                .and_then(Value::as_str)
        })
        .map(str::to_owned)
}

/// Decodes the base64url a `JWT` payload is written in.
///
/// Hand-rolled rather than pulled in: it is a few lines and the only base64 this crate needs, and
/// the workspace would otherwise carry a dependency to read one field out of one token.
fn decode_base64url(raw: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(raw.len());
    let mut acc: u32 = 0;
    let mut sextets: u32 = 0;
    for byte in raw.bytes() {
        let value = match byte {
            b'A'..=b'Z' => u32::from(byte.wrapping_sub(b'A')),
            b'a'..=b'z' => u32::from(byte.wrapping_sub(b'a')).wrapping_add(26),
            b'0'..=b'9' => u32::from(byte.wrapping_sub(b'0')).wrapping_add(52),
            b'-' => 62,
            b'_' => 63,
            // Padding ends the payload; anything else is not base64url.
            b'=' => break,
            _ => return None,
        };
        // Four sextets are twenty-four bits, and the accumulator is flushed at each group, so the
        // shift can never overflow a `u32`.
        acc = (acc << 6) | value;
        sextets = sextets.wrapping_add(1);
        if sextets == 4 {
            out.push(u8::try_from(acc >> 16).ok()?);
            out.push(u8::try_from((acc >> 8) & 0xff).ok()?);
            out.push(u8::try_from(acc & 0xff).ok()?);
            acc = 0;
            sextets = 0;
        }
    }
    match sextets {
        2 => out.push(u8::try_from((acc >> 4) & 0xff).ok()?),
        3 => {
            out.push(u8::try_from(acc >> 10).ok()?);
            out.push(u8::try_from((acc >> 2) & 0xff).ok()?);
        }
        _ => {}
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The decoder reads the base64url a `JWT` payload is written in, padding and all.
    #[test]
    fn base64url_decodes_the_payload_of_a_token() {
        // `{"a":1}` is `eyJhIjoxfQ`.
        assert_eq!(
            decode_base64url("eyJhIjoxfQ").as_deref(),
            Some(br#"{"a":1}"#.as_slice())
        );
        // Padding is accepted and ignored.
        assert_eq!(
            decode_base64url("eyJhIjoxfQ==").as_deref(),
            Some(br#"{"a":1}"#.as_slice())
        );
        // A character outside the alphabet is refused rather than guessed at.
        assert!(decode_base64url("not+base64").is_none());
    }

    /// The account id comes out of the claims, under any of the three names the tokens use.
    #[test]
    fn the_account_id_is_read_from_the_claims() {
        let token =
            |claims: &str| format!("header.{}.signature", base64url_encode(claims.as_bytes()));
        assert_eq!(
            claim(&token(r#"{"chatgpt_account_id":"acct-1"}"#)).as_deref(),
            Some("acct-1")
        );
        assert_eq!(
            claim(&token(
                r#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acct-2"}}"#
            ))
            .as_deref(),
            Some("acct-2")
        );
        assert_eq!(
            claim(&token(r#"{"organizations":[{"id":"acct-3"}]}"#)).as_deref(),
            Some("acct-3")
        );
        // A token with no such claim, or not a token at all, answers nothing rather than guessing.
        assert_eq!(claim(&token(r#"{"sub":"someone"}"#)), None);
        assert_eq!(claim("not-a-token"), None);
    }

    /// The stored form round-trips, so a filed authorization is readable back.
    #[test]
    fn a_token_set_round_trips_through_its_stored_form() {
        let tokens = Tokens {
            access_token: String::from("access"),
            refresh_token: String::from("refresh"),
            id_token: String::from("id"),
            expires_at: Some(1_700_000_000_000),
            account_id: Some(String::from("acct-1")),
        };
        let encoded = tokens.encode().expect("the token set encodes");
        let decoded = Tokens::decode(&encoded).expect("the token set decodes");
        assert_eq!(decoded.access_token, "access");
        assert_eq!(decoded.refresh_token, "refresh");
        assert_eq!(decoded.account_id.as_deref(), Some("acct-1"));
        assert_eq!(
            decoded.expires_at,
            Some(1_700_000_000_000),
            "the instant is stored, not a lifetime measured from whenever it is read"
        );
        // Something that is not a token set is refused rather than read as empty fields.
        assert!(Tokens::decode("not json").is_err());
    }

    /// The grant is renewed when its life has run out, and only then.
    ///
    /// This is the property that made the relative form useless: a lifetime recorded as "an hour"
    /// reads as an hour from whenever it is *read*, so a set filed a day ago looked fresh and an
    /// expired access token was sent until the provider refused the turn.
    #[test]
    fn a_grant_is_renewed_once_its_lifetime_has_passed() {
        let now = now_ms().expect("a clock");
        let margin = Duration::from_mins(5);
        let set = |expires_at| Tokens {
            access_token: String::from("access"),
            refresh_token: String::from("refresh"),
            id_token: String::new(),
            expires_at,
            account_id: None,
        };
        // An hour of life granted two hours ago: the hour has passed, so it is renewed.
        assert!(
            set(Some(now.saturating_sub(2 * 60 * 60 * 1_000))).is_expired(margin),
            "an expired grant is renewed"
        );
        // An hour of life left: sent rather than renewed.
        assert!(
            !set(Some(now.saturating_add(60 * 60 * 1_000))).is_expired(margin),
            "a live grant is used"
        );
        // Inside the margin, a request that leaves now would arrive after the expiry.
        assert!(
            set(Some(now.saturating_add(60 * 1_000))).is_expired(margin),
            "a grant about to expire is renewed early"
        );
        // Nothing recorded: renewed rather than trusted.
        assert!(set(None).is_expired(margin), "an unrecorded expiry renews");
    }

    /// A lifetime from the service becomes an instant on the clock, recorded once.
    #[test]
    fn a_stated_lifetime_becomes_an_absolute_instant() {
        let before = now_ms().expect("a clock");
        let instant = expiry_of(Some(3_600)).expect("an hour becomes an instant");
        let after = now_ms().expect("a clock");
        assert!(
            (before + 3_600_000..=after + 3_600_000).contains(&instant),
            "the instant is an hour from the moment it was minted: {instant} not in {before}..={after}"
        );
        assert_eq!(
            expiry_of(None),
            None,
            "a service that states no lifetime leaves it unrecorded, which reads as expired"
        );
    }

    /// The `Debug` of a token set names the shape and keeps the grant out of a log line.
    #[test]
    fn the_debug_of_a_token_set_keeps_the_grant_out() {
        let tokens = Tokens {
            access_token: String::from("access-secret"),
            refresh_token: String::from("refresh-secret"),
            id_token: String::from("id-secret"),
            expires_at: Some(1_700_000_000_000),
            account_id: Some(String::from("acct-1")),
        };
        let rendered = format!("{tokens:?}");
        for secret in ["access-secret", "refresh-secret", "id-secret"] {
            assert!(
                !rendered.contains(secret),
                "{secret} reached a log: {rendered}"
            );
        }
        // The shape is still useful: what is present, and when it expires.
        assert!(rendered.contains("expires_at"), "{rendered}");
        assert!(rendered.contains("acct-1"), "{rendered}");
        assert!(rendered.contains("<redacted>"), "{rendered}");
    }

    /// Test-side base64url, independent of the decoder under test.
    fn base64url_encode(bytes: &[u8]) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let mut acc = 0u32;
            for (index, byte) in chunk.iter().enumerate() {
                let shift =
                    16u32.saturating_sub(8u32.saturating_mul(u32::try_from(index).unwrap_or(0)));
                acc |= u32::from(*byte) << shift;
            }
            let groups = chunk.len().saturating_add(1);
            for index in 0..groups {
                let shift =
                    18u32.saturating_sub(6u32.saturating_mul(u32::try_from(index).unwrap_or(0)));
                let sextet = (acc >> shift) & 0x3f;
                out.push(char::from(ALPHABET[usize::try_from(sextet).unwrap_or(0)]));
            }
        }
        out
    }
}
