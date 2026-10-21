//! The interface's side of the session-level controls: `/clear`, `/handoff` and `/compact`.
//!
//! This is view state and the sentences that explain an action before the human confirms it. The
//! agent alone drafts, validates and writes anything; the interface only holds what the agent
//! drafted until the human says `y`, and says plainly what the action leaves behind. Nothing here
//! reaches a link or a store.

/// What a compaction draft used, as the agent reported it. Plain counts, so the view layer needs no
/// link type to draw them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UsageFigures {
    /// Tokens sent to the model.
    pub prompt_tokens: u32,
    /// Tokens the model generated.
    pub completion_tokens: u32,
    /// The share of the generated tokens spent reasoning.
    pub reasoning_tokens: u32,
    /// The share of the prompt served from the provider's cache.
    pub cache_hit_tokens: u32,
    /// The share of the prompt that missed the cache.
    pub cache_miss_tokens: u32,
}

/// A draft the agent has made and the human has not yet confirmed or cancelled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Preview {
    /// A handoff document, and the name the new session will take.
    Handoff {
        /// The id a commit names.
        draft_id: String,
        /// The whole document: the header, then the model's narrative.
        text: String,
        /// The name asked for when the draft was made, if any.
        name: Option<String>,
    },
    /// A compaction summary and what it was drawn from.
    Compact {
        /// The id a commit names.
        draft_id: String,
        /// The summary the cut would carry.
        text: String,
        /// Whole turns the request had to leave out, so the summary covers only what the model saw.
        dropped_turns: u32,
        /// The event count the cut would be made at; `0` when the agent did not say.
        boundary_event_count: u64,
        /// What the summary request cost, when the provider reported it.
        usage: Option<UsageFigures>,
    },
}

impl Preview {
    /// The id a `y` sends back to the agent.
    #[must_use]
    pub fn draft_id(&self) -> &str {
        match self {
            Self::Handoff { draft_id, .. } | Self::Compact { draft_id, .. } => draft_id,
        }
    }

    /// The heading the preview is drawn under.
    #[must_use]
    pub const fn title(&self) -> &'static str {
        match self {
            Self::Handoff { .. } => " handoff preview ",
            Self::Compact { .. } => " compaction preview ",
        }
    }

    /// The action keys the preview's footer names.
    #[must_use]
    pub const fn footer(&self) -> &'static str {
        match self {
            Self::Handoff { .. } => {
                "y starts the new session · Esc cancels, nothing is created\n\
                 Up/Down scroll · F6 copy"
            }
            Self::Compact { .. } => {
                "y compacts the session · Esc cancels, nothing is changed\n\
                 Up/Down scroll · F6 copy"
            }
        }
    }
}

/// The `/clear` warning, open until the human answers it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClearDialog {
    /// The conversation a clear would stop.
    pub old_id: String,
    /// Its name, when it has one.
    pub name: Option<String>,
    /// Whether a turn is running in it, which a clear would stop first.
    pub busy: bool,
}

/// A switch of conversation a request asked for, which the attachment that follows completes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Switch {
    /// A clear: the conversation the interface left, which the notice after the switch names.
    Clear {
        /// The conversation left behind.
        old_id: String,
        /// Its name, when it has one.
        name: Option<String>,
    },
    /// A handoff commit: the name asked for, which the seed's name is checked against.
    Handoff {
        /// The name the human asked the new session to take.
        name: Option<String>,
    },
}

/// The warning a handoff preview leads with: what the new session gets, and what does not change.
pub const HANDOFF_WARNING: &str = "The new session opens with this document as generated data, \
not as your words: a model wrote it from this conversation, and it is not a transcript. The old \
session stays as it is and records the handoff, so it can still be resumed by id.";

/// The warning a compaction preview leads with: what the cut takes out of the model's reach.
pub const COMPACT_WARNING: &str = "Earlier content leaves the model's reach. From the cut on, the \
model reads this summary in place of the conversation before it, and a legacy session has no way \
to read that conversation back. Nothing is deleted: the raw log and the transcript keep it.";

/// The sentence that says a compaction's summary covers less than the conversation.
///
/// Only drawn when the request left turns out, so a reader is never told about a cut that did not
/// happen.
#[must_use]
pub fn dropped_turns_sentence(dropped: u32) -> String {
    let noun = if dropped == 1 { "turn" } else { "turns" };
    format!(
        "The summary covers only the most recent turns the model could see: {dropped} older \
         {noun} did not fit the request and are not in it."
    )
}

/// The sentence that says where the cut falls, from the event count the agent drafted it at.
///
/// A count of zero is the agent not saying, because a cut always follows a completed turn: it is
/// shown as unknown rather than as a cut at the start of the session.
#[must_use]
pub fn cut_sentence(boundary_event_count: u64) -> String {
    if boundary_event_count == 0 {
        String::from(
            "The agent did not say which event the cut falls at; everything before it leaves the \
             model's reach.",
        )
    } else {
        format!(
            "The cut is at event {boundary_event_count}: everything before it leaves the model's \
             reach."
        )
    }
}

/// The usage line a compaction preview shows, when there is usage to show.
#[must_use]
pub fn usage_line(usage: UsageFigures) -> String {
    format!(
        "This summary cost {} prompt and {} generated tokens ({} reasoning, {} cached).",
        usage.prompt_tokens,
        usage.completion_tokens,
        usage.reasoning_tokens,
        usage.cache_hit_tokens
    )
}

/// The `/clear` warning's text: what stops, what is kept, and how to get back to it.
#[must_use]
pub fn clear_warning(dialog: &ClearDialog) -> String {
    let named = dialog
        .name
        .as_deref()
        .map_or_else(String::new, |name| format!(" (named {name:?})"));
    let stopping = if dialog.busy {
        " A turn running now will be stopped first, and its answer will end where it is."
    } else {
        ""
    };
    format!(
        "Clear stops this conversation and starts a new one.{stopping} The old session \
         {}{named} is stopped, not deleted: its log is kept, and it can be resumed by id.",
        dialog.old_id
    )
}

/// The notice that names the conversation a clear left behind, so the reader can get back to it.
#[must_use]
pub fn left_behind_notice(old_id: &str, name: Option<&str>) -> String {
    let named = name.map_or_else(String::new, |name| format!(" ({name})"));
    format!(
        "the previous conversation {old_id}{named} is stopped and kept; it can be resumed by id"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dropped_turns_sentence_names_the_count_and_reads_for_one_or_many() {
        assert!(dropped_turns_sentence(1).contains("1 older turn did not fit"));
        assert!(dropped_turns_sentence(3).contains("3 older turns did not fit"));
    }

    #[test]
    fn the_clear_warning_says_stopped_not_deleted_and_names_the_turn_when_one_runs() {
        let idle = ClearDialog {
            old_id: "s-old".to_owned(),
            name: None,
            busy: false,
        };
        let text = clear_warning(&idle);
        assert!(text.contains("stopped, not deleted"), "{text}");
        assert!(text.contains("s-old"));
        assert!(!text.contains("will be stopped first"), "{text}");

        let busy = ClearDialog {
            busy: true,
            name: Some("notes".to_owned()),
            ..idle
        };
        let text = clear_warning(&busy);
        assert!(text.contains("will be stopped first"), "{text}");
        assert!(text.contains("named \"notes\""), "{text}");
    }

    #[test]
    fn every_preview_names_the_draft_it_will_commit_and_its_own_escape() {
        let handoff = Preview::Handoff {
            draft_id: "d1".to_owned(),
            text: String::new(),
            name: None,
        };
        let compact = Preview::Compact {
            draft_id: "d2".to_owned(),
            text: String::new(),
            dropped_turns: 0,
            boundary_event_count: 8,
            usage: None,
        };
        assert_eq!(handoff.draft_id(), "d1");
        assert_eq!(compact.draft_id(), "d2");
        assert!(handoff.footer().contains("Esc cancels"));
        assert!(compact.footer().contains("Esc cancels"));
    }

    #[test]
    fn the_cut_sentence_names_the_event_and_says_unknown_for_none() {
        assert!(cut_sentence(8).contains("The cut is at event 8"));
        let unknown = cut_sentence(0);
        assert!(unknown.contains("did not say"), "{unknown}");
        assert!(!unknown.contains("event 0"), "{unknown}");
    }

    #[test]
    fn the_left_behind_notice_names_the_old_conversation() {
        assert!(left_behind_notice("s-old", None).contains("s-old"));
        assert!(left_behind_notice("s-old", Some("notes")).contains("(notes)"));
    }
}
