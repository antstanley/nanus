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

/// The model that reads frames for `provider`, when one has verified image input.
///
/// A model qualifies by having an exact image profile with live evidence in
/// `docs/vision-evidence.md`; nothing here infers support from a name. Both are the smaller
/// of the provider's verified models, because describing four stills does not need the largest.
const fn analysis_model(provider: Provider) -> Option<&'static str> {
    match provider {
        Provider::Anthropic => Some("claude-sonnet-5-5"),
        Provider::OpenAi => Some("gpt-6-luna"),
        Provider::DeepSeek | Provider::Zai => None,
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
                     cannot be described there; switch to anthropic or openai"
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
                protocol: protocol_of(provider).to_owned(),
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
fn origin_of(endpoint: &str) -> String {
    let (scheme, rest) = endpoint.split_once("://").unwrap_or(("https", endpoint));
    let host = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let host = host.rsplit('@').next().unwrap_or(host);
    format!("{scheme}://{host}")
}

/// The wire the provider's analysis model is spoken to on.
const fn protocol_of(provider: Provider) -> &'static str {
    match provider {
        Provider::Anthropic => "messages",
        Provider::OpenAi => "responses",
        Provider::DeepSeek | Provider::Zai => "chat-completions",
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
        assert_eq!(
            analysis_model(Provider::Anthropic),
            Some("claude-sonnet-5-5")
        );
        assert_eq!(analysis_model(Provider::OpenAi), Some("gpt-6-luna"));
        assert_eq!(analysis_model(Provider::DeepSeek), None);
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
    }
}
