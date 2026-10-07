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
//! ## Adaptive thinking and signed replay
//!
//! Opus 5.5, Sonnet 5.5 and Fable 5.1 request adaptive thinking. Original ordered signed blocks, including
//! empty thinking, are retained beside the neutral assistant response and replayed only
//! with the unchanged system/tools/history prefix. A changed prefix strips old thinking
//! from the derived request; the durable log remains unchanged. Other encoders use the
//! neutral text/tool calls. Earlier model behavior is preserved.
//!
//! A managed-context request applies exactly the same per-turn check to the body it
//! prepares, and refuses a kept turn that only its signed blocks could express rather than
//! skipping it; the reasoning is in the `managed` module.
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
mod managed;
#[cfg(test)]
mod managed_tests;

mod response;
#[cfg(test)]
mod tool_support_tests;
mod wire;

pub use config::{
    API_KEY_ENV, API_VERSION, AnthropicConfig, DEFAULT_BASE_URL, MAX_OUTPUT_TOKENS, PROVIDER,
    effort_levels, model_max_output_tokens,
};
pub use error::AnthropicError;
pub use nanus_ports::ReasoningEffort;

use core::pin::Pin;
use futures::StreamExt as _;
use nanus_ports::{ChatRequest, LlmEvent, LlmPort, LlmStream};

/// The path appended to a configured base URL for a message request.
const MESSAGES_PATH: &str = "/messages";

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

    fn reasoning_effort(&self, model: &str) -> Option<ReasoningEffort> {
        // The API's own default is `high`, and only the models that take the parameter
        // report an effort at all: Haiku 4.5 and the previous generation answer with the
        // absence, which is the truth rather than a default. The id is the request's, not
        // the configured one, because a runner may switch models without rebuilding the
        // adapter — the same reason `effort_levels` takes it.
        if effort_levels(model).is_empty() {
            return None;
        }
        Some(if model == "claude-opus-5-5" {
            ReasoningEffort::Medium
        } else {
            ReasoningEffort::High
        })
    }

    fn effort_levels(&self, model: &str) -> &'static [ReasoningEffort] {
        effort_levels(model)
    }

    fn tool_call_support(
        &self,
        model: &str,
        request_effort: Option<ReasoningEffort>,
    ) -> nanus_ports::ToolCallSupport {
        if self.config.base_url().trim_end_matches('/') != DEFAULT_BASE_URL
            || !matches!(
                model,
                "claude-opus-5-5" | "claude-sonnet-5-5" | "claude-fable-5-1"
            )
        {
            return nanus_ports::ToolCallSupport::Unknown;
        }
        let effective = request_effort.or_else(|| self.reasoning_effort(model));
        if effective.is_some_and(|effort| effort_levels(model).contains(&effort)) {
            nanus_ports::ToolCallSupport::Supported
        } else {
            nanus_ports::ToolCallSupport::Unsupported
        }
    }

    fn capabilities(&self, model: &str) -> nanus_ports::ModelCapabilities {
        self.config.capabilities(model)
    }

    fn estimate_request(
        &self,
        request: &ChatRequest,
    ) -> nanus_ports::LlmResult<nanus_ports::RequestEstimate> {
        nanus_ports::tool_support::validate_input(
            self.tool_call_support(&request.model, request.reasoning_effort),
            request,
        )?;
        nanus_ports::capabilities::estimate_payload(
            self.capabilities(&request.model),
            request,
            &self.encode(request),
        )
    }

    fn stream_chat(&self, request: ChatRequest) -> LlmStream {
        if let Err(error) = nanus_ports::capabilities::validate_image_input(
            self.capabilities(&request.model),
            &request,
        ) {
            return error_stream(&error.to_string());
        }
        if let Err(error) = nanus_ports::tool_support::validate_input(
            self.tool_call_support(&request.model, request.reasoning_effort),
            &request,
        ) {
            return error_stream(&error.to_string());
        }
        if request.context_budget.is_some()
            || nanus_ports::capabilities::has_images(&request.messages)
        {
            let checked = self.estimate_request(&request).and_then(|estimate| {
                nanus_ports::capabilities::validate_estimate(
                    self.capabilities(&request.model),
                    &request,
                    estimate,
                )
            });
            if let Err(error) = checked {
                return error_stream(&error.to_string());
            }
        }
        let payload = self.encode(&request);

        let prefix_digest = wire::request_prefix(&payload);
        let Ok(body) = serde_json::to_string(&payload) else {
            return error_stream("could not encode the request as JSON");
        };
        tracing::debug!(
            model = %request.model,
            endpoint = %self.endpoint(),
            messages = request.messages.len(),
            tools = request.tools.len(),
            "dispatching a message request"
        );

        let accumulator = wire::StreamAccumulator::with_prefix(prefix_digest);
        send(self.transport(), body, accumulator)
    }

    fn managed_support(&self, model: &str) -> nanus_ports::ManagedSupport {
        managed::support(&self.config, model)
    }

    fn prepare_managed(
        &self,
        request: nanus_ports::ManagedRequest,
    ) -> nanus_ports::LlmResult<Box<dyn nanus_ports::PreparedModelCall>> {
        Ok(Box::new(managed::prepare(self, request)?))
    }
}

impl AnthropicLlm {
    /// Everything a dispatch needs beyond its body, resolved now and owned from here on.
    fn transport(&self) -> Transport {
        Transport {
            client: self.client.clone(),
            endpoint: self.endpoint(),
            api_key: self.config.api_key().to_owned(),
            // The host survives as an owned value inside the stream: the transport failure is
            // reported after `&self` has gone out of scope.
            host: self.config.base_url().to_owned(),
            limits: self.config.response_limits(),
        }
    }
}

/// The resolved route and credential of one dispatch.
///
/// Built once, before the body is sent, so a prepared call cannot change where it goes or
/// which key it carries between the moment it is admitted and the moment it is sent.
#[derive(Clone)]
struct Transport {
    client: reqwest::Client,
    endpoint: String,
    api_key: String,
    host: String,
    limits: Option<nanus_ports::ResponseLimits>,
}

/// Sends exactly `body` over `transport` and decodes the answer with `accumulator`.
///
/// Nothing touches the network until the returned stream is polled: the request is built
/// inside the future the stream resolves first.
fn send(transport: Transport, body: String, accumulator: wire::StreamAccumulator) -> LlmStream {
    let Transport {
        client,
        endpoint,
        api_key,
        host,
        limits,
    } = transport;
    let stream_host = host.clone();
    let response = async move {
        let sent = client
            .post(&endpoint)
            // The credential is a header of its own rather than a bearer token, and the
            // version is declared on every request: there is no "latest", which is what makes
            // this client's expectations explicit.
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

    let mut accumulator = Some(accumulator);
    let stream = futures::stream::once(response).flat_map(move |outcome| match outcome {
        Ok(response) => {
            let Some(accumulator) = accumulator.take() else {
                return error_stream_owned("response decoder was already consumed".into());
            };
            let head = futures::stream::iter([LlmEvent::ResponseHead]);
            let announced: EventStream = Box::pin(head.chain(response::decode(
                response,
                stream_host.clone(),
                accumulator,
                limits,
            )));
            announced
        }
        Err(message) => error_stream_owned(message),
    });
    Box::pin(stream)
}

/// A one-event stream carrying an error.
fn error_stream(message: &str) -> LlmStream {
    error_stream_owned(message.to_owned())
}

/// A one-event stream carrying an error message.
fn error_stream_owned(message: String) -> LlmStream {
    Box::pin(futures::stream::iter(vec![LlmEvent::Error(message)]))
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

    /// Which models take an effort parameter is a fact about the id, not about the adapter's
    /// configuration: a 5-series id reports the API default and a previous-generation id reports
    /// the absence, whatever the adapter was built for.
    #[test]
    fn only_the_models_that_take_an_effort_report_one() {
        let Some(legacy) = adapter() else {
            return;
        };
        // The adapter is built for a previous-generation model, and still answers for the model it
        // is asked about — which is what makes a switch truthful without a rebuild.
        assert_eq!(legacy.reasoning_effort("claude-sonnet-4-20250514"), None);
        assert_eq!(
            legacy.reasoning_effort("claude-sonnet-5"),
            Some(ReasoningEffort::High)
        );
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
