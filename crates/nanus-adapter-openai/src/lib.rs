//! # nanus-adapter-openai
//!
//! The `OpenAI`-compatible model adapter: `OpenAI`'s own API and z.ai's GLM API over
//! the `chat/completions` protocol.
//!
//! ## Why one crate serves two providers
//!
//! `OpenAI`'s chat-completions protocol is a de facto standard, and z.ai implements
//! it: the request body, the tool-call envelope, the server-sent-events framing, and
//! the usage object are the same shape for both. What differs is a handful of
//! documented facts — the host, the credential variable, the model ids, the output
//! ceiling, and how reasoning is requested — and those are collected in
//! [`Vendor`] rather than being spread through the code as `if provider == ...`.
//! A third compatible provider is one more entry in that enum.
//!
//! `DeepSeek` has its own adapter despite also being compatible, because its
//! requirements are its own: it needs an earlier turn's `reasoning_content` replayed
//! and it expresses "do not think" as a mode beside the effort. Folding three
//! vendors' exceptions into one encoder would make the exceptions harder to see
//! than they are here, where each one is a line in a table.
//!
//! ## Plans are endpoints
//!
//! z.ai sells a pay-as-you-go API and a coding subscription, and they are the same
//! key and the same protocol at different hosts, so a plan is a base URL
//! ([`ZAI_CODING_BASE_URL`]) rather than a second adapter. The coding plan is
//! selected by the harness, which knows about plans; this crate only needs to be
//! told where to send.
//!
//! ## What the adapter owns
//!
//! HTTP and stream decoding. Everything it produces is [`nanus_ports`] vocabulary,
//! so the agent core never learns what a `reqwest::Response` is. A transport
//! failure becomes a single terminal [`LlmEvent::Error`] rather than a stream that
//! ends silently, so the agent loop always learns why a step stopped.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
// A `pub` item inside a private module is reachable only through this crate's own
// re-exports. The lint cannot tell that from an orphaned item, and every such item
// here is deliberate.
#![allow(unreachable_pub)]

mod config;
mod error;
pub mod oauth;
mod wire;

pub use config::{
    OPENAI_API_KEY_ENV, OPENAI_BASE_URL, OpenAiConfig, Vendor, ZAI_API_KEY_ENV, ZAI_BASE_URL,
    ZAI_CODING_BASE_URL, effort_spelling, openai_effort_levels, zai_effort_levels,
};
pub use error::OpenAiError;
pub use nanus_ports::ReasoningEffort;

use core::pin::Pin;
use futures::StreamExt as _;
use nanus_ports::{ChatRequest, LlmEvent, LlmPort, LlmStream};

/// The path appended to a configured base URL for a chat completion.
const CHAT_COMPLETIONS_PATH: &str = "/chat/completions";

/// Maximum length of an error body echoed back to the caller.
const BODY_SNIPPET_MAX: usize = 2_000;

/// A stream of model events.
type EventStream = Pin<Box<dyn futures::Stream<Item = LlmEvent> + 'static>>;

/// A chat-completions client for one `OpenAI`-compatible vendor.
///
/// The client holds one `reqwest::Client` so connections are pooled across steps,
/// which matters for an agent loop that issues a request per step.
pub struct OpenAiLlm {
    client: reqwest::Client,
    config: OpenAiConfig,
}

impl core::fmt::Debug for OpenAiLlm {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // The API key is deliberately absent: a `Debug` rendering reaches logs.
        f.debug_struct("OpenAiLlm")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl OpenAiLlm {
    /// Builds an adapter for `config`.
    ///
    /// # Errors
    ///
    /// Returns [`OpenAiError::Client`] when the underlying HTTP client cannot be
    /// constructed, which happens only when the platform TLS stack is unavailable.
    pub fn new(config: OpenAiConfig) -> Result<Self, OpenAiError> {
        let client = reqwest::Client::builder()
            .build()
            .map_err(|source| OpenAiError::client(&source))?;
        Ok(Self { client, config })
    }

    /// Builds an adapter for `model`, reading the key from the vendor's variable.
    ///
    /// # Errors
    ///
    /// Returns [`OpenAiError::MissingCredential`] when the variable is unset or
    /// empty. Failing here rather than on the first step is deliberate: a missing
    /// credential is a configuration mistake, and reporting it at startup is what
    /// makes it diagnosable.
    pub fn from_env(vendor: Vendor, model: impl Into<String>) -> Result<Self, OpenAiError> {
        Self::new(OpenAiConfig::from_env(vendor, model)?)
    }

    /// Returns the configuration in use.
    #[must_use]
    pub const fn config(&self) -> &OpenAiConfig {
        &self.config
    }

    /// Returns the vendor this adapter talks to.
    #[must_use]
    pub const fn vendor(&self) -> Vendor {
        self.config.vendor()
    }

    /// The full endpoint a request is posted to.
    #[must_use]
    pub fn endpoint(&self) -> String {
        let base = self.config.base_url().trim_end_matches('/');
        // Postcondition: a usable base URL is absolute, so a misconfigured one fails
        // here rather than as a confusing transport error later.
        assert!(base.starts_with("http"), "a base URL is absolute");
        format!("{base}{CHAT_COMPLETIONS_PATH}")
    }

    /// Encodes a request as the JSON body the vendor expects.
    ///
    /// Exposed so tests and diagnostics can inspect the exact wire payload without
    /// performing a request.
    #[must_use]
    pub fn encode(&self, request: &ChatRequest) -> serde_json::Value {
        wire::build_request(&self.config, request)
    }
}

impl LlmPort for OpenAiLlm {
    fn model(&self) -> &str {
        self.config.model()
    }

    fn reasoning_effort(&self) -> Option<ReasoningEffort> {
        // Both vendors take the same scale on the wire now, so a chosen step is a fact
        // worth recording for either.
        Some(self.config.reasoning_effort())
    }

    fn effort_levels(&self, model: &str) -> &'static [ReasoningEffort] {
        self.config.vendor().effort_levels(model)
    }

    fn stream_chat(&self, request: ChatRequest) -> LlmStream {
        let payload = self.encode(&request);
        let Ok(body) = serde_json::to_string(&payload) else {
            return error_stream("could not encode the request as JSON");
        };
        tracing::debug!(
            vendor = %self.config.vendor(),
            model = %self.config.model(),
            endpoint = %self.endpoint(),
            messages = request.messages.len(),
            tools = request.tools.len(),
            "dispatching a chat completion"
        );

        let client = self.client.clone();
        let endpoint = self.endpoint();
        let api_key = self.config.api_key().to_owned();
        let vendor = self.config.vendor();
        // The host survives as an owned value inside the stream: the transport
        // failure is reported after `&self` has gone out of scope.
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
                Err(source) => Err(OpenAiError::transport(&source, &host).to_string()),
            }
        };

        // The stream resolves the request, then decodes the body it produced. A
        // transport failure becomes a single terminal `Error` event rather than a
        // stream that ends silently, so the agent loop always learns why.
        let stream = futures::stream::once(response).flat_map(move |outcome| match outcome {
            Ok(response) => {
                // The head is in hand, so the server is answering and everything
                // from here is its body. Reported before the body is read, because
                // this is the earliest moment the fact is true.
                let head = futures::stream::iter([LlmEvent::ResponseHead]);
                let announced: EventStream =
                    Box::pin(head.chain(decode(response, stream_host.clone(), vendor)));
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
/// `host` is the base URL the request was sent to, carried so that a failure part
/// way through the body names the same endpoint the request did.
fn decode(response: reqwest::Response, host: String, vendor: Vendor) -> EventStream {
    let status = response.status();
    if !status.is_success() {
        // The body carries the vendor's own message, which is the only useful thing
        // to show a user. It is excerpted so a large error page cannot flood the
        // transcript.
        let body = async move {
            let text = response
                .text()
                .await
                .unwrap_or_else(|error| format!("<{error}>"));
            nanus_ports::error_body_snippet(&text, BODY_SNIPPET_MAX)
        };
        let stream = futures::stream::once(body).map(move |body| {
            LlmEvent::Error(OpenAiError::status(vendor, status.as_u16(), body).to_string())
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
                // the socket to close means a stalled connection cannot hold a turn
                // open.
                return core::task::Poll::Ready(None);
            }
            match bytes.poll_next_unpin(cx) {
                core::task::Poll::Ready(Some(Ok(chunk))) => {
                    for line in decoder.push(&chunk) {
                        accumulator.observe_line(&line);
                    }
                    if decoder.is_done() {
                        // The sentinel means the server has sent everything, and the
                        // accumulator must be closed *here*: it is what emits the
                        // assembled tool calls and the usage report.
                        accumulator.close();
                        done = true;
                    }
                }
                core::task::Poll::Ready(Some(Err(error))) => {
                    accumulator.fail(OpenAiError::transport(&error, &host).to_string());
                    done = true;
                }
                core::task::Poll::Ready(None) => {
                    // A server that closes without a trailing newline still sent its
                    // last frame, so the decoder's tail is flushed before the
                    // accumulator is closed.
                    if let Some(tail) = decoder.finish() {
                        accumulator.observe_line(&tail);
                    }
                    accumulator.close();
                    done = true;
                }
                core::task::Poll::Pending => return core::task::Poll::Pending,
            }
        }
    });
    Box::pin(stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adapter(vendor: Vendor) -> Option<OpenAiLlm> {
        OpenAiLlm::new(OpenAiConfig::new(vendor, "test-model", "test-key")).ok()
    }

    #[test]
    fn the_endpoint_carries_the_vendors_version_prefix() {
        let Some(llm) = adapter(Vendor::OpenAi) else {
            return;
        };
        // OpenAI's documentation puts `/v1` in the base URL, unlike DeepSeek's.
        assert_eq!(llm.endpoint(), "https://api.openai.com/v1/chat/completions");

        let Some(zai) = adapter(Vendor::Zai) else {
            return;
        };
        assert_eq!(
            zai.endpoint(),
            "https://api.z.ai/api/paas/v4/chat/completions"
        );
    }

    #[test]
    fn a_trailing_slash_on_the_base_url_is_tolerated() {
        let config = OpenAiConfig::with_base_url(
            Vendor::Zai,
            "glm-4.5",
            "test-key",
            format!("{ZAI_CODING_BASE_URL}/"),
        );
        let Ok(llm) = OpenAiLlm::new(config) else {
            return;
        };
        // The coding plan is a host, so the path is the same one the API plan uses.
        assert_eq!(
            llm.endpoint(),
            "https://api.z.ai/api/coding/paas/v4/chat/completions"
        );
    }

    #[test]
    fn debug_never_renders_the_api_key() {
        let config = OpenAiConfig::new(Vendor::OpenAi, "gpt-5", "sk-super-secret-value");
        let Ok(llm) = OpenAiLlm::new(config) else {
            return;
        };
        let rendered = format!("{llm:?}");
        assert!(!rendered.contains("sk-super-secret-value"), "{rendered}");
        assert!(rendered.contains("gpt-5"), "{rendered}");
    }

    /// Both vendors name an effort on the wire now, so both report the one they would send.
    #[test]
    fn both_vendors_report_the_effort_they_send() {
        let Some(openai) = adapter(Vendor::OpenAi) else {
            return;
        };
        assert_eq!(openai.reasoning_effort(), Some(ReasoningEffort::Medium));
        let Some(zai) = adapter(Vendor::Zai) else {
            return;
        };
        assert_eq!(zai.reasoning_effort(), Some(ReasoningEffort::Medium));
    }

    #[test]
    fn a_missing_credential_is_a_typed_error() {
        // An empty key is rejected by validation rather than at the first request.
        let config = OpenAiConfig::new(Vendor::Zai, "glm-4.5", "");
        assert!(matches!(
            OpenAiConfig::validate(&config),
            Err(OpenAiError::MissingCredential { .. })
        ));
    }
}
