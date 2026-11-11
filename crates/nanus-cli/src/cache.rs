//! `nanus sessions cache`: a recorded session's prompt-cache audit, as a table or as JSON.
//!
//! The audit itself is `nanus_bundle::cache_audit`'s; this is only how it is written down. The
//! table is for a person and the JSON for a script, and both are stable: the table's columns and
//! wording are documented in `docs/context-management.md`, and the JSON is the audit's own
//! serialisation, so a field is never renamed for one reader and not the other.

use nanus_bundle::cache_audit::{
    Attribution, AuditSummary, AuditedRequest, Basis, CacheAudit, CacheRead, ClassTotal,
    RequestKind, Verification,
};

/// Renders the audit as the JSON object a script reads.
///
/// A value that cannot encode would be a bug in the audit's types rather than bad input, so the
/// fallback is an empty object rather than a panic in a reporting command.
pub fn render_json(audit: &CacheAudit) -> String {
    serde_json::to_string_pretty(audit).unwrap_or_else(|_| String::from("{}"))
}

/// Renders the audit as the table and summary a person reads.
pub fn render_text(audit: &CacheAudit) -> String {
    let mut lines = vec![
        format!("cache audit of session {}", audit.session),
        format!(
            "  {:>4}  {:<9} {:<8} {:<20} {:>9} {:>14} {:>9} {:>4}  {:<8} attribution",
            "#", "turn.step", "kind", "model", "prompt", "cache read", "idle", "rev", "verified"
        ),
    ];
    for (index, request) in audit.requests.iter().enumerate() {
        lines.push(row(index.saturating_add(1), request));
    }
    lines.push(String::new());
    lines.extend(summary(&audit.summary));
    lines.join("\n") + "\n"
}

/// One request's row.
fn row(number: usize, request: &AuditedRequest) -> String {
    let kind = match request.kind {
        RequestKind::Managed => "managed",
        RequestKind::Legacy => "legacy",
        RequestKind::Handoff => "handoff",
    };
    let verified = match request.verification {
        Verification::Digest { .. } => "digest",
        Verification::Rebuilt => "rebuilt",
        Verification::Unverified { .. } => "no",
    };
    format!(
        "  {number:>4}  {:<9} {kind:<8} {:<20} {:>9} {:>14} {:>9} {:>4}  {verified:<8} {}",
        format!("{}.{}", request.turn, request.step),
        request.model.as_deref().unwrap_or("-"),
        request
            .prompt_tokens
            .map_or_else(|| String::from("-"), |tokens| tokens.to_string()),
        cache_read(request.cache_read),
        request.idle_ms.map_or_else(|| String::from("-"), seconds),
        request
            .revision
            .map_or_else(|| String::from("-"), |revision| revision.to_string()),
        attribution(request),
    )
}

/// The cache read as recorded: a count, or which of the absences it is.
fn cache_read(read: CacheRead) -> String {
    match read {
        CacheRead::Reported { tokens } => tokens.to_string(),
        CacheRead::NotReported => String::from("not reported"),
        CacheRead::ZeroOrNotReported => String::from("0 or none"),
        CacheRead::NoUsage => String::from("-"),
    }
}

/// Milliseconds as seconds to a tenth.
fn seconds(ms: u64) -> String {
    let whole = ms.checked_div(1_000).unwrap_or(0);
    let tenth = ms
        .checked_rem(1_000)
        .and_then(|rest| rest.checked_div(100))
        .unwrap_or(0);
    format!("{whole}.{tenth}s")
}

/// The attribution, in the words the guide documents.
fn attribution(request: &AuditedRequest) -> String {
    let rebuilt = |basis: Basis, text: String| match basis {
        Basis::Digest => text,
        Basis::Rebuilt => format!("{text} [rebuilt, not digest-verified]"),
    };
    match &request.attribution {
        Attribution::First => String::from("first"),
        Attribution::PrefixChanged {
            offset,
            previous_bytes,
            causes,
            basis,
        } => {
            let cause = if causes.is_empty() {
                String::from("no recorded cause")
            } else {
                causes.join("; ")
            };
            rebuilt(
                *basis,
                format!("prefix changed at {offset} of {previous_bytes} bytes ({cause})"),
            )
        }
        Attribution::Hit { basis } => rebuilt(*basis, String::from("prefix stable, hit")),
        Attribution::ProviderMiss { expired, basis } => {
            rebuilt(*basis, provider_miss(*expired, request))
        }
        Attribution::StableUnmeasured { basis } => rebuilt(
            *basis,
            String::from("prefix stable, read not comparable with the previous prompt"),
        ),
        Attribution::NotSent => String::from("not sent: refused before dispatch"),
        Attribution::Unverified { reason } => format!("unverified: {reason}"),
    }
}

/// A provider's short read, with whether the idle gap explains it.
fn provider_miss(expired: Option<bool>, request: &AuditedRequest) -> String {
    let lifetime = request
        .lifetime_ms
        .map_or_else(|| String::from("?"), seconds);
    let idle = request.idle_ms.map_or_else(|| String::from("?"), seconds);
    match expired {
        Some(true) => format!(
            "prefix stable, provider read less: cache expired (idle {idle} > lifetime {lifetime})"
        ),
        Some(false) => format!(
            "prefix stable, provider read less: unexplained (idle {idle} within lifetime \
             {lifetime})"
        ),
        None => String::from("prefix stable, provider read less: idle gap not recorded"),
    }
}

/// The class a total is under, by name.
fn class<'a>(summary: &'a AuditSummary, name: &str) -> Option<&'a ClassTotal> {
    summary.classes.iter().find(|total| total.class == name)
}

/// The summary's lines: counts, tokens, the one-line verdict and the per-class totals.
fn summary(summary: &AuditSummary) -> Vec<String> {
    let read_share = summary
        .read_percent
        .map_or_else(|| String::from("-"), |percent| format!("{percent}%"));
    let mut lines = vec![
        String::from("summary"),
        format!(
            "  requests    {} · {} verified by digest · {} rebuilt · {} unverified",
            summary.requests, summary.digest_verified, summary.rebuilt, summary.unverified
        ),
        format!(
            "  prompt      {} tokens · {} read from cache ({read_share} where both were reported)",
            summary.prompt_tokens, summary.cache_read_tokens
        ),
        format!("  verdict     {}", verdict(summary)),
        format!(
            "  {:<28} {:>8} {:>12} {:>12} {:>12}",
            "by class", "requests", "prompt", "read", "unread"
        ),
    ];
    lines.extend(summary.classes.iter().map(|total| {
        format!(
            "    {:<26} {:>8} {:>12} {:>12} {:>12}",
            total.class,
            total.requests,
            total.prompt_tokens,
            total.cache_read_tokens,
            total.unread_tokens
        )
    }));
    lines
}

/// "Was it us or them", in one line.
fn verdict(summary: &AuditSummary) -> String {
    let requests = |name: &str| class(summary, name).map_or(0, |total| total.requests);
    let unread = |name: &str| class(summary, name).map_or(0, |total| total.unread_tokens);
    let theirs = [
        "provider_miss_expired",
        "provider_miss_unexplained",
        "provider_miss_idle_unknown",
    ];
    let theirs_requests = theirs
        .iter()
        .map(|name| requests(name))
        .fold(0, u64::saturating_add);
    let theirs_unread = theirs
        .iter()
        .map(|name| unread(name))
        .fold(0, u64::saturating_add);
    let unattributed = ["stable_unmeasured", "unverified"]
        .iter()
        .map(|name| requests(name))
        .fold(0, u64::saturating_add);
    format!(
        "ours {} ({} tokens unread) · theirs {} ({} tokens unread; {} expired, {} unexplained) · \
         hits {} · not attributed {}",
        requests("prefix_changed"),
        unread("prefix_changed"),
        theirs_requests,
        theirs_unread,
        requests("provider_miss_expired"),
        requests("provider_miss_unexplained"),
        requests("hit"),
        unattributed
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use nanus_bundle::cache_audit::Basis;

    fn request(attribution: Attribution, read: CacheRead) -> AuditedRequest {
        AuditedRequest {
            turn: 0,
            step: 1,
            kind: RequestKind::Managed,
            model: Some(String::from("deepseek-flash")),
            prompt_tokens: Some(8_000),
            cache_read: read,
            idle_ms: Some(3_600_001),
            lifetime_ms: Some(3_600_000),
            revision: Some(2),
            verification: Verification::Digest {
                retention: String::from("short"),
                budget_hint: false,
            },
            attribution,
        }
    }

    #[test]
    fn a_reported_zero_and_an_unreported_read_print_differently() {
        assert_eq!(cache_read(CacheRead::Reported { tokens: 0 }), "0");
        assert_eq!(cache_read(CacheRead::NotReported), "not reported");
        assert_eq!(cache_read(CacheRead::ZeroOrNotReported), "0 or none");
        assert_eq!(cache_read(CacheRead::NoUsage), "-");
    }

    #[test]
    fn an_expiry_is_told_apart_from_an_unexplained_miss() {
        let expired = request(
            Attribution::ProviderMiss {
                expired: Some(true),
                basis: Basis::Digest,
            },
            CacheRead::Reported { tokens: 0 },
        );
        assert!(attribution(&expired).contains("cache expired (idle 3600.0s > lifetime 3600.0s)"));
        let unexplained = request(
            Attribution::ProviderMiss {
                expired: Some(false),
                basis: Basis::Digest,
            },
            CacheRead::Reported { tokens: 0 },
        );
        assert!(attribution(&unexplained).contains("unexplained"));
    }

    #[test]
    fn a_changed_prefix_names_its_offset_and_its_cause_or_says_it_has_none() {
        let changed = |causes: Vec<String>| {
            attribution(&request(
                Attribution::PrefixChanged {
                    offset: 120,
                    previous_bytes: 4_000,
                    causes,
                    basis: Basis::Digest,
                },
                CacheRead::Reported { tokens: 10 },
            ))
        };
        assert_eq!(
            changed(vec![String::from("a goal change moved the working data")]),
            "prefix changed at 120 of 4000 bytes (a goal change moved the working data)"
        );
        assert!(changed(Vec::new()).ends_with("(no recorded cause)"));
    }

    #[test]
    fn a_rebuilt_comparison_is_labelled_as_not_digest_verified() {
        let rebuilt = request(
            Attribution::Hit {
                basis: Basis::Rebuilt,
            },
            CacheRead::NoUsage,
        );
        assert_eq!(
            attribution(&rebuilt),
            "prefix stable, hit [rebuilt, not digest-verified]"
        );
        let proven = request(
            Attribution::Hit {
                basis: Basis::Digest,
            },
            CacheRead::NoUsage,
        );
        assert_eq!(attribution(&proven), "prefix stable, hit");
    }

    #[test]
    fn seconds_are_whole_to_a_tenth_and_never_rounded_up() {
        assert_eq!(seconds(0), "0.0s");
        assert_eq!(seconds(1_999), "1.9s");
        assert_eq!(seconds(3_600_000), "3600.0s");
    }
}
