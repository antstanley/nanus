//! The context chooser and the warnings a mid-session change deserves.
//!
//! This is view state, not policy: the agent alone validates and persists the requested change.

use nanus_domain::context::managed::ContextMode;

/// A human context change, distinct from a status read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContextChoice {
    /// Whole-turn fitting, keeping the managed projection for later.
    Legacy,
    /// Managed fragment selection and working notes.
    Managed,
    /// Clear the managed projection and return to legacy fitting.
    Reset,
}

impl ContextChoice {
    /// The mode a saved change selects.
    #[must_use]
    pub const fn mode(self) -> ContextMode {
        match self {
            Self::Legacy | Self::Reset => ContextMode::Legacy,
            Self::Managed => ContextMode::Managed,
        }
    }

    /// The label drawn in the chooser.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Legacy => "legacy - fit whole user turns",
            Self::Managed => "managed - select fragments and keep working notes",
            Self::Reset => "reset - clear managed selection and notes",
        }
    }

    /// The impact shown before the human can confirm a change.
    #[must_use]
    pub const fn warning(self) -> &'static str {
        match self {
            Self::Managed => {
                "Managed keeps every user message, but may hide older assistant work \
                and tool results and add working notes. Re-enabling reuses the saved selection \
                and notes; it does not start fresh. The model may see a different history on the \
                next request.\n\n\
                This is opt-in and has not been live-provider or quality/cost validated. The \
                agent refuses unsupported model paths (including Responses) or output reserves. \
                Changing history can invalidate prompt caches, lose prior reasoning replay, and \
                change token use, cost and answer continuity.\n\n\
                Enabling upgrades the stored session format; disabling cannot downgrade it. \
                Older builds may no longer read it. The existing output reserve is kept. Shell \
                capture is not enabled by this switch."
            }
            Self::Legacy => {
                "Legacy stops using managed hiding and working notes. Previously \
                hidden assistant work and tool results become eligible for replay, but the \
                whole-turn fitter may drop old user turns, including their constraints. The \
                model may see a different history on the next request.\n\n\
                More replay can increase token use and cost and invalidate prompt caches. \
                Answers may lose continuity. Context management and recall tools are no longer \
                offered, and new shell evidence capture stops.\n\n\
                The saved selection and notes remain for re-enabling managed mode. The stored \
                format stays upgraded; no downgrade is performed. Use reset only if you mean \
                to clear that selection."
            }
            Self::Reset => {
                "Reset clears the accepted hidden-fragment selection and working \
                notes, then selects legacy. Re-enabling managed mode will not restore the \
                cleared selection or notes.\n\n\
                Previously hidden work becomes eligible for replay, but legacy fitting can \
                drop whole old user turns and their constraints. Token use and cost may rise, \
                prompt caches may be invalidated, and answers may lose continuity. Context \
                management and recall tools disappear; new shell evidence capture stops.\n\n\
                The raw transcript, prior records and existing archives are kept. The stored \
                format stays upgraded. Reset is not deletion and does not rerun any tool."
            }
        }
    }
}

/// An open chooser, or a confirmation showing one choice's impact.
#[derive(Clone, Debug)]
pub struct ContextDialog {
    /// The selected row: legacy, managed, reset.
    pub selection: usize,
    /// A choice awaiting explicit `y`; `None` draws the chooser.
    pub confirming: Option<ContextChoice>,
    /// Rows scrolled through the warning on a short terminal.
    pub scroll: u16,
}

impl ContextDialog {
    /// Opens on the recorded mode, or directly on a named change's confirmation.
    #[must_use]
    pub const fn new(mode: ContextMode, confirming: Option<ContextChoice>) -> Self {
        Self {
            selection: match mode {
                ContextMode::Legacy => 0,
                ContextMode::Managed => 1,
            },
            confirming,
            scroll: 0,
        }
    }

    /// The three choices, in display order.
    pub const CHOICES: [ContextChoice; 3] = [
        ContextChoice::Legacy,
        ContextChoice::Managed,
        ContextChoice::Reset,
    ];

    /// The selected change.
    #[must_use]
    pub fn selected(&self) -> ContextChoice {
        assert!(self.selection < Self::CHOICES.len());
        Self::CHOICES[self.selection]
    }
}

/// Wraps the ASCII warning into actual rows so scrolling and drawing use the same coordinates.
pub(crate) fn warning_rows(choice: ContextChoice, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = Vec::new();
    let text = format!(
        "Change to {}?\n\n{}\n\nOnly changes future requests. The raw session log is kept.",
        choice.label(),
        choice.warning()
    );
    for paragraph in text.split('\n') {
        let mut row = String::new();
        for word in paragraph.split_whitespace() {
            if !row.is_empty() && row.len().saturating_add(word.len()).saturating_add(1) > width {
                rows.push(std::mem::take(&mut row));
            }
            if !row.is_empty() {
                row.push(' ');
            }
            // A terminal can be narrower than one word. Split that word rather than clipping
            // evidence of the impact off the right edge; the warnings are fixed ASCII prose.
            assert!(word.is_ascii());
            let mut rest = word;
            while rest.len() > width {
                rows.push(rest[..width].to_owned());
                rest = &rest[width..];
            }
            row.push_str(rest);
        }
        rows.push(row);
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_warning_fits_and_preserves_all_words_even_in_a_narrow_terminal() {
        for choice in ContextDialog::CHOICES {
            let words = warning_rows(choice, 1000).join(" ");
            for width in 1..=80 {
                let rows = warning_rows(choice, width);
                assert!(rows.iter().all(|row| row.len() <= width));
                // Wrapping inserts whitespace, including inside a word too wide for a row;
                // no non-whitespace character of the original warning may disappear.
                let joined = rows.concat();
                let compact = |text: &str| {
                    text.chars()
                        .filter(|c| !c.is_whitespace())
                        .collect::<String>()
                };
                assert_eq!(compact(&joined), compact(&words));
            }
        }
    }
}
