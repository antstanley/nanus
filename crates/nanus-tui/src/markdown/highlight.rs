//! A lexer for fenced code blocks.
//!
//! ## Why this is hand-written
//!
//! The renderer is a pure function of the transcript, and a highlighting library is not:
//! the usual ones load a syntax definition and a theme from disk, or embed megabytes of
//! them, and either way the answer would be coloured by whatever is installed on the
//! machine rather than by what the interface draws. What a coding answer is written in is
//! a handful of languages, and a lexer for those — keywords, comments, strings, numbers —
//! is a few hundred lines with no I/O at all.
//!
//! ## What it is not
//!
//! It is not a parser and it does not pretend to be one. A lifetime in Rust is not a
//! character literal, an identifier is a keyword only because it is spelled like one, and
//! a language that is not in the table is drawn verbatim: the classes below decide colour,
//! and nothing here decides *meaning*. Colouring a fence wrongly is worse than leaving it
//! plain, so an unrecognised language keeps every cell it would have had.
//!
//! A construct that survives the end of a line — a block comment, a triple-quoted string —
//! is carried to the next line, so a half-arrived answer is coloured the way it will be
//! when it finishes. A single-quoted string that never closes is the newest line of itself
//! and is drawn as one, because treating the rest of the answer as code is worse.

/// What a run of characters is, for the purpose of colour.
///
/// Deliberately a colour vocabulary rather than a syntax tree: the renderer maps a class to
/// a style and does nothing else with it, and an exhaustive match there means a class added
/// here cannot quietly reach the screen unstyled.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Class {
    /// Ordinary code.
    Plain,
    /// A word the language reserves, or a constant such as `true`.
    Keyword,
    /// A string literal, its quotes included.
    Literal,
    /// A numeric literal.
    Number,
    /// A comment, to the end of its line or of its block.
    Comment,
}

/// The lexical syntax of one language.
struct Rules {
    /// The words drawn as keywords.
    keywords: &'static [&'static str],
    /// Where a comment runs to the end of the line, if it does.
    line_comment: Option<&'static str>,
    /// The delimiters of a comment that spans lines, if the language has one.
    block_comment: Option<(&'static str, &'static str)>,
    /// Whether `'` opens a string. False in Rust, where it opens a lifetime.
    single_quotes: bool,
    /// Whether `"""` and `'''` open a string that spans lines.
    triple_quotes: bool,
}

/// A construct that outlives the line it started on.
#[derive(Clone, Copy)]
enum Open {
    /// A block comment, waiting for this terminator.
    Block(&'static str),
    /// A triple-quoted string, waiting for this terminator.
    Triple(&'static str),
}

/// Lexes one fence, one line at a time.
pub(crate) struct Highlighter {
    /// The language's syntax, or `None` for a language that is drawn verbatim.
    rules: Option<&'static Rules>,
    /// What the previous line left open, if anything.
    open: Option<Open>,
}

impl Highlighter {
    /// Builds a highlighter for the language named on a fence.
    ///
    /// The name is read as a fence writes it: `rust,ignore`, `ts`, `Python` and `sh` all
    /// resolve, and anything else — including an empty name — colours nothing. An unknown
    /// language is not an error: a fence is drawn as it always was, in the code style.
    #[must_use]
    pub(crate) fn for_language(name: &str) -> Self {
        let head = name
            .trim()
            .split([',', ';', '{', '\t', ' '])
            .next()
            .map_or("", str::trim);
        Self {
            rules: rules_for(&head.to_ascii_lowercase()),
            open: None,
        }
    }

    /// Classifies one line, in order, as runs of the same class.
    ///
    /// Runs rather than characters: a span per character would be thousands of spans for a
    /// long fence, and the merging is what keeps a document's worth of code to a handful.
    /// Every character of `text` is in exactly one returned run, so the renderer's output
    /// says what the source said.
    pub(crate) fn line(&mut self, text: &str) -> Vec<(Class, String)> {
        let Some(rules) = self.rules else {
            return vec![(Class::Plain, text.to_owned())];
        };
        let mut out: Vec<(Class, String)> = Vec::new();
        let mut index = 0_usize;
        while index < text.len() {
            let rest = &text[index..];
            // A construct carried in from the previous line is finished first, and it can
            // finish in the middle of this one: the code after `*/` is code again.
            let (class, used) = if let Some(open) = self.open {
                let (class, used, closed) = resume(open, rest);
                if closed {
                    self.open = None;
                }
                (class, used)
            } else {
                let (class, used, opened) = fresh(rules, rest);
                self.open = opened;
                (class, used)
            };
            push(&mut out, class, &rest[..used]);
            index = index.saturating_add(used);
        }
        out
    }
}

/// Consumes the rest of a construct that started on an earlier line.
///
/// Returns the class to draw it with, how many bytes of `text` it consumed, and whether it
/// closed: an unterminated block comment at the end of the answer is a comment to the end
/// of the answer, which is what a reader watching it arrive expects to see.
fn resume(open: Open, text: &str) -> (Class, usize, bool) {
    let (terminator, class) = match open {
        Open::Block(end) => (end, Class::Comment),
        Open::Triple(end) => (end, Class::Literal),
    };
    let Some(at) = text.find(terminator) else {
        return (class, text.len(), false);
    };
    (
        class,
        at.saturating_add(terminator.len()).min(text.len()),
        true,
    )
}

/// Classifies the token at the start of `text`, and whatever it leaves open.
fn fresh(rules: &Rules, text: &str) -> (Class, usize, Option<Open>) {
    if let Some(marker) = rules.line_comment
        && text.starts_with(marker)
    {
        return (Class::Comment, text.len(), None);
    }
    if let Some((start, end)) = rules.block_comment
        && text.starts_with(start)
    {
        return block_comment(start, end, text);
    }
    if rules.triple_quotes
        && let Some(quote) = triple_quote(text)
    {
        return triple_string(quote, text);
    }
    if let Some(used) = literal_len(rules, text) {
        return (Class::Literal, used, None);
    }
    if let Some(used) = number_len(text) {
        return (Class::Number, used, None);
    }
    if let Some(used) = identifier_len(text) {
        let word = &text[..used];
        let class = if rules.keywords.contains(&word) {
            Class::Keyword
        } else {
            Class::Plain
        };
        return (class, used, None);
    }
    (Class::Plain, first_char_len(text), None)
}

/// Consumes a block comment, opening one that does not close on this line.
fn block_comment(start: &str, end: &'static str, text: &str) -> (Class, usize, Option<Open>) {
    let after = &text[start.len().min(text.len())..];
    let Some(at) = after.find(end) else {
        return (Class::Comment, text.len(), Some(Open::Block(end)));
    };
    let used = start.len().saturating_add(at).saturating_add(end.len());
    (Class::Comment, used.min(text.len()), None)
}

/// Consumes a triple-quoted string, opening one that does not close on this line.
fn triple_string(quote: &'static str, text: &str) -> (Class, usize, Option<Open>) {
    let after = &text[quote.len().min(text.len())..];
    let Some(at) = after.find(quote) else {
        return (Class::Literal, text.len(), Some(Open::Triple(quote)));
    };
    let used = quote.len().saturating_add(at).saturating_add(quote.len());
    (Class::Literal, used.min(text.len()), None)
}

/// Returns the quote that opens a string spanning lines, when one opens here.
fn triple_quote(text: &str) -> Option<&'static str> {
    if text.starts_with("\"\"\"") {
        return Some("\"\"\"");
    }
    if text.starts_with("'''") {
        return Some("'''");
    }
    None
}

/// Measures a string literal starting at the front of `text`, quotes included.
///
/// An unterminated one is measured to the end of the line rather than refused: it is what a
/// streamed answer looks like in the middle of a string, and the next delta will close it.
fn literal_len(rules: &Rules, text: &str) -> Option<usize> {
    let open = match text.chars().next()? {
        '"' => '"',
        '\'' if rules.single_quotes => '\'',
        _ => return None,
    };
    let mut escaped = false;
    for (offset, character) in text.char_indices().skip(1) {
        if escaped {
            escaped = false;
            continue;
        }
        match character {
            '\\' => escaped = true,
            _ if character == open => return Some(offset.saturating_add(character.len_utf8())),
            _ => {}
        }
    }
    Some(text.len())
}

/// Measures a numeric literal starting at the front of `text`.
///
/// A dot belongs to the number only when a digit follows it, so `1.5` is one number while
/// `1..2` is a number, punctuation, and a number — which is what a reader sees in the
/// source, and a range drawn as one token reads as a mistake.
fn number_len(text: &str) -> Option<usize> {
    if !text.starts_with(|character: char| character.is_ascii_digit()) {
        return None;
    }
    let mut used = 0_usize;
    let mut dot_available = true;
    for (offset, character) in text.char_indices() {
        let keep = if character.is_ascii_alphanumeric() || character == '_' {
            true
        } else if character == '.' && dot_available {
            dot_available = false;
            text.get(offset.saturating_add(1)..)
                .is_some_and(|rest| rest.starts_with(|next: char| next.is_ascii_digit()))
        } else {
            false
        };
        if !keep {
            break;
        }
        used = offset.saturating_add(character.len_utf8());
    }
    Some(used)
}

/// Measures an identifier starting at the front of `text`.
fn identifier_len(text: &str) -> Option<usize> {
    let mut used = 0_usize;
    for (offset, character) in text.char_indices() {
        if !(character.is_alphanumeric() || character == '_') {
            break;
        }
        used = offset.saturating_add(character.len_utf8());
    }
    (used > 0).then_some(used)
}

/// The byte length of the first character, or zero for an empty string.
fn first_char_len(text: &str) -> usize {
    text.chars().next().map_or(0, char::len_utf8)
}

/// Appends a run, merging it into the previous one when the class is the same.
fn push(out: &mut Vec<(Class, String)>, class: Class, text: &str) {
    if text.is_empty() {
        return;
    }
    if let Some((last_class, last)) = out.last_mut()
        && *last_class == class
    {
        last.push_str(text);
        return;
    }
    out.push((class, text.to_owned()));
}

/// The syntax of the languages a coding answer is usually written in.
///
/// A language absent from this table is drawn verbatim. That is the deliberate answer for
/// `diff`, `console`, and `text`, and the honest one for everything else: a fence the
/// interface cannot read is a fence it must not colour.
fn rules_for(language: &str) -> Option<&'static Rules> {
    let rules = match language {
        "rust" | "rs" => &RUST,
        "python" | "py" => &PYTHON,
        "javascript" | "js" | "jsx" | "mjs" | "cjs" => &JAVASCRIPT,
        "typescript" | "ts" | "tsx" => &TYPESCRIPT,
        "json" | "json5" => &JSON,
        "toml" => &TOML,
        "bash" | "sh" | "shell" | "zsh" => &BASH,
        "yaml" | "yml" => &YAML,
        _ => return None,
    };
    Some(rules)
}

/// Rust: line and block comments, no single-quoted strings.
const RUST: Rules = Rules {
    keywords: &[
        "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum",
        "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move",
        "mut", "pub", "ref", "return", "self", "Self", "static", "struct", "super", "trait",
        "true", "type", "unsafe", "use", "where", "while", "bool", "str", "usize", "u8", "u16",
        "u32", "u64", "i8", "i16", "i32", "i64", "f32", "f64",
    ],
    line_comment: Some("//"),
    block_comment: Some(("/*", "*/")),
    single_quotes: false,
    triple_quotes: false,
};

/// Python: hashed comments and strings that span lines.
const PYTHON: Rules = Rules {
    keywords: &[
        "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del",
        "elif", "else", "except", "False", "finally", "for", "from", "global", "if", "import",
        "in", "is", "lambda", "None", "nonlocal", "not", "or", "pass", "raise", "return", "self",
        "True", "try", "while", "with", "yield",
    ],
    line_comment: Some("#"),
    block_comment: None,
    single_quotes: true,
    triple_quotes: true,
};

/// JavaScript: block and line comments, both quote styles, no construct that spans lines.
const JAVASCRIPT: Rules = Rules {
    keywords: &[
        "async",
        "await",
        "break",
        "case",
        "catch",
        "class",
        "const",
        "continue",
        "default",
        "delete",
        "do",
        "else",
        "export",
        "extends",
        "false",
        "finally",
        "for",
        "from",
        "function",
        "if",
        "import",
        "in",
        "instanceof",
        "let",
        "new",
        "null",
        "of",
        "return",
        "switch",
        "this",
        "throw",
        "true",
        "try",
        "typeof",
        "undefined",
        "var",
        "void",
        "while",
        "yield",
    ],
    line_comment: Some("//"),
    block_comment: Some(("/*", "*/")),
    single_quotes: true,
    triple_quotes: false,
};

/// TypeScript: JavaScript plus the words the type syntax adds.
const TYPESCRIPT: Rules = Rules {
    keywords: &[
        "and",
        "any",
        "as",
        "asserts",
        "async",
        "await",
        "boolean",
        "break",
        "case",
        "catch",
        "class",
        "const",
        "constructor",
        "continue",
        "declare",
        "default",
        "delete",
        "do",
        "else",
        "enum",
        "export",
        "extends",
        "false",
        "finally",
        "for",
        "from",
        "function",
        "if",
        "implements",
        "import",
        "in",
        "instanceof",
        "interface",
        "keyof",
        "let",
        "namespace",
        "never",
        "new",
        "null",
        "number",
        "of",
        "private",
        "protected",
        "public",
        "readonly",
        "return",
        "satisfies",
        "static",
        "string",
        "super",
        "switch",
        "this",
        "throw",
        "true",
        "try",
        "type",
        "typeof",
        "undefined",
        "unknown",
        "var",
        "void",
        "while",
        "yield",
    ],
    line_comment: Some("//"),
    block_comment: Some(("/*", "*/")),
    single_quotes: true,
    triple_quotes: false,
};

/// JSON: no comments, so `//` in one is not a comment.
const JSON: Rules = Rules {
    keywords: &["true", "false", "null"],
    line_comment: None,
    block_comment: None,
    single_quotes: false,
    triple_quotes: false,
};

/// TOML: hashed comments and strings that span lines.
const TOML: Rules = Rules {
    keywords: &["true", "false"],
    line_comment: Some("#"),
    block_comment: None,
    single_quotes: true,
    triple_quotes: true,
};

/// Shell: hashed comments, both quote styles, no construct that spans lines.
const BASH: Rules = Rules {
    keywords: &[
        "alias", "break", "case", "cd", "continue", "declare", "do", "done", "echo", "elif",
        "else", "esac", "eval", "exec", "exit", "export", "false", "fi", "for", "function", "if",
        "in", "let", "local", "printf", "read", "readonly", "return", "set", "shift", "source",
        "then", "trap", "true", "unset", "until", "while",
    ],
    line_comment: Some("#"),
    block_comment: None,
    single_quotes: true,
    triple_quotes: false,
};

/// YAML: hashed comments and single-quoted strings.
const YAML: Rules = Rules {
    keywords: &["true", "false", "null", "yes", "no", "on", "off"],
    line_comment: Some("#"),
    block_comment: None,
    single_quotes: true,
    triple_quotes: false,
};

#[cfg(test)]
mod tests {
    use super::*;

    /// The classes a whole fence is drawn with, line by line.
    fn classes(language: &str, source: &str) -> Vec<(Class, String)> {
        lines_of(language, source).into_iter().flatten().collect()
    }

    /// The classes each line is drawn with, kept apart so a multi-line construct can be
    /// asserted line by line.
    fn lines_of(language: &str, source: &str) -> Vec<Vec<(Class, String)>> {
        let mut highlighter = Highlighter::for_language(language);
        source
            .split('\n')
            .map(|line| highlighter.line(line))
            .collect()
    }

    /// The text of the runs, which must always be the source verbatim.
    fn text_of(language: &str, source: &str) -> String {
        classes(language, source)
            .into_iter()
            .map(|(_, text)| text)
            .collect()
    }

    #[test]
    fn a_rust_line_is_classified_by_kind() {
        let runs = classes("rust", "let x = 1; // twelve");
        assert_eq!(
            runs,
            vec![
                (Class::Keyword, String::from("let")),
                (Class::Plain, String::from(" x = ")),
                (Class::Number, String::from("1")),
                (Class::Plain, String::from("; ")),
                (Class::Comment, String::from("// twelve")),
            ]
        );
    }

    #[test]
    fn a_string_is_one_run_including_its_quotes() {
        let runs = classes("rust", r#"let s = "a \" b";"#);
        assert!(
            runs.iter()
                .any(|(class, text)| *class == Class::Literal && text == r#""a \" b""#),
            "the escaped quote does not end the string: {runs:?}"
        );
    }

    /// An unterminated literal is the newest line of a stream, not a mistake.
    #[test]
    fn a_string_that_never_closes_is_still_a_string() {
        let runs = classes("python", "x = \"half an answer");
        assert_eq!(
            runs.last(),
            Some(&(Class::Literal, String::from("\"half an answer")))
        );
    }

    #[test]
    fn a_block_comment_carries_across_lines_and_ends_where_it_ends() {
        let runs = classes("rust", "/* one\ntwo */ let x = 2;");
        assert_eq!(
            runs,
            vec![
                (Class::Comment, String::from("/* one")),
                (Class::Comment, String::from("two */")),
                (Class::Plain, String::from(" ")),
                (Class::Keyword, String::from("let")),
                (Class::Plain, String::from(" x = ")),
                (Class::Number, String::from("2")),
                (Class::Plain, String::from(";")),
            ]
        );
    }

    #[test]
    fn a_python_docstring_spans_lines() {
        let lines = lines_of("python", "def f():\n    \"\"\"one\n    two\"\"\"");
        assert_eq!(
            lines.first(),
            Some(&vec![
                (Class::Keyword, String::from("def")),
                (Class::Plain, String::from(" f():")),
            ])
        );
        assert_eq!(
            lines.get(1),
            Some(&vec![
                (Class::Plain, String::from("    ")),
                (Class::Literal, String::from("\"\"\"one"))
            ])
        );
        // The third line is inside the docstring from its first column, terminator included.
        assert_eq!(
            lines.get(2),
            Some(&vec![(Class::Literal, String::from("    two\"\"\""))])
        );
    }

    /// Rust has lifetimes, and this lexer does not have character literals: colour is not
    /// meaning, and a lifetime coloured as a string would be worse than one left plain.
    #[test]
    fn a_rust_lifetime_is_not_a_string() {
        let runs = classes("rust", "fn f<'a>(x: &'a str) {}");
        assert!(
            !runs.iter().any(|(class, _)| *class == Class::Literal),
            "a lifetime opens no literal: {runs:?}"
        );
    }

    #[test]
    fn json_has_no_comments() {
        let runs = classes("json", "{\"a\": 1} // not a comment");
        assert!(!runs.iter().any(|(class, _)| *class == Class::Comment));
        assert!(
            runs.iter()
                .any(|(class, text)| *class == Class::Number && text == "1")
        );
    }

    #[test]
    fn a_range_is_not_one_number() {
        let runs = classes("rust", "0..12");
        let numbers: Vec<&String> = runs
            .iter()
            .filter(|(class, _)| *class == Class::Number)
            .map(|(_, text)| text)
            .collect();
        assert_eq!(numbers, vec!["0", "12"]);
    }

    #[test]
    fn an_unknown_language_is_drawn_verbatim() {
        let source = "let x = 1; // not highlighted";
        assert_eq!(
            classes("diff", source),
            vec![(Class::Plain, String::from(source))]
        );
        assert_eq!(
            classes("", source),
            vec![(Class::Plain, String::from(source))]
        );
    }

    /// A fence may qualify its language: `rust,ignore` is Rust, and `Rust` is too.
    #[test]
    fn a_fence_name_is_read_as_a_fence_writes_it() {
        assert!(rules_for("rust").is_some());
        for name in ["rust,ignore", "Rust", " rust ", "ts", "sh"] {
            assert!(
                Highlighter::for_language(name).rules.is_some(),
                "{name} names a language to highlight"
            );
        }
        for name in ["diff", "console", "text", "brainfuck"] {
            assert!(
                Highlighter::for_language(name).rules.is_none(),
                "{name} is not a language this lexer knows"
            );
        }
    }

    /// The one invariant every class list has to keep: nothing is added, dropped, or
    /// reordered, whatever the language or the shape of the line.
    #[test]
    fn every_character_is_drawn_exactly_as_it_arrived() {
        let sources = [
            "let x = 1; // twelve",
            "  indented(\"a\", 'b');",
            "/* open",
            "still open */ after",
            "x = \"\"\" one",
            "two \"\"\"",
            "# comment with 'quotes'",
            "",
            "   ",
        ];
        for language in [
            "rust",
            "python",
            "javascript",
            "json",
            "toml",
            "bash",
            "yaml",
            "diff",
        ] {
            for source in sources {
                assert_eq!(text_of(language, source), source, "{language}: {source}");
            }
        }
    }
}
