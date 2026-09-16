//! The block model the parser produces and the renderer consumes.
//!
//! Deliberately a small subset of `CommonMark`: the block types a coding assistant
//! actually emits, and nothing whose meaning a terminal cannot carry. A parse is
//! lossless in the one way that matters — anything unrecognised stays a paragraph
//! and is drawn literally, so a renderer that does not understand a construct shows
//! it rather than swallowing it.

/// How a list item is marked.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Marker {
    /// `-`, `*`, or `+`.
    Bullet,
    /// `1.`, `2.`, …, with the number the source wrote.
    Ordered(u32),
    /// `- [ ]` or `- [x]`; `true` is a checked box.
    Task(bool),
}

/// One parsed block.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Block {
    /// `#` through `######`; levels are clamped to three by the renderer.
    Heading {
        /// 1 through 6, as written.
        level: u8,
        /// The heading's inline text.
        text: String,
    },
    /// A run of prose, with embedded `\n` for the source's own line breaks.
    Paragraph(String),
    /// A fenced code block.
    Code {
        /// The info string's first word, empty when the fence named none.
        lang: String,
        /// The code, verbatim and without its fence.
        code: String,
    },
    /// One list item.
    Item {
        /// How the item is marked.
        marker: Marker,
        /// Nesting, counted from the leading indentation.
        indent: usize,
        /// The item's inline text.
        text: String,
    },
    /// A blockquote, one entry per source line.
    ///
    /// Depth is per line rather than per block because `> > nested` is a single
    /// source line and because a quote can change depth part way down.
    Quote {
        /// `(depth, text)`, depth one for a single `>`.
        lines: Vec<(usize, String)>,
    },
    /// `---`, `***`, or `___`.
    Rule,
    /// A pipe table, once its separator row has confirmed it is one.
    Table {
        /// The header cells, in order.
        headers: Vec<String>,
        /// The body cells, each row the same length as `headers`.
        rows: Vec<Vec<String>>,
    },
    /// A whole-line `![alt](path)`.
    Image {
        /// The bracketed alt text.
        alt: String,
        /// The parenthesised path or URL.
        path: String,
    },
    /// A blank source line, kept so paragraph spacing survives parsing.
    Blank,
}
