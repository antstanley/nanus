//! Blocks to styled lines.
//!
//! Every function here returns lines no wider than the width it was given, because the
//! transcript counts rows from these lines before drawing them. A renderer that let a
//! long table through would make the scroll bound wrong by exactly the columns that
//! overflowed.
//!
//! Inline emphasis, links, and code are parsed here, per block, from the raw text the
//! parser kept — so a marker that is still being streamed shows as itself until it
//! closes, and nothing has to be un-parsed.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::inline;
use super::text::{char_width, str_width};
use super::theme::MarkdownTheme;
use super::types::{Block, Marker};
use super::wrap::{hard_wrap, wrap};

/// The widest a table column may grow before it is treated as a paragraph of its own.
const MAX_COLUMN: usize = 48;

/// The narrowest a table column may be squeezed before the table is drawn flat instead.
///
/// A column one or two cells wide is a grid of ellipses: it shows that data exists without
/// showing any of it. Below this the flat form, which wraps whole cell text, says more.
const MIN_COLUMN: usize = 4;

/// Renders blocks to lines at `width` columns.
pub(crate) fn blocks(
    blocks: &[Block],
    width: usize,
    mermaid: bool,
    theme: &MarkdownTheme,
) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    for block in blocks {
        match block {
            Block::Blank => push_blank(&mut out),
            Block::Heading { level, text } => {
                if !out.is_empty() {
                    push_blank(&mut out);
                }
                out.extend(heading(*level, text, width, theme));
            }
            Block::Paragraph(text) => out.extend(paragraph(text, width, theme)),
            Block::Code { lang, code } => {
                if let Some(diagram) = mermaid_diagram(lang, code, mermaid, width, theme) {
                    out.extend(diagram);
                } else {
                    out.extend(code_block(lang, code, width, theme));
                }
            }
            Block::Item {
                marker,
                indent,
                text,
            } => out.extend(item(*marker, *indent, text, width, theme)),
            Block::Quote { lines } => out.extend(quote(lines, width, theme)),
            Block::Rule => out.push(rule(width, theme)),
            Block::Table { headers, rows } => out.extend(table(headers, rows, width, theme)),
            Block::Image { alt, path } => out.extend(image(alt, path, width, theme)),
        }
    }
    while out.last().is_some_and(|line| line.width() == 0) {
        out.pop();
    }
    if out.is_empty() {
        out.push(Line::from(""));
    }
    out
}

/// Draws a `mermaid` fence as a diagram, or returns `None` to fall back to code.
fn mermaid_diagram(
    lang: &str,
    code: &str,
    enabled: bool,
    width: usize,
    theme: &MarkdownTheme,
) -> Option<Vec<Line<'static>>> {
    if !enabled || !lang.eq_ignore_ascii_case("mermaid") {
        return None;
    }
    super::mermaid::render(code, width, theme)
}

/// Adds a blank separator line, collapsing runs of them.
fn push_blank(out: &mut Vec<Line<'static>>) {
    if out.last().is_none_or(|line| line.width() > 0) {
        out.push(Line::from(""));
    }
}

/// Renders a heading, with its level deciding the emphasis.
fn heading(level: u8, text: &str, width: usize, theme: &MarkdownTheme) -> Vec<Line<'static>> {
    let style = match level {
        1 => theme.heading,
        2 => theme.subheading,
        _ => theme.subheading.add_modifier(Modifier::ITALIC),
    };
    let spans = inline::spans_with(text, style, theme);
    wrap(&spans, width, "", "", style)
}

/// Renders a paragraph, wrapping at word boundaries.
fn paragraph(text: &str, width: usize, theme: &MarkdownTheme) -> Vec<Line<'static>> {
    let spans = inline::spans(text, theme);
    wrap(&spans, width, "", "", theme.text)
}

/// Renders a fenced code block, preserving line breaks and cutting long lines.
fn code_block(lang: &str, code: &str, width: usize, theme: &MarkdownTheme) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    if !lang.is_empty() {
        out.push(Line::from(Span::styled(format!("── {lang}"), theme.aside)));
    }
    out.extend(hard_wrap(code, width, "│ ", theme.border, theme.code));
    out
}

/// Renders one list item, continuing wrapped lines under the text.
fn item(
    marker: Marker,
    indent: usize,
    text: &str,
    width: usize,
    theme: &MarkdownTheme,
) -> Vec<Line<'static>> {
    // Deep nesting past this is almost always a parser artefact rather than intent, and
    // an unbounded prefix would eat the whole line.
    let depth = indent.min(4);
    let pad = "  ".repeat(depth);
    let bullet = match marker {
        Marker::Bullet => "• ".to_owned(),
        Marker::Ordered(number) => format!("{number}. "),
        Marker::Task(true) => "[x] ".to_owned(),
        Marker::Task(false) => "[ ] ".to_owned(),
    };
    let prefix = format!("{pad}{bullet}");
    let continuation = " ".repeat(str_width(&prefix));
    let spans = inline::spans_with(text, theme.text, theme);
    wrap(&spans, width, &prefix, &continuation, theme.list_marker)
}

/// Renders a blockquote, one bar per nesting level.
fn quote(lines: &[(usize, String)], width: usize, theme: &MarkdownTheme) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    for (depth, text) in lines {
        let level = (*depth).clamp(1, 8);
        let prefix = "│ ".repeat(level);
        let continuation = "  ".repeat(level);
        let spans = inline::spans_with(text, theme.quote, theme);
        out.extend(wrap(&spans, width, &prefix, &continuation, theme.quote));
    }
    out
}

/// Renders a horizontal rule across the full width.
fn rule(width: usize, theme: &MarkdownTheme) -> Line<'static> {
    Line::from(Span::styled("─".repeat(width.max(1)), theme.rule))
}

/// Renders a whole-line image as a labelled placeholder.
///
/// There is deliberately no image here: the view layer performs no I/O, a remote URL
/// must not be fetched on a model's say-so, and not every terminal can draw a bitmap.
/// The placeholder keeps the alt text and the path visible, which is what the reader
/// needs to decide whether the picture matters.
fn image(alt: &str, path: &str, width: usize, theme: &MarkdownTheme) -> Vec<Line<'static>> {
    let label = if alt.is_empty() { path } else { alt };
    let mut spans = vec![Span::styled(label.to_owned(), theme.text)];
    if !alt.is_empty() && !path.is_empty() {
        spans.push(Span::styled(format!(" ({path})"), theme.aside));
    }
    wrap(&spans, width, "[image] ", "        ", theme.aside)
}

/// Renders a pipe table, sizing columns to the content and the width available.
fn table(
    headers: &[String],
    rows: &[Vec<String>],
    width: usize,
    theme: &MarkdownTheme,
) -> Vec<Line<'static>> {
    let columns = headers.len();
    if columns == 0 {
        return Vec::new();
    }
    let gaps = columns.saturating_sub(1).saturating_mul(3);
    // Too narrow for even one column per cell plus a separator: fall back to a plain
    // wrapped rendering rather than drawing a table that overflows the terminal.
    if width < gaps.saturating_add(columns) {
        return table_flat(headers, rows, width, theme);
    }
    let available = width.saturating_sub(gaps);
    let natural: Vec<usize> = (0..columns)
        .map(|column| {
            let header = headers.get(column).map_or(0, |cell| str_width(cell));
            rows.iter()
                .map(|row| row.get(column).map_or(0, |cell| str_width(cell)))
                .fold(header, usize::max)
                .min(MAX_COLUMN)
        })
        .collect();
    let widths = column_widths(&natural, available);
    // A column cut below the minimum is not worth a grid: at that point the flat form shows
    // more of every cell than a row of ellipses does. This is a test on the *allocated*
    // widths rather than on the terminal width alone, because columns that are naturally
    // narrow are fine however tight the terminal — only being cut is not.
    let cramped = widths
        .iter()
        .zip(&natural)
        .any(|(&width, &natural)| width < natural.min(MIN_COLUMN));
    if cramped {
        return table_flat(headers, rows, width, theme);
    }
    let mut out = Vec::new();
    out.push(table_row(headers, &widths, theme.table_header, theme));
    let separator: usize = widths.iter().copied().fold(gaps, usize::saturating_add);
    out.push(Line::from(Span::styled(
        "─".repeat(separator),
        theme.border,
    )));
    for row in rows {
        out.push(table_row(row, &widths, theme.text, theme));
    }
    out
}

/// Renders a table as wrapped lines when there is no room for readable columns.
fn table_flat(
    headers: &[String],
    rows: &[Vec<String>],
    width: usize,
    theme: &MarkdownTheme,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let header = headers.join(" | ");
    let spans = inline::spans_with(&header, theme.table_header, theme);
    out.extend(wrap(&spans, width, "", "", theme.table_header));
    for row in rows {
        out.extend(paragraph(&row.join(" | "), width, theme));
    }
    out
}

/// Scales natural column widths down to the space available.
fn column_widths(natural: &[usize], available: usize) -> Vec<usize> {
    let total = natural.iter().copied().fold(0_usize, usize::saturating_add);
    let mut widths: Vec<usize> = natural
        .iter()
        .map(|&value| {
            if total <= available {
                value
            } else {
                value
                    .saturating_mul(available)
                    .checked_div(total)
                    .unwrap_or(0)
            }
            .max(1)
        })
        .collect();
    // Rounding can leave the widths a column or two over once each is at least one;
    // shave the widest column until the row fits.
    while widths.iter().copied().fold(0_usize, usize::saturating_add) > available {
        let Some((widest, value)) = widths.iter().enumerate().max_by_key(|(_, value)| **value)
        else {
            break;
        };
        if *value <= 1 {
            break;
        }
        widths[widest] = value.saturating_sub(1);
    }
    widths
}

/// Renders one table row, padding or truncating each cell to its column.
fn table_row(
    cells: &[String],
    widths: &[usize],
    base: Style,
    theme: &MarkdownTheme,
) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (column, &cell_width) in widths.iter().enumerate() {
        if column > 0 {
            spans.push(Span::styled(" │ ", theme.border));
        }
        let text = cells.get(column).map_or("", String::as_str);
        spans.extend(cell_spans(text, cell_width, base, theme));
    }
    Line::from(spans)
}

/// Renders a table cell's inline styling, padded to its column width.
///
/// The padding is measured from the *rendered* spans rather than the source text.
/// Inline markup is syntax: `**bold**` and `` `code` `` are wider as written than as
/// drawn, so padding by the source width leaves the cell short and shifts every later
/// column left — a handful of markup characters is a handful of columns of misalignment.
fn cell_spans(text: &str, width: usize, base: Style, theme: &MarkdownTheme) -> Vec<Span<'static>> {
    let mut spans = inline::spans_with(text, base, theme);
    let rendered = spans_width(&spans);
    if rendered > width {
        return fit_spans(spans, width, base);
    }
    let padding = width.saturating_sub(rendered);
    if padding > 0 {
        spans.push(Span::styled(" ".repeat(padding), base));
    }
    spans
}

/// The display width of a run of styled spans.
fn spans_width(spans: &[Span<'static>]) -> usize {
    spans.iter().fold(0_usize, |total, span| {
        total.saturating_add(str_width(span.content.as_ref()))
    })
}

/// Truncates styled spans to `width` columns, ending with an ellipsis when content was cut.
///
/// Truncation runs on the parsed spans, not the source, so a cut cell never shows a
/// half-consumed marker like `**` or an unmatched backtick.
fn fit_spans(spans: Vec<Span<'static>>, width: usize, base: Style) -> Vec<Span<'static>> {
    if width == 0 {
        return Vec::new();
    }
    let room = width.saturating_sub(1);
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut used = 0_usize;
    // Whether a character did not fit and ended the walk. The partial span accumulated so
    // far is pushed *before* the outer loop stops, which a labelled `break` would skip —
    // and skipping it would throw away the whole truncated cell for the sake of the dot.
    let mut cut = false;
    for span in spans {
        let style = span.style;
        let mut kept = String::new();
        for character in span.content.chars() {
            let current = char_width(character);
            if used.saturating_add(current) > room {
                cut = true;
                break;
            }
            kept.push(character);
            used = used.saturating_add(current);
        }
        if !kept.is_empty() {
            out.push(Span::styled(kept, style));
        }
        if cut {
            break;
        }
    }
    out.push(Span::styled("…", base));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::Theme;

    fn render(source: &str, width: usize) -> Vec<String> {
        let theme = MarkdownTheme::from_view(&Theme::default());
        blocks(&super::super::parse::parse(source), width, false, &theme)
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    #[test]
    fn a_paragraph_is_rendered_without_its_markers() {
        assert_eq!(render("hello **bold** world", 40), vec!["hello bold world"]);
    }

    #[test]
    fn a_heading_is_rendered_without_its_hashes() {
        assert_eq!(render("# Title", 40), vec!["Title"]);
    }

    #[test]
    fn emphasis_markers_that_never_close_are_literal() {
        // The streaming case: the closing `**` has not arrived.
        assert_eq!(render("a **b", 40), vec!["a **b"]);
    }

    #[test]
    fn a_link_keeps_its_label_and_shows_its_destination() {
        assert_eq!(
            render("see [docs](https://x.test)", 40),
            vec!["see docs (https://x.test)"]
        );
    }

    #[test]
    fn a_code_block_keeps_its_line_breaks() {
        let out = render("```rust\nfn main() {}\nlet x = 1;\n```", 40);
        assert_eq!(out[0], "── rust");
        assert_eq!(out[1], "│ fn main() {}");
        assert_eq!(out[2], "│ let x = 1;");
    }

    #[test]
    fn a_long_code_line_is_cut_at_the_budget() {
        let out = render("```\nabcdefgh\n```", 5);
        assert_eq!(out, vec!["│ abc", "│ def", "│ gh"]);
    }

    #[test]
    fn a_table_fits_the_width_it_is_given() {
        let out = render("| a | b |\n| - | - |\n| one | two |", 20);
        let widest = out.iter().map(|line| str_width(line)).max().unwrap_or(0);
        assert!(widest <= 20, "{out:?}");
        assert!(out[0].starts_with('a'), "{out:?}");
    }

    #[test]
    fn a_table_too_narrow_for_its_cells_still_fits() {
        let out = render("| alpha | beta |\n| - | - |\n| gamma | delta |", 9);
        let widest = out.iter().map(|line| str_width(line)).max().unwrap_or(0);
        assert!(widest <= 9, "{out:?}");
    }

    /// Columns squeezed to a cell or two are a grid of ellipses, which says nothing. Below
    /// the readable minimum the table is drawn flat, where the whole cell text wraps.
    #[test]
    fn a_table_too_narrow_for_readable_columns_is_drawn_flat() {
        let out = render(
            "| Diagram kind | Syntax opener | Verified in the live capture |\n\
             | --- | --- | --- |\n\
             | Flowchart | graph TD | yes - correct when acyclic |",
            12,
        );
        assert!(
            out.iter().all(|line| !line.contains('│')),
            "a grid of ellipses is not a table: {out:?}"
        );
        let joined = out.join("\n");
        assert!(joined.contains("Diagram"), "{out:?}");
        assert!(joined.contains("Flowchart"), "{out:?}");
        let widest = out.iter().map(|line| str_width(line)).max().unwrap_or(0);
        assert!(widest <= 12, "{out:?}");
    }

    /// Naturally narrow columns are not "cramped": a small table of short cells stays a
    /// table however tight the terminal, because no cell is being cut.
    #[test]
    fn narrow_columns_that_need_no_cutting_stay_a_table() {
        let out = render("| a | b | c |\n| - | - | - |\n| 1 | 22 | 3 |", 20);
        assert!(out.iter().any(|line| line.contains('│')), "{out:?}");
    }

    /// A cell's padding is measured from what is *drawn*, so inline markup a cell keeps
    /// as syntax does not pull the columns to its right out of line.
    #[test]
    fn inline_markup_in_a_cell_keeps_the_columns_aligned() {
        let out = render(
            "| kind | syntax | note |\n\
             | --- | --- | --- |\n\
             | flow | `graph TD` | no markup here |\n\
             | plain | words | **bold** tail |",
            60,
        );
        let bars = |line: &str| {
            line.char_indices()
                .filter(|(_, character)| *character == '│')
                .map(|(index, _)| index)
                .collect::<Vec<_>>()
        };
        let header = bars(&out[0]);
        assert_eq!(header.len(), 2, "{out:?}");
        for row in &out[2..] {
            assert_eq!(bars(row), header, "row {row:?} is out of line in {out:?}");
        }
    }

    /// Truncation runs on the parsed spans, so a cut never leaves half a marker behind.
    #[test]
    fn a_truncated_cell_does_not_show_a_marker() {
        let out = render(
            "| a | b |\n| - | - |\n| x | **a bold cell that must be cut** |",
            24,
        );
        let last = out.last().cloned().unwrap_or_default();
        assert!(last.ends_with('…'), "{out:?}");
        assert!(!last.contains("**"), "{out:?}");
        assert!(str_width(&last) <= 24, "{out:?}");
    }

    #[test]
    fn a_list_item_carries_its_bullet() {
        assert_eq!(render("- one", 40), vec!["• one"]);
    }

    #[test]
    fn an_ordered_item_keeps_its_number() {
        assert_eq!(render("2. two", 40), vec!["2. two"]);
    }

    #[test]
    fn a_task_item_shows_its_box() {
        assert_eq!(render("- [x] done", 40), vec!["[x] done"]);
        assert_eq!(render("- [ ] todo", 40), vec!["[ ] todo"]);
    }

    #[test]
    fn a_nested_item_is_indented() {
        assert_eq!(render("    - inner", 40), vec!["    • inner"]);
    }

    #[test]
    fn a_quote_carries_a_bar_per_level() {
        assert_eq!(render("> one\n> > two", 40), vec!["│ one", "│ │ two"]);
    }

    #[test]
    fn an_image_is_a_placeholder_not_a_fetch() {
        let out = render("![a cat](cat.png)", 40);
        assert_eq!(out, vec!["[image] a cat (cat.png)"]);
    }

    #[test]
    fn every_rendered_line_fits_a_narrow_width() {
        let source = "# H\n\n- item with several words\n\n> quoted text that is long\n\n```\nlong code line here\n```";
        for width in 1..=40_usize {
            let theme = MarkdownTheme::from_view(&Theme::default());
            for line in blocks(&super::super::parse::parse(source), width, false, &theme) {
                assert!(
                    line.width() <= width.max(1),
                    "width {width}: {:?} is {}",
                    line.to_string(),
                    line.width()
                );
            }
        }
    }
}
