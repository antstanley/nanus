//! What the interface knows about how fast the model is going.
//!
//! The numbers come from the agent one request at a time — the counters a request
//! reports and how long that request was in flight — and the two rates a reader wants
//! are derived from them here rather than accumulated as rates, because an average of
//! averages is not an average. Every rate is measured against *active* request time:
//! the time a request spent in flight and nothing else.

/// Tokens per second, from a token count and a duration in milliseconds.
///
/// Whole numbers rather than fractions: a rate that moves by a tenth of a token is
/// noise, and integer arithmetic keeps the workspace's rules about lossy casts and
/// panicking division out of the display path. A duration of zero — which is what an
/// agent that reports no timing sends — has no rate, and says so rather than showing
/// zero, which would read as "the model generated nothing".
fn rate(tokens: u64, millis: u64) -> Option<u64> {
    tokens.checked_mul(1_000)?.checked_div(millis)
}

/// A percentage, from a part and a whole.
///
/// `None` when the whole is zero, which is the "no requests yet" case rather than a
/// cache that served nothing.
fn percent(part: u64, whole: u64) -> Option<u64> {
    part.checked_mul(100)?.checked_div(whole)
}

/// The model's throughput, as the interface reckons it.
///
/// Every rate here is measured against *active* request time rather than wall-clock
/// time, so a session that sat idle between turns, or that spent its time running tools,
/// reports the speed of the model rather than the speed of the person reading it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Throughput {
    /// Tokens the model generated across the session.
    completion_tokens: u64,
    /// Active request time across the session, in milliseconds.
    active_ms: u64,
    /// Tokens the model generated in the last request.
    last_completion_tokens: u32,
    /// Active time of the last request, in milliseconds.
    last_active_ms: u64,
    /// Prompt tokens the provider served from its cache.
    cache_hit_tokens: u64,
    /// Prompt tokens the provider had to read.
    cache_miss_tokens: u64,
}

impl Throughput {
    /// Records one completed request.
    pub fn record(
        &mut self,
        completion_tokens: u32,
        cache_hit_tokens: u32,
        cache_miss_tokens: u32,
        duration_ms: u64,
    ) {
        self.last_completion_tokens = completion_tokens;
        self.last_active_ms = duration_ms;
        self.completion_tokens = self
            .completion_tokens
            .saturating_add(u64::from(completion_tokens));
        self.active_ms = self.active_ms.saturating_add(duration_ms);
        self.cache_hit_tokens = self
            .cache_hit_tokens
            .saturating_add(u64::from(cache_hit_tokens));
        self.cache_miss_tokens = self
            .cache_miss_tokens
            .saturating_add(u64::from(cache_miss_tokens));
    }

    /// How fast the last request generated tokens, per second.
    ///
    /// `None` before the first request of a session, and from an agent that does not
    /// report how long a request took: a rate with no time in it is not a rate, and a
    /// zero would be a claim about the model rather than about the accounting.
    #[must_use]
    pub fn last_rate(&self) -> Option<u64> {
        rate(u64::from(self.last_completion_tokens), self.last_active_ms)
    }

    /// How fast the session's requests have generated tokens, per second, on average.
    #[must_use]
    pub fn average_rate(&self) -> Option<u64> {
        rate(self.completion_tokens, self.active_ms)
    }

    /// The share of the session's prompt tokens that came from the provider's cache.
    ///
    /// The counters partition the prompt, so their sum is the prompt rather than a
    /// separate figure the provider could report inconsistently with them.
    #[must_use]
    pub fn cache_hit_percent(&self) -> Option<u64> {
        let prompt = self.cache_hit_tokens.checked_add(self.cache_miss_tokens)?;
        percent(self.cache_hit_tokens, prompt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rate_is_tokens_over_active_time() {
        let mut stats = Throughput::default();
        // 500 tokens in two seconds is 250 a second.
        stats.record(500, 0, 0, 2_000);
        assert_eq!(stats.last_rate(), Some(250));
        assert_eq!(stats.average_rate(), Some(250));
    }

    /// A request whose timing was not reported has no rate. Showing zero would say the
    /// model generated nothing, which is a different claim from "nobody measured it".
    #[test]
    fn a_request_with_no_duration_has_no_rate() {
        let mut stats = Throughput::default();
        stats.record(500, 0, 0, 0);
        assert_eq!(stats.last_rate(), None);
        assert_eq!(stats.average_rate(), None);
    }

    #[test]
    fn nothing_recorded_is_no_rate_and_no_percentage() {
        let stats = Throughput::default();
        assert_eq!(stats.last_rate(), None);
        assert_eq!(stats.average_rate(), None);
        assert_eq!(stats.cache_hit_percent(), None);
    }

    /// The average is over the session's totals, not the mean of its per-request rates:
    /// a fast request and a slow one do not average to the mean of their speeds unless
    /// they took the same time, and the totals are the honest figure.
    #[test]
    fn the_average_is_over_the_totals_rather_than_the_rates() {
        let mut stats = Throughput::default();
        stats.record(900, 0, 0, 1_000); // 900 tokens in one second.
        stats.record(100, 0, 0, 9_000); // 100 tokens in nine seconds.
        // The mean of the two rates would be 450; the totals say 100 tokens a second.
        assert_eq!(stats.last_rate(), Some(11));
        assert_eq!(stats.average_rate(), Some(100));
    }

    #[test]
    fn the_cache_percentage_is_the_hit_share_of_the_prompt() {
        let mut stats = Throughput::default();
        stats.record(10, 900, 100, 1_000);
        assert_eq!(stats.cache_hit_percent(), Some(90));
        // A second request moves both counters, and the share follows the totals.
        stats.record(10, 0, 1_000, 1_000);
        assert_eq!(stats.cache_hit_percent(), Some(45));
    }

    /// A provider that reports a prompt with no cache accounting at all reports zeroes,
    /// and zero of zero is not zero per cent — it is nothing measured.
    #[test]
    fn a_prompt_with_no_counters_has_no_percentage() {
        let mut stats = Throughput::default();
        stats.record(10, 0, 0, 1_000);
        assert_eq!(stats.cache_hit_percent(), None);
    }
}
