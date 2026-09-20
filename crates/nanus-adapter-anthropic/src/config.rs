//! Adapter configuration: endpoint, model, credential, and the version header.
//!
//! Anthropic's API is not OpenAI-compatible, so this crate carries its own numbers
//! rather than sharing the `OpenAI` adapter's table: the host is `api.anthropic.com`
//! with a required `/v1`, the credential travels in `x-api-key` rather than as a
//! bearer token, and every request must name the API version it was written against.

use crate::error::AnthropicError;

/// The provider name the harness knows this adapter by.
pub const PROVIDER: &str = "anthropic";

/// Anthropic's API host, in the form its documentation uses.
pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com/v1";

/// The environment variable the API key is read from.
pub const API_KEY_ENV: &str = "ANTHROPIC_API_KEY";

/// The API version every request declares.
///
/// Anthropic versions its wire contract by date and requires the header on every
/// request; there is no "latest", which is what makes a client's expectations
/// explicit rather than implied.
pub const API_VERSION: &str = "2023-06-01";

/// The models offered, in cycling order.
///
/// Pinned snapshots rather than aliases, because an alias that resolves to a different
/// model next month would make two runs silently incomparable: the 4.x ids are dated
/// snapshots, and from the 4.6 generation on Anthropic's dateless ids are themselves
/// pinned, so both forms name one immutable model.
///
/// The current lineup comes first, so the fallback default is one of them, and the
/// previous generation stays behind it rather than being dropped: a session resumed
/// against a 4.x model can still cycle back to it.
static MODELS: [&str; 6] = [
    "claude-sonnet-5",
    "claude-opus-5",
    "claude-fable-5-1",
    "claude-haiku-4-5-20251001",
    "claude-sonnet-4-20250514",
    "claude-opus-4-20250514",
];

/// The documented maximum output tokens the adapter will send.
///
/// A request above a model's ceiling is refused rather than truncated, so the adapter
/// sends the smaller of the configured budget and this. It is the *smallest* ceiling of
/// the offered models — Claude Haiku 4.5 caps at 64K while the 5-series caps at 128K —
/// because one value has to hold for whichever model a configuration names. It is a
/// provider fact and therefore lives here rather than in the ports crate.
pub const MAX_OUTPUT_TOKENS: u32 = 64_000;

/// The adapter's settings.
///
/// `Debug` is implemented by hand so the API key cannot leak into a log line; this
/// is the type that reaches `tracing` output, so it is the place to enforce that.
#[derive(Clone)]
pub struct AnthropicConfig {
    model: String,
    api_key: String,
    base_url: String,
    max_tokens: u32,
    temperature: Option<f32>,
}

impl core::fmt::Debug for AnthropicConfig {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AnthropicConfig")
            .field("model", &self.model)
            .field("api_key", &"<redacted>")
            .field("base_url", &self.base_url)
            .field("max_tokens", &self.max_tokens)
            .field("temperature", &self.temperature)
            .finish()
    }
}

impl AnthropicConfig {
    /// Builds a configuration for `model` at the documented endpoint.
    #[must_use]
    pub fn new(model: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            api_key: api_key.into(),
            base_url: DEFAULT_BASE_URL.to_owned(),
            max_tokens: MAX_OUTPUT_TOKENS,
            temperature: None,
        }
    }

    /// Builds a configuration pointing at a different endpoint.
    #[must_use]
    pub fn with_base_url(
        model: impl Into<String>,
        api_key: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Self {
        let mut config = Self::new(model, api_key);
        config.base_url = base_url.into();
        config
    }

    /// Builds a configuration for `model`, reading the key from the environment.
    ///
    /// # Errors
    ///
    /// Returns [`AnthropicError::MissingCredential`] when the variable is unset or
    /// empty.
    pub fn from_env(model: impl Into<String>) -> Result<Self, AnthropicError> {
        let key = std::env::var(API_KEY_ENV).unwrap_or_default();
        let config = Self::new(model, key);
        Self::validate(&config)?;
        Ok(config)
    }

    /// Checks that the configuration can issue a request.
    ///
    /// # Errors
    ///
    /// Returns [`AnthropicError::MissingCredential`] for an empty key, and
    /// [`AnthropicError::InvalidConfig`] for an empty model or base URL.
    pub fn validate(config: &Self) -> Result<(), AnthropicError> {
        if config.model.trim().is_empty() {
            return Err(AnthropicError::invalid_config("model", "must not be empty"));
        }
        if config.base_url.trim().is_empty() {
            return Err(AnthropicError::invalid_config(
                "base_url",
                "must not be empty",
            ));
        }
        if config.api_key.trim().is_empty() {
            return Err(AnthropicError::missing_credential(API_KEY_ENV));
        }
        Ok(())
    }

    /// Returns the models this adapter offers.
    #[must_use]
    pub const fn models() -> &'static [&'static str] {
        &MODELS
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

    /// Returns the requested maximum output tokens, before the ceiling is applied.
    #[must_use]
    pub const fn max_tokens(&self) -> u32 {
        self.max_tokens
    }

    /// Returns the maximum output tokens a request may actually ask for.
    #[must_use]
    pub const fn effective_max_tokens(&self) -> u32 {
        if self.max_tokens < MAX_OUTPUT_TOKENS {
            self.max_tokens
        } else {
            MAX_OUTPUT_TOKENS
        }
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
    /// Returns [`AnthropicError::InvalidConfig`] when `max_tokens` is zero.
    pub fn set_max_tokens(&mut self, max_tokens: u32) -> Result<(), AnthropicError> {
        if max_tokens == 0 {
            return Err(AnthropicError::invalid_config(
                "max_tokens",
                "must be greater than zero",
            ));
        }
        self.max_tokens = max_tokens;
        Ok(())
    }

    /// Sets the sampling temperature.
    ///
    /// # Errors
    ///
    /// Returns [`AnthropicError::InvalidConfig`] when `temperature` is outside the
    /// range Anthropic accepts, which is `0.0` to `1.0` inclusive. The bound is the
    /// provider's rather than the harness's: sending `1.5` is a request the API
    /// refuses, so it is refused here with a clearer sentence.
    pub fn set_temperature(&mut self, temperature: f32) -> Result<(), AnthropicError> {
        if !temperature.is_finite() || !(0.0..=1.0).contains(&temperature) {
            return Err(AnthropicError::invalid_config(
                "temperature",
                "Anthropic accepts 0.0 to 1.0",
            ));
        }
        self.temperature = Some(temperature);
        Ok(())
    }

    /// Sets the base URL.
    ///
    /// # Errors
    ///
    /// Returns [`AnthropicError::InvalidConfig`] when `base_url` is empty.
    pub fn set_base_url(&mut self, base_url: impl Into<String>) -> Result<(), AnthropicError> {
        let base_url = base_url.into();
        if base_url.trim().is_empty() {
            return Err(AnthropicError::invalid_config(
                "base_url",
                "must not be empty",
            ));
        }
        self.base_url = base_url;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_documented_numbers_are_what_the_api_expects() {
        assert_eq!(DEFAULT_BASE_URL, "https://api.anthropic.com/v1");
        assert_eq!(API_KEY_ENV, "ANTHROPIC_API_KEY");
        assert_eq!(API_VERSION, "2023-06-01");
        // The offered ids are pinned snapshots, which is what the API accepts.
        assert_eq!(
            AnthropicConfig::models(),
            [
                "claude-sonnet-5",
                "claude-opus-5",
                "claude-fable-5-1",
                "claude-haiku-4-5-20251001",
                "claude-sonnet-4-20250514",
                "claude-opus-4-20250514",
            ]
        );
    }

    /// The budget sent is capped, because a request above the model's ceiling is
    /// refused rather than truncated.
    #[test]
    fn the_requested_budget_is_capped() {
        let mut config = AnthropicConfig::new("claude-sonnet-4-20250514", "key");
        // The shared configuration default is above the ceiling, so the cap is what
        // makes a default configuration usable at all.
        assert!(config.set_max_tokens(128_000).is_ok());
        assert_eq!(config.max_tokens(), 128_000);
        assert_eq!(config.effective_max_tokens(), MAX_OUTPUT_TOKENS);

        assert!(config.set_max_tokens(4_096).is_ok());
        assert_eq!(config.effective_max_tokens(), 4_096);
        assert!(config.set_max_tokens(0).is_err());
    }

    /// Anthropic's own temperature bound, not the harness's.
    #[test]
    fn temperature_must_be_inside_the_providers_range() {
        let mut config = AnthropicConfig::new("claude-sonnet-4-20250514", "key");
        assert!(config.set_temperature(0.0).is_ok());
        assert!(config.set_temperature(1.0).is_ok());
        // 1.5 is a value DeepSeek accepts and Anthropic refuses, which is why the
        // bound belongs to the adapter.
        assert!(config.set_temperature(1.5).is_err());
        assert!(config.set_temperature(-0.1).is_err());
        assert!(config.set_temperature(f32::NAN).is_err());
        assert_eq!(config.temperature(), Some(1.0));
    }

    #[test]
    fn validate_rejects_each_empty_field() {
        assert!(AnthropicConfig::validate(&AnthropicConfig::new("m", "key")).is_ok());
        assert!(matches!(
            AnthropicConfig::validate(&AnthropicConfig::new("", "key")),
            Err(AnthropicError::InvalidConfig { field: "model", .. })
        ));
        assert!(matches!(
            AnthropicConfig::validate(&AnthropicConfig::new("m", "  ")),
            Err(AnthropicError::MissingCredential { .. })
        ));
    }

    #[test]
    fn debug_never_renders_the_api_key() {
        let config = AnthropicConfig::new("claude-sonnet-4-20250514", "sk-ant-secret");
        let rendered = format!("{config:?}");
        assert!(!rendered.contains("sk-ant-secret"), "{rendered}");
        assert!(rendered.contains("claude-sonnet-4"), "{rendered}");
    }
}
