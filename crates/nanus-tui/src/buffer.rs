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
//!
//! ## Lines, not a single row
//!
//! The composer holds a newline like any other character, so a prompt can span
//! several lines. The cursor is still one character index into the whole text; the
//! line-oriented operations ([`InputBuffer::move_line_up`],
//! [`InputBuffer::move_home`]) derive their line from it rather than storing a
//! second, easily-desynchronised position.

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
    /// What the last kill removed, which `Ctrl+Y` puts back.
    ///
    /// One slot rather than a ring: a composer is not an editor, and the second-most
    /// recent kill is not a thing anybody reaches for while writing a prompt.
    killed: String,
    /// The reverse search through the history, while one is running.
    search: Option<Search>,
}

/// A reverse search through the submitted history.
///
/// Held by the composer rather than by the key handler because what it does is *replace
/// the composer's text* with a match and put it back when cancelled — that is the
/// composer's business, and a caller that owned the search would have to reach into the
/// text to do it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Search {
    /// What has been typed into the search so far.
    query: String,
    /// Where the current match sits in the history, when the query found one.
    matched: Option<usize>,
    /// What the composer held when the search began, restored when it is cancelled.
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
            killed: String::new(),
            search: None,
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
            killed: String::new(),
            search: None,
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

    /// Returns the text split into display lines, without their newlines.
    ///
    /// A trailing newline yields a final empty line, so the cursor can sit on the
    /// blank line a user opened with `Alt+Enter`.
    #[must_use]
    pub fn lines(&self) -> Vec<String> {
        self.text().split('\n').map(str::to_owned).collect()
    }

    /// Returns how many display lines the text occupies.
    ///
    /// Always at least one: an empty composer is one blank line, not zero.
    #[must_use]
    pub fn line_count(&self) -> usize {
        let newlines = self
            .text
            .iter()
            .filter(|character| **character == '\n')
            .count();
        newlines.saturating_add(1)
    }

    /// Returns `true` when the text contains a newline.
    #[must_use]
    pub fn is_multiline(&self) -> bool {
        self.text.contains(&'\n')
    }

    /// Returns the cursor's line and its column within that line.
    ///
    /// The column counts `char`s from the start of the line, matching the character
    /// cursor the rest of the buffer uses.
    #[must_use]
    pub fn cursor_line_col(&self) -> (usize, usize) {
        let mut line: usize = 0;
        let mut column: usize = 0;
        for (index, character) in self.text.iter().enumerate() {
            if index >= self.cursor {
                break;
            }
            if *character == '\n' {
                line = line.saturating_add(1);
                column = 0;
            } else {
                column = column.saturating_add(1);
            }
        }
        (line, column)
    }

    /// Returns the character index at which `line` begins.
    ///
    /// A line past the end yields the text length, so callers clamp rather than
    /// index out of bounds.
    fn line_start(&self, line: usize) -> usize {
        let mut current = 0;
        for (index, character) in self.text.iter().enumerate() {
            if current == line {
                return index;
            }
            if *character == '\n' {
                current = current.saturating_add(1);
            }
        }
        self.text.len()
    }

    /// Returns how many characters `line` holds before its newline.
    fn line_length(&self, line: usize) -> usize {
        let mut length: usize = 0;
        for character in self.text.iter().skip(self.line_start(line)) {
            if *character == '\n' {
                break;
            }
            length = length.saturating_add(1);
        }
        length
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

    /// Moves the cursor to the start of the current line.
    ///
    /// Line-relative rather than document-relative, which is what every editor does
    /// and what a multi-line composer needs: `Home` on the second line belongs at the
    /// second line's start, not at the top of the prompt.
    pub fn move_home(&mut self) {
        let (line, _) = self.cursor_line_col();
        self.cursor = self.line_start(line);
    }

    /// Moves the cursor to the end of the current line.
    pub fn move_end(&mut self) {
        let (line, _) = self.cursor_line_col();
        self.cursor = self.line_start(line).saturating_add(self.line_length(line));
    }

    /// Moves the cursor up a line, keeping its column where the line allows.
    ///
    /// Returns whether it moved; `false` means the cursor was already on the first
    /// line, which is why a caller can fall back to history browsing.
    pub fn move_line_up(&mut self) -> bool {
        let (line, column) = self.cursor_line_col();
        if line == 0 {
            return false;
        }
        let target = line.saturating_sub(1);
        let column = column.min(self.line_length(target));
        self.cursor = self.line_start(target).saturating_add(column);
        true
    }

    /// Moves the cursor down a line, keeping its column where the line allows.
    ///
    /// Returns whether it moved; `false` means the cursor was already on the last
    /// line.
    pub fn move_line_down(&mut self) -> bool {
        let (line, column) = self.cursor_line_col();
        let target = line.saturating_add(1);
        if target >= self.line_count() {
            return false;
        }
        let column = column.min(self.line_length(target));
        self.cursor = self.line_start(target).saturating_add(column);
        true
    }

    /// Moves the cursor to `column` characters into `line`, clamping to the line's end.
    ///
    /// The mouse names a cell rather than an index into the text, so a click arrives as a
    /// line and a column. Clamping rather than refusing means a click past the end of a
    /// line puts the caret at the end of it, which is what the click meant.
    pub fn place_cursor(&mut self, line: usize, column: usize) {
        let start = self.line_start(line);
        let column = column.min(self.line_length(line));
        self.cursor = start.saturating_add(column);
        self.leave_history();
        // Postcondition: the cursor stays inside the text.
        assert!(self.cursor <= self.text.len());
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

    /// Breaks the line where a trailing backslash is, which is the multiline escape.
    ///
    /// Returns whether it did. `\` followed by Enter means "break the line" in every
    /// terminal, which is the one spelling that needs no terminal cooperation at all — no
    /// keyboard protocol, no setup, no distinction between Enter and Shift+Enter.
    pub fn break_line_after_escape(&mut self) -> bool {
        if self.character_before(self.cursor) != Some('\\') {
            return false;
        }
        self.cursor = self.cursor.saturating_sub(1);
        self.text.remove(self.cursor);
        self.insert('\n');
        true
    }

    /// Deletes from the cursor to the end of the line, remembering what it removed.
    ///
    /// Line-relative rather than document-relative: a prompt is several lines often
    /// enough that "delete to the end" has to mean the end of the line the cursor is on,
    /// which is what every editor with this binding does.
    ///
    /// Returns whether anything was removed.
    pub fn kill_to_end(&mut self) -> bool {
        let end = {
            let (line, _) = self.cursor_line_col();
            self.line_start(line).saturating_add(self.line_length(line))
        };
        if end <= self.cursor {
            return false;
        }
        let removed: String = self.text.drain(self.cursor..end).collect();
        self.killed = removed;
        self.leave_history();
        true
    }

    /// Deletes the line the cursor is on, remembering what it removed.
    ///
    /// The newline that ends the line stays: removing it would join two lines the reader
    /// did not ask to join, and on the last line it would leave nothing to have deleted.
    ///
    /// Returns whether anything was removed.
    pub fn kill_line(&mut self) -> bool {
        let (line, _) = self.cursor_line_col();
        let start = self.line_start(line);
        let end = start.saturating_add(self.line_length(line));
        if start == end {
            return false;
        }
        let removed: String = self.text.drain(start..end).collect();
        self.killed = removed;
        self.cursor = start;
        self.leave_history();
        true
    }

    /// Inserts what the last kill removed, at the cursor.
    ///
    /// Returns whether there was anything to put back.
    pub fn yank(&mut self) -> bool {
        if self.killed.is_empty() {
            return false;
        }
        let killed = self.killed.clone();
        self.insert_str(&killed);
        true
    }

    /// Moves the cursor back to the start of the word before it.
    ///
    /// The same idea as [`InputBuffer::delete_word`], and deliberately: the two are used
    /// together, and a cursor that stopped somewhere other than where a delete would have
    /// started would make `Alt+B` then `Ctrl+W` delete a different word from the one the
    /// reader moved to.
    pub fn move_word_left(&mut self) {
        while self.cursor > 0
            && self
                .character_before(self.cursor)
                .is_some_and(char::is_whitespace)
        {
            self.cursor = self.cursor.saturating_sub(1);
        }
        while self.cursor > 0
            && self
                .character_before(self.cursor)
                .is_some_and(|character| !character.is_whitespace())
        {
            self.cursor = self.cursor.saturating_sub(1);
        }
    }

    /// Moves the cursor to the end of the word it is in or before.
    pub fn move_word_right(&mut self) {
        let length = self.text.len();
        while self.cursor < length
            && self
                .character_at(self.cursor)
                .is_some_and(char::is_whitespace)
        {
            self.cursor = self.cursor.saturating_add(1);
        }
        while self.cursor < length
            && self
                .character_at(self.cursor)
                .is_some_and(|character| !character.is_whitespace())
        {
            self.cursor = self.cursor.saturating_add(1);
        }
    }

    /// Returns the character just before `index`, if there is one.
    ///
    /// `checked_sub` rather than `saturating_sub`: at index zero there is no character
    /// before, and saturating turned "before the start" into *the first character*. A
    /// composer whose text began with a backslash therefore treated it as a trailing one —
    /// `\` plus Enter with the caret at the start broke the line and ate the backslash
    /// instead of submitting the prompt.
    fn character_before(&self, index: usize) -> Option<char> {
        self.text.get(index.checked_sub(1)?).copied()
    }

    /// Returns the character at `index`, if there is one.
    fn character_at(&self, index: usize) -> Option<char> {
        self.text.get(index).copied()
    }

    /// Returns `true` while a reverse search is running.
    #[must_use]
    pub const fn is_searching(&self) -> bool {
        self.search.is_some()
    }

    /// Returns the search query and whether it has found anything, for a status line.
    #[must_use]
    pub fn search_query(&self) -> Option<(&str, bool)> {
        self.search
            .as_ref()
            .map(|search| (search.query.as_str(), search.matched.is_some()))
    }

    /// Starts a reverse search, remembering the draft to restore if it is cancelled.
    pub fn search_start(&mut self) {
        if self.search.is_some() {
            return;
        }
        self.search = Some(Search {
            query: String::new(),
            matched: None,
            draft: self.text(),
        });
        self.search_refind();
    }

    /// Adds a character to the search query.
    pub fn search_push(&mut self, character: char) {
        let Some(search) = self.search.as_mut() else {
            return;
        };
        search.query.push(character);
        self.search_refind();
    }

    /// Removes the last character from the search query.
    ///
    /// Returns whether there was one: on an empty query there is nothing to delete, which
    /// is how a caller knows that a further Backspace is the reader trying to leave.
    pub fn search_backspace(&mut self) -> bool {
        let Some(search) = self.search.as_mut() else {
            return false;
        };
        if search.query.pop().is_none() {
            return false;
        }
        self.search_refind();
        true
    }

    /// Steps to the next older match.
    pub fn search_older(&mut self) {
        let Some(from) = self.search.as_ref().and_then(|search| search.matched) else {
            self.search_refind();
            return;
        };
        // Walking past the oldest match stands still rather than emptying the composer.
        // The reader has reached the end of what matches, and taking the last match away
        // would be a worse answer to "older" than keeping it — which is what a test found
        // this doing before it was written this way.
        let older = from.checked_sub(1).and_then(|older| self.find_match(older));
        if let (Some(found), Some(search)) = (older, self.search.as_mut()) {
            search.matched = Some(found);
        }
        self.show_match();
    }

    /// Ends the search, keeping the match the composer is now showing.
    pub fn search_accept(&mut self) {
        self.search = None;
        self.leave_history();
    }

    /// Ends the search and puts back what the composer held when it began.
    pub fn search_cancel(&mut self) {
        let Some(search) = self.search.take() else {
            return;
        };
        self.set_text(&search.draft);
    }

    /// Finds the newest match for the current query and shows it.
    fn search_refind(&mut self) {
        let newest = self.history.len().checked_sub(1);
        let found = newest.and_then(|newest| self.find_match(newest));
        if let Some(search) = self.search.as_mut() {
            search.matched = found;
        }
        self.show_match();
    }

    /// Shows the current match, or the draft when the query found nothing.
    ///
    /// The draft rather than the last match, because the reader is looking at a search
    /// that failed and what they will get back if they cancel is the useful thing to see.
    fn show_match(&mut self) {
        let Some(search) = self.search.as_ref() else {
            return;
        };
        let shown = search
            .matched
            .and_then(|index| self.history.get(index).cloned())
            .unwrap_or_else(|| search.draft.clone());
        self.set_text(&shown);
    }

    /// Returns the newest history entry at or before `from` that contains the query.
    ///
    /// Case-insensitive, because a prompt is prose: a reader searching for "refactor"
    /// means the entry that says "Refactor", and a search that could not see it would
    /// look broken rather than strict.
    fn find_match(&self, from: usize) -> Option<usize> {
        let needle = self.search.as_ref()?.query.to_lowercase();
        self.history
            .iter()
            .enumerate()
            .rev()
            .find(|(index, entry)| *index <= from && entry.to_lowercase().contains(&needle))
            .map(|(index, _)| index)
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
    fn kill_to_end_removes_the_rest_of_the_line_and_remembers_it() {
        let mut buffer = InputBuffer::with_text("keep this and drop that");
        for _ in 0.."keep this and drop that".len() - "keep this ".len() {
            buffer.move_left();
        }
        assert!(buffer.kill_to_end());
        assert_eq!(buffer.text(), "keep this ");
        assert!(buffer.yank());
        assert_eq!(
            buffer.text(),
            "keep this and drop that",
            "yank puts it back"
        );
    }

    /// A kill stops at the line's end rather than the buffer's, because a prompt is
    /// several lines often enough that the difference is visible.
    #[test]
    fn kill_to_end_stops_at_the_end_of_the_line() {
        let mut buffer = InputBuffer::with_text("one\ntwo\nthree");
        buffer.move_home();
        buffer.move_line_up();
        assert!(buffer.kill_to_end());
        assert_eq!(buffer.text(), "one\n\nthree");
    }

    #[test]
    fn kill_line_removes_the_line_the_cursor_is_on() {
        let mut buffer = InputBuffer::with_text("one\ntwo\nthree");
        buffer.move_home();
        buffer.move_line_up();
        assert!(buffer.kill_line());
        assert_eq!(buffer.text(), "one\n\nthree");
        assert_eq!(buffer.cursor_line_col(), (1, 0), "the cursor stays put");
        // The line is already gone: a second press has nothing to delete.
        assert!(!buffer.kill_line());
    }

    #[test]
    fn a_yank_with_nothing_killed_does_nothing() {
        let mut buffer = InputBuffer::with_text("abc");
        assert!(!buffer.yank());
        assert_eq!(buffer.text(), "abc");
    }

    /// The pairing that matters: a cursor moved by `Alt+B` is where `Ctrl+W` starts
    /// deleting, so the two keys act on the same word.
    #[test]
    fn word_movement_lands_where_a_word_delete_starts() {
        let mut moved = InputBuffer::with_text("one two three");
        moved.move_word_left();
        let mut deleted = InputBuffer::with_text("one two three");
        deleted.delete_word();
        assert_eq!(moved.cursor(), deleted.cursor());
        assert_eq!(moved.text(), "one two three");

        moved.move_word_left();
        assert_eq!(moved.cursor(), "one ".len());
        moved.move_word_right();
        assert_eq!(moved.cursor(), "one two".len());
    }

    #[test]
    fn word_movement_stops_at_the_ends_of_the_text() {
        let mut buffer = InputBuffer::with_text("   ");
        buffer.move_word_left();
        assert_eq!(buffer.cursor(), 0);
        buffer.move_word_right();
        assert_eq!(buffer.cursor(), 3);
        let mut empty = InputBuffer::new();
        empty.move_word_left();
        empty.move_word_right();
        assert_eq!(empty.cursor(), 0);
    }

    #[test]
    fn a_backslash_before_enter_breaks_the_line() {
        let mut buffer = InputBuffer::with_text("first \\");
        assert!(buffer.break_line_after_escape());
        assert_eq!(buffer.text(), "first \n", "the backslash is consumed");
        // Without one, the caller is free to treat Enter as a submission.
        assert!(!buffer.break_line_after_escape());
    }

    /// A backslash *at* the caret is not before it. Reading "before the start" as the first
    /// character made a prompt that opens with a backslash unsendable: Enter broke the line
    /// and ate the character instead of submitting.
    #[test]
    fn a_backslash_at_the_start_of_the_line_is_not_before_the_caret() {
        let mut buffer = InputBuffer::with_text("\\abc");
        buffer.move_home();
        assert_eq!(buffer.cursor(), 0);
        assert!(!buffer.break_line_after_escape());
        assert_eq!(buffer.text(), "\\abc", "nothing was consumed");
        // The other direction: one character later, it is before the caret again.
        buffer.move_right();
        assert!(buffer.break_line_after_escape());
        assert_eq!(buffer.text(), "\nabc");
    }

    /// The search walks the history newest-first, which is what makes repeated `Ctrl+R`
    /// mean "older" rather than "another".
    #[test]
    fn a_reverse_search_finds_the_newest_match_and_walks_back() {
        let mut buffer = InputBuffer::new();
        for entry in ["cargo test", "cargo fmt", "git status"] {
            buffer.insert_str(entry);
            assert!(buffer.submit().is_some());
        }
        buffer.search_start();
        assert_eq!(
            buffer.text(),
            "git status",
            "the newest entry to start with"
        );
        buffer.search_push('c');
        buffer.search_push('a');
        buffer.search_push('r');
        assert_eq!(buffer.text(), "cargo fmt");
        buffer.search_older();
        assert_eq!(
            buffer.text(),
            "cargo test",
            "and further back on a second press"
        );
        buffer.search_older();
        assert_eq!(buffer.text(), "cargo test", "and stops at the oldest match");
    }

    /// Matching is case-insensitive: a prompt is prose, and a search that could not see
    /// "Refactor" when asked for "refactor" would read as broken rather than strict.
    #[test]
    fn a_search_ignores_case() {
        let mut buffer = InputBuffer::new();
        buffer.insert_str("Refactor the parser");
        assert!(buffer.submit().is_some());
        buffer.search_start();
        for character in "refactor".chars() {
            buffer.search_push(character);
        }
        assert_eq!(buffer.text(), "Refactor the parser");
        assert_eq!(buffer.search_query(), Some(("refactor", true)));
    }

    #[test]
    fn a_search_that_matches_nothing_says_so_and_keeps_the_draft() {
        let mut buffer = InputBuffer::new();
        buffer.insert_str("something else");
        assert!(buffer.submit().is_some());
        buffer.insert_str("half a thought");
        buffer.search_start();
        buffer.search_push('z');
        assert_eq!(buffer.search_query(), Some(("z", false)));
        assert_eq!(
            buffer.text(),
            "half a thought",
            "the draft, not a stale match"
        );
    }

    #[test]
    fn cancelling_a_search_gives_back_what_was_being_typed() {
        let mut buffer = InputBuffer::new();
        buffer.insert_str("an earlier prompt");
        assert!(buffer.submit().is_some());
        buffer.insert_str("in progress");
        buffer.search_start();
        assert_eq!(
            buffer.text(),
            "an earlier prompt",
            "the search shows the match"
        );
        buffer.search_cancel();
        assert!(!buffer.is_searching());
        assert_eq!(buffer.text(), "in progress");
    }

    #[test]
    fn accepting_a_search_keeps_the_match_and_leaves_the_search() {
        let mut buffer = InputBuffer::new();
        buffer.insert_str("an earlier prompt");
        assert!(buffer.submit().is_some());
        buffer.search_start();
        buffer.search_accept();
        assert!(!buffer.is_searching());
        assert_eq!(buffer.text(), "an earlier prompt");
    }

    #[test]
    fn backspace_stops_at_an_empty_query() {
        let mut buffer = InputBuffer::new();
        buffer.insert_str("an earlier prompt");
        assert!(buffer.submit().is_some());
        buffer.search_start();
        buffer.search_push('e');
        assert!(buffer.search_backspace(), "there was a character to remove");
        assert!(!buffer.search_backspace(), "and then there was not");
        assert!(buffer.is_searching(), "which is the caller's cue to leave");
    }

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

    #[test]
    fn a_newline_splits_the_text_into_lines() {
        let mut buffer = InputBuffer::new();
        buffer.insert_str("one");
        buffer.insert('\n');
        buffer.insert_str("two");
        assert_eq!(buffer.line_count(), 2);
        assert!(buffer.is_multiline());
        assert_eq!(buffer.lines(), ["one".to_owned(), "two".to_owned()]);
        // A single-line buffer is the common case and is not reported as multi-line.
        let single = InputBuffer::with_text("one");
        assert!(!single.is_multiline());
        assert_eq!(single.line_count(), 1);
    }

    #[test]
    fn a_trailing_newline_leaves_a_blank_line_for_the_cursor() {
        let mut buffer = InputBuffer::new();
        buffer.insert_str("line\n");
        assert_eq!(buffer.line_count(), 2);
        // The cursor sits on the blank line the newline opened, which is where the
        // next keystroke lands.
        assert_eq!(buffer.cursor_line_col(), (1, 0));
        assert_eq!(buffer.lines(), ["line".to_owned(), String::new()]);
    }

    #[test]
    fn the_cursor_line_and_column_track_newlines() {
        let mut buffer = InputBuffer::with_text("ab\ncde");
        assert_eq!(buffer.cursor_line_col(), (1, 3));
        buffer.move_home();
        assert_eq!(buffer.cursor_line_col(), (1, 0));
        // `Home` and `End` are line-relative, so the top of the prompt is a separate
        // move rather than where `Home` returns to.
        buffer.move_line_up();
        assert_eq!(buffer.cursor_line_col(), (0, 0));
        buffer.move_end();
        assert_eq!(buffer.cursor_line_col(), (0, 2));
        assert!(buffer.move_line_down());
        assert_eq!(buffer.cursor_line_col(), (1, 2));
        assert!(!buffer.move_line_down(), "the last line is the floor");
        assert!(buffer.move_line_up(), "moving back up is possible");
    }

    #[test]
    fn moving_between_lines_keeps_the_column_where_it_fits() {
        let mut buffer = InputBuffer::with_text("abcd\nxy");
        // `with_text` leaves the cursor at the end of the last line; `Home` is
        // line-relative, so reaching column two of the *first* line takes a line move.
        buffer.move_home();
        buffer.move_line_up();
        buffer.move_right();
        buffer.move_right();
        assert_eq!(buffer.cursor_line_col(), (0, 2));
        assert!(buffer.move_line_down());
        assert_eq!(buffer.cursor_line_col(), (1, 2));
        assert!(buffer.move_line_up());
        assert_eq!(buffer.cursor_line_col(), (0, 2));
    }

    #[test]
    fn a_short_line_clamps_the_column() {
        let mut buffer = InputBuffer::with_text("xy\nabcdef");
        buffer.move_end();
        assert_eq!(buffer.cursor_line_col(), (1, 6));
        assert!(buffer.move_line_up());
        // The first line holds only two characters, so the cursor cannot keep its
        // column of six.
        assert_eq!(buffer.cursor_line_col(), (0, 2));
        // The top of the buffer is the end of the upward walk.
        assert!(!buffer.move_line_up());
    }

    #[test]
    fn deleting_within_a_multiline_draft_does_not_panic() {
        let mut buffer = InputBuffer::with_text("ab\ncd");
        buffer.move_home();
        buffer.move_line_up();
        buffer.move_end();
        // Backspace removes the `b`, leaving the newline and the second line intact.
        assert!(buffer.backspace());
        assert_eq!(buffer.text(), "a\ncd");
        assert_eq!(buffer.line_count(), 2);
    }
}
