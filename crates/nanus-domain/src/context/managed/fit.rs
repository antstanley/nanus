//! Deterministic hard fitting for managed requests.
//!
//! Fitting acts only when the candidate is over the hard allowance, and only on eligible
//! fragments — never on a user message. It hides the oldest eligible complete fragment first,
//! in event order; once the candidate is under the ceiling it keeps going toward the 60 % target
//! while eligible fragments remain, so the next few steps do not each pay for a fit of their
//! own. A protected floor above the target but within the allowance is a success; a floor above
//! the allowance is a refusal, and no partial hidden set is installed for it.
//!
//! The cost function is the selected adapter's estimate of the exact candidate, notice and
//! catalog included, so every probe is measured the way the request would be sent. Probes are
//! placed by binary search over how many of the oldest eligible fragments are hidden. Hiding
//! one more fragment removes its messages and adds at most one catalog id, so the cost falls as
//! the count rises; the search's answer is still checked, never assumed.

use std::collections::BTreeSet;

use super::fragments::Fragments;
use super::ids::{ErrorCode, FragmentId};
use super::limits;

/// What a fit decided.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fitted {
    /// The full hidden set to install.
    pub hidden: Vec<FragmentId>,
    /// The candidate's estimate under it.
    pub estimate: u32,
}

/// The allowance a fit aims for once it has to act.
#[must_use]
pub fn target(allowance: u32) -> u32 {
    let target = u64::from(allowance)
        .saturating_mul(u64::from(limits::TARGET_PERCENT))
        .checked_div(100)
        .unwrap_or(0);
    u32::try_from(target).unwrap_or(allowance)
}

/// Fits the candidate under `allowance`, or reports that it already fits.
///
/// `Ok(None)` means the accepted selection fits and nothing changes.
///
/// # Errors
///
/// Returns [`ErrorCode::ProtectedFloorTooLarge`] when hiding every eligible fragment does not
/// fit, [`ErrorCode::StorageCapacity`] when fitting would pass the hidden-id cap, and any error
/// the cost function reports.
pub fn hard_fit(
    fragments: &Fragments,
    protected: &BTreeSet<FragmentId>,
    accepted: &[FragmentId],
    allowance: u32,
    mut cost: impl FnMut(&[FragmentId]) -> Result<u32, ErrorCode>,
) -> Result<Option<Fitted>, ErrorCode> {
    let current = cost(accepted)?;
    if current <= allowance {
        return Ok(None);
    }
    let eligible: Vec<FragmentId> = fragments
        .all()
        .iter()
        .filter(|fragment| fragment.settled)
        .map(|fragment| fragment.id)
        .filter(|id| !protected.contains(id) && !accepted.contains(id))
        .collect();
    let with = |count: usize| -> Vec<FragmentId> {
        let mut hidden: Vec<FragmentId> = accepted.to_vec();
        hidden.extend(eligible.iter().take(count).copied());
        hidden.sort_unstable();
        hidden
    };
    let floor = cost(&with(eligible.len()))?;
    if floor > allowance {
        return Err(ErrorCode::ProtectedFloorTooLarge);
    }
    let crossing = first_within(eligible.len(), allowance, |count| cost(&with(count)))?;
    let aim = target(allowance);
    let settled = first_within(eligible.len(), aim, |count| cost(&with(count)))?;
    // The hidden-id cap bounds how far toward the target fitting goes, not whether it fits:
    // only a crossing that itself needs more ids than the cap allows is refused.
    let room = limits::HIDDEN_MAX.saturating_sub(accepted.len());
    if crossing > room {
        return Err(ErrorCode::StorageCapacity);
    }
    let count = settled.max(crossing).min(room);
    let hidden = with(count);
    let estimate = cost(&hidden)?;
    if estimate > allowance {
        // The search assumes the cost falls as fragments are hidden. A candidate that breaks
        // that is refused rather than sent over the limit.
        return Err(ErrorCode::CandidateTooLarge);
    }
    Ok(Some(Fitted { hidden, estimate }))
}

/// The smallest count in `1..=len` whose cost is within `bound`, or `len` when none is.
fn first_within(
    len: usize,
    bound: u32,
    mut cost: impl FnMut(usize) -> Result<u32, ErrorCode>,
) -> Result<usize, ErrorCode> {
    let (mut low, mut high) = (1_usize, len);
    if len == 0 {
        return Ok(0);
    }
    if cost(high)? > bound {
        return Ok(len);
    }
    while low < high {
        let middle = low.saturating_add(high.saturating_sub(low).checked_div(2).unwrap_or(0));
        if cost(middle)? <= bound {
            high = middle;
        } else {
            low = middle.saturating_add(1);
        }
    }
    Ok(low)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::managed::fragments::derive;
    use crate::session::{SessionEvent, SessionLog};
    use crate::{ToolCall, ToolCallId, ToolName};
    use serde_json::json;

    /// A log of `count` fragments, each one call and its result.
    fn log(count: usize) -> SessionLog {
        let mut log = SessionLog::new();
        log.append(SessionEvent::UserMessage { text: "go".into() });
        for index in 0..count {
            let id = format!("c{index}");
            log.append(SessionEvent::AssistantMessage {
                replay: None,
                text: None,
                reasoning: None,
                tool_calls: vec![ToolCall::new(
                    ToolCallId::new(id.clone()),
                    ToolName::new("read").unwrap_or_else(|_| unreachable!("valid")),
                    json!({}),
                )],
                usage: None,
                interrupted: false,
                model: None,
                effort: None,
            });
            log.append(SessionEvent::ToolResult {
                call_id: ToolCallId::new(id),
                content: "x".into(),
                content_blocks: None,
                is_error: false,
            });
        }
        log
    }

    /// Ten tokens per visible fragment, plus a hundred for the rest of the request.
    fn cost(total: usize) -> impl FnMut(&[FragmentId]) -> Result<u32, ErrorCode> {
        move |hidden| {
            let shown = total.saturating_sub(hidden.len());
            Ok(100_u32.saturating_add(u32::try_from(shown).unwrap_or(0).saturating_mul(10)))
        }
    }

    #[test]
    fn a_fitting_candidate_is_left_alone() {
        let fragments = derive(&log(5)).unwrap_or_default();
        let fit = hard_fit(&fragments, &BTreeSet::new(), &[], 150, cost(5));
        assert_eq!(fit, Ok(None));
    }

    #[test]
    fn the_oldest_eligible_fragments_go_first_and_fitting_continues_to_the_target() {
        let fragments = derive(&log(10)).unwrap_or_default();
        let protected: BTreeSet<FragmentId> =
            fragments.all().iter().rev().take(2).map(|f| f.id).collect();
        // 200 is over 190; the target is 60 % of 190 = 114, which needs eight hidden — but two
        // are protected, so the floor of 120 is where it stops, inside the allowance.
        let fit = hard_fit(&fragments, &protected, &[], 190, cost(10));
        let Ok(Some(fitted)) = fit else {
            panic!("it fits: {fit:?}");
        };
        assert_eq!(fitted.hidden.len(), 8);
        assert_eq!(fitted.estimate, 120);
        assert_eq!(
            fitted.hidden.first(),
            Some(&fragments.all()[0].id),
            "oldest first"
        );
        assert!(fitted.hidden.iter().all(|id| !protected.contains(id)));
        assert!(fitted.hidden.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn the_target_is_reached_when_it_can_be() {
        let fragments = derive(&log(20)).unwrap_or_default();
        // 300 is over 250; the target is 150, which hiding fifteen reaches exactly.
        let fit = hard_fit(&fragments, &BTreeSet::new(), &[], 250, cost(20));
        assert_eq!(
            fit.map(|fit| fit.map(|fitted| fitted.hidden.len())),
            Ok(Some(15))
        );
    }

    #[test]
    fn a_protected_floor_over_the_allowance_is_refused_whole() {
        let fragments = derive(&log(3)).unwrap_or_default();
        let protected: BTreeSet<FragmentId> = fragments.all().iter().map(|f| f.id).collect();
        let fit = hard_fit(&fragments, &protected, &[], 110, cost(3));
        assert_eq!(fit, Err(ErrorCode::ProtectedFloorTooLarge));
    }

    /// F16: the hidden-id cap stops fitting short of the target rather than refusing a fit
    /// that exists inside it.
    #[test]
    fn the_hidden_cap_limits_how_far_fitting_goes_not_whether_it_fits() {
        let fragments = derive(&log(4_102)).unwrap_or_default();
        let accepted: Vec<FragmentId> = fragments.all().iter().take(4_090).map(|f| f.id).collect();
        // 12 shown cost 220 against an allowance of 215: one more hidden crosses; the target
        // would want ten, and only six fit under the cap.
        let fit = hard_fit(&fragments, &BTreeSet::new(), &accepted, 215, cost(4_102));
        let Ok(Some(fitted)) = fit else {
            panic!("it fits inside the cap: {fit:?}");
        };
        assert_eq!(fitted.hidden.len(), limits::HIDDEN_MAX);
        assert_eq!(fitted.estimate, 160);
        let full: Vec<FragmentId> = fragments.all().iter().take(4_096).map(|f| f.id).collect();
        assert_eq!(
            hard_fit(&fragments, &BTreeSet::new(), &full, 150, cost(4_102)),
            Err(ErrorCode::StorageCapacity),
            "a crossing that needs more ids than the cap allows is refused"
        );
    }

    #[test]
    fn the_target_is_sixty_percent() {
        assert_eq!(target(1_000), 600);
        assert_eq!(target(0), 0);
        assert!(target(u32::MAX) < u32::MAX);
    }
}
