//! Finding the configuration a recorded request was prepared under, and proving it by digest.
//!
//! A route's runners differ in what the recording composition fixed when it started — the plan,
//! and the system prompt's runtime section — and a request also depends on what a turn may switch
//! and nothing records whole: the effort the runner asked for and the retention. Each candidate is
//! tried until one prepares the recorded digest. A digest is BLAKE3 of the whole body, so a match
//! is not a guess, and the order only decides how soon one is found.
//!
//! The combination that matched is tried first for the route's next request, which is the same
//! one for every request of a session that was never switched. A route whose requests keep failing
//! is not searched exhaustively for ever: after [`FULL_SEARCHES`] failed searches its requests are
//! tried only under the likeliest combination, so an audit of a long session recorded by another
//! build costs a few preparations per request, not hundreds.

use std::collections::BTreeMap;

use nanus_domain::Session;
use nanus_domain::context::managed::{ProjectionRevision, RequestAttemptRecord, SelectionIdentity};
use nanus_ports::{PromptCacheRetention, ReasoningEffort};

use super::AuditRoutes;
use crate::agent_loop::UNKNOWN_CACHE_LIFETIME_MS;
use crate::{AgentRunner, RebuiltLegacy, RecordedRequest, Rederivation, Rederived};

/// How many exhaustive searches a route's requests get before only the likeliest combination is
/// tried.
const FULL_SEARCHES: u32 = 2;

/// Every step of the effort scale, for a recorded effort no step is spelled as.
const EFFORTS: [ReasoningEffort; 7] = [
    ReasoningEffort::None,
    ReasoningEffort::Minimal,
    ReasoningEffort::Low,
    ReasoningEffort::Medium,
    ReasoningEffort::High,
    ReasoningEffort::XHigh,
    ReasoningEffort::Max,
];

/// The retentions a runner can ask for: the stock run's, the link's, and none.
const RETENTIONS: [PromptCacheRetention; 3] = [
    PromptCacheRetention::Short,
    PromptCacheRetention::Long,
    PromptCacheRetention::None,
];

/// One way of preparing a request: which runner, which effort, which retention.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Combo {
    runner: usize,
    effort: Option<ReasoningEffort>,
    retention: PromptCacheRetention,
}

/// What the audit knows about one recorded route.
struct Route {
    runners: Result<Vec<AgentRunner>, String>,
    /// The combination that last prepared a request of this route to its digest.
    matched: Option<Combo>,
    /// The first combination that reached the recorded route, though not its digest.
    near: Option<Combo>,
    failed_searches: u32,
}

/// What preparing a managed request again came to.
pub(super) enum Managed {
    /// Prepared to exactly the recorded digest.
    Verified {
        /// What the matching preparation produced.
        rederived: Rederived,
        /// The retention it asked for.
        retention: PromptCacheRetention,
        /// The cache lifetime its adapter states for that retention, or the unknown one.
        lifetime_ms: u64,
    },
    /// Not proven, and why.
    Unverified(String),
}

/// What every failed combination for one request said, for the reason it is unverified.
#[derive(Default)]
struct Failures {
    /// The parts of the recorded route that every candidate differed in, when one did.
    route: Option<Vec<&'static str>>,
    digest: bool,
    error: Option<String>,
}

impl Failures {
    /// Notes the parts a candidate's route differed from the recorded one in, keeping only those
    /// every candidate so far differed in: the part no candidate could reach.
    fn differed(&mut self, recorded: &SelectionIdentity, formed: &SelectionIdentity) {
        let parts: Vec<&'static str> = [
            ("provider", recorded.provider != formed.provider),
            (
                "endpoint",
                recorded.endpoint_digest != formed.endpoint_digest,
            ),
            ("wire", recorded.protocol != formed.protocol),
            ("model", recorded.model != formed.model),
            ("effort", recorded.effort != formed.effort),
        ]
        .into_iter()
        .filter_map(|(part, differs)| differs.then_some(part))
        .collect();
        self.route = Some(match self.route.take() {
            Some(earlier) => earlier
                .into_iter()
                .filter(|part| parts.contains(part))
                .collect(),
            None => parts,
        });
    }

    fn reason(self, searched: bool) -> String {
        let mut reason = if self.digest {
            String::from(
                "prepared again on the recorded route, but no configuration this audit can form \
                 reproduces the recorded digest: it may have been recorded by another build, under \
                 another configuration, or with tools this audit does not offer",
            )
        } else if let Some(parts) = self.route {
            format!(
                "no route this build can form matches the recorded one: every candidate differed \
                 in its {}",
                parts.join(", ")
            )
        } else {
            self.error.map_or_else(
                || String::from("no runner could prepare it"),
                |error| format!("it could not be prepared again: {error}"),
            )
        };
        if !searched {
            reason.push_str(
                "; earlier requests on this route did not verify either, so only the likeliest \
                 configuration was tried",
            );
        }
        reason
    }
}

/// Finds and remembers the configuration each recorded route's requests were prepared under.
pub(super) struct Verifier<'r> {
    routes: &'r dyn AuditRoutes,
    managed: BTreeMap<String, Route>,
    legacy: BTreeMap<String, Result<Vec<AgentRunner>, String>>,
}

impl<'r> Verifier<'r> {
    pub(super) fn new(routes: &'r dyn AuditRoutes) -> Self {
        Self {
            routes,
            managed: BTreeMap::new(),
            legacy: BTreeMap::new(),
        }
    }

    /// Prepares the managed request `started` recorded again, against `before`.
    pub(super) fn managed(
        &mut self,
        before: &Session,
        started: &RequestAttemptRecord,
        automatic: Option<&ProjectionRevision>,
    ) -> Managed {
        let selection = &started.selection;
        // A runner refuses to switch to an empty model, and a request never named one.
        if selection.model.trim().is_empty() {
            return Managed::Unverified(String::from("its intent names no model"));
        }
        let routes = self.routes;
        let route = self
            .managed
            .entry(route_key(selection))
            .or_insert_with(|| Route {
                runners: routes.managed(selection),
                matched: None,
                near: None,
                failed_searches: 0,
            });
        let runners = match &route.runners {
            Ok(runners) if !runners.is_empty() => runners,
            Ok(_) => return Managed::Unverified(String::from("no runner serves this route")),
            Err(reason) => return Managed::Unverified(reason.clone()),
        };
        let searched = route.failed_searches < FULL_SEARCHES;
        let first = route.matched.or(route.near);
        let combos = combos(runners.len(), selection, first, searched);
        let recorded = RecordedRequest {
            selection,
            digest: &started.request_digest,
        };
        let mut failures = Failures::default();
        for combo in combos {
            let Some(runner) = runners.get(combo.runner) else {
                continue;
            };
            runner.set_model(&selection.model);
            runner.set_effort(combo.effort);
            runner.set_prompt_cache(combo.retention);
            match runner.rederive_managed_request(before, automatic, recorded) {
                Ok(Rederivation::Matched(rederived)) => {
                    route.matched = Some(combo);
                    let lifetime_ms = runner
                        .llm()
                        .prompt_cache_lifetime_ms(&selection.model, combo.retention)
                        .unwrap_or(UNKNOWN_CACHE_LIFETIME_MS);
                    return Managed::Verified {
                        rederived,
                        retention: combo.retention,
                        lifetime_ms,
                    };
                }
                Ok(Rederivation::RouteDiffers(formed)) => failures.differed(selection, &formed),
                Ok(Rederivation::DigestDiffers) => {
                    failures.digest = true;
                    route.near.get_or_insert(combo);
                }
                Err(error) => {
                    failures.error.get_or_insert_with(|| error.to_string());
                }
            }
        }
        if searched {
            route.failed_searches = route.failed_searches.saturating_add(1);
        }
        Managed::Unverified(failures.reason(searched))
    }

    /// Builds the legacy request a step naming `model` and `effort` would have sent, against
    /// `before`, with the first runner the routes give for it.
    pub(super) fn legacy(
        &mut self,
        before: &Session,
        model: Option<&str>,
        effort: Option<&str>,
    ) -> Result<RebuiltLegacy, String> {
        let routes = self.routes;
        let runners = self
            .legacy
            .entry(model.unwrap_or_default().to_owned())
            .or_insert_with(|| routes.legacy(model));
        let runner = match runners {
            Ok(runners) => runners
                .first()
                .ok_or_else(|| String::from("no runner builds this model's requests"))?,
            Err(reason) => return Err(reason.clone()),
        };
        if let Some(model) = model.filter(|model| !model.trim().is_empty()) {
            runner.set_model(model);
        }
        runner.set_effort(effort.and_then(ReasoningEffort::parse));
        runner
            .rebuild_legacy_request(before)
            .map_err(|error| format!("it could not be built again: {error}"))
    }
}

/// The key a route's runners are kept under: everything in its identity but the epoch.
fn route_key(selection: &SelectionIdentity) -> String {
    format!(
        "{}\u{1f}{}\u{1f}{}\u{1f}{}",
        selection.provider,
        selection.endpoint_digest.as_str(),
        selection.protocol,
        selection.model
    )
}

/// The efforts to ask a runner for, the likeliest first: the recorded one read as a step, the
/// adapter's own default, then every other step for a spelling no step's name is.
fn efforts(recorded: Option<&str>) -> Vec<Option<ReasoningEffort>> {
    let mut efforts = Vec::with_capacity(EFFORTS.len().saturating_add(1));
    if let Some(step) = recorded.and_then(ReasoningEffort::parse) {
        efforts.push(Some(step));
    }
    efforts.push(None);
    for step in EFFORTS {
        if !efforts.contains(&Some(step)) {
            efforts.push(Some(step));
        }
    }
    efforts
}

/// The combinations to try, in order: `first` — the one that matched last, or else the first that
/// reached the recorded route — then, when the route may still be searched, every other; otherwise
/// `first` alone, or the likeliest when there is none.
fn combos(
    runners: usize,
    selection: &SelectionIdentity,
    first: Option<Combo>,
    searched: bool,
) -> Vec<Combo> {
    let efforts = efforts(selection.effort.as_deref());
    let likeliest = Combo {
        runner: 0,
        effort: efforts.first().copied().flatten(),
        retention: PromptCacheRetention::Short,
    };
    let mut combos: Vec<Combo> = first.into_iter().collect();
    if !searched {
        if combos.is_empty() {
            combos.push(likeliest);
        }
        return combos;
    }
    for runner in 0..runners {
        for &effort in &efforts {
            for retention in RETENTIONS {
                let combo = Combo {
                    runner,
                    effort,
                    retention,
                };
                if !combos.contains(&combo) {
                    combos.push(combo);
                }
            }
        }
    }
    combos
}

#[cfg(test)]
mod tests {
    use super::*;
    use nanus_domain::context::managed::Digest;

    fn selection(effort: Option<&str>) -> SelectionIdentity {
        SelectionIdentity {
            provider: "p".into(),
            endpoint_digest: Digest::of(b"e"),
            protocol: "w".into(),
            model: "m".into(),
            effort: effort.map(str::to_owned),
            epoch: 0,
        }
    }

    #[test]
    fn the_recorded_effort_is_tried_first_and_every_step_after_it() {
        let tried = efforts(Some("high"));
        assert_eq!(tried.first(), Some(&Some(ReasoningEffort::High)));
        assert_eq!(tried.get(1), Some(&None));
        assert_eq!(tried.len(), 8);
    }

    #[test]
    fn an_effort_no_step_is_spelled_as_starts_from_the_adapters_default() {
        let tried = efforts(Some("vendor-specific"));
        assert_eq!(tried.first(), Some(&None));
        assert_eq!(tried.len(), 8);
    }

    #[test]
    fn the_combination_that_matched_is_tried_first_and_not_twice() {
        let matched = Combo {
            runner: 1,
            effort: None,
            retention: PromptCacheRetention::Long,
        };
        let tried = combos(2, &selection(None), Some(matched), true);
        assert_eq!(tried.first(), Some(&matched));
        assert_eq!(tried.iter().filter(|combo| **combo == matched).count(), 1);
        assert_eq!(tried.len(), 2 * 8 * 3);
    }

    #[test]
    fn a_route_that_is_no_longer_searched_tries_only_the_likeliest() {
        let tried = combos(3, &selection(Some("high")), None, false);
        assert_eq!(
            tried,
            [Combo {
                runner: 0,
                effort: Some(ReasoningEffort::High),
                retention: PromptCacheRetention::Short,
            }]
        );
    }
}
