//! The transcript: what the interface is a view of.
//!
//! The transcript is deliberately *not* the session log. A session log is the
//! durable, model-visible record; a transcript is what a human is currently looking
//! at, including transient state that is never persisted — a tool call that has
//! started but not finished, or a reply that is still streaming in.
//!
//! Keeping them separate is what lets the interface render a half-finished answer
//! without inventing a session event for a fact that is not yet settled.

use core::fmt;

/// Who produced an entry.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    /// The human at the keyboard.
    User,
    /// The model's answer.
    Assistant,
    /// The model's reasoning, which is not an answer.
    Reasoning,
    /// A tool call the model requested, or its result.
    Tool,
    /// The harness itself: a status change, an error, or a notice.
    Harness,
}

impl Role {
    /// Returns a short label for the role.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::User => "you",
            Self::Assistant => "nanus",
            Self::Reasoning => "thinking",
            Self::Tool => "tool",
            Self::Harness => "harness",
        }
    }

    /// Returns `true` when the entry is the model's own output.
    #[must_use]
    pub const fn is_model(&self) -> bool {
        matches!(self, Self::Assistant | Self::Reasoning)
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// The kind of an entry, which decides how it renders.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum EntryKind {
    /// Prose.
    Text,
    /// A tool invocation, before or after it runs.
    ToolCall {
        /// The tool's name.
        name: String,
        /// The arguments, rendered as JSON.
        arguments: String,
    },
    /// A tool's outcome.
    ToolResult {
        /// The tool's name.
        name: String,
        /// Whether the tool reported failure.
        is_error: bool,
        /// The rendered content.
        content: String,
    },
    /// A harness notice: a policy decision, a lifecycle event, a warning.
    Notice,
}

/// One rendered line of the conversation.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Entry {
    role: Role,
    kind: EntryKind,
    text: String,
    /// `true` while the entry is still being appended to.
    streaming: bool,
}

impl Entry {
    /// Builds a settled prose entry.
    ///
    /// Named `prose` rather than `text` so the [`Entry::text`] accessor can carry
    /// the obvious name.
    #[must_use]
    pub fn prose(role: Role, text: impl Into<String>) -> Self {
        Self {
            role,
            kind: EntryKind::Text,
            text: text.into(),
            streaming: false,
        }
    }

    /// Builds a streaming text entry, which the view renders with a cursor.
    #[must_use]
    pub fn streaming(role: Role) -> Self {
        Self {
            role,
            kind: EntryKind::Text,
            text: String::new(),
            streaming: true,
        }
    }

    /// Builds a tool-call entry.
    #[must_use]
    pub fn tool_call(name: impl Into<String>, arguments: impl Into<String>) -> Self {
        let name = name.into();
        Self {
            role: Role::Tool,
            kind: EntryKind::ToolCall {
                name: name.clone(),
                arguments: arguments.into(),
            },
            text: name,
            streaming: true,
        }
    }

    /// Builds a tool-result entry.
    #[must_use]
    pub fn tool_result(
        name: impl Into<String>,
        is_error: bool,
        content: impl Into<String>,
    ) -> Self {
        let name = name.into();
        let content = content.into();
        // The content is both the entry's rendered text and part of its kind, so it
        // is cloned exactly once; the name is only ever used inside the kind.
        // Build the kind first, moving the content in, then clone it back out for
        // the entry's rendered text. That is one clone rather than two.
        let kind = EntryKind::ToolResult {
            name,
            is_error,
            content,
        };
        let text = match &kind {
            EntryKind::ToolResult { content, .. } => content.clone(),
            _ => String::new(),
        };
        Self {
            role: Role::Tool,
            kind,
            text,
            streaming: false,
        }
    }

    /// Builds a harness notice.
    #[must_use]
    pub fn notice(text: impl Into<String>) -> Self {
        Self {
            role: Role::Harness,
            kind: EntryKind::Notice,
            text: text.into(),
            streaming: false,
        }
    }

    /// Returns the entry's rendered text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Returns the entry's role.
    #[must_use]
    pub const fn role(&self) -> Role {
        self.role
    }

    /// Returns the entry's kind.
    #[must_use]
    pub const fn kind(&self) -> &EntryKind {
        &self.kind
    }

    /// Returns `true` while the entry is still being appended to.
    #[must_use]
    pub const fn is_streaming(&self) -> bool {
        self.streaming
    }

    /// Appends a fragment, marking the entry settled when `final_chunk` is true.
    ///
    /// # Panics
    ///
    /// Panics when the entry is already settled. Appending to a settled entry would
    /// silently rewrite history the reader has already seen.
    pub fn push_str(&mut self, fragment: &str, final_chunk: bool) {
        assert!(self.streaming, "a settled entry is never appended to");
        self.text.push_str(fragment);
        if final_chunk {
            self.streaming = false;
        }
    }

    /// Marks the entry settled without appending anything.
    pub fn settle(&mut self) {
        self.streaming = false;
    }

    /// Returns the number of lines the entry occupies at `width` columns in the **full**
    /// rendering.
    ///
    /// The view needs this before rendering so it can scroll without measuring the
    /// buffer, and an entry's height must be computable from the entry alone.
    ///
    /// The full rendering is named deliberately: the compact form the interface draws by
    /// default is one line for a tool call and one for a thinking segment, so this is the
    /// height of what `tui_detail = "full"` shows rather than of what is on screen. The
    /// view measures the lines it actually draws instead of asking here, which is why the
    /// two cannot drift.
    #[must_use]
    pub fn height_at(&self, width: u16) -> u16 {
        let usable = width.max(1);
        let mut lines: u32 = 0;
        for paragraph in self.text.split('\n') {
            let wrapped = wrap_count(paragraph, usable);
            lines = lines.saturating_add(wrapped);
        }
        // A tool call renders a header line, an argument block, and a blank spacer.
        let extra = match &self.kind {
            EntryKind::ToolCall { .. } => 3,
            EntryKind::ToolResult { .. } => 2,
            EntryKind::Text | EntryKind::Notice => 1,
        };
        let total = lines.saturating_add(extra);
        u16::try_from(total).unwrap_or(u16::MAX)
    }
}

/// Counts the display rows a line of text needs at `width` columns.
///
/// The break modelled here is the one the renderer makes: words are packed until the next will
/// not fit, the space that would have separated them is dropped when the row breaks, and a word
/// longer than a row is split across rows.
///
/// That last part is why this is not `ceil(characters / width)`. Ceiling division cannot see the
/// space a broken row wastes, so it under-counts a line made of words that are each just over
/// half the width — the shape of a list of paths — and an under-count is not a cosmetic error:
/// the scroll bound is computed from it, so the rows it missed cannot be reached at any offset.
/// The estimate was a lower bound and the reader had no way to see the rest of the line.
#[must_use]
pub(crate) fn wrap_count(text: &str, width: u16) -> u32 {
    let usable = u32::from(width.max(1));
    // An empty paragraph still occupies the row it is drawn on.
    if text.is_empty() {
        return 1;
    }
    text.split('\n')
        .map(|line| rows_of(line, usable))
        .fold(0_u32, u32::saturating_add)
}

/// Counts the rows one paragraph of `text` occupies, in rows of at most `width` columns.
fn rows_of(line: &str, width: u32) -> u32 {
    if line.is_empty() {
        return 1;
    }
    let mut rows = 1_u32;
    let mut used = 0_u32;
    // Whitespace since the last word: it is kept inside a row and dropped at a break.
    let mut pending = 0_u32;
    let mut word = 0_u32;
    for character in line.chars() {
        if character.is_whitespace() {
            if word > 0 {
                place(&mut rows, &mut used, &mut pending, word, width);
                word = 0;
            }
            pending = pending.saturating_add(1);
        } else {
            word = word.saturating_add(1);
        }
    }
    if word > 0 {
        place(&mut rows, &mut used, &mut pending, word, width);
    }
    rows
}

/// Places one word on the current row, breaking the row — and the word — as it must.
fn place(rows: &mut u32, used: &mut u32, pending: &mut u32, mut word: u32, width: u32) {
    let separator = if *used == 0 { 0 } else { *pending };
    if used.saturating_add(separator).saturating_add(word) <= width {
        *used = used.saturating_add(separator).saturating_add(word);
        *pending = 0;
        return;
    }
    if *used > 0 {
        *rows = rows.saturating_add(1);
        *used = 0;
    }
    *pending = 0;
    // A word wider than a row is split rather than allowed to overflow one.
    while word > width {
        *rows = rows.saturating_add(1);
        word = word.saturating_sub(width);
    }
    *used = word;
}

/// An ordered list of entries, with a streaming tail at most.
///
/// The transcript enforces one invariant that matters for rendering: **only the
/// last entry may be streaming**. Two concurrently streaming entries would make the
/// cursor ambiguous, and the agent loop never produces them — a step yields either
/// model text or tool calls, not both at once.
#[derive(Default)]
pub struct Transcript {
    entries: Vec<Entry>,
}

impl fmt::Debug for Transcript {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Transcript")
            .field("entries", &self.entries.len())
            .field("streaming", &self.is_streaming())
            .finish_non_exhaustive()
    }
}

impl Transcript {
    /// Creates an empty transcript.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Returns the entries.
    #[must_use]
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Returns the number of entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns `true` when there is nothing to show.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Returns `true` when the last entry is still streaming.
    #[must_use]
    pub fn is_streaming(&self) -> bool {
        self.entries.last().is_some_and(Entry::is_streaming)
    }

    /// Appends a settled entry.
    ///
    /// Any streaming tail is settled first: a new entry means the previous one is
    /// finished, whether or not the model said so.
    pub fn push(&mut self, entry: Entry) {
        self.settle_tail();
        self.entries.push(entry);
        // Postcondition: at most the last entry streams, which the view relies on
        // to draw a single cursor.
        assert!(self.streaming_count() <= 1, "at most one entry streams");
    }

    /// Appends a fragment to the streaming tail, starting one when needed.
    ///
    /// A fragment whose role differs from the streaming tail starts a new entry: the
    /// model's reasoning and its answer are different things, and letting them share
    /// one entry would render them as a single paragraph under one label. Arriving at
    /// the answer therefore settles the reasoning that preceded it.
    ///
    /// Returns a mutable handle to the entry the fragment landed in.
    pub fn append_stream(&mut self, role: Role, fragment: &str, final_chunk: bool) -> &mut Entry {
        let continues = self
            .entries
            .last()
            .is_some_and(|entry| entry.is_streaming() && entry.role() == role);
        if !continues {
            self.settle_tail();
            self.entries.push(Entry::streaming(role));
        }
        // The push above guarantees a tail exists, so the `let else` below cannot
        // take the failing branch; it is written as an assertion rather than a panic
        // path.
        let tail = self.entries.last_mut();
        assert!(tail.is_some(), "a streaming tail was just appended");
        let Some(entry) = tail else {
            unreachable!("a streaming tail was just appended")
        };
        entry.push_str(fragment, final_chunk);
        entry
    }

    /// Settles the streaming tail, if there is one.
    pub fn settle_tail(&mut self) {
        if let Some(entry) = self.entries.last_mut() {
            entry.settle();
        }
    }

    /// Settles the turn's final text into the transcript, without drawing it twice.
    ///
    /// The answer arrives twice on purpose: once as it streamed, delta by delta, and once
    /// whole in the frame that ends the turn, so a client that attached late or lost a
    /// delta still ends up with it. When the tail already holds exactly that text the two
    /// are the same words, and appending the second copy draws the answer twice — once
    /// where it was written and once after everything that came later. Settling the entry
    /// that is already there is what makes the frame a reconciliation rather than a
    /// repetition.
    pub fn settle_with(&mut self, role: Role, text: &str) {
        let already_said = self
            .entries
            .last()
            .is_some_and(|entry| entry.role() == role && entry.text() == text);
        // An empty answer is not worth an entry of its own, and a turn with nothing to
        // say still has a stream to settle.
        if !already_said && !text.is_empty() {
            self.push(Entry::prose(role, text));
        }
        self.settle_tail();
    }

    /// Removes every entry.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Returns how many entries are currently streaming.
    #[must_use]
    pub fn streaming_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| entry.is_streaming())
            .count()
    }

    /// Returns the total rendered height of the transcript at `width` columns.
    #[must_use]
    pub fn height_at(&self, width: u16) -> u32 {
        self.entries.iter().fold(0_u32, |total, entry| {
            total.saturating_add(u32::from(entry.height_at(width)))
        })
    }

    /// Returns the entries that can be fully rendered starting at `offset` rows.
    ///
    /// Used to render a viewport without measuring the whole buffer twice: the view
    /// asks for the slice it can draw, and the transcript answers which entries that
    /// is.
    #[must_use]
    pub fn viewport(&self, width: u16, offset: u32, height: u16) -> Vec<&Entry> {
        let mut skipped = 0_u32;
        let mut remaining = u32::from(height);
        let mut visible = Vec::new();
        for entry in &self.entries {
            let rows = u32::from(entry.height_at(width));
            if skipped.saturating_add(rows) <= offset {
                skipped = skipped.saturating_add(rows);
                continue;
            }
            if remaining == 0 {
                break;
            }
            visible.push(entry);
            remaining = remaining.saturating_sub(rows);
        }
        visible
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_labels_are_stable() {
        assert_eq!(Role::User.label(), "you");
        assert_eq!(Role::Assistant.label(), "nanus");
        assert_eq!(Role::Reasoning.label(), "thinking");
        assert_eq!(Role::Tool.label(), "tool");
        assert_eq!(Role::Harness.label(), "harness");
        // Only the model's own output counts as model output.
        assert!(Role::Assistant.is_model());
        assert!(Role::Reasoning.is_model());
        assert!(!Role::User.is_model());
        assert!(!Role::Tool.is_model());
    }

    #[test]
    fn appending_streams_then_settles() {
        let mut transcript = Transcript::new();
        let entry = transcript.append_stream(Role::Assistant, "Hel", false);
        assert_eq!(entry.text(), "Hel");
        assert!(entry.is_streaming());

        let entry = transcript.append_stream(Role::Assistant, "lo", true);
        assert_eq!(entry.text(), "Hello");
        assert!(!entry.is_streaming());
        assert_eq!(transcript.len(), 1);
        assert!(!transcript.is_streaming());
    }

    #[test]
    fn a_new_entry_settles_the_previous_stream() {
        let mut transcript = Transcript::new();
        transcript.append_stream(Role::Assistant, "partial", false);
        transcript.push(Entry::prose(Role::User, "next"));
        // Two streaming entries would make the cursor ambiguous.
        assert_eq!(transcript.streaming_count(), 0);
        assert_eq!(transcript.len(), 2);
    }

    #[test]
    fn a_role_change_settles_the_previous_stream() {
        let mut transcript = Transcript::new();
        transcript.append_stream(Role::Reasoning, "why", false);
        assert!(transcript.is_streaming());
        transcript.append_stream(Role::Assistant, "because", false);
        // The reasoning is finished, and the answer is its own entry.
        assert_eq!(transcript.len(), 2);
        assert_eq!(transcript.streaming_count(), 1);
        assert_eq!(transcript.entries()[0].role(), Role::Reasoning);
        assert!(!transcript.entries()[0].is_streaming());
        assert_eq!(transcript.entries()[1].role(), Role::Assistant);
    }

    #[test]
    fn a_different_role_starts_a_new_entry() {
        let mut transcript = Transcript::new();
        transcript.append_stream(Role::Reasoning, "thinking", true);
        transcript.append_stream(Role::Assistant, "answer", true);
        assert_eq!(transcript.len(), 2);
        assert_eq!(transcript.entries()[0].role(), Role::Reasoning);
        assert_eq!(transcript.entries()[1].role(), Role::Assistant);
    }

    #[test]
    fn push_never_leaves_two_streams() {
        let mut transcript = Transcript::new();
        transcript.append_stream(Role::Assistant, "a", false);
        transcript.push(Entry::tool_call("read", "{}"));
        // The tool call is itself the streaming tail.
        assert_eq!(transcript.streaming_count(), 1);
        assert!(transcript.is_streaming());
    }

    #[test]
    fn clear_empties_the_transcript() {
        let mut transcript = Transcript::new();
        transcript.push(Entry::prose(Role::User, "hi"));
        assert!(!transcript.is_empty());
        transcript.clear();
        assert!(transcript.is_empty());
        assert_eq!(transcript.height_at(80), 0);
    }

    #[test]
    fn wrapping_counts_lines_at_the_boundary() {
        // Every text entry carries one header row, so the assertions are one more
        // than the wrapped paragraph's height.
        let entry = Entry::prose(Role::Assistant, "aaaa");
        // Exactly at the width: one row.
        assert_eq!(entry.height_at(4), 2);
        // One column narrower: `aaaa` splits into `aaa` and `a`, so two rows.
        assert_eq!(entry.height_at(3), 3);
        // One column wider: still one row.
        assert_eq!(entry.height_at(5), 2);
    }

    #[test]
    fn a_word_longer_than_the_line_still_wraps() {
        let entry = Entry::prose(Role::Assistant, "abcdefghij");
        // Ten characters at four columns is three rows (`abcd`, `efgh`, `ij`),
        // plus the header.
        assert_eq!(entry.height_at(4), 4);
    }

    #[test]
    fn embedded_newlines_each_start_a_line() {
        let entry = Entry::prose(Role::Assistant, "one\ntwo\nthree");
        // Three source lines plus the header row.
        assert_eq!(entry.height_at(40), 4);
    }

    #[test]
    fn tool_entries_are_taller_than_prose() {
        let text = Entry::prose(Role::Assistant, "x");
        let call = Entry::tool_call("read", "{}");
        let result = Entry::tool_result("read", false, "x");
        assert!(call.height_at(40) > text.height_at(40));
        assert!(result.height_at(40) > text.height_at(40));
    }

    #[test]
    fn viewport_skips_the_scrolled_off_prefix() {
        let mut transcript = Transcript::new();
        // Each entry is two rows at width 80: one header plus one row of text.
        for index in 0..5 {
            transcript.push(Entry::prose(Role::User, format!("{index}")));
        }
        let all = transcript.viewport(80, 0, 100);
        assert_eq!(all.len(), 5);
        // Scrolling past the first two entries (four rows) leaves the last three.
        let scrolled = transcript.viewport(80, 4, 100);
        assert_eq!(scrolled.len(), 3);
        assert_eq!(scrolled[0].text(), "2");
    }

    #[test]
    fn viewport_respects_the_height_budget() {
        let mut transcript = Transcript::new();
        for index in 0..10 {
            transcript.push(Entry::prose(Role::User, format!("line {index}")));
        }
        // Two rows per entry, so a four-row budget shows two entries.
        let visible = transcript.viewport(80, 0, 4);
        assert_eq!(visible.len(), 2);
    }

    #[test]
    fn total_height_is_the_sum_of_entries() {
        let mut transcript = Transcript::new();
        transcript.push(Entry::prose(Role::User, "a"));
        transcript.push(Entry::prose(Role::Assistant, "b"));
        let expected = transcript.entries().iter().fold(0_u32, |total, entry| {
            total.saturating_add(u32::from(entry.height_at(80)))
        });
        assert_eq!(transcript.height_at(80), expected);
        assert!(expected > 0);
    }

    #[test]
    fn tool_result_records_failure_distinctly() {
        let ok = Entry::tool_result("read", false, "content");
        let bad = Entry::tool_result("read", true, "missing");
        assert_ne!(ok.kind(), bad.kind());
        assert!(matches!(
            bad.kind(),
            EntryKind::ToolResult { is_error: true, .. }
        ));
    }

    #[test]
    fn a_settled_entry_refuses_an_append() {
        let entry = Entry::prose(Role::User, "done");
        assert!(!entry.is_streaming());
        // Appending would rewrite text the reader has already seen, so it is a
        // programmer error rather than a silent mutation.
        let outcome = std::panic::catch_unwind(move || {
            let mut entry = entry;
            entry.push_str("more", false);
        });
        assert!(outcome.is_err());
    }
    /// The count models the break the renderer makes, and these are the cases where ceiling
    /// division and word wrapping disagree — the first two measured against ratatui's own line
    /// count. Every one of them is a line whose bottom rows were unreachable while the estimate
    /// was a lower bound.
    #[test]
    fn a_wrapped_line_is_counted_the_way_it_is_drawn() {
        assert_eq!(wrap_count("hello world this is a test of wrapping", 10), 5);
        assert_eq!(wrap_count("a bbbbb a bbbbb", 5), 4);
        // Words just over half the width: one per row, where `ceil(chars / width)` said four.
        let tokens = ["TOKEN0000000000000000"; 6].join(" ");
        assert_eq!(tokens.chars().count(), 131);
        assert_eq!(wrap_count(&tokens, 40), 6);
        // A word wider than a row is split rather than allowed to overflow one.
        assert_eq!(wrap_count(&"x".repeat(25), 10), 3);
        // Lines break where the text does, and the obvious counts stay obvious.
        assert_eq!(wrap_count("one\ntwo", 40), 2);
        assert_eq!(wrap_count("short", 40), 1);
        assert_eq!(wrap_count("", 40), 1);
        // A row of spaces is a row.
        assert_eq!(wrap_count("   ", 40), 1);
    }
}
