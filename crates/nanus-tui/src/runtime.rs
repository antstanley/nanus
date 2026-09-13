//! The interactive loop: a terminal, the keyboard, and a running harness.
//!
//! This module is behind the `runtime` feature because it is the only part of the
//! crate that needs a terminal and an agent. Everything the interface *shows* lives in
//! [`crate::view`] and is tested against a headless backend; what lives here is the
//! plumbing that connects it to a real keyboard and a real model.
//!
//! ## The event loop's shape
//!
//! Terminal applications fail in two ways that matter. The first is a panic that
//! leaves the terminal in raw mode with the alternate screen up, which is why the
//! terminal is restored in a guard rather than at the end of the function. The second
//! is blocking on a key while the model is streaming, which is why the keyboard and
//! the agent are two tasks and the interface redraws on either.
//!
//! ## Keys
//!
//! | Key | Effect |
//! |---|---|
//! | `Enter` | submit the composer |
//! | `Alt+Enter` | insert a newline |
//! | `Backspace` / `Delete` | delete a character |
//! | `Ctrl+W` | delete a word |
//! | `Up` / `Down` | move between lines, then browse submitted prompts |
//! | `PageUp` / `PageDown` | scroll the transcript |
//! | `Ctrl+L` | clear the transcript |
//! | `Ctrl+C` / `Ctrl+D` | quit |

use core::future::Future;
use std::io::{self, IsTerminal};
use std::path::Path;
use std::rc::Rc;

use crossterm::event::EventStream;
use futures::StreamExt as _;
use nanus_bundle::{AgentRunner, Harness, Progress};
use nanus_domain::{Session, ToolName, Usage};
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{
    Event as TerminalEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use tokio::sync::mpsc;

use crate::transcript::{Entry, Role};
use crate::view::ViewState;

/// Rows scrolled per `PageUp` or `PageDown`.
const PAGE_ROWS: i32 = 10;

/// A message from the agent to the interface.
#[derive(Debug)]
enum Update {
    /// A turn finished, with the answer and how it ended.
    Done(Result<String, String>),
    /// The agent produced progress.
    Event(AgentEvent),
}

/// One piece of progress from the agent.
#[derive(Debug)]
enum AgentEvent {
    /// Model text arrived.
    Text(String),
    /// Model reasoning arrived.
    Reasoning(String),
    /// A step began.
    Step(u32),
    /// A tool started.
    Tool(String),
    /// A tool finished.
    ToolDone(String, bool),
    /// Usage was reported.
    Usage(Usage),
}

/// Bridges the loop's synchronous [`Progress`] callbacks to an async channel.
///
/// The loop runs the agent on the same thread as the interface, so the channel is a
/// queue rather than a thread boundary. A full queue drops progress rather than
/// blocking the agent: an interface that cannot keep up must not slow the work down.
struct ChannelProgress {
    sender: mpsc::Sender<Update>,
}

impl Progress for ChannelProgress {
    fn text(&mut self, delta: &str) {
        self.send(AgentEvent::Text(delta.to_owned()));
    }

    fn reasoning(&mut self, delta: &str) {
        self.send(AgentEvent::Reasoning(delta.to_owned()));
    }

    fn step_started(&mut self, step: u32) {
        self.send(AgentEvent::Step(step));
    }

    fn tool_started(&mut self, name: &ToolName) {
        self.send(AgentEvent::Tool(name.as_str().to_owned()));
    }

    fn tool_finished(&mut self, name: &ToolName, is_error: bool) {
        self.send(AgentEvent::ToolDone(name.as_str().to_owned(), is_error));
    }

    fn usage(&mut self, usage: &Usage) {
        self.send(AgentEvent::Usage(*usage));
    }
}

impl ChannelProgress {
    /// Queues one event, dropping it when the interface is behind.
    fn send(&self, event: AgentEvent) {
        if self.sender.try_send(Update::Event(event)).is_err() {
            // Falling behind is a rendering problem, not a work problem.
            tracing::trace!("the interface is behind; dropping a progress event");
        }
    }
}

/// Restores the terminal when it goes out of scope.
///
/// A guard rather than a call at the end of the function, because a panic between
/// entering raw mode and leaving it would otherwise leave the user's terminal unusable
/// — the failure that makes a TUI feel broken even after it is fixed.
struct TerminalGuard {
    terminal: DefaultTerminal,
}

impl TerminalGuard {
    /// Enters raw mode and the alternate screen.
    fn enter() -> Self {
        Self {
            terminal: ratatui::init(),
        }
    }

    /// Returns the terminal to draw into.
    fn terminal(&mut self) -> &mut DefaultTerminal {
        &mut self.terminal
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        ratatui::restore();
    }
}

/// Where the interface gets its conversation from.
///
/// Two implementations, and the difference is the whole reason this trait exists: a
/// composed harness can *run* a turn, and a recorded session cannot. Keeping that apart
/// means browsing a transcript neither needs an API key nor pretends to be able to talk
/// to a model.
pub trait SessionSource {
    /// The session to display.
    fn session(&self) -> &Session;

    /// The runner that can extend it, when there is one.
    ///
    /// `None` means the interface is reading rather than driving: a submission is
    /// refused with an explanation instead of being silently dropped.
    fn runner(&self) -> Option<&Rc<AgentRunner>> {
        None
    }

    /// Rows to scroll back from the end when the interface opens.
    ///
    /// A conversation opens at its end, which is where the answer is; a reader who wants
    /// to show or review the *middle* of one — the reasoning and the tool calls — needs a
    /// way to start there rather than scrolling by hand.
    fn initial_scroll(&self) -> u32 {
        0
    }

    /// Releases whatever the source owns.
    ///
    /// # Errors
    ///
    /// Returns a message when teardown fails. The interface has already been restored by
    /// the time this runs, so a failure is reported rather than fatal.
    fn shutdown(&self) -> Result<(), String> {
        Ok(())
    }
}

/// Opens a recorded session for reading.
///
/// With no id, the most recent session is opened, which is what a bare
/// `nanus tui --session` means.
///
/// # Errors
///
/// Returns a message when the store cannot be read or the session does not exist. An id
/// that does not exist is only reported as missing — `nanus sessions` is what lists the
/// ones that do, so a mistyped uuid is worth checking against it.
pub fn view(
    store: &nanus_ports::StoreHandle,
    id: Option<&str>,
    scroll_back: u32,
) -> Result<(), String> {
    let store = Rc::clone(store);
    let requested = id.map(str::to_owned);
    let session = block_on(async move {
        let chosen = match requested {
            Some(raw) => Some(nanus_domain::SessionId::new(raw)),
            None => store
                .list()
                .await
                .map_err(|error| error.to_string())?
                .into_iter()
                .max_by_key(|summary| summary.last_event_at_ms)
                .map(|summary| summary.id),
        };
        let Some(id) = chosen else {
            return Err(String::from("no sessions recorded; run a task first"));
        };
        store.load(&id).await.map_err(|error| error.to_string())
    })?;
    let recording = Recording {
        session,
        scroll_back,
    };
    run_source(&recording).map_err(|error| error.to_string())
}

/// Drives a future to completion on the kernel runtime.
///
/// The view path is synchronous and `main` has already entered the runtime, so this is
/// `block_on` on the same thread rather than a second runtime.
fn block_on<F: Future>(future: F) -> F::Output {
    nanus_kernel::runtime::block_on(future)
}

/// A recorded session, opened for reading.
pub struct Recording {
    session: Session,
    /// Rows to scroll back from the end when the interface opens.
    scroll_back: u32,
}

impl Recording {
    /// Wraps a recorded session, opening at its end.
    #[must_use]
    pub const fn new(session: Session) -> Self {
        Self {
            session,
            scroll_back: 0,
        }
    }

    /// Opens `rows` back from the end, for reviewing the middle of a conversation.
    #[must_use]
    pub const fn scrolled_back(mut self, rows: u32) -> Self {
        self.scroll_back = rows;
        self
    }
}

impl SessionSource for Recording {
    fn session(&self) -> &Session {
        &self.session
    }

    fn initial_scroll(&self) -> u32 {
        self.scroll_back
    }
}

/// Whether there is a terminal to draw on and a keyboard to read.
///
/// Both ends are checked, because the interface needs both. `is_terminal` rather than a
/// `tty` call, which keeps the workspace's `unsafe` ban intact and avoids a
/// platform-specific branch.
///
/// This is asked *before* the terminal is taken, because taking one that is not there
/// does not fail politely: `ratatui::init` panics, so a piped invocation would abort
/// with a message about the drawing library rather than about the missing terminal.
#[must_use]
pub fn interactive() -> bool {
    io::stdout().is_terminal() && io::stdin().is_terminal()
}

/// Runs the interactive interface against `harness`, in `workspace`.
///
/// The workspace is passed in rather than read from the current directory, so that the
/// interface and a headless `nanus run` agree about which directory a session belongs
/// to when the configuration names one.
///
/// # Errors
///
/// Returns an error when the terminal cannot be put into raw mode or an event cannot
/// be read. The terminal is restored either way.
pub fn run(harness: &Harness, workspace: &Path) -> io::Result<()> {
    let session = harness.new_session(workspace);
    let source = Live { harness, session };
    run_source(&source)
}

/// A live harness plus the session the interface is driving.
struct Live<'a> {
    harness: &'a Harness,
    session: Session,
}

impl SessionSource for Live<'_> {
    fn session(&self) -> &Session {
        &self.session
    }

    fn runner(&self) -> Option<&Rc<AgentRunner>> {
        Some(&self.harness.runner)
    }

    fn shutdown(&self) -> Result<(), String> {
        self.harness.shutdown().map_err(|error| error.to_string())
    }
}

/// Refuses to take a terminal that is not there.
///
/// A pure function of whether a terminal exists, so the decision can be tested without
/// one — and, more importantly, without a test that would take over the terminal when the
/// suite happens to be run from one.
///
/// # Errors
///
/// Returns the message a user sees when there is nowhere to draw.
fn require_terminal(present: bool) -> io::Result<()> {
    if present {
        return Ok(());
    }
    Err(io::Error::other(
        "the interactive interface needs a terminal on stdin and stdout",
    ))
}

/// Runs the interface for any [`SessionSource`].
///
/// # Errors
///
/// Returns an error when the terminal cannot be put into raw mode or an event cannot be
/// read. The terminal is restored either way.
pub fn run_source(source: &dyn SessionSource) -> io::Result<()> {
    // Checked here as well as by the caller, because this is the function that takes the
    // terminal. A guard in the caller protects the paths that exist today; this protects
    // the ones added later, and it is the last point at which the answer is still an
    // error rather than a panic.
    require_terminal(interactive())?;
    // The loop runs on the kernel runtime with a local task set, because a submitted
    // prompt becomes a `!Send` local task: the agent's state is `Rc`-shared and the kernel
    // is single-threaded, so `tokio::spawn` cannot carry it. `spawn_local` is both legal
    // and *driven* only inside a local set, and a set that is merely entered never polls
    // what it spawned — so the set has to own the `block_on`.
    let outcome = nanus_kernel::runtime::block_on_local(event_loop(source));
    // Torn down outside the runtime: shutting the composition down drives its own
    // `block_on`, which cannot be nested inside a running one. A shutdown that fails still
    // exits — the terminal has already been restored by the guard, and the run is over.
    if let Err(error) = source.shutdown() {
        tracing::warn!(%error, "the composition did not shut down cleanly");
    }
    outcome
}

/// The event loop: draw, then wait for a keystroke or for the turn to make progress.
///
/// Asynchronous rather than a blocking poll for input, and that is the whole point. A
/// turn runs as a local task on this same thread, so waiting synchronously for a key — or
/// sleeping for a redraw tick — would stop the model's stream from being polled at all.
/// The interface would show a frozen turn and then deliver the entire answer at once,
/// which is exactly the freeze the local task exists to prevent.
async fn event_loop(source: &dyn SessionSource) -> io::Result<()> {
    let mut guard = TerminalGuard::enter();
    let mut view = ViewState::new();
    // The transcript comes from the session itself, so a recorded one looks exactly like
    // the live conversation it was: same event log, same rendering. The one difference is
    // the header, which belongs to a recording and to nothing else.
    let viewing_only = source.runner().is_none();
    view.transcript = if viewing_only {
        crate::replay::recording_of(source.session())
    } else {
        crate::replay::transcript_of(source.session())
    };
    view.tokens_used = u64::from(source.session().usage_totals().total_tokens());
    // A conversation opens at its end, where the answer is. The viewport has to be
    // recorded first, because "the bottom" depends on how many rows exist and how many
    // the terminal shows — without this the offset stays zero and the reader is left at
    // the beginning of the conversation.
    // Opening at the end, or part way back from it: the offset is applied on the first
    // render, when the viewport it is measured against is known.
    view.pending_scroll_back = Some(source.initial_scroll());
    if viewing_only {
        view.status = String::from("viewing a recorded session · Ctrl-C quits");
    }

    let (sender, mut receiver) = mpsc::channel::<Update>(256);
    let runner: Option<Rc<AgentRunner>> = source.runner().map(Rc::clone);
    let mut events = EventStream::new();

    loop {
        guard.terminal().draw(|frame| view.render(frame))?;
        // Redrawn after every wake-up rather than on a timer: the interface has no
        // animation, so every reason to redraw is either a keystroke or progress from the
        // turn, and both arrive here. A tick would only add idle wake-ups.
        tokio::select! {
            event = events.next() => {
                let Some(event) = event else {
                    // The event stream ended, which means there is no keyboard left to
                    // read. Leaving is the only sensible answer.
                    break;
                };
                let TerminalEvent::Key(key) = event? else {
                    continue;
                };
                // Windows reports both press and release; only a press is a keystroke.
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match handle_key(key, &mut view) {
                    Outcome::Quit => break,
                    Outcome::Submit(prompt) => {
                        let Some(runner) = runner.as_ref() else {
                            // Submitting in a recorded session would need a model this
                            // interface does not have. Saying so beats silently discarding
                            // what the user typed.
                            view.transcript.push(Entry::notice(
                                "this is a recorded session; start `nanus tui` without --session to continue it",
                            ));
                            view.scroll_to_bottom();
                            continue;
                        };
                        // The transcript is seeded here, where the mutable view lives.
                        view.transcript
                            .push(Entry::prose(Role::User, prompt.clone()));
                        view.begin_turn(1);
                        view.scroll_to_bottom();
                        spawn_turn(runner, source.session(), prompt, &sender);
                    }
                    Outcome::Continue => {}
                }
            }
            update = receiver.recv() => {
                // The sender is cloned into every turn and lives in this scope, so it
                // cannot be dropped while the loop runs.
                let Some(update) = update else {
                    continue;
                };
                apply(update, &mut view);
                // Progress arrives in bursts — one message per streamed fragment — and
                // each redraw costs a full frame, so the queue is emptied before the next
                // one.
                drain_updates(&mut receiver, &mut view);
            }
        }
    }
    Ok(())
}

/// What a keystroke asked for.
enum Outcome {
    /// Keep going.
    Continue,
    /// Leave.
    Quit,
    /// Send this prompt.
    Submit(String),
}

/// Applies one keystroke to the view.
fn handle_key(key: KeyEvent, view: &mut ViewState) -> Outcome {
    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    match key.code {
        KeyCode::Char('c' | 'd') if control => Outcome::Quit,
        KeyCode::Char('l') if control => {
            view.transcript.clear();
            Outcome::Continue
        }
        KeyCode::Char('w') if control => {
            view.input.delete_word();
            Outcome::Continue
        }
        KeyCode::Enter if alt => {
            view.input.insert('\n');
            Outcome::Continue
        }
        // An empty composer is not an error; it just does nothing.
        KeyCode::Enter => view
            .input
            .submit()
            .map_or(Outcome::Continue, Outcome::Submit),
        KeyCode::Char(character) => {
            view.input.insert(character);
            Outcome::Continue
        }
        KeyCode::Backspace => {
            view.input.backspace();
            Outcome::Continue
        }
        KeyCode::Delete => {
            view.input.delete();
            Outcome::Continue
        }
        KeyCode::Left => {
            view.input.move_left();
            Outcome::Continue
        }
        KeyCode::Right => {
            view.input.move_right();
            Outcome::Continue
        }
        KeyCode::Home => {
            view.input.move_home();
            Outcome::Continue
        }
        KeyCode::End => {
            view.input.move_end();
            Outcome::Continue
        }
        KeyCode::Up => {
            // Inside a multi-line prompt an arrow moves between lines; only from the
            // top line does it browse history, which is what keeps the common
            // single-line case behaving exactly as it did.
            if !view.input.move_line_up() {
                view.input.history_previous();
            }
            Outcome::Continue
        }
        KeyCode::Down => {
            if !view.input.move_line_down() {
                view.input.history_next();
            }
            Outcome::Continue
        }
        KeyCode::PageUp => {
            view.scroll(PAGE_ROWS);
            Outcome::Continue
        }
        KeyCode::PageDown => {
            view.scroll(-PAGE_ROWS);
            Outcome::Continue
        }
        KeyCode::Esc => Outcome::Quit,
        _ => Outcome::Continue,
    }
}

/// Starts a turn and streams its progress into the view.
///
/// The turn runs as a local task so the interface keeps responding: a model that takes
/// thirty seconds must not freeze the keyboard.
fn spawn_turn(
    runner: &Rc<AgentRunner>,
    session: &Session,
    prompt: String,
    sender: &mpsc::Sender<Update>,
) {
    let runner = Rc::clone(runner);
    let mut session = session.clone();
    let sender = sender.clone();
    tokio::task::spawn_local(async move {
        let mut progress = ChannelProgress {
            sender: sender.clone(),
        };
        let outcome = runner.run_turn(&mut session, &prompt, &mut progress).await;
        let message = match outcome {
            Ok(result) => Ok(result.answer),
            Err(error) => Err(error.to_string()),
        };
        // A receiver that has gone away means the interface is closing, so the failure
        // is not worth reporting.
        let _ignored = sender.send(Update::Done(message)).await;
    });
}

/// Applies one update to the view.
fn apply(update: Update, view: &mut ViewState) {
    match update {
        Update::Done(Ok(answer)) => {
            view.transcript.push(Entry::prose(Role::Assistant, answer));
            view.end_turn();
            view.scroll_to_bottom();
        }
        Update::Done(Err(message)) => {
            view.transcript.push(Entry::notice(message));
            view.end_turn();
        }
        Update::Event(AgentEvent::Text(delta)) => {
            view.transcript
                .append_stream(Role::Assistant, &delta, false);
            view.scroll_to_bottom();
        }
        Update::Event(AgentEvent::Reasoning(delta)) => {
            view.transcript
                .append_stream(Role::Reasoning, &delta, false);
            view.scroll_to_bottom();
        }
        Update::Event(AgentEvent::Step(step)) => view.begin_turn(step),
        Update::Event(AgentEvent::Tool(name)) => {
            view.transcript.push(Entry::tool_call(name, ""));
        }
        Update::Event(AgentEvent::ToolDone(name, is_error)) => {
            view.transcript
                .push(Entry::tool_result(name, is_error, "done"));
        }
        Update::Event(AgentEvent::Usage(usage)) => {
            view.add_tokens(usage.total_tokens());
        }
    }
}

/// Applies every already-queued update to the view, without waiting.
fn drain_updates(receiver: &mut mpsc::Receiver<Update>, view: &mut ViewState) {
    while let Ok(update) = receiver.try_recv() {
        apply(update, view);
    }
}

#[cfg(test)]
mod tests {
    use nanus_domain::{AgentConfig, SessionId, ToolRegistry};
    use nanus_ports::{ChatRequest, FinishReason, LlmEvent, LlmPort, LlmStream};

    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn ctrl_c_and_ctrl_d_quit() {
        let mut view = ViewState::new();
        assert!(matches!(
            handle_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL), &mut view),
            Outcome::Quit
        ));
        assert!(matches!(
            handle_key(key(KeyCode::Char('d'), KeyModifiers::CONTROL), &mut view),
            Outcome::Quit
        ));
        // `c` alone is a character, not a command.
        assert!(matches!(
            handle_key(key(KeyCode::Char('c'), KeyModifiers::NONE), &mut view),
            Outcome::Continue
        ));
        assert_eq!(view.input.text(), "c");
    }

    #[test]
    fn enter_submits_and_alt_enter_inserts_a_newline() {
        let mut view = ViewState::new();
        view.input.insert_str("hello");
        assert!(matches!(
            handle_key(key(KeyCode::Enter, KeyModifiers::NONE), &mut view),
            Outcome::Submit(prompt) if prompt == "hello"
        ));
        assert!(view.input.is_empty());

        view.input.insert_str("line");
        assert!(matches!(
            handle_key(key(KeyCode::Enter, KeyModifiers::ALT), &mut view),
            Outcome::Continue
        ));
        assert_eq!(view.input.text(), "line\n");
    }

    #[test]
    fn an_empty_enter_does_nothing() {
        let mut view = ViewState::new();
        assert!(matches!(
            handle_key(key(KeyCode::Enter, KeyModifiers::NONE), &mut view),
            Outcome::Continue
        ));
    }

    #[test]
    fn editing_keys_reach_the_composer() {
        let mut view = ViewState::new();
        for character in "abc".chars() {
            let _ = handle_key(key(KeyCode::Char(character), KeyModifiers::NONE), &mut view);
        }
        assert_eq!(view.input.text(), "abc");
        let _ = handle_key(key(KeyCode::Backspace, KeyModifiers::NONE), &mut view);
        assert_eq!(view.input.text(), "ab");
        let _ = handle_key(key(KeyCode::Home, KeyModifiers::NONE), &mut view);
        let _ = handle_key(key(KeyCode::Delete, KeyModifiers::NONE), &mut view);
        assert_eq!(view.input.text(), "b");
        let _ = handle_key(key(KeyCode::End, KeyModifiers::NONE), &mut view);
        assert!(view.input.is_at_end());
    }

    #[test]
    fn ctrl_w_deletes_a_word() {
        let mut view = ViewState::new();
        view.input.insert_str("read the file");
        let _ = handle_key(key(KeyCode::Char('w'), KeyModifiers::CONTROL), &mut view);
        assert_eq!(view.input.text(), "read the ");
    }

    #[test]
    fn ctrl_l_clears_the_transcript_but_not_the_draft() {
        let mut view = ViewState::new();
        view.transcript.push(Entry::prose(Role::User, "old"));
        view.input.insert_str("draft");
        let _ = handle_key(key(KeyCode::Char('l'), KeyModifiers::CONTROL), &mut view);
        assert!(view.transcript.is_empty());
        // Clearing the conversation must not discard what the user is typing.
        assert_eq!(view.input.text(), "draft");
    }

    #[test]
    fn history_keys_walk_through_submitted_prompts() {
        let mut view = ViewState::new();
        view.input.insert_str("first");
        let _ = handle_key(key(KeyCode::Enter, KeyModifiers::NONE), &mut view);
        let _ = handle_key(key(KeyCode::Up, KeyModifiers::NONE), &mut view);
        assert_eq!(view.input.text(), "first");
        let _ = handle_key(key(KeyCode::Down, KeyModifiers::NONE), &mut view);
        assert!(view.input.is_empty());
    }

    #[test]
    fn up_and_down_move_between_lines_before_history() {
        let mut view = ViewState::new();
        view.input.insert_str("one");
        let _ = handle_key(key(KeyCode::Enter, KeyModifiers::ALT), &mut view);
        view.input.insert_str("two");
        assert_eq!(view.input.cursor_line_col(), (1, 3));
        let _ = handle_key(key(KeyCode::Up, KeyModifiers::NONE), &mut view);
        assert_eq!(
            view.input.cursor_line_col(),
            (0, 3),
            "Up moves a line first"
        );
        // The top line is the end of the upward walk, so the next press falls back to
        // history rather than wrapping; the history is empty here.
        let _ = handle_key(key(KeyCode::Up, KeyModifiers::NONE), &mut view);
        assert_eq!(view.input.cursor_line_col(), (0, 3));
        let _ = handle_key(key(KeyCode::Down, KeyModifiers::NONE), &mut view);
        assert_eq!(view.input.cursor_line_col(), (1, 3), "Down returns");
    }

    #[test]
    fn escape_quits() {
        let mut view = ViewState::new();
        assert!(matches!(
            handle_key(key(KeyCode::Esc, KeyModifiers::NONE), &mut view),
            Outcome::Quit
        ));
    }

    #[test]
    fn an_unhandled_key_is_ignored() {
        let mut view = ViewState::new();
        assert!(matches!(
            handle_key(key(KeyCode::F(5), KeyModifiers::NONE), &mut view),
            Outcome::Continue
        ));
    }

    #[test]
    fn streaming_progress_lands_in_the_transcript() {
        let (sender, mut receiver) = mpsc::channel::<Update>(8);
        let mut progress = ChannelProgress { sender };
        progress.step_started(2);
        progress.text("hello");
        progress.reasoning("thinking");
        progress.usage(&Usage::default());

        let mut view = ViewState::new();
        drain_updates(&mut receiver, &mut view);
        // The step and usage events update status; the two deltas land as entries with
        // distinct roles, which is what lets a reader tell them apart.
        assert!(view.busy);
        assert!(
            view.transcript
                .entries()
                .iter()
                .any(|entry| entry.role() == Role::Reasoning)
        );
        assert!(
            view.transcript
                .entries()
                .iter()
                .any(|entry| entry.role() == Role::Assistant)
        );
        assert_eq!(view.step, 2);
    }

    #[test]
    fn a_finished_turn_settles_the_stream_and_closes_the_turn() {
        let (sender, mut receiver) = mpsc::channel::<Update>(8);
        let mut view = ViewState::new();
        view.begin_turn(1);
        assert!(
            sender
                .try_send(Update::Done(Ok("answer".to_owned())))
                .is_ok()
        );
        drain_updates(&mut receiver, &mut view);
        assert!(!view.busy);
        assert!(!view.transcript.is_streaming());
        let last = view.transcript.entries().last();
        assert!(last.is_some_and(|entry| entry.text() == "answer"));
    }

    #[test]
    fn a_failed_turn_is_reported_as_a_notice() {
        let (sender, mut receiver) = mpsc::channel::<Update>(8);
        let mut view = ViewState::new();
        view.begin_turn(1);
        assert!(
            sender
                .try_send(Update::Done(Err("boom".to_owned())))
                .is_ok()
        );
        drain_updates(&mut receiver, &mut view);
        assert!(!view.busy);
        assert!(
            view.transcript
                .entries()
                .iter()
                .any(|entry| entry.role() == Role::Harness && entry.text().contains("boom"))
        );
    }

    #[test]
    fn tool_events_become_transcript_entries() {
        let (sender, mut receiver) = mpsc::channel::<Update>(8);
        let mut view = ViewState::new();
        assert!(
            sender
                .try_send(Update::Event(AgentEvent::Tool("read".to_owned())))
                .is_ok()
        );
        assert!(
            sender
                .try_send(Update::Event(AgentEvent::ToolDone("read".to_owned(), true)))
                .is_ok()
        );
        drain_updates(&mut receiver, &mut view);
        assert_eq!(view.transcript.len(), 2);
        assert!(matches!(
            view.transcript.entries().first().map(Entry::kind),
            Some(crate::transcript::EntryKind::ToolCall { .. })
        ));
    }

    #[test]
    fn a_full_queue_drops_progress_rather_than_blocking() {
        // The channel is bounded, so a slow interface cannot apply backpressure to the
        // agent. Sending past the bound must not panic.
        let (sender, _receiver) = mpsc::channel::<Update>(1);
        let mut progress = ChannelProgress { sender };
        for index in 0..64 {
            progress.text(&format!("delta {index}"));
        }
    }

    #[test]
    fn the_interface_refuses_a_terminal_that_is_not_there() {
        // `ratatui::init` panics rather than returning when there is no terminal, so a
        // piped invocation used to abort with a message about the drawing library. The
        // refusal is what turns that into an error a person can read.
        let refused = require_terminal(false);
        assert!(refused.is_err(), "a missing terminal must be refused");
        let Err(error) = refused else {
            return;
        };
        assert!(error.to_string().contains("terminal"), "{error}");
        assert!(require_terminal(true).is_ok(), "a terminal is accepted");
    }

    /// A model that answers once with fixed text.
    struct ScriptedLlm;

    impl LlmPort for ScriptedLlm {
        fn model(&self) -> &'static str {
            "scripted"
        }

        fn stream_chat(&self, _request: ChatRequest) -> LlmStream {
            Box::pin(futures::stream::iter(vec![
                LlmEvent::TextDelta("hello back".to_owned()),
                LlmEvent::Finished {
                    reason: FinishReason::Stop,
                },
            ]))
        }
    }

    /// A runner whose model replies without a network.
    fn scripted_runner() -> AgentRunner {
        let llm: Rc<Box<dyn LlmPort>> = Rc::new(Box::new(ScriptedLlm));
        let config = AgentConfig::new(4, 1, "scripted", 4096)
            .unwrap_or_else(|error| panic!("valid config: {error}"));
        AgentRunner::new(llm, Rc::new(ToolRegistry::new()), "you are a test", config)
            .unwrap_or_else(|error| panic!("valid runner: {error}"))
    }

    #[test]
    fn a_submitted_prompt_is_driven_to_an_answer() {
        // The regression this pins, which nothing covered: submitting a prompt calls
        // `spawn_turn`, which spawns a `!Send` local task. `spawn_local` panics outside a
        // `LocalSet`, and a set that is only entered never polls what it spawned. The
        // first keystroke in a real terminal was therefore the first time this code had
        // ever run — and it aborted the process.
        let runner = Rc::new(scripted_runner());
        let session = Session::new(SessionId::new("tui-turn"), 0, "/tmp");
        let (sender, mut receiver) = mpsc::channel::<Update>(16);
        let answer = nanus_kernel::runtime::block_on_local(async {
            spawn_turn(&runner, &session, "hello".to_owned(), &sender);
            loop {
                match receiver.recv().await {
                    Some(Update::Done(result)) => break result,
                    // Streamed progress arrives first and is not the answer.
                    Some(_) => {}
                    None => panic!("the turn ended without a result"),
                }
            }
        });
        match answer {
            Ok(text) => assert_eq!(text, "hello back"),
            Err(error) => panic!("the turn failed: {error}"),
        }
    }
}
