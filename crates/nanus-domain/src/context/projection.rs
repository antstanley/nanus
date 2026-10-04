//! Exact whole-turn projection checks. A textual notice alone grants no elision authority.
use crate::{Message, content::ContentError};

/// Counts/budget describing the deterministic fitted projection of an immutable source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContextProjection {
    /// Original fitting budget in estimated tokens.
    pub budget: u32,
    /// Complete oldest human turns omitted.
    pub dropped_turns: u32,
    /// Original messages those turns contained.
    pub dropped_messages: u32,
}

fn refused() -> ContentError {
    ContentError::new("invalid whole-turn context projection")
}

/// Verifies a candidate against the complete source and exact fitting notice.
/// # Errors
/// Refuses rewritten messages, partial turns, a changed prompt/notice or omission of the newest turn.
pub fn identify(
    source: &[Message],
    candidate: &[Message],
    budget: u32,
) -> Result<ContextProjection, ContentError> {
    projection(source, candidate, budget, source.len())
}

/// Checks the same projection for pure estimation of final complete-batch result values.
///
/// Call ids/order, assistant calls, users, prompts and all prior observations remain exact.
/// This permits no dispatch or mutation of the original source.
/// # Errors
/// Refuses substitutions outside the final balanced assistant/tool batch or invalid elision.
pub fn identify_results(
    source: &[Message],
    candidate: &[Message],
    budget: u32,
) -> Result<ContextProjection, ContentError> {
    projection(source, candidate, budget, result_tail(source))
}

fn result_tail(source: &[Message]) -> usize {
    let Some(index) = source
        .iter()
        .rposition(|message| matches!(message, Message::Assistant { .. }))
    else {
        return source.len();
    };
    let Message::Assistant { tool_calls, .. } = &source[index] else {
        return source.len();
    };
    let Some(start) = index.checked_add(1) else {
        return source.len();
    };
    if tool_calls.is_empty() || source.len().checked_sub(start) != Some(tool_calls.len()) {
        return source.len();
    }
    if tool_calls
        .iter()
        .zip(&source[start..])
        .all(|(call, message)| matches!(message,Message::Tool {call_id,..} if call_id==&call.id))
    {
        start
    } else {
        source.len()
    }
}

fn matching(source: &[Message], candidate: &[Message], offset: usize, tail: usize) -> bool {
    source.len()==candidate.len() && source.iter().zip(candidate).enumerate().all(|(index,(a,b))|
        if offset.checked_add(index).is_some_and(|index|index>=tail) {
            matches!((a,b),(Message::Tool {call_id:a,..},Message::Tool {call_id:b,..}) if a==b)
        } else {a==b})
}

fn projection(
    source: &[Message],
    candidate: &[Message],
    budget: u32,
    tail: usize,
) -> Result<ContextProjection, ContentError> {
    if budget == 0 {
        return Err(refused());
    }
    if matching(source, candidate, 0, tail) {
        return Ok(ContextProjection {
            budget,
            dropped_turns: 0,
            dropped_messages: 0,
        });
    }
    let head = super::leading_prompt(source);
    let dropped = source
        .len()
        .checked_add(1)
        .and_then(|v| v.checked_sub(candidate.len()))
        .filter(|v| *v > 0)
        .ok_or_else(refused)?;
    let cut = head
        .checked_add(dropped)
        .filter(|v| *v < source.len())
        .ok_or_else(refused)?;
    let kept_head = head
        .checked_add(1)
        .filter(|v| *v < candidate.len())
        .ok_or_else(refused)?;
    if !matches!(source[cut], Message::User { .. })
        || source[..head] != candidate[..head]
        || !matching(&source[cut..], &candidate[kept_head..], cut, tail)
    {
        return Err(refused());
    }
    let turns = source[head..cut]
        .iter()
        .filter(|message| matches!(message, Message::User { .. }))
        .count();
    let dropped_turns = u32::try_from(turns)
        .ok()
        .filter(|v| *v > 0)
        .ok_or_else(refused)?;
    let dropped_messages = u32::try_from(dropped).map_err(|_| refused())?;
    let notice = super::notice_for(dropped_messages, dropped_turns, budget).ok_or_else(refused)?;
    if candidate[head] != notice {
        return Err(refused());
    }
    Ok(ContextProjection {
        budget,
        dropped_turns,
        dropped_messages,
    })
}
