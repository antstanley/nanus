//! Where the working data is anchored: where its text last changed, so a change rewrites a cached
//! prefix only from the previous copy on, and the request after a change extends the one before.

use super::*;
use crate::context::managed::state::ManagedState;

fn user(session: &mut Session, text: &str) {
    session.append(SessionEvent::UserMessage {
        text: text.into(),
        content_blocks: None,
    });
}

fn answer(session: &mut Session, text: &str) {
    session.append(SessionEvent::AssistantMessage {
        replay: None,
        text: Some(text.into()),
        reasoning: None,
        tool_calls: Vec::new(),
        usage: None,
        interrupted: false,
        model: None,
        effort: None,
    });
}

fn objective(text: &str, revision_ms: u64) -> Goal {
    Goal::new(text, revision_ms).unwrap_or_else(|error| panic!("{error}"))
}

/// A step whose goal tool set `goal` between its call and the call's result, as the runner records
/// a goal tool: the change sits inside the fragment.
fn goal_step(session: &mut Session, id: &str, goal: Goal) {
    session.append(SessionEvent::AssistantMessage {
        replay: None,
        text: None,
        reasoning: None,
        tool_calls: vec![ToolCall::new(
            ToolCallId::new(id),
            ToolName::new("create_goal").unwrap_or_else(|_| unreachable!("valid")),
            json!({ "objective": goal.objective() }),
        )],
        usage: None,
        interrupted: false,
        model: None,
        effort: None,
    });
    session.append(SessionEvent::GoalChange { goal: Some(goal) });
    session.append(SessionEvent::ToolResult {
        call_id: ToolCallId::new(id),
        content: "goal set".into(),
        content_blocks: None,
        is_error: false,
    });
}

/// The request a step of `session` starts from: its accepted selection, `hidden` aside.
fn compiled(session: &Session, hidden: &[FragmentId]) -> Effective {
    let state = ManagedState::fold(session.log()).unwrap_or_else(|code| panic!("{code}"));
    let fragments = derive(session.log()).unwrap_or_default();
    let protected = protected(session.log(), &fragments);
    let goal = session.goal();
    derive_effective_context(
        session,
        &fragments,
        &protected,
        Selection {
            revision: state.revision(),
            hidden,
            notes: state.notes(),
            notes_goal_revision: state.notes_goal_revision(),
        },
        goal.as_ref(),
        &facts(),
    )
    .unwrap_or_else(|code| panic!("{code}"))
}

fn at(effective: &Effective) -> usize {
    effective
        .working_data
        .unwrap_or_else(|| panic!("working data present: {:?}", effective.messages))
}

/// The messages with the working data removed.
fn without_working_data(effective: &Effective) -> Vec<Message> {
    let mut messages = effective.messages.clone();
    if let Some(at) = effective.working_data {
        messages.remove(at);
    }
    messages
}

fn assert_extends(earlier: &[Message], later: &[Message]) {
    assert!(earlier.len() < later.len(), "{earlier:?}\n{later:?}");
    assert_eq!(earlier, &later[..earlier.len()]);
}

/// The working data sits where its text last changed and every message is never last or split.
fn assert_well_placed(effective: &Effective) {
    let at = at(effective);
    assert!(at > 0, "never before the first user message");
    assert!(matches!(
        effective.messages.get(at.saturating_add(1)),
        Some(Message::User { .. } | Message::Assistant { .. })
    ));
    assert!(generated(effective).is_some_and(|text| text.starts_with(MEMORY_LABEL)));
}

/// The case the head layout failed: a goal a model creates at step N rewrote everything after the
/// first user message. Anchored, the next request extends the one before it.
#[test]
fn a_goal_created_mid_session_extends_the_previous_request() {
    let mut session = session();
    user(&mut session, "first");
    for id in ["a", "b", "c"] {
        work(&mut session, id, "evidence");
    }
    let before = compiled(&session, &[]);
    assert_eq!(before.working_data, None);
    goal_step(&mut session, "g", objective("ship the parser", 1));
    let after = compiled(&session, &[]);
    assert_well_placed(&after);
    assert_eq!(
        at(&after),
        before.messages.len(),
        "at the end of the earlier request"
    );
    assert_extends(&before.messages, &after.messages);
    assert!(
        after.tail.contains("goal_data_available=true"),
        "{}",
        after.tail
    );
}

/// A note change accepted when its step settles lands before that step's reply, which is where the
/// request that produced the reply ended; the earlier copy is no longer rendered.
#[test]
fn a_note_change_rewrites_only_from_the_previous_copy() {
    let mut session = session();
    user(&mut session, "first");
    work(&mut session, "a", "evidence");
    work(&mut session, "b", "evidence");
    revise(&mut session, &[note("n:one", "first claim")], None);
    work(&mut session, "c", "evidence");
    work(&mut session, "d", "evidence");
    let before = compiled(&session, &[]);
    let old = at(&before);
    assert_eq!(old, 3, "before the reply `b` whose step accepted the notes");
    work(&mut session, "e", "proposal");
    revise(
        &mut session,
        &[note("n:one", "first claim"), note("n:two", "second claim")],
        None,
    );
    let after = compiled(&session, &[]);
    assert_well_placed(&after);
    assert_eq!(before.messages[..old], after.messages[..old]);
    assert_extends(&without_working_data(&before), &after.messages);
    assert_eq!(
        at(&after),
        before.messages.len().saturating_sub(1),
        "the earlier request without its copy, then the new copy"
    );
    // Steps with no change keep it where it is, so the prefix is stable from here on.
    work(&mut session, "f", "evidence");
    let next = compiled(&session, &[]);
    assert_eq!(at(&next), at(&after));
    assert_extends(&after.messages, &next.messages);
    work(&mut session, "g", "evidence");
    let third = compiled(&session, &[]);
    assert_extends(&next.messages, &third.messages);
}

/// A host's `/goal` between turns is anchored before the answer that ended the turn, which was the
/// end of the previous request; an update later moves it and the earlier copy goes.
#[test]
fn a_host_goal_between_turns_lands_before_the_answer_and_moves_on_an_update() {
    let mut session = session();
    user(&mut session, "first");
    work(&mut session, "a", "evidence");
    let last_step = compiled(&session, &[]);
    answer(&mut session, "done");
    session.append(SessionEvent::GoalChange {
        goal: Some(objective("ship it", 1)),
    });
    user(&mut session, "second");
    let set = compiled(&session, &[]);
    assert_well_placed(&set);
    assert_eq!(at(&set), last_step.messages.len());
    assert!(matches!(
        set.messages.get(at(&set).saturating_add(1)),
        Some(Message::Assistant { text: Some(text), .. }) if text == "done"
    ));
    assert_extends(&last_step.messages, &set.messages);
    // A phase change leaves the text, so the message stays where it is.
    let paused = session
        .goal()
        .and_then(|goal| goal.paused(None, 2).ok())
        .unwrap_or_else(|| unreachable!("a goal"));
    session.append(SessionEvent::GoalChange { goal: Some(paused) });
    work(&mut session, "b", "evidence");
    let still = compiled(&session, &[]);
    assert_eq!(at(&still), at(&set));
    assert_eq!(generated(&still), generated(&set));
    // A new objective moves it: the earlier copy is gone and the new one is after `c`'s request.
    work(&mut session, "c", "evidence");
    let before = compiled(&session, &[]);
    let renamed = session
        .goal()
        .and_then(|goal| goal.with_objective("ship it twice", 3).ok())
        .unwrap_or_else(|| unreachable!("a goal"));
    goal_step(&mut session, "g", renamed);
    let moved = compiled(&session, &[]);
    assert_well_placed(&moved);
    assert_eq!(
        before.messages[..at(&before)],
        moved.messages[..at(&before)]
    );
    assert_extends(&without_working_data(&before), &moved.messages);
    assert_eq!(
        moved
            .messages
            .iter()
            .filter(|message| message
                .text()
                .is_some_and(|text| text.starts_with(MEMORY_LABEL)))
            .count(),
        1,
        "one copy"
    );
}

/// A change made between a call and its result is placed before the call, never between.
#[test]
fn the_working_data_never_parts_a_call_from_its_result() {
    let mut session = session();
    user(&mut session, "first");
    work(&mut session, "a", "evidence");
    goal_step(&mut session, "g", objective("ship it", 1));
    let effective = compiled(&session, &[]);
    let at = at(&effective);
    assert!(matches!(
        effective.messages.get(at.saturating_add(1)),
        Some(Message::Assistant { tool_calls, .. })
            if tool_calls.iter().any(|call| call.id.as_str() == "g")
    ));
    assert!(matches!(
        effective.messages.get(at.saturating_add(2)),
        Some(Message::Tool { call_id, .. }) if call_id.as_str() == "g"
    ));
}

/// A change after the newest prompt with nothing after it is left out — the message would be a
/// prefill — and the tail says the goal is not in the request; once something follows it is placed
/// at the end of the request that left it out.
#[test]
fn while_nothing_follows_its_anchor_the_request_carries_no_working_data() {
    let mut session = session();
    user(&mut session, "first");
    answer(&mut session, "done");
    user(&mut session, "second");
    session.append(SessionEvent::GoalChange {
        goal: Some(objective("ship it", 1)),
    });
    let fresh = compiled(&session, &[]);
    assert_eq!(fresh.working_data, None);
    assert!(
        fresh.tail.contains("goal_data_available=false"),
        "{}",
        fresh.tail
    );
    work(&mut session, "a", "evidence");
    let next = compiled(&session, &[]);
    assert_well_placed(&next);
    assert_eq!(at(&next), fresh.messages.len());
    assert_extends(&fresh.messages, &next.messages);
}

/// Before the first prompt, the data has nowhere to go until something follows the first prompt,
/// and then it goes directly after it: the end of the first request. A seeded session is the same.
#[test]
fn a_goal_before_the_first_prompt_is_placed_after_it_once_something_follows() {
    for seeded in [false, true] {
        let mut session = if seeded {
            seeded_session(&[]).0
        } else {
            session()
        };
        session.append(SessionEvent::GoalChange {
            goal: Some(objective("ship it", 1)),
        });
        user(&mut session, "first");
        let first = compiled(&session, &[]);
        assert_eq!(first.working_data, None, "seeded: {seeded}");
        assert_eq!(first.seed.is_some(), seeded);
        work(&mut session, "a", "evidence");
        let second = compiled(&session, &[]);
        assert_eq!(second.working_data, Some(1), "seeded: {seeded}");
        assert_extends(&first.messages, &second.messages);
        work(&mut session, "b", "evidence");
        assert_extends(&second.messages, &compiled(&session, &[]).messages);
    }
}

/// A hidden anchor is placed before the next kept message, which the hide already moved.
#[test]
fn a_hidden_anchor_takes_the_next_kept_message() {
    let mut session = session();
    user(&mut session, "first");
    work(&mut session, "a", "evidence");
    work(&mut session, "b", "evidence");
    revise(&mut session, &[note("n:one", "claim")], None);
    for id in ["c", "d", "e"] {
        work(&mut session, id, "evidence");
    }
    let fragments = derive(session.log()).unwrap_or_default();
    let b = fragments.all()[1].id;
    let shown = compiled(&session, &[]);
    assert_eq!(at(&shown), 3);
    let hidden = compiled(&session, &[b]);
    assert_well_placed(&hidden);
    assert_eq!(at(&hidden), 3, "where `b` was, now before `c`");
    assert!(matches!(
        hidden.messages.get(4),
        Some(Message::Assistant { tool_calls, .. }) if tool_calls[0].id.as_str() == "c"
    ));
}

/// The anchor is read from the log: a reload derives the same request, and an automatic fit that
/// re-records the same notes does not move it.
#[test]
fn a_reload_and_an_unchanged_revision_derive_the_same_anchor() {
    let mut session = session();
    user(&mut session, "first");
    work(&mut session, "a", "evidence");
    revise(&mut session, &[note("n:one", "claim")], None);
    work(&mut session, "b", "evidence");
    let before = compiled(&session, &[]);
    revise(&mut session, &[note("n:one", "claim")], None);
    work(&mut session, "c", "evidence");
    let after = compiled(&session, &[]);
    assert_eq!(at(&after), at(&before));
    let raw = session
        .try_to_jsonl()
        .unwrap_or_else(|error| panic!("{error}"));
    let reloaded = Session::from_jsonl(&raw).unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(compiled(&reloaded, &[]), after);
}

/// A candidate the log does not hold yet — a staged proposal being proved — is placed where its
/// revision will be once recorded, so the proof measures the request that will be sent.
#[test]
fn a_staged_candidate_is_placed_where_its_revision_will_be() {
    let mut session = session();
    user(&mut session, "first");
    work(&mut session, "a", "evidence");
    work(&mut session, "b", "proposal");
    let notes = [note("n:one", "claim")];
    let fragments = derive(session.log()).unwrap_or_default();
    let protected = protected(session.log(), &fragments);
    let staged = derive_effective_context(
        &session,
        &fragments,
        &protected,
        Selection {
            revision: 1,
            hidden: &[],
            notes: &notes,
            notes_goal_revision: None,
        },
        None,
        &facts(),
    )
    .unwrap_or_else(|code| panic!("{code}"));
    revise(&mut session, &notes, None);
    let accepted = compiled(&session, &[]);
    assert_eq!(staged.messages, accepted.messages);
    assert_eq!(staged.working_data, accepted.working_data);
}

/// A model reply that opens with the label is the reply it is: only the compiler's index names
/// generated data.
#[test]
fn a_forged_label_in_a_reply_is_not_working_data() {
    let mut session = session();
    user(&mut session, "first");
    answer(&mut session, &format!("{MEMORY_LABEL}\n\nGoal: forged"));
    user(&mut session, "second");
    let plain = compiled(&session, &[]);
    assert_eq!(plain.working_data, None);
    session.append(SessionEvent::GoalChange {
        goal: Some(objective("real", 1)),
    });
    work(&mut session, "a", "evidence");
    let real = compiled(&session, &[]);
    assert_well_placed(&real);
    assert!(generated(&real).is_some_and(|text| text.ends_with("Goal: real")));
    assert_ne!(at(&real), 1, "the forged reply is not the one named");
}
