//! The transcript's layout, kept between frames.
//!
//! Drawing a frame used to render every entry of the conversation — markdown included — and
//! then throw the result away, and to do it twice: once to clamp the scroll offset and once to
//! draw. A streamed token did it a third time, to find the bottom. The cost of every frame was
//! the cost of the whole conversation, so a long session drew slowly, and slower with every
//! turn, however little of it was on screen.
//!
//! This keeps what a frame needs to *place* the conversation — how many rows each rendered line
//! takes — for every block, and keeps the rendered lines themselves only for the blocks that
//! were last on screen. A block is rendered when it first appears or changes, which an entry's
//! stamp says without reading its text; everything else about a frame is arithmetic on the rows
//! already measured.
//!
//! The rendering itself is not here. A block is drawn by the view, by the same code that drew
//! the whole transcript before, so the cached form and a fresh rendering cannot differ: this
//! module only decides *when* that code runs and remembers what it produced.

// The module is private, so `pub(crate)` and `pub` are the same reachability; the explicit
// `pub(crate)` says the surface is the view's, as the markdown module's does for the same reason.
#![allow(clippy::redundant_pub_crate)]

use ratatui::text::Line;

use crate::compact::{self, Detail};
use crate::transcript::Entry;
use crate::view::Theme;

/// What a block of the transcript is drawn as.
///
/// The variants are the branches of the transcript's rendering, so a block is the unit that
/// rendering produces: one entry, or a run of them that is drawn as one line.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Piece {
    /// A run of tool entries, collapsed to one summary line.
    ToolRun,
    /// A run of reasoning entries, collapsed to one summary line.
    ReasoningRun,
    /// A tool call in the compact form: one line, carrying how the call ended.
    ///
    /// The state is part of the piece because it comes from *another* entry — the result —
    /// so the call's own stamp cannot say when its line has to change.
    CompactCall(compact::ToolState),
    /// A result that answers a call, in the compact form: only what the tool said.
    CompactResult,
    /// A thinking segment in the compact form: its newest line.
    CompactThinking,
    /// An entry drawn whole, under its heading.
    Whole,
    /// The settled head of an answer that is still streaming: its heading and the markdown up
    /// to the byte offset, which renders the same however the answer goes on.
    ///
    /// Matched by the entry's identity rather than its stamp, since the stamp moves with every
    /// token and the head does not: an entry's text only grows at its end, so the same entry up
    /// to the same offset is the same text.
    StreamHead(usize),
    /// A settled part of an answer that is still streaming, between two byte offsets. Matched
    /// by identity, as the head is.
    StreamChunk(usize, usize),
    /// The rest of an answer that is still streaming, from the byte offset: the part a token
    /// changes, with the cursor and the row that separates it from the next entry.
    StreamTail(usize),
}

impl Piece {
    /// What a block drawn as this piece remembers of an entry to know it is unchanged.
    fn token(self, entry: &Entry) -> u64 {
        match self {
            Self::StreamHead(_) | Self::StreamChunk(..) => entry.id(),
            Self::ToolRun
            | Self::ReasoningRun
            | Self::CompactCall(_)
            | Self::CompactResult
            | Self::CompactThinking
            | Self::Whole
            | Self::StreamTail(_) => entry.stamp(),
        }
    }
}

/// Everything outside the entries that decides how they are drawn.
///
/// Compared on every refresh, and a difference discards the layout: a resize re-wraps every
/// line, and a toggle of a display preference re-renders every block. Both are a reader's
/// deliberate act rather than something that happens per token.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct LayoutKey {
    /// The width the lines are wrapped and clipped to.
    pub(crate) width: u16,
    /// Compact or full detail.
    pub(crate) detail: Detail,
    /// Whether answers are rendered as markdown.
    pub(crate) markdown: bool,
    /// Whether `mermaid` fences are drawn as diagrams.
    pub(crate) mermaid: bool,
    /// Whether runs of tool activity collapse to a line.
    pub(crate) collapse_tools: bool,
    /// Whether runs of reasoning collapse to a line.
    pub(crate) collapse_reasoning: bool,
    /// The colours every line is styled with.
    pub(crate) theme: Theme,
}

/// What a block remembers of its entries to know they are unchanged: one token per entry.
///
/// Nearly every block is one entry, so that case is held inline rather than as a slice on the
/// heap: a long conversation has a block for every entry it holds.
#[derive(Debug)]
enum Tokens {
    /// A block of one entry.
    One(u64),
    /// A run of entries drawn as one line.
    Many(Box<[u64]>),
}

impl Tokens {
    fn of(piece: Piece, run: &[Entry]) -> Self {
        match run {
            [entry] => Self::One(piece.token(entry)),
            _ => Self::Many(run.iter().map(|entry| piece.token(entry)).collect()),
        }
    }

    fn as_slice(&self) -> &[u64] {
        match self {
            Self::One(token) => core::slice::from_ref(token),
            Self::Many(tokens) => tokens,
        }
    }
}

/// One block: what it was drawn from, how many lines it drew, and the lines themselves while it
/// is on screen. Its lines' heights live in the layout, with everyone else's.
#[derive(Debug)]
pub(crate) struct LayoutBlock {
    /// What the block is drawn as.
    pub(crate) piece: Piece,
    /// The index of its first entry.
    pub(crate) start: usize,
    /// What it was rendered from ([`Piece::token`]).
    tokens: Tokens,
    /// How many lines it renders to.
    len: usize,
    /// The rendered lines, kept only while the block is on screen.
    lines: Option<Box<[Line<'static>]>>,
}

impl LayoutBlock {
    /// A block holding a fresh rendering of `run`.
    pub(crate) fn new(
        piece: Piece,
        start: usize,
        run: &[Entry],
        lines: Vec<Line<'static>>,
    ) -> Self {
        Self {
            piece,
            start,
            tokens: Tokens::of(piece, run),
            len: lines.len(),
            lines: Some(lines.into_boxed_slice()),
        }
    }

    /// Whether this block is still the rendering of `run` drawn as `piece`.
    pub(crate) fn matches(&self, piece: Piece, start: usize, run: &[Entry]) -> bool {
        let tokens = self.tokens.as_slice();
        self.piece == piece
            && self.start == start
            && tokens.len() == run.len()
            && tokens
                .iter()
                .zip(run)
                .all(|(token, entry)| *token == piece.token(entry))
    }

    /// The index one past its last entry.
    pub(crate) fn end(&self) -> usize {
        self.start.saturating_add(self.tokens.as_slice().len())
    }

    /// How many lines it renders to.
    pub(crate) const fn len(&self) -> usize {
        self.len
    }

    /// Its rendered lines, if it still holds them.
    pub(crate) fn lines(&self) -> Option<&[Line<'static>]> {
        self.lines.as_deref()
    }

    /// Gives it its lines back, after a re-rendering that must match what was measured.
    pub(crate) fn restore(&mut self, lines: Vec<Line<'static>>) {
        assert_eq!(lines.len(), self.len, "a re-rendering matches its measure");
        self.lines = Some(lines.into_boxed_slice());
    }

    /// Lets go of its lines, keeping what it measured.
    pub(crate) fn release(&mut self) {
        self.lines = None;
    }
}

/// The measured transcript: every block's rows, and the lines of the blocks on screen.
///
/// Lines are addressed by their index in the whole transcript, as they were when the transcript
/// was one list of lines, because that is the coordinate a selection and the scroll arithmetic
/// are written in.
#[derive(Debug, Default)]
pub(crate) struct TranscriptLayout {
    /// The conditions the blocks were drawn under, or `None` before the first refresh.
    key: Option<LayoutKey>,
    /// The blocks, in transcript order.
    pub(crate) blocks: Vec<LayoutBlock>,
    /// Every line's display rows, in transcript order: the only copy.
    heights: Vec<u32>,
    /// The index of each block's first line.
    starts: Vec<usize>,
    /// The sum of `heights`, kept as they change.
    rows: u32,
}

impl TranscriptLayout {
    /// Starts over when the conditions have changed, and records the new ones.
    pub(crate) fn key(&mut self, key: LayoutKey) {
        if self.key != Some(key) {
            *self = Self {
                key: Some(key),
                ..Self::default()
            };
        }
    }

    /// Every line's display rows.
    pub(crate) fn heights(&self) -> &[u32] {
        &self.heights
    }

    /// The display rows of the whole transcript.
    pub(crate) const fn rows(&self) -> u32 {
        self.rows
    }

    /// How many lines the whole transcript renders to.
    pub(crate) fn line_count(&self) -> usize {
        self.heights.len()
    }

    /// The block that holds line `line`, if there is such a line.
    pub(crate) fn block_of(&self, line: usize) -> Option<usize> {
        if line >= self.heights.len() {
            return None;
        }
        // The last block whose first line is at or before `line`. Blocks with no lines share a
        // start with the block after them, and the partition point steps past them both, so the
        // block found is the one that actually holds the line.
        let after = self.starts.partition_point(|start| *start <= line);
        after.checked_sub(1)
    }

    /// The index of block `block`'s first line.
    pub(crate) fn start_of(&self, block: usize) -> usize {
        self.starts
            .get(block)
            .copied()
            .unwrap_or(self.heights.len())
    }

    /// Puts `block`, whose lines take `heights` rows each, at `position`: in place of the block
    /// there, or after the last.
    ///
    /// The heights are spliced into the one list of them, so a block that changes at the tail —
    /// a streamed token — moves nothing before it, and the line index of the blocks after it is
    /// moved by the difference straight away.
    pub(crate) fn place(&mut self, position: usize, block: LayoutBlock, heights: &[u32]) {
        assert_eq!(block.len(), heights.len(), "one height per line");
        assert!(position <= self.blocks.len(), "blocks are placed in order");
        let added = sum(heights);
        if let Some(old) = self.blocks.get(position) {
            let (old_len, new_len) = (old.len(), block.len());
            let from = self.start_of(position);
            let to = from.saturating_add(old_len).min(self.heights.len());
            let removed = sum(self.heights.get(from..to).unwrap_or_default());
            self.heights.splice(from..to, heights.iter().copied());
            self.rows = self.rows.saturating_sub(removed).saturating_add(added);
            self.blocks[position] = block;
            // The lines after it moved by the difference, and a block placed later in the same
            // refresh finds its lines through these starts, so they are put right now rather than
            // at the end.
            if new_len != old_len {
                for start in self.starts.iter_mut().skip(position.saturating_add(1)) {
                    *start = start.saturating_sub(old_len).saturating_add(new_len);
                }
            }
        } else {
            self.starts.push(self.heights.len());
            self.heights.extend_from_slice(heights);
            self.rows = self.rows.saturating_add(added);
            self.blocks.push(block);
        }
    }

    /// Drops every block from `count` on.
    pub(crate) fn truncate(&mut self, count: usize) {
        if count >= self.blocks.len() {
            return;
        }
        let kept = self.start_of(count).min(self.heights.len());
        let removed = sum(self.heights.get(kept..).unwrap_or_default());
        self.heights.truncate(kept);
        self.rows = self.rows.saturating_sub(removed);
        self.blocks.truncate(count);
        self.starts.truncate(count);
    }

    /// Checks the line index from block `from` onwards, after the blocks there changed.
    ///
    /// [`TranscriptLayout::place`] and [`TranscriptLayout::truncate`] keep the index right as
    /// they go; this states that they did, over the blocks a refresh touched.
    pub(crate) fn reflow(&self, from: usize) {
        let mut next = self.start_of(from.min(self.blocks.len()));
        for (index, block) in self.blocks.iter().enumerate().skip(from) {
            assert_eq!(
                self.starts.get(index).copied(),
                Some(next),
                "block {index} starts here"
            );
            next = next.saturating_add(block.len());
        }
        // Postcondition: one start per block, and every line is counted once.
        assert_eq!(self.starts.len(), self.blocks.len());
        assert_eq!(
            next,
            self.heights.len(),
            "the blocks' lines are the heights' lines"
        );
    }

    /// Lets go of the lines of every block outside `keep`, a range of block indices.
    ///
    /// What is left is a few numbers per line for the whole conversation and the lines of what
    /// is on screen, rather than every line the conversation renders to.
    pub(crate) fn release_outside(&mut self, keep: core::ops::Range<usize>) {
        for (index, block) in self.blocks.iter_mut().enumerate() {
            if !keep.contains(&index) {
                block.release();
            }
        }
    }
}

/// The total of some heights, saturating.
fn sum(heights: &[u32]) -> u32 {
    heights.iter().copied().fold(0_u32, u32::saturating_add)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::Role;

    fn block(start: usize, lines: usize) -> (LayoutBlock, Vec<u32>) {
        let entries = [Entry::prose(Role::User, "x")];
        let block = LayoutBlock::new(Piece::Whole, start, &entries, vec![Line::from("x"); lines]);
        (block, vec![1; lines])
    }

    fn layout(sizes: &[usize]) -> TranscriptLayout {
        let mut layout = TranscriptLayout::default();
        for (index, lines) in sizes.iter().enumerate() {
            let (block, heights) = block(index, *lines);
            layout.place(index, block, &heights);
        }
        layout.reflow(0);
        layout
    }

    #[test]
    fn a_line_is_found_in_the_block_that_holds_it() {
        let layout = layout(&[2, 3]);
        assert_eq!(layout.block_of(0), Some(0));
        assert_eq!(layout.block_of(1), Some(0));
        assert_eq!(layout.block_of(2), Some(1));
        assert_eq!(layout.block_of(4), Some(1));
        assert_eq!(layout.block_of(5), None, "past the last line is no block");
    }

    #[test]
    fn a_block_with_no_lines_never_holds_one() {
        // A result whose call already carries its mark renders to nothing; the line after it
        // belongs to the next block, not to the empty one that shares its start.
        let layout = layout(&[2, 0, 1]);
        assert_eq!(layout.block_of(1), Some(0));
        assert_eq!(layout.block_of(2), Some(2));
    }

    #[test]
    fn two_blocks_that_change_length_in_one_refresh_find_their_own_lines() {
        let mut layout = layout(&[2, 3, 1, 4]);
        let (longer, heights) = block(1, 5);
        layout.place(1, longer, &heights);
        let (shorter, heights) = block(3, 1);
        layout.place(3, shorter, &heights);
        layout.reflow(1);
        assert_eq!(layout.line_count(), 9);
        assert_eq!(layout.start_of(3), 8);
        assert_eq!(layout.rows(), 9);
    }

    #[test]
    fn a_replaced_block_moves_the_lines_after_it_and_counts_its_own() {
        let mut layout = layout(&[2, 3, 1]);
        assert_eq!(layout.rows(), 6);
        let (shorter, heights) = block(1, 1);
        layout.place(1, shorter, &heights);
        layout.reflow(1);
        assert_eq!(layout.rows(), 4);
        assert_eq!(layout.line_count(), 4);
        assert_eq!(layout.start_of(2), 3, "the last block's lines moved up");
        assert_eq!(layout.block_of(3), Some(2));
    }

    #[test]
    fn truncating_drops_the_blocks_and_their_rows() {
        let mut layout = layout(&[2, 3, 1]);
        layout.truncate(1);
        layout.reflow(1);
        assert_eq!(layout.rows(), 2);
        assert_eq!(layout.blocks.len(), 1);
        assert_eq!(layout.block_of(2), None);
    }

    #[test]
    fn a_changed_entry_no_longer_matches_its_block() {
        let mut entries = vec![Entry::streaming(Role::Assistant)];
        let block = LayoutBlock::new(Piece::Whole, 0, &entries, Vec::new());
        assert!(block.matches(Piece::Whole, 0, &entries));
        entries[0].push_str("more", false);
        assert!(!block.matches(Piece::Whole, 0, &entries), "a new stamp");
        assert!(!block.matches(Piece::CompactThinking, 0, &entries[..0]));
    }

    #[test]
    fn a_run_matches_only_the_same_entries() {
        let entries = vec![Entry::notice("a"), Entry::notice("b")];
        let run = LayoutBlock::new(Piece::ToolRun, 0, &entries, Vec::new());
        assert!(run.matches(Piece::ToolRun, 0, &entries));
        assert!(
            !run.matches(Piece::ToolRun, 0, &entries[..1]),
            "a shorter run"
        );
        assert_eq!(run.end(), 2);
    }

    #[test]
    fn a_streaming_head_survives_its_entry_growing_and_not_being_replaced() {
        let mut entries = vec![Entry::streaming(Role::Assistant)];
        entries[0].push_str("settled\n\n", false);
        let head = Piece::StreamHead(9);
        let block = LayoutBlock::new(head, 0, &entries, Vec::new());
        entries[0].push_str("more", false);
        assert!(block.matches(head, 0, &entries), "the same entry, grown");
        let copy = entries[0].clone();
        assert!(
            !block.matches(head, 0, &[copy]),
            "a clone may grow differently"
        );
        assert!(
            !block.matches(Piece::StreamHead(12), 0, &entries),
            "a later cut"
        );
    }

    #[test]
    fn released_lines_leave_the_measurements_behind() {
        let mut layout = layout(&[2, 3, 1]);
        layout.release_outside(1..2);
        assert!(layout.blocks[0].lines().is_none());
        assert!(layout.blocks[1].lines().is_some());
        assert!(layout.blocks[2].lines().is_none());
        assert_eq!(layout.rows(), 6, "rows survive the release");
    }

    #[test]
    fn new_conditions_discard_the_layout_and_the_same_ones_keep_it() {
        let key = LayoutKey {
            width: 80,
            detail: Detail::Compact,
            markdown: true,
            mermaid: true,
            collapse_tools: false,
            collapse_reasoning: false,
            theme: Theme::default(),
        };
        let mut layout = layout(&[2]);
        layout.key(key);
        assert!(
            layout.blocks.is_empty(),
            "the first key starts from nothing"
        );
        let (kept, heights) = block(0, 2);
        layout.place(0, kept, &heights);
        layout.reflow(0);
        layout.key(key);
        assert_eq!(
            layout.blocks.len(),
            1,
            "the same conditions keep the blocks"
        );
        layout.key(LayoutKey { width: 81, ..key });
        assert!(layout.blocks.is_empty(), "a resize re-wraps everything");
    }
}
