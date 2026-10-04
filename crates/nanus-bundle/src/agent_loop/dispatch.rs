//! Keep approval, goal effects and chunked registered work separate from admission.
use nanus_domain::{Session, ToolCall, ToolResult};
use nanus_ports::{ToolBatchReservation, TurnControl};

use super::{AgentRunner, Approver, Progress, interrupted_result, is_cancelled, until_cancelled};

pub(super) struct Phase<'a> {
    pub(super) position: (u32, u32),
    pub(super) approver: Option<&'a dyn Approver>,
    pub(super) control: Option<&'a dyn TurnControl>,
}
#[derive(Clone, Copy)]
pub(super) struct Dispatch<'a> {
    pub(super) control: Option<&'a dyn TurnControl>,
    pub(super) reservation: Option<&'a dyn ToolBatchReservation>,
}

impl AgentRunner {
    pub(super) async fn execute_permitted(
        &self,
        session: &mut Session,
        calls: &[ToolCall],
        results: &mut [Option<ToolResult>],
        progress: &mut dyn Progress,
        context: Dispatch<'_>,
    ) {
        assert_eq!(calls.len(), results.len());
        let (goals, registry): (Vec<usize>, Vec<usize>) = (0..calls.len())
            .filter(|&i| results[i].is_none())
            .partition(|&i| crate::goal_tools::is_goal_tool(&calls[i].name));
        self.execute_goals(session, calls, (&goals, results), progress, context);
        self.execute_registry(calls, &registry, results, progress, context)
            .await;
        assert!(results.iter().all(Option::is_some));
    }

    pub(super) async fn gate_batch(
        &self,
        calls: &[ToolCall],
        progress: &mut dyn Progress,
        approver: Option<&dyn Approver>,
        control: Option<&dyn TurnControl>,
    ) -> Vec<Option<ToolResult>> {
        let mut results = Vec::with_capacity(calls.len());
        for call in calls {
            let denial = if is_cancelled(progress, control) {
                Some(interrupted_result(call))
            } else {
                self.gate(call, approver, control).await
            };
            results.push(denial.map(|result| self.finish_result(call, result, progress)));
        }
        assert_eq!(results.len(), calls.len());
        results
    }

    pub(super) fn execute_goals(
        &self,
        session: &mut Session,
        calls: &[ToolCall],
        outputs: (&[usize], &mut [Option<ToolResult>]),
        progress: &mut dyn Progress,
        context: Dispatch<'_>,
    ) {
        let (indexes, results) = outputs;
        let Dispatch {
            control,
            reservation,
        } = context;
        assert_eq!(calls.len(), results.len());
        for &index in indexes {
            let call = &calls[index];
            let result = if is_cancelled(progress, control) {
                interrupted_result(call)
            } else if let Some(refused) = super::admission::before_dispatch(call, reservation) {
                refused
            } else if control.is_some_and(TurnControl::is_cancelled) {
                interrupted_result(call)
            } else {
                self.run_goal_tool(session, call, progress)
            };
            results[index] = Some(self.finish_admitted(call, result, progress, reservation));
        }
        assert!(indexes.iter().all(|&index| results[index].is_some()));
    }

    pub(super) async fn execute_registry(
        &self,
        calls: &[ToolCall],
        indexes: &[usize],
        results: &mut [Option<ToolResult>],
        progress: &mut dyn Progress,
        context: Dispatch<'_>,
    ) {
        let Dispatch {
            control,
            reservation,
        } = context;
        assert_eq!(calls.len(), results.len());
        for batch in indexes.chunks(self.parallel_limit()) {
            // Static futures own their work. Never retain a registry borrow across an await.
            let running = batch.iter().map(|index| async {
                let call = &calls[*index];
                if control.is_some_and(TurnControl::is_cancelled) {
                    return interrupted_result(call);
                }
                if let Some(refused) = super::admission::before_dispatch(call, reservation) {
                    return refused;
                }
                if control.is_some_and(TurnControl::is_cancelled) {
                    return interrupted_result(call);
                }
                let work = self.tools.borrow().execute(call.clone());
                until_cancelled(control, work)
                    .await
                    .unwrap_or_else(|| interrupted_result(call))
            });
            let finished = if is_cancelled(progress, control) {
                batch
                    .iter()
                    .map(|index| interrupted_result(&calls[*index]))
                    .collect()
            } else {
                futures::future::join_all(running).await
            };
            for (index, result) in batch.iter().zip(finished) {
                results[*index] =
                    Some(self.finish_admitted(&calls[*index], result, progress, reservation));
            }
        }
        assert!(indexes.iter().all(|&index| results[index].is_some()));
    }
}
