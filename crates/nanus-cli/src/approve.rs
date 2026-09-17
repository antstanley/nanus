//! Approval at a terminal, for a headless run.
//!
//! `nanus run` is the one mode with no interface to press a key in, and the approval gate
//! still has to mean something there: when the loop reaches a call the sandbox does not
//! permit, this asks the person at the terminal and waits.
//!
//! ## Why the answer is read asynchronously
//!
//! A blocking read would stop the runtime thread, and the signal watcher that turns Ctrl-C
//! into an orderly stop lives on it. While the prompt was up, Ctrl-C would then be
//! *swallowed* — the handler installed for `SIGINT` cannot run — and a reader who changed
//! their mind could not get out. Reading through `tokio::io::stdin` puts the read on a
//! blocking thread and leaves the runtime running.
//!
//! ## Why a piped stdin is not an answerer
//!
//! Only a terminal is asked. `nanus run "task" < data` has a stdin that carries the task's
//! input, and reading an approval answer from it would eat that input and could read a
//! stray `y` in a file as consent. Without a terminal the answer is
//! [`ApprovalOutcome::Unavailable`], which the loop denies, exactly as if nobody were there
//! — because nobody is.

use std::io::IsTerminal as _;
use std::rc::Rc;

use nanus_bundle::Approver;
use nanus_domain::{ApprovalOutcome, ApprovalRequest};
use nanus_ports::LocalBoxFuture;
use tokio::io::AsyncReadExt as _;
use tokio::sync::Notify;

/// The longest answer worth reading, in bytes.
///
/// A terminal in canonical mode delivers one line, and a person types `y` or `n`. The cap
/// is what stops a paste from filling memory, and it is generous enough that a typed `yes`
/// or `no` is never truncated.
const MAX_ANSWER_BYTES: usize = 64;

/// Asks the terminal whether a tool call may run.
pub struct TerminalApprover {
    /// Whether stdin is a terminal worth asking.
    interactive: bool,
    /// Woken when the run is asked to stop, which abandons the question.
    stop: Option<Rc<Notify>>,
}

impl TerminalApprover {
    /// Builds an approver for this process's standard streams.
    #[must_use]
    pub fn standard() -> Self {
        Self {
            // Stdin, not stdout: the question goes to stderr and the answer comes from the
            // keyboard. A run whose *output* is redirected but whose input is a terminal is
            // still a person watching.
            interactive: std::io::stdin().is_terminal(),
            stop: None,
        }
    }

    /// Abandons the question when `stop` is woken, which is how `Ctrl-C` gets out of a
    /// prompt.
    ///
    /// Without this the key set the turn's stop flag and then *nothing happened*: the turn is
    /// parked inside this question, so it never reaches the checkpoint that reads the flag.
    /// The reader's one key for "stop" appeared to do nothing while the question was up, and
    /// they had to answer it before stopping for real.
    #[must_use]
    pub fn abandoned_when(mut self, stop: Rc<Notify>) -> Self {
        self.stop = Some(stop);
        self
    }
}

impl Approver for TerminalApprover {
    fn decide(&self, request: ApprovalRequest) -> LocalBoxFuture<'_, ApprovalOutcome> {
        Box::pin(async move {
            if !self.interactive {
                return ApprovalOutcome::Unavailable;
            }
            ask(&request);
            let answer = match self.stop.as_ref() {
                Some(stop) => {
                    tokio::select! {
                        answer = read_answer() => answer,
                        // An interrupt is not an answer, and it denies the call: the reader
                        // asked to stop, and `Cancelled` says exactly that while the turn
                        // reads its flag at the next checkpoint.
                        () = stop.notified() => Vec::new(),
                    }
                }
                None => read_answer().await,
            };
            answer_of(&answer)
        })
    }
}

/// Writes the question to stderr.
///
/// The call's arguments are deliberately absent, exactly as [`ApprovalRequest`] carries
/// none: model-controlled text must not be put in front of the decision. The tool and the
/// harness's reason are what a person decides on, and `--verbose` shows what the call is.
fn ask(request: &ApprovalRequest) {
    eprintln!("nanus: approve the `{}` call?", request.tool.as_str());
    if let Some(reason) = request.reason.as_deref() {
        eprintln!("       {reason}");
    }
    eprintln!("       [y] allow once  [n] deny");
    // A prompt without a newline, so the cursor sits after it and the answer is typed where
    // it is read from.
    eprint!("nanus: ");
}

/// Reads one line from the terminal, bounded.
///
/// Returns the bytes typed before the newline, empty for an empty line, and empty for
/// end-of-file too — the caller cannot tell "the reader pressed Enter" from "the reader
/// pressed Ctrl-D", and both mean the question went unanswered.
async fn read_answer() -> Vec<u8> {
    let mut stdin = tokio::io::stdin();
    let mut line = Vec::new();
    loop {
        let mut byte = [0_u8; 1];
        match stdin.read(&mut byte).await {
            // End of file and a read failure are the same fact here: no answer arrived.
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let next = byte.first().copied().unwrap_or(b'\n');
                if next == b'\n' || next == b'\r' {
                    break;
                }
                if line.len() < MAX_ANSWER_BYTES {
                    line.push(next);
                }
            }
        }
    }
    line
}

/// Turns typed bytes into a decision.
///
/// `y` allows the call once and `n` denies it. Anything else — an empty line, a word that
/// starts with neither, end-of-file — is a cancellation rather than a denial, because a
/// reader who did not answer has not said no either; both deny the call, and the model is
/// told which happened.
fn answer_of(answer: &[u8]) -> ApprovalOutcome {
    match answer.first().map(u8::to_ascii_lowercase) {
        Some(b'y') => ApprovalOutcome::AllowedOnce,
        Some(b'n') => ApprovalOutcome::Rejected,
        _ => ApprovalOutcome::Cancelled,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yes_means_allow_once_and_no_means_rejected() {
        assert_eq!(answer_of(b"y"), ApprovalOutcome::AllowedOnce);
        assert_eq!(answer_of(b"Y"), ApprovalOutcome::AllowedOnce);
        assert_eq!(answer_of(b"yes"), ApprovalOutcome::AllowedOnce);
        assert_eq!(answer_of(b"n"), ApprovalOutcome::Rejected);
        assert_eq!(answer_of(b"N"), ApprovalOutcome::Rejected);
        assert_eq!(answer_of(b"no"), ApprovalOutcome::Rejected);
    }

    #[test]
    fn an_unanswered_question_is_cancelled_rather_than_allowed() {
        // The fail-closed direction: an empty line, a stray word, and end-of-file all deny,
        // and none of them reads as consent.
        assert_eq!(answer_of(b""), ApprovalOutcome::Cancelled);
        assert_eq!(answer_of(b"what?"), ApprovalOutcome::Cancelled);
        assert_eq!(answer_of(b"\n"), ApprovalOutcome::Cancelled);
        assert!(!ApprovalOutcome::Cancelled.is_allowed());
    }

    #[tokio::test]
    async fn without_a_terminal_the_answer_is_unavailable() {
        // A piped stdin is not an answerer, and the loop denies what it cannot have
        // approved — the same outcome as nobody being at the keyboard.
        let approver = TerminalApprover {
            interactive: false,
            stop: None,
        };
        let outcome = approver.decide(ApprovalRequest::new(tool())).await;
        assert_eq!(outcome, ApprovalOutcome::Unavailable);
    }

    fn tool() -> nanus_domain::ToolName {
        nanus_domain::ToolName::new("bash").unwrap_or_else(|_| panic!("a valid tool name"))
    }

    /// An interrupt abandons the question instead of leaving the reader stuck in it.
    ///
    /// Note the order: the question is *already* open and nobody is typing, because that is
    /// the state a reader is in when they press `Ctrl-C` here.
    #[tokio::test]
    async fn an_interrupt_abandons_an_open_question() {
        let stop = Rc::new(Notify::new());
        let approver = TerminalApprover {
            interactive: true,
            stop: Some(Rc::clone(&stop)),
        };
        let asking = approver.decide(ApprovalRequest::new(tool()));
        let answering = async {
            // Let the question open, then raise the interrupt.
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            stop.notify_one();
        };
        let (outcome, ()) = tokio::join!(asking, answering);
        // Cancelled rather than `Rejected`: the reader did not answer, which is a different
        // thing to have said — and both deny the call.
        assert_eq!(outcome, ApprovalOutcome::Cancelled);
        assert!(!outcome.is_allowed());
    }

    /// An interrupt raised *before* the question opens is still honoured, which is what makes
    /// the key reliable rather than a race: `notify_one` stores a permit, so a waiter that
    /// arrives late finds it.
    #[tokio::test]
    async fn an_interrupt_before_the_question_is_not_lost() {
        let stop = Rc::new(Notify::new());
        stop.notify_one();
        let approver = TerminalApprover {
            interactive: true,
            stop: Some(Rc::clone(&stop)),
        };
        let outcome = approver.decide(ApprovalRequest::new(tool())).await;
        assert_eq!(outcome, ApprovalOutcome::Cancelled);
    }
}
