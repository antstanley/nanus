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
//! | `Up` / `Down` | browse submitted prompts |
//! | `PageUp` / `PageDown` | scroll the transcript |
//! | `Ctrl+L` | clear the transcript |
//! | `Ctrl+C` / `Ctrl+D` | quit |

use std::io;
use std::rc::Rc;
use std::time::Duration;

use nanus_bundle::{AgentRunner, Harness, Progress};
use nanus_domain::{Session, ToolName, Usage};
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{
    self, Event as TerminalEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use tokio::sync::mpsc;

use crate::transcript::{Entry, Role};
use crate::view::ViewState;

/// How long to wait for a terminal event before redrawing.
///
/// A short poll keeps streaming text visible without spinning: the loop wakes often
/// enough to paint a delta and rarely enough to stay idle.
const TICK: Duration = Duration::from_millis(50);

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

/// Runs the interactive interface against `harness`.
///
/// # Errors
///
/// Returns an error when the terminal cannot be put into raw mode or an event cannot
/// be read. The terminal is restored either way.
pub fn run(harness: &Harness) -> io::Result<()> {
    let mut guard = TerminalGuard::enter();
    let workspace = std::env::current_dir()?;
    let session = harness.new_session(&workspace);
    let mut view = ViewState::new();
    view.transcript.push(Entry::notice(format!(
        "workspace {} · model {} · {} tools",
        workspace.display(),
        harness.llm.model(),
        harness.tool_count()
    )));

    let (sender, mut receiver) = mpsc::channel::<Update>(256);
    let runner: Rc<AgentRunner> = Rc::clone(&harness.runner);

    loop {
        guard.terminal().draw(|frame| view.render(frame))?;
        if event::poll(TICK)? {
            let TerminalEvent::Key(key) = event::read()? else {
                continue;
            };
            // Windows reports both press and release; only a press is a keystroke.
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match handle_key(key, &mut view) {
                Outcome::Quit => break,
                Outcome::Submit(prompt) => {
                    // The transcript is seeded here, where the mutable view lives.
                    view.transcript
                        .push(Entry::prose(Role::User, prompt.clone()));
                    view.begin_turn(1);
                    view.scroll_to_bottom();
                    spawn_turn(&runner, &session, prompt, &sender);
                }
                Outcome::Continue => {}
            }
        }
        drain_updates(&mut receiver, &mut view);
        if !view.busy {
            // Nothing is streaming, so the loop can wait for a key rather than
            // redrawing on every tick.
            drain_updates(&mut receiver, &mut view);
        }
    }

    // A shutdown that fails still exits: the terminal has already been restored by the
    // guard, and the run is over.
    if let Err(error) = harness.shutdown() {
        tracing::warn!(%error, "the composition did not shut down cleanly");
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
            view.input.history_previous();
            Outcome::Continue
        }
        KeyCode::Down => {
            view.input.history_next();
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

/// Applies every queued update to the view.
fn drain_updates(receiver: &mut mpsc::Receiver<Update>, view: &mut ViewState) {
    while let Ok(update) = receiver.try_recv() {
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
}

#[cfg(test)]
mod tests {
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
}
