//! Fitting a managed session toward its working target once the provider's cache has lapsed.
//!
//! Every case runs a first turn that grows the session past a working target of 12,000 estimated
//! tokens and far under its 95,904-token allowance, moves the runner's clock, and runs a second
//! turn through the real runner with a scripted model. What is asserted is what reached the model
//! and what reached the log: a lapsed cache over the target hides the oldest eligible work before
//! the request is sent and records it as one automatic revision; a warm cache, a session under the
//! target, a session that never sent a managed request, and a runner with the switch off send
//! exactly what they would have sent before.

// A panic in a test *is* the assertion. The workspace denies the lint for production code.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

#[path = "managed_context/harness.rs"]
mod harness;

use std::cell::Cell;
use std::rc::Rc;

use harness::*;
use nanus_bundle::{AgentRunner, Silent};
use nanus_domain::context::managed::{
    AttemptPhase, ContextPolicy, FragmentId, ModeActor, RevisionAuthor, RevisionReason, fit,
    fragments, state,
};
use nanus_domain::{Session, SessionEvent};
use nanus_ports::{ChatRequest, PromptCacheRetention};

/// Five minutes and an hour: the lifetimes a stated retention has.
const SHORT_MS: u64 = 300_000;
const LONG_MS: u64 = 3_600_000;

/// The working target every case names unless it says otherwise: under the 14,571 estimated tokens
/// the first turn leaves, and three fifths of it, 7,200, reachable by hiding five of the seven
/// fragments nothing protects.
const TARGET: u32 = 12_000;

/// How one resumed session is set up.
#[derive(Clone, Copy)]
struct Setup {
    /// The idle time between the first turn's last request and the second turn.
    gap_ms: u64,
    budget: u32,
    target: u32,
    /// The size of the second turn's prompt, or zero for `resume`.
    prompt_bytes: usize,
    /// Whether the port states its cache lifetime for the retention asked.
    lifetimes: bool,
    retention: PromptCacheRetention,
    cold_fit: bool,
}

impl Setup {
    const fn after(gap_ms: u64) -> Self {
        Self {
            gap_ms,
            budget: 100_000,
            target: TARGET,
            prompt_bytes: 0,
            lifetimes: false,
            retention: PromptCacheRetention::None,
            cold_fit: true,
        }
    }

    /// A runner over `model` on `time`, as this setup describes.
    fn runner(self, model: &Model, time: &Time) -> AgentRunner {
        model.set_cache_lifetimes(self.lifetimes);
        let target = core::num::NonZeroU32::new(self.target).unwrap();
        runner_timed(model, self.budget, time)
            .with_context_working_target(target)
            .with_prompt_cache(self.retention)
            .with_context_cold_fit(self.cold_fit)
    }

    /// The second turn's prompt.
    fn prompt(self) -> String {
        if self.prompt_bytes == 0 {
            String::from("resume")
        } else {
            "x".repeat(self.prompt_bytes)
        }
    }
}

/// What the second turn did.
struct Resumed {
    /// Every request the second turn sent.
    sent: Vec<ChatRequest>,
    /// The hidden sets of the automatic revisions the second turn recorded.
    fits: Vec<Vec<FragmentId>>,
    /// The session as the first turn left it.
    before: Session,
    /// The session after the second turn.
    after: Session,
}

/// Eight echoes of 3,000 bytes and an answer: about 14,500 estimated tokens at its end.
fn first_turn(runner: &AgentRunner, model: &Model, session: &mut Session, disk: &Disk) {
    for index in 0..8 {
        model.push(call(&format!("c{index}"), "echo", r#"{"size": 3000}"#));
    }
    model.push(text("done"));
    let context = context();
    let outcome = block(runner.run_turn_with_runtime(
        session,
        "the long task",
        &mut Silent,
        host(disk, &context),
    ));
    assert!(outcome.outcome.is_ok(), "{:?}", outcome.outcome);
}

/// One small echo and an answer, after `prompt`.
fn second_turn(
    (runner, model): (&AgentRunner, &Model),
    session: &mut Session,
    disk: &Disk,
    prompt: &str,
) {
    // Named for where it lands, so a session resumed twice carries no call id twice.
    let id = format!("r{}", session.event_count());
    model.push(call(&id, "echo", r#"{"size": 10}"#));
    model.push(text("done again"));
    let context = context();
    let outcome =
        block(runner.run_turn_with_runtime(session, prompt, &mut Silent, host(disk, &context)));
    assert!(outcome.outcome.is_ok(), "{:?}", outcome.outcome);
}

/// The hidden sets of the automatic revisions in `session` from event `from`, in log order.
fn automatic_since(session: &Session, from: usize) -> Vec<Vec<FragmentId>> {
    session
        .log()
        .events()
        .iter()
        .skip(from)
        .filter_map(|event| match event {
            SessionEvent::ContextRevision { payload }
                if payload.author == RevisionAuthor::Automatic =>
            {
                assert_eq!(
                    payload.reason,
                    RevisionReason::HardFit,
                    "the recorded reason"
                );
                Some(payload.hidden.clone())
            }
            _ => None,
        })
        .collect()
}

/// Runs the first turn, waits `setup.gap_ms`, and runs the second, all in one process.
fn resume(setup: Setup) -> Resumed {
    let time: Time = Rc::new(Cell::new(EPOCH_MS));
    let model = <Model as ModelExt>::new(Vec::new());
    let runner = setup.runner(&model, &time);
    let (mut session, disk, _) = managed(&runner);
    first_turn(&runner, &model, &mut session, &disk);
    let before = session.clone();
    let first = model.sent().len();
    time.set(time.get().saturating_add(setup.gap_ms));
    second_turn((&runner, &model), &mut session, &disk, &setup.prompt());
    assert_eq!(&disk.last(), &session, "the disk holds the whole turn");
    Resumed {
        sent: model.sent().split_off(first),
        fits: automatic_since(&session, before.event_count()),
        before,
        after: session,
    }
}

/// The value of `key` in a request's tail notice line.
fn tail_value(request: &ChatRequest, key: &str) -> String {
    let tail = request.context_tail.as_deref().unwrap_or_default();
    let notice = tail.lines().next().unwrap_or_default();
    let prefix = format!("{key}=");
    notice
        .split(' ')
        .find_map(|field| field.strip_prefix(prefix.as_str()))
        .unwrap_or_else(|| panic!("{key} is in the tail: {tail}"))
        .to_owned()
}

fn calls_in(request: &ChatRequest) -> usize {
    request.messages.iter().map(|m| m.tool_calls().len()).sum()
}

/// The session's settled fragments that nothing protects, oldest first.
fn eligible(session: &Session) -> Vec<FragmentId> {
    let all = fragments::derive(session.log()).unwrap();
    let protected = state::protected(session.log(), &all);
    all.all()
        .iter()
        .filter(|fragment| fragment.settled && !protected.contains(&fragment.id))
        .map(|fragment| fragment.id)
        .collect()
}

/// After a gap longer than the provider keeps a prefix, a session over its working target has
/// its oldest eligible work hidden toward 60 % of the target before the request is sent: one
/// automatic revision, persisted with the intent, listed in the tail, every user message kept.
#[test]
fn a_cold_start_over_the_target_hides_the_oldest_work_before_the_request_is_sent() {
    let resumed = resume(Setup::after(LONG_MS + 1));
    assert_eq!(resumed.fits.len(), 1, "one fit, at the cold start only");
    let hidden = &resumed.fits[0];
    let eligible = eligible(&resumed.before);
    assert!(
        !hidden.is_empty() && hidden.len() < eligible.len(),
        "{hidden:?}"
    );
    assert_eq!(hidden.as_slice(), &eligible[..hidden.len()], "oldest first");

    let first = &resumed.sent[0];
    let estimate: u32 = tail_value(first, "estimate_input_tokens").parse().unwrap();
    assert!(
        estimate <= fit::target(TARGET),
        "fitted to the aim: {estimate}"
    );
    assert_eq!(
        tail_value(first, "hidden_fragments"),
        hidden.len().to_string()
    );
    let tail = first.context_tail.as_deref().unwrap();
    assert!(
        tail.contains(&format!(" {} ", hidden[0])),
        "the catalog lists it: {tail}"
    );
    assert_eq!(
        calls_in(first),
        8 - hidden.len(),
        "whole fragments left the request"
    );
    for prompt in ["the long task", "resume"] {
        assert!(
            first.messages.iter().any(|m| m.text() == Some(prompt)),
            "{prompt} survives the fit"
        );
    }
    // The revision is recorded ahead of the intent of the request it shaped, which is the commit
    // that carries both to disk before anything is sent.
    let events = resumed.after.log().events();
    let revision = events
        .iter()
        .position(|e| matches!(e, SessionEvent::ContextRevision { .. }))
        .unwrap();
    let intent = events
        .iter()
        .enumerate()
        .skip(resumed.before.event_count())
        .find_map(|(index, event)| match event {
            SessionEvent::RequestAttempt { payload } if payload.phase == AttemptPhase::Started => {
                Some((index, payload.projection_revision))
            }
            _ => None,
        })
        .unwrap();
    assert!(revision > resumed.before.event_count() && revision < intent.0);
    assert_eq!(
        intent.1, 1,
        "the request was built from the fitted revision"
    );
    // The step after it is warm, and the fit has left room: nothing more is hidden.
    assert_eq!(
        tail_value(&resumed.sent[1], "hidden_fragments"),
        hidden.len().to_string()
    );
}

/// The same session resumed while the cache is warm sends exactly what it sends with the switch
/// off and what it sends with no gap at all: no revision, and the whole conversation.
#[test]
fn a_warm_resume_sends_the_unfitted_request() {
    let warm = resume(Setup::after(LONG_MS));
    let none = resume(Setup::after(0));
    let off = resume(Setup {
        cold_fit: false,
        ..Setup::after(LONG_MS + 1)
    });
    assert!(warm.fits.is_empty() && none.fits.is_empty() && off.fits.is_empty());
    assert_eq!(warm.sent, none.sent, "a warm gap is no gap");
    assert_eq!(
        warm.sent, off.sent,
        "and is what a runner without cold fitting sends"
    );
    assert_eq!(calls_in(&warm.sent[0]), 8, "nothing was hidden");
    assert_eq!(
        tail_value(&warm.sent[0], "budget_hint"),
        "reduce",
        "only the hint asks"
    );
}

/// A cold start under the working target changes nothing: the cache is cold, but there is nothing
/// the target asks to be hidden.
#[test]
fn a_cold_start_under_the_target_changes_nothing() {
    let roomy = |cold_fit| {
        resume(Setup {
            target: 40_000,
            cold_fit,
            ..Setup::after(LONG_MS * 3)
        })
    };
    let (on, off) = (roomy(true), roomy(false));
    assert!(on.fits.is_empty());
    assert_eq!(on.sent, off.sent);
}

/// A session reloaded from disk reads its last request's time from its own records, so a cold
/// start after a reload fits exactly as one in the same process does.
#[test]
fn a_cold_start_after_a_reload_fits_as_one_in_process_does() {
    let setup = Setup::after(LONG_MS * 2);
    let in_process = resume(setup);
    assert_eq!(in_process.fits.len(), 1);

    let time: Time = Rc::new(Cell::new(EPOCH_MS));
    let first_model = <Model as ModelExt>::new(Vec::new());
    let first = setup.runner(&first_model, &time);
    let (mut session, disk, _) = managed(&first);
    first_turn(&first, &first_model, &mut session, &disk);
    drop(first);
    // A new process: a new runner and model, the session read back from what was written.
    let mut reloaded = Session::from_jsonl(&disk.last().try_to_jsonl().unwrap()).unwrap();
    assert_eq!(reloaded, session);
    let later: Time = Rc::new(Cell::new(EPOCH_MS + setup.gap_ms));
    let model = <Model as ModelExt>::new(Vec::new());
    let runner = setup.runner(&model, &later);
    second_turn((&runner, &model), &mut reloaded, &disk, "resume");
    assert_eq!(
        automatic_since(&reloaded, session.event_count()),
        in_process.fits
    );
    assert_eq!(model.sent(), in_process.sent);
}

/// A stated lifetime is the boundary, to the millisecond: five minutes under the short retention
/// and an hour under the long one.
#[test]
fn a_stated_lifetime_is_the_boundary_for_short_and_long_retention() {
    let fits = |retention, gap_ms| {
        resume(Setup {
            lifetimes: true,
            retention,
            ..Setup::after(gap_ms)
        })
        .fits
        .len()
    };
    assert_eq!(fits(PromptCacheRetention::Short, SHORT_MS), 0);
    assert_eq!(fits(PromptCacheRetention::Short, SHORT_MS + 1), 1);
    assert_eq!(fits(PromptCacheRetention::Long, SHORT_MS + 1), 0);
    assert_eq!(fits(PromptCacheRetention::Long, LONG_MS), 0);
    assert_eq!(fits(PromptCacheRetention::Long, LONG_MS + 1), 1);
}

/// A lifetime the adapter does not state is taken to be an hour, whatever the runner asked for:
/// an adapter that caches by its own rules, and a runner that asked for no entries, wait as long
/// as the longest retention before anything is hidden.
#[test]
fn an_unknown_lifetime_is_taken_to_be_the_longest() {
    let fits = |lifetimes, retention, gap_ms| {
        resume(Setup {
            lifetimes,
            retention,
            ..Setup::after(gap_ms)
        })
        .fits
        .len()
    };
    assert_eq!(fits(false, PromptCacheRetention::Short, SHORT_MS + 1), 0);
    assert_eq!(fits(false, PromptCacheRetention::Short, LONG_MS + 1), 1);
    assert_eq!(fits(true, PromptCacheRetention::None, SHORT_MS + 1), 0);
    assert_eq!(fits(true, PromptCacheRetention::None, LONG_MS + 1), 1);
}

/// A protected floor above the target is as far as a cold fit goes: every eligible fragment is
/// hidden and the request is sent over the target, never refused, because only the allowance
/// refuses.
#[test]
fn a_protected_floor_over_the_target_is_the_best_and_is_sent() {
    let resumed = resume(Setup {
        target: 1_000,
        ..Setup::after(LONG_MS + 1)
    });
    assert_eq!(resumed.fits, vec![eligible(&resumed.before)]);
    let estimate: u32 = tail_value(&resumed.sent[0], "estimate_input_tokens")
        .parse()
        .unwrap();
    assert!(estimate > 1_000, "the floor is over the target: {estimate}");
    assert_eq!(resumed.sent.len(), 2, "and the turn ran to its answer");
}

/// The first managed request of a session that has history but has never sent one is not cold in
/// any way that matters: nothing of its managed prefix was ever cached. The next cold start is.
#[test]
fn a_session_that_never_sent_a_managed_request_is_not_fitted() {
    let time: Time = Rc::new(Cell::new(EPOCH_MS));
    let model = <Model as ModelExt>::new(Vec::new());
    let runner = Setup::after(0).runner(&model, &time);
    let mut session = session();
    for index in 0..8 {
        model.push(call(&format!("c{index}"), "echo", r#"{"size": 3000}"#));
    }
    model.push(text("done"));
    block(runner.run_turn(&mut session, "the long task", &mut Silent, None)).unwrap();
    let disk = Disk::new();
    let context = context();
    let policy = ContextPolicy {
        output_reserve_follows_model: false,
        ..ContextPolicy::managed()
    };
    block(runner.set_context_policy(
        &mut session,
        policy,
        ModeActor::Human,
        host(&disk, &context).runtime,
    ))
    .unwrap();
    let legacy = model.sent().len();
    time.set(EPOCH_MS + LONG_MS * 2);
    second_turn((&runner, &model), &mut session, &disk, "resume");
    assert!(
        automatic_since(&session, 0).is_empty(),
        "no attempt, so no fit"
    );
    assert_eq!(calls_in(&model.sent()[legacy]), 8);

    time.set(time.get() + LONG_MS * 2);
    second_turn((&runner, &model), &mut session, &disk, "and again");
    assert_eq!(
        automatic_since(&session, 0).len(),
        1,
        "a recorded request starts the clock"
    );
}

/// Over the allowance, a warm cache is hard fitted exactly as before — the same revision and the
/// same requests with the switch on or off — and a lapsed one is fitted toward the target instead
/// of three fifths of the allowance, hiding more.
#[test]
fn hard_fitting_is_unchanged_while_warm_and_aims_at_the_target_once_cold() {
    // An allowance of 15,504: over the first turn's largest request and under the second's first,
    // which its 8,000-byte prompt takes to about 16,600.
    let over = |gap_ms, cold_fit| {
        resume(Setup {
            budget: 19_600,
            target: 2_000,
            prompt_bytes: 8_000,
            lifetimes: true,
            retention: PromptCacheRetention::Short,
            cold_fit,
            ..Setup::after(gap_ms)
        })
    };
    let warm = over(SHORT_MS, true);
    let off = over(SHORT_MS + 1, false);
    assert_eq!(
        warm.fits.len(),
        1,
        "the allowance is passed, so hard fitting acts"
    );
    assert_eq!(warm.fits, off.fits);
    assert_eq!(warm.sent, off.sent);
    let cold = over(SHORT_MS + 1, true);
    assert_eq!(cold.fits.len(), 1);
    assert!(
        cold.fits[0].len() > warm.fits[0].len(),
        "{:?} against {:?}",
        cold.fits,
        warm.fits
    );
}
