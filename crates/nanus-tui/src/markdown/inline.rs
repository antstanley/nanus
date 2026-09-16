//! Inline markdown: emphasis, inline code, and links.
//!
//! A hand-written scanner rather than a grammar, because the inline subset a coding
//! assistant emits is four constructs wide. The rule throughout is *fall back to the
//! literal text*: a `*` with no partner, an unclosed backtick, a `[` with no `]` —
//! each is drawn as itself. That is what makes the parser safe to run on a half-streamed
//! message, where every construct is temporarily unterminated.

use ratatui::style::{Modifier, Style};
use ratatui::text::Span;

use super::theme::MarkdownTheme;

/// Splits `text` into styled spans using the theme's body style.
pub(crate) fn spans(text: &str, theme: &MarkdownTheme) -> Vec<Span<'static>> {
    spans_with(text, theme.text, theme)
}

/// Splits `text` into spans, styling unstyled runs with `base`.
pub(crate) fn spans_with(text: &str, base: Style, theme: &MarkdownTheme) -> Vec<Span<'static>> {
    let chars: Vec<char> = text.chars().collect();
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut literal = String::new();
    let mut index = 0_usize;
    while index < chars.len() {
        let character = chars[index];
        // A backslash before a marker is that marker, literally.
        if let Some(next) = escaped(&chars, index) {
            literal.push(next);
            index = index.saturating_add(2);
            continue;
        }
        if character == '`'
            && let Some(end) = find_run(&chars, index.saturating_add(1), '`', 1)
        {
            flush(&mut out, &mut literal, base);
            let code: String = chars[index.saturating_add(1)..end].iter().collect();
            out.push(Span::styled(code.trim().to_owned(), theme.inline_code));
            index = end.saturating_add(1);
            continue;
        }
        if character == '['
            && let Some((label, url, next)) = link(&chars, index)
        {
            flush(&mut out, &mut literal, base);
            out.push(Span::styled(label, theme.link));
            if !url.is_empty() {
                out.push(Span::styled(format!(" ({url})"), theme.aside));
            }
            index = next;
            continue;
        }
        if let Some((style, content, next)) = emphasis(&chars, index, base) {
            flush(&mut out, &mut literal, base);
            out.push(Span::styled(content, style));
            index = next;
            continue;
        }
        literal.push(character);
        index = index.saturating_add(1);
    }
    flush(&mut out, &mut literal, base);
    out
}

/// Returns the character a backslash at `index` escapes, if it escapes one.
fn escaped(chars: &[char], index: usize) -> Option<char> {
    if chars.get(index) != Some(&'\\') {
        return None;
    }
    let next = *chars.get(index.saturating_add(1))?;
    matches!(next, '*' | '_' | '`' | '[' | ']' | '(' | ')' | '\\').then_some(next)
}

/// Matches `***x***`, `**x**` / `__x__`, or `*x*` / `_x_` at `index`.
fn emphasis(chars: &[char], index: usize, base: Style) -> Option<(Style, String, usize)> {
    let marker = *chars.get(index)?;
    if marker != '*' && marker != '_' {
        return None;
    }
    if !left_flanking(chars, index) {
        return None;
    }
    let open = run_length(chars, index, marker);
    // Longest first: `***` must win over `**`, which must win over `*`.
    for (count, modifier) in [
        (3_usize, Modifier::BOLD | Modifier::ITALIC),
        (2_usize, Modifier::BOLD),
        (1_usize, Modifier::ITALIC),
    ] {
        if open < count {
            continue;
        }
        let start = index.saturating_add(count);
        if let Some(end) = find_run(chars, start, marker, count)
            && end > start
        {
            let content: String = chars[start..end].iter().collect();
            if !content.trim().is_empty() {
                let next = end.saturating_add(count);
                return Some((base.add_modifier(modifier), content, next));
            }
        }
    }
    None
}

/// Matches `[label](destination)` at `index`.
fn link(chars: &[char], index: usize) -> Option<(String, String, usize)> {
    if chars.get(index) != Some(&'[') {
        return None;
    }
    let close = find_run(chars, index.saturating_add(1), ']', 1)?;
    let label: String = chars[index.saturating_add(1)..close].iter().collect();
    if chars.get(close.saturating_add(1)) != Some(&'(') {
        return None;
    }
    let end = find_run(chars, close.saturating_add(2), ')', 1)?;
    let url: String = chars[close.saturating_add(2)..end].iter().collect();
    Some((label, url.trim().to_owned(), end.saturating_add(1)))
}

/// The length of the run of `marker` starting at `index`.
fn run_length(chars: &[char], index: usize, marker: char) -> usize {
    let mut count = 0_usize;
    while chars.get(index.saturating_add(count)) == Some(&marker) {
        count = count.saturating_add(1);
    }
    count
}

/// The position of a run of exactly/at least `count` `marker`s at or after `start`.
fn find_run(chars: &[char], start: usize, marker: char, count: usize) -> Option<usize> {
    let mut index = start;
    while index < chars.len() {
        if chars[index] == marker {
            let run = run_length(chars, index, marker);
            if run >= count && (count > 1 || run == count) {
                return Some(index);
            }
            index = index.saturating_add(run);
        } else {
            index = index.saturating_add(1);
        }
    }
    None
}

/// Whether a delimiter at `index` can open emphasis.
///
/// `_` additionally has to start at a word boundary so that `snake_case` and
/// `a_b_c` are not mangled into italics, which is the one place `CommonMark`'s rules
/// are worth the effort here.
fn left_flanking(chars: &[char], index: usize) -> bool {
    if chars.get(index) == Some(&'_') {
        return index == 0
            || chars
                .get(index.saturating_sub(1))
                .is_none_or(|previous| !previous.is_alphanumeric());
    }
    true
}

/// Pushes the accumulated literal text, if any.
fn flush(out: &mut Vec<Span<'static>>, literal: &mut String, style: Style) {
    if !literal.is_empty() {
        out.push(Span::styled(std::mem::take(literal), style));
    }
}

/// Strips control characters and expands tabs.
///
/// A model's answer, or a file it read, can contain an escape sequence: a raw `ESC`
/// that reached a terminal could move the cursor, clear the screen, or worse. The
/// renderer's job is to draw text, so every control character except a newline is
/// removed here — at the single point where untrusted text enters the markdown path
/// — and a tab becomes four spaces so that the wrap arithmetic agrees with the drawing.
pub(crate) fn sanitize(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    for character in source.chars() {
        if character == '\t' {
            out.push_str("    ");
        } else if character == '\n' || !character.is_control() {
            out.push(character);
        }
    }
    out
}
