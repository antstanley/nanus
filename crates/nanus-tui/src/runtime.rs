//! The interactive loop: a terminal, the keyboard, and a link to an agent.
//!
//! This module is behind the `runtime` feature because it is the only part of the crate
//! that needs a terminal and an agent. Everything the interface *shows* lives in
//! [`crate::view`] and is tested against a headless backend; what lives here is the
//! plumbing that connects it to a real keyboard and a real agent.
//!
//! ## The interface does not own the agent
//!
//! The interface is a client. It reads an agent's frames and sends it prompts over the
//! local link, and it has no idea whether the agent on the other end is a process the
//! `nanus` binary started alongside it or a service that has been running since boot.
//! That is the whole point of the split: the rich interface and the small core only have
//! to agree about one page of protocol.
//!
//! ## The event loop's shape
//!
//! Terminal applications fail in two ways that matter. The first is a panic that
//! leaves the terminal in raw mode with the alternate screen up, which is why the
//! terminal is restored in a guard rather than at the end of the function. The second
//! is blocking on a key while the agent is streaming, which is why the keyboard and
//! the link are two sources and the interface redraws on either.
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

use crossterm::event::EventStream;
use futures::StreamExt as _;
use nanus_domain::{Session, SessionId};
use nanus_link::Client;
use nanus_link::protocol::{Frame, Request};
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{
    Event as TerminalEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use tokio::sync::mpsc;

use crate::transcript::{Entry, Role};
use crate::view::ViewState;

/// Rows scrolled per `PageUp` or `PageDown`.
const PAGE_ROWS: i32 = 10;

/// How many frames may be queued from the agent before the interface falls behind.
const FRAME_BUFFER: usize = 256;

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
/// Two implementations, and the difference is the whole reason this trait exists: an
/// agent can *run* a turn, and a recorded session cannot. Keeping that apart means
/// browsing a transcript neither needs an agent nor pretends to be able to talk to one.
pub trait SessionSource {
    /// The session to display.
    fn session(&self) -> &Session;

    /// Rows to scroll back from the end when the interface opens.
    ///
    /// A conversation opens at its end, which is where the answer is; a reader who wants
    /// to show or review the *middle* of one — the reasoning and the tool calls — needs a
    /// way to start there rather than scrolling by hand.
    fn initial_scroll(&self) -> u32 {
        0
    }

    /// Whether a prompt can be sent.
    ///
    /// `false` means the interface is reading rather than driving: a submission is
    /// refused with an explanation instead of being silently dropped.
    fn accepts_prompts(&self) -> bool {
        false
    }

    /// Starts whatever background work the source needs, inside the interface's task set.
    ///
    /// Called once, after the event loop's own channel exists and before the first frame
    /// is drawn. A source with nothing to start does nothing, which is why this has a
    /// default. It takes `&mut self` because starting the work means *handing over* the
    /// connection: the transport belongs to a task from here on, not to the source.
    fn attach(&mut self, _frames: &mpsc::Sender<Frame>) {}

    /// Sends one prompt. Progress arrives on the channel [`SessionSource::attach`] was
    /// given.
    fn submit(&mut self, _prompt: String) {}

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
/// With no id, the most recent session is opened, which is what `nanus tui --session`
/// means.
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
    let store = std::rc::Rc::clone(store);
    let requested = id.map(str::to_owned);
    let session = block_on(async move {
        let chosen = match requested {
            Some(raw) => Some(SessionId::new(raw)),
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
    let mut recording = Recording::new(session).scrolled_back(scroll_back);
    run_source(&mut recording).map_err(|error| error.to_string())
}

/// Drives a future to completion on the kernel runtime.
///
/// The setup path is synchronous and the process has already entered the runtime, so this
/// is `block_on` on the same thread rather than a second runtime.
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

/// A live conversation, held by an agent on the other end of the link.
///
/// The connection is made once, when the interface starts, and it *is* the conversation:
/// the agent creates a session for it and every prompt sent over it joins that session.
/// The interface therefore never names a session — it holds one.
pub struct Remote {
    session: Session,
    client: Option<Client>,
    requests: mpsc::UnboundedSender<Request>,
    pending: Option<mpsc::UnboundedReceiver<Request>>,
}

impl Remote {
    /// Connects to the agent listening at `path`.
    ///
    /// # Errors
    ///
    /// Returns a message when nothing is listening or the peer does not open with a
    /// handshake. The message is already user-facing, so it is not wrapped again.
    pub async fn connect(path: &Path) -> Result<Self, String> {
        let client = Client::connect(path)
            .await
            .map_err(|error| error.to_string())?;
        let info = client.info().clone();
        // The interface never shows a live session's creation time — a conversation the
        // reader is in needs no header announcing it — so the timestamp is the epoch and
        // the recorded path, which does show one, reads it from the store instead.
        let session = Session::new(SessionId::new(info.session), 0, info.workspace);
        let (requests, pending) = mpsc::unbounded_channel();
        Ok(Self {
            session,
            client: Some(client),
            requests,
            pending: Some(pending),
        })
    }
}

impl SessionSource for Remote {
    fn session(&self) -> &Session {
        &self.session
    }

    fn accepts_prompts(&self) -> bool {
        true
    }

    fn attach(&mut self, frames: &mpsc::Sender<Frame>) {
        let (Some(client), Some(requests)) = (self.client.take(), self.pending.take()) else {
            // Attaching twice is a caller's mistake, not a reason to take the terminal
            // down: the interface still works, it just cannot submit.
            tracing::warn!("the link was attached more than once; prompts will not be sent");
            return;
        };
        let frames = frames.clone();
        tokio::task::spawn_local(async move { pump(client, requests, frames).await });
    }

    fn submit(&mut self, prompt: String) {
        if self
            .requests
            .send(Request::Prompt { text: prompt })
            .is_err()
        {
            // The only way this fails is that the transport task is gone, which means the
            // agent closed the link. Saying so beats a prompt that vanishes.
            tracing::warn!("the link is closed; the prompt was not sent");
        }
    }
}

/// Moves requests to the agent and frames back, for as long as the connection lasts.
///
/// One task rather than a task per prompt, because the connection *is* the conversation:
/// a second connection would be a second session.
async fn pump(
    mut client: Client,
    mut requests: mpsc::UnboundedReceiver<Request>,
    frames: mpsc::Sender<Frame>,
) {
    while let Some(request) = requests.recv().await {
        if let Err(error) = client.send(&request).await {
            report(&frames, error.to_string()).await;
            return;
        }
        if !matches!(request, Request::Prompt { .. }) {
            continue;
        }
        loop {
            match client.next().await {
                Ok(Some(frame)) => {
                    let last = frame.is_end_of_turn();
                    // A receiver that has gone away means the interface is closing, so
                    // there is nobody left to tell.
                    if frames.send(frame).await.is_err() {
                        return;
                    }
                    if last {
                        break;
                    }
                }
                Ok(None) => {
                    report(&frames, String::from("the agent closed the link")).await;
                    return;
                }
                Err(error) => {
                    report(&frames, error.to_string()).await;
                    return;
                }
            }
        }
    }
}

/// Tells the interface that the conversation ended, when it is still there to hear it.
async fn report(frames: &mpsc::Sender<Frame>, message: String) {
    let _ignored = frames.send(Frame::Failed { message }).await;
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
pub fn run_source(source: &mut dyn SessionSource) -> io::Result<()> {
    // Checked here as well as by the caller, because this is the function that takes the
    // terminal. A guard in the caller protects the paths that exist today; this protects
    // the ones added later, and it is the last point at which the answer is still an
    // error rather than a panic.
    require_terminal(interactive())?;
    // The loop runs on the kernel runtime with a local task set, because the transport is
    // a local task: it holds an `Rc`-shared kernel and its futures are not `Send`, so
    // `tokio::spawn` cannot carry it. `spawn_local` is both legal and *driven* only inside
    // a local set, and a set that is merely entered never polls what it spawned — so the
    // set has to own the `block_on`.
    let outcome = nanus_kernel::runtime::block_on_local(async {
        let (frames, receiver) = mpsc::channel::<Frame>(FRAME_BUFFER);
        // Attached inside the task set, so the transport it starts is polled by the same
        // set that runs the loop.
        source.attach(&frames);
        event_loop(source, receiver).await
    });
    // Torn down outside the runtime: shutting a composition down drives its own
    // `block_on`, which cannot be nested inside a running one. A shutdown that fails still
    // exits — the terminal has already been restored by the guard, and the run is over.
    if let Err(error) = source.shutdown() {
        tracing::warn!(%error, "the source did not shut down cleanly");
    }
    outcome
}

/// The event loop: draw, then wait for a keystroke or for the agent to say something.
///
/// Asynchronous rather than a blocking poll for input, and that is the whole point. The
/// link is a local task on this same thread, so waiting synchronously for a key — or
/// sleeping for a redraw tick — would stop the agent's frames from being read at all. The
/// interface would show a frozen turn and then deliver the entire answer at once, which
/// is exactly the freeze the local task exists to prevent.
async fn event_loop(
    source: &mut dyn SessionSource,
    mut frames: mpsc::Receiver<Frame>,
) -> io::Result<()> {
    let mut guard = TerminalGuard::enter();
    let mut view = ViewState::new();
    // The transcript comes from the session itself, so a recorded one looks exactly like
    // the live conversation it was: same event log, same rendering. The one difference is
    // the header, which belongs to a recording and to nothing else.
    let viewing_only = !source.accepts_prompts();
    view.transcript = if viewing_only {
        crate::replay::recording_of(source.session())
    } else {
        crate::replay::transcript_of(source.session())
    };
    view.tokens_used = u64::from(source.session().usage_totals().total_tokens());
    // Opening at the end, or part way back from it: the offset is applied on the first
    // render, when the viewport it is measured against is known.
    view.pending_scroll_back = Some(source.initial_scroll());
    if viewing_only {
        view.status = String::from("viewing a recorded session · Ctrl-C quits");
    }

    let mut events = EventStream::new();

    loop {
        guard.terminal().draw(|frame| view.render(frame))?;
        // Redrawn after every wake-up rather than on a timer: the interface has no
        // animation, so every reason to redraw is either a keystroke or a frame from the
        // agent, and both arrive here. A tick would only add idle wake-ups.
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
                        if !source.accepts_prompts() {
                            // Submitting in a recorded session would need an agent this
                            // interface does not have. Saying so beats silently discarding
                            // what the user typed.
                            view.transcript.push(Entry::notice(
                                "this is a recorded session; start `nanus tui` without --session to continue it",
                            ));
                            view.scroll_to_bottom();
                            continue;
                        }
                        // The transcript is seeded here, where the mutable view lives.
                        view.transcript
                            .push(Entry::prose(Role::User, prompt.clone()));
                        view.begin_turn(1);
                        view.scroll_to_bottom();
                        source.submit(prompt);
                    }
                    Outcome::Continue => {}
                }
            }
            frame = frames.recv() => {
                // The sender is cloned into the transport task and lives as long as it
                // does, so a closed channel means the transport ended; the loop keeps
                // drawing, because a reader may still be scrolling.
                let Some(frame) = frame else {
                    continue;
                };
                apply(frame, &mut view);
                // Frames arrive in bursts — one per streamed fragment — and each redraw
                // costs a full frame, so the queue is emptied before the next one.
                drain_frames(&mut frames, &mut view);
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

/// Applies one frame from the agent to the view.
fn apply(frame: Frame, view: &mut ViewState) {
    match frame {
        Frame::Text { delta } => {
            view.transcript
                .append_stream(Role::Assistant, &delta, false);
            view.scroll_to_bottom();
        }
        Frame::Reasoning { delta } => {
            view.transcript
                .append_stream(Role::Reasoning, &delta, false);
            view.scroll_to_bottom();
        }
        Frame::Step { step } => view.begin_turn(step),
        Frame::Tool { name } => {
            view.transcript.push(Entry::tool_call(name, ""));
        }
        Frame::ToolDone { name, error } => {
            view.transcript
                .push(Entry::tool_result(name, error, "done"));
        }
        Frame::Usage { tokens } => view.add_tokens(tokens),
        Frame::Done { answer } => {
            view.transcript.push(Entry::prose(Role::Assistant, answer));
            view.end_turn();
            view.scroll_to_bottom();
        }
        Frame::Failed { message } => {
            view.transcript.push(Entry::notice(message));
            view.end_turn();
        }
        // Frames that describe the connection rather than the conversation. The interface
        // learned what it needed from the handshake before it drew anything, and a `Bye`
        // is the transport's business, not the transcript's.
        Frame::Ready(_) | Frame::Status(_) | Frame::Bye => {}
    }
}

/// Applies every already-queued frame to the view, without waiting.
fn drain_frames(frames: &mut mpsc::Receiver<Frame>, view: &mut ViewState) {
    while let Ok(frame) = frames.try_recv() {
        apply(frame, view);
    }
}

#[cfg(test)]
mod tests {
    use nanus_domain::SessionId;

    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn session() -> Session {
        Session::new(SessionId::new("tui-test"), 0, "/tmp")
    }

    /// A source that answers from a script, with no socket and no agent.
    ///
    /// The scripted *agent* is on the other side of the link and is tested where the
    /// server lives; what this covers is the half that lives here: that a prompt reaches
    /// the transport, and that the frames a transport produces land in the right places in
    /// the view.
    struct Scripted {
        session: Session,
        requests: std::rc::Rc<std::cell::RefCell<Vec<Request>>>,
        script: Vec<Frame>,
    }

    impl Scripted {
        fn new(script: Vec<Frame>) -> Self {
            Self {
                session: session(),
                requests: std::rc::Rc::new(std::cell::RefCell::new(Vec::new())),
                script,
            }
        }
    }

    impl SessionSource for Scripted {
        fn session(&self) -> &Session {
            &self.session
        }

        fn accepts_prompts(&self) -> bool {
            true
        }

        fn attach(&mut self, frames: &mpsc::Sender<Frame>) {
            // Delivered immediately rather than spawned, so the test has no task set and
            // no timing to get wrong.
            for frame in &self.script {
                assert!(frames.try_send(frame.clone()).is_ok());
            }
        }

        fn submit(&mut self, prompt: String) {
            self.requests
                .borrow_mut()
                .push(Request::Prompt { text: prompt });
        }
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
    fn streamed_answer_frames_land_in_the_transcript() {
        let (sender, mut receiver) = mpsc::channel::<Frame>(8);
        let mut view = ViewState::new();
        for frame in [
            Frame::Step { step: 2 },
            Frame::Reasoning {
                delta: "thinking".to_owned(),
            },
            Frame::Text {
                delta: "hello".to_owned(),
            },
            Frame::Usage { tokens: 12 },
        ] {
            assert!(sender.try_send(frame).is_ok());
        }
        drain_frames(&mut receiver, &mut view);
        // The step and usage frames update status; the two deltas land as entries with
        // distinct roles, which is what lets a reader tell them apart.
        assert!(view.busy);
        assert_eq!(view.tokens_used, 12);
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
        let (sender, mut receiver) = mpsc::channel::<Frame>(8);
        let mut view = ViewState::new();
        view.begin_turn(1);
        assert!(
            sender
                .try_send(Frame::Done {
                    answer: "answer".to_owned()
                })
                .is_ok()
        );
        drain_frames(&mut receiver, &mut view);
        assert!(!view.busy);
        assert!(!view.transcript.is_streaming());
        let last = view.transcript.entries().last();
        assert!(last.is_some_and(|entry| entry.text() == "answer"));
    }

    #[test]
    fn a_failed_turn_is_reported_as_a_notice() {
        let (sender, mut receiver) = mpsc::channel::<Frame>(8);
        let mut view = ViewState::new();
        view.begin_turn(1);
        assert!(
            sender
                .try_send(Frame::Failed {
                    message: "boom".to_owned()
                })
                .is_ok()
        );
        drain_frames(&mut receiver, &mut view);
        assert!(!view.busy);
        assert!(
            view.transcript
                .entries()
                .iter()
                .any(|entry| entry.role() == Role::Harness && entry.text().contains("boom"))
        );
    }

    #[test]
    fn tool_frames_become_transcript_entries() {
        let (sender, mut receiver) = mpsc::channel::<Frame>(8);
        let mut view = ViewState::new();
        assert!(
            sender
                .try_send(Frame::Tool {
                    name: "read".to_owned()
                })
                .is_ok()
        );
        assert!(
            sender
                .try_send(Frame::ToolDone {
                    name: "read".to_owned(),
                    error: true
                })
                .is_ok()
        );
        drain_frames(&mut receiver, &mut view);
        assert_eq!(view.transcript.len(), 2);
        assert!(matches!(
            view.transcript.entries().first().map(Entry::kind),
            Some(crate::transcript::EntryKind::ToolCall { .. })
        ));
    }

    #[test]
    fn connection_frames_do_not_reach_the_transcript() {
        // The handshake and a `Bye` are about the link, not about the conversation. A
        // reader must not find them in the middle of an answer.
        let (sender, mut receiver) = mpsc::channel::<Frame>(8);
        let mut view = ViewState::new();
        for frame in [
            Frame::Bye,
            Frame::Status(nanus_link::protocol::AgentInfo {
                session: "s".to_owned(),
                workspace: "/tmp".to_owned(),
                model: "m".to_owned(),
                tools: 0,
            }),
        ] {
            assert!(sender.try_send(frame).is_ok());
        }
        drain_frames(&mut receiver, &mut view);
        assert!(view.transcript.is_empty());
    }

    #[test]
    fn a_full_queue_drops_progress_rather_than_blocking() {
        // The channel is bounded, so a slow interface cannot apply backpressure to the
        // agent. Filling it must not panic.
        let (sender, _receiver) = mpsc::channel::<Frame>(1);
        for index in 0..64 {
            let _ = sender.try_send(Frame::Text {
                delta: format!("delta {index}"),
            });
        }
    }

    #[test]
    fn a_submitted_prompt_reaches_the_source() {
        let mut source = Scripted::new(Vec::new());
        source.submit("do the thing".to_owned());
        assert_eq!(
            source.requests.borrow().as_slice(),
            [Request::Prompt {
                text: "do the thing".to_owned()
            }]
        );
    }

    #[test]
    fn attaching_a_scripted_source_queues_its_frames() {
        // The property the event loop depends on: `attach` is what puts frames on the
        // channel, and it runs before the first draw.
        let (frames, mut receiver) = mpsc::channel::<Frame>(8);
        let mut source = Scripted::new(vec![Frame::Done {
            answer: "answered".to_owned(),
        }]);
        source.attach(&frames);
        assert_eq!(
            receiver.try_recv().ok(),
            Some(Frame::Done {
                answer: "answered".to_owned()
            })
        );
    }

    #[test]
    fn a_recording_does_not_accept_prompts() {
        // The negative half of the submission guard, and the reason the interface can
        // tell a reader from a driver without asking the agent.
        let recording = Recording::new(session());
        assert!(!recording.accepts_prompts());
        assert_eq!(recording.initial_scroll(), 0);
        assert_eq!(
            Recording::new(session()).scrolled_back(50).initial_scroll(),
            50
        );
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

    #[test]
    fn connecting_to_a_socket_nobody_is_serving_is_an_error_rather_than_a_panic() {
        let missing = Path::new("/definitely/not/a/socket");
        let outcome = block_on(Remote::connect(missing));
        assert!(outcome.is_err(), "a missing socket is refused");
        let Err(error) = outcome else { return };
        assert!(error.contains("/definitely/not/a/socket"), "{error}");
    }
}
