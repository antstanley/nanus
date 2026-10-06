//! One managed step, from preparation to its settled checkpoint.

use std::time::Instant;

use nanus_domain::context::managed::{
    AttemptPhase, AttemptTimings, ContextDecision, ContextStatus, DecisionOutcome, Digest,
    RequestAttemptRecord, limits,
};
use nanus_domain::{Session, SessionEvent, StepOutcome, ToolCallId, ToolName, Usage};
use nanus_ports::{
    CheckpointReason, FinishReason, PreparedModelCall, StepRecordProjection, StepRecordReservation,
    TurnRecordReservation,
};

use super::{ManagedTurn, Prepared, Staged, refusal};
use crate::BundleError;
use crate::agent_loop::{AgentRunner, Progress, dispatch, is_cancelled};

/// What one step carries between its phases.
#[derive(Default)]
pub(in crate::agent_loop) struct StepState {
    /// The snapshot profile the step's request was prepared under.
    pub(in crate::agent_loop) profile: Option<Digest>,
    /// The status the step's request was prepared under.
    pub(in crate::agent_loop) status: Option<ContextStatus>,
    /// A proposal staged during the step, waiting for it to settle.
    pub(in crate::agent_loop) staged: Option<Staged>,
}

/// What the stream told the step, beyond the assembled message.
#[derive(Debug, Default)]
pub(in crate::agent_loop) struct StreamFacts {
    pub(in crate::agent_loop) usage: Option<Usage>,
    pub(in crate::agent_loop) assistant_seq: Option<u64>,
    pub(in crate::agent_loop) timings: Option<AttemptTimings>,
}

/// A [`Progress`] that forwards everything and notes the times and usage an attempt records.
struct Timing<'p> {
    inner: &'p mut dyn Progress,
    started: Instant,
    head: Option<Instant>,
    first: Option<Instant>,
    last: Option<Instant>,
    usage: Option<Usage>,
}

impl<'p> Timing<'p> {
    fn new(inner: &'p mut dyn Progress) -> Self {
        Self {
            inner,
            started: Instant::now(),
            head: None,
            first: None,
            last: None,
            usage: None,
        }
    }

    fn token(&mut self) {
        let now = Instant::now();
        self.first.get_or_insert(now);
        self.last = Some(now);
    }

    fn finish(&self, assistant_seq: Option<u64>) -> StreamFacts {
        let millis = |from: Instant, to: Instant| {
            u64::try_from(to.saturating_duration_since(from).as_millis()).ok()
        };
        let end = Instant::now();
        StreamFacts {
            usage: self.usage,
            assistant_seq,
            timings: Some(AttemptTimings {
                response_head_ms: self.head.and_then(|head| millis(self.started, head)),
                first_token_ms: self.first.and_then(|first| millis(self.started, first)),
                decode_ms: self
                    .first
                    .zip(self.last)
                    .and_then(|(first, last)| millis(first, last)),
                total_ms: millis(self.started, end),
            }),
        }
    }
}

impl Progress for Timing<'_> {
    fn text(&mut self, delta: &str) {
        self.token();
        self.inner.text(delta);
    }
    fn reasoning(&mut self, delta: &str) {
        self.token();
        self.inner.reasoning(delta);
    }
    fn tool_call(&mut self, delta: &str) {
        self.token();
        self.inner.tool_call(delta);
    }
    fn step_started(&mut self, step: u32) {
        self.inner.step_started(step);
    }
    fn response_head(&mut self) {
        self.head.get_or_insert_with(Instant::now);
        self.inner.response_head();
    }
    fn tool_started(&mut self, id: &ToolCallId, name: &ToolName, arguments: &serde_json::Value) {
        self.inner.tool_started(id, name, arguments);
    }
    fn tool_finished(&mut self, id: &ToolCallId, name: &ToolName, is_error: bool) {
        self.inner.tool_finished(id, name, is_error);
    }
    fn usage(&mut self, usage: &Usage) {
        // Cumulative usage replaces the attempt's latest snapshot rather than adding to it.
        self.usage = Some(*usage);
        self.inner.usage(usage);
    }
    fn elided(&mut self, elision: &nanus_domain::Elision) {
        self.inner.elided(elision);
    }
    fn goal_changed(&mut self, goal: Option<&nanus_domain::Goal>) {
        self.inner.goal_changed(goal);
    }
    fn context_status(&mut self, status: &ContextStatus) {
        self.inner.context_status(status);
    }
    fn context_decision(&mut self, decision: &ContextDecision) {
        self.inner.context_decision(decision);
    }
    fn checkpointed(&mut self, receipt: &nanus_domain::context::managed::CheckpointReceipt) {
        self.inner.checkpointed(receipt);
    }
    fn cancelled(&self) -> bool {
        self.inner.cancelled()
    }
}

impl AgentRunner {
    /// Runs one managed step and settles it.
    pub(in crate::agent_loop) async fn run_managed_step(
        &self,
        session: &mut Session,
        progress: &mut dyn Progress,
        phase: dispatch::Phase<'_>,
        turn_lease: Option<&dyn TurnRecordReservation>,
    ) -> Result<StepOutcome, BundleError> {
        let Some(turn) = phase.managed else {
            return Err(refusal(
                nanus_domain::context::managed::ErrorCode::UnsupportedMode,
            ));
        };
        let _selection_hold = self.hold_selection(true)?;
        Self::check_capacity(session, limits::CLOSING_RESERVE_BYTES)?;
        let prepared = self.prepare_step(session, turn)?;
        let Prepared {
            call,
            request,
            auto,
            status,
            management,
            profile,
            revision,
        } = prepared;
        let attempt = self.started_attempt(session, phase.position, (&*call, revision, management));
        self.record_intent(session, turn, auto, &attempt, progress)
            .await?;
        progress.context_status(&status);
        *turn.step.borrow_mut() = StepState {
            profile: Some(profile),
            status: Some(status),
            staged: None,
        };
        let lease = match self.begin_managed_records(session, phase.position, turn_lease, &request)
        {
            Ok(lease) => lease,
            Err(error) => {
                // The intent is already durable, so its outcome is recorded too: refused before
                // dispatch, never an open intent in a closed turn.
                let refused = self.refused_attempt(attempt);
                session.append(SessionEvent::RequestAttempt {
                    payload: Box::new(refused),
                });
                return Err(error);
            }
        };
        let mut timing = Timing::new(progress);
        let mut assistant = None;
        let result = self
            .perform_managed(
                session,
                call,
                &mut timing,
                phase,
                (lease.as_deref(), &mut assistant),
            )
            .await;
        let facts = timing.finish(assistant);
        Self::end_step_records(session, phase.position, lease.as_deref())?;
        self.settle(
            session,
            turn,
            (attempt, facts, &result),
            progress,
            phase.control,
        )
        .await?;
        result
    }

    /// Refuses a step that could not record its own ending.
    pub(in crate::agent_loop) fn check_capacity(
        session: &Session,
        reserve: usize,
    ) -> Result<(), BundleError> {
        let used = Self::session_bytes(session)?;
        let needed = used
            .checked_add(nanus_domain::content::RECORD_BYTES_MAX)
            .and_then(|bytes| bytes.checked_add(reserve));
        if needed.is_some_and(|bytes| bytes <= nanus_domain::content::SESSION_BYTES_MAX) {
            Ok(())
        } else {
            Err(refusal(
                nanus_domain::context::managed::ErrorCode::StorageCapacity,
            ))
        }
    }

    /// The encoded size of a session, framing included.
    pub(in crate::agent_loop) fn session_bytes(session: &Session) -> Result<usize, BundleError> {
        session
            .encoded_len()
            .map_err(|_| refusal(nanus_domain::context::managed::ErrorCode::StorageCapacity))
    }

    /// Builds the started record of this step's attempt.
    fn started_attempt(
        &self,
        session: &Session,
        position: (u32, u32),
        prepared: (
            &dyn PreparedModelCall,
            u64,
            Vec<nanus_domain::context::managed::FragmentId>,
        ),
    ) -> RequestAttemptRecord {
        let (call, revision, management) = prepared;
        RequestAttemptRecord {
            attempt_id: format!("t{}s{}e{}", position.0, position.1, session.event_count()),
            retry_of: None,
            turn: u64::from(position.0),
            step: u64::from(position.1),
            selection: call.selection().clone(),
            projection_revision: revision,
            request_digest: call.request_digest().clone(),
            phase: AttemptPhase::Started,
            outcome: None,
            usage: None,
            assistant_seq: None,
            included_management_fragments: management,
            started_at_ms: self.clock.now_ms(),
            finished_at_ms: None,
            timings_ms: None,
        }
    }

    /// The finished record of an attempt refused before it was dispatched.
    fn refused_attempt(&self, mut attempt: RequestAttemptRecord) -> RequestAttemptRecord {
        attempt.phase = AttemptPhase::Finished;
        attempt.outcome = Some(nanus_domain::context::managed::AttemptOutcome::Refused);
        attempt.finished_at_ms = Some(self.clock.now_ms());
        attempt
    }

    /// Checkpoints the settled prefix, any automatic revision and the request intent, before
    /// anything is sent. Only an acknowledged commit installs them.
    async fn record_intent(
        &self,
        session: &mut Session,
        turn: &ManagedTurn<'_>,
        auto: Option<nanus_domain::context::managed::ProjectionRevision>,
        attempt: &RequestAttemptRecord,
        progress: &mut dyn Progress,
    ) -> Result<(), BundleError> {
        let mut candidate = session.clone();
        let decision = auto.map(|revision| {
            let decision = ContextDecision {
                decision_id: revision.decision_id.clone(),
                outcome: DecisionOutcome::Accepted,
                revision: Some(revision.revision),
                error_code: None,
            };
            candidate.append(SessionEvent::ContextRevision {
                payload: Box::new(revision),
            });
            candidate.append(SessionEvent::ContextDecision {
                payload: Box::new(decision.clone()),
            });
            decision
        });
        candidate.append(SessionEvent::RequestAttempt {
            payload: Box::new(attempt.clone()),
        });
        self.commit(turn, &candidate, CheckpointReason::RequestIntent, progress)
            .await?;
        *session = candidate;
        if let Some(decision) = &decision {
            progress.context_decision(decision);
        }
        Ok(())
    }

    /// Opens the step's records: admission of the effective request, then `StepStart`.
    fn begin_managed_records(
        &self,
        session: &mut Session,
        position: (u32, u32),
        turn_lease: Option<&dyn TurnRecordReservation>,
        effective: &nanus_ports::ChatRequest,
    ) -> Result<Option<Box<dyn StepRecordReservation>>, BundleError> {
        let (turn, step) = position;
        let Some(turn_lease) = turn_lease else {
            session.append(SessionEvent::StepStart { turn, step });
            return Ok(None);
        };
        let mut request = effective.clone();
        request.messages = vec![nanus_domain::Message::system(self.managed_system_prompt())];
        request.messages.extend(session.derive_messages());
        let llm = self.llm();
        let estimate = |request: &nanus_ports::ChatRequest| llm.estimate_request(request);
        let projection = StepRecordProjection {
            session_id: session.id(),
            events: session.log().events(),
            next_sequence: session.log().next_seq(),
            position,
            selection_epoch: self.selection.epoch()?,
            request: &request,
            fitted_request: effective,
            effort: self.effort(),
            capabilities: llm.capabilities(&request.model),
            estimate: &estimate,
        };
        let lease = turn_lease.reserve_step(&projection).map_err(|_| {
            BundleError::context(nanus_ports::record_admission::RECORD_ADMISSION_REFUSAL_TEXT)
        })?;
        session.append(SessionEvent::StepStart { turn, step });
        Ok(Some(lease))
    }

    /// Dispatches the frozen call and runs what it asked for.
    async fn perform_managed(
        &self,
        session: &mut Session,
        call: Box<dyn PreparedModelCall>,
        progress: &mut dyn Progress,
        context: dispatch::Phase<'_>,
        records: (Option<&dyn StepRecordReservation>, &mut Option<u64>),
    ) -> Result<StepOutcome, BundleError> {
        let (reservation, assistant) = records;
        if is_cancelled(progress, context.control) {
            return Ok(StepOutcome::Interrupted);
        }
        let mut stream = call.stream();
        let assembled = self
            .consume_stream(
                &mut stream,
                progress,
                context.control,
                nanus_ports::ToolArgumentLimits::managed(),
            )
            .await?;
        drop(stream);
        let interrupted = assembled.interrupted;
        let max_tokens = assembled.finish == FinishReason::Length;
        let seq = session.log().next_seq().value();
        let calls = self.append_model_records(session, context.position, assembled, reservation)?;
        *assistant = Some(seq);
        if interrupted {
            return Ok(StepOutcome::Interrupted);
        }
        if calls.is_empty() {
            return Ok(if max_tokens {
                StepOutcome::MaxTokens
            } else {
                StepOutcome::FinalAnswer
            });
        }
        if self.run_tools(session, &calls, progress, context).await? {
            Ok(StepOutcome::Interrupted)
        } else {
            Ok(StepOutcome::ToolCalls {
                count: u32::try_from(calls.len()).unwrap_or(u32::MAX),
            })
        }
    }
}
