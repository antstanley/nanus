//! Walking a session's log into the requests it sent, each prepared again where it can be.
//!
//! A managed step's intent is recorded in the checkpoint that also carries any automatic revision
//! the step's fit made, and its preparation saw the log as it stood before both. A legacy step's
//! request was built from the log as it stood before its `StepStart`. So the walk grows one copy
//! of the session event by event, and prepares each request against the copy at the length its
//! preparation saw — the work a live run did once per step, done once more.

use std::collections::BTreeMap;

use nanus_domain::context::managed::{
    AttemptOutcome, AttemptPhase, RequestAttemptRecord, RevisionAuthor, SelectionIdentity,
    UsageObservation,
};
use nanus_domain::{Session, SessionEvent, Usage};
use nanus_ports::CanonicalEncoding;

use super::verify::{Managed, Verifier};
use super::{Attribution, AuditedRequest, CacheRead, RequestKind, Verification};

/// Where a request's row comes from in the log.
enum Source {
    /// A managed step's intent, prepared from the log's first `at` events.
    Managed {
        attempt: usize,
        at: usize,
        automatic: Option<usize>,
    },
    /// A handoff draft's intent.
    Handoff { attempt: usize },
    /// A legacy step, built from the log before its `StepStart`.
    Legacy { step_start: usize },
}

impl Source {
    /// How many events the request's preparation saw.
    const fn at(&self) -> usize {
        match self {
            Self::Managed { at, .. } => *at,
            Self::Handoff { attempt } => *attempt,
            Self::Legacy { step_start } => *step_start,
        }
    }
}

/// A request as the log and its re-derivation describe it, before it is attributed.
pub(super) struct Observed {
    /// The row, its attribution not yet decided.
    pub(super) row: AuditedRequest,
    /// The index of the request's own record: its intent, or its `StepStart`.
    pub(super) anchor: usize,
    /// The canonical encoding of the request, when it was prepared again or rebuilt.
    pub(super) canonical: Option<CanonicalEncoding>,
    /// The recorded route, for a managed request.
    pub(super) selection: Option<SelectionIdentity>,
    /// The effort a legacy request's reply recorded.
    pub(super) effort: Option<String>,
    /// How many turns a rebuilt legacy request left out.
    pub(super) dropped_turns: Option<u32>,
    /// When the request started and finished, as its attempt recorded.
    pub(super) times: (Option<u64>, Option<u64>),
    /// Whether the request was dispatched at all.
    pub(super) sent: bool,
}

/// Every request `session` recorded, in log order, each prepared again where it can be.
pub(super) fn observe(session: &Session, verifier: &mut Verifier<'_>) -> Vec<Observed> {
    let events = session.log().events();
    let Ok(mut before) = session.prefix(0) else {
        return Vec::new();
    };
    let finished = finished_records(events);
    let mut walked = 0_usize;
    let mut rows = Vec::new();
    for source in sources(events) {
        while walked < source.at() {
            if let Some(event) = events.get(walked) {
                before.append(event.clone());
            }
            walked = walked.saturating_add(1);
        }
        // Precondition of every preparation: the copy is the log as that preparation saw it.
        assert_eq!(
            before.event_count(),
            source.at(),
            "the walk is at the source"
        );
        let row = match source {
            Source::Managed {
                attempt, automatic, ..
            } => managed_row(verifier, &before, (events, &finished), attempt, automatic),
            Source::Handoff { attempt } => handoff_row(events, &finished, attempt),
            Source::Legacy { step_start } => legacy_row(verifier, &before, events, step_start),
        };
        rows.extend(row);
    }
    rows
}

/// Every request's source, in the order their preparations saw the log.
fn sources(events: &[SessionEvent]) -> Vec<Source> {
    let mut sources = Vec::new();
    for (index, event) in events.iter().enumerate() {
        match event {
            SessionEvent::RequestAttempt { payload } if payload.phase == AttemptPhase::Started => {
                if payload.kind.is_some() {
                    sources.push(Source::Handoff { attempt: index });
                    continue;
                }
                let automatic = automatic_before(events, index);
                let at = automatic.unwrap_or(index);
                sources.push(Source::Managed {
                    attempt: index,
                    at,
                    automatic,
                });
            }
            SessionEvent::StepStart { turn, step }
                if !managed_step(events, index, *turn, *step) =>
            {
                sources.push(Source::Legacy { step_start: index });
            }
            _ => {}
        }
    }
    sources
}

/// The automatic revision recorded with the intent at `index`, when there is one: the revision
/// and its accepted decision directly before it, as the intent's checkpoint writes them.
fn automatic_before(events: &[SessionEvent], index: usize) -> Option<usize> {
    let at = index.checked_sub(2)?;
    let (
        Some(SessionEvent::ContextRevision { payload: revision }),
        Some(SessionEvent::ContextDecision { payload: decision }),
    ) = (events.get(at), events.get(at.saturating_add(1)))
    else {
        return None;
    };
    (revision.author == RevisionAuthor::Automatic && decision.decision_id == revision.decision_id)
        .then_some(at)
}

/// Whether the `StepStart` at `index` opens a managed step: its intent is directly before it.
fn managed_step(events: &[SessionEvent], index: usize, turn: u32, step: u32) -> bool {
    let Some(previous) = index.checked_sub(1).and_then(|at| events.get(at)) else {
        return false;
    };
    matches!(
        previous,
        SessionEvent::RequestAttempt { payload }
            if payload.phase == AttemptPhase::Started
                && payload.turn == u64::from(turn)
                && payload.step == u64::from(step)
    )
}

/// Every finished attempt record, by attempt id.
fn finished_records(events: &[SessionEvent]) -> BTreeMap<&str, &RequestAttemptRecord> {
    events
        .iter()
        .filter_map(|event| match event {
            SessionEvent::RequestAttempt { payload } if payload.phase == AttemptPhase::Finished => {
                Some((payload.attempt_id.as_str(), &**payload))
            }
            _ => None,
        })
        .collect()
}

/// A managed step's row.
fn managed_row(
    verifier: &mut Verifier<'_>,
    before: &Session,
    (events, finished): (&[SessionEvent], &BTreeMap<&str, &RequestAttemptRecord>),
    attempt: usize,
    automatic: Option<usize>,
) -> Option<Observed> {
    let Some(SessionEvent::RequestAttempt { payload: started }) = events.get(attempt) else {
        return None;
    };
    let done = finished.get(started.attempt_id.as_str()).copied();
    let (prompt_tokens, cache_read) = attempt_usage(done, events);
    let sent = done.and_then(|record| record.outcome) != Some(AttemptOutcome::Refused);
    let revision = automatic.and_then(|at| match events.get(at) {
        Some(SessionEvent::ContextRevision { payload }) => Some(&**payload),
        _ => None,
    });
    let (verification, canonical, lifetime_ms) = if sent {
        verified(verifier.managed(before, started, revision))
    } else {
        let reason = String::from("refused before dispatch, so never sent");
        (Verification::Unverified { reason }, None, None)
    };
    Some(Observed {
        row: AuditedRequest {
            turn: started.turn,
            step: started.step,
            kind: RequestKind::Managed,
            model: Some(started.selection.model.clone()),
            prompt_tokens,
            cache_read,
            idle_ms: None,
            lifetime_ms,
            revision: Some(started.projection_revision),
            attribution: placeholder(&verification),
            verification,
        },
        anchor: attempt,
        canonical,
        selection: Some(started.selection.clone()),
        effort: None,
        dropped_turns: None,
        times: (
            Some(started.started_at_ms),
            done.and_then(|record| record.finished_at_ms),
        ),
        sent,
    })
}

/// The verification a managed preparation came to, its canonical encoding, and its lifetime.
fn verified(outcome: Managed) -> (Verification, Option<CanonicalEncoding>, Option<u64>) {
    match outcome {
        Managed::Verified {
            rederived,
            retention,
            lifetime_ms,
        } => (
            Verification::Digest {
                retention: String::from(retention_name(retention)),
                budget_hint: rederived.budget_hint,
            },
            rederived.canonical,
            Some(lifetime_ms),
        ),
        Managed::Unverified(reason) => (Verification::Unverified { reason }, None, None),
    }
}

/// The name a retention is reported under.
const fn retention_name(retention: nanus_ports::PromptCacheRetention) -> &'static str {
    match retention {
        nanus_ports::PromptCacheRetention::None => "none",
        nanus_ports::PromptCacheRetention::Short => "short",
        nanus_ports::PromptCacheRetention::Long => "long",
    }
}

/// A handoff draft's row: recorded, and not prepared again.
fn handoff_row(
    events: &[SessionEvent],
    finished: &BTreeMap<&str, &RequestAttemptRecord>,
    attempt: usize,
) -> Option<Observed> {
    let Some(SessionEvent::RequestAttempt { payload: started }) = events.get(attempt) else {
        return None;
    };
    let done = finished.get(started.attempt_id.as_str()).copied();
    let (prompt_tokens, cache_read) = attempt_usage(done, events);
    let verification = Verification::Unverified {
        reason: String::from("a handoff draft, which this audit does not prepare again"),
    };
    Some(Observed {
        row: AuditedRequest {
            turn: started.turn,
            step: started.step,
            kind: RequestKind::Handoff,
            model: Some(started.selection.model.clone()),
            prompt_tokens,
            cache_read,
            idle_ms: None,
            lifetime_ms: None,
            revision: Some(started.projection_revision),
            attribution: placeholder(&verification),
            verification,
        },
        anchor: attempt,
        canonical: None,
        selection: Some(started.selection.clone()),
        effort: None,
        dropped_turns: None,
        times: (
            Some(started.started_at_ms),
            done.and_then(|record| record.finished_at_ms),
        ),
        sent: done.and_then(|record| record.outcome) != Some(AttemptOutcome::Refused),
    })
}

/// A legacy step's row, its request rebuilt through the runner's own fitter.
fn legacy_row(
    verifier: &mut Verifier<'_>,
    before: &Session,
    events: &[SessionEvent],
    step_start: usize,
) -> Option<Observed> {
    let Some(SessionEvent::StepStart { turn, step }) = events.get(step_start) else {
        return None;
    };
    let reply = reply_after(events, step_start);
    let (model, effort, usage) = reply.map_or((None, None, None), |(model, effort, usage)| {
        (model.cloned(), effort.cloned(), usage)
    });
    let (prompt_tokens, cache_read) = usage.map_or((None, CacheRead::NoUsage), neutral_usage);
    let (verification, canonical, dropped_turns) =
        match verifier.legacy(before, model.as_deref(), effort.as_deref()) {
            Ok(rebuilt) => (
                Verification::Rebuilt,
                rebuilt.canonical,
                Some(rebuilt.dropped_turns),
            ),
            Err(reason) => (Verification::Unverified { reason }, None, None),
        };
    Some(Observed {
        row: AuditedRequest {
            turn: u64::from(*turn),
            step: u64::from(*step),
            kind: RequestKind::Legacy,
            model,
            prompt_tokens,
            cache_read,
            idle_ms: None,
            lifetime_ms: None,
            revision: None,
            attribution: placeholder(&verification),
            verification,
        },
        anchor: step_start,
        canonical,
        selection: None,
        effort,
        dropped_turns,
        times: (None, None),
        sent: true,
    })
}

/// What a reply records about the request it answered: its model, its effort and its usage.
type Reply<'a> = (Option<&'a String>, Option<&'a String>, Option<&'a Usage>);

/// The model, effort and usage the reply to the step at `step_start` recorded, when it has one.
fn reply_after(events: &[SessionEvent], step_start: usize) -> Option<Reply<'_>> {
    events
        .iter()
        .skip(step_start.saturating_add(1))
        .take_while(|event| {
            !matches!(
                event,
                SessionEvent::StepStart { .. }
                    | SessionEvent::StepEnd { .. }
                    | SessionEvent::TurnEnd { .. }
            )
        })
        .find_map(|event| match event {
            SessionEvent::AssistantMessage {
                model,
                effort,
                usage,
                ..
            } => Some((model.as_ref(), effort.as_ref(), usage.as_ref())),
            _ => None,
        })
}

/// The prompt tokens and cache read an attempt recorded: its own usage, or its reply's.
fn attempt_usage(
    done: Option<&RequestAttemptRecord>,
    events: &[SessionEvent],
) -> (Option<u64>, CacheRead) {
    if let Some(usage) = done.and_then(|record| record.usage.as_ref()) {
        return observed_usage(usage);
    }
    let reply = done
        .and_then(|record| record.assistant_seq)
        .and_then(|seq| usize::try_from(seq).ok())
        .and_then(|at| events.get(at));
    match reply {
        Some(SessionEvent::AssistantMessage {
            usage: Some(usage), ..
        }) => neutral_usage(usage),
        _ => (None, CacheRead::NoUsage),
    }
}

/// An attempt's recorded usage, read under the semantics it was written with.
fn observed_usage(usage: &UsageObservation) -> (Option<u64>, CacheRead) {
    let current = usage.semantics_version == UsageObservation::NEUTRAL_SEMANTICS;
    let read = match (usage.cache_read_tokens, current) {
        (Some(tokens), _) => CacheRead::Reported { tokens },
        (None, true) => CacheRead::NotReported,
        // Under the earlier label a null is a reported zero or no report, and nothing says which.
        (None, false) => CacheRead::ZeroOrNotReported,
    };
    (usage.prompt_tokens, read)
}

/// A reply's recorded usage, whose shape always writes the cache read: a zero there is a zero or
/// no report, and only a positive count is certainly a report.
fn neutral_usage(usage: &Usage) -> (Option<u64>, CacheRead) {
    let read = if usage.cache_hit_tokens > 0 {
        CacheRead::Reported {
            tokens: u64::from(usage.cache_hit_tokens),
        }
    } else {
        CacheRead::ZeroOrNotReported
    };
    (Some(u64::from(usage.prompt_tokens)), read)
}

/// The attribution a row carries until it is decided: its verification's reason, if it has one.
fn placeholder(verification: &Verification) -> Attribution {
    let reason = match verification {
        Verification::Unverified { reason } => reason.clone(),
        _ => String::from("not yet attributed"),
    };
    Attribution::Unverified { reason }
}
