//! The composer: the text the human is typing, and what a key does to it.
//!
//! Line editing is the part of a terminal interface that is easiest to get subtly
//! wrong, so it is modelled explicitly rather than spread across key handlers. The
//! buffer owns the text, a *character* cursor, and a history list; it exposes
//! operations and returns an outcome describing what the caller should do.
//!
//! ## Characters, not bytes
//!
//! Every cursor position is a count of `char`s, never a byte offset. A byte cursor
//! is what turns typing an accented letter into a panic on a non-boundary slice,
//! and no amount of care at the call site fixes a buffer whose model is wrong.

use core::fmt;

/// What a key press meant, once the buffer has been updated.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum KeyOutcome {
    /// Nothing to do beyond redrawing.
    Edited,
    /// The text was submitted; the buffer is now empty.
    Submitted(String),
    /// The user asked to leave.
    Quit,
    /// A key the buffer does not handle, left for the caller to interpret.
    Ignored,
}

/// The composer.
#[derive(Default)]
pub struct InputBuffer {
    text: Vec<char>,
    cursor: usize,
    history: Vec<String>,
    /// How far back through the history the caret currently sits, if at all.
    history_index: Option<usize>,
    /// The text that was being typed before history browsing began, so leaving the
    /// history restores it rather than discarding the user's work.
    draft: String,
}

impl fmt::Debug for InputBuffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InputBuffer")
            .field("length", &self.text.len())
            .field("cursor", &self.cursor)
            .field("history", &self.history.len())
            .finish_non_exhaustive()
    }
}

impl InputBuffer {
    /// Creates an empty composer.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            text: Vec::new(),
            cursor: 0,
            history: Vec::new(),
            history_index: None,
            draft: String::new(),
        }
    }

    /// Builds a composer that already holds `text`, with the cursor at the end.
    #[must_use]
    pub fn with_text(text: &str) -> Self {
        let characters: Vec<char> = text.chars().collect();
        let cursor = characters.len();
        Self {
            text: characters,
            cursor,
            history: Vec::new(),
            history_index: None,
            draft: String::new(),
        }
    }

    /// Returns the text as a string.
    #[must_use]
    pub fn text(&self) -> String {
        self.text.iter().collect()
    }

    /// Returns `true` when there is nothing to submit.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.text.iter().all(|character| character.is_whitespace())
    }

    /// Returns the number of characters held.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.text.len()
    }

    /// Returns the cursor position as a character index.
    #[must_use]
    pub const fn cursor(&self) -> usize {
        self.cursor
    }

    /// Returns `true` when the cursor is at the end of the text.
    #[must_use]
    pub fn is_at_end(&self) -> bool {
        self.cursor >= self.text.len()
    }

    /// Returns a character range ending at the cursor, for rendering the text
    /// before the caret.
    #[must_use]
    pub fn text_before_cursor(&self) -> String {
        self.text.iter().take(self.cursor).collect()
    }

    /// Returns the character range starting at the cursor.
    #[must_use]
    pub fn text_after_cursor(&self) -> String {
        self.text.iter().skip(self.cursor).collect()
    }

    /// Returns the recorded history, oldest first.
    #[must_use]
    pub fn history(&self) -> &[String] {
        &self.history
    }

    /// Inserts a character at the cursor.
    pub fn insert(&mut self, character: char) {
        // A newline in the composer is ordinary text; the caller decides whether a
        // bare Enter submits or inserts, and passes the newline only for the latter.
        self.text.insert(self.cursor, character);
        self.cursor = self.cursor.saturating_add(1);
        self.leave_history();
        // Postcondition: the cursor never passes the end of the text.
        assert!(
            self.cursor <= self.text.len(),
            "the cursor stays within the text"
        );
    }

    /// Inserts a string at the cursor.
    pub fn insert_str(&mut self, text: &str) {
        for character in text.chars() {
            self.insert(character);
        }
    }

    /// Deletes the character before the cursor.
    ///
    /// Returns whether anything was removed.
    pub fn backspace(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        self.cursor = self.cursor.saturating_sub(1);
        self.text.remove(self.cursor);
        self.leave_history();
        true
    }

    /// Deletes the character under the cursor.
    ///
    /// Returns whether anything was removed.
    pub fn delete(&mut self) -> bool {
        if self.cursor >= self.text.len() {
            return false;
        }
        self.text.remove(self.cursor);
        self.leave_history();
        true
    }

    /// Moves the cursor one character left.
    pub const fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    /// Moves the cursor one character right, stopping at the end.
    pub fn move_right(&mut self) {
        if self.cursor < self.text.len() {
            self.cursor = self.cursor.saturating_add(1);
        }
    }

    /// Moves the cursor to the start of the text.
    pub const fn move_home(&mut self) {
        self.cursor = 0;
    }

    /// Moves the cursor to the end of the text.
    pub fn move_end(&mut self) {
        self.cursor = self.text.len();
    }

    /// Deletes from the cursor back to the start of the current word.
    ///
    /// Returns whether anything was removed.
    pub fn delete_word(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        let mut end = self.cursor;
        // Skip the whitespace immediately before the cursor first, so a repeated
        // press keeps eating words rather than stalling on a space.
        while end > 0
            && self
                .text
                .get(end.saturating_sub(1))
                .is_some_and(|c| c.is_whitespace())
        {
            end = end.saturating_sub(1);
        }
        while end > 0
            && self
                .text
                .get(end.saturating_sub(1))
                .is_some_and(|c| !c.is_whitespace())
        {
            end = end.saturating_sub(1);
        }
        let removed = end < self.cursor;
        self.text.drain(end..self.cursor);
        self.cursor = end;
        if removed {
            self.leave_history();
        }
        removed
    }

    /// Clears the buffer, leaving the history intact.
    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.leave_history();
    }

    /// Submits the current text.
    ///
    /// Returns `None` when there is nothing but whitespace to submit, and the
    /// buffer is left untouched so a stray Enter does not silently erase a draft.
    pub fn submit(&mut self) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        let text = self.text();
        self.remember(&text);
        self.clear();
        Some(text)
    }

    /// Records `text` in the history, most recent last.
    ///
    /// Consecutive duplicates are collapsed, because a repeated identical prompt is
    /// almost always an accidental double submission and it makes history browsing
    /// useless.
    fn remember(&mut self, text: &str) {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        if self.history.last().is_some_and(|last| last == trimmed) {
            return;
        }
        self.history.push(trimmed.to_owned());
        // Postcondition: the newest entry is what was just submitted.
        assert_eq!(self.history.last().map(String::as_str), Some(trimmed));
    }

    /// Steps backwards through the history.
    pub fn history_previous(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let next = match self.history_index {
            None => {
                // Capture the in-progress text so leaving the history restores it.
                self.draft = self.text();
                self.history.len().saturating_sub(1)
            }
            Some(0) => 0,
            Some(index) => index.saturating_sub(1),
        };
        self.history_index = Some(next);
        self.load_history(next);
    }

    /// Steps forwards through the history, returning to the draft at the end.
    pub fn history_next(&mut self) {
        let Some(index) = self.history_index else {
            return;
        };
        let last = self.history.len().saturating_sub(1);
        if index >= last {
            self.history_index = None;
            let draft = std::mem::take(&mut self.draft);
            self.set_text(&draft);
            return;
        }
        let next = index.saturating_add(1);
        self.history_index = Some(next);
        self.load_history(next);
    }

    /// Replaces the buffer with a history entry.
    fn load_history(&mut self, index: usize) {
        let Some(entry) = self.history.get(index) else {
            return;
        };
        let entry = entry.clone();
        self.set_text(&entry);
    }

    /// Replaces the buffer contents and puts the cursor at the end.
    fn set_text(&mut self, text: &str) {
        self.text = text.chars().collect();
        self.cursor = self.text.len();
    }

    /// Leaves history browsing, keeping the current text.
    ///
    /// Editing a recalled entry means the user is composing something new, so
    /// further up/down presses should browse from the top rather than jump.
    const fn leave_history(&mut self) {
        self.history_index = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typing_inserts_at_the_cursor() {
        let mut buffer = InputBuffer::new();
        buffer.insert_str("hello");
        assert_eq!(buffer.text(), "hello");
        assert!(buffer.is_at_end());
        buffer.move_left();
        buffer.insert('X');
        assert_eq!(buffer.text(), "hellXo");
        assert_eq!(buffer.cursor(), 5);
    }

    #[test]
    fn multibyte_text_survives_every_edit() {
        let mut buffer = InputBuffer::new();
        buffer.insert_str("日本語");
        assert_eq!(buffer.len(), 3);
        assert_eq!(buffer.text(), "日本語");
        // A byte cursor would slice mid-character here; a character cursor cannot.
        buffer.backspace();
        assert_eq!(buffer.text(), "日本");

        // Insert a multi-byte character at the start, then delete exactly it. After
        // insertion the cursor sits *after* the new character, which is the position
        // `delete` removes from — no extra move is needed, and an extra move would
        // eat the wrong character.
        buffer.move_home();
        buffer.insert('é');
        assert_eq!(buffer.text(), "é日本");
        assert_eq!(buffer.cursor(), 1);
        assert!(buffer.delete());
        // The cursor sat after `é`, so `delete` removes the character *under* it,
        // which is `本` — the byte-oriented mistake would have removed `日`.
        assert_eq!(buffer.text(), "é本");

        // Backspace is symmetric: it removes the character *before* the cursor.
        buffer.move_end();
        assert!(buffer.backspace());
        assert_eq!(buffer.text(), "é");
    }

    #[test]
    fn backspace_at_the_start_does_nothing() {
        let mut buffer = InputBuffer::new();
        assert!(!buffer.backspace());
        buffer.insert_str("ab");
        buffer.move_home();
        assert!(!buffer.backspace());
        // The text is untouched by a no-op.
        assert_eq!(buffer.text(), "ab");
    }

    #[test]
    fn delete_at_the_end_does_nothing() {
        let mut buffer = InputBuffer::new();
        assert!(!buffer.delete());
        buffer.insert_str("ab");
        assert!(!buffer.delete());
        assert_eq!(buffer.text(), "ab");
        buffer.move_home();
        assert!(buffer.delete());
        assert_eq!(buffer.text(), "b");
    }

    #[test]
    fn the_cursor_stays_within_the_text() {
        let mut buffer = InputBuffer::new();
        buffer.insert_str("abc");
        for _ in 0..10 {
            buffer.move_left();
        }
        assert_eq!(buffer.cursor(), 0);
        for _ in 0..10 {
            buffer.move_right();
        }
        assert_eq!(buffer.cursor(), 3);
        buffer.move_home();
        assert_eq!(buffer.cursor(), 0);
        buffer.move_end();
        assert_eq!(buffer.cursor(), 3);
    }

    #[test]
    fn the_cursor_splits_the_text_for_rendering() {
        let mut buffer = InputBuffer::with_text("abcdef");
        buffer.move_home();
        buffer.move_right();
        buffer.move_right();
        assert_eq!(buffer.text_before_cursor(), "ab");
        assert_eq!(buffer.text_after_cursor(), "cdef");
        // Pair assertion: the two halves reassemble the whole.
        assert_eq!(
            format!(
                "{}{}",
                buffer.text_before_cursor(),
                buffer.text_after_cursor()
            ),
            buffer.text()
        );
    }

    #[test]
    fn submitting_clears_and_remembers() {
        let mut buffer = InputBuffer::new();
        buffer.insert_str("do the thing");
        let submitted = buffer.submit();
        assert_eq!(submitted.as_deref(), Some("do the thing"));
        assert!(buffer.is_empty());
        assert_eq!(buffer.history(), ["do the thing".to_owned()]);
    }

    #[test]
    fn whitespace_only_input_is_not_submitted() {
        let mut buffer = InputBuffer::new();
        buffer.insert_str("   \t ");
        assert!(buffer.is_empty());
        assert_eq!(buffer.submit(), None);
        // Negative space: the draft survives a stray Enter.
        assert_eq!(buffer.text(), "   \t ");
        assert!(buffer.history().is_empty());
    }

    #[test]
    fn consecutive_duplicates_are_collapsed() {
        let mut buffer = InputBuffer::new();
        for _ in 0..3 {
            buffer.insert_str("same");
            assert!(buffer.submit().is_some());
        }
        assert_eq!(buffer.history(), ["same".to_owned()]);

        buffer.insert_str("other");
        assert!(buffer.submit().is_some());
        // A genuinely different command is recorded after it.
        assert_eq!(buffer.history(), ["same".to_owned(), "other".to_owned()]);
    }

    #[test]
    fn history_browsing_walks_backwards_then_forwards() {
        let mut buffer = InputBuffer::new();
        for text in ["one", "two", "three"] {
            buffer.insert_str(text);
            assert!(buffer.submit().is_some());
        }

        buffer.history_previous();
        assert_eq!(buffer.text(), "three");
        buffer.history_previous();
        assert_eq!(buffer.text(), "two");
        buffer.history_previous();
        assert_eq!(buffer.text(), "one");
        // Further presses stay at the oldest entry rather than wrapping.
        buffer.history_previous();
        assert_eq!(buffer.text(), "one");

        buffer.history_next();
        assert_eq!(buffer.text(), "two");
        buffer.history_next();
        assert_eq!(buffer.text(), "three");
    }

    #[test]
    fn leaving_history_restores_the_draft() {
        let mut buffer = InputBuffer::new();
        buffer.insert_str("first");
        assert!(buffer.submit().is_some());

        buffer.insert_str("half-typed");
        buffer.history_previous();
        assert_eq!(buffer.text(), "first");
        // Coming back past the newest entry restores what the user was typing,
        // rather than leaving them with the recalled command.
        buffer.history_next();
        assert_eq!(buffer.text(), "half-typed");
    }

    #[test]
    fn editing_a_recalled_entry_restarts_browsing() {
        let mut buffer = InputBuffer::new();
        for text in ["one", "two"] {
            buffer.insert_str(text);
            assert!(buffer.submit().is_some());
        }
        buffer.history_previous();
        assert_eq!(buffer.text(), "two");
        buffer.insert('!');
        assert_eq!(buffer.text(), "two!");
        // Editing means composing something new, so the next Up starts from the
        // newest entry instead of continuing from the middle.
        buffer.history_previous();
        assert_eq!(buffer.text(), "two");
    }

    #[test]
    fn browsing_an_empty_history_does_nothing() {
        let mut buffer = InputBuffer::new();
        buffer.insert_str("draft");
        buffer.history_previous();
        assert_eq!(buffer.text(), "draft");
        buffer.history_next();
        assert_eq!(buffer.text(), "draft");
    }

    #[test]
    fn delete_word_eats_the_word_and_its_trailing_space() {
        let mut buffer = InputBuffer::with_text("read the file ");
        assert!(buffer.delete_word());
        assert_eq!(buffer.text(), "read the ");
        assert!(buffer.delete_word());
        assert_eq!(buffer.text(), "read ");
        assert!(buffer.delete_word());
        assert_eq!(buffer.text(), "");
        // Negative space: nothing left to remove.
        assert!(!buffer.delete_word());
    }

    #[test]
    fn delete_word_handles_a_non_breaking_space() {
        // A non-breaking space is not `char::is_whitespace`, so it is part of a
        // word; the count is characters, so no boundary is split either way.
        let mut buffer = InputBuffer::with_text("a\u{a0}b");
        assert!(buffer.delete_word());
        assert_eq!(buffer.text(), "a\u{a0}");
    }

    #[test]
    fn delete_word_from_the_start_is_a_no_op() {
        let mut buffer = InputBuffer::with_text("word");
        buffer.move_home();
        assert!(!buffer.delete_word());
        assert_eq!(buffer.text(), "word");
    }

    #[test]
    fn with_text_places_the_cursor_at_the_end() {
        let mut buffer = InputBuffer::with_text("abc");
        assert_eq!(buffer.cursor(), 3);
        assert!(buffer.is_at_end());
        buffer.insert('d');
        assert_eq!(buffer.text(), "abcd");
    }

    #[test]
    fn key_outcome_variants_are_distinguishable() {
        // The caller matches on these, so each must be distinct.
        assert_ne!(KeyOutcome::Edited, KeyOutcome::Quit);
        assert_ne!(KeyOutcome::Ignored, KeyOutcome::Edited);
        assert_ne!(
            KeyOutcome::Submitted("a".to_owned()),
            KeyOutcome::Submitted("b".to_owned())
        );
        assert_eq!(
            KeyOutcome::Submitted("a".to_owned()),
            KeyOutcome::Submitted("a".to_owned())
        );
    }

    #[test]
    fn clear_keeps_the_history() {
        let mut buffer = InputBuffer::new();
        buffer.insert_str("kept");
        assert!(buffer.submit().is_some());
        buffer.insert_str("discarded");
        buffer.clear();
        assert!(buffer.is_empty());
        // The history is not the draft, so clearing the draft must not lose it.
        assert_eq!(buffer.history(), ["kept".to_owned()]);
    }
}
