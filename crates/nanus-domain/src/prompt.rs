//! Ordered prompt assembly.
//!
//! A prompt is not a string built by concatenation at the call site. It is a set
//! of named, ordered sections with explicit variables, assembled by one function
//! so that the result is reproducible and inspectable. Two of the rules matter
//! more than the rest:
//!
//! - Sections are ordered by `order`, then by name. Registration order is *not*
//!   the order, because a plugin that adds a section must not be able to change
//!   where an unrelated section lands.
//! - A reference to an undefined variable is an error. Substituting an empty
//!   string would ship a prompt with a silent hole in it, and a prompt bug is
//!   invisible until a model behaves oddly.
//!
//! ## Template grammar
//!
//! - `{{name}}` — a variable reference. The variable must be defined.
//! - `{{#name}}…{{/name}}` — a **complete group**: emitted (with its body
//!   interpolated) only when `name` is defined and non-blank. Both markers are
//!   required, and they must agree; a group that is unclosed, whose closing
//!   marker has no opener, or whose name is not a valid reference is a
//!   [`PromptError::MalformedGroup`].
//! - Groups nest.
//!
//! There is no escape sequence for a literal `{{`. A prompt that needs one is a
//! prompt whose author should be using a variable.

use core::fmt;
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::approval::{ApprovalPolicy, SandboxMode};

/// One named, ordered block of prompt text.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptSection {
    /// The section's stable name, used for ordering ties and error messages.
    pub name: String,
    /// Sort key; lower comes first.
    pub order: i32,
    /// The template text.
    pub text: String,
}

impl PromptSection {
    /// Builds a section.
    #[must_use]
    pub fn new(name: impl Into<String>, order: i32, text: impl Into<String>) -> Self {
        let name = name.into();
        // A section name is what an error message points at, so an empty one
        // would produce an error nobody can act on.
        assert!(!name.is_empty(), "a prompt section has a name");
        Self {
            name,
            order,
            text: text.into(),
        }
    }
}

/// Why a prompt could not be rendered.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum PromptError {
    /// A reference names a variable that was never defined.
    #[error("prompt section {section:?} references undefined variable {name:?}")]
    UnresolvedVariable {
        /// The section the reference appears in.
        section: String,
        /// The undefined variable's name.
        name: String,
    },

    /// A complete group is not well formed.
    #[error("prompt section {section:?} has a malformed complete group {name:?}: {detail}")]
    MalformedGroup {
        /// The section the group appears in.
        section: String,
        /// The group's name, empty when the marker itself is unreadable.
        name: String,
        /// What is wrong with it.
        detail: String,
    },
}

/// Renders a deployment description of the running harness.
///
/// This is the one section every prompt needs and no plugin should own: it
/// describes the process, not the task.
#[must_use]
pub fn runtime_context(
    cwd: &str,
    model: &str,
    approval_policy: ApprovalPolicy,
    sandbox_mode: SandboxMode,
) -> String {
    let text = format!(
        "## Runtime\n- Working directory: {cwd}\n- Model: {model}\n- Approval policy: \
         {approval_policy}\n- Sandbox: {sandbox_mode}"
    );
    // Postconditions: every knob a model might reason about is present in the
    // rendered text, so a caller cannot pass a value that never appears.
    assert!(text.contains(cwd), "the working directory is rendered");
    assert!(text.contains(model), "the model is rendered");
    assert!(text.contains(approval_policy.as_str()));
    assert!(text.contains(sandbox_mode.as_str()));
    text
}

/// Assembles a prompt from named sections and variables.
#[derive(Clone, Debug, Default)]
pub struct PromptBuilder {
    /// Registered sections, in registration order.
    sections: Vec<PromptSection>,
    /// Defined variables, ordered so rendering is deterministic.
    variables: BTreeMap<String, String>,
}

impl PromptBuilder {
    /// Creates an empty builder.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            sections: Vec::new(),
            variables: BTreeMap::new(),
        }
    }

    /// Adds a section.
    #[must_use]
    pub fn section(mut self, name: impl Into<String>, order: i32, text: impl Into<String>) -> Self {
        self.sections.push(PromptSection::new(name, order, text));
        self
    }

    /// Defines a variable.
    #[must_use]
    pub fn variable(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.set_variable(name, value);
        self
    }

    /// Defines a variable in place.
    ///
    /// # Panics
    ///
    /// Panics when `name` is empty or contains a byte outside `[A-Za-z0-9_.-]`.
    /// A name that cannot be written as `{{name}}` could never be resolved, so a
    /// caller supplying one has made a mistake the builder will not hide.
    pub fn set_variable(&mut self, name: impl Into<String>, value: impl Into<String>) {
        let name = name.into();
        // The name is interpolated into a template, so it must be expressible as
        // a reference; an invalid one could never be looked up.
        assert!(is_valid_name(&name), "a variable name is referenceable");
        self.variables.insert(name, value.into());
    }

    /// Returns the registered sections, in registration order.
    #[must_use]
    pub fn sections(&self) -> &[PromptSection] {
        &self.sections
    }

    /// Returns the defined variables.
    #[must_use]
    pub const fn variables(&self) -> &BTreeMap<String, String> {
        &self.variables
    }

    /// Renders the prompt.
    ///
    /// Sections are sorted by `order` then name, empty ones are dropped, and the
    /// survivors are joined with a blank line.
    ///
    /// # Errors
    ///
    /// Returns [`PromptError::UnresolvedVariable`] when a section references an
    /// undefined variable, and [`PromptError::MalformedGroup`] when a complete
    /// group is not well formed.
    pub fn render(&self) -> Result<String, PromptError> {
        let mut sections: Vec<&PromptSection> = self
            .sections
            .iter()
            .filter(|section| !section.text.trim().is_empty())
            .collect();
        sections.sort_by(|left, right| {
            left.order
                .cmp(&right.order)
                .then_with(|| left.name.cmp(&right.name))
        });
        let mut rendered: Vec<String> = Vec::with_capacity(sections.len());
        for section in sections {
            let text = interpolate(&section.name, &section.text, &self.variables)?;
            // A section whose text was only a group that did not fire renders
            // empty and is dropped: the blank line it would leave is noise.
            if !text.trim().is_empty() {
                rendered.push(text);
            }
        }
        // Postcondition: rendering removes sections, never invents them.
        assert!(rendered.len() <= self.sections.len());
        Ok(rendered.join("\n\n"))
    }
}

/// Returns `true` when `name` can be written as a template reference.
fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
}

/// Validates a reference name, attributing the failure to `section`.
fn validate_name<'a>(section: &str, raw: &'a str) -> Result<&'a str, PromptError> {
    let name = raw.trim();
    if !is_valid_name(name) {
        return Err(PromptError::MalformedGroup {
            section: section.to_owned(),
            name: name.to_owned(),
            detail: String::from("a reference name is non-empty and made of [A-Za-z0-9_.-]"),
        });
    }
    Ok(name)
}

/// Interpolates `{{variable}}` references and `{{#group}}` blocks into `text`.
fn interpolate(
    section: &str,
    text: &str,
    variables: &BTreeMap<String, String>,
) -> Result<String, PromptError> {
    let mut out = String::new();
    let mut rest = text;
    // `split_once` rather than slicing: it cannot produce a panic, and the
    // halves it returns are always on `{{` boundaries.
    while let Some((head, after_open)) = rest.split_once("{{") {
        let before = rest.len();
        out.push_str(head);
        let Some((token, tail)) = after_open.split_once("}}") else {
            return Err(PromptError::MalformedGroup {
                section: section.to_owned(),
                name: String::new(),
                detail: String::from("a reference is never closed"),
            });
        };
        let group = token.trim();
        if let Some(name) = group.strip_prefix('#') {
            let name = validate_name(section, name)?;
            let closing = format!("{{{{/{name}}}}}");
            let Some((body, after_group)) = tail.split_once(&closing) else {
                return Err(PromptError::MalformedGroup {
                    section: section.to_owned(),
                    name: name.to_owned(),
                    detail: format!("a complete group is never closed with {closing}"),
                });
            };
            let value = variables
                .get(name)
                .ok_or_else(|| unresolved(section, name))?;
            if !value.trim().is_empty() {
                out.push_str(&interpolate(section, body, variables)?);
            }
            rest = after_group;
        } else if let Some(name) = group.strip_prefix('/') {
            return Err(PromptError::MalformedGroup {
                section: section.to_owned(),
                name: name.trim().to_owned(),
                detail: String::from("a closing marker has no opening marker"),
            });
        } else {
            let name = validate_name(section, group)?;
            let value = variables
                .get(name)
                .ok_or_else(|| unresolved(section, name))?;
            out.push_str(value);
            rest = tail;
        }
        // Postcondition: every iteration consumes at least a whole marker, so
        // the loop terminates and no part of the template is re-scanned.
        assert!(rest.len() < before, "the interpolator always advances");
    }
    out.push_str(rest);
    Ok(out)
}

/// Builds the error for a reference that names no variable.
fn unresolved(section: &str, name: &str) -> PromptError {
    PromptError::UnresolvedVariable {
        section: section.to_owned(),
        name: name.to_owned(),
    }
}

impl fmt::Display for PromptSection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}[{}]", self.name, self.order)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sections_render_in_order_then_name() {
        let prompt = PromptBuilder::new()
            .section("b", 10, "second")
            .section("a", 0, "first")
            .section("c", 10, "third");
        let rendered = prompt.render();
        assert!(rendered.is_ok());
        assert_eq!(rendered.unwrap_or_default(), "first\n\nsecond\n\nthird");
    }

    #[test]
    fn registration_order_does_not_decide_render_order() {
        // The invariant: a plugin adding a section cannot move an unrelated one.
        let forward = PromptBuilder::new()
            .section("a", 0, "a")
            .section("b", 1, "b");
        let backward = PromptBuilder::new()
            .section("b", 1, "b")
            .section("a", 0, "a");
        assert_eq!(forward.render(), backward.render());
    }

    #[test]
    fn empty_sections_are_dropped_rather_than_leaving_blank_lines() {
        let prompt = PromptBuilder::new()
            .section("a", 0, "kept")
            .section("blank", 1, "   \n  ")
            .section("b", 2, "also kept");
        let rendered = prompt.render();
        assert!(rendered.is_ok());
        assert_eq!(rendered.unwrap_or_default(), "kept\n\nalso kept");
    }

    #[test]
    fn a_prompt_with_no_sections_renders_empty() {
        let prompt = PromptBuilder::new();
        assert_eq!(prompt.render().unwrap_or_default(), "");
        assert!(prompt.sections().is_empty());
    }

    #[test]
    fn variables_are_interpolated() {
        let prompt = PromptBuilder::new()
            .section("runtime", 0, "cwd is {{cwd}} on {{os}}")
            .variable("cwd", "/work")
            .variable("os", "unix");
        assert_eq!(prompt.render().unwrap_or_default(), "cwd is /work on unix");
    }

    #[test]
    fn an_unresolved_reference_is_an_error_not_an_empty_string() {
        let prompt = PromptBuilder::new().section("runtime", 0, "cwd is {{cwd}}");
        let rendered = prompt.render();
        assert!(matches!(
            rendered,
            Err(PromptError::UnresolvedVariable { .. })
        ));
        // The negative half of the rule: the failure is not a silent hole.
        assert!(!matches!(rendered, Ok(ref text) if text.contains("cwd is ")));
    }

    #[test]
    fn a_variable_set_after_a_failed_render_is_picked_up() {
        let mut prompt = PromptBuilder::new().section("runtime", 0, "cwd is {{cwd}}");
        assert!(prompt.render().is_err());
        prompt.set_variable("cwd", "/work");
        assert_eq!(prompt.render().unwrap_or_default(), "cwd is /work");
    }

    #[test]
    fn a_complete_group_renders_only_when_its_variable_is_non_blank() {
        let template = "before {{#tools}}tools: {{tools}}{{/tools}} after";
        let with = PromptBuilder::new()
            .section("s", 0, template)
            .variable("tools", "read, write");
        assert_eq!(
            with.render().unwrap_or_default(),
            "before tools: read, write after"
        );

        let without = PromptBuilder::new()
            .section("s", 0, template)
            .variable("tools", "   ");
        assert_eq!(without.render().unwrap_or_default(), "before  after");
    }

    #[test]
    fn a_group_whose_variable_is_undefined_is_an_error() {
        // The group is a reference too, so an undefined name is not "off".
        let prompt = PromptBuilder::new().section("s", 0, "{{#tools}}x{{/tools}}");
        assert!(matches!(
            prompt.render(),
            Err(PromptError::UnresolvedVariable { .. })
        ));
    }

    #[test]
    fn a_malformed_complete_group_is_an_error() {
        let unclosed = PromptBuilder::new()
            .section("s", 0, "{{#tools}}x")
            .variable("tools", "y");
        assert!(matches!(
            unclosed.render(),
            Err(PromptError::MalformedGroup { .. })
        ));

        let stray_close = PromptBuilder::new().section("s", 0, "x{{/tools}}");
        assert!(matches!(
            stray_close.render(),
            Err(PromptError::MalformedGroup { .. })
        ));

        let unterminated = PromptBuilder::new().section("s", 0, "x{{tools");
        assert!(matches!(
            unterminated.render(),
            Err(PromptError::MalformedGroup { .. })
        ));

        let empty = PromptBuilder::new().section("s", 0, "{{}}");
        assert!(matches!(
            empty.render(),
            Err(PromptError::MalformedGroup { .. })
        ));
    }

    #[test]
    fn groups_nest() {
        let prompt = PromptBuilder::new()
            .section(
                "s",
                0,
                "{{#outer}}[{{#inner}}{{inner}}{{/inner}}]{{/outer}}",
            )
            .variable("outer", "yes")
            .variable("inner", "deep");
        assert_eq!(prompt.render().unwrap_or_default(), "[deep]");
    }

    #[test]
    fn a_group_can_drop_a_section_entirely() {
        let prompt = PromptBuilder::new()
            .section("optional", 0, "{{#flag}}only when flagged{{/flag}}")
            .section("required", 1, "always")
            .variable("flag", "");
        assert_eq!(prompt.render().unwrap_or_default(), "always");
    }

    #[test]
    fn the_boundary_between_a_variable_and_a_group_is_the_hash() {
        // `{{tools}}` reads; `{{#tools}}` tests. Both names must be defined for
        // the section to render, which is what makes the two forms distinct.
        let prompt = PromptBuilder::new()
            .section("s", 0, "{{#tools}}{{tools}}{{/tools}}")
            .variable("tools", "read");
        assert_eq!(prompt.render().unwrap_or_default(), "read");
    }

    #[test]
    fn valid_variable_names_are_stored() {
        let mut builder = PromptBuilder::new();
        builder.set_variable("cwd", "/work");
        builder.set_variable("a.b-c_1", "ok");
        assert_eq!(builder.variables().len(), 2);
        assert!(builder.variables().contains_key("cwd"));
    }

    #[test]
    fn runtime_context_names_every_knob() {
        let text = runtime_context(
            "/work",
            "deepseek-flash",
            ApprovalPolicy::PerCall,
            SandboxMode::WorkspaceWrite,
        );
        assert!(text.contains("/work"));
        assert!(text.contains("deepseek-flash"));
        assert!(text.contains("per_call"));
        assert!(text.contains("workspace_write"));
        assert!(text.starts_with("## Runtime"));
    }

    #[test]
    fn runtime_context_can_be_a_section() {
        let text = runtime_context(
            "/work",
            "deepseek-flash",
            ApprovalPolicy::AllCalls,
            SandboxMode::DangerFullAccess,
        );
        let prompt = PromptBuilder::new().section("runtime", -100, text);
        let rendered = prompt.render();
        assert!(rendered.is_ok());
        assert!(
            rendered.unwrap_or_default().contains("danger_full_access"),
            "the deployment section survives assembly"
        );
    }

    #[test]
    fn a_section_displays_its_name_and_order() {
        let section = PromptSection::new("runtime", -3, "x");
        assert_eq!(section.to_string(), "runtime[-3]");
    }
}
