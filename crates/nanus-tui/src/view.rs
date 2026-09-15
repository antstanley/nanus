//! Rendering: turning a transcript and a composer into terminal cells.
//!
//! The view is a pure function of [`ViewState`], which is what makes it testable
//! against ratatui's `TestBackend` without a terminal. Nothing here performs I/O.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::buffer::InputBuffer;
use crate::compact::{self, Detail};
use crate::stats::{Throughput, share, show, show_duration};
use crate::transcript::{Entry, EntryKind, Role, Transcript, wrap_count};

/// Colours and emphasis for each role.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Theme {
    /// The style applied to the human's input.
    pub user: Style,
    /// The style applied to the model's answer.
    pub assistant: Style,
    /// The style applied to the model's reasoning.
    pub reasoning: Style,
    /// The style applied to tool calls and results.
    pub tool: Style,
    /// The style applied to harness notices.
    pub notice: Style,
    /// The style applied to errors.
    pub error: Style,
    /// The style applied to an open turn's status.
    pub busy: Style,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            user: Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            // White rather than a colour: the answer is the thing a reader came for, and
            // the roles that are *not* the answer are the ones that should be marked. A
            // colour here competes with the transcript instead of settling it.
            assistant: Style::default().fg(Color::White),
            // Reasoning is dimmed deliberately: it is not the answer, and a reader
            // scanning for the answer should be able to skip it.
            reasoning: Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
            tool: Style::default().fg(Color::Yellow),
            notice: Style::default().fg(Color::Blue),
            error: Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            busy: Style::default().fg(Color::Magenta),
        }
    }
}

impl Theme {
    /// Returns the same theme with every colour removed, for a terminal that asked for
    /// none.
    ///
    /// `NO_COLOR` is honoured by not *asking* for colour, rather than by asking and
    /// letting the backend drop it. The backend writes a colour change as one command
    /// covering the foreground *and* the background, and when crossterm is suppressing
    /// colour that command degenerates to a bare `ESC[;m`, which is not "no colour" but a
    /// full SGR reset — so it clears the attributes set for the same cell just before it.
    /// The caret is a reversed cell, so under a colour theme it was the caret that
    /// vanished whenever it landed on a coloured prompt prefix. A theme with no colour in
    /// it has no colour command to degenerate, and every modifier survives.
    ///
    /// The modifiers are kept exactly as the colour theme has them: this is that theme
    /// with its colours taken out, not a second design. Roles that were told apart only
    /// by colour are no longer told apart, which is what asking for no colour means.
    #[must_use]
    pub const fn monochrome() -> Self {
        Self {
            user: Style::new().add_modifier(Modifier::BOLD),
            assistant: Style::new(),
            reasoning: Style::new().add_modifier(Modifier::ITALIC),
            tool: Style::new(),
            notice: Style::new(),
            error: Style::new().add_modifier(Modifier::BOLD),
            busy: Style::new(),
        }
    }

    /// Returns the style for a role.
    #[must_use]
    pub const fn style_for(&self, role: Role) -> Style {
        match role {
            Role::User => self.user,
            Role::Assistant => self.assistant,
            Role::Reasoning => self.reasoning,
            Role::Tool => self.tool,
            Role::Harness => self.notice,
        }
    }

    /// Returns the style for an entry, accounting for failure.
    #[must_use]
    pub const fn style_for_entry(&self, entry: &Entry) -> Style {
        if let EntryKind::ToolResult { is_error: true, .. } = entry.kind() {
            return self.error;
        }
        self.style_for(entry.role())
    }
}

/// How many rows the composer grows to before it scrolls to follow the cursor.
///
/// A prompt longer than this stays editable; the window moves so the line being
/// typed is the one on screen.
const MAX_COMPOSER_ROWS: u16 = 5;

/// The rows the composer's border adds around its text.
const COMPOSER_BORDER_ROWS: u16 = 2;

/// The rows the transcript keeps whatever else wants them.
const TRANSCRIPT_FLOOR: u16 = 3;

/// The columns the composer's border adds around its text.
const COMPOSER_BORDER_COLS: u16 = 2;

/// What the interface is currently showing.
///
/// ## The scroll convention
///
/// `scroll_offset` is a count of rows scrolled **off the top**: zero shows the
/// beginning of the conversation, and the maximum shows its end. It is deliberately
/// *not* bottom-relative, because a bottom-relative offset would silently change
/// meaning every time a token arrived — a reader who had scrolled up to study
/// something would be dragged along by the model's next sentence.
///
/// The consequence is that a conversation longer than the viewport opens at its
/// beginning, and following the newest output means asking for the bottom
/// ([`ViewState::scroll_to_bottom`]). The runtime calls that when a turn starts.
pub struct ViewState {
    /// The conversation.
    pub transcript: Transcript,
    /// The composer.
    pub input: InputBuffer,
    /// Rows scrolled off the top.
    pub scroll_offset: u32,
    /// Whether new output should drag the view to the bottom.
    ///
    /// `false` once the reader has scrolled away from the bottom, and `true` again when
    /// they scroll back to it. Without this a turn in progress is unreadable: every
    /// streamed token would yank the view down while somebody was reading further up.
    pub following: bool,
    /// `true` while a turn is open.
    pub busy: bool,
    /// The current step within the open turn.
    pub step: u32,
    /// Total tokens the session has used.
    pub tokens_used: u64,
    /// How fast the model has been going, for the line under the composer.
    pub stats: Throughput,
    /// What the model is currently doing, for the status line.
    pub status: String,
    /// Whether runs of tool calls are drawn as one summary line.
    pub collapse_tools: bool,
    /// Whether runs of model reasoning are drawn as one summary line.
    pub collapse_reasoning: bool,
    /// How much of a tool call and a thinking segment the transcript draws.
    ///
    /// [`Detail::Compact`] — the default — is one line each: which tool is doing what, and
    /// the newest line of the model's thinking. [`Detail::Full`] is the whole call,
    /// arguments and all, and the whole thinking segment, which is what the interface drew
    /// before the compact form existed and what `tui_detail = "full"` asks for. The
    /// setting lives in the configuration file rather than behind a key because it is a
    /// standing preference rather than something to toggle mid-turn.
    pub detail: Detail,
    /// What to call the session in the title bar, when the interface is in one.
    ///
    /// A live conversation is a conversation *with something*, and once sessions can be
    /// resumed that something has a name worth showing: two terminals can be attached to
    /// two different sessions, and a reader should be able to tell which is which without
    /// scrolling back to the first prompt.
    pub label: Option<String>,
    /// Styling.
    pub theme: Theme,
    /// Rows to scroll back from the end on the next render, if a caller asked for it.
    ///
    /// Deferred rather than applied immediately because "48 rows back from the end"
    /// cannot be honoured until the viewport is known: applying it before the first
    /// render leaves the offset above the maximum, and the next render clamps it back to
    /// the top.
    pub pending_scroll_back: Option<u32>,

    /// The transcript area the view last drew into, if it has drawn.
    ///
    /// Recorded because "the bottom" is not a constant: it depends on how many rows
    /// the transcript occupies and how many the viewport shows, so a caller cannot
    /// compute it without knowing the terminal size.
    last_viewport: Option<(u16, u16)>,
}

impl Default for ViewState {
    fn default() -> Self {
        Self {
            transcript: Transcript::new(),
            input: InputBuffer::new(),
            scroll_offset: 0,
            following: true,
            busy: false,
            step: 0,
            tokens_used: 0,
            stats: Throughput::default(),
            status: "ready".to_owned(),
            collapse_tools: false,
            collapse_reasoning: false,
            detail: Detail::default(),
            label: None,
            theme: Theme::default(),
            pending_scroll_back: None,
            last_viewport: None,
        }
    }
}

impl std::fmt::Debug for ViewState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ViewState")
            .field("transcript", &self.transcript)
            .field("input", &self.input)
            .field("scroll_offset", &self.scroll_offset)
            .field("following", &self.following)
            .field("busy", &self.busy)
            .field("step", &self.step)
            .field("tokens_used", &self.tokens_used)
            .finish_non_exhaustive()
    }
}

/// Splits `text` into one styled line per source line, indented by `depth`.
///
/// A `Line` holds spans, not paragraphs: an embedded newline inside a span is rendered
/// literally rather than breaking the line. Anything multi-line therefore has to be split
/// before it reaches the widget.
fn indented(text: &str, depth: usize, style: Style) -> Vec<Line<'static>> {
    let pad = " ".repeat(depth);
    let lines: Vec<Line<'static>> = text
        .split('\n')
        .map(|line| Line::from(Span::styled(format!("{pad}{line}"), style)))
        .collect();
    if lines.is_empty() {
        return vec![Line::from(Span::styled(pad, style))];
    }
    lines
}

/// The separator between two readings on the stats row.
const STATS_SEPARATOR: &str = "  \u{b7}  ";

impl ViewState {
    /// Creates an empty view.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adjusts the scroll offset by `rows`, clamping to the visible range.
    ///
    /// `rows` is a **delta on [`ViewState::scroll_offset`]**, which counts rows skipped
    /// from the top of the transcript: **positive moves toward the newest content** and
    /// negative moves back toward the oldest. A key handler therefore passes a negative
    /// delta for Page-Up and a positive one for Page-Down.
    ///
    /// This paragraph used to say the opposite, and the key handler believed it, so
    /// Page-Up scrolled *forward* — from the bottom of a conversation, where a live one
    /// always is, it clamped and appeared to do nothing at all.
    ///
    /// Scrolling past either end clamps, so the newest entry ends up flush with the
    /// bottom rather than the view scrolling into blank space.
    pub fn scroll_by(&mut self, rows: i32, viewport_height: u16, width: u16) {
        // The caller has just told us the viewport, so remember it: a scroll before
        // the first render then still leaves `scroll_to_bottom` computable.
        self.last_viewport = Some((width, viewport_height));
        let total = self.transcript_rows();
        let visible = u32::from(viewport_height);
        let maximum = i64::from(total.saturating_sub(visible));
        let current = i64::from(self.scroll_offset);
        // The delta is applied through a branch rather than as `current + rows`,
        // because adding a large negative number to an `i64` saturates at the
        // *numeric* minimum instead of at zero; `clamp` would then be handed an
        // already-wrapped value and the view would jump to the wrong end.
        let moved = if rows >= 0 {
            current.saturating_add(i64::from(rows))
        } else {
            current.saturating_sub(i64::from(rows.unsigned_abs()))
        }
        .clamp(0, maximum.max(0));
        let maximum_unsigned = u32::try_from(maximum.max(0)).unwrap_or(0);
        self.scroll_offset = u32::try_from(moved).unwrap_or(maximum_unsigned);
        // Following is a consequence of where the reader is, not a separate command:
        // scrolling away from the bottom stops new output dragging the view, and
        // scrolling back to it resumes. That makes Page-Down a way to catch up rather
        // than a key that has to be pressed after every turn.
        self.following = self.scroll_offset >= maximum_unsigned;
        // Postcondition: the offset never exceeds the scrollable range, so a render
        // cannot look past the end of the transcript.
        assert!(self.scroll_offset <= maximum_unsigned);
    }

    /// Follows the newest output, unless the reader has scrolled away from it.
    ///
    /// Everything that appends to the conversation calls this rather than
    /// [`ViewState::scroll_to_bottom`]: a reader who has scrolled up is reading
    /// something, and dragging them back down on the next streamed token is how a
    /// transcript becomes unreadable while a turn is running.
    pub fn follow(&mut self) {
        if self.following {
            self.scroll_to_bottom();
        }
    }

    /// Scrolls so the newest entry is visible.
    ///
    /// A no-op before the first render, because the offset that means "the bottom"
    /// depends on a viewport this view has not been given yet.
    pub fn scroll_to_bottom(&mut self) {
        // Set before the early return, so a caller that scrolls before the first render
        // still records the intent to follow.
        self.following = true;
        let Some((_width, height)) = self.last_viewport else {
            return;
        };
        let total = self.transcript_rows();
        self.scroll_offset = total.saturating_sub(u32::from(height));
    }

    /// Scrolls to the beginning of the conversation.
    pub fn scroll_to_top(&mut self) {
        self.scroll_offset = 0;
        // At the top by definition, so new output must not drag the reader away.
        self.following = false;
    }

    /// Switches between the one-line form and the whole of a tool call and a thinking
    /// segment.
    ///
    /// The same choice the configuration file makes with `tui_detail`, reachable without
    /// editing a file and restarting: which of the two a reader wants depends on what they
    /// are doing at that moment, not on how they started the interface.
    pub const fn toggle_detail(&mut self) {
        self.detail = match self.detail {
            Detail::Compact => Detail::Full,
            Detail::Full => Detail::Compact,
        };
    }

    /// Returns the largest valid scroll offset for the last drawn viewport.
    #[must_use]
    pub fn max_scroll(&self) -> u32 {
        let Some((_width, height)) = self.last_viewport else {
            return 0;
        };
        let rows = self.transcript_rows();
        rows.saturating_sub(u32::from(height))
    }

    /// Reduces the scroll offset when the transcript no longer fills the viewport.
    ///
    /// Streaming and resizing both change how many rows exist, so an offset that was
    /// valid when it was set can become greater than the scrollable range. Clamping
    /// here keeps the invariant at the point it can be broken.
    pub fn clamp_scroll(&mut self, viewport_height: u16, _width: u16) {
        let total = self.transcript_rows();
        let maximum = total.saturating_sub(u32::from(viewport_height));
        self.scroll_offset = self.scroll_offset.min(maximum);
        // Postcondition: the offset addresses a row that exists, or the top.
        assert!(self.scroll_offset <= maximum);
    }

    /// Marks a turn as open and records the step.
    pub fn begin_turn(&mut self, step: u32) {
        self.busy = true;
        self.step = step;
        self.status = format!("thinking (step {step})");
    }

    /// Marks the turn closed.
    pub fn end_turn(&mut self) {
        self.busy = false;
        self.status = String::from("ready");
    }

    /// Adds to the running token total.
    pub fn add_tokens(&mut self, tokens: u32) {
        self.tokens_used = self.tokens_used.saturating_add(u64::from(tokens));
    }

    /// Builds the styled lines for one entry.
    ///
    /// The entry's own text carries the style; tool entries add a header so a reader
    /// can tell an invocation from its result.
    #[must_use]
    pub fn lines_for(&self, entry: &Entry) -> Vec<Line<'static>> {
        let style = self.theme.style_for_entry(entry);
        match entry.kind() {
            EntryKind::ToolCall { name, arguments } => {
                let mut lines = vec![Line::from(vec![
                    Span::styled("⚙ ", style),
                    Span::styled(format!("{name}("), style.add_modifier(Modifier::BOLD)),
                ])];
                lines.extend(indented(arguments, 2, style));
                lines.push(Line::from(Span::styled(")", style)));
                lines
            }
            EntryKind::ToolResult {
                name,
                is_error,
                content,
            } => {
                let marker = if *is_error { "✗" } else { "✓" };
                let mut lines = vec![Line::from(Span::styled(format!("{marker} {name}"), style))];
                // A result is multi-line by nature — a `read` returns a file. `Line` does
                // not break on `\n`, so each source line becomes its own `Line`; joining
                // them would render the newlines as control characters, which is what a
                // transcript full of `^J` was.
                lines.extend(indented(content, 2, style));
                lines
            }
            EntryKind::Notice => {
                vec![Line::from(Span::styled(
                    format!("· {}", entry.text()),
                    style,
                ))]
            }
            EntryKind::Text => {
                let mut lines: Vec<Line<'static>> = entry
                    .text()
                    .split('\n')
                    .map(|line| Line::from(Span::styled(line.to_owned(), style)))
                    .collect();
                if entry.is_streaming() {
                    // A cursor makes "still arriving" visible, which is the
                    // difference between a slow model and a hung one.
                    if let Some(last) = lines.last_mut() {
                        last.spans.push(Span::styled("▌", style));
                    }
                }
                if lines.is_empty() {
                    lines.push(Line::from(Span::styled(String::new(), style)));
                }
                lines
            }
        }
    }

    /// Builds the header line naming the role.
    ///
    /// A notice has no header: it is the interface speaking, and a role heading would
    /// suggest otherwise.
    #[must_use]
    pub fn header_for(&self, entry: &Entry) -> Line<'static> {
        let style = self.theme.style_for_entry(entry);
        if matches!(entry.kind(), EntryKind::Notice) {
            return Line::from(Span::styled("──", style));
        }
        Line::from(Span::styled(format!("── {} ", entry.role().label()), style))
    }

    /// Scrolls by `rows`: negative toward older content, positive toward newer.
    ///
    /// A `PageUp` passes a negative delta, which is what "back" means when the offset
    /// counts rows off the top.
    pub fn scroll(&mut self, rows: i32) {
        // The viewport is whatever the last render measured; before the first render
        // there is nothing to scroll.
        let (width, height) = self.last_viewport.unwrap_or((80, 24));
        self.scroll_by(rows, height, width);
    }

    /// Renders the whole interface into `frame`.
    ///
    /// Takes `&mut self` because rendering clamps a scroll offset that the current
    /// viewport has made stale, and a stale offset would render past the end of the
    /// transcript.
    pub fn render(&mut self, frame: &mut Frame<'_>) {
        let area = frame.area();
        // The composer grows with the prompt, so a multi-line one is visible rather
        // than clipped to a single row. The transcript keeps a floor of rows so that a
        // tall composer cannot squeeze the conversation out entirely.
        let composer = self
            .composer_rows(area.width.saturating_sub(COMPOSER_BORDER_COLS))
            .saturating_add(COMPOSER_BORDER_ROWS);
        // The throughput line is the first thing to give up its row when there are not
        // enough: a terminal too short for everything should cost the reader a number
        // they can live without, not the row they are typing on. Everything else here
        // has a floor it keeps, and this is measured against those floors rather than
        // against the terminal alone, so it cannot be granted a row that the composer
        // needed. The chunk stays in the layout at zero height so that the rows below it
        // do not move.
        let stats = u16::from(
            area.height
                >= 1_u16
                    .saturating_add(TRANSCRIPT_FLOOR)
                    .saturating_add(composer)
                    .saturating_add(2),
        );
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(TRANSCRIPT_FLOOR),
                Constraint::Length(composer),
                Constraint::Length(stats),
                Constraint::Length(1),
            ])
            .split(area);

        if let Some(title) = chunks.first() {
            Self::render_title(frame, *title, self.label.as_deref());
        }
        if let Some(body) = chunks.get(1) {
            self.last_viewport = Some((body.width, body.height));
            // A deferred scroll is applied now, while the viewport it is relative to is
            // in hand, and only once.
            if let Some(back) = self.pending_scroll_back.take() {
                self.scroll_to_bottom();
                // `saturating_neg` rather than a unary minus: a scroll of `i32::MIN`
                // would overflow on negation, and the workspace treats wrapping
                // arithmetic as a defect.
                let delta = i32::try_from(back).unwrap_or(i32::MAX).saturating_neg();
                self.scroll_by(delta, body.height, body.width);
            }
            self.clamp_scroll(body.height, body.width);
            self.render_transcript(frame, *body);
        }
        if let Some(input) = chunks.get(2) {
            self.render_input(frame, *input);
        }
        if let Some(stats) = chunks.get(3) {
            self.render_stats(frame, *stats);
        }
        if let Some(status) = chunks.get(4) {
            self.render_status(frame, *status);
        }
    }

    /// Renders the title bar.
    ///
    /// Associated rather than a method: the title bar shows the same things in every
    /// state, so taking `self` would suggest a dependence that does not exist. The session
    /// label is passed in because it is the one part that varies.
    fn render_title(frame: &mut Frame<'_>, area: Rect, label: Option<&str>) {
        let mut spans = vec![Span::styled(
            "nanus",
            Style::default().add_modifier(Modifier::BOLD),
        )];
        if let Some(label) = label {
            spans.push(Span::styled(
                format!("  ·  {label}"),
                Style::default().fg(Color::Cyan),
            ));
        }
        // The three keys a reader cannot guess: how to send, how to break a line, and how
        // to find something they typed an hour ago. The rest are in docs/tui.md.
        spans.push(Span::styled(
            "  ·  Enter sends · Alt+Enter newline · Ctrl+R search · Ctrl+O verbose",
            Style::default().fg(Color::DarkGray),
        ));
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    /// Builds every line the transcript renders to, oldest first, at `width` columns.
    ///
    /// Rendering works on *lines* rather than entries so that the visible window is
    /// computed from the exact set of rows being drawn. Slicing by estimated
    /// per-entry heights drifts whenever an estimate and the renderer disagree, and
    /// the drift shows up as the newest line being clipped — the one line a reader
    /// most wants.
    ///
    /// The width is taken rather than looked up because the compact form clips to it, and
    /// a line that is clipped has to be clipped to the width it will be *drawn* at: a
    /// paragraph that wrapped would be more rows than the one line it promises, and the
    /// scroll arithmetic is built on these heights.
    #[must_use]
    pub fn transcript_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut lines: Vec<Line<'static>> = Vec::new();
        let entries = self.transcript.entries();
        let mut index = 0;
        while let Some(entry) = entries.get(index) {
            // A *run* is what gets summarised, not each entry: six tool calls in a row
            // are one thought the model had, and six collapsed lines would be as noisy
            // as the six lines they replaced.
            if self.collapse_tools && entry.role() == Role::Tool {
                let run = Self::run_length(entries, index, Role::Tool);
                lines.push(self.tool_summary(&entries[index..index.saturating_add(run)]));
                lines.push(Line::from(""));
                index = index.saturating_add(run);
                continue;
            }
            if self.collapse_reasoning && entry.role() == Role::Reasoning {
                let run = Self::run_length(entries, index, Role::Reasoning);
                lines.push(self.reasoning_summary(&entries[index..index.saturating_add(run)]));
                lines.push(Line::from(""));
                index = index.saturating_add(run);
                continue;
            }
            // The compact form draws the two things that are *about* the work as one row
            // each, with no heading and no blank row of their own: that is what "no space
            // above or below" comes to, and it is what makes a turn's machinery read as a
            // block rather than as a wall of paragraphs with gaps in it. A reader who wants
            // the argument blocks and the whole of the reasoning asks for
            // [`Detail::Full`].
            if self.detail == Detail::Compact {
                if let EntryKind::ToolCall { name, arguments } = entry.kind() {
                    let state = Self::tool_state(entries, index, name);
                    lines.push(self.compact_tool_line(state, name, arguments, width));
                    index = index.saturating_add(1);
                    continue;
                }
                // The call's line carries the outcome, so a result that answers one
                // contributes only what the tool *said*. In the live view it says nothing
                // — the frame has no room for output — and the call is then exactly the one
                // line the compact form promises.
                if Self::answers_call_before(entries, index) {
                    let EntryKind::ToolResult { content, .. } = entry.kind() else {
                        unreachable!("only a result answers a call")
                    };
                    if !content.is_empty() {
                        lines.extend(indented(content, 2, self.theme.style_for_entry(entry)));
                        lines.push(Line::from(""));
                    }
                    index = index.saturating_add(1);
                    continue;
                }
                if entry.role() == Role::Reasoning {
                    lines.push(self.compact_thinking_line(entry, width));
                    index = index.saturating_add(1);
                    continue;
                }
            }
            // A tool's heading is dropped in the compact form: the call's own line already
            // names the tool, and the heading only answers a question nobody asked.
            if self.detail != Detail::Compact || entry.role() != Role::Tool {
                lines.push(self.header_for(entry));
            }
            lines.extend(self.lines_for(entry));
            // A blank row between entries, so two consecutive messages do not read
            // as one paragraph.
            lines.push(Line::from(""));
            index = index.saturating_add(1);
        }
        lines
    }

    /// Returns how the call at `index` ended, as far as the transcript knows.
    ///
    /// Adjacency is the pairing: a result is appended immediately after the call it
    /// answers, which is what the session log guarantees by construction and what the link
    /// does by sending one `Tool` and then one `ToolDone`. A call with no result after it
    /// is still running, which is a state the line has to be able to show — a call that
    /// looked finished while it was running would be a worse lie than a missing mark.
    fn tool_state(entries: &[Entry], index: usize, name: &str) -> compact::ToolState {
        let result = entries.get(index.saturating_add(1)).map(Entry::kind);
        match result {
            Some(EntryKind::ToolResult {
                name: result_name,
                is_error,
                ..
            }) if result_name == name => {
                if *is_error {
                    compact::ToolState::Failed
                } else {
                    compact::ToolState::Ok
                }
            }
            _ => compact::ToolState::Running,
        }
    }

    /// Returns whether the entry at `index` is the result of the call before it.
    ///
    /// The other half of [`ViewState::tool_state`]: this is the result asking whether its
    /// mark is already on the line above, so that it does not draw a second one.
    fn answers_call_before(entries: &[Entry], index: usize) -> bool {
        let Some(previous) = index
            .checked_sub(1)
            .and_then(|previous| entries.get(previous))
        else {
            return false;
        };
        match (previous.kind(), entries.get(index).map(Entry::kind)) {
            (
                EntryKind::ToolCall { name: call, .. },
                Some(EntryKind::ToolResult { name: result, .. }),
            ) => call == result,
            _ => false,
        }
    }

    /// Counts the entries from `start` that carry `role`, consecutively.
    fn run_length(entries: &[Entry], start: usize, role: Role) -> usize {
        entries
            .iter()
            .skip(start)
            .take_while(|entry| entry.role() == role)
            .count()
            .max(1)
    }

    /// Draws a tool call as the single line that says what it is and what it is doing.
    ///
    /// The phrasing is [`crate::compact`]'s business, not the view's: which argument a tool
    /// is acting on is knowledge about the toolset, and the view's job is only to style the
    /// line and to place it.
    fn compact_tool_line(
        &self,
        state: compact::ToolState,
        name: &str,
        arguments: &str,
        width: u16,
    ) -> Line<'static> {
        let text = compact::tool_line(state, name, arguments, width);
        Line::from(Span::styled(text, self.theme.tool))
    }

    /// Draws a thinking segment as its newest line.
    fn compact_thinking_line(&self, entry: &Entry, width: u16) -> Line<'static> {
        let text = compact::thinking_line(entry.text(), entry.is_streaming(), width);
        Line::from(Span::styled(text, self.theme.reasoning))
    }

    /// Draws a run of tool activity as one line.
    ///
    /// The line keeps the two things a reader scanning for a problem needs — how much
    /// happened, and whether any of it failed — and drops the part that is only wanted
    /// when reading closely, which is what the toggle is for.
    fn tool_summary(&self, run: &[Entry]) -> Line<'static> {
        let mut calls: usize = 0;
        let mut failures: usize = 0;
        let mut names: Vec<&str> = Vec::new();
        for entry in run {
            match entry.kind() {
                EntryKind::ToolCall { name, .. } => {
                    calls = calls.saturating_add(1);
                    if !names.contains(&name.as_str()) {
                        names.push(name.as_str());
                    }
                }
                EntryKind::ToolResult { is_error, .. } => {
                    if *is_error {
                        failures = failures.saturating_add(1);
                    }
                }
                EntryKind::Text | EntryKind::Notice => {}
            }
        }
        let plural = if calls == 1 { "call" } else { "calls" };
        let failures = if failures > 0 {
            format!(" · {failures} failed")
        } else {
            String::new()
        };
        Line::from(Span::styled(
            format!("── {calls} tool {plural} · {}{failures}", names.join(", ")),
            self.theme.tool,
        ))
    }

    /// Draws a run of reasoning as one line.
    fn reasoning_summary(&self, run: &[Entry]) -> Line<'static> {
        let characters: usize = run.iter().map(|entry| entry.text().chars().count()).sum();
        let parts = if run.len() == 1 { "part" } else { "parts" };
        Line::from(Span::styled(
            format!(
                "── thinking · {} {parts} · {characters} characters",
                run.len()
            ),
            self.theme.reasoning,
        ))
    }

    /// Renders the conversation.
    fn render_transcript(&self, frame: &mut Frame<'_>, area: Rect) {
        let lines = self.transcript_lines(area.width);
        let heights = Self::line_heights(&lines, area.width);
        let (start, padding) = Self::window(&heights, self.scroll_offset, area.height);
        // Blank rows above the tail, so the newest line sits at the bottom of the
        // viewport rather than floating in the middle of it.
        let blanks = core::iter::repeat_with(|| Line::from(""))
            .take(usize::try_from(padding).unwrap_or(usize::MAX));
        let visible = blanks.chain(lines.into_iter().skip(start));
        let paragraph = Paragraph::new(Text::from_iter(visible))
            .block(Block::default().borders(Borders::NONE))
            .wrap(Wrap { trim: false });
        frame.render_widget(paragraph, area);
    }

    /// The display rows each rendered line occupies at `width`.
    ///
    /// Counted by the same rule the renderer breaks by, rather than by dividing the line's
    /// width: ceiling division is a *lower bound* for word wrapping, because a row whose next
    /// word does not fit ends early and wastes the rest of itself. A window computed from a
    /// bound that is too low leaves the rows it under-counted below the fold *permanently* —
    /// scrolling can only reach content whose height it knows about — which is how the last
    /// rows of a long line of paths became visible at no offset at all.
    ///
    /// A line that fits is one row and needs no counting, which is most of them.
    fn line_heights(lines: &[Line<'static>], width: u16) -> Vec<u32> {
        let usable = width.max(1);
        lines
            .iter()
            .map(|line| {
                if line.width() <= usize::from(usable) {
                    return 1;
                }
                // Measured from the text rather than from the line's width: the width is a
                // number of columns and the question is how many *rows* those columns take,
                // which depends on where the words break.
                let text: String = line
                    .spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect();
                wrap_count(&text, usable)
            })
            .collect()
    }

    /// Picks the lines to draw for a scroll offset measured in display rows.
    ///
    /// Returns the first line to draw and how many blank rows to draw above it.
    ///
    /// The offset is in rows because that is what a reader scrolls, and the renderer
    /// draws whole lines — but a line that wraps covers more than one row, so the two
    /// are not interchangeable. Treating one as the other is what put the newest
    /// content permanently below the fold: `scroll_to_bottom` computed an offset in
    /// rows from a total counted in lines, so every wrapped line made the bottom
    /// unreachable by exactly the rows it wrapped into.
    ///
    /// At the end of the transcript the window is anchored to the bottom. A last line
    /// that wraps cannot be drawn in part, so padding the top is the only way to keep
    /// the newest row — the one the reader scrolled to — on screen.
    fn window(heights: &[u32], offset: u32, height: u16) -> (usize, u32) {
        let available = u32::from(height);
        let total = heights.iter().copied().fold(0_u32, u32::saturating_add);

        // The line the offset lands inside, and the rows before it.
        let mut before = 0_u32;
        let mut start = heights.len();
        for (index, rows) in heights.iter().enumerate() {
            if before.saturating_add(*rows) > offset {
                start = index;
                break;
            }
            before = before.saturating_add(*rows);
        }

        let anchored = offset > 0 && offset.saturating_add(available) >= total;
        if !anchored {
            return (start, 0);
        }
        // Walk back from the end taking whole lines while they fit.
        let mut used = 0_u32;
        let mut anchor = heights.len();
        for (index, rows) in heights.iter().enumerate().rev() {
            if used.saturating_add(*rows) > available {
                break;
            }
            used = used.saturating_add(*rows);
            anchor = index;
        }
        (anchor, available.saturating_sub(used))
    }

    /// Returns how many display rows the whole transcript renders to.
    ///
    /// Rows rather than lines, because every scroll bound in this type is compared
    /// against a viewport measured in rows. The width comes from the last render, which
    /// is also the only width these bounds are ever used at.
    #[must_use]
    pub fn transcript_rows(&self) -> u32 {
        let Some((width, _)) = self.last_viewport else {
            return 0;
        };
        let lines = self.transcript_lines(width);
        Self::line_heights(&lines, width)
            .iter()
            .copied()
            .fold(0_u32, u32::saturating_add)
    }

    /// Renders the composer.
    fn render_input(&self, frame: &mut Frame<'_>, area: Rect) {
        let inner = area.width.saturating_sub(COMPOSER_BORDER_COLS);
        // The viewport is the height actually granted, not the one asked for: when the
        // terminal is short the layout gives the composer less than `composer_rows`, and
        // a window sized from the request would scroll the caret just off the bottom.
        let visible = area.height.saturating_sub(COMPOSER_BORDER_ROWS);
        let composer = self.composer_layout(inner);
        // The window follows the caret, keeping it on the last row it can when the
        // prompt is taller than the composer.
        let caret = u16::try_from(composer.caret_row).unwrap_or(u16::MAX);
        let offset = caret.saturating_sub(visible.saturating_sub(1));
        let block = Block::default().borders(Borders::ALL).title(" message ");
        // No `Wrap`: the rows are already wrapped, and asking the renderer to wrap them
        // again would re-break lines the layout has counted — which is how the caret's
        // row and the drawn row drifted apart in the first place.
        frame.render_widget(
            Paragraph::new(composer.lines(self))
                .block(block)
                .scroll((offset, 0)),
            area,
        );
    }

    /// Lays the composer out into display rows, and says which row holds the caret.
    ///
    /// Wrapping is done here rather than left to the renderer, and that is the point of
    /// this function. The row a character lands on is what decides whether the composer
    /// has to scroll, and a renderer's wrapper breaks at word boundaries: a word that
    /// does not fit starts a new row instead of filling the one before it. Counting
    /// characters cannot see that, so the estimate came out a row short exactly when a
    /// long word was pushed down — leaving the caret one row below the window, which is
    /// what a user typing a long line sees. Owning the wrap makes the caret's row a fact
    /// rather than a guess.
    fn composer_layout(&self, inner_width: u16) -> ComposerLayout {
        let usable = usize::from(inner_width.max(1));
        let (cursor_line, cursor_column) = self.input.cursor_line_col();
        let prompt = Self::prompt(self.busy).to_owned();
        let indent = " ".repeat(Self::PROMPT_WIDTH);
        let mut rows: Vec<ComposerRow> = Vec::new();
        let mut caret_row = 0;
        let mut caret_column = 0;
        for (index, text) in self.input.lines().iter().enumerate() {
            let prefix = if index == 0 {
                prompt.clone()
            } else {
                indent.clone()
            };
            // The prefix takes room on the line's first row only; the rows a line wraps
            // into start at the left edge, so they get the full width.
            let first = usable.saturating_sub(Self::PROMPT_WIDTH).max(1);
            let wrapped = wrap_words(text, first, usable);
            if index == cursor_line {
                let (row, column) = caret_position(&wrapped, cursor_column);
                caret_row = rows.len().saturating_add(row);
                // Only a line's first row carries the prefix, and it is drawn inside the
                // row's width rather than beside it, so the caret's column has to include
                // it to mean a cell on the screen.
                caret_column = column.saturating_add(if row == 0 { Self::PROMPT_WIDTH } else { 0 });
            }
            for (offset, (line, _, _)) in wrapped.into_iter().enumerate() {
                rows.push(ComposerRow {
                    prefix: if offset == 0 {
                        prefix.clone()
                    } else {
                        String::new()
                    },
                    text: line,
                });
            }
        }
        if rows.is_empty() {
            // An empty prompt is still one blank row to put the caret on, and the caret
            // sits after the prompt rather than inside it.
            rows.push(ComposerRow {
                prefix: prompt,
                text: String::new(),
            });
            caret_row = 0;
            caret_column = Self::PROMPT_WIDTH;
        }
        // A caret with no cell to be drawn in. A row that is exactly full has no column
        // past its last character, so the caret had nothing to reverse and the cursor
        // vanished for the keystroke in which a prompt crossed a row boundary. A
        // terminal wraps the cursor onto the next line and so does this: the caret
        // becomes the first cell of the following row — the continuation indent, or a
        // row added for it when the caret was already on the last one.
        if caret_column >= usable {
            let next = caret_row.saturating_add(1);
            caret_row = if next < rows.len() {
                next
            } else {
                rows.push(ComposerRow {
                    prefix: String::new(),
                    text: String::new(),
                });
                rows.len().saturating_sub(1)
            };
            caret_column = 0;
        }
        ComposerLayout {
            rows,
            caret_row,
            caret_column,
        }
    }

    /// Returns how many display rows the composer occupies, capped at
    /// [`MAX_COMPOSER_ROWS`], at the given inner width.
    ///
    /// The composer grows with the prompt so an explicit newline is visible, and stops
    /// at the cap so a long one cannot squeeze the conversation away; past the cap the
    /// window follows the caret instead.
    fn composer_rows(&self, inner_width: u16) -> u16 {
        let rows = self.composer_layout(inner_width).rows.len();
        u16::try_from(rows)
            .unwrap_or(u16::MAX)
            .clamp(1, MAX_COMPOSER_ROWS)
    }

    /// The prompt drawn before the composer's first line.
    fn prompt(busy: bool) -> &'static str {
        if busy { "… " } else { "› " }
    }

    /// The columns the prompt occupies, which is also the continuation indent.
    const PROMPT_WIDTH: usize = 2;

    /// Renders the status line.
    /// Renders the throughput line, directly under the composer.
    ///
    /// Its own row rather than more of the status line: the status line is about the
    /// turn in progress and answers "what is happening", while these are the model's
    /// running numbers and answer "how is it going". Crowding them together would push
    /// whichever the reader wanted off the end of a narrow terminal.
    ///
    /// A number that has not been measured is drawn as a dash. Zero is a measurement —
    /// it says the model generated nothing — and showing it for "no request has finished
    /// yet" would be a claim rather than a blank.
    ///
    /// Each rate is written as a pair, `generating/whole-request`, because those are the two
    /// answers to "how fast" and the gap between them is the point: the first is the model's
    /// speed and the second is what a reader actually waited through, so a long prompt shows up
    /// as a wide pair rather than as a slow model. They are packed onto one row instead of two
    /// so the block keeps its single line, and `/stats` spells the pair out in full.
    ///
    /// The wait sits beside them rather than inside them, because it is the one figure here a
    /// reader can act on: the generating rate is the provider's and is not theirs to change,
    /// while the wait is what a shorter prompt — or a cached one — buys back.
    fn render_stats(&self, frame: &mut Frame<'_>, area: Rect) {
        let dim = Style::default().fg(Color::DarkGray);
        // Ordered by what a reader would give up last, because the row takes as many as fit and
        // drops the rest from the end: the rates first, then the wait that explains the gap
        // between them, then the cache share that usually explains the wait.
        let readings = [
            format!(
                "last {}/{} tok/s",
                show(self.stats.last_rate()),
                show(self.stats.last_request_rate())
            ),
            format!(
                "avg {}/{} tok/s",
                show(self.stats.average_rate()),
                show(self.stats.average_request_rate())
            ),
            format!("ttft {}", show_duration(self.stats.last_ttft_ms())),
            format!("cache hit {}", share(self.stats.cache_hit_percent())),
        ];
        let spans: Vec<Span<'_>> = readings
            .iter()
            .take(Self::fitting(&readings, area.width))
            .enumerate()
            .map(|(index, reading)| {
                let text = if index == 0 {
                    reading.clone()
                } else {
                    format!("{STATS_SEPARATOR}{reading}")
                };
                Span::styled(text, dim)
            })
            .collect();
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    /// How many of `readings` fit on one row of `width` columns.
    ///
    /// Read in order until one does not fit, so the row is always a prefix of its readings
    /// rather than a selection from the middle. At least one is taken however narrow the
    /// terminal, because the first reading is what the row exists for and a row that drew
    /// nothing would spend a line saying nothing.
    fn fitting(readings: &[String], width: u16) -> usize {
        let mut taken = 0_usize;
        for count in 1..=readings.len() {
            let slice: Vec<&str> = readings.iter().take(count).map(String::as_str).collect();
            if !Self::row_fits(&slice, width) {
                break;
            }
            taken = count;
        }
        if readings.is_empty() { 0 } else { taken.max(1) }
    }

    /// Whether `parts`, laid out with separators between them, fit in `width` columns.
    ///
    /// Counted in characters rather than bytes: the separator is a multi-byte glyph, and
    /// measuring it in bytes would drop a reading that fits. Saturating throughout, because
    /// laying out a row is a rendering detail and must not be able to overflow a counter.
    fn row_fits(parts: &[&str], width: u16) -> bool {
        let reading = parts
            .iter()
            .map(|part| part.chars().count())
            .fold(0_usize, usize::saturating_add);
        let gaps = parts.len().saturating_sub(1);
        let total = reading.saturating_add(gaps.saturating_mul(STATS_SEPARATOR.chars().count()));
        total <= usize::from(width)
    }

    fn render_status(&self, frame: &mut Frame<'_>, area: Rect) {
        // A running search takes the line: the composer is showing a match rather than the
        // reader's draft, and without the query on screen there is nothing to say what is
        // being searched for or why the text changed.
        if let Some((query, matched)) = self.input.search_query() {
            let tail = if matched { "" } else { " (no match)" };
            let line = Line::from(Span::styled(
                format!("(reverse-i-search)`{query}'{tail}"),
                self.theme.busy,
            ));
            frame.render_widget(Paragraph::new(line), area);
            return;
        }
        let busy = if self.busy {
            Span::styled(format!("● {}", self.status), self.theme.busy)
        } else {
            Span::styled(
                format!("○ {}", self.status),
                Style::default().fg(Color::DarkGray),
            )
        };
        let usage = Span::styled(
            format!("  ·  {} tokens", self.tokens_used),
            Style::default().fg(Color::DarkGray),
        );
        let step = if self.step > 0 {
            Span::styled(
                format!("  ·  step {}", self.step),
                Style::default().fg(Color::DarkGray),
            )
        } else {
            Span::raw("")
        };
        // What is collapsed, so a reader who toggled something (or inherited a toggle
        // from an earlier keypress) can see why the transcript looks the way it does.
        let collapsed = if self.collapse_tools || self.collapse_reasoning {
            let mut what: Vec<&str> = Vec::new();
            if self.collapse_tools {
                what.push("tools");
            }
            if self.collapse_reasoning {
                what.push("thinking");
            }
            Span::styled(
                format!("  ·  summarized: {}", what.join(" and ")),
                Style::default().fg(Color::DarkGray),
            )
        } else {
            Span::raw("")
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![busy, step, usage, collapsed])),
            area,
        );
    }
}

/// The composer laid out for drawing: its rows, and where the caret sits among them.
struct ComposerLayout {
    /// One entry per display row, in the order they are drawn.
    rows: Vec<ComposerRow>,
    /// The index of the row holding the caret.
    caret_row: usize,
    /// The caret's column counted from the start of that row, **prefix included**.
    ///
    /// Row-relative rather than text-relative because the caret can be on the prefix:
    /// a caret at the end of a row that is exactly full has no column inside the row's
    /// text to sit on, and belongs at the start of the next row — which begins with the
    /// continuation indent.
    caret_column: usize,
}

/// One display row of the composer.
struct ComposerRow {
    /// The prompt or the continuation indent drawn before the text.
    ///
    /// Empty on the rows a long line wraps into, which start at the left edge.
    prefix: String,
    /// The row's text, without the caret.
    text: String,
}

impl ComposerLayout {
    /// Builds the rows as drawable lines, cutting the caret's row around the caret.
    fn lines(&self, view: &ViewState) -> Vec<Line<'static>> {
        let prompt_style = if view.busy {
            view.theme.busy
        } else {
            view.theme.user
        };
        let mut lines = Vec::with_capacity(self.rows.len());
        for (index, row) in self.rows.iter().enumerate() {
            let mut spans: Vec<Span<'static>> = Vec::new();
            let prefix_width = row.prefix.chars().count();
            let caret_here = index == self.caret_row;
            if caret_here && self.caret_column < prefix_width {
                // The caret is on the indent. That is where a caret at the end of a full
                // row lands: the character after it is the first of the *next* line, and
                // the insertion point is before that line's indent, not on its text.
                push_caret_run(&mut spans, &row.prefix, self.caret_column, prompt_style);
                if !row.text.is_empty() {
                    spans.push(Span::styled(row.text.clone(), Style::default()));
                }
            } else {
                if !row.prefix.is_empty() {
                    spans.push(Span::styled(row.prefix.clone(), prompt_style));
                }
                if caret_here {
                    push_caret_run(
                        &mut spans,
                        &row.text,
                        self.caret_column.saturating_sub(prefix_width),
                        Style::default(),
                    );
                } else {
                    spans.push(Span::styled(row.text.clone(), Style::default()));
                }
            }
            lines.push(Line::from(spans));
        }
        lines
    }
}

/// Wraps one source line into display rows at the given widths.
///
/// Greedy and word-aware: a row takes words while they fit, a word longer than the row
/// is hard-broken, and the whitespace a break lands on is dropped. Each row carries the
/// range of source characters it covers, so the caret can be placed on the row it
/// actually belongs to rather than on one inferred from its index.
///
/// The first row is narrower than the rest, because the prompt or indent is drawn once
/// at the start of the line and the rows it wraps into begin at the left edge.
///
/// Characters are counted, not display columns, matching the estimate the transcript
/// uses: a wide character can therefore make a row one column wider than intended, which
/// the terminal clips rather than mislays. Wrapping here rather than leaving it to the
/// renderer is what makes the row count exact; see [`ViewState::composer_layout`].
fn wrap_words(text: &str, first: usize, rest: usize) -> Vec<(String, usize, usize)> {
    let mut rows: Vec<(String, usize, usize)> = Vec::new();
    let mut row: Vec<char> = Vec::new();
    let mut sources: Vec<usize> = Vec::new();
    let mut breaks: Option<usize> = None;
    let mut capacity = first.max(1);

    for (source, character) in text.chars().enumerate() {
        if row.len() >= capacity {
            // The row is full. Break at the last space when there is one, so the word
            // that did not fit starts the next row whole instead of being split.
            let split = match breaks {
                Some(position) if position > 0 => position,
                _ => row.len(),
            };
            rows.push((
                row.iter().take(split).collect(),
                sources.first().copied().unwrap_or(source),
                sources
                    .get(split.saturating_sub(1))
                    .map_or(source, |last| last.saturating_add(1)),
            ));
            // The space the break lands on belongs to no row, which is why the ranges
            // can have a gap in them.
            let keep_from = if split < row.len() {
                split.saturating_add(1)
            } else {
                split
            };
            row = row.iter().skip(keep_from).copied().collect();
            sources = sources.iter().skip(keep_from).copied().collect();
            capacity = rest.max(1);
            breaks = row.iter().rposition(|character| character.is_whitespace());
        }
        if character.is_whitespace() {
            breaks = Some(row.len());
        }
        row.push(character);
        sources.push(source);
    }

    if !row.is_empty() {
        let start = sources.first().copied().unwrap_or(0);
        rows.push((
            row.into_iter().collect(),
            start,
            sources.last().map_or(start, |last| last.saturating_add(1)),
        ));
    }
    if rows.is_empty() {
        // An empty line is still a row to put the caret on.
        rows.push((String::new(), 0, 0));
    }
    rows
}

/// Pushes `run` as spans with the caret drawn *in* the cell it is over.
///
/// The caret is a style on a cell rather than a glyph in a cell of its own: a glyph
/// takes a column and pushes the rest of the row along, so moving the cursor through a
/// word looked like editing it. A column past the end of the run has no cell to reverse,
/// so the caret becomes one — a block in the space the next character will occupy.
fn push_caret_run(spans: &mut Vec<Span<'static>>, run: &str, column: usize, style: Style) {
    let characters: Vec<char> = run.chars().collect();
    let at = column.min(characters.len());
    let before: String = characters.iter().take(at).collect();
    let under: String = characters
        .get(at)
        .map_or_else(|| String::from(" "), char::to_string);
    let after: String = characters.iter().skip(at.saturating_add(1)).collect();
    if !before.is_empty() {
        spans.push(Span::styled(before, style));
    }
    spans.push(Span::styled(under, style.add_modifier(Modifier::REVERSED)));
    if !after.is_empty() {
        spans.push(Span::styled(after, style));
    }
}

/// Returns the row and column that the source character `caret` lands on.
///
/// A caret in whitespace that a break dropped belongs to the row it was dropped from,
/// at that row's end, which is where the next keystroke would appear. Testing that the
/// caret is *inside* a row's range, rather than merely before its end, is what stops the
/// row after the gap from claiming it: the ranges are not contiguous, so "before the
/// end" is also true of a caret sitting in the space between two of them.
fn caret_position(rows: &[(String, usize, usize)], caret: usize) -> (usize, usize) {
    let mut fallback = (0, 0);
    for (index, (text, start, end)) in rows.iter().enumerate() {
        if caret >= *start && caret < *end {
            return (
                index,
                caret.saturating_sub(*start).min(text.chars().count()),
            );
        }
        if *end <= caret {
            fallback = (index, text.chars().count());
        }
    }
    fallback
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::stats::Generation;

    /// Renders `state` into a test terminal and returns the buffer as text.
    fn rendered(state: &mut ViewState, width: u16, height: u16) -> String {
        draw_with_caret(state, width, height).0
    }

    /// Where the caret was drawn: the cell, and the character in it.
    ///
    /// A named type rather than a tuple because the tests assert on all three parts and
    /// `caret.row` says which is which.
    struct Caret {
        row: u16,
        column: u16,
        symbol: String,
    }

    /// Draws the view, returning the rendered text and the cell the caret is drawn on.
    ///
    /// The caret is a *style* rather than a glyph, so a test that looked only at the text
    /// could not tell whether one was drawn at all. Where it landed comes back too,
    /// because "the caret is on screen" is only half of what the tests need to say.
    fn draw_with_caret(state: &mut ViewState, width: u16, height: u16) -> (String, Option<Caret>) {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("a test terminal always builds");
        let drawn = terminal.draw(|frame| state.render(frame));
        assert!(drawn.is_ok(), "rendering must not fail");
        let buffer = terminal.backend().buffer();
        let mut text = String::new();
        let mut caret: Option<Caret> = None;
        for row in 0..buffer.area.height {
            for column in 0..buffer.area.width {
                let Some(cell) = buffer.cell((column, row)) else {
                    text.push(' ');
                    continue;
                };
                text.push_str(cell.symbol());
                if caret.is_none() && cell.style().add_modifier.contains(Modifier::REVERSED) {
                    caret = Some(Caret {
                        row,
                        column,
                        symbol: cell.symbol().to_owned(),
                    });
                }
            }
            text.push('\n');
        }
        (text, caret)
    }

    fn state_with(entries: Vec<Entry>) -> ViewState {
        let mut state = ViewState::new();
        for entry in entries {
            state.transcript.push(entry);
        }
        state
    }

    #[test]
    fn the_title_and_status_are_always_drawn() {
        let mut state = ViewState::new();
        let text = rendered(&mut state, 60, 12);
        assert!(text.contains("nanus"));
        assert!(text.contains("ready"));
        assert!(text.contains("message"));
    }

    #[test]
    fn a_named_session_is_shown_in_the_title_bar() {
        // Which conversation this is matters once several can be resumed: two terminals
        // can be attached to two sessions, and the title bar is how a reader tells.
        let mut state = ViewState::new();
        assert!(!rendered(&mut state, 60, 12).contains("the-glob-bug"));
        state.label = Some("the-glob-bug".to_owned());
        assert!(rendered(&mut state, 60, 12).contains("the-glob-bug"));
    }

    #[test]
    fn a_user_entry_renders_with_its_role_label() {
        let mut state = state_with(vec![Entry::prose(Role::User, "hello there")]);
        let text = rendered(&mut state, 60, 12);
        assert!(text.contains("you"));
        assert!(text.contains("hello there"));
    }

    #[test]
    fn an_answer_and_its_reasoning_are_visually_distinct() {
        let mut state = state_with(vec![
            Entry::prose(Role::Reasoning, "weighing options"),
            Entry::prose(Role::Assistant, "the answer"),
        ]);
        let text = rendered(&mut state, 60, 14);
        // The labels differ, so a reader can tell the two apart.
        assert!(text.contains("thinking"));
        assert!(text.contains("nanus"));
        assert!(text.contains("weighing options"));
        assert!(text.contains("the answer"));
    }

    #[test]
    fn a_streaming_entry_shows_a_cursor() {
        let mut state = ViewState::new();
        state
            .transcript
            .append_stream(Role::Assistant, "partial", false);
        let text = rendered(&mut state, 60, 12);
        assert!(text.contains('▌'), "a streaming entry is marked");
        assert!(text.contains("partial"));
    }

    #[test]
    fn a_settled_entry_has_no_cursor() {
        let mut state = state_with(vec![Entry::prose(Role::Assistant, "done")]);
        let text = rendered(&mut state, 60, 12);
        assert!(!text.contains('▌'));
    }

    #[test]
    fn a_tool_call_is_one_line_saying_what_it_is_doing() {
        let mut state = state_with(vec![Entry::tool_call("read", "{\"file_path\":\"a.txt\"}")]);
        let text = rendered(&mut state, 70, 12);
        assert!(text.contains("⚙ Read File · a.txt"), "{text}");
        // The arguments are not spelled out: that is the whole of what the compact form
        // drops, and a reader who wants them asks for `tui_detail = "full"`.
        assert!(!text.contains("file_path"), "{text}");
    }

    #[test]
    fn a_compact_tool_call_occupies_exactly_one_row() {
        // The row count is the claim, not the text: `transcript_lines` is what both the
        // renderer and the scroll arithmetic measure, so one line here is one row there.
        let mut state = state_with(vec![
            Entry::tool_call("edit", "{\"file_path\":\"a.rs\"}"),
            Entry::tool_call("bash", "{\"command\":\"cargo test\"}"),
        ]);
        let lines = state.transcript_lines(70);
        assert_eq!(lines.len(), 2, "{lines:?}");
        // No blank row above or below either of them, so consecutive calls are adjacent.
        assert!(lines.iter().all(|line| line.width() > 0), "{lines:?}");

        state.detail = Detail::Full;
        let full = state.transcript_lines(70);
        assert!(full.len() > 2, "the full form is taller: {full:?}");
        assert!(
            full.iter()
                .any(|line| line.to_string().contains("file_path")),
            "{full:?}"
        );
    }

    #[test]
    fn a_settled_thinking_segment_is_drawn_as_its_newest_line() {
        let mut state = state_with(vec![Entry::prose(Role::Reasoning, "first\nsecond\nthird")]);
        let text = rendered(&mut state, 70, 12);
        assert!(text.contains("── thinking · third"), "{text}");
        assert!(!text.contains("first"), "{text}");

        // And the full form still draws the whole segment, which is what the setting
        // reverts to.
        state.detail = Detail::Full;
        let text = rendered(&mut state, 70, 12);
        assert!(text.contains("first"), "{text}");
        assert!(text.contains("third"), "{text}");
    }

    #[test]
    fn a_thinking_line_follows_the_deltas_as_they_arrive() {
        // This is the scrolling: the line is the newest one, so each delta changes what is
        // drawn rather than adding to a paragraph.
        let mut state = ViewState::new();
        state
            .transcript
            .append_stream(Role::Reasoning, "checking the caller", false);
        let before = rendered(&mut state, 70, 12);
        state
            .transcript
            .append_stream(Role::Reasoning, "\nand the glob itself", false);
        let after = rendered(&mut state, 70, 12);
        assert!(before.contains("checking the caller"), "{before}");
        assert!(after.contains("and the glob itself"), "{after}");
        assert!(!after.contains("checking the caller"), "{after}");

        // The cursor stays on the line while it is still arriving.
        assert!(after.contains('▌'), "{after}");
    }

    #[test]
    fn the_compact_lines_are_drawn_in_their_roles_colours() {
        // The style is the view's business, and the compact form must not lose it: a
        // one-line thinking entry is still dimmed, and a call is still tool-coloured.
        let mut state = state_with(vec![
            Entry::prose(Role::Reasoning, "considering"),
            Entry::tool_call("read", "{\"file_path\":\"a.txt\"}"),
        ]);
        let text = rendered(&mut state, 70, 12);
        assert!(text.contains("considering"), "{text}");
        assert!(text.contains("Read File"), "{text}");
    }

    /// The compact form's one-line promise: the call and how it went are a single row, and
    /// the result contributes only what the tool said.
    #[test]
    fn a_tool_call_and_its_result_are_one_line() {
        let mut state = state_with(vec![
            Entry::tool_call("read", "{\"file_path\":\"a.txt\"}"),
            Entry::tool_result("read", false, "content"),
        ]);
        let rows: Vec<String> = rendered(&mut state, 70, 12)
            .lines()
            .map(str::to_owned)
            .collect();
        let call = rows
            .iter()
            .position(|row| row.contains("Read File"))
            .expect("the call is drawn");
        assert!(
            rows.get(call).is_some_and(|row| row.contains('✓')),
            "the outcome is on the call's own line: {rows:?}"
        );
        assert!(
            rows.get(call).is_some_and(|row| row.contains("a.txt")),
            "and so is what it acted on: {rows:?}"
        );
        assert_eq!(
            rows.iter().filter(|row| row.contains("Read File")).count(),
            1,
            "the tool is named once, not once per entry: {rows:?}"
        );
        assert!(
            rows.get(call.saturating_add(1))
                .is_some_and(|row| row.contains("content")),
            "the tool's own output follows it: {rows:?}"
        );
    }

    /// The live view is the case that made this one line rather than three: the frame that
    /// ends a call carries no output, so the call's line is the whole of it — and the
    /// placeholder the interface used to invent under it is gone.
    #[test]
    fn a_live_tool_call_is_one_line() {
        let mut state = state_with(vec![
            Entry::tool_call("read", "{\"file_path\":\"src/view.rs\"}"),
            Entry::tool_result("read", false, ""),
        ]);
        let text = rendered(&mut state, 70, 12);
        let rows: Vec<&str> = text.lines().collect();
        assert_eq!(
            rows.iter().filter(|row| row.contains("Read File")).count(),
            1,
            "one line for the call: {text}"
        );
        assert!(
            !rows.iter().any(|row| row.trim() == "done"),
            "and nothing said under it: {text}"
        );
    }

    /// A call with no result yet is running, and the line has to say so: a call drawn as
    /// finished while it is still running is a worse lie than no mark at all.
    #[test]
    fn a_call_still_running_is_marked_as_running() {
        let mut state = state_with(vec![Entry::tool_call("read", "{\"file_path\":\"a.txt\"}")]);
        let text = rendered(&mut state, 70, 12);
        assert!(text.contains("⚙ Read File · a.txt"), "{text}");
        assert!(!text.contains('✓'), "nothing has finished: {text}");
    }

    #[test]
    fn a_failed_tool_result_is_marked_as_a_failure() {
        let mut ok = state_with(vec![Entry::tool_result("read", false, "content")]);
        let mut bad = state_with(vec![Entry::tool_result("read", true, "missing")]);
        let ok_text = rendered(&mut ok, 60, 12);
        let bad_text = rendered(&mut bad, 60, 12);
        assert!(ok_text.contains('✓'));
        assert!(bad_text.contains('✗'));
        assert_ne!(ok_text, bad_text);
    }

    #[test]
    fn busy_state_changes_the_prompt_and_status() {
        let mut state = ViewState::new();
        state.begin_turn(3);
        let text = rendered(&mut state, 60, 12);
        assert!(text.contains("thinking (step 3)"));
        assert!(text.contains("step 3"));
        // The open-turn marker is distinct from the idle one.
        assert!(text.contains('●'));
        assert!(!text.contains('○'));

        state.end_turn();
        let text = rendered(&mut state, 60, 12);
        assert!(text.contains("ready"));
        assert!(text.contains('○'));
    }

    /// A running search takes the status line, because the composer is showing a match
    /// rather than the reader's draft: without the query there is nothing on screen
    /// saying what is being searched for or why the text changed.
    #[test]
    fn the_status_line_shows_a_running_search() {
        let mut state = ViewState::new();
        state.input.insert_str("an earlier prompt");
        assert!(state.input.submit().is_some());
        state.input.search_start();
        for character in "earl".chars() {
            state.input.search_push(character);
        }
        let text = rendered(&mut state, 80, 12);
        assert!(text.contains("reverse-i-search"), "{text}");
        assert!(text.contains("earl"), "{text}");
        assert!(text.contains("an earlier prompt"), "and the match: {text}");

        // The other half: a query that found nothing says so rather than looking stalled.
        state.input.search_push('z');
        let text = rendered(&mut state, 80, 12);
        assert!(text.contains("no match"), "{text}");
    }

    #[test]
    fn token_usage_is_shown() {
        let mut state = ViewState::new();
        state.add_tokens(1200);
        let text = rendered(&mut state, 60, 12);
        assert!(text.contains("1200 tokens"));
    }

    /// The three numbers asked for, on a row of their own between the composer and the
    /// status line. The position is the assertion that matters: the same line drawn
    /// anywhere else would read as being about something else.
    #[test]
    fn the_throughput_line_is_drawn_under_the_composer() {
        let mut state = ViewState::new();
        // 300 tokens generated in two seconds is 150 a second, and the half-second wait in
        // front of them is not generation — which is the whole point of the split.
        state.stats.record(Generation {
            completion_tokens: 300,
            cache_hit_tokens: 900,
            cache_miss_tokens: 100,
            ttft_ms: 500,
            decode_ms: 2_000,
            duration_ms: 2_500,
            ..Generation::default()
        });
        let text = rendered(&mut state, 80, 14);
        assert!(text.contains("cache hit 90%"), "{text}");
        assert!(text.contains("ttft 500ms"), "{text}");
        // 300 tokens in two seconds of generation is 150; the same 300 over the request's whole
        // 2.5 seconds is 120. The pair is the point: the model ran at 150 and the reader waited
        // at 120, and the half-second wait is exactly the difference.
        assert!(text.contains("last 150/120 tok/s"), "{text}");
        assert!(text.contains("avg 150/120 tok/s"), "{text}");
        let rows: Vec<&str> = text.lines().collect();
        let composer = rows
            .iter()
            .position(|row| row.contains("message"))
            .expect("the composer is drawn");
        let throughput = rows
            .iter()
            .position(|row| row.contains("cache hit"))
            .expect("the stats are drawn");
        let status = rows
            .iter()
            .position(|row| row.contains("ready"))
            .expect("the status line is drawn");
        assert!(
            composer < throughput && throughput < status,
            "composer {composer}, stats {throughput}, status {status}:\n{text}"
        );
    }

    /// Readings give up their places whole, from the end, because half a reading is worse than one
    /// reading fewer: a terminal a few columns too narrow otherwise shows `avg 150` with the unit
    /// lopped off, which is not a number anyone can use. What goes first is the cache share — the
    /// context for the rates rather than a rate — and the two rates are what the row is for.
    #[test]
    fn a_narrow_terminal_drops_whole_readings_rather_than_half_of_one() {
        let mut state = ViewState::new();
        state.stats.record(Generation {
            completion_tokens: 300,
            cache_hit_tokens: 900,
            cache_miss_tokens: 100,
            ttft_ms: 500,
            decode_ms: 2_000,
            duration_ms: 2_500,
            ..Generation::default()
        });
        // All four readings need 71 columns; these three need 53.
        let narrow = rendered(&mut state, 60, 14);
        assert!(
            !narrow.contains("cache hit"),
            "the cache share gave up its place: {narrow}"
        );
        assert!(narrow.contains("last 150/120 tok/s"), "{narrow}");
        assert!(narrow.contains("avg 150/120 tok/s"), "{narrow}");
        assert!(narrow.contains("ttft 500ms"), "{narrow}");
    }

    /// Nothing measured is drawn as a dash rather than as a zero. A zero is a
    /// measurement — it says the model generated nothing — and a reader whose first
    /// request has not finished has not been told that.
    #[test]
    fn the_throughput_line_shows_a_dash_for_what_it_has_not_measured() {
        let mut state = ViewState::new();
        let text = rendered(&mut state, 60, 14);
        assert!(text.contains("cache hit \u{2014}"), "{text}");
        assert!(text.contains("ttft \u{2014}"), "{text}");
        assert!(text.contains("last \u{2014}/\u{2014} tok/s"), "{text}");
        assert!(text.contains("avg \u{2014}/\u{2014} tok/s"), "{text}");
        assert!(
            !text.contains("cache hit 0%"),
            "not a zero it never measured"
        );
    }

    /// A wait is printed in milliseconds until it reaches a second and in seconds after that,
    /// because a tenth of a second is all this row can use but rounding a short wait down to
    /// `0.0s` would read as no wait at all — which is the opposite of what a slow prefill is.
    #[test]
    fn a_wait_is_shown_in_the_unit_that_does_not_round_it_away() {
        assert_eq!(show_duration(Some(40)), "40ms");
        assert_eq!(show_duration(Some(500)), "500ms");
        assert_eq!(show_duration(Some(999)), "999ms");
        assert_eq!(show_duration(Some(1_000)), "1.0s");
        assert_eq!(show_duration(Some(1_250)), "1.2s");
        assert_eq!(show_duration(Some(15_342)), "15.3s");
        // The other direction: a wait nobody measured is a dash, not a zero.
        assert_eq!(show_duration(None), "\u{2014}");
    }

    /// The throughput line gives up its row before the composer gives up one of its own.
    /// A terminal with rows to spare shows both; one without shows the row the reader is
    /// typing on, which is the one thing on this screen they cannot do without.
    #[test]
    fn the_throughput_line_yields_its_row_to_the_composer() {
        let mut state = ViewState::new();
        state.stats.record(Generation {
            completion_tokens: 300,
            cache_hit_tokens: 900,
            cache_miss_tokens: 100,
            ttft_ms: 500,
            decode_ms: 2_000,
            duration_ms: 2_500,
            ..Generation::default()
        });
        // Nine rows is the least that holds the title, the transcript's floor, an empty
        // composer, the throughput line, and the status line.
        let roomy = rendered(&mut state, 60, 9);
        assert!(
            roomy.contains("last 150/120 tok/s"),
            "rows to spare: {roomy}"
        );
        assert!(roomy.contains('›'), "and the composer too: {roomy}");
        let cramped = rendered(&mut state, 60, 8);
        assert!(
            !cramped.contains("tok/s"),
            "the stats go without: {cramped}"
        );
        assert!(
            cramped.contains('›'),
            "the composer keeps its row: {cramped}"
        );
    }

    #[test]
    fn a_long_conversation_opens_at_its_beginning() {
        // Thirty entries at two rows each is sixty rows, so the transcript does not
        // fit a small viewport. Offset zero is the start, by the convention above.
        let entries: Vec<Entry> = (0..30)
            .map(|index| Entry::prose(Role::User, format!("entry {index}")))
            .collect();
        let mut state = state_with(entries);
        let text = rendered(&mut state, 40, 20);
        assert!(text.contains("entry 0"), "the beginning is shown");
        assert!(!text.contains("entry 29"), "the end is off-screen");
        assert_eq!(state.scroll_offset, 0);
    }

    #[test]
    fn a_notice_appended_to_a_long_conversation_is_visible() {
        // Opening a recorded session scrolls a long transcript to its end. Submitting
        // into one appends a notice and scrolls again, and that notice is the only
        // feedback the user gets, so it has to land on screen rather than below it.
        let entries: Vec<Entry> = (0..30)
            .map(|index| Entry::prose(Role::User, format!("{} entry-{index}", "x".repeat(300))))
            .collect();
        let mut state = state_with(entries);
        // What the recorded view does: open at the end.
        state.pending_scroll_back = Some(0);
        drop(rendered(&mut state, 40, 20));

        state.transcript.push(Entry::notice("SENTINEL NOTICE"));
        state.scroll_to_bottom();
        let text = rendered(&mut state, 40, 20);
        assert!(text.contains("SENTINEL NOTICE"), "{text}");
    }

    #[test]
    fn scrolling_forward_reaches_the_newest_entry() {
        let entries: Vec<Entry> = (0..30)
            .map(|index| Entry::prose(Role::User, format!("entry {index}")))
            .collect();
        let mut state = state_with(entries);
        // Render once so the view knows the real transcript area. Offset zero is the
        // beginning of the conversation, by the convention above.
        drop(rendered(&mut state, 40, 20));
        assert_eq!(state.scroll_offset, 0, "the beginning is shown first");

        // At the top there is nowhere older to go, so a backward step clamps.
        state.scroll_by(-4, 16, 40);
        assert_eq!(state.scroll_offset, 0, "already at the oldest content");

        // Forward, toward the newest.
        state.scroll_by(4, 16, 40);
        assert_eq!(state.scroll_offset, 4, "one step of four rows");
        let text = rendered(&mut state, 40, 20);
        assert!(
            text.contains("entry 1") || text.contains("entry 2"),
            "the view moved on from the first entry: {text}"
        );

        // And enough forward steps reach the end.
        for _ in 0..40 {
            state.scroll_by(4, 16, 40);
        }
        assert_eq!(
            state.scroll_offset,
            state.max_scroll(),
            "stepping forward reaches the newest content"
        );
    }

    #[test]
    fn scroll_to_bottom_follows_the_newest_output() {
        let entries: Vec<Entry> = (0..30)
            .map(|index| Entry::prose(Role::User, format!("entry {index}")))
            .collect();
        let mut state = state_with(entries);
        // Before the viewport is known, this is a no-op rather than a guess.
        state.scroll_to_bottom();
        assert_eq!(state.scroll_offset, 0);

        drop(rendered(&mut state, 40, 20));
        state.scroll_to_bottom();
        let text = rendered(&mut state, 40, 20);
        assert!(text.contains("entry 29"), "the newest entry is shown");
        assert_eq!(state.scroll_offset, state.max_scroll());
    }

    #[test]
    fn scroll_to_top_returns_to_the_beginning() {
        let entries: Vec<Entry> = (0..30)
            .map(|index| Entry::prose(Role::User, format!("entry {index}")))
            .collect();
        let mut state = state_with(entries);
        state.scroll_to_bottom();
        // Still a no-op: no viewport has been reported yet.
        assert_eq!(state.scroll_offset, 0);
        state.scroll_by(0, 20, 40);
        state.scroll_to_bottom();
        assert!(state.scroll_offset > 0);
        state.scroll_to_top();
        assert_eq!(state.scroll_offset, 0);
    }

    #[test]
    fn a_stale_scroll_offset_is_clamped_when_the_viewport_grows() {
        let entries: Vec<Entry> = (0..5)
            .map(|index| Entry::prose(Role::User, format!("e{index}")))
            .collect();
        let mut state = state_with(entries);
        // Scroll further down than the transcript allows, so the offset is at its
        // maximum for a small viewport...
        state.scroll_by(1000, 4, 40);
        let scrolled = state.scroll_offset;
        assert!(scrolled > 0);

        // ...then render with a viewport tall enough to show everything. Without
        // clamping, the offset would address rows that no longer exist.
        let text = rendered(&mut state, 40, 60);
        assert_eq!(state.scroll_offset, 0);
        assert!(text.contains("e0"));
    }

    #[test]
    fn scrolling_clamps_at_both_ends() {
        let entries: Vec<Entry> = (0..5)
            .map(|index| Entry::prose(Role::User, format!("e{index}")))
            .collect();
        let mut state = state_with(entries);

        // Scrolling toward older content clamps at the start.
        state.scroll_by(-1000, 10, 40);
        assert_eq!(state.scroll_offset, 0);

        // Scrolling toward newer content clamps at the bottom.
        state.scroll_by(1000, 10, 40);
        assert_eq!(state.scroll_offset, state.max_scroll());
    }

    #[test]
    fn scroll_to_bottom_resets_the_offset() {
        let entries: Vec<Entry> = (0..30)
            .map(|index| Entry::prose(Role::User, format!("e{index}")))
            .collect();
        let mut state = state_with(entries);
        // A positive delta reveals older content, so this scrolls away from the
        // bottom before the assertions below bring it back.
        state.scroll_by(50, 10, 40);
        assert!(state.scroll_offset > 0);
        // At the bottom, the offset is the maximum for that viewport.
        state.scroll_to_bottom();
        assert_eq!(state.scroll_offset, state.max_scroll());
        // And scrolling to the top returns to zero.
        state.scroll_to_top();
        assert_eq!(state.scroll_offset, 0);
    }

    #[test]
    fn a_notice_renders_with_a_bullet() {
        let mut state = state_with(vec![Entry::notice("workspace-write enabled")]);
        let text = rendered(&mut state, 60, 12);
        assert!(text.contains('·'));
        assert!(text.contains("workspace-write enabled"));
    }

    #[test]
    fn the_composer_shows_the_cursor_position_without_moving_the_text() {
        // The bug this pins: the caret was drawn as a glyph in a column of its own, so
        // every character after it slid one place right whenever the cursor moved. In the
        // middle of a word being corrected that reads as the cursor displacing the text
        // it is moving across.
        let mut state = ViewState::new();
        state.input = InputBuffer::with_text("ab");
        state.input.move_home();
        state.input.move_right();
        let (text, caret) = draw_with_caret(&mut state, 60, 12);
        assert!(
            text.contains("ab"),
            "the letters stay where they were: {text}"
        );
        assert!(
            !text.contains('▏'),
            "no caret glyph is drawn at all: {text}"
        );
        let caret = caret.expect("the caret is drawn");
        assert_eq!(
            caret.symbol, "b",
            "over the character after the insertion point"
        );
        assert_eq!(
            usize::from(caret.row),
            text.lines()
                .position(|line| line.contains("ab"))
                .expect("the composer's row is drawn"),
            "on the row the text is drawn on"
        );
    }

    #[test]
    fn a_caret_at_the_end_of_a_full_row_is_drawn_on_the_next_one() {
        // The gap this closes: a row that is exactly full has no column past its last
        // character, so there was nothing to reverse and the cursor simply vanished for
        // the one keystroke in which a prompt crossed a row boundary.
        let mut state = ViewState::new();
        // A forty-column terminal inside the composer's borders leaves thirty-eight
        // columns, two of which the prompt takes on the first row: thirty-six characters
        // fill it exactly.
        state.input = InputBuffer::with_text(&"x".repeat(36));
        let (text, caret) = draw_with_caret(&mut state, 40, 12);
        let caret = caret.expect("the caret is drawn even when its row is full");
        assert_eq!(caret.symbol, " ", "a block where the next character goes");
        assert_eq!(caret.column, 1, "at the left edge of the row it wrapped to");
        let text_row = text
            .lines()
            .position(|line| line.contains("xxxx"))
            .expect("the text is drawn");
        assert_eq!(
            usize::from(caret.row),
            text_row.saturating_add(1),
            "on the row after the text: {text}"
        );
    }

    #[test]
    fn a_caret_at_the_end_of_a_full_line_moves_to_the_next_lines_indent() {
        // The same rule in the middle of a prompt. The character after the caret belongs
        // to the *next* line, so the caret belongs before that line's indent — not over
        // the first character of its text, which is what column zero of the next row
        // would mean if the caret's column ignored the prefix.
        let mut state = ViewState::new();
        state.input = InputBuffer::with_text(&format!("{}\nsecond", "x".repeat(36)));
        // To the end of the first line: home to its start, up a line, then along it.
        state.input.move_home();
        assert!(state.input.move_line_up(), "the caret moves up a line");
        state.input.move_end();
        assert_eq!(
            state.input.cursor_line_col(),
            (0, 36),
            "at the end of a full row"
        );

        let (text, caret) = draw_with_caret(&mut state, 40, 12);
        let caret = caret.expect("the caret is drawn");
        assert_eq!(
            caret.symbol, " ",
            "on the indent's first cell, not on the text"
        );
        assert_eq!(caret.column, 1, "the inner left edge");
        let row = text
            .lines()
            .position(|line| line.contains("second"))
            .expect("the second line is drawn");
        assert_eq!(
            usize::from(caret.row),
            row,
            "on the row the next line begins on, before its text: {text}"
        );
    }

    #[test]
    fn an_empty_composer_puts_the_caret_after_the_prompt() {
        // The empty prompt takes the fallback path, and it is the case that catches a
        // caret column read as row-relative: without the prefix counted, the caret sat on
        // the `›` itself.
        let mut state = ViewState::new();
        let (text, caret) = draw_with_caret(&mut state, 60, 12);
        assert!(text.contains('›'), "the prompt is drawn: {text}");
        let caret = caret.expect("the caret is drawn");
        assert_eq!(caret.symbol, " ", "a block after the prompt");
        assert_eq!(
            caret.column, 3,
            "one border, one inner column, two of prompt"
        );
    }

    #[test]
    fn the_composer_does_not_change_height_as_a_prompt_crosses_a_full_row() {
        // The caret's row is added when a prompt fills its row exactly, so the height has
        // to be the same on both sides of that boundary. Otherwise the composer would
        // flicker a row taller for the single keystroke that lands on it.
        let rows = |length: usize| {
            let mut state = ViewState::new();
            // Thirty-eight columns, as a forty-column terminal leaves inside the borders.
            state.input = InputBuffer::with_text(&"x".repeat(length));
            state.composer_rows(38)
        };
        assert_eq!(rows(35), 1, "one short of filling the row");
        assert_eq!(rows(36), 2, "exactly full: the caret gets a row of its own");
        assert_eq!(rows(37), 2, "and the next character wraps into that row");
    }

    #[test]
    fn the_caret_past_the_last_character_is_a_block_where_the_next_one_goes() {
        // There is no character to reverse at the end of a prompt, so the caret becomes
        // one cell: a block in the space the next keystroke will occupy.
        let mut state = ViewState::new();
        state.input = InputBuffer::with_text("ab");
        let (text, caret) = draw_with_caret(&mut state, 60, 12);
        assert!(text.contains("ab"), "{text}");
        assert_eq!(
            caret.map(|caret| caret.symbol),
            Some(String::from(" ")),
            "a block in the space the next character goes in"
        );
    }

    #[test]
    fn moving_the_cursor_through_a_word_never_changes_where_the_letters_are() {
        // The property behind the report, stated directly: the drawn text is the same
        // wherever the cursor is. Only the reversed cell moves.
        let mut state = ViewState::new();
        state.input = InputBuffer::with_text("corvid");
        let mut drawn = Vec::new();
        for _ in 0..=6 {
            let (text, caret) = draw_with_caret(&mut state, 60, 12);
            drawn.push((text, caret));
            state.input.move_left();
        }
        for (index, (text, _)) in drawn.iter().enumerate() {
            assert!(
                text.contains("corvid"),
                "the word is intact with the cursor {index} steps from the end: {text}"
            );
        }
        // And the caret did move: six steps of `move_left` visit six cells.
        let cells: Vec<Option<String>> = drawn
            .iter()
            .map(|(_, caret)| caret.as_ref().map(|caret| caret.symbol.clone()))
            .collect();
        assert_eq!(
            cells,
            vec![
                Some(String::from(" ")),
                Some(String::from("d")),
                Some(String::from("i")),
                Some(String::from("v")),
                Some(String::from("r")),
                Some(String::from("o")),
                Some(String::from("c")),
            ]
        );
    }

    /// A transcript long enough to scroll, one row per entry.
    fn a_long_conversation(count: usize) -> ViewState {
        let mut state = ViewState::new();
        for index in 0..count {
            state
                .transcript
                .push(Entry::prose(Role::User, format!("line {index}")));
        }
        state
    }

    #[test]
    fn the_composer_stops_at_five_rows_and_the_window_follows_the_caret() {
        // Five rows: a prompt worth writing gets room, and past that the conversation
        // keeps its share of the screen.
        let mut state = ViewState::new();
        state
            .input
            .insert_str("alpha\nbravo\ncharlie\ndelta\necho\nfoxtrot");
        assert_eq!(
            state.composer_rows(40),
            5,
            "six lines are drawn in five rows"
        );
        // The caret is on the last line, so the window has scrolled to it: the first
        // line is the one that fell off, and the line being typed is still on screen.
        let text = rendered(&mut state, 40, 24);
        assert!(
            text.contains("foxtrot"),
            "the caret's line is drawn: {text}"
        );
        assert!(
            !text.contains("alpha"),
            "the first line scrolled off: {text}"
        );
    }

    #[test]
    fn scrolling_up_stops_the_newest_output_dragging_the_view() {
        // The bug this pins: every streamed token called `scroll_to_bottom`, so a reader
        // who scrolled up during a turn was pulled back down on the next token. A
        // transcript cannot be read while it is being written unless scrolling away is
        // respected.
        let mut state = a_long_conversation(200);
        let _ = rendered(&mut state, 60, 20);
        state.scroll_to_bottom();
        state.scroll_to_bottom();
        let bottom = state.scroll_offset;
        assert!(bottom > 0, "the conversation is longer than the viewport");
        assert!(state.following, "a reader at the bottom is following");

        // A negative delta is what a Page-Up asks the runtime for.
        state.scroll(-10);
        let scrolled = state.scroll_offset;
        assert!(scrolled < bottom, "Page-Up moved back through the history");
        assert!(!state.following, "and stopped following");

        // New output arrives while the reader is looking further up.
        state
            .transcript
            .append_stream(Role::Assistant, "a new token", false);
        state.follow();
        assert_eq!(
            state.scroll_offset, scrolled,
            "the reader was left where they were"
        );

        // Scrolling back to the bottom resumes following, so no key has to be remembered.
        for _ in 0..8 {
            state.scroll(10);
        }
        assert!(state.following, "back at the bottom, following again");
        state
            .transcript
            .append_stream(Role::Assistant, "another", false);
        state.follow();
        assert_eq!(
            state.max_scroll(),
            state.scroll_offset,
            "and the newest line is in view"
        );
    }

    #[test]
    fn scrolling_to_the_top_does_not_follow() {
        // The other end of the same rule: the top is not the bottom.
        let mut state = a_long_conversation(200);
        let _ = rendered(&mut state, 60, 20);
        state.scroll_to_top();
        assert!(!state.following);
        state.follow();
        assert_eq!(state.scroll_offset, 0, "the view stayed at the top");
    }

    #[test]
    fn a_run_of_tool_calls_is_one_line_when_summarized() {
        let mut state = state_with(vec![
            Entry::prose(Role::User, "do the thing"),
            Entry::tool_call("read", "{}"),
            Entry::tool_result("read", false, "contents"),
            Entry::tool_call("grep", "{}"),
            Entry::tool_result("grep", true, "no match"),
            Entry::prose(Role::Assistant, "done"),
        ]);
        // Off by default: the detail is what a reader asked for by running a tool.
        let plain = rendered(&mut state, 80, 24);
        assert!(plain.contains("read"), "the call is drawn in full: {plain}");
        assert!(plain.contains("contents"), "and its result");

        state.collapse_tools = true;
        let folded = rendered(&mut state, 80, 24);
        assert!(
            folded.contains("2 tool calls"),
            "the run is counted: {folded}"
        );
        assert!(folded.contains("read, grep"), "and named: {folded}");
        assert!(
            folded.contains("1 failed"),
            "and failures survive: {folded}"
        );
        // The point of folding: the detail is gone.
        assert!(
            !folded.contains("contents"),
            "the output is hidden: {folded}"
        );
        // And a reader can see the toggle is on.
        assert!(folded.contains("summarized: tools"), "{folded}");
        // What was *not* asked for is untouched.
        assert!(folded.contains("do the thing"));
        assert!(folded.contains("done"));
    }

    #[test]
    fn a_run_of_reasoning_is_one_line_when_summarized() {
        let mut state = state_with(vec![Entry::prose(Role::Reasoning, "weighing options")]);
        let plain = rendered(&mut state, 80, 24);
        assert!(plain.contains("weighing options"));

        state.collapse_reasoning = true;
        let folded = rendered(&mut state, 80, 24);
        assert!(
            folded.contains("thinking · 1 part · 16 characters"),
            "the run is sized: {folded}"
        );
        assert!(!folded.contains("weighing options"), "{folded}");
        assert!(folded.contains("summarized: thinking"), "{folded}");
    }

    #[test]
    fn only_sequential_runs_are_summarized() {
        // "Sequential" is the whole of it: two tool calls with an answer between them are
        // two thoughts, and folding them into one line would misrepresent the turn.
        let mut state = state_with(vec![
            Entry::tool_call("read", "{}"),
            Entry::tool_result("read", false, "one"),
            Entry::prose(Role::Assistant, "a sentence between them"),
            Entry::tool_call("write", "{}"),
            Entry::tool_result("write", false, "two"),
        ]);
        state.collapse_tools = true;
        let folded = rendered(&mut state, 80, 24);
        assert_eq!(
            folded.matches("1 tool call ·").count(),
            2,
            "two runs of one: {folded}"
        );
        assert!(folded.contains("a sentence between them"), "{folded}");
    }

    #[test]
    fn the_composer_grows_with_its_lines_and_stops_at_the_cap() {
        let mut state = ViewState::new();
        assert_eq!(
            state.composer_rows(40),
            1,
            "an empty prompt is one blank line"
        );
        state.input.insert_str("a\nb");
        assert_eq!(state.composer_rows(40), 2, "a newline adds a row");
        for _ in 0..20 {
            state.input.insert('\n');
        }
        assert_eq!(
            state.composer_rows(40),
            MAX_COMPOSER_ROWS,
            "a tall prompt stops growing so the conversation keeps its room"
        );
    }

    #[test]
    fn wrapping_breaks_at_a_space_rather_than_inside_a_word() {
        let rows = wrap_words("alpha beta", 6, 6);
        let texts: Vec<&str> = rows.iter().map(|(text, _, _)| text.as_str()).collect();
        assert_eq!(texts, ["alpha", "beta"]);
        // The space a break lands on belongs to neither row, so the second row starts
        // one character later than the first one ends.
        assert_eq!(
            rows.first().map(|(_, start, end)| (*start, *end)),
            Some((0, 5))
        );
        assert_eq!(
            rows.get(1).map(|(_, start, end)| (*start, *end)),
            Some((6, 10))
        );
    }

    #[test]
    fn a_word_longer_than_a_row_is_broken_across_rows() {
        let rows = wrap_words("abcdefgh", 3, 3);
        let texts: Vec<&str> = rows.iter().map(|(text, _, _)| text.as_str()).collect();
        assert_eq!(texts, ["abc", "def", "gh"]);
    }

    #[test]
    fn an_empty_line_is_still_one_row() {
        let rows = wrap_words("", 10, 10);
        assert_eq!(rows.len(), 1);
        assert_eq!(caret_position(&rows, 0), (0, 0));
    }

    #[test]
    fn the_caret_follows_the_row_its_character_wrapped_onto() {
        let rows = wrap_words("alpha beta", 6, 6);
        assert_eq!(caret_position(&rows, 2), (0, 2), "inside the first word");
        // The space the break dropped: the caret is drawn at the end of the row it was
        // dropped from, which is where the next keystroke will appear.
        assert_eq!(caret_position(&rows, 5), (0, 5));
        assert_eq!(
            caret_position(&rows, 6),
            (1, 0),
            "the second row's first column"
        );
        assert_eq!(
            caret_position(&rows, 10),
            (1, 4),
            "past the end is the last row"
        );
    }

    #[test]
    fn the_caret_of_a_long_word_is_on_its_last_row() {
        // Two hundred characters at thirty-eight columns is five full rows and ten
        // characters over, so the caret at the end belongs on row five. This is the case
        // the estimate got wrong: a renderer that breaks a long word onto a fresh row
        // instead of filling the one before it puts that character on row six, and the
        // caret ends up one row below the composer's window.
        let text = "x".repeat(200);
        let rows = wrap_words(&text, 38, 38);
        assert_eq!(rows.len(), 6, "five full rows and a short one");
        assert_eq!(caret_position(&rows, 200), (5, 10));
    }

    #[test]
    fn the_composer_grows_with_a_wrapped_line_not_just_a_newline() {
        let mut state = ViewState::new();
        // No newline, but far more text than the width holds: the row count is rows,
        // so wrapping counts the same as an explicit break.
        state.input = InputBuffer::with_text(&"x".repeat(200));
        let rows = state.composer_rows(40);
        assert_eq!(rows, MAX_COMPOSER_ROWS, "a wrapped line grows the composer");
        // And the caret at the end of that one long line is still on screen, which is
        // what a line-based offset got wrong.
        let (text, caret) = draw_with_caret(&mut state, 40, 12);
        assert!(
            caret.is_some(),
            "the caret must stay visible when its line wraps: {text}"
        );
    }

    #[test]
    fn the_composer_scrolls_to_the_caret_of_a_wrapped_line() {
        let mut state = ViewState::new();
        let mut text = "y".repeat(500);
        text.push_str("\nlast");
        state.input = InputBuffer::with_text(&text);
        let (drawn, caret) = draw_with_caret(&mut state, 40, 12);
        // `with_text` leaves the caret at the end of the text, which is on the line
        // after a very tall wrapped one. The caret can only be on screen if the window
        // has scrolled, and it has to be *at* the caret rather than one row short of it:
        // the row a long word wraps onto is not the row a character count predicts.
        assert!(
            drawn.contains("last"),
            "the caret's line is visible: {drawn}"
        );
        let caret_row = caret.expect("the caret is drawn").row;
        let last_row = drawn
            .lines()
            .position(|line| line.contains("last"))
            .expect("the wrapped tail is drawn");
        assert_eq!(
            usize::from(caret_row),
            last_row,
            "the caret is on the row with the text it follows: {drawn}"
        );
    }

    #[test]
    fn a_multi_line_composer_draws_every_line() {
        let mut state = ViewState::new();
        state.input = InputBuffer::with_text("first\nsecond");
        let text = rendered(&mut state, 40, 12);
        // An embedded newline must break the row rather than being drawn literally.
        assert!(text.contains("first"));
        assert!(text.contains("second"));
    }

    #[test]
    fn the_caret_is_drawn_on_the_line_it_is_on() {
        let mut state = ViewState::new();
        state.input = InputBuffer::with_text("ab\ncd");
        state.input.move_home();
        let (text, caret) = draw_with_caret(&mut state, 40, 12);
        assert!(text.contains("ab"), "the first line is drawn");
        assert!(text.contains("cd"), "and the second, undisplaced");
        let caret = caret.expect("the caret is drawn");
        assert_eq!(
            caret.symbol, "c",
            "over the first character of the second line"
        );
        assert_eq!(
            usize::from(caret.row),
            text.lines()
                .position(|line| line.contains("cd"))
                .expect("the second line is drawn"),
            "on the row that line is drawn on"
        );
    }

    #[test]
    fn a_tall_composer_scrolls_to_follow_the_cursor() {
        let prompt = (0..8)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut state = ViewState::new();
        state.input = InputBuffer::with_text(&prompt);
        let text = rendered(&mut state, 40, 20);
        // The cursor is at the end, so the window has scrolled past the first line to
        // keep the line being typed on screen.
        assert!(text.contains("line 7"), "the cursor's line is visible");
        assert!(
            !text.contains("line 0"),
            "the window followed the cursor off the top"
        );
    }

    #[test]
    fn a_narrow_terminal_still_renders() {
        // The narrowest useful terminal must not panic or produce nothing.
        let mut state = state_with(vec![Entry::prose(Role::User, "a fairly long line of text")]);
        let text = rendered(&mut state, 10, 8);
        assert!(text.contains("you") || text.contains("nanus"));
        assert!(!text.is_empty());
    }

    #[test]
    fn a_theme_maps_every_role_to_a_style() {
        let theme = Theme::default();
        for role in [
            Role::User,
            Role::Assistant,
            Role::Reasoning,
            Role::Tool,
            Role::Harness,
        ] {
            let style = theme.style_for(role);
            // Each role has *some* styling; an unstyled role would be invisible in a
            // monochrome-scanned transcript.
            assert!(style != Style::default() || role == Role::Harness);
        }
        // The answer is white, as asked for. Checked on the style rather than on a
        // capture, because a terminal that already draws in white emits no escape for
        // it and the rendering looks identical either way.
        assert_eq!(
            Theme::default().assistant.fg,
            Some(Color::White),
            "the model's answer is drawn in white"
        );
        let failure = Entry::tool_result("read", true, "x");
        assert_eq!(theme.style_for_entry(&failure), theme.error);
    }

    /// The caret is a modifier, so it has to survive a theme chosen for `NO_COLOR`: the
    /// point of that theme is that the caret is still drawn when the terminal asked for
    /// no colour, and a test that only checked the colours could not say so.
    #[test]
    fn the_caret_is_still_drawn_in_the_monochrome_theme() {
        let mut state = ViewState::new();
        state.theme = Theme::monochrome();
        state.input.insert_str("hi");
        let (_, caret) = draw_with_caret(&mut state, 60, 12);
        let caret = caret.expect("the caret is drawn without colour too");
        assert_eq!(caret.symbol, " ");
    }

    /// The monochrome theme exists so that a terminal which asked for no colour still
    /// gets a caret, and the way it manages that is by *asking for none*. The colour
    /// assertion is the one that earns its keep: a single `fg` left behind is enough for
    /// the backend to emit the command that clears the reversal.
    #[test]
    fn the_monochrome_theme_asks_for_no_colour_and_keeps_the_modifiers() {
        let colour = Theme::default();
        let mono = Theme::monochrome();
        let pairs = [
            (colour.user, mono.user),
            (colour.assistant, mono.assistant),
            (colour.reasoning, mono.reasoning),
            (colour.tool, mono.tool),
            (colour.notice, mono.notice),
            (colour.error, mono.error),
            (colour.busy, mono.busy),
        ];
        for (colour, mono) in pairs {
            assert_eq!(mono.fg, None, "no foreground colour is asked for");
            assert_eq!(mono.bg, None, "no background colour is asked for");
            assert_eq!(
                mono.add_modifier, colour.add_modifier,
                "bold and italic still mark the roles the colour theme marked"
            );
        }
    }
}

#[cfg(test)]
mod colour_tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::style::Color;

    use super::*;
    use crate::transcript::{Entry, Role};

    /// The colours the rendered cells actually carry.
    ///
    /// This asserts on the *buffer*, not on the theme, because the question is whether
    /// styling survives rendering. A theme that is correct in isolation and never applied
    /// is a defect that reading the theme would never reveal.
    #[test]
    fn roles_are_rendered_in_their_theme_colours() {
        let mut state = ViewState::new();
        state
            .transcript
            .push(Entry::prose(Role::User, "a question"));
        state
            .transcript
            .push(Entry::prose(Role::Assistant, "an answer"));
        state
            .transcript
            .push(Entry::prose(Role::Reasoning, "a thought"));

        let backend = TestBackend::new(60, 20);
        let mut terminal = Terminal::new(backend).expect("a test terminal always builds");
        let drawn = terminal.draw(|frame| state.render(frame));
        assert!(drawn.is_ok());

        let theme = Theme::default();
        let buffer = terminal.backend().buffer();
        // `Style::fg` is already an `Option<Color>`, so it is compared directly rather
        // than unwrapped: an unset colour must not match a theme colour.
        let carries = |wanted: Option<Color>| {
            (0..buffer.area.height).any(|row| {
                (0..buffer.area.width).any(|column| {
                    buffer.cell((column, row)).is_some_and(|cell| {
                        !cell.symbol().trim().is_empty() && cell.style().fg == wanted
                    })
                })
            })
        };

        // Each role's colour must appear somewhere. Without this a reader could not tell
        // a question from an answer without reading the labels.
        assert!(carries(theme.user.fg), "the user's colour is rendered");
        assert!(
            carries(theme.assistant.fg),
            "the assistant's colour is rendered"
        );
        assert!(
            carries(theme.reasoning.fg),
            "the reasoning colour is rendered"
        );
    }
}
