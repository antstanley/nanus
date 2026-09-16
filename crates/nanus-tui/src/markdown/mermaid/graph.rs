//! Graph-shaped diagrams: flowcharts, state diagrams, and block diagrams.
//!
//! All three reduce to the same model — nodes with labels, edges between them — so they
//! share one parser for node references and one layered layout. A node lands in a layer
//! below every layer that reaches it, which is what turns `A --> B --> C` into a column
//! rather than a heap.

use std::collections::{HashMap, HashSet};

use ratatui::text::Line;

use super::super::text::str_width;
use super::super::theme::MarkdownTheme;
use super::{Canvas, half, junction, truncate};

/// The height of every node box, in rows.
const BOX_H: usize = 3;

/// The empty columns between two boxes in the same layer.
const GAP: usize = 2;

/// The empty rows between two boxes stacked in a horizontal layer.
const ROW_GAP: usize = 1;

/// A node's outline.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Shape {
    /// Square corners.
    Rect,
    /// Rounded corners.
    Rounded,
    /// A decision diamond, drawn rounded.
    Diamond,
    /// A state circle, drawn rounded.
    Circle,
}

/// One node.
pub(crate) struct Node {
    label: String,
    shape: Shape,
}

/// One edge.
pub(crate) struct Edge {
    from: usize,
    to: usize,
    label: Option<String>,
}

/// A parsed diagram ready to lay out.
struct Graph {
    vertical: bool,
    reverse: bool,
    nodes: Vec<Node>,
    edges: Vec<Edge>,
}

/// Parses and renders a `graph`/`flowchart` block.
pub(crate) fn flowchart(
    source: &str,
    width: usize,
    theme: &MarkdownTheme,
) -> Option<Vec<Line<'static>>> {
    let mut lines = source.lines();
    let header = lines.next()?.trim();
    let code = header.split_whitespace().nth(1).unwrap_or("");
    let (vertical, reverse) = direction(code);
    let mut nodes = Vec::new();
    let mut index = HashMap::new();
    let mut edges = Vec::new();
    for line in lines {
        let line = strip_comment(line).trim();
        if line.is_empty() {
            continue;
        }
        parse_line(line, &mut nodes, &mut index, &mut edges, None);
    }
    render_graph(vertical, reverse, nodes, edges, width, theme)
}

/// Parses and renders a `stateDiagram` block as a downward graph.
pub(crate) fn state(
    source: &str,
    width: usize,
    theme: &MarkdownTheme,
) -> Option<Vec<Line<'static>>> {
    let mut nodes = Vec::new();
    let mut index = HashMap::new();
    let mut edges = Vec::new();
    for line in source.lines() {
        let line = strip_comment(line).trim();
        if line.is_empty()
            || line.starts_with("stateDiagram")
            || line.starts_with("state ")
            || line.starts_with("note ")
            || line == "}"
            || line.ends_with('{')
        {
            continue;
        }
        let (body, label) = match line.split_once(':') {
            Some((body, label)) => (body.trim(), Some(label.trim())),
            None => (line, None),
        };
        let body = body.replace("[*]", "__mark__");
        parse_line(&body, &mut nodes, &mut index, &mut edges, label);
    }
    render_graph(true, false, nodes, edges, width, theme)
}

/// Parses and renders a `block` diagram as a grid of connected boxes.
pub(crate) fn block(
    source: &str,
    width: usize,
    theme: &MarkdownTheme,
) -> Option<Vec<Line<'static>>> {
    let mut blocks: Vec<String> = Vec::new();
    let mut columns = 1_usize;
    for line in source.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('%') {
            continue;
        }
        if line.starts_with("block") {
            continue;
        }
        if let Some(rest) = line.strip_prefix("columns ") {
            if let Ok(count) = rest.trim().parse::<usize>() {
                columns = count.max(1);
            }
            continue;
        }
        for token in line.split_whitespace() {
            if token == "columns" || token.parse::<usize>().is_ok() {
                continue;
            }
            let cleaned = token.trim_matches('"').trim_matches('\'');
            if !cleaned.is_empty() {
                blocks.push(cleaned.to_owned());
            }
        }
    }
    if blocks.is_empty() {
        return None;
    }
    let mut nodes: Vec<Node> = Vec::new();
    let mut index = HashMap::new();
    let mut edges = Vec::new();
    for label in &blocks {
        insert_node(&mut nodes, &mut index, label, label, Shape::Rounded);
    }
    for left in 0..blocks.len() {
        if left.saturating_add(columns) < blocks.len() {
            edges.push(Edge {
                from: left,
                to: left.saturating_add(columns),
                label: None,
            });
        }
        let wrapped = left
            .saturating_add(1)
            .checked_rem(columns)
            .unwrap_or_default();
        if left.saturating_add(1) < blocks.len() && wrapped != 0 {
            edges.push(Edge {
                from: left,
                to: left.saturating_add(1),
                label: None,
            });
        }
    }
    render_graph(true, false, nodes, edges, width, theme)
}

/// Lays out and draws a graph.
fn render_graph(
    vertical: bool,
    reverse: bool,
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    width: usize,
    theme: &MarkdownTheme,
) -> Option<Vec<Line<'static>>> {
    if nodes.is_empty() {
        return None;
    }
    let graph = Graph {
        vertical,
        reverse,
        nodes,
        edges,
    };
    let width = width.max(1);
    let max_label = width.saturating_sub(4).max(1);
    let labels: Vec<String> = graph
        .nodes
        .iter()
        .map(|node| truncate(&node.label, max_label))
        .collect();
    let widths: Vec<usize> = labels
        .iter()
        .map(|label| str_width(label).saturating_add(2).max(3))
        .collect();
    let layers = layers(graph.nodes.len(), &graph.edges);
    let groups = group(&layers);
    let boxes = place(vertical, &groups, &widths, width);
    let (canvas_width, canvas_height) = canvas_size(vertical, &boxes, width);
    let mut canvas = Canvas::new(canvas_width, canvas_height);
    draw_boxes(&mut canvas, &graph.nodes, &boxes, &labels, theme);
    draw_edges(&mut canvas, &graph, &boxes, theme);
    Some(canvas.into_lines())
}

/// A node's position and size.
#[derive(Clone, Copy)]
struct Box {
    x: usize,
    y: usize,
    w: usize,
    h: usize,
}

/// Assigns each node a layer, one below every layer that reaches it.
fn layers(count: usize, edges: &[Edge]) -> Vec<usize> {
    let mut layer = vec![0_usize; count];
    let mut in_degree = vec![0_usize; count];
    let mut adjacency: Vec<Vec<usize>> = vec![Vec::new(); count];
    let mut seen: HashSet<(usize, usize)> = HashSet::new();
    for edge in edges {
        if edge.from == edge.to || !seen.insert((edge.from, edge.to)) {
            continue;
        }
        if let Some(target) = in_degree.get_mut(edge.to) {
            *target = target.saturating_add(1);
        }
        if let Some(neighbours) = adjacency.get_mut(edge.from) {
            neighbours.push(edge.to);
        }
    }
    let mut queue: Vec<usize> = (0..count).filter(|&node| in_degree[node] == 0).collect();
    while let Some(node) = queue.pop() {
        let Some(neighbours) = adjacency.get(node) else {
            continue;
        };
        for &next in neighbours {
            let deepest = layer.get(node).copied().unwrap_or(0).saturating_add(1);
            if let Some(current) = layer.get_mut(next) {
                *current = (*current).max(deepest);
            }
            if let Some(target) = in_degree.get_mut(next) {
                *target = target.saturating_sub(1);
                if *target == 0 {
                    queue.push(next);
                }
            }
        }
    }
    layer
}

/// Groups node indices by layer, preserving node order within a layer.
fn group(layers: &[usize]) -> Vec<Vec<usize>> {
    let deepest = layers.iter().copied().max().unwrap_or(0);
    let mut groups: Vec<Vec<usize>> = vec![Vec::new(); deepest.saturating_add(1)];
    for (node, &layer) in layers.iter().enumerate() {
        if let Some(group) = groups.get_mut(layer) {
            group.push(node);
        }
    }
    groups
}

/// Places every node, returning one box per node.
fn place(vertical: bool, groups: &[Vec<usize>], widths: &[usize], width: usize) -> Vec<Box> {
    let mut boxes = vec![
        Box {
            x: 0,
            y: 0,
            w: 3,
            h: BOX_H,
        };
        widths.len()
    ];
    if vertical {
        place_vertical(&mut boxes, groups, widths, width);
    } else {
        place_horizontal(&mut boxes, groups, widths, width);
    }
    boxes
}

/// Stacks layers top to bottom, nodes side by side within a layer.
fn place_vertical(boxes: &mut [Box], groups: &[Vec<usize>], widths: &[usize], width: usize) {
    for (depth, members) in groups.iter().enumerate() {
        let span = members.iter().fold(0_usize, |total, &node| {
            total.saturating_add(widths.get(node).copied().unwrap_or(3))
        });
        let gaps = members.len().saturating_sub(1).saturating_mul(GAP);
        let total = span.saturating_add(gaps);
        let mut x = if total < width {
            half(width.saturating_sub(total))
        } else {
            0
        };
        let y = depth.saturating_mul(BOX_H.saturating_add(1));
        for &node in members {
            let node_width = widths.get(node).copied().unwrap_or(3);
            let clamped = x.min(width.saturating_sub(node_width));
            if let Some(slot) = boxes.get_mut(node) {
                *slot = Box {
                    x: clamped,
                    y,
                    w: node_width,
                    h: BOX_H,
                };
            }
            x = x.saturating_add(node_width).saturating_add(GAP);
        }
    }
}

/// Places layers left to right, nodes stacked within a layer.
fn place_horizontal(boxes: &mut [Box], groups: &[Vec<usize>], widths: &[usize], width: usize) {
    let widest = widths.iter().copied().max().unwrap_or(3);
    let stride = widest.saturating_add(3);
    for (depth, members) in groups.iter().enumerate() {
        let x = depth.saturating_mul(stride);
        let mut y = 0_usize;
        for &node in members {
            let node_width = widths.get(node).copied().unwrap_or(3);
            let clamped_x = x.min(width.saturating_sub(node_width));
            if let Some(slot) = boxes.get_mut(node) {
                *slot = Box {
                    x: clamped_x,
                    y,
                    w: node_width,
                    h: BOX_H,
                };
            }
            y = y.saturating_add(BOX_H).saturating_add(ROW_GAP);
        }
    }
}

/// The canvas size a placed diagram needs.
fn canvas_size(vertical: bool, boxes: &[Box], width: usize) -> (usize, usize) {
    let right = boxes
        .iter()
        .map(|slot| slot.x.saturating_add(slot.w))
        .max()
        .unwrap_or(width);
    let bottom = boxes
        .iter()
        .map(|slot| slot.y.saturating_add(slot.h))
        .max()
        .unwrap_or(BOX_H);
    let canvas_width = if vertical {
        width
    } else {
        right.min(width).max(1)
    };
    (canvas_width.max(1), bottom.saturating_add(2).max(BOX_H))
}

/// Draws every node box and its label.
fn draw_boxes(
    canvas: &mut Canvas,
    nodes: &[Node],
    boxes: &[Box],
    labels: &[String],
    theme: &MarkdownTheme,
) {
    for (node, slot) in nodes.iter().zip(boxes) {
        draw_box(canvas, slot, node.shape, theme);
    }
    for (slot, label) in boxes.iter().zip(labels) {
        if slot.h < BOX_H {
            continue;
        }
        canvas.centered(
            slot.x.saturating_add(1),
            slot.y.saturating_add(1),
            slot.w.saturating_sub(2),
            label,
            theme.diagram_text,
        );
    }
}

/// Draws one box outline, choosing corners from the shape.
fn draw_box(canvas: &mut Canvas, slot: &Box, shape: Shape, theme: &MarkdownTheme) {
    if slot.w < 2 || slot.h < 2 {
        return;
    }
    let (top_left, top_right, bottom_left, bottom_right) = match shape {
        Shape::Rect => ('┌', '┐', '└', '┘'),
        Shape::Rounded | Shape::Diamond | Shape::Circle => ('╭', '╮', '╰', '╯'),
    };
    let right = slot.x.saturating_add(slot.w).saturating_sub(1);
    let bottom = slot.y.saturating_add(slot.h).saturating_sub(1);
    canvas.put(slot.x, slot.y, top_left, theme.diagram_border);
    canvas.put(right, slot.y, top_right, theme.diagram_border);
    canvas.put(slot.x, bottom, bottom_left, theme.diagram_border);
    canvas.put(right, bottom, bottom_right, theme.diagram_border);
    let mut x = slot.x.saturating_add(1);
    while x < right {
        canvas.put(x, slot.y, '─', theme.diagram_border);
        canvas.put(x, bottom, '─', theme.diagram_border);
        x = x.saturating_add(1);
    }
    let mut y = slot.y.saturating_add(1);
    while y < bottom {
        canvas.put(slot.x, y, '│', theme.diagram_border);
        canvas.put(right, y, '│', theme.diagram_border);
        y = y.saturating_add(1);
    }
}

/// Routes and draws every edge, then resolves the junctions.
fn draw_edges(canvas: &mut Canvas, graph: &Graph, boxes: &[Box], theme: &MarkdownTheme) {
    let mut cells: HashSet<(usize, usize)> = HashSet::new();
    let mut arrows: Vec<(usize, usize, char)> = Vec::new();
    let mut labels: Vec<(usize, usize, String)> = Vec::new();
    for edge in &graph.edges {
        let (Some(from), Some(to)) = (boxes.get(edge.from), boxes.get(edge.to)) else {
            continue;
        };
        if graph.vertical {
            route_vertical(
                from,
                to,
                &mut cells,
                &mut arrows,
                &mut labels,
                edge,
                graph.reverse,
            );
        } else {
            route_horizontal(
                from,
                to,
                &mut cells,
                &mut arrows,
                &mut labels,
                edge,
                graph.reverse,
            );
        }
    }
    for &(x, y) in &cells {
        if !canvas.is_free(x, y) {
            continue;
        }
        let up = cells.contains(&(x, y.saturating_sub(1)));
        let down = cells.contains(&(x, y.saturating_add(1)));
        let left = cells.contains(&(x.saturating_sub(1), y));
        let right = cells.contains(&(x.saturating_add(1), y));
        canvas.put(x, y, junction(up, down, left, right), theme.diagram_edge);
    }
    for &(x, y, arrow) in &arrows {
        canvas.put(x, y, arrow, theme.diagram_arrow);
    }
    for (x, y, label) in &labels {
        if *y > 0 {
            canvas.text(
                x.saturating_sub(half(str_width(label))),
                *y,
                label,
                theme.diagram_label,
            );
        }
    }
}

/// Routes an edge between two boxes in a vertical layout.
// Routing threads the edge set, the arrow list, and the label list through one pass, so
// the parameter count is the shape of the algorithm rather than an oversight. Splitting
// them into a struct would not reduce the coupling.
#[allow(clippy::too_many_arguments)]
fn route_vertical(
    from: &Box,
    to: &Box,
    cells: &mut HashSet<(usize, usize)>,
    arrows: &mut Vec<(usize, usize, char)>,
    labels: &mut Vec<(usize, usize, String)>,
    edge: &Edge,
    reverse: bool,
) {
    let source_x = from.x.saturating_add(half(from.w));
    let target_x = to.x.saturating_add(half(to.w));
    let start_y = from.y.saturating_add(from.h);
    let row = to.y.saturating_sub(1);
    let top = start_y.min(row);
    let bottom = start_y.max(row);
    add_vertical(cells, source_x, top, bottom);
    add_horizontal(cells, source_x.min(target_x), source_x.max(target_x), row);
    let arrow = if reverse { '▲' } else { '▼' };
    arrows.push((target_x, row, arrow));
    if let Some(label) = &edge.label {
        labels.push((
            source_x
                .saturating_add(target_x)
                .checked_div(2)
                .unwrap_or(target_x),
            row,
            label.clone(),
        ));
    }
}

/// Routes an edge between two boxes in a horizontal layout.
// The horizontal twin of `route_vertical`, with the same parameter shape for the same
// reason.
#[allow(clippy::too_many_arguments)]
fn route_horizontal(
    from: &Box,
    to: &Box,
    cells: &mut HashSet<(usize, usize)>,
    arrows: &mut Vec<(usize, usize, char)>,
    labels: &mut Vec<(usize, usize, String)>,
    edge: &Edge,
    reverse: bool,
) {
    let source_y = from.y.saturating_add(half(from.h));
    let target_y = to.y.saturating_add(half(to.h));
    let start_x = from.x.saturating_add(from.w);
    let column = to.x.saturating_sub(1);
    let left = start_x.min(column);
    let right = start_x.max(column);
    add_horizontal(cells, left, right, source_y);
    add_vertical(
        cells,
        column,
        source_y.min(target_y),
        source_y.max(target_y),
    );
    let arrow = if reverse { '◄' } else { '►' };
    arrows.push((column, target_y, arrow));
    if let Some(label) = &edge.label {
        labels.push((
            column,
            source_y
                .saturating_add(target_y)
                .checked_div(2)
                .unwrap_or(source_y),
            label.clone(),
        ));
    }
}

/// Adds every cell of a horizontal segment, inclusive.
fn add_horizontal(cells: &mut HashSet<(usize, usize)>, x1: usize, x2: usize, y: usize) {
    for x in x1.min(x2)..=x1.max(x2) {
        cells.insert((x, y));
    }
}

/// Adds every cell of a vertical segment, inclusive.
fn add_vertical(cells: &mut HashSet<(usize, usize)>, x: usize, y1: usize, y2: usize) {
    for y in y1.min(y2)..=y1.max(y2) {
        cells.insert((x, y));
    }
}

/// Splits a statement into alternating node and link pieces.
fn parse_line(
    line: &str,
    nodes: &mut Vec<Node>,
    index: &mut HashMap<String, usize>,
    edges: &mut Vec<Edge>,
    default_label: Option<&str>,
) {
    let mut previous: Option<usize> = None;
    let mut pending: Option<Option<String>> = None;
    for piece in pieces(line) {
        match piece {
            Piece::Node(text) => {
                if let Some(current) = ensure_node(&text, nodes, index) {
                    if let (Some(from), Some(label)) = (previous, pending.take()) {
                        edges.push(Edge {
                            from,
                            to: current,
                            label: label.or_else(|| default_label.map(str::to_owned)),
                        });
                    }
                    previous = Some(current);
                }
            }
            Piece::Link(label) => pending = Some(label),
        }
    }
}

/// A statement piece: a node reference or the link that follows one.
enum Piece {
    Node(String),
    Link(Option<String>),
}

/// Splits `line` on its arrows, keeping any `|label|` with the link.
fn pieces(line: &str) -> Vec<Piece> {
    let mut out = Vec::new();
    let mut rest = line;
    loop {
        let Some((position, length)) = find_arrow(rest) else {
            out.push(Piece::Node(rest.trim().to_owned()));
            break;
        };
        out.push(Piece::Node(
            rest.get(..position).unwrap_or("").trim().to_owned(),
        ));
        let after = rest.get(position.saturating_add(length)..).unwrap_or("");
        let (label, remainder) = take_label(after);
        out.push(Piece::Link(label));
        rest = remainder;
    }
    out
}

/// Finds the earliest arrow in `text`, preferring the longest at that position.
fn find_arrow(text: &str) -> Option<(usize, usize)> {
    let mut best: Option<(usize, usize)> = None;
    for arrow in ["-->", "---", "-.->", "==>", "->", "--"] {
        if let Some(position) = text.find(arrow) {
            let candidate = (position, arrow.len());
            let keep = best.is_some_and(|current| current.0 <= position);
            if !keep {
                best = Some(candidate);
            }
        }
    }
    best
}

/// Reads an optional `|label|` after an arrow.
fn take_label(after: &str) -> (Option<String>, &str) {
    let Some(inner) = after.strip_prefix('|') else {
        return (None, after);
    };
    inner.find('|').map_or((None, after), |end| {
        (
            Some(inner.get(..end).unwrap_or("").trim().to_owned()),
            inner.get(end.saturating_add(1)..).unwrap_or(""),
        )
    })
}

/// Builds a node from a reference like `A[Label]`, returning its index.
fn ensure_node(
    text: &str,
    nodes: &mut Vec<Node>,
    index: &mut HashMap<String, usize>,
) -> Option<usize> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if text == "__mark__" {
        return Some(insert_node(nodes, index, text, "●", Shape::Circle));
    }
    for (open, close, shape) in [
        ("((", "))", Shape::Circle),
        ("[[", "]]", Shape::Rect),
        ("{{", "}}", Shape::Diamond),
        ("(", ")", Shape::Rounded),
        ("[", "]", Shape::Rect),
        ("{", "}", Shape::Diamond),
    ] {
        let Some(start) = text.find(open) else {
            continue;
        };
        let Some(inner) = inner_of(text, open, close) else {
            continue;
        };
        let id = text.get(..start).unwrap_or("").trim();
        let label = inner.trim().trim_matches('"').trim();
        let id = if id.is_empty() { label } else { id };
        return Some(insert_node(nodes, index, id, label, shape));
    }
    Some(insert_node(nodes, index, text, text, Shape::Rect))
}

/// Inserts a node once, returning its stable index.
fn insert_node(
    nodes: &mut Vec<Node>,
    index: &mut HashMap<String, usize>,
    id: &str,
    label: &str,
    shape: Shape,
) -> usize {
    if let Some(&existing) = index.get(id) {
        if let Some(node) = nodes.get_mut(existing)
            && node.label == id
            && label != id
        {
            label.clone_into(&mut node.label);
        }
        return existing;
    }
    let position = nodes.len();
    index.insert(id.to_owned(), position);
    nodes.push(Node {
        label: label.to_owned(),
        shape,
    });
    position
}

/// The text between `open` and its matching `close`.
fn inner_of<'a>(text: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let start = text.find(open)?;
    let body = start.saturating_add(open.len());
    let end = text.get(body..)?.find(close)?;
    text.get(body..body.saturating_add(end))
}

/// Removes a `%%` comment from a line.
fn strip_comment(line: &str) -> &str {
    line.split("%%").next().unwrap_or(line)
}

/// Maps a direction code to (vertical, points-backward).
fn direction(code: &str) -> (bool, bool) {
    match code {
        "BT" => (true, true),
        "LR" => (false, false),
        "RL" => (false, true),
        _ => (true, false),
    }
}
