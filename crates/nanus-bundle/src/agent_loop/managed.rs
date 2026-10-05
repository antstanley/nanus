//! The managed turn: selection, staged context edits and checkpoints around the ordinary loop.
//!
//! Provisional entry points; the transaction is filled in behind these signatures.
use nanus_domain::Session;
use nanus_domain::context::managed::{ContextPolicy, ContextStatus, ErrorCode, ModeActor};
use nanus_ports::{PersistenceState, TurnControl, TurnRuntime};

use super::{AgentRunner, Approver, Progress, RunOutcome};
use crate::BundleError;

/// What a managed run produced, and whether its session is durable.
///
/// The persistence state is returned on failure too: a host decides from it — and only from
/// it — whether any further save is permitted. `None` means no checkpoint was attempted.
#[derive(Debug)]
pub struct ManagedRun {
    /// The ordinary turn result.
    pub outcome: Result<RunOutcome, BundleError>,
    /// Where the session's durability stands.
    pub persistence: Option<PersistenceState>,
}

/// Everything a host lends one turn: who answers approvals, how it is stopped, and the
/// session-scoped runtime it saves through.
#[derive(Clone, Copy, Default)]
pub struct TurnHost<'a> {
    /// Who answers an approval question; `None` denies what the sandbox does not permit.
    pub approver: Option<&'a dyn Approver>,
    /// Wakeable cancellation, observed beside [`Progress::cancelled`].
    pub control: Option<&'a dyn TurnControl>,
    /// The session-scoped checkpoint and context runtime.
    pub runtime: TurnRuntime<'a>,
}

impl core::fmt::Debug for TurnHost<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TurnHost")
            .field("approver", &self.approver.is_some())
            .field("control", &self.control.is_some())
            .field("runtime", &self.runtime)
            .finish()
    }
}

impl AgentRunner {
    /// Runs one turn of a session through its host-bound runtime.
    ///
    /// Every save goes through `host.runtime.checkpoint`; the host must not save the session
    /// itself afterwards except as [`PersistenceState`] permits.
    pub async fn run_turn_with_runtime(
        &self,
        session: &mut Session,
        message: &str,
        progress: &mut dyn Progress,
        host: TurnHost<'_>,
    ) -> ManagedRun {
        ManagedRun {
            outcome: self
                .run_controlled(session, message, progress, host.approver, host.control)
                .await,
            persistence: None,
        }
    }

    /// Reports the session's context status without preparing a request.
    ///
    /// # Errors
    ///
    /// Returns [`BundleError::Managed`] for a session whose managed state does not validate.
    pub fn context_status(
        &self,
        session: &Session,
        runtime: TurnRuntime<'_>,
    ) -> Result<ContextStatus, BundleError> {
        let _ = (session, runtime);
        Err(BundleError::managed(
            ErrorCode::UnsupportedMode,
            "not yet available",
        ))
    }

    /// Changes the session's context policy while it is idle, persisting the change first.
    ///
    /// # Errors
    ///
    /// Returns [`BundleError::Managed`] when the policy cannot be supported here.
    pub async fn set_context_policy(
        &self,
        session: &mut Session,
        policy: ContextPolicy,
        actor: ModeActor,
        runtime: TurnRuntime<'_>,
    ) -> Result<PersistenceState, BundleError> {
        let _ = (session, policy, actor, runtime);
        Err(BundleError::managed(
            ErrorCode::UnsupportedMode,
            "not yet available",
        ))
    }

    /// Resets the session's context to an empty selection and selects legacy replay.
    ///
    /// # Errors
    ///
    /// Returns [`BundleError::Managed`] when the raw session does not validate.
    pub async fn reset_context(
        &self,
        session: &mut Session,
        runtime: TurnRuntime<'_>,
    ) -> Result<PersistenceState, BundleError> {
        let _ = (session, runtime);
        Err(BundleError::managed(
            ErrorCode::UnsupportedMode,
            "not yet available",
        ))
    }

    /// Closes what a crash left open in a managed session, before new work is admitted.
    ///
    /// `Ok(None)` when nothing needed recovery.
    ///
    /// # Errors
    ///
    /// Returns [`BundleError::Managed`] when recovery cannot be checkpointed.
    pub async fn recover_session(
        &self,
        session: &mut Session,
        runtime: TurnRuntime<'_>,
    ) -> Result<Option<PersistenceState>, BundleError> {
        let _ = (session, runtime);
        Ok(None)
    }
}
