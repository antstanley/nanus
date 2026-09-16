//! Chart-shaped diagrams: pie, gantt, and quadrant.
//!
//! These are not graphs, so they do not use the layered layout. Each one is a small
//! parse and a direct drawing: slice bars scaled to the total, task bars scaled to the
//! schedule, and points placed into a quadrant grid. Every fraction is integer
//! arithmetic — a whole number of cells — which is what keeps the bar inside its budget.

use ratatui::text::{Line, Span};

use super::super::text::{percent, scaled, str_width};
use super::super::theme::MarkdownTheme;
use super::{Canvas, half, truncate};

/// Renders a `pie` chart.
pub(crate) fn pie(source: &str, width: usize, theme: &MarkdownTheme) -> Option<Vec<Line<'static>>> {
    let mut title: Option<String> = None;
    let mut slices: Vec<(String, u64)> = Vec::new();
    for line in source.lines() {
        let line = strip_comment(line).trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("pie") {
            let rest = rest.trim();
            if let Some(name) = rest.strip_prefix("title ") {
                title = Some(name.trim().to_owned());
            } else if !rest.is_empty() {
                title = Some(rest.to_owned());
            }
            continue;
        }
        if let Some(name) = line.strip_prefix("title ") {
            title = Some(name.trim().to_owned());
            continue;
        }
        if let Some((label, value)) = line.split_once(':')
            && let Some(value) = parse_uint(value)
        {
            slices.push((label.trim().trim_matches('"').to_owned(), value));
        }
    }
    if slices.is_empty() {
        return None;
    }
    let total: u64 = slices
        .iter()
        .map(|(_, value)| *value)
        .fold(0, u64::saturating_add);
    let label_width = slices
        .iter()
        .map(|(label, _)| str_width(label))
        .max()
        .unwrap_or(1)
        .min(width.checked_div(3).unwrap_or(1).max(1));
    let bars = width.saturating_sub(label_width).saturating_sub(8).max(1);
    let mut out: Vec<Line<'static>> = Vec::new();
    if let Some(title) = title {
        out.push(Line::from(Span::styled(title, theme.diagram_accent)));
    }
    for (label, value) in &slices {
        let cells = scaled(*value, total, u64::try_from(bars).unwrap_or(1));
        let cells = usize::try_from(cells).unwrap_or(0);
        let bar = "█".repeat(cells);
        let share = percent(*value, total);
        out.push(Line::from(vec![
            Span::styled(
                format!("{:<label_width$} ", truncate(label, label_width)),
                theme.diagram_text,
            ),
            Span::styled(bar, theme.diagram_accent),
            Span::styled(format!(" {share}%"), theme.diagram_label),
        ]));
    }
    Some(out)
}

/// One gantt task.
struct Task {
    name: String,
    id: Option<String>,
    dep: Option<String>,
    duration: u64,
    start: u64,
}

/// One gantt section.
struct Section {
    name: String,
    tasks: Vec<Task>,
}

/// Renders a `gantt` chart.
pub(crate) fn gantt(
    source: &str,
    width: usize,
    theme: &MarkdownTheme,
) -> Option<Vec<Line<'static>>> {
    let mut title: Option<String> = None;
    let mut sections: Vec<Section> = Vec::new();
    for line in source.lines() {
        let line = strip_comment(line).trim();
        if line.is_empty() || line == "gantt" {
            continue;
        }
        if let Some(name) = line.strip_prefix("title ") {
            title = Some(name.trim().to_owned());
            continue;
        }
        if keyword(line) {
            continue;
        }
        if let Some(name) = line.strip_prefix("section ") {
            sections.push(Section {
                name: name.trim().to_owned(),
                tasks: Vec::new(),
            });
            continue;
        }
        let Some((name, fields)) = line.split_once(':') else {
            continue;
        };
        let fields: Vec<&str> = fields.split(',').map(str::trim).collect();
        let duration = fields
            .iter()
            .rev()
            .find_map(|field| parse_duration(field))
            .unwrap_or(1);
        let dep = fields.iter().find_map(|field| {
            field
                .strip_prefix("after ")
                .map(|name| name.trim().to_owned())
        });
        let id = if fields.len() >= 3 {
            Some(fields.first().copied().unwrap_or("").to_owned())
        } else {
            None
        };
        if sections.is_empty() {
            sections.push(Section {
                name: String::new(),
                tasks: Vec::new(),
            });
        }
        if let Some(section) = sections.last_mut() {
            section.tasks.push(Task {
                name: name.trim().to_owned(),
                id,
                dep,
                duration,
                start: 0,
            });
        }
    }
    schedule(&mut sections);
    let total = sections
        .iter()
        .flat_map(|section| section.tasks.iter())
        .map(|task| task.start.saturating_add(task.duration))
        .max()
        .unwrap_or(0);
    if total == 0 {
        return None;
    }
    let name_width = sections
        .iter()
        .flat_map(|section| section.tasks.iter())
        .map(|task| str_width(&task.name))
        .max()
        .unwrap_or(1)
        .min(width.checked_div(2).unwrap_or(1).max(1));
    let avail = u64::try_from(width.saturating_sub(name_width).saturating_sub(1)).unwrap_or(1);
    let mut out: Vec<Line<'static>> = Vec::new();
    if let Some(title) = title {
        out.push(Line::from(Span::styled(title, theme.diagram_accent)));
    }
    out.extend(gantt_bars(&sections, total, name_width, avail, theme));
    Some(out)
}

/// Renders scheduled sections as one bar per task.
fn gantt_bars(
    sections: &[Section],
    total: u64,
    name_width: usize,
    avail: u64,
    theme: &MarkdownTheme,
) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    for section in sections {
        if !section.name.is_empty() {
            out.push(Line::from(Span::styled(
                format!("── {}", section.name),
                theme.diagram_label,
            )));
        }
        for task in &section.tasks {
            let offset = usize::try_from(scaled(task.start, total, avail)).unwrap_or(0);
            let length = usize::try_from(scaled(task.duration, total, avail))
                .unwrap_or(1)
                .max(1);
            out.push(Line::from(vec![
                Span::styled(
                    format!("{:<name_width$} ", truncate(&task.name, name_width)),
                    theme.diagram_text,
                ),
                Span::styled(
                    format!("{}{}", " ".repeat(offset), "█".repeat(length)),
                    theme.diagram_accent,
                ),
            ]));
        }
    }
    out
}

/// One quadrant point.
struct Point {
    label: String,
    x: u64,
    y: u64,
}

/// Renders a `quadrantChart`.
pub(crate) fn quadrant(
    source: &str,
    width: usize,
    theme: &MarkdownTheme,
) -> Option<Vec<Line<'static>>> {
    let mut title: Option<String> = None;
    let mut x_left = String::from("Low");
    let mut x_right = String::from("High");
    let mut y_bottom = String::from("Low");
    let mut y_top = String::from("High");
    let mut quadrants = [String::new(), String::new(), String::new(), String::new()];
    let mut points: Vec<Point> = Vec::new();
    for line in source.lines() {
        let line = strip_comment(line).trim();
        if line.is_empty() || line.starts_with("quadrantChart") {
            continue;
        }
        if let Some(name) = line.strip_prefix("title ") {
            title = Some(name.trim().to_owned());
            continue;
        }
        if let Some(rest) = line.strip_prefix("x-axis ") {
            if let Some((left, right)) = rest.split_once("-->") {
                left.trim().clone_into(&mut x_left);
                right.trim().clone_into(&mut x_right);
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("y-axis ") {
            if let Some((bottom, top)) = rest.split_once("-->") {
                bottom.trim().clone_into(&mut y_bottom);
                top.trim().clone_into(&mut y_top);
            }
            continue;
        }
        if let Some(rest) = line
            .strip_prefix("quadrant-1 ")
            .or_else(|| line.strip_prefix("quadrant-2 "))
            .or_else(|| line.strip_prefix("quadrant-3 "))
            .or_else(|| line.strip_prefix("quadrant-4 "))
        {
            let index = line
                .chars()
                .nth(9)
                .and_then(|digit| digit.to_digit(10))
                .unwrap_or(1);
            if let Some(slot) =
                quadrants.get_mut(usize::try_from(index.saturating_sub(1)).unwrap_or(0))
            {
                rest.trim().clone_into(slot);
            }
            continue;
        }
        if let Some(point) = parse_point(line) {
            points.push(point);
        }
    }
    if points.is_empty() {
        return None;
    }
    Some(draw_quadrant(
        &Quad {
            title,
            x_left,
            x_right,
            y_bottom,
            y_top,
            quadrants,
            points,
        },
        width,
        theme,
    ))
}

/// A parsed quadrant chart.
struct Quad {
    title: Option<String>,
    x_left: String,
    x_right: String,
    y_bottom: String,
    y_top: String,
    quadrants: [String; 4],
    points: Vec<Point>,
}

/// Draws a parsed quadrant chart.
fn draw_quadrant(quad: &Quad, width: usize, theme: &MarkdownTheme) -> Vec<Line<'static>> {
    let Quad {
        title,
        x_left,
        x_right,
        y_bottom,
        y_top,
        quadrants,
        points,
    } = quad;
    let chart_width = width.clamp(1, 64);
    let chart_height = 11_usize;
    let mut canvas = Canvas::new(chart_width, chart_height);
    let center_x = half(chart_width);
    let center_y = half(chart_height);
    for y in 0..chart_height {
        canvas.put(center_x, y, '│', theme.diagram_border);
    }
    for x in 0..chart_width {
        canvas.put(x, center_y, '─', theme.diagram_border);
    }
    canvas.put(center_x, center_y, '┼', theme.diagram_border);
    canvas.text(
        center_x.saturating_add(2),
        0,
        &truncate(&quadrants[0], center_x.saturating_sub(2)),
        theme.diagram_label,
    );
    canvas.text(
        1,
        0,
        &truncate(&quadrants[1], center_x.saturating_sub(2)),
        theme.diagram_label,
    );
    canvas.text(
        1,
        chart_height.saturating_sub(1),
        &truncate(&quadrants[2], center_x.saturating_sub(2)),
        theme.diagram_label,
    );
    canvas.text(
        center_x.saturating_add(2),
        chart_height.saturating_sub(1),
        &truncate(&quadrants[3], center_x.saturating_sub(2)),
        theme.diagram_label,
    );
    let inner_w = u64::try_from(chart_width.saturating_sub(1)).unwrap_or(1);
    let inner_h = u64::try_from(chart_height.saturating_sub(1)).unwrap_or(1);
    for point in points {
        let x = usize::try_from(scaled(point.x, 1000, inner_w))
            .unwrap_or(0)
            .min(chart_width.saturating_sub(1));
        let from_bottom = usize::try_from(scaled(point.y, 1000, inner_h)).unwrap_or(0);
        let y = chart_height.saturating_sub(1).saturating_sub(from_bottom);
        let marker = if canvas.is_free(x, y) { '●' } else { '◆' };
        canvas.put(x, y, marker, theme.diagram_accent);
    }
    let mut out: Vec<Line<'static>> = Vec::new();
    if let Some(title) = title {
        out.push(Line::from(Span::styled(
            title.as_str().to_owned(),
            theme.diagram_accent,
        )));
    }
    out.extend(canvas.into_lines());
    out.push(Line::from(Span::styled(
        format!("{y_bottom} → {y_top}"),
        theme.diagram_label,
    )));
    out.push(Line::from(Span::styled(
        format!("{x_left} → {x_right}"),
        theme.diagram_label,
    )));
    let names: Vec<&str> = points.iter().map(|point| point.label.as_str()).collect();
    out.push(Line::from(Span::styled(
        truncate(&format!("● {}", names.join(", ")), width.max(1)),
        theme.diagram_label,
    )));
    out
}

/// Schedules tasks, resolving `after` dependencies and a declining cursor.
fn schedule(sections: &mut [Section]) {
    let mut ends: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    let mut cursor = 0_u64;
    for section in sections.iter_mut() {
        for task in &mut section.tasks {
            let dependency = task.dep.as_ref().and_then(|id| ends.get(id).copied());
            let start = dependency.unwrap_or(cursor).max(cursor);
            let end = start.saturating_add(task.duration);
            task.start = start;
            cursor = cursor.max(end);
            if let Some(id) = &task.id {
                ends.insert(id.clone(), end);
            }
        }
    }
}

/// Parses a `Label: [x, y]` point line.
fn parse_point(line: &str) -> Option<Point> {
    let (label, rest) = line.split_once(':')?;
    let open = rest.find('[')?;
    let close = rest.find(']')?;
    let coords = rest.get(open.saturating_add(1)..close)?;
    let (x, y) = coords.split_once(',')?;
    Some(Point {
        label: label.trim().to_owned(),
        x: parse_unit(x)?,
        y: parse_unit(y)?,
    })
}

/// The leading integer of `text`, if it has one.
fn parse_uint(text: &str) -> Option<u64> {
    let head = text
        .trim()
        .split(|character: char| !character.is_ascii_digit())
        .next()
        .unwrap_or("");
    head.parse::<u64>().ok()
}

/// A duration like `5d`, `2w`, or `3m`, in days.
fn parse_duration(text: &str) -> Option<u64> {
    let text = text.trim();
    let digits: String = text.chars().take_while(char::is_ascii_digit).collect();
    let value = digits.parse::<u64>().ok()?;
    let factor = match text.chars().last()? {
        'w' | 'W' => 7,
        'm' | 'M' => 30,
        'y' | 'Y' => 365,
        _ => 1,
    };
    Some(value.saturating_mul(factor))
}

/// A coordinate in per-mille, clipped to `0..=1000`.
fn parse_unit(text: &str) -> Option<u64> {
    let trimmed = text.trim();
    let (whole, fraction) = match trimmed.split_once('.') {
        Some((whole, fraction)) => (whole, fraction),
        None => (trimmed, ""),
    };
    let whole = whole.trim().parse::<u64>().ok()?;
    let mut digits = String::new();
    for character in fraction.chars().take(3) {
        if !character.is_ascii_digit() {
            break;
        }
        digits.push(character);
    }
    while digits.len() < 3 {
        digits.push('0');
    }
    let fraction = digits.parse::<u64>().ok()?;
    Some(
        whole
            .saturating_mul(1000)
            .saturating_add(fraction)
            .min(1000),
    )
}

/// Whether a gantt line is a directive rather than a task.
fn keyword(line: &str) -> bool {
    [
        "dateFormat",
        "axisFormat",
        "excludes",
        "todayMarker",
        "tickInterval",
        "weekday",
    ]
    .iter()
    .any(|keyword| line.starts_with(keyword))
}

/// Removes a `%%` comment from a line.
fn strip_comment(line: &str) -> &str {
    line.split("%%").next().unwrap_or(line)
}
