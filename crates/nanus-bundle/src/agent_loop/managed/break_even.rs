//! Refusing a model's hide that cannot pay for the cache rewrite it forces.
//!
//! An accepted hide makes the provider re-read the next request uncached from the earliest
//! fragment it changed, and saves only the hidden tokens' cache-read price on each step after it.
//! The domain's rule ([`break_even`]) says when that pays back; this is where the runner measures a
//! proposal against it, with the selected adapter's own estimates, and refuses one that does not
//! before anything is staged.
//!
//! The refusal is an ordinary bounded tool failure, the form a malformed argument already takes:
//! no decision record, no new code, nothing recorded but the call's result. A proposal is the only
//! call in its message, so nothing between its validation here and its step's end can change the
//! numbers, and the re-validation after `StepEnd` is exactly what it was.
//!
//! The measurement costs three dry preparations of the next request — under the accepted selection,
//! under the candidate's hidden set, and of the candidate's unchanged prefix — and only for a
//! proposal that hides while no waiver applies. An inspect, a step, a restore and a note change
//! cost nothing. A measurement that cannot be made stages the proposal: the rule never stops a
//! session getting room, and the re-validation still checks what it always has.

use nanus_domain::Session;
use nanus_domain::context::managed::break_even::{self, HideCost, HideRule, HideWaiver};
use nanus_domain::context::managed::{ContextManageInput, FragmentId, Selection, limits, proposal};

use super::ManagedTurn;
use super::prepare::{Snapshot, probe_facts};
use crate::agent_loop::AgentRunner;

impl AgentRunner {
    /// Why any hide proposed in this step is accepted whatever its size, or `None` when the rule
    /// applies: the pressure the step's tail stated, then whether the provider's cache has lapsed.
    fn hide_waiver(&self, session: &Session, turn: &ManagedTurn<'_>) -> Option<HideWaiver> {
        let pressure = turn.step.borrow().pressure;
        break_even::waiver(pressure, self.cache_cold(session))
    }

    /// The rule an inspect result states, or `None` when this runner does not apply it.
    pub(in crate::agent_loop) fn hide_rule(
        &self,
        session: &Session,
        turn: &ManagedTurn<'_>,
    ) -> Option<HideRule> {
        self.hide_break_even
            .then(|| HideRule::with(self.hide_waiver(session, turn)))
    }

    /// Why a validated proposal is not staged, as the sentence the model is answered with, or
    /// `None` when it may be. `snapshot` is the one the proposal was validated against.
    ///
    /// Only a proposal that hides is judged, and only when no waiver applies; the candidate is
    /// measured with the accepted notes, so a note change in the same proposal neither helps nor
    /// hurts its hide.
    pub(in crate::agent_loop) fn unpaid_hide(
        &self,
        session: &Session,
        turn: &ManagedTurn<'_>,
        snapshot: &Snapshot,
        (input, staged): (&ContextManageInput, &proposal::Staged),
    ) -> Option<String> {
        if !self.hide_break_even
            || input.hide.is_empty()
            || self.hide_waiver(session, turn).is_some()
        {
            return None;
        }
        let cost = self.hide_cost(session, turn, snapshot, &staged.hidden)?;
        if cost.pays_for_itself() {
            return None;
        }
        let earliest = break_even::earliest_change(snapshot.state.hidden(), &staged.hidden)?;
        Some(refusal_text(cost, earliest))
    }

    /// Measures a candidate hidden set against the accepted one, on the next request this session
    /// would send, or `None` when any of the three preparations is refused.
    ///
    /// The three requests share everything but their messages: schemas, system prompt, notes and
    /// the probe's widest notice, so what differs between their estimates is the conversation.
    /// The unchanged prefix is the candidate's messages up to the first one that differs from the
    /// accepted selection's, which is the earliest fragment the proposal hides or restores.
    fn hide_cost(
        &self,
        session: &Session,
        turn: &ManagedTurn<'_>,
        snapshot: &Snapshot,
        hidden: &[FragmentId],
    ) -> Option<HideCost> {
        let policy = turn.policy();
        let caps = self.llm().capabilities(&self.model());
        let facts = probe_facts(
            self.input_allowance(caps, policy),
            self.working_target(caps, policy),
        );
        let base = self.base_request(session, policy);
        let compose = |hidden: &[FragmentId]| {
            let selection = Selection {
                revision: snapshot.state.revision(),
                hidden,
                notes: snapshot.state.notes(),
                notes_goal_revision: snapshot.state.notes_goal_revision(),
            };
            self.compose(session, snapshot, selection, &facts, &base)
                .ok()
        };
        let current = compose(snapshot.state.hidden())?;
        let candidate = compose(hidden)?;
        let unchanged = current
            .messages
            .iter()
            .zip(&candidate.messages)
            .take_while(|(accepted, proposed)| accepted == proposed)
            .count();
        let mut prefix = candidate.clone();
        prefix.messages.truncate(unchanged);
        // Postcondition of the truncation: the prefix begins the candidate, and is no longer.
        assert!(prefix.messages.len() <= candidate.messages.len());
        let tokens = |request| {
            self.prepare_dry(request)
                .ok()
                .map(|call| u64::from(call.estimate().input_tokens))
        };
        Some(HideCost::measured(
            tokens(current)?,
            tokens(candidate)?,
            tokens(prefix)?,
        ))
    }
}

/// The failure a hide that does not pay for itself is answered with: the numbers, the rule, and
/// what would be accepted. Numbers and an opaque id only, so it is bounded.
fn refusal_text(cost: HideCost, earliest: FragmentId) -> String {
    format!(
        "not staged: this hide saves less than the cache rewrite it forces. The next request \
         would be about {removed} estimated tokens smaller, and the provider would re-read about \
         {reread} estimated tokens uncached, from {earliest} to the end. A hide must remove at \
         least 1/{ratio} of what it forces to be re-read ({needed} here) to pay that back within \
         {horizon} steps. Nothing was staged and the context is unchanged. Hide more of the \
         resolved stretch in one proposal, or later work; any hide is accepted once \
         pressure_percent reaches {pressure} or the provider's cache has lapsed, and a proposal \
         that hides nothing is never refused for this.",
        removed = cost.removed_tokens,
        reread = cost.reread_tokens,
        ratio = break_even::REREAD_PER_HIDDEN_TOKEN_MAX,
        needed = cost.removed_needed(),
        horizon = break_even::BREAK_EVEN_HORIZON_STEPS,
        pressure = limits::SOFT_PRESSURE_PERCENT,
    )
}
