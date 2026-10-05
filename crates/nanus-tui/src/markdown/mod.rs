//! Markdown rendering for the transcript.
//!
//! The model's answers are markdown, and a terminal that shows the source shows the
//! scaffolding — `**`, backticks, `|` tables — instead of the prose. This module turns
//! that source into the same [`Line`]s every other transcript entry is drawn from, so
//! the view, the wrapping, and the scroll arithmetic need know nothing about markdown.
//!
//! ## What is deliberately not here
//!
//! No I/O and no images. An `![alt](path)` becomes a labelled placeholder: the view is a
//! pure function of the transcript and the composer, and resolving a path — let alone
//! fetching a URL a model wrote — would break that and hand a remote party a request the
//! reader did not ask for. Syntax highlighting lives in [`highlight`], which is a lexer for
//! the handful of languages a coding answer is written in and nothing more: no syntax
//! definition is read from disk, and a language the lexer does not know is drawn in the
//! code style, exactly as every fence used to be.
//!
//! ## Trust
//!
//! The source is untrusted: it is model output, and a model may have copied it from a
//! file. Control characters are stripped at the parse boundary, so an escape sequence
//! cannot reach the terminal through a code fence or a heading.

// The module is private, so `pub(crate)` and `pub` are the same reachability. The
// explicit `pub(crate)` says which surface these items are meant for if the renderer is
// ever lifted into its own crate; the crate already allows `unreachable_pub` for exactly
// this reason, and this is its clippy counterpart.
#![allow(clippy::redundant_pub_crate)]

mod highlight;
mod inline;
mod mermaid;
mod parse;
mod render;
mod text;
mod theme;
mod types;
mod wrap;

pub(crate) use theme::MarkdownTheme;

use ratatui::text::Line;

/// Renders `source` as markdown at `width` columns.
///
/// `mermaid` selects whether a `mermaid` fence is drawn as a diagram; when it is off,
/// or when the diagram cannot be parsed, the fence falls back to an ordinary code block,
/// so the source is never lost.
pub(crate) fn render(
    source: &str,
    width: u16,
    mermaid: bool,
    theme: &MarkdownTheme,
) -> Vec<Line<'static>> {
    let blocks = parse::parse(source);
    render::blocks(&blocks, usize::from(width), mermaid, theme)
}

/// Where a streaming answer can be cut, as byte offsets into `source`, in order: everything
/// before a cut is settled, and renders the same however the answer goes on.
///
/// An answer arrives a few characters at a time and is drawn after each, so rendering all of it
/// every time costs the whole answer per token. Cut, the parts between cuts are rendered once
/// ([`render_head`], [`render_middle`]) and only the part after the last is rendered again
/// ([`render_tail`]); together they are exactly [`render`] of the whole.
///
/// `cuts` holds the cuts already found for an earlier, shorter version of the same answer, and
/// is brought up to date. A cut stays a cut — the text before it is fixed, and so is every
/// complete block after it — except that the block the latest cut starts may still be growing,
/// and can grow into drawing nothing: a backtick is drawn, two are an empty code span. So the
/// text after the latest cut is parsed once, its first block is checked, and the new cuts are
/// taken from the same parse; a token costs what it added rather than the whole answer.
///
/// The block after a cut has to draw a row with something on it. A rendering of the whole drops
/// the blank rows at its end, and if what followed a cut drew nothing those would be rows the
/// part before it, rendered once and kept, cannot take back. So each candidate's first block is
/// rendered on its own, at the width and with the diagram setting the answer is drawn with.
///
/// Text holding control characters is not cut, because the parser strips them and its lines
/// would then not be the text's lines.
pub(crate) fn extend_cuts(
    source: &str,
    cuts: &mut Vec<usize>,
    width: u16,
    mermaid: bool,
    theme: &MarkdownTheme,
) {
    loop {
        let from = cuts.last().copied().unwrap_or(0);
        let Some(rest) = source.get(from..) else {
            cuts.clear();
            return;
        };
        if has_control(rest) {
            // The text before `from` was checked when its cuts were found; nothing after it is
            // cut while it holds a character the lines cannot be counted through.
            return;
        }
        let mut points = parse::cut_points(rest, from > 0).into_iter().peekable();
        if from > 0 {
            let holds = points
                .next_if(|(line, _)| *line == 0)
                .is_some_and(|(_, block)| draws_something(&block, width, mermaid, theme));
            if !holds {
                cuts.pop();
                continue;
            }
        }
        let lines: Vec<usize> = points
            .filter(|(_, block)| draws_something(block, width, mermaid, theme))
            .map(|(line, _)| line)
            .collect();
        cuts.extend(offsets_of(rest, from, &lines).filter(|cut| *cut < source.len()));
        return;
    }
}

/// The byte offsets of `lines`, line indices into `rest`, which starts at byte `from`.
///
/// The parser's lines are the text's lines, one for one: a tab widens a line without splitting
/// it, and every other control character has been ruled out by the caller.
fn offsets_of<'a>(
    rest: &'a str,
    from: usize,
    lines: &'a [usize],
) -> impl Iterator<Item = usize> + 'a {
    let mut offset = from;
    rest.split_inclusive('\n')
        .enumerate()
        .filter_map(move |(index, line)| {
            let start = offset;
            offset = offset.saturating_add(line.len());
            lines.contains(&index).then_some(start)
        })
        .filter(move |cut| *cut > from)
}

/// Whether text holds a control character other than a newline or a tab.
fn has_control(text: &str) -> bool {
    text.chars()
        .any(|character| character.is_control() && character != '\n' && character != '\t')
}

/// Whether a block, rendered on its own, draws a row with something on it.
fn draws_something(block: &types::Block, width: u16, mermaid: bool, theme: &MarkdownTheme) -> bool {
    let mut drawn = Vec::new();
    render::blocks_into(
        &mut drawn,
        core::slice::from_ref(block),
        usize::from(width),
        mermaid,
        theme,
    );
    drawn.iter().any(|row| row.width() > 0)
}

/// Renders the settled head of a streaming answer: the text up to its first cut.
///
/// Not finished the way [`render`] finishes, because it is not the end of anything: its blank
/// rows are the separation before what follows, and they stay.
pub(crate) fn render_head(
    head: &str,
    width: u16,
    mermaid: bool,
    theme: &MarkdownTheme,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    render::blocks_into(
        &mut out,
        &parse::parse(head),
        usize::from(width),
        mermaid,
        theme,
    );
    out
}

/// Renders a settled part of a streaming answer between two cuts.
///
/// Every cut follows a blank row, so the part is rendered as if it followed one — which is what
/// the seed row stands for, and what makes the spacing rules decide as they would have in a
/// rendering of the whole. Not finished, for the reason [`render_head`] is not.
pub(crate) fn render_middle(
    middle: &str,
    width: u16,
    mermaid: bool,
    theme: &MarkdownTheme,
) -> Vec<Line<'static>> {
    let mut out = vec![Line::from("")];
    let blocks = parse::parse_continued(middle);
    render::blocks_into(&mut out, &blocks, usize::from(width), mermaid, theme);
    out.remove(0);
    out
}

/// Renders the unsettled tail of a streaming answer: the text after its last cut.
///
/// Rendered as [`render_middle`] is, and then finished as the end of the answer is: without its
/// trailing blank rows. The block a cut starts draws something, so finishing takes rows from the
/// tail alone and never from the parts before it.
pub(crate) fn render_tail(
    tail: &str,
    width: u16,
    mermaid: bool,
    theme: &MarkdownTheme,
) -> Vec<Line<'static>> {
    let mut out = render_middle(tail, width, mermaid, theme);
    render::trim_trailing_blanks(&mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::Theme;

    /// Answers chosen for the places a cut could go wrong: fences that hold blank lines, a fence
    /// that opens after a blank line, quotes, tables, lists, headings after blank runs, a
    /// diagram, and frontmatter.
    const ANSWERS: [&str; 6] = [
        "# Title\n\nSome prose that wraps across the width of the terminal.\n\n- one\n- two\n\n\
         ```rust\nfn main() {\n\n    let x = 1;\n\n}\n```\n\nAfter the fence.\n",
        "Intro.\n\n> quoted\n> still quoted\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n## Next\n\ntext",
        "One.\n\n\n\nTwo after a blank run.\n\n```\nopen fence\n\nstill open",
        "```mermaid\ngraph TD\n  A --> B\n```\n\n1. first\n2. second\n\n---\n\nend",
        "+++\ntitle = 1\n+++\n\nBody.\n\nMore body.",
        "Tabs\tinside.\n\n\tindented code-ish\n\n![alt](path.png)\n\n#\n\nlast",
    ];

    fn theme() -> MarkdownTheme {
        MarkdownTheme::from_view(&Theme::default())
    }

    /// Renders `source` cut at every cut it has: head, middles, tail.
    fn cut_rendering(source: &str, width: u16) -> Option<Vec<Line<'static>>> {
        let theme = theme();
        let mut cuts = Vec::new();
        extend_cuts(source, &mut cuts, width, true, &theme);
        let (first, last) = (*cuts.first()?, *cuts.last()?);
        let mut lines = render_head(&source[..first], width, true, &theme);
        for pair in cuts.windows(2) {
            lines.extend(render_middle(
                &source[pair[0]..pair[1]],
                width,
                true,
                &theme,
            ));
        }
        lines.extend(render_tail(&source[last..], width, true, &theme));
        Some(lines)
    }

    #[test]
    fn every_prefix_of_an_answer_renders_the_same_cut_as_whole() {
        let mut cuts = 0_usize;
        for answer in ANSWERS {
            for (end, _) in answer.char_indices().chain([(answer.len(), ' ')]) {
                let prefix = &answer[..end];
                for width in [12_u16, 40, 100] {
                    if let Some(cut) = cut_rendering(prefix, width) {
                        cuts = cuts.saturating_add(1);
                        assert_eq!(
                            cut,
                            render(prefix, width, true, &theme()),
                            "prefix {prefix:?} at width {width}"
                        );
                    }
                }
            }
        }
        assert!(
            cuts > 500,
            "the answers are cut often enough to mean something: {cuts}"
        );
    }

    fn all_cuts(source: &str) -> Vec<usize> {
        let mut cuts = Vec::new();
        extend_cuts(source, &mut cuts, 80, true, &theme());
        cuts
    }

    fn cut(source: &str) -> Option<usize> {
        all_cuts(source).last().copied()
    }

    #[test]
    fn cuts_found_as_an_answer_grows_are_the_cuts_of_the_whole() {
        let theme = theme();
        for answer in ANSWERS {
            let mut known: Vec<usize> = Vec::new();
            for (end, _) in answer.char_indices().chain([(answer.len(), ' ')]) {
                let prefix = &answer[..end];
                extend_cuts(prefix, &mut known, 40, true, &theme);
                let mut fresh = Vec::new();
                extend_cuts(prefix, &mut fresh, 40, true, &theme);
                assert_eq!(known, fresh, "at {prefix:?}");
            }
        }
    }

    #[test]
    fn an_answer_with_no_settled_point_is_not_cut() {
        assert_eq!(cut("a single paragraph still arriving"), None);
        assert_eq!(
            cut("```\nopen\n\nfence"),
            None,
            "a blank line inside a fence"
        );
        assert_eq!(cut("+++\na = 1\n+++\n\nbody"), None, "frontmatter");
        assert_eq!(cut("bell\u{7}\n\nmore"), None, "a control character");
    }

    #[test]
    fn a_block_that_draws_nothing_is_not_a_place_to_cut() {
        // An empty code span draws no row, so the cut stays at the last block that does. A fence
        // that has only just opened is a place to cut: it draws its label straight away.
        assert_eq!(cut("first\n\nsecond\n\n``"), Some("first\n\n".len()));
        assert_eq!(cut("first\n\n```rust"), Some("first\n\n".len()));
        // And a cut whose block grows into drawing nothing is given up.
        let mut cuts = vec![3];
        extend_cuts("a\n\n`", &mut cuts, 80, true, &theme());
        assert_eq!(cuts, vec![3], "a backtick is drawn");
        extend_cuts("a\n\n``", &mut cuts, 80, true, &theme());
        assert!(cuts.is_empty(), "an empty code span is not");
    }

    #[test]
    fn an_answer_is_cut_after_every_blank_line() {
        let source = "first\n\nsecond\n\nthird";
        let all = all_cuts(source);
        assert_eq!(all, vec!["first\n\n".len(), "first\n\nsecond\n\n".len()]);
        let mut later = vec!["first\n\n".len()];
        extend_cuts(source, &mut later, 80, true, &theme());
        assert_eq!(later, all, "found from the latest cut, the same cuts");
    }
}
