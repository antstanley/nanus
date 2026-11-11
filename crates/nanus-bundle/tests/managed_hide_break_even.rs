//! Refusing a model's hide that cannot pay for the cache rewrite it forces.
//!
//! Every case runs one turn through the real runner with a scripted model: a small echo, four
//! large ones, two small ones the recent protection keeps, and an inspect, followed by the
//! proposal under test. Hiding the small first fragment saves a few dozen estimated tokens and
//! forces the provider to re-read everything after it, thousands, so it is refused before it is
//! staged. The same hide is staged under pressure, once the cache has lapsed, and with the rule off;
//! a hide of the whole resolved stretch, a late hide, a restore and a note change are staged with
//! the rule on; and the checks that refused a proposal before still answer first.

// A panic in a test *is* the assertion. The workspace denies the lint for production code.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

#[path = "managed_context/harness.rs"]
mod harness;

use std::cell::Cell;
use std::rc::Rc;

use harness::*;
use nanus_bundle::{AgentRunner, Silent};
use nanus_domain::context::managed::{DecisionOutcome, ManagedState, RevisionAuthor, break_even};
use nanus_domain::{Session, SessionEvent};
use nanus_ports::{ChatRequest, LlmEvent};
use serde_json::{Value, json};

/// The lifetime taken for a cache whose adapter states none: an hour.
const LONG_MS: u64 = 3_600_000;

/// How one turn's runner is set up.
#[derive(Clone, Copy)]
struct Setup {
    /// A working target, or `None` for the default, clamped to the 95,904-token allowance.
    target: Option<u32>,
    /// Whether the break-even rule is on.
    rule: bool,
    /// Whether a lapsed cache is fitted toward the target.
    cold_fit: bool,
    /// Milliseconds the clock moves while the proposal's step is answered.
    stall_ms: u64,
}

impl Setup {
    const fn stock() -> Self {
        Self {
            target: None,
            rule: true,
            cold_fit: true,
            stall_ms: 0,
        }
    }

    fn runner(self, model: &Model, time: &Time) -> AgentRunner {
        let runner = runner_timed(model, 100_000, time)
            .with_context_hide_break_even(self.rule)
            .with_context_cold_fit(self.cold_fit);
        match self.target.and_then(core::num::NonZeroU32::new) {
            Some(target) => runner.with_context_working_target(target),
            None => runner,
        }
    }
}

/// One scripted step, computed from the request that asks for it.
type Script = Box<dyn Fn(&ChatRequest) -> Vec<LlmEvent>>;

/// What the turn left.
struct Ran {
    session: Session,
    sent: Vec<ChatRequest>,
}

/// Seven worked fragments and an inspect, as the module describes.
fn work(model: &Model) {
    model.push(call("c0", "echo", r#"{"size": 10}"#));
    for index in 1..=4 {
        model.push(call(&format!("c{index}"), "echo", r#"{"size": 3000}"#));
    }
    model.push(call("c5", "echo", r#"{"size": 10}"#));
    model.push(call("c6", "echo", r#"{"size": 10}"#));
    model.push(call("i1", "context_manage", INSPECT));
}

/// Runs the work, then each of `then` as one step, then an answer, under `setup`.
fn run(setup: Setup, then: Vec<Script>) -> Ran {
    let time: Time = Rc::new(Cell::new(EPOCH_MS));
    let model = <Model as ModelExt>::new(Vec::new());
    let runner = setup.runner(&model, &time);
    let (mut session, disk, context) = managed(&runner);
    work(&model);
    let count = then.len();
    for (index, step) in then.into_iter().enumerate() {
        let clock = Rc::clone(&time);
        let last = index.saturating_add(1) == count;
        model.push_with(move |request| {
            // The proposal's step is the last one scripted; a stall there leaves the cache of the
            // request that carried it lapsed by the time the proposal is validated.
            if last {
                clock.set(clock.get().saturating_add(setup.stall_ms));
            }
            step(request)
        });
    }
    model.push(text("done"));
    let outcome = block(runner.run_turn_with_runtime(
        &mut session,
        "work, then tidy up",
        &mut Silent,
        host(&disk, &context),
    ));
    assert!(outcome.outcome.is_ok(), "{:?}", outcome.outcome);
    assert_eq!(&disk.last(), &session, "the disk holds the whole turn");
    Ran {
        session,
        sent: model.sent(),
    }
}

/// A proposal from the latest inspect result: `hide` and `restore` chosen from its eligible ids
/// (oldest first) and its hidden ids, with `notes`.
fn propose(
    id: &'static str,
    hide: fn(&[String]) -> Vec<String>,
    restore: fn(&[String]) -> Vec<String>,
    notes: fn(&Value) -> Vec<Value>,
) -> Script {
    Box::new(move |request| {
        let inspected = last_tool_json(request);
        let hidden: Vec<String> = inspected["fragments"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|fragment| fragment["hidden"] == true)
            .map(|fragment| fragment["id"].as_str().unwrap().to_owned())
            .collect();
        let context = &inspected["context"];
        let arguments = json!({
            "action": "propose",
            "base_revision": context["revision"],
            "base_frontier": context["frontier"],
            "hide": hide(&eligible(&inspected)),
            "restore": restore(&hidden),
            "notes": notes(&inspected),
            "cursor": null,
            "base_profile_digest": context["profile_digest"],
        });
        call(id, "context_manage", &arguments.to_string())
    })
}

fn none(_: &[String]) -> Vec<String> {
    Vec::new()
}

fn oldest(ids: &[String]) -> Vec<String> {
    assert_eq!(ids.len(), 5, "c0 to c4 are eligible: {ids:?}");
    ids[..1].to_vec()
}

fn newest(ids: &[String]) -> Vec<String> {
    assert_eq!(ids.len(), 5, "c0 to c4 are eligible: {ids:?}");
    ids[4..].to_vec()
}

fn every(ids: &[String]) -> Vec<String> {
    ids.to_vec()
}

fn no_notes(_: &Value) -> Vec<Value> {
    Vec::new()
}

/// One note citing the first fragment's result, the ten bytes of filler `c0` returned.
fn a_note(inspected: &Value) -> Vec<Value> {
    let first = inspected["fragments"][0]["id"].as_str().unwrap();
    let seq: u64 = first.trim_start_matches("f:").parse().unwrap();
    vec![
        json!({"id": "n:echo", "claim": "echo returns filler", "category": "observed",
        "sources": [{"kind": "event", "event_seq": seq.saturating_add(2), "block_index": null,
        "artifact_id": null, "offset": 0, "length": 10,
        "source_digest": nanus_domain::context::managed::Digest::of(b"xxxxxxxxxx\n").as_str(),
        "field": "tool_text"}]}),
    ]
}

/// The proposal that hides the small first fragment and nothing else.
fn small_early_hide() -> Script {
    propose("p", oldest, none, no_notes)
}

/// Whether the proposal `id` was staged and then accepted.
fn accepted(ran: &Ran, id: &str) -> bool {
    let staged = tool_result_json(&ran.session, id)["status"] == "staged";
    let decided = ran.session.log().events().iter().any(|event| {
        matches!(event, SessionEvent::ContextDecision { payload }
            if payload.decision_id.starts_with("p-")
                && payload.outcome == DecisionOutcome::Accepted)
    });
    staged && decided
}

/// The whole-number value after `before` in `text`.
fn number_after(text: &str, before: &str) -> u64 {
    let (_, after) = text
        .split_once(before)
        .unwrap_or_else(|| panic!("{before} in {text}"));
    after
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .unwrap()
}

/// The value of `key` in a request's tail notice line.
fn tail_value(request: &ChatRequest, key: &str) -> String {
    let notice = request
        .context_tail
        .as_deref()
        .unwrap_or_default()
        .lines()
        .next()
        .unwrap_or_default();
    let prefix = format!("{key}=");
    notice
        .split(' ')
        .find_map(|field| field.strip_prefix(prefix.as_str()))
        .unwrap()
        .to_owned()
}

/// A small hide early in the conversation is refused before it is staged, with the numbers that
/// refuse it; nothing is recorded but the call's failed result, the revision does not move, and the
/// next request extends the one before it rather than rewriting it.
#[test]
fn a_small_early_hide_ahead_of_a_long_suffix_is_refused_with_its_numbers() {
    let ran = run(Setup::stock(), vec![small_early_hide()]);
    let (sentence, failed) = tool_result(&ran.session, "p");
    assert!(failed, "{sentence}");
    assert!(sentence.starts_with("not staged:"), "{sentence}");
    let first = last_tool_json(&ran.sent[8])["fragments"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        sentence.contains(&format!("from {first} to the end")),
        "{sentence}"
    );
    let removed = number_after(&sentence, "would be about ");
    let reread = number_after(&sentence, "re-read about ");
    let needed = number_after(&sentence, "re-read (");
    assert!(removed > 0 && reread > 1_000, "{sentence}");
    assert!(removed.saturating_mul(break_even::REREAD_PER_HIDDEN_TOKEN_MAX) < reread);
    assert_eq!(
        needed,
        reread.div_ceil(break_even::REREAD_PER_HIDDEN_TOKEN_MAX)
    );

    let records = ran.session.log().events().iter().filter(|event| {
        matches!(
            event,
            SessionEvent::ContextDecision { .. } | SessionEvent::ContextRevision { .. }
        )
    });
    assert_eq!(records.count(), 0, "nothing staged, so nothing decided");
    assert_eq!(ManagedState::fold(ran.session.log()).unwrap().revision(), 0);

    // The request that carried the proposal and the one after its refusal: the later one begins
    // with every message of the earlier, so nothing was rewritten, and it hides nothing.
    let (carried, next) = (&ran.sent[8], &ran.sent[9]);
    assert_eq!(
        ran.sent.len(),
        10,
        "the work, the inspect, the proposal and the answer"
    );
    assert_eq!(
        &next.messages[..carried.messages.len()],
        &carried.messages[..]
    );
    assert_eq!(tail_value(next, "revision"), "0");
    assert_eq!(tail_value(next, "hidden_fragments"), "0");
}

/// The inspect page states the rule the proposal is then measured by, and nothing when it is off.
#[test]
fn inspect_states_the_rule_and_its_waiver() {
    let stated = |setup: Setup| {
        let ran = run(setup, vec![small_early_hide()]);
        tool_result_json(&ran.session, "i1")["hide_rule"].clone()
    };
    assert_eq!(
        stated(Setup::stock()),
        json!({"max_reread_per_hidden_token": 5, "waived": null})
    );
    assert_eq!(
        stated(Setup {
            target: Some(2_000),
            ..Setup::stock()
        }),
        json!({"max_reread_per_hidden_token": 5, "waived": "pressure"})
    );
    assert_eq!(
        stated(Setup {
            rule: false,
            ..Setup::stock()
        }),
        Value::Null
    );
}

/// Under the pressure the budget hint is given at, any hide is welcome: the same small hide is
/// staged and accepted.
#[test]
fn the_same_hide_is_staged_under_pressure() {
    let ran = run(
        Setup {
            target: Some(2_000),
            ..Setup::stock()
        },
        vec![small_early_hide()],
    );
    assert!(accepted(&ran, "p"), "{}", tool_result(&ran.session, "p").0);
}

/// Once the provider's cache has lapsed, the rewrite is free: the same small hide is staged, with
/// cold fitting on or off.
#[test]
fn the_same_hide_is_staged_once_the_cache_has_lapsed() {
    for cold_fit in [true, false] {
        let lapsed = Setup {
            stall_ms: LONG_MS + 1,
            cold_fit,
            ..Setup::stock()
        };
        let ran = run(lapsed, vec![small_early_hide()]);
        assert!(accepted(&ran, "p"), "{}", tool_result(&ran.session, "p").0);
        // An hour exactly is still warm, and the rule applies.
        let warm = run(
            Setup {
                stall_ms: LONG_MS,
                ..lapsed
            },
            vec![small_early_hide()],
        );
        assert!(tool_result(&warm.session, "p").1, "refused while warm");
    }
}

/// With the rule off, the same small hide is staged and accepted as it always was.
#[test]
fn the_same_hide_is_staged_with_the_rule_off() {
    let ran = run(
        Setup {
            rule: false,
            ..Setup::stock()
        },
        vec![small_early_hide()],
    );
    assert!(accepted(&ran, "p"));
}

/// Hiding the whole resolved stretch in one proposal pays for itself, and so does a hide near the
/// end, whose suffix is small.
#[test]
fn a_large_hide_and_a_late_hide_are_staged() {
    let large = run(Setup::stock(), vec![propose("p", every, none, no_notes)]);
    assert!(
        accepted(&large, "p"),
        "{}",
        tool_result(&large.session, "p").0
    );
    let late = run(Setup::stock(), vec![propose("p", newest, none, no_notes)]);
    assert!(
        accepted(&late, "p"),
        "{}",
        tool_result(&late.session, "p").0
    );
    let hidden = |ran: &Ran| {
        ManagedState::fold(ran.session.log())
            .unwrap()
            .hidden()
            .len()
    };
    assert_eq!((hidden(&large), hidden(&late)), (5, 1));
}

/// A proposal that hides nothing is never judged: a restore of the earliest fragment, which forces
/// the longest rewrite there is, and a note change alone are both staged.
#[test]
fn a_restore_and_a_note_change_are_never_judged() {
    let restore = run(
        Setup::stock(),
        vec![
            propose("h", every, none, no_notes),
            Box::new(|_: &ChatRequest| call("i2", "context_manage", INSPECT)),
            propose("p", none, oldest_hidden, no_notes),
        ],
    );
    assert!(
        tool_result_json(&restore.session, "p")["status"] == "staged",
        "{}",
        tool_result(&restore.session, "p").0
    );
    let state = ManagedState::fold(restore.session.log()).unwrap();
    assert_eq!((state.revision(), state.hidden().len()), (2, 4));
    assert_eq!(
        state.last_decision.unwrap().outcome,
        DecisionOutcome::Accepted
    );

    let notes = run(Setup::stock(), vec![propose("p", none, none, a_note)]);
    assert!(
        accepted(&notes, "p"),
        "{}",
        tool_result(&notes.session, "p").0
    );
    let state = ManagedState::fold(notes.session.log()).unwrap();
    assert_eq!((state.notes().len(), state.hidden().len()), (1, 0));
}

fn oldest_hidden(ids: &[String]) -> Vec<String> {
    assert_eq!(ids.len(), 5, "{ids:?}");
    ids[..1].to_vec()
}

/// A small hide that comes with a note change is judged on its hide: the note does not buy it a
/// pass, and the refusal stages neither.
#[test]
fn a_note_does_not_carry_a_small_hide() {
    let ran = run(Setup::stock(), vec![propose("p", oldest, none, a_note)]);
    let (sentence, failed) = tool_result(&ran.session, "p");
    assert!(failed && sentence.starts_with("not staged:"), "{sentence}");
    assert!(
        ManagedState::fold(ran.session.log())
            .unwrap()
            .notes()
            .is_empty()
    );
}

/// The checks that refused a proposal before the rule existed still answer first: a mixed batch is
/// refused whole, and a stale base with its code, not with the rule's sentence.
#[test]
fn the_mixed_batch_rule_and_a_stale_base_still_answer_first() {
    let mixed = run(
        Setup::stock(),
        vec![Box::new(|request: &ChatRequest| {
            let mut events = small_early_hide()(request);
            let mut echo = call("e", "echo", r#"{"size": 10}"#);
            if let LlmEvent::ToolCallDelta { index, .. } = &mut echo[0] {
                *index = 1;
            }
            events.insert(1, echo.remove(0));
            events
        })],
    );
    for id in ["p", "e"] {
        let (sentence, failed) = tool_result(&mixed.session, id);
        assert!(
            failed && sentence.contains("mixed_mutation_batch"),
            "{sentence}"
        );
    }

    let stale = run(
        Setup::stock(),
        vec![Box::new(|request: &ChatRequest| {
            let events = small_early_hide()(request);
            let LlmEvent::ToolCallDelta {
                arguments_delta, ..
            } = &events[0]
            else {
                panic!("a call")
            };
            let mut arguments: Value = serde_json::from_str(arguments_delta).unwrap();
            arguments["base_revision"] = json!(7);
            call("p", "context_manage", &arguments.to_string())
        })],
    );
    let refused = tool_result_json(&stale.session, "p");
    assert_eq!(refused["status"], "refused");
    assert_eq!(refused["error_code"], "stale_base");
}

/// The inspect the scripted proposals read lists five eligible fragments; checked here so a change
/// to the fixture fails loudly rather than as a wrong proposal.
#[test]
fn the_fixture_leaves_five_eligible_fragments_and_protects_the_rest() {
    let ran = run(Setup::stock(), vec![small_early_hide()]);
    let inspected = last_tool_json(&ran.sent[8]);
    assert_eq!(eligible(&inspected).len(), 5, "{inspected}");
    assert_eq!(
        inspected["fragments"].as_array().unwrap().len(),
        7,
        "the seven echoes: the inspect's own call is not answered yet"
    );
    assert!(
        ran.session.log().events().iter().all(
            |event| !matches!(event, SessionEvent::ContextRevision { payload }
                if payload.author == RevisionAuthor::Automatic)
        ),
        "nothing is fitted: the session is far under its allowance"
    );
}
