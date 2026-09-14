//! Slash commands: the lines the interface answers itself rather than sending anywhere.
//!
//! A leading slash makes the first word a command, which is what every interface with this
//! convention does, including the one these key bindings follow. The cost is that a prompt
//! opening with a path is not a prompt. The alternative — sending anything unrecognised to
//! the model — spends tokens answering a typo, and leaves a reader who mistyped a command
//! waiting for an answer to a question they did not ask, so the cost is worth paying and is
//! paid loudly: an unrecognised command says so and names the ones that exist.

/// A command the interface answers itself.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Command {
    /// Leave the interface — and, unless the agent is a service, the agent with it.
    Exit,
}

impl Command {
    /// Every command, by the names that reach it.
    ///
    /// The table is the list an unrecognised command is answered with, so a command added
    /// here is documented by existing rather than by somebody remembering to update a
    /// message.
    pub const NAMES: &'static [&'static str] = &["/exit", "/quit"];
}

/// What the interface makes of a line the reader submitted.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Submission {
    /// Not a command: it belongs to the model.
    Prompt,
    /// A command the interface answers itself.
    Run(Command),
    /// Something that looks like a command and is not one, by the name that was typed.
    Unknown(String),
}

/// Reads a submitted line.
///
/// Only the first word is looked at, so `/quit` and `/quit now` are the same command: a
/// reader who types a word after a command has not asked for something else.
#[must_use]
pub fn submission_of(text: &str) -> Submission {
    let mut words = text.split_whitespace();
    let Some(first) = words.next() else {
        return Submission::Prompt;
    };
    if !first.starts_with('/') {
        return Submission::Prompt;
    }
    match first {
        "/exit" | "/quit" => Submission::Run(Command::Exit),
        // A slash alone, or a path, or a typo: named as what was typed rather than
        // guessed at, because "no such command: /quitx" is what tells a reader they
        // fat-fingered it.
        other => Submission::Unknown(other.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_and_quit_are_the_same_command() {
        assert_eq!(submission_of("/exit"), Submission::Run(Command::Exit));
        assert_eq!(submission_of("/quit"), Submission::Run(Command::Exit));
        // Whitespace around it is the reader's business, not the command's.
        assert_eq!(submission_of("  /exit  "), Submission::Run(Command::Exit));
        assert_eq!(submission_of("/quit now"), Submission::Run(Command::Exit));
    }

    #[test]
    fn prose_is_prose() {
        assert_eq!(submission_of("exit the loop"), Submission::Prompt);
        assert_eq!(submission_of("what does /exit do?"), Submission::Prompt);
        assert_eq!(submission_of(""), Submission::Prompt);
        assert_eq!(submission_of("   "), Submission::Prompt);
    }

    /// The cost of the convention, written down where it is paid: a prompt that opens with
    /// a path is read as a command attempt. Sending it to the model instead would spend
    /// tokens on it and answer a question nobody asked.
    #[test]
    fn a_leading_slash_is_always_a_command_attempt() {
        assert_eq!(
            submission_of("/etc/hosts is wrong"),
            Submission::Unknown(String::from("/etc/hosts"))
        );
        assert_eq!(submission_of("/"), Submission::Unknown(String::from("/")));
        assert_eq!(
            submission_of("/quitx"),
            Submission::Unknown(String::from("/quitx")),
            "a typo is named as the typo rather than treated as a prompt"
        );
    }

    #[test]
    fn every_name_in_the_table_reaches_a_command() {
        for name in Command::NAMES {
            assert!(
                matches!(submission_of(name), Submission::Run(_)),
                "{name} is named but does not resolve"
            );
        }
    }
}
