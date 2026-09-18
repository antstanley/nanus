//! Fitting a conversation into the model's context window.
//!
//! Prompt assembly replays the whole log, so a long session eventually asks for more tokens
//! than the model has. Something has to give, and the only three answers are: send it and let
//! the provider refuse, drop the oldest part of the conversation, or refuse the turn here with
//! a sentence. The first is a failure with no diagnosis; the third is right when there is
//! nothing left to drop; so this module is the second, written to be *deterministic* and
//! *visible* rather than clever:
//!
//! - **Deterministic.** The same messages and the same budget always produce the same prompt,
//!   because a policy whose output depended on a clock, a hash, or a provider would make two
//!   runs of one session incomparable — and comparability is what a session log is for.
//! - **Visible.** What was left out is a message the model reads, at the point where the gap
//!   is, and an [`Elision`] the caller reports to whoever is watching. A model that cannot see
//!   the beginning of a conversation and does not know it will confidently contradict itself.
//! - **Whole turns, oldest first.** The unit dropped is a turn — a human message and
//!   everything the model did about it — because dropping *messages* can separate a tool call
//!   from the result that answers it, and a provider refuses such a request outright. The most
//!   recent turn is never dropped: it is the question being answered.
//!
//! ## Why the estimate is an estimate
//!
//! There is no tokenizer here, and there will not be one: it is a large dependency, it is
//! provider-specific, and the provider reports the *real* count with every response, which is
//! the number a budget is eventually checked against. What this module has is a deterministic
//! approximation — characters over four, plus a small fixed cost per message — close for prose,
//! generous for code and JSON, and identical on every platform. A budget is therefore a ceiling
//! to stay under rather than a promise about a number, which is why the configuration's default
//! sits well below every provider's window.

use crate::message::Message;

/// Characters per token in the estimate.
const CHARS_PER_TOKEN: usize = 4;

/// Tokens charged for each message, whatever it contains.
///
/// Every provider frames a message with role and delimiter tokens, so counting only the text
/// would let a conversation of many tiny messages be estimated at nearly nothing.
const TOKENS_PER_MESSAGE: u32 = 4;

/// What fitting a conversation left out.
///
/// Reported rather than discarded: a turn that quietly saw half a conversation is a turn whose
/// answer cannot be trusted, and a reader has to be able to tell it from one that saw all of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Elision {
    /// How many messages were left out.
    pub dropped_messages: u32,
    /// How many turns those messages made up.
    pub dropped_turns: u32,
    /// The estimated size of the prompt that was sent, notice included.
    pub kept_tokens: u32,
    /// The budget it was fitted into.
    pub budget: u32,
}

/// A conversation that fits, and what was dropped to make it.
#[derive(Clone, Debug, PartialEq)]
pub struct Fitted {
    /// The messages to send, with the notice already in place.
    pub messages: Vec<Message>,
    /// What was left out, when anything was.
    pub elision: Option<Elision>,
}

/// Why a conversation could not be fitted at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum FitError {
    /// The newest turn alone does not fit the budget.
    ///
    /// Refused rather than sent: dropping it would answer a question the model never saw, and
    /// sending it would be refused by the provider with a message about the request rather than
    /// about the budget. The sentence names the knob to turn.
    #[error(
        "the newest turn is {estimated} estimated tokens and the budget is {budget}; \
         raise `context_budget` or start a new session"
    )]
    TooLarge {
        /// The estimated size of the smallest prompt that could answer the question.
        estimated: u32,
        /// The budget it was measured against.
        budget: u32,
    },
}

/// Estimates the tokens a list of messages costs.
///
/// Saturating, so an absurd input reports an absurd number rather than wrapping to zero and
/// reading as "it fits".
#[must_use]
pub fn estimate(messages: &[Message]) -> u32 {
    messages.iter().fold(0_u32, |total, message| {
        total.saturating_add(estimate_message(message))
    })
}

/// Estimates the tokens one message costs.
#[must_use]
pub fn estimate_message(message: &Message) -> u32 {
    let mut chars = 0_usize;
    match message {
        Message::System { text } | Message::User { text } => {
            chars = chars.saturating_add(text.chars().count());
        }
        Message::Assistant {
            text,
            reasoning,
            tool_calls,
        } => {
            chars = chars.saturating_add(text.as_ref().map_or(0, |text| text.chars().count()));
            chars = chars.saturating_add(reasoning.as_ref().map_or(0, |text| text.chars().count()));
            for call in tool_calls {
                // The arguments travel as JSON, so they are charged as JSON.
                chars = chars.saturating_add(call.arguments.to_string().chars().count());
            }
        }
        Message::Tool { content, .. } => {
            chars = chars.saturating_add(content.chars().count());
        }
    }
    let tokens = chars.checked_div(CHARS_PER_TOKEN).unwrap_or_default();
    TOKENS_PER_MESSAGE.saturating_add(u32::try_from(tokens).unwrap_or(u32::MAX))
}

/// Fits `messages` into `budget` estimated tokens.
///
/// The leading run of non-human messages — the system prompt — is never dropped, because
/// everything after it depends on it, and the newest turn is never dropped, because it is the
/// question. Everything between is dropped whole, oldest first, until what remains fits along
/// with the notice that says so.
///
/// # Errors
///
/// Returns [`FitError::TooLarge`] when the prompt that cannot be shortened further does not fit.
pub fn fit(messages: Vec<Message>, budget: u32) -> Result<Fitted, FitError> {
    let head = leading_prompt(&messages);
    let tail = &messages[head..];
    let turns = split_turns(tail);
    if turns.is_empty() {
        // No human turn at all: there is nothing to drop and nothing to answer, so the only
        // question is whether the prompt itself fits.
        let estimated = estimate(&messages);
        if estimated > budget {
            return Err(FitError::TooLarge { estimated, budget });
        }
        return Ok(Fitted {
            messages,
            elision: None,
        });
    }
    let mut dropped_turns = 0_usize;
    loop {
        // Indices are within `tail`, which is what makes the count of dropped messages the
        // start of the first kept turn: a dropped turn begins at zero of the conversation.
        let kept = &tail[turns[dropped_turns]..];
        let dropped_messages = u32::try_from(turns[dropped_turns]).unwrap_or(u32::MAX);
        let turns_dropped = u32::try_from(dropped_turns).unwrap_or(u32::MAX);
        let notice = notice_for(dropped_messages, turns_dropped, budget);
        let mut candidate: Vec<Message> = messages[..head].to_vec();
        if let Some(notice) = &notice {
            candidate.push(notice.clone());
        }
        candidate.extend_from_slice(kept);
        let kept_tokens = estimate(&candidate);
        if kept_tokens <= budget {
            let elision = notice.map(|_| Elision {
                dropped_messages,
                dropped_turns: turns_dropped,
                kept_tokens,
                budget,
            });
            return Ok(Fitted {
                messages: candidate,
                elision,
            });
        }
        // The newest turn is the floor. If it does not fit alongside the prompt that cannot be
        // dropped, nothing here fits, and saying so is the point of refusing.
        if dropped_turns.saturating_add(1) >= turns.len() {
            return Err(FitError::TooLarge {
                estimated: kept_tokens,
                budget,
            });
        }
        dropped_turns = dropped_turns.saturating_add(1);
    }
}

/// Returns how many leading messages are not part of any turn.
///
/// The system prompt, in the shape `build_request` assembles. Anything before the first human
/// message belongs to the harness rather than to the conversation, and is kept whatever else
/// goes.
fn leading_prompt(messages: &[Message]) -> usize {
    messages
        .iter()
        .position(|message| matches!(message, Message::User { .. }))
        .unwrap_or(messages.len())
}

/// Returns the index each turn begins at.
///
/// A turn begins at a human message, so a turn dropped whole takes the model's answer, its tool
/// calls, and their results with it, and can never leave a call without the result answering it.
fn split_turns(messages: &[Message]) -> Vec<usize> {
    messages
        .iter()
        .enumerate()
        .filter(|(_, message)| matches!(message, Message::User { .. }))
        .map(|(index, _)| index)
        .collect()
}

/// Builds the notice a trimmed prompt carries, or `None` when nothing was dropped.
///
/// A system message rather than an aside in the newest turn: it is the harness speaking about
/// the conversation, and a model that reads it knows the gap is there rather than inferring a
/// beginning that was never shown.
fn notice_for(dropped_messages: u32, dropped_turns: u32, budget: u32) -> Option<Message> {
    if dropped_messages == 0 {
        return None;
    }
    Some(Message::system(format!(
        "Earlier parts of this conversation are not shown: {dropped_messages} messages \
         ({dropped_turns} turns) were left out because the prompt budget of {budget} estimated \
         tokens was reached. The oldest thing shown is where the drop ends, not the start of \
         the conversation.",
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ToolCall, ToolCallId, ToolName};

    fn tool_name(raw: &str) -> ToolName {
        ToolName::new(raw).unwrap_or_else(|error| panic!("test tool name {raw}: {error}"))
    }

    /// One turn: a question, a tool-using answer, its result, and a final answer.
    fn turn(question: &str, filler: usize) -> Vec<Message> {
        let call = ToolCall::new(
            ToolCallId::new(format!("call-{question}")),
            tool_name("read"),
            serde_json::json!({ "path": "a.txt" }),
        );
        vec![
            Message::user(question),
            Message::assistant(None, None, vec![call.clone()]),
            Message::tool(call.id, "x".repeat(filler), false),
            Message::assistant(Some(format!("answer to {question}")), None, Vec::new()),
        ]
    }

    /// A conversation of `turns` turns, each padded to `filler` characters.
    fn conversation(turns: usize, filler: usize) -> Vec<Message> {
        let mut messages = vec![Message::system("you are nanus")];
        for index in 0..turns {
            messages.extend(turn(&format!("q{index}"), filler));
        }
        messages
    }

    /// A budget that fits everything, and one that fits nothing but the system prompt.
    fn fits_everything(messages: &[Message]) -> u32 {
        estimate(messages)
    }

    #[test]
    fn a_conversation_that_fits_is_sent_untouched() {
        let messages = conversation(3, 40);
        let fitted = fit(messages.clone(), fits_everything(&messages)).expect("it fits");
        assert_eq!(
            fitted.messages, messages,
            "nothing is changed when nothing must be"
        );
        assert!(fitted.elision.is_none());
        // The boundary is inclusive: exactly the budget fits untouched, and one token less
        // trims rather than failing — the conversation *can* be sent, one turn shorter.
        let exact = estimate(&messages);
        assert!(
            fit(messages.clone(), exact)
                .expect("fits")
                .elision
                .is_none()
        );
        let trimmed = fit(messages, exact.saturating_sub(1)).expect("one turn shorter fits");
        assert!(trimmed.elision.is_some());
    }

    /// The oldest turns go first, the newest turn stays, and the system prompt is never dropped.
    #[test]
    fn the_oldest_turns_are_dropped_and_the_newest_is_kept() {
        let messages = conversation(6, 400);
        // Room for the prompt and roughly two turns.
        let budget = estimate(&messages[..9]);
        let fitted = fit(messages, budget).expect("it fits once trimmed");
        let elision = fitted.elision.expect("something was dropped");
        assert!(elision.dropped_turns > 0, "{elision:?}");
        assert!(elision.kept_tokens <= budget, "{elision:?}");

        // The prompt is still first, and the newest turn is still there in full.
        assert!(matches!(
            fitted.messages.first(),
            Some(Message::System { .. })
        ));
        let last = fitted.messages.last().and_then(Message::text);
        assert_eq!(last, Some("answer to q5"), "{:?}", fitted.messages);
        assert!(
            fitted
                .messages
                .iter()
                .any(|message| message.text() == Some("q5"))
        );
        // The oldest question is gone, and so is everything that answered it.
        assert!(
            !fitted
                .messages
                .iter()
                .any(|message| message.text() == Some("q0"))
        );
    }

    /// A dropped turn takes its tool call *and* the result answering it: a call with no result
    /// is a request every provider refuses.
    #[test]
    fn a_dropped_turn_takes_its_tool_calls_with_it() {
        let messages = conversation(5, 300);
        let budget = estimate(&messages[..13]);
        let fitted = fit(messages, budget).expect("it fits once trimmed");
        let mut calls = 0_u32;
        let mut results = 0_u32;
        for message in &fitted.messages {
            match message {
                Message::Assistant { tool_calls, .. } => {
                    calls = calls.saturating_add(u32::try_from(tool_calls.len()).unwrap_or(0));
                }
                Message::Tool { .. } => results = results.saturating_add(1),
                _ => {}
            }
        }
        assert_eq!(calls, results, "every surviving call has its result");
        assert_eq!(
            calls, 2,
            "two of the five turns survived: {:?}",
            fitted.messages
        );
    }

    /// The model is told, at the point where the gap is, and the report says the same thing.
    #[test]
    fn a_trimmed_prompt_carries_a_notice_and_a_report() {
        let messages = conversation(4, 400);
        let budget = estimate(&messages[..9]);
        let fitted = fit(messages, budget).expect("it fits once trimmed");
        let elision = fitted.elision.expect("something was dropped");

        let notice = fitted
            .messages
            .iter()
            .find(|message| {
                message
                    .text()
                    .is_some_and(|text| text.contains("not shown"))
            })
            .expect("the model is told");
        // Between the prompt and the oldest thing kept, which is where the gap is.
        assert!(matches!(notice, Message::System { .. }));
        assert_eq!(
            fitted.messages.get(1).and_then(Message::text),
            notice.text(),
            "the notice sits where the drop ends"
        );
        // The report agrees with the notice about what was left out.
        let text = notice.text().unwrap_or_default();
        assert!(
            text.contains(&elision.dropped_messages.to_string()),
            "{text} / {elision:?}"
        );
        assert!(
            text.contains(&elision.budget.to_string()),
            "{text} / {elision:?}"
        );
    }

    /// The notice costs tokens, and the policy pays for it: a prompt that only fits without one
    /// drops another turn rather than quietly exceeding the budget.
    #[test]
    fn the_notice_is_counted_against_the_budget() {
        let messages = conversation(6, 100);
        let full = estimate(&messages);
        // Tighten the budget until trimming starts, then check every step below it: whatever the
        // policy sent is inside the budget, and it never sends a prompt that is over.
        let trimmed = (1..full).find_map(|cut| {
            let budget = full.saturating_sub(cut);
            let fitted = fit(messages.clone(), budget).ok()?;
            fitted.elision.map(|_| (budget, fitted))
        });
        let (budget, fitted) = trimmed.expect("some budget trims this conversation");
        assert!(
            estimate(&fitted.messages) <= budget,
            "the sent prompt is inside the budget, notice included: {} > {budget}",
            estimate(&fitted.messages)
        );
    }

    /// The same input gives the same answer, which is what makes two runs comparable.
    #[test]
    fn fitting_is_deterministic() {
        let messages = conversation(7, 200);
        let budget = estimate(&messages[..17]);
        let once = fit(messages.clone(), budget).expect("fits");
        let twice = fit(messages, budget).expect("fits");
        assert_eq!(once, twice);
    }

    /// When the newest turn alone does not fit, the turn is refused rather than answered from a
    /// conversation the model never saw.
    #[test]
    fn a_prompt_that_cannot_be_shortened_is_refused() {
        let messages = conversation(3, 400);
        let error = fit(messages, 10).expect_err("it cannot fit");
        assert!(matches!(error, FitError::TooLarge { .. }));
        // And the sentence names the knob, because a refusal a reader cannot act on is noise.
        assert!(error.to_string().contains("context_budget"), "{error}");
    }

    /// A prompt with no human turn is measured rather than trimmed: there is nothing to drop.
    #[test]
    fn a_prompt_with_no_turn_is_measured_not_trimmed() {
        let just_a_prompt = vec![Message::system("you are nanus")];
        let fitted = fit(just_a_prompt.clone(), 1_000).expect("fits");
        assert!(fitted.elision.is_none());
        assert_eq!(fitted.messages, just_a_prompt);
    }

    /// An enormous message reports an enormous estimate rather than wrapping to something that
    /// reads as "it fits".
    #[test]
    fn the_estimate_saturates() {
        let huge = Message::user("x".repeat(100_000));
        assert!(estimate_message(&huge) > 20_000);
        assert!(estimate(&[huge]) < u32::MAX);
    }
}
