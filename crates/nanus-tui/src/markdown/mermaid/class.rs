//! Class diagrams: class boxes and their relationships.
//!
//! Drawn as a stack of member boxes followed by a relationship list, rather than routed
//! on a canvas. A class box is the information a reader wants — its members — and a
//! relationship line says what connects to what; both survive a narrow terminal, which a
//! crossed layout does not.

use ratatui::text::{Line, Span};

use super::super::text::str_width;
use super::super::theme::MarkdownTheme;
use super::truncate;

/// One class and its members.
struct Class {
    name: String,
    members: Vec<String>,
}

/// How two classes are related.
#[derive(Clone, Copy)]
enum Relation {
    Inheritance,
    Composition,
    Aggregation,
    Dependency,
    Association,
    Link,
}

/// One relationship.
struct Edge {
    left: String,
    right: String,
    kind: Relation,
    label: Option<String>,
}

/// Parses and renders a `classDiagram`.
pub(crate) fn render(
    source: &str,
    width: usize,
    theme: &MarkdownTheme,
) -> Option<Vec<Line<'static>>> {
    let mut classes: Vec<Class> = Vec::new();
    let mut edges: Vec<Edge> = Vec::new();
    let mut current: Option<usize> = None;
    for line in source.lines() {
        let line = strip_comment(line).trim();
        if line.is_empty() || line == "classDiagram" {
            continue;
        }
        if line == "}" {
            current = None;
            continue;
        }
        if let Some(rest) = line.strip_prefix("class ") {
            let name = rest.trim().trim_end_matches('{').trim();
            current = Some(ensure_class(&mut classes, name));
            continue;
        }
        if let Some(edge) = parse_edge(line) {
            edges.push(edge);
            continue;
        }
        if let Some(index) = current
            && let Some(class) = classes.get_mut(index)
        {
            class.members.push(line.to_owned());
        }
    }
    if classes.is_empty() {
        return None;
    }
    let content = classes
        .iter()
        .map(|class| {
            class
                .members
                .iter()
                .map(|member| str_width(member))
                .fold(str_width(&class.name), usize::max)
        })
        .max()
        .unwrap_or(1);
    let box_width = content.saturating_add(4).min(width.max(4));
    let mut out: Vec<Line<'static>> = Vec::new();
    for class in &classes {
        out.extend(class_box(class, box_width, theme));
    }
    if !edges.is_empty() {
        out.push(Line::from(""));
        for edge in &edges {
            out.push(relation_line(edge, width, theme));
        }
    }
    Some(out)
}

/// Finds a class by name, adding it when new.
fn ensure_class(classes: &mut Vec<Class>, name: &str) -> usize {
    if let Some(position) = classes.iter().position(|class| class.name == name) {
        return position;
    }
    let position = classes.len();
    classes.push(Class {
        name: name.to_owned(),
        members: Vec::new(),
    });
    position
}

/// Renders one class box.
fn class_box(class: &Class, width: usize, theme: &MarkdownTheme) -> Vec<Line<'static>> {
    let inner = width.saturating_sub(2).max(1);
    let title = truncate(&class.name, inner.saturating_sub(2).max(1));
    let mut top = String::from("┌─ ");
    top.push_str(&title);
    if str_width(&top) < width.saturating_sub(2) {
        top.push(' ');
    }
    while str_width(&top) < width.saturating_sub(1) {
        top.push('─');
    }
    top.push('┐');
    let mut out = vec![Line::from(Span::styled(top, theme.diagram_border))];
    for member in &class.members {
        let text = truncate(member, inner.saturating_sub(1).max(1));
        let mut line = String::from("│ ");
        line.push_str(&text);
        let used = str_width(&line);
        if used < width.saturating_sub(1) {
            line.push_str(&" ".repeat(width.saturating_sub(1).saturating_sub(used)));
        }
        line.push('│');
        out.push(Line::from(Span::styled(line, theme.diagram_text)));
    }
    out.push(Line::from(Span::styled(
        format!("└{}┘", "─".repeat(inner)),
        theme.diagram_border,
    )));
    out
}

/// Renders one relationship as a labelled line.
fn relation_line(edge: &Edge, width: usize, theme: &MarkdownTheme) -> Line<'static> {
    let glyph = match edge.kind {
        Relation::Inheritance => "◁──",
        Relation::Composition => "◆──",
        Relation::Aggregation => "○──",
        Relation::Dependency => "┄┄▶",
        Relation::Association => "──▶",
        Relation::Link => "──",
    };
    let label = edge
        .label
        .clone()
        .unwrap_or_else(|| default_label(edge.kind).to_owned());
    let base = format!("{} {} {}", edge.left, glyph, edge.right);
    let text = if label.is_empty() {
        base
    } else {
        format!("{base} : {label}")
    };
    Line::from(Span::styled(
        truncate(&text, width.max(1)),
        theme.diagram_text,
    ))
}

/// The default relationship word for a kind.
const fn default_label(kind: Relation) -> &'static str {
    match kind {
        Relation::Inheritance => "extends",
        Relation::Composition | Relation::Aggregation => "has",
        Relation::Dependency => "depends",
        Relation::Association => "uses",
        Relation::Link => "",
    }
}

/// Parses a relationship line, if it is one.
fn parse_edge(line: &str) -> Option<Edge> {
    let (token, position) = find_relation(line)?;
    let left = line.get(..position)?.trim().to_owned();
    let after = line.get(position.saturating_add(token.len())..)?.trim();
    let (right, label) = match after.split_once(':') {
        Some((right, label)) => (right.trim().to_owned(), Some(label.trim().to_owned())),
        None => (after.to_owned(), None),
    };
    if left.is_empty() || right.is_empty() {
        return None;
    }
    Some(Edge {
        left,
        right,
        kind: classify(token),
        label,
    })
}

/// Finds the earliest relationship token in `line`.
fn find_relation(line: &str) -> Option<(&'static str, usize)> {
    let mut best: Option<(&'static str, usize)> = None;
    for token in [
        "<|--", "--|>", "<|..", "..|>", "*--", "o--", "..>", "-->", "..", "--",
    ] {
        if let Some(position) = line.find(token) {
            let keep = best.is_some_and(|(_, current)| current <= position);
            if !keep {
                best = Some((token, position));
            }
        }
    }
    best
}

/// Classifies a relationship token.
fn classify(token: &str) -> Relation {
    if token.contains("<|") {
        Relation::Inheritance
    } else if token.starts_with('*') {
        Relation::Composition
    } else if token.starts_with('o') {
        Relation::Aggregation
    } else if token.starts_with("..") {
        Relation::Dependency
    } else if token.ends_with('>') {
        Relation::Association
    } else {
        Relation::Link
    }
}

/// Removes a `%%` comment from a line.
fn strip_comment(line: &str) -> &str {
    line.split("%%").next().unwrap_or(line)
}
