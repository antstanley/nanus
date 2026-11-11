//! Re-deriving a recorded request from the log, offline, to check it against what was recorded.
//!
//! Preparation is deterministic: a managed request is compiled from the log as it stood, the
//! selection the log accepted (or the automatic revision recorded with the request's intent), and
//! the runner's configuration, and the adapter encodes it without I/O. So a request can be
//! prepared again from a recorded session and its digest compared with the one the intent
//! recorded. A match proves the re-derived request is the one whose body was sent, and the
//! adapter's canonical encoding of that request is then the prefix the provider was offered.
//!
//! Nothing here is ever dispatched, recorded or installed. The tail's facts are re-derived the way
//! the step derived them: the estimate is the probe's, the allowance and target are the runner's,
//! and the budget hint, which is process-held reminder state no record carries, is tried both
//! ways only where the pressure could have given it.

use nanus_domain::Session;
use nanus_domain::context::managed::{
    Digest, ErrorCode, NoticeFacts, ProjectionRevision, Selection, SelectionIdentity, compile,
    limits,
};
use nanus_ports::CanonicalEncoding;

use super::prepare::probe_facts;
use super::{recorded_policy, refusal};
use crate::BundleError;
use crate::agent_loop::AgentRunner;

/// What a recorded request is checked against: the route and the digest its intent recorded.
#[derive(Clone, Copy, Debug)]
pub struct RecordedRequest<'a> {
    /// The selection identity the intent recorded.
    pub selection: &'a SelectionIdentity,
    /// The digest of the body the intent recorded.
    pub digest: &'a Digest,
}

/// A recorded managed request this runner prepared again to exactly its recorded digest.
#[derive(Clone, Debug)]
pub struct Rederived {
    /// The digest, equal to the recorded one.
    pub digest: Digest,
    /// The pressure its tail stated, as a percentage of the working target.
    pub pressure_percent: u32,
    /// Whether its tail carried the budget hint.
    pub budget_hint: bool,
    /// The adapter's canonical encoding of the request, when the adapter states one.
    pub canonical: Option<CanonicalEncoding>,
}

/// How preparing a recorded managed request again came out.
#[derive(Clone, Debug)]
pub enum Rederivation {
    /// The body's digest equals the recorded one.
    Matched(Rederived),
    /// The adapter would send this request on a route other than the recorded one: another
    /// provider, endpoint, wire, model or effort.
    RouteDiffers(SelectionIdentity),
    /// The route is the recorded one and the body's digest is not.
    DigestDiffers,
}

/// A legacy request this runner built again from the log.
///
/// Nothing checks it: a legacy step records no digest, so this is the request this runner's
/// configuration would build, not a proof of the one that was sent.
#[derive(Clone, Debug)]
pub struct RebuiltLegacy {
    /// The adapter's canonical encoding of the request, when the adapter states one.
    pub canonical: Option<CanonicalEncoding>,
    /// How many whole turns the fitter left out, zero when it left out none.
    pub dropped_turns: u32,
}

/// Whether two identities name the same route. The epoch is the recording runner's counter of
/// selection changes, not a fact about the request, so it is not compared.
fn same_route(left: &SelectionIdentity, right: &SelectionIdentity) -> bool {
    left.provider == right.provider
        && left.endpoint_digest == right.endpoint_digest
        && left.protocol == right.protocol
        && left.model == right.model
        && left.effort == right.effort
}

impl AgentRunner {
    /// Prepares again the managed request a step of a recorded session sent, and checks it.
    ///
    /// `before` is the session as it stood when the step was prepared: every event before the
    /// automatic revision and the intent its checkpoint recorded. `automatic` is that revision,
    /// when the intent was recorded with one, and the request is built under its hidden set; it is
    /// otherwise built under the selection `before` accepted. The runner must be configured as the
    /// recording one was — the same system prompt, schemas, budget, working target, retention and
    /// effort — or the digest will not match, which is the point: a match is evidence of exactly
    /// that.
    ///
    /// The tail's budget hint is not recorded. It is tried off, and on only when the stated
    /// pressure reaches the hint's threshold, where it could have been given.
    ///
    /// # Errors
    ///
    /// Returns the managed refusal this runner would have made: a model path it cannot carry
    /// managed, a log whose accepted state does not validate, or a candidate over the allowance.
    pub fn rederive_managed_request(
        &self,
        before: &Session,
        automatic: Option<&ProjectionRevision>,
        recorded: RecordedRequest<'_>,
    ) -> Result<Rederivation, BundleError> {
        let policy = recorded_policy(before);
        if let Some(code) = self.unready(policy) {
            return Err(refusal(code));
        }
        let snapshot = Self::snapshot(before)?;
        let caps = self.llm().capabilities(&self.model());
        let (allowance, target) = (
            self.input_allowance(caps, policy),
            self.working_target(caps, policy),
        );
        let base = self.base_request(before, policy);
        let notes = snapshot.state.notes().to_vec();
        let (hidden, revision) = automatic.map_or_else(
            || (snapshot.state.hidden().to_vec(), snapshot.state.revision()),
            |fitted| (fitted.hidden.clone(), fitted.revision),
        );
        let selection = Selection {
            revision,
            hidden: &hidden,
            notes: &notes,
            notes_goal_revision: snapshot.state.notes_goal_revision(),
        };
        let compose = |facts: &NoticeFacts| {
            self.compose(before, &snapshot, selection, facts, &base)
                .map_err(refusal)
        };
        // The probe the step measured its notice with, which is also where the route shows.
        let probe = self
            .prepare_call(compose(&probe_facts(allowance, target))?)
            .map_err(refusal)?;
        if !same_route(probe.selection(), recorded.selection) {
            return Ok(Rederivation::RouteDiffers(probe.selection().clone()));
        }
        let pre = probe.estimate().input_tokens;
        if pre > allowance {
            return Err(refusal(ErrorCode::CandidateTooLarge));
        }
        let pressure = compile::pressure_percent(pre, target);
        let hints: &[bool] = if pressure >= limits::SOFT_PRESSURE_PERCENT {
            &[false, true]
        } else {
            &[false]
        };
        for &budget_hint in hints {
            let facts = NoticeFacts {
                estimate_input_tokens: Some(pre),
                estimator: String::from(nanus_ports::capabilities::MANAGED_ESTIMATOR),
                input_allowance: allowance,
                working_target: target,
                budget_hint,
                recovery_available: true,
            };
            let request = compose(&facts)?;
            let call = self.prepare_call(request.clone()).map_err(refusal)?;
            if call.request_digest() == recorded.digest {
                return Ok(Rederivation::Matched(Rederived {
                    digest: call.request_digest().clone(),
                    pressure_percent: pressure,
                    budget_hint,
                    canonical: self.llm().canonical_encoding(&request),
                }));
            }
        }
        Ok(Rederivation::DigestDiffers)
    }

    /// Builds again the legacy request a step of a recorded session would have sent.
    ///
    /// `before` is the session as it stood when the step began. The request is the whole-turn
    /// fitter's under this runner's configuration, exactly as a legacy step builds it, so it is
    /// the request that was sent only if the configuration is the one it was sent under, which
    /// nothing recorded can confirm.
    ///
    /// # Errors
    ///
    /// Returns the fitter's refusal when the conversation cannot be fitted.
    pub fn rebuild_legacy_request(&self, before: &Session) -> Result<RebuiltLegacy, BundleError> {
        let (request, elision) = self.build_request(before)?;
        Ok(RebuiltLegacy {
            canonical: self.llm().canonical_encoding(&request),
            dropped_turns: elision.map_or(0, |elision| elision.dropped_turns),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nanus_domain::context::managed::Digest;

    fn identity(model: &str, epoch: u64) -> SelectionIdentity {
        SelectionIdentity {
            provider: "p".into(),
            endpoint_digest: Digest::of(b"e"),
            protocol: "w".into(),
            model: model.into(),
            effort: Some("high".into()),
            epoch,
        }
    }

    #[test]
    fn a_route_is_the_same_whatever_the_recording_runners_epoch() {
        assert!(same_route(&identity("m", 1), &identity("m", 9)));
    }

    #[test]
    fn a_route_on_another_model_or_effort_is_another_route() {
        assert!(!same_route(&identity("m", 1), &identity("n", 1)));
        let mut effortless = identity("m", 1);
        effortless.effort = None;
        assert!(!same_route(&identity("m", 1), &effortless));
    }
}
