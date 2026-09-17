//! # nanus-bundle
//!
//! The composition: everything that turns the kernel, the domain, and the adapters
//! into a working harness.
//!
//! ## What a bundle is
//!
//! A bundle is a set of Cordis plugins and the wiring between them. It owns no
//! policy of its own beyond assembly: the tools live in [`tools`], the argument
//! contract in [`args`], and the loop in [`agent_loop`]. What lives *here* is the
//! decision about which of those exist and how they find each other.
//!
//! ## The toolset is deliberately small
//!
//! Seven tools: `read`, `write`, `edit`, `read_image`, `glob`, `grep`, and `bash`.
//! Each is a mechanism the shell cannot provide as well — a bounded read window, an
//! exactly-once edit, a diff, a capped search — rather than a convenience wrapper
//! around something the shell already does. Adding a tool costs a description in
//! every request and a schema the model must choose between, so a tool has to earn
//! its place.
//!
//! ## Services this bundle publishes
//!
//! | Key | Type | Provided by |
//! |---|---|---|
//! | `tools` | [`ToolRegistryHandle`] | [`tools_plugin`] |
//!
//! ## How a harness is assembled
//!
//! ```
//! use nanus_kernel::{Kernel, PluginId};
//!
//! // A bundle contributes plugins; the kernel mounts them and resolves their
//! // requirements, so the order here is not a boot sequence.
//! let kernel = Kernel::new();
//! let _context = kernel.into_context();
//! ```
//!
//! The adapters publish `llm`, `fs`, `shell`, `store`, and `clock`; the tool plugin
//! requires `fs` and `shell` and publishes `tools`; the loop requires `llm`, `tools`,
//! and `session`. Nothing lists those in order, because the kernel activates each
//! plugin when its requirements appear.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
// A `pub` item inside a private module is reachable only through this crate's own
// re-exports. The lint cannot tell that from an orphaned item, and every such item
// here is deliberate.
#![allow(unreachable_pub)]
#![allow(
    clippy::unwrap_or_default,
    clippy::manual_unwrap_or,
    clippy::manual_unwrap_or_default
)]

pub mod agent_loop;
pub mod args;
pub mod compose;
pub mod error;
pub mod tools;

pub use agent_loop::{AgentRunner, Approver, Progress, RunOutcome, Silent};
pub use compose::{DEFAULT_SYSTEM_PROMPT, Harness, compose};
pub use error::BundleError;

#[cfg(test)]
pub(crate) mod tests_support;

use core::cell::RefCell;
use std::rc::Rc;

use nanus_domain::{ToolDefinition, ToolRegistry};
use nanus_kernel::{MountContext, PluginId, ServiceKey};
use nanus_ports::{FsHandle, ShellHandle};

/// The shared tool registry, as published on the context.
///
/// A named handle rather than a bare `Rc<RefCell<..>>` for two reasons: the service
/// key has to name a concrete type, and a shared registry that one plugin fills and
/// another dispatches from is a concept worth naming. The handle derefs to the
/// registry, so a consumer uses it as one.
#[derive(Clone)]
pub struct ToolRegistryHandle(Rc<RefCell<ToolRegistry>>);

impl ToolRegistryHandle {
    /// Wraps a registry for sharing.
    #[must_use]
    pub fn new(registry: ToolRegistry) -> Self {
        Self(Rc::new(RefCell::new(registry)))
    }

    /// Borrows the registry.
    #[must_use]
    pub fn borrow(&self) -> core::cell::Ref<'_, ToolRegistry> {
        self.0.borrow()
    }

    /// Borrows the registry mutably, for registering a tool.
    #[must_use]
    pub fn borrow_mut(&self) -> core::cell::RefMut<'_, ToolRegistry> {
        self.0.borrow_mut()
    }
}

impl core::fmt::Debug for ToolRegistryHandle {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let borrowed = self.0.borrow();
        f.debug_struct("ToolRegistryHandle")
            .field("tools", &borrowed.len())
            .finish_non_exhaustive()
    }
}

/// The service key the tool registry is published under.
///
/// A function rather than a constant because `ServiceKey::of` validates, and
/// validation is not available in a `const` on stable Rust.
#[must_use]
pub fn tools_key() -> ServiceKey<ToolRegistryHandle> {
    ServiceKey::of("tools")
}

/// Builds the seven model-facing tools over the filesystem and shell ports.
///
/// # Errors
///
/// Returns an error only if two tools claim the same name, which cannot happen for
/// the shipped set and is asserted by a test.
pub fn build_toolset(
    fs: &FsHandle,
    shell: &ShellHandle,
) -> Result<ToolRegistry, nanus_domain::ToolError> {
    let mut registry = ToolRegistry::new();
    let definitions: [ToolDefinition; 7] = [
        tools::read_tool(Rc::clone(fs)),
        tools::write_tool(Rc::clone(fs)),
        tools::edit_tool(Rc::clone(fs)),
        tools::read_image_tool(Rc::clone(fs)),
        tools::glob_tool(Rc::clone(fs)),
        tools::grep_tool(Rc::clone(fs)),
        tools::bash_tool(Rc::clone(shell)),
    ];
    for definition in definitions {
        registry.register(definition)?;
    }
    // Postcondition: the shipped set is exactly seven distinct tools, which is the
    // scope the design committed to.
    assert_eq!(registry.len(), 7, "the shipped toolset has seven tools");
    Ok(registry)
}

/// The plugin that publishes the model-facing toolset.
///
/// Requires the filesystem and shell ports, so the kernel activates it only once a
/// provider for each exists. That is the whole of its boot ordering: nothing calls
/// "mount the tools now".
// The handles are taken by value because the returned plugin must be `'static`: a
// borrowing plugin could not be staged on a kernel that outlives this call. The clones
// are of `Rc`, so taking them by value costs a refcount and buys the lifetime the
// composition needs.
#[allow(clippy::needless_pass_by_value)]
pub fn tools_plugin(fs: FsHandle, shell: ShellHandle) -> impl nanus_kernel::Plugin {
    let ready = match build_toolset(&fs, &shell) {
        Ok(built) => built,
        Err(error) => {
            tracing::error!(error = %error, "the shipped toolset could not be built");
            ToolRegistry::new()
        }
    };
    Provider {
        id: PluginId::new("tools").unwrap_or_else(|_| unreachable!("tools is a valid plugin id")),
        // The kernel's registry hands out clones of the published handle, so the
        // value it stores is an `Rc` of the handle.
        registry: Rc::new(ToolRegistryHandle::new(ready)),
    }
}

/// Publishes the tool registry as a service.
struct Provider {
    id: PluginId,
    /// Shared through the kernel's service registry, which lends clones rather than
    /// the value itself.
    registry: Rc<ToolRegistryHandle>,
}

impl nanus_kernel::Plugin for Provider {
    fn id(&self) -> PluginId {
        self.id
    }

    fn description(&self) -> &'static str {
        "the model-facing toolset: read, write, edit, read_image, glob, grep, bash"
    }

    fn requirements(&self) -> Vec<nanus_kernel::AnyServiceKey> {
        vec![
            nanus_kernel::AnyServiceKey::from_typed(nanus_ports::fs_key()),
            nanus_kernel::AnyServiceKey::from_typed(nanus_ports::shell_key()),
        ]
    }

    fn init(&mut self, _cx: &nanus_kernel::Context) -> nanus_kernel::PluginFuture {
        Box::pin(async { Ok(()) })
    }

    fn mount(&mut self, cx: &mut MountContext<'_>) -> nanus_kernel::PluginFuture {
        // The handle is published by value; `provide` records the withdrawal as an
        // effect, so the registry leaves the context when this plugin unloads.
        let outcome = cx.provide(tools_key(), self.registry.clone());
        Box::pin(async move { outcome })
    }

    fn unmount(&mut self, _cx: &nanus_kernel::Context) -> nanus_kernel::PluginFuture {
        Box::pin(async { Ok(()) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fs_handle() -> FsHandle {
        let port: Box<dyn nanus_ports::FsPort> = Box::new(tests_support::UnusedFs);
        Rc::new(port)
    }

    fn shell_handle() -> ShellHandle {
        let port: Box<dyn nanus_ports::ShellPort> = Box::new(tests_support::UnusedShell);
        Rc::new(port)
    }

    #[test]
    fn the_shipped_toolset_is_exactly_the_seven_tools() {
        let registry = build_toolset(&fs_handle(), &shell_handle());
        assert!(registry.is_ok());
        let Ok(registry) = registry else {
            return;
        };
        let mut names: Vec<String> = registry
            .names()
            .into_iter()
            .map(|name| name.as_str().to_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                "bash",
                "edit",
                "glob",
                "grep",
                "read",
                "read_image",
                "write"
            ]
        );
    }

    #[test]
    fn every_tool_schema_is_a_valid_json_schema_object() {
        let Ok(registry) = build_toolset(&fs_handle(), &shell_handle()) else {
            return;
        };
        for schema in registry.schemas() {
            let Some(parameters) = schema.parameters.as_object() else {
                panic!("{} parameters must be an object", schema.name);
            };
            assert_eq!(
                parameters.get("type").and_then(|value| value.as_str()),
                Some("object"),
                "{} declares an object",
                schema.name
            );
            assert!(
                parameters.contains_key("properties"),
                "{} declares properties",
                schema.name
            );
            // A description is the model's only documentation for the tool, so a
            // missing or trivial one is a defect.
            assert!(
                schema.description.len() > 20,
                "{} has a usable description",
                schema.name
            );
        }
    }

    #[test]
    fn no_tool_forbids_unknown_arguments_silently() {
        let Ok(registry) = build_toolset(&fs_handle(), &shell_handle()) else {
            return;
        };
        for schema in registry.schemas() {
            let parameters = &schema.parameters;
            // Refusing unknown properties is what turns a model's typo into a clear
            // error instead of a silently ignored argument.
            assert_eq!(
                parameters
                    .get("additionalProperties")
                    .and_then(serde_json::Value::as_bool),
                Some(false),
                "{} forbids extra properties",
                schema.name
            );
        }
    }

    #[test]
    fn the_tools_key_is_stable() {
        let key = tools_key();
        assert_eq!(key.as_str(), "tools");
        // Two constructions agree, which is what lets a provider and a consumer find
        // each other without sharing a value.
        assert_eq!(key, tools_key());
        assert_eq!(key.type_id(), tools_key().type_id());
    }
}
