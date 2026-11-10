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

/// How the part of a streaming answer after a cut joins the part before it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Seam {
    /// After a blank line at the top level.
    Blank,
    /// Straight after a list item, with no row between the two.
    Flush,
    /// At a line of code inside a fence.
    Code {
        /// The byte offset of the line the fence opens on, which names its marker and its
        /// language.
        opener: usize,
        /// What the fence's earlier lines leave open for the lexer.
        carry: highlight::Carry,
    },
}

/// A place a streaming answer is cut: a byte offset into it, and how the text from there on
/// joins the text before.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Cut {
    /// The byte offset, which is the start of a line.
    pub(crate) offset: usize,
    /// How the part that starts here is drawn against the part before it.
    pub(crate) seam: Seam,
}

/// Where a streaming answer can be cut, in order: everything before a cut is settled, and
/// renders the same however the answer goes on.
///
/// An answer arrives a few characters at a time and is drawn after each, so rendering all of it
/// every time costs the whole answer per token. Cut, the parts between cuts are rendered once
/// ([`render_head`], [`render_middle`]) and only the part after the last is rendered again
/// ([`render_tail`]); together they are exactly [`render`] of the whole.
///
/// `cuts` holds the cuts already found for an earlier, shorter version of the same answer, and
/// is brought up to date. A cut stays a cut — the text before it is fixed, and so is every
/// complete block after it — except that the block the latest cut starts may still be growing,
/// and can grow into drawing nothing: a backtick is drawn, two are an empty code span, and a
/// line of code that grows into three backticks is the fence's close. So the text after the
/// latest cut is parsed once, its first block is checked, and the new cuts are taken from the
/// same parse; a token costs what it added rather than the whole answer.
///
/// The block after a cut has to draw a row with something on it. A rendering of the whole drops
/// the blank rows at its end, and if what followed a cut drew nothing those would be rows the
/// part before it, rendered once and kept, cannot take back. So each candidate's first block is
/// rendered on its own, at the width and with the diagram setting the answer is drawn with.
///
/// Text holding control characters is not cut, because the parser strips them and its lines
/// would then not be the text's lines. A carriage return is the exception: it is stripped
/// without moving a line break, and an answer written with `\r\n` line ends is still an answer
/// with blank lines in it.
pub(crate) fn extend_cuts(
    source: &str,
    cuts: &mut Vec<Cut>,
    width: u16,
    mermaid: bool,
    theme: &MarkdownTheme,
) {
    loop {
        let last = cuts.last().copied();
        let from = last.map_or(0, |cut| cut.offset);
        let Some(rest) = source.get(from..) else {
            cuts.clear();
            return;
        };
        if has_control(rest) {
            // The text before `from` was checked when its cuts were found; nothing after it is
            // cut while it holds a character the lines cannot be counted through.
            return;
        }
        let fence = last.and_then(|cut| fence_of(source, cut.seam));
        let resume = match (&fence, last) {
            (Some(open), _) => parse::Resume::InFence {
                marker: open.marker,
                lang: &open.lang,
                carry: open.carry,
            },
            (
                None,
                Some(Cut {
                    seam: Seam::Code { .. },
                    ..
                }),
            ) => {
                // A fence whose opening line no longer reads as one is not a cut to build on.
                cuts.pop();
                continue;
            }
            (None, Some(_)) => parse::Resume::Continued,
            (None, None) => parse::Resume::Message,
        };
        let mut points = parse::cut_points(rest, &resume, mermaid)
            .into_iter()
            .peekable();
        if last.is_some() {
            let holds = points
                .next_if(|point| point.line == 0)
                .is_some_and(|point| {
                    still_joins(last, &point) && draws_something(&point, width, mermaid, theme)
                });
            if !holds {
                cuts.pop();
                continue;
            }
        }
        let kept: Vec<parse::Point> = points
            .filter(|point| draws_something(point, width, mermaid, theme))
            .collect();
        let inherited = fence.map(|open| open.opener);
        cuts.extend(cuts_of(rest, from, inherited, &kept).filter(|cut| cut.offset < source.len()));
        return;
    }
}

/// Whether the block the latest cut starts is still one its seam can precede.
///
/// A cut between two list items is made while the second is an item, and a line that is an
/// item can grow into something else: `- -` is an item and `- - -` is a rule, and an item with
/// a `|` in it becomes a table's header when a separator arrives beneath it. Either is drawn
/// the same after the cut as in the whole, but neither is where a fresh search would cut, and
/// the cuts found as an answer grows are held to the cuts of the whole.
fn still_joins(last: Option<Cut>, point: &parse::Point) -> bool {
    match last.map(|cut| cut.seam) {
        Some(Seam::Flush) => matches!(point.block, Some(types::Block::Item { .. })),
        _ => true,
    }
}

/// A fence a cut lies inside: what its opening line says, and the lexer's state at the cut.
struct OpenFence {
    marker: char,
    lang: String,
    opener: usize,
    carry: highlight::Carry,
}

/// Reads the fence a code seam lies inside from the line that opened it.
fn fence_of(source: &str, seam: Seam) -> Option<OpenFence> {
    let Seam::Code { opener, carry } = seam else {
        return None;
    };
    let line = source.get(opener..)?.lines().next()?;
    let (marker, lang) = parse::fence_at(line)?;
    Some(OpenFence {
        marker,
        lang,
        opener,
        carry,
    })
}

/// The cuts at `points`, which name lines of `rest`, which starts at byte `from`.
///
/// The parser's lines are the text's lines, one for one: a tab widens a line and a carriage
/// return is dropped from one without either splitting it, and every other control character
/// has been ruled out by the caller. `inherited` is the opening line of the fence `rest` begins
/// inside, when it begins inside one.
fn cuts_of<'a>(
    rest: &'a str,
    from: usize,
    inherited: Option<usize>,
    points: &'a [parse::Point],
) -> impl Iterator<Item = Cut> + 'a {
    let mut starts: Vec<usize> = Vec::new();
    let mut offset = from;
    for line in rest.split_inclusive('\n') {
        starts.push(offset);
        offset = offset.saturating_add(line.len());
    }
    points
        .iter()
        .filter_map(move |point| {
            let offset = starts.get(point.line).copied()?;
            let seam = match point.joint {
                parse::Joint::Blank => Seam::Blank,
                parse::Joint::Flush => Seam::Flush,
                parse::Joint::Code { opener, carry } => Seam::Code {
                    opener: match opener {
                        Some(line) => starts.get(line).copied()?,
                        None => inherited?,
                    },
                    carry,
                },
            };
            Some(Cut { offset, seam })
        })
        .filter(move |cut| cut.offset > from)
}

/// Whether text holds a control character other than a newline, a tab, or a carriage return.
fn has_control(text: &str) -> bool {
    text.chars()
        .any(|character| character.is_control() && !matches!(character, '\n' | '\t' | '\r'))
}

/// Whether what a candidate cut starts, rendered on its own, draws a row with something on it.
///
/// A line of code that is not empty always does, which is the only kind a cut inside a fence
/// is offered at.
fn draws_something(point: &parse::Point, width: u16, mermaid: bool, theme: &MarkdownTheme) -> bool {
    let Some(block) = &point.block else {
        return true;
    };
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

/// Renders a settled part of a streaming answer: `source` from the cut `from` up to byte `to`.
///
/// Drawn the way its seam says it joins the part before. After a blank line it is rendered as
/// if it followed one — which is what the seed row stands for, and what makes the spacing rules
/// decide as they would have in a rendering of the whole. After a list item it is rendered
/// from nothing, because an item is drawn without looking back. Inside a fence its code is
/// drawn first, picking the lexer up where the earlier lines left it, and whatever follows the
/// fence's close after that. Not finished, for the reason [`render_head`] is not.
pub(crate) fn render_middle(
    source: &str,
    from: Cut,
    to: usize,
    width: u16,
    mermaid: bool,
    theme: &MarkdownTheme,
) -> Vec<Line<'static>> {
    let part = source.get(from.offset..to).unwrap_or_default();
    let columns = usize::from(width);
    match from.seam {
        Seam::Flush => {
            let mut out = Vec::new();
            let blocks = parse::parse_continued(part);
            render::blocks_into(&mut out, &blocks, columns, mermaid, theme);
            out
        }
        Seam::Code { carry, .. } => {
            let Some(fence) = fence_of(source, from.seam) else {
                return after_blank(part, columns, mermaid, theme);
            };
            let (code, blocks) = parse::parse_in_fence(part, fence.marker);
            let mut out = render::code_rest(&fence.lang, carry, &code, columns, theme);
            render::blocks_into(&mut out, &blocks, columns, mermaid, theme);
            out
        }
        Seam::Blank => after_blank(part, columns, mermaid, theme),
    }
}

/// Renders a part that follows a blank line.
fn after_blank(
    part: &str,
    width: usize,
    mermaid: bool,
    theme: &MarkdownTheme,
) -> Vec<Line<'static>> {
    let mut out = vec![Line::from("")];
    let blocks = parse::parse_continued(part);
    render::blocks_into(&mut out, &blocks, width, mermaid, theme);
    out.remove(0);
    out
}

/// Renders the unsettled tail of a streaming answer: `source` from its last cut on.
///
/// Rendered as [`render_middle`] is, and then finished as the end of the answer is: without its
/// trailing blank rows. The block a cut starts draws something, so finishing takes rows from the
/// tail alone and never from the parts before it.
pub(crate) fn render_tail(
    source: &str,
    from: Cut,
    width: u16,
    mermaid: bool,
    theme: &MarkdownTheme,
) -> Vec<Line<'static>> {
    let mut out = render_middle(source, from, source.len(), width, mermaid, theme);
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
    const ANSWERS: [&str; 11] = [
        "# Title\n\nSome prose that wraps across the width of the terminal.\n\n- one\n- two\n\n\
         ```rust\nfn main() {\n\n    let x = 1;\n\n}\n```\n\nAfter the fence.\n",
        "Intro.\n\n> quoted\n> still quoted\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n## Next\n\ntext",
        "One.\n\n\n\nTwo after a blank run.\n\n```\nopen fence\n\nstill open",
        "```mermaid\ngraph TD\n  A --> B\n```\n\n1. first\n2. second\n\n---\n\nend",
        "+++\ntitle = 1\n+++\n\nBody.\n\nMore body.",
        "Tabs\tinside.\n\n\tindented code-ish\n\n![alt](path.png)\n\n#\n\nlast",
        // A fence with no blank line in it, a comment the lexer carries across lines, and text
        // straight after the close.
        "Here:\n\n```rust\nfn a() {\n    /* a block\n    comment */\n    let x = 1;\n    \
         let y = \"s\";\n}\n```\nafter the fence\n\nend",
        // A list with no blank line: items, one that could become a table, a rule made of
        // dashes, numbers, and prose in between.
        "- one\n- two `c`\n- three | pipe\n---|---\n- four\n- five\n- - -\n1. first\n2. second\n\
         text after\n- more\n- most",
        // Carriage returns, and a string the lexer carries across them.
        "One.\r\n\r\nTwo.\r\n\r\n```py\r\nx = \"\"\"a\r\nb\"\"\"\r\ny = 1\r\nz = 2\r\n```\r\n\r\nend",
        // Lines of code that grow into something that looks like a close, and one that is.
        "~~~\na\n`\n``\n```\nb\n~~~\nc\n\n```\nx\ny\n```\nz",
        // A fence that ends on blank lines, and one whose code a wide terminal still wraps.
        "```sh\none\ntwo\n\n\n```\n\n```\nlong long long long long long long long long\nshort\n",
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
        let mut lines = render_head(&source[..first.offset], width, true, &theme);
        for pair in cuts.windows(2) {
            lines.extend(render_middle(
                source,
                pair[0],
                pair[1].offset,
                width,
                true,
                &theme,
            ));
        }
        lines.extend(render_tail(source, last, width, true, &theme));
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
        cuts.iter().map(|cut| cut.offset).collect()
    }

    /// A cut after a blank line, which is the kind every cut used to be.
    const fn blank_cut(offset: usize) -> Cut {
        Cut {
            offset,
            seam: Seam::Blank,
        }
    }

    fn cut(source: &str) -> Option<usize> {
        all_cuts(source).last().copied()
    }

    #[test]
    fn cuts_found_as_an_answer_grows_are_the_cuts_of_the_whole() {
        let theme = theme();
        for answer in ANSWERS {
            let mut known: Vec<Cut> = Vec::new();
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
        let mut cuts = vec![blank_cut(3)];
        extend_cuts("a\n\n`", &mut cuts, 80, true, &theme());
        assert_eq!(cuts, vec![blank_cut(3)], "a backtick is drawn");
        extend_cuts("a\n\n``", &mut cuts, 80, true, &theme());
        assert!(cuts.is_empty(), "an empty code span is not");
    }

    #[test]
    fn an_answer_is_cut_after_every_blank_line() {
        let source = "first\n\nsecond\n\nthird";
        let all = all_cuts(source);
        assert_eq!(all, vec!["first\n\n".len(), "first\n\nsecond\n\n".len()]);
        let mut later = vec![blank_cut("first\n\n".len())];
        extend_cuts(source, &mut later, 80, true, &theme());
        let later: Vec<usize> = later.iter().map(|cut| cut.offset).collect();
        assert_eq!(later, all, "found from the latest cut, the same cuts");
    }

    #[test]
    fn a_fence_is_cut_between_lines_of_code_but_not_beside_a_blank_one() {
        let source = "```rust\nlet a = 1;\nlet b = 2;\nlet c = 3;\n\nlet d = 4;\nlet e = 5;\n";
        let at = |line: &str| source.find(line).unwrap_or(usize::MAX);
        // After the opening line there has to be a line of code before the first cut, and a
        // blank line of code is a place to cut neither before nor after.
        assert_eq!(
            all_cuts(source),
            vec![at("let b"), at("let c"), at("let e")],
            "{source:?}"
        );
        // The seam names the fence, so the part is drawn as code and not as prose.
        let mut cuts = Vec::new();
        extend_cuts(source, &mut cuts, 80, true, &theme());
        assert!(
            cuts.iter()
                .all(|cut| matches!(cut.seam, Seam::Code { opener: 0, .. }))
        );
        // The other direction: a diagram is drawn from all of its source, so it is not cut —
        // unless diagrams are off, when the fence is code like any other.
        let diagram = "```mermaid\ngraph TD\n  A --> B\n  B --> C\n";
        assert_eq!(all_cuts(diagram), Vec::<usize>::new());
        let mut plain = Vec::new();
        extend_cuts(diagram, &mut plain, 80, false, &theme());
        assert_eq!(plain.len(), 2, "{plain:?}");
    }

    #[test]
    fn a_line_of_code_that_becomes_the_close_gives_its_cut_up() {
        let mut cuts = Vec::new();
        extend_cuts("```\none\ntwo\n``", &mut cuts, 80, true, &theme());
        let offsets = |cuts: &[Cut]| cuts.iter().map(|cut| cut.offset).collect::<Vec<_>>();
        assert_eq!(offsets(&cuts), vec![8, 12], "two backticks are code");
        extend_cuts("```\none\ntwo\n```", &mut cuts, 80, true, &theme());
        assert_eq!(
            offsets(&cuts),
            vec![8],
            "three are the close, which draws nothing"
        );
    }

    #[test]
    fn a_list_is_cut_between_items_but_not_after_one_that_could_head_a_table() {
        let source = "- one\n- two\n- three | pipe\n- four\ntext\n- five";
        let at = |line: &str| source.find(line).unwrap_or(usize::MAX);
        // Not before `four`, whose predecessor holds a pipe, and not before `five`, which
        // follows a paragraph rather than an item.
        assert_eq!(all_cuts(source), vec![at("- two"), at("- three")]);
        let mut cuts = Vec::new();
        extend_cuts(source, &mut cuts, 80, true, &theme());
        assert!(cuts.iter().all(|cut| cut.seam == Seam::Flush));
        // An item that grows into a rule is no longer where a list would be cut.
        let mut grown = Vec::new();
        extend_cuts("- one\n- -", &mut grown, 80, true, &theme());
        assert_eq!(grown.len(), 1, "an item, so far");
        extend_cuts("- one\n- - -", &mut grown, 80, true, &theme());
        assert!(grown.is_empty(), "a rule: {grown:?}");
    }

    #[test]
    fn a_carriage_return_is_not_a_reason_to_stop_cutting_and_a_bell_still_is() {
        assert_eq!(
            all_cuts("first\r\n\r\nsecond\r\n\r\nthird"),
            vec!["first\r\n\r\n".len(), "first\r\n\r\nsecond\r\n\r\n".len()]
        );
        assert_eq!(cut("first\r\n\r\nbell\u{7}\r\n\r\nthird"), None);
    }
}
