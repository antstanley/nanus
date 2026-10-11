//! The key list, and the rows the overlay draws it as.
//!
//! The table lives here rather than only in `docs/tui.md` because the interface has to be
//! able to *show* it: a reader who does not know a binding exists has no way to find it, and
//! the documentation is not on the screen they are looking at. The overlay, the `/help`
//! command, and the tests all read this one list, so a binding added to the interface is
//! added in one place.
//!
//! Bindings and explanations wrap by display columns in the shared dialog renderer; they are never
//! shortened to a two-column table that hides the end of a long instruction.

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
    ("Shift+arrows", "select text in the transcript"),
    (
        "Ctrl+C",
        "copy what is highlighted, on the transcript or anywhere else",
    ),
    (
        "Esc",
        "clear the selection; then stop the running turn; then cancel the prompt; then quit",
    ),
    ("Ctrl+Q / F9", "quit from every dialog"),
    ("Ctrl+D", "quit from every dialog"),
    (
        "F6 / Ctrl+Shift+C",
        "copy the full dialog, or the screen if no dialog is open",
    ),
    ("F7 / Ctrl+Shift+U", "copy the authorization URL"),
    ("F8 / Ctrl+Shift+Y", "copy the authorization code"),
    ("Ctrl+R", "reverse-search submitted prompts"),
    (
        "Ctrl+P",
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
    (
        "Shift+Tab",
        "open permissions from the composer or an approval",
    ),
    ("Alt+P", "switch to the next model the agent offers"),
    ("Alt+T", "ask for the next step of reasoning effort"),
    ("Ctrl+V", "paste an image from the clipboard, as a path"),
    ("@path", "name a file; Tab completes it"),
    ("!command", "run a shell command here, without the model"),
    (
        "/context",
        "choose context mode and review its impact before switching",
    ),
    (
        "/context status",
        "read context mode, readiness and selection counts",
    ),
    ("/copy", "put the newest answer on the clipboard"),
    ("?", "show this list, when the prompt is empty"),
    ("", "while an approval dialog is up"),
    ("y", "allow the call once"),
    (
        "a",
        "review a standing grant; after a pause y confirms it, any other key cancels",
    ),
    ("Tab / arrows", "select an approval answer (initially deny)"),
    ("Enter", "apply the selected approval answer"),
    (
        "PageUp / PageDown",
        "read the full dialog without changing its decision",
    ),
    ("n / Esc", "deny it"),
    (
        "any key, mid-typing",
        "nothing: an answer needs half a second of quiet first (Esc excepted)",
    ),
];

/// What the overlay is titled.
pub(crate) const TITLE: &str = " keys ";

/// What the overlay says under its last row.
pub(crate) const FOOTER: &str = "Esc closes · Up/Down scroll\nF6 copies this list";

#[cfg(test)]
mod tests {
    use super::*;

    /// Binding names stay ASCII so their terminal spellings remain unambiguous.
    #[test]
    fn the_table_is_ascii() {
        for (keys, effect) in KEYS {
            assert!(keys.is_ascii(), "{keys} is not ASCII");
            assert!(effect.is_ascii(), "{effect} is not ASCII");
        }
        assert!(TITLE.is_ascii());
    }
}
