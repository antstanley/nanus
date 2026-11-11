//! Whether a model's hide pays for the cache rewrite it forces.
//!
//! Provider prompt caches are prefix caches. When an accepted proposal hides fragments, the next
//! request is read uncached from the earliest fragment the proposal changed to its end — once —
//! and every request after it is smaller by what was hidden. While the cache is warm, a hidden
//! token saves only its cache-read price on each later step, about a tenth of the input price,
//! and the suffix that is re-read costs the full input price. So a small hide early in a long
//! conversation costs more than it ever saves: hiding 20,000 tokens ahead of a 200,000-token
//! suffix takes on the order of a hundred steps to pay back.
//!
//! The rule here quantifies that from two numbers the adapter estimates, and a proposal whose hide
//! cannot pay back within [`BREAK_EVEN_HORIZON_STEPS`] is refused before it is staged — unless the
//! session needs the room (the request is under the soft pressure the budget hint answers) or the
//! rewrite is free (the provider's cache has lapsed). A proposal that hides nothing is never
//! judged: a restore is about correctness, and a note change is a different cost.
//!
//! The arithmetic, with `R` the tokens the hide removes and `S` the tokens the provider would
//! re-read from the earliest change to the end of the next request:
//!
//! - each later step saves `R / CACHE_READ_DISCOUNT` input-price tokens, so the hide saves
//!   `R × BREAK_EVEN_HORIZON_STEPS / CACHE_READ_DISCOUNT` over the horizon;
//! - the rewrite costs `S` input-price tokens once. Strictly it costs `S × (1 − 1/10)` more than a
//!   warm read would, and on Anthropic a cache write carries a premium of 1.25 (five minutes) or 2
//!   (an hour) on top; those corrections pull in opposite directions and are within the rounding of
//!   the horizon, so the suffix is taken at the full input price;
//! - so the hide pays for itself when `R × 50 / 10 ≥ S`: **the tokens a hide removes must be at
//!   least one fifth of the tokens it forces to be re-read.**
//!
//! A hide near the end of the conversation has a small suffix and passes by the arithmetic alone.
//! Nothing here is recorded: the rule decides only whether a proposal is staged, a refused one is
//! an ordinary tool failure, and a staged proposal is re-validated after its step exactly as
//! before.

use serde::{Deserialize, Serialize};

use super::ids::FragmentId;
use super::limits;

/// How many times cheaper a cache read is than an uncached input token.
///
/// A documented price ratio, not a measured one: a cache read costs a tenth of the input price on
/// Anthropic's models (a twentieth on Opus 5.5), and the other supported providers document a
/// discount of the same order or deeper. The deeper the discount, the less a hide saves, so the
/// shallowest documented figure is the one that refuses least.
pub const CACHE_READ_DISCOUNT: u64 = 10;

/// The later steps a hide is given to pay back the rewrite it forces.
///
/// A long agentic turn runs tens of steps between two prompts, and a session served over the link
/// keeps its cache across prompts for an hour. Fifty steps is a long turn's worth: a hide that
/// needs more than that is betting on a session it cannot see.
pub const BREAK_EVEN_HORIZON_STEPS: u64 = 50;

/// The tokens a hide may force to be re-read per token it removes: the horizon over the discount.
///
/// `50 / 10 = 5`, so a hide must remove at least a fifth of what it makes the provider re-read.
pub const REREAD_PER_HIDDEN_TOKEN_MAX: u64 = 5;

const _: () = assert!(
    matches!(
        REREAD_PER_HIDDEN_TOKEN_MAX.checked_mul(CACHE_READ_DISCOUNT),
        Some(BREAK_EVEN_HORIZON_STEPS)
    ),
    "the ratio is the horizon over the discount"
);

/// What a proposed hide costs and saves, in the adapter's estimated tokens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HideCost {
    /// The tokens the next request would be smaller by: the accepted selection's estimate less the
    /// candidate's.
    pub removed_tokens: u64,
    /// The tokens the next request would carry from the earliest changed fragment to its end: the
    /// candidate's estimate less that of its unchanged prefix.
    pub reread_tokens: u64,
}

impl HideCost {
    /// The cost of a candidate from three estimates of the next request: under the accepted
    /// selection, under the candidate, and of the candidate's prefix the change leaves untouched.
    ///
    /// Each difference saturates: a candidate no smaller than the accepted selection removes
    /// nothing, and a prefix is never larger than the request it begins.
    #[must_use]
    pub const fn measured(current: u64, candidate: u64, prefix: u64) -> Self {
        Self {
            removed_tokens: current.saturating_sub(candidate),
            reread_tokens: candidate.saturating_sub(prefix),
        }
    }

    /// Whether the hide pays for its rewrite within [`BREAK_EVEN_HORIZON_STEPS`]: it removes at
    /// least one [`REREAD_PER_HIDDEN_TOKEN_MAX`]th of what it forces to be re-read.
    #[must_use]
    pub const fn pays_for_itself(self) -> bool {
        self.removed_tokens
            .saturating_mul(REREAD_PER_HIDDEN_TOKEN_MAX)
            >= self.reread_tokens
    }

    /// The fewest tokens a hide ahead of this suffix must remove to pay for itself.
    #[must_use]
    pub const fn removed_needed(self) -> u64 {
        self.reread_tokens.div_ceil(REREAD_PER_HIDDEN_TOKEN_MAX)
    }
}

/// Why any hide is accepted now, whatever its size.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HideWaiver {
    /// The request is at or over the soft pressure the budget hint is given at: the session needs
    /// the room, so any reduction is welcome.
    Pressure,
    /// The provider's cache of the session has lapsed: the next request is read uncached whatever
    /// it carries, so the rewrite is free.
    CacheLapsed,
}

/// The waiver in force, if any: pressure first, because it is what the model was shown.
///
/// `pressure_percent` is the step's estimate as a percentage of the working target, exactly as the
/// tail states it; `None` when the step stated none, which waives nothing.
#[must_use]
pub fn waiver(pressure_percent: Option<u32>, cache_lapsed: bool) -> Option<HideWaiver> {
    if pressure_percent.is_some_and(|percent| percent >= limits::SOFT_PRESSURE_PERCENT) {
        Some(HideWaiver::Pressure)
    } else if cache_lapsed {
        Some(HideWaiver::CacheLapsed)
    } else {
        None
    }
}

/// What an inspect result states about the rule, so a model can plan one proposal that passes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HideRule {
    /// The estimated tokens a hide may force the provider to re-read, from the earliest fragment
    /// it changes to the end of the request, per estimated token it removes.
    pub max_reread_per_hidden_token: u64,
    /// Why any hide is accepted now whatever its size, or `None` when the ratio applies.
    pub waived: Option<HideWaiver>,
}

impl HideRule {
    /// The rule in force, with `waived` as the waiver.
    #[must_use]
    pub const fn with(waived: Option<HideWaiver>) -> Self {
        Self {
            max_reread_per_hidden_token: REREAD_PER_HIDDEN_TOKEN_MAX,
            waived,
        }
    }
}

/// The earliest fragment a change from `accepted` to `next` hides or restores, or `None` when the
/// two hidden sets are the same.
#[must_use]
pub fn earliest_change(accepted: &[FragmentId], next: &[FragmentId]) -> Option<FragmentId> {
    let hidden_now = accepted.iter().filter(|id| !next.contains(id));
    let hidden_next = next.iter().filter(|id| !accepted.contains(id));
    hidden_now.chain(hidden_next).min().copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(seq: u64) -> FragmentId {
        FragmentId::new(seq)
    }

    #[test]
    fn a_small_hide_ahead_of_a_long_suffix_does_not_pay_for_itself() {
        // The figure the rule exists for: 20,000 hidden ahead of 200,000 re-read.
        let small = HideCost {
            removed_tokens: 20_000,
            reread_tokens: 200_000,
        };
        assert!(!small.pays_for_itself());
        assert_eq!(small.removed_needed(), 40_000);
        let enough = HideCost {
            removed_tokens: 40_000,
            ..small
        };
        assert!(
            enough.pays_for_itself(),
            "a fifth is the boundary, inclusive"
        );
        let short = HideCost {
            removed_tokens: 39_999,
            ..small
        };
        assert!(!short.pays_for_itself());
    }

    #[test]
    fn a_hide_near_the_end_passes_by_the_arithmetic_alone() {
        let late = HideCost::measured(110_000, 104_000, 90_000);
        assert_eq!(late.removed_tokens, 6_000);
        assert_eq!(late.reread_tokens, 14_000);
        assert!(late.pays_for_itself());
        let early = HideCost::measured(110_000, 104_000, 10_000);
        assert!(!early.pays_for_itself());
    }

    #[test]
    fn a_candidate_that_removes_nothing_pays_only_for_an_empty_suffix() {
        let grows = HideCost::measured(100, 120, 40);
        assert_eq!(grows.removed_tokens, 0);
        assert!(!grows.pays_for_itself());
        let nothing = HideCost::measured(100, 100, 100);
        assert!(nothing.pays_for_itself());
        assert_eq!(nothing.removed_needed(), 0);
    }

    #[test]
    fn pressure_and_a_lapsed_cache_waive_the_rule_and_nothing_else_does() {
        let at = limits::SOFT_PRESSURE_PERCENT;
        assert_eq!(waiver(Some(at), false), Some(HideWaiver::Pressure));
        assert_eq!(waiver(Some(at), true), Some(HideWaiver::Pressure));
        assert_eq!(waiver(Some(at - 1), true), Some(HideWaiver::CacheLapsed));
        assert_eq!(waiver(Some(at - 1), false), None);
        assert_eq!(waiver(None, false), None);
    }

    #[test]
    fn the_earliest_change_is_the_oldest_hidden_or_restored_fragment() {
        assert_eq!(earliest_change(&[id(5)], &[id(5)]), None);
        assert_eq!(earliest_change(&[], &[id(9), id(12)]), Some(id(9)));
        assert_eq!(
            earliest_change(&[id(3), id(9)], &[id(9), id(20)]),
            Some(id(3))
        );
        assert_eq!(earliest_change(&[id(30)], &[id(30), id(7)]), Some(id(7)));
    }

    #[test]
    fn the_rule_serializes_as_a_number_and_an_optional_reason() {
        let value = serde_json::to_value(HideRule::with(Some(HideWaiver::CacheLapsed))).unwrap();
        assert_eq!(
            value,
            serde_json::json!({"max_reread_per_hidden_token": 5, "waived": "cache_lapsed"})
        );
        let none = serde_json::to_value(HideRule::with(None)).unwrap();
        assert_eq!(none["waived"], serde_json::Value::Null);
    }
}
