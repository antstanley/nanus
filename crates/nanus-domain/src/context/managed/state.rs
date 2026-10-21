//! The accepted managed state, folded from the log, and what it protects.
//!
//! There is no state beside the log: the policy is the last `context/mode` record, the selection
//! is the last `context/revision`, and the most recent outcome is the last `context/decision`.
//! The fold validates as it goes, so a log whose revisions do not chain — a skipped number, a
//! base that is not the revision before it, a decision id accepted twice — is refused rather than
//! half-applied.

use std::collections::BTreeSet;

use super::fragments::{Fragment, Fragments};
use super::ids::{ErrorCode, FragmentId};
use super::limits;
use super::records::{
    AttemptOutcome, AttemptPhase, ContextDecision, ContextFrontier, ContextMode, ContextPolicy,
    DecisionOutcome, ProjectionRevision, RecoveryReason, RecoveryRecord, RequestAttemptRecord,
    WorkingNote,
};
use crate::session::{SESSION_FORMAT_VERSION, Session, SessionEvent, SessionLog, TurnEndReason};

/// The managed state a log has accepted.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ManagedState {
    /// The policy of the last mode record; `None` when the session never recorded one.
    pub policy: Option<ContextPolicy>,
    /// The last accepted revision; `None` is revision zero, the raw selection.
    pub accepted: Option<ProjectionRevision>,
    /// The highest revision number ever accepted, which a reset advances past.
    pub max_revision: u64,
    /// The last decision recorded.
    pub last_decision: Option<ContextDecision>,
    /// Where the last mode record is.
    pub mode_seq: Option<u64>,
}

impl ManagedState {
    /// Folds and validates the managed records of a log.
    ///
    /// A reset is a barrier. What came before the last reset revision is read only for the
    /// facts a reset preserves — the policy records, the highest revision number, the last
    /// decision — and is never validated as a selection, so a reset recovers a session whose
    /// earlier projection is invalid without executing it. Everything from the reset on is
    /// validated strictly.
    ///
    /// # Errors
    ///
    /// Returns the code of the first record that breaks the chain.
    pub fn fold(log: &SessionLog) -> Result<Self, ErrorCode> {
        let events = log.events();
        let barrier = events
            .iter()
            .rposition(|event| {
                matches!(event, SessionEvent::ContextRevision { payload }
                    if payload.reason == super::records::RevisionReason::Reset)
            })
            .unwrap_or(0);
        let mut state = Self::default();
        for (index, event) in events.iter().enumerate().take(barrier) {
            match event {
                SessionEvent::ContextMode { payload } => {
                    state.policy = Some(payload.policy);
                    state.mode_seq = u64::try_from(index).ok();
                }
                SessionEvent::ContextRevision { payload } => {
                    state.max_revision = state.max_revision.max(payload.revision);
                    state.accepted = Some((**payload).clone());
                }
                SessionEvent::ContextDecision { payload } => {
                    state.last_decision = Some((**payload).clone());
                }
                _ => {}
            }
        }
        state.fold_strict(events, barrier)?;
        check_attempts(events)?;
        check_handoff_positions(None, events).map_err(|(_, code)| code)?;
        check_compacted_mode(events)?;
        Ok(state)
    }

    /// Folds `events[start..]` strictly onto the state the prefix left.
    fn fold_strict(&mut self, events: &[SessionEvent], start: usize) -> Result<(), ErrorCode> {
        let state = self;
        let mut accepted_ids: BTreeSet<&str> = BTreeSet::new();
        let mut decided_ids: BTreeSet<&str> = BTreeSet::new();
        for (index, event) in events.iter().enumerate().skip(start) {
            let seq = u64::try_from(index).map_err(|_| ErrorCode::SourceCorrupt)?;
            match event {
                SessionEvent::ContextMode { payload } => {
                    payload.policy.validate()?;
                    state.policy = Some(payload.policy);
                    state.mode_seq = Some(seq);
                }
                SessionEvent::ContextRevision { payload } => {
                    payload.validate()?;
                    let next = state
                        .max_revision
                        .checked_add(1)
                        .ok_or(ErrorCode::StaleBase)?;
                    // A reset's base is the highest number before it, whatever was selected.
                    let base = if index == start && start > 0 {
                        state.max_revision
                    } else {
                        state.revision()
                    };
                    let chained = payload.revision == next
                        && payload.base_revision == base
                        && payload.base_frontier.event_count <= seq;
                    if !chained || !accepted_ids.insert(&payload.decision_id) {
                        return Err(ErrorCode::StaleBase);
                    }
                    state.max_revision = payload.revision;
                    state.accepted = Some((**payload).clone());
                }
                SessionEvent::ContextDecision { payload } => {
                    let accepted = payload.outcome == DecisionOutcome::Accepted;
                    let matches = state
                        .accepted
                        .as_ref()
                        .is_some_and(|revision| revision.decision_id == payload.decision_id);
                    // A decision is accepted once: a second acceptance record for one id is a
                    // replayed record, not a second decision.
                    if accepted && (!matches || !decided_ids.insert(&payload.decision_id)) {
                        return Err(ErrorCode::StaleBase);
                    }
                    state.last_decision = Some((**payload).clone());
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Returns the highest syntactically valid revision number, without validating selections.
    ///
    /// What a reset of an invalid projection advances past: the raw envelope and order are
    /// valid (the session decoded), the selection may not be, and it is never executed here.
    #[must_use]
    pub fn highest_revision(log: &SessionLog) -> u64 {
        log.events()
            .iter()
            .filter_map(|event| match event {
                SessionEvent::ContextRevision { payload } => Some(payload.revision),
                _ => None,
            })
            .max()
            .unwrap_or(0)
    }

    /// The accepted revision number; zero is the raw selection.
    #[must_use]
    pub fn revision(&self) -> u64 {
        self.accepted
            .as_ref()
            .map_or(0, |revision| revision.revision)
    }

    /// The mode in force; legacy when no mode record exists.
    #[must_use]
    pub fn mode(&self) -> ContextMode {
        self.policy
            .map_or(ContextMode::Legacy, |policy| policy.mode)
    }

    /// The policy in force; the default when no mode record exists.
    #[must_use]
    pub fn policy_or_default(&self) -> ContextPolicy {
        self.policy.unwrap_or_default()
    }

    /// The accepted hidden set.
    #[must_use]
    pub fn hidden(&self) -> &[FragmentId] {
        self.accepted
            .as_ref()
            .map_or(&[], |revision| &revision.hidden)
    }

    /// The accepted notes.
    #[must_use]
    pub fn notes(&self) -> &[WorkingNote] {
        self.accepted
            .as_ref()
            .map_or(&[], |revision| &revision.notes)
    }

    /// The goal revision the accepted notes are bound to.
    #[must_use]
    pub fn notes_goal_revision(&self) -> Option<u64> {
        self.accepted
            .as_ref()
            .and_then(|revision| revision.goal_revision)
    }
}

/// Checks where a seed sits and that a session's handoffs are distinct and move forward.
///
/// A seed is only ever the second event, directly after the managed `context/mode` record that
/// opens the new session, so the leading system message it becomes is never built from a seed the
/// log could not have written. A session may hand off more than once, so each `context/handoff`
/// names a new session of its own and a frontier strictly past the one before it: a replayed or
/// reordered record is refused. When the holding session's id is known — on a session write or
/// read, not in a fold of a bare log — a back-link's frontier must name that session, since it
/// says where in *this* session the handoff was made.
///
/// The session runs this on every write and read, and the managed fold runs it again, so a file
/// that breaks it is never opened and a log assembled in memory is never folded.
///
/// # Errors
///
/// Returns the index of the offending event with [`ErrorCode::HandoffUnavailable`] for a seed out
/// of place, a back-link naming another session, and a repeated or backwards handoff record.
pub(crate) fn check_handoff_positions(
    session_id: Option<&str>,
    events: &[SessionEvent],
) -> Result<(), (usize, ErrorCode)> {
    let mut new_sessions: BTreeSet<&str> = BTreeSet::new();
    let mut last_frontier: Option<u64> = None;
    for (index, event) in events.iter().enumerate() {
        let refused = |code| (index, code);
        match event {
            SessionEvent::ContextSeed { payload } => {
                payload.validate().map_err(refused)?;
                let opened_managed = matches!(
                    events.first(),
                    Some(SessionEvent::ContextMode { payload })
                        if payload.policy.mode == ContextMode::Managed
                );
                if index != 1 || !opened_managed {
                    return Err(refused(ErrorCode::HandoffUnavailable));
                }
            }
            SessionEvent::ContextHandoff { payload } => {
                payload.validate().map_err(refused)?;
                let frontier = payload.source_frontier.event_count;
                let moves_forward = last_frontier.is_none_or(|last| frontier > last);
                let names_holder =
                    session_id.is_none_or(|id| payload.source_frontier.session_id == id);
                if !moves_forward || !names_holder || !new_sessions.insert(&payload.new_session_id)
                {
                    return Err(refused(ErrorCode::HandoffUnavailable));
                }
                last_frontier = Some(frontier);
            }
            _ => {}
        }
    }
    Ok(())
}

/// Refuses managed context enabled on a session that a compaction has cut.
///
/// Managed selection keeps every user message, which is exactly what a cut removes, so a compacted
/// session cannot become managed. Enabling is a `context/mode` record that sets the managed policy,
/// and one after a marker is refused wherever the log is folded. The session refuses it too, on
/// every write and every read, with the same code, so a file that holds one is never opened.
///
/// # Errors
///
/// Returns [`ErrorCode::CompactedSession`] for a managed mode record after a compaction marker.
fn check_compacted_mode(events: &[SessionEvent]) -> Result<(), ErrorCode> {
    let Some(first_marker) = events
        .iter()
        .position(|event| matches!(event, SessionEvent::ContextCompact { .. }))
    else {
        return Ok(());
    };
    let enables_managed = events.iter().skip(first_marker).any(|event| {
        matches!(event, SessionEvent::ContextMode { payload }
            if payload.policy.mode == ContextMode::Managed)
    });
    if enables_managed {
        Err(ErrorCode::CompactedSession)
    } else {
        Ok(())
    }
}

/// Refuses an attempt finished twice, or finished without having started.
///
/// An attempt's outcome is recorded once: a second finished record would count its usage twice
/// and let an accounting reader choose between two outcomes for one request.
fn check_attempts(events: &[SessionEvent]) -> Result<(), ErrorCode> {
    let mut started: BTreeSet<&str> = BTreeSet::new();
    let mut finished: BTreeSet<&str> = BTreeSet::new();
    for event in events {
        let SessionEvent::RequestAttempt { payload } = event else {
            continue;
        };
        let fresh = match payload.phase {
            AttemptPhase::Started => started.insert(&payload.attempt_id),
            AttemptPhase::Finished => {
                started.contains(payload.attempt_id.as_str())
                    && finished.insert(&payload.attempt_id)
            }
        };
        if !fresh {
            return Err(ErrorCode::SourceCorrupt);
        }
    }
    Ok(())
}

/// Returns the frontier of `session` at its full length.
///
/// # Errors
///
/// Returns [`ErrorCode::SourceCorrupt`] when the prefix cannot be encoded.
pub fn frontier(session: &Session, projection_revision: u64) -> Result<ContextFrontier, ErrorCode> {
    let count = u64::try_from(session.event_count()).map_err(|_| ErrorCode::SourceCorrupt)?;
    frontier_at(session, count, projection_revision)
}

/// Returns the frontier of `session` at `count` events.
///
/// # Errors
///
/// Returns [`ErrorCode::SourceCorrupt`] when `count` is past the log.
pub fn frontier_at(
    session: &Session,
    count: u64,
    projection_revision: u64,
) -> Result<ContextFrontier, ErrorCode> {
    let prefix_blake3 = session
        .prefix_digest(count)
        .map_err(|_| ErrorCode::SourceCorrupt)?;
    Ok(ContextFrontier {
        session_id: session.id().as_str().to_owned(),
        event_count: count,
        prefix_blake3,
        projection_revision,
    })
}

/// Checks that a frontier describes a prefix of `session` exactly.
///
/// # Errors
///
/// Returns [`ErrorCode::StaleBase`] for another session, a count past the end or a digest that
/// does not match the stored prefix.
pub fn check_frontier(session: &Session, frontier: &ContextFrontier) -> Result<(), ErrorCode> {
    let count = u64::try_from(session.event_count()).map_err(|_| ErrorCode::SourceCorrupt)?;
    if frontier.session_id != session.id().as_str() || frontier.event_count > count {
        return Err(ErrorCode::StaleBase);
    }
    let actual = session
        .prefix_digest(frontier.event_count)
        .map_err(|_| ErrorCode::StaleBase)?;
    if actual == frontier.prefix_blake3 {
        return Ok(());
    }
    // The digest covers the header, and a body's version moves as it gains records — typed user
    // content takes it to 4, a handoff draft's `kind` attempt to 5 — while the events it covers do
    // not change. A frontier taken before such a move names the header the body had then, so it
    // is checked against each earlier version too; a digest that matches none is still refused.
    let earlier = SESSION_FORMAT_VERSION..session.body_version();
    let matches_earlier = earlier.into_iter().any(|version| {
        session
            .prefix_digest_at_version(frontier.event_count, version)
            .is_ok_and(|digest| digest == frontier.prefix_blake3)
    });
    if matches_earlier {
        Ok(())
    } else {
        Err(ErrorCode::StaleBase)
    }
}

/// Checks that the accepted revision still describes this session.
///
/// Its base frontier is a prefix of the log, and every hidden id names a settled fragment below
/// that base which nothing now protects.
///
/// # Errors
///
/// Returns [`ErrorCode::SourceCorrupt`] for an accepted state the log no longer supports.
pub fn check_accepted(
    session: &Session,
    state: &ManagedState,
    fragments: &Fragments,
    protected: &BTreeSet<FragmentId>,
) -> Result<(), ErrorCode> {
    let Some(revision) = &state.accepted else {
        return Ok(());
    };
    check_frontier(session, &revision.base_frontier).map_err(|_| ErrorCode::SourceCorrupt)?;
    for id in &revision.hidden {
        let fragment = fragments.get(*id).ok_or(ErrorCode::SourceCorrupt)?;
        let below = fragment.end() <= revision.base_frontier.event_count;
        if !fragment.settled || !below || protected.contains(id) {
            return Err(ErrorCode::SourceCorrupt);
        }
    }
    Ok(())
}

/// Returns the fragments nothing may hide.
///
/// Unsettled fragments; the latest two completed substantive fragments (or all, when fewer);
/// and the newest management fragment until a completed, admitted response to a request that
/// carried it is in the log.
#[must_use]
pub fn protected(log: &SessionLog, fragments: &Fragments) -> BTreeSet<FragmentId> {
    let mut protected: BTreeSet<FragmentId> = fragments
        .all()
        .iter()
        .filter(|fragment| !fragment.settled)
        .map(|fragment| fragment.id)
        .collect();
    protected.extend(
        fragments
            .all()
            .iter()
            .rev()
            .filter(|fragment| fragment.settled && fragment.is_substantive())
            .take(limits::RECENT_PROTECTED)
            .map(|fragment| fragment.id),
    );
    if let Some(management) = fragments.all().iter().rev().find(|f| f.management)
        && !consumed(log, management)
    {
        protected.insert(management.id);
    }
    protected
}

/// Whether a later completed, admitted response came from a request that carried `fragment`.
fn consumed(log: &SessionLog, fragment: &Fragment) -> bool {
    log.events().iter().any(|event| match event {
        SessionEvent::RequestAttempt { payload } => {
            payload.phase == AttemptPhase::Finished
                && payload.outcome == Some(AttemptOutcome::Completed)
                && payload.assistant_seq.is_some()
                && payload.included_management_fragments.contains(&fragment.id)
        }
        _ => false,
    })
}

/// Returns the fragments a request carries that a later response must consume.
#[must_use]
pub fn management_in_flight(
    fragments: &Fragments,
    protected: &BTreeSet<FragmentId>,
) -> Vec<FragmentId> {
    fragments
        .all()
        .iter()
        .rev()
        .find(|fragment| fragment.management && fragment.settled)
        .filter(|fragment| protected.contains(&fragment.id))
        .map(|fragment| fragment.id)
        .into_iter()
        .collect()
}

/// What a resumed session must append before it admits new work.
#[derive(Clone, Debug, PartialEq)]
pub struct RecoveryPlan {
    /// The records to append, in order.
    pub events: Vec<SessionEvent>,
}

/// Plans the closing of what a checkpoint left open: a turn, or a maintenance request's intent.
///
/// `None` when nothing is open. A step still open is closed too: a checkpoint never writes one,
/// so finding one is a damaged history, and it is closed as a failure rather than completed with
/// fabricated results. An intent with no outcome is finished as `unknown_dispatch` — it may or
/// may not have been sent, and it is never evidence of a paid request. Nothing is rerun.
///
/// A maintenance request (a handoff draft) runs outside any turn, so its unanswered intent is
/// closed on its own, under a `settled_maintenance_attempt` recovery record, and never under a
/// turn it had nothing to do with. Recovery runs before every managed turn and every draft, so an
/// idle session's dangling intent is settled before anything new is admitted.
///
/// # Errors
///
/// Returns [`ErrorCode::SourceCorrupt`] when the frontier cannot be encoded.
pub fn recovery_plan(
    session: &Session,
    state: &ManagedState,
    now_ms: u64,
) -> Result<Option<RecoveryPlan>, ErrorCode> {
    let log = session.log();
    let open = open_turn(log);
    let (maintenance, steps): (Vec<_>, Vec<_>) = unmatched_attempts(log)
        .into_iter()
        .partition(|attempt| attempt.kind.is_some());
    if open.is_none() && maintenance.is_empty() {
        return Ok(None);
    }
    let recovered_frontier = frontier(session, state.revision())?;
    let mut events = Vec::new();
    if let Some(first) = maintenance.first() {
        let turn = first.turn;
        events.extend(finished_unknown(&maintenance, now_ms));
        events.push(recovery_record(
            &recovered_frontier,
            turn,
            &maintenance,
            RecoveryReason::SettledMaintenanceAttempt,
        ));
    }
    let Some(turn) = open else {
        return Ok(Some(RecoveryPlan { events }));
    };
    let open_step = open_step(log);
    if let Some((step_turn, step)) = open_step {
        events.push(SessionEvent::StepEnd {
            turn: step_turn,
            step,
        });
    }
    events.extend(finished_unknown(&steps, now_ms));
    events.push(recovery_record(
        &recovered_frontier,
        u64::from(turn),
        &steps,
        RecoveryReason::SettledOpenTurn,
    ));
    let reason = if open_step.is_some() {
        TurnEndReason::Error {
            message: "recovery closed a step a checkpoint should not have held".to_owned(),
        }
    } else {
        TurnEndReason::Interrupted
    };
    events.push(SessionEvent::TurnEnd { turn, reason });
    Ok(Some(RecoveryPlan { events }))
}

/// The finished records that close `attempts` as unknown dispatches.
fn finished_unknown(attempts: &[RequestAttemptRecord], now_ms: u64) -> Vec<SessionEvent> {
    attempts
        .iter()
        .map(|attempt| {
            let mut finished = attempt.clone();
            finished.phase = AttemptPhase::Finished;
            finished.outcome = Some(AttemptOutcome::UnknownDispatch);
            finished.finished_at_ms = Some(now_ms);
            SessionEvent::RequestAttempt {
                payload: Box::new(finished),
            }
        })
        .collect()
}

/// The recovery record naming what was closed, and why.
fn recovery_record(
    recovered_frontier: &ContextFrontier,
    turn: u64,
    closed: &[RequestAttemptRecord],
    reason: RecoveryReason,
) -> SessionEvent {
    SessionEvent::ContextRecovery {
        payload: Box::new(RecoveryRecord {
            recovered_frontier: recovered_frontier.clone(),
            turn,
            unmatched_attempt_ids: closed
                .iter()
                .take(16)
                .map(|attempt| attempt.attempt_id.clone())
                .collect(),
            reason,
        }),
    }
}

/// The turn started and not ended, when there is one.
fn open_turn(log: &SessionLog) -> Option<u32> {
    let mut open = None;
    for event in log.events() {
        match event {
            SessionEvent::TurnStart { turn } => open = Some(*turn),
            SessionEvent::TurnEnd { .. } => open = None,
            _ => {}
        }
    }
    open
}

/// The step started and not ended, when there is one.
fn open_step(log: &SessionLog) -> Option<(u32, u32)> {
    let mut open = None;
    for event in log.events() {
        match event {
            SessionEvent::StepStart { turn, step } => open = Some((*turn, *step)),
            SessionEvent::StepEnd { .. } | SessionEvent::TurnEnd { .. } => open = None,
            _ => {}
        }
    }
    open
}

/// Started attempts no finished record answers, in log order.
#[must_use]
pub fn unmatched_attempts(log: &SessionLog) -> Vec<RequestAttemptRecord> {
    let mut open: Vec<RequestAttemptRecord> = Vec::new();
    for event in log.events() {
        if let SessionEvent::RequestAttempt { payload } = event {
            match payload.phase {
                AttemptPhase::Started => open.push((**payload).clone()),
                AttemptPhase::Finished => {
                    open.retain(|attempt| attempt.attempt_id != payload.attempt_id);
                }
            }
        }
    }
    open
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::managed::ids::Digest;
    use crate::context::managed::records::{RevisionAuthor, RevisionReason};
    use crate::{SessionId, ToolCall, ToolCallId, ToolName};
    use serde_json::json;

    fn session() -> Session {
        let mut session = Session::new(SessionId::new("s"), 0, "/w");
        session.upgrade_to_managed_body();
        session
    }

    fn work(session: &mut Session, id: &str, tool: &str) {
        session.append(SessionEvent::AssistantMessage {
            replay: None,
            text: None,
            reasoning: None,
            tool_calls: vec![ToolCall::new(
                ToolCallId::new(id),
                ToolName::new(tool).unwrap_or_else(|_| unreachable!("valid")),
                json!({}),
            )],
            usage: None,
            interrupted: false,
            model: None,
            effort: None,
        });
        session.append(SessionEvent::ToolResult {
            call_id: ToolCallId::new(id),
            content: "x".repeat(40),
            content_blocks: None,
            is_error: false,
        });
    }

    fn revision(
        session: &Session,
        number: u64,
        base: u64,
        hidden: Vec<FragmentId>,
    ) -> Box<ProjectionRevision> {
        let count = u64::try_from(session.event_count()).unwrap_or(0);
        Box::new(ProjectionRevision {
            revision: number,
            base_revision: base,
            base_frontier: frontier_at(session, count, base).unwrap_or_else(|_| unreachable!()),
            hidden,
            notes: Vec::new(),
            author: RevisionAuthor::Automatic,
            reason: RevisionReason::HardFit,
            policy_version: 1,
            decision_id: format!("d:{number}"),
            goal_revision: None,
            notes_digest: super::super::notes::notes_digest(&[]),
            base_profile_digest: Digest::empty(),
        })
    }

    #[test]
    fn the_latest_two_substantive_fragments_and_unsettled_work_are_protected() {
        let mut session = session();
        session.append(SessionEvent::UserMessage {
            text: "go".into(),
            content_blocks: None,
        });
        for id in ["a", "b", "c"] {
            work(&mut session, id, "read");
        }
        let fragments = super::super::fragments::derive(session.log()).unwrap_or_default();
        let protected = protected(session.log(), &fragments);
        assert_eq!(
            protected,
            BTreeSet::from([FragmentId::new(3), FragmentId::new(5)])
        );
        assert!(!protected.contains(&FragmentId::new(1)));
    }

    #[test]
    fn revisions_must_chain_and_a_decision_cannot_be_accepted_twice() {
        let mut session = session();
        session.append(SessionEvent::UserMessage {
            text: "go".into(),
            content_blocks: None,
        });
        work(&mut session, "a", "read");
        let first = revision(&session, 1, 0, vec![FragmentId::new(1)]);
        session.append(SessionEvent::ContextRevision { payload: first });
        let state = ManagedState::fold(session.log());
        assert_eq!(state.as_ref().map(ManagedState::revision), Ok(1));

        let mut skipped = session.clone();
        let jump = revision(&skipped, 3, 1, Vec::new());
        skipped.append(SessionEvent::ContextRevision { payload: jump });
        assert_eq!(ManagedState::fold(skipped.log()), Err(ErrorCode::StaleBase));
        assert_eq!(ManagedState::highest_revision(skipped.log()), 3);

        let mut replayed = session.clone();
        let mut again = revision(&replayed, 2, 1, Vec::new());
        again.decision_id = "d:1".to_owned();
        replayed.append(SessionEvent::ContextRevision { payload: again });
        assert_eq!(
            ManagedState::fold(replayed.log()),
            Err(ErrorCode::StaleBase)
        );
    }

    /// T35: a reset revision is a barrier — an invalid selection before it is never validated
    /// or executed, numbers keep rising, and the selection after it is empty.
    #[test]
    fn a_reset_recovers_an_invalid_projection_and_keeps_numbers_monotonic() {
        let mut session = session();
        session.append(SessionEvent::UserMessage {
            text: "go".into(),
            content_blocks: None,
        });
        work(&mut session, "a", "read");
        let first = revision(&session, 1, 0, Vec::new());
        session.append(SessionEvent::ContextRevision { payload: first });
        let jump = revision(&session, 5, 1, Vec::new());
        session.append(SessionEvent::ContextRevision { payload: jump });
        assert!(
            ManagedState::fold(session.log()).is_err(),
            "a broken chain refuses"
        );
        assert_eq!(ManagedState::highest_revision(session.log()), 5);

        let mut reset = revision(&session, 6, 5, Vec::new());
        reset.reason = RevisionReason::Reset;
        reset.author = RevisionAuthor::Host;
        session.append(SessionEvent::ContextRevision { payload: reset });
        let state = ManagedState::fold(session.log());
        assert_eq!(state.as_ref().map(ManagedState::revision), Ok(6));
        assert_eq!(state.as_ref().map(|state| state.hidden().len()), Ok(0));

        let mut restarted = session.clone();
        let mut low = revision(&restarted, 6, 5, Vec::new());
        low.reason = RevisionReason::Reset;
        low.author = RevisionAuthor::Host;
        low.decision_id = "d:again".into();
        restarted.append(SessionEvent::ContextRevision { payload: low });
        assert!(
            ManagedState::fold(restarted.log()).is_err(),
            "numbers never restart"
        );
    }

    /// F18: an acceptance or an attempt outcome recorded twice is refused.
    #[test]
    fn a_decision_or_an_attempt_outcome_is_recorded_once() {
        use crate::context::managed::records::{
            AttemptPhase, ContextDecision, DecisionOutcome, RequestAttemptRecord, SelectionIdentity,
        };
        let mut session = session();
        session.append(SessionEvent::UserMessage {
            text: "go".into(),
            content_blocks: None,
        });
        let first = revision(&session, 1, 0, Vec::new());
        session.append(SessionEvent::ContextRevision { payload: first });
        let accepted = ContextDecision {
            decision_id: "d:1".into(),
            outcome: DecisionOutcome::Accepted,
            revision: Some(1),
            error_code: None,
        };
        session.append(SessionEvent::ContextDecision {
            payload: Box::new(accepted.clone()),
        });
        assert!(ManagedState::fold(session.log()).is_ok());
        let mut twice = session.clone();
        twice.append(SessionEvent::ContextDecision {
            payload: Box::new(accepted),
        });
        assert_eq!(ManagedState::fold(twice.log()), Err(ErrorCode::StaleBase));

        let attempt = |phase| RequestAttemptRecord {
            attempt_id: "a1".into(),
            retry_of: None,
            turn: 1,
            step: 1,
            selection: SelectionIdentity {
                provider: "p".into(),
                endpoint_digest: Digest::empty(),
                protocol: "c".into(),
                model: "m".into(),
                effort: None,
                epoch: 0,
            },
            projection_revision: 0,
            request_digest: Digest::empty(),
            phase,
            outcome: (phase == AttemptPhase::Finished).then_some(AttemptOutcome::Failed),
            usage: None,
            assistant_seq: None,
            included_management_fragments: Vec::new(),
            kind: None,
            started_at_ms: 0,
            finished_at_ms: None,
            timings_ms: None,
        };
        let mut attempts = session.clone();
        attempts.append(SessionEvent::RequestAttempt {
            payload: Box::new(attempt(AttemptPhase::Started)),
        });
        attempts.append(SessionEvent::RequestAttempt {
            payload: Box::new(attempt(AttemptPhase::Finished)),
        });
        assert!(ManagedState::fold(attempts.log()).is_ok());
        attempts.append(SessionEvent::RequestAttempt {
            payload: Box::new(attempt(AttemptPhase::Finished)),
        });
        assert_eq!(
            ManagedState::fold(attempts.log()),
            Err(ErrorCode::SourceCorrupt)
        );
    }

    #[test]
    fn a_frontier_detects_a_changed_prefix() {
        let mut session = session();
        session.append(SessionEvent::UserMessage {
            text: "go".into(),
            content_blocks: None,
        });
        let at_one = frontier(&session, 0).unwrap_or_else(|_| unreachable!());
        session.append(SessionEvent::UserMessage {
            text: "more".into(),
            content_blocks: None,
        });
        assert!(
            check_frontier(&session, &at_one).is_ok(),
            "appends keep the prefix"
        );
        let mut other = Session::new(SessionId::new("s"), 0, "/w");
        other.upgrade_to_managed_body();
        other.append(SessionEvent::UserMessage {
            text: "GO".into(),
            content_blocks: None,
        });
        assert_eq!(check_frontier(&other, &at_one), Err(ErrorCode::StaleBase));
        let mut future = at_one;
        future.event_count = 9;
        assert_eq!(check_frontier(&session, &future), Err(ErrorCode::StaleBase));
    }

    /// An accepted revision whose base frontier was taken at `count` events under today's header.
    fn accepted_at(session: &Session) -> ManagedState {
        ManagedState {
            accepted: Some(*revision(session, 1, 0, Vec::new())),
            max_revision: 1,
            ..ManagedState::default()
        }
    }

    fn accepted_holds(session: &Session, state: &ManagedState) -> Result<(), ErrorCode> {
        let fragments = super::super::fragments::derive(session.log()).unwrap_or_default();
        let floor = protected(session.log(), &fragments);
        check_accepted(session, state, &fragments, &floor)
    }

    #[test]
    fn a_frontier_taken_before_the_body_moved_version_still_describes_its_prefix() {
        let mut session = session();
        session.append(SessionEvent::UserMessage {
            text: "go".into(),
            content_blocks: None,
        });
        assert_eq!(session.body_version(), 3);
        let at_three = frontier(&session, 0).unwrap_or_else(|_| unreachable!());
        let state = accepted_at(&session);
        // Typed user content moves the body to version 4.
        session.append(SessionEvent::UserMessage {
            text: "look".into(),
            content_blocks: Some(vec![crate::ContentBlock::Text("look".into())]),
        });
        assert_eq!(session.body_version(), 4);
        assert_eq!(check_frontier(&session, &at_three), Ok(()));
        assert_eq!(accepted_holds(&session, &state), Ok(()));
        let at_four = frontier(&session, 0).unwrap_or_else(|_| unreachable!());
        // A handoff draft's attempt carries a `kind`, which moves the body to version 5.
        session.append(handoff_intent(0));
        assert_eq!(session.body_version(), 5);
        assert_eq!(check_frontier(&session, &at_three), Ok(()));
        assert_eq!(check_frontier(&session, &at_four), Ok(()));
        assert_eq!(accepted_holds(&session, &state), Ok(()));
    }

    #[test]
    fn a_frontier_whose_digest_matches_no_version_of_the_header_is_refused() {
        let mut session = session();
        session.append(SessionEvent::UserMessage {
            text: "go".into(),
            content_blocks: None,
        });
        let mut forged = frontier(&session, 0).unwrap_or_else(|_| unreachable!());
        forged.prefix_blake3 = Digest::of(b"another prefix");
        let mut state = accepted_at(&session);
        if let Some(revision) = state.accepted.as_mut() {
            revision.base_frontier.prefix_blake3 = Digest::of(b"another prefix");
        }
        session.append(handoff_intent(0));
        assert_eq!(session.body_version(), 5);
        assert_eq!(check_frontier(&session, &forged), Err(ErrorCode::StaleBase));
        assert_eq!(
            accepted_holds(&session, &state),
            Err(ErrorCode::SourceCorrupt)
        );
    }

    #[test]
    fn recovery_closes_an_open_turn_and_marks_unmatched_intents_unknown() {
        let mut session = session();
        session.append(SessionEvent::TurnStart { turn: 1 });
        session.append(SessionEvent::UserMessage {
            text: "go".into(),
            content_blocks: None,
        });
        let plan = recovery_plan(&session, &ManagedState::default(), 5);
        let Ok(Some(plan)) = plan else {
            panic!("an open turn needs recovery: {plan:?}");
        };
        assert!(matches!(
            plan.events.last(),
            Some(SessionEvent::TurnEnd {
                reason: TurnEndReason::Interrupted,
                ..
            })
        ));
        session.append(SessionEvent::TurnEnd {
            turn: 1,
            reason: TurnEndReason::Completed,
        });
        assert_eq!(
            recovery_plan(&session, &ManagedState::default(), 5),
            Ok(None)
        );
    }

    /// The intent a handoff draft records before it sends, with no outcome yet.
    fn handoff_intent(turn: u64) -> SessionEvent {
        use crate::context::managed::records::{MaintenanceAttemptKind, SelectionIdentity};
        SessionEvent::RequestAttempt {
            payload: Box::new(RequestAttemptRecord {
                attempt_id: format!("t{turn}s0e2"),
                retry_of: None,
                turn,
                step: 0,
                selection: SelectionIdentity {
                    provider: "p".into(),
                    endpoint_digest: Digest::of(b"e"),
                    protocol: "chat".into(),
                    model: "m".into(),
                    effort: None,
                    epoch: 1,
                },
                projection_revision: 0,
                request_digest: Digest::of(b"r"),
                phase: AttemptPhase::Started,
                outcome: None,
                usage: None,
                assistant_seq: None,
                included_management_fragments: Vec::new(),
                kind: Some(MaintenanceAttemptKind::Handoff),
                started_at_ms: 0,
                finished_at_ms: None,
                timings_ms: None,
            }),
        }
    }

    #[test]
    fn a_handoff_intent_left_open_in_an_idle_session_is_settled_on_its_own() {
        let mut session = session();
        session.append(SessionEvent::TurnStart { turn: 0 });
        session.append(SessionEvent::TurnEnd {
            turn: 0,
            reason: TurnEndReason::Completed,
        });
        session.append(handoff_intent(0));
        let plan = recovery_plan(&session, &ManagedState::default(), 5);
        let Ok(Some(plan)) = plan else {
            panic!("a dangling intent needs recovery: {plan:?}");
        };
        assert!(matches!(
            &plan.events[0],
            SessionEvent::RequestAttempt { payload }
                if payload.outcome == Some(AttemptOutcome::UnknownDispatch)
        ));
        assert!(matches!(
            &plan.events[1],
            SessionEvent::ContextRecovery { payload }
                if payload.reason == RecoveryReason::SettledMaintenanceAttempt
        ));
        assert_eq!(plan.events.len(), 2, "no turn is opened or closed for it");
        for event in plan.events {
            session.append(event);
        }
        assert_eq!(
            recovery_plan(&session, &ManagedState::default(), 5),
            Ok(None)
        );
    }

    #[test]
    fn an_open_turn_recovered_beside_a_dangling_intent_keeps_the_two_apart() {
        let mut session = session();
        session.append(handoff_intent(0));
        session.append(SessionEvent::TurnStart { turn: 1 });
        let Ok(Some(plan)) = recovery_plan(&session, &ManagedState::default(), 5) else {
            panic!("an open turn and an intent need recovery");
        };
        let reasons: Vec<RecoveryReason> = plan
            .events
            .iter()
            .filter_map(|event| match event {
                SessionEvent::ContextRecovery { payload } => Some(payload.reason),
                _ => None,
            })
            .collect();
        assert_eq!(
            reasons,
            [
                RecoveryReason::SettledMaintenanceAttempt,
                RecoveryReason::SettledOpenTurn
            ]
        );
        let turn_record = plan.events.iter().rev().find_map(|event| match event {
            SessionEvent::ContextRecovery { payload } => Some(payload),
            _ => None,
        });
        assert!(turn_record.is_some_and(|record| record.unmatched_attempt_ids.is_empty()));
    }
}
