//! The styles the markdown renderer draws with.
//!
//! Derived from the interface's own [`Theme`](crate::view::Theme) rather than invented
//! from scratch, so the answer keeps the answer's colour and a diagram borrows the
//! interface's existing accents. Every style is composed from a role style, which means
//! `Theme::monochrome` — the theme with its colours removed for `NO_COLOR` — yields a
//! monochrome markdown theme for free, with the modifiers intact.

use ratatui::style::{Modifier, Style};

use crate::view::Theme;

/// Styles for markdown blocks and diagrams.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct MarkdownTheme {
    /// Body prose.
    pub text: Style,
    /// De-emphasised prose: code, borders, rules.
    pub muted: Style,
    /// A first- or second-level heading.
    pub heading: Style,
    /// A third-level heading.
    pub subheading: Style,
    /// Code block content.
    pub code: Style,
    /// A keyword in a code block.
    pub code_keyword: Style,
    /// A string literal in a code block.
    pub code_literal: Style,
    /// A numeric literal in a code block.
    pub code_number: Style,
    /// A comment in a code block.
    pub code_comment: Style,
    /// Inline code.
    pub inline_code: Style,
    /// A link's label.
    pub link: Style,
    /// A link's destination, or another aside.
    pub aside: Style,
    /// Box and table borders.
    pub border: Style,
    /// A table's header row.
    pub table_header: Style,
    /// A list item's bullet or number.
    pub list_marker: Style,
    /// A blockquote's bar.
    pub quote: Style,
    /// A horizontal rule.
    pub rule: Style,
    /// A diagram's node borders.
    pub diagram_border: Style,
    /// A diagram's node text.
    pub diagram_text: Style,
    /// A diagram's connecting lines.
    pub diagram_edge: Style,
    /// A diagram's arrowheads.
    pub diagram_arrow: Style,
    /// A diagram's edge or axis labels.
    pub diagram_label: Style,
    /// A diagram's title or filled bar.
    pub diagram_accent: Style,
}

impl MarkdownTheme {
    /// Builds the markdown styles from the interface's role styles.
    ///
    /// The answer is `assistant`, and everything the renderer emphasises within the
    /// answer is a modifier on top of it rather than a new colour: the interface's
    /// rule is that the answer is plain and the *roles* around it are marked, and a
    /// heading that switched colour would put a second palette inside the one thing a
    /// reader is trying to read. Diagrams are the exception and borrow the tool and
    /// notice accents, because a diagram is a figure rather than prose.
    pub(crate) fn from_view(theme: &Theme) -> Self {
        let body = theme.assistant;
        let muted = body.add_modifier(Modifier::DIM);
        Self {
            text: body,
            muted,
            heading: body.add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            subheading: body.add_modifier(Modifier::BOLD),
            code: muted,
            // Highlighting borrows the roles the interface already has rather than adding a
            // second palette: a keyword takes the tool accent, which is the one colour here
            // that means "this is a name for an action", a string takes the human's input
            // colour, a number takes the busy accent, and a comment takes the reasoning
            // style — dimmed and italic, because a comment is the one thing in a fence that
            // is *about* the code rather than part of it. Under `NO_COLOR` every one of them
            // loses its colour and keeps its modifier, so a monochrome terminal still sees
            // keywords in bold and comments in italics.
            code_keyword: theme.tool.add_modifier(Modifier::BOLD),
            code_literal: theme.user,
            code_number: theme.busy,
            code_comment: theme.reasoning,
            inline_code: body.add_modifier(Modifier::BOLD),
            link: theme.notice.add_modifier(Modifier::UNDERLINED),
            aside: muted,
            border: muted,
            table_header: body.add_modifier(Modifier::BOLD),
            list_marker: body.add_modifier(Modifier::BOLD),
            quote: muted,
            rule: muted,
            diagram_border: muted,
            diagram_text: body,
            diagram_edge: muted,
            diagram_arrow: body.add_modifier(Modifier::BOLD),
            diagram_label: theme.notice.add_modifier(Modifier::ITALIC),
            diagram_accent: theme.tool,
        }
    }
}
