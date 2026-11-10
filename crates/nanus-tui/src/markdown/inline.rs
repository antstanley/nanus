//! Inline markdown: emphasis, inline code, and links.
//!
//! A hand-written scanner rather than a grammar, because the inline subset a coding
//! assistant emits is four constructs wide. The rule throughout is *fall back to the
//! literal text*: a `*` with no partner, an unclosed backtick, a `[` with no `]` —
//! each is drawn as itself. That is what makes the parser safe to run on a half-streamed
//! message, where every construct is temporarily unterminated.

use ratatui::style::{Modifier, Style};
use ratatui::text::Span;

use super::theme::MarkdownTheme;

/// Splits `text` into styled spans using the theme's body style.
pub(crate) fn spans(text: &str, theme: &MarkdownTheme) -> Vec<Span<'static>> {
    spans_with(text, theme.text, theme)
}

/// Splits `text` into spans, styling unstyled runs with `base`.
pub(crate) fn spans_with(text: &str, base: Style, theme: &MarkdownTheme) -> Vec<Span<'static>> {
    let chars: Vec<char> = text.chars().collect();
    let mut scanner = Scanner::new(&chars);
    scan(&mut scanner, base, theme)
}

/// Walks the characters once, turning each construct that closes into a span.
fn scan(scanner: &mut Scanner<'_>, base: Style, theme: &MarkdownTheme) -> Vec<Span<'static>> {
    let chars = scanner.chars;
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut literal = String::new();
    let mut index = 0_usize;
    while index < chars.len() {
        let character = chars[index];
        // A backslash before a marker is that marker, literally.
        if let Some(next) = escaped(chars, index) {
            literal.push(next);
            index = index.saturating_add(2);
            continue;
        }
        if character == '`'
            && let Some(end) = scanner.find(index.saturating_add(1), '`', 1)
        {
            flush(&mut out, &mut literal, base);
            let code: String = chars[index.saturating_add(1)..end].iter().collect();
            out.push(Span::styled(code.trim().to_owned(), theme.inline_code));
            index = end.saturating_add(1);
            continue;
        }
        if character == '['
            && let Some((label, url, next)) = link(scanner, index)
        {
            flush(&mut out, &mut literal, base);
            out.push(Span::styled(label, theme.link));
            if !url.is_empty() {
                out.push(Span::styled(format!(" ({url})"), theme.aside));
            }
            index = next;
            continue;
        }
        if let Some((style, content, next)) = emphasis(scanner, index, base) {
            flush(&mut out, &mut literal, base);
            out.push(Span::styled(content, style));
            index = next;
            continue;
        }
        literal.push(character);
        index = index.saturating_add(1);
    }
    flush(&mut out, &mut literal, base);
    out
}

/// Returns the character a backslash at `index` escapes, if it escapes one.
fn escaped(chars: &[char], index: usize) -> Option<char> {
    if chars.get(index) != Some(&'\\') {
        return None;
    }
    let next = *chars.get(index.saturating_add(1))?;
    matches!(next, '*' | '_' | '`' | '[' | ']' | '(' | ')' | '\\').then_some(next)
}

/// Matches `***x***`, `**x**` / `__x__`, or `*x*` / `_x_` at `index`.
fn emphasis(
    scanner: &mut Scanner<'_>,
    index: usize,
    base: Style,
) -> Option<(Style, String, usize)> {
    let chars = scanner.chars;
    let marker = *chars.get(index)?;
    if marker != '*' && marker != '_' {
        return None;
    }
    if !left_flanking(chars, index) {
        return None;
    }
    let open = scanner.run_end(index, marker).saturating_sub(index);
    // Longest first: `***` must win over `**`, which must win over `*`.
    for (count, modifier) in [
        (3_usize, Modifier::BOLD | Modifier::ITALIC),
        (2_usize, Modifier::BOLD),
        (1_usize, Modifier::ITALIC),
    ] {
        if open < count {
            continue;
        }
        let start = index.saturating_add(count);
        if let Some(end) = scanner.find(start, marker, count)
            && end > start
        {
            let content: String = chars[start..end].iter().collect();
            if !content.trim().is_empty() {
                let next = end.saturating_add(count);
                return Some((base.add_modifier(modifier), content, next));
            }
        }
    }
    None
}

/// Matches `[label](destination)` at `index`.
fn link(scanner: &mut Scanner<'_>, index: usize) -> Option<(String, String, usize)> {
    let chars = scanner.chars;
    if chars.get(index) != Some(&'[') {
        return None;
    }
    let close = scanner.find(index.saturating_add(1), ']', 1)?;
    // Asked before the label is copied out: a bracket with no destination after it is the
    // common case, and copying a label that is then thrown away cost its length every time.
    if chars.get(close.saturating_add(1)) != Some(&'(') {
        return None;
    }
    let end = scanner.find(close.saturating_add(2), ')', 1)?;
    let label: String = chars[index.saturating_add(1)..close].iter().collect();
    let url: String = chars[close.saturating_add(2)..end].iter().collect();
    Some((label, url.trim().to_owned(), end.saturating_add(1)))
}

/// How many kinds of closer a search can be for: five markers, each as a run of one, two, or
/// three.
const KINDS: usize = 15;

/// The characters of one block of text, and what the searches through them have already found.
///
/// Every construct is opened by looking for its closer further on, and a closer that is not
/// there is looked for to the end of the text. Done afresh for each opener, a paragraph of
/// `[` with no `]` cost the square of its length — a model's answer, drawn again for every
/// token of it. The walk only moves forward, so a search that starts later than one already
/// made can reuse its answer: what that search passed over is still not a closer, and what it
/// found is still the first. Each kind of closer is therefore scanned for once across the
/// text, not once per opener.
struct Scanner<'a> {
    chars: &'a [char],
    /// For each kind of closer, where the last search for it began and what it found.
    found: [Option<(usize, Option<usize>)>; KINDS],
    /// The last run of a marker that was measured: the marker, where the measuring began, and
    /// the index one past the run. A run of a thousand asterisks is measured once, not once
    /// from each asterisk in it.
    run: Option<(char, usize, usize)>,
    /// How many characters the searches have looked at, which a test holds to the text's
    /// length.
    looked: usize,
}

impl<'a> Scanner<'a> {
    const fn new(chars: &'a [char]) -> Self {
        Self {
            chars,
            found: [None; KINDS],
            run: None,
            looked: 0,
        }
    }

    /// The index one past the run of `marker` that `index` is in.
    fn run_end(&mut self, index: usize, marker: char) -> usize {
        if let Some((measured, from, end)) = self.run
            && measured == marker
            && from <= index
            && index < end
        {
            return end;
        }
        let mut end = index;
        while self.chars.get(end) == Some(&marker) {
            end = end.saturating_add(1);
            self.looked = self.looked.saturating_add(1);
        }
        self.run = Some((marker, index, end));
        end
    }

    /// The position of a run of exactly/at least `count` `marker`s at or after `start`.
    ///
    /// Exactly `count` when it is one — a lone backtick closes a code span and a pair does not
    /// — and at least `count` otherwise. A search that begins inside a run judges what is left
    /// of the run, as it always has.
    fn find(&mut self, start: usize, marker: char, count: usize) -> Option<usize> {
        let from = if self.chars.get(start) == Some(&marker) {
            let end = self.run_end(start, marker);
            if closes(end.saturating_sub(start), count) {
                return Some(start);
            }
            end
        } else {
            start
        };
        // `from` is now outside any run of the marker, so every run at or after it is judged
        // whole — which is what makes one search's answer another's.
        let slot = slot_of(marker, count);
        if let Some((searched, result)) = slot.and_then(|slot| self.found.get(slot).copied()?)
            && searched <= from
            && result.is_none_or(|at| at >= from)
        {
            return result;
        }
        let result = self.scan_from(from, marker, count);
        if let Some(found) = slot.and_then(|slot| self.found.get_mut(slot)) {
            *found = Some((from, result));
        }
        result
    }

    /// Looks through the text from `from`, a position outside any run of `marker`.
    fn scan_from(&mut self, from: usize, marker: char, count: usize) -> Option<usize> {
        let mut index = from;
        while index < self.chars.len() {
            if self.chars.get(index) == Some(&marker) {
                let end = self.run_end(index, marker);
                if closes(end.saturating_sub(index), count) {
                    return Some(index);
                }
                index = end;
            } else {
                index = index.saturating_add(1);
                self.looked = self.looked.saturating_add(1);
            }
        }
        None
    }
}

/// Whether a run of `run` markers closes a construct opened with `count` of them.
const fn closes(run: usize, count: usize) -> bool {
    run >= count && (count > 1 || run == count)
}

/// Which remembered search a closer belongs to, or `None` for one that is not remembered.
const fn slot_of(marker: char, count: usize) -> Option<usize> {
    let base = match marker {
        '`' => 0_usize,
        ']' => 3,
        ')' => 6,
        '*' => 9,
        '_' => 12,
        _ => return None,
    };
    if count == 0 || count > 3 {
        return None;
    }
    Some(base.saturating_add(count).saturating_sub(1))
}

/// Whether a delimiter at `index` can open emphasis.
///
/// `_` additionally has to start at a word boundary so that `snake_case` and
/// `a_b_c` are not mangled into italics, which is the one place `CommonMark`'s rules
/// are worth the effort here.
fn left_flanking(chars: &[char], index: usize) -> bool {
    if chars.get(index) == Some(&'_') {
        return index == 0
            || chars
                .get(index.saturating_sub(1))
                .is_none_or(|previous| !previous.is_alphanumeric());
    }
    true
}

/// Pushes the accumulated literal text, if any.
fn flush(out: &mut Vec<Span<'static>>, literal: &mut String, style: Style) {
    if !literal.is_empty() {
        out.push(Span::styled(std::mem::take(literal), style));
    }
}

/// Strips control characters and expands tabs.
///
/// A model's answer, or a file it read, can contain an escape sequence: a raw `ESC`
/// that reached a terminal could move the cursor, clear the screen, or worse. The
/// renderer's job is to draw text, so every control character except a newline is
/// removed here — at the single point where untrusted text enters the markdown path
/// — and a tab becomes four spaces so that the wrap arithmetic agrees with the drawing.
pub(crate) fn sanitize(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    for character in source.chars() {
        if character == '\t' {
            out.push_str("    ");
        } else if character == '\n' || !character.is_control() {
            out.push(character);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::Theme;

    /// The search as it was before it remembered anything: one scan to the end per call.
    fn find_afresh(chars: &[char], start: usize, marker: char, count: usize) -> Option<usize> {
        let mut index = start;
        while index < chars.len() {
            if chars[index] == marker {
                let mut run = 0_usize;
                while chars.get(index.saturating_add(run)) == Some(&marker) {
                    run = run.saturating_add(1);
                }
                if closes(run, count) {
                    return Some(index);
                }
                index = index.saturating_add(run);
            } else {
                index = index.saturating_add(1);
            }
        }
        None
    }

    /// A small deterministic generator, so the comparison needs no dependency.
    struct Dice(u64);

    impl Dice {
        fn roll(&mut self, sides: usize) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            usize::try_from(self.0)
                .unwrap_or(0)
                .checked_rem(sides)
                .unwrap_or(0)
        }
    }

    fn plain(text: &str) -> String {
        let theme = MarkdownTheme::from_view(&Theme::default());
        spans(text, &theme)
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    /// A remembered search answers what a fresh one would, wherever it starts — including in
    /// the middle of a run, where what is left of the run is what is judged.
    #[test]
    fn a_remembered_search_finds_what_a_fresh_one_does() {
        let alphabet: Vec<char> = "**_``[]()a a".chars().collect();
        let mut dice = Dice(0x2545_F491_4F6C_DD1D);
        for _ in 0..300 {
            let chars: Vec<char> = (0..dice.roll(60))
                .map(|_| alphabet[dice.roll(alphabet.len())])
                .collect();
            let mut scanner = Scanner::new(&chars);
            let mut start = 0_usize;
            // Starts only move forward, as the walk's do, and every kind is asked for.
            while start <= chars.len() {
                for marker in ['`', ']', ')', '*', '_'] {
                    for count in 1..=3 {
                        assert_eq!(
                            scanner.find(start, marker, count),
                            find_afresh(&chars, start, marker, count),
                            "{chars:?} from {start} for {count} of {marker:?}"
                        );
                    }
                }
                start = start.saturating_add(1).saturating_add(dice.roll(3));
            }
        }
    }

    /// Openers with nothing to close them are looked through once, not once each: the cost of
    /// a paragraph is its length, where it was the square of it.
    #[test]
    fn unclosed_openers_cost_the_length_of_the_text_and_not_its_square() {
        let theme = MarkdownTheme::from_view(&Theme::default());
        let shapes = [
            "[".repeat(4_000),
            "** ".repeat(1_500),
            "*".repeat(4_000),
            format!("{}]", "[x ".repeat(1_500)),
            "_a ".repeat(1_500),
        ];
        for text in shapes {
            let chars: Vec<char> = text.chars().collect();
            let mut scanner = Scanner::new(&chars);
            let drawn = scan(&mut scanner, theme.text, &theme);
            assert!(
                scanner.looked <= chars.len().saturating_mul(16),
                "{} characters looked at for {} of text",
                scanner.looked,
                chars.len()
            );
            assert!(!drawn.is_empty());
        }
    }

    /// The constructs still close when they close, and are still themselves when they do not.
    #[test]
    fn constructs_close_or_stay_literal_as_before() {
        assert_eq!(plain("a [label](url) b"), "a label (url) b");
        assert_eq!(plain("[[[ no closer"), "[[[ no closer");
        assert_eq!(plain("[x] not a link"), "[x] not a link");
        assert_eq!(
            plain("**bold** and *it* and `code`"),
            "bold and it and code"
        );
        assert_eq!(plain("** ** ** stars"), "** ** ** stars");
        assert_eq!(plain("a ` lone tick"), "a ` lone tick");
    }
}
