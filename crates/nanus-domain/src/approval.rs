//! The two orthogonal permission knobs.
//!
//! `dsh` separates *whether a human is asked* from *what the machine is allowed
//! to touch*, and nanus keeps that separation because the two answer different
//! questions. `ApprovalPolicy` decides whether a call needs consent;
//! `SandboxMode` decides what a call may do once consent is irrelevant. Collapsing
//! them into one "permission level" would make "ask me before writing, but then
//! let it write anywhere" inexpressible.
//!
//! Both enums are **fail-closed**: their defaults are the most restrictive
//! values, and a decision type's only permissive arm is explicit.

use core::fmt;

use serde::{Deserialize, Serialize};

use crate::error::DomainError;
use crate::message::ToolCallId;
use crate::tool::ToolName;

/// Whether a tool call needs a human decision before it runs.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalPolicy {
    /// Every call that is not otherwise permitted asks a human first.
    #[default]
    Ask,
    /// No call ever asks; the sandbox is the whole of the control.
    Never,
}

impl ApprovalPolicy {
    /// Returns the wire/config name of the policy.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Never => "never",
        }
    }

    /// Parses a policy from its config name.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::Validation`] for any other name. An unknown policy
    /// is never silently degraded to a default, because the default is
    /// "ask" and a typo must not turn prompts off.
    pub fn parse(raw: &str) -> Result<Self, DomainError> {
        match raw {
            "ask" => Ok(Self::Ask),
            "never" => Ok(Self::Never),
            other => Err(DomainError::Validation {
                field: "approval_policy",
                reason: format!("unknown approval policy {other:?}"),
            }),
        }
    }
}

impl fmt::Display for ApprovalPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a tool call may touch.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum SandboxMode {
    /// Reads only; every write is refused.
    #[default]
    ReadOnly,
    /// Writes are confined to the workspace root.
    WorkspaceWrite,
    /// No confinement. Only ever selected explicitly by a human.
    DangerFullAccess,
}

impl SandboxMode {
    /// Returns the wire/config name of the mode.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::WorkspaceWrite => "workspace_write",
            Self::DangerFullAccess => "danger_full_access",
        }
    }

    /// Returns `true` when the mode permits writing anywhere at all.
    #[must_use]
    pub const fn permits_writes(self) -> bool {
        matches!(self, Self::WorkspaceWrite | Self::DangerFullAccess)
    }

    /// Returns `true` when the mode confines writes to a workspace root.
    #[must_use]
    pub const fn is_confined(self) -> bool {
        matches!(self, Self::ReadOnly | Self::WorkspaceWrite)
    }

    /// Parses a mode from its config name.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::Validation`] for any other name, for the same
    /// reason [`ApprovalPolicy::parse`] rejects one.
    pub fn parse(raw: &str) -> Result<Self, DomainError> {
        match raw {
            "read_only" => Ok(Self::ReadOnly),
            "workspace_write" => Ok(Self::WorkspaceWrite),
            "danger_full_access" => Ok(Self::DangerFullAccess),
            other => Err(DomainError::Validation {
                field: "sandbox_mode",
                reason: format!("unknown sandbox mode {other:?}"),
            }),
        }
    }
}

impl fmt::Display for SandboxMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a human decided about one tool call.
///
/// The enum is closed and fail-closed: there is no `Maybe`, no partial approval,
/// and [`is_allowed`](ApprovalOutcome::is_allowed) is true for exactly one arm.
/// A harness that adds an arm must decide what it means, rather than inheriting
/// permission by default.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalOutcome {
    /// Run this call, this once. Not a standing permission.
    AllowedOnce,
    /// A human said no. The model is told, and may try something else.
    Rejected,
    /// The prompt went away without an answer, e.g. the terminal closed.
    Cancelled,
    /// There was nobody and nothing to ask.
    Unavailable,
}

impl ApprovalOutcome {
    /// Returns `true` only for [`ApprovalOutcome::AllowedOnce`].
    #[must_use]
    pub const fn is_allowed(self) -> bool {
        matches!(self, Self::AllowedOnce)
    }

    /// Returns the label used in a transcript.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::AllowedOnce => "allowed-once",
            Self::Rejected => "rejected",
            Self::Cancelled => "cancelled",
            Self::Unavailable => "unavailable",
        }
    }
}

impl fmt::Display for ApprovalOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Everything an approval prompt is allowed to know about a call.
///
/// The arguments are deliberately absent. A prompt that renders the arguments
/// invites a human to approve a call they cannot fully evaluate, and it puts
/// model-controlled text in front of the decision. The tool name, the call id,
/// and the harness's own reason are enough to decide whether to look closer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalRequest {
    /// The tool the model wants to run.
    pub tool: ToolName,
    /// The call being decided, when the prompt is attached to one.
    pub call_id: Option<ToolCallId>,
    /// Why the harness is asking, e.g. "writes outside the workspace".
    pub reason: Option<String>,
}

impl ApprovalRequest {
    /// Builds a request naming only the tool.
    #[must_use]
    pub const fn new(tool: ToolName) -> Self {
        Self {
            tool,
            call_id: None,
            reason: None,
        }
    }

    /// Attaches the call being decided.
    #[must_use]
    pub fn with_call_id(mut self, call_id: ToolCallId) -> Self {
        self.call_id = Some(call_id);
        self
    }

    /// Attaches the harness's reason for asking.
    #[must_use]
    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }
}

/// The name of a permission preset.
///
/// [`PresetName::Custom`] exists to *describe* a bundle that names no preset. It
/// is never a switch target: [`PresetName::parse`] rejects it, so a config file
/// cannot say `preset = "custom"` and receive a bundle that some other code
/// chose. A derived name and a configured name therefore cannot be confused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PresetName {
    /// Reads only, with approval on every call that is not a read.
    ReadOnly,
    /// Writes confined to the workspace, asking first.
    WorkspaceWrite,
    /// Unconfined and unasked.
    DangerFullAccess,
    /// The bundle matches no preset; derived only.
    Custom,
}

impl PresetName {
    /// Returns the config name of the preset.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::WorkspaceWrite => "workspace_write",
            Self::DangerFullAccess => "danger_full_access",
            Self::Custom => "custom",
        }
    }

    /// Parses a preset name a human may select.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::Validation`] for `"custom"` and for any unknown
    /// name. `"custom"` is rejected on purpose: it is a description of a bundle
    /// that was built, never an instruction for how to build one.
    pub fn parse(raw: &str) -> Result<Self, DomainError> {
        match raw {
            "read_only" => Ok(Self::ReadOnly),
            "workspace_write" => Ok(Self::WorkspaceWrite),
            "danger_full_access" => Ok(Self::DangerFullAccess),
            "custom" => Err(DomainError::Validation {
                field: "permission_preset",
                reason: String::from(
                    "custom is derived from the knobs and cannot be selected by name",
                ),
            }),
            other => Err(DomainError::Validation {
                field: "permission_preset",
                reason: format!("unknown permission preset {other:?}"),
            }),
        }
    }
}

impl fmt::Display for PresetName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Both permission knobs, bundled.
///
/// The bundle is a convenience for configuration and display; it holds no policy
/// of its own. Every preset is just a pair of values, which is what makes the
/// derived [`name`](PermissionPreset::name) trustworthy: it is a pure function of
/// the knobs and cannot drift from them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PermissionPreset {
    /// Whether calls ask a human first.
    pub approval: ApprovalPolicy,
    /// What calls may touch.
    pub sandbox: SandboxMode,
}

impl PermissionPreset {
    /// The read-only preset: reads only, asking first.
    #[must_use]
    pub const fn read_only() -> Self {
        Self {
            approval: ApprovalPolicy::Ask,
            sandbox: SandboxMode::ReadOnly,
        }
    }

    /// The workspace-write preset: writes confined to the workspace, asking first.
    #[must_use]
    pub const fn workspace_write() -> Self {
        Self {
            approval: ApprovalPolicy::Ask,
            sandbox: SandboxMode::WorkspaceWrite,
        }
    }

    /// The danger-full-access preset: unconfined, and never asking.
    ///
    /// The pairing is the point. Unconfined writes with approval still on would
    /// ask a human to bless every write; the preset exists for the case where a
    /// human has already decided to run unattended.
    #[must_use]
    pub const fn danger_full_access() -> Self {
        Self {
            approval: ApprovalPolicy::Never,
            sandbox: SandboxMode::DangerFullAccess,
        }
    }

    /// Builds an arbitrary bundle.
    #[must_use]
    pub const fn custom(approval: ApprovalPolicy, sandbox: SandboxMode) -> Self {
        Self { approval, sandbox }
    }

    /// Returns the preset name this bundle matches, or `Custom`.
    #[must_use]
    pub const fn name(self) -> PresetName {
        match (self.approval, self.sandbox) {
            (ApprovalPolicy::Ask, SandboxMode::ReadOnly) => PresetName::ReadOnly,
            (ApprovalPolicy::Ask, SandboxMode::WorkspaceWrite) => PresetName::WorkspaceWrite,
            (ApprovalPolicy::Never, SandboxMode::DangerFullAccess) => PresetName::DangerFullAccess,
            (ApprovalPolicy::Ask | ApprovalPolicy::Never, _) => PresetName::Custom,
        }
    }

    /// Replaces the approval knob.
    #[must_use]
    pub const fn with_approval(mut self, approval: ApprovalPolicy) -> Self {
        self.approval = approval;
        self
    }

    /// Replaces the sandbox knob.
    #[must_use]
    pub const fn with_sandbox(mut self, sandbox: SandboxMode) -> Self {
        self.sandbox = sandbox;
        self
    }
}

impl Default for PermissionPreset {
    /// The fail-closed default: reads only, asking first.
    fn default() -> Self {
        Self::read_only()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_are_the_most_restrictive_values() {
        // Fail-closed: a default that permitted a write would make every missing
        // configuration decision a security decision.
        assert_eq!(ApprovalPolicy::default(), ApprovalPolicy::Ask);
        assert_eq!(SandboxMode::default(), SandboxMode::ReadOnly);
        assert_eq!(PermissionPreset::default(), PermissionPreset::read_only());
    }

    #[test]
    fn only_allowed_once_permits_a_call() {
        assert!(ApprovalOutcome::AllowedOnce.is_allowed());
        // Negative space, and the whole point of the enum: every other arm is a
        // refusal, including the ambiguous ones.
        assert!(!ApprovalOutcome::Rejected.is_allowed());
        assert!(!ApprovalOutcome::Cancelled.is_allowed());
        assert!(!ApprovalOutcome::Unavailable.is_allowed());
    }

    #[test]
    fn presets_round_trip_through_their_own_names() {
        for preset in [
            PermissionPreset::read_only(),
            PermissionPreset::workspace_write(),
            PermissionPreset::danger_full_access(),
        ] {
            let name = preset.name();
            assert_ne!(name, PresetName::Custom, "{name} is a real preset");
            let parsed = PresetName::parse(name.as_str());
            assert!(parsed.is_ok(), "{name} parses back");
        }
    }

    #[test]
    fn a_mixed_bundle_is_custom_and_has_no_switch_target() {
        let mixed = PermissionPreset::custom(ApprovalPolicy::Never, SandboxMode::WorkspaceWrite);
        assert_eq!(mixed.name(), PresetName::Custom);
        // Negative space: the derived name is not selectable, so a config file
        // cannot ask for a bundle that some other code chose.
        assert!(PresetName::parse("custom").is_err());
    }

    #[test]
    fn an_unknown_policy_is_rejected_rather_than_defaulted() {
        // A typo must not silently turn prompts off, which is what defaulting
        // would do in the other direction.
        assert!(ApprovalPolicy::parse("sometimes").is_err());
        assert!(SandboxMode::parse("readonly").is_err());
        assert!(PresetName::parse("yolo").is_err());
        assert_eq!(ApprovalPolicy::parse("never"), Ok(ApprovalPolicy::Never));
        assert_eq!(
            SandboxMode::parse("danger_full_access"),
            Ok(SandboxMode::DangerFullAccess)
        );
    }

    #[test]
    fn sandbox_modes_report_what_they_permit() {
        assert!(!SandboxMode::ReadOnly.permits_writes());
        assert!(SandboxMode::WorkspaceWrite.permits_writes());
        assert!(SandboxMode::DangerFullAccess.permits_writes());
        assert!(SandboxMode::ReadOnly.is_confined());
        assert!(SandboxMode::WorkspaceWrite.is_confined());
        assert!(!SandboxMode::DangerFullAccess.is_confined());
    }

    #[test]
    fn the_knobs_are_orthogonal() {
        // If the two were one axis, these four bundles could not exist.
        let bundles = [
            PermissionPreset::custom(ApprovalPolicy::Ask, SandboxMode::ReadOnly),
            PermissionPreset::custom(ApprovalPolicy::Ask, SandboxMode::DangerFullAccess),
            PermissionPreset::custom(ApprovalPolicy::Never, SandboxMode::ReadOnly),
            PermissionPreset::custom(ApprovalPolicy::Never, SandboxMode::DangerFullAccess),
        ];
        assert_eq!(bundles.len(), 4);
        assert_eq!(
            bundles.first().map(|b| b.name()),
            Some(PresetName::ReadOnly)
        );
        assert_eq!(
            bundles.get(3).map(|b| b.name()),
            Some(PresetName::DangerFullAccess)
        );
        assert_eq!(bundles.get(1).map(|b| b.name()), Some(PresetName::Custom));
        assert_eq!(bundles.get(2).map(|b| b.name()), Some(PresetName::Custom));
    }

    #[test]
    fn an_approval_request_carries_no_arguments_by_construction() {
        let tool = ToolName::new("write");
        assert!(tool.is_ok());
        let Ok(tool) = tool else { return };
        let request = ApprovalRequest::new(tool)
            .with_call_id(ToolCallId::new("c-1"))
            .with_reason("writes outside the workspace");
        let encoded = serde_json::to_value(&request).unwrap_or(serde_json::Value::Null);
        let Some(object) = encoded.as_object() else {
            panic!("an approval request is a JSON object");
        };
        // The request has exactly three fields, none of which is `arguments`.
        let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["call_id", "reason", "tool"]);
        assert!(!encoded.to_string().contains("arguments"));
    }

    #[test]
    fn with_helpers_replace_one_knob_at_a_time() {
        let preset = PermissionPreset::workspace_write().with_approval(ApprovalPolicy::Never);
        assert_eq!(preset.approval, ApprovalPolicy::Never);
        assert_eq!(preset.sandbox, SandboxMode::WorkspaceWrite);
        assert_eq!(preset.name(), PresetName::Custom);
    }
}
