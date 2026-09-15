//! What the interface knows about how fast the model is going.
//!
//! The numbers come from the agent one request at a time — the counters a request reports and
//! the three durations it reports them with — and the rates a reader wants are derived from
//! them here rather than accumulated as rates, because an average of averages is not an
//! average. Every rate is measured against *generation* time: the stretch from a request's
//! first generated token to its last, and nothing else.
//!
//! ## Why generation time rather than the whole request
//!
//! A request's active time is the wait for its first token, the generation, and however long
//! the stream took to close. Only the middle of those is the model generating, and in a coding
//! session the other two dominate: a tool call is a short generation behind a long wait,
//! because the wait is where the prompt is read. Dividing generated tokens by the whole
//! request therefore reports a number well below the model's speed and calls it the model's
//! speed — worst for exactly the steps a coding session is mostly made of.
//!
//! The wait is reported as its own figure rather than folded in, because it is a real cost a
//! reader waits through *and* one they can do something about: a shorter prompt or a cached one
//! shortens it, whereas the generation rate is the provider's and is not theirs to change.

/// Tokens per second, from a token count and a duration in milliseconds.
///
/// Whole numbers rather than fractions: a rate that moves by a tenth of a token is noise, and
/// integer arithmetic keeps the workspace's rules about lossy casts and panicking division out
/// of the display path. A duration of zero — which is what an agent that reports no timing
/// sends — has no rate, and says so rather than showing zero, which would read as "the model
/// generated nothing".
fn rate(tokens: u64, millis: u64) -> Option<u64> {
    tokens.checked_mul(1_000)?.checked_div(millis)
}

/// A percentage, from a part and a whole.
///
/// `None` when the whole is zero, which is the "nothing was counted" case rather than a
/// quantity that happens to be empty.
fn percent(part: u64, whole: u64) -> Option<u64> {
    part.checked_mul(100)?.checked_div(whole)
}

/// A duration that was measured, or nothing.
///
/// A missing duration and a measured one of zero are the same number on the wire, and for
/// these figures zero is the missing case: reaching a provider and coming back takes longer
/// than a millisecond, so a wait or a generation of exactly zero milliseconds is not something
/// that happens, and zero is free to mean "not measured". Showing `0` for it would be a claim
/// about the model rather than about the accounting.
fn measured(millis: u64) -> Option<u64> {
    (millis > 0).then_some(millis)
}

/// A reading written out, or a dash when nobody took it.
///
/// Zero is left as zero: it is a measurement — the model generated nothing, or none of what it
/// generated was thinking — and a dash would say the reader was not told, which is a different
/// thing to have been told.
pub(crate) fn show(value: Option<u64>) -> String {
    value.map_or_else(|| String::from("\u{2014}"), |number| number.to_string())
}

/// The same, with a per-cent sign.
pub(crate) fn share(value: Option<u64>) -> String {
    value.map_or_else(|| String::from("\u{2014}"), |number| format!("{number}%"))
}

/// A duration in the form the interface writes one: whole milliseconds below a second, one
/// decimal of a second above it, and a dash for one that was not measured.
///
/// Integer arithmetic rather than a float, for the same reason the rates are whole numbers: this
/// workspace denies the operators that would need a lossy cast to print, and a tenth of a second
/// is all the precision a reader glancing at a row can use. Milliseconds rather than seconds
/// below the first second is not decoration — a wait of forty milliseconds printed as `0.0s`
/// would read as no wait at all.
pub(crate) fn show_duration(millis: Option<u64>) -> String {
    let Some(millis) = millis else {
        return String::from("\u{2014}");
    };
    let seconds = millis.checked_div(1_000).unwrap_or(0);
    if seconds == 0 {
        return format!("{millis}ms");
    }
    let whole = seconds.checked_mul(1_000).unwrap_or(0);
    let tenths = millis.saturating_sub(whole).checked_div(100).unwrap_or(0);
    format!("{seconds}.{tenths}s")
}

/// One request's generation, as the agent measured it.
///
/// The sum of two of these is another one, field by field, which is how the session totals
/// below are kept: what a reader wants over more requests is the same shape, not a different
/// one. Every field a provider did not report is zero, and every duration the agent could not
/// measure is zero — see [`measured`] for why that is unambiguous.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Generation {
    /// Tokens the model generated, thinking included.
    pub completion_tokens: u64,
    /// The share of `completion_tokens` spent thinking, which is inside it rather than beside
    /// it and so must never be added to it.
    pub reasoning_tokens: u64,
    /// Prompt tokens the provider served from its cache.
    pub cache_hit_tokens: u64,
    /// Prompt tokens the provider had to read.
    pub cache_miss_tokens: u64,
    /// How long the request waited for its first generated token.
    pub ttft_ms: u64,
    /// How long it spent generating: its first token to its last.
    pub decode_ms: u64,
    /// How long it was in flight at all.
    pub duration_ms: u64,
}

impl Generation {
    /// Adds `other` into this one, field by field, saturating.
    fn absorb(&mut self, other: Self) {
        self.completion_tokens = self
            .completion_tokens
            .saturating_add(other.completion_tokens);
        self.reasoning_tokens = self.reasoning_tokens.saturating_add(other.reasoning_tokens);
        self.cache_hit_tokens = self.cache_hit_tokens.saturating_add(other.cache_hit_tokens);
        self.cache_miss_tokens = self
            .cache_miss_tokens
            .saturating_add(other.cache_miss_tokens);
        self.ttft_ms = self.ttft_ms.saturating_add(other.ttft_ms);
        self.decode_ms = self.decode_ms.saturating_add(other.decode_ms);
        self.duration_ms = self.duration_ms.saturating_add(other.duration_ms);
    }

    /// Whether this request reported a generation window.
    fn timed(&self) -> bool {
        self.decode_ms > 0
    }
}

/// The model's throughput, as the interface reckons it.
///
/// Every rate here is measured against *active* request time rather than wall-clock time, so a
/// session that sat idle between turns, or that spent its time running tools, reports the speed
/// of the model rather than the speed of the person reading it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Throughput {
    /// The last request, as reported.
    last: Generation,
    /// Every request, summed.
    all: Generation,
    /// Only the requests that reported a generation window, summed.
    timed: Generation,
    /// How many requests reported a generation window.
    timed_requests: u64,
    /// How many requests have been recorded.
    requests: u64,
}

impl Throughput {
    /// Records one completed request.
    pub fn record(&mut self, generation: Generation) {
        self.requests = self.requests.saturating_add(1);
        self.all.absorb(generation);
        // Only a request that reported a window joins the timed totals. One that did not is
        // left out of both sides of every rate derived from them, rather than contributing its
        // tokens to the numerator and nothing to the denominator — which is how an untimed
        // request would make the model look arbitrarily fast.
        if generation.timed() {
            self.timed_requests = self.timed_requests.saturating_add(1);
            self.timed.absorb(generation);
        }
        self.last = generation;
    }

    /// How fast the last request generated tokens, per second.
    ///
    /// `None` before the first request of a session, and whenever the agent reported no
    /// generation window — an agent that predates the measurement, or a response that arrived
    /// in a single chunk, where there is no interval between an instant and itself. A blank is
    /// the honest reading there, and it costs the reader nothing now that
    /// [`last_request_rate`](Self::last_request_rate) sits beside it with the one denominator
    /// that is always available.
    #[must_use]
    pub fn last_rate(&self) -> Option<u64> {
        rate(self.last.completion_tokens, self.last.decode_ms)
    }

    /// How fast the last request ran end to end, per second.
    ///
    /// Generated tokens over the request's whole active time — its wait for a first token, its
    /// generation, and however long its stream took to close. This is the rate a reader
    /// *experiences* rather than the one the model achieves, and the two differ by exactly the
    /// time the model spent not generating. It is always available when a request was timed at
    /// all, which is what makes it the figure to read when the generation window is missing.
    #[must_use]
    pub fn last_request_rate(&self) -> Option<u64> {
        rate(self.last.completion_tokens, self.last.duration_ms)
    }

    /// How fast the session's requests have generated tokens, per second, on average.
    ///
    /// Over the session's totals rather than the mean of its per-request rates, because a mean
    /// of rates is only an average when every request took the same time. Over the requests
    /// that reported a window, and only those: a request that reported none is left out of both
    /// sides rather than contributing its tokens to the numerator and nothing to the
    /// denominator, which is how an untimed request would report a model of infinite speed.
    #[must_use]
    pub fn average_rate(&self) -> Option<u64> {
        rate(self.timed.completion_tokens, self.timed.decode_ms)
    }

    /// How fast the session's requests have run end to end, per second, on average.
    ///
    /// Over every request's active time, because that denominator needs no measurement the
    /// agent might not have made — which is why it is the one average that survives an agent
    /// that reports no timing at all.
    #[must_use]
    pub fn average_request_rate(&self) -> Option<u64> {
        rate(self.all.completion_tokens, self.all.duration_ms)
    }

    /// How long the last request waited for its first token.
    #[must_use]
    pub fn last_ttft_ms(&self) -> Option<u64> {
        measured(self.last.ttft_ms)
    }

    /// How long the session's timed requests have waited for a first token, on average.
    #[must_use]
    pub fn average_ttft_ms(&self) -> Option<u64> {
        let mean = self.timed.ttft_ms.checked_div(self.timed_requests)?;
        measured(mean)
    }

    /// The share of the session's prompt tokens that came from the provider's cache.
    ///
    /// The counters partition the prompt, so their sum is the prompt rather than a separate
    /// figure the provider could report inconsistently with them. Over every request rather
    /// than only the timed ones: what the cache served is a fact about the prompt, and it does
    /// not need a clock to be true.
    #[must_use]
    pub fn cache_hit_percent(&self) -> Option<u64> {
        let prompt = self
            .all
            .cache_hit_tokens
            .checked_add(self.all.cache_miss_tokens)?;
        percent(self.all.cache_hit_tokens, prompt)
    }

    /// The share of the session's generated tokens the model spent thinking.
    ///
    /// Worth a number because thinking is generated at the model's speed but is not in the
    /// answer, so a reader comparing what they watched arrive against the tokens they were
    /// billed for has no other way to account for the difference.
    #[must_use]
    pub fn reasoning_percent(&self) -> Option<u64> {
        percent(self.all.reasoning_tokens, self.all.completion_tokens)
    }

    /// Prompt tokens the session's timed requests moved per second of waiting for a first
    /// token.
    ///
    /// A *floor* on the provider's prefill throughput rather than a measurement of it. The wait
    /// covers the connection and the provider's queue as well as the prompt being read, so the
    /// real prefill is at least this fast and possibly much faster — and nothing at this end of
    /// a socket can separate the three, which is why the figure is named for what crosses
    /// rather than for what the provider did.
    #[must_use]
    pub fn encode_rate(&self) -> Option<u64> {
        let prompt = self
            .timed
            .cache_hit_tokens
            .checked_add(self.timed.cache_miss_tokens)?;
        rate(prompt, self.timed.ttft_ms)
    }

    /// How many requests have been recorded.
    #[must_use]
    pub const fn requests(&self) -> u64 {
        self.requests
    }

    /// Every prompt token the session sent, cached and not.
    ///
    /// Derived from the two cache counters rather than tracked beside them, because they
    /// partition the prompt: a separate total is one more number that could disagree with the
    /// parts it is meant to be the sum of.
    #[must_use]
    pub fn prompt_tokens(&self) -> u64 {
        self.all
            .cache_hit_tokens
            .saturating_add(self.all.cache_miss_tokens)
    }

    /// Everything the session has, as the block `/stats` prints into the transcript.
    ///
    /// The phrasing is the interface's, because none of it is sent anywhere: it is written to be
    /// read once. So every reading carries its unit, a reading nobody took is a dash rather than
    /// a zero, and the two rates are named for what they divide by — *generating* is the model's
    /// speed and *whole request* is the reader's, and the gap between them is the time the model
    /// spent not generating.
    #[must_use]
    pub fn report(&self) -> String {
        let row = |label: &str, reading: String| format!("  {label:<13} {reading}");
        let generated = self.all.completion_tokens;
        let thinking = self.all.reasoning_tokens;
        let lines = [
            String::from("session stats"),
            row("requests", self.requests.to_string()),
            row(
                "generated",
                format!(
                    "{generated} tokens ({thinking} thinking \u{b7} {})",
                    share(self.reasoning_percent())
                ),
            ),
            row(
                "prompt",
                format!(
                    "{} tokens \u{b7} {} cached, {} read \u{b7} {} hit",
                    self.prompt_tokens(),
                    self.all.cache_hit_tokens,
                    self.all.cache_miss_tokens,
                    share(self.cache_hit_percent())
                ),
            ),
            row(
                "generating",
                format!(
                    "last {} tok/s \u{b7} average {} tok/s",
                    show(self.last_rate()),
                    show(self.average_rate())
                ),
            ),
            row(
                "whole request",
                format!(
                    "last {} tok/s \u{b7} average {} tok/s",
                    show(self.last_request_rate()),
                    show(self.average_request_rate())
                ),
            ),
            row(
                "first token",
                format!(
                    "last {} \u{b7} average {}",
                    show_duration(self.last_ttft_ms()),
                    show_duration(self.average_ttft_ms())
                ),
            ),
            row(
                "prefill",
                format!(
                    "{} prompt tok/s while waiting (a floor, not the provider's rate)",
                    show(self.encode_rate())
                ),
            ),
        ];
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A request that generated `tokens` over `decode_ms` of generation, behind a `ttft_ms`
    /// wait, with the wait and the generation adding up to its active time.
    fn generation(tokens: u64, ttft_ms: u64, decode_ms: u64) -> Generation {
        Generation {
            completion_tokens: tokens,
            ttft_ms,
            decode_ms,
            duration_ms: ttft_ms.saturating_add(decode_ms),
            ..Generation::default()
        }
    }

    /// The change this whole module exists for: a rate is measured over the generation, not
    /// over the request. 500 tokens generated in two seconds is 250 a second however long the
    /// model waited before its first token, and counting the wait would report 100.
    #[test]
    fn a_rate_is_tokens_over_generation_time_rather_than_over_the_request() {
        let mut stats = Throughput::default();
        stats.record(generation(500, 3_000, 2_000));
        assert_eq!(stats.last_rate(), Some(250), "the wait is not generation");
        assert_eq!(stats.average_rate(), Some(250));
    }

    /// A request that generated nothing over a measured window rates a real zero: zero tokens
    /// a second is a measurement, and a different claim from "nobody measured it" — which is
    /// what a request with no duration at all reports, below.
    #[test]
    fn a_request_that_generated_nothing_rates_zero() {
        let mut stats = Throughput::default();
        stats.record(generation(0, 800, 1_200));
        assert_eq!(stats.last_rate(), Some(0));
    }

    /// A request whose generation window was not measured leaves the generating rate blank rather
    /// than filling it with the whole-request rate. The blank costs nothing now that the
    /// whole-request rate is reported in its own right: a reader has the figure either way, and
    /// showing one number under two labels would claim the model was generating during a wait it
    /// was not.
    #[test]
    fn an_unmeasured_window_leaves_the_generating_rate_blank() {
        let mut stats = Throughput::default();
        stats.record(Generation {
            completion_tokens: 500,
            ttft_ms: 0,
            decode_ms: 0,
            duration_ms: 2_500,
            ..Generation::default()
        });
        assert_eq!(stats.last_rate(), None, "no window, so no generating rate");
        assert_eq!(stats.average_rate(), None);
        assert_eq!(stats.last_request_rate(), Some(200), "the wait included");
        assert_eq!(stats.average_request_rate(), Some(200));
    }

    /// A request that reported no window at all must not join the timed totals: its tokens in
    /// the numerator against nothing in the denominator would report a session that generated
    /// arbitrarily fast. The three timed requests say 500 tokens a second, and the untimed one
    /// with its 10,000 tokens must not move that.
    #[test]
    fn an_untimed_request_does_not_join_the_average_its_tokens_would_inflate() {
        let mut stats = Throughput::default();
        stats.record(generation(100, 100, 200));
        stats.record(generation(100, 100, 200));
        stats.record(generation(100, 100, 200));
        assert_eq!(stats.average_rate(), Some(500));

        stats.record(Generation {
            completion_tokens: 10_000,
            duration_ms: 1,
            ..Generation::default()
        });
        assert_eq!(
            stats.average_rate(),
            Some(500),
            "the untimed request is left out"
        );
        // Its own figures are still reported, from the one denominator it has: nothing was
        // measured for its generation, and its whole request took a millisecond.
        assert_eq!(stats.last_rate(), None);
        assert_eq!(stats.last_request_rate(), Some(10_000_000));
    }

    /// A request whose timing was not reported has no rate. Showing zero would say the model
    /// generated nothing, which is a different claim from "nobody measured it".
    #[test]
    fn a_request_with_no_duration_has_no_rate() {
        let mut stats = Throughput::default();
        stats.record(Generation {
            completion_tokens: 500,
            ..Generation::default()
        });
        assert_eq!(stats.last_rate(), None);
        assert_eq!(stats.average_rate(), None);
        assert_eq!(stats.last_request_rate(), None, "no clock at all");
        assert_eq!(stats.average_request_rate(), None);
    }

    #[test]
    fn nothing_recorded_is_no_rate_and_no_percentage() {
        let stats = Throughput::default();
        assert_eq!(stats.last_rate(), None);
        assert_eq!(stats.average_rate(), None);
        assert_eq!(stats.cache_hit_percent(), None);
        assert_eq!(stats.last_ttft_ms(), None);
        assert_eq!(stats.average_ttft_ms(), None);
        assert_eq!(stats.reasoning_percent(), None);
        assert_eq!(stats.encode_rate(), None);
        assert_eq!(stats.requests(), 0);
    }

    /// The average is over the session's totals, not the mean of its per-request rates: a fast
    /// request and a slow one do not average to the mean of their speeds unless they took the
    /// same time, and the totals are the honest figure.
    #[test]
    fn the_average_is_over_the_totals_rather_than_the_rates() {
        let mut stats = Throughput::default();
        stats.record(generation(900, 0, 1_000)); // 900 tokens in one second.
        stats.record(generation(100, 0, 9_000)); // 100 tokens in nine seconds.
        // The mean of the two rates would be 450; the totals say 100 tokens a second.
        assert_eq!(stats.last_rate(), Some(11));
        assert_eq!(stats.average_rate(), Some(100));
    }

    #[test]
    fn the_cache_percentage_is_the_hit_share_of_the_prompt() {
        let mut stats = Throughput::default();
        stats.record(Generation {
            cache_hit_tokens: 900,
            cache_miss_tokens: 100,
            ..generation(10, 100, 200)
        });
        assert_eq!(stats.cache_hit_percent(), Some(90));
        // A second request moves both counters, and the share follows the totals.
        stats.record(Generation {
            cache_hit_tokens: 0,
            cache_miss_tokens: 1_000,
            ..generation(10, 100, 200)
        });
        assert_eq!(stats.cache_hit_percent(), Some(45));
    }

    /// A provider that reports a prompt with no cache accounting at all reports zeroes, and
    /// zero of zero is not zero per cent — it is nothing measured.
    #[test]
    fn a_prompt_with_no_counters_has_no_percentage() {
        let mut stats = Throughput::default();
        stats.record(generation(10, 100, 200));
        assert_eq!(stats.cache_hit_percent(), None);
    }

    /// Thinking is a subset of what was generated, so its share is a share of the whole and
    /// cannot exceed it. A request that generated no thinking at all reports a real zero,
    /// which is a measurement rather than an absence.
    #[test]
    fn the_reasoning_share_is_of_everything_generated() {
        let mut stats = Throughput::default();
        stats.record(Generation {
            reasoning_tokens: 300,
            ..generation(1_000, 100, 200)
        });
        assert_eq!(stats.reasoning_percent(), Some(30));

        stats.record(generation(1_000, 100, 200));
        assert_eq!(stats.reasoning_percent(), Some(15), "300 of 2,000");
    }

    #[test]
    fn a_turn_that_generated_nothing_has_no_reasoning_share() {
        let mut stats = Throughput::default();
        stats.record(generation(0, 100, 200));
        assert_eq!(stats.reasoning_percent(), None);
    }

    /// The encode figure is prompt tokens per second of waiting, over the requests that
    /// reported a window — because the wait it divides by belongs to those requests.
    #[test]
    fn the_encode_rate_is_the_prompt_crossing_during_the_wait() {
        let mut stats = Throughput::default();
        stats.record(Generation {
            cache_hit_tokens: 9_000,
            cache_miss_tokens: 1_000,
            ..generation(10, 1_000, 2_000)
        });
        assert_eq!(
            stats.encode_rate(),
            Some(10_000),
            "10,000 tokens in one second"
        );
    }

    /// A request that reported no window is left out of the encode figure too: its prompt
    /// tokens would be counted against waits that are not its own.
    #[test]
    fn an_untimed_request_does_not_join_the_encode_figure() {
        let mut stats = Throughput::default();
        stats.record(Generation {
            cache_hit_tokens: 1_000,
            cache_miss_tokens: 0,
            ..generation(10, 1_000, 2_000)
        });
        assert_eq!(stats.encode_rate(), Some(1_000));

        stats.record(Generation {
            cache_hit_tokens: 500_000,
            cache_miss_tokens: 0,
            duration_ms: 5_000,
            ..Generation::default()
        });
        assert_eq!(
            stats.encode_rate(),
            Some(1_000),
            "the untimed prompt is left out"
        );
    }

    /// The two averages divide the same tokens by different clocks, so the generating one is the
    /// higher of the two by exactly the share of the session spent not generating.
    #[test]
    fn the_two_average_rates_differ_by_the_wait() {
        let mut stats = Throughput::default();
        // Two seconds generating behind one second of waiting: 3,000 over 2,000 is 1,500, and
        // 3,000 over the whole three seconds is 1,000.
        stats.record(generation(3_000, 1_000, 2_000));
        assert_eq!(stats.last_rate(), Some(1_500));
        assert_eq!(stats.last_request_rate(), Some(1_000));
        assert_eq!(stats.average_rate(), Some(1_500));
        assert_eq!(stats.average_request_rate(), Some(1_000));
    }

    /// The report `/stats` prints carries every figure the session has, each under a label. A
    /// number without its label is a puzzle, which is the reason the block exists at all.
    #[test]
    fn the_report_names_every_figure_the_session_has() {
        let mut stats = Throughput::default();
        stats.record(Generation {
            completion_tokens: 1_000,
            reasoning_tokens: 250,
            cache_hit_tokens: 4_500,
            cache_miss_tokens: 500,
            ttft_ms: 1_000,
            decode_ms: 2_000,
            duration_ms: 3_000,
        });
        let report = stats.report();
        for expected in [
            "session stats",
            "requests      1",
            "generated     1000 tokens (250 thinking \u{b7} 25%)",
            "prompt        5000 tokens \u{b7} 4500 cached, 500 read \u{b7} 90% hit",
            "generating    last 500 tok/s \u{b7} average 500 tok/s",
            "whole request last 333 tok/s \u{b7} average 333 tok/s",
            "first token   last 1.0s \u{b7} average 1.0s",
            "prefill       5000 prompt tok/s while waiting",
        ] {
            assert!(
                report.contains(expected),
                "missing {expected:?} in:\n{report}"
            );
        }
    }

    /// The other direction: a session with nothing in it still reports, and every reading it has
    /// no value for is a dash rather than a zero. A zero would say the model generated nothing,
    /// which is a measurement nobody took.
    #[test]
    fn a_report_of_nothing_is_dashes_rather_than_zeroes() {
        let report = Throughput::default().report();
        assert!(report.contains("requests      0"), "{report}");
        assert!(
            report.contains("generating    last \u{2014} tok/s \u{b7} average \u{2014} tok/s"),
            "{report}"
        );
        assert!(
            report.contains("prompt        0 tokens \u{b7} 0 cached, 0 read \u{b7} \u{2014} hit"),
            "{report}"
        );
        assert!(
            !report.contains("0 tok/s"),
            "not a rate nobody measured: {report}"
        );
    }

    #[test]
    fn the_waits_are_averaged_over_the_requests_that_reported_one() {
        let mut stats = Throughput::default();
        stats.record(generation(10, 1_000, 500));
        stats.record(generation(10, 3_000, 500));
        assert_eq!(stats.last_ttft_ms(), Some(3_000));
        assert_eq!(stats.average_ttft_ms(), Some(2_000));
    }

    /// A session whose requests reported no wait has no average wait, rather than an average
    /// of zero: the mean is divided by the requests that reported one, and there are none.
    #[test]
    fn a_session_with_no_timed_requests_has_no_average_wait() {
        let mut stats = Throughput::default();
        stats.record(Generation {
            completion_tokens: 100,
            duration_ms: 1_000,
            ..Generation::default()
        });
        assert_eq!(stats.average_ttft_ms(), None);
        assert_eq!(stats.requests(), 1);
    }
}
