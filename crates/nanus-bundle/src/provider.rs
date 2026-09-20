//! The providers this build can talk to, and what a configuration resolves to.
//!
//! ## Why a table rather than a `match` at the call site
//!
//! Everything a caller needs to know about a provider — its name, its credential
//! variable, its endpoint, its plans, the models it offers, and the ceiling the
//! provider refuses to exceed — is a fact, so it lives in a table. Adding a provider
//! is a row here plus one arm in the composition that builds an adapter for it;
//! nothing else in the harness changes, and nothing outside this module needs to
//! know that one provider speaks the Messages API and the others speak chat
//! completions.
//!
//! ## What a plan is
//!
//! A plan is what a provider calls a tier of service: z.ai's coding subscription is
//! the same protocol and the same key at a different host, and `OpenAI`'s is a
//! different default model on the same host. So a plan is an endpoint plus a default
//! model, plus — where this build cannot honour it — a sentence saying why not.
//!
//! A plan this build cannot use is *listed and refused by name* rather than absent:
//! "unknown plan" tells a reader nothing, while "the subscription plan needs an
//! OAuth token and the Responses API, which this build does not encode" tells them
//! what would have to change.
//!
//! ## Absent means the provider's answer
//!
//! The configuration names a provider, a plan, an endpoint, and a model, and every
//! one of them may be absent. Resolution fills each gap from the layer above it, so
//! a file that names nothing gets the shipped default and a file that names a
//! provider gets that provider's host and model. This is where "which model" is
//! decided, and the one place a configuration's meaning is written down.

use nanus_adapter_config::NanusConfig;
use nanus_adapter_deepseek::{
    self as deepseek, DEFAULT_MAX_OUTPUT_TOKENS as DEEPSEEK_MAX_OUTPUT_TOKENS,
};
use nanus_adapter_openai::Vendor;

use crate::error::BundleError;

/// The plan every provider offers, under this name.
pub const DEFAULT_PLAN: &str = "api";

/// The provider the composition uses when a configuration names none.
pub const DEFAULT_PROVIDER: Provider = Provider::DeepSeek;

/// One provider this build can talk to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Provider {
    /// `DeepSeek`, over its own `OpenAI`-compatible endpoint.
    DeepSeek,
    /// z.ai's GLM API, pay-as-you-go and coding plans alike.
    Zai,
    /// Anthropic's Messages API.
    Anthropic,
    /// `OpenAI`'s own API.
    OpenAi,
}

/// The credential a plan takes, when it does not share the provider's own.
///
/// Two kinds, because they are obtained in different ways: a *key* is typed by the user and filed
/// under an account, and an *authorization* is completed in a browser and the resulting token set
/// is filed instead. z.ai's coding subscription is the first kind; `OpenAI`'s `ChatGPT` subscription
/// is the second.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanCredential {
    /// A key the user types, filed under `account` and falling back to `env`.
    Key {
        /// The account the credential is filed under.
        account: &'static str,
        /// The environment variable it falls back to.
        env: &'static str,
    },
    /// An OAuth authorization the user completes elsewhere, stored as a token set.
    Oauth {
        /// The account the token set is filed under.
        account: &'static str,
    },
}

impl PlanCredential {
    /// Returns the account this credential is filed under.
    #[must_use]
    pub const fn account(self) -> &'static str {
        match self {
            Self::Key { account, .. } | Self::Oauth { account } => account,
        }
    }

    /// Returns the environment variable it falls back to, when it has one.
    ///
    /// An authorization has none: a token set is not a key that can be exported into a shell, and
    /// advertising a variable for it would suggest one could.
    #[must_use]
    pub const fn env(self) -> Option<&'static str> {
        match self {
            Self::Key { env, .. } => Some(env),
            Self::Oauth { .. } => None,
        }
    }

    /// Returns `true` when this credential is an authorization rather than a typed key.
    #[must_use]
    pub const fn is_oauth(self) -> bool {
        matches!(self, Self::Oauth { .. })
    }
}

/// One tier of a provider's service.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Plan {
    /// The name a configuration writes.
    pub name: &'static str,
    /// The endpoint the plan sends to.
    pub endpoint: &'static str,
    /// The model used when the configuration names none.
    pub model: Option<&'static str>,
    /// Why this build cannot use the plan, when it cannot.
    pub refusal: Option<&'static str>,
    /// The credential this plan takes, when it does not share the provider's own.
    ///
    /// `None` means the provider's own account and variable — one key for every plan.
    pub credential: Option<PlanCredential>,
}

/// `DeepSeek`'s plans: one endpoint, no tiers.
static DEEPSEEK_PLANS: [Plan; 1] = [Plan {
    name: DEFAULT_PLAN,
    endpoint: deepseek::DEFAULT_BASE_URL,
    model: Some(deepseek::MODEL_FLASH),
    refusal: None,
    credential: None,
}];

/// z.ai's plans: the API and the coding subscription, the same protocol at two hosts and two keys.
///
/// Both default to `glm-5.3-flashx`. The coding subscription takes a coding-plan key rather than the
/// pay-as-you-go API key, so it is filed under its own account: a reader who set one for the API is
/// asked for one for the coding plan rather than having the API key quietly sent to the coding host.
static ZAI_PLANS: [Plan; 2] = [
    Plan {
        name: DEFAULT_PLAN,
        endpoint: nanus_adapter_openai::ZAI_BASE_URL,
        model: Some("glm-5.3-flashx"),
        refusal: None,
        credential: None,
    },
    Plan {
        name: "coding",
        endpoint: nanus_adapter_openai::ZAI_CODING_BASE_URL,
        // The coding host serves the same models, so the plan changes where a
        // request goes and which key it carries, and nothing else.
        model: Some("glm-5.3-flashx"),
        refusal: None,
        credential: Some(PlanCredential::Key {
            account: "zai:coding",
            env: "ZAI_CODING_API_KEY",
        }),
    },
];

/// Anthropic's plans: the API only, which is what this build implements.
static ANTHROPIC_PLANS: [Plan; 1] = [Plan {
    name: DEFAULT_PLAN,
    endpoint: nanus_adapter_anthropic::DEFAULT_BASE_URL,
    model: Some("claude-sonnet-5"),
    refusal: None,
    credential: None,
}];

/// `OpenAI`'s plans: the API, and the `ChatGPT` subscription reached with an OAuth authorization.
///
/// The subscription is not a different host for the same key: it is a `ChatGPT` account, authorized
/// in a browser, whose token is filed under its own account. There is no third plan — the coding
/// models are served by the API with the same key, so they are models of the `api` plan rather
/// than a plan of their own.
static OPENAI_PLANS: [Plan; 2] = [
    Plan {
        name: DEFAULT_PLAN,
        endpoint: nanus_adapter_openai::OPENAI_BASE_URL,
        model: Some("gpt-6-astra"),
        refusal: None,
        credential: None,
    },
    Plan {
        name: "subscription",
        endpoint: "https://chatgpt.com/backend-api/codex",
        model: Some("gpt-5.3-codex"),
        refusal: None,
        credential: Some(PlanCredential::Oauth {
            account: "openai:subscription",
        }),
    },
];

/// The model ids `DeepSeek` offers, in cycling order.
static DEEPSEEK_MODELS: [&str; 2] = [deepseek::MODEL_FLASH, deepseek::MODEL_PRO];

impl Provider {
    /// Every provider this build offers, in the order a listing shows them.
    pub const ALL: [Self; 4] = [Self::DeepSeek, Self::Zai, Self::Anthropic, Self::OpenAi];

    /// Returns the name a configuration writes.
    ///
    /// The same word is the secret-store account a `nanus auth` command files a key
    /// under and the stem of the credential variable, so one spelling names a
    /// provider everywhere it appears.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::DeepSeek => deepseek::PROVIDER,
            Self::Zai => Vendor::Zai.as_str(),
            Self::Anthropic => nanus_adapter_anthropic::PROVIDER,
            Self::OpenAi => Vendor::OpenAi.as_str(),
        }
    }

    /// Returns the provider a name names, when it names one.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|provider| provider.name() == raw)
    }

    /// Returns every provider's name, for an error that lists what is offered.
    #[must_use]
    pub fn names() -> Vec<&'static str> {
        Self::ALL.iter().map(|provider| provider.name()).collect()
    }

    /// Returns the environment variable the credential falls back to.
    #[must_use]
    pub const fn env_var(self) -> &'static str {
        match self {
            Self::DeepSeek => deepseek::API_KEY_ENV,
            Self::Zai => nanus_adapter_openai::ZAI_API_KEY_ENV,
            Self::Anthropic => nanus_adapter_anthropic::API_KEY_ENV,
            Self::OpenAi => nanus_adapter_openai::OPENAI_API_KEY_ENV,
        }
    }

    /// Returns the provider's plans.
    #[must_use]
    pub const fn plans(self) -> &'static [Plan] {
        match self {
            Self::DeepSeek => &DEEPSEEK_PLANS,
            Self::Zai => &ZAI_PLANS,
            Self::Anthropic => &ANTHROPIC_PLANS,
            Self::OpenAi => &OPENAI_PLANS,
        }
    }

    /// Returns the plan a configuration gets when it names none.
    ///
    /// Every provider offers [`DEFAULT_PLAN`], which is what makes "a plan" something
    /// a configuration may omit. The fallback keeps the function total for a provider
    /// with no plans, which a test forbids a shipped one from having.
    #[must_use]
    pub fn default_plan(self) -> &'static Plan {
        self.plans()
            .iter()
            .find(|plan| plan.name == DEFAULT_PLAN)
            .unwrap_or(&DEEPSEEK_PLANS[0])
    }

    /// Returns the plan a name names, when this provider offers it.
    #[must_use]
    pub fn plan(self, name: &str) -> Option<&'static Plan> {
        self.plans().iter().find(|plan| plan.name == name)
    }

    /// Returns the account a credential for `plan` is filed under.
    ///
    /// The provider's own account unless the plan takes a credential of its own — which is what
    /// makes z.ai's coding subscription ask for a key even when the API key is stored. An unknown
    /// plan answers the provider's account, because resolving it is [`Selection`]'s job and a
    /// refusal there names the plans that do exist.
    #[must_use]
    pub fn credential_account(self, plan: Option<&str>) -> &'static str {
        self.credential(plan)
            .map_or_else(|| self.name(), PlanCredential::account)
    }

    /// Returns the environment variable a credential for `plan` falls back to.
    ///
    /// `None` for an authorization, which is completed in a browser and has no variable to export.
    #[must_use]
    pub fn credential_env(self, plan: Option<&str>) -> Option<&'static str> {
        self.credential(plan)
            .map_or_else(|| Some(self.env_var()), PlanCredential::env)
    }

    /// Returns `true` when `plan` is reached with an authorization rather than a typed key.
    #[must_use]
    pub fn credential_is_oauth(self, plan: Option<&str>) -> bool {
        self.credential(plan).is_some_and(PlanCredential::is_oauth)
    }

    /// Returns the credential `plan` takes of its own, when it takes one.
    fn credential(self, plan: Option<&str>) -> Option<PlanCredential> {
        self.plan(plan.unwrap_or(DEFAULT_PLAN))
            .and_then(|plan| plan.credential)
    }

    /// Returns every account a credential for this provider may be filed under, with the variable
    /// each falls back to (none, for an authorization).
    ///
    /// The provider's own account first, then each plan that takes a credential of its own: what
    /// `nanus auth status` lists, so a reader sees the account a coding plan or a subscription is
    /// asked for rather than having to guess its name.
    #[must_use]
    pub fn accounts(self) -> Vec<(&'static str, Option<&'static str>)> {
        let mut accounts = vec![(self.name(), Some(self.env_var()))];
        for plan in self.plans() {
            if let Some(credential) = plan.credential {
                accounts.push((credential.account(), credential.env()));
            }
        }
        accounts
    }

    /// Returns the models a client may switch between, in cycling order.
    #[must_use]
    pub const fn models(self) -> &'static [&'static str] {
        match self {
            Self::DeepSeek => &DEEPSEEK_MODELS,
            Self::Zai => Vendor::Zai.models(),
            // Taken from the adapter rather than restated: the ids are the provider's
            // fact, and a second copy could only drift from the one the adapter uses to
            // validate a plan.
            Self::Anthropic => nanus_adapter_anthropic::AnthropicConfig::models(),
            Self::OpenAi => Vendor::OpenAi.models(),
        }
    }

    /// Returns the model used when neither a plan nor a configuration names one.
    #[must_use]
    pub fn default_model(self) -> &'static str {
        let mut models = self.models().iter();
        models.next().copied().unwrap_or(deepseek::MODEL_FLASH)
    }

    /// Returns the provider's documented maximum output tokens.
    ///
    /// Read from the adapter that talks to it rather than restated here: the number
    /// is a provider fact and the adapter is the crate that owns provider facts, so
    /// two copies could only drift.
    #[must_use]
    pub const fn max_output_tokens(self) -> u32 {
        match self {
            Self::DeepSeek => DEEPSEEK_MAX_OUTPUT_TOKENS,
            Self::Zai => Vendor::Zai.max_output_tokens(),
            Self::Anthropic => nanus_adapter_anthropic::MAX_OUTPUT_TOKENS,
            Self::OpenAi => Vendor::OpenAi.max_output_tokens(),
        }
    }

    /// Returns `true` when `reasoning_effort` reaches the provider.
    ///
    /// False for Anthropic, which expresses thinking as a token budget and needs the
    /// signed thinking blocks of the previous turn replayed — something the message
    /// model has no place for, so the adapter does not ask. Reported by
    /// `nanus config` so an inert knob is visible rather than silent.
    #[must_use]
    pub const fn effort_applies(self) -> bool {
        !matches!(self, Self::Anthropic)
    }
}

impl core::fmt::Display for Provider {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.name())
    }
}

/// What a configuration resolves to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Selection {
    provider: Provider,
    plan: Plan,
    model: String,
    endpoint: String,
}

impl Selection {
    /// Resolves a configuration into the provider, plan, model, and endpoint a run
    /// will use.
    ///
    /// # Errors
    ///
    /// Returns [`BundleError::Config`] for a provider or plan this build does not
    /// offer, for a plan it cannot use, and for an endpoint that is not absolute.
    pub fn resolve(config: &NanusConfig) -> Result<Self, BundleError> {
        let provider = Self::resolve_provider(config)?;
        let plan = Self::resolve_plan(config, provider)?;
        let endpoint = config
            .base_url
            .clone()
            .unwrap_or_else(|| plan.endpoint.to_owned());
        if !endpoint.starts_with("http") {
            return Err(BundleError::config(format!(
                "the endpoint {endpoint:?} is not absolute"
            )));
        }
        let model = config
            .model
            .clone()
            .or_else(|| plan.model.map(str::to_owned))
            .unwrap_or_else(|| provider.default_model().to_owned());
        if model.trim().is_empty() {
            return Err(BundleError::config(String::from("no model is configured")));
        }
        Ok(Self {
            provider,
            plan,
            model,
            endpoint,
        })
    }

    /// Resolves which provider is selected.
    fn resolve_provider(config: &NanusConfig) -> Result<Provider, BundleError> {
        let Some(name) = config.provider.as_deref() else {
            return Ok(DEFAULT_PROVIDER);
        };
        Provider::parse(name).ok_or_else(|| {
            BundleError::config(format!(
                "unknown provider {name:?}: this build offers {}",
                Provider::names().join(", ")
            ))
        })
    }

    /// Resolves which plan is selected, refusing one this build cannot use.
    fn resolve_plan(config: &NanusConfig, provider: Provider) -> Result<Plan, BundleError> {
        let name = config.plan.as_deref().unwrap_or(DEFAULT_PLAN);
        let plan = provider.plan(name).ok_or_else(|| {
            let offered: Vec<&str> = provider.plans().iter().map(|plan| plan.name).collect();
            BundleError::config(format!(
                "unknown plan {name:?} for {provider}: it offers {}",
                offered.join(", ")
            ))
        })?;
        if let Some(refusal) = plan.refusal {
            return Err(BundleError::config(format!(
                "the {name:?} plan for {provider} is not usable by this build: {refusal}"
            )));
        }
        Ok(*plan)
    }

    /// Returns the default resolution, for a caller that has no configuration.
    #[must_use]
    pub fn default_for(provider: Provider) -> Self {
        let plan = *provider.default_plan();
        Self {
            provider,
            plan,
            model: plan
                .model
                .map_or_else(|| provider.default_model().to_owned(), str::to_owned),
            endpoint: plan.endpoint.to_owned(),
        }
    }

    /// Returns the provider.
    #[must_use]
    pub const fn provider(&self) -> Provider {
        self.provider
    }

    /// Returns the plan in force.
    #[must_use]
    pub const fn plan(&self) -> &Plan {
        &self.plan
    }

    /// Returns the model a request will name.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Returns the endpoint a request is posted to.
    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Returns the models a client may switch this run between.
    #[must_use]
    pub const fn models(&self) -> &'static [&'static str] {
        self.provider.models()
    }

    /// Returns the account a credential is filed under.
    ///
    /// The provider's own name, or the plan's own account when it takes one: z.ai's coding
    /// subscription is `zai:coding`, so storing an API key does not satisfy it.
    #[must_use]
    pub fn credential_account(&self) -> &'static str {
        self.plan
            .credential
            .map_or_else(|| self.provider.name(), |credential| credential.account())
    }

    /// Returns the environment variable a credential falls back to, when it has one.
    ///
    /// `None` for an authorization: a token set is completed in a browser rather than read from the
    /// environment.
    #[must_use]
    pub fn credential_env(&self) -> Option<&'static str> {
        self.plan
            .credential
            .map_or_else(|| Some(self.provider.env_var()), PlanCredential::env)
    }

    /// Returns `true` when this plan is reached with an authorization rather than a typed key.
    #[must_use]
    pub fn credential_is_oauth(&self) -> bool {
        self.plan.credential.is_some_and(PlanCredential::is_oauth)
    }

    /// Returns the provider's documented maximum output tokens.
    #[must_use]
    pub const fn max_output_tokens(&self) -> u32 {
        self.provider.max_output_tokens()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> NanusConfig {
        NanusConfig::default()
    }

    /// A configuration that names nothing runs the shipped default, and the model is
    /// the provider's own rather than a value the schema guessed.
    #[test]
    fn an_empty_configuration_resolves_to_the_default_provider() {
        let selection = Selection::resolve(&config()).expect("the default resolves");
        assert_eq!(selection.provider(), Provider::DeepSeek);
        assert_eq!(selection.plan().name, DEFAULT_PLAN);
        assert_eq!(selection.model(), "deepseek-flash");
        assert_eq!(selection.endpoint(), "https://api.deepseek.com");
        // The retired ids do not sneak back in through a default.
        assert_ne!(selection.model(), "deepseek-chat");
        assert_ne!(selection.model(), "deepseek-reasoner");
        assert_eq!(selection.credential_account(), "deepseek");
    }

    /// Naming a provider fills the host and the model from that provider, which is
    /// the whole reason a model is optional in the schema.
    #[test]
    fn naming_a_provider_supplies_its_host_and_model() {
        let openai = NanusConfig {
            provider: Some(String::from("openai")),
            ..config()
        };
        let selection = Selection::resolve(&openai).expect("openai resolves");
        assert_eq!(selection.provider(), Provider::OpenAi);
        assert_eq!(selection.endpoint(), "https://api.openai.com/v1");
        assert_eq!(selection.model(), "gpt-6-astra");
        assert_eq!(selection.credential_env(), Some("OPENAI_API_KEY"));

        let anthropic = NanusConfig {
            provider: Some(String::from("anthropic")),
            ..config()
        };
        let selection = Selection::resolve(&anthropic).expect("anthropic resolves");
        assert_eq!(selection.model(), "claude-sonnet-5");
        assert_eq!(selection.endpoint(), "https://api.anthropic.com/v1");
        // And a provider that does not use the effort knob says so.
        assert!(!selection.provider().effort_applies());
    }

    /// A plan changes the endpoint (z.ai's coding plan, `OpenAI`'s subscription) or the default
    /// model (`OpenAI`'s coding models), because that is what a plan is.
    #[test]
    fn a_plan_changes_the_endpoint_or_the_model() {
        let zai_coding = NanusConfig {
            provider: Some(String::from("zai")),
            plan: Some(String::from("coding")),
            ..config()
        };
        let selection = Selection::resolve(&zai_coding).expect("the coding plan resolves");
        assert_eq!(selection.endpoint(), "https://api.z.ai/api/coding/paas/v4");

        let zai_api = NanusConfig {
            provider: Some(String::from("zai")),
            ..config()
        };
        let selection = Selection::resolve(&zai_api).expect("the api plan resolves");
        assert_eq!(selection.endpoint(), "https://api.z.ai/api/paas/v4");

        // The subscription is a different host *and* a different credential.
        let openai_subscription = NanusConfig {
            provider: Some(String::from("openai")),
            plan: Some(String::from("subscription")),
            ..config()
        };
        let selection =
            Selection::resolve(&openai_subscription).expect("the subscription plan resolves");
        assert_eq!(
            selection.endpoint(),
            "https://chatgpt.com/backend-api/codex"
        );
        assert!(
            selection.credential_is_oauth(),
            "reached with an authorization"
        );
    }

    /// An explicit model or endpoint wins over the plan's, because the more specific
    /// instruction is the one a person typed.
    #[test]
    fn an_explicit_model_and_endpoint_win() {
        let config = NanusConfig {
            provider: Some(String::from("openai")),
            model: Some(String::from("gpt-5-mini")),
            base_url: Some(String::from("https://gateway.internal/v1")),
            ..config()
        };
        let selection = Selection::resolve(&config).expect("an override resolves");
        assert_eq!(selection.model(), "gpt-5-mini");
        assert_eq!(selection.endpoint(), "https://gateway.internal/v1");
    }

    /// Every failure names what is offered, because an error that lists the
    /// alternatives is one a reader can act on.
    #[test]
    fn an_unknown_provider_or_plan_is_refused_by_name() {
        let unknown = NanusConfig {
            provider: Some(String::from("gemini")),
            ..config()
        };
        let error = Selection::resolve(&unknown).expect_err("an unknown provider is refused");
        let rendered = error.to_string();
        assert!(rendered.contains("gemini"), "{rendered}");
        for name in Provider::names() {
            assert!(rendered.contains(name), "{rendered} omits {name}");
        }

        let unknown_plan = NanusConfig {
            provider: Some(String::from("anthropic")),
            plan: Some(String::from("coding")),
            ..config()
        };
        let error = Selection::resolve(&unknown_plan).expect_err("an unknown plan is refused");
        let rendered = error.to_string();
        assert!(rendered.contains("coding"), "{rendered}");
        assert!(rendered.contains("api"), "{rendered}");
    }

    /// The subscription is a usable plan now, and what it takes is an authorization
    /// rather than a key.
    #[test]
    fn the_subscription_resolves_and_authorizes() {
        let config = NanusConfig {
            provider: Some(String::from("openai")),
            plan: Some(String::from("subscription")),
            ..config()
        };
        let selection = Selection::resolve(&config).expect("the subscription plan resolves");
        assert_eq!(selection.plan().name, "subscription");
        assert_eq!(
            selection.endpoint(),
            "https://chatgpt.com/backend-api/codex"
        );
        assert!(selection.credential_is_oauth());
        assert_eq!(selection.credential_account(), "openai:subscription");
        assert_eq!(selection.credential_env(), None);
    }

    /// A relative endpoint would fail as a confusing transport error later, so it is
    /// refused where it is configured.
    #[test]
    fn a_relative_endpoint_is_refused() {
        let config = NanusConfig {
            base_url: Some(String::from("api.example.com")),
            ..config()
        };
        let outcome = Selection::resolve(&config);
        assert!(matches!(outcome, Err(BundleError::Config(_))));
    }

    /// Every provider names a distinct account and offers a plan, models, and a
    /// ceiling, so a key cannot be sent to the wrong place and a listing cannot be
    /// empty.
    #[test]
    fn every_provider_has_its_own_credential_and_models() {
        let mut accounts: Vec<&str> = Vec::new();
        for provider in Provider::ALL {
            assert!(!provider.models().is_empty(), "{provider} offers models");
            assert!(
                provider
                    .plans()
                    .iter()
                    .any(|plan| plan.name == DEFAULT_PLAN),
                "{provider} offers the default plan"
            );
            assert_eq!(provider.default_plan().name, DEFAULT_PLAN, "{provider}");
            assert!(provider.max_output_tokens() > 0, "{provider}");
            assert!(!accounts.contains(&provider.name()), "{provider} is unique");
            accounts.push(provider.name());
            for model in provider.models() {
                assert!(!model.is_empty(), "{provider} lists a real model");
            }
            // Every plan's own default model has to be one the agent offers, or a run under
            // that plan starts on an id a client cannot name and `SetModel` would refuse — and
            // the cycle, which treats an unknown current model as a fresh start, could leave it
            // and never return. A plan with no model of its own is a refusal, checked elsewhere.
            for plan in provider.plans() {
                if let Some(model) = plan.model {
                    assert!(
                        provider.models().contains(&model),
                        "{provider}'s {:?} plan resolves to {model}, which it does not offer",
                        plan.name
                    );
                }
            }
        }
        assert_eq!(accounts, Provider::names());
    }

    /// A part-named provider still answers what its default is, which is what a
    /// caller with no configuration relies on.
    #[test]
    fn a_default_selection_needs_no_configuration() {
        for provider in Provider::ALL {
            let selection = Selection::default_for(provider);
            assert_eq!(selection.provider(), provider);
            assert_eq!(selection.model(), provider.default_model());
            assert!(!selection.endpoint().is_empty());
            assert!(
                selection.models().contains(&selection.model()),
                "{provider}'s default model is one it offers"
            );
        }
    }

    /// A plan may carry a credential of its own — a key or an authorization — and one that does not
    /// shares the provider's.
    #[test]
    fn a_plan_may_carry_its_own_credential() {
        // z.ai's coding subscription is a different key on a different host.
        assert_eq!(
            Provider::Zai.credential_account(Some("coding")),
            "zai:coding"
        );
        assert_eq!(
            Provider::Zai.credential_env(Some("coding")),
            Some("ZAI_CODING_API_KEY")
        );
        assert!(!Provider::Zai.credential_is_oauth(Some("coding")));
        // The API plan is the provider's own account, and so is an unset plan.
        assert_eq!(Provider::Zai.credential_account(Some("api")), "zai");
        assert_eq!(Provider::Zai.credential_account(None), "zai");
        assert_eq!(Provider::Zai.credential_env(None), Some("ZAI_API_KEY"));

        // OpenAI's subscription is an authorization: an account of its own, and no variable to name
        // because it is completed in a browser. The API plan shares the provider's key.
        assert_eq!(
            Provider::OpenAi.credential_account(Some("subscription")),
            "openai:subscription"
        );
        assert!(Provider::OpenAi.credential_is_oauth(Some("subscription")));
        assert_eq!(Provider::OpenAi.credential_env(Some("subscription")), None);
        assert_eq!(Provider::OpenAi.credential_account(Some("api")), "openai");
        assert!(!Provider::OpenAi.credential_is_oauth(Some("api")));

        // The account a selection resolves to follows the plan, so composition reads the right one.
        let subscription = Selection::resolve(&NanusConfig {
            provider: Some(String::from("openai")),
            plan: Some(String::from("subscription")),
            ..NanusConfig::default()
        })
        .expect("the subscription plan resolves");
        assert_eq!(subscription.credential_account(), "openai:subscription");
        assert_eq!(subscription.credential_env(), None);
        assert!(subscription.credential_is_oauth());

        // Every account a provider may be filed under is one the credential store accepts.
        for provider in Provider::ALL {
            for (account, _) in provider.accounts() {
                assert!(
                    nanus_adapter_secret::valid_account(account),
                    "{account} is a usable account name"
                );
            }
        }
        // And a listing shows them, so the name a plan is asked for is visible.
        let zai: Vec<&str> = Provider::Zai
            .accounts()
            .iter()
            .map(|(account, _)| *account)
            .collect();
        assert_eq!(zai, vec!["zai", "zai:coding"]);
        let openai: Vec<&str> = Provider::OpenAi
            .accounts()
            .iter()
            .map(|(account, _)| *account)
            .collect();
        assert_eq!(openai, vec!["openai", "openai:subscription"]);
    }
}
