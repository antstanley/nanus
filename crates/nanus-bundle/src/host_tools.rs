//! Tools a host registers beside the stock seven.
//!
//! An embedding host — an application that links `nanus-bundle` rather than running the
//! `nanus` binary — often has a capability of its own to hand the model: a browser it
//! drives, a document it owns, a service only it can reach. This module is the supported
//! way to add one. `read_video` is installed the same way, so a host's tool is a peer of a
//! shipped optional tool rather than a patch on the toolset.
//!
//! ## Two doors, one rule
//!
//! - **Stock composition.** [`Pending::with_tools`](crate::compose::Pending::with_tools)
//!   queues an installer that [`Pending::start`](crate::compose::Pending::start) runs once the
//!   runner and the provider switch exist and before the harness is returned. A tool installed
//!   there is in the count the link handshake advertises and in the first request's schemas.
//! - **A runner the host builds itself** (`default-features = false`). The host owns the
//!   [`ToolRegistry`] it hands to [`AgentRunner::new`](crate::AgentRunner::new), and
//!   [`register`] is the same checked registration for it.
//!
//! Either way a name the loop runs itself is refused rather than shadowed: the loop answers a
//! goal-tool or context-tool call before the registry is consulted, so a registered tool by
//! one of those names could never run. A name already taken is refused by the registry, so a
//! host's tool never replaces a stock one.
//!
//! ## What a host tool declares
//!
//! Its [`ToolAccess`](nanus_domain::ToolAccess) decides which sandbox modes run it without
//! asking, and its [`Destructive`](nanus_domain::Destructive) declaration decides whether the
//! `permitted` approval state does. Both default to the most restrictive value, so a tool that
//! declares nothing is asked about until its author says what it does. `docs/host-tools.md` is
//! the guide.

use nanus_domain::{ToolDefinition, ToolName, ToolRegistry};

use crate::error::BundleError;

/// Returns whether `name` belongs to a tool the agent loop runs itself.
///
/// The five goal tools and the two context tools are answered by the loop before the
/// registry is consulted, whether or not the session enables managed context, so a registered
/// tool by one of these names would be offered to the model and never run.
#[must_use]
pub fn is_reserved(name: &ToolName) -> bool {
    crate::goal_tools::is_goal_tool(name) || crate::context_tools::is_context_tool(name)
}

/// Registers a host's tool, refusing a name the loop reserves or the registry already holds.
///
/// # Errors
///
/// Returns [`BundleError::Config`] naming the tool when the name is reserved (see
/// [`is_reserved`]) or already registered. Nothing is registered in either case.
pub fn register(
    registry: &mut ToolRegistry,
    definition: ToolDefinition,
) -> Result<(), BundleError> {
    let name = definition.name().clone();
    if is_reserved(&name) {
        return Err(BundleError::config(format!(
            "the tool {name} cannot be registered: the agent loop runs a tool by that name \
             itself, so a registered one would never be called"
        )));
    }
    let before = registry.len();
    registry.register(definition).map_err(|error| {
        BundleError::config(format!("the tool {name} cannot be registered: {error}"))
    })?;
    // Postcondition: exactly one tool was added, under the name the definition carried.
    assert_eq!(registry.len(), before.saturating_add(1));
    assert!(registry.contains(&name));
    Ok(())
}

#[cfg(feature = "stock-compose")]
pub use stock::{HostTools, ToolInstaller};

#[cfg(feature = "stock-compose")]
mod stock {
    use std::path::Path;
    use std::rc::{Rc, Weak};

    use nanus_domain::ToolDefinition;
    use nanus_ports::{ClockHandle, FsHandle, ShellHandle};

    use crate::ToolRegistryHandle;
    use crate::compose::ProviderSwitch;
    use crate::error::BundleError;

    /// A queued installer: what [`Pending::with_tools`](crate::compose::Pending::with_tools)
    /// keeps until the harness starts.
    pub type ToolInstaller = Box<dyn FnOnce(&mut HostTools<'_>) -> Result<(), BundleError>>;

    /// What an installer is handed: the registry to add to, and the handles a tool is built on.
    ///
    /// The handles are the composition's own — the rooted filesystem, the process-group shell,
    /// the clock — so a host's tool reads and runs inside the same workspace and under the same
    /// rules as the stock tools. The registry is borrowed only inside [`register`](Self::register),
    /// so an installer may build a tool that asks the runner something without tripping over a
    /// borrow it is itself holding.
    pub struct HostTools<'a> {
        pub(crate) tools: &'a ToolRegistryHandle,
        pub(crate) fs: &'a FsHandle,
        pub(crate) shell: &'a ShellHandle,
        pub(crate) clock: &'a ClockHandle,
        pub(crate) workspace: &'a Path,
        pub(crate) switch: &'a Rc<ProviderSwitch>,
    }

    impl HostTools<'_> {
        /// Registers a tool; see [`crate::host_tools::register`].
        ///
        /// # Errors
        ///
        /// As [`crate::host_tools::register`]: a reserved or already-registered name.
        pub fn register(&mut self, definition: ToolDefinition) -> Result<(), BundleError> {
            super::register(&mut self.tools.borrow_mut(), definition)
        }

        /// Returns whether a tool by `name` is already registered.
        #[must_use]
        pub fn contains(&self, name: &str) -> bool {
            nanus_domain::ToolName::new(name).is_ok_and(|name| self.tools.borrow().contains(&name))
        }

        /// The composition's filesystem, rooted at the workspace.
        #[must_use]
        pub fn fs(&self) -> FsHandle {
            Rc::clone(self.fs)
        }

        /// The composition's shell, which kills a call's whole process group (a Job Object on
        /// Windows) when the call ends.
        #[must_use]
        pub fn shell(&self) -> ShellHandle {
            Rc::clone(self.shell)
        }

        /// The composition's clock.
        #[must_use]
        pub fn clock(&self) -> ClockHandle {
            Rc::clone(self.clock)
        }

        /// The workspace root every stock tool is confined to.
        #[must_use]
        pub const fn workspace(&self) -> &Path {
            self.workspace
        }

        /// The provider switch, for a tool that asks a model something of its own.
        ///
        /// Weak, and it must stay weak inside the tool: the switch owns the runner, the runner
        /// owns the registry, and the registry owns the tool, so a strong handle would be a cycle
        /// that keeps the whole harness alive. Upgrade it per call and report a failed upgrade
        /// as the tool's own failure, as `read_video` does.
        #[must_use]
        pub fn switch(&self) -> Weak<ProviderSwitch> {
            Rc::downgrade(self.switch)
        }
    }
}

#[cfg(test)]
mod tests {
    use nanus_domain::{ToolCall, ToolFuture, ToolResult, ToolSchema};

    use super::*;

    struct Idle;

    impl nanus_domain::ToolExecutor for Idle {
        fn execute(&self, call: ToolCall) -> ToolFuture {
            Box::pin(async move { ToolResult::failure(call.id, "unused") })
        }
    }

    fn tool(name: &str) -> ToolDefinition {
        let schema = ToolSchema {
            name: ToolName::new(name).unwrap_or_else(|_| panic!("a valid tool name")),
            description: String::from("a host tool"),
            parameters: serde_json::json!({ "type": "object" }),
        };
        ToolDefinition::new(schema, Idle)
    }

    #[test]
    fn a_host_tool_registers_under_a_free_name() {
        let mut registry = ToolRegistry::new();
        let registered = register(&mut registry, tool("host_lookup"));
        assert!(registered.is_ok(), "{registered:?}");
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn a_name_the_loop_runs_itself_is_refused() {
        for name in crate::goal_tools::NAMES
            .iter()
            .copied()
            .chain(crate::context_tools::NAMES.iter().copied())
        {
            let name_ok = ToolName::new(name).unwrap_or_else(|_| panic!("a valid tool name"));
            assert!(is_reserved(&name_ok), "{name} is reserved");
            let mut registry = ToolRegistry::new();
            let refused = register(&mut registry, tool(name));
            assert!(
                matches!(&refused, Err(BundleError::Config(message)) if message.contains(name)),
                "{name}: {refused:?}"
            );
            assert!(registry.is_empty(), "nothing is registered for {name}");
        }
    }

    #[test]
    fn a_taken_name_is_refused_rather_than_replaced() {
        let mut registry = ToolRegistry::new();
        assert!(register(&mut registry, tool("host_lookup")).is_ok());
        let again = register(&mut registry, tool("host_lookup"));
        assert!(matches!(again, Err(BundleError::Config(_))), "{again:?}");
        assert_eq!(registry.len(), 1);
    }
}
