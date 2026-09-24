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
    /// Write the session's model figures into the transcript.
    Stats,
    /// Draw the key list.
    ///
    /// The same overlay `?` opens, so a reader who reaches for a command rather than a key
    /// gets the same list rather than a second one that says almost the same thing.
    Help,
    /// Empty the transcript.
    ///
    /// The keys and the draft survive, because this is about the conversation on screen and
    /// not about what is being typed: the panel is a view, and the session it views is
    /// untouched.
    Clear,
    /// Switch the model: open the selector when no id is given, and switch to the named one when
    /// it is.
    ///
    /// The selector rather than a blind cycle, because the list is the agent's and a reader who
    /// wants a particular model should be able to read the ids before choosing one. `Alt+P` is
    /// still the cycle for a reader who would rather press a key than a dialog.
    Model,
    /// Choose how hard the model is asked to think: open the chooser when no step is given, and
    /// switch to the named one when it is.
    ///
    /// The chooser lists only the steps the current model takes, which is a provider fact that
    /// differs between models, so a reader cannot pick one the provider would refuse. `Alt+T` is
    /// still the cycle.
    Effort,
    /// Choose a provider (and plan) to send requests to: open the chooser when none is named, and
    /// switch to the named one when it is.
    ///
    /// A provider change rebuilds the agent's adapter, and a provider with no credential is the one
    /// refusal the interface can act on: it is answered, and the interface asks whether to store a
    /// key before trying again.
    Provider,
    /// Set, read, or move the session's goal.
    ///
    /// A goal is a durable objective the session carries, so this is the one command that has to
    /// reach the agent rather than being answered on screen: the interface sends it and the agent
    /// answers. A bare `/goal` reads the current one, a lifecycle word (`pause`, `resume`,
    /// `complete`, `done`, `abandon`, `clear`) moves it, and anything else is an objective to set.
    Goal,
    /// Put the newest answer on the clipboard.
    ///
    /// A command as well as a key, because the commonest thing a reader wants out of a transcript is
    /// the last answer, and getting it by highlighting it with the mouse is a fiddly way to ask for
    /// something the interface knows the bounds of.
    Copy,
}

impl Command {
    /// Every command, with the names that reach it.
    ///
    /// One table rather than two, because the two uses must not drift: this is what
    /// [`submission_of`] resolves against *and* what an unrecognised command is answered with, so a
    /// name cannot resolve without being offered, and a command cannot be offered without resolving.
    /// `/model` and `/copy` were each added to the enum and to the resolver while this list stayed as
    /// it was, which made the interface tell a reader who mistyped one that it did not exist.
    ///
    /// The variants are written out rather than derived, because the language cannot enumerate an
    /// enum; the test below is what holds the table and the enum together.
    const TABLE: &'static [(Self, &'static [&'static str])] = &[
        (Self::Exit, &["/exit", "/quit"]),
        (Self::Stats, &["/stats"]),
        (Self::Help, &["/help"]),
        (Self::Clear, &["/clear"]),
        (Self::Model, &["/model"]),
        (Self::Effort, &["/effort"]),
        (Self::Provider, &["/provider"]),
        (Self::Goal, &["/goal"]),
        (Self::Copy, &["/copy"]),
    ];

    /// Every name that reaches a command, in the order a refusal lists them.
    #[must_use]
    pub fn names() -> Vec<&'static str> {
        Self::TABLE
            .iter()
            .flat_map(|(_, names)| names.iter().copied())
            .collect()
    }

    /// The same names as a sentence, which is how a refusal says them.
    #[must_use]
    pub fn names_sentence() -> String {
        sentence(&Self::names())
    }
}

/// Joins names into a sentence: commas between them and one `and` before the last.
///
/// The refusal used to put `and` between every pair, so nine commands read as nine clauses and a
/// reader scanning for the one they meant had no shape to scan. Commas are that shape; the single
/// `and` at the end is what keeps it a sentence rather than a table. Nothing else is special-cased,
/// because there is nothing else: a lone name is a name, and a pair needs no comma between two.
///
/// An empty list is an empty string, which no caller can reach — [`Command::TABLE`] is never empty,
/// and a test below holds that down.
#[must_use]
fn sentence(names: &[&str]) -> String {
    let Some((last, rest)) = names.split_last() else {
        return String::new();
    };
    if rest.is_empty() {
        return (*last).to_owned();
    }
    let mut joined = rest.join(", ");
    joined.push_str(" and ");
    joined.push_str(last);
    joined
}

/// What the interface makes of a line the reader submitted.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Submission {
    /// Not a command: it belongs to the model.
    Prompt,
    /// A command the interface runs itself.
    ///
    /// Not a command *for* the interface and not a prompt: `!` is the escape hatch that does not go
    /// through the model, so the line is handed to a shell rather than to anything here. The whole
    /// line is the command, unlike a slash command, where only the first word is read: a shell
    /// command is a command *line*.
    Shell(String),
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
    let trimmed = text.trim_start();
    // `!` leads, and everything after it is the command: what follows is a command line rather than
    // a word, so the leading space a reader typed is theirs to leave out.
    if let Some(command) = trimmed.strip_prefix('!') {
        return Submission::Shell(command.trim_start().to_owned());
    }
    let mut words = trimmed.split_whitespace();
    let Some(first) = words.next() else {
        return Submission::Prompt;
    };
    if !first.starts_with('/') {
        return Submission::Prompt;
    }
    // Read from the table rather than matched again here, so the names that resolve and the names
    // that are offered are the same list.
    match Command::TABLE
        .iter()
        .find(|(_, names)| names.contains(&first))
    {
        Some((command, _)) => Submission::Run(*command),
        // A slash alone, or a path, or a typo: named as what was typed rather than
        // guessed at, because "no such command: /quitx" is what tells a reader they
        // fat-fingered it.
        None => Submission::Unknown(first.to_owned()),
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

    /// The figures a session has are not on the screen at once — four readings fit under the
    /// composer and the rest do not — so the command is how the whole set is read.
    #[test]
    fn stats_is_a_command() {
        assert_eq!(submission_of("/stats"), Submission::Run(Command::Stats));
        assert_eq!(submission_of("  /stats  "), Submission::Run(Command::Stats));
        // Only the first word is read, so a word after it changes nothing.
        assert_eq!(submission_of("/stats now"), Submission::Run(Command::Stats));
    }

    /// The two commands that act on the screen rather than on the session: they are the
    /// interface's, and they are answered in a recording too, where there is no agent at all.
    #[test]
    fn help_and_clear_are_commands() {
        assert_eq!(submission_of("/help"), Submission::Run(Command::Help));
        assert_eq!(submission_of("/clear"), Submission::Run(Command::Clear));
        // Only the first word is read, so a word after either changes nothing.
        assert_eq!(submission_of("/clear now"), Submission::Run(Command::Clear));
        assert_eq!(submission_of("/help me"), Submission::Run(Command::Help));
    }

    /// The model switch has a default and an argument, which is what makes it a command
    /// rather than only a key: `Alt+P` cycles and `/model <id>` names one.
    #[test]
    fn model_is_a_command() {
        assert_eq!(submission_of("/model"), Submission::Run(Command::Model));
        assert_eq!(
            submission_of("/model deepseek-v4-pro"),
            Submission::Run(Command::Model)
        );
    }

    /// The effort has the same shape as the model: `Alt+T` cycles and `/effort <state>` names a
    /// step, which is what a command with a useful default and a useful argument offers.
    #[test]
    fn effort_is_a_command() {
        assert_eq!(submission_of("/effort"), Submission::Run(Command::Effort));
        assert_eq!(
            submission_of("/effort high"),
            Submission::Run(Command::Effort)
        );
    }

    /// The provider is the command that configures a vendor, and it is a command rather than a
    /// key because it has a list to read before one is named.
    #[test]
    fn provider_is_a_command() {
        assert_eq!(
            submission_of("/provider"),
            Submission::Run(Command::Provider)
        );
        assert_eq!(
            submission_of("/provider anthropic"),
            Submission::Run(Command::Provider)
        );
    }

    /// `!` is the escape hatch: the line is a shell command rather than a prompt, and the whole line
    /// is the command rather than the first word.
    #[test]
    fn a_bang_line_is_a_shell_command() {
        assert_eq!(
            submission_of("!ls -la | wc -l"),
            Submission::Shell(String::from("ls -la | wc -l"))
        );
        // The space after the `!` is the reader's, not part of the command.
        assert_eq!(
            submission_of("! pwd"),
            Submission::Shell(String::from("pwd"))
        );
        // Leading whitespace does not make it prose: a reader who indented their command meant it.
        assert_eq!(
            submission_of("  !git status"),
            Submission::Shell(String::from("git status"))
        );
        // A `!` with nothing after it is a command with nothing in it, which the interface refuses
        // rather than running: an empty shell line is not a mistake worth guessing at.
        assert_eq!(submission_of("!"), Submission::Shell(String::new()));
        assert_eq!(submission_of("!  "), Submission::Shell(String::new()));
        // And a `!` that is not the first character is just punctuation in a sentence.
        assert_eq!(submission_of("that was exciting!"), Submission::Prompt);
        assert_eq!(submission_of("wow! /exit"), Submission::Prompt);
    }

    /// The goal has a default and an argument, like the model: `/goal` reads the current one and
    /// `/goal <objective>` sets one, with the lifecycle words between them.
    #[test]
    fn goal_is_a_command() {
        assert_eq!(submission_of("/goal"), Submission::Run(Command::Goal));
        assert_eq!(submission_of("  /goal  "), Submission::Run(Command::Goal));
        assert_eq!(submission_of("/goal pause"), Submission::Run(Command::Goal));
        assert_eq!(
            submission_of("/goal reduce p95 latency below 120 ms"),
            Submission::Run(Command::Goal),
            "the whole line is the command's argument, not the first word after it"
        );
    }

    #[test]
    fn copy_is_a_command() {
        assert_eq!(submission_of("/copy"), Submission::Run(Command::Copy));
        assert_eq!(submission_of("  /copy  "), Submission::Run(Command::Copy));
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

    /// The table and the enum are one list: every command is in the table under the names that reach
    /// it, every one of those names resolves back to its command, and the table holds nothing the enum
    /// does not have.
    ///
    /// The variants are written out here because the language cannot enumerate them, and that written
    /// list is exactly what this test exists to check — a command added to the enum and forgotten in
    /// the table is a command the interface answers a typo by denying.
    #[test]
    fn every_command_is_named_and_every_name_reaches_its_command() {
        let every = [
            Command::Exit,
            Command::Stats,
            Command::Help,
            Command::Clear,
            Command::Model,
            Command::Effort,
            Command::Provider,
            Command::Goal,
            Command::Copy,
        ];
        for command in every {
            let (_, names) = Command::TABLE
                .iter()
                .find(|(held, _)| *held == command)
                .unwrap_or_else(|| panic!("{command:?} reaches nothing: it is not in the table"));
            assert!(!names.is_empty(), "{command:?} has no name");
            for name in *names {
                assert_eq!(submission_of(name), Submission::Run(command), "{name}");
            }
        }
        assert_eq!(
            Command::TABLE.len(),
            every.len(),
            "the table holds a command the enum no longer has, or is missing one"
        );

        // And the sentence a typo is answered with lists all of them, which is the whole point of
        // the table being one list rather than two.
        let offered = Command::names();
        assert_eq!(
            offered,
            vec![
                "/exit",
                "/quit",
                "/stats",
                "/help",
                "/clear",
                "/model",
                "/effort",
                "/provider",
                "/goal",
                "/copy"
            ]
        );
        for name in offered {
            assert!(matches!(submission_of(name), Submission::Run(_)), "{name}");
        }
    }

    /// The names a refusal offers are a sentence rather than a chain of `and`s: commas between
    /// them and one `and` before the last, whatever the length.
    #[test]
    fn a_list_of_names_reads_as_a_sentence() {
        assert_eq!(sentence(&[]), "");
        assert_eq!(sentence(&["/exit"]), "/exit");
        assert_eq!(sentence(&["/exit", "/quit"]), "/exit and /quit");
        assert_eq!(
            sentence(&["/exit", "/quit", "/stats"]),
            "/exit, /quit and /stats"
        );

        // And the real list, in the order the table offers them: the property above is about the
        // joining, and this is about what a reader who mistypes a command actually sees.
        assert_eq!(
            Command::names_sentence(),
            "/exit, /quit, /stats, /help, /clear, /model, /effort, /provider, /goal and /copy"
        );
    }
}
