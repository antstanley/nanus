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
