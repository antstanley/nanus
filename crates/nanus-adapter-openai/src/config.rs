//! Adapter configuration: which compatible vendor, which endpoint, and which knobs.
//!
//! Two providers share this adapter because they share a protocol: `OpenAI`'s
//! `chat/completions` and z.ai's GLM endpoint both take the same request body and
//! stream the same frames. What differs is the *vendor* — the host, the credential
//! variable, the model ids, the output ceiling, and whether the reasoning knob is
//! a scale or a mode — so that is what [`Vendor`] names, and the rest of the crate
//! reads it rather than branching on a provider string.

use nanus_ports::ReasoningEffort;

use crate::error::OpenAiError;

/// `OpenAI`'s API host, with the version prefix its documentation requires.
pub const OPENAI_BASE_URL: &str = "https://api.openai.com/v1";

/// z.ai's pay-as-you-go API host.
pub const ZAI_BASE_URL: &str = "https://api.z.ai/api/paas/v4";

/// z.ai's coding-plan host.
///
/// The subscription plans are a different *endpoint* for the same key and the same
/// protocol rather than a different product, which is why a plan is a base URL here
/// and needs no adapter of its own: a coding-plan key is used exactly like an API
/// key, against this host.
pub const ZAI_CODING_BASE_URL: &str = "https://api.z.ai/api/coding/paas/v4";

/// The credential variable `OpenAI`'s own tooling reads.
pub const OPENAI_API_KEY_ENV: &str = "OPENAI_API_KEY";

/// The credential variable z.ai's documentation uses.
pub const ZAI_API_KEY_ENV: &str = "ZAI_API_KEY";

/// The models offered for `OpenAI`, in cycling order.
///
/// The reasoning models are the ones offered because they are the ones that accept
/// the `reasoning_effort` control this adapter sends, and because a harness that
/// reports thinking wants a model that produces it. A non-reasoning id set by hand
/// still works, but the provider refuses the effort field — which is why the
/// offered list is the reasoning family.
///
/// The coding model is in the list because the composition's `coding` plan resolves to
/// it: a plan whose default model the agent does not offer would put a client on a model
/// it could cycle away from and never back to, and `SetModel` would refuse the id the
/// session was already running.
///
/// The current flagships come first, so the fallback default is one of them, and the
/// previous generation stays behind them rather than being dropped: a session resumed
/// against `gpt-5` can still cycle back to it, and removing an offered id would strand
/// exactly the runs that predate the update.
static OPENAI_MODELS: [&str; 8] = [
    "gpt-6-astra",
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-5.6-luna",
    "gpt-5.3-codex",
    "gpt-5",
    "gpt-5-mini",
    "gpt-5-codex",
];

/// The models offered for z.ai, in cycling order.
///
/// The default is the first, which is the same rule the composition's plan follows: z.ai's
/// plans resolve to `glm-5.3-flashx`, and a plan whose default model the agent did not offer
/// would put a client on a model it could cycle away from and never back to.
static ZAI_MODELS: [&str; 4] = ["glm-5.3-flashx", "glm-5.3-flash", "glm-5.3", "glm-5.2"];

/// One `OpenAI`-compatible vendor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Vendor {
    /// `OpenAI`'s own API.
    OpenAi,
    /// z.ai's GLM API, pay-as-you-go and coding plans alike.
    Zai,
}

impl Vendor {
    /// Returns the name the harness knows this provider by.
    ///
    /// The same word is the secret-store account, the configuration's `provider`
    /// value, and the `nanus auth set` argument, so one spelling names a provider
    /// everywhere.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Zai => "zai",
        }
    }

    /// Returns the vendor a provider name names, when it names one.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "openai" => Some(Self::OpenAi),
            "zai" | "z.ai" => Some(Self::Zai),
            _ => None,
        }
    }

    /// Returns the environment variable this vendor's key is read from.
    #[must_use]
    pub const fn env_var(self) -> &'static str {
        match self {
            Self::OpenAi => OPENAI_API_KEY_ENV,
            Self::Zai => ZAI_API_KEY_ENV,
        }
    }

    /// Returns the default base URL.
    #[must_use]
    pub const fn default_base_url(self) -> &'static str {
        match self {
            Self::OpenAi => OPENAI_BASE_URL,
            Self::Zai => ZAI_BASE_URL,
        }
    }

    /// Returns the models offered for this vendor, in cycling order.
    #[must_use]
    pub const fn models(self) -> &'static [&'static str] {
        match self {
            Self::OpenAi => &OPENAI_MODELS,
            Self::Zai => &ZAI_MODELS,
        }
    }

    /// Returns the field a request names its output ceiling with.
    ///
    /// The two vendors spell this differently and neither accepts the other's: `OpenAI`'s
    /// reasoning models require `max_completion_tokens` and reject `max_tokens`, while
    /// z.ai's GLM models use the original `max_tokens`. Sending the wrong one is a
    /// refused request rather than a truncated answer, so the field is a vendor fact in
    /// the same sense the endpoint and the credential are.
    #[must_use]
    pub const fn output_token_field(self) -> &'static str {
        match self {
            Self::OpenAi => "max_completion_tokens",
            Self::Zai => "max_tokens",
        }
    }

    /// Returns the effort steps this vendor acts on for `model`, in increasing order.
    ///
    /// Which steps a model takes is the provider's fact and it differs between them: the
    /// `gpt-5.6` family takes `none` through `max` while `gpt-5.3-codex` has no `none`, and
    /// z.ai's GLM-5.3 models cannot turn thinking off at all. A chooser that offered the whole
    /// scale would offer steps the provider refuses, so the interface draws this list.
    #[must_use]
    pub fn effort_levels(self, model: &str) -> &'static [ReasoningEffort] {
        match self {
            Self::OpenAi => openai_effort_levels(model),
            Self::Zai => zai_effort_levels(model),
        }
    }

    /// Returns the provider's documented maximum output tokens.
    ///
    /// A request may not ask for more than this: the provider refuses the request
    /// rather than truncating the answer, so [`crate::OpenAiConfig`] caps the
    /// configured budget at it and says when it did. The number is a provider fact
    /// and therefore lives here rather than in the ports crate, exactly as the
    /// output ceiling does for `DeepSeek`.
    #[must_use]
    pub const fn max_output_tokens(self) -> u32 {
        match self {
            Self::OpenAi => 128_000,
            Self::Zai => 98_304,
        }
    }
}

impl core::fmt::Display for Vendor {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The adapter's settings.
///
/// `Debug` is implemented by hand so the API key cannot leak into a log line; this
/// is the type that reaches `tracing` output, so it is the place to enforce that.
#[derive(Clone)]
pub struct OpenAiConfig {
    vendor: Vendor,
    model: String,
    api_key: String,
    base_url: String,
    max_tokens: u32,
    reasoning_effort: ReasoningEffort,
    temperature: Option<f32>,
}

impl core::fmt::Debug for OpenAiConfig {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("OpenAiConfig")
            .field("vendor", &self.vendor)
            .field("model", &self.model)
            .field("api_key", &"<redacted>")
            .field("base_url", &self.base_url)
            .field("max_tokens", &self.max_tokens)
            .field("reasoning_effort", &self.reasoning_effort)
            .field("temperature", &self.temperature)
            .finish()
    }
}

impl OpenAiConfig {
    /// Builds a configuration for `model` at the vendor's default endpoint.
    #[must_use]
    pub fn new(vendor: Vendor, model: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            vendor,
            model: model.into(),
            api_key: api_key.into(),
            base_url: vendor.default_base_url().to_owned(),
            max_tokens: vendor.max_output_tokens(),
            reasoning_effort: ReasoningEffort::Medium,
            temperature: None,
        }
    }

    /// Builds a configuration pointing at a different endpoint.
    ///
    /// This is what a *plan* is: z.ai's coding plan is the same protocol and the
    /// same key at a different host, so choosing a plan is choosing a base URL.
    #[must_use]
    pub fn with_base_url(
        vendor: Vendor,
        model: impl Into<String>,
        api_key: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Self {
        let mut config = Self::new(vendor, model, api_key);
        config.base_url = base_url.into();
        config
    }

    /// Builds a configuration for `model`, reading the key from the environment.
    ///
    /// # Errors
    ///
    /// Returns [`OpenAiError::MissingCredential`] when the vendor's variable is
    /// unset or empty.
    pub fn from_env(vendor: Vendor, model: impl Into<String>) -> Result<Self, OpenAiError> {
        let key = std::env::var(vendor.env_var()).unwrap_or_default();
        let config = Self::new(vendor, model, key);
        Self::validate(&config)?;
        Ok(config)
    }

    /// Checks that the configuration can issue a request.
    ///
    /// # Errors
    ///
    /// Returns [`OpenAiError::MissingCredential`] for an empty key, and
    /// [`OpenAiError::InvalidConfig`] for an empty model or base URL.
    pub fn validate(config: &Self) -> Result<(), OpenAiError> {
        if config.model.trim().is_empty() {
            return Err(OpenAiError::invalid_config("model", "must not be empty"));
        }
        if config.base_url.trim().is_empty() {
            return Err(OpenAiError::invalid_config("base_url", "must not be empty"));
        }
        if config.api_key.trim().is_empty() {
            return Err(OpenAiError::missing_credential(config.vendor));
        }
        Ok(())
    }

    /// Returns the vendor.
    #[must_use]
    pub const fn vendor(&self) -> Vendor {
        self.vendor
    }

    /// Returns the model id.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Returns the API key.
    #[must_use]
    pub fn api_key(&self) -> &str {
        &self.api_key
    }

    /// Returns the base URL.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Returns the requested maximum output tokens, before the provider's ceiling
    /// is applied.
    #[must_use]
    pub const fn max_tokens(&self) -> u32 {
        self.max_tokens
    }

    /// Returns the maximum output tokens a request may actually ask for.
    ///
    /// The provider refuses a request above its ceiling rather than truncating the
    /// answer, so the budget sent is the smaller of what was configured and what
    /// the provider permits — and a caller that wants to *report* the difference
    /// asks this rather than repeating the rule.
    #[must_use]
    pub const fn effective_max_tokens(&self) -> u32 {
        if self.max_tokens < self.vendor.max_output_tokens() {
            self.max_tokens
        } else {
            self.vendor.max_output_tokens()
        }
    }

    /// Returns the configured reasoning effort.
    #[must_use]
    pub const fn reasoning_effort(&self) -> ReasoningEffort {
        self.reasoning_effort
    }

    /// Returns the configured sampling temperature, if any.
    #[must_use]
    pub const fn temperature(&self) -> Option<f32> {
        self.temperature
    }

    /// Sets the maximum output tokens.
    ///
    /// # Errors
    ///
    /// Returns [`OpenAiError::InvalidConfig`] when `max_tokens` is zero.
    pub fn set_max_tokens(&mut self, max_tokens: u32) -> Result<(), OpenAiError> {
        if max_tokens == 0 {
            return Err(OpenAiError::invalid_config(
                "max_tokens",
                "must be greater than zero",
            ));
        }
        self.max_tokens = max_tokens;
        Ok(())
    }

    /// Sets the reasoning effort.
    pub const fn set_reasoning_effort(&mut self, effort: ReasoningEffort) {
        self.reasoning_effort = effort;
    }

    /// Sets the sampling temperature.
    ///
    /// # Errors
    ///
    /// Returns [`OpenAiError::InvalidConfig`] when `temperature` is negative or not
    /// finite.
    pub fn set_temperature(&mut self, temperature: f32) -> Result<(), OpenAiError> {
        if !temperature.is_finite() || temperature < 0.0 {
            return Err(OpenAiError::invalid_config(
                "temperature",
                "must be finite and non-negative",
            ));
        }
        self.temperature = Some(temperature);
        Ok(())
    }

    /// Sets the base URL.
    ///
    /// # Errors
    ///
    /// Returns [`OpenAiError::InvalidConfig`] when `base_url` is empty.
    pub fn set_base_url(&mut self, base_url: impl Into<String>) -> Result<(), OpenAiError> {
        let base_url = base_url.into();
        if base_url.trim().is_empty() {
            return Err(OpenAiError::invalid_config("base_url", "must not be empty"));
        }
        self.base_url = base_url;
        Ok(())
    }
}

/// The spelling of a neutral effort on the wire.
///
/// Both vendors name reasoning depth with the same seven words — `none`, `minimal`, `low`,
/// `medium`, `high`, `xhigh`, `max` — so the neutral step travels as written. They differ in how
/// they express *thinking off*: `OpenAI` just sends `none`, while z.ai needs the thinking switch on
/// as well, which [`crate::wire`] writes for it.
#[must_use]
pub const fn effort_spelling(effort: ReasoningEffort) -> &'static str {
    effort.as_str()
}

/// The effort steps `OpenAI`'s `gpt-5.6` family takes: `none` through `max`.
const OPENAI_FULL: &[ReasoningEffort] = &[
    ReasoningEffort::None,
    ReasoningEffort::Low,
    ReasoningEffort::Medium,
    ReasoningEffort::High,
    ReasoningEffort::XHigh,
    ReasoningEffort::Max,
];

/// The steps a model takes that has no `none`: `gpt-6-astra`.
const OPENAI_NO_NONE: &[ReasoningEffort] = &[
    ReasoningEffort::Low,
    ReasoningEffort::Medium,
    ReasoningEffort::High,
    ReasoningEffort::XHigh,
    ReasoningEffort::Max,
];

/// The steps `gpt-5.3-codex` takes: no `none` and no `max`.
const OPENAI_CODEX: &[ReasoningEffort] = &[
    ReasoningEffort::Low,
    ReasoningEffort::Medium,
    ReasoningEffort::High,
    ReasoningEffort::XHigh,
];

/// The steps the previous-generation models take: `minimal` through `high`.
const OPENAI_LEGACY: &[ReasoningEffort] = &[
    ReasoningEffort::Minimal,
    ReasoningEffort::Low,
    ReasoningEffort::Medium,
    ReasoningEffort::High,
];

/// The documented effort steps for one `OpenAI` model.
///
/// Read from each model's own page rather than guessed: the family splits between four and six
/// steps, and a model the table does not know falls back to the previous generation's four, which
/// every reasoning model accepts.
#[must_use]
pub fn openai_effort_levels(model: &str) -> &'static [ReasoningEffort] {
    match model {
        "gpt-6-astra" => OPENAI_NO_NONE,
        "gpt-5.6-sol" | "gpt-5.6-terra" | "gpt-5.6-luna" => OPENAI_FULL,
        "gpt-5.3-codex" => OPENAI_CODEX,
        _ => OPENAI_LEGACY,
    }
}

/// The documented effort steps for one z.ai model.
///
/// The GLM-5.3 family can no longer turn thinking off — sending a disabled switch is an error —
/// so its list starts at `low`; GLM-5.2 keeps the whole scale, `none` included.
#[must_use]
pub fn zai_effort_levels(model: &str) -> &'static [ReasoningEffort] {
    match model {
        "glm-5.2" => OPENAI_FULL,
        _ => ZAI_FORCED,
    }
}

/// The steps a z.ai model that cannot disable thinking takes.
const ZAI_FORCED: &[ReasoningEffort] = &[
    ReasoningEffort::Low,
    ReasoningEffort::Medium,
    ReasoningEffort::High,
    ReasoningEffort::XHigh,
    ReasoningEffort::Max,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_vendor_name_round_trips() {
        assert_eq!(Vendor::parse("openai"), Some(Vendor::OpenAi));
        assert_eq!(Vendor::parse("zai"), Some(Vendor::Zai));
        // z.ai is written with a dot in prose; both spellings resolve so a
        // configuration copied from the vendor's own documentation works.
        assert_eq!(Vendor::parse("z.ai"), Some(Vendor::Zai));
        assert_eq!(Vendor::parse("deepseek"), None);
        assert_eq!(Vendor::OpenAi.as_str(), "openai");
        assert_eq!(Vendor::Zai.to_string(), "zai");
    }

    /// Each vendor's endpoint and credential are its own, so a configuration cannot
    /// send an `OpenAI` key to z.ai by accident.
    #[test]
    fn each_vendor_has_its_own_host_and_variable() {
        assert_eq!(Vendor::OpenAi.default_base_url(), OPENAI_BASE_URL);
        assert_eq!(Vendor::OpenAi.env_var(), "OPENAI_API_KEY");
        assert_eq!(Vendor::Zai.default_base_url(), ZAI_BASE_URL);
        assert_eq!(Vendor::Zai.env_var(), "ZAI_API_KEY");
        // The coding plan is a host, not a different provider.
        assert_eq!(ZAI_CODING_BASE_URL, "https://api.z.ai/api/coding/paas/v4");
        assert_ne!(ZAI_BASE_URL, ZAI_CODING_BASE_URL);
    }

    /// The offered models are the vendor's own, and none of them is another
    /// vendor's.
    #[test]
    fn the_offered_models_belong_to_their_vendor() {
        assert_eq!(
            Vendor::OpenAi.models(),
            [
                "gpt-6-astra",
                "gpt-5.6-sol",
                "gpt-5.6-terra",
                "gpt-5.6-luna",
                "gpt-5.3-codex",
                "gpt-5",
                "gpt-5-mini",
                "gpt-5-codex",
            ]
        );
        assert_eq!(
            Vendor::Zai.models(),
            ["glm-5.3-flashx", "glm-5.3-flash", "glm-5.3", "glm-5.2"]
        );
        for model in Vendor::OpenAi.models() {
            assert!(!model.contains("glm"), "{model}");
        }
        // The default is the first, and it stays the general model rather than the
        // coding one: the coding model is reachable by the plan that names it, and by
        // a cycle, without becoming what a configuration that names nothing gets.
        assert_eq!(
            Vendor::OpenAi.models().first().copied(),
            Some("gpt-6-astra")
        );
    }

    /// The budget sent is capped at the provider's documented ceiling, because a
    /// request above it is refused rather than truncated.
    #[test]
    fn the_requested_budget_is_capped_by_the_provider() {
        // The configuration default is 128000: OpenAI's ceiling admits it, and
        // z.ai's does not, so one provider caps and the other does not.
        let mut openai = OpenAiConfig::new(Vendor::OpenAi, "gpt-5", "key");
        assert!(openai.set_max_tokens(128_000).is_ok());
        assert_eq!(openai.max_tokens(), 128_000);
        assert_eq!(openai.effective_max_tokens(), 128_000);

        let mut zai = OpenAiConfig::new(Vendor::Zai, "glm-4.5", "key");
        assert!(zai.set_max_tokens(128_000).is_ok());
        assert_eq!(zai.effective_max_tokens(), 98_304);

        // Above the ceiling, the ceiling is what is sent.
        assert!(openai.set_max_tokens(200_000).is_ok());
        assert_eq!(openai.effective_max_tokens(), 128_000);

        // Below it, the configured value is what is sent.
        assert!(zai.set_max_tokens(4_096).is_ok());
        assert_eq!(zai.effective_max_tokens(), 4_096);
    }

    /// Each vendor's ceiling field is its own, and neither accepts the other's.
    #[test]
    fn each_vendor_names_its_output_ceiling_its_own_way() {
        assert_eq!(Vendor::OpenAi.output_token_field(), "max_completion_tokens");
        assert_eq!(Vendor::Zai.output_token_field(), "max_tokens");
    }

    /// Every neutral step travels as its own name, for both vendors.
    #[test]
    fn every_effort_travels_as_its_own_name() {
        for effort in [
            ReasoningEffort::None,
            ReasoningEffort::Minimal,
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High,
            ReasoningEffort::XHigh,
            ReasoningEffort::Max,
        ] {
            assert_eq!(effort_spelling(effort), effort.as_str());
        }
    }

    /// The steps offered are the ones each model documents, which differ by model.
    #[test]
    fn the_offered_levels_follow_the_model() {
        // The `gpt-5.6` family takes `none` through `max`.
        assert_eq!(
            Vendor::OpenAi.effort_levels("gpt-5.6-sol"),
            [
                ReasoningEffort::None,
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::High,
                ReasoningEffort::XHigh,
                ReasoningEffort::Max,
            ]
        );
        // The flagship has no `none` of its own.
        assert!(
            !Vendor::OpenAi
                .effort_levels("gpt-6-astra")
                .contains(&ReasoningEffort::None)
        );
        // The previous generation stops at `high`.
        assert!(
            !Vendor::OpenAi
                .effort_levels("gpt-5")
                .contains(&ReasoningEffort::XHigh)
        );

        // z.ai's GLM-5.3 models cannot turn thinking off, so `none` is not offered; GLM-5.2 can.
        assert!(
            !Vendor::Zai
                .effort_levels("glm-5.3-flashx")
                .contains(&ReasoningEffort::None)
        );
        assert!(
            Vendor::Zai
                .effort_levels("glm-5.2")
                .contains(&ReasoningEffort::None)
        );
    }

    #[test]
    fn validate_rejects_each_empty_field() {
        let mut config = OpenAiConfig::new(Vendor::Zai, "glm-4.5", "key");
        assert!(OpenAiConfig::validate(&config).is_ok());
        assert!(config.set_max_tokens(0).is_err());

        let empty_model = OpenAiConfig::new(Vendor::Zai, "", "key");
        assert!(matches!(
            OpenAiConfig::validate(&empty_model),
            Err(OpenAiError::InvalidConfig { field: "model", .. })
        ));
        let empty_key = OpenAiConfig::new(Vendor::Zai, "glm-4.5", "");
        assert!(matches!(
            OpenAiConfig::validate(&empty_key),
            Err(OpenAiError::MissingCredential { .. })
        ));
    }

    #[test]
    fn temperature_must_be_finite_and_non_negative() {
        let mut config = OpenAiConfig::new(Vendor::OpenAi, "gpt-5", "key");
        assert!(config.set_temperature(0.0).is_ok());
        assert!(config.set_temperature(-0.1).is_err());
        assert!(config.set_temperature(f32::NAN).is_err());
        // Postcondition: the last accepted value survives the rejections.
        assert_eq!(config.temperature(), Some(0.0));
    }

    /// The `Debug` rendering reaches logs, so the key must not be recoverable from
    /// it — for either vendor.
    #[test]
    fn debug_never_renders_the_api_key() {
        for vendor in [Vendor::OpenAi, Vendor::Zai] {
            let config = OpenAiConfig::new(vendor, "some-model", "sk-super-secret-value");
            let rendered = format!("{config:?}");
            assert!(!rendered.contains("sk-super-secret-value"), "{rendered}");
            assert!(rendered.contains("some-model"), "{rendered}");
        }
    }
}
