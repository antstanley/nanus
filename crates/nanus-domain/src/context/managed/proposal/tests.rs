//! Proposals: the snapshot compare-and-set, its suffix rule, and what a revision binds.

use serde_json::json;

use super::*;
use crate::context::managed::fragments::derive;
use crate::context::managed::ids::NoteId;
use crate::context::managed::records::{NoteCategory, SourceField, SourceKind, SourceRef};
use crate::context::managed::state::{frontier, protected};
use crate::{SessionId, ToolCall, ToolCallId, ToolName};

fn session() -> Session {
    let mut session = Session::new(SessionId::new("s"), 0, "/w");
    session.upgrade_to_managed_body();
    session.append(SessionEvent::UserMessage { text: "go".into() });
    for id in ["a", "b", "c", "d"] {
        session.append(SessionEvent::AssistantMessage {
            replay: None,
            text: None,
            reasoning: None,
            tool_calls: vec![ToolCall::new(
                ToolCallId::new(id),
                ToolName::new("read").unwrap_or_else(|_| unreachable!("valid")),
                json!({}),
            )],
            usage: None,
            interrupted: false,
            model: None,
            effort: None,
        });
        session.append(SessionEvent::ToolResult {
            call_id: ToolCallId::new(id),
            content: format!("output of {id}"),
            content_blocks: None,
            is_error: false,
        });
    }
    session
}

fn no_artifacts(_: &ArtifactId) -> Option<ArtifactFacts> {
    None
}

/// Builds a snapshot over owned parts and runs `check` against it.
fn with_snapshot<T>(
    session: &Session,
    profile: &Digest,
    check: impl FnOnce(&Snapshot<'_>) -> T,
) -> T {
    let state = ManagedState::fold(session.log()).unwrap_or_default();
    let fragments = derive(session.log()).unwrap_or_default();
    let protected = protected(session.log(), &fragments);
    let snapshot = Snapshot {
        session,
        state: &state,
        fragments: &fragments,
        protected: &protected,
        profile_digest: profile,
        goal_revision: None,
        artifacts: &no_artifacts,
    };
    check(&snapshot)
}

fn propose(
    session: &Session,
    profile: &Digest,
    hide: &[u64],
    notes: Vec<WorkingNote>,
) -> ContextManageInput {
    ContextManageInput {
        action: ManageAction::Propose,
        base_revision: Some(0),
        base_frontier: frontier(session, 0).ok(),
        hide: hide.iter().copied().map(FragmentId::new).collect(),
        restore: Vec::new(),
        notes,
        cursor: None,
        base_profile_digest: Some(profile.clone()),
    }
}

fn note(seq: u64, digest: Digest) -> WorkingNote {
    WorkingNote {
        id: NoteId::parse("n:read-a").unwrap_or_else(|| unreachable!("valid")),
        claim: "a was read".into(),
        category: NoteCategory::Observed,
        sources: vec![SourceRef {
            kind: SourceKind::Event,
            event_seq: Some(seq),
            block_index: None,
            artifact_id: None,
            offset: 0,
            length: 6,
            source_digest: digest,
            field: SourceField::ToolText,
        }],
    }
}

#[test]
fn a_valid_proposal_stages_and_becomes_the_next_revision() {
    let session = session();
    let profile = Digest::of(b"profile");
    let input = propose(
        &session,
        &profile,
        &[1, 3],
        vec![note(2, Digest::of(b"output of a"))],
    );
    let staged = with_snapshot(&session, &profile, |snapshot| stage(snapshot, &input));
    let Ok(staged) = staged else {
        panic!("stages: {staged:?}");
    };
    assert_eq!(staged.hidden, vec![FragmentId::new(1), FragmentId::new(3)]);
    let revision = revision_from(&staged, "d:1".into()).unwrap_or_else(|code| panic!("{code}"));
    assert_eq!(revision.revision, 1);
    assert!(revision.validate().is_ok());
}

/// T06: a stale base, profile, suffix or fragment refuses before anything is staged.
#[test]
fn stale_or_invalid_proposals_are_refused() {
    let session = session();
    let profile = Digest::of(b"profile");
    let staged = |session: &Session, input: &ContextManageInput| {
        with_snapshot(session, &profile, |snapshot| stage(snapshot, input)).err()
    };
    let mut wrong_revision = propose(&session, &profile, &[1], Vec::new());
    wrong_revision.base_revision = Some(1);
    assert_eq!(
        staged(&session, &wrong_revision),
        Some(ErrorCode::StaleBase)
    );

    let other_profile = propose(&session, &Digest::of(b"another model"), &[1], Vec::new());
    assert_eq!(staged(&session, &other_profile), Some(ErrorCode::StaleBase));

    let input = propose(&session, &profile, &[1], Vec::new());
    let mut later = session.clone();
    later.append(SessionEvent::UserMessage {
        text: "a new constraint".into(),
    });
    assert_eq!(
        staged(&later, &input),
        Some(ErrorCode::StaleBase),
        "a new user message"
    );

    let mut goal = session.clone();
    goal.append(SessionEvent::GoalChange { goal: None });
    assert_eq!(
        staged(&goal, &input),
        Some(ErrorCode::StaleBase),
        "a goal change"
    );

    let mut bookkeeping = session.clone();
    bookkeeping.append(SessionEvent::StepEnd { turn: 1, step: 1 });
    assert_eq!(
        staged(&bookkeeping, &input),
        None,
        "other suffix events are preserved"
    );

    let unknown = propose(&session, &profile, &[2], Vec::new());
    assert_eq!(
        staged(&session, &unknown),
        Some(ErrorCode::InvalidFragment),
        "a result seq"
    );
    let future = propose(&session, &profile, &[99], Vec::new());
    assert_eq!(staged(&session, &future), Some(ErrorCode::InvalidFragment));
    let recent = propose(&session, &profile, &[7], Vec::new());
    assert_eq!(
        staged(&session, &recent),
        Some(ErrorCode::ProtectedFragment)
    );
}

/// T06: a fragment completed after the base frontier cannot be targeted by that proposal.
#[test]
fn a_fragment_past_the_base_frontier_is_not_eligible() {
    let session = session();
    let profile = Digest::of(b"profile");
    let mut input = propose(&session, &profile, &[1], Vec::new());
    input.base_frontier = frontier_at(&session, 2);
    let refused = with_snapshot(&session, &profile, |snapshot| stage(snapshot, &input));
    assert_eq!(refused.err(), Some(ErrorCode::InvalidFragment));
}

fn frontier_at(session: &Session, count: u64) -> Option<ContextFrontier> {
    crate::context::managed::state::frontier_at(session, count, 0).ok()
}

/// T07: a note citing a nonexistent or mismatched source is refused.
#[test]
fn a_note_must_cite_a_real_source_inside_the_base() {
    let session = session();
    let profile = Digest::of(b"profile");
    let wrong = propose(
        &session,
        &profile,
        &[],
        vec![note(2, Digest::of(b"forged"))],
    );
    let refused = with_snapshot(&session, &profile, |snapshot| stage(snapshot, &wrong));
    assert_eq!(refused.err(), Some(ErrorCode::InvalidReference));
    let missing = propose(&session, &profile, &[], vec![note(500, Digest::of(b"x"))]);
    let refused = with_snapshot(&session, &profile, |snapshot| stage(snapshot, &missing));
    assert_eq!(refused.err(), Some(ErrorCode::InvalidReference));
}

#[test]
fn restore_needs_a_hidden_fragment() {
    let session = session();
    let profile = Digest::of(b"profile");
    let mut input = propose(&session, &profile, &[], Vec::new());
    input.restore = vec![FragmentId::new(1)];
    let refused = with_snapshot(&session, &profile, |snapshot| stage(snapshot, &input));
    assert_eq!(refused.err(), Some(ErrorCode::InvalidFragment));
}

#[test]
fn a_cursor_opens_only_under_its_key_and_unaltered() {
    let token = cursor::seal(b"process key", "catalog:3:17");
    assert_eq!(cursor::open(b"process key", &token), Some("catalog:3:17"));
    assert_eq!(
        cursor::open(b"another key", &token),
        None,
        "a restart changes the key"
    );
    let forged = token.replace(":17", ":18");
    assert_eq!(cursor::open(b"process key", &forged), None);
    assert_eq!(cursor::open(b"process key", "no-tag"), None);
}

#[test]
fn the_catalog_marks_protected_and_hidden_fragments() {
    let session = session();
    let profile = Digest::of(b"profile");
    let descriptors = with_snapshot(&session, &profile, describe);
    assert_eq!(descriptors.len(), 4);
    assert!(!descriptors[0].protected && descriptors[3].protected);
    assert!(
        descriptors[0].summary.contains("read ok"),
        "{:?}",
        descriptors[0]
    );
}
