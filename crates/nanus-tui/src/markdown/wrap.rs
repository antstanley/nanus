//! Wrapping styled spans to a column budget.
//!
//! The renderer builds each block's text as spans and then hands them here, so the
//! wrap decision is made once, in one place, by display width. A line this module says
//! fits within `width` is a line ratatui will draw as one row — which is the property
//! the transcript's scroll arithmetic depends on, since it counts wrapped rows before
//! the widget ever draws them.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::text::{char_width, str_width};

/// One unit of a span's text.
enum Token {
    /// A run with no spaces in it.
    Word(String),
    /// A single space.
    Space,
    /// A hard line break.
    Newline,
}

/// Wraps styled spans, prefixing the first line and indenting continuations.
///
/// Word wrapping: a word is placed whole unless it is wider than a line, in which case
/// it is split. The space that would have separated a wrapped pair is dropped at the
/// break rather than left dangling at the end of the row.
pub(crate) fn wrap(
    spans: &[Span<'static>],
    width: usize,
    first_prefix: &str,
    continuation: &str,
    prefix_style: Style,
) -> Vec<Line<'static>> {
    let mut builder = Builder::new(width, first_prefix, continuation, prefix_style);
    for span in spans {
        for token in tokens(&span.content) {
            match token {
                Token::Word(word) => builder.word(&word, span.style),
                Token::Space => builder.pending = true,
                Token::Newline => builder.newline(),
            }
        }
    }
    builder.finish()
}

/// Hard-wraps `text` at `width`, breaking lines mid-word and preserving indentation.
///
/// This is the code-block rule: a word wrap would reflow a program into something that
/// no longer compiles, so a code line too long for the terminal is cut at the column
/// budget and continued on the next row, indented by `prefix` again.
pub(crate) fn hard_wrap(
    text: &str,
    width: usize,
    prefix: &str,
    prefix_style: Style,
    body_style: Style,
) -> Vec<Line<'static>> {
    let width = width.max(1);
    // Leave at least one column for content, so a prefix as wide as the terminal
    // cannot push a drawn line past it.
    let prefix = clamp(prefix, width.saturating_sub(1));
    let prefix_width = str_width(&prefix);
    let room = width.saturating_sub(prefix_width).max(1);
    let mut out: Vec<Line<'static>> = Vec::new();
    for source_line in text.split('\n') {
        let mut spans: Vec<Span<'static>> = vec![Span::styled(prefix.clone(), prefix_style)];
        let mut used = 0_usize;
        for character in source_line.chars() {
            let current = char_width(character);
            if used.saturating_add(current) > room && used > 0 {
                out.push(Line::from(std::mem::take(&mut spans)));
                spans = vec![Span::styled(prefix.clone(), prefix_style)];
                used = 0;
            }
            let mut buffer = [0_u8; 4];
            spans.push(Span::styled(
                character.encode_utf8(&mut buffer).to_owned(),
                body_style,
            ));
            used = used.saturating_add(current);
        }
        out.push(Line::from(spans));
    }
    out
}

/// Truncates `text` to at most `width` columns, for a prefix that will not fit.
fn clamp(text: &str, width: usize) -> String {
    if str_width(text) <= width {
        return text.to_owned();
    }
    let mut out = String::new();
    let mut used = 0_usize;
    for character in text.chars() {
        let current = char_width(character);
        if used.saturating_add(current) > width {
            break;
        }
        out.push(character);
        used = used.saturating_add(current);
    }
    out
}

/// Splits one span's text into tokens, keeping wide characters breakable.
fn tokens(text: &str) -> Vec<Token> {
    let mut out = Vec::new();
    let mut word = String::new();
    for character in text.chars() {
        match character {
            '\n' => {
                push_word(&mut out, &mut word);
                out.push(Token::Newline);
            }
            ' ' => {
                push_word(&mut out, &mut word);
                out.push(Token::Space);
            }
            _ if char_width(character) >= 2 => {
                // A wide glyph is its own token so that a run of CJK can break between
                // any two characters, which is what a reader of it expects.
                push_word(&mut out, &mut word);
                out.push(Token::Word(character.to_string()));
            }
            _ => word.push(character),
        }
    }
    push_word(&mut out, &mut word);
    out
}

/// Flushes the in-progress word, if any.
fn push_word(out: &mut Vec<Token>, word: &mut String) {
    if !word.is_empty() {
        out.push(Token::Word(std::mem::take(word)));
    }
}

/// Accumulates wrapped lines, tracking the current row's width.
struct Builder {
    out: Vec<Line<'static>>,
    line: Vec<Span<'static>>,
    used: usize,
    /// The width of the current line's prefix, so the wrap compares content to content.
    prefix: usize,
    /// Whether the current line holds anything beyond its prefix.
    content: bool,
    pending: bool,
    budget: usize,
    continuation: String,
    prefix_style: Style,
}

impl Builder {
    /// Starts a builder on its first line.
    fn new(budget: usize, first: &str, continuation: &str, prefix_style: Style) -> Self {
        let mut builder = Self {
            out: Vec::new(),
            line: Vec::new(),
            used: 0,
            prefix: 0,
            content: false,
            pending: false,
            budget: budget.max(1),
            continuation: continuation.to_owned(),
            prefix_style,
        };
        builder.open(first);
        builder
    }

    /// Begins a fresh line with `prefix`.
    fn open(&mut self, prefix: &str) {
        // Reserve at least one column for content, so an over-wide prefix cannot make
        // a drawn line wider than the budget.
        let prefix = clamp(prefix, self.budget.saturating_sub(1));
        self.used = str_width(&prefix);
        self.prefix = self.used;
        self.content = false;
        self.pending = false;
        if !prefix.is_empty() {
            self.line.push(Span::styled(prefix, self.prefix_style));
        }
    }

    /// Ends the current line and starts the next on the continuation prefix.
    fn newline(&mut self) {
        self.out.push(Line::from(std::mem::take(&mut self.line)));
        let continuation = self.continuation.clone();
        self.open(&continuation);
    }

    /// Places one word, wrapping first if it would not fit.
    fn word(&mut self, word: &str, style: Style) {
        let word_width = str_width(word);
        let separator = usize::from(self.pending && self.content);
        let would_overflow = self
            .used
            .saturating_add(separator)
            .saturating_add(word_width)
            > self.budget;
        if self.content && would_overflow {
            self.newline();
        }
        if self.pending && self.content {
            self.push(" ", style);
        }
        self.pending = false;
        if word_width > self.budget.saturating_sub(self.prefix) {
            self.hard_split(word, style);
        } else {
            self.push(word, style);
        }
    }

    /// Splits a word wider than a line across as many rows as it needs.
    fn hard_split(&mut self, word: &str, style: Style) {
        let mut chunk = String::new();
        let mut chunk_width = 0_usize;
        for character in word.chars() {
            let current = char_width(character);
            let room = self.budget.saturating_sub(self.prefix).max(1);
            if chunk_width.saturating_add(current) > room && !chunk.is_empty() {
                self.push(&chunk, style);
                chunk.clear();
                chunk_width = 0;
                self.newline();
            }
            chunk.push(character);
            chunk_width = chunk_width.saturating_add(current);
        }
        if !chunk.is_empty() {
            self.push(&chunk, style);
        }
    }

    /// Appends styled text to the current line and records its width.
    fn push(&mut self, text: &str, style: Style) {
        if !text.is_empty() {
            self.line.push(Span::styled(text.to_owned(), style));
            self.used = self.used.saturating_add(str_width(text));
            self.content = true;
        }
    }

    /// Returns the wrapped lines.
    fn finish(mut self) -> Vec<Line<'static>> {
        if self.content || self.out.is_empty() {
            self.out.push(Line::from(self.line));
        }
        self.out
    }
}

#[cfg(test)]
mod tests {
    use ratatui::style::Style;

    use super::*;

    fn plain(text: &str) -> Vec<Span<'static>> {
        vec![Span::styled(text.to_owned(), Style::new())]
    }

    fn texts(lines: &[Line<'static>]) -> Vec<String> {
        lines.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn a_line_within_the_budget_stays_one_line() {
        let lines = wrap(&plain("hello world"), 20, "", "", Style::new());
        assert_eq!(texts(&lines), ["hello world"]);
    }

    #[test]
    fn words_wrap_at_the_budget() {
        let lines = wrap(&plain("hello world"), 5, "", "", Style::new());
        assert_eq!(texts(&lines), ["hello", "world"]);
    }

    #[test]
    fn a_space_at_a_break_is_dropped() {
        let lines = wrap(&plain("aaa bbb"), 3, "", "", Style::new());
        assert_eq!(texts(&lines), ["aaa", "bbb"]);
    }

    #[test]
    fn a_word_wider_than_a_line_is_split() {
        let lines = wrap(&plain("abcdefgh"), 3, "", "", Style::new());
        assert_eq!(texts(&lines), ["abc", "def", "gh"]);
    }

    #[test]
    fn a_prefix_is_repeated_on_continuations() {
        let lines = wrap(&plain("one two three"), 8, "• ", "  ", Style::new());
        assert_eq!(texts(&lines), ["• one", "  two", "  three"]);
    }

    #[test]
    fn an_embedded_newline_is_a_hard_break() {
        let lines = wrap(&plain("one\ntwo"), 20, "", "", Style::new());
        assert_eq!(texts(&lines), ["one", "two"]);
    }

    #[test]
    fn wide_characters_are_measured_in_columns() {
        // Three two-column glyphs are six columns, so only two fit in five.
        let lines = wrap(&plain("日本語"), 5, "", "", Style::new());
        assert_eq!(texts(&lines), ["日本", "語"]);
    }

    #[test]
    fn hard_wrap_does_not_reflow_a_code_line() {
        let lines = hard_wrap("a b c d", 4, "│ ", Style::new(), Style::new());
        assert_eq!(texts(&lines), ["│ a ", "│ b ", "│ c ", "│ d"]);
    }
}
