//! Fitting toward the working target once the provider's cache has lapsed.
//!
//! Hiding history rewrites the provider's cached prefix from the earliest fragment hidden, so the
//! runner hides nothing of its own below the hard allowance while the cache is warm: the model's
//! own batched proposal is the cheap way to shrink. Once the cache has lapsed the next request is
//! read uncached whatever it carries, and a smaller one costs less on this step and every step
//! after it. So when the gap since the session's last request is longer than the provider keeps a
//! prefix, the step's fit is measured against the working target rather than the allowance alone.
//!
//! The gap is read from the session's own `request/attempt` records and the runner's clock, so a
//! session reloaded from disk decides exactly as one held in memory, and nothing new is recorded:
//! the fit is an automatic revision like any other. Where the decision can be wrong, it errs
//! toward not fitting, because a fit while the prefix is still cached is the cost this exists to
//! avoid. A session with no attempt is not fitted, and an unknown lifetime is taken to be the
//! longest retention the runner can ask for.

use nanus_domain::Session;
use nanus_domain::context::managed::{fit::FitBounds, state};

use crate::agent_loop::AgentRunner;

/// The lifetime taken for a cache whose adapter does not state one: an hour, the longest
/// retention a runner can ask for.
///
/// Every wire but Anthropic's own caches by rules of its own, and a request that asked for no
/// entries is not told how long its provider keeps what it caches anyway. A fit while a prefix is
/// still cached rewrites it, which is the one cost this mechanism exists to avoid, so not knowing
/// is answered with the longest lifetime rather than the one the runner asked for.
pub(in crate::agent_loop) const UNKNOWN_CACHE_LIFETIME_MS: u64 = 3_600_000;

impl AgentRunner {
    /// How long the provider is taken to keep this session's prefix after it was last sent: the
    /// adapter's answer for the retention requests ask for, or [`UNKNOWN_CACHE_LIFETIME_MS`].
    pub(in crate::agent_loop) fn cache_lifetime_ms(&self) -> u64 {
        self.llm()
            .prompt_cache_lifetime_ms(&self.model(), self.prompt_cache())
            .unwrap_or(UNKNOWN_CACHE_LIFETIME_MS)
    }

    /// Whether the provider's cache of `session`'s prefix has lapsed: the time since the newest
    /// attempt it records is longer than [`cache_lifetime_ms`](Self::cache_lifetime_ms).
    ///
    /// Never for a runner built without cold fitting, and never for a session that has sent no
    /// managed request. A clock behind the record reads as no time at all.
    pub(in crate::agent_loop) fn cache_lapsed(&self, session: &Session) -> bool {
        self.cold_fit && self.cache_cold(session)
    }

    /// Whether the provider's cache of `session`'s prefix has lapsed, whatever this runner does
    /// about it: the fact [`cache_lapsed`](Self::cache_lapsed) fits on, without its switch.
    ///
    /// A hide's break-even rule is waived on it too, and a host that turned cold fitting off has
    /// not made a rewrite of a lapsed cache any less free. Never for a session that has sent no
    /// managed request.
    pub(in crate::agent_loop) fn cache_cold(&self, session: &Session) -> bool {
        let Some(last) = state::last_request_ms(session.log()) else {
            return false;
        };
        let idle = self.clock.now_ms().saturating_sub(last);
        idle > self.cache_lifetime_ms()
    }

    /// The sizes this step's fit is measured against: the working target when the cache has
    /// lapsed, and the hard allowance alone otherwise.
    ///
    /// Either way the allowance alone refuses. A target at or above the allowance is hard fitting
    /// exactly, so a model whose allowance is the target fits as it always has.
    pub(in crate::agent_loop) fn fit_bounds(
        &self,
        session: &Session,
        allowance: u32,
        target: u32,
    ) -> FitBounds {
        if self.cache_lapsed(session) {
            FitBounds::toward(target, allowance)
        } else {
            FitBounds::hard(allowance)
        }
    }
}
