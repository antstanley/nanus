//! Original record reservations: no fabricated prefix and no caller effect authority.
use nanus_domain::{Elision, Session, SessionEvent, ToolCall, TurnEndReason};
use nanus_ports::record_admission::RECORD_ADMISSION_REFUSAL_TEXT;
use nanus_ports::{
    ChatRequest, ModelRecordProjection, RecordEndProjection, StepRecordProjection,
    StepRecordReservation, TurnRecordProjection, TurnRecordReservation,
};

use super::{AgentRunner, Assembled, Progress, is_cancelled};
use crate::BundleError;

type StepOpening = (
    ChatRequest,
    Option<Elision>,
    Option<Box<dyn StepRecordReservation>>,
);

fn refused() -> BundleError {
    BundleError::context(RECORD_ADMISSION_REFUSAL_TEXT)
}
fn validate_replay(event: &SessionEvent) -> Result<(), BundleError> {
    if let SessionEvent::AssistantMessage {
        replay: Some(replay),
        text,
        tool_calls,
        ..
    } = event
    {
        replay
            .validate_response(text.as_deref(), tool_calls)
            .map_err(|error| BundleError::Model(error.to_string()))?;
    }
    Ok(())
}
fn ending<'a>(session: &'a Session, event: &'a SessionEvent) -> RecordEndProjection<'a> {
    RecordEndProjection {
        session_id: session.id(),
        events: session.log().events(),
        next_sequence: session.log().next_seq(),
        event,
    }
}

impl AgentRunner {
    pub(super) fn reserve_turn_records(
        &self,
        session: &Session,
        turn: u32,
        message: &str,
        progress: &dyn Progress,
        control: Option<&dyn nanus_ports::TurnControl>,
    ) -> Result<Option<Box<dyn TurnRecordReservation>>, BundleError> {
        let Some(admission) = &self.records else {
            return Ok(None);
        };
        if is_cancelled(progress, control) {
            return Err(refused());
        }
        let projection = TurnRecordProjection {
            session_id: session.id(),
            events: session.log().events(),
            next_sequence: session.log().next_seq(),
            turn,
            message,
        };
        let lease = admission.reserve_turn(&projection).map_err(|_| refused())?;
        if is_cancelled(progress, control) {
            return Err(refused());
        }
        assert_eq!(projection.next_sequence, session.log().next_seq());
        Ok(Some(lease))
    }

    pub(super) fn begin_step_records(
        &self,
        session: &mut Session,
        position: (u32, u32),
        turn_lease: Option<&dyn TurnRecordReservation>,
    ) -> Result<StepOpening, BundleError> {
        let (turn, step) = position;
        let Some(turn_lease) = turn_lease else {
            // Preserve stock failure framing: it starts the step before request fitting.
            session.append(SessionEvent::StepStart { turn, step });
            return match self.build_request(session) {
                Ok((request, elision)) => Ok((request, elision, None)),
                Err(error) => {
                    Self::end_step_records(session, position, None)?;
                    Err(error)
                }
            };
        };
        let (fitted_request, elision) = self.build_request(session)?;
        let mut request = fitted_request.clone();
        request.messages = vec![nanus_domain::Message::system(self.system_prompt.clone())];
        request.messages.extend(session.derive_messages());
        let llm = self.llm();
        let estimate = |request: &ChatRequest| llm.estimate_request(request);
        let projection = StepRecordProjection {
            session_id: session.id(),
            events: session.log().events(),
            next_sequence: session.log().next_seq(),
            position,
            selection_epoch: self.selection.epoch()?,
            request: &request,
            fitted_request: &fitted_request,
            effort: self.effort(),
            capabilities: llm.capabilities(&request.model),
            estimate: &estimate,
        };
        let lease = turn_lease
            .reserve_step(&projection)
            .map_err(|_| refused())?;
        assert_eq!(projection.next_sequence, session.log().next_seq());
        session.append(SessionEvent::StepStart { turn, step });
        Ok((fitted_request, elision, Some(lease)))
    }

    pub(super) fn append_model_records(
        &self,
        session: &mut Session,
        position: (u32, u32),
        assembled: Assembled,
        lease: Option<&dyn StepRecordReservation>,
    ) -> Result<Vec<ToolCall>, BundleError> {
        // Move every assembled field into the actual event. Calls/replay/text are not copied
        // until the host has counted this complete record and every future audit copy.
        let event = SessionEvent::AssistantMessage {
            replay: assembled.replay,
            text: if assembled.text.is_empty() {
                None
            } else {
                Some(assembled.text)
            },
            reasoning: if assembled.reasoning.is_empty() {
                None
            } else {
                Some(assembled.reasoning)
            },
            tool_calls: assembled.calls,
            usage: assembled.usage,
            interrupted: assembled.interrupted,
            model: Some(self.model()),
            effort: self.effort().map(|value| value.as_str().to_owned()),
        };
        if let Some(lease) = lease {
            let projection = ModelRecordProjection {
                session_id: session.id(),
                events: session.log().events(),
                next_sequence: session.log().next_seq(),
                position,
                selection_epoch: self.selection.epoch()?,
                assistant: &event,
            };
            lease.validate_model(&projection).map_err(|_| refused())?;
            validate_replay(&event)?;
        }
        let SessionEvent::AssistantMessage { tool_calls, .. } = &event else {
            return Err(refused());
        };
        let calls = tool_calls.clone();
        assert_eq!(calls.len(), tool_calls.len());
        session.append(event);
        assert!(matches!(
            session.log().events().last(),
            Some(SessionEvent::AssistantMessage { .. })
        ));
        Ok(calls)
    }

    pub(super) fn end_step_records(
        session: &mut Session,
        (turn, step): (u32, u32),
        lease: Option<&dyn StepRecordReservation>,
    ) -> Result<(), BundleError> {
        let event = SessionEvent::StepEnd { turn, step };
        if let Some(lease) = lease {
            lease
                .validate_end(&ending(session, &event))
                .map_err(|_| refused())?;
        }
        session.append(event);
        assert!(matches!(
            session.log().events().last(),
            Some(SessionEvent::StepEnd { .. })
        ));
        Ok(())
    }

    pub(super) fn end_turn_records(
        session: &mut Session,
        turn: u32,
        reason: TurnEndReason,
        lease: Option<&dyn TurnRecordReservation>,
    ) -> Result<(), BundleError> {
        let event = SessionEvent::TurnEnd { turn, reason };
        if let Some(lease) = lease {
            lease
                .validate_end(&ending(session, &event))
                .map_err(|_| refused())?;
        }
        session.append(event);
        assert!(session.log().last_turn_end().is_some());
        Ok(())
    }
}
