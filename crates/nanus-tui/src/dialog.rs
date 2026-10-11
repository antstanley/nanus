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

use crate::Theme;

/// The visible modal, in descending priority when several states are pending.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Modal {
    Permissions,
    Approval,
    KeyEntry,
    Credential,
    Authorization,
    Provider,
    Context,
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

/// Wraps literal text by display columns, preserving all content and hard newlines.
pub(crate) fn wrapped(text: &str, width: usize, style: Style) -> Vec<Line<'static>> {
    crate::markdown::wrap_text(text, width, style)
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
/// The offset is kept by the view. On entry or selection change the caller clears it; on ordinary
/// scrolling it is clamped here against the actual wrapped row count and viewport.
pub(crate) fn render(
    frame: &mut Frame<'_>,
    area: Rect,
    maximum: u16,
    content: &Content,
    scroll: &mut u16,
    appearance: (bool, &Theme),
) {
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
        return;
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
