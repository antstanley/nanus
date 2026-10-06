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
mod function_policy;
mod managed;
#[cfg(test)]
mod managed_tests;
pub mod oauth;
mod response;
#[cfg(test)]
mod stateless_tests;
mod tool_support;

pub mod responses;
#[cfg(test)]
mod tool_support_tests;
mod wire;
mod zai;
#[cfg(test)]
mod zai_tests;

pub use config::{
    OPENAI_API_KEY_ENV, OPENAI_BASE_URL, OPENAI_SUBSCRIPTION_BASE_URL, OpenAiConfig, Protocol,
    ProtocolPreference, Vendor, ZAI_API_KEY_ENV, ZAI_BASE_URL, ZAI_CODING_BASE_URL,
    effort_spelling, openai_effort_levels, zai_effort_levels,
};
pub use error::OpenAiError;
pub use nanus_ports::ReasoningEffort;

use core::pin::Pin;
use futures::StreamExt as _;
use nanus_ports::{ChatRequest, LlmEvent, LlmPort, LlmStream};

/// A stream of model events.
type EventStream = Pin<Box<dyn futures::Stream<Item = LlmEvent> + 'static>>;

/// The per-protocol half of decoding: how a body's events become model events.
///
/// An enum rather than a trait object, because the decode loop is monomorphic and the two arms are
/// known: each protocol's accumulator owns the part that differs, and the loop only moves bytes and
/// flushes.
enum Decoder {
    /// `chat/completions`: `choices` deltas.
    Chat(Box<wire::StreamAccumulator>),
    /// `responses`: named events.
    Responses(Box<responses::StreamAccumulator>),
}

impl Decoder {
    fn set_response_limits(&mut self, limits: Option<nanus_ports::ResponseLimits>) {
        match self {
            Self::Chat(accumulator) => accumulator.set_response_limits(limits),
            Self::Responses(accumulator) => accumulator.set_response_limits(limits),
        }
    }
    fn is_closed(&self) -> bool {
        match self {
            Self::Chat(accumulator) => accumulator.is_closed(),
            Self::Responses(accumulator) => accumulator.is_closed(),
        }
    }
    fn terminal_received(&self, sentinel: bool) -> bool {
        match self {
            Self::Chat(accumulator) => sentinel || accumulator.is_closed(),
            Self::Responses(accumulator) => accumulator.terminal_received(),
        }
    }
    fn take_ready(&mut self) -> Option<LlmEvent> {
        match self {
            Self::Chat(accumulator) => accumulator.take_ready(),
            Self::Responses(accumulator) => accumulator.take_ready(),
        }
    }

    fn fail(&mut self, message: String) {
        match self {
            Self::Chat(accumulator) => accumulator.fail(message),
            Self::Responses(accumulator) => accumulator.fail(message),
        }
    }

    fn observe_line(&mut self, payload: &str) {
        match self {
            Self::Chat(accumulator) => accumulator.observe_line(payload),
            Self::Responses(accumulator) => accumulator.observe_line(payload),
        }
    }

    fn close(&mut self) {
        match self {
            Self::Chat(accumulator) => accumulator.close(),
            Self::Responses(accumulator) => accumulator.close(),
        }
    }
}

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
    /// constructed, or `UnsupportedProtocol` when an exact policy conflicts with its endpoint.
    pub fn new(config: OpenAiConfig) -> Result<Self, OpenAiError> {
        config
            .resolve_protocol(config.model())
            .map_err(|_| OpenAiError::UnsupportedProtocol)?;
        if config.stateless_responses()
            && (config.vendor() != Vendor::OpenAi
                || config.base_url().trim_end_matches('/') != OPENAI_BASE_URL
                || config.account_id().is_some()
                || config.response_limits().is_none()
                || config.protocol_preference() != ProtocolPreference::Exact(Protocol::Responses))
        {
            return Err(OpenAiError::UnsupportedProtocol);
        }
        let builder = reqwest::Client::builder();
        // A receipt names one exact endpoint. Redirects cannot silently select a different one.
        let builder = if config.stateless_responses() {
            builder.redirect(reqwest::redirect::Policy::none())
        } else {
            builder
        };
        let client = builder
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

    /// The full endpoint a request for the configured model is posted to.
    #[must_use]
    pub fn endpoint(&self) -> String {
        self.endpoint_for(self.config.model())
    }

    /// The full endpoint a request for `model` is posted to.
    ///
    /// The caller's exact preference wins; otherwise the model and fallback select the wire.
    /// This diagnostic accessor does not validate endpoint compatibility.
    #[must_use]
    pub fn endpoint_for(&self, model: &str) -> String {
        self.url(self.config.protocol_for(model))
    }

    /// The wire a request takes: its model's.
    fn protocol_of(&self, request: &ChatRequest) -> Protocol {
        self.config.protocol_for(&request.model)
    }

    fn checked_protocol(&self, request: &ChatRequest) -> nanus_ports::LlmResult<Protocol> {
        let protocol = self.config.resolve_protocol(&request.model)?;
        function_policy::validate(&self.config, request)?;
        if matches!(
            self.config.protocol_preference(),
            ProtocolPreference::Exact(_)
        ) && protocol == Protocol::Responses
            && !self.config.sends_output_ceiling()
            && request.max_tokens.is_some()
        {
            return Err(nanus_ports::LlmError::Unsupported {
                feature: "this exact endpoint cannot honor an explicit output ceiling".into(),
            });
        }
        Ok(protocol)
    }

    fn url(&self, protocol: Protocol) -> String {
        let base = self.config.base_url().trim_end_matches('/');
        // Postcondition: a usable base URL is absolute, so a misconfigured one fails
        // here rather than as a confusing transport error later.
        assert!(base.starts_with("http"), "a base URL is absolute");
        format!("{base}{}", protocol.path())
    }

    /// Prepares a bounded, stateless public Responses body with original replay items.
    ///
    /// This pure inspection API performs no HTTP; stock dispatch keeps its existing encoder.
    /// The adapter supplies its own exact-model capabilities and tool support evidence.
    ///
    /// # Errors
    ///
    /// Refuses incompatible endpoints, unknown models, changed controls, incomplete history,
    /// legacy assistant turns and requests outside the explicit byte or token ceilings.
    pub fn prepare_responses(
        &self,
        request: &ChatRequest,
    ) -> nanus_ports::LlmResult<responses::PreparedResponses> {
        self.checked_protocol(request)?;
        responses::prepare_request(
            &self.config,
            request,
            self.capabilities(&request.model),
            self.tool_call_support(&request.model, request.reasoning_effort),
        )
    }

    /// Estimates original Responses items, allowing only final complete-batch result substitution.
    ///
    /// Pure estimation never dispatches the candidate or mutates its immutable source.
    /// # Errors
    /// Refuses invalid history/controls, substitutions outside the final batch, or exceeded limits.
    pub fn estimate_responses(
        &self,
        request: &ChatRequest,
    ) -> nanus_ports::LlmResult<nanus_ports::RequestEstimate> {
        self.checked_protocol(request)?;
        responses::estimate_request(
            &self.config,
            request,
            self.capabilities(&request.model),
            self.tool_call_support(&request.model, request.reasoning_effort),
        )
    }

    /// Encodes a request as the JSON body the vendor expects.
    ///
    /// Raw encoding for wire inspection, without capability or endpoint validation.
    /// `estimate_request` and `stream_chat` validate before estimating or dispatching.
    #[must_use]
    pub fn encode(&self, request: &ChatRequest) -> serde_json::Value {
        match self.protocol_of(request) {
            Protocol::ChatCompletions => wire::build_request(&self.config, request),
            Protocol::Responses => responses::build_request(&self.config, request),
        }
    }
}

impl OpenAiLlm {
    fn prepare_dispatch(
        &self,
        request: &ChatRequest,
    ) -> nanus_ports::LlmResult<(Protocol, serde_json::Value, Decoder)> {
        let protocol = self.checked_protocol(request)?;
        if self.config.stateless_responses() {
            let prepared = self.prepare_responses(request)?;
            let limits = self.config.response_limits().ok_or_else(|| {
                nanus_ports::LlmError::Unsupported {
                    feature: "stateless Responses requires response limits".into(),
                }
            })?;
            let decoder = responses::StreamAccumulator::with_context(
                prepared.prefix_digest,
                prepared.context_receipt,
                limits,
            )?;
            return Ok((
                protocol,
                prepared.body,
                Decoder::Responses(Box::new(decoder)),
            ));
        }
        nanus_ports::capabilities::validate_image_input(
            self.capabilities(&request.model),
            request,
        )?;
        nanus_ports::tool_support::validate_input(
            self.tool_call_support(&request.model, request.reasoning_effort),
            request,
        )?;
        if zai::requires_preflight(&self.config, request) {
            let checked = self.estimate_request(request).and_then(|estimate| {
                nanus_ports::capabilities::validate_estimate(
                    self.capabilities(&request.model),
                    request,
                    estimate,
                )
            });
            checked?;
        }
        let decoder = match protocol {
            Protocol::ChatCompletions => Decoder::Chat(Box::default()),
            Protocol::Responses => Decoder::Responses(Box::default()),
        };
        Ok((protocol, self.encode(request), decoder))
    }

    fn transmit(
        &self,
        request: &ChatRequest,
        payload: &serde_json::Value,
        decoder: Decoder,
        endpoint: &str,
    ) -> LlmStream {
        let Ok(body) = serde_json::to_string(payload) else {
            return error_stream("could not encode the request as JSON");
        };
        tracing::debug!(vendor=%self.config.vendor(),model=%request.model,endpoint=%endpoint,
            messages=request.messages.len(),tools=request.tools.len(),"dispatching a chat completion");
        send(self.transport(endpoint), body, decoder)
    }

    /// Everything a dispatch needs beyond its body, resolved now and owned from here on.
    fn transport(&self, endpoint: &str) -> Transport {
        Transport {
            client: self.client.clone(),
            endpoint: endpoint.to_owned(),
            api_key: self.config.api_key().to_owned(),
            account_id: self.config.account_id().map(str::to_owned),
            host: self.config.base_url().to_owned(),
            vendor: self.config.vendor(),
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
    account_id: Option<String>,
    host: String,
    vendor: Vendor,
    limits: Option<nanus_ports::ResponseLimits>,
}

/// Sends exactly `body` over `transport` and decodes the answer with `decoder`.
///
/// Building the request performs no I/O; nothing touches the network until the returned stream
/// is polled.
fn send(transport: Transport, body: String, decoder: Decoder) -> LlmStream {
    let Transport {
        client,
        endpoint,
        api_key,
        account_id,
        host,
        vendor,
        limits,
    } = transport;
    let mut sent = client
        .post(&endpoint)
        .header("accept", "text/event-stream")
        .header("content-type", "application/json")
        .bearer_auth(api_key)
        .body(body);
    if let Some(account) = account_id {
        sent = sent.header("chatgpt-account-id", account);
    }
    let transport_host = host.clone();
    let response = async move {
        sent.send()
            .await
            .map_err(|source| OpenAiError::transport(&source, &transport_host).to_string())
    };
    let mut decoder = Some(decoder);
    Box::pin(
        futures::stream::once(response).flat_map(move |outcome| match outcome {
            Ok(response) => {
                let Some(decoder) = decoder.take() else {
                    return error_stream_owned("response decoder was already consumed".into());
                };
                let head = futures::stream::iter([LlmEvent::ResponseHead]);
                let announced: EventStream = Box::pin(head.chain(response::decode(
                    response,
                    host.clone(),
                    vendor,
                    decoder,
                    limits,
                )));
                announced
            }
            Err(message) => error_stream_owned(message),
        }),
    )
}

impl LlmPort for OpenAiLlm {
    fn model(&self) -> &str {
        self.config.model()
    }

    fn reasoning_effort(&self, _model: &str) -> Option<ReasoningEffort> {
        // Record the captured default. Admission separately checks the request model and endpoint.
        Some(self.config.reasoning_effort())
    }

    fn effort_levels(&self, model: &str) -> &'static [ReasoningEffort] {
        zai::efforts(&self.config, model)
            .unwrap_or_else(|| self.config.vendor().effort_levels(model))
    }

    fn tool_call_support(
        &self,
        model: &str,
        request_effort: Option<ReasoningEffort>,
    ) -> nanus_ports::ToolCallSupport {
        tool_support::support(&self.config, model, request_effort)
    }

    fn capabilities(&self, model: &str) -> nanus_ports::ModelCapabilities {
        if self.config.vendor() == Vendor::Zai {
            return zai::capabilities(&self.config, model);
        }
        if !self.config.has_verified_responses_endpoint()
            || !matches!(self.config.resolve_protocol(model), Ok(Protocol::Responses))
        {
            return nanus_ports::ModelCapabilities::default();
        }
        let Some(profile) = nanus_ports::capabilities::ImageProfile::for_openai_model(model) else {
            return nanus_ports::ModelCapabilities::default();
        };
        // Evidence belongs to an exact model on these Responses endpoints, never Chat or a proxy.
        nanus_ports::ModelCapabilities {
            image_input: nanus_ports::ImageInputSupport::Supported,
            image_profile: Some(profile),
            context_window_tokens: Some(1_050_000),
            max_input_tokens: Some(if model == "gpt-6-astra" {
                1_050_000
            } else {
                922_000
            }),
            max_output_tokens: Some(128_000),
        }
    }

    fn estimate_request(
        &self,
        request: &ChatRequest,
    ) -> nanus_ports::LlmResult<nanus_ports::RequestEstimate> {
        if self.config.stateless_responses() {
            return self.estimate_responses(request);
        }
        self.checked_protocol(request)?;
        zai::validate(&self.config, request)?;
        let capabilities = self.capabilities(&request.model);
        nanus_ports::capabilities::validate_image_input(capabilities, request)?;
        nanus_ports::tool_support::validate_input(
            self.tool_call_support(&request.model, request.reasoning_effort),
            request,
        )?;
        nanus_ports::capabilities::estimate_payload(capabilities, request, &self.encode(request))
    }

    fn stream_chat(&self, request: ChatRequest) -> LlmStream {
        match self.prepare_dispatch(&request) {
            Ok((protocol, payload, decoder)) => {
                self.transmit(&request, &payload, decoder, &self.url(protocol))
            }
            Err(error) => error_stream(&error.to_string()),
        }
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

    /// A switch between models changes the endpoint and the body with it, because the adapter was
    /// built once and the request names the model.
    #[test]
    fn a_newer_model_is_posted_to_responses_and_an_older_one_to_chat_completions() {
        let Some(llm) = adapter(Vendor::OpenAi) else {
            return;
        };
        assert_eq!(
            llm.endpoint_for("gpt-6.1-sol"),
            "https://api.openai.com/v1/responses"
        );
        assert_eq!(
            llm.endpoint_for("gpt-5"),
            "https://api.openai.com/v1/chat/completions"
        );
        let newer = llm.encode(&ChatRequest::new(
            "gpt-6.1-sol",
            vec![nanus_domain::Message::user("hi")],
        ));
        assert!(newer.get("input").is_some() && newer.get("messages").is_none());
        let older = llm.encode(&ChatRequest::new(
            "gpt-5",
            vec![nanus_domain::Message::user("hi")],
        ));
        assert!(older.get("messages").is_some() && older.get("input").is_none());
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

    /// Both vendors name an effort on the wire now, and every model they offer takes it, so the
    /// answer is the configured default whatever id a switch names.
    #[test]
    fn both_vendors_report_the_effort_they_send() {
        let Some(openai) = adapter(Vendor::OpenAi) else {
            return;
        };
        assert_eq!(
            openai.reasoning_effort("gpt-5.6-sol"),
            Some(ReasoningEffort::Medium)
        );
        let Some(zai) = adapter(Vendor::Zai) else {
            return;
        };
        assert_eq!(
            zai.reasoning_effort("glm-5.2"),
            Some(ReasoningEffort::Medium)
        );
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
