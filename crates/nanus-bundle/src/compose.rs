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

use nanus_adapter_config::{DEFAULT_MAX_TOKENS, NanusConfig};
use nanus_adapter_deepseek::{DEFAULT_MAX_OUTPUT_TOKENS, DeepSeekConfig, DeepSeekLlm};
use nanus_adapter_local::{LocalFs, LocalShell, SystemClock};
use nanus_adapter_store::JsonlStore;
use nanus_domain::{AgentConfig, Origin, Session};
use nanus_kernel::{Context, Kernel, MountContext, Plugin, PluginId};
use nanus_ports::{
    ClockHandle, FsHandle, LlmHandle, LlmPort, SandboxPolicy, ShellHandle, StoreHandle,
};

use crate::ToolRegistryHandle;
use crate::agent_loop::AgentRunner;
use crate::error::BundleError;

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
    /// The model adapter, for its model id.
    pub llm: LlmHandle,
    /// The model ids a client may switch this harness between, in cycling order.
    ///
    /// Taken from [`crate::model_ids`] rather than from the adapter, because which ids are
    /// offered is a decision about this deployment and not about the provider: the adapter
    /// accepts what it is told, and the list is what the interface may name.
    models: Vec<String>,
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
            .field("model", &self.llm.model())
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
    pub fn models(&self) -> &[String] {
        &self.models
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
    fs: FsHandle,
    shell: ShellHandle,
    clock: ClockHandle,
    store: StoreHandle,
    llm: LlmHandle,
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
        let context = mount(
            &self.fs,
            &self.shell,
            &self.clock,
            &self.store,
            &self.llm,
            &self.tools,
        )?;
        let runner = build_runner(&self.llm, &self.tools, &self.config, &self.workspace)?;
        // Built before the adapters are moved into the harness, and from the same
        // configuration the runner was built from, so the record cannot disagree with what
        // the run will do.
        let origin = origin_of(&self.config, &self.llm);
        // Postconditions: the request model is the configured one, and the registry the
        // runner dispatches from is the one the context published. The second is the
        // property this whole construction exists to hold — a runner over a *copy* of the
        // toolset would advertise tools it could not dispatch.
        assert_eq!(runner.config().model, self.config.model);
        assert!(Rc::ptr_eq(
            &self.tools.0,
            &context
                .get(crate::tools_key())
                .map_err(|error| BundleError::Kernel(error.to_string()))?
                .0
        ));
        Ok(Harness {
            context,
            runner: Rc::new(runner),
            store: self.store,
            clock: self.clock,
            llm: self.llm,
            models: crate::model_ids()
                .iter()
                .map(|id| (*id).to_owned())
                .collect(),
            origin,
        })
    }
}

/// Builds the record of what a session created here is being run under.
///
/// Read from the same configuration the runner and the prompt are built from, so the
/// recorded facts are the ones in force rather than ones a caller restated. The effort is
/// asked of the adapter instead, because an adapter fills in an unset effort from its own
/// configuration — which is the only place the answer exists — and it is left absent when
/// the adapter has no notion of one.
fn origin_of(config: &NanusConfig, llm: &LlmHandle) -> Origin {
    Origin {
        model: Some(config.model.clone()),
        effort: llm
            .reasoning_effort()
            .map(|effort| effort.as_str().to_owned()),
        sandbox: Some(config.sandbox_mode.as_str().to_owned()),
        approval: Some(config.approval_policy.as_str().to_owned()),
        harness: Some(format!("nanus/{}", env!("CARGO_PKG_VERSION"))),
    }
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
    let workspace = workspace_root(config)?;
    let api_key = nanus_adapter_config::api_key().ok_or_else(|| {
        BundleError::config(format!(
            "no DeepSeek API key: set {}",
            nanus_adapter_deepseek::API_KEY_ENV
        ))
    })?;

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
    let store = JsonlStore::new(store_home()?)
        .await
        .map_err(|error| BundleError::session(error.to_string()))?
        .handle();
    let llm = build_llm(config, &api_key)?;
    let tools = build_tools(&fs, &shell)?;

    Ok(Pending {
        config: config.clone(),
        workspace,
        fs,
        shell,
        clock,
        store,
        llm,
        tools,
    })
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

/// Builds the model adapter for `config`.
fn build_llm(config: &NanusConfig, api_key: &str) -> Result<LlmHandle, BundleError> {
    let mut adapter = DeepSeekConfig::new(config.model.clone(), api_key);
    adapter
        .set_max_tokens(config.max_tokens)
        .map_err(|error| BundleError::config(error.to_string()))?;
    // The configuration carries its own spelling of the effort so a TOML file can
    // name it; the adapter speaks the ports vocabulary.
    adapter.set_reasoning_effort(config.reasoning_effort.to_port());
    let llm = DeepSeekLlm::new(adapter).map_err(|error| BundleError::config(error.to_string()))?;
    let port: Box<dyn LlmPort> = Box::new(llm);
    Ok(Rc::new(port))
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
    workspace: &std::path::Path,
) -> Result<AgentRunner, BundleError> {
    let prompt = config
        .system_prompt
        .clone()
        .unwrap_or_else(|| DEFAULT_SYSTEM_PROMPT.to_owned());
    let runtime = nanus_domain::runtime_context(
        &workspace.display().to_string(),
        &config.model,
        config.approval_policy,
        config.sandbox_mode,
    );
    let agent = AgentConfig::new(
        config.max_steps_per_turn,
        config.max_parallel_tools,
        config.model.clone(),
        AGENT_SYSTEM_PROMPT_MAX,
    )
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
fn mount(
    fs: &FsHandle,
    shell: &ShellHandle,
    clock: &ClockHandle,
    store: &StoreHandle,
    llm: &LlmHandle,
    tools: &ToolRegistryHandle,
) -> Result<Context, BundleError> {
    let kernel = Kernel::new()
        .with_plugin(plugin_id("clock"), clock_provider(clock))
        .with_plugin(plugin_id("fs"), fs_provider(fs))
        .with_plugin(plugin_id("shell"), shell_provider(shell))
        .with_plugin(plugin_id("store"), store_provider(store))
        .with_plugin(plugin_id("llm"), llm_provider(llm))
        .with_plugin(plugin_id("tools"), crate::tools_plugin(tools.clone()));
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
fn llm_provider(llm: &LlmHandle) -> PortProvider<Box<dyn LlmPort>> {
    PortProvider::new("llm", nanus_ports::llm_key(), llm.clone())
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
}
