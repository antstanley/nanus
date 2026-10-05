//! Running the context tools inside a managed step, and the batch rules around them.

use nanus_domain::context::managed::proposal::cursor;
use nanus_domain::context::managed::{
    ArtifactReceipt, CaptureReason, CaptureStatus, CaptureStream, RawEncoding,
};
use nanus_domain::context::managed::{
    ContextDecision, ContextManageInput, ContextRecallInput, ContextStatus, DecisionOutcome,
    ErrorCode, FragmentDescriptor, MANAGE_TOOL, ManageAction, ManageResult, ManageStatus, limits,
    proposal, state,
};
use nanus_domain::{Session, SessionEvent, ToolCall, ToolOutcome, ToolResult};
use nanus_ports::control::until_cancelled;
use nanus_ports::{ToolPolicyDecision, TurnControl};

use super::{ManagedTurn, Staged};
use crate::BundleError;
use crate::agent_loop::{AgentRunner, Progress, dispatch, interrupted_result, is_cancelled};

/// The failure every call of a mixed proposal batch is answered with.
const MIXED: &str = "not run: a context_manage proposal must be the only call in its message, \
    so no call in this message ran (mixed_mutation_batch). Send the proposal on its own.";

/// The failure a batch past the session's record capacity is answered with.
const FULL: &str = "not run: the session has no room left to record this call's result \
    (storage_capacity). Start a new session to continue.";

impl AgentRunner {
    /// Refuses every call of a batch that mixes a proposal with any other call.
    pub(in crate::agent_loop) fn refuse_mixed(calls: &[ToolCall]) -> Option<Vec<ToolResult>> {
        let mixed = calls.len() > 1 && calls.iter().any(crate::context_tools::is_mutating);
        mixed.then(|| {
            calls
                .iter()
                .map(|call| ToolResult::failure(call.id.clone(), MIXED))
                .collect()
        })
    }

    /// Refuses the calls a session at its record limit could not record, before any effect.
    ///
    /// Each permitted call is charged a whole record, because its result is not known yet; the
    /// closing reserve is kept back, so the turn can always end honestly.
    pub(in crate::agent_loop) fn refuse_over_capacity(
        session: &Session,
        calls: &[ToolCall],
        results: &mut [Option<ToolResult>],
        _turn: &ManagedTurn<'_>,
    ) {
        let pending = results.iter().filter(|result| result.is_none()).count();
        let need = pending
            .checked_mul(nanus_domain::content::RECORD_BYTES_MAX)
            .and_then(|bytes| bytes.checked_add(limits::CLOSING_RESERVE_BYTES));
        let used = Self::session_bytes(session).ok();
        let fits = need
            .zip(used)
            .and_then(|(need, used)| need.checked_add(used))
            .is_some_and(|total| total <= nanus_domain::content::SESSION_BYTES_MAX);
        if fits {
            return;
        }
        for (call, result) in calls.iter().zip(results.iter_mut()) {
            if result.is_none() {
                *result = Some(ToolResult::failure(call.id.clone(), FULL));
            }
        }
    }

    /// Puts a context call to the host policy with its access descriptor. No bypass.
    ///
    /// The stock default — no policy, or a policy that defers — is the narrowly scoped access
    /// enabling managed mode grants: reads of this session's own evidence and writes to its own
    /// projection. A host policy can deny either.
    pub(in crate::agent_loop) async fn gate_context(
        &self,
        call: &ToolCall,
        control: Option<&dyn TurnControl>,
    ) -> Option<ToolResult> {
        let Some(policy) = &self.policy else {
            return None;
        };
        let access = crate::context_tools::access(call);
        match until_cancelled(control, policy.decide(call, access)).await {
            Some(Ok(ToolPolicyDecision::UseDefault | ToolPolicyDecision::AllowOnce)) => None,
            Some(Ok(ToolPolicyDecision::Deny { reason })) => {
                Some(ToolResult::failure(call.id.clone(), reason))
            }
            Some(Err(error)) => Some(ToolResult::failure(call.id.clone(), error.to_string())),
            None => Some(interrupted_result(call)),
        }
    }

    /// Runs the permitted context calls of a batch, in call order.
    pub(in crate::agent_loop) async fn execute_context(
        &self,
        session: &Session,
        calls: &[ToolCall],
        outputs: (&[usize], &mut [Option<ToolResult>]),
        progress: &mut dyn Progress,
        context: (dispatch::Dispatch<'_>, &ManagedTurn<'_>),
    ) {
        let (indexes, results) = outputs;
        let (dispatch, turn) = context;
        for &index in indexes {
            let call = &calls[index];
            let result = if is_cancelled(progress, dispatch.control) {
                interrupted_result(call)
            } else if let Some(refused) =
                crate::agent_loop::admission::before_dispatch(call, dispatch.reservation)
            {
                refused
            } else if call.name.as_str() == MANAGE_TOOL {
                Self::run_manage(session, call, turn)
            } else {
                self.run_recall(session, call, turn).await
            };
            results[index] =
                Some(self.finish_admitted(call, result, progress, dispatch.reservation));
        }
        assert!(indexes.iter().all(|&index| results[index].is_some()));
    }

    /// Runs one `context_manage` call.
    fn run_manage(session: &Session, call: &ToolCall, turn: &ManagedTurn<'_>) -> ToolResult {
        if let Some(refused) = oversized(call, limits::MANAGE_ARGUMENT_BYTES_MAX) {
            return refused;
        }
        let input = match ContextManageInput::parse(&call.arguments) {
            Ok(input) => input,
            Err(message) => return ToolResult::failure(call.id.clone(), message),
        };
        let status = match Self::live_status(session, turn) {
            Ok(status) => status,
            Err(error) => return ToolResult::failure(call.id.clone(), error.to_string()),
        };
        let result = match input.action {
            ManageAction::Inspect => Self::inspect(session, turn, &input, status),
            ManageAction::Propose => Self::propose(session, turn, &input, status),
        };
        let failed = result.status == ManageStatus::Refused;
        let value = serde_json::to_value(&result).unwrap_or_default();
        if failed {
            ToolResult::failure(call.id.clone(), value.to_string())
        } else {
            ToolResult::new(call.id.clone(), ToolOutcome::success(value))
        }
    }

    /// The status at this boundary: the step's prepared status, its frontier moved to now.
    fn live_status(
        session: &Session,
        turn: &ManagedTurn<'_>,
    ) -> Result<ContextStatus, BundleError> {
        let mut status = turn
            .step
            .borrow()
            .status
            .clone()
            .ok_or_else(|| super::refusal(ErrorCode::StaleBase))?;
        let snapshot = Self::snapshot(session)?;
        status.frontier =
            state::frontier(session, snapshot.state.revision()).map_err(super::refusal)?;
        status.revision = snapshot.state.revision();
        status.hidden_fragments = u64::try_from(snapshot.state.hidden().len()).unwrap_or(u64::MAX);
        status.protected_fragments = u64::try_from(snapshot.protected.len()).unwrap_or(u64::MAX);
        status.last_decision = snapshot.state.last_decision;
        Ok(status)
    }

    /// One inspect page, sized to the encoded output bound.
    fn inspect(
        session: &Session,
        turn: &ManagedTurn<'_>,
        input: &ContextManageInput,
        status: ContextStatus,
    ) -> ManageResult {
        let key = turn
            .context
            .map_or(&[][..], nanus_ports::ContextRuntime::cursor_key);
        let binding = format!(
            "c1:{}:{}:{}",
            status.revision,
            status.frontier.event_count,
            status.profile_digest.as_str().get(..16).unwrap_or_default()
        );
        let start = match input.cursor.as_deref() {
            None => 0,
            Some(token) => match cursor::open(key, token)
                .and_then(|payload| payload.strip_prefix(&format!("{binding}:")))
                .and_then(|index| index.parse::<usize>().ok())
            {
                Some(index) => index,
                None => return refused_manage(status, ErrorCode::CursorExpired),
            },
        };
        let all = match Self::descriptors(session, turn) {
            Ok(all) => all,
            Err(code) => return refused_manage(status, code),
        };
        page(&all, start, &status, |next| {
            cursor::seal(key, &format!("{binding}:{next}"))
        })
    }

    fn descriptors(
        session: &Session,
        turn: &ManagedTurn<'_>,
    ) -> Result<Vec<FragmentDescriptor>, ErrorCode> {
        let snapshot = Self::snapshot(session)
            .map_err(|error| error.managed_code().unwrap_or(ErrorCode::SourceCorrupt))?;
        let profile = turn
            .step
            .borrow()
            .profile
            .clone()
            .ok_or(ErrorCode::StaleBase)?;
        let lookup = super::settle::artifact_facts(session);
        let check = proposal::Snapshot {
            session,
            state: &snapshot.state,
            fragments: &snapshot.fragments,
            protected: &snapshot.protected,
            profile_digest: &profile,
            goal_revision: snapshot.goal.as_ref().map(nanus_domain::Goal::revision),
            artifacts: &lookup,
        };
        Ok(proposal::describe(&check))
    }

    /// Validates and stages one proposal. Staged is not accepted: the step must settle first.
    fn propose(
        session: &Session,
        turn: &ManagedTurn<'_>,
        input: &ContextManageInput,
        status: ContextStatus,
    ) -> ManageResult {
        let Ok(snapshot) = Self::snapshot(session) else {
            return refused_manage(status, ErrorCode::SourceCorrupt);
        };
        let Some(profile) = turn.step.borrow().profile.clone() else {
            return refused_manage(status, ErrorCode::StaleBase);
        };
        let lookup = super::settle::artifact_facts(session);
        let check = proposal::Snapshot {
            session,
            state: &snapshot.state,
            fragments: &snapshot.fragments,
            protected: &snapshot.protected,
            profile_digest: &profile,
            goal_revision: snapshot.goal.as_ref().map(nanus_domain::Goal::revision),
            artifacts: &lookup,
        };
        match proposal::stage(&check, input) {
            Ok(staged) => {
                let decision_id = format!("p-{}", session.event_count());
                let decision = ContextDecision {
                    decision_id: decision_id.clone(),
                    outcome: DecisionOutcome::Staged,
                    revision: None,
                    error_code: None,
                };
                turn.step.borrow_mut().staged = Some(Staged {
                    staged,
                    decision_id,
                });
                ManageResult {
                    status: ManageStatus::Staged,
                    context: status,
                    decision: Some(decision),
                    fragments: Vec::new(),
                    next_cursor: None,
                    error_code: None,
                }
            }
            Err(code) => refused_manage(status, code),
        }
    }

    /// Runs one `context_recall` call against the step's snapshot.
    async fn run_recall(
        &self,
        session: &Session,
        call: &ToolCall,
        turn: &ManagedTurn<'_>,
    ) -> ToolResult {
        if let Some(refused) = oversized(call, limits::RECALL_ARGUMENT_BYTES_MAX) {
            return refused;
        }
        let input = match ContextRecallInput::parse(&call.arguments) {
            Ok(input) => input,
            Err(message) => return ToolResult::failure(call.id.clone(), message),
        };
        let revision = ManagedTurn::revision(session);
        let frontier = match state::frontier(session, revision) {
            Ok(frontier) => frontier,
            Err(code) => return ToolResult::failure(call.id.clone(), code.to_string()),
        };
        let scope = crate::recall::Scope {
            session,
            frontier,
            durable: turn.durable.get(),
            archive: turn.context.and_then(nanus_ports::ContextRuntime::archive),
            key: turn
                .context
                .map_or(&[][..], nanus_ports::ContextRuntime::cursor_key),
        };
        let result = crate::recall::recall(&scope, &input).await;
        let failed = result.status != nanus_domain::context::managed::RecallStatus::Ok;
        let value = serde_json::to_value(&result).unwrap_or_default();
        if failed {
            ToolResult::failure(call.id.clone(), value.to_string())
        } else {
            ToolResult::new(call.id.clone(), ToolOutcome::success(value))
        }
    }

    /// Whether this runner can capture shell output for the archive.
    pub(in crate::agent_loop) const fn can_capture(&self) -> bool {
        self.capture.is_some()
    }

    /// Reserves archive capture for the permitted `bash` calls of a batch.
    ///
    /// Best effort once a call is authorized: a refused reservation becomes an unavailable
    /// receipt for each stream and never changes whether, or how, the command runs.
    pub(in crate::agent_loop) async fn reserve_captures(
        &self,
        session: &Session,
        calls: &[ToolCall],
        results: &[Option<ToolResult>],
        turn: &ManagedTurn<'_>,
    ) {
        let (Some(broker), Some(archive)) = (
            &self.capture,
            turn.context.and_then(nanus_ports::ContextRuntime::archive),
        ) else {
            return;
        };
        if !turn.policy().capture_shell {
            return;
        }
        for (call, result) in calls.iter().zip(results) {
            if result.is_some() || call.name.as_str() != "bash" {
                continue;
            }
            let limits = nanus_ports::CaptureLimits::default();
            match archive
                .reserve_capture(session.id(), &call.id, limits)
                .await
            {
                Ok(lease) => {
                    // A lease left over for this id is handed back and released here.
                    drop(broker.insert(lease));
                }
                Err(failure) => turn.unpublished.borrow_mut().extend(
                    [CaptureStream::Stdout, CaptureStream::Stderr]
                        .map(|stream| unavailable(call, stream, failure.reason())),
                ),
            }
        }
    }

    /// Publishes the receipts of a batch's captures after its results, and queues every
    /// finalized object for the next checkpoint to verify.
    pub(in crate::agent_loop) fn publish_captures(
        &self,
        session: &mut Session,
        calls: &[ToolCall],
        turn: &ManagedTurn<'_>,
    ) {
        let pending = core::mem::take(&mut *turn.unpublished.borrow_mut());
        for receipt in pending {
            session.append(SessionEvent::ArtifactPublished {
                payload: Box::new(receipt),
            });
        }
        let Some(broker) = &self.capture else {
            return;
        };
        for call in calls {
            for finalization in broker.take_finalizations(&call.id) {
                if let Some(artifact) = finalization.artifact {
                    turn.artifacts.borrow_mut().push(artifact);
                }
                session.append(SessionEvent::ArtifactPublished {
                    payload: Box::new(finalization.receipt),
                });
            }
            broker.discard(&call.id);
        }
    }
}

/// The receipt of a stream nothing could be reserved for.
fn unavailable(call: &ToolCall, stream: CaptureStream, reason: CaptureReason) -> ArtifactReceipt {
    ArtifactReceipt {
        artifact_id: None,
        call_id: call.id.as_str().to_owned(),
        stream,
        retained_bytes: 0,
        observed_bytes: 0,
        retained_sha256: None,
        status: CaptureStatus::Unavailable,
        reason,
        encoding: RawEncoding::Raw,
        chunk_sha256: Vec::new(),
    }
}

/// The failure for a call whose raw arguments were dropped at the decoder for their size.
fn oversized(call: &ToolCall, limit: usize) -> Option<ToolResult> {
    (call.arguments.as_str() == Some(nanus_ports::OVERSIZED_ARGUMENTS)).then(|| {
        ToolResult::failure(
            call.id.clone(),
            format!(
                "{}: the arguments exceeded {limit} bytes and were not read; send fewer edits, \
                 shorter notes or a shorter query",
                call.name
            ),
        )
    })
}

/// A refused manage result.
fn refused_manage(context: ContextStatus, code: ErrorCode) -> ManageResult {
    ManageResult {
        status: ManageStatus::Refused,
        context,
        decision: None,
        fragments: Vec::new(),
        next_cursor: None,
        error_code: Some(code),
    }
}

/// The largest page from `start` that fits the encoded bound, at most forty descriptors.
fn page(
    all: &[FragmentDescriptor],
    start: usize,
    context: &ContextStatus,
    seal: impl Fn(usize) -> String,
) -> ManageResult {
    let mut count = all
        .len()
        .saturating_sub(start)
        .min(limits::INSPECT_PAGE_MAX);
    loop {
        let end = start.saturating_add(count);
        let result = ManageResult {
            status: ManageStatus::Inspected,
            context: context.clone(),
            decision: None,
            fragments: all.get(start..end).unwrap_or_default().to_vec(),
            next_cursor: (end < all.len()).then(|| seal(end)),
            error_code: None,
        };
        let fits =
            nanus_domain::content::serialized_size(&result, limits::INSPECT_BYTES_MAX).is_ok();
        if fits || count == 0 {
            return result;
        }
        count = count.saturating_sub(1);
    }
}
