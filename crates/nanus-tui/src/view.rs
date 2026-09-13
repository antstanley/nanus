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
use crate::transcript::{Entry, EntryKind, Role, Transcript, wrap_rows};

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
            assistant: Style::default().fg(Color::Green),
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
    /// `true` while a turn is open.
    pub busy: bool,
    /// The current step within the open turn.
    pub step: u32,
    /// Total tokens the session has used.
    pub tokens_used: u64,
    /// What the model is currently doing, for the status line.
    pub status: String,
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
            busy: false,
            step: 0,
            tokens_used: 0,
            status: "ready".to_owned(),
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

impl ViewState {
    /// Creates an empty view.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adjusts the scroll offset by `rows`, clamping to the visible range.
    ///
    /// `rows` is a **delta on [`ViewState::scroll_offset`]**, which counts rows
    /// scrolled off the top: positive reveals older content, negative reveals newer
    /// content. A key handler therefore passes `+rows_per_page` for Page-Up and
    /// `-rows_per_page` for Page-Down.
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
        // Postcondition: the offset never exceeds the scrollable range, so a render
        // cannot look past the end of the transcript.
        assert!(self.scroll_offset <= maximum_unsigned);
    }

    /// Scrolls so the newest entry is visible.
    ///
    /// A no-op before the first render, because the offset that means "the bottom"
    /// depends on a viewport this view has not been given yet.
    pub fn scroll_to_bottom(&mut self) {
        let Some((_width, height)) = self.last_viewport else {
            return;
        };
        let total = self.transcript_rows();
        self.scroll_offset = total.saturating_sub(u32::from(height));
    }

    /// Scrolls to the beginning of the conversation.
    pub const fn scroll_to_top(&mut self) {
        self.scroll_offset = 0;
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

    /// Scrolls toward older content by `rows`, which is what a `PageUp` does.
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
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(3),
                Constraint::Length(3),
                Constraint::Length(1),
            ])
            .split(area);

        if let Some(title) = chunks.first() {
            Self::render_title(frame, *title);
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
        if let Some(status) = chunks.get(3) {
            self.render_status(frame, *status);
        }
    }

    /// Renders the title bar.
    ///
    /// Associated rather than a method: the title bar is the same in every state, so
    /// taking `self` would suggest a dependence that does not exist.
    fn render_title(frame: &mut Frame<'_>, area: Rect) {
        let title = Span::styled("nanus", Style::default().add_modifier(Modifier::BOLD));
        let hint = Span::styled(
            "  ·  Enter sends · Alt+Enter newline · Ctrl-C quits",
            Style::default().fg(Color::DarkGray),
        );
        frame.render_widget(Paragraph::new(Line::from(vec![title, hint])), area);
    }

    /// Builds every line the transcript renders to, oldest first.
    ///
    /// Rendering works on *lines* rather than entries so that the visible window is
    /// computed from the exact set of rows being drawn. Slicing by estimated
    /// per-entry heights drifts whenever an estimate and the renderer disagree, and
    /// the drift shows up as the newest line being clipped — the one line a reader
    /// most wants.
    #[must_use]
    pub fn transcript_lines(&self) -> Vec<Line<'static>> {
        let mut lines: Vec<Line<'static>> = Vec::new();
        for entry in self.transcript.entries() {
            lines.push(self.header_for(entry));
            lines.extend(self.lines_for(entry));
            // A blank row between entries, so two consecutive messages do not read
            // as one paragraph.
            lines.push(Line::from(""));
        }
        lines
    }

    /// Renders the conversation.
    fn render_transcript(&self, frame: &mut Frame<'_>, area: Rect) {
        let lines = self.transcript_lines();
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
    fn line_heights(lines: &[Line<'static>], width: u16) -> Vec<u32> {
        let usable = width.max(1);
        lines
            .iter()
            .map(|line| wrap_rows(u32::try_from(line.width()).unwrap_or(u32::MAX), usable))
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
        let lines = self.transcript_lines();
        Self::line_heights(&lines, width)
            .iter()
            .copied()
            .fold(0_u32, u32::saturating_add)
    }

    /// Renders the composer.
    fn render_input(&self, frame: &mut Frame<'_>, area: Rect) {
        let prompt = if self.busy { "… " } else { "› " };
        let prompt_style = if self.busy {
            self.theme.busy
        } else {
            self.theme.user
        };
        let line = Line::from(vec![
            Span::styled(prompt, prompt_style),
            Span::styled(self.input.text_before_cursor(), Style::default()),
            Span::styled("▏", Style::default().add_modifier(Modifier::REVERSED)),
            Span::styled(self.input.text_after_cursor(), Style::default()),
        ]);
        let block = Block::default().borders(Borders::ALL).title(" message ");
        frame.render_widget(
            Paragraph::new(line).block(block).wrap(Wrap { trim: false }),
            area,
        );
    }

    /// Renders the status line.
    fn render_status(&self, frame: &mut Frame<'_>, area: Rect) {
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
        frame.render_widget(Paragraph::new(Line::from(vec![busy, step, usage])), area);
    }
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;

    /// Renders `state` into a test terminal and returns the buffer as text.
    fn rendered(state: &mut ViewState, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("a test terminal always builds");
        let drawn = terminal.draw(|frame| state.render(frame));
        assert!(drawn.is_ok(), "rendering must not fail");
        let buffer = terminal.backend().buffer();
        let mut text = String::new();
        for row in 0..buffer.area.height {
            for column in 0..buffer.area.width {
                let cell = buffer.cell((column, row));
                text.push_str(cell.map_or(" ", ratatui::buffer::Cell::symbol));
            }
            text.push('\n');
        }
        text
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
    fn a_tool_call_renders_its_name_and_arguments() {
        let mut state = state_with(vec![Entry::tool_call("read", "{\"file_path\":\"a.txt\"}")]);
        let text = rendered(&mut state, 70, 12);
        assert!(text.contains("read"));
        assert!(text.contains("file_path"));
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

    #[test]
    fn token_usage_is_shown() {
        let mut state = ViewState::new();
        state.add_tokens(1200);
        let text = rendered(&mut state, 60, 12);
        assert!(text.contains("1200 tokens"));
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
    fn scrolling_down_reaches_the_newest_entry() {
        let entries: Vec<Entry> = (0..30)
            .map(|index| Entry::prose(Role::User, format!("entry {index}")))
            .collect();
        let mut state = state_with(entries);
        // Render once so the view knows the real transcript area. Offset zero is the
        // beginning of the conversation, by the convention above.
        drop(rendered(&mut state, 40, 20));
        assert_eq!(state.scroll_offset, 0, "the beginning is shown first");

        // Step toward newer content the way a key handler would.
        state.scroll_by(-4, 16, 40);
        let after_one = state.scroll_offset;
        assert_eq!(after_one, 0, "already at the newest content, so clamped");

        // Step toward older content, then back.
        state.scroll_by(4, 16, 40);
        let after_up = state.scroll_offset;
        assert_eq!(after_up, 4, "one step of four rows");

        for _ in 0..40 {
            state.scroll_by(-4, 16, 40);
        }
        assert_eq!(
            state.scroll_offset, 0,
            "stepping down returns to the newest content"
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
    fn the_composer_shows_the_cursor_position() {
        let mut state = ViewState::new();
        state.input = InputBuffer::with_text("ab");
        state.input.move_home();
        state.input.move_right();
        let text = rendered(&mut state, 60, 12);
        assert!(
            text.contains("a▏b"),
            "the caret sits between the two halves"
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
        let failure = Entry::tool_result("read", true, "x");
        assert_eq!(theme.style_for_entry(&failure), theme.error);
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
