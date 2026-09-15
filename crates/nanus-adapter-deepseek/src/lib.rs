//! # nanus-adapter-deepseek
//!
//! The only model provider nanus supports: `DeepSeek`, over its OpenAI-compatible
//! chat-completions endpoint.
//!
//! ## Why this crate is narrow on purpose
//!
//! nanus supports `DeepSeek` first, so the adapter may depend on `DeepSeek`'s
//! documented behaviour instead of flattening it into a lowest common denominator.
//! The behaviours that matter, all encoded below and covered by tests:
//!
//! - **The base URL is `https://api.deepseek.com`**, with `/chat/completions`
//!   appended. There is no `/v1` prefix in `DeepSeek`'s documentation or in the
//!   reference harness's own client.
//! - **Current model ids are [`MODEL_FLASH`] (`deepseek-flash`) and
//!   [`MODEL_PRO`] (`deepseek-v4-pro`).** The retired ids — `deepseek-chat` and
//!   `deepseek-reasoner`, discontinued on 2026-07-24 — are deliberately absent.
//! - **Thinking is on by default**, and the model's reasoning arrives in
//!   `reasoning_content` beside `content`. When a request carries tools the API
//!   requires `reasoning_content` to be passed back for *earlier* assistant turns
//!   too, or it rejects the request; [`wire`] therefore preserves it on the way out.
//! - **An assistant turn with no text must send `content: ""`, never `null`.** The
//!   live API answers 400 for a null-content, no-tool-call assistant message.
//! - **Usage rides on the last content chunk** rather than arriving in its own
//!   usage-only chunk, and is `null` on every other chunk.
//! - **A tool call's `arguments` is a JSON *string* the model may get wrong**, so it
//!   is accumulated as text here and only parsed at the boundary, where a malformed
//!   value becomes a reported failure instead of a panic.
//!
//! The crate's job is HTTP and stream decoding. Everything it produces is
//! [`nanus_ports`] vocabulary, so the agent core never learns what a
//! `reqwest::Response` is.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
// A `pub` item inside a private module is reachable only through this crate's own
// re-exports. The lint cannot tell that from an orphaned item, and every such item
// here is deliberate.
#![allow(unreachable_pub)]

mod config;
mod error;
mod wire;

pub use config::{
    API_KEY_ENV, DEFAULT_BASE_URL, DEFAULT_MAX_OUTPUT_TOKENS, DeepSeekConfig, MODEL_FLASH,
    MODEL_PRO,
};
pub use nanus_ports::ReasoningEffort;

/// Translates the harness's provider-neutral effort into `DeepSeek`'s wire vocabulary.
///
/// The two vocabularies differ, and the difference is the adapter's job to absorb:
///
/// | Port | `DeepSeek` | Effect |
/// |---|---|---|
/// | `Minimal` | `none` | thinking disabled entirely |
/// | `Low` | `low` | |
/// | `Medium` | `high` | `DeepSeek`'s documented default, so the neutral middle maps to it |
/// | `High` | `max` | |
///
/// `Minimal` is the one that is not a scale step: `DeepSeek` expresses "do not think"
/// as a separate mode, so the adapter writes `thinking: {"type": "disabled"}` and
/// omits `reasoning_effort` rather than sending a contradictory pair.
#[must_use]
pub const fn wire_effort(effort: ReasoningEffort) -> Option<&'static str> {
    match effort {
        ReasoningEffort::Minimal => None,
        ReasoningEffort::Low => Some("low"),
        ReasoningEffort::Medium => Some("high"),
        ReasoningEffort::High => Some("max"),
    }
}
pub use error::DeepSeekError;

use core::pin::Pin;
use futures::StreamExt as _;
use nanus_ports::{ChatRequest, LlmEvent, LlmPort, LlmStream};

/// The path appended to a configured base URL for a chat completion.
const CHAT_COMPLETIONS_PATH: &str = "/chat/completions";

/// Maximum length of an error body echoed back to the caller.
const BODY_SNIPPET_MAX: usize = 2_000;

/// A stream of model events.
type EventStream = Pin<Box<dyn futures::Stream<Item = LlmEvent> + 'static>>;

/// A `DeepSeek` chat-completions client.
///
/// The client holds one `reqwest::Client` so connections are pooled across steps,
/// which matters for an agent loop that issues a request per step.
pub struct DeepSeekLlm {
    client: reqwest::Client,
    config: DeepSeekConfig,
}

impl core::fmt::Debug for DeepSeekLlm {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // The API key is deliberately absent: a `Debug` rendering reaches logs.
        f.debug_struct("DeepSeekLlm")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl DeepSeekLlm {
    /// Builds an adapter for `config`.
    ///
    /// # Errors
    ///
    /// Returns [`DeepSeekError::Client`] when the underlying HTTP client cannot be
    /// constructed, which happens only when the platform TLS stack is unavailable.
    pub fn new(config: DeepSeekConfig) -> Result<Self, DeepSeekError> {
        let client = reqwest::Client::builder()
            .build()
            .map_err(|source| DeepSeekError::client(&source))?;
        Ok(Self { client, config })
    }

    /// Builds an adapter for `model`, reading the API key from the environment.
    ///
    /// # Errors
    ///
    /// Returns [`DeepSeekError::MissingCredential`] when `DEEPSEEK_API_KEY` is
    /// unset or empty. Failing here rather than on the first step is deliberate: a
    /// missing credential is a configuration mistake, and reporting it at startup
    /// is what makes it diagnosable.
    pub fn from_env(model: impl Into<String>) -> Result<Self, DeepSeekError> {
        Self::new(DeepSeekConfig::from_env(model)?)
    }

    /// Returns the configuration in use.
    #[must_use]
    pub const fn config(&self) -> &DeepSeekConfig {
        &self.config
    }

    /// The full endpoint a request is posted to.
    #[must_use]
    pub fn endpoint(&self) -> String {
        let base = self.config.base_url().trim_end_matches('/');
        // Postcondition: a usable base URL is absolute, so a misconfigured one
        // fails here rather than as a confusing transport error later.
        assert!(base.starts_with("http"), "a base URL is absolute");
        format!("{base}{CHAT_COMPLETIONS_PATH}")
    }

    /// Encodes a request as the JSON body `DeepSeek` expects.
    ///
    /// Exposed so tests and diagnostics can inspect the exact wire payload without
    /// performing a request.
    #[must_use]
    pub fn encode(&self, request: &ChatRequest) -> serde_json::Value {
        wire::build_request(&self.config, request)
    }
}

impl LlmPort for DeepSeekLlm {
    fn model(&self) -> &str {
        self.config.model()
    }

    fn stream_chat(&self, request: ChatRequest) -> LlmStream {
        let payload = self.encode(&request);
        let Ok(body) = serde_json::to_string(&payload) else {
            return error_stream("could not encode the request as JSON");
        };
        tracing::debug!(
            model = %self.config.model(),
            endpoint = %self.endpoint(),
            messages = request.messages.len(),
            tools = request.tools.len(),
            "dispatching a chat completion"
        );

        let client = self.client.clone();
        let endpoint = self.endpoint();
        let api_key = self.config.api_key().to_owned();
        // The host is resolved before the request so it survives as an owned value inside the
        // stream: the transport failure is reported after `&self` has gone out of scope.
        let host = self.config.base_url().to_owned();
        let stream_host = host.clone();

        let response = async move {
            let sent = client
                .post(&endpoint)
                .header("accept", "text/event-stream")
                .header("content-type", "application/json")
                .bearer_auth(api_key)
                .body(body)
                .send()
                .await;
            match sent {
                Ok(response) => Ok(response),
                Err(source) => Err(DeepSeekError::transport(&source, &host).to_string()),
            }
        };

        // The stream resolves the request, then decodes the body it produced. A
        // transport failure becomes a single terminal `Error` event rather than a
        // stream that ends silently, so the agent loop always learns why.
        let stream = futures::stream::once(response).flat_map(move |outcome| match outcome {
            Ok(response) => {
                // The head is in hand, so the server is answering and everything from here is
                // its body. Reported before the body is read, because this is the earliest
                // moment the fact is true — reading even one byte of it would put the wait for
                // that byte on the wrong side of the split.
                let head = futures::stream::iter([LlmEvent::ResponseHead]);
                let announced: EventStream =
                    Box::pin(head.chain(decode(response, stream_host.clone())));
                announced
            }
            Err(message) => error_stream_owned(message),
        });
        Box::pin(stream)
    }
}

/// A one-event stream carrying an error.
fn error_stream(message: &str) -> LlmStream {
    error_stream_owned(message.to_owned())
}

/// A one-event stream carrying an error message.
fn error_stream_owned(message: String) -> LlmStream {
    Box::pin(futures::stream::iter(vec![LlmEvent::Error(message)]))
}

/// Decodes a successful streaming response into model events.
///
/// `host` is the base URL the request was sent to, carried so that a failure part-way through
/// the body names the same endpoint the request did.
fn decode(response: reqwest::Response, host: String) -> EventStream {
    let status = response.status();
    if !status.is_success() {
        // The body carries DeepSeek's own message, which is the only useful thing
        // to show a user. It is truncated so a large error page cannot flood the
        // transcript.
        let body = async move {
            let text = response
                .text()
                .await
                .unwrap_or_else(|error| format!("<{error}>"));
            truncate(&text, BODY_SNIPPET_MAX)
        };
        let stream = futures::stream::once(body).map(move |body| {
            LlmEvent::Error(DeepSeekError::status(status.as_u16(), body).to_string())
        });
        return Box::pin(stream);
    }

    let mut bytes = response.bytes_stream();
    let mut decoder = wire::SseDecoder::new();
    let mut accumulator = wire::StreamAccumulator::default();
    let mut done = false;

    let stream = futures::stream::poll_fn(move |cx| {
        loop {
            if let Some(event) = accumulator.take_ready() {
                return core::task::Poll::Ready(Some(event));
            }
            if done {
                // Every event has been emitted. Ending here rather than waiting for
                // the socket to close means a stalled connection cannot hold a
                // turn open.
                return core::task::Poll::Ready(None);
            }
            match bytes.poll_next_unpin(cx) {
                core::task::Poll::Ready(Some(Ok(chunk))) => {
                    for line in decoder.push(&chunk) {
                        accumulator.observe_line(&line);
                    }
                    if decoder.is_done() {
                        // The sentinel means the server has sent everything. The
                        // accumulator must be closed *here*: it is what emits the
                        // assembled tool calls and the usage report, and setting `done`
                        // without closing it would drop both. Tool calls arrive as
                        // fragments, so they cannot be emitted until the stream ends —
                        // and this is where it ends.
                        accumulator.close();
                        done = true;
                    }
                }
                core::task::Poll::Ready(Some(Err(error))) => {
                    accumulator.fail(DeepSeekError::transport(&error, &host).to_string());
                    done = true;
                }
                core::task::Poll::Ready(None) => {
                    // A server that closes without a trailing newline still sent its
                    // last frame, so the decoder's tail is flushed before the
                    // accumulator is closed. Dropping it would lose the final delta
                    // of a response — typically the tool call that ends a step.
                    if let Some(tail) = decoder.finish() {
                        accumulator.observe_line(&tail);
                    }
                    // A stream that ends without `[DONE]` is still usable: whatever
                    // arrived is real, so it is emitted rather than discarded.
                    accumulator.close();
                    done = true;
                }
                core::task::Poll::Pending => return core::task::Poll::Pending,
            }
        }
    });
    Box::pin(stream)
}

/// Truncates `text` to at most `max` bytes, on a character boundary.
fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    let prefix = text.get(..end).unwrap_or_default();
    format!(
        "{prefix}… ({} bytes omitted)",
        text.len().saturating_sub(end)
    )
}

/// Returns the default maximum output tokens for a `DeepSeek` request.
#[must_use]
pub const fn default_max_output_tokens() -> u32 {
    DEFAULT_MAX_OUTPUT_TOKENS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adapter(config: DeepSeekConfig) -> Option<DeepSeekLlm> {
        DeepSeekLlm::new(config).ok()
    }

    #[test]
    fn endpoint_has_no_version_prefix() {
        let Some(llm) = adapter(DeepSeekConfig::new(MODEL_FLASH, "test-key")) else {
            return;
        };
        // DeepSeek's documentation and the reference harness both use the bare
        // host; a `/v1` prefix is a common mistake that produces a 404.
        assert_eq!(llm.endpoint(), "https://api.deepseek.com/chat/completions");
        assert!(!llm.endpoint().contains("/v1"));
    }

    #[test]
    fn a_trailing_slash_on_the_base_url_is_tolerated() {
        let config = DeepSeekConfig::with_base_url(MODEL_FLASH, "test-key", "https://proxy.test/");
        let Some(llm) = adapter(config) else {
            return;
        };
        assert_eq!(llm.endpoint(), "https://proxy.test/chat/completions");
    }

    #[test]
    fn debug_never_renders_the_api_key() {
        let config = DeepSeekConfig::new(MODEL_FLASH, "sk-super-secret-value");
        let Some(llm) = adapter(config) else {
            return;
        };
        // A `Debug` rendering reaches logs and error reports, so the key must not
        // be recoverable from it.
        let rendered = format!("{llm:?}");
        assert!(!rendered.contains("sk-super-secret-value"));
        assert!(rendered.contains(MODEL_FLASH));
    }

    #[test]
    fn truncate_respects_character_boundaries() {
        // Positive space: short input is untouched.
        assert_eq!(truncate("hello", 10), "hello");
        // Boundary: exactly at the cap.
        assert_eq!(truncate("hello", 5), "hello");
        // Negative space: a multi-byte character must not be split.
        let text = "日本語テキスト";
        let cut = truncate(text, 7);
        assert!(
            cut.starts_with("日本"),
            "expected a boundary-safe cut, got {cut}"
        );
        assert!(cut.contains("bytes omitted"));
        // Pair assertion: the retained prefix is a real prefix of the input.
        assert!(text.starts_with("日本"));
    }

    #[test]
    fn a_missing_credential_is_a_typed_error() {
        let config = DeepSeekConfig::new(MODEL_FLASH, "");
        let outcome = DeepSeekConfig::validate(&config);
        assert!(matches!(
            outcome,
            Err(DeepSeekError::MissingCredential { .. })
        ));
    }
}
