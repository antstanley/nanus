//! Generic host admission around every registered and goal-tool dispatch.
use std::rc::Rc;

use nanus_domain::{Session, SessionEvent, ToolCall, ToolResult};
use nanus_ports::tool_admission::{
    TOOL_ADMISSION_REFUSAL_TEXT, TOOL_ADMISSION_RESULT_REFUSAL_TEXT,
};
use nanus_ports::{ToolAdmission, ToolBatchProjection, ToolBatchReservation};

use super::{AgentRunner, Progress, bounded_result};
use crate::BundleError;

pub(super) struct Unreserved<'a> {
    pub(super) admission: Option<Rc<dyn ToolAdmission>>,
    pub(super) calls: &'a [ToolCall],
}
impl Drop for Unreserved<'_> {
    fn drop(&mut self) {
        if let Some(admission) = &self.admission {
            admission.release_unreserved(self.calls);
        }
    }
}

impl AgentRunner {
    pub(super) fn reserve_batch(
        &self,
        session: &Session,
        position: (u32, u32),
        calls: &[ToolCall],
        results: &[Option<ToolResult>],
    ) -> Result<Option<Box<dyn ToolBatchReservation>>, BundleError> {
        let Some(admission) = &self.admission else {
            return Ok(None);
        };
        assert_eq!(calls.len(), results.len());
        // Replay deliberately omits unanswered calls. A private prospective copy adds
        // every actual denial or fixed reserved failure, preserving replay/call pairing.
        // This never writes fabricated outcomes to the authoritative session.
        let mut prospective = session.clone();
        let base = calls
            .iter()
            .zip(results)
            .map(|(call, result)| Some(result.clone().unwrap_or_else(|| refusal(call))))
            .collect();
        Self::append_results(&mut prospective, calls, base);
        let (fitted_base, _) = self.build_request(&prospective)?;
        let mut request = fitted_base.clone();
        request.messages = vec![nanus_domain::Message::system(self.system_prompt.clone())];
        request.messages.extend(prospective.derive_messages());
        let llm = self.llm();
        let estimate = |request: &nanus_ports::ChatRequest| llm.estimate_request(request);
        let projection = ToolBatchProjection {
            session_id: session.id(),
            position,
            selection_epoch: self.selection.epoch()?,
            capabilities: llm.capabilities(&request.model),
            request: &request,
            fitted_base: &fitted_base,
            events: session.log().events(),
            calls,
            outcomes: results,
            estimate: &estimate,
            managed: None,
        };
        let reservation = admission
            .reserve(&projection)
            .map_err(|error| BundleError::context(error.to_string()))?;
        assert_eq!(projection.calls.len(), projection.outcomes.len());
        Ok(Some(reservation))
    }

    pub(super) fn admit_batch(
        &self,
        calls: &[ToolCall],
        results: &mut [Option<ToolResult>],
        progress: &mut dyn Progress,
        reservation: Option<&dyn ToolBatchReservation>,
    ) {
        let Some(reservation) = reservation else {
            return;
        };
        assert_eq!(calls.len(), results.len());
        for (call, result) in calls.iter().zip(results.iter_mut()) {
            if result.is_none() && reservation.admit(call).is_err() {
                *result =
                    Some(self.finish_admitted(call, refusal(call), progress, Some(reservation)));
            }
        }
        assert_eq!(calls.len(), results.len());
    }

    pub(super) fn finish_admitted(
        &self,
        call: &ToolCall,
        result: ToolResult,
        progress: &mut dyn Progress,
        reservation: Option<&dyn ToolBatchReservation>,
    ) -> ToolResult {
        let raw = result;
        let envelope_refusal = self.hold_to_envelope(call, &raw);
        let candidate = envelope_refusal.as_ref().unwrap_or(&raw);
        let mut result = self.validate_result_images(bounded_result(candidate));
        if reservation.is_some_and(|lease| lease.validate_result(call, &raw, &result).is_err()) {
            result = ToolResult::failure(call.id.clone(), TOOL_ADMISSION_RESULT_REFUSAL_TEXT);
        }
        assert_eq!(
            result.call_id, call.id,
            "a result answers the admitted call"
        );
        progress.tool_finished(&call.id, &call.name, !result.outcome.is_success());
        result
    }

    pub(super) fn append_results(
        session: &mut Session,
        calls: &[ToolCall],
        results: Vec<Option<ToolResult>>,
    ) {
        assert_eq!(calls.len(), results.len());
        assert!(
            results.iter().all(Option::is_some),
            "every call has a result"
        );
        for (call, result) in calls.iter().zip(results) {
            let Some(result) = result else { continue };
            assert_eq!(
                call.id, result.call_id,
                "a result answers the call it names"
            );
            session.append(SessionEvent::ToolResult {
                content: super::render_content(result.outcome.content()),
                content_blocks: Some(result.outcome.content().to_vec()),
                call_id: result.call_id,
                is_error: !result.outcome.is_success(),
            });
        }
    }

    pub(super) fn refuse_unanswered(
        &self,
        calls: &[ToolCall],
        results: &mut [Option<ToolResult>],
        progress: &mut dyn Progress,
    ) {
        assert_eq!(calls.len(), results.len());
        for (call, result) in calls.iter().zip(results.iter_mut()) {
            if result.is_none() {
                *result = Some(self.finish_result(call, refusal(call), progress));
            }
        }
        assert!(results.iter().all(Option::is_some));
    }
}

pub(super) fn refusal(call: &ToolCall) -> ToolResult {
    ToolResult::failure(call.id.clone(), TOOL_ADMISSION_REFUSAL_TEXT)
}

pub(super) fn before_dispatch(
    call: &ToolCall,
    reservation: Option<&dyn ToolBatchReservation>,
) -> Option<ToolResult> {
    reservation.and_then(|lease| lease.before_dispatch(call).err().map(|_| refusal(call)))
}
