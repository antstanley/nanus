//! # nanus-adapter-anthropic
//!
//! Anthropic's Messages API: request encoding, event-typed streaming, and tool use.
//!
//! ## Why this is its own crate rather than another vendor
//!
//! Anthropic's API is not chat-completions with different names. A system turn is a
//! top-level field rather than a role; a tool result is a *user* turn carrying a
//! `tool_result` block rather than a `tool` role; a tool call's arguments are a JSON
//! object rather than a JSON string; the credential travels in `x-api-key` and every
//! request must declare an API version; and the stream is event-typed with a
//! `message_stop` rather than a `[DONE]` sentinel. Those are the shapes this crate
//! translates, and they belong beside each other rather than inside an
//! `OpenAI`-compatible encoder that would need a branch per line.
//!
//! ## What is deliberately not requested
//!
//! **Extended thinking.** Anthropic expresses it as a token budget, and a
//! tool-using turn requires the *signed* thinking blocks of the previous turn to be
//! replayed. A signature is not part of the message vocabulary `nanus-domain` owns,
//! so asking for thinking would produce a conversation that cannot be continued —
//! the second request of a tool loop would be refused. So no `thinking` field is
//! ever sent.
//!
//! That is a separate knob from **effort**, which this adapter does send: the
//! 5-series models take `output_config.effort` (a behavioral signal) rather than the
//! thinking budget, and it needs no signed block to be replayed. A model that takes
//! no effort parameter is sent nothing, and [`LlmPort::reasoning_effort`] reports the
//! absence for it rather than a plausible value.
//!
//! ## What the adapter owns
//!
//! HTTP and stream decoding, exactly as the other adapters do. Everything it
//! produces is [`nanus_ports`] vocabulary.

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
    API_KEY_ENV, API_VERSION, AnthropicConfig, DEFAULT_BASE_URL, MAX_OUTPUT_TOKENS, PROVIDER,
    effort_levels,
};
pub use error::AnthropicError;
pub use nanus_ports::ReasoningEffort;

use core::pin::Pin;
use futures::StreamExt as _;
use nanus_ports::{ChatRequest, LlmEvent, LlmPort, LlmStream};

/// The path appended to a configured base URL for a message request.
const MESSAGES_PATH: &str = "/messages";

/// Maximum length of an error body echoed back to the caller.
const BODY_SNIPPET_MAX: usize = 2_000;

/// A stream of model events.
type EventStream = Pin<Box<dyn futures::Stream<Item = LlmEvent> + 'static>>;

/// An Anthropic Messages client.
///
/// The client holds one `reqwest::Client` so connections are pooled across steps,
/// which matters for an agent loop that issues a request per step.
pub struct AnthropicLlm {
    client: reqwest::Client,
    config: AnthropicConfig,
}

impl core::fmt::Debug for AnthropicLlm {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // The API key is deliberately absent: a `Debug` rendering reaches logs.
        f.debug_struct("AnthropicLlm")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl AnthropicLlm {
    /// Builds an adapter for `config`.
    ///
    /// # Errors
    ///
    /// Returns [`AnthropicError::Client`] when the underlying HTTP client cannot be
    /// constructed, which happens only when the platform TLS stack is unavailable.
    pub fn new(config: AnthropicConfig) -> Result<Self, AnthropicError> {
        let client = reqwest::Client::builder()
            .build()
            .map_err(|source| AnthropicError::client(&source))?;
        Ok(Self { client, config })
    }

    /// Builds an adapter for `model`, reading the key from the environment.
    ///
    /// # Errors
    ///
    /// Returns [`AnthropicError::MissingCredential`] when `ANTHROPIC_API_KEY` is
    /// unset or empty.
    pub fn from_env(model: impl Into<String>) -> Result<Self, AnthropicError> {
        Self::new(AnthropicConfig::from_env(model)?)
    }

    /// Returns the configuration in use.
    #[must_use]
    pub const fn config(&self) -> &AnthropicConfig {
        &self.config
    }

    /// The full endpoint a request is posted to.
    #[must_use]
    pub fn endpoint(&self) -> String {
        let base = self.config.base_url().trim_end_matches('/');
        // Postcondition: a usable base URL is absolute, so a misconfigured one fails
        // here rather than as a confusing transport error later.
        assert!(base.starts_with("http"), "a base URL is absolute");
        format!("{base}{MESSAGES_PATH}")
    }

    /// Encodes a request as the JSON body the API expects.
    ///
    /// Exposed so tests and diagnostics can inspect the exact wire payload without
    /// performing a request.
    #[must_use]
    pub fn encode(&self, request: &ChatRequest) -> serde_json::Value {
        wire::build_request(&self.config, request)
    }
}

impl LlmPort for AnthropicLlm {
    fn model(&self) -> &str {
        self.config.model()
    }

    fn reasoning_effort(&self) -> Option<ReasoningEffort> {
        // The API's own default is `high`, and only the models that take the parameter
        // report an effort at all: Haiku 4.5 and the previous generation answer with the
        // absence, which is the truth rather than a default.
        if effort_levels(self.config.model()).is_empty() {
            return None;
        }
        Some(ReasoningEffort::High)
    }

    fn effort_levels(&self, model: &str) -> &'static [ReasoningEffort] {
        effort_levels(model)
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
            "dispatching a message request"
        );

        let client = self.client.clone();
        let endpoint = self.endpoint();
        let api_key = self.config.api_key().to_owned();
        // The host survives as an owned value inside the stream: the transport
        // failure is reported after `&self` has gone out of scope.
        let host = self.config.base_url().to_owned();
        let stream_host = host.clone();

        let response = async move {
            let sent = client
                .post(&endpoint)
                // The credential is a header of its own rather than a bearer token,
                // and the version is declared on every request: there is no
                // "latest", which is what makes this client's expectations explicit.
                .header("x-api-key", api_key)
                .header("anthropic-version", API_VERSION)
                .header("accept", "text/event-stream")
                .header("content-type", "application/json")
                .body(body)
                .send()
                .await;
            match sent {
                Ok(response) => Ok(response),
                Err(source) => Err(AnthropicError::transport(&source, &host).to_string()),
            }
        };

        let stream = futures::stream::once(response).flat_map(move |outcome| match outcome {
            Ok(response) => {
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
/// `host` is the base URL the request was sent to, carried so that a failure part
/// way through the body names the same endpoint the request did.
fn decode(response: reqwest::Response, host: String) -> EventStream {
    let status = response.status();
    if !status.is_success() {
        // The body carries Anthropic's own message, which is the only useful thing
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
            LlmEvent::Error(AnthropicError::status(status.as_u16(), body).to_string())
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
                    // There is no sentinel in this protocol: the stream is over when
                    // `message_stop` has been seen, which is what closes the
                    // accumulator.
                    if accumulator.is_closed() {
                        done = true;
                    }
                }
                core::task::Poll::Ready(Some(Err(error))) => {
                    accumulator.fail(AnthropicError::transport(&error, &host).to_string());
                    done = true;
                }
                core::task::Poll::Ready(None) => {
                    // A server that closes without a trailing newline still sent its
                    // last frame, so the decoder's tail is flushed before the
                    // accumulator is closed.
                    if let Some(tail) = decoder.finish() {
                        accumulator.observe_line(&tail);
                    }
                    // A stream that ended without `message_stop` is still usable:
                    // whatever arrived is real, so it is emitted rather than
                    // discarded.
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

    fn adapter() -> Option<AnthropicLlm> {
        AnthropicLlm::new(AnthropicConfig::new("claude-sonnet-4-20250514", "test-key")).ok()
    }

    #[test]
    fn the_endpoint_is_the_messages_path() {
        let Some(llm) = adapter() else {
            return;
        };
        assert_eq!(llm.endpoint(), "https://api.anthropic.com/v1/messages");
        // The version prefix is part of the host, not of the credential.
        assert!(llm.endpoint().ends_with("/v1/messages"));
    }

    #[test]
    fn a_trailing_slash_on_the_base_url_is_tolerated() {
        let config = AnthropicConfig::with_base_url(
            "claude-sonnet-4-20250514",
            "test-key",
            "https://proxy.test/",
        );
        let Ok(llm) = AnthropicLlm::new(config) else {
            return;
        };
        assert_eq!(llm.endpoint(), "https://proxy.test/messages");
    }

    #[test]
    fn debug_never_renders_the_api_key() {
        let config = AnthropicConfig::new("claude-sonnet-4-20250514", "sk-ant-super-secret-value");
        let Ok(llm) = AnthropicLlm::new(config) else {
            return;
        };
        let rendered = format!("{llm:?}");
        assert!(
            !rendered.contains("sk-ant-super-secret-value"),
            "{rendered}"
        );
        assert!(rendered.contains("claude-sonnet-4"), "{rendered}");
    }

    /// A model that takes no effort parameter reports the absence; a 5-series model reports the
    /// API default it applies.
    #[test]
    fn only_the_models_that_take_an_effort_report_one() {
        let Some(llm) = adapter() else {
            return;
        };
        // The test adapter is built for a previous-generation model.
        assert_eq!(llm.reasoning_effort(), None);
        let Some(modern) =
            AnthropicLlm::new(AnthropicConfig::new("claude-sonnet-5", "test-key")).ok()
        else {
            return;
        };
        assert_eq!(modern.reasoning_effort(), Some(ReasoningEffort::High));
        assert_eq!(API_VERSION, "2023-06-01");
    }

    #[test]
    fn a_missing_credential_is_a_typed_error() {
        let config = AnthropicConfig::new("claude-sonnet-4-20250514", "");
        assert!(matches!(
            AnthropicConfig::validate(&config),
            Err(AnthropicError::MissingCredential { .. })
        ));
    }
}
