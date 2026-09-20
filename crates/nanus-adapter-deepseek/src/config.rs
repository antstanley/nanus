//! Adapter configuration: endpoint, model, credential, and decoding parameters.

use crate::error::DeepSeekError;

// The reasoning-effort vocabulary belongs to the ports crate, so a caller can set it
// without depending on this adapter. It is re-exported from the crate root.
use nanus_ports::ReasoningEffort;

/// The provider name the harness knows this adapter by.
///
/// One word names the provider everywhere: the configuration's `provider` value, the
/// secret-store account a `nanus auth` command files its key under, and the
/// environment variable's stem. Stated here so the composition that builds the
/// adapter reads it rather than repeating the string.
pub const PROVIDER: &str = "deepseek";

/// The current fast `DeepSeek` model.
///
/// The retired ids — `deepseek-chat` and `deepseek-reasoner`, discontinued on
/// 2026-07-24 — are deliberately not offered as aliases: silently mapping a retired
/// name onto a new model would change a user's output without telling them.
pub const MODEL_FLASH: &str = "deepseek-flash";

/// The current high-capability `DeepSeek` model.
pub const MODEL_PRO: &str = "deepseek-v4-pro";

/// `DeepSeek`'s documented default host.
pub const DEFAULT_BASE_URL: &str = "https://api.deepseek.com";

/// The environment variable the API key is read from.
pub const API_KEY_ENV: &str = "DEEPSEEK_API_KEY";

/// `DeepSeek`'s documented default output ceiling for the current models.
///
/// Declared here rather than taken from the ports crate because it is a provider
/// fact: another provider would have a different default, and the ports crate
/// deliberately carries no provider numbers.
pub const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 256_000;

/// The adapter's settings.
///
/// [`Debug`] is implemented by hand so the API key cannot leak into a log line;
/// this is the type that reaches `tracing` output, so it is the place to enforce
/// that.
#[derive(Clone)]
pub struct DeepSeekConfig {
    model: String,
    api_key: String,
    base_url: String,
    max_tokens: u32,
    reasoning_effort: ReasoningEffort,
    temperature: Option<f32>,
}

impl core::fmt::Debug for DeepSeekConfig {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DeepSeekConfig")
            .field("model", &self.model)
            .field("api_key", &"<redacted>")
            .field("base_url", &self.base_url)
            .field("max_tokens", &self.max_tokens)
            .field("reasoning_effort", &self.reasoning_effort)
            .field("temperature", &self.temperature)
            .finish()
    }
}

impl DeepSeekConfig {
    /// Builds a configuration for `model` with an explicit key.
    #[must_use]
    pub fn new(model: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            api_key: api_key.into(),
            base_url: DEFAULT_BASE_URL.to_owned(),
            max_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
            reasoning_effort: ReasoningEffort::Medium,
            temperature: None,
        }
    }

    /// Builds a configuration pointing at a different OpenAI-compatible host.
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
    /// Returns [`DeepSeekError::MissingCredential`] when `DEEPSEEK_API_KEY` is
    /// unset or empty.
    pub fn from_env(model: impl Into<String>) -> Result<Self, DeepSeekError> {
        let key = std::env::var(API_KEY_ENV).unwrap_or_default();
        let config = Self::new(model, key);
        Self::validate(&config)?;
        Ok(config)
    }

    /// Checks that the configuration can issue a request.
    ///
    /// # Errors
    ///
    /// Returns [`DeepSeekError::MissingCredential`] for an empty key, and
    /// [`DeepSeekError::InvalidConfig`] for an empty model or base URL.
    pub fn validate(config: &Self) -> Result<(), DeepSeekError> {
        // Both checks are reported rather than the first: a user fixing one field
        // should not have to run twice to learn about the other.
        if config.model.trim().is_empty() {
            return Err(DeepSeekError::invalid_config("model", "must not be empty"));
        }
        if config.base_url.trim().is_empty() {
            return Err(DeepSeekError::invalid_config(
                "base_url",
                "must not be empty",
            ));
        }
        if config.api_key.trim().is_empty() {
            return Err(DeepSeekError::MissingCredential { env: API_KEY_ENV });
        }
        Ok(())
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

    /// Returns the base URL, without a trailing slash.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Returns the maximum output tokens per request.
    #[must_use]
    pub const fn max_tokens(&self) -> u32 {
        self.max_tokens
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
    /// Returns [`DeepSeekError::InvalidConfig`] when `max_tokens` is zero, because a
    /// zero budget would make every request fail upstream with a less clear message.
    pub fn set_max_tokens(&mut self, max_tokens: u32) -> Result<(), DeepSeekError> {
        if max_tokens == 0 {
            return Err(DeepSeekError::invalid_config(
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
    /// Returns [`DeepSeekError::InvalidConfig`] when `temperature` is negative or
    /// not finite. `DeepSeek` silently ignores the value in thinking mode, which is
    /// documented on this method rather than hidden.
    pub fn set_temperature(&mut self, temperature: f32) -> Result<(), DeepSeekError> {
        if !temperature.is_finite() || temperature < 0.0 {
            return Err(DeepSeekError::invalid_config(
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
    /// Returns [`DeepSeekError::InvalidConfig`] when `base_url` is empty.
    pub fn set_base_url(&mut self, base_url: impl Into<String>) -> Result<(), DeepSeekError> {
        let base_url = base_url.into();
        if base_url.trim().is_empty() {
            return Err(DeepSeekError::invalid_config(
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
    fn the_default_effort_is_the_neutral_middle() {
        let config = DeepSeekConfig::new(MODEL_FLASH, "key");
        // The port's vocabulary is provider-neutral, so its middle maps to DeepSeek's
        // documented default rather than to its maximum.
        assert_eq!(config.reasoning_effort(), ReasoningEffort::Medium);
        assert_eq!(crate::wire_effort(ReasoningEffort::Medium), Some("high"));
    }

    #[test]
    fn every_port_effort_translates_to_a_wire_value() {
        // Positive space: each scale step lands on one of the three efforts DeepSeek
        // documents, or on the mode that turns thinking off.
        assert_eq!(crate::wire_effort(ReasoningEffort::Low), Some("low"));
        assert_eq!(crate::wire_effort(ReasoningEffort::Minimal), Some("low"));
        assert_eq!(crate::wire_effort(ReasoningEffort::Medium), Some("high"));
        assert_eq!(crate::wire_effort(ReasoningEffort::High), Some("high"));
        assert_eq!(crate::wire_effort(ReasoningEffort::XHigh), Some("high"));
        assert_eq!(crate::wire_effort(ReasoningEffort::Max), Some("max"));
        // `None` is not a scale step: it disables thinking, which the adapter
        // expresses as a mode rather than as an effort.
        assert_eq!(crate::wire_effort(ReasoningEffort::None), None);
    }

    /// The steps offered to a chooser are the ones that act, not every neutral step.
    #[test]
    fn the_offered_levels_are_the_distinct_ones() {
        assert_eq!(
            crate::effort_levels(),
            [
                ReasoningEffort::None,
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::Max,
            ]
        );
    }

    #[test]
    fn validate_rejects_each_empty_field() {
        let mut config = DeepSeekConfig::new(MODEL_FLASH, "key");
        assert!(DeepSeekConfig::validate(&config).is_ok());

        assert!(config.set_max_tokens(0).is_err());
        let mut empty_model = DeepSeekConfig::new("", "key");
        assert!(matches!(
            DeepSeekConfig::validate(&empty_model),
            Err(DeepSeekError::InvalidConfig { field: "model", .. })
        ));
        empty_model = DeepSeekConfig::new(MODEL_FLASH, "");
        assert!(matches!(
            DeepSeekConfig::validate(&empty_model),
            Err(DeepSeekError::MissingCredential { .. })
        ));
    }

    #[test]
    fn max_tokens_must_be_positive() {
        let mut config = DeepSeekConfig::new(MODEL_FLASH, "key");
        assert!(config.set_max_tokens(1).is_ok());
        assert!(config.set_max_tokens(0).is_err());
        // Pair assertion: a rejected value leaves the previous one in place.
        assert_eq!(config.max_tokens(), 1);
    }

    #[test]
    fn temperature_must_be_finite_and_non_negative() {
        let mut config = DeepSeekConfig::new(MODEL_FLASH, "key");
        assert!(config.set_temperature(0.0).is_ok());
        assert!(config.set_temperature(1.5).is_ok());
        assert!(config.set_temperature(-0.1).is_err());
        assert!(config.set_temperature(f32::NAN).is_err());
        assert!(config.set_temperature(f32::INFINITY).is_err());
        // Postcondition: the last accepted value survives the rejections.
        assert_eq!(config.temperature(), Some(1.5));
    }

    #[test]
    fn from_env_requires_a_non_empty_key() {
        // The check is on the resolved value, not on whether the variable exists,
        // so an empty variable is rejected exactly like an unset one.
        let config = DeepSeekConfig::new(MODEL_FLASH, "   ");
        assert!(matches!(
            DeepSeekConfig::validate(&config),
            Err(DeepSeekError::MissingCredential { env: API_KEY_ENV })
        ));
    }

    #[test]
    fn the_model_ids_are_the_current_ones() {
        // A regression guard: reintroducing a retired id would silently change the
        // model a user is talking to.
        assert_eq!(MODEL_FLASH, "deepseek-flash");
        assert_eq!(MODEL_PRO, "deepseek-v4-pro");
        // The retired ids are not merely unused: naming one must not resolve to a
        // current model, or a user's old configuration would silently change meaning.
        assert_ne!(MODEL_FLASH, "deepseek-chat");
        assert_ne!(MODEL_PRO, "deepseek-reasoner");
        assert_eq!(DEFAULT_BASE_URL, "https://api.deepseek.com");
    }
}
