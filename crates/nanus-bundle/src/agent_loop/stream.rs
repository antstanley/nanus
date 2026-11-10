//! One stream assembly path; opted-in record admission validates before replay joins.
use super::{AgentRunner, Assembled, Progress, is_cancelled, until_cancelled};
use crate::BundleError;
use nanus_domain::StepOutcome;
use nanus_ports::{FinishReason, LlmEvent, TurnControl};

/// The most of a provider's own finish reason that is quoted back in a failure.
const QUOTED_REASON_CHARS: usize = 64;

/// Whether a response ended because the model was done, rather than because something cut it.
///
/// `Stop` and `ToolCalls` are the model's own endings. Everything else is somebody else's: the
/// token ceiling, a content filter, or a reason this build has no name for — and a reason with no
/// name cannot be vouched for as an ending, which is the same rule a session's absent fields
/// follow: unknown is not "default".
pub(super) const fn ended_itself(finish: &FinishReason) -> bool {
    matches!(finish, FinishReason::Stop | FinishReason::ToolCalls)
}

/// What a step that asked for no tool amounts to, read from why its response ended.
///
/// Only the model's own ending is a final answer. The token ceiling has an outcome of its own. A
/// content filter or an unrecognised reason closes the turn as a failure that says which, so the
/// words that did arrive stay in the log and on the reader's screen but the run is not reported
/// as complete: `nanus run` exits zero only for a completed turn, and an answer a filter cut in
/// half is not one. This used to be read as "anything but the ceiling is an answer", which is
/// how a refused or filtered response exited zero.
pub(super) fn answer_outcome(finish: &FinishReason) -> StepOutcome {
    match finish {
        FinishReason::Stop | FinishReason::ToolCalls => StepOutcome::FinalAnswer,
        FinishReason::Length => StepOutcome::MaxTokens,
        FinishReason::ContentFilter => StepOutcome::Error {
            message: String::from(
                "the response was stopped by the provider's content filter, so what arrived is \
                 not a complete answer",
            ),
        },
        FinishReason::Unknown(raw) => {
            // The provider's text is quoted, so it is bounded and kept on one line.
            let quoted: String = raw
                .chars()
                .take(QUOTED_REASON_CHARS)
                .map(|character| {
                    if character.is_control() {
                        ' '
                    } else {
                        character
                    }
                })
                .collect();
            StepOutcome::Error {
                message: format!(
                    "the response ended for a reason this build does not know (`{quoted}`), so \
                     it is not reported as a complete answer"
                ),
            }
        }
    }
}

impl AgentRunner {
    /// Consumes a model stream into an assembled assistant turn.
    pub(super) async fn consume_stream(
        &self,
        stream: &mut nanus_ports::LlmStream,
        progress: &mut dyn Progress,
        control: Option<&dyn TurnControl>,
        limits: nanus_ports::ToolArgumentLimits,
    ) -> Result<Assembled, BundleError> {
        use futures::StreamExt as _;

        let mut assembled = Assembled {
            limits,
            ..Assembled::default()
        };
        loop {
            let next = until_cancelled(control, stream.next()).await;
            let Some(next) = next else {
                assembled.interrupt();
                break;
            };
            let Some(event) = next else { break };
            // A stop asked for while the model is streaming is taken at the next token
            // rather than at the end of the response: waiting out a long answer to a
            // question nobody wants answered any more is the whole thing the reader is
            // trying to avoid. What the model has already said is kept and marked
            // interrupted, because a conversation that forgets words the reader watched
            // arrive is worse than one that keeps them — but the tool calls it was part way
            // through naming are *dropped*: a call in the log with no result to answer it
            // would be replayed as one that ran, and the next request would be refused for
            // a call nothing ever answered.
            if is_cancelled(progress, control) {
                assembled.interrupt();
                break;
            }
            absorb_event(&mut assembled, progress, event, self.records.is_some())?;
        }
        // Asked once more now the stream has ended, which is the last moment before this step's
        // tool calls would run. A stop that arrives during the *closing* await — the end of the
        // response body, after the last event — is not seen by the check inside the loop,
        // because there is no next event to see it at, and running a command the reader has
        // just asked to stop is the one thing stopping is for.
        if is_cancelled(progress, control) {
            assembled.interrupt();
        }
        assembled.settle();
        if self.records.is_none()
            && let Some(replay) = &assembled.replay
        {
            replay
                .validate_response(Some(&assembled.text), &assembled.calls)
                .map_err(|error| BundleError::Model(error.to_string()))?;
        }
        Ok(assembled)
    }
}

fn absorb_event(
    assembled: &mut Assembled,
    progress: &mut dyn Progress,
    event: LlmEvent,
    record_admission: bool,
) -> Result<(), BundleError> {
    match event {
        LlmEvent::TextDelta(delta) => {
            progress.text(&delta);
            assembled.text.push_str(&delta);
        }
        LlmEvent::ReasoningDelta(delta) => {
            progress.reasoning(&delta);
            assembled.reasoning.push_str(&delta);
        }
        LlmEvent::ToolCallDelta {
            id,
            name,
            arguments_delta,
            ..
        } => {
            progress.tool_call(&arguments_delta);
            assembled.absorb(id, name, &arguments_delta);
        }
        LlmEvent::AssistantReplay(replay) => {
            // Responses shape validation parses raw function arguments. With admission,
            // the host must count the moved original record before that allocation too.
            if !record_admission {
                replay
                    .validate()
                    .map_err(|error| BundleError::Model(error.to_string()))?;
            }
            assembled.replay = Some(replay);
        }
        LlmEvent::ResponseHead => progress.response_head(),
        LlmEvent::Usage(usage) => {
            progress.usage(&usage);
            assembled.usage = Some(usage);
        }
        LlmEvent::Finished { reason } => assembled.finish = reason,
        LlmEvent::Error(message) => {
            // The failure is recorded as a turn-level error so the session
            // explains why it stopped, rather than appearing truncated.
            return Err(BundleError::Model(message));
        }
    }
    Ok(())
}
