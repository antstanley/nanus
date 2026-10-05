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

/// One unit of a span's text, borrowed from it.
enum Token<'a> {
    /// A run with no spaces in it.
    Word(&'a str),
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
        each_token(&span.content, |token| match token {
            Token::Word(word) => builder.word(word, span.style),
            Token::Space => builder.pending = true,
            Token::Newline => builder.newline(),
        });
    }
    builder.finish()
}

/// Hard-wraps styled spans at `width`, breaking mid-run and repeating `prefix`.
///
/// This is the code-block rule: a word wrap would reflow a program into something that
/// no longer compiles, so a line too long for the terminal is cut at the column budget
/// and continued on the next row, indented by `prefix` again. The spans are kept rather
/// than rebuilt, because a highlighted line arrives already cut into runs — a wrap that
/// flattened them would colour the first row of a long command and no other.
pub(crate) fn hard_wrap_spans(
    spans: &[Span<'static>],
    width: usize,
    prefix: &str,
    prefix_style: Style,
) -> Vec<Line<'static>> {
    let width = width.max(1);
    // Leave at least one column for content, so a prefix as wide as the terminal
    // cannot push a drawn line past it.
    let prefix = clamp(prefix, width.saturating_sub(1));
    let room = width.saturating_sub(str_width(&prefix)).max(1);
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut line = Run::new(&prefix, prefix_style);
    let mut used = 0_usize;
    for span in spans {
        for character in span.content.chars() {
            let current = char_width(character);
            if used.saturating_add(current) > room && used > 0 {
                out.push(line.finish());
                line = Run::new(&prefix, prefix_style);
                used = 0;
            }
            let mut buffer = [0_u8; 4];
            line.push(character.encode_utf8(&mut buffer), span.style);
            used = used.saturating_add(current);
        }
    }
    out.push(line.finish());
    out
}

/// A line being laid down: its finished spans, and the run of one style still growing.
///
/// Runs are merged as they are laid down, so a line holds one span per colour rather than one
/// per character or per word. The growing run is kept in a buffer of its own and copied out once,
/// at its exact length, when the style changes or the line ends: growing each span in place
/// reallocated it every time it doubled, and left the slack in every line the view keeps.
struct Run {
    spans: Vec<Span<'static>>,
    text: String,
    style: Option<Style>,
}

impl Run {
    /// A line that opens with `prefix`, unless the prefix is empty.
    fn new(prefix: &str, prefix_style: Style) -> Self {
        let mut spans = Vec::new();
        if !prefix.is_empty() {
            spans.push(Span::styled(prefix.to_owned(), prefix_style));
        }
        Self {
            spans,
            text: String::new(),
            style: None,
        }
    }

    /// Lays `text` down in `style`, joining the run when it is the run's style.
    fn push(&mut self, text: &str, style: Style) {
        if self.style != Some(style) {
            self.close();
            self.style = Some(style);
        }
        self.text.push_str(text);
    }

    /// Turns the growing run into a span.
    fn close(&mut self) {
        if let Some(style) = self.style.take()
            && !self.text.is_empty()
        {
            self.spans
                .push(Span::styled(self.text.as_str().to_owned(), style));
            self.text.clear();
        }
    }

    /// The finished line.
    fn finish(mut self) -> Line<'static> {
        self.close();
        Line::from(self.spans)
    }

    /// The finished line, leaving this one empty and its buffer ready for the next.
    fn take(&mut self) -> Line<'static> {
        self.close();
        Line::from(std::mem::take(&mut self.spans))
    }
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

/// Walks one span's text as tokens, keeping wide characters breakable.
///
/// The words are slices of the text rather than copies of it: the builder copies what it keeps
/// into the line it is building, and a copy made only to be handed over was an allocation for
/// every word of every answer.
fn each_token<'a>(text: &'a str, mut visit: impl FnMut(Token<'a>)) {
    let mut word_start: Option<usize> = None;
    for (index, character) in text.char_indices() {
        let wide = char_width(character) >= 2;
        if character != '\n' && character != ' ' && !wide {
            word_start.get_or_insert(index);
            continue;
        }
        if let Some(from) = word_start.take() {
            visit(Token::Word(text.get(from..index).unwrap_or_default()));
        }
        match character {
            '\n' => visit(Token::Newline),
            ' ' => visit(Token::Space),
            // A wide glyph is its own token so that a run of CJK can break between any two
            // characters, which is what a reader of it expects.
            _ => {
                let end = index.saturating_add(character.len_utf8());
                visit(Token::Word(text.get(index..end).unwrap_or_default()));
            }
        }
    }
    if let Some(from) = word_start {
        visit(Token::Word(text.get(from..).unwrap_or_default()));
    }
}

/// Accumulates wrapped lines, tracking the current row's width.
struct Builder {
    out: Vec<Line<'static>>,
    line: Run,
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
            line: Run::new("", prefix_style),
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
            self.line.close();
            self.line
                .spans
                .push(Span::styled(prefix, self.prefix_style));
        }
    }

    /// Ends the current line and starts the next on the continuation prefix.
    fn newline(&mut self) {
        let line = self.line.take();
        self.out.push(line);
        // Taken and put back rather than cloned: the prefix is read, not kept, by `open`.
        let continuation = std::mem::take(&mut self.continuation);
        self.open(&continuation);
        self.continuation = continuation;
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
    ///
    /// Text in the style of the run before it joins that run, so a line of prose is one span
    /// rather than a span for every word and another for every space between them — which was
    /// a string per word to build, to keep, and to draw. The prefix is never joined: it is the
    /// line's own furniture, and stays the first span whatever its style.
    fn push(&mut self, text: &str, style: Style) {
        if text.is_empty() {
            return;
        }
        self.line.push(text, style);
        self.used = self.used.saturating_add(str_width(text));
        self.content = true;
    }

    /// Returns the wrapped lines.
    fn finish(mut self) -> Vec<Line<'static>> {
        if self.content || self.out.is_empty() {
            self.out.push(self.line.finish());
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
    fn a_run_of_one_style_is_one_span_and_a_change_of_style_starts_another() {
        let bold = Style::new().add_modifier(ratatui::style::Modifier::BOLD);
        let spans = vec![
            Span::styled(String::from("plain words here "), Style::new()),
            Span::styled(String::from("bold"), bold),
        ];
        let lines = wrap(&spans, 40, "• ", "  ", Style::new());
        assert_eq!(texts(&lines), ["• plain words here bold"]);
        let styles: Vec<Style> = lines[0].spans.iter().map(|span| span.style).collect();
        assert_eq!(
            styles,
            [Style::new(), Style::new(), bold],
            "prefix, the run, the bold"
        );
        // A space takes the style of the word after it, so it opens the bold run.
        assert_eq!(lines[0].spans[1].content, "plain words here");
        assert_eq!(lines[0].spans[2].content, " bold");
    }

    #[test]
    fn hard_wrap_does_not_reflow_a_code_line() {
        let lines = hard_wrap_spans(&plain("a b c d"), 4, "│ ", Style::new());
        assert_eq!(texts(&lines), ["│ a ", "│ b ", "│ c ", "│ d"]);
    }

    #[test]
    fn a_wrapped_code_line_keeps_each_run_s_own_style() {
        let bold = Style::new().add_modifier(ratatui::style::Modifier::BOLD);
        let spans = vec![
            Span::styled(String::from("ab"), bold),
            Span::styled(String::from("cd"), Style::new()),
        ];
        let lines = hard_wrap_spans(&spans, 2, "", Style::new());
        assert_eq!(texts(&lines), ["ab", "cd"]);
        assert_eq!(lines[0].spans[0].style, bold, "the run keeps its style");
        assert_eq!(lines[1].spans[0].style, Style::new());
    }
}
