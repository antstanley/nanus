//! Reading arguments out of a tool call.
//!
//! Every tool faces the same first problem: the model sent a JSON object and the
//! tool needs a string, a number, an optional boolean. Because a model *does* send
//! the wrong shape — a number where a string belongs, a missing required field, a
//! nested object instead of a path — the extraction is written once here and
//! reported back to the model as a [`ToolOutcome::Failure`] rather than as a
//! harness panic.
//!
//! The reported message is deliberately specific: it is the model's only signal
//! about how to fix its call, and "invalid arguments" teaches it nothing.

use nanus_domain::{ToolOutcome, ToolResult};
use serde_json::Value;

/// The arguments of one call, with typed accessors.
pub struct Arguments<'a> {
    tool: &'a str,
    value: &'a Value,
}

impl<'a> Arguments<'a> {
    /// Wraps a call's arguments.
    ///
    /// # Panics
    ///
    /// Panics when the arguments are not a JSON object. [`ToolCall::new`] and the
    /// deserialiser both normalise `null` to an empty object, so a non-object can
    /// only arrive from a caller that constructed a `Value` by hand — a harness bug
    /// rather than a model error.
    ///
    /// [`ToolCall::new`]: nanus_domain::ToolCall::new
    #[must_use]
    pub fn new(tool: &'a str, value: &'a Value) -> Self {
        assert!(
            value.is_object(),
            "a tool's arguments are a JSON object by the time a tool sees them"
        );
        Self { tool, value }
    }

    /// Returns the raw argument object.
    #[must_use]
    pub const fn raw(&self) -> &Value {
        self.value
    }

    /// Reads a required string.
    ///
    /// # Errors
    ///
    /// Returns a failure outcome naming both the field and the type that was found,
    /// which is what the model needs in order to correct itself.
    pub fn required_str(&self, field: &str) -> Result<String, ToolOutcome> {
        match self.value.get(field) {
            Some(Value::String(text)) => Ok(text.clone()),
            Some(other) => Err(self.type_failure(field, "a string", json_kind(other))),
            None => Err(self.missing_failure(field)),
        }
    }

    /// Reads an optional string.
    ///
    /// # Errors
    ///
    /// Returns a failure outcome when the field is present but not a string. An
    /// absent field is `Ok(None)`, which is the difference between "not asked for"
    /// and "asked for wrongly".
    pub fn optional_str(&self, field: &str) -> Result<Option<String>, ToolOutcome> {
        match self.value.get(field) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(text)) => Ok(Some(text.clone())),
            Some(other) => Err(self.type_failure(field, "a string", json_kind(other))),
        }
    }

    /// Reads an optional non-negative integer.
    ///
    /// # Errors
    ///
    /// Returns a failure outcome when the field is present but is not an integer, or
    /// is an integer that does not fit the requested width.
    pub fn optional_u32(&self, field: &str) -> Result<Option<u32>, ToolOutcome> {
        match self.value.get(field) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Number(number)) => number.as_u64().map_or_else(
                || Err(self.type_failure(field, "a non-negative integer", "a negative number")),
                |value| {
                    u32::try_from(value).map(Some).map_err(|_| {
                        self.type_failure(field, "an integer below 2^32", "a huge number")
                    })
                },
            ),
            Some(other) => Err(self.type_failure(field, "an integer", json_kind(other))),
        }
    }

    /// Reads an optional boolean, defaulting to `false`.
    ///
    /// # Errors
    ///
    /// Returns a failure outcome when the field is present but is not a boolean. A
    /// string like `"true"` is rejected rather than coerced: guessing here would
    /// make a tool's behaviour depend on a spelling.
    pub fn flag(&self, field: &str) -> Result<bool, ToolOutcome> {
        match self.value.get(field) {
            None | Some(Value::Null) => Ok(false),
            Some(Value::Bool(value)) => Ok(*value),
            Some(other) => Err(self.type_failure(field, "a boolean", json_kind(other))),
        }
    }

    /// Builds a "field is missing" failure.
    fn missing_failure(&self, field: &str) -> ToolOutcome {
        ToolOutcome::failure(format!(
            "{} needs a {field:?} argument, and none was given",
            self.tool
        ))
    }

    /// Builds a "field has the wrong type" failure.
    fn type_failure(&self, field: &str, expected: &str, found: &str) -> ToolOutcome {
        ToolOutcome::failure(format!(
            "{} needs {field:?} to be {expected}, but it was {found}",
            self.tool
        ))
    }
}

/// Names the JSON kind of a value, for an error message.
fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// Wraps an outcome as the result of a call.
#[must_use]
pub fn result_of(call_id: nanus_domain::ToolCallId, outcome: ToolOutcome) -> ToolResult {
    ToolResult::new(call_id, outcome)
}

/// Unwraps a `Result` whose error is already a model-facing outcome.
///
/// The shape the extractors produce is "either a value or a finished failure", so
/// this collapses it into the single [`ToolResult`] a tool must return.
#[must_use]
pub fn finish<T>(
    call_id: nanus_domain::ToolCallId,
    outcome: Result<T, ToolOutcome>,
    on_ok: impl FnOnce(T) -> ToolOutcome,
) -> ToolResult {
    match outcome {
        Ok(value) => ToolResult::new(call_id, on_ok(value)),
        Err(failure) => ToolResult::new(call_id, failure),
    }
}

#[cfg(test)]
mod tests {
    use nanus_domain::ToolCallId;
    use serde_json::json;

    use super::*;

    fn args(value: &Value) -> Arguments<'_> {
        Arguments::new("demo", value)
    }

    #[test]
    fn a_required_string_is_returned() {
        let value = json!({ "file_path": "src/main.rs" });
        let outcome = args(&value).required_str("file_path");
        assert_eq!(outcome.ok().as_deref(), Some("src/main.rs"));
    }

    #[test]
    fn a_missing_required_string_names_the_field() {
        let value = json!({});
        let outcome = args(&value).required_str("file_path");
        let Some(failure) = outcome.err() else {
            panic!("a missing field is a failure");
        };
        let ToolOutcome::Failure { message, .. } = failure else {
            panic!("expected a failure outcome");
        };
        // The message must teach the model what to fix, so both the tool and the
        // field are named.
        assert!(message.contains("demo"), "{message}");
        assert!(message.contains("file_path"), "{message}");
    }

    #[test]
    fn a_wrong_type_names_what_was_found() {
        let value = json!({ "limit": "ten" });
        let outcome = args(&value).optional_u32("limit");
        let Some(failure) = outcome.err() else {
            panic!("a wrong type is a failure");
        };
        let ToolOutcome::Failure { message, .. } = failure else {
            panic!("expected a failure outcome");
        };
        assert!(message.contains("limit"), "{message}");
        assert!(message.contains("a string"), "{message}");
    }

    #[test]
    fn an_absent_optional_field_is_not_a_failure() {
        let value = json!({});
        assert_eq!(args(&value).optional_str("offset").ok(), Some(None));
        assert_eq!(args(&value).optional_u32("offset").ok(), Some(None));
        assert_eq!(args(&value).flag("replace_all").ok(), Some(false));
    }

    #[test]
    fn an_explicit_null_is_treated_as_absent() {
        // A model that has no value for a field often sends an explicit null, and
        // refusing that would be pedantry rather than safety.
        let value = json!({ "offset": null, "replace_all": null });
        assert_eq!(args(&value).optional_u32("offset").ok(), Some(None));
        assert_eq!(args(&value).flag("replace_all").ok(), Some(false));
    }

    #[test]
    fn a_string_boolean_is_rejected_rather_than_coerced() {
        let value = json!({ "replace_all": "true" });
        let outcome = args(&value).flag("replace_all");
        assert!(outcome.is_err(), "a string is not a boolean");
        // Pair assertion: the real boolean is accepted.
        let real = json!({ "replace_all": true });
        assert_eq!(args(&real).flag("replace_all").ok(), Some(true));
    }

    #[test]
    fn an_integer_boundary_is_respected() {
        // Boundary: zero, one, the largest `u32`, and one past it.
        let zero = json!({ "offset": 0 });
        assert_eq!(args(&zero).optional_u32("offset").ok(), Some(Some(0)));
        let max = json!({ "offset": u32::MAX });
        assert_eq!(args(&max).optional_u32("offset").ok(), Some(Some(u32::MAX)));
        let too_big = json!({ "offset": u64::from(u32::MAX) + 1 });
        assert!(args(&too_big).optional_u32("offset").is_err());
        let negative = json!({ "offset": -1 });
        assert!(args(&negative).optional_u32("offset").is_err());
    }

    #[test]
    fn a_fractional_number_is_not_an_integer() {
        let value = json!({ "offset": 1.5 });
        let outcome = args(&value).optional_u32("offset");
        assert!(outcome.is_err());
    }

    #[test]
    fn finish_unwraps_either_side() {
        let id = ToolCallId::new("call-1");
        let ok = finish(id.clone(), Ok(3_u32), |value: u32| {
            ToolOutcome::success(json!(value))
        });
        assert!(ok.is_success());

        let failure = finish(id, Err(ToolOutcome::failure("bad")), |value: u32| {
            ToolOutcome::success(json!(value))
        });
        assert!(!failure.is_success());
    }

    #[test]
    fn json_kinds_are_named_for_a_reader() {
        assert_eq!(json_kind(&json!(null)), "null");
        assert_eq!(json_kind(&json!(true)), "a boolean");
        assert_eq!(json_kind(&json!(1)), "a number");
        assert_eq!(json_kind(&json!("x")), "a string");
        assert_eq!(json_kind(&json!([])), "an array");
        assert_eq!(json_kind(&json!({})), "an object");
    }

    #[test]
    fn result_of_carries_the_call_id() {
        let id = ToolCallId::new("call-9");
        let result = result_of(id, ToolOutcome::success(json!(1)));
        assert_eq!(result.call_id.as_str(), "call-9");
    }
}
