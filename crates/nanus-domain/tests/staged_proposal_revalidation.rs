//! A staged proposal is revalidated against the selection it was staged under. A profile digest
//! that differs from the one the proposal was staged with is refused as `stale_base`, so a
//! selection change between staging and acceptance cannot install a proposal made under another.

use nanus_domain::context::managed::proposal::{Snapshot, Staged, recheck};
use nanus_domain::context::managed::{
    ArtifactId, Digest, ErrorCode, ManagedState, fragments, state,
};
use nanus_domain::{Session, SessionId};

#[test]
fn a_staged_proposal_is_refused_when_its_profile_no_longer_matches() {
    let mut session = Session::new(SessionId::new("staged"), 0, "/w");
    session.upgrade_to_managed_body();
    let state = ManagedState::fold(session.log()).unwrap();
    let revision = state.revision();
    let derived = fragments::derive(session.log()).unwrap();
    let protected = state::protected(session.log(), &derived);
    let frontier = state::frontier(&session, revision).unwrap();
    let staged_under = Digest::of(b"the selection the proposal was staged under");
    let staged = Staged {
        base_revision: revision,
        base_frontier: frontier,
        base_profile_digest: staged_under.clone(),
        hidden: Vec::new(),
        notes: Vec::new(),
        goal_revision: None,
    };
    let artifacts = |_: &ArtifactId| None;
    let changed = Digest::of(b"a selection another model or plan made");

    let under_staged = Snapshot {
        session: &session,
        state: &state,
        fragments: &derived,
        protected: &protected,
        profile_digest: &staged_under,
        goal_revision: None,
        artifacts: &artifacts,
    };
    assert_eq!(recheck(&under_staged, &staged), Ok(()));

    let under_changed = Snapshot {
        profile_digest: &changed,
        ..under_staged
    };
    assert_eq!(
        recheck(&under_changed, &staged),
        Err(ErrorCode::StaleBase),
        "a profile that changed is refused"
    );
}
