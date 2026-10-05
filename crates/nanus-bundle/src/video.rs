//! The optional `read_video` tool, wired to the stock providers.
//!
//! The tool and its decoder live in `nanus-tool-video`, which names no provider. This module
//! is the stock composition's half: it decides which model reads the frames for each provider
//! and builds that model's adapter from the credential already in use.
//!
//! The analysis model is always on the **same provider, plan, endpoint and credential account**
//! as the conversation, and only the model id differs. A provider with no model whose image
//! input has live evidence has no route here, and the tool says so rather than guessing.

use std::rc::{Rc, Weak};

use nanus_adapter_config::NanusConfig;
use nanus_adapter_openai::Protocol;
use nanus_ports::{FsHandle, LocalBoxFuture};
use nanus_tool_video::{
    AnalysisBudget, FfmpegDecoder, FsSource, LlmAnalyzer, Provenance, VideoAnalyzer, VideoError,
    VideoRouting, VideoServices, read_video_tool,
};

use crate::compose::ProviderSwitch;
use crate::error::BundleError;
use crate::provider::Provider;

/// The plan whose endpoint takes no output ceiling on the wire.
const SUBSCRIPTION: &str = "subscription";
/// What is recorded for an endpoint whose origin cannot be told apart from what else it carries.
const ORIGIN_UNKNOWN: &str = "unknown";

/// The model that reads frames for `provider`, when one has verified image input.
///
/// z.ai has none: no model there has a profile with live evidence, and its credentials were never
/// available to gather it.
///
/// A model qualifies by having an exact image profile with live evidence in
/// `docs/vision-evidence.md`; nothing here infers support from a name. Both are the smaller
/// of the provider's verified models, because describing four stills does not need the largest.
const fn analysis_model(provider: Provider) -> Option<&'static str> {
    match provider {
        Provider::Anthropic => Some("claude-sonnet-5-5"),
        Provider::OpenAi => Some("gpt-6-luna"),
        Provider::DeepSeek => Some("deepseek-flash"),
        Provider::Zai => None,
    }
}

/// Routes `read_video` through whatever provider the harness is currently using.
struct StockRouting {
    /// Weak, because the switch owns the runner that owns the registry that owns this tool.
    switch: Weak<ProviderSwitch>,
    /// What analysis requests may spend, shared across calls and provider switches.
    budget: Rc<AnalysisBudget>,
}

impl StockRouting {
    fn switch(&self) -> Result<Rc<ProviderSwitch>, VideoError> {
        self.switch.upgrade().ok_or_else(|| {
            VideoError::Unavailable("read_video: the agent has shut down".to_owned())
        })
    }
}

impl VideoRouting for StockRouting {
    fn main_model_sees_images(&self) -> bool {
        self.switch()
            .is_ok_and(|switch| switch.main_model_sees_images())
    }

    fn analyzer(&self) -> LocalBoxFuture<'_, Result<Rc<dyn VideoAnalyzer>, VideoError>> {
        Box::pin(async move {
            let switch = self.switch()?;
            let current = switch.current_selection();
            let provider = current.provider();
            let Some(model) = analysis_model(provider) else {
                return Err(VideoError::Unavailable(format!(
                    "read_video: {provider} has no model with verified image input, so frames \
                     cannot be described there; switch to anthropic, openai or deepseek"
                )));
            };
            let (llm, selection) = switch
                .adapter_for(model)
                .await
                .map_err(|error| VideoError::Unavailable(format!("read_video: {error}")))?;
            let provenance = Provenance {
                provider: provider.name().to_owned(),
                plan: selection.plan().name.to_owned(),
                model: model.to_owned(),
                endpoint_origin: origin_of(selection.endpoint()),
                protocol: protocol_of(provider, selection.protocol(), model).to_owned(),
                profile_version: String::new(),
                processing: "one request, no retries; lowest reasoning effort; answer capped \
                             at 24 KiB locally"
                    .to_owned(),
            };
            let mut analyzer = LlmAnalyzer::new(llm, model, provenance)?;
            if selection.plan().name == SUBSCRIPTION {
                // This endpoint takes no output ceiling, so the request's ceiling only sizes
                // the local reservation. Reserve the provider's whole documented ceiling.
                analyzer = analyzer.with_request_tokens(selection.max_output_tokens());
            }
            let analyzer = analyzer.with_budget(Rc::clone(&self.budget))?;
            let analyzer: Rc<dyn VideoAnalyzer> = Rc::new(analyzer);
            Ok(analyzer)
        })
    }
}

/// The scheme and host of an endpoint: no path, no query, nothing a credential could ride in.
///
/// The authority ends at the first `/`, `?`, `#` or `\`, which is where an HTTP client ends it
/// too. A userinfo that contains one of those (`user:pa/ss@host`) is therefore cut in two, and
/// what precedes the cut would look like a host: so an `@` anywhere after the authority makes
/// the origin unknown rather than recorded, and so does anything left that is not a host and
/// port. Recording nothing is the failure that cannot leak a credential into a transcript.
fn origin_of(endpoint: &str) -> String {
    let Some((scheme, rest)) = endpoint.split_once("://") else {
        return ORIGIN_UNKNOWN.to_owned();
    };
    let end = rest.find(['/', '?', '#', '\\']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    if tail.contains('@') {
        return ORIGIN_UNKNOWN.to_owned();
    }
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let scheme_ok = !scheme.is_empty()
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    let host_ok = !host.is_empty()
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | ':' | '[' | ']'));
    if scheme_ok && host_ok {
        format!("{scheme}://{host}")
    } else {
        ORIGIN_UNKNOWN.to_owned()
    }
}

/// The wire the analysis request is sent on, as the adapter `adapter_for` builds decides it.
///
/// For `OpenAI` that is not the provider's fact but the plan's and the model's: the stock
/// composition leaves the adapter on automatic routing, under which the plan's wire is used
/// except that `OpenAI`'s own models from the `gpt-5.6` generation on go to Responses. This is
/// `OpenAiConfig::protocol_for` under that policy, and a test holds the two together.
fn protocol_of(provider: Provider, plan: Protocol, model: &str) -> &'static str {
    match provider {
        Provider::Anthropic => "messages",
        Provider::OpenAi if plan == Protocol::Responses || Protocol::responses_first(model) => {
            "responses"
        }
        Provider::OpenAi | Provider::DeepSeek | Provider::Zai => "chat-completions",
    }
}

/// Registers `read_video` beside the tools the runner already dispatches.
///
/// # Errors
///
/// Returns [`BundleError::Config`] when `FFmpeg` is missing or lacks a mandatory component, or
/// when the name is already registered. A configuration that asks for the tool and cannot have
/// it fails to start rather than quietly lacking it.
pub fn install(
    switch: &Rc<ProviderSwitch>,
    config: &NanusConfig,
    fs: FsHandle,
) -> Result<(), BundleError> {
    let decoder = FfmpegDecoder::prepare(config.ffmpeg_dir.as_deref())
        .map_err(|error| BundleError::config(error.to_string()))?;
    let tool = read_video_tool(VideoServices {
        source: Rc::new(FsSource::new(fs)),
        decoder: Rc::new(decoder),
        routing: Rc::new(StockRouting {
            switch: Rc::downgrade(switch),
            budget: AnalysisBudget::new(config.video_analysis_budget),
        }),
    })
    .map_err(|error| BundleError::config(error.to_string()))?;
    switch
        .runner_tools()
        .borrow_mut()
        .register(tool)
        .map_err(|error| {
            BundleError::config(format!("read_video could not be registered: {error}"))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_providers_with_verified_image_models_have_a_route() {
        // z.ai is the one stock provider with nothing to route to.
        assert_eq!(
            analysis_model(Provider::Anthropic),
            Some("claude-sonnet-5-5")
        );
        assert_eq!(analysis_model(Provider::OpenAi), Some("gpt-6-luna"));
        assert_eq!(analysis_model(Provider::DeepSeek), Some("deepseek-flash"));
        assert_eq!(analysis_model(Provider::Zai), None);
    }

    #[test]
    fn the_recorded_origin_carries_no_path_query_or_userinfo() {
        assert_eq!(
            origin_of("https://api.openai.com/v1/responses?key=x"),
            "https://api.openai.com"
        );
        assert_eq!(
            origin_of("https://user:secret@proxy.example:8443/a/b"),
            "https://proxy.example:8443"
        );
        // A userinfo with a delimiter in it is cut where a client cuts it; none of it is kept.
        for leaky in [
            "https://user:pa/ss@host.example/v1",
            "https://user:123/x@host.example",
            "https://user:p?ss@host.example",
            "https://user:p#ss@host.example",
            "api.example.com/v1",
        ] {
            assert_eq!(origin_of(leaky), ORIGIN_UNKNOWN, "{leaky}");
        }
    }

    #[test]
    fn the_recorded_protocol_is_the_one_the_adapter_routes_to() {
        use nanus_adapter_openai::{OpenAiConfig, Vendor};
        for plan in [Protocol::ChatCompletions, Protocol::Responses] {
            for model in [
                "gpt-6-luna",
                "gpt-5.6-luna",
                "gpt-5.5",
                "gpt-4.1",
                "o4-mini",
            ] {
                let mut adapter =
                    OpenAiConfig::with_base_url(Vendor::OpenAi, model, "", "https://x.invalid");
                adapter.set_protocol(plan);
                let routed = match adapter.protocol_for(model) {
                    Protocol::Responses => "responses",
                    Protocol::ChatCompletions => "chat-completions",
                };
                assert_eq!(
                    protocol_of(Provider::OpenAi, plan, model),
                    routed,
                    "{model} on {plan:?}"
                );
            }
        }
        // The stock analysis model is on Responses on either plan, and an older one is not.
        let api = Protocol::ChatCompletions;
        assert_eq!(
            protocol_of(Provider::OpenAi, api, "gpt-6-luna"),
            "responses"
        );
        assert_eq!(
            protocol_of(Provider::OpenAi, api, "gpt-4.1"),
            "chat-completions"
        );
        assert_eq!(
            protocol_of(Provider::Anthropic, api, "claude-sonnet-5-5"),
            "messages"
        );
    }
}
