//! One modal priority and one bounded, scrollable layout for every dialog.
//!
//! Rendering and input both ask which modal is active. Content is wrapped before measuring it,
//! while actions occupy a separate footer: a long reason cannot push a decision out of reach.

// These are internal view helpers, not a second public widget API.
#![allow(clippy::redundant_pub_crate)]

use core::ops::Range;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthStr as _;

use crate::Theme;

/// The visible modal, in descending priority when several states are pending.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Modal {
    Permissions,
    KeyEntry,
    Approval,
    Credential,
    Authorization,
    Provider,
    Context,
    /// A handoff or compaction draft, read before the human says `y` to commit it.
    Preview,
    /// The `/clear` warning, which names the conversation a clear would stop.
    Clear,
    Models,
    Effort,
    Help,
    Queue,
}

/// Fully wrapped dialog content and its fixed action footer.
pub(crate) struct Content {
    pub(crate) title: &'static str,
    pub(crate) lines: Vec<Line<'static>>,
    pub(crate) footer: Vec<Line<'static>>,
    /// The selected entry's rows; its first row must stay visible on a selection change.
    pub(crate) selected: Option<Range<usize>>,
}

impl Content {
    /// Starts an empty dialog, wrapping the footer with the same budget as the content.
    pub(crate) fn new(title: &'static str, footer: &str, width: usize, theme: &Theme) -> Self {
        Self {
            title,
            lines: Vec::new(),
            footer: wrapped(footer, width, theme.secondary),
            selected: None,
        }
    }

    /// Adds prose without parsing it as markdown; instructions and tool names are literal text.
    pub(crate) fn push(&mut self, text: &str, width: usize, style: Style) {
        self.lines.extend(wrapped(text, width, style));
    }

    /// Separates one section of the dialog from the next with a blank row.
    ///
    /// Distinct paragraphs a builder pushes are otherwise drawn flush against each other,
    /// which reads as a wall of text: the question, the choices, and the notes each get
    /// room to breathe. Repeated calls collapse to the same gap, so a section that
    /// conditionally adds more text does not double the space. The gap is one row, not
    /// the two a prose paragraph gets: the blanks between sections would otherwise push
    /// the end of a long reason or a selected choice off a short terminal before it can
    /// be scrolled to.
    pub(crate) fn section(&mut self) {
        if self.lines.last().is_some_and(|line| line.width() > 0) {
            self.lines.push(Line::from(""));
        }
    }

    /// Adds an entire choice, marking every continuation as part of the selected entry.
    pub(crate) fn choice(
        &mut self,
        index: usize,
        text: &str,
        selected: bool,
        width: usize,
        theme: &Theme,
    ) {
        let marker = if selected { "▶" } else { " " };
        let style = if selected {
            theme.selected
        } else {
            theme.assistant
        };
        let start = self.lines.len();
        self.push(
            &format!("{marker} {} {text}", index.saturating_add(1)),
            width,
            style,
        );
        if selected {
            self.selected = Some(start..self.lines.len());
        }
    }

    /// The complete text, not just the currently visible page. A masked key stays masked here too.
    pub(crate) fn text(&self) -> String {
        let mut rows = vec![self.title.trim().to_owned()];
        rows.extend(self.lines.iter().map(ToString::to_string));
        rows.extend(self.footer.iter().map(ToString::to_string));
        rows.join("\n")
    }
}

/// The columns a tab is drawn as: a terminal cell cannot hold one, and dropping it joins two words.
const TAB: &str = "    ";

/// The blank rows a paragraph break inside prose is drawn with.
///
/// Prose used to separate paragraphs with a single empty row, which read as cramped and
/// dense. Two give the eye a real pause; runs of blank source lines still collapse to one
/// gap, so a mess of empty lines cannot turn a dialog into a column of whitespace.
pub(crate) const PARAGRAPH_GAP: usize = 2;

/// Wraps literal text by display columns, preserving all content and hard newlines.
///
/// Its own wrapper rather than the markdown renderer's, which is for prose: that one treats a run
/// of spaces as a single separator and drops the ones a line begins with. That is right for a
/// paragraph and wrong here, where the text may be the command a reader is being asked to approve
/// and `printf 'a    b'` is not `printf 'a b'`. Every space is kept except the ones a break lands
/// on and the ones a line ends with, neither of which a reader could have seen.
///
/// A blank source line is a paragraph break: it widens the gap ([`PARAGRAPH_GAP`]) and collapses
/// runs of blanks, so paragraphs get air without a dialog ever opening or closing on stray empty
/// rows.
pub(crate) fn wrapped(text: &str, width: usize, style: Style) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut rows: Vec<String> = Vec::new();
    let mut pending_gap = false;
    for raw in text.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if line.is_empty() {
            if !rows.is_empty() && rows.last().is_some_and(|row| !row.is_empty()) {
                pending_gap = true;
            }
            continue;
        }
        if pending_gap {
            rows.extend((0..PARAGRAPH_GAP).map(|_| String::new()));
            pending_gap = false;
        }
        if line.contains('\t') {
            wrap_line(&line.replace('\t', TAB), width, &mut rows);
        } else {
            wrap_line(line, width, &mut rows);
        }
    }
    if rows.is_empty() {
        rows.push(String::new());
    }
    rows.into_iter()
        .map(|row| Line::styled(row, style))
        .collect()
}

/// Wraps one line that has no newline in it, adding at least one row.
fn wrap_line(line: &str, width: usize, rows: &mut Vec<String>) {
    let mut row = String::new();
    let mut used = 0_usize;
    // Whether the row holds anything but indentation, which is what makes a break before the
    // next word worth taking: breaking an indent off onto a row of its own shows a blank line.
    let mut content = false;
    // The spaces since the last word, waiting to learn whether another follows on this row.
    let mut gap = 0_usize;
    for (index, word) in line.split(' ').enumerate() {
        if index > 0 {
            gap = gap.saturating_add(1);
        }
        if word.is_empty() {
            continue;
        }
        let columns = word.width();
        if content && used.saturating_add(gap).saturating_add(columns) > width {
            rows.push(core::mem::take(&mut row));
            (used, gap, content) = (0, 0, false);
        }
        // An indent wider than the dialog keeps a column for the text it is indenting.
        let spaces = gap.min(width.saturating_sub(used).saturating_sub(1));
        row.push_str(&" ".repeat(spaces));
        used = used.saturating_add(spaces);
        gap = 0;
        for grapheme in word.graphemes(true) {
            let cells = grapheme.width();
            // A word longer than a row is cut by columns, never inside a grapheme. One wider
            // than the whole dialog still gets a row to itself, so the loop always advances.
            if used.saturating_add(cells) > width && used > 0 {
                if content {
                    rows.push(core::mem::take(&mut row));
                } else {
                    row.clear();
                }
                used = 0;
            }
            row.push_str(grapheme);
            used = used.saturating_add(cells);
            content = true;
        }
    }
    rows.push(row);
}

/// A dialog's outer rectangle, always contained in the actual terminal area.
pub(crate) fn bounds(area: Rect, maximum: u16, rows: usize) -> Rect {
    // Small terminals give up margin before content or actions. There is no minimum width that
    // can accidentally exceed the screen, even while a window is being dragged through zero size.
    let width = if area.width > 30 {
        area.width.saturating_sub(4)
    } else {
        area.width
    };
    let available = if area.height > 12 {
        area.height.saturating_sub(4)
    } else {
        area.height
    };
    let height = u16::try_from(rows).unwrap_or(u16::MAX).min(available);
    let width = width.min(maximum);
    Rect {
        x: area.x.saturating_add(area.width.saturating_sub(width) >> 1),
        y: area
            .y
            .saturating_add(area.height.saturating_sub(height) >> 1),
        width,
        height,
    }
}

/// The content width, accounting for borders only when they leave useful text room.
pub(crate) fn inner_width(area: Rect, maximum: u16) -> usize {
    let rect = bounds(area, maximum, usize::from(area.height));
    usize::from(
        rect.width
            .saturating_sub(if rect.width >= 4 { 2 } else { 0 })
            .max(1),
    )
}

/// Draws one page, reserving footer rows before granting rows to the scrollable body.
///
/// The offset is kept by the view. The caller clears it when a different dialog opens; a moved
/// selection is brought into view from wherever the reader was, and ordinary scrolling is clamped
/// here against the actual wrapped row count and viewport.
///
/// Returns the rectangle the dialog was drawn in, which is what tells the caller what it covers.
pub(crate) fn render(
    frame: &mut Frame<'_>,
    area: Rect,
    maximum: u16,
    content: &Content,
    scroll: &mut u16,
    appearance: (bool, &Theme),
) -> Rect {
    let (reveal_selection, theme) = appearance;
    let count = content
        .lines
        .len()
        .saturating_add(content.footer.len())
        .saturating_add(2);
    let rect = bounds(area, maximum, count);
    frame.render_widget(Clear, rect);
    let bordered = rect.width >= 4 && rect.height >= 4;
    let block = Block::default()
        .borders(if bordered {
            Borders::ALL
        } else {
            Borders::NONE
        })
        .style(theme.assistant)
        .border_style(theme.border)
        .title(Line::styled(content.title, theme.notice));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    // If even the controls do not fit, keep them reachable in a single scrollable surface.
    let footer_rows = u16::try_from(content.footer.len()).unwrap_or(u16::MAX);
    if footer_rows >= inner.height {
        render_tiny(frame, inner, content, scroll, appearance);
        return rect;
    }
    let body_rows = inner.height.saturating_sub(footer_rows);
    let limit = content.lines.len().saturating_sub(usize::from(body_rows));
    let mut offset = usize::from(*scroll).min(limit);
    if reveal_selection && let Some(selected) = &content.selected {
        let room = usize::from(body_rows);
        if selected.start < offset {
            offset = selected.start;
        } else if selected.end > offset.saturating_add(room) {
            offset = if selected.len() <= room {
                selected.end.saturating_sub(room)
            } else {
                selected.start
            };
        }
        offset = offset.min(limit);
    }
    *scroll = u16::try_from(offset).unwrap_or(u16::MAX);
    let body = Rect {
        height: body_rows,
        ..inner
    };
    let footer = Rect {
        y: inner.y.saturating_add(body_rows),
        height: footer_rows,
        ..inner
    };
    let lines = content
        .lines
        .iter()
        .skip(offset)
        .take(usize::from(body_rows))
        .cloned()
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(lines).style(theme.assistant), body);
    frame.render_widget(
        Paragraph::new(content.footer.clone()).style(theme.secondary),
        footer,
    );
    rect
}

/// A tiny terminal has no room for fixed chrome. Nothing is discarded, including action hints.
fn render_tiny(
    frame: &mut Frame<'_>,
    inner: Rect,
    content: &Content,
    scroll: &mut u16,
    appearance: (bool, &Theme),
) {
    let (reveal_selection, theme) = appearance;
    let all = content
        .lines
        .iter()
        .chain(&content.footer)
        .cloned()
        .collect::<Vec<_>>();
    let limit = all.len().saturating_sub(usize::from(inner.height));
    let offset = if reveal_selection && let Some(selected) = &content.selected {
        selected.start.min(limit)
    } else {
        usize::from(*scroll).min(limit)
    };
    *scroll = u16::try_from(offset).unwrap_or(u16::MAX);
    let visible = all
        .into_iter()
        .skip(offset)
        .take(usize::from(inner.height))
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(visible).style(theme.assistant), inner);
}
