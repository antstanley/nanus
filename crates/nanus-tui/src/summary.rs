//! The table a session is summarised by when the interface exits.
//!
//! The interface has just given the terminal back, so this is the last thing a reader sees
//! and the only durable reading of the session they are about to lose sight of: `/stats`
//! scrolls away with the transcript, and the next time the session is opened its totals will
//! include turns somebody else ran. So it reports everything the session knows rather than a
//! selection of it.
//!
//! ## Two tables, because the figures have two scopes
//!
//! The first is the *session's*: turns, steps, requests, tokens, and the configuration that
//! produced them, all read from the log. It covers the whole conversation, including the
//! turns that were run before this interface opened — a resumed session is not a new one, and
//! a summary that started counting at the attach would under-report it.
//!
//! The second is *this run's*: the rates and waits the interface measured as it watched. Only
//! a process that saw the responses arrive can know these, which is why they are not in the
//! log beside the totals — and why the section is absent entirely when this interface watched
//! nothing. A recording has no throughput, and a table of dashes would say it had some that
//! went unmeasured.
//!
//! ## Plain columns rather than a drawn box
//!
//! The width of the terminal was the interface's business and is not known here any more, and
//! a frame that wraps is less readable than no frame at all. Aligned columns carry the same
//! information and cannot break.
//!
//! An absent reading is a dash rather than a zero, for the reason the live statistics give:
//! zero is a measurement, and reporting zero to somebody who was told nothing is a different
//! claim from reporting that they were told nothing.

use nanus_domain::Session;

use crate::stats::{Throughput, percent, share, show, show_duration};

/// The width of the label column.
///
/// One wider than the longest label, so the widest row still has two spaces before its
/// reading rather than fusing into it. The same rule the live statistics row follows.
const LABEL: usize = 14;

/// Renders the summary the interface prints when it exits.
///
/// `label` is the name the session answers to, when it has one worth showing.
#[must_use]
pub fn session_summary(session: &Session, label: Option<&str>, stats: &Throughput) -> String {
    let mut lines = vec![String::new(), String::from("session summary")];
    lines.extend(session_rows(session, label));
    // Absent when this interface watched no request at all — a recording, or a session that
    // was opened and closed again. The scope is in the heading, because the counts above are
    // the session's and these are this process's.
    if stats.requests() > 0 {
        lines.push(String::new());
        lines.push(format!(
            "throughput (this run, {} requests)",
            stats.requests()
        ));
        lines.extend(throughput_rows(stats));
    }
    lines.push(String::new());
    lines.join("\n")
}

/// The session's own rows, read from the log.
///
/// Everything here survives the process that wrote it: a session is the only record of what
/// happened, and these are the figures it can answer for on its own.
fn session_rows(session: &Session, label: Option<&str>) -> Vec<String> {
    // The name, when it is a name. A live session that nobody named is labelled by its own
    // short id — that is what the title bar shows — and bracketing a prefix of the id on the
    // row that already carries the whole of it would be noise rather than a name.
    let name = label.filter(|name| !session.id().as_str().starts_with(name));
    let mut rows = vec![
        row(
            "session",
            &format!(
                "{}{}",
                session.id().as_str(),
                name.map_or_else(String::new, |name| format!("  [{name}]"))
            ),
        ),
        row(
            "started",
            &session
                .created_at_rfc3339()
                .unwrap_or_else(|| format!("{} (ms since the epoch)", session.created_at_ms())),
        ),
        row("workspace", session.cwd()),
    ];

    // The configuration, one field per row. A session from before this was recorded has
    // nothing here at all, and saying that once beats five rows of dashes that read as five
    // things which were measured and came back empty.
    match session.origin() {
        Some(origin) => {
            rows.push(row("model", &show_str(origin.model.as_deref())));
            rows.push(row("effort", &show_str(origin.effort.as_deref())));
            rows.push(row("sandbox", &show_str(origin.sandbox.as_deref())));
            rows.push(row("approval", &show_str(origin.approval.as_deref())));
            rows.push(row("harness", &show_str(origin.harness.as_deref())));
        }
        None => rows.push(row(
            "recorded",
            "nothing — this session predates the record",
        )),
    }

    let usage = session.usage_totals();
    rows.push(row("turns", &session.turn_count().to_string()));
    rows.push(row("steps", &session.step_count().to_string()));
    rows.push(row("requests", &session.request_count().to_string()));
    rows.push(row(
        "prompt",
        &format!(
            "{} tokens \u{b7} {} cached ({}) \u{b7} {} read",
            usage.prompt_tokens,
            usage.cache_hit_tokens,
            share(percent(
                u64::from(usage.cache_hit_tokens),
                u64::from(usage.prompt_tokens)
            )),
            usage.cache_miss_tokens
        ),
    ));
    rows.push(row(
        "generated",
        &format!(
            "{} tokens \u{b7} {} thinking ({})",
            usage.completion_tokens,
            usage.reasoning_tokens,
            share(percent(
                u64::from(usage.reasoning_tokens),
                u64::from(usage.completion_tokens)
            ))
        ),
    ));
    rows.push(row(
        "ended",
        session
            .log()
            .last_turn_end()
            .map_or_else(
                || "\u{2014} (still open)".to_owned(),
                |reason| reason.label().to_owned(),
            )
            .as_str(),
    ));

    // Only when a session used more than one model, which takes a resume against another:
    // one row per model for a session that had a single model would repeat the rows above it.
    let by_model = session.usage_by_model();
    if by_model.len() > 1 {
        for (model, totals) in &by_model {
            rows.push(row(
                "  per model",
                &format!(
                    "{} \u{b7} {} prompt ({} cached) + {} generated",
                    model.as_deref().unwrap_or("<not recorded>"),
                    totals.prompt_tokens,
                    totals.cache_hit_tokens,
                    totals.completion_tokens
                ),
            ));
        }
    }
    rows
}

/// This run's rows: the rates and waits only the interface could have measured.
fn throughput_rows(stats: &Throughput) -> Vec<String> {
    vec![
        // `generating` rather than `generated`: the row above names the tokens the model
        // produced and this one names the speed it produced them at. The live statistics draw
        // that distinction with those two words, and a table with two rows labelled
        // `generated` would be one a reader has to guess their way through.
        row(
            "generating",
            &pair(&rate(stats.last_rate()), &rate(stats.average_rate())),
        ),
        row(
            "whole request",
            &pair(
                &rate(stats.last_request_rate()),
                &rate(stats.average_request_rate()),
            ),
        ),
        row(
            "first token",
            &pair(
                &show_duration(stats.last_ttft_ms()),
                &show_duration(stats.average_ttft_ms()),
            ),
        ),
        row(
            "until head",
            &pair(
                &show_duration(stats.last_head_ms()),
                &show_duration(stats.average_head_ms()),
            ),
        ),
        // The server's own share of the wait: the two durations above are not the whole, and
        // a reader cannot subtract one from the other when each is reported as an average over
        // a different set of requests.
        row(
            "from head",
            &pair(
                &show_duration(stats.last_server_ms()),
                &show_duration(stats.average_server_ms()),
            ),
        ),
        // One figure rather than a last/average pair: it is a bound over the requests that
        // reported the whole wait's split, and there is no single request it is the "last" of.
        row(
            "prefill",
            &format!(
                "{} tok/s while the server worked",
                show(stats.encode_rate())
            ),
        ),
    ]
}

/// Renders one row of the table.
fn row(label: &str, reading: &str) -> String {
    format!("  {label:<LABEL$} {reading}")
}

/// Renders a last/average pair, which is how every rate and wait is reported.
fn pair(last: &str, average: &str) -> String {
    format!("last {last} \u{b7} average {average}")
}

/// Renders a rate, or a dash when it was not measured.
fn rate(value: Option<u64>) -> String {
    value.map_or_else(
        || String::from("\u{2014}"),
        |value| format!("{value} tok/s"),
    )
}

/// Renders a recorded string, or a dash when the field was not recorded.
fn show_str(value: Option<&str>) -> String {
    value.map_or_else(|| String::from("\u{2014}"), str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::Generation;
    use nanus_domain::{Origin, SessionEvent, SessionId, TurnEndReason, Usage};

    /// A session that ran one turn under a recorded configuration.
    fn session() -> Session {
        let mut session = Session::new(SessionId::new("s-1"), 1_700_000_000_000, "/work")
            .with_origin(Origin {
                model: Some("deepseek-flash".to_owned()),
                effort: Some("high".to_owned()),
                sandbox: Some("read_only".to_owned()),
                approval: Some("per_call".to_owned()),
                harness: Some("nanus/0.1.0".to_owned()),
            });
        session.append(SessionEvent::TurnStart { turn: 0 });
        session.append(SessionEvent::StepStart { turn: 0, step: 0 });
        session.append(SessionEvent::UserMessage {
            text: "read the file".to_owned(),
        });
        session.append(SessionEvent::AssistantMessage {
            text: Some(String::from("done")),
            reasoning: None,
            tool_calls: Vec::new(),
            usage: Some(Usage::new(1_000, 100, 40, 800, 200)),
            interrupted: false,
            model: Some("deepseek-flash".to_owned()),
            effort: Some("high".to_owned()),
        });
        session.append(SessionEvent::StepEnd { turn: 0, step: 0 });
        session.append(SessionEvent::TurnEnd {
            turn: 0,
            reason: TurnEndReason::Completed,
        });
        session
    }

    /// Throughput as one timed request leaves it: 900 tokens generated over 20s inside a 25s
    /// request, after a 300ms wait for the head and a 900ms wait for a first token.
    fn stats() -> Throughput {
        let mut stats = Throughput::default();
        stats.record(Generation {
            completion_tokens: 900,
            reasoning_tokens: 300,
            cache_hit_tokens: 800,
            cache_miss_tokens: 200,
            head_ms: 300,
            ttft_ms: 900,
            decode_ms: 20_000,
            duration_ms: 25_000,
        });
        stats
    }

    /// The reading on the row labelled `label`.
    ///
    /// Read as a table rather than as a string: what these tests are about is which figure is
    /// on which row, and asserting on the padding between them would make every one of them
    /// fail the day the column is resized.
    fn reading<'a>(text: &'a str, label: &str) -> &'a str {
        let prefix = format!("  {label:<LABEL$} ");
        text.lines()
            .find_map(|line| line.strip_prefix(&prefix))
            .unwrap_or_else(|| panic!("no row labelled {label:?} in:\n{text}"))
    }

    /// Every reading on a row labelled `label`, in the order the rows appear.
    fn rows<'a>(text: &'a str, label: &str) -> Vec<&'a str> {
        let prefix = format!("  {label:<LABEL$} ");
        text.lines()
            .filter_map(|line| line.strip_prefix(&prefix))
            .collect()
    }

    /// Whether a row with this label is in the table at all.
    fn has_row(text: &str, label: &str) -> bool {
        let prefix = format!("  {label:<LABEL$} ");
        text.lines().any(|line| line.starts_with(&prefix))
    }

    #[test]
    fn the_summary_names_the_configuration_and_the_totals() {
        let text = session_summary(&session(), Some("nightly"), &stats());
        assert!(reading(&text, "session").ends_with("[nightly]"), "{text}");
        assert_eq!(reading(&text, "workspace"), "/work");
        assert_eq!(reading(&text, "model"), "deepseek-flash");
        assert_eq!(reading(&text, "effort"), "high");
        assert_eq!(reading(&text, "sandbox"), "read_only");
        assert_eq!(reading(&text, "approval"), "per_call");
        assert_eq!(reading(&text, "harness"), "nanus/0.1.0");
        assert_eq!(
            reading(&text, "prompt"),
            "1000 tokens \u{b7} 800 cached (80%) \u{b7} 200 read"
        );
        assert_eq!(
            reading(&text, "generated"),
            "100 tokens \u{b7} 40 thinking (40%)"
        );
        assert_eq!(reading(&text, "ended"), "completed");
    }

    #[test]
    fn the_counts_are_the_sessions_and_not_only_this_runs() {
        // The reason they come from the log rather than being counted here: a resumed
        // session's totals include the turns that ran before this interface existed.
        let text = session_summary(&session(), None, &stats());
        assert_eq!(reading(&text, "turns"), "1");
        assert_eq!(reading(&text, "steps"), "1");
        assert_eq!(reading(&text, "requests"), "1");
    }

    #[test]
    fn the_throughput_table_reports_last_and_average_apart() {
        let text = session_summary(&session(), None, &stats());
        assert!(text.contains("throughput (this run, 1 requests)"), "{text}");
        // 900 tokens over the 20s generation window and over the whole 25s request: the two
        // rates differ by exactly the time the model spent not generating, which is why the
        // interface reports both rather than one.
        assert_eq!(
            reading(&text, "generating"),
            "last 45 tok/s \u{b7} average 45 tok/s"
        );
        assert_eq!(
            reading(&text, "whole request"),
            "last 36 tok/s \u{b7} average 36 tok/s"
        );
        assert_eq!(
            reading(&text, "first token"),
            "last 900ms \u{b7} average 900ms"
        );
        assert_eq!(
            reading(&text, "until head"),
            "last 300ms \u{b7} average 300ms"
        );
        // The server's own share of the wait: 900ms to the first token less the 300ms to the
        // head. Reported for the same reason the two rates are — the halves are not the whole.
        assert_eq!(
            reading(&text, "from head"),
            "last 600ms \u{b7} average 600ms"
        );
        // The prefill bound: the 1000 prompt tokens over the 600ms the server itself was
        // working, which is the whole wait less the part of it that cannot contain prefill.
        assert_eq!(
            reading(&text, "prefill"),
            "1666 tok/s while the server worked"
        );
    }

    #[test]
    fn a_run_that_watched_nothing_gets_no_throughput_table() {
        // A recording, or a session opened and closed again. A table of dashes would say the
        // interface watched something and failed to measure it.
        let text = session_summary(&session(), None, &Throughput::default());
        assert!(!text.contains("throughput"), "{text}");
        // The session's own rows are still there: they are read from the log, which does not
        // need anybody to have been watching.
        assert_eq!(reading(&text, "turns"), "1");
    }

    #[test]
    fn a_session_that_recorded_no_configuration_says_so_once() {
        let bare = Session::new(SessionId::new("s-2"), 1_700_000_000_000, "/work");
        let text = session_summary(&bare, None, &Throughput::default());
        assert!(
            text.contains("predates the record"),
            "the gap is named:\n{text}"
        );
        for absent in ["model", "effort", "sandbox", "approval", "harness"] {
            assert!(
                !has_row(&text, absent),
                "five dashed rows would read as five measurements: {text}"
            );
        }
    }

    #[test]
    fn a_partly_recorded_configuration_shows_a_dash_for_what_is_missing() {
        // One field recorded and the rest not, which is what a session written by a build
        // that knew about models and not about efforts would look like.
        let partial =
            Session::new(SessionId::new("s-3"), 1_700_000_000_000, "/work").with_origin(Origin {
                model: Some("deepseek-flash".to_owned()),
                ..Origin::default()
            });
        let text = session_summary(&partial, None, &Throughput::default());
        assert_eq!(reading(&text, "model"), "deepseek-flash");
        assert_eq!(reading(&text, "effort"), "\u{2014}");
        assert_eq!(reading(&text, "sandbox"), "\u{2014}");
    }

    #[test]
    fn a_label_that_is_only_a_shorter_id_is_not_repeated() {
        // An unnamed live session is labelled by its own first eight characters. Printing
        // that beside the full id would put the same fact on the row twice.
        let text = session_summary(&session(), Some("s-1"), &Throughput::default());
        assert!(!text.contains("[s-1]"), "{text}");
        // A real name still shows, because it is the thing the row cannot otherwise say.
        let named = session_summary(&session(), Some("nightly"), &Throughput::default());
        assert!(named.contains("[nightly]"), "{named}");
    }

    #[test]
    fn a_session_with_one_model_does_not_repeat_itself_per_model() {
        let text = session_summary(&session(), None, &Throughput::default());
        assert!(!text.contains("per model"), "{text}");
    }

    #[test]
    fn a_session_that_used_two_models_shows_both() {
        // What a resume against another model leaves behind, and the reason the per-model
        // rows exist: the rows above are the session's total and cannot say which model
        // spent it.
        let mut session = session();
        session.append(SessionEvent::AssistantMessage {
            text: Some(String::from("done again")),
            reasoning: None,
            tool_calls: Vec::new(),
            usage: Some(Usage::new(500, 50, 0, 100, 400)),
            interrupted: false,
            model: Some("deepseek-v4-pro".to_owned()),
            effort: Some("medium".to_owned()),
        });
        let text = session_summary(&session, None, &Throughput::default());
        let per_model = rows(&text, "  per model");
        assert_eq!(per_model.len(), 2, "one row per model:\n{text}");
        assert!(per_model[0].starts_with("deepseek-flash"), "{text}");
        assert!(per_model[1].starts_with("deepseek-v4-pro"), "{text}");
    }

    #[test]
    fn a_turn_still_running_is_not_reported_as_ended() {
        let mut open = Session::new(SessionId::new("s-4"), 1_700_000_000_000, "/work");
        open.append(SessionEvent::TurnStart { turn: 0 });
        let text = session_summary(&open, None, &Throughput::default());
        assert_eq!(reading(&text, "ended"), "\u{2014} (still open)");
    }

    #[test]
    fn the_table_leaves_the_shell_a_clear_line_either_side() {
        // The prompt starts on its own line, and the interface's last drawn frame is not
        // fused to the first row of the table.
        let text = session_summary(&session(), None, &stats());
        assert!(text.starts_with('\n'), "{text:?}");
        assert!(text.ends_with('\n'), "{text:?}");
    }
}
