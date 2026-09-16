//! Mermaid diagrams, drawn as text.
//!
//! Eight diagram kinds, dispatched on the first line of the source exactly as Mermaid
//! itself dispatches them. Everything is pure text on a character canvas: no graphics
//! protocol, no image, no I/O — a diagram that cannot be parsed returns `None` and the
//! caller falls back to showing the fenced code, which is the one behaviour that keeps a
//! half-written diagram from disappearing.
//!
//! The canvas is a grid of styled cells rather than a list of strings because diagrams
//! have junctions: two edges meeting need `┼`, not the last one drawn. Edges are
//! rasterised into a set, then each cell resolves its own glyph against its orthogonal
//! neighbours, so a crossing is right no matter which edge put it there.

mod charts;
mod class;
mod graph;
mod sequence;

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::text::{char_width, str_width};
use super::theme::MarkdownTheme;

/// Renders a mermaid source, or `None` when it is not a diagram this module knows.
pub(crate) fn render(
    source: &str,
    width: usize,
    theme: &MarkdownTheme,
) -> Option<Vec<Line<'static>>> {
    let first = source.lines().next()?.trim();
    if first.starts_with("graph") || first.starts_with("flowchart") {
        graph::flowchart(source, width, theme)
    } else if first.starts_with("sequenceDiagram") {
        sequence::render(source, width, theme)
    } else if first.starts_with("pie") {
        charts::pie(source, width, theme)
    } else if first == "gantt" || first.starts_with("gantt ") {
        charts::gantt(source, width, theme)
    } else if first.starts_with("stateDiagram") {
        graph::state(source, width, theme)
    } else if first.starts_with("classDiagram") {
        class::render(source, width, theme)
    } else if first.starts_with("quadrantChart") {
        charts::quadrant(source, width, theme)
    } else if first.starts_with("block") {
        graph::block(source, width, theme)
    } else {
        None
    }
}

/// One styled character position on a [`Canvas`].
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct Cell {
    /// The character drawn here.
    pub ch: char,
    /// How it is drawn.
    pub style: Style,
}

impl Cell {
    /// A blank, unstyled cell.
    pub(crate) const fn blank() -> Self {
        Self {
            ch: ' ',
            style: Style::new(),
        }
    }
}

/// A rectangle of styled cells that renders to lines.
pub(crate) struct Canvas {
    cells: Vec<Vec<Cell>>,
}

impl Canvas {
    /// Creates a canvas of `width` by `height` blank cells.
    pub(crate) fn new(width: usize, height: usize) -> Self {
        Self {
            cells: vec![vec![Cell::blank(); width]; height],
        }
    }

    /// Writes one character, ignoring a position outside the canvas.
    pub(crate) fn put(&mut self, x: usize, y: usize, ch: char, style: Style) {
        if let Some(cell) = self.cells.get_mut(y).and_then(|row| row.get_mut(x)) {
            *cell = Cell { ch, style };
        }
    }

    /// Whether a cell is outside the canvas or still blank.
    pub(crate) fn is_free(&self, x: usize, y: usize) -> bool {
        self.cells
            .get(y)
            .and_then(|row| row.get(x))
            .is_none_or(|cell| cell.ch == ' ')
    }

    /// Writes a string starting at `(x, y)`, advancing by display width.
    pub(crate) fn text(&mut self, x: usize, y: usize, text: &str, style: Style) {
        let mut cursor = x;
        for character in text.chars() {
            self.put(cursor, y, character, style);
            cursor = cursor.saturating_add(char_width(character).max(1));
        }
    }

    /// Writes `text` centred in `[x, x + width)`.
    pub(crate) fn centered(&mut self, x: usize, y: usize, width: usize, text: &str, style: Style) {
        let text_width = str_width(text);
        let offset = x.saturating_add(half(width.saturating_sub(text_width)));
        self.text(offset, y, text, style);
    }

    /// Consumes the canvas, one `Line` per row, with trailing blanks trimmed.
    pub(crate) fn into_lines(self) -> Vec<Line<'static>> {
        self.cells.iter().map(|row| row_to_line(row)).collect()
    }
}

/// Renders one row, skipping the continuation cells of wide characters.
fn row_to_line(row: &[Cell]) -> Line<'static> {
    let end = row
        .iter()
        .rposition(|cell| cell.ch != ' ')
        .map_or(0, |index| index.saturating_add(1));
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut index = 0_usize;
    while index < end {
        let Some(cell) = row.get(index) else {
            break;
        };
        let style = cell.style;
        let mut text = String::new();
        while let Some(current) = row.get(index) {
            if index >= end || current.style != style {
                break;
            }
            text.push(current.ch);
            index = index.saturating_add(char_width(current.ch).max(1));
        }
        spans.push(Span::styled(text, style));
    }
    Line::from(spans)
}

/// Half of `value`, rounded down.
pub(crate) fn half(value: usize) -> usize {
    value.checked_div(2).unwrap_or(0)
}

/// Truncates `text` to `width` display columns, marking a cut with an ellipsis.
pub(crate) fn truncate(text: &str, width: usize) -> String {
    if str_width(text) <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let room = width.saturating_sub(1);
    let mut out = String::new();
    let mut used = 0_usize;
    for character in text.chars() {
        let current = char_width(character);
        if used.saturating_add(current) > room {
            break;
        }
        out.push(character);
        used = used.saturating_add(current);
    }
    out.push('…');
    out
}

/// Resolves a junction glyph from which orthogonal neighbours are connected.
///
/// The complete sixteen-entry truth table, so no combination can silently fall through
/// to a wrong character.
// A junction is exactly four directions; a struct of four bools would describe the same
// thing with more ceremony than the grid cell it is derived from.
#[allow(clippy::fn_params_excessive_bools)]
pub(crate) fn junction(up: bool, down: bool, left: bool, right: bool) -> char {
    match (up, down, left, right) {
        (true, true, true, true) => '┼',
        (true, true, true, false) => '┤',
        (true, true, false, true) => '├',
        (true, false, true, true) => '┴',
        (false, true, true, true) => '┬',
        (true, false, true, false) => '┘',
        (true, false, false, true) => '└',
        (false, true, true, false) => '┐',
        (false, true, false, true) => '┌',
        // Everything left is a straight run or a dead end: a cell with a vertical
        // neighbour is part of a vertical line, and otherwise it is horizontal.
        (up, down, _, _) if up || down => '│',
        _ => '─',
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::Theme;

    /// Every diagram kind, at a readable width, for the coverage tests.
    const DIAGRAMS: [&str; 9] = [
        "graph TD\nA[Start] --> B[End]",
        "graph LR\nA --> B",
        "sequenceDiagram\nAlice->>Bob: Hello\nBob-->>Alice: Hi",
        "pie title Pets\n\"Dogs\" : 386\n\"Cats\" : 85",
        "gantt\ntitle Project\nsection Phase 1\nDesign :d1, 5d\nBuild :d2, after d1, 3d",
        "stateDiagram-v2\n[*] --> Idle\nIdle --> Running",
        "classDiagram\nclass Animal {\n+name\n}\nAnimal <|-- Dog",
        "quadrantChart\ntitle Reach\nquadrant-1 High\nSpeed: [0.3, 0.6]",
        "block-beta\ncolumns 2\nA B\nC D",
    ];

    fn draw(source: &str, width: usize) -> Option<String> {
        let theme = MarkdownTheme::from_view(&Theme::default());
        render(source, width, &theme).map(|lines| {
            lines
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n")
        })
    }

    fn expect(source: &str) -> String {
        draw(source, 60).unwrap_or_default()
    }

    #[test]
    fn a_flowchart_draws_its_nodes_and_a_connector() {
        let text = expect("graph TD\nA[Start] --> B[End]");
        assert!(text.contains("Start"), "{text}");
        assert!(text.contains("End"), "{text}");
        assert!(text.contains('▼'), "{text}");
    }

    #[test]
    fn a_sequence_diagram_draws_its_participants_and_message() {
        let text = expect("sequenceDiagram\nAlice->>Bob: Hello");
        assert!(text.contains("Alice"), "{text}");
        assert!(text.contains("Bob"), "{text}");
        assert!(text.contains("Hello"), "{text}");
        assert!(text.contains('►'), "{text}");
    }

    #[test]
    fn a_pie_chart_draws_a_bar_for_each_slice() {
        let text = expect("pie title Pets\n\"Dogs\" : 386\n\"Cats\" : 85");
        assert!(text.contains("Pets"), "{text}");
        assert!(text.contains("Dogs"), "{text}");
        assert!(text.contains('%'), "{text}");
        assert!(text.contains('█'), "{text}");
    }

    #[test]
    fn a_gantt_chart_draws_its_sections_and_tasks() {
        let text = expect(
            "gantt\ntitle Project\nsection Phase 1\nDesign :d1, 5d\nBuild :d2, after d1, 3d",
        );
        assert!(text.contains("Project"), "{text}");
        assert!(text.contains("Phase 1"), "{text}");
        assert!(text.contains("Design"), "{text}");
        assert!(text.contains('█'), "{text}");
    }

    #[test]
    fn a_state_diagram_draws_its_states() {
        let text = expect("stateDiagram-v2\n[*] --> Idle\nIdle --> Running");
        assert!(text.contains("Idle"), "{text}");
        assert!(text.contains("Running"), "{text}");
    }

    #[test]
    fn a_class_diagram_draws_its_classes_and_relationship() {
        let text = expect("classDiagram\nclass Animal {\n+name\n}\nAnimal <|-- Dog");
        assert!(text.contains("Animal"), "{text}");
        assert!(text.contains("name"), "{text}");
        assert!(text.contains("Dog"), "{text}");
    }

    #[test]
    fn a_quadrant_chart_draws_its_point_legend() {
        let text = expect("quadrantChart\ntitle Reach\nquadrant-1 High\nSpeed: [0.3, 0.6]");
        assert!(text.contains("Reach"), "{text}");
        assert!(text.contains("Speed"), "{text}");
    }

    #[test]
    fn a_block_diagram_draws_its_boxes() {
        let text = expect("block-beta\ncolumns 2\nA B\nC D");
        assert!(text.contains('A'), "{text}");
        assert!(text.contains('D'), "{text}");
    }

    #[test]
    fn an_unknown_diagram_is_not_a_diagram() {
        assert!(draw("not-diagram\nx", 60).is_none());
    }

    /// A diagram is drawn as the code block's replacement, so it must obey the same
    /// width budget or the transcript's row arithmetic drifts.
    #[test]
    fn every_diagram_fits_the_width_it_is_given() {
        let theme = MarkdownTheme::from_view(&Theme::default());
        for source in DIAGRAMS {
            for width in 12..=60_usize {
                let Some(lines) = render(source, width, &theme) else {
                    panic!("{source} did not render at {width}");
                };
                for line in lines {
                    assert!(
                        line.width() <= width,
                        "width {width}: {:?} is {}",
                        line.to_string(),
                        line.width()
                    );
                }
            }
        }
    }
}
