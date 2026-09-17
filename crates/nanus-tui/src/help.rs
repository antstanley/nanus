//! The key list, and the rows the overlay draws it as.
//!
//! The table lives here rather than only in `docs/tui.md` because the interface has to be
//! able to *show* it: a reader who does not know a binding exists has no way to find it, and
//! the documentation is not on the screen they are looking at. The overlay, the `/help`
//! command, and the tests all read this one list, so a binding added to the interface is
//! added in one place.
//!
//! Every string here is ASCII, which is what lets the layout count characters rather than
//! columns; there is a test that fails if that stops being true.

// The module is private, so `pub(crate)` and `pub` are the same reachability; the explicit
// `pub(crate)` says which surface these items are meant for, and this is the lint's
// counterpart. `markdown/mod.rs` carries the same allow for the same reason.
#![allow(clippy::redundant_pub_crate)]

/// Every binding, as its keys and what they do.
///
/// An empty key field is a group heading rather than a binding: the approval keys are a
/// different keyboard from the rest, because while that dialog is up nothing else reaches
/// the composer.
pub(crate) const KEYS: &[(&str, &str)] = &[
    ("Enter", "submit"),
    (
        "\\ + Enter",
        "newline: the escape hatch that needs no terminal cooperation",
    ),
    ("Alt+Enter / Shift+Enter / Ctrl+J", "newline"),
    (
        "Up / Down",
        "move between lines, then browse submitted prompts",
    ),
    ("Left / Right, Home / End", "move the cursor"),
    (
        "PageUp / PageDown",
        "scroll back and forward through the conversation",
    ),
    (
        "Ctrl+C / Esc",
        "stop the running turn; then cancel the prompt; then quit",
    ),
    ("Ctrl+D", "quit"),
    ("Ctrl+R", "reverse-search submitted prompts"),
    (
        "Ctrl+Q",
        "open the queue of prompts waiting for the turn to end",
    ),
    (
        "Ctrl+O",
        "switch between the one-line and the whole form of a tool call",
    ),
    ("Ctrl+T", "summarise runs of tool calls"),
    ("Ctrl+E", "summarise runs of reasoning"),
    ("Ctrl+L", "clear the transcript"),
    ("Ctrl+K", "delete to the end of the line"),
    ("Ctrl+U", "delete the line"),
    ("Ctrl+Y", "put back what Ctrl+K or Ctrl+U deleted"),
    ("Ctrl+W", "delete the previous word"),
    ("Alt+B / Alt+F", "move the cursor a word back or forward"),
    ("Shift+Tab", "choose the approval state, from anywhere"),
    ("Alt+P", "switch to the next model the agent offers"),
    ("Alt+T", "ask for the next step of reasoning effort"),
    ("Ctrl+V", "paste an image from the clipboard, as a path"),
    ("@path", "name a file; Tab completes it"),
    ("?", "show this list, when the prompt is empty"),
    ("", "while an approval dialog is up"),
    ("y", "allow the call once"),
    ("a", "allow it, and record the tool for the session"),
    ("n / Esc", "deny it"),
    ("Ctrl+C", "deny it and stop the turn"),
];

/// What the overlay is titled.
pub(crate) const TITLE: &str = " keys ";

/// What the overlay says under its last row.
pub(crate) const FOOTER: &str = " Esc closes, Up and Down scroll ";

/// One row of the overlay.
pub(crate) enum Row {
    /// A group heading, drawn without a key.
    Heading(String),
    /// A binding: the keys in one column, what they do in the other.
    Keys {
        /// The keys, padded to the column width.
        key: String,
        /// What they do, cut to the room that is left.
        effect: String,
    },
}

/// Lays the list out for a dialog `width` columns wide, from `offset`, for `rows` rows.
///
/// The window is the caller's because only the caller knows how tall the terminal is; the
/// offset is clamped here, so a scroll past the end lands on the last page rather than on
/// blank rows.
#[must_use]
pub(crate) fn rows(offset: usize, rows: usize, width: usize) -> Vec<Row> {
    let key_width = key_width(width);
    let total = KEYS.len();
    let offset = offset.min(total.saturating_sub(rows));
    KEYS.iter()
        .skip(offset)
        .take(rows)
        .map(|(keys, effect)| shape(keys, effect, key_width, width))
        .collect()
}

/// How many rows the list needs, at any width.
///
/// Every binding is one row whatever the width, because a row is cut rather than wrapped:
/// wrapping one would make the caller's row budget a guess.
#[must_use]
pub(crate) fn height() -> usize {
    KEYS.len()
}

/// Lays one entry out.
fn shape(keys: &str, effect: &str, key_width: usize, width: usize) -> Row {
    if keys.is_empty() {
        return Row::Heading(clip(effect, width));
    }
    let room = width.saturating_sub(key_width).saturating_sub(2);
    Row::Keys {
        key: pad(keys, key_width),
        effect: clip(effect, room),
    }
}

/// The width of the key column: the longest key, squeezed to leave room for the effect.
///
/// A terminal too narrow for both columns loses part of the longest key rather than every
/// effect: the key is the part a reader came for, and a truncated one is still a key.
fn key_width(width: usize) -> usize {
    let longest = KEYS
        .iter()
        .map(|(keys, _)| keys.chars().count())
        .max()
        .unwrap_or(0);
    longest.min(width.saturating_sub(16))
}

/// Pads `text` to `width` characters, cutting it when it is longer.
fn pad(text: &str, width: usize) -> String {
    let mut out: String = text.chars().take(width).collect();
    let missing = width.saturating_sub(out.chars().count());
    out.push_str(&" ".repeat(missing));
    out
}

/// Cuts `text` to `width` characters, ending with an ellipsis when anything was lost.
///
/// Characters rather than columns, which is honest because the whole table is ASCII — the
/// test below is what keeps that true.
fn clip(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }
    let keep = width.saturating_sub(1);
    let mut out: String = text.chars().take(keep).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The layout counts characters, so the table has to be one character per column.
    #[test]
    fn the_table_is_ascii() {
        for (keys, effect) in KEYS {
            assert!(keys.is_ascii(), "{keys} is not ASCII");
            assert!(effect.is_ascii(), "{effect} is not ASCII");
        }
        assert!(TITLE.is_ascii() && FOOTER.is_ascii());
    }

    #[test]
    fn every_row_fits_the_width_it_is_given() {
        for width in 8..=100_usize {
            for row in rows(0, height(), width) {
                match row {
                    Row::Heading(text) => assert!(
                        text.chars().count() <= width,
                        "width {width}: heading {text:?} is too wide"
                    ),
                    Row::Keys { key, effect } => assert!(
                        key.chars().count().saturating_add(effect.chars().count()) <= width,
                        "width {width}: {key:?} {effect:?} is too wide"
                    ),
                }
            }
        }
    }

    /// The window is a run of the table, and a scroll past the end lands on the last page
    /// rather than on blank rows.
    #[test]
    fn the_window_is_a_contiguous_run_of_the_table() {
        let all = rows(0, height(), 60);
        assert_eq!(all.len(), KEYS.len());
        let last = rows(height(), 4, 60);
        assert_eq!(last.len(), 4);
        assert!(matches!(last.last(), Some(Row::Keys { key, .. }) if key.trim() == "Ctrl+C"));
    }

    /// A row that names no keys is a group heading rather than a binding.
    #[test]
    fn a_row_with_no_keys_is_a_heading() {
        let drawn = rows(0, height(), 60);
        assert_eq!(
            drawn
                .iter()
                .filter(|row| matches!(row, Row::Heading(_)))
                .count(),
            1
        );
    }

    #[test]
    fn a_long_effect_is_cut_rather_than_wrapped() {
        let row = shape("Enter", &"x".repeat(200), 10, 30);
        let Row::Keys { effect, .. } = row else {
            panic!("a keyed row");
        };
        assert!(effect.ends_with('…'), "{effect:?}");
        assert_eq!(effect.chars().count(), 18);
    }
}
