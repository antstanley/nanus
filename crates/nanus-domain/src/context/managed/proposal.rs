//! Proposals: validation against a complete snapshot, staging, and the revision they become.
//!
//! A proposal names its base three ways — the accepted revision, the frontier it was inspected
//! at, and the snapshot profile's digest — and all three must still hold, once when it is staged
//! and again before its revision is committed. Events appended after the base are a suffix the
//! proposal cannot target; a new user message, goal change or mode change in that suffix makes
//! the proposal stale, because each is a fact the model had not seen when it decided.

use std::collections::BTreeSet;

use super::fragments::Fragments;
use super::ids::{ArtifactId, Digest, ErrorCode, FragmentId};
use super::limits;
use super::notes::{ArtifactFacts, notes_digest, resolve_source, validate_notes};
use super::records::{
    ContextFrontier, ProjectionRevision, RevisionAuthor, RevisionReason, WorkingNote,
};
use super::state::{ManagedState, check_frontier};
use super::tooling::{ContextManageInput, FragmentDescriptor, ManageAction};
use crate::session::{Session, SessionEvent};

/// Everything a proposal is validated against.
pub struct Snapshot<'a> {
    /// The bound session.
    pub session: &'a Session,
    /// Its accepted state.
    pub state: &'a ManagedState,
    /// Its fragments.
    pub fragments: &'a Fragments,
    /// What nothing may hide.
    pub protected: &'a BTreeSet<FragmentId>,
    /// The current snapshot profile's digest.
    pub profile_digest: &'a Digest,
    /// The current goal revision.
    pub goal_revision: Option<u64>,
    /// Published artifacts, by id, from this session's own records.
    pub artifacts: &'a dyn Fn(&ArtifactId) -> Option<ArtifactFacts>,
}

impl core::fmt::Debug for Snapshot<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Snapshot")
            .field("session", &self.session.id())
            .field("revision", &self.state.revision())
            .finish_non_exhaustive()
    }
}

/// A validated proposal, waiting for its step to settle. Not an accepted revision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Staged {
    /// The revision it was made against.
    pub base_revision: u64,
    /// The frontier it was made against.
    pub base_frontier: ContextFrontier,
    /// The profile it was made against.
    pub base_profile_digest: Digest,
    /// The full hidden set it asks for.
    pub hidden: Vec<FragmentId>,
    /// The complete note array it asks for.
    pub notes: Vec<WorkingNote>,
    /// The goal revision its notes bind to.
    pub goal_revision: Option<u64>,
}

/// Validates a proposal against the snapshot and stages it.
///
/// # Errors
///
/// Returns the stable code of the first failed check.
pub fn stage(snapshot: &Snapshot<'_>, input: &ContextManageInput) -> Result<Staged, ErrorCode> {
    if input.action != ManageAction::Propose {
        return Err(ErrorCode::InvalidFragment);
    }
    let (Some(base_revision), Some(base_frontier), Some(profile)) = (
        input.base_revision,
        input.base_frontier.as_ref(),
        input.base_profile_digest.as_ref(),
    ) else {
        return Err(ErrorCode::StaleBase);
    };
    let staged = Staged {
        base_revision,
        base_frontier: base_frontier.clone(),
        base_profile_digest: profile.clone(),
        hidden: next_hidden(snapshot, base_frontier.event_count, input)?,
        notes: input.notes.clone(),
        goal_revision: snapshot.goal_revision,
    };
    recheck(snapshot, &staged)?;
    Ok(staged)
}

/// Re-validates a staged proposal, as the commit path does before checkpointing it.
///
/// # Errors
///
/// Returns the stable code of the first failed check.
pub fn recheck(snapshot: &Snapshot<'_>, staged: &Staged) -> Result<(), ErrorCode> {
    let accepted = snapshot.state.revision();
    if staged.base_revision != accepted || staged.base_frontier.projection_revision != accepted {
        return Err(ErrorCode::StaleBase);
    }
    if &staged.base_profile_digest != snapshot.profile_digest
        || staged.goal_revision != snapshot.goal_revision
    {
        return Err(ErrorCode::StaleBase);
    }
    check_frontier(snapshot.session, &staged.base_frontier)?;
    let base =
        usize::try_from(staged.base_frontier.event_count).map_err(|_| ErrorCode::StaleBase)?;
    let suffix = snapshot
        .session
        .log()
        .events()
        .get(base..)
        .unwrap_or_default();
    if suffix.iter().any(|event| {
        matches!(
            event,
            SessionEvent::UserMessage { .. }
                | SessionEvent::GoalChange { .. }
                | SessionEvent::ContextMode { .. }
                | SessionEvent::ContextRevision { .. }
        )
    }) {
        return Err(ErrorCode::StaleBase);
    }
    for id in &staged.hidden {
        let fragment = snapshot
            .fragments
            .get(*id)
            .ok_or(ErrorCode::InvalidFragment)?;
        if !fragment.settled || fragment.end() > staged.base_frontier.event_count {
            return Err(ErrorCode::InvalidFragment);
        }
        if snapshot.protected.contains(id) {
            return Err(ErrorCode::ProtectedFragment);
        }
    }
    validate_notes(&staged.notes)?;
    for note in &staged.notes {
        for source in &note.sources {
            resolve_source(
                snapshot.session.log(),
                staged.base_frontier.event_count,
                source,
                snapshot.artifacts,
            )?;
        }
    }
    Ok(())
}

/// Applies the hide and restore deltas to the accepted hidden set.
fn next_hidden(
    snapshot: &Snapshot<'_>,
    base_count: u64,
    input: &ContextManageInput,
) -> Result<Vec<FragmentId>, ErrorCode> {
    let mut hidden: BTreeSet<FragmentId> = snapshot.state.hidden().iter().copied().collect();
    for id in &input.restore {
        if !hidden.remove(id) {
            return Err(ErrorCode::InvalidFragment);
        }
    }
    for id in &input.hide {
        let fragment = snapshot
            .fragments
            .get(*id)
            .ok_or(ErrorCode::InvalidFragment)?;
        if !fragment.settled || fragment.end() > base_count || !hidden.insert(*id) {
            return Err(ErrorCode::InvalidFragment);
        }
        if snapshot.protected.contains(id) {
            return Err(ErrorCode::ProtectedFragment);
        }
    }
    if hidden.len() > limits::HIDDEN_MAX {
        return Err(ErrorCode::StorageCapacity);
    }
    Ok(hidden.into_iter().collect())
}

/// Builds the revision a staged proposal becomes, with the host's revision and decision ids.
///
/// # Errors
///
/// Returns [`ErrorCode::StaleBase`] when the revision counter is exhausted.
pub fn revision_from(
    staged: &Staged,
    decision_id: String,
) -> Result<ProjectionRevision, ErrorCode> {
    let revision = staged
        .base_revision
        .checked_add(1)
        .ok_or(ErrorCode::StaleBase)?;
    Ok(ProjectionRevision {
        revision,
        base_revision: staged.base_revision,
        base_frontier: staged.base_frontier.clone(),
        hidden: staged.hidden.clone(),
        notes_digest: notes_digest(&staged.notes),
        notes: staged.notes.clone(),
        author: RevisionAuthor::Model,
        reason: RevisionReason::ModelProposal,
        policy_version: limits::POLICY_VERSION,
        decision_id,
        goal_revision: staged.goal_revision,
        base_profile_digest: staged.base_profile_digest.clone(),
    })
}

/// Builds the revision an automatic fit becomes: the accepted notes kept with their binding.
///
/// # Errors
///
/// Returns [`ErrorCode::StaleBase`] when the revision counter is exhausted.
pub fn automatic_revision(
    state: &ManagedState,
    base_frontier: ContextFrontier,
    hidden: Vec<FragmentId>,
    profile: Digest,
    decision_id: String,
) -> Result<ProjectionRevision, ErrorCode> {
    let base_revision = state.revision();
    let revision = state
        .max_revision
        .max(base_revision)
        .checked_add(1)
        .ok_or(ErrorCode::StaleBase)?;
    let notes = state.notes().to_vec();
    Ok(ProjectionRevision {
        revision,
        base_revision,
        base_frontier,
        hidden,
        notes_digest: notes_digest(&notes),
        notes,
        author: RevisionAuthor::Automatic,
        reason: RevisionReason::HardFit,
        policy_version: limits::POLICY_VERSION,
        decision_id,
        // The binding is preserved, never restamped: fitting does not make stale notes fresh.
        goal_revision: state.notes_goal_revision(),
        base_profile_digest: profile,
    })
}

/// Builds the empty revision a reset appends.
///
/// # Errors
///
/// Returns [`ErrorCode::StaleBase`] when the revision counter is exhausted.
pub fn reset_revision(
    highest: u64,
    base_revision: u64,
    base_frontier: ContextFrontier,
    profile: Digest,
    decision_id: String,
) -> Result<ProjectionRevision, ErrorCode> {
    let revision = highest.checked_add(1).ok_or(ErrorCode::StaleBase)?;
    Ok(ProjectionRevision {
        revision,
        base_revision,
        base_frontier,
        hidden: Vec::new(),
        notes: Vec::new(),
        notes_digest: notes_digest(&[]),
        author: RevisionAuthor::Host,
        reason: RevisionReason::Reset,
        policy_version: limits::POLICY_VERSION,
        decision_id,
        goal_revision: None,
        base_profile_digest: profile,
    })
}

/// Describes every fragment for the inspect catalog, oldest first.
#[must_use]
pub fn describe(snapshot: &Snapshot<'_>) -> Vec<FragmentDescriptor> {
    let hidden: BTreeSet<FragmentId> = snapshot.state.hidden().iter().copied().collect();
    snapshot
        .fragments
        .all()
        .iter()
        .map(|fragment| FragmentDescriptor {
            id: fragment.id,
            assistant_seq: fragment.assistant_seq,
            last_result_seq: fragment.result_seqs.last().copied(),
            protected: snapshot.protected.contains(&fragment.id),
            hidden: hidden.contains(&fragment.id),
            summary: summary(snapshot.session, fragment),
        })
        .collect()
}

/// A bounded summary: the tools, their outcomes and sizes, and whether there was prose.
fn summary(session: &Session, fragment: &super::fragments::Fragment) -> String {
    let events = session.log().events();
    let at = |seq: u64| {
        usize::try_from(seq)
            .ok()
            .and_then(|index| events.get(index))
    };
    let mut parts: Vec<String> = Vec::new();
    if let Some(SessionEvent::AssistantMessage {
        text: Some(text), ..
    }) = at(fragment.assistant_seq)
        && !text.is_empty()
    {
        parts.push(format!("text {}B", text.len()));
    }
    for seq in &fragment.result_seqs {
        if let Some(SessionEvent::ToolResult {
            content,
            is_error,
            call_id,
            ..
        }) = at(*seq)
        {
            let tool = fragment_tool(events, fragment.assistant_seq, call_id);
            let outcome = if *is_error { "error" } else { "ok" };
            parts.push(format!("{tool} {outcome} {}B", content.len()));
        }
    }
    let mut summary = parts.join(", ");
    if summary.chars().count() > limits::DESCRIPTOR_SUMMARY_CHARS_MAX {
        summary = summary
            .chars()
            .take(limits::DESCRIPTOR_SUMMARY_CHARS_MAX.saturating_sub(1))
            .collect();
        summary.push('…');
    }
    summary
}

/// The tool a call in an assistant event named.
fn fragment_tool(events: &[SessionEvent], seq: u64, id: &crate::ToolCallId) -> String {
    let event = usize::try_from(seq)
        .ok()
        .and_then(|index| events.get(index));
    match event {
        Some(SessionEvent::AssistantMessage { tool_calls, .. }) => {
            tool_calls.iter().find(|call| &call.id == id).map_or_else(
                || String::from("tool"),
                |call| call.name.as_str().to_owned(),
            )
        }
        _ => String::from("tool"),
    }
}

/// Keyed, opaque cursors: a position the model can hand back but cannot forge or reuse stale.
pub mod cursor {
    /// Separates the key a cursor is tagged under from any other use of the same secret.
    const CONTEXT: &str = "nanus 2026-10 managed-context recall cursor";

    /// The tag's length in hex digits: 128 bits, enough that a guess is not a strategy.
    const TAG_HEX: usize = 32;

    /// Seals `payload` under `key` as `payload.tag`.
    #[must_use]
    pub fn seal(key: &[u8], payload: &str) -> String {
        format!("{payload}.{}", tag(key, payload))
    }

    /// Opens a sealed cursor, returning its payload when the tag verifies.
    ///
    /// The comparison takes the same time wherever the tags first differ, so how long a refusal
    /// takes says nothing about how much of a forged tag was right.
    #[must_use]
    pub fn open<'a>(key: &[u8], token: &'a str) -> Option<&'a str> {
        let (payload, given) = token.rsplit_once('.')?;
        let expected = tag(key, payload);
        let differs = expected
            .bytes()
            .zip(given.bytes())
            .fold(0_u8, |acc, (left, right)| acc | (left ^ right));
        (given.len() == expected.len() && differs == 0).then_some(payload)
    }

    /// Keyed BLAKE3 of `payload`, truncated to [`TAG_HEX`] hex digits.
    ///
    /// The host's key may be any length, so the 32-byte MAC key is derived from it rather than
    /// taken from it.
    fn tag(key: &[u8], payload: &str) -> String {
        let mac_key = blake3::derive_key(CONTEXT, key);
        let hex = blake3::keyed_hash(&mac_key, payload.as_bytes()).to_hex();
        hex.as_str().get(..TAG_HEX).unwrap_or_default().to_owned()
    }
}

#[cfg(test)]
mod tests;
