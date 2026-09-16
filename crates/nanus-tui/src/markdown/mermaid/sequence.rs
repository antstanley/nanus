//! Sequence diagrams: participants, lifelines, and messages.
//!
//! Laid out on the shared [`Canvas`]: participants are columns, lifelines run down,
//! and each message is a row with its text above the arrow. A message to the same
//! participant is drawn as a small loop rather than a zero-length line.

use ratatui::text::Line;

use super::super::text::str_width;
use super::super::theme::MarkdownTheme;
use super::{Canvas, half, truncate};

/// The rows a participant's box occupies.
const HEADER_H: usize = 3;

/// One participant.
struct Participant {
    id: String,
    label: String,
}

/// One message.
struct Message {
    from: usize,
    to: usize,
    text: String,
    dotted: bool,
    self_note: bool,
}

/// Parses and renders a `sequenceDiagram` block.
pub(crate) fn render(
    source: &str,
    width: usize,
    theme: &MarkdownTheme,
) -> Option<Vec<Line<'static>>> {
    let mut participants: Vec<Participant> = Vec::new();
    let mut messages: Vec<Message> = Vec::new();
    for line in source.lines() {
        let line = strip_comment(line).trim();
        if line.is_empty() || structural(line) {
            continue;
        }
        if let Some(rest) = line
            .strip_prefix("participant ")
            .or_else(|| line.strip_prefix("actor "))
        {
            let (id, label) = match rest.split_once(" as ") {
                Some((id, label)) => (id.trim(), label.trim()),
                None => (rest.trim(), rest.trim()),
            };
            let _ = ensure(&mut participants, id, label);
            continue;
        }
        if let Some(rest) = line.strip_prefix("Note ") {
            if let Some((name, text)) = rest.split_once(':') {
                let id = name
                    .split(',')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .trim_start_matches("over ")
                    .trim_start_matches("left of ")
                    .trim_start_matches("right of ")
                    .trim();
                let index = ensure(&mut participants, id, id);
                messages.push(Message {
                    from: index,
                    to: index,
                    text: format!("[{}]", text.trim()),
                    dotted: true,
                    self_note: true,
                });
            }
            continue;
        }
        if let Some((from, to, text, dotted)) = parse_message(line) {
            let from = ensure(&mut participants, from, from);
            let to = ensure(&mut participants, to, to);
            messages.push(Message {
                from,
                to,
                text,
                dotted,
                self_note: false,
            });
        }
    }
    if participants.is_empty() {
        return None;
    }
    Some(draw(&participants, &messages, width, theme))
}

/// Finds a participant by id, adding it with `label` when it is new.
fn ensure(participants: &mut Vec<Participant>, id: &str, label: &str) -> usize {
    if let Some(position) = participants
        .iter()
        .position(|participant| participant.id == id)
    {
        if let Some(participant) = participants.get_mut(position)
            && participant.label == participant.id
            && label != id
        {
            label.clone_into(&mut participant.label);
        }
        return position;
    }
    let position = participants.len();
    participants.push(Participant {
        id: id.to_owned(),
        label: if label.is_empty() {
            id.to_owned()
        } else {
            label.to_owned()
        },
    });
    position
}

/// Renders the diagram.
fn draw(
    participants: &[Participant],
    messages: &[Message],
    width: usize,
    theme: &MarkdownTheme,
) -> Vec<Line<'static>> {
    let width = width.max(4);
    let height = HEADER_H
        .saturating_add(messages.len().saturating_mul(2))
        .saturating_add(1);
    let mut canvas = Canvas::new(width, height);
    let count = participants.len();
    let slot = width.checked_div(count).unwrap_or(width).max(1);
    let centers: Vec<usize> = (0..count)
        .map(|index| {
            index
                .saturating_mul(slot)
                .saturating_add(half(slot))
                .min(width.saturating_sub(1))
        })
        .collect();
    draw_headers(&mut canvas, participants, &centers, slot, width, theme);
    for center in &centers {
        let mut y = HEADER_H;
        while y < height {
            canvas.put(*center, y, '│', theme.diagram_border);
            y = y.saturating_add(1);
        }
    }
    for (index, message) in messages.iter().enumerate() {
        let row = HEADER_H
            .saturating_add(index.saturating_mul(2))
            .saturating_add(1);
        draw_message(&mut canvas, message, &centers, row, width, theme);
    }
    canvas.into_lines()
}

/// Draws the participant boxes across the top.
fn draw_headers(
    canvas: &mut Canvas,
    participants: &[Participant],
    centers: &[usize],
    slot: usize,
    width: usize,
    theme: &MarkdownTheme,
) {
    for (participant, center) in participants.iter().zip(centers) {
        let label = truncate(&participant.label, slot.saturating_sub(2).max(1));
        let box_width = str_width(&label).saturating_add(2).min(slot).max(3);
        let left = center.saturating_sub(half(box_width));
        let right = left
            .saturating_add(box_width)
            .saturating_sub(1)
            .min(width.saturating_sub(1));
        canvas.put(left, 0, '╭', theme.diagram_border);
        canvas.put(right, 0, '╮', theme.diagram_border);
        canvas.put(left, 2, '╰', theme.diagram_border);
        canvas.put(right, 2, '╯', theme.diagram_border);
        let mut x = left.saturating_add(1);
        while x < right {
            canvas.put(x, 0, '─', theme.diagram_border);
            canvas.put(x, 2, '─', theme.diagram_border);
            x = x.saturating_add(1);
        }
        canvas.centered(
            left.saturating_add(1),
            1,
            right.saturating_sub(left).saturating_sub(1),
            &label,
            theme.diagram_text,
        );
    }
}

/// Draws one message's text and arrow.
fn draw_message(
    canvas: &mut Canvas,
    message: &Message,
    centers: &[usize],
    row: usize,
    width: usize,
    theme: &MarkdownTheme,
) {
    let (Some(&from), Some(&to)) = (centers.get(message.from), centers.get(message.to)) else {
        return;
    };
    if message.self_note || from == to {
        canvas.text(from.saturating_add(1), row, "↻", theme.diagram_arrow);
        canvas.text(
            from.saturating_add(3),
            row,
            &truncate(&message.text, width.saturating_sub(from).saturating_sub(4)),
            theme.diagram_label,
        );
        return;
    }
    let left = from.min(to);
    let right = from.max(to);
    let mut x = left;
    while x <= right {
        canvas.put(
            x,
            row,
            if message.dotted { '┄' } else { '─' },
            theme.diagram_edge,
        );
        x = x.saturating_add(1);
    }
    let arrow = if to > from { '►' } else { '◄' };
    canvas.put(right, row, arrow, theme.diagram_arrow);
    if !message.text.is_empty() {
        let label = truncate(&message.text, width.saturating_sub(2).max(1));
        let start = half(left.saturating_add(right)).saturating_sub(half(str_width(&label)));
        canvas.text(start, row.saturating_sub(1), &label, theme.diagram_label);
    }
}

/// Whether a line is a sequence keyword rather than a message.
fn structural(line: &str) -> bool {
    line == "sequenceDiagram"
        || line == "autonumber"
        || line == "end"
        || line.starts_with("loop")
        || line.starts_with("alt")
        || line.starts_with("else")
        || line.starts_with("opt")
        || line.starts_with("par")
        || line.starts_with("critical")
        || line.starts_with("activate")
        || line.starts_with("deactivate")
}

/// Parses `A->>B: text` into its endpoints, text, and whether the arrow is dotted.
fn parse_message(line: &str) -> Option<(&str, &str, String, bool)> {
    for arrow in ["-->>", "->>", "-->", "->"] {
        if let Some(position) = line.find(arrow) {
            let from = line.get(..position)?.trim();
            let after = line.get(position.saturating_add(arrow.len())..)?;
            let (to, text) = match after.split_once(':') {
                Some((to, text)) => (to.trim(), text.trim()),
                None => (after.trim(), ""),
            };
            if from.is_empty() || to.is_empty() {
                return None;
            }
            return Some((from, to, text.to_owned(), arrow.contains("--")));
        }
    }
    None
}

/// Removes a `%%` comment from a line.
fn strip_comment(line: &str) -> &str {
    line.split("%%").next().unwrap_or(line)
}
