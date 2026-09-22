//! Assembling a running harness from a configuration.
//!
//! Everything below the kernel is a plugin, and this module is where the shipped set
//! is named. It is deliberately the *only* place that knows which concrete adapters
//! exist: the loop depends on the `llm` service, the tools depend on `fs` and
//! `shell`, and nothing depends on `DeepSeekLlm` or `LocalFs`.
//!
//! ## Why composition is a function that returns a context
//!
//! The kernel resolves dependencies, so this function does not order anything. It
//! mounts the adapters and the loop, and the runtime activates each as its
//! requirements appear. That is what makes a different filesystem or a different
//! model a one-line change here rather than a refactor.

use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use nanus_adapter_anthropic::{AnthropicConfig, AnthropicLlm};
use nanus_adapter_config::{DEFAULT_MAX_TOKENS, NanusConfig};
use nanus_adapter_deepseek::{DEFAULT_MAX_OUTPUT_TOKENS, DeepSeekConfig, DeepSeekLlm};
use nanus_adapter_local::{LocalFs, LocalShell, SystemClock};
use nanus_adapter_openai::{OpenAiConfig, OpenAiLlm, Protocol, Vendor, oauth};
use nanus_adapter_secret::Secrets;
use nanus_adapter_store::JsonlStore;
use nanus_domain::{AgentConfig, Origin, Session};
use nanus_kernel::{Context, Kernel, MountContext, Plugin, PluginId};
use nanus_ports::{
    ClockHandle, FsHandle, LlmEvent, LlmHandle, LlmPort, LlmStream, ReasoningEffort, SandboxPolicy,
    Secret, SecretHandle, SecretPort, ShellHandle, StoreHandle,
};

use crate::ToolRegistryHandle;
use crate::agent_loop::AgentRunner;
use crate::error::BundleError;
use crate::provider::{Provider, Selection};
use crate::selection::LastSelection;

/// The default system prompt.
///
/// Short on purpose. Every sentence here is a sentence the model reads on every
/// request, and a prompt that describes a tool's behaviour in prose duplicates what
/// the tool's own description already says.
pub const DEFAULT_SYSTEM_PROMPT: &str = "\
You are nanus, a coding agent working in a single workspace.

Use the tools available to you: read and search the workspace to understand it \
before changing it, edit files rather than rewriting them, and run commands to \
verify your work. Prefer finding out to assuming.

Your working directory is the workspace root. A non-zero exit code from `bash` is \
a result, not a failure of the tool: read the output and decide what to do next.";

/// The model adapter a composition falls back to when no provider credential is available.
///
/// It is not a provider and it never answers: every request fails with the sentence that says
/// how to configure one. It exists so an agent can *start* without a credential, so the
/// interface opens and the reader can run `/provider` — the failure then arrives where it can
/// be acted on, rather than as a refusal to open the program at all.
///
/// The model id it reports is the one the configuration resolved to, so the title bar still
/// names what *would* answer once a credential is supplied.
struct UnconfiguredLlm {
    /// The model id the resolved selection named.
    model: String,
    /// How to configure a provider, in the words the reader needs.
    reason: String,
}

impl LlmPort for UnconfiguredLlm {
    fn model(&self) -> &str {
        &self.model
    }

    fn stream_chat(&self, _request: nanus_ports::ChatRequest) -> LlmStream {
        // A stream that fails once, rather than a panic: the loop reads it like any other
        // model failure and reports it to whoever asked. Nothing is surfaced until a request
        // is made, which is what lets the agent start and the interface open first.
        let error = LlmEvent::Error(self.reason.clone());
        Box::pin(futures::stream::once(async move { error }))
    }
}

/// A composed harness: the kernel context, the runner, and the service handles.
///
/// The handles are kept so a caller can reach the store (to list sessions) or the
/// clock (to timestamp a new session) without resolving them from the context.
pub struct Harness {
    /// The running composition.
    pub context: Context,
    /// The loop, ready to run turns.
    pub runner: Rc<AgentRunner>,
    /// The session store.
    pub store: StoreHandle,
    /// The clock.
    pub clock: ClockHandle,
    /// What provider the harness is talking to, and how to change it.
    ///
    /// Replaces a bare adapter handle: the switch owns the model adapter the runner issues requests
    /// through, so a provider change reaches the loop rather than being a fact kept beside it.
    pub switch: Rc<ProviderSwitch>,
    /// Whether a provider credential was found, and therefore whether a request can run.
    ///
    /// `false` is the unconfigured agent: it started, and the interface it serves can configure
    /// a provider with `/provider`. The adapter in force is the placeholder that reports the
    /// missing credential on the first request.
    configured: bool,
    /// What a session created here is being run under.
    ///
    /// Kept on the harness rather than passed to [`Harness::new_session`], so a session is
    /// stamped by the composition that will actually run it: a caller cannot forget to
    /// pass it, and cannot pass a different one than the runner is using.
    origin: Origin,
}

impl core::fmt::Debug for Harness {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Harness")
            .field("model", &self.switch.model())
            .field("steps", &self.context.stats())
            .finish_non_exhaustive()
    }
}

impl Harness {
    /// Returns the number of tools the harness exposes to the model.
    #[must_use]
    pub fn tool_count(&self) -> usize {
        self.context
            .try_get(crate::tools_key())
            .map_or(0, |handle| handle.borrow().len())
    }

    /// Returns the model ids a client may switch this harness between.
    #[must_use]
    pub fn models(&self) -> Vec<String> {
        self.switch.models()
    }

    /// Returns the model adapter in force, for its model id.
    ///
    /// Read through the runner rather than kept beside it: a client may switch providers, which
    /// replaces the adapter, and a copy here would be a second answer to a question that has one.
    #[must_use]
    pub fn llm(&self) -> LlmHandle {
        self.runner.llm()
    }

    /// Returns whether a provider credential was found when this harness was built.
    ///
    /// `false` means the agent started unconfigured: it can be told to switch to a provider whose
    /// credential is stored, and until it is, every request fails with the sentence that says so.
    #[must_use]
    pub const fn configured(&self) -> bool {
        self.configured
    }

    /// Starts a new session.
    ///
    /// The session records what this composition is configured to do, so a transcript can
    /// say which model and which permission state produced it. A session loaded from the
    /// store keeps whatever it recorded when it was created: resuming does not restamp it,
    /// because the earlier part of the conversation really was produced by the earlier
    /// configuration and rewriting that would be a lie about the past.
    #[must_use]
    pub fn new_session(&self, workspace: &std::path::Path) -> Session {
        new_session(&self.clock, workspace).with_origin(self.origin.clone())
    }

    /// Tears the composition down, reverting every plugin's effects.
    ///
    /// # Errors
    ///
    /// Returns the first revert failure. Remaining plugins still revert, because a
    /// partially unwound context is more useful than one abandoned halfway.
    pub fn shutdown(&self) -> Result<(), BundleError> {
        self.context
            .shutdown()
            .map_err(|error| BundleError::Kernel(error.to_string()))
    }
}

/// A harness whose adapters are built but whose composition is not yet mounted.
///
/// This exists to keep two things apart that must not be mixed:
///
/// 1. **Async bootstrap.** Opening the session store awaits, and so does the agent
///    loop, so both must run while the caller is inside a runtime.
/// 2. **Synchronous composition.** The kernel drives its plugin hooks with its own
///    `block_on`, and `block_on` cannot be called from inside a runtime: doing so
///    panics with "cannot start a runtime from within a runtime".
///
/// So [`compose`] awaits everything that needs awaiting and returns this, and
/// [`Pending::start`] mounts the kernel after the caller has left the runtime. The
/// type is the seam that makes the ordering a compile-time fact rather than a
/// convention someone has to remember.
pub struct Pending {
    config: NanusConfig,
    workspace: PathBuf,
    /// What the configuration resolved to: the provider, plan, model, and endpoint.
    selection: Selection,
    /// Whether a credential was found for the selected provider.
    ///
    /// Carried from [`compose`] to [`Pending::start`], where it decides whether the harness
    /// reports itself configured.
    configured: bool,
    /// The credential stores, as the chain this composition consults.
    secrets: SecretHandle,
    fs: FsHandle,
    shell: ShellHandle,
    clock: ClockHandle,
    store: StoreHandle,
    llm: LlmHandle,
    /// The effort a remembered selection asked for, if any.
    ///
    /// Carried rather than folded into the configuration because the configuration's field has
    /// four steps and a model's scale can have seven: an effort of `none` or `max` is a step the
    /// interface can choose and the configuration cannot name. Applied in [`Pending::start`],
    /// where the runner it belongs to exists.
    remembered_effort: Option<ReasoningEffort>,
    /// The one tool registry this composition has: the runner's and the published service's.
    tools: ToolRegistryHandle,
}

impl core::fmt::Debug for Pending {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Pending")
            .field("model", &self.llm.model())
            .field("tools", &self.tools.borrow().len())
            .finish_non_exhaustive()
    }
}

impl Pending {
    /// Returns whether a provider credential was found for the selected provider.
    ///
    /// `false` is the unconfigured agent: it will mount, and every request through it fails
    /// with the sentence naming the command that stores a credential.
    #[must_use]
    pub const fn configured(&self) -> bool {
        self.configured
    }

    /// Mounts the composition and returns the running harness.
    ///
    /// Must be called **outside** any async runtime, because the kernel drives plugin
    /// hooks with its own `block_on`. [`compose`] followed by [`Pending::start`] from
    /// a synchronous context is the intended shape:
    ///
    /// ```no_run
    /// # use nanus_adapter_config::NanusConfig;
    /// # use nanus_bundle::compose::{compose, Pending};
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let config = NanusConfig::default();
    /// // Await the adapters, then leave the runtime before mounting.
    /// let pending = nanus_kernel::runtime::block_on(compose(&config))?;
    /// let harness = pending.start()?;
    /// # let _ = harness;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`BundleError::Kernel`] when a plugin fails to mount.
    pub fn start(self) -> Result<Harness, BundleError> {
        let context = mount(&self)?;
        let runner = build_runner(
            &self.llm,
            &self.tools,
            &self.config,
            &self.selection,
            &self.workspace,
        )?;
        // The effort a remembered selection asked for is put on the runner rather than into the
        // configuration, because it can name a step the configuration's field cannot. It goes on
        // before the origin is built, so the record says what the requests will carry rather
        // than what the configuration happened to say.
        if let Some(effort) = self.remembered_effort {
            runner.set_effort(Some(effort));
        }
        // Built before the adapters are moved into the harness, and from the same
        // selection the runner was built from, so the record cannot disagree with what
        // the run will do.
        let origin = origin_of(&self.config, &self.selection, runner.effort());
        // Postconditions: the request model is the resolved one, and the registry the
        // runner dispatches from is the one the context published. The second is the
        // property this whole construction exists to hold — a runner over a *copy* of the
        // toolset would advertise tools it could not dispatch.
        assert_eq!(runner.config().model, self.selection.model());
        assert!(Rc::ptr_eq(
            &self.tools.0,
            &context
                .get(crate::tools_key())
                .map_err(|error| BundleError::Kernel(error.to_string()))?
                .0
        ));
        let runner = Rc::new(runner);
        let switch = Rc::new(ProviderSwitch {
            config: self.config.clone(),
            secrets: self.secrets.clone(),
            runner: Rc::clone(&runner),
            selection: core::cell::RefCell::new(self.selection),
        });
        Ok(Harness {
            context,
            runner,
            store: self.store,
            clock: self.clock,
            switch,
            configured: self.configured,
            origin,
        })
    }
}

/// The provider a harness is talking to, and how to change it.
///
/// A provider change is a *recomposition* of one plugin: the model adapter is rebuilt from the
/// configuration, the credential store, and the provider table, and the runner is pointed at the
/// replacement. Nothing else moves — the tools, the store, the sessions, and the system prompt are
/// the same objects they were — which is what lets a conversation survive a switch.
///
/// The configuration is kept rather than only its resolution: a switch names a provider and a plan,
/// and everything else (the workspace, the ceilings, the sandbox) has to be resolved again from the
/// same file the startup used.
pub struct ProviderSwitch {
    config: NanusConfig,
    secrets: SecretHandle,
    runner: Rc<AgentRunner>,
    selection: core::cell::RefCell<Selection>,
}

impl ProviderSwitch {
    /// Returns the name of the provider in force.
    #[must_use]
    pub fn provider(&self) -> String {
        self.selection.borrow().provider().name().to_owned()
    }

    /// Returns the plan in force.
    #[must_use]
    pub fn plan(&self) -> String {
        self.selection.borrow().plan().name.to_owned()
    }

    /// Returns the model the current provider will name in its next request.
    #[must_use]
    pub fn model(&self) -> String {
        self.selection.borrow().model().to_owned()
    }

    /// Returns the model ids the provider in force offers, the one in use first.
    #[must_use]
    pub fn models(&self) -> Vec<String> {
        let selection = self.selection.borrow();
        let current = selection.model().to_owned();
        let mut models: Vec<String> = selection
            .models()
            .iter()
            .map(|id| (*id).to_owned())
            .collect();
        if !models.contains(&current) {
            models.insert(0, current);
        }
        models
    }

    /// Returns whether a credential is configured for `provider`'s `plan`.
    ///
    /// Asked of the store rather than remembered, because a key set from another terminal between
    /// two attempts has to be seen: the answer is what decides whether a switch can go ahead. The
    /// plan matters — z.ai's coding subscription has an account of its own, so an API key already
    /// stored is not one for it.
    pub async fn has_credential(&self, provider: Provider, plan: Option<&str>) -> bool {
        matches!(
            self.secrets.get(provider.credential_account(plan)).await,
            Ok(Some(ref secret)) if !secret.is_blank()
        )
    }

    /// Files a credential for `provider`'s `plan`.
    ///
    /// # Errors
    ///
    /// Returns [`BundleError::Config`] when the store refuses the write, so a key that could not be
    /// saved is a sentence rather than a switch that silently cannot happen.
    pub async fn set_credential(
        &self,
        provider: Provider,
        plan: Option<&str>,
        key: &str,
    ) -> Result<(), BundleError> {
        if key.trim().is_empty() {
            return Err(BundleError::config("a credential is not empty"));
        }
        let account = provider.credential_account(plan);
        self.secrets
            .set(account, key)
            .await
            .map_err(|error| BundleError::config(format!("{account}: {error}")))
    }

    /// Starts an authorization for `provider`'s `plan`.
    ///
    /// # Errors
    ///
    /// Returns [`BundleError::Config`] when the plan is reached with a key rather than an
    /// authorization, or when the authorization service cannot be reached.
    pub async fn begin_authorization(
        &self,
        provider: Provider,
        plan: Option<&str>,
    ) -> Result<crate::authorize::PendingAuth, BundleError> {
        crate::authorize::begin_for(provider, plan).await
    }

    /// Polls an authorization once, filing the token set when the user has finished.
    ///
    /// `false` means still waiting. The caller decides how long to keep asking; the service's own
    /// code expires on its own.
    ///
    /// # Errors
    ///
    /// Returns [`BundleError::Config`] when the service cannot be reached or refuses the poll, and
    /// when the store refuses the write.
    pub async fn poll_authorization(
        &self,
        pending: &crate::authorize::PendingAuth,
    ) -> Result<bool, BundleError> {
        crate::authorize::poll(&self.secrets, pending).await
    }

    /// Rebuilds the model adapter for `provider` and `plan`, and points the runner at it.
    ///
    /// Returns the model ids the new provider offers. The startup configuration's other fields are
    /// carried over, and its provider, plan, model, and endpoint are overwritten: a switch names a
    /// provider, not a whole configuration, so everything else has to stay what the file said.
    ///
    /// # Errors
    ///
    /// Returns [`BundleError::Config`] when the provider or plan is unknown, the plan is one this
    /// build refuses, or no credential is configured — the same resolutions composition performs, so
    /// a switch cannot land on a provider a fresh run would refuse.
    pub async fn switch(
        &self,
        provider: Provider,
        plan: Option<&str>,
    ) -> Result<Vec<String>, BundleError> {
        let mut config = self.config.clone();
        config.provider = Some(provider.name().to_owned());
        config.plan = plan.map(str::to_owned);
        // The model and the endpoint belong to the provider being left, so they are dropped and
        // resolved afresh: a DeepSeek id sent to OpenAI is a refused request, not a substitution.
        config.model = None;
        config.base_url = None;
        let selection = Selection::resolve(&config)?;
        let credential = resolve_credential(&self.secrets, &selection).await?;
        let llm = build_llm(&config, &selection, &credential)?;
        self.runner.set_llm(llm);
        self.runner.set_model(selection.model());
        // The effort in force was chosen for the model being left, and the steps a model takes
        // differ, so it goes back to the adapter's own default rather than naming a step the new
        // model may not accept.
        self.runner.set_effort(None);
        *self.selection.borrow_mut() = selection;
        Ok(self.models())
    }
}

/// Builds the record of what a session created here is being run under.
///
/// Read from the same selection the runner and the prompt are built from, so the
/// recorded facts are the ones in force rather than ones a caller restated. The effort is
/// the runner's own answer — the chosen step when a caller chose one, and otherwise what the
/// adapter applies to a request that sets none, which is the only place that answer exists.
/// It is left absent when the adapter has no notion of an effort: a provider without an effort
/// knob (Anthropic) is recorded as having none, which is an absence rather than a default.
fn origin_of(
    config: &NanusConfig,
    selection: &Selection,
    effort: Option<ReasoningEffort>,
) -> Origin {
    Origin {
        model: Some(selection.model().to_owned()),
        effort: effort.map(|effort| effort.as_str().to_owned()),
        sandbox: Some(config.sandbox_mode.as_str().to_owned()),
        approval: Some(config.approval_policy.as_str().to_owned()),
        harness: Some(format!("nanus/{}", env!("CARGO_PKG_VERSION"))),
    }
}

/// Overlays the selection a previous change left behind onto a configuration.
///
/// The record is written when an interface changes the provider, model, or effort of a running
/// agent (see [`LastSelection`]); this is the read that makes the next start begin there. A
/// configuration is complete without it, so an absent or unreadable record changes nothing.
///
/// Returns the remembered effort, which is *not* folded into the configuration: the
/// configuration's field has four steps and a model's scale can have seven, so `none` and `max`
/// have nowhere to go. The caller puts it on the runner instead.
pub fn apply_remembered(
    config: &mut NanusConfig,
    home: &std::path::Path,
) -> Option<ReasoningEffort> {
    let remembered = LastSelection::load(home).unwrap_or_default();
    remembered.apply(config);
    remembered.effort()
}

/// Builds the adapters a harness needs, leaving the kernel unmounted.
///
/// The caller awaits this, then leaves the runtime and calls [`Pending::start`]. See
/// [`Pending`] for why the two steps cannot be one.
///
/// # Errors
///
/// Returns [`BundleError::Config`] when the configuration or the API key is unusable,
/// and [`BundleError::Session`] when the session store cannot be opened.
pub async fn compose(config: &NanusConfig) -> Result<Pending, BundleError> {
    let home = store_home()?;
    // The default this start begins from is the selection the last *change* left behind, when
    // there is one: the configuration resolves everything else, and resolves all of it when
    // nothing was recorded. Read before the selection is resolved because that is the one place
    // a provider, plan, or model is decided.
    let mut config = config.clone();
    let remembered_effort = apply_remembered(&mut config, &home);
    let workspace = workspace_root(&config)?;
    // Which provider, plan, model, and endpoint this run uses is decided before
    // anything is built, because a configuration that cannot resolve is a sentence
    // rather than a partly assembled harness.
    let selection = Selection::resolve(&config)?;
    // The secret stores are consulted by *account*, which is the provider's name, so
    // a key stored for one provider can never be sent to another.
    let secrets = Secrets::new(&home).handle();
    // A missing credential is the one failure an agent may start *through*: the interface opens
    // so the reader can supply one with `/provider`, and the placeholder reports it on the first
    // request. Every other failure still refuses to compose.
    let (llm, configured) = match build_adapter(&config, &selection, &secrets).await {
        Ok(llm) => (llm, true),
        Err(BundleError::Credential(reason)) => {
            let placeholder: LlmHandle = Rc::new(Box::new(UnconfiguredLlm {
                model: selection.model().to_owned(),
                reason,
            }));
            (placeholder, false)
        }
        Err(other) => return Err(other),
    };

    // The adapters are built before the kernel mounts them, because several need to
    // await (opening a store) and a plugin's `mount` hook should not block on I/O that
    // could have failed earlier. Building them here also means a failure to open the
    // store is reported before anything is mounted, so there is nothing to unwind.
    let fs = LocalFs::new(workspace.clone())
        .map_err(|error| BundleError::config(error.to_string()))?
        .handle();
    let policy = SandboxPolicy::new(config.sandbox_mode, workspace.clone());
    let shell = LocalShell::new(policy).handle();
    let clock = SystemClock::new().handle();
    let store = JsonlStore::new(home)
        .await
        .map_err(|error| BundleError::session(error.to_string()))?
        .handle();
    let tools = build_tools(&fs, &shell)?;

    Ok(Pending {
        config,
        workspace,
        selection,
        configured,
        secrets,
        fs,
        shell,
        clock,
        store,
        llm,
        remembered_effort,
        tools,
    })
}

/// Builds the adapter for a resolved selection, reading the credential it needs.
///
/// The one place a selection becomes a runnable adapter.
///
/// # Errors
///
/// Returns [`BundleError::Credential`] when no store holds a credential for the provider, and
/// [`BundleError::Config`] when the adapter rejects the configuration.
async fn build_adapter(
    config: &NanusConfig,
    selection: &Selection,
    secrets: &SecretHandle,
) -> Result<LlmHandle, BundleError> {
    let credential = resolve_credential(secrets, selection).await?;
    build_llm(config, selection, &credential)
}

/// Resolves the credential the selected provider needs.
///
/// One call covers every store: the chain is the platform keychain, then the private
/// file, then the environment — so a keychain entry, a `nanus auth set` file, and a
/// variable a CI job exports are all answered here, and the first one that has a value
/// wins.
///
/// # Errors
///
/// Returns [`BundleError::Credential`] when no store holds a credential, and the sentence
/// names both ways to supply one, because either may be the one a reader can act on.
async fn resolve_credential(
    secrets: &SecretHandle,
    selection: &Selection,
) -> Result<Secret, BundleError> {
    let account = selection.credential_account();
    // A plan reached with an authorization has no variable to name and nothing to type: it is
    // completed in a browser, whichever door the reader came through, so the sentence names the two
    // commands that start it rather than a shell variable that does not exist.
    let hint = selection.credential_env().map_or_else(
        || {
            format!(
                "run `nanus auth login {account}` or choose it in the interface to authorize it"
            )
        },
        |env| format!("run `nanus auth set {account}`, or set {env}"),
    );
    match secrets.get(account).await {
        Ok(Some(secret)) if !secret.is_blank() => {
            // An authorization is a grant rather than a key, so an expired one is renewed here
            // rather than sent and refused.
            if selection.credential_is_oauth() {
                renew(secrets, selection, secret).await
            } else {
                Ok(secret)
            }
        }
        Ok(_) => Err(BundleError::credential(format!(
            "no credential for {account}: {hint}"
        ))),
        // The store failed rather than answering, so its own sentence is kept: a
        // locked keychain is a different problem from an unset key, and the fix is
        // different too.
        Err(error) => Err(BundleError::credential(format!(
            "no credential for {account}: {error}; {hint}"
        ))),
    }
}

/// How long before an access token's expiry it is renewed.
///
/// A request that leaves just before the expiry would arrive just after it, so the renewal happens
/// early rather than exactly on time.
const REFRESH_MARGIN: Duration = Duration::from_mins(5);

/// Renews an authorization whose access token is at or past expiry, storing the new set.
///
/// A grant is a long-lived refresh token and a short-lived access token, so an authorization older
/// than its access token is renewed here rather than sent and refused. The renewed set is written
/// back, so the next request does not renew again.
async fn renew(
    secrets: &SecretHandle,
    selection: &Selection,
    stored: Secret,
) -> Result<Secret, BundleError> {
    let account = selection.credential_account();
    let tokens = oauth::Tokens::decode(stored.expose())
        .map_err(|error| BundleError::config(format!("{account}: {error}")))?;
    if !tokens.is_expired(REFRESH_MARGIN) {
        return Ok(stored);
    }
    let renewed = oauth::refresh(oauth::ISSUER, &tokens).await.map_err(|error| {
        BundleError::config(format!(
            "{account} could not be renewed: {error}; run `nanus auth login {account}` to authorize \
             it again"
        ))
    })?;
    let encoded = renewed
        .encode()
        .map_err(|error| BundleError::config(format!("{account}: {error}")))?;
    secrets
        .set(account, &encoded)
        .await
        .map_err(|error| BundleError::config(format!("{account}: {error}")))?;
    Ok(Secret::new(encoded))
}

/// Opens the credential stores without composing a harness.
///
/// `nanus auth` needs the stores and nothing else — no model, no tools, no session
/// store — and demanding a credential in order to *store* one would be a circle. The
/// chain is the same one a run uses, so a key written here is found there.
///
/// # Errors
///
/// Returns [`BundleError::Session`] when no home directory can be determined, which is
/// where the private file store lives.
pub fn open_secrets() -> Result<SecretHandle, BundleError> {
    let home = store_home()?;
    Ok(Secrets::new(&home).handle())
}

/// Opens the session store without composing a harness.
///
/// Listing sessions needs no model and no tools, and demanding an API key to read one's
/// own transcripts would be a barrier with no purpose. This is the smallest composition
/// that answers "what have I run".
///
/// # Errors
///
/// Returns [`BundleError::Session`] when the store cannot be opened.
pub async fn open_store() -> Result<StoreHandle, BundleError> {
    let store = JsonlStore::new(store_home()?)
        .await
        .map_err(|error| BundleError::session(error.to_string()))?;
    Ok(store.handle())
}

/// Starts a session against `workspace`: a fresh id, the current time, and where it
/// belongs.
///
/// A free function rather than only a method on [`Harness`], because the three facts it
/// needs are not the harness's alone. An agent served over a link starts sessions too,
/// and it must start them the same way: a transcript that depended on which door a
/// session came through would not be a transcript of the agent.
#[must_use]
pub fn new_session(clock: &ClockHandle, workspace: &std::path::Path) -> Session {
    Session::new(
        nanus_adapter_store::new_session_id(),
        clock.now_ms(),
        workspace.display().to_string(),
    )
}

/// Returns the workspace root a run is confined to.
///
/// # Errors
///
/// Returns [`BundleError::Config`] when the configured root does not exist.
pub fn workspace_root(config: &NanusConfig) -> Result<PathBuf, BundleError> {
    let root = match &config.workspace_root {
        Some(configured) => configured.clone(),
        None => std::env::current_dir()
            .map_err(|error| BundleError::config(format!("no working directory: {error}")))?,
    };
    if !root.is_dir() {
        return Err(BundleError::config(format!(
            "the workspace root {} is not a directory",
            root.display()
        )));
    }
    // Canonicalised, so that everything downstream holds the same absolute path this check
    // just accepted. A relative `workspace_root` in the configuration file used to reach the
    // sandbox policy as written, and `ensure_within` asserts that a root is absolute — a
    // panic in the shell tool, from a configuration the file's own documentation allows.
    // (The filesystem adapter canonicalised all along; this is the shell agreeing with it.)
    std::fs::canonicalize(&root)
        .map_err(|error| BundleError::config(format!("the workspace root is unusable: {error}")))
}

/// Returns the harness home directory.
///
/// # Errors
///
/// Returns [`BundleError::Session`] when no home directory can be determined.
pub fn store_home() -> Result<PathBuf, BundleError> {
    nanus_adapter_store::resolve_home(None).map_err(|error| BundleError::session(error.to_string()))
}

/// The configured default has to fit inside the ceiling the shipped provider documents.
///
/// `build_llm` always overrides the adapter's own default with the configured one, so the
/// configuration's default is what every request sends. A default above the provider's
/// documented ceiling would ask for more output than the provider permits on every single
/// request, and that failure surfaces as a refused request rather than as anything pointing
/// back here. Stated as a constant so it fails the build instead of a run, and stated *here*
/// because this is the only module where both numbers are in scope.
const _: () = assert!(DEFAULT_MAX_TOKENS <= DEFAULT_MAX_OUTPUT_TOKENS);

/// Builds the model adapter the selection calls for.
///
/// This is the one place that maps a provider to an adapter, which is what keeps every
/// other crate ignorant of which vendor speaks which protocol.
fn build_llm(
    config: &NanusConfig,
    selection: &Selection,
    key: &Secret,
) -> Result<LlmHandle, BundleError> {
    let port: Box<dyn LlmPort> = match selection.provider() {
        Provider::DeepSeek => Box::new(build_deepseek(config, selection, key)?),
        Provider::Zai => Box::new(build_compatible(Vendor::Zai, config, selection, key)?),
        // Which wire an `OpenAI` request takes is the *plan's* fact, not the credential's: the
        // `ChatGPT` backend serves the Responses API because that is the endpoint it is, and the
        // subscription plan names it. The credential decides only how the request authenticates.
        Provider::OpenAi if matches!(selection.protocol(), Protocol::Responses) => {
            Box::new(build_responses(config, selection, key)?)
        }
        Provider::OpenAi => Box::new(build_compatible(Vendor::OpenAi, config, selection, key)?),
        Provider::Anthropic => Box::new(build_anthropic(config, selection, key)?),
    };
    Ok(Rc::new(port))
}

/// Builds the Responses adapter a `ChatGPT` subscription is reached through.
///
/// The credential is the stored token set rather than a key, so the access token becomes the bearer
/// and the `ChatGPT` account the token names is sent in its own header: those two are what the
/// subscription backend authorizes a request with.
///
/// # Errors
///
/// Returns [`BundleError::Config`] when the stored credential is not a token set — which is a
/// credential filed by something else, not a subscription — or when the adapter cannot be built.
/// Builds the client an OAuth-authorized `OpenAI` plan needs: the grant's access token as the
/// bearer, and the `ChatGPT` account named in its own header.
///
/// The wire is not set here — it comes from the plan, as it does for a keyed request — so the only
/// thing this adds is where the credential comes from: a token set rather than a key.
fn build_responses(
    config: &NanusConfig,
    selection: &Selection,
    key: &Secret,
) -> Result<OpenAiLlm, BundleError> {
    let tokens = oauth::Tokens::decode(key.expose())
        .map_err(|error| BundleError::config(error.to_string()))?;
    let mut adapter = compatible_config(Vendor::OpenAi, config, selection, &tokens.access_token)?;
    if let Some(account_id) = tokens.account_id {
        adapter.set_account_id(account_id);
    }
    OpenAiLlm::new(adapter).map_err(|error| BundleError::config(error.to_string()))
}

/// Builds the `DeepSeek` adapter.
fn build_deepseek(
    config: &NanusConfig,
    selection: &Selection,
    key: &Secret,
) -> Result<DeepSeekLlm, BundleError> {
    let mut adapter =
        DeepSeekConfig::with_base_url(selection.model(), key.expose(), selection.endpoint());
    adapter
        .set_max_tokens(config.max_tokens)
        .map_err(|error| BundleError::config(error.to_string()))?;
    // The configuration carries its own spelling of the effort so a TOML file can
    // name it; the adapter speaks the ports vocabulary.
    adapter.set_reasoning_effort(config.reasoning_effort.to_port());
    DeepSeekLlm::new(adapter).map_err(|error| BundleError::config(error.to_string()))
}

/// Builds an adapter for an `OpenAI`-compatible vendor.
fn build_compatible(
    vendor: Vendor,
    config: &NanusConfig,
    selection: &Selection,
    key: &Secret,
) -> Result<OpenAiLlm, BundleError> {
    let adapter = compatible_config(vendor, config, selection, key.expose())?;
    OpenAiLlm::new(adapter).map_err(|error| BundleError::config(error.to_string()))
}

/// Builds the configuration an `OpenAI`-compatible request is made from.
///
/// Shared by the two builders here, so the facts a plan carries — the endpoint, the wire it serves,
/// the output ceiling, the effort — are applied once, whichever way the request authenticates. The
/// wire is the plan's and not the credential's: an API endpoint serves `chat/completions` and the
/// `ChatGPT` backend serves `responses`, and a subscription is that backend whether the grant
/// arrives as a key or as a token set.
///
/// # Errors
///
/// Returns [`BundleError::Config`] when the configuration asks for more output than the adapter's
/// ceiling permits.
fn compatible_config(
    vendor: Vendor,
    config: &NanusConfig,
    selection: &Selection,
    key: &str,
) -> Result<OpenAiConfig, BundleError> {
    let mut adapter =
        OpenAiConfig::with_base_url(vendor, selection.model(), key, selection.endpoint());
    adapter.set_protocol(selection.protocol());
    adapter
        .set_max_tokens(config.max_tokens)
        .map_err(|error| BundleError::config(error.to_string()))?;
    adapter.set_reasoning_effort(config.reasoning_effort.to_port());
    Ok(adapter)
}

/// Builds the `Anthropic` adapter.
///
/// The configured reasoning effort is deliberately **not** applied: Anthropic's
/// thinking needs the signed blocks of the previous turn replayed, which the message
/// model has no place for, so the adapter never asks for thinking — see its crate
/// documentation. Passing the effort in would imply it reached the wire.
fn build_anthropic(
    config: &NanusConfig,
    selection: &Selection,
    key: &Secret,
) -> Result<AnthropicLlm, BundleError> {
    let mut adapter =
        AnthropicConfig::with_base_url(selection.model(), key.expose(), selection.endpoint());
    adapter
        .set_max_tokens(config.max_tokens)
        .map_err(|error| BundleError::config(error.to_string()))?;
    AnthropicLlm::new(adapter).map_err(|error| BundleError::config(error.to_string()))
}

/// Builds the tool registry this composition shares.
///
/// Built once, here, and handed to both the runner and the plugin that publishes it: the
/// registry the model is offered and the registry an agent advertises have to be one object
/// or the two can disagree.
fn build_tools(fs: &FsHandle, shell: &ShellHandle) -> Result<ToolRegistryHandle, BundleError> {
    crate::build_toolset(fs, shell)
        .map(ToolRegistryHandle::new)
        .map_err(|error| BundleError::config(format!("the toolset could not be built: {error}")))
}

/// Builds the agent runner.
///
/// The runtime context is appended here rather than written into [`DEFAULT_SYSTEM_PROMPT`],
/// for the same reason the step budget is appended by the runner: it describes *this*
/// deployment — where the tools are rooted, which model answers, and what the two
/// permission knobs are — and a user-supplied prompt must receive it too. A model that does
/// not know it is writing under `read_only` cannot pace its work against that, and one that
/// does not know the approval policy cannot tell a refusal it can ask about from one it
/// cannot.
fn build_runner(
    llm: &LlmHandle,
    tools: &ToolRegistryHandle,
    config: &NanusConfig,
    selection: &Selection,
    workspace: &std::path::Path,
) -> Result<AgentRunner, BundleError> {
    let prompt = config
        .system_prompt
        .clone()
        .unwrap_or_else(|| DEFAULT_SYSTEM_PROMPT.to_owned());
    let runtime = nanus_domain::runtime_context(
        &workspace.display().to_string(),
        selection.model(),
        config.approval_policy,
        config.sandbox_mode,
    );
    let agent = AgentConfig::new(
        config.max_steps_per_turn,
        config.max_parallel_tools,
        selection.model().to_owned(),
        AGENT_SYSTEM_PROMPT_MAX,
    )
    .map_err(|error| BundleError::config(error.to_string()))?
    .with_context_budget(config.context_budget)
    .map_err(|error| BundleError::config(error.to_string()))?
    .with_approval(config.approval_policy)
    .with_sandbox(config.sandbox_mode);
    // The runner is given the same handle the context publishes, so registering a tool
    // later is visible on the next request rather than requiring a rebuild.
    AgentRunner::new(
        Rc::clone(llm),
        tools.clone(),
        format!("{prompt}\n\n{runtime}"),
        agent,
    )
}

/// Mounts the adapters and the tool provider on a kernel.
///
/// Synchronous on purpose: the kernel's activation sweep calls `block_on`, which would
/// panic if this ran inside a runtime. [`Pending::start`] is the only caller, and it is
/// documented as synchronous.
fn mount(pending: &Pending) -> Result<Context, BundleError> {
    let kernel = Kernel::new()
        .with_plugin(plugin_id("clock"), clock_provider(&pending.clock))
        .with_plugin(plugin_id("fs"), fs_provider(&pending.fs))
        .with_plugin(plugin_id("shell"), shell_provider(&pending.shell))
        .with_plugin(plugin_id("store"), store_provider(&pending.store))
        .with_plugin(plugin_id("llm"), llm_provider(&pending.llm))
        .with_plugin(plugin_id("secrets"), secret_provider(&pending.secrets))
        .with_plugin(
            plugin_id("tools"),
            crate::tools_plugin(pending.tools.clone()),
        );
    kernel
        .start()
        .map_err(|error| BundleError::Kernel(error.to_string()))
}

/// Builds a plugin id, with the panic reserved for a literal that cannot be invalid.
fn plugin_id(raw: &'static str) -> PluginId {
    PluginId::new(raw).unwrap_or_else(|_| unreachable!("a shipped plugin id is valid"))
}

/// A plugin that publishes one already-built port handle.
///
/// The adapters are constructed before the kernel mounts them, so each one arrives
/// as a ready handle rather than as something whose `mount` hook has to await. The
/// key comes from the ports crate, so a provider and its consumer cannot disagree
/// about a name.
// The published value is an `Rc` of the handle, and the handle is itself an `Rc`, so
// the field is an `Rc<Rc<T>>`. That is the kernel's registry shape, not an accident:
// `ServiceKey<T>` publishes an `Rc<T>`, and for a port `T` is the shared handle.
#[allow(clippy::redundant_allocation)] // see the note below
struct PortProvider<T: 'static> {
    name: &'static str,
    key: nanus_kernel::ServiceKey<Rc<T>>,
    value: Rc<Rc<T>>,
}

impl<T: 'static> PortProvider<T> {
    /// Builds a provider for `key`.
    fn new(name: &'static str, key: nanus_kernel::ServiceKey<Rc<T>>, value: Rc<T>) -> Self {
        Self {
            name,
            key,
            value: Rc::new(value),
        }
    }
}

impl<T: 'static> Plugin for PortProvider<T> {
    fn id(&self) -> PluginId {
        plugin_id(self.name)
    }

    fn description(&self) -> &'static str {
        "publishes a pre-built port handle"
    }

    fn init(&mut self, _cx: &Context) -> nanus_kernel::PluginFuture {
        Box::pin(async { Ok(()) })
    }

    fn mount(&mut self, cx: &mut MountContext<'_>) -> nanus_kernel::PluginFuture {
        // The registration is an effect, so it is recorded synchronously and only the
        // outcome is carried into the future. Awaiting nothing keeps the hook honest
        // about what it does. The kernel publishes an `Rc` of the handle, so the
        // stored value is the handle itself.
        let outcome = cx.provide(self.key, self.value.clone());
        Box::pin(async move { outcome })
    }

    fn unmount(&mut self, _cx: &Context) -> nanus_kernel::PluginFuture {
        Box::pin(async { Ok(()) })
    }
}

/// Builds the plugin that publishes the clock.
fn clock_provider(clock: &ClockHandle) -> PortProvider<Box<dyn nanus_ports::ClockPort>> {
    PortProvider::new("clock", nanus_ports::clock_key(), clock.clone())
}

/// Builds the plugin that publishes the filesystem.
fn fs_provider(fs: &FsHandle) -> PortProvider<Box<dyn nanus_ports::FsPort>> {
    PortProvider::new("fs", nanus_ports::fs_key(), fs.clone())
}

/// Builds the plugin that publishes the shell.
fn shell_provider(shell: &ShellHandle) -> PortProvider<Box<dyn nanus_ports::ShellPort>> {
    PortProvider::new("shell", nanus_ports::shell_key(), shell.clone())
}

/// Builds the plugin that publishes the session store.
fn store_provider(store: &StoreHandle) -> PortProvider<Box<dyn nanus_ports::StorePort>> {
    PortProvider::new("store", nanus_ports::store_key(), store.clone())
}

/// Builds the plugin that publishes the model adapter.
///
/// The published handle is the adapter the composition *started* with. A `/provider` replaces the
/// one the runner issues requests through (see [`AgentRunner::set_llm`]), and the runner is the
/// authority for everything about a request — so this service is a description of the deployment
/// rather than a live view of the adapter in force, and nothing mounted reads it. It is published
/// so a future plugin finds the capability by key rather than by being handed it.
fn llm_provider(llm: &LlmHandle) -> PortProvider<Box<dyn LlmPort>> {
    PortProvider::new("llm", nanus_ports::llm_key(), llm.clone())
}

/// Builds the plugin that publishes the credential stores.
///
/// Published like every other port so the capability is a service rather than a value
/// a caller has to be handed: nothing mounted today requires it — the composition
/// resolves the credential before the kernel starts — but a plugin that later needs to
/// read or write one finds it by key rather than by being passed it.
fn secret_provider(secrets: &SecretHandle) -> PortProvider<Box<dyn SecretPort>> {
    PortProvider::new("secrets", nanus_ports::secret_key(), secrets.clone())
}

/// Maximum size of an assembled system prompt.
const AGENT_SYSTEM_PROMPT_MAX: usize = 32_768;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_prompt_names_the_workspace_and_the_exit_code_rule() {
        // A prompt that omits the exit-code rule leads a model to treat `grep`
        // finding nothing as a broken tool.
        assert!(DEFAULT_SYSTEM_PROMPT.contains("exit code"));
        assert!(DEFAULT_SYSTEM_PROMPT.contains("workspace"));
    }

    #[test]
    fn a_workspace_that_is_not_a_directory_is_refused() {
        let config = NanusConfig {
            workspace_root: Some(PathBuf::from("/definitely/not/a/directory/here")),
            ..NanusConfig::default()
        };
        let outcome = workspace_root(&config);
        assert!(matches!(outcome, Err(BundleError::Config(_))));
    }

    /// A relative configured root is made absolute, so nothing downstream meets a path the
    /// sandbox policy asserts is absolute. The shell adapter resolves a `workdir` against the
    /// policy root and `ensure_within` asserts that root is absolute, so a relative
    /// `workspace_root` in the configuration file used to panic the shell tool.
    #[test]
    fn a_relative_workspace_root_is_made_absolute() {
        let config = NanusConfig {
            workspace_root: Some(PathBuf::from(".")),
            ..NanusConfig::default()
        };
        let root = workspace_root(&config).expect("the current directory is a directory");
        assert!(
            root.is_absolute(),
            "the root everything is confined to is absolute: {}",
            root.display()
        );
    }

    #[test]
    fn the_current_directory_is_the_default_workspace() {
        let config = NanusConfig::default();
        let outcome = workspace_root(&config);
        assert!(outcome.is_ok());
    }

    /// The placeholder an unconfigured agent talks through: it names the model that would answer
    /// and reports the missing credential on the first request rather than panicking or hanging.
    ///
    /// Polling the stream is what a turn does, so this is the path a reader's first prompt takes.
    #[test]
    fn an_unconfigured_adapter_reports_the_missing_credential_when_asked() {
        use futures::StreamExt as _;
        let adapter = UnconfiguredLlm {
            model: String::from("deepseek-flash"),
            reason: String::from("no credential for deepseek: run `nanus auth set deepseek`"),
        };
        // Nothing is surfaced until a request is made: the model is named, so an interface has
        // something to draw, and no failure has happened yet.
        assert_eq!(adapter.model(), "deepseek-flash");
        let request = nanus_ports::ChatRequest::new("deepseek-flash", Vec::new());
        let collected: Vec<LlmEvent> =
            nanus_kernel::runtime::block_on(adapter.stream_chat(request).collect());
        assert!(
            matches!(collected.as_slice(), [LlmEvent::Error(message)]
                if message.contains("nanus auth set deepseek")),
            "{collected:?}"
        );
    }

    /// A `SecretPort` holding nothing, so the unconfigured case does not depend on whether the
    /// machine running the test happens to have a key in its keychain or environment.
    struct NoSecrets;

    impl SecretPort for NoSecrets {
        fn get<'a>(
            &'a self,
            _account: &'a str,
        ) -> nanus_ports::LocalBoxFuture<'a, nanus_ports::SecretResult<Option<Secret>>> {
            Box::pin(async { Ok(None) })
        }

        fn set<'a>(
            &'a self,
            _account: &'a str,
            _secret: &'a str,
        ) -> nanus_ports::LocalBoxFuture<'a, nanus_ports::SecretResult<()>> {
            Box::pin(async { Ok(()) })
        }

        fn clear<'a>(
            &'a self,
            _account: &'a str,
        ) -> nanus_ports::LocalBoxFuture<'a, nanus_ports::SecretResult<bool>> {
            Box::pin(async { Ok(false) })
        }

        fn backend(&self) -> &'static str {
            "none"
        }
    }

    /// A missing credential is the one configuration failure the composition distinguishes: it is
    /// reported as [`BundleError::Credential`], which is what lets [`compose`] start unconfigured
    /// instead of refusing. The sentence names the command that stores one.
    #[test]
    fn a_missing_credential_is_reported_as_a_credential_error() {
        let selection = Selection::resolve(&NanusConfig::default()).expect("the default resolves");
        let secrets: SecretHandle = Rc::new(Box::new(NoSecrets));
        let outcome = nanus_kernel::runtime::block_on(resolve_credential(&secrets, &selection));
        let Err(BundleError::Credential(message)) = outcome else {
            panic!("a missing credential is reported as a credential error");
        };
        assert!(message.contains("nanus auth set"), "{message}");
    }
}
