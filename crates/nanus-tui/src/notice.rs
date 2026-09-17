//! What the interface says when a turn stopped without finishing.
//!
//! A turn can stop for reasons that are not the model finishing — it can run out of steps, hit
//! its token ceiling, be interrupted, be refused by a policy, or fail — and a reader has to be
//! told which, because the alternative is a transcript that stops mid-sentence and leaves them
//! to work out why.
//!
//! There are two ways the interface learns the reason, and this module exists because of the
//! second. A live turn ends with the link's `Done` frame, which carries a `TurnEnd`. A
//! *recorded* turn ends with a `SessionEvent::TurnEnd`, which carries the domain's
//! [`TurnEndReason`]. Rendering those in two places is how the two drifted: the live view said
//! `the turn stopped at its step budget after 32 steps` and the replay of the same session said
//! nothing at all, so a conversation that reads as finished on screen reads as finished a day
//! later too. One vocabulary, one renderer, two translations.
//!
//! The wire's translation is compiled only where the wire is: this module belongs to the view layer,
//! which is built without the link — a test of the rendering links the view and nothing else — so
//! that half of the pair lives behind the `runtime` feature. The interface's own [`Ending`] and the
//! domain's translation are the same in both builds, because a recorded session has to say why a
//! turn stopped whether or not there is an agent to talk to.

use nanus_domain::TurnEndReason;
#[cfg(feature = "runtime")]
use nanus_link::protocol::TurnEnd;

/// Why a turn ended, in the interface's own vocabulary.
///
/// The two sources list the same reasons in different types because they are different
/// boundaries — one is a wire format with its own stability promises, the other is the domain —
/// and this is where they meet.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Ending {
    /// The model finished and nothing was owed.
    Completed,
    /// The turn hit its step budget.
    MaxSteps,
    /// The model hit its output ceiling.
    MaxTokens,
    /// A person stopped the turn.
    Interrupted,
    /// A person stopped the turn for a recorded reason.
    Aborted(String),
    /// A policy refused to continue.
    Blocked,
    /// The turn failed.
    Failed(String),
}

#[cfg(feature = "runtime")]
impl From<&TurnEnd> for Ending {
    fn from(reason: &TurnEnd) -> Self {
        match reason {
            TurnEnd::Completed => Self::Completed,
            TurnEnd::MaxSteps => Self::MaxSteps,
            TurnEnd::MaxTokens => Self::MaxTokens,
            TurnEnd::Interrupted => Self::Interrupted,
            TurnEnd::Aborted { reason } => Self::Aborted(reason.clone()),
            TurnEnd::Blocked => Self::Blocked,
            TurnEnd::Error { message } => Self::Failed(message.clone()),
        }
    }
}

impl From<&TurnEndReason> for Ending {
    fn from(reason: &TurnEndReason) -> Self {
        match reason {
            TurnEndReason::Completed => Self::Completed,
            TurnEndReason::MaxSteps => Self::MaxSteps,
            TurnEndReason::MaxTokens => Self::MaxTokens,
            TurnEndReason::Interrupted => Self::Interrupted,
            TurnEndReason::Aborted { reason } => Self::Aborted(reason.clone()),
            TurnEndReason::Blocked => Self::Blocked,
            TurnEndReason::Error { message } => Self::Failed(message.clone()),
        }
    }
}

/// The sentence a turn that did not simply finish leaves in the transcript.
///
/// `None` means the turn completed and whatever it said is its answer. Every other ending gets
/// its own phrasing rather than a shared one, because the reason is what a reader acts on: a
/// budget is something to wait for or raise, a token ceiling means the answer is cut off, and a
/// failure is something to read.
#[must_use]
pub fn stopping(ending: &Ending, steps: u32) -> Option<String> {
    match ending {
        Ending::Completed => None,
        Ending::MaxSteps => {
            // A budget that ended after one step is not "after 1 steps", and the notice is read
            // by a person who did not configure the number and has no reason to expect a
            // template that forgot.
            let noun = if steps == 1 { "step" } else { "steps" };
            Some(format!(
                "the turn stopped at its step budget after {steps} {noun}, so the work is \
                 unfinished"
            ))
        }
        Ending::MaxTokens => Some(String::from(
            "the turn stopped at the model's token ceiling, so the answer is cut off",
        )),
        Ending::Interrupted => Some(String::from("the turn was interrupted")),
        Ending::Aborted(reason) => Some(format!("the turn was stopped: {reason}")),
        Ending::Blocked => Some(String::from("the turn was blocked by a policy")),
        Ending::Failed(message) => Some(format!("the turn failed: {message}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every reason a turn can stop for is said in words, and the wording names the reason
    /// rather than the enum. `max_tokens` is a label; "the answer is cut off" is information.
    #[test]
    fn every_ending_that_is_not_a_completion_has_something_to_say() {
        let cases = [
            (Ending::MaxSteps, "step budget"),
            (Ending::MaxTokens, "token ceiling"),
            (Ending::Interrupted, "interrupted"),
            (Ending::Blocked, "policy"),
            (
                Ending::Aborted(String::from("the human said stop")),
                "the human said stop",
            ),
            (
                Ending::Failed(String::from("the model call failed")),
                "the model call failed",
            ),
        ];
        for (ending, expected) in cases {
            let notice = stopping(&ending, 3);
            assert!(
                notice
                    .as_deref()
                    .is_some_and(|text| text.contains(expected)),
                "{ending:?} is reported as {notice:?}, which does not mention {expected:?}"
            );
        }
        assert_eq!(
            stopping(&Ending::Completed, 3),
            None,
            "a turn that finished has nothing to explain"
        );
    }

    #[test]
    fn one_step_is_not_one_steps() {
        let notice = stopping(&Ending::MaxSteps, 1);
        assert!(
            notice
                .as_deref()
                .is_some_and(|text| text.contains("after 1 step,")),
            "{notice:?}"
        );
    }

    /// The two sources say the same thing about the same turn, which is the whole reason this
    /// module exists: a reason that translated differently on one side would put the live and
    /// recorded views back where they started. Compiled only with the link, since it is the only
    /// thing here that names both vocabularies.
    #[cfg(feature = "runtime")]
    #[test]
    fn both_vocabularies_reach_the_same_sentence() {
        let pairs = [
            (TurnEnd::Completed, TurnEndReason::Completed),
            (TurnEnd::MaxSteps, TurnEndReason::MaxSteps),
            (TurnEnd::MaxTokens, TurnEndReason::MaxTokens),
            (TurnEnd::Interrupted, TurnEndReason::Interrupted),
            (TurnEnd::Blocked, TurnEndReason::Blocked),
            (
                TurnEnd::Aborted {
                    reason: String::from("stopped"),
                },
                TurnEndReason::Aborted {
                    reason: String::from("stopped"),
                },
            ),
            (
                TurnEnd::Error {
                    message: String::from("broken"),
                },
                TurnEndReason::Error {
                    message: String::from("broken"),
                },
            ),
        ];
        for (from_link, from_domain) in pairs {
            assert_eq!(
                stopping(&Ending::from(&from_link), 7),
                stopping(&Ending::from(&from_domain), 7),
                "{from_link:?} and {from_domain:?} must read the same"
            );
        }
    }
}
