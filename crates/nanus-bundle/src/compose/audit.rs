//! The runners a stock composition could have prepared a recorded session's requests with.
//!
//! A cache audit ([`crate::cache_audit`]) prepares each recorded request again and claims it only
//! when the body has the recorded digest. These are the candidates it tries, built exactly as
//! [`compose`](super::compose) builds a runner — the same adapters, toolset, system prompt and
//! runner settings — with three differences that cannot change a body:
//!
//! - **No credential.** Every adapter is built with a placeholder key. A digest covers the body,
//!   which never carries a credential, and nothing built here is ever dispatched.
//! - **The route is the recorded one.** The provider, plan and model come from the request's
//!   selection identity rather than from the configuration, and every plan of that provider is a
//!   candidate, because the identity names an endpoint by digest only.
//! - **The runtime section is the recording's.** The system prompt names the workspace, the model
//!   the composition started with and its approval and sandbox states. The workspace is the
//!   session's own; the rest is tried as the session's origin recorded it, then as the
//!   configuration says now.
//!
//! What it cannot reproduce makes a request unverified rather than wrongly claimed: a host's own
//! tools, the optional `read_video` tool, a system prompt or budget the configuration has changed
//! since, or another build's prompt, schemas or encoder.

use std::path::{Path, PathBuf};
use std::rc::Rc;

use nanus_adapter_config::NanusConfig;
use nanus_adapter_local::{LocalFs, LocalShell, SystemClock};
use nanus_adapter_openai::Vendor;
use nanus_domain::context::managed::SelectionIdentity;
use nanus_domain::{ApprovalPolicy, Origin, SandboxMode, Session};
use nanus_ports::{ClockHandle, LlmHandle, LlmPort, SandboxPolicy, Secret};

use super::{build_anthropic, build_compatible, build_deepseek, build_runner, build_tools};
use crate::ToolRegistryHandle;
use crate::agent_loop::AgentRunner;
use crate::cache_audit::AuditRoutes;
use crate::error::BundleError;
use crate::provider::{Provider, Selection};

/// The key every audit adapter is built with. A body never carries a credential, and nothing
/// built with this one is sent.
const PLACEHOLDER_KEY: &str = "nanus-cache-audit";

/// The stock composition's candidates for one recorded session.
pub struct StockAuditRoutes {
    config: NanusConfig,
    workspace: PathBuf,
    origin: Option<Origin>,
    tools: ToolRegistryHandle,
    clock: ClockHandle,
}

impl core::fmt::Debug for StockAuditRoutes {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("StockAuditRoutes")
            .field("workspace", &self.workspace)
            .finish_non_exhaustive()
    }
}

/// The candidates a stock composition under `config` gives for `session`'s requests.
///
/// Reads no credential, opens no store and writes nothing. The toolset's filesystem is rooted at
/// the session's workspace when it still exists and at the temporary directory otherwise; nothing
/// runs a tool, and no stock schema names its root.
///
/// # Errors
///
/// Returns [`BundleError::Config`] when the toolset cannot be built.
pub fn audit_routes(
    config: &NanusConfig,
    session: &Session,
) -> Result<StockAuditRoutes, BundleError> {
    let workspace = PathBuf::from(session.cwd());
    let root = if workspace.is_dir() {
        workspace.clone()
    } else {
        std::env::temp_dir()
    };
    let fs = LocalFs::new(root.clone())
        .map_err(|error| BundleError::config(error.to_string()))?
        .handle();
    let shell = LocalShell::new(SandboxPolicy::new(config.sandbox_mode, root)).handle();
    let tools = build_tools(&fs, &shell, &crate::CaptureBroker::new())?;
    Ok(StockAuditRoutes {
        config: config.clone(),
        workspace,
        origin: session.origin().cloned(),
        tools,
        clock: SystemClock::new().handle(),
    })
}

impl StockAuditRoutes {
    /// The configurations a route on `provider` for `model` may have been composed under: each
    /// of its plans at its own endpoint, and the configured endpoint when it is this provider's.
    fn route_configs(&self, provider: Provider, model: &str) -> Vec<NanusConfig> {
        let configured = Selection::resolve(&self.config).ok();
        let mut configs = Vec::new();
        if let Some(configured) = configured.filter(|selection| selection.provider() == provider) {
            let mut config = self.config.clone();
            config.plan = Some(configured.plan().name.to_owned());
            config.model = Some(model.to_owned());
            configs.push(config);
        }
        for plan in provider.plans() {
            let mut config = self.config.clone();
            config.provider = Some(provider.name().to_owned());
            config.plan = Some(plan.name.to_owned());
            config.base_url = None;
            config.model = Some(model.to_owned());
            if !configs.contains(&config) {
                configs.push(config);
            }
        }
        configs
    }

    /// The runtime sections a runner may have been composed with: the model it started on and
    /// its approval and sandbox states, as the origin recorded them and then as configured.
    fn runtime_variants(&self, model: &str) -> Vec<(String, ApprovalPolicy, SandboxMode)> {
        let origin = self.origin.as_ref();
        let mut models: Vec<String> = origin
            .and_then(|origin| origin.model.clone())
            .into_iter()
            .collect();
        if !models.iter().any(|known| known == model) {
            models.push(model.to_owned());
        }
        let mut approvals: Vec<ApprovalPolicy> = origin
            .and_then(|origin| origin.approval.as_deref())
            .and_then(|raw| ApprovalPolicy::parse(raw).ok())
            .into_iter()
            .collect();
        if !approvals.contains(&self.config.approval_policy) {
            approvals.push(self.config.approval_policy);
        }
        let mut sandboxes: Vec<SandboxMode> = origin
            .and_then(|origin| origin.sandbox.as_deref())
            .and_then(|raw| SandboxMode::parse(raw).ok())
            .into_iter()
            .collect();
        if !sandboxes.contains(&self.config.sandbox_mode) {
            sandboxes.push(self.config.sandbox_mode);
        }
        let mut variants = Vec::new();
        for model in &models {
            for &approval in &approvals {
                for &sandbox in &sandboxes {
                    variants.push((model.clone(), approval, sandbox));
                }
            }
        }
        variants
    }

    /// Every runner for `provider` and `model`, the likeliest first.
    fn runners(&self, provider: Provider, model: &str) -> Result<Vec<AgentRunner>, String> {
        let mut runners = Vec::new();
        let mut failure = None;
        for config in self.route_configs(provider, model) {
            let built = Selection::resolve(&config)
                .and_then(|selection| Ok((keyless_llm(&config, &selection)?, selection)));
            let (llm, _) = match built {
                Ok(built) => built,
                Err(error) => {
                    failure.get_or_insert_with(|| error.to_string());
                    continue;
                }
            };
            for (started_on, approval, sandbox) in self.runtime_variants(model) {
                match self.runner(&llm, &config, &started_on, (approval, sandbox)) {
                    Ok(runner) => runners.push(runner),
                    Err(error) => {
                        failure.get_or_insert_with(|| error.to_string());
                    }
                }
            }
        }
        if runners.is_empty() {
            return Err(failure.unwrap_or_else(|| String::from("no runner could be built")));
        }
        Ok(runners)
    }

    /// One runner, composed as [`build_runner`] composes one, over `llm`.
    fn runner(
        &self,
        llm: &LlmHandle,
        config: &NanusConfig,
        started_on: &str,
        (approval, sandbox): (ApprovalPolicy, SandboxMode),
    ) -> Result<AgentRunner, BundleError> {
        let mut composed = config.clone();
        composed.model = Some(started_on.to_owned());
        composed.approval_policy = approval;
        composed.sandbox_mode = sandbox;
        let selection = Selection::resolve(&composed)?;
        build_runner(
            llm,
            &self.tools,
            &composed,
            &selection,
            Path::new(&self.workspace),
            &self.clock,
        )
    }
}

impl AuditRoutes for StockAuditRoutes {
    fn managed(&self, selection: &SelectionIdentity) -> Result<Vec<AgentRunner>, String> {
        let provider = Provider::parse(&selection.provider).ok_or_else(|| {
            format!(
                "recorded on provider {:?}, which this build does not offer",
                selection.provider
            )
        })?;
        self.runners(provider, &selection.model)
    }

    fn legacy(&self, model: Option<&str>) -> Result<Vec<AgentRunner>, String> {
        let configured = Selection::resolve(&self.config).map_err(|error| error.to_string())?;
        let Some(model) = model else {
            return self.runners(configured.provider(), configured.model());
        };
        let provider =
            if configured.provider().models().contains(&model) || configured.model() == model {
                configured.provider()
            } else {
                Provider::ALL
                    .into_iter()
                    .find(|provider| provider.models().contains(&model))
                    .ok_or_else(|| {
                        format!("no provider this build offers serves the model {model:?}")
                    })?
            };
        self.runners(provider, model)
    }
}

/// Builds the adapter a selection calls for, with the placeholder key and no renewal.
///
/// The subscription's adapter is built from its plan as a keyed one is: its token set decides only
/// how a request authenticates, never what its body holds.
fn keyless_llm(config: &NanusConfig, selection: &Selection) -> Result<LlmHandle, BundleError> {
    let key = Secret::new(PLACEHOLDER_KEY);
    let port: Box<dyn LlmPort> = match selection.provider() {
        Provider::DeepSeek => Box::new(build_deepseek(config, selection, &key)?),
        Provider::Zai => Box::new(build_compatible(Vendor::Zai, config, selection, &key)?),
        Provider::OpenAi => Box::new(build_compatible(Vendor::OpenAi, config, selection, &key)?),
        Provider::Inception => Box::new(build_compatible(
            Vendor::Inception,
            config,
            selection,
            &key,
        )?),
        Provider::Anthropic => Box::new(build_anthropic(config, selection, &key)?),
    };
    Ok(Rc::new(port))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nanus_domain::SessionId;
    use nanus_domain::context::managed::Digest;

    fn routes() -> StockAuditRoutes {
        let session = Session::new(SessionId::new("s"), 0, "/a/workspace/that/is/not/there");
        audit_routes(&NanusConfig::default(), &session).unwrap()
    }

    fn recorded(provider: &str, model: &str) -> SelectionIdentity {
        SelectionIdentity {
            provider: provider.into(),
            endpoint_digest: Digest::of(b"e"),
            protocol: "w".into(),
            model: model.into(),
            effort: None,
            epoch: 0,
        }
    }

    #[test]
    fn a_route_on_a_provider_this_build_does_not_offer_is_refused_by_name() {
        let refused = routes().managed(&recorded("elsewhere", "m"));
        assert!(refused.is_err_and(|reason| reason.contains("\"elsewhere\"")));
    }

    #[test]
    fn every_plan_of_the_recorded_provider_is_a_candidate_on_the_recorded_model() {
        let runners = routes()
            .managed(&recorded("openai", "gpt-5.6-sol"))
            .unwrap();
        // The API plan and the subscription, under the one runtime section the configuration has.
        assert_eq!(runners.len(), OPENAI_PLANS);
        assert!(runners.iter().all(|runner| runner.model() == "gpt-5.6-sol"));
    }

    #[test]
    fn a_legacy_model_is_built_by_the_provider_that_offers_it_and_an_unknown_one_is_not() {
        let runners = routes().legacy(Some("claude-sonnet-5-5")).unwrap();
        assert_eq!(runners[0].model(), "claude-sonnet-5-5");
        let refused = routes().legacy(Some("a-model-nobody-offers"));
        assert!(refused.is_err_and(|reason| reason.contains("a-model-nobody-offers")));
    }

    /// `OpenAI`'s plan count, from the table rather than restated.
    const OPENAI_PLANS: usize = Provider::OpenAi.plans().len();

    /// A stock adapter whose prepared calls answer from a script instead of the network: what is
    /// prepared, digested and recorded is the adapter's own, and only the reply is not.
    struct Scripted {
        inner: LlmHandle,
        script: Rc<core::cell::RefCell<Vec<Vec<nanus_ports::LlmEvent>>>>,
    }

    struct ScriptedCall {
        inner: Box<dyn nanus_ports::PreparedModelCall>,
        script: Rc<core::cell::RefCell<Vec<Vec<nanus_ports::LlmEvent>>>>,
    }

    impl nanus_ports::PreparedModelCall for ScriptedCall {
        fn estimate(&self) -> nanus_ports::RequestEstimate {
            self.inner.estimate()
        }
        fn request_digest(&self) -> &Digest {
            self.inner.request_digest()
        }
        fn selection(&self) -> &SelectionIdentity {
            self.inner.selection()
        }
        fn stream(self: Box<Self>) -> nanus_ports::LlmStream {
            // Taken as the call is sent, not as it is prepared: a step prepares more than once.
            let events = self.script.borrow_mut().pop().unwrap_or_default();
            Box::pin(futures::stream::iter(events))
        }
    }

    impl LlmPort for Scripted {
        fn model(&self) -> &str {
            self.inner.model()
        }
        fn reasoning_effort(&self, model: &str) -> Option<nanus_ports::ReasoningEffort> {
            self.inner.reasoning_effort(model)
        }
        fn capabilities(&self, model: &str) -> nanus_ports::ModelCapabilities {
            self.inner.capabilities(model)
        }
        fn stream_chat(&self, _: nanus_ports::ChatRequest) -> nanus_ports::LlmStream {
            Box::pin(futures::stream::empty())
        }
        fn managed_support(&self, model: &str) -> nanus_ports::ManagedSupport {
            self.inner.managed_support(model)
        }
        fn prepare_managed(
            &self,
            request: nanus_ports::ManagedRequest,
        ) -> nanus_ports::LlmResult<Box<dyn nanus_ports::PreparedModelCall>> {
            let inner = self.inner.prepare_managed(request)?;
            Ok(Box::new(ScriptedCall {
                inner,
                script: Rc::clone(&self.script),
            }))
        }
    }

    fn reply(
        events: Vec<nanus_ports::LlmEvent>,
        prompt: u32,
        read: u32,
    ) -> Vec<nanus_ports::LlmEvent> {
        let mut events = events;
        let usage = nanus_domain::Usage::new(prompt, 5, 0, read, prompt.saturating_sub(read))
            .with_cache_hit_reported(true);
        events.insert(0, nanus_ports::LlmEvent::Usage(usage));
        events.push(nanus_ports::LlmEvent::Finished {
            reason: nanus_ports::FinishReason::Stop,
        });
        events
    }

    /// Records a managed two-step turn through the stock runner and `provider`'s real adapter,
    /// in a scratch store and workspace, and returns the stored session and its configuration.
    fn record(provider: Provider, model: &str, home: &Path) -> (Session, NanusConfig) {
        let workspace = home.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("notes.txt"), "the parser is done\n").unwrap();
        let config = NanusConfig {
            provider: Some(provider.name().to_owned()),
            model: Some(model.to_owned()),
            ..NanusConfig::default()
        };
        let selection = Selection::resolve(&config).unwrap();
        let secrets = nanus_adapter_secret::Secrets::new_in(home, "nanus")
            .unwrap()
            .handle();
        let real =
            super::super::build_llm(&config, &selection, &Secret::new("k"), &secrets).unwrap();
        let read_call = nanus_ports::LlmEvent::ToolCallDelta {
            index: 0,
            id: Some(nanus_domain::ToolCallId::new("call_1")),
            name: Some(nanus_domain::ToolName::new("read").unwrap()),
            arguments_delta: String::from(r#"{"file_path": "notes.txt"}"#),
        };
        // Popped from the end: the read first, then the answer.
        let script = Rc::new(core::cell::RefCell::new(vec![
            reply(
                vec![nanus_ports::LlmEvent::TextDelta("done".into())],
                9_200,
                9_000,
            ),
            reply(vec![read_call], 9_000, 0),
        ]));
        let llm: LlmHandle = Rc::new(Box::new(Scripted {
            inner: real,
            script,
        }));
        let fs = LocalFs::new(workspace.clone()).unwrap().handle();
        let shell =
            LocalShell::new(SandboxPolicy::new(config.sandbox_mode, workspace.clone())).handle();
        let tools = build_tools(&fs, &shell, &crate::CaptureBroker::new()).unwrap();
        let clock = SystemClock::new().handle();
        let runner = build_runner(&llm, &tools, &config, &selection, &workspace, &clock).unwrap();
        let store = nanus_kernel::runtime::block_on(nanus_adapter_store::JsonlStore::new(
            home.join("store"),
        ))
        .unwrap()
        .handle();
        let mut session = super::super::new_session(&clock, &workspace);
        nanus_kernel::runtime::block_on_local(async {
            store.lock(session.id(), "test").await.unwrap();
            let checkpoint = crate::StoreCheckpoint::bind(store.clone(), session.id().clone())
                .await
                .unwrap();
            let context = crate::SessionContext::with_key(Some(store.clone()), [3; 32]);
            let runtime = nanus_ports::TurnRuntime {
                context: Some(&context),
                checkpoint: Some(&checkpoint),
            };
            let policy = nanus_domain::context::managed::ContextPolicy::managed();
            let actor = nanus_domain::context::managed::ModeActor::Human;
            runner
                .set_context_policy(&mut session, policy, actor, runtime)
                .await
                .unwrap();
            let host = crate::TurnHost {
                approver: None,
                control: None,
                runtime,
            };
            let run = runner
                .run_turn_with_runtime(&mut session, "read the notes", &mut crate::Silent, host)
                .await;
            assert!(run.outcome.is_ok(), "{provider}: {:?}", run.outcome);
            store.release_lock(session.id());
            (store.load(session.id()).await.unwrap(), config)
        })
    }

    /// A managed turn recorded through each chat wire's real adapter and the stock runner is
    /// prepared again to its digests by the stock routes, with no key: the path `nanus sessions
    /// cache` takes, on the wires the binary's own end-to-end test does not reach.
    #[test]
    fn a_turn_through_each_stock_chat_adapter_audits_as_digest_verified() {
        use crate::cache_audit::{Attribution, Basis, Verification, audit};
        for (provider, model) in [
            (Provider::DeepSeek, "deepseek-flash"),
            (Provider::Inception, "mercury-3"),
            (Provider::Zai, "glm-5.2"),
            (Provider::Anthropic, "claude-sonnet-5-5"),
        ] {
            let home = tempfile::tempdir().unwrap();
            let (session, config) = record(provider, model, home.path());
            let audited = audit(&session, &audit_routes(&config, &session).unwrap());
            assert_eq!(audited.requests.len(), 2, "{provider}: {audited:#?}");
            for request in &audited.requests {
                assert!(
                    matches!(request.verification, Verification::Digest { .. }),
                    "{provider}: {audited:#?}"
                );
            }
            assert_eq!(
                audited.requests[0].attribution,
                Attribution::First,
                "{provider}"
            );
            assert_eq!(
                audited.requests[1].attribution,
                Attribution::Hit {
                    basis: Basis::Digest
                },
                "{provider}: {audited:#?}"
            );
        }
    }
}
