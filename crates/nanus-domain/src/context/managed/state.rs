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
use crate::session::{Session, SessionEvent, SessionLog, TurnEndReason};

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
        Ok(state)
    }

    /// Folds `events[start..]` strictly onto the state the prefix left.
    fn fold_strict(&mut self, events: &[SessionEvent], start: usize) -> Result<(), ErrorCode> {
        let state = self;
        let mut accepted_ids: BTreeSet<&str> = BTreeSet::new();
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
                    if accepted && !matches {
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
    let prefix_sha256 = session
        .prefix_digest(count)
        .map_err(|_| ErrorCode::SourceCorrupt)?;
    Ok(ContextFrontier {
        session_id: session.id().as_str().to_owned(),
        event_count: count,
        prefix_sha256,
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
    if actual == frontier.prefix_sha256 {
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

/// Plans the closing of a turn a checkpoint left open.
///
/// `None` when nothing is open. A step still open is closed too: a checkpoint never writes one,
/// so finding one is a damaged history, and it is closed as a failure rather than completed with
/// fabricated results. An intent with no outcome is finished as `unknown_dispatch` — it may or
/// may not have been sent, and it is never evidence of a paid request. Nothing is rerun.
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
    let Some(turn) = open_turn(log) else {
        return Ok(None);
    };
    let recovered_frontier = frontier(session, state.revision())?;
    let mut events = Vec::new();
    let open_step = open_step(log);
    if let Some((step_turn, step)) = open_step {
        events.push(SessionEvent::StepEnd {
            turn: step_turn,
            step,
        });
    }
    let unmatched = unmatched_attempts(log);
    for attempt in &unmatched {
        let mut finished = attempt.clone();
        finished.phase = AttemptPhase::Finished;
        finished.outcome = Some(AttemptOutcome::UnknownDispatch);
        finished.finished_at_ms = Some(now_ms);
        events.push(SessionEvent::RequestAttempt {
            payload: Box::new(finished),
        });
    }
    events.push(SessionEvent::ContextRecovery {
        payload: Box::new(RecoveryRecord {
            recovered_frontier,
            turn: u64::from(turn),
            unmatched_attempt_ids: unmatched
                .iter()
                .take(16)
                .map(|attempt| attempt.attempt_id.clone())
                .collect(),
            reason: RecoveryReason::SettledOpenTurn,
        }),
    });
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
        session.append(SessionEvent::UserMessage { text: "go".into() });
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
        session.append(SessionEvent::UserMessage { text: "go".into() });
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
        session.append(SessionEvent::UserMessage { text: "go".into() });
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

    #[test]
    fn a_frontier_detects_a_changed_prefix() {
        let mut session = session();
        session.append(SessionEvent::UserMessage { text: "go".into() });
        let at_one = frontier(&session, 0).unwrap_or_else(|_| unreachable!());
        session.append(SessionEvent::UserMessage {
            text: "more".into(),
        });
        assert!(
            check_frontier(&session, &at_one).is_ok(),
            "appends keep the prefix"
        );
        let mut other = Session::new(SessionId::new("s"), 0, "/w");
        other.upgrade_to_managed_body();
        other.append(SessionEvent::UserMessage { text: "GO".into() });
        assert_eq!(check_frontier(&other, &at_one), Err(ErrorCode::StaleBase));
        let mut future = at_one;
        future.event_count = 9;
        assert_eq!(check_frontier(&session, &future), Err(ErrorCode::StaleBase));
    }

    #[test]
    fn recovery_closes_an_open_turn_and_marks_unmatched_intents_unknown() {
        let mut session = session();
        session.append(SessionEvent::TurnStart { turn: 1 });
        session.append(SessionEvent::UserMessage { text: "go".into() });
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
}
