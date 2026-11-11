//! Auditing a recorded session's prompt-cache reads, offline, and attributing each miss.
//!
//! When a step reads little or nothing from a provider's prompt cache, the session log alone
//! cannot say whether nanus changed the request's prefix or the provider missed on a prefix it had
//! seen. Recording that per attempt would need a new field on the `request/attempt` record, whose
//! readers refuse unknown fields, so it would move the body version. Nothing here records
//! anything: the question is answered from sessions already recorded.
//!
//! That is possible because preparation is deterministic. A managed request's intent records its
//! route and the digest of the body it dispatched, so the audit walks the log, prepares each
//! request again from the log as it stood (see [`AgentRunner::rederive_managed_request`]), and
//! claims a request only when the body it prepared has exactly the recorded digest. Two
//! consecutive claimed requests are then compared by their canonical encodings — no tail, no
//! cache hints — and the earlier one is a prefix of the later one, or the audit says the byte
//! where they part.
//!
//! A legacy step records no digest. Its request is built again through the runner's own fitter
//! ([`AgentRunner::rebuild_legacy_request`]), and every comparison that rests on one is labelled
//! as rebuilt rather than verified: it says what this configuration would have sent, not what was.
//!
//! The audit needs no network and no credential, and it reads the session it is given: it never
//! writes to a store and takes no claim. Which runners can have prepared a request is the
//! caller's answer ([`AuditRoutes`]); the stock composition gives one in
//! `compose::audit_routes`, behind the `stock-compose` feature.

use serde::{Deserialize, Serialize};

use nanus_domain::Session;
use nanus_domain::context::managed::SelectionIdentity;

use crate::AgentRunner;

#[path = "cache_audit/attribute.rs"]
mod attribute;
#[path = "cache_audit/summary.rs"]
mod summary;
#[path = "cache_audit/timeline.rs"]
mod timeline;
#[path = "cache_audit/verify.rs"]
mod verify;

pub use summary::{AuditSummary, CLASSES, ClassTotal, class_of};

/// A read this many tokens or fewer short of the previous request's prompt still covers it.
///
/// The tail is the request's last content, and every provider's cache ends its entries on whole
/// units before it: `DeepSeek` reads the previous prompt less a few hundred tokens, `OpenAI` to a
/// 128-token granularity, Inception less up to about a thousand (see the context-management
/// guide's measurements). A shortfall that size is the layout working, not a miss.
pub const HIT_SHORTFALL_TOKENS: u64 = 1_024;

/// A read short of the previous prompt by this share of it or less still covers it.
///
/// For long prompts, whose tail and unit rounding are a smaller share than they are of a short one
/// but whose last tool result may be long: the larger of this and [`HIT_SHORTFALL_TOKENS`] is the
/// slack.
pub const HIT_SHORTFALL_PERCENT: u64 = 10;

/// Where an audit gets the runners that may have prepared a recorded request.
///
/// A runner is configured as the recording one was — system prompt, schemas, budget, working
/// target — and the audit varies only what a request records or a turn can switch: the model, the
/// effort and the retention. The audit mutates the runners it is given, so they are its own.
pub trait AuditRoutes {
    /// The runners a managed request recorded under `selection` may have been prepared by, the
    /// likeliest first.
    ///
    /// # Errors
    ///
    /// A sentence saying why no runner can be built for it, which the audit reports as the reason
    /// the request is unverified.
    fn managed(&self, selection: &SelectionIdentity) -> Result<Vec<AgentRunner>, String>;

    /// The runners a legacy request that named `model` may have been built by, the likeliest
    /// first. The audit builds with the first.
    ///
    /// # Errors
    ///
    /// A sentence saying why none can be built.
    fn legacy(&self, model: Option<&str>) -> Result<Vec<AgentRunner>, String>;
}

/// One recorded session's audit: a row per model request, in order, and the totals.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheAudit {
    /// The session's store key.
    pub session: String,
    /// One row per model request, in log order.
    pub requests: Vec<AuditedRequest>,
    /// The totals, and the counts per attribution class.
    pub summary: AuditSummary,
}

/// What kind of request a row is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestKind {
    /// A managed step, with a recorded intent and digest.
    Managed,
    /// A legacy step, which records no intent and no digest.
    Legacy,
    /// A managed handoff draft.
    Handoff,
}

/// The cache read a request's usage recorded, and what its absence means.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum CacheRead {
    /// The provider reported this many tokens read from its cache, zero included.
    Reported {
        /// The tokens read.
        tokens: u64,
    },
    /// The provider's report carried no cache-read counter.
    NotReported,
    /// The record cannot say whether the provider reported zero or nothing: an attempt written
    /// under `nanus.usage.v1`, or an assistant message's usage, whose shape always writes zero.
    ZeroOrNotReported,
    /// No usage was recorded for the request at all.
    NoUsage,
}

impl CacheRead {
    /// The tokens read, when the provider reported a count.
    #[must_use]
    pub const fn tokens(self) -> Option<u64> {
        match self {
            Self::Reported { tokens } => Some(tokens),
            _ => None,
        }
    }
}

/// How far the audit got in proving which request a row was.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "basis", rename_all = "snake_case")]
pub enum Verification {
    /// Prepared again to exactly the recorded digest.
    Digest {
        /// The retention the matching preparation asked for: `none`, `short` or `long`.
        retention: String,
        /// Whether its tail carried the budget hint.
        budget_hint: bool,
    },
    /// A legacy request built again from the log; nothing recorded can confirm it.
    Rebuilt,
    /// Not re-derived.
    Unverified {
        /// Why.
        reason: String,
    },
}

/// Whether a comparison rests on two verified requests or on at least one rebuilt one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Basis {
    /// Both requests were prepared again to their recorded digests.
    Digest,
    /// At least one of them is a rebuilt legacy request.
    Rebuilt,
}

/// Why a request read what it read from the provider's cache.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "class", rename_all = "snake_case")]
pub enum Attribution {
    /// The session's first request: nothing earlier to read.
    First,
    /// The previous request is not a prefix of this one: nanus changed what the provider had
    /// cached.
    PrefixChanged {
        /// The byte, over the previous request's canonical encoding, where the two part.
        offset: u64,
        /// The previous request's canonical length in bytes.
        previous_bytes: u64,
        /// What the log records between the two that changes a request's prefix, when anything.
        causes: Vec<String>,
        /// What the comparison rests on.
        basis: Basis,
    },
    /// The prefix was stable and the read covered about the previous prompt.
    Hit {
        /// What the comparison rests on.
        basis: Basis,
    },
    /// The prefix was stable and the provider read less than the previous prompt.
    ProviderMiss {
        /// Whether the idle gap exceeded the cache lifetime taken for the route: `None` when the
        /// gap is not recorded.
        expired: Option<bool>,
        /// What the comparison rests on.
        basis: Basis,
    },
    /// The prefix was stable, and the read cannot be compared with the previous prompt: one of
    /// the two was not reported.
    StableUnmeasured {
        /// What the comparison rests on.
        basis: Basis,
    },
    /// Refused before dispatch: the provider never saw it.
    NotSent,
    /// Not attributed.
    Unverified {
        /// Why.
        reason: String,
    },
}

/// One model request and what the audit found.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditedRequest {
    /// The turn, from zero.
    pub turn: u64,
    /// The step within the turn, from zero.
    pub step: u64,
    /// What kind of request it was.
    pub kind: RequestKind,
    /// The model it named, when recorded.
    pub model: Option<String>,
    /// The prompt tokens the provider reported, when it did.
    pub prompt_tokens: Option<u64>,
    /// The cache read, as recorded.
    pub cache_read: CacheRead,
    /// Milliseconds from the previous request's finish to this one's start, when both are
    /// recorded.
    pub idle_ms: Option<u64>,
    /// The cache lifetime taken for this request's route, when it is known.
    pub lifetime_ms: Option<u64>,
    /// The projection revision it was built from, for a managed request.
    pub revision: Option<u64>,
    /// How far the audit got in proving which request it was.
    pub verification: Verification,
    /// Why it read what it read.
    pub attribution: Attribution,
}

/// Audits `session`, preparing its requests again with the runners `routes` gives.
///
/// Reads `session` and nothing else: no network, no credential, no store.
#[must_use]
pub fn audit(session: &Session, routes: &dyn AuditRoutes) -> CacheAudit {
    let observed = timeline::observe(session, &mut verify::Verifier::new(routes));
    let requests = attribute::attribute(session, &observed);
    let summary = summary::summarise(&requests);
    CacheAudit {
        session: session.id().as_str().to_owned(),
        requests,
        summary,
    }
}
