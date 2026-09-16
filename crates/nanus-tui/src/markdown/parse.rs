//! The block parser: source text to [`Block`]s.
//!
//! Line-oriented and stateful in one place only — a fenced code block, which has to
//! remember that it is open across lines. Everything else is decided per line and is
//! therefore safe to re-run on text that is still arriving: an unterminated fence
//! simply swallows the rest of the message as code, which is what a half-streamed code
//! block looks like and is preferable to dropping it.

use super::inline;
use super::types::{Block, Marker};

/// Parses markdown into blocks, sanitising control characters on the way in.
pub(crate) fn parse(source: &str) -> Vec<Block> {
    let cleaned = inline::sanitize(source);
    let mut lines: Vec<&str> = cleaned.lines().collect();
    strip_frontmatter(&mut lines);
    let mut blocks = Vec::new();
    let mut index = 0_usize;
    while index < lines.len() {
        let line = lines[index];
        let trimmed = line.trim();
        if let Some((marker, lang)) = fence_open(trimmed) {
            let (code, next) = read_code(&lines, index.saturating_add(1), marker);
            blocks.push(Block::Code { lang, code });
            index = next;
            continue;
        }
        if trimmed.is_empty() {
            blocks.push(Block::Blank);
            index = index.saturating_add(1);
            continue;
        }
        if is_rule(trimmed) {
            blocks.push(Block::Rule);
            index = index.saturating_add(1);
            continue;
        }
        if let Some((level, text)) = heading(trimmed) {
            blocks.push(Block::Heading { level, text });
            index = index.saturating_add(1);
            continue;
        }
        if quote_line(line).is_some() {
            let (quoted, next) = read_quote(&lines, index);
            blocks.push(Block::Quote { lines: quoted });
            index = next;
            continue;
        }
        if is_table_start(&lines, index) {
            let (table, next) = read_table(&lines, index);
            blocks.push(table);
            index = next;
            continue;
        }
        if let Some((marker, indent, text)) = list_item(line) {
            blocks.push(Block::Item {
                marker,
                indent,
                text,
            });
            index = index.saturating_add(1);
            continue;
        }
        if let Some((alt, path)) = image_line(trimmed) {
            blocks.push(Block::Image { alt, path });
            index = index.saturating_add(1);
            continue;
        }
        let (paragraph, next) = read_paragraph(&lines, index);
        blocks.push(Block::Paragraph(paragraph));
        index = next;
    }
    blocks
}

/// Removes a leading `+++`-delimited TOML frontmatter block, if there is one.
fn strip_frontmatter(lines: &mut Vec<&str>) {
    if lines.first().map(|line| line.trim()) != Some("+++") {
        return;
    }
    let Some(close) = lines.iter().skip(1).position(|line| line.trim() == "+++") else {
        return;
    };
    let end = close.saturating_add(2).min(lines.len());
    lines.drain(..end);
}

/// Opens a fence when `trimmed` is a row of three or more backticks or tildes.
fn fence_open(trimmed: &str) -> Option<(char, String)> {
    let marker = trimmed.chars().next()?;
    if marker != '`' && marker != '~' {
        return None;
    }
    if run_of(trimmed, marker) < 3 {
        return None;
    }
    let info = trimmed.trim_start_matches(marker).trim();
    let lang = info.split_whitespace().next().unwrap_or("").to_owned();
    Some((marker, lang))
}

/// Whether `trimmed` closes a fence opened with `marker`.
fn is_fence_close(trimmed: &str, marker: char) -> bool {
    run_of(trimmed, marker) >= 3 && trimmed.chars().all(|character| character == marker)
}

/// Reads a fence's body up to its close, or to the end when it never closes.
fn read_code(lines: &[&str], start: usize, marker: char) -> (String, usize) {
    let mut code = String::new();
    let mut index = start;
    while index < lines.len() {
        if is_fence_close(lines[index].trim(), marker) {
            return (
                code.trim_end_matches('\n').to_owned(),
                index.saturating_add(1),
            );
        }
        code.push_str(lines[index]);
        code.push('\n');
        index = index.saturating_add(1);
    }
    (code.trim_end_matches('\n').to_owned(), index)
}

/// Reads consecutive `>`-prefixed lines as one blockquote.
fn read_quote(lines: &[&str], start: usize) -> (Vec<(usize, String)>, usize) {
    let mut quoted = Vec::new();
    let mut index = start;
    while let Some(entry) = lines.get(index).and_then(|line| quote_line(line)) {
        quoted.push(entry);
        index = index.saturating_add(1);
    }
    (quoted, index)
}

/// Splits one `>`-prefixed line into its nesting depth and text.
fn quote_line(line: &str) -> Option<(usize, String)> {
    let mut rest = line.trim_start();
    if !rest.starts_with('>') {
        return None;
    }
    let mut depth = 0_usize;
    while let Some(after) = rest.strip_prefix('>') {
        depth = depth.saturating_add(1);
        rest = after.strip_prefix(' ').unwrap_or(after);
    }
    Some((depth, rest.trim_end().to_owned()))
}

/// Reads a paragraph: consecutive lines that do not start another block.
fn read_paragraph(lines: &[&str], start: usize) -> (String, usize) {
    let mut parts: Vec<&str> = Vec::new();
    let mut index = start;
    while index < lines.len() {
        let trimmed = lines[index].trim();
        if trimmed.is_empty() || (index > start && starts_block(lines, index)) {
            break;
        }
        parts.push(trimmed);
        index = index.saturating_add(1);
    }
    (parts.join("\n"), index)
}

/// Whether the line at `index` begins a non-paragraph block.
fn starts_block(lines: &[&str], index: usize) -> bool {
    let Some(line) = lines.get(index) else {
        return false;
    };
    let trimmed = line.trim();
    trimmed.is_empty()
        || fence_open(trimmed).is_some()
        || is_rule(trimmed)
        || heading(trimmed).is_some()
        || quote_line(line).is_some()
        || is_table_start(lines, index)
        || list_item(line).is_some()
        || image_line(trimmed).is_some()
}

/// Reads a pipe table beginning at its header row; `is_table_start` must hold.
fn read_table(lines: &[&str], start: usize) -> (Block, usize) {
    let headers = split_row(lines[start]);
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut index = start.saturating_add(2);
    while index < lines.len() {
        let trimmed = lines[index].trim();
        if trimmed.is_empty() || !trimmed.contains('|') {
            break;
        }
        if !is_table_separator(trimmed) {
            let mut row = split_row(trimmed);
            row.truncate(headers.len());
            while row.len() < headers.len() {
                row.push(String::new());
            }
            rows.push(row);
        }
        index = index.saturating_add(1);
    }
    (Block::Table { headers, rows }, index)
}

/// Whether a header row at `index` is followed by a separator row.
fn is_table_start(lines: &[&str], index: usize) -> bool {
    let header = lines.get(index).is_some_and(|line| line.contains('|'));
    let separator = lines
        .get(index.saturating_add(1))
        .is_some_and(|line| is_table_separator(line));
    header && separator
}

/// Whether a row is a table's `|---|:--:|` separator.
fn is_table_separator(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.contains('-')
        && trimmed
            .chars()
            .all(|character| matches!(character, '|' | '-' | ':' | ' '))
}

/// Splits a pipe-table row into trimmed cells, tolerating omitted edge pipes.
fn split_row(line: &str) -> Vec<String> {
    let trimmed = line.trim();
    let inner = trimmed.strip_prefix('|').unwrap_or(trimmed);
    let inner = inner.strip_suffix('|').unwrap_or(inner);
    inner
        .split('|')
        .map(|cell| cell.trim().to_owned())
        .collect()
}

/// Matches an ATX heading and returns its level and text.
fn heading(trimmed: &str) -> Option<(u8, String)> {
    let hashes = trimmed
        .chars()
        .take_while(|character| *character == '#')
        .count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = trimmed.get(hashes..)?;
    let text = rest.strip_prefix(' ')?;
    Some((
        u8::try_from(hashes).unwrap_or(6),
        text.trim_start().to_owned(),
    ))
}

/// Whether a line is a horizontal rule: three or more of one of `-`, `*`, `_`.
fn is_rule(trimmed: &str) -> bool {
    let stripped: String = trimmed
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();
    if stripped.chars().count() < 3 {
        return false;
    }
    let mut characters = stripped.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    matches!(first, '-' | '*' | '_') && characters.all(|character| character == first)
}

/// Matches a list item and returns its marker, indentation level, and text.
fn list_item(line: &str) -> Option<(Marker, usize, String)> {
    let trimmed = line.trim_start();
    let spaces = line.len().saturating_sub(trimmed.len());
    let indent = spaces.checked_div(2).unwrap_or(0);
    let (marker, rest) = marker_of(trimmed)?;
    Some((marker, indent, rest.to_owned()))
}

/// Splits a list marker from the text that follows it.
fn marker_of(trimmed: &str) -> Option<(Marker, &str)> {
    for prefix in ["- ", "* ", "+ "] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            if let Some(after) = rest.strip_prefix("[ ] ") {
                return Some((Marker::Task(false), after));
            }
            if let Some(after) = rest
                .strip_prefix("[x] ")
                .or_else(|| rest.strip_prefix("[X] "))
            {
                return Some((Marker::Task(true), after));
            }
            return Some((Marker::Bullet, rest));
        }
    }
    let digits = trimmed.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 || digits > 4 {
        return None;
    }
    let rest = trimmed.get(digits..)?;
    let after = rest.strip_prefix(". ")?;
    let number = trimmed.get(..digits)?.parse::<u32>().ok()?;
    Some((Marker::Ordered(number), after))
}

/// Matches a whole-line image and returns its alt text and path.
fn image_line(trimmed: &str) -> Option<(String, String)> {
    let rest = trimmed.strip_prefix("![")?;
    let close = rest.find(']')?;
    let alt = rest.get(..close)?.to_owned();
    let after = rest.get(close.saturating_add(1)..)?;
    let inner = after.strip_prefix('(')?;
    let end = inner.find(')')?;
    let path = inner.get(..end)?.trim();
    let trailing = inner.get(end.saturating_add(1)..)?;
    if path.is_empty() || !trailing.trim().is_empty() {
        return None;
    }
    Some((alt, path.to_owned()))
}

/// The length of the run of `marker` at the start of `text`.
fn run_of(text: &str, marker: char) -> usize {
    text.chars()
        .take_while(|character| *character == marker)
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_heading_keeps_its_level_and_text() {
        assert_eq!(
            parse("## A title"),
            vec![Block::Heading {
                level: 2,
                text: "A title".to_owned()
            }]
        );
    }

    #[test]
    fn a_hash_without_a_space_is_not_a_heading() {
        // `#tag` is a word, not a heading; requiring the space is what keeps it so.
        assert_eq!(parse("#tag"), vec![Block::Paragraph("#tag".to_owned())]);
    }

    #[test]
    fn an_unterminated_fence_still_yields_its_code() {
        // The streaming case: the closing fence has not arrived yet.
        assert_eq!(
            parse("```rust\nlet x = 1;"),
            vec![Block::Code {
                lang: "rust".to_owned(),
                code: "let x = 1;".to_owned()
            }]
        );
    }

    #[test]
    fn a_closed_fence_does_not_swallow_what_follows() {
        let blocks = parse("```\ncode\n```\nafter");
        assert_eq!(blocks.len(), 2, "{blocks:?}");
        assert!(matches!(blocks[0], Block::Code { .. }));
        assert_eq!(blocks[1], Block::Paragraph("after".to_owned()));
    }

    #[test]
    fn a_table_needs_a_separator_to_be_a_table() {
        assert!(matches!(
            parse("| a | b |\n| - | - |\n| 1 | 2 |").first(),
            Some(Block::Table { .. })
        ));
        // Without the separator the pipes are just characters in a paragraph.
        assert!(matches!(
            parse("| a | b |").first(),
            Some(Block::Paragraph(_))
        ));
    }

    #[test]
    fn a_table_row_is_padded_and_truncated_to_the_header() {
        let blocks = parse("| a | b |\n| - | - |\n| 1 |");
        let Some(Block::Table { rows, .. }) = blocks.first() else {
            return;
        };
        assert_eq!(rows, &vec![vec!["1".to_owned(), String::new()]]);
    }

    #[test]
    fn task_items_are_distinguished_from_bullets() {
        assert_eq!(
            parse("- [x] done")[0],
            Block::Item {
                marker: Marker::Task(true),
                indent: 0,
                text: "done".to_owned()
            }
        );
        assert_eq!(
            parse("- [ ] todo")[0],
            Block::Item {
                marker: Marker::Task(false),
                indent: 0,
                text: "todo".to_owned()
            }
        );
    }

    #[test]
    fn indentation_becomes_a_nesting_level() {
        assert_eq!(
            parse("    - nested")[0],
            Block::Item {
                marker: Marker::Bullet,
                indent: 2,
                text: "nested".to_owned()
            }
        );
    }

    #[test]
    fn ordered_items_keep_their_number() {
        assert_eq!(
            parse("3. third")[0],
            Block::Item {
                marker: Marker::Ordered(3),
                indent: 0,
                text: "third".to_owned()
            }
        );
    }

    #[test]
    fn a_rule_is_not_a_list_item() {
        assert_eq!(parse("---"), vec![Block::Rule]);
        assert_eq!(parse("***"), vec![Block::Rule]);
    }

    #[test]
    fn nested_quotes_count_their_depth() {
        assert_eq!(
            parse("> one\n> > two"),
            vec![Block::Quote {
                lines: vec![(1, "one".to_owned()), (2, "two".to_owned())]
            }]
        );
    }

    #[test]
    fn frontmatter_is_stripped_but_only_when_it_terminates() {
        assert_eq!(
            parse("+++\ntitle = \"x\"\n+++\n# Body"),
            vec![Block::Heading {
                level: 1,
                text: "Body".to_owned()
            }]
        );
        // An unterminated `+++` is not frontmatter; the text stays.
        assert_eq!(
            parse("+++\ntitle = \"x\""),
            vec![Block::Paragraph("+++\ntitle = \"x\"".to_owned())]
        );
    }

    #[test]
    fn paragraphs_break_on_a_blank_line() {
        assert_eq!(
            parse("one\ntwo\n\nthree"),
            vec![
                Block::Paragraph("one\ntwo".to_owned()),
                Block::Blank,
                Block::Paragraph("three".to_owned())
            ]
        );
    }

    #[test]
    fn an_image_is_recognised_only_as_a_whole_line() {
        assert_eq!(
            parse("![alt](a.png)")[0],
            Block::Image {
                alt: "alt".to_owned(),
                path: "a.png".to_owned()
            }
        );
        assert!(matches!(
            parse("see ![alt](a.png) here")[0],
            Block::Paragraph(_)
        ));
    }

    #[test]
    fn control_characters_are_removed_at_the_boundary() {
        // An escape sequence must not survive into a drawn line.
        let blocks = parse("hi\u{1b}[31mred");
        let Block::Paragraph(text) = &blocks[0] else {
            return;
        };
        assert!(!text.contains('\u{1b}'), "{text:?}");
        assert!(text.contains("hi"), "{text:?}");
        assert!(text.contains("[31mred"), "{text:?}");
    }
}
