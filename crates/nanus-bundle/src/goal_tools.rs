//! The goal tools: the model's way to read and move a session's durable objective.
//!
//! A goal is the instruction "keep going" written down once — see
//! [`nanus_domain::goal`]. `get_goal`, `create_goal`, `update_goal`, `pause_goal`, and
//! `abandon_goal` are how the model reads it, sets it, changes it, suspends it, and
//! gives up on it.
//!
//! ## Why the loop runs these rather than the registry
//!
//! A goal change is a `goal/change` record appended to the session log, and while a turn
//! runs the log is held as `&mut Session` by the loop. A registered tool's executor is
//! `'static` and owns everything it closes over, so it cannot borrow the session; a
//! session-scoped handle kept beside the loop would be shared by every session the agent
//! holds, which two concurrent turns would race. So the loop — the one place that owns
//! the session — executes these itself, and this module is the part that can be written
//! and tested without a session at all: it is a pure function from the current goal and
//! one call to the goal to record and the text the model reads.
//!
//! ## The tool list is one list
//!
//! These tools are offered beside the registered toolset rather than inside it, so the
//! two are combined in exactly one place — [`crate::AgentRunner`], which is what sends
//! the schemas and advertises the count. [`schemas`] and [`COUNT`] are the same five
//! tools, asserted below.
//!
//! ## What the model may and may not decide
//!
//! The model may create a goal, change its objective, and — with evidence — complete it,
//! because those are judgements about the work. It may pause a goal that cannot proceed
//! now and abandon one it has concluded cannot be achieved, both of which require saying
//! why. It may not *clear* a goal: removing the objective is the person's decision, and
//! the interface's `/goal clear` is where that lives.

use nanus_domain::{Goal, ToolCall, ToolName, ToolOutcome, ToolSchema};
use serde_json::{Value, json};

use crate::args::Arguments;

/// The tool that reads the session's goal.
pub const GET_GOAL: &str = "get_goal";
/// The tool that creates a goal.
pub const CREATE_GOAL: &str = "create_goal";
/// The tool that changes a goal's objective or marks it complete.
pub const UPDATE_GOAL: &str = "update_goal";
/// The tool that suspends a goal.
pub const PAUSE_GOAL: &str = "pause_goal";
/// The tool that gives up on a goal.
pub const ABANDON_GOAL: &str = "abandon_goal";

/// Every goal tool name, in the order they are offered.
pub const NAMES: [&str; 5] = [GET_GOAL, CREATE_GOAL, UPDATE_GOAL, PAUSE_GOAL, ABANDON_GOAL];

/// How many goal tools there are.
pub const COUNT: usize = NAMES.len();

/// The argument name carrying an objective.
const OBJECTIVE: &str = "objective";
/// The argument name carrying a status.
const STATUS: &str = "status";
/// The argument name carrying the evidence a completion rests on.
const EVIDENCE: &str = "evidence";
/// The argument name carrying why a goal was paused or abandoned.
const REASON: &str = "reason";

/// Returns `true` when `name` is one of the goal tools.
///
/// The loop asks this before dispatch: a goal tool is the loop's own and is never handed to
/// the registry, which does not hold it.
#[must_use]
pub fn is_goal_tool(name: &ToolName) -> bool {
    NAMES.contains(&name.as_str())
}

/// Returns the goal tools' wire schemas.
#[must_use]
pub fn schemas() -> Vec<ToolSchema> {
    vec![
        schema(
            GET_GOAL,
            "Read the objective this session is working toward: its text, whether it is \
             active, paused, complete, or abandoned, and any note on it. There is at most \
             one goal per session. Call this before assuming there is one.",
            parameters(json!({}), &[]),
        ),
        schema(
            CREATE_GOAL,
            "Create a durable objective for this session, for work that will not fit in one \
             turn. The objective is the outcome to reach, not a plan or a step. Create one \
             only when the user asks for an ongoing objective; there is at most one goal per \
             session.",
            parameters(
                json!({
                    OBJECTIVE: {
                        "type": "string",
                        "description": "The outcome to reach, as one sentence.",
                    },
                }),
                &[OBJECTIVE],
            ),
        ),
        schema(
            UPDATE_GOAL,
            "Change the session's existing goal: give it a new objective, resume it with \
             status \"active\", or mark it complete with status \"complete\" — which requires \
             `evidence` describing what was checked to show the objective is met. At least \
             one of `objective` and `status` is needed.",
            parameters(
                json!({
                    OBJECTIVE: {
                        "type": "string",
                        "description": "A new objective, replacing the current one.",
                    },
                    STATUS: {
                        "type": "string",
                        "enum": ["active", "complete"],
                        "description": "Resume it, or mark it complete.",
                    },
                    EVIDENCE: {
                        "type": "string",
                        "description": "What was checked to show the objective is met. \
                                        Required when status is \"complete\".",
                    },
                }),
                &[],
            ),
        ),
        schema(
            PAUSE_GOAL,
            "Suspend the session's goal so it is not worked on, recording why when the \
             reason is known. A paused goal is resumed with update_goal's status \"active\". \
             Pause because progress is impossible for now, not because the turn is over.",
            parameters(
                json!({
                    REASON: {
                        "type": "string",
                        "description": "Why the goal cannot be worked on now.",
                    },
                }),
                &[],
            ),
        ),
        schema(
            ABANDON_GOAL,
            "Give up on the session's goal because it cannot be achieved, recording why. \
             This is not completion: the objective was not met, and the reason is required so \
             the decision is auditable. It is terminal, so reach for it only when the \
             objective is genuinely out of reach.",
            parameters(
                json!({
                    REASON: {
                        "type": "string",
                        "description": "Why the objective cannot be achieved.",
                    },
                }),
                &[REASON],
            ),
        ),
    ]
}

/// What one goal tool call leaves behind.
pub struct Applied {
    /// The goal to record, when the call changed it.
    ///
    /// `None` for a read, and nothing is appended: a goal change is a durable record, and a
    /// tool call that changed nothing must not add one.
    pub record: Option<Goal>,
    /// What the model reads as the call's result.
    pub outcome: ToolOutcome,
}

/// Runs one goal tool call against the session's current goal.
///
/// Pure: it reads `current` and the call and returns what to record and what to say. The
/// caller owns the log and appends the record, so this is testable without a session.
///
/// A call whose arguments are not a JSON object is reported as a failure the model can
/// correct, the same way the registry reports one, rather than panicking.
#[must_use]
pub fn run(current: Option<&Goal>, call: &ToolCall, now_ms: u64) -> Applied {
    // The loop intercepts these before the registry, so the argument-shape check the registry
    // would have done is done here instead.
    if let Err(error) = call.arguments_object() {
        return Applied::failed(error.to_string());
    }
    let args = Arguments::new(call.name.as_str(), &call.arguments);
    match call.name.as_str() {
        GET_GOAL => get(current),
        CREATE_GOAL => create(current, &args, now_ms),
        UPDATE_GOAL => update(current, &args, now_ms),
        PAUSE_GOAL => pause(current, &args, now_ms),
        ABANDON_GOAL => abandon(current, &args, now_ms),
        other => Applied::failed(format!("{other} is not a goal tool")),
    }
}

impl Applied {
    /// A read: nothing to record, and the goal as the model should see it.
    fn read(current: Option<&Goal>) -> Self {
        let text = current.map_or_else(
            || String::from("(no goal is set)"),
            |goal| format!("the goal is {}", describe(goal)),
        );
        Self {
            record: None,
            outcome: success(current, &text),
        }
    }

    /// A change: the goal to record, and its rendering for the model.
    fn changed(goal: Goal) -> Self {
        let text = format!("the goal is now {}", describe(&goal));
        let outcome = success(Some(&goal), &text);
        Self {
            record: Some(goal),
            outcome,
        }
    }

    /// A transition that left the goal as it was: nothing to record, and a result that says so.
    fn unchanged(goal: &Goal) -> Self {
        let text = format!("the goal is unchanged; it is already {}", describe(goal));
        Self {
            record: None,
            outcome: success(Some(goal), &text),
        }
    }

    /// A failure the model can act on.
    fn failed(message: impl Into<String>) -> Self {
        Self {
            record: None,
            outcome: ToolOutcome::failure(message),
        }
    }

    /// Lifts an argument failure into an outcome.
    fn from_outcome(outcome: ToolOutcome) -> Self {
        Self {
            record: None,
            outcome,
        }
    }
}

/// Reads the goal, or reports that there is none.
fn get(current: Option<&Goal>) -> Applied {
    Applied::read(current)
}

/// Creates a goal, or a fresh objective when the last one is terminal.
fn create(current: Option<&Goal>, args: &Arguments<'_>, now_ms: u64) -> Applied {
    let objective = match args.required_str(OBJECTIVE) {
        Ok(objective) => objective,
        Err(outcome) => return Applied::from_outcome(outcome),
    };
    match current {
        // An open goal is not replaced by "create": the model must say what it is doing to
        // the goal it already has, and the refusal names the three ways to do it.
        Some(existing) if !existing.phase().is_terminal() => Applied::failed(format!(
            "a goal already exists and is {}: {}. Use update_goal to change its objective, \
             pause_goal to suspend it, or abandon_goal to give up on it.",
            existing.phase(),
            existing.objective()
        )),
        Some(existing) => transitioned(current, existing.with_objective(objective, now_ms)),
        None => transitioned(current, Goal::new(objective, now_ms)),
    }
}

/// Changes an open goal: a new objective, a resume, or a completion.
fn update(current: Option<&Goal>, args: &Arguments<'_>, now_ms: u64) -> Applied {
    let Some(existing) = current else {
        return Applied::failed("there is no goal to update; create one with create_goal");
    };
    if existing.phase().is_terminal() {
        return Applied::failed(format!(
            "the goal is {} and cannot be updated; create_goal starts a new objective",
            existing.phase()
        ));
    }
    let objective = match args.optional_str(OBJECTIVE) {
        Ok(value) => value,
        Err(outcome) => return Applied::from_outcome(outcome),
    };
    let status = match args.optional_str(STATUS) {
        Ok(value) => value,
        Err(outcome) => return Applied::from_outcome(outcome),
    };
    let evidence = match args.optional_str(EVIDENCE) {
        Ok(value) => value,
        Err(outcome) => return Applied::from_outcome(outcome),
    };
    match status.as_deref() {
        Some("complete") => {
            if objective.is_some() {
                return Applied::failed(
                    "completing a goal does not take an objective; update the objective first",
                );
            }
            if evidence.as_ref().is_none_or(|text| text.trim().is_empty()) {
                return Applied::failed(
                    "completing a goal needs `evidence`: what was checked to show the \
                     objective is met. Verify it against tests, logs, or the files themselves \
                     before calling it done.",
                );
            }
            transitioned(current, existing.completed(evidence, now_ms))
        }
        Some("active") => objective.map_or_else(
            || transitioned(current, existing.resumed(now_ms)),
            |objective| transitioned(current, existing.with_objective(objective, now_ms)),
        ),
        Some(other) => Applied::failed(format!(
            "update_goal's status is \"active\" or \"complete\", not {other:?}"
        )),
        None => objective.map_or_else(
            || {
                Applied::failed(
                    "update_goal needs an `objective`, or a `status` of \"active\" or \
                     \"complete\"",
                )
            },
            |objective| transitioned(current, existing.with_objective(objective, now_ms)),
        ),
    }
}

/// Suspends an open goal, recording why when a reason was given.
fn pause(current: Option<&Goal>, args: &Arguments<'_>, now_ms: u64) -> Applied {
    let Some(existing) = current else {
        return Applied::failed("there is no goal to pause");
    };
    if existing.phase().is_terminal() {
        return Applied::failed(format!(
            "the goal is {}; there is nothing to pause",
            existing.phase()
        ));
    }
    match args.optional_str(REASON) {
        Ok(reason) => transitioned(current, existing.paused(reason, now_ms)),
        Err(outcome) => Applied::from_outcome(outcome),
    }
}

/// Gives up on an open goal, recording why.
fn abandon(current: Option<&Goal>, args: &Arguments<'_>, now_ms: u64) -> Applied {
    let Some(existing) = current else {
        return Applied::failed("there is no goal to abandon");
    };
    if existing.phase().is_terminal() {
        return Applied::failed(format!(
            "the goal is {}; there is nothing to abandon",
            existing.phase()
        ));
    }
    let reason = match args.required_str(REASON) {
        Ok(reason) => reason,
        Err(outcome) => return Applied::from_outcome(outcome),
    };
    if reason.trim().is_empty() {
        return Applied::failed(
            "abandoning a goal needs a `reason`: why the objective cannot be achieved. If it \
             can be achieved later, pause_goal instead.",
        );
    }
    transitioned(current, existing.abandoned(Some(reason), now_ms))
}

/// Lifts a transition's result into what the model reads.
///
/// A transition the domain answered with the goal it was given — pausing a goal already
/// paused, resuming one already active — changed nothing, so it is answered as a read and
/// leaves no record: a `goal/change` that is not a change would replay as a second notice of
/// the same goal, and would tell the model a new reason was kept when the old one was.
fn transitioned(
    current: Option<&Goal>,
    outcome: Result<Goal, nanus_domain::DomainError>,
) -> Applied {
    match outcome {
        Ok(goal) if current == Some(&goal) => Applied::unchanged(&goal),
        Ok(goal) => Applied::changed(goal),
        Err(error) => Applied::failed(error.to_string()),
    }
}

/// Renders a goal as one line for the model, with its note beneath when there is one.
fn describe(goal: &Goal) -> String {
    let mut text = format!(
        "{} (revision {}): {}",
        goal.phase(),
        goal.revision(),
        goal.objective()
    );
    if let Some(note) = goal.note() {
        text.push_str("\nnote: ");
        text.push_str(note);
    }
    text
}

/// Builds a successful outcome carrying the goal as both value and content.
fn success(goal: Option<&Goal>, text: &str) -> ToolOutcome {
    let value = value(goal);
    ToolOutcome::success_with(
        value,
        vec![nanus_domain::ContentBlock::Text(text.to_owned())],
    )
}

/// Builds the machine-readable half: the goal, or null when there is none.
fn value(goal: Option<&Goal>) -> Value {
    let encoded = goal.map_or(Value::Null, |goal| {
        serde_json::to_value(goal).unwrap_or(Value::Null)
    });
    json!({ "goal": encoded })
}

/// Builds a schema, with the panic reserved for a literal name that cannot be invalid.
fn schema(name: &str, description: &str, parameters: Value) -> ToolSchema {
    ToolSchema {
        name: ToolName::new(name).unwrap_or_else(|_| unreachable!("a shipped tool name is valid")),
        description: description.to_owned(),
        parameters,
    }
}

/// Builds a JSON-schema object, forbidding arguments the tool does not declare.
fn parameters(properties: Value, required: &[&str]) -> Value {
    let mut object = serde_json::Map::new();
    object.insert(String::from("type"), Value::String(String::from("object")));
    object.insert(String::from("properties"), properties);
    if !required.is_empty() {
        object.insert(
            String::from("required"),
            Value::Array(
                required
                    .iter()
                    .map(|name| Value::String((*name).to_owned()))
                    .collect(),
            ),
        );
    }
    object.insert(String::from("additionalProperties"), Value::Bool(false));
    Value::Object(object)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nanus_domain::GoalPhase;

    fn name(raw: &str) -> ToolName {
        ToolName::new(raw).unwrap_or_else(|error| panic!("test tool name {raw}: {error}"))
    }

    fn call(tool: &str, arguments: Value) -> ToolCall {
        ToolCall::new(
            nanus_domain::ToolCallId::new("call-1"),
            name(tool),
            arguments,
        )
    }

    fn active() -> Goal {
        Goal::new("reduce p95 latency", 1_000).unwrap_or_else(|error| panic!("{error}"))
    }

    /// The text a successful or failed outcome carries.
    fn text(applied: &Applied) -> String {
        applied.outcome.render_text()
    }

    #[test]
    fn the_five_tools_are_the_five_names() {
        assert_eq!(schemas().len(), COUNT);
        assert_eq!(COUNT, 5);
        for (schema, expected) in schemas().iter().zip(NAMES) {
            assert_eq!(schema.name.as_str(), expected);
            assert!(
                schema.description.len() > 20,
                "{} has a usable description",
                schema.name
            );
            assert_eq!(
                schema
                    .parameters
                    .get("additionalProperties")
                    .and_then(Value::as_bool),
                Some(false),
                "{} forbids undeclared arguments",
                schema.name
            );
        }
        for raw in NAMES {
            assert!(is_goal_tool(&name(raw)), "{raw} is a goal tool");
        }
        assert!(!is_goal_tool(&name("read")));
    }

    #[test]
    fn get_goal_reports_the_goal_and_records_nothing() {
        let applied = run(Some(&active()), &call(GET_GOAL, json!({})), 2_000);
        assert!(applied.record.is_none(), "a read changes nothing");
        assert!(applied.outcome.is_success());
        assert!(text(&applied).contains("reduce p95 latency"));
        assert!(text(&applied).contains("active"));
        assert_eq!(
            applied.outcome.value().and_then(|value| value.get("goal")),
            serde_json::to_value(active()).ok().as_ref(),
            "the goal is carried as a value as well as rendered"
        );
    }

    #[test]
    fn get_goal_with_no_goal_says_so_and_is_not_a_failure() {
        let applied = run(None, &call(GET_GOAL, json!({})), 2_000);
        assert!(applied.outcome.is_success(), "asking is not an error");
        assert_eq!(text(&applied), "(no goal is set)");
        assert_eq!(
            applied.outcome.value().and_then(|value| value.get("goal")),
            Some(&Value::Null)
        );
    }

    #[test]
    fn create_goal_sets_an_active_objective() {
        let applied = run(
            None,
            &call(CREATE_GOAL, json!({ "objective": "ship the notes" })),
            5_000,
        );
        let recorded = applied.record.as_ref().expect("a create records a goal");
        assert_eq!(recorded.objective(), "ship the notes");
        assert_eq!(recorded.phase(), GoalPhase::Active);
        assert!(text(&applied).contains("ship the notes"));
    }

    #[test]
    fn create_goal_refuses_while_an_open_goal_exists_and_names_the_alternatives() {
        for existing in [
            active(),
            active()
                .paused(None, 2)
                .unwrap_or_else(|error| panic!("{error}")),
        ] {
            let applied = run(
                Some(&existing),
                &call(CREATE_GOAL, json!({ "objective": "another" })),
                5_000,
            );
            assert!(applied.record.is_none(), "nothing is recorded");
            assert!(!applied.outcome.is_success());
            let message = text(&applied);
            assert!(message.contains("update_goal"), "{message}");
            assert!(message.contains("pause_goal"), "{message}");
            assert!(message.contains("abandon_goal"), "{message}");
        }
    }

    #[test]
    fn create_goal_replaces_a_terminal_goal() {
        let done = active()
            .completed(Some("shipped".to_owned()), 2)
            .unwrap_or_else(|error| panic!("{error}"));
        let applied = run(
            Some(&done),
            &call(CREATE_GOAL, json!({ "objective": "the next objective" })),
            9_000,
        );
        let recorded = applied
            .record
            .as_ref()
            .expect("a terminal goal is replaced");
        assert_eq!(recorded.objective(), "the next objective");
        assert_eq!(recorded.phase(), GoalPhase::Active);
        assert_eq!(
            recorded.created_at_ms(),
            1_000,
            "the goal's own creation time survives"
        );
    }

    #[test]
    fn create_goal_needs_an_objective() {
        let applied = run(None, &call(CREATE_GOAL, json!({})), 5_000);
        assert!(applied.record.is_none());
        assert!(!applied.outcome.is_success());
        assert!(text(&applied).contains(OBJECTIVE), "{}", text(&applied));
    }

    #[test]
    fn update_goal_changes_the_objective() {
        let applied = run(
            Some(&active()),
            &call(UPDATE_GOAL, json!({ "objective": "reduce p99 latency" })),
            6_000,
        );
        let recorded = applied.record.as_ref().expect("an update records a goal");
        assert_eq!(recorded.objective(), "reduce p99 latency");
        assert_eq!(recorded.phase(), GoalPhase::Active);
    }

    #[test]
    fn update_goal_resumes_a_paused_goal() {
        let paused = active()
            .paused(Some("waiting for the release window".to_owned()), 2_000)
            .unwrap_or_else(|error| panic!("{error}"));
        let applied = run(
            Some(&paused),
            &call(UPDATE_GOAL, json!({ "status": "active" })),
            7_000,
        );
        let recorded = applied.record.as_ref().expect("a resume records a goal");
        assert_eq!(recorded.phase(), GoalPhase::Active);
        assert_eq!(
            recorded.note(),
            None,
            "resuming clears the reason it stopped"
        );
    }

    #[test]
    fn update_goal_completes_only_with_evidence() {
        // The evidence requirement, stated as a refusal and then as the accepting case.
        let refused = run(
            Some(&active()),
            &call(UPDATE_GOAL, json!({ "status": "complete" })),
            8_000,
        );
        assert!(refused.record.is_none(), "a refusal records nothing");
        assert!(!refused.outcome.is_success());
        assert!(text(&refused).contains(EVIDENCE), "{}", text(&refused));

        let accepted = run(
            Some(&active()),
            &call(
                UPDATE_GOAL,
                json!({ "status": "complete", "evidence": "p95 is 118 ms in the benchmark log" }),
            ),
            8_000,
        );
        let recorded = accepted
            .record
            .as_ref()
            .expect("a completion records a goal");
        assert_eq!(recorded.phase(), GoalPhase::Complete);
        assert_eq!(
            recorded.note(),
            Some("p95 is 118 ms in the benchmark log"),
            "the evidence is kept as the goal's note"
        );
    }

    #[test]
    fn update_goal_refuses_what_it_cannot_do() {
        // No goal at all.
        assert!(
            !run(None, &call(UPDATE_GOAL, json!({ "objective": "x" })), 0)
                .outcome
                .is_success()
        );
        // Neither an objective nor a status.
        let empty = run(Some(&active()), &call(UPDATE_GOAL, json!({})), 0);
        assert!(empty.record.is_none());
        assert!(text(&empty).contains(OBJECTIVE));
        // A status that is not on the scale.
        let bad = run(
            Some(&active()),
            &call(UPDATE_GOAL, json!({ "status": "paused" })),
            0,
        );
        assert!(bad.record.is_none());
        assert!(text(&bad).contains("active"), "{}", text(&bad));
        // Completing while also replacing the objective.
        let two = run(
            Some(&active()),
            &call(
                UPDATE_GOAL,
                json!({ "status": "complete", "objective": "x", "evidence": "y" }),
            ),
            0,
        );
        assert!(two.record.is_none());
        // A terminal goal.
        let done = active()
            .completed(Some("e".to_owned()), 1)
            .unwrap_or_else(|error| panic!("{error}"));
        let terminal = run(
            Some(&done),
            &call(UPDATE_GOAL, json!({ "objective": "x" })),
            0,
        );
        assert!(terminal.record.is_none());
        assert!(text(&terminal).contains(CREATE_GOAL), "{}", text(&terminal));
    }

    #[test]
    fn pause_goal_suspends_and_keeps_the_reason() {
        let applied = run(
            Some(&active()),
            &call(PAUSE_GOAL, json!({ "reason": "the fixture is broken" })),
            4_000,
        );
        let recorded = applied.record.as_ref().expect("a pause records a goal");
        assert_eq!(recorded.phase(), GoalPhase::Paused);
        assert_eq!(recorded.note(), Some("the fixture is broken"));
    }

    /// A transition to the phase the goal is already in changes nothing, so it records nothing:
    /// a second `pause_goal`, or `update_goal` asking an active goal to be active, would
    /// otherwise append a `goal/change` that replays as a second notice of the same goal — and
    /// the result says the goal is unchanged rather than claiming a new reason was kept.
    #[test]
    fn a_transition_to_the_phase_the_goal_is_in_records_nothing() {
        let paused = active()
            .paused(Some(String::from("waiting for CI")), 2_000)
            .unwrap_or_else(|error| panic!("{error}"));
        let again = run(
            Some(&paused),
            &call(PAUSE_GOAL, json!({ "reason": "a different reason" })),
            3_000,
        );
        assert!(
            again.record.is_none(),
            "pausing a paused goal is not a change"
        );
        assert!(again.outcome.is_success(), "and it is not a failure either");
        let message = text(&again);
        assert!(message.contains("unchanged"), "{message}");
        assert!(
            message.contains("waiting for CI"),
            "the kept note is the one shown: {message}"
        );
        assert!(!message.contains("a different reason"), "{message}");

        let resumed = run(
            Some(&active()),
            &call(UPDATE_GOAL, json!({ "status": "active" })),
            3_000,
        );
        assert!(
            resumed.record.is_none(),
            "resuming an active goal is not a change"
        );
        assert!(text(&resumed).contains("unchanged"));

        // The other direction: a transition that does move the goal still records it.
        let moved = run(Some(&active()), &call(PAUSE_GOAL, json!({})), 3_000);
        let recorded = moved
            .record
            .as_ref()
            .expect("pausing an active goal records");
        assert_eq!(recorded.phase(), GoalPhase::Paused);
        assert!(text(&moved).contains("now"));
    }

    #[test]
    fn abandon_goal_is_terminal_and_needs_a_reason() {
        // Missing, then blank: both are refusals, and the blank one points at the cheaper move.
        let missing = run(Some(&active()), &call(ABANDON_GOAL, json!({})), 4_000);
        assert!(missing.record.is_none());
        assert!(!missing.outcome.is_success());
        assert!(text(&missing).contains(REASON), "{}", text(&missing));

        let blank = run(
            Some(&active()),
            &call(ABANDON_GOAL, json!({ "reason": "   " })),
            4_000,
        );
        assert!(blank.record.is_none());
        assert!(text(&blank).contains("pause_goal"), "{}", text(&blank));

        let applied = run(
            Some(&active()),
            &call(
                ABANDON_GOAL,
                json!({ "reason": "the API cannot express it" }),
            ),
            4_000,
        );
        let recorded = applied.record.as_ref().expect("an abandon records a goal");
        assert_eq!(recorded.phase(), GoalPhase::Abandoned);
        assert_eq!(recorded.note(), Some("the API cannot express it"));
        assert!(recorded.phase().is_terminal());
    }

    #[test]
    fn pause_and_abandon_refuse_a_goal_that_is_already_answered() {
        let done = active()
            .completed(Some("e".to_owned()), 1)
            .unwrap_or_else(|error| panic!("{error}"));
        for tool in [PAUSE_GOAL, ABANDON_GOAL] {
            let applied = run(Some(&done), &call(tool, json!({ "reason": "r" })), 0);
            assert!(applied.record.is_none(), "{tool}");
            assert!(!applied.outcome.is_success(), "{tool}");
        }
        for tool in [PAUSE_GOAL, ABANDON_GOAL] {
            let applied = run(None, &call(tool, json!({ "reason": "r" })), 0);
            assert!(applied.record.is_none(), "{tool} with no goal");
        }
    }

    #[test]
    fn a_wrong_argument_type_is_a_message_rather_than_a_panic() {
        let applied = run(None, &call(CREATE_GOAL, json!({ "objective": 7 })), 0);
        assert!(applied.record.is_none());
        assert!(!applied.outcome.is_success());
        assert!(text(&applied).contains(OBJECTIVE), "{}", text(&applied));
    }

    #[test]
    fn arguments_that_are_not_an_object_are_reported_rather_than_panicking() {
        let mut raw = call(GET_GOAL, json!({}));
        raw.arguments = json!([1, 2, 3]);
        let applied = run(None, &raw, 0);
        assert!(!applied.outcome.is_success());
        assert!(text(&applied).contains("JSON object"), "{}", text(&applied));
    }

    #[test]
    fn a_goal_with_a_note_renders_it() {
        let paused = active()
            .paused(Some("waiting".to_owned()), 2)
            .unwrap_or_else(|error| panic!("{error}"));
        let applied = run(Some(&paused), &call(GET_GOAL, json!({})), 0);
        assert!(
            text(&applied).contains("note: waiting"),
            "{}",
            text(&applied)
        );
    }

    /// The outcome type is the model's, so a failure is an outcome and not an error.
    #[test]
    fn every_failure_is_an_outcome_and_never_an_error() {
        let outcomes = [
            run(None, &call(PAUSE_GOAL, json!({})), 0),
            run(None, &call(ABANDON_GOAL, json!({})), 0),
            run(None, &call(CREATE_GOAL, json!({})), 0),
        ];
        for applied in outcomes {
            let outcome = applied.outcome;
            assert!(!outcome.is_success());
            assert!(outcome.message().is_some(), "{outcome:?}");
        }
    }
}
