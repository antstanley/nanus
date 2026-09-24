//! A durable objective that outlives a turn.
//!
//! Some work does not fit in one turn. "Keep going" is the instruction a person
//! ends up typing after every intermediate result, and a goal is that instruction
//! written down once: an objective the session remembers, with a lifecycle a
//! person controls.
//!
//! ## Why the log, and not the configuration
//!
//! The objective belongs to the *conversation*. It is scoped to a session, it
//! survives a resume, and two sessions on one machine have two goals. The log is
//! already the only place a session's history lives, so the goal goes there too
//! and is read back by a fold — see
//! [`SessionLog::goal`](crate::SessionLog::goal).
//!
//! ## The lifecycle
//!
//! [`GoalPhase::Active`] is a goal being pursued, [`GoalPhase::Paused`] one a
//! person has suspended, [`GoalPhase::Complete`] one whose objective is achieved,
//! and [`GoalPhase::Abandoned`] one given up on without achieving it. Every change
//! bumps the [`Goal::revision`], which is what lets two clients that edited the
//! same goal be told apart without a lock: the log is append-only, so a change is
//! a new record and the newest record wins.
//!
//! [`GoalPhase::Complete`] and [`GoalPhase::Abandoned`] are *terminal*: neither
//! can become anything else, because "achieved" and "given up on" are answers
//! rather than stages. A terminal goal is replaced by setting a new objective,
//! which is how the next piece of work begins.
//!
//! There is deliberately no `blocked` phase. A goal that cannot proceed at this
//! moment is *paused* with a note saying why; one that cannot be achieved at all is
//! *abandoned* with the same. That is cheaper than `DeepSeek`'s coded, threshold-gated
//! blocker and is the one a status line can explain in a sentence.

use core::fmt;

use serde::{Deserialize, Serialize};

use crate::error::DomainError;

/// The largest number of characters an objective may hold.
///
/// The objective is replayed into future prompts, so it is bounded the way every
/// other model-visible string is: an unbounded one is a way to spend a context
/// window on a single instruction.
pub const GOAL_OBJECTIVE_MAX_CHARS: usize = 4_000;

/// Where a goal is in its lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalPhase {
    /// Being pursued.
    Active,
    /// Suspended by a person; not continued.
    Paused,
    /// The objective is achieved.
    Complete,
    /// Given up on without achieving the objective.
    Abandoned,
}

impl GoalPhase {
    /// Returns the phase's name, as a transcript or a status line shows it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Complete => "complete",
            Self::Abandoned => "abandoned",
        }
    }

    /// Returns `true` when a goal in this phase may still be worked on.
    ///
    /// Only [`GoalPhase::Active`] is open: a paused goal waits for a person, and a
    /// terminal one is answered.
    #[must_use]
    pub const fn is_open(self) -> bool {
        matches!(self, Self::Active)
    }

    /// Returns `true` when this phase is an answer rather than a stage.
    ///
    /// A terminal goal cannot be paused, resumed, completed, or abandoned again; it
    /// is replaced by setting a new objective.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Complete | Self::Abandoned)
    }
}

impl fmt::Display for GoalPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A durable objective, and where it is in its lifecycle.
///
/// Immutable: every transition returns the next goal rather than mutating this
/// one, which is what makes the sequence of changes a fold over the log rather
/// than a value someone has to remember to persist.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Goal {
    /// What is to be done.
    objective: String,
    /// Where it is in its lifecycle.
    phase: GoalPhase,
    /// How many changes this goal has had, starting at one.
    revision: u64,
    /// When it was created, in milliseconds since the Unix epoch.
    created_at_ms: u64,
    /// When it was last changed, in milliseconds since the Unix epoch.
    updated_at_ms: u64,
    /// Why it is in its current phase, when a reason was given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    note: Option<String>,
}

impl Goal {
    /// Creates an active goal for `objective`.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::Validation`] when the objective is blank or longer
    /// than [`GOAL_OBJECTIVE_MAX_CHARS`] characters. The objective is trimmed, so
    /// leading and trailing whitespace is the caller's to leave out.
    pub fn new(objective: impl Into<String>, now_ms: u64) -> Result<Self, DomainError> {
        let objective = validate_objective(&objective.into())?;
        Ok(Self {
            objective,
            phase: GoalPhase::Active,
            revision: 1,
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
            note: None,
        })
    }

    /// Returns the objective.
    #[must_use]
    pub fn objective(&self) -> &str {
        &self.objective
    }

    /// Returns the phase.
    #[must_use]
    pub const fn phase(&self) -> GoalPhase {
        self.phase
    }

    /// Returns the revision, which every change increments.
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    /// Returns when the goal was created, in milliseconds since the Unix epoch.
    #[must_use]
    pub const fn created_at_ms(&self) -> u64 {
        self.created_at_ms
    }

    /// Returns when the goal was last changed, in milliseconds since the Unix epoch.
    #[must_use]
    pub const fn updated_at_ms(&self) -> u64 {
        self.updated_at_ms
    }

    /// Returns the note explaining the current phase, when one was recorded.
    #[must_use]
    pub fn note(&self) -> Option<&str> {
        self.note.as_deref()
    }

    /// Returns `true` when the goal may still be worked on.
    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.phase.is_open()
    }

    /// Returns a copy with `objective` replacing this one's, active once more.
    ///
    /// A completed goal may be replaced: setting an objective is how a new goal
    /// begins, and refusing it while the old one is still marked complete would
    /// leave a person with no way to start the next one.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::Validation`] under the same conditions as
    /// [`Goal::new`].
    pub fn with_objective(
        &self,
        objective: impl Into<String>,
        now_ms: u64,
    ) -> Result<Self, DomainError> {
        let objective = validate_objective(&objective.into())?;
        Ok(Self {
            objective,
            phase: GoalPhase::Active,
            revision: self.revision.saturating_add(1),
            created_at_ms: self.created_at_ms,
            updated_at_ms: now_ms,
            note: None,
        })
    }

    /// Returns a copy suspended, waiting for a person to resume it.
    ///
    /// `note` is why, when a reason was given. Resuming clears it, because a reason for
    /// stopping is not a fact about a goal that is running again.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::Validation`] when the goal is terminal, because a
    /// goal that has been achieved or given up on is not something to pause.
    pub fn paused(&self, note: Option<String>, now_ms: u64) -> Result<Self, DomainError> {
        self.transitioned(GoalPhase::Paused, now_ms, note)
    }

    /// Returns a copy active again.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::Validation`] when the goal is terminal, for the same
    /// reason [`Goal::paused`] refuses it.
    pub fn resumed(&self, now_ms: u64) -> Result<Self, DomainError> {
        self.transitioned(GoalPhase::Active, now_ms, None)
    }

    /// Returns a copy marked complete, carrying the reason it was given.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::Validation`] when the goal is already terminal.
    pub fn completed(&self, note: Option<String>, now_ms: u64) -> Result<Self, DomainError> {
        self.transitioned(GoalPhase::Complete, now_ms, note)
    }

    /// Returns a copy abandoned, carrying the reason it was given.
    ///
    /// Abandoning is not completing: the objective was *not* achieved, and the reason
    /// says why the pursuit stopped. It is terminal, so it is the model's way to stop
    /// spending on something it has concluded it cannot do.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::Validation`] when the goal is already terminal.
    pub fn abandoned(&self, note: Option<String>, now_ms: u64) -> Result<Self, DomainError> {
        self.transitioned(GoalPhase::Abandoned, now_ms, note)
    }

    /// Moves to `phase`, keeping the objective and stamping the change.
    ///
    /// Idempotent: a goal already in `phase` is returned unchanged, revision and all. That is
    /// what lets a caller skip writing a record for a transition that changed nothing — pausing
    /// a goal already paused is not a change to log.
    fn transitioned(
        &self,
        phase: GoalPhase,
        now_ms: u64,
        note: Option<String>,
    ) -> Result<Self, DomainError> {
        if self.phase.is_terminal() {
            return Err(DomainError::validation(
                "goal",
                format!(
                    "a {} goal cannot become {phase}; set a new objective instead",
                    self.phase
                ),
            ));
        }
        if self.phase == phase {
            return Ok(self.clone());
        }
        Ok(Self {
            objective: self.objective.clone(),
            phase,
            revision: self.revision.saturating_add(1),
            created_at_ms: self.created_at_ms,
            updated_at_ms: now_ms,
            note,
        })
    }
}

impl fmt::Display for Goal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.objective, self.phase)
    }
}

/// Trims and checks an objective, returning the text to store.
///
/// # Errors
///
/// Returns [`DomainError::Validation`] when the trimmed text is empty, or when it
/// is longer than [`GOAL_OBJECTIVE_MAX_CHARS`] characters.
fn validate_objective(raw: &str) -> Result<String, DomainError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(DomainError::validation("goal", "an objective is not blank"));
    }
    let length = trimmed.chars().count();
    if length > GOAL_OBJECTIVE_MAX_CHARS {
        return Err(DomainError::validation(
            "goal",
            format!(
                "an objective holds at most {GOAL_OBJECTIVE_MAX_CHARS} characters, not {length}"
            ),
        ));
    }
    // Postcondition: an accepted objective is non-blank and within the ceiling,
    // which is the whole of what the checks above promise.
    assert!(!trimmed.is_empty());
    assert!(trimmed.chars().count() <= GOAL_OBJECTIVE_MAX_CHARS);
    Ok(trimmed.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn goal() -> Goal {
        Goal::new("ship the release notes", 1_000)
            .unwrap_or_else(|error| panic!("test goal: {error}"))
    }

    #[test]
    fn a_new_goal_is_active_and_first_revision() {
        let goal = goal();
        assert_eq!(goal.objective(), "ship the release notes");
        assert_eq!(goal.phase(), GoalPhase::Active);
        assert!(goal.is_open());
        assert_eq!(goal.revision(), 1);
        assert_eq!(goal.created_at_ms(), 1_000);
        assert_eq!(goal.updated_at_ms(), 1_000);
        assert_eq!(goal.note(), None);
    }

    #[test]
    fn an_objective_is_trimmed_of_surrounding_whitespace() {
        let goal = Goal::new("  catch the bug\n", 0).expect("a non-blank objective");
        assert_eq!(goal.objective(), "catch the bug");
    }

    #[test]
    fn a_blank_objective_is_refused() {
        // Negative space: the shapes a caller might pass by accident.
        for blank in ["", "   ", "\n\t "] {
            let refused = Goal::new(blank, 0);
            assert!(
                matches!(refused, Err(DomainError::Validation { field: "goal", .. })),
                "{blank:?} is not an objective"
            );
        }
    }

    #[test]
    fn an_objective_at_the_ceiling_is_accepted_and_one_past_it_is_not() {
        let at = "a".repeat(GOAL_OBJECTIVE_MAX_CHARS);
        assert!(Goal::new(at, 0).is_ok());
        let over = "a".repeat(GOAL_OBJECTIVE_MAX_CHARS.saturating_add(1));
        assert!(Goal::new(over, 0).is_err());
    }

    #[test]
    fn every_change_bumps_the_revision_and_the_updated_time() {
        let created = goal();
        let edited = created
            .with_objective("a different objective", 2_000)
            .expect("a non-blank objective");
        assert_eq!(edited.revision(), 2);
        assert_eq!(
            edited.created_at_ms(),
            1_000,
            "the creation survives an edit"
        );
        assert_eq!(edited.updated_at_ms(), 2_000);
        assert_eq!(edited.objective(), "a different objective");

        let paused = edited
            .paused(None, 3_000)
            .expect("an active goal can pause");
        assert_eq!(paused.phase(), GoalPhase::Paused);
        assert_eq!(paused.revision(), 3);
        assert!(!paused.is_open());

        let resumed = paused.resumed(4_000).expect("a paused goal can resume");
        assert_eq!(resumed.phase(), GoalPhase::Active);
        assert_eq!(resumed.revision(), 4);
        assert_eq!(resumed.objective(), "a different objective");
    }

    #[test]
    fn completing_records_the_reason_it_was_given() {
        let done = goal()
            .completed(Some("the notes are published".to_owned()), 9_000)
            .expect("an active goal can complete");
        assert_eq!(done.phase(), GoalPhase::Complete);
        assert_eq!(done.note(), Some("the notes are published"));
        assert!(!done.is_open());
    }

    #[test]
    fn abandoning_records_the_reason_and_is_not_completing() {
        let given_up = goal()
            .abandoned(
                Some("the benchmark cannot be made deterministic".to_owned()),
                8_000,
            )
            .expect("an active goal can be abandoned");
        assert_eq!(given_up.phase(), GoalPhase::Abandoned);
        assert_eq!(
            given_up.note(),
            Some("the benchmark cannot be made deterministic")
        );
        assert!(!given_up.is_open());
        assert!(given_up.phase().is_terminal());
        assert!(
            !goal().phase().is_terminal(),
            "a fresh goal is a stage rather than an answer"
        );
    }

    #[test]
    fn a_terminal_goal_cannot_be_paused_resumed_completed_or_abandoned() {
        for terminal in [
            goal().completed(None, 5_000).expect("it can complete"),
            goal().abandoned(None, 5_000).expect("it can be abandoned"),
        ] {
            assert!(terminal.phase().is_terminal());
            assert!(terminal.paused(None, 6_000).is_err(), "{terminal}");
            assert!(terminal.resumed(6_000).is_err(), "{terminal}");
            assert!(terminal.completed(None, 6_000).is_err(), "{terminal}");
            assert!(terminal.abandoned(None, 6_000).is_err(), "{terminal}");
        }
    }

    #[test]
    fn a_completed_goal_can_be_replaced_by_a_new_objective() {
        // The way to start the next goal: replacing the objective reactivates it.
        let done = goal().completed(None, 5_000).expect("it can complete");
        let next = done
            .with_objective("write the retrospective", 6_000)
            .expect("a non-blank objective");
        assert_eq!(next.phase(), GoalPhase::Active);
        assert_eq!(next.revision(), 3);
        assert_eq!(next.created_at_ms(), 1_000);
    }

    #[test]
    fn a_transition_to_the_phase_it_is_already_in_changes_nothing() {
        // What lets a caller skip logging a transition that records no change.
        let goal = goal();
        assert_eq!(
            goal.resumed(500).ok(),
            Some(goal.clone()),
            "resuming an active goal is not a change"
        );
        let paused = goal.paused(None, 600).expect("an active goal can pause");
        assert_eq!(
            paused.paused(None, 700).ok(),
            Some(paused),
            "pausing a paused goal is not a change"
        );
    }

    #[test]
    fn a_goal_round_trips_through_serde() {
        let original = goal()
            .paused(None, 7_000)
            .expect("an active goal can pause");
        let encoded = serde_json::to_string(&original).unwrap_or_default();
        let decoded: Result<Goal, _> = serde_json::from_str(&encoded);
        assert_eq!(decoded.ok(), Some(original));
    }

    #[test]
    fn phases_and_goals_display_their_names() {
        assert_eq!(GoalPhase::Active.to_string(), "active");
        assert_eq!(GoalPhase::Paused.to_string(), "paused");
        assert_eq!(GoalPhase::Complete.to_string(), "complete");
        assert_eq!(GoalPhase::Abandoned.to_string(), "abandoned");
        assert_eq!(
            goal().to_string(),
            "ship the release notes (active)",
            "a goal renders its objective and its phase"
        );
    }
}
