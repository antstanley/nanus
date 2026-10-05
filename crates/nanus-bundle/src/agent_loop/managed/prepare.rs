//! Preparing a managed request: compile, fit, freeze, and describe what was prepared.
//!
//! Every candidate the fitter probes is compiled by the domain and prepared by the selected
//! adapter, so its cost is the cost of the body that adapter would send — schemas, notice,
//! generated data, typed content and the output reservation included. The candidate that is
//! chosen is prepared once more with its final notice, and that object is what is recorded and
//! dispatched; nothing is re-encoded in between.

use std::collections::BTreeSet;

use nanus_domain::context::managed::{
    ContextFrontier, ContextMode, ContextPolicy, ContextStatus, Digest, ErrorCode, FragmentId,
    Fragments, MANUAL_POLICY_V1, ManagedState, NoticeFacts, ProjectionRevision, Selection,
    SnapshotProfile, derive_effective_context, fit, fragments, limits, proposal, state,
};
use nanus_domain::{Goal, Message, Session};
use nanus_ports::{ChatRequest, ManagedRequest, ModelCapabilities, PreparedModelCall, TurnRuntime};

use super::ManagedTurn;
use crate::BundleError;
use crate::agent_loop::AgentRunner;

/// A frozen managed request and what it was prepared under.
pub(in crate::agent_loop) struct Prepared {
    /// The adapter-owned call that will be dispatched.
    pub(in crate::agent_loop) call: Box<dyn PreparedModelCall>,
    /// The effective request it was prepared from.
    pub(in crate::agent_loop) request: ChatRequest,
    /// The automatic revision this request installs, when fitting acted.
    pub(in crate::agent_loop) auto: Option<ProjectionRevision>,
    /// The status it was prepared under.
    pub(in crate::agent_loop) status: ContextStatus,
    /// The management fragment it carries that a response will consume.
    pub(in crate::agent_loop) management: Vec<FragmentId>,
    /// The snapshot profile's digest.
    pub(in crate::agent_loop) profile: Digest,
    /// The revision the request was built from.
    pub(in crate::agent_loop) revision: u64,
}

/// Everything a compile needs, folded once per preparation.
pub(in crate::agent_loop) struct Snapshot {
    pub(in crate::agent_loop) state: ManagedState,
    pub(in crate::agent_loop) fragments: Fragments,
    pub(in crate::agent_loop) protected: BTreeSet<FragmentId>,
    pub(in crate::agent_loop) goal: Option<Goal>,
}

/// Maps a managed refusal into the bundle error.
pub(in crate::agent_loop) fn refusal(code: ErrorCode) -> BundleError {
    let message = match code {
        ErrorCode::ProtectedFloorTooLarge => {
            "every user message and the protected recent work do not fit the context budget on \
             their own; start a new or narrower session"
        }
        ErrorCode::CandidateTooLarge => "the request does not fit the context budget",
        ErrorCode::ProtocolIncompatible | ErrorCode::UnsupportedMode => {
            "the selected provider and model cannot carry a managed context; choose a supported \
             model or reset the session's context"
        }
        ErrorCode::SourceCorrupt | ErrorCode::StaleBase | ErrorCode::InvalidFragment => {
            "the session's accepted context does not validate; reset its context to continue"
        }
        ErrorCode::StorageCapacity => "the session has no room left for another step's records",
        _ => "managed context refused the request",
    };
    BundleError::managed(code, message)
}

impl AgentRunner {
    /// The system prompt a managed request carries: the runner's, then the manual policy.
    pub(in crate::agent_loop) fn managed_system_prompt(&self) -> String {
        format!("{}\n\n{MANUAL_POLICY_V1}", self.system_prompt)
    }

    /// Every schema a managed request offers: registered, goal, then context tools.
    pub(in crate::agent_loop) fn managed_schemas(&self) -> Vec<nanus_domain::ToolSchema> {
        let mut schemas = self.offered_schemas();
        schemas.extend(crate::context_tools::schemas());
        schemas
    }

    /// Folds the session into the parts a compile needs, validating the accepted state.
    pub(in crate::agent_loop) fn snapshot(session: &Session) -> Result<Snapshot, BundleError> {
        let state = ManagedState::fold(session.log()).map_err(refusal)?;
        let fragments = fragments::derive(session.log()).map_err(refusal)?;
        let protected = state::protected(session.log(), &fragments);
        state::check_accepted(session, &state, &fragments, &protected).map_err(refusal)?;
        Ok(Snapshot {
            state,
            fragments,
            protected,
            goal: session.goal(),
        })
    }

    /// Refuses a managed turn this runner and model cannot carry, before anything is recorded.
    pub(in crate::agent_loop) fn check_managed_ready(
        &self,
        session: &Session,
        turn: &ManagedTurn<'_>,
    ) -> Result<(), BundleError> {
        if let Some(code) = self.unready(turn.policy()) {
            return Err(refusal(code));
        }
        Self::snapshot(session).map(|_| ())
    }

    /// Why managed requests cannot be prepared now, or `None` when they can.
    pub(in crate::agent_loop) fn unready(&self, policy: ContextPolicy) -> Option<ErrorCode> {
        if policy.validate().is_err() || policy.mode != ContextMode::Managed {
            return Some(ErrorCode::UnsupportedMode);
        }
        let collides = self
            .tools
            .borrow()
            .names()
            .iter()
            .any(|name| crate::context_tools::is_context_tool(name));
        if collides {
            return Some(ErrorCode::UnsupportedMode);
        }
        let model = self.model();
        let llm = self.llm();
        if !llm.managed_support(&model).supports(limits::POLICY_VERSION) {
            return Some(ErrorCode::ProtocolIncompatible);
        }
        let caps = llm.capabilities(&model);
        if caps
            .max_output_tokens
            .is_some_and(|ceiling| ceiling < policy.output_reserve_tokens)
        {
            return Some(ErrorCode::UnsupportedMode);
        }
        None
    }

    /// The admissible input allowance: the budget and window less both reservations.
    pub(in crate::agent_loop) fn input_allowance(
        &self,
        caps: ModelCapabilities,
        policy: ContextPolicy,
    ) -> u32 {
        let separate = self
            .request_reservation
            .map_or(0, |(_, reasoning)| reasoning);
        let reservation = policy.output_reserve_tokens.saturating_add(separate);
        let window = caps
            .context_window_tokens
            .unwrap_or(u32::MAX)
            .min(self.config.context_budget);
        window
            .saturating_sub(reservation)
            .min(caps.max_input_tokens.unwrap_or(u32::MAX))
    }

    /// The request every candidate shares: model, schemas, effort and reservations.
    fn base_request(&self, session: &Session, policy: ContextPolicy) -> ChatRequest {
        let mut request = ChatRequest::new(self.model(), Vec::new());
        request.tools = self.managed_schemas();
        request.reasoning_effort = self.effort.get();
        request.max_tokens = Some(policy.output_reserve_tokens);
        request.separate_reasoning_tokens = self
            .request_reservation
            .map_or(0, |(_, reasoning)| reasoning);
        request.context_budget = Some(self.config.context_budget);
        let mut source = vec![Message::system(self.managed_system_prompt())];
        source.extend(session.derive_messages());
        request.source_history = Some(source.into());
        request
    }

    /// Compiles one candidate into a request.
    fn compose(
        &self,
        session: &Session,
        snapshot: &Snapshot,
        selection: Selection<'_>,
        facts: &NoticeFacts,
        base: &ChatRequest,
    ) -> Result<ChatRequest, ErrorCode> {
        let effective = derive_effective_context(
            session,
            &snapshot.fragments,
            &snapshot.protected,
            selection,
            snapshot.goal.as_ref(),
            facts,
        )?;
        let mut request = base.clone();
        request.messages = Vec::with_capacity(effective.messages.len().saturating_add(2));
        request
            .messages
            .push(Message::system(self.managed_system_prompt()));
        request.messages.push(effective.notice);
        request.messages.extend(effective.messages);
        Ok(request)
    }

    /// Compiles a candidate over a fresh base request, for a dry preparation.
    pub(in crate::agent_loop) fn compose_for(
        &self,
        session: &Session,
        snapshot: &Snapshot,
        selection: Selection<'_>,
        facts: &NoticeFacts,
        policy: ContextPolicy,
    ) -> Result<ChatRequest, ErrorCode> {
        let base = self.base_request(session, policy);
        self.compose(session, snapshot, selection, facts, &base)
    }

    /// Prepares a candidate that will never be sent: its estimate is all that is wanted.
    pub(in crate::agent_loop) fn prepare_dry(
        &self,
        request: ChatRequest,
    ) -> Result<Box<dyn PreparedModelCall>, ErrorCode> {
        self.prepare_call(request)
    }

    /// Prepares a candidate with the selected adapter, refusing what it cannot carry.
    fn prepare_call(&self, request: ChatRequest) -> Result<Box<dyn PreparedModelCall>, ErrorCode> {
        let epoch = self
            .selection
            .epoch()
            .map_err(|_| ErrorCode::UnsupportedMode)?;
        self.llm()
            .prepare_managed(ManagedRequest {
                request,
                selection_epoch: epoch,
            })
            .map_err(|_| ErrorCode::ProtocolIncompatible)
    }

    /// Prepares this step's request: fit, freeze, describe.
    pub(in crate::agent_loop) fn prepare_step(
        &self,
        session: &Session,
        turn: &ManagedTurn<'_>,
    ) -> Result<Prepared, BundleError> {
        let policy = turn.policy();
        if let Some(code) = self.unready(policy) {
            return Err(refusal(code));
        }
        let snapshot = Self::snapshot(session)?;
        let caps = self.llm().capabilities(&self.model());
        let allowance = self.input_allowance(caps, policy);
        let base = self.base_request(session, policy);
        let notes = snapshot.state.notes().to_vec();
        let bound = snapshot.state.notes_goal_revision();
        let probe = NoticeFacts {
            input_allowance: allowance,
            recovery_available: true,
            ..NoticeFacts::default()
        };
        let cost = |hidden: &[FragmentId], revision: u64| -> Result<u32, ErrorCode> {
            let selection = Selection {
                revision,
                hidden,
                notes: &notes,
                notes_goal_revision: bound,
            };
            let request = self.compose(session, &snapshot, selection, &probe, &base)?;
            Ok(self.prepare_call(request)?.estimate().input_tokens)
        };
        let next = snapshot
            .state
            .max_revision
            .max(snapshot.state.revision())
            .saturating_add(1);
        let fitted = fit::hard_fit(
            &snapshot.fragments,
            &snapshot.protected,
            snapshot.state.hidden(),
            allowance,
            |hidden| cost(hidden, next),
        )
        .map_err(refusal)?;
        let (hidden, revision) = match &fitted {
            Some(fitted) => (fitted.hidden.clone(), next),
            None => (snapshot.state.hidden().to_vec(), snapshot.state.revision()),
        };
        let selection = Selection {
            revision,
            hidden: &hidden,
            notes: &notes,
            notes_goal_revision: bound,
        };
        self.freeze(
            session,
            turn,
            &snapshot,
            (selection, fitted.is_some()),
            (&base, allowance),
        )
    }

    /// Prepares the chosen candidate with its final notice and builds what it is recorded as.
    fn freeze(
        &self,
        session: &Session,
        turn: &ManagedTurn<'_>,
        snapshot: &Snapshot,
        chosen: (Selection<'_>, bool),
        shared: (&ChatRequest, u32),
    ) -> Result<Prepared, BundleError> {
        let (selection, automatic) = chosen;
        let (base, allowance) = shared;
        let probe = NoticeFacts {
            input_allowance: allowance,
            recovery_available: true,
            ..NoticeFacts::default()
        };
        let draft = self
            .compose(session, snapshot, selection, &probe, base)
            .map_err(refusal)?;
        let pre = self.prepare_call(draft).map_err(refusal)?.estimate();
        let mut reminder = turn.reminder();
        let percent =
            nanus_domain::context::managed::compile::pressure_percent(pre.input_tokens, allowance);
        let facts = NoticeFacts {
            estimate_input_tokens: Some(pre.input_tokens),
            estimator: String::from("adapter-serialized-body"),
            input_allowance: allowance,
            budget_hint: reminder.observe(percent),
            recovery_available: true,
        };
        turn.set_reminder(reminder);
        let request = self
            .compose(session, snapshot, selection, &facts, base)
            .map_err(refusal)?;
        let caps = self.llm().capabilities(&request.model);
        nanus_ports::capabilities::validate_image_input(caps, &request)
            .map_err(|error| BundleError::Model(error.to_string()))?;
        let call = self.prepare_call(request.clone()).map_err(refusal)?;
        let estimate = call.estimate();
        if !estimate.fits(caps, &request) || estimate.input_tokens > allowance {
            return Err(refusal(ErrorCode::CandidateTooLarge));
        }
        let profile = self.profile(call.selection().clone(), turn.policy(), snapshot);
        let frontier = state::frontier(session, snapshot.state.revision()).map_err(refusal)?;
        let auto = if automatic {
            Some(
                proposal::automatic_revision(
                    &snapshot.state,
                    frontier.clone(),
                    selection.hidden.to_vec(),
                    profile.clone(),
                    format!("a-{}", frontier.event_count),
                )
                .map_err(refusal)?,
            )
        } else {
            None
        };
        let status = Self::status_of(
            (session, snapshot, frontier),
            (selection, estimate.input_tokens, &facts),
            (
                turn.context
                    .is_some_and(|context| context.archive().is_some()),
                &profile,
            ),
        );
        Ok(Prepared {
            management: state::management_in_flight(&snapshot.fragments, &snapshot.protected),
            call,
            request,
            auto,
            status,
            profile,
            revision: selection.revision,
        })
    }

    /// The snapshot profile's digest for a selection identity.
    pub(in crate::agent_loop) fn profile(
        &self,
        selection: nanus_domain::context::managed::SelectionIdentity,
        policy: ContextPolicy,
        snapshot: &Snapshot,
    ) -> Digest {
        let schemas = serde_json::to_string(&self.managed_schemas()).unwrap_or_default();
        SnapshotProfile {
            selection,
            system_prompt_digest: Digest::of(self.managed_system_prompt().as_bytes()),
            tool_schema_digest: Digest::of(schemas.as_bytes()),
            policy,
            goal_revision: snapshot.goal.as_ref().map(Goal::revision),
        }
        .digest()
    }

    /// Builds the status a prepared request is described by.
    fn status_of(
        at: (&Session, &Snapshot, ContextFrontier),
        prepared: (Selection<'_>, u32, &NoticeFacts),
        extra: (bool, &Digest),
    ) -> ContextStatus {
        let (session, snapshot, frontier) = at;
        let (selection, estimate, facts) = prepared;
        let (archive, profile) = extra;
        let anchor_follows = session.derive_messages().len() > 1;
        ContextStatus {
            mode: ContextMode::Managed,
            revision: selection.revision,
            frontier,
            estimate_input_tokens: Some(u64::from(estimate)),
            estimate_protected_tokens: None,
            output_reserve_tokens: u64::from(
                snapshot.state.policy_or_default().output_reserve_tokens,
            ),
            estimator: facts.estimator.clone(),
            hidden_fragments: u64::try_from(selection.hidden.len()).unwrap_or(u64::MAX),
            protected_fragments: u64::try_from(snapshot.protected.len()).unwrap_or(u64::MAX),
            goal_revision: snapshot.goal.as_ref().map(Goal::revision),
            goal_data_available: snapshot.goal.is_some() && anchor_follows,
            recall_available: true,
            archive_available: archive,
            last_decision: snapshot.state.last_decision.clone(),
            profile_digest: profile.clone(),
            managed_ready: true,
            unavailable_reason: None,
        }
    }

    /// The effective request of `session` under its accepted selection, for admission and
    /// commit: the same compile the next step would start from, with no automatic fit.
    pub(in crate::agent_loop) fn effective_request(
        &self,
        session: &Session,
        turn: &ManagedTurn<'_>,
    ) -> Result<ChatRequest, BundleError> {
        let snapshot = Self::snapshot(session)?;
        let caps = self.llm().capabilities(&self.model());
        let facts = NoticeFacts {
            input_allowance: self.input_allowance(caps, turn.policy()),
            recovery_available: true,
            ..NoticeFacts::default()
        };
        let notes = snapshot.state.notes().to_vec();
        let hidden = snapshot.state.hidden().to_vec();
        let selection = Selection {
            revision: snapshot.state.revision(),
            hidden: &hidden,
            notes: &notes,
            notes_goal_revision: snapshot.state.notes_goal_revision(),
        };
        let base = self.base_request(session, turn.policy());
        self.compose(session, &snapshot, selection, &facts, &base)
            .map_err(refusal)
    }

    /// The status of an idle session, from a dry preparation of its accepted selection.
    pub(in crate::agent_loop) fn idle_status(
        &self,
        session: &Session,
        runtime: TurnRuntime<'_>,
    ) -> Result<ContextStatus, BundleError> {
        let policy = super::recorded_policy(session);
        let revision = ManagedState::fold(session.log()).map_or(0, |state| state.revision());
        let frontier = state::frontier(session, revision).map_err(refusal)?;
        let archive = runtime
            .context
            .is_some_and(|context| context.archive().is_some());
        let mut status = ContextStatus {
            mode: policy.mode,
            revision,
            frontier,
            estimate_input_tokens: None,
            estimate_protected_tokens: None,
            output_reserve_tokens: u64::from(policy.output_reserve_tokens),
            estimator: String::from("adapter-serialized-body"),
            hidden_fragments: 0,
            protected_fragments: 0,
            goal_revision: session.goal().as_ref().map(Goal::revision),
            goal_data_available: false,
            recall_available: policy.mode == ContextMode::Managed,
            archive_available: archive,
            last_decision: None,
            profile_digest: Digest::empty(),
            managed_ready: false,
            unavailable_reason: None,
        };
        let snapshot = match Self::snapshot(session) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                status.unavailable_reason = error.managed_code();
                return Ok(status);
            }
        };
        status.hidden_fragments = u64::try_from(snapshot.state.hidden().len()).unwrap_or(0);
        status.protected_fragments = u64::try_from(snapshot.protected.len()).unwrap_or(0);
        status
            .last_decision
            .clone_from(&snapshot.state.last_decision);
        status.unavailable_reason = self.unready(policy);
        if status.unavailable_reason.is_none() {
            self.dry_estimate(session, &snapshot, policy, &mut status);
        }
        Ok(status)
    }

    /// Fills the estimate and profile of an idle status from a dry preparation.
    fn dry_estimate(
        &self,
        session: &Session,
        snapshot: &Snapshot,
        policy: ContextPolicy,
        status: &mut ContextStatus,
    ) {
        let caps = self.llm().capabilities(&self.model());
        let facts = NoticeFacts {
            input_allowance: self.input_allowance(caps, policy),
            recovery_available: true,
            ..NoticeFacts::default()
        };
        let notes = snapshot.state.notes().to_vec();
        let hidden = snapshot.state.hidden().to_vec();
        let selection = Selection {
            revision: snapshot.state.revision(),
            hidden: &hidden,
            notes: &notes,
            notes_goal_revision: snapshot.state.notes_goal_revision(),
        };
        let base = self.base_request(session, policy);
        let prepared = self
            .compose(session, snapshot, selection, &facts, &base)
            .and_then(|request| self.prepare_call(request));
        match prepared {
            Ok(call) => {
                status.estimate_input_tokens = Some(u64::from(call.estimate().input_tokens));
                status.profile_digest = self.profile(call.selection().clone(), policy, snapshot);
                status.managed_ready = true;
            }
            Err(code) => status.unavailable_reason = Some(code),
        }
    }
}
