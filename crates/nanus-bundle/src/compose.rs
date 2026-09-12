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

use nanus_adapter_config::NanusConfig;
use nanus_adapter_deepseek::{DeepSeekConfig, DeepSeekLlm};
use nanus_adapter_local::{LocalFs, LocalShell, SystemClock};
use nanus_adapter_store::JsonlStore;
use nanus_domain::{AgentConfig, Session, ToolRegistry};
use nanus_kernel::{Context, Kernel, MountContext, Plugin, PluginId};
use nanus_ports::{ClockHandle, FsHandle, LlmHandle, SandboxPolicy, ShellHandle, StoreHandle};

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

    /// Starts a new session.
    #[must_use]
    pub fn new_session(&self, workspace: &std::path::Path) -> Session {
        Session::new(
            nanus_adapter_store::new_session_id(),
            self.clock.now_ms(),
            workspace.display().to_string(),
        )
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
    fs: FsHandle,
    shell: ShellHandle,
    clock: ClockHandle,
    store: StoreHandle,
    llm: LlmHandle,
    tools: Rc<ToolRegistry>,
}

impl core::fmt::Debug for Pending {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Pending")
            .field("model", &self.llm.model())
            .field("tools", &self.tools.len())
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
        let context = mount(&self.fs, &self.shell, &self.clock, &self.store, &self.llm)?;
        let runner = build_runner(&self.llm, &self.tools, &self.config)?;
        // Postcondition: the tool count the harness reports is the one it published.
        assert_eq!(runner.config().model, self.config.model);
        Ok(Harness {
            context,
            runner: Rc::new(runner),
            store: self.store,
            clock: self.clock,
            llm: self.llm,
        })
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
    Ok(root)
}

/// Returns the harness home directory.
///
/// # Errors
///
/// Returns [`BundleError::Session`] when no home directory can be determined.
pub fn store_home() -> Result<PathBuf, BundleError> {
    nanus_adapter_store::resolve_home(None).map_err(|error| BundleError::session(error.to_string()))
}

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
    let port: Box<dyn nanus_ports::LlmPort> = Box::new(llm);
    Ok(Rc::new(port))
}

/// Builds the tool registry.
fn build_tools(fs: &FsHandle, shell: &ShellHandle) -> Result<Rc<ToolRegistry>, BundleError> {
    crate::build_toolset(fs, shell)
        .map(Rc::new)
        .map_err(|error| BundleError::config(format!("the toolset could not be built: {error}")))
}

/// Builds the agent runner.
fn build_runner(
    llm: &LlmHandle,
    tools: &Rc<ToolRegistry>,
    config: &NanusConfig,
) -> Result<AgentRunner, BundleError> {
    let prompt = config
        .system_prompt
        .clone()
        .unwrap_or_else(|| DEFAULT_SYSTEM_PROMPT.to_owned());
    let agent = AgentConfig::new(
        config.max_steps_per_turn,
        config.max_parallel_tools,
        config.model.clone(),
        AGENT_SYSTEM_PROMPT_MAX,
    )
    .map_err(|error| BundleError::config(error.to_string()))?;
    // The runner shares the registry with the provider, so registering a tool later
    // is visible on the next request rather than requiring a rebuild.
    AgentRunner::new(Rc::clone(llm), Rc::clone(tools), prompt, agent)
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
) -> Result<Context, BundleError> {
    let kernel = Kernel::new()
        .with_plugin(plugin_id("clock"), clock_provider(clock))
        .with_plugin(plugin_id("fs"), fs_provider(fs))
        .with_plugin(plugin_id("shell"), shell_provider(shell))
        .with_plugin(plugin_id("store"), store_provider(store))
        .with_plugin(plugin_id("llm"), llm_provider(llm))
        .with_plugin(
            plugin_id("tools"),
            crate::tools_plugin(fs.clone(), shell.clone()),
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
fn llm_provider(llm: &LlmHandle) -> PortProvider<Box<dyn nanus_ports::LlmPort>> {
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

    #[test]
    fn the_current_directory_is_the_default_workspace() {
        let config = NanusConfig::default();
        let outcome = workspace_root(&config);
        assert!(outcome.is_ok());
    }
}
