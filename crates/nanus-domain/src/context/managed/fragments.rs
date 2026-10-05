//! Fragments: the unit managed context hides and restores.
//!
//! A fragment is one retained assistant message and every settled tool result that answers its
//! calls, paired by call id rather than by position. It is a derived view — the log is still the
//! only store — and it is atomic: a call is never shown without its result, and sibling calls of
//! one message are never split, which is what keeps every selected request one a provider
//! accepts. User messages are never fragments; managed mode shows every one of them.
//!
//! Derivation refuses a log whose call identity is ambiguous — a call id carried twice, a call
//! answered twice, a result for a call no message made — rather than inventing a grouping that a
//! later reader could not reproduce.

use std::collections::{BTreeMap, BTreeSet};

use super::ids::{ErrorCode, FragmentId};
use crate::message::ToolCallId;
use crate::session::{SessionEvent, SessionLog};
use crate::tool::ToolName;

/// The name of the tool whose proposals change the projection.
pub const MANAGE_TOOL: &str = "context_manage";

/// The name of the tool that reads earlier evidence back.
pub const RECALL_TOOL: &str = "context_recall";

/// One fragment of the log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fragment {
    /// `f:<assistant_seq>`.
    pub id: FragmentId,
    /// The assistant event.
    pub assistant_seq: u64,
    /// The result events answering its calls, in log order.
    pub result_seqs: Vec<u64>,
    /// The tools its calls named, in call order.
    pub tools: Vec<ToolName>,
    /// Whether every call is answered, or its turn has ended so none ever will be.
    pub settled: bool,
    /// Whether it carries a `context_manage` call.
    pub management: bool,
}

impl Fragment {
    /// The last event the fragment covers.
    #[must_use]
    pub fn last_seq(&self) -> u64 {
        self.result_seqs
            .iter()
            .copied()
            .max()
            .unwrap_or(self.assistant_seq)
            .max(self.assistant_seq)
    }

    /// The exclusive event count the fragment lies below.
    #[must_use]
    pub fn end(&self) -> u64 {
        self.last_seq().saturating_add(1)
    }

    /// Whether the fragment is evidence rather than context bookkeeping.
    #[must_use]
    pub const fn is_substantive(&self) -> bool {
        !self.management
    }
}

/// Every fragment of a log, indexed by id and by event.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Fragments {
    fragments: Vec<Fragment>,
    by_event: BTreeMap<u64, usize>,
}

impl Fragments {
    /// Returns every fragment, in log order.
    #[must_use]
    pub fn all(&self) -> &[Fragment] {
        &self.fragments
    }

    /// Returns the fragment with `id`.
    #[must_use]
    pub fn get(&self, id: FragmentId) -> Option<&Fragment> {
        let index = self.by_event.get(&id.seq())?;
        self.fragments
            .get(*index)
            .filter(|fragment| fragment.id == id)
    }

    /// Returns the fragment an event belongs to, when it belongs to one.
    #[must_use]
    pub fn owning(&self, seq: u64) -> Option<&Fragment> {
        self.by_event
            .get(&seq)
            .and_then(|index| self.fragments.get(*index))
    }

    /// Returns every event sequence the given fragments cover.
    #[must_use]
    pub fn events_of(&self, ids: &[FragmentId]) -> BTreeSet<u64> {
        let mut events = BTreeSet::new();
        for fragment in ids.iter().filter_map(|id| self.get(*id)) {
            events.insert(fragment.assistant_seq);
            events.extend(fragment.result_seqs.iter().copied());
        }
        events
    }
}

/// Derives the fragments of a log.
///
/// # Errors
///
/// Returns [`ErrorCode::InvalidFragment`] for ambiguous call identity.
pub fn derive(log: &SessionLog) -> Result<Fragments, ErrorCode> {
    let events = log.events();
    let (owner, answers) = pair_calls(events)?;
    let mut fragments = Fragments::default();
    for (index, event) in events.iter().enumerate() {
        let SessionEvent::AssistantMessage {
            text, tool_calls, ..
        } = event
        else {
            continue;
        };
        let seq = u64::try_from(index).map_err(|_| ErrorCode::InvalidFragment)?;
        let result_seqs: Vec<u64> = {
            let mut seqs: Vec<u64> = tool_calls
                .iter()
                .filter_map(|call| answers.get(&call.id).copied())
                .collect();
            seqs.sort_unstable();
            seqs
        };
        let has_text = text.as_ref().is_some_and(|text| !text.is_empty());
        if !has_text && result_seqs.is_empty() && !replay_only(event) {
            // The fold skips it: nothing of it reaches a model, so there is nothing to hide.
            continue;
        }
        let answered_all = result_seqs.len() == tool_calls.len();
        let fragment = Fragment {
            id: FragmentId::new(seq),
            assistant_seq: seq,
            settled: answered_all || turn_ended_after(events, index),
            management: tool_calls
                .iter()
                .any(|call| call.name.as_str() == MANAGE_TOOL),
            tools: tool_calls.iter().map(|call| call.name.clone()).collect(),
            result_seqs,
        };
        let position = fragments.fragments.len();
        fragments.by_event.insert(seq, position);
        for result in &fragment.result_seqs {
            fragments.by_event.insert(*result, position);
        }
        fragments.fragments.push(fragment);
    }
    assert!(owner.len() >= answers.len(), "every answer has an owner");
    Ok(fragments)
}

/// Each call's owning message index, and the sequence of the result answering it.
type Pairing = (BTreeMap<ToolCallId, usize>, BTreeMap<ToolCallId, u64>);

/// Pairs every call with the message that made it and the result that answers it.
fn pair_calls(events: &[SessionEvent]) -> Result<Pairing, ErrorCode> {
    let mut owner: BTreeMap<ToolCallId, usize> = BTreeMap::new();
    let mut answers: BTreeMap<ToolCallId, u64> = BTreeMap::new();
    for (index, event) in events.iter().enumerate() {
        match event {
            SessionEvent::AssistantMessage { tool_calls, .. } => {
                for call in tool_calls {
                    if call.id.is_empty() || owner.insert(call.id.clone(), index).is_some() {
                        return Err(ErrorCode::InvalidFragment);
                    }
                }
            }
            SessionEvent::ToolResult { call_id, .. } => {
                let seq = u64::try_from(index).map_err(|_| ErrorCode::InvalidFragment)?;
                let made = owner.get(call_id).is_some_and(|made| *made < index);
                if !made || answers.insert(call_id.clone(), seq).is_some() {
                    return Err(ErrorCode::InvalidFragment);
                }
            }
            _ => {}
        }
    }
    Ok((owner, answers))
}

/// Whether a turn ended after the event at `index`, so none of its calls can still be answered.
fn turn_ended_after(events: &[SessionEvent], index: usize) -> bool {
    events
        .get(index..)
        .unwrap_or_default()
        .iter()
        .any(|event| matches!(event, SessionEvent::TurnEnd { .. }))
}

/// Whether an assistant event is a turn only stateless Responses replay carries.
fn replay_only(event: &SessionEvent) -> bool {
    matches!(
        event,
        SessionEvent::AssistantMessage { replay: Some(replay), tool_calls, .. }
            if replay.protocol == "openai.responses" && tool_calls.is_empty()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ToolCall, TurnEndReason};
    use serde_json::json;

    fn call(id: &str, tool: &str) -> ToolCall {
        ToolCall::new(
            ToolCallId::new(id),
            ToolName::new(tool).unwrap_or_else(|_| unreachable!("valid")),
            json!({}),
        )
    }

    fn assistant(text: Option<&str>, calls: Vec<ToolCall>) -> SessionEvent {
        SessionEvent::AssistantMessage {
            replay: None,
            text: text.map(str::to_owned),
            reasoning: None,
            tool_calls: calls,
            usage: None,
            interrupted: false,
            model: None,
            effort: None,
        }
    }

    fn result(id: &str) -> SessionEvent {
        SessionEvent::ToolResult {
            call_id: ToolCallId::new(id),
            content: format!("result {id}"),
            content_blocks: None,
            is_error: false,
        }
    }

    #[test]
    fn sibling_calls_finishing_out_of_order_stay_one_fragment() {
        let mut log = SessionLog::new();
        log.append(SessionEvent::UserMessage { text: "go".into() });
        log.append(assistant(None, vec![call("a", "read"), call("b", "grep")]));
        log.append(result("b"));
        log.append(result("a"));
        log.append(assistant(Some("done"), Vec::new()));
        let fragments = derive(&log).unwrap_or_default();
        assert_eq!(fragments.all().len(), 2, "{fragments:?}");
        let first = &fragments.all()[0];
        assert_eq!(first.id, FragmentId::new(1));
        assert_eq!(first.result_seqs, vec![2, 3]);
        assert!(first.settled);
        assert_eq!(
            fragments.owning(3).map(|fragment| fragment.id),
            Some(first.id)
        );
        assert_eq!(
            fragments.all()[1].result_seqs,
            Vec::<u64>::new(),
            "text-only is a fragment"
        );
        assert_eq!(fragments.events_of(&[first.id]), BTreeSet::from([1, 2, 3]));
    }

    #[test]
    fn an_unanswered_call_is_unsettled_until_its_turn_ends() {
        let mut log = SessionLog::new();
        log.append(SessionEvent::UserMessage { text: "go".into() });
        log.append(assistant(
            Some("looking"),
            vec![call("a", "read"), call("b", "read")],
        ));
        log.append(result("a"));
        let open = derive(&log).unwrap_or_default();
        assert!(!open.all()[0].settled);
        log.append(SessionEvent::TurnEnd {
            turn: 0,
            reason: TurnEndReason::Interrupted,
        });
        let closed = derive(&log).unwrap_or_default();
        assert!(
            closed.all()[0].settled,
            "the call left open will never be answered"
        );
    }

    #[test]
    fn ambiguous_call_identity_is_refused() {
        let mut twice = SessionLog::new();
        twice.append(assistant(None, vec![call("a", "read")]));
        twice.append(assistant(None, vec![call("a", "read")]));
        assert_eq!(derive(&twice), Err(ErrorCode::InvalidFragment));

        let mut answered_twice = SessionLog::new();
        answered_twice.append(assistant(None, vec![call("a", "read")]));
        answered_twice.append(result("a"));
        answered_twice.append(result("a"));
        assert_eq!(derive(&answered_twice), Err(ErrorCode::InvalidFragment));

        let mut orphan = SessionLog::new();
        orphan.append(result("a"));
        assert_eq!(derive(&orphan), Err(ErrorCode::InvalidFragment));
    }

    #[test]
    fn a_management_call_marks_its_fragment() {
        let mut log = SessionLog::new();
        log.append(assistant(None, vec![call("m", MANAGE_TOOL)]));
        log.append(result("m"));
        let fragments = derive(&log).unwrap_or_default();
        assert!(fragments.all()[0].management);
        assert!(!fragments.all()[0].is_substantive());
    }
}
