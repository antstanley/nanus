//! Breaking a rendered line into the rows it is drawn on.
//!
//! The transcript is scrolled by rows and was drawn by lines. A line wider than the terminal was
//! left for the drawing library to wrap, and its height was *predicted*: by a count of characters
//! rather than of cells, and by a rule that dropped the spaces a break fell among where the
//! library keeps them. The prediction was short for a line of wide characters and for a line with
//! runs of spaces in it — an aligned listing, an indented block — and a height that is short puts
//! the rows it missed below the fold with no offset that reaches them. A line was also drawn from
//! its top or not at all, so one taller than the viewport could never show its end, and at the
//! bottom of a conversation it drew nothing.
//!
//! So a line is broken here, before it is measured, into one [`Line`] per row. Nothing is left to
//! predict: a line that fits the width is one row, every line the layout holds fits, and a row is
//! the unit the view scrolls by and draws.
//!
//! The break is the drawing library's own — ratatui's word wrap with leading whitespace kept —
//! reproduced rather than replaced, so a line is cut where it always was and the rows on screen
//! are the rows that were on screen before. It differs in one case, on purpose: the library
//! lets a character two cells wide start in the last column of a row and overhang it, and here
//! that character begins the next row. The markdown renderer wraps its own prose and never
//! reaches the slow path here; this is for text drawn as itself.

// The module is private, so `pub(crate)` and `pub` are the same reachability; the explicit
// `pub(crate)` says the surface is the view's, as the layout module's does for the same reason.
#![allow(clippy::redundant_pub_crate)]

use std::collections::VecDeque;

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr as _;

/// A zero-width space, which the drawing library treats as somewhere a line may break.
const ZWSP: &str = "\u{200b}";

/// A no-break space, which it does not.
const NBSP: &str = "\u{a0}";

/// One grapheme of a line, with the style it is drawn in and the cells it covers.
#[derive(Clone, Copy)]
struct Glyph<'a> {
    symbol: &'a str,
    style: Style,
    cells: usize,
}

impl Glyph<'_> {
    /// Whether a line may break here, by the drawing library's rule.
    fn is_space(&self) -> bool {
        self.symbol == ZWSP || (self.symbol.chars().all(char::is_whitespace) && self.symbol != NBSP)
    }
}

/// Breaks every line wider than `width` into the rows it is drawn on.
///
/// A line that fits is handed back untouched, which is nearly all of them: the cost of this for
/// an ordinary transcript is a width measurement per line.
pub(crate) fn rows(lines: Vec<Line<'static>>, width: u16) -> Vec<Line<'static>> {
    let limit = usize::from(width.max(1));
    if lines.iter().all(|line| line.width() <= limit) {
        return lines;
    }
    let mut out: Vec<Line<'static>> = Vec::with_capacity(lines.len());
    for line in lines {
        if line.width() <= limit {
            out.push(line);
        } else {
            break_line(&line, limit, &mut out);
        }
    }
    // Postcondition: every row fits, which is what makes a row and a line the same thing to
    // the scroll arithmetic.
    assert!(
        out.iter().all(|line| line.width() <= limit),
        "a broken line fits the width"
    );
    out
}

/// The state of one line being broken: the row being built, and the word and the whitespace
/// before it that have not yet been given a row.
struct Breaker<'a> {
    limit: usize,
    row: Vec<Glyph<'a>>,
    row_cells: usize,
    word: Vec<Glyph<'a>>,
    word_cells: usize,
    space: VecDeque<Glyph<'a>>,
    space_cells: usize,
    /// Whether the glyph before this one was part of a word.
    in_word: bool,
}

impl<'a> Breaker<'a> {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            row: Vec::new(),
            row_cells: 0,
            word: Vec::new(),
            word_cells: 0,
            space: VecDeque::new(),
            space_cells: 0,
            in_word: false,
        }
    }

    /// Moves the pending whitespace and word onto the row being built.
    fn commit(&mut self) {
        self.row.extend(self.space.drain(..));
        self.row_cells = self.row_cells.saturating_add(self.space_cells);
        self.row.append(&mut self.word);
        self.row_cells = self.row_cells.saturating_add(self.word_cells);
        self.space_cells = 0;
        self.word_cells = 0;
    }

    /// Ends the row being built, dropping the whitespace that would have filled out its end.
    fn end_row(&mut self, out: &mut Vec<Line<'static>>) {
        let mut remaining = self.limit.saturating_sub(self.row_cells);
        out.push(line_of(&self.row));
        self.row.clear();
        self.row_cells = 0;
        while let Some(front) = self.space.front() {
            if front.cells > remaining {
                break;
            }
            self.space_cells = self.space_cells.saturating_sub(front.cells);
            remaining = remaining.saturating_sub(front.cells);
            self.space.pop_front();
        }
    }

    /// Takes one glyph, ending a row first when the glyph will not fit on it.
    fn take(&mut self, glyph: Glyph<'a>, out: &mut Vec<Line<'static>>) {
        // A glyph wider than the whole width has no row it could be drawn on.
        if glyph.cells > self.limit {
            return;
        }
        let space = glyph.is_space();
        let word_ended = self.in_word && space;
        let pending = self.word_cells.saturating_add(self.space_cells);
        let overflows_alone =
            self.row.is_empty() && pending.saturating_add(glyph.cells) > self.limit;
        if word_ended || overflows_alone {
            self.commit();
        }
        let held = self
            .row_cells
            .saturating_add(self.space_cells)
            .saturating_add(self.word_cells);
        // The library asks whether the row is already full, which lets a glyph two cells wide
        // start in a row's last column and overhang it by one. Asking whether *this glyph*
        // fits is the same question for a glyph one cell wide and the right one for a wide
        // one, and it is the single place these rows differ from the library's.
        let overhangs = glyph.cells > 0 && held.saturating_add(glyph.cells) > self.limit;
        if self.row_cells >= self.limit || overhangs {
            self.end_row(out);
            // The whitespace a break lands on is not carried to the next row.
            if space && self.space.is_empty() {
                return;
            }
        }
        if space {
            self.space_cells = self.space_cells.saturating_add(glyph.cells);
            self.space.push_back(glyph);
        } else {
            self.word_cells = self.word_cells.saturating_add(glyph.cells);
            self.word.push(glyph);
        }
        self.in_word = !space;
    }
}

/// Breaks one line into rows of at most `limit` cells, onto the end of `out`.
///
/// Word wrapping with every space kept except the ones a break lands on: a word is placed whole
/// unless it is wider than a row, in which case it is cut by cells, never inside a grapheme.
fn break_line(line: &Line<'static>, limit: usize, out: &mut Vec<Line<'static>>) {
    let before = out.len();
    let mut breaker = Breaker::new(limit);
    for grapheme in line.styled_graphemes(Style::default()) {
        let glyph = Glyph {
            symbol: grapheme.symbol,
            style: grapheme.style,
            cells: grapheme.symbol.width(),
        };
        breaker.take(glyph, out);
    }
    breaker.commit();
    if !breaker.row.is_empty() {
        out.push(line_of(&breaker.row));
    }
    if out.len() == before {
        out.push(Line::default());
    }
}

/// Builds the line a row of glyphs is drawn as, one span per run of a style.
///
/// A glyph that covers no cell is left out. It is never drawn, and left in it could sit on a
/// row's edge where the drawing library would count the row as full and break it again — the one
/// way a row built here could become two.
fn line_of(row: &[Glyph<'_>]) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut text = String::new();
    let mut style: Option<Style> = None;
    for glyph in row.iter().filter(|glyph| glyph.cells > 0) {
        if style != Some(glyph.style) {
            if let Some(style) = style.take()
                && !text.is_empty()
            {
                spans.push(Span::styled(core::mem::take(&mut text), style));
            }
            style = Some(glyph.style);
        }
        text.push_str(glyph.symbol);
    }
    if let Some(style) = style
        && !text.is_empty()
    {
        spans.push(Span::styled(text, style));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::style::Modifier;
    use ratatui::widgets::{Paragraph, Wrap};

    use super::*;

    /// The rows the drawing library draws `lines` on at `width`, as text.
    fn drawn(lines: Vec<Line<'static>>, width: u16) -> Vec<String> {
        let height = 400_u16;
        let mut terminal = Terminal::new(TestBackend::new(width, height))
            .unwrap_or_else(|error| panic!("a test terminal: {error}"));
        terminal
            .draw(|frame| {
                let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
                frame.render_widget(paragraph, frame.area());
            })
            .unwrap_or_else(|error| panic!("a test frame: {error}"));
        let buffer = terminal.backend().buffer();
        let mut rows: Vec<String> = (0..height)
            .map(|y| {
                let mut row = String::new();
                let mut skip = 0_usize;
                for x in 0..width {
                    if skip > 0 {
                        skip = skip.saturating_sub(1);
                        continue;
                    }
                    let symbol = buffer[(x, y)].symbol();
                    row.push_str(symbol);
                    skip = symbol.width().saturating_sub(1);
                }
                row.trim_end().to_owned()
            })
            .collect();
        while rows.last().is_some_and(String::is_empty) {
            rows.pop();
        }
        rows
    }

    fn text_of(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| line.to_string().trim_end().to_owned())
            .collect()
    }

    /// A small deterministic generator, so the property test needs no dependency.
    struct Dice(u64);

    impl Dice {
        fn roll(&mut self, sides: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0.checked_rem(sides).unwrap_or(0)
        }
    }

    /// A line of words drawn from `alphabets`, with runs of spaces and words longer than a row.
    fn awkward_line(dice: &mut Dice, alphabets: &[&str]) -> String {
        use unicode_segmentation::UnicodeSegmentation as _;
        let mut text = " ".repeat(usize::try_from(dice.roll(4)).unwrap_or(0));
        for _ in 0..=dice.roll(24) {
            let choice = usize::try_from(dice.roll(alphabets.len() as u64)).unwrap_or(0);
            let alphabet: Vec<&str> = alphabets[choice].graphemes(true).collect();
            let long = dice.roll(7) == 0;
            for _ in 0..=dice.roll(if long { 50 } else { 8 }) {
                let index = usize::try_from(dice.roll(alphabet.len() as u64)).unwrap_or(0);
                text.push_str(alphabet[index]);
            }
            let gap = if dice.roll(4) == 0 { dice.roll(9) } else { 0 };
            text.push_str(&" ".repeat(usize::try_from(gap).unwrap_or(0).saturating_add(1)));
        }
        text
    }

    /// Everything a row draws, with the spaces taken out: what a break must never lose.
    fn ink(rows: &[String]) -> String {
        rows.concat().split_whitespace().collect()
    }

    #[test]
    fn a_line_that_fits_is_handed_back_as_it_was() {
        let lines = vec![Line::from("short"), Line::from("")];
        assert_eq!(rows(lines.clone(), 20), lines);
        assert_eq!(rows(vec![Line::from("exactly ten")], 11).len(), 1);
    }

    #[test]
    fn a_line_too_wide_is_broken_at_words_and_keeps_its_styles() {
        let bold = Style::new().add_modifier(Modifier::BOLD);
        let line = Line::from(vec![
            Span::raw("plain words "),
            Span::styled("bold words", bold),
        ]);
        let broken = rows(vec![line], 14);
        assert_eq!(text_of(&broken), ["plain words", "bold words"]);
        assert_eq!(broken[1].spans.first().map(|span| span.style), Some(bold));
    }

    #[test]
    fn wide_characters_are_counted_by_the_cells_they_cover() {
        // Twelve ideographs are twenty-four cells: three rows of ten, where a count of
        // characters called it two.
        let broken = rows(vec![Line::from("漢".repeat(12))], 10);
        assert_eq!(text_of(&broken), ["漢漢漢漢漢", "漢漢漢漢漢", "漢漢"]);
        // And a combining mark covers no cell of its own: ten accented letters are one row.
        assert_eq!(rows(vec![Line::from("e\u{301}".repeat(10))], 10).len(), 1);
    }

    #[test]
    fn runs_of_spaces_are_kept_where_the_drawing_keeps_them() {
        let line = "name      size      modified";
        let broken = rows(vec![Line::from(line)], 12);
        assert_eq!(text_of(&broken), drawn(vec![Line::from(line)], 12));
        assert!(broken.len() > 2, "{broken:?}");
    }

    /// The property the layout rests on: where no character is wide, the rows built here are
    /// exactly the rows the drawing library would have made of the line.
    #[test]
    fn the_rows_are_the_ones_the_drawing_library_would_have_drawn() {
        let narrow = ["abcdefgh", "e\u{301}a\u{308}", "x\u{200b}y", "-/._"];
        let mut dice = Dice(0x2545_F491_4F6C_DD1D);
        for _ in 0..600 {
            let width = u16::try_from(dice.roll(40)).unwrap_or(0).saturating_add(2);
            let text = awkward_line(&mut dice, &narrow);
            let line = Line::from(text.clone());
            let broken = rows(vec![line.clone()], width);
            assert_eq!(
                drawn(broken, width),
                drawn(vec![line], width),
                "width {width}, line {text:?}"
            );
        }
    }

    /// And in every alphabet, wide characters included: each row fits, each is drawn as the one
    /// row it is, and nothing the line said is lost at a break.
    #[test]
    fn every_row_fits_and_is_drawn_as_one_row_and_nothing_is_lost() {
        let mixed = [
            "abcdefgh",
            "漢字仮名交",
            "😀🎉",
            "e\u{301}a\u{308}",
            "x\u{200b}y",
        ];
        let mut dice = Dice(0x9E37_79B9_7F4A_7C15);
        for _ in 0..600 {
            let width = u16::try_from(dice.roll(40)).unwrap_or(0).saturating_add(2);
            let text = awkward_line(&mut dice, &mixed);
            let broken = rows(vec![Line::from(text.clone())], width);
            assert!(broken.iter().all(|row| row.width() <= usize::from(width)));
            // A zero-width space is never drawn, so it is not part of what a row shows.
            let ours: Vec<String> = text_of(&broken)
                .iter()
                .map(|row| row.replace('\u{200b}', "").trim_end().to_owned())
                .collect();
            // Rows of nothing but spaces are drawn blank, and a blank tail is not compared.
            let mut shown = ours.clone();
            while shown.last().is_some_and(String::is_empty) {
                shown.pop();
            }
            assert_eq!(drawn(broken, width), shown, "width {width}, line {text:?}");
            let said: String = text.replace('\u{200b}', "").split_whitespace().collect();
            assert_eq!(ink(&ours), said, "width {width}, line {text:?}");
        }
    }

    /// The one place the rows differ from the library's: a wide character that would start in a
    /// row's last column begins the next row instead of overhanging this one.
    #[test]
    fn a_wide_character_never_overhangs_the_edge() {
        let broken = rows(vec![Line::from("漢".repeat(6))], 5);
        assert_eq!(text_of(&broken), ["漢漢", "漢漢", "漢漢"]);
    }
}
