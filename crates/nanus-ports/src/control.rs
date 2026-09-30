//! Local host decisions and wakeable turn cancellation.

use core::future::Future;

use nanus_domain::{ToolAccess, ToolCall};

use crate::LocalBoxFuture;

/// An exact-call decision, with no standing permission changes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolPolicyDecision {
    /// Consult the ordinary sandbox and approval gate.
    UseDefault,
    /// Grant only the invocation supplied to the policy.
    AllowOnce,
    /// Refuse the invocation with a model-visible explanation.
    Deny {
        /// Why this call cannot run.
        reason: String,
    },
}

/// A host policy that could not decide; errors always refuse execution.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("host policy failed: {message}")]
pub struct PolicyError {
    /// A model-visible explanation, without credentials or private host state.
    pub message: String,
}

/// Optional argument-aware policy, consulted before the stock approval gate.
///
/// Decisions arrive in model order. The runner holds no registry borrow while waiting.
/// Executors must still check live scope immediately before performing effects.
pub trait ToolPolicy {
    /// Decides this exact immutable call, including reads permitted by the sandbox.
    fn decide<'a>(
        &'a self,
        call: &'a ToolCall,
        access: ToolAccess,
    ) -> LocalBoxFuture<'a, Result<ToolPolicyDecision, PolicyError>>;
}

/// A caller-owned, sticky cancellation signal for one turn.
///
/// Once cancelled, both methods must continue reporting cancellation. Construct a new
/// control for a later turn. Dropping a port future does not join detached workers or kill
/// a process tree: the host supplies cancel-safe ports or owns that teardown itself.
pub trait TurnControl {
    /// Reports cancellation without waiting, including immediately before dispatch.
    fn is_cancelled(&self) -> bool;

    /// Wakes when cancellation is signalled, even while all other I/O is idle.
    fn cancelled(&self) -> LocalBoxFuture<'_, ()>;
}

/// Races local work with cancellation, polling the signal before the work.
///
/// `None` drops the unfinished work. A ready decision racing a cancellation never grants
/// an effect, because dispatch also checks the sticky state.
pub async fn until_cancelled<F: Future>(
    control: Option<&dyn TurnControl>,
    work: F,
) -> Option<F::Output> {
    let Some(control) = control else {
        return Some(work.await);
    };
    if control.is_cancelled() {
        return None;
    }
    match futures::future::select(control.cancelled(), Box::pin(work)).await {
        futures::future::Either::Left(_) => None,
        futures::future::Either::Right((value, _)) => Some(value),
    }
}
