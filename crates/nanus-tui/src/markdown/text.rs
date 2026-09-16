//! Display width, and the integer arithmetic that goes with it.
//!
//! The markdown renderer wraps text to a column budget, so "how wide is this" is a
//! question about the terminal rather than about the string: a CJK ideograph or a
//! wide emoji occupies two columns, a combining mark none. `unicode-width` is the
//! same crate ratatui itself measures with, which is what lets a line this module
//! decides "fits" actually fit when the widget draws it.
//!
//! Every function here is small and total. The workspace forbids wrapping arithmetic
//! on integers, so the few divisions are checked and the sums saturate; nothing in a
//! renderer should be able to panic on a hostile line of model output.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// The number of terminal columns one character occupies.
///
/// A control character measures zero here rather than one; the markdown front-end
/// sanitises those away before they reach a `Line`, and a measurement of zero keeps
/// a stray one from inflating a wrap decision.
pub(crate) fn char_width(character: char) -> usize {
    UnicodeWidthChar::width(character).unwrap_or(0)
}

/// The number of terminal columns a string occupies.
pub(crate) fn str_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// `part` as a whole-number percentage of `whole`, saturating at zero.
///
/// Whole numbers because the workspace forbids floating-point casts back to `usize`,
/// and a diagram legend does not need fractional percent. `whole == 0` reads as zero
/// rather than as a division failure.
pub(crate) fn percent(part: u64, whole: u64) -> u64 {
    part.saturating_mul(100).checked_div(whole).unwrap_or(0)
}

/// `part` scaled to `scale` whole units of `whole`, saturating at `scale`.
///
/// The bar-width arithmetic for a pie slice or a gantt task: a fraction has to land
/// on a whole number of cells without ever exceeding the space allotted to it.
pub(crate) fn scaled(part: u64, whole: u64, scale: u64) -> u64 {
    if whole == 0 {
        return 0;
    }
    part.saturating_mul(scale)
        .checked_div(whole)
        .unwrap_or(0)
        .min(scale)
}
