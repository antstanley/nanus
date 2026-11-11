//! Deciding, for each request, whether its cache read was lost to nanus or to the provider.
//!
//! An attribution is claimed only between two requests the audit proved: both prepared again to
//! their recorded digests, or — labelled as such — rebuilt legacy requests. The previous request
//! sent is compared with this one by canonical encoding. Parted, the change is nanus's, and the
//! log between the two is searched for what changes a prefix. Not parted, the provider had the
//! previous request's prefix to read, and the read says whether it did.

use std::collections::BTreeMap;

use nanus_domain::context::managed::{
    Digest, FragmentId, ProjectionRevision, RevisionAuthor, RevisionReason,
};
use nanus_domain::{Session, SessionEvent};

use super::timeline::Observed;
use super::{
    Attribution, AuditedRequest, Basis, HIT_SHORTFALL_PERCENT, HIT_SHORTFALL_TOKENS, Verification,
};

/// Attributes every observed request, in order.
pub(super) fn attribute(session: &Session, observed: &[Observed]) -> Vec<AuditedRequest> {
    let causes = causes_by_index(session.log().events());
    let mut rows = Vec::with_capacity(observed.len());
    let mut previous: Option<&Observed> = None;
    for current in observed {
        let mut row = current.row.clone();
        row.idle_ms = previous.and_then(|previous| idle_ms(previous, current));
        row.attribution = decide(previous, current, &causes, row.idle_ms);
        rows.push(row);
        if current.sent {
            previous = Some(current);
        }
    }
    // Postcondition: one row per request, in the order they were observed.
    assert_eq!(rows.len(), observed.len(), "every request is attributed");
    rows
}

/// Milliseconds from the previous request's finish, or its start when it recorded none, to this
/// one's start. A clock set back reads as no time at all.
fn idle_ms(previous: &Observed, current: &Observed) -> Option<u64> {
    let (started, _) = current.times;
    let (previous_started, previous_finished) = previous.times;
    let since = previous_finished.or(previous_started)?;
    Some(started?.saturating_sub(since))
}

/// The attribution of `current`, given the previous request sent.
fn decide(
    previous: Option<&Observed>,
    current: &Observed,
    causes: &BTreeMap<usize, String>,
    idle: Option<u64>,
) -> Attribution {
    if !current.sent {
        return Attribution::NotSent;
    }
    if let Verification::Unverified { reason } = &current.row.verification {
        return Attribution::Unverified {
            reason: reason.clone(),
        };
    }
    let Some(previous) = previous else {
        return Attribution::First;
    };
    if matches!(previous.row.verification, Verification::Unverified { .. }) {
        return Attribution::Unverified {
            reason: String::from(
                "the request before it was not prepared again, so nothing proven precedes it",
            ),
        };
    }
    let (Some(earlier), Some(later)) = (&previous.canonical, &current.canonical) else {
        return Attribution::Unverified {
            reason: String::from("the adapter states no canonical encoding to compare"),
        };
    };
    let basis = if matches!(previous.row.verification, Verification::Digest { .. })
        && matches!(current.row.verification, Verification::Digest { .. })
    {
        Basis::Digest
    } else {
        Basis::Rebuilt
    };
    if let Some(offset) = earlier.divergence(later) {
        return Attribution::PrefixChanged {
            offset,
            previous_bytes: earlier.byte_len(),
            causes: causes_between(previous, current, causes),
            basis,
        };
    }
    match (current.row.cache_read.tokens(), previous.row.prompt_tokens) {
        (Some(read), Some(prompt)) if covers(read, prompt) => Attribution::Hit { basis },
        (Some(_), Some(_)) => Attribution::ProviderMiss {
            expired: idle
                .zip(current.row.lifetime_ms)
                .map(|(idle, lifetime)| idle > lifetime),
            basis,
        },
        _ => Attribution::StableUnmeasured { basis },
    }
}

/// Whether a read of `read` tokens covers about a previous prompt of `prompt` tokens.
fn covers(read: u64, prompt: u64) -> bool {
    let share = prompt
        .saturating_mul(HIT_SHORTFALL_PERCENT)
        .checked_div(100)
        .unwrap_or(0);
    prompt.saturating_sub(read) <= HIT_SHORTFALL_TOKENS.max(share)
}

/// What the log records between two requests that changes a prefix, and what differs between
/// their own routes.
fn causes_between(
    previous: &Observed,
    current: &Observed,
    causes: &BTreeMap<usize, String>,
) -> Vec<String> {
    let mut found: Vec<String> = causes
        .range(previous.anchor.saturating_add(1)..current.anchor)
        .map(|(_, cause)| cause.clone())
        .collect();
    if let (Some(earlier), Some(later)) = (&previous.selection, &current.selection) {
        if earlier.model != later.model {
            found.push(format!(
                "the model changed from {} to {}",
                earlier.model, later.model
            ));
        } else if earlier.effort != later.effort {
            found.push(String::from("the effort changed"));
        } else if earlier.provider != later.provider
            || earlier.endpoint_digest != later.endpoint_digest
            || earlier.protocol != later.protocol
        {
            found.push(String::from("the provider, endpoint or wire changed"));
        }
    } else if previous.selection.is_none() && current.selection.is_none() {
        legacy_causes(previous, current, &mut found);
    }
    found
}

/// What differs between two rebuilt legacy requests' own records.
fn legacy_causes(previous: &Observed, current: &Observed, found: &mut Vec<String>) {
    if previous.row.model != current.row.model {
        found.push(String::from("the model changed"));
    } else if previous.effort != current.effort {
        found.push(String::from("the effort changed"));
    }
    let (Some(before), Some(after)) = (previous.dropped_turns, current.dropped_turns) else {
        return;
    };
    if after > before {
        found.push(format!(
            "the legacy fitter left out {} more turns",
            after.saturating_sub(before)
        ));
    } else if after < before {
        found.push(String::from("the legacy fitter left out fewer turns"));
    }
}

/// What each event that changes a request's prefix changes, by its index in the log.
fn causes_by_index(events: &[SessionEvent]) -> BTreeMap<usize, String> {
    let mut causes = BTreeMap::new();
    let mut hidden: Vec<FragmentId> = Vec::new();
    let mut notes: Option<Digest> = None;
    let mut objective: Option<String> = None;
    for (index, event) in events.iter().enumerate() {
        let cause = match event {
            SessionEvent::ContextRevision { payload } => {
                let cause = revision_cause(&hidden, notes.as_ref(), payload);
                hidden.clone_from(&payload.hidden);
                notes = Some(payload.notes_digest.clone());
                cause
            }
            SessionEvent::GoalChange { goal } => {
                let after = goal.as_ref().map(|goal| goal.objective().to_owned());
                let moved = after != objective;
                objective = after;
                moved.then(|| String::from("a goal change moved the working data"))
            }
            SessionEvent::ContextMode { payload } => Some(format!(
                "the context policy changed (now {})",
                payload.policy.mode.as_str()
            )),
            SessionEvent::ContextSeed { .. } => Some(String::from("a handoff seed")),
            SessionEvent::ContextCompact { .. } => Some(String::from("a compaction cut")),
            _ => None,
        };
        if let Some(cause) = cause {
            causes.insert(index, cause);
        }
    }
    causes
}

/// What one accepted revision changes: fragments hidden or restored, or the notes.
fn revision_cause(
    hidden: &[FragmentId],
    notes: Option<&Digest>,
    revision: &ProjectionRevision,
) -> Option<String> {
    if revision.reason == RevisionReason::Reset {
        return Some(format!("a context reset (revision {})", revision.revision));
    }
    let who = match revision.author {
        RevisionAuthor::Model => "the model's proposal",
        RevisionAuthor::Automatic => "an automatic fit",
        RevisionAuthor::Host => "a host's revision",
    };
    let hid = revision
        .hidden
        .iter()
        .filter(|id| !hidden.contains(id))
        .count();
    let restored = hidden
        .iter()
        .filter(|id| !revision.hidden.contains(id))
        .count();
    let notes_changed = notes.map_or(!revision.notes.is_empty(), |notes| {
        *notes != revision.notes_digest
    });
    let mut parts = Vec::new();
    if hid > 0 {
        parts.push(format!("hid {}", fragments(hid)));
    }
    if restored > 0 {
        parts.push(format!("restored {}", fragments(restored)));
    }
    if notes_changed {
        parts.push(String::from("changed the notes, moving the working data"));
    }
    (!parts.is_empty()).then(|| {
        format!(
            "revision {}: {who} {}",
            revision.revision,
            parts.join(" and ")
        )
    })
}

/// `count` fragments, in words.
fn fragments(count: usize) -> String {
    if count == 1 {
        String::from("1 fragment")
    } else {
        format!("{count} fragments")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_read_within_the_slack_of_the_previous_prompt_covers_it() {
        assert!(covers(9_000, 10_000));
        assert!(covers(10_000, 10_000));
        assert!(covers(180_000, 200_000));
    }

    #[test]
    fn a_read_further_short_than_the_slack_does_not() {
        assert!(!covers(8_900, 10_000));
        assert!(!covers(0, 10_000));
        assert!(!covers(179_999, 200_000));
    }

    #[test]
    fn a_short_prompt_has_the_fixed_slack() {
        assert!(covers(0, 1_024));
        assert!(!covers(0, 1_025));
    }
}
