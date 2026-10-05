//! One stream assembly path; opted-in record admission validates before replay joins.
use super::{AgentRunner, Assembled, Progress, is_cancelled, until_cancelled};
use crate::BundleError;
use nanus_ports::{LlmEvent, TurnControl};

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
