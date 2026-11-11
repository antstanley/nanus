//! The audit's totals: tokens, the read share, and what each attribution class cost.

use serde::{Deserialize, Serialize};

use super::{Attribution, AuditedRequest, Verification};

/// What one attribution class covers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClassTotal {
    /// The class: `first`, `prefix_changed`, `hit`, `provider_miss_expired`,
    /// `provider_miss_unexplained`, `provider_miss_idle_unknown`, `stable_unmeasured`, `not_sent`
    /// or `unverified`.
    pub class: String,
    /// How many requests it holds.
    pub requests: u64,
    /// Their reported prompt tokens.
    pub prompt_tokens: u64,
    /// Their reported cache reads.
    pub cache_read_tokens: u64,
    /// Prompt tokens not read from the cache, over the requests that reported both.
    pub unread_tokens: u64,
}

/// The audit's totals.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditSummary {
    /// Requests in the session, sent or not.
    pub requests: u64,
    /// Prompt tokens over every request that reported them.
    pub prompt_tokens: u64,
    /// Cache-read tokens over every request that reported a read.
    pub cache_read_tokens: u64,
    /// The cache-read share of the prompt, in whole percent, over the requests that reported
    /// both; absent when none did.
    pub read_percent: Option<u64>,
    /// Requests prepared again to their recorded digests.
    pub digest_verified: u64,
    /// Legacy requests rebuilt, which nothing recorded can confirm.
    pub rebuilt: u64,
    /// Requests neither verified nor rebuilt.
    pub unverified: u64,
    /// Every class with at least one request, in a fixed order.
    pub classes: Vec<ClassTotal>,
}

/// The classes, in the order the summary lists them.
pub const CLASSES: [&str; 9] = [
    "first",
    "prefix_changed",
    "hit",
    "provider_miss_expired",
    "provider_miss_unexplained",
    "provider_miss_idle_unknown",
    "stable_unmeasured",
    "not_sent",
    "unverified",
];

/// The class a row is counted under.
#[must_use]
pub const fn class_of(attribution: &Attribution) -> &'static str {
    match attribution {
        Attribution::First => "first",
        Attribution::PrefixChanged { .. } => "prefix_changed",
        Attribution::Hit { .. } => "hit",
        Attribution::ProviderMiss {
            expired: Some(true),
            ..
        } => "provider_miss_expired",
        Attribution::ProviderMiss {
            expired: Some(false),
            ..
        } => "provider_miss_unexplained",
        Attribution::ProviderMiss { expired: None, .. } => "provider_miss_idle_unknown",
        Attribution::StableUnmeasured { .. } => "stable_unmeasured",
        Attribution::NotSent => "not_sent",
        Attribution::Unverified { .. } => "unverified",
    }
}

/// Sums the rows.
pub(super) fn summarise(rows: &[AuditedRequest]) -> AuditSummary {
    let count = |keep: &dyn Fn(&AuditedRequest) -> bool| {
        u64::try_from(rows.iter().filter(|row| keep(row)).count()).unwrap_or(u64::MAX)
    };
    let both: Vec<(u64, u64)> = rows.iter().filter_map(reported_pair).collect();
    let (both_prompt, both_read) = both.iter().fold((0_u64, 0_u64), |(prompt, read), pair| {
        (prompt.saturating_add(pair.0), read.saturating_add(pair.1))
    });
    let classes = CLASSES
        .iter()
        .map(|class| class_total(class, rows))
        .filter(|total| total.requests > 0)
        .collect();
    AuditSummary {
        requests: u64::try_from(rows.len()).unwrap_or(u64::MAX),
        prompt_tokens: rows
            .iter()
            .filter_map(|row| row.prompt_tokens)
            .fold(0, u64::saturating_add),
        cache_read_tokens: rows
            .iter()
            .filter_map(|row| row.cache_read.tokens())
            .fold(0, u64::saturating_add),
        read_percent: both_read
            .checked_mul(100)
            .and_then(|scaled| scaled.checked_div(both_prompt)),
        digest_verified: count(&|row| matches!(row.verification, Verification::Digest { .. })),
        rebuilt: count(&|row| matches!(row.verification, Verification::Rebuilt)),
        unverified: count(&|row| matches!(row.verification, Verification::Unverified { .. })),
        classes,
    }
}

/// A row's prompt and read, when it reported both.
fn reported_pair(row: &AuditedRequest) -> Option<(u64, u64)> {
    row.prompt_tokens.zip(row.cache_read.tokens())
}

/// One class's totals over `rows`.
fn class_total(class: &str, rows: &[AuditedRequest]) -> ClassTotal {
    let members: Vec<&AuditedRequest> = rows
        .iter()
        .filter(|row| class_of(&row.attribution) == class)
        .collect();
    let unread = members
        .iter()
        .filter_map(|row| reported_pair(row))
        .fold(0_u64, |total, (prompt, read)| {
            total.saturating_add(prompt.saturating_sub(read))
        });
    ClassTotal {
        class: class.to_owned(),
        requests: u64::try_from(members.len()).unwrap_or(u64::MAX),
        prompt_tokens: members
            .iter()
            .filter_map(|row| row.prompt_tokens)
            .fold(0, u64::saturating_add),
        cache_read_tokens: members
            .iter()
            .filter_map(|row| row.cache_read.tokens())
            .fold(0, u64::saturating_add),
        unread_tokens: unread,
    }
}
