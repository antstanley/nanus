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
//! The set follows [Claude Code's interactive mode][cc-keys] where this interface has
//! something to bind, so a reader arriving from there does not have to learn a second set.
//! `docs/tui.md` lists what is deliberately absent and why.
//!
//! | Key | Effect |
//! |---|---|
//! | `Enter` | submit the composer |
//! | `\` + `Enter` | insert a newline, which no terminal can misreport |
//! | `Alt+Enter` / `Shift+Enter` / `Ctrl+J` | insert a newline |
//! | `Ctrl+C` / `Esc` | stop the running turn; then cancel the composer; then quit |
//! | `Ctrl+D` | quit |
//! | `Ctrl+R` | reverse-search the submitted prompts |
//! | `Ctrl+O` | switch between the one-line and full forms |
//! | `Ctrl+T` / `Ctrl+E` | summarise runs of tool calls / of reasoning |
//! | `Ctrl+K` / `Ctrl+U` / `Ctrl+Y` | delete to the line's end / the line / put it back |
//! | `Ctrl+W` / `Alt+B` / `Alt+F` | delete a word / move a word back / forward |
//! | `Ctrl+L` | clear the transcript |
//! | `Backspace` / `Delete` | delete a character |
//! | `Up` / `Down` | move between lines, then browse submitted prompts |
//! | `PageUp` / `PageDown` | scroll the transcript |
//! | `Left` / `Right`, `Home` / `End` | move the cursor |
//!
//! The mouse navigates too: the wheel scrolls the conversation, and a left click in the
//! composer puts the caret where it landed.
//!
//! A line whose first word opens with `/` is a command the interface answers itself:
//! `/exit` and `/quit` leave, and anything else is named as unrecognised rather than sent
//! to the model. See [`crate::command`].
//!
//! [cc-keys]: https://code.claude.com/docs/en/interactive-mode

use core::future::Future;
use std::ffi::OsStr;
use std::io::{self, IsTerminal};
use std::path::Path;

use crossterm::event::EventStream;
use futures::StreamExt as _;
use nanus_adapter_config::{NanusConfig, TuiDetail};
use nanus_domain::{Session, SessionId};
use nanus_link::Client;
use nanus_link::protocol::{Frame, Request, SessionInfo, TurnEnd};
use nanus_ports::{StoreError, StoreHandle};
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{
    Event as TerminalEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use tokio::sync::mpsc;

use crate::command::{Command, Submission, submission_of};
use crate::compact::Detail;
use crate::notice::{self, Ending};
use crate::stats::Generation;
use crate::transcript::{Entry, Role};
use crate::view::{Theme, ViewState};

/// Rows scrolled per `PageUp` or `PageDown`.
const PAGE_ROWS: i32 = 10;

/// Rows scrolled per notch of the mouse wheel.
///
/// Smaller than a page because a wheel notch is a nudge rather than a jump: a page at a
/// time with a wheel overshoots whatever the reader was aiming at.
const MOUSE_SCROLL_ROWS: i32 = 3;

/// Whether the terminal asked for no colour.
///
/// The convention is "present and not empty": an unset variable is not a request and
/// neither is one set to the empty string, so the environment cannot ask by accident.
/// Taking the value as an argument rather than reading it here is what makes the rule
/// testable — tests cannot set an environment variable, and reading it in the test module
/// would make every other test's behaviour depend on the machine it runs on.
fn no_color_requested(value: Option<&OsStr>) -> bool {
    value.is_some_and(|value| !value.is_empty())
}

/// Reads the interface's display preference from the configuration.
///
/// Here, at the boundary with the terminal, rather than inside the view, for the reason
/// [`no_color_requested`] is a function of a value rather than of the environment: the view
/// is a pure function of a transcript and a composer, and one that opened a file would
/// render differently in a test that happened to inherit a configuration from whatever
/// machine ran it.
///
/// The file is the same one the core reads, resolved the same way — the explicit path is
/// not a parameter because the interface is a separate process, given the socket and the
/// conversation and nothing else, and `NANUS_CONFIG` and the platform location reach both
/// programs equally.
///
/// # Errors
///
/// A configuration that exists and cannot be read is an error rather than a default. It is
/// the same file the core refuses to start on, and silently drawing a transcript the reader
/// did not ask for would be the wrong kind of silence.
fn configured_preferences() -> io::Result<Preferences> {
    NanusConfig::load(None)
        .map(|config| Preferences {
            detail: detail_from(config.tui_detail),
            markdown: config.markdown,
            mermaid: config.mermaid,
        })
        .map_err(|error| io::Error::other(format!("the configuration could not be read: {error}")))
}

/// The interface's display preferences, read together from one file.
///
/// One read rather than one per setting: the file is the same file, and a load that fails
/// should be a single sentence on stderr rather than three attempts at the same thing.
struct Preferences {
    detail: Detail,
    markdown: bool,
    mermaid: bool,
}

/// Maps the configured spelling onto the rendering it selects.
///
/// Two enums rather than one because the configuration adapter cannot see the interface —
/// it sits *below* it, and the view layer must keep building without the runtime — so the
/// mapping belongs here, in the process that is both. It is an exhaustive match, so a
/// rendering the configuration grows is a build error until this decides what it means.
const fn detail_from(configured: TuiDetail) -> Detail {
    match configured {
        TuiDetail::Compact => Detail::Compact,
        TuiDetail::Full => Detail::Full,
    }
}

/// How many frames may be queued from the agent before the interface falls behind.
const FRAME_BUFFER: usize = 256;

/// Restores the terminal when it goes out of scope.
///
/// A guard rather than a call at the end of the function, because a panic between
/// entering raw mode and leaving it would otherwise leave the user's terminal unusable
/// — the failure that makes a TUI feel broken even after it is fixed.
struct TerminalGuard {
    terminal: DefaultTerminal,
    /// Whether the keyboard protocol was asked for, so it is only given back if it was.
    enhanced: bool,
    /// Whether the mouse was captured, so it is only released if it was.
    mouse: bool,
}

impl TerminalGuard {
    /// Enters raw mode, the alternate screen, mouse reporting, and — where the terminal
    /// supports it — the keyboard protocol that reports modifiers.
    ///
    /// A terminal in its default mode sends one byte for `Enter`, and it is the same byte
    /// whether or not Shift is held: `Shift+Enter` is not a key a program is told about,
    /// it is a key that arrives as `Enter`. Terminals that implement the kitty keyboard
    /// protocol can say otherwise, so the protocol is requested when the terminal
    /// answers that it speaks it, and not requested when it does not — a terminal that
    /// does not understand the request may print the escape sequence instead.
    ///
    /// Mouse reporting is asked for unconditionally: a terminal that does not speak it
    /// ignores the request rather than printing it, and until it is asked for the wheel
    /// and the pointer are the terminal's rather than this program's.
    fn enter() -> Self {
        let terminal = ratatui::init();
        let mouse = enable_mouse();
        let enhanced = enable_keyboard_protocol();
        Self {
            terminal,
            enhanced,
            mouse,
        }
    }

    /// Returns the terminal to draw into.
    fn terminal(&mut self) -> &mut DefaultTerminal {
        &mut self.terminal
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // Given back before the screen is restored, so the shell that inherits the
        // terminal is not left with a mouse mode it never asked for.
        if self.mouse {
            let _ = crossterm::execute!(io::stdout(), crossterm::event::DisableMouseCapture);
        }
        if self.enhanced {
            // Given back before the screen is restored, so the shell that inherits the
            // terminal does not inherit a keyboard mode it never asked for.
            use crossterm::event::PopKeyboardEnhancementFlags;
            let _ = crossterm::execute!(io::stdout(), PopKeyboardEnhancementFlags);
        }
        ratatui::restore();
    }
}

/// Asks the terminal to report mouse events.
///
/// Returns whether the request was made. A write that fails leaves the interface exactly
/// as it was before the feature existed — keyboard-only — which is why this is a
/// best-effort upgrade rather than a requirement, like [`enable_keyboard_protocol`].
fn enable_mouse() -> bool {
    match crossterm::execute!(io::stdout(), crossterm::event::EnableMouseCapture) {
        Ok(()) => true,
        Err(error) => {
            tracing::debug!(%error, "mouse capture could not be enabled");
            false
        }
    }
}

/// Asks the terminal to report key modifiers, if it can.
///
/// Returns whether the request was made. Anything that goes wrong — a terminal that does
/// not answer, a write that fails — leaves the interface exactly as it was before the
/// feature existed, which is why this is a best-effort upgrade rather than a requirement.
fn enable_keyboard_protocol() -> bool {
    use crossterm::event::{KeyboardEnhancementFlags, PushKeyboardEnhancementFlags};

    match crossterm::terminal::supports_keyboard_enhancement() {
        Ok(true) => {
            // `DISAMBIGUATE_ESCAPE_CODES` is the flag that makes `Shift+Enter` its own
            // key rather than another `Enter`. The others are deliberately not asked for:
            // reporting key *release* would double every keystroke, and reporting
            // alternate keys would make `Ctrl+T` arrive as something else on keyboards
            // with dead keys.
            let flags = KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES;
            match crossterm::execute!(io::stdout(), PushKeyboardEnhancementFlags(flags)) {
                Ok(()) => true,
                Err(error) => {
                    tracing::debug!(%error, "the keyboard protocol could not be enabled");
                    false
                }
            }
        }
        Ok(false) => {
            tracing::debug!("this terminal does not report key modifiers");
            false
        }
        Err(error) => {
            tracing::debug!(%error, "the keyboard protocol could not be queried");
            false
        }
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

    /// What to call the session in the title bar, when it has a name worth showing.
    ///
    /// `None` is the ordinary case for a recording, which announces itself with a banner
    /// instead, and for a live conversation the agent did not name.
    fn label(&self) -> Option<&str> {
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

    /// Asks the agent to stop the turn running in this session.
    ///
    /// A default of doing nothing: a source that cannot run a turn — a recording — has
    /// nothing to stop, and the interface never asks one to.
    fn interrupt(&mut self) {}

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
pub fn view(store: &StoreHandle, id: Option<&str>, scroll_back: u32) -> Result<(), String> {
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

/// What the interface was asked to talk to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// A new session, recorded under `name` if one was given.
    New {
        /// The name to record it under, if any.
        name: Option<String>,
    },
    /// A session that already exists, named by a name or an id.
    Resume(String),
}

/// A live conversation, held by an agent on the other end of the link.
///
/// The interface does not own the session — the agent does, and this is one view of it.
/// That is what makes a resume possible: the conversation is loaded from the store as it
/// is already written down, the connection is attached to it, and everything after that
/// arrives as frames.
pub struct Remote {
    session: Session,
    label: String,
    client: Option<Client>,
    requests: mpsc::UnboundedSender<Request>,
    pending: Option<mpsc::UnboundedReceiver<Request>>,
}

impl Remote {
    /// Connects to the agent listening at `path` and attaches to `target`.
    ///
    /// # Errors
    ///
    /// Returns a message when nothing is listening, the agent refuses the session, or the
    /// store cannot be read. The message is already user-facing, so it is not wrapped
    /// again.
    pub async fn connect(path: &Path, store: &StoreHandle, target: Target) -> Result<Self, String> {
        let mut client = Client::connect(path)
            .await
            .map_err(|error| error.to_string())?;
        let agent = client.info().clone();
        let attached = match target {
            Target::New { name } => client.start(name).await,
            Target::Resume(reference) => client.attach(&reference).await,
        }
        .map_err(|error| error.to_string())?;
        let session = history(store, &attached, &agent.workspace).await;
        let label = attached.name.unwrap_or_else(|| short_id(&attached.session));
        let (requests, pending) = mpsc::unbounded_channel();
        Ok(Self {
            session,
            label,
            client: Some(client),
            requests,
            pending: Some(pending),
        })
    }
}

/// The first few characters of a session id, for a title bar.
///
/// A uuid is too long to draw and too wide to read, and eight characters is what the
/// session listing shows and what a person copies.
fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

/// Reads the conversation a client has attached to.
///
/// The history comes from the store rather than down the link, and the two ways that can
/// go wrong are different things:
///
/// - **Nothing is stored under that key.** A brand-new session, which is not a failure at
///   all: it is a conversation that has not said anything yet.
/// - **Something is stored and cannot be read.** A damaged log, or an unreadable
///   directory. The client can still talk, so it is given an empty conversation rather
///   than refused — but it is *reported*, because quietly showing an empty transcript for
///   a session that has history would misrepresent the conversation the reader is in.
///
/// A live session never shows a creation time — the reader is in it — so the timestamp is
/// the epoch and the recorded path reads a real one.
async fn history(store: &StoreHandle, attached: &SessionInfo, workspace: &str) -> Session {
    let id = SessionId::new(&attached.session);
    match store.load(&id).await {
        Ok(session) => session,
        Err(StoreError::NotFound { .. }) => Session::new(id, 0, workspace),
        Err(error) => {
            tracing::warn!(%error, "the recorded history could not be read");
            Session::new(id, 0, workspace)
        }
    }
}

impl SessionSource for Remote {
    fn session(&self) -> &Session {
        &self.session
    }

    fn label(&self) -> Option<&str> {
        Some(&self.label)
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
        self.send(Request::Prompt { text: prompt });
    }

    fn interrupt(&mut self) {
        self.send(Request::Interrupt);
    }
}

impl Remote {
    /// Hands one request to the transport task.
    fn send(&self, request: Request) {
        if self.requests.send(request).is_err() {
            // The only way this fails is that the transport task is gone, which means the
            // agent closed the link. Saying so beats a request that vanishes.
            tracing::warn!("the link is closed; the request was not sent");
        }
    }
}

/// Moves requests to the agent and frames back, for as long as the connection lasts.
///
/// One task, reading and writing at once. Reading *continuously* rather than only after a
/// prompt is the whole point: a connection is a view of a session now, so a client that
/// only listened while it had a question would miss everything the other clients did —
/// which is exactly what watching a running session means.
async fn pump(
    client: Client,
    mut requests: mpsc::UnboundedReceiver<Request>,
    frames: mpsc::Sender<Frame>,
) {
    // Split, because the two directions run at once and one borrow cannot serve both.
    let (mut reader, mut sender) = client.split();
    loop {
        tokio::select! {
            request = requests.recv() => {
                let Some(request) = request else {
                    // Every sender is gone, which means the interface is closing.
                    return;
                };
                if let Err(error) = sender.send(&request).await {
                    report(&frames, error.to_string()).await;
                    return;
                }
            }
            frame = reader.next() => {
                match frame {
                    Ok(Some(frame)) => {
                        // A receiver that has gone away means the interface is closing, so
                        // there is nobody left to tell.
                        if frames.send(frame).await.is_err() {
                            return;
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
    // Read before the terminal is taken, so a configuration that cannot be read is a
    // sentence on stderr rather than an abort with a screen already in raw mode.
    let preferences = configured_preferences()?;
    let mut guard = TerminalGuard::enter();
    let mut view = ViewState::new();
    view.detail = preferences.detail;
    view.markdown = preferences.markdown;
    view.mermaid = preferences.mermaid;
    // Chosen here, at the boundary with the terminal, rather than inside the view: the
    // environment is a property of this run, and a view built with one would render
    // differently in a test that happened to inherit `NO_COLOR` from whatever ran it.
    if no_color_requested(std::env::var_os("NO_COLOR").as_deref()) {
        view.theme = Theme::monochrome();
    }
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
    view.label = source.label().map(str::to_owned);
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
                let event = event?;
                // Mouse events are routed before the keyboard, so the key handling
                // below stays the one flat match it always was. The event is borrowed
                // rather than moved so it is still there when it is not a mouse event.
                if let TerminalEvent::Mouse(mouse) = &event {
                    handle_mouse(*mouse, &mut view);
                    continue;
                }
                let TerminalEvent::Key(key) = event else {
                    continue;
                };
                // Windows reports both press and release; only a press is a keystroke.
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match handle_key(key, &mut view) {
                    Outcome::Quit => break,
                    Outcome::Interrupt => {
                        // The turn ends when the agent says it did, with a frame carrying
                        // the reason — so this only says what has been asked for, and the
                        // transcript gets the ending it would have got anyway.
                        view.status = String::from("stopping");
                        source.interrupt();
                    }
                    Outcome::Submit(prompt) => {
                        match route_submission(prompt, source.accepts_prompts()) {
                            Routed::Leave => break,
                            Routed::Stats => {
                                // A notice rather than prose: the model did not say this, the
                                // interface did, and the colour is how a reader tells them apart.
                                view.transcript.push(Entry::notice(view.stats.report()));
                                view.scroll_to_bottom();
                            }
                            Routed::Say(message) => {
                                view.transcript.push(Entry::notice(message));
                                view.scroll_to_bottom();
                            }
                            Routed::Send(prompt) => {
                                // The transcript is seeded here, where the mutable view
                                // lives.
                                view.transcript
                                    .push(Entry::prose(Role::User, prompt.clone()));
                                view.begin_turn(1);
                                view.scroll_to_bottom();
                                source.submit(prompt);
                            }
                        }
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

/// What the event loop should do with a submitted line.
#[derive(Clone, PartialEq, Eq, Debug)]
enum Routed {
    /// Send it to the model.
    Send(String),
    /// Leave the interface.
    Leave,
    /// Write the session's figures into the transcript.
    ///
    /// Routed rather than said, because the text is not known until the view is: the router is a
    /// pure function of the line and what the source accepts, and the figures are state.
    Stats,
    /// Say this in the transcript instead.
    Say(String),
}

/// Routes a submitted line: the interface's own commands first, then whether the source can
/// take a prompt at all.
///
/// Commands are answered before the source is consulted, which is what makes them work in
/// every mode: a recorded session cannot be talked to, and `/exit` still has to leave it.
fn route_submission(prompt: String, accepts_prompts: bool) -> Routed {
    match submission_of(&prompt) {
        Submission::Run(Command::Exit) => Routed::Leave,
        Submission::Run(Command::Stats) => Routed::Stats,
        Submission::Unknown(name) => Routed::Say(format!(
            "no such command: {name} — this interface knows {}",
            Command::NAMES.join(" and ")
        )),
        Submission::Prompt => {
            if accepts_prompts {
                Routed::Send(prompt)
            } else {
                // Submitting in a recorded session would need an agent this interface does
                // not have. Saying so beats silently discarding what the user typed.
                Routed::Say(String::from(
                    "this is a recorded session; start `nanus tui` without --session to continue it",
                ))
            }
        }
    }
}

/// What a keystroke asked for.
enum Outcome {
    /// Keep going.
    Continue,
    /// Leave.
    Quit,
    /// Send this prompt.
    Submit(String),
    /// Ask the agent to stop the turn that is running.
    Interrupt,
}

/// Applies one keystroke to the view.
fn handle_key(key: KeyEvent, view: &mut ViewState) -> Outcome {
    // A running search rewrites what every key means: the composer is showing a match
    // rather than the reader's own text, so typing extends the *query* and the keys that
    // would normally edit the prompt end the search instead. Routing it before anything
    // else keeps the two meanings from being interleaved.
    if view.input.is_searching() {
        return handle_search_key(key, view);
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return handle_control_key(key, view);
    }
    handle_plain_key(key, view)
}

/// Routes a key with Control held.
///
/// Its own function for two reasons. These bindings are the ones a terminal is most likely
/// to report oddly — see the note on case below — and they are all one key, one effect,
/// with no fallthrough into text editing, which is what keeps the ordinary handler readable.
fn handle_control_key(key: KeyEvent, view: &mut ViewState) -> Outcome {
    // These match either case, and that is not tidiness. A terminal that reports modifiers
    // reports the *character* its modifier state produces, so with Caps Lock on `Ctrl+C`
    // arrives as `Char('C')` with CONTROL. Matching only the lowercase form left the
    // interface impossible to quit and the toggles dead — the same mistake as reading a key
    // without asking what the modifiers did to it.
    match key.code {
        // Stop what is happening, in the order a reader means it: the turn if one is
        // running, then the prompt, then the session.
        KeyCode::Char('c' | 'C') => stop_or_cancel_or_quit(view),
        KeyCode::Char('d' | 'D') => Outcome::Quit,
        KeyCode::Char('l' | 'L') => {
            view.transcript.clear();
            Outcome::Continue
        }
        // Verbose output, which is the same toggle the configuration file sets: the
        // one-line form for a reader skimming, the whole call for a reader studying it.
        KeyCode::Char('o' | 'O') => {
            view.toggle_detail();
            Outcome::Continue
        }
        KeyCode::Char('k' | 'K') => {
            view.input.kill_to_end();
            Outcome::Continue
        }
        KeyCode::Char('u' | 'U') => {
            view.input.kill_line();
            Outcome::Continue
        }
        KeyCode::Char('y' | 'Y') => {
            view.input.yank();
            Outcome::Continue
        }
        KeyCode::Char('w' | 'W') => {
            view.input.delete_word();
            Outcome::Continue
        }
        KeyCode::Char('t' | 'T') => {
            view.collapse_tools = !view.collapse_tools;
            Outcome::Continue
        }
        // `Ctrl+E` rather than `Ctrl+R`: `Ctrl+R` is the reverse history search, which is
        // what it is in every interface that has one, including the one this mirrors.
        KeyCode::Char('e' | 'E') => {
            view.collapse_reasoning = !view.collapse_reasoning;
            Outcome::Continue
        }
        KeyCode::Char('r' | 'R') => {
            view.input.search_start();
            Outcome::Continue
        }
        // `Ctrl+J` is a line feed, and a line feed is what a terminal sends when
        // `Shift+Enter` is bound to "insert a newline" rather than to a key — which is how
        // Ghostty is configured by default, and why the request for the kitty keyboard
        // protocol does not help there: the terminal is not reporting a key at all, it is
        // typing a character. Ctrl+J has meant "new line" since readline, so this is the
        // same binding twice over rather than a special case.
        KeyCode::Char('j' | 'J') => {
            view.input.insert('\n');
            Outcome::Continue
        }
        // A character with Control held is not text. Terminals report control bytes as
        // `Ctrl+<letter>`, so without this arm every unbound control key typed its letter:
        // Ctrl+K inserted a `k`, and Ctrl+H an `h` for what is also backspace.
        KeyCode::Char(_) => Outcome::Continue,
        _ => handle_plain_key(key, view),
    }
}

/// Routes a key with no Control held: text, movement, and the keys that send.
fn handle_plain_key(key: KeyEvent, view: &mut ViewState) -> Outcome {
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    match key.code {
        // Shift+Enter reaches here only from a terminal that reports it as distinct from
        // Enter — see `TerminalGuard::enter`. Elsewhere it is the same byte, and a newline
        // in a prompt is not worth breaking the submit key for, so `Alt+Enter` and `Ctrl+J`
        // stay the spellings that always work.
        KeyCode::Enter if alt || shift => {
            view.input.insert('\n');
            Outcome::Continue
        }
        // A backslash before Enter breaks the line: the one spelling that needs no terminal
        // cooperation at all, which is what makes it worth having beside the ones that do.
        KeyCode::Enter if view.input.break_line_after_escape() => Outcome::Continue,
        // An empty composer is not an error; it just does nothing.
        KeyCode::Enter => view
            .input
            .submit()
            .map_or(Outcome::Continue, Outcome::Submit),
        // Word-wise movement, matching the marks the shell and every readline do, so that
        // `Alt+B` and then `Ctrl+W` delete the word the cursor just moved to. Alt is
        // deliberately not swallowed for other letters: on many terminals an Option/Alt
        // press arrives as `Alt+<letter>` on its way to producing a character, and eating
        // those would stop some keyboards typing at all.
        KeyCode::Char('b' | 'B') if alt => {
            view.input.move_word_left();
            Outcome::Continue
        }
        KeyCode::Char('f' | 'F') if alt => {
            view.input.move_word_right();
            Outcome::Continue
        }
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
            // Back in time. The offset counts rows skipped from the top, so older content
            // is *less* offset — and this was the other way round, which is why Page-Up
            // did nothing at all from the bottom of a conversation.
            view.scroll(-PAGE_ROWS);
            Outcome::Continue
        }
        KeyCode::PageDown => {
            view.scroll(PAGE_ROWS);
            Outcome::Continue
        }
        // The same key as `Ctrl+C`, for the same reason: what a reader wants stopped is
        // whatever is happening now, and the key should not need reading the screen first.
        KeyCode::Esc => stop_or_cancel_or_quit(view),
        _ => Outcome::Continue,
    }
}

/// Routes a mouse event.
///
/// The wheel is the whole of scrolling with the mouse, and it is not confined to the
/// transcript band: it is the one gesture for "move through the conversation", and
/// requiring the pointer to be over a particular area would make it fail exactly when a
/// reader reached for it without looking. A left click belongs to the composer, and only
/// there: the transcript is read, not pointed at, so a click that misses the prompt is
/// not a command.
fn handle_mouse(mouse: MouseEvent, view: &mut ViewState) {
    match mouse.kind {
        MouseEventKind::ScrollUp => view.scroll(-MOUSE_SCROLL_ROWS),
        MouseEventKind::ScrollDown => view.scroll(MOUSE_SCROLL_ROWS),
        MouseEventKind::Down(MouseButton::Left) => {
            let _ = view.place_caret(mouse.column, mouse.row);
        }
        // Scrolling sideways and every other button are not things this interface has
        // anywhere to put. Ignoring them is deliberate rather than an oversight.
        _ => {}
    }
}

/// Stops the turn that is running, or cancels the prompt, or leaves.
///
/// Three meanings on one key, in the order a reader means them. A turn in flight is what
/// the key stops first, because that is the thing happening now and the thing a reader
/// pressing "stop" is looking at — and stopping it is a request to the agent rather than a
/// keystroke, since the turn belongs to the session and not to this terminal. With nothing
/// running the key reaches the prompt, and only an empty prompt leaves, which is what keeps
/// a key meaning "stop" from throwing away what somebody spent a minute writing.
fn stop_or_cancel_or_quit(view: &mut ViewState) -> Outcome {
    if view.busy {
        return Outcome::Interrupt;
    }
    if view.input.is_empty() {
        return Outcome::Quit;
    }
    view.input.clear();
    Outcome::Continue
}

/// Routes a key while a reverse history search is running.
///
/// Every binding here is about the search rather than about the prompt, because the
/// composer is showing a match: editing keys that reached the text would be editing
/// history, which is not what a reader searching it is doing.
fn handle_search_key(key: KeyEvent, view: &mut ViewState) -> Outcome {
    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    match key.code {
        // Enter takes the match and sends it. A search that has found the prompt you were
        // looking for has done its job, and the thing you want next is to run it.
        KeyCode::Enter => {
            view.input.search_accept();
            view.input
                .submit()
                .map_or(Outcome::Continue, Outcome::Submit)
        }
        // Tab and Esc take the match and leave it in the composer to be edited, which is
        // the other half of the same idea.
        KeyCode::Tab | KeyCode::Esc => {
            view.input.search_accept();
            Outcome::Continue
        }
        // Repeated `Ctrl+R` walks further back through the matches.
        KeyCode::Char('r' | 'R') if control => {
            view.input.search_older();
            Outcome::Continue
        }
        // `Ctrl+C` abandons the search and gives back what was being typed: the cancel
        // that means "forget this", as opposed to the accept that means "use it".
        KeyCode::Char('c' | 'C') if control => {
            view.input.search_cancel();
            Outcome::Continue
        }
        KeyCode::Char(character) if !control && !alt => {
            view.input.search_push(character);
            Outcome::Continue
        }
        KeyCode::Backspace => {
            // Backspace with an empty query leaves the search rather than deleting
            // nothing for ever, which is what makes the key safe to lean on.
            if !view.input.search_backspace() {
                view.input.search_cancel();
            }
            Outcome::Continue
        }
        _ => Outcome::Continue,
    }
}

/// What to tell the reader when a turn stopped without finishing, if it did.
///
/// The wording lives in [`crate::notice`], with the replay path's, because the two render the
/// same fact: a live turn learns it from the ending frame and a recorded one from the log, and
/// when those were two renderers the recorded one said nothing at all.
fn stopping_notice(reason: &TurnEnd, step: u32) -> Option<String> {
    notice::stopping(&Ending::from(reason), step)
}

/// Applies one frame from the agent to the view.
fn apply(frame: Frame, view: &mut ViewState) {
    match frame {
        Frame::Text { delta } => {
            view.transcript
                .append_stream(Role::Assistant, &delta, false);
            view.follow();
        }
        Frame::Reasoning { delta } => {
            view.transcript
                .append_stream(Role::Reasoning, &delta, false);
            view.follow();
        }
        Frame::User { text } => {
            // Somebody else's prompt in a session this interface is watching: the
            // reader needs to see the question before the answer.
            view.transcript.push(Entry::prose(Role::User, text));
            view.begin_turn(1);
            // Somebody else's prompt is not a reason to drag a reader who is looking
            // further up: they are reading, and this is not their action.
            view.follow();
        }
        Frame::Step { step } => view.begin_turn(step),
        Frame::Tool { name, arguments } => {
            // Followed like every other append: a tool line that arrives below the fold is a
            // line the reader is not shown, and the transcript's own rule is that everything
            // which appends follows.
            // Rendered once, here, because a transcript entry holds a *rendered* form: what
            // a reader sees is the view's decision, and the wire's job is to carry what the
            // model sent. A frame from an agent that predates the arguments decodes to
            // `null`, which is "nothing to show" rather than the word `null` — the compact
            // line is then the tool alone.
            let arguments = match arguments {
                serde_json::Value::Null => String::new(),
                other => other.to_string(),
            };
            view.transcript.push(Entry::tool_call(name, arguments));
            view.follow();
        }
        Frame::ToolDone { name, error } => {
            // No content, because the frame carries none: a tool's output is in the
            // session log, and this frame says only that the call is over and how it
            // went. A placeholder here used to put the word "done" under every tool call
            // in a live transcript — a line of screen saying nothing — and with the
            // outcome moved onto the call's own line it would have said it twice.
            view.transcript.push(Entry::tool_result(name, error, ""));
            view.follow();
        }
        Frame::Usage {
            tokens,
            completion_tokens,
            cache_hit_tokens,
            cache_miss_tokens,
            duration_ms,
            reasoning_tokens,
            head_ms,
            ttft_ms,
            decode_ms,
        } => {
            view.add_tokens(tokens);
            // The frame's counters are the agent's; the shape the interface reckons in is its
            // own, because the agent's vocabulary is about a request and this one is about a
            // rate. Converting here keeps `stats` free of the link's types and the wire free
            // of the interface's.
            view.stats.record(Generation {
                completion_tokens: u64::from(completion_tokens),
                reasoning_tokens: u64::from(reasoning_tokens),
                cache_hit_tokens: u64::from(cache_hit_tokens),
                cache_miss_tokens: u64::from(cache_miss_tokens),
                head_ms,
                ttft_ms,
                decode_ms,
                duration_ms,
            });
        }
        // A turn that stopped early is not a turn that finished, and the reason is the
        // only thing that says which one this is. Drawing the answer either way is how a
        // turn that ran out of steps came to look like a completed one: the last thing
        // the model happened to say was put on screen as its conclusion, and the reader
        // was left to work out from the silence that the work had been cut off.
        Frame::Done { answer, reason } => {
            if let Some(notice) = stopping_notice(&reason, view.step) {
                view.transcript.settle_tail();
                view.transcript.push(Entry::notice(notice));
            } else {
                // Reconciliation rather than a second copy: the answer already streamed
                // in delta by delta, and settling that entry is what stops the same
                // paragraph being drawn twice.
                view.transcript.settle_with(Role::Assistant, &answer);
            }
            view.end_turn();
            view.follow();
        }
        Frame::Failed { message } => {
            // The last frame a transport sends, so an unfollowed notice is one the reader may
            // never see: the status line goes back to ready either way, and a failure nobody
            // was shown is a failure reported nowhere.
            view.transcript.push(Entry::notice(message));
            view.follow();
            view.end_turn();
        }
        // Frames that describe the connection rather than the conversation. The interface
        // learned what it needed from the handshake and the attachment before it drew
        // anything, and a `Bye` is the transport's business, not the transcript's.
        Frame::Ready(_)
        | Frame::Attached(_)
        | Frame::Sessions { .. }
        | Frame::Status(_)
        | Frame::Bye => {}
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
    use nanus_adapter_store::JsonlStore;
    use nanus_domain::SessionId;
    // `save` and `session_dir` live on different types: the port and the concrete adapter.
    use nanus_ports::StorePort as _;

    use super::*;
    use crate::transcript::EntryKind;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    /// A mouse event at a cell, for the wheel and the click.
    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    /// `NO_COLOR` is `present and not empty`, and the empty case is the one that matters:
    /// a variable exported with no value is a common accident in a shell startup file, and
    /// treating it as a request would silently strip the interface's colours for someone
    /// who never asked.
    #[test]
    fn no_color_counts_when_it_is_present_and_not_empty() {
        assert!(no_color_requested(Some(OsStr::new("1"))));
        assert!(no_color_requested(Some(OsStr::new("anything at all"))));
        assert!(!no_color_requested(Some(OsStr::new(""))));
        assert!(!no_color_requested(None));
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
    fn shift_enter_starts_a_new_line_and_plain_enter_still_submits() {
        // Shift+Enter only arrives as itself from a terminal that reports modifiers; the
        // point of the test is that when it does, it does the thing a person expects
        // rather than submitting half a prompt.
        let mut view = ViewState::new();
        view.input.insert_str("line");
        assert!(matches!(
            handle_key(key(KeyCode::Enter, KeyModifiers::SHIFT), &mut view),
            Outcome::Continue
        ));
        assert_eq!(view.input.text(), "line\n");

        view.input.insert_str("more");
        assert!(matches!(
            handle_key(key(KeyCode::Enter, KeyModifiers::NONE), &mut view),
            Outcome::Submit(prompt) if prompt == "line\nmore"
        ));
    }

    #[test]
    fn a_line_feed_starts_a_new_line_however_the_terminal_spells_it() {
        // The reported bug: on Ghostty, `Shift+Enter` is bound to "send a newline", so the
        // terminal types a line feed rather than reporting a key. In raw mode crossterm
        // reports a line feed as `Ctrl+J` — which the catch-all arm then inserted as the
        // letter `j`. Both spellings now mean the same thing.
        let mut view = ViewState::new();
        view.input.insert_str("line");
        assert!(matches!(
            handle_key(key(KeyCode::Char('j'), KeyModifiers::CONTROL), &mut view),
            Outcome::Continue
        ));
        assert_eq!(view.input.text(), "line\n");

        // And the kitty encoding of the same key, which some terminals do send.
        let mut kitty = ViewState::new();
        kitty.input.insert_str("line");
        let _ = handle_key(key(KeyCode::Enter, KeyModifiers::SHIFT), &mut kitty);
        assert_eq!(kitty.input.text(), view.input.text());
    }

    #[test]
    fn a_control_binding_works_whatever_case_the_terminal_reports() {
        // Reported by injecting what Caps Lock produces once the keyboard protocol is on:
        // the terminal reports the character its modifier state makes, so `Ctrl+C` arrives
        // as `Char('C')` with CONTROL. Matching only lowercase left the interface
        // impossible to quit.
        // `Ctrl+D` leaves outright; `Ctrl+C` cancels first, so an empty composer is the
        // state in which it quits — which is what makes the two keys different rather
        // than two spellings of one.
        for character in ['d', 'D'] {
            let mut view = ViewState::new();
            assert!(
                matches!(
                    handle_key(
                        key(KeyCode::Char(character), KeyModifiers::CONTROL),
                        &mut view
                    ),
                    Outcome::Quit
                ),
                "Ctrl+{character} quits"
            );
        }
        for character in ['c', 'C'] {
            let mut view = ViewState::new();
            assert!(
                matches!(
                    handle_key(
                        key(KeyCode::Char(character), KeyModifiers::CONTROL),
                        &mut view
                    ),
                    Outcome::Quit
                ),
                "Ctrl+{character} quits when there is nothing to cancel"
            );
        }

        // And the toggles, which were equally dead in the shifted case.
        for character in ['t', 'T'] {
            let mut view = ViewState::new();
            let _ = handle_key(
                key(KeyCode::Char(character), KeyModifiers::CONTROL),
                &mut view,
            );
            assert!(view.collapse_tools, "Ctrl+{character} toggles tools");
        }
        for character in ['e', 'E'] {
            let mut view = ViewState::new();
            let _ = handle_key(
                key(KeyCode::Char(character), KeyModifiers::CONTROL),
                &mut view,
            );
            assert!(view.collapse_reasoning, "Ctrl+{character} toggles thinking");
        }
        for character in ['o', 'O'] {
            let mut view = ViewState::new();
            let _ = handle_key(
                key(KeyCode::Char(character), KeyModifiers::CONTROL),
                &mut view,
            );
            assert_eq!(view.detail, Detail::Full, "Ctrl+{character} is verbose");
        }
        for character in ['j', 'J'] {
            let mut view = ViewState::new();
            let _ = handle_key(
                key(KeyCode::Char(character), KeyModifiers::CONTROL),
                &mut view,
            );
            assert_eq!(view.input.text(), "\n", "Ctrl+{character} starts a line");
        }
    }

    #[test]
    fn a_shifted_character_is_inserted_as_the_terminal_sent_it() {
        // A terminal that reports key modifiers attaches SHIFT to the *characters* those
        // modifiers produce: `?` arrives as `Char('?')` with SHIFT rather than as a bare
        // `?`. That is the shape that breaks a keymap comparing whole key events — a
        // binding written `<?>` stops firing while `Shift-?>` keeps working, which is
        // ratatui/templates#26 — and it is not a problem here, because the handler reads
        // the key *code* and ignores the modifiers when inserting text. The terminal has
        // already decided which character the key produced.
        let mut view = ViewState::new();
        for character in ['?', '!', '@', '#', 'A'] {
            let _ = handle_key(
                key(KeyCode::Char(character), KeyModifiers::SHIFT),
                &mut view,
            );
        }
        assert_eq!(view.input.text(), "?!@#A");
    }

    #[test]
    fn an_unbound_control_key_types_nothing() {
        // The wider half of the same defect. Terminals report control bytes 0x01-0x1A as
        // `Ctrl+<letter>`, so the catch-all arm was typing a letter for every control key
        // the interface did not claim: Ctrl+K inserted `k`, and Ctrl+H inserted `h` for a
        // byte that is also backspace.
        let mut view = ViewState::new();
        for character in ['k', 'h', 'j', 'p', 'z'] {
            let _ = handle_key(
                key(KeyCode::Char(character), KeyModifiers::CONTROL),
                &mut view,
            );
        }
        assert_eq!(view.input.text(), "\n", "only Ctrl+J did anything");

        // The other direction: an unmodified letter is still text, and so is one with Alt,
        // which some keyboards use on the way to producing a character.
        let mut plain = ViewState::new();
        let _ = handle_key(key(KeyCode::Char('k'), KeyModifiers::NONE), &mut plain);
        let _ = handle_key(key(KeyCode::Char('a'), KeyModifiers::ALT), &mut plain);
        assert_eq!(plain.input.text(), "ka");
    }

    #[test]
    fn ctrl_t_and_ctrl_e_toggle_the_summaries() {
        let mut view = ViewState::new();
        assert!(!view.collapse_tools);
        assert!(!view.collapse_reasoning);

        let _ = handle_key(key(KeyCode::Char('t'), KeyModifiers::CONTROL), &mut view);
        assert!(view.collapse_tools, "Ctrl-T summarizes tool runs");
        assert!(!view.collapse_reasoning, "and nothing else");
        // `Ctrl+E`, because `Ctrl+R` is the history search — see
        // `ctrl_r_searches_the_history_it_does_not_toggle_thinking`.
        let _ = handle_key(key(KeyCode::Char('e'), KeyModifiers::CONTROL), &mut view);
        assert!(view.collapse_reasoning, "Ctrl-E summarizes thinking");

        // Both toggles go back off, and the letters alone are still characters.
        let _ = handle_key(key(KeyCode::Char('t'), KeyModifiers::CONTROL), &mut view);
        let _ = handle_key(key(KeyCode::Char('e'), KeyModifiers::CONTROL), &mut view);
        assert!(!view.collapse_tools);
        assert!(!view.collapse_reasoning);
        let _ = handle_key(key(KeyCode::Char('t'), KeyModifiers::NONE), &mut view);
        assert_eq!(view.input.text(), "t", "a bare letter is still text");
    }

    /// The two-step exit: a key that means "stop" must not be able to lose a prompt that
    /// somebody is halfway through writing.
    /// The stop key means the thing that is happening now: with a turn running it asks the
    /// agent to stop, and only with nothing running does it reach the prompt and the
    /// session.
    #[test]
    fn the_stop_key_stops_a_running_turn_before_it_touches_the_prompt() {
        let mut view = ViewState::new();
        view.input.insert_str("half a thought");
        view.begin_turn(3);

        assert!(matches!(
            handle_key(key(KeyCode::Esc, KeyModifiers::NONE), &mut view),
            Outcome::Interrupt
        ));
        assert_eq!(
            view.input.text(),
            "half a thought",
            "the prompt is not what the key was for"
        );
        assert!(matches!(
            handle_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL), &mut view),
            Outcome::Interrupt
        ));

        // With the turn over the same key falls back, in the same order as before.
        view.end_turn();
        assert!(matches!(
            handle_key(key(KeyCode::Esc, KeyModifiers::NONE), &mut view),
            Outcome::Continue
        ));
        assert!(view.input.is_empty(), "the second press cancels the prompt");
        assert!(matches!(
            handle_key(key(KeyCode::Esc, KeyModifiers::NONE), &mut view),
            Outcome::Quit
        ));
    }

    /// `/exit` and `/quit` are the same command, they work even where a prompt cannot be
    /// sent, and anything else that opens with a slash is named rather than sent.
    #[test]
    fn slash_commands_are_the_interfaces_business() {
        for leaving in ["/exit", "/quit"] {
            assert_eq!(
                route_submission(String::from(leaving), true),
                Routed::Leave,
                "{leaving} leaves"
            );
            assert_eq!(
                route_submission(String::from(leaving), false),
                Routed::Leave,
                "{leaving} leaves a recording too, where no prompt can be sent"
            );
        }

        let Routed::Say(message) = route_submission(String::from("/quitx"), true) else {
            panic!("a typo is answered rather than sent to the model");
        };
        assert!(message.contains("/quitx"), "it names the typo: {message}");
        assert!(
            message.contains("/exit") && message.contains("/quit"),
            "and the commands that exist: {message}"
        );

        // `/stats` is the interface's to answer, and it is answered in a recording too: what it
        // reports belongs to the session, and a recorded one has a session.
        for accepts in [true, false] {
            assert_eq!(
                route_submission(String::from("/stats"), accepts),
                Routed::Stats,
                "the figures are the interface's to report"
            );
        }

        // Prose is prose, and a prompt in a recording is refused rather than dropped.
        assert_eq!(
            route_submission(String::from("hello"), true),
            Routed::Send(String::from("hello"))
        );
        let Routed::Say(message) = route_submission(String::from("hello"), false) else {
            panic!("a recording cannot take a prompt");
        };
        assert!(message.contains("recorded session"), "{message}");
    }

    #[test]
    fn ctrl_c_cancels_the_input_before_it_quits() {
        let mut view = ViewState::new();
        view.input.insert_str("half a thought");
        assert!(
            matches!(
                handle_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL), &mut view),
                Outcome::Continue
            ),
            "the first press cancels"
        );
        assert!(
            view.input.is_empty(),
            "and the prompt is gone, not the session"
        );
        assert!(
            matches!(
                handle_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL), &mut view),
                Outcome::Quit
            ),
            "the second one, with nothing left to cancel, leaves"
        );
    }

    /// `Esc` cancels on the same terms. It used to leave on the first press, which is the
    /// one thing a reader with a half-written prompt does not want it to do.
    #[test]
    fn escape_cancels_the_input_before_it_quits() {
        let mut view = ViewState::new();
        view.input.insert_str("half a thought");
        assert!(matches!(
            handle_key(key(KeyCode::Esc, KeyModifiers::NONE), &mut view),
            Outcome::Continue
        ));
        assert!(view.input.is_empty());
        assert!(matches!(
            handle_key(key(KeyCode::Esc, KeyModifiers::NONE), &mut view),
            Outcome::Quit
        ));
    }

    /// `Ctrl+R` is the history search here because it is the history search everywhere
    /// else, including in the interface this set of bindings is modelled on. The thinking
    /// toggle moved to `Ctrl+E` rather than the other way round.
    #[test]
    fn ctrl_r_searches_the_history_it_does_not_toggle_thinking() {
        let mut view = ViewState::new();
        let _ = handle_key(key(KeyCode::Char('r'), KeyModifiers::CONTROL), &mut view);
        assert!(view.input.is_searching(), "Ctrl-R starts a search");
        assert!(!view.collapse_reasoning, "and leaves the summaries alone");

        view.input.search_cancel();
        let _ = handle_key(key(KeyCode::Char('e'), KeyModifiers::CONTROL), &mut view);
        assert!(view.collapse_reasoning, "Ctrl-E is the thinking toggle");
        assert!(!view.input.is_searching());
    }

    /// While a search is running the composer is showing a match, so the keys that would
    /// edit a prompt have to mean something else — and the ones that end the search have
    /// to be reachable without leaving the interface by accident.
    #[test]
    fn a_search_takes_the_keyboard_until_it_is_finished() {
        let mut view = ViewState::new();
        view.input.insert_str("an earlier prompt");
        assert!(view.input.submit().is_some());

        let _ = handle_key(key(KeyCode::Char('r'), KeyModifiers::CONTROL), &mut view);
        assert_eq!(view.input.text(), "an earlier prompt", "the newest match");

        // Typing narrows the search rather than editing the prompt.
        let _ = handle_key(key(KeyCode::Char('z'), KeyModifiers::NONE), &mut view);
        assert_eq!(view.input.search_query(), Some(("z", false)));
        assert_eq!(view.input.text(), "", "the draft, since nothing matched");

        // `Esc` takes the match rather than quitting, which is the whole reason the search
        // is routed before the ordinary bindings.
        assert!(matches!(
            handle_key(key(KeyCode::Esc, KeyModifiers::NONE), &mut view),
            Outcome::Continue
        ));
        assert!(!view.input.is_searching());

        // And with the search over, an empty composer means `Esc` quits again.
        view.input.clear();
        assert!(matches!(
            handle_key(key(KeyCode::Esc, KeyModifiers::NONE), &mut view),
            Outcome::Quit
        ));
    }

    /// Enter during a search takes the match and sends it: the search has found the prompt
    /// the reader wanted, and the next thing they want is to run it.
    #[test]
    fn enter_during_a_search_submits_the_match() {
        let mut view = ViewState::new();
        view.input.insert_str("run the tests");
        assert!(view.input.submit().is_some());
        let _ = handle_key(key(KeyCode::Char('r'), KeyModifiers::CONTROL), &mut view);
        let Outcome::Submit(text) = handle_key(key(KeyCode::Enter, KeyModifiers::NONE), &mut view)
        else {
            panic!("Enter during a search submits the match");
        };
        assert_eq!(text, "run the tests");
        assert!(!view.input.is_searching());
    }

    /// `Ctrl+C` during a search abandons it rather than clearing the prompt or quitting:
    /// "forget this search" is what the key means while one is running.
    #[test]
    fn ctrl_c_cancels_a_search_rather_than_quitting() {
        let mut view = ViewState::new();
        view.input.insert_str("an earlier prompt");
        assert!(view.input.submit().is_some());
        view.input.insert_str("in progress");
        let _ = handle_key(key(KeyCode::Char('r'), KeyModifiers::CONTROL), &mut view);
        assert!(matches!(
            handle_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL), &mut view),
            Outcome::Continue
        ));
        assert!(!view.input.is_searching());
        assert_eq!(view.input.text(), "in progress", "the draft came back");
    }

    /// `Ctrl+O` is the same choice the configuration file makes, reachable at the keyboard:
    /// a reader who wants the whole call wants it now, not after a restart.
    #[test]
    fn ctrl_o_switches_between_the_one_line_and_full_forms() {
        let mut view = ViewState::new();
        assert_eq!(
            view.detail,
            Detail::Compact,
            "the default is the short form"
        );
        let _ = handle_key(key(KeyCode::Char('o'), KeyModifiers::CONTROL), &mut view);
        assert_eq!(view.detail, Detail::Full);
        let _ = handle_key(key(KeyCode::Char('o'), KeyModifiers::CONTROL), &mut view);
        assert_eq!(view.detail, Detail::Compact);
    }

    #[test]
    fn ctrl_k_ctrl_u_and_ctrl_y_edit_the_line_and_put_it_back() {
        let mut view = ViewState::new();
        view.input.insert_str("keep this and drop that");
        for _ in 0.."drop that".len() {
            let _ = handle_key(key(KeyCode::Left, KeyModifiers::NONE), &mut view);
        }
        let _ = handle_key(key(KeyCode::Char('k'), KeyModifiers::CONTROL), &mut view);
        assert_eq!(view.input.text(), "keep this and ");
        let _ = handle_key(key(KeyCode::Char('y'), KeyModifiers::CONTROL), &mut view);
        assert_eq!(
            view.input.text(),
            "keep this and drop that",
            "yank restores it"
        );

        let _ = handle_key(key(KeyCode::Char('u'), KeyModifiers::CONTROL), &mut view);
        assert_eq!(view.input.text(), "", "Ctrl-U takes the line");
        let _ = handle_key(key(KeyCode::Char('y'), KeyModifiers::CONTROL), &mut view);
        assert_eq!(view.input.text(), "keep this and drop that");
    }

    #[test]
    fn alt_b_and_alt_f_move_the_cursor_by_word() {
        let mut view = ViewState::new();
        view.input.insert_str("one two three");
        let _ = handle_key(key(KeyCode::Char('b'), KeyModifiers::ALT), &mut view);
        assert_eq!(
            view.input.cursor(),
            "one two ".len(),
            "at the start of the word before the cursor"
        );
        let _ = handle_key(key(KeyCode::Char('f'), KeyModifiers::ALT), &mut view);
        assert_eq!(view.input.cursor(), "one two three".len());
        // The bare letters stay text, which is what keeps an Alt-less terminal usable.
        let _ = handle_key(key(KeyCode::Char('b'), KeyModifiers::NONE), &mut view);
        assert_eq!(view.input.text(), "one two threeb");
    }

    /// The multiline escape that needs no terminal cooperation: `\` then Enter.
    #[test]
    fn a_backslash_before_enter_breaks_the_line_instead_of_sending() {
        let mut view = ViewState::new();
        view.input.insert_str("first \\");
        assert!(matches!(
            handle_key(key(KeyCode::Enter, KeyModifiers::NONE), &mut view),
            Outcome::Continue
        ));
        assert_eq!(view.input.text(), "first \n");
        // Without the backslash, Enter still sends.
        view.input.insert_str("second");
        assert!(matches!(
            handle_key(key(KeyCode::Enter, KeyModifiers::NONE), &mut view),
            Outcome::Submit(_)
        ));
    }

    /// Every frame that appends to the transcript follows it, which is the view's own rule.
    ///
    /// `Frame::Tool`, `ToolDone` and `Failed` appended without following, so a line arriving
    /// below a full transcript stayed below the fold. The failure is the one that matters most:
    /// it is the last frame a transport sends, so its notice was never shown while the status
    /// line went back to "ready" — a failure reported nowhere on screen.
    #[test]
    fn the_frames_that_append_also_follow() {
        let frames = [
            Frame::Tool {
                name: "read".to_owned(),
                arguments: serde_json::json!({}),
            },
            Frame::ToolDone {
                name: "read".to_owned(),
                error: false,
            },
            Frame::Failed {
                message: "the agent closed the link".to_owned(),
            },
        ];
        for frame in frames {
            let (sender, mut receiver) = mpsc::channel::<Frame>(8);
            let mut view = ViewState::new();
            for index in 0..200 {
                view.transcript
                    .push(Entry::prose(Role::User, format!("entry {index}")));
            }
            // Any scroll tells the view what it is scrolling inside.
            view.scroll_by(0, 20, 60);
            view.scroll_to_bottom();
            assert!(view.following, "a reader at the bottom is following");

            assert!(sender.try_send(frame).is_ok());
            drain_frames(&mut receiver, &mut view);
            assert_eq!(
                view.scroll_offset,
                view.max_scroll(),
                "the frame was followed to the bottom"
            );
        }
    }

    #[test]
    fn page_up_moves_back_through_the_conversation() {
        // The bug this pins: the offset counts rows skipped from the top, and Page-Up was
        // adding to it. From the bottom — where a live conversation always is — that
        // clamped, so Page-Up did nothing at all and scrolling back looked broken.
        let mut view = ViewState::new();
        for index in 0..200 {
            view.transcript
                .push(Entry::prose(Role::User, format!("entry {index}")));
        }
        // Any scroll tells the view what it is scrolling inside; twenty rows of
        // conversation is the shape of a small terminal.
        view.scroll_by(0, 20, 60);
        view.scroll_to_bottom();
        let bottom = view.scroll_offset;
        assert!(bottom > 0, "the conversation is longer than the viewport");

        let _ = handle_key(key(KeyCode::PageUp, KeyModifiers::NONE), &mut view);
        let after_up = view.scroll_offset;
        assert!(
            after_up < bottom,
            "Page-Up went back: {after_up} < {bottom}"
        );
        assert!(!view.following, "and stopped following the newest output");

        let _ = handle_key(key(KeyCode::PageDown, KeyModifiers::NONE), &mut view);
        assert!(
            view.scroll_offset > after_up,
            "Page-Down came forward again"
        );
    }

    #[test]
    fn the_wheel_scrolls_the_conversation() {
        // The wheel is a nudge rather than a page: up moves toward older content, down
        // toward the newest, and a reader who scrolled away stops following.
        let mut view = ViewState::new();
        for index in 0..200 {
            view.transcript
                .push(Entry::prose(Role::User, format!("entry {index}")));
        }
        view.scroll_by(0, 20, 60);
        view.scroll_to_bottom();
        let bottom = view.scroll_offset;
        assert!(bottom > 0, "the conversation is longer than the viewport");

        handle_mouse(mouse(MouseEventKind::ScrollUp, 0, 0), &mut view);
        let back = view.scroll_offset;
        assert!(back < bottom, "the wheel went back: {back} < {bottom}");
        assert!(!view.following, "and stopped following the newest output");
        assert_eq!(bottom.saturating_sub(back), 3, "one notch is a nudge");

        handle_mouse(mouse(MouseEventKind::ScrollDown, 0, 0), &mut view);
        assert_eq!(view.scroll_offset, bottom, "and forward again");
    }

    #[test]
    fn a_click_outside_the_composer_is_ignored() {
        // Nothing in the transcript is clickable, so a click that is not on the prompt
        // has no meaning and must not move the caret.
        let mut view = ViewState::new();
        view.input.insert_str("draft");
        handle_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), 0, 0),
            &mut view,
        );
        assert_eq!(view.input.cursor(), 5, "the caret was left where it was");
    }

    #[test]
    fn streamed_output_does_not_drag_a_reader_who_scrolled_back() {
        // The end-to-end version of the rule: a turn in progress must not fight the
        // reader. Every streamed frame used to call `scroll_to_bottom`, so scrolling back
        // during a turn was impossible — the next token pulled the view down again.
        let (sender, mut receiver) = mpsc::channel::<Frame>(8);
        let mut view = ViewState::new();
        for index in 0..200 {
            view.transcript
                .push(Entry::prose(Role::User, format!("entry {index}")));
        }
        view.scroll_by(0, 20, 60);
        view.scroll_to_bottom();
        let _ = handle_key(key(KeyCode::PageUp, KeyModifiers::NONE), &mut view);
        let reading = view.scroll_offset;

        for delta in ["a new ", "token ", "arrives"] {
            assert!(
                sender
                    .try_send(Frame::Text {
                        delta: delta.to_owned()
                    })
                    .is_ok()
            );
        }
        drain_frames(&mut receiver, &mut view);
        assert_eq!(
            view.scroll_offset, reading,
            "the reader was left where they were while the model kept talking"
        );
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
    fn another_clients_prompt_lands_in_the_transcript_as_the_user() {
        // Watching a session means seeing both sides of it: an answer to a question the
        // reader never saw is unreadable.
        let (sender, mut receiver) = mpsc::channel::<Frame>(8);
        let mut view = ViewState::new();
        assert!(
            sender
                .try_send(Frame::User {
                    text: "somebody else asked".to_owned()
                })
                .is_ok()
        );
        drain_frames(&mut receiver, &mut view);
        let first = view.transcript.entries().first();
        assert_eq!(first.map(Entry::text), Some("somebody else asked"));
        assert_eq!(first.map(Entry::role), Some(Role::User));
        assert!(view.busy, "a turn is running");
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
            Frame::Usage {
                tokens: 12,
                completion_tokens: 9,
                cache_hit_tokens: 800,
                cache_miss_tokens: 200,
                duration_ms: 1_000,
                reasoning_tokens: 4,
                head_ms: 250,
                ttft_ms: 600,
                decode_ms: 300,
            },
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
                    answer: "answer".to_owned(),
                    reason: TurnEnd::Completed,
                })
                .is_ok()
        );
        drain_frames(&mut receiver, &mut view);
        assert!(!view.busy);
        assert!(!view.transcript.is_streaming());
        let last = view.transcript.entries().last();
        assert!(last.is_some_and(|entry| entry.text() == "answer"));
    }

    /// The defect this closes: the ending used to say only that a turn was over, so a
    /// turn that ran out of steps was drawn exactly like one that had finished — the last
    /// thing the model said became the answer, and the reader was left to notice the work
    /// had stopped.
    #[test]
    fn a_turn_that_stops_at_its_budget_is_reported_rather_than_dressed_as_an_answer() {
        let (sender, mut receiver) = mpsc::channel::<Frame>(8);
        let mut view = ViewState::new();
        view.begin_turn(7);
        for frame in [
            Frame::Text {
                delta: "now I will edit the server".to_owned(),
            },
            Frame::Done {
                answer: "now I will edit the server".to_owned(),
                reason: TurnEnd::MaxSteps,
            },
        ] {
            assert!(sender.try_send(frame).is_ok());
        }
        drain_frames(&mut receiver, &mut view);

        assert!(!view.busy, "the turn is over");
        let last = view.transcript.entries().last();
        assert_eq!(last.map(Entry::role), Some(Role::Harness), "a notice");
        assert!(
            last.is_some_and(|entry| entry.text().contains("step budget")),
            "the notice names the budget: {:?}",
            last.map(Entry::text)
        );
        assert!(
            last.is_some_and(|entry| entry.text().contains('7')),
            "and how far it got: {:?}",
            last.map(Entry::text)
        );
        let said = view
            .transcript
            .entries()
            .iter()
            .filter(|entry| entry.text() == "now I will edit the server")
            .count();
        assert_eq!(said, 1, "the narration is not also offered as the answer");
    }

    /// The other half of the same field: a reason that is not about the budget still has
    /// to reach the reader, and the sentence has to be about that reason.
    #[test]
    fn every_reason_a_turn_can_stop_for_is_said_in_words() {
        let cases = [
            (TurnEnd::MaxTokens, "token ceiling"),
            (TurnEnd::Interrupted, "interrupted"),
            (TurnEnd::Blocked, "policy"),
            (
                TurnEnd::Aborted {
                    reason: "the human said stop".to_owned(),
                },
                "the human said stop",
            ),
            (
                TurnEnd::Error {
                    message: "the model call failed".to_owned(),
                },
                "the model call failed",
            ),
        ];
        for (reason, expected) in cases {
            let notice = stopping_notice(&reason, 3);
            assert!(
                notice
                    .as_deref()
                    .is_some_and(|text| text.contains(expected)),
                "{reason:?} is reported as {notice:?}, which does not mention {expected:?}"
            );
        }
        assert_eq!(
            stopping_notice(&TurnEnd::Completed, 3),
            None,
            "a turn that finished has nothing to explain"
        );
        // The count is read by a person, so a budget that ended after one step does not
        // say "after 1 steps".
        let one = stopping_notice(&TurnEnd::MaxSteps, 1);
        assert!(
            one.as_deref()
                .is_some_and(|text| text.contains("after 1 step,")),
            "{one:?}"
        );
    }

    /// The answer arrives twice — streamed and then whole in the ending — and the
    /// interface used to draw both, so every completed turn finished with the same
    /// paragraph repeated.
    #[test]
    fn a_streamed_answer_is_settled_rather_than_repeated() {
        let (sender, mut receiver) = mpsc::channel::<Frame>(8);
        let mut view = ViewState::new();
        view.begin_turn(1);
        for frame in [
            Frame::Text {
                delta: "the answer".to_owned(),
            },
            Frame::Done {
                answer: "the answer".to_owned(),
                reason: TurnEnd::Completed,
            },
        ] {
            assert!(sender.try_send(frame).is_ok());
        }
        drain_frames(&mut receiver, &mut view);

        let said: Vec<&str> = view.transcript.entries().iter().map(Entry::text).collect();
        assert_eq!(said, vec!["the answer"], "drawn once, not twice");
        assert!(!view.transcript.is_streaming(), "and settled");
    }

    /// The frame still carries the answer whole, so a client whose deltas never arrived
    /// ends up with it. Reconciling must not turn into dropping.
    #[test]
    fn an_answer_that_never_streamed_is_still_shown() {
        let (sender, mut receiver) = mpsc::channel::<Frame>(8);
        let mut view = ViewState::new();
        view.begin_turn(1);
        assert!(
            sender
                .try_send(Frame::Done {
                    answer: "never streamed".to_owned(),
                    reason: TurnEnd::Completed,
                })
                .is_ok()
        );
        drain_frames(&mut receiver, &mut view);

        let said: Vec<&str> = view.transcript.entries().iter().map(Entry::text).collect();
        assert_eq!(said, vec!["never streamed"]);
    }

    /// What the throughput line is fed from: one frame per request, carrying the counters that
    /// request reported and the three durations it was measured with.
    #[test]
    fn usage_frames_feed_the_throughput_line() {
        let (sender, mut receiver) = mpsc::channel::<Frame>(8);
        let mut view = ViewState::new();
        assert!(
            sender
                .try_send(Frame::Usage {
                    tokens: 1_212,
                    completion_tokens: 300,
                    cache_hit_tokens: 900,
                    cache_miss_tokens: 100,
                    duration_ms: 2_500,
                    reasoning_tokens: 120,
                    head_ms: 200,
                    ttft_ms: 500,
                    decode_ms: 2_000,
                })
                .is_ok()
        );
        drain_frames(&mut receiver, &mut view);

        assert_eq!(
            view.tokens_used, 1_212,
            "the running total still accumulates"
        );
        assert_eq!(view.stats.last_rate(), Some(150));
        assert_eq!(view.stats.average_rate(), Some(150));
        assert_eq!(view.stats.cache_hit_percent(), Some(90));
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
                    name: "read".to_owned(),
                    arguments: serde_json::json!({"file_path": "src/view.rs"}),
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
        let Some(EntryKind::ToolCall {
            name, arguments, ..
        }) = view.transcript.entries().first().map(Entry::kind)
        else {
            panic!("a tool call entry: {:?}", view.transcript.entries());
        };
        assert_eq!(name, "read");
        // What the call is acting on has to reach the entry, because that is the half a
        // reader cannot get anywhere else: the session log has it, but a client watching a
        // turn is not reading the log as it is written.
        assert!(arguments.contains("src/view.rs"), "{arguments}");
    }

    #[test]
    fn a_tool_frame_without_arguments_is_the_tool_alone() {
        // What an agent that predates the field sends. The entry says nothing about the
        // call's arguments, and the compact line is the tool's name rather than the word
        // `null` — which is what the frame carries for "the sender did not say".
        let (sender, mut receiver) = mpsc::channel::<Frame>(8);
        let mut view = ViewState::new();
        assert!(
            sender
                .try_send(Frame::Tool {
                    name: "glob".to_owned(),
                    arguments: serde_json::Value::Null,
                })
                .is_ok()
        );
        drain_frames(&mut receiver, &mut view);
        let Some(EntryKind::ToolCall { arguments, .. }) =
            view.transcript.entries().first().map(Entry::kind)
        else {
            panic!("a tool call entry: {:?}", view.transcript.entries());
        };
        assert!(arguments.is_empty(), "{arguments}");
    }

    #[test]
    fn the_configured_detail_selects_the_rendering() {
        // Both directions, because the point of the setting is that one spelling reverts
        // the default and the other is the default.
        assert_eq!(detail_from(TuiDetail::Compact), Detail::Compact);
        assert_eq!(detail_from(TuiDetail::Full), Detail::Full);
        assert_eq!(Detail::default(), detail_from(TuiDetail::default()));
    }

    #[test]
    fn connection_frames_do_not_reach_the_transcript() {
        // The handshake and a `Bye` are about the link, not about the conversation. A
        // reader must not find them in the middle of an answer.
        let (sender, mut receiver) = mpsc::channel::<Frame>(8);
        let mut view = ViewState::new();
        for frame in [
            Frame::Bye,
            Frame::Attached(SessionInfo {
                session: "s".to_owned(),
                name: None,
                title: None,
                events: 0,
                busy: false,
                viewers: 1,
            }),
            Frame::Status(nanus_link::protocol::AgentInfo {
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
            reason: TurnEnd::Completed,
        }]);
        source.attach(&frames);
        assert_eq!(
            receiver.try_recv().ok(),
            Some(Frame::Done {
                answer: "answered".to_owned(),
                reason: TurnEnd::Completed,
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
    fn a_history_read_distinguishes_nothing_stored_from_something_unreadable() {
        // Two ways reading a conversation's history can go wrong, and they are different
        // things:
        //
        //   - nothing is stored: a new session, which is not a failure at all;
        //   - something is stored and cannot be read: a damaged log.
        //
        // Both fall back to an empty conversation, and that is the *policy* this pins — a
        // damaged transcript must not become a refusal to start. The difference between
        // them is a log line, which a behavioural test cannot see; what it can see is that
        // the session's identity survives, so a reader is still in the right conversation
        // rather than silently shown an empty one under a different name.
        let home = std::env::temp_dir().join(format!("nanus-tui-history-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let store = block_on(JsonlStore::new(&home)).expect("a store in the temporary directory");

        let recorded = SessionId::new("recorded");
        let mut saved = Session::new(recorded, 1, "/work");
        saved.append(nanus_domain::SessionEvent::UserMessage {
            text: "hello".to_owned(),
        });
        block_on(store.save(&saved)).expect("save");

        // The damaged log is written before the store is shared, because `handle` consumes
        // it and `session_dir` is the concrete type's own answer to where a log lives.
        let damaged = SessionId::new("damaged");
        let dir = JsonlStore::session_dir(&store, &damaged).expect("a session directory");
        std::fs::create_dir_all(&dir).expect("the directory");
        std::fs::write(dir.join("session.jsonl"), "not a session at all\n").expect("a damaged log");
        let store = store.handle();

        let info = |session: &str| SessionInfo {
            session: session.to_owned(),
            name: None,
            title: None,
            events: 0,
            busy: false,
            viewers: 1,
        };

        let read = block_on(history(&store, &info("recorded"), "/work"));
        assert_eq!(read.event_count(), 1, "a recorded log comes back whole");

        let fresh = block_on(history(&store, &info("never-saved"), "/work"));
        assert_eq!(fresh.event_count(), 0, "nothing stored is an empty session");
        assert_eq!(fresh.id().as_str(), "never-saved");
        assert_eq!(fresh.cwd(), "/work");

        let broken = block_on(history(&store, &info("damaged"), "/work"));
        assert_eq!(broken.event_count(), 0, "a damaged log reads as empty");
        assert_eq!(
            broken.id().as_str(),
            "damaged",
            "and it is still the session the client attached to"
        );
    }

    #[test]
    fn connecting_to_a_socket_nobody_is_serving_is_an_error_rather_than_a_panic() {
        let missing = Path::new("/definitely/not/a/socket");
        let store = nanus_kernel::runtime::block_on(open_store_for_test());
        let outcome = block_on(Remote::connect(missing, &store, Target::New { name: None }));
        assert!(outcome.is_err(), "a missing socket is refused");
        let Err(error) = outcome else { return };
        assert!(error.contains("/definitely/not/a/socket"), "{error}");
    }

    /// A store that is never read, for a test that only wants a connection refused.
    async fn open_store_for_test() -> StoreHandle {
        // `NANUS_HOME` is read by the resolver; the temporary directory is what keeps a
        // test from touching the real one.
        let home = std::env::temp_dir().join("nanus-tui-test-store");
        let built = JsonlStore::new(home).await;
        built.map_or_else(
            |error| panic!("a store in the temporary directory: {error}"),
            JsonlStore::handle,
        )
    }
}
