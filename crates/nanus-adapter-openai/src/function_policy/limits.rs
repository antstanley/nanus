//! Documented strict-schema provider ceilings, additional to local traversal/JSON byte limits.
use nanus_ports::LlmResult;
use serde_json::Value;

const PROPERTIES_MAX: usize = 5000;
const ENUMS_MAX: usize = 1000;
const STRING_CHARS_MAX: usize = 120_000;
const LARGE_ENUM_VALUES: usize = 250;
const LARGE_ENUM_CHARS_MAX: usize = 15_000;

#[derive(Default)]
pub struct Counts {
    properties: usize,
    enums: usize,
    string_chars: usize,
}

fn add(used: usize, added: usize, limit: usize) -> LlmResult<usize> {
    used.checked_add(added)
        .filter(|v| *v <= limit)
        .ok_or_else(super::refused)
}

impl Counts {
    pub fn observe(&mut self, schema: &Value) -> LlmResult<()> {
        if let Some(properties) = schema["properties"].as_object() {
            self.properties = add(self.properties, properties.len(), PROPERTIES_MAX)?;
            for name in properties.keys() {
                self.string_chars = add(self.string_chars, name.chars().count(), STRING_CHARS_MAX)?;
            }
        }
        if let Some(values) = schema["enum"].as_array() {
            self.enums = add(self.enums, values.len(), ENUMS_MAX)?;
            let mut enum_chars = 0;
            // Primitive enum comparisons are bounded by the documented 1000-value ceiling.
            let mut seen = Vec::with_capacity(values.len());
            for value in values {
                if !compatible(schema, value) || seen.contains(&value) {
                    return Err(super::refused());
                }
                seen.push(value);
                let chars = value.as_str().map_or(0, |v| v.chars().count());
                enum_chars = add(enum_chars, chars, STRING_CHARS_MAX)?;
                self.string_chars = add(self.string_chars, chars, STRING_CHARS_MAX)?;
            }
            if values.len() > LARGE_ENUM_VALUES && enum_chars > LARGE_ENUM_CHARS_MAX {
                return Err(super::refused());
            }
        }
        Ok(())
    }
}

fn compatible(schema: &Value, value: &Value) -> bool {
    let accepts = |name| super::includes_type(schema, name);
    match value {
        Value::Null => accepts("null"),
        Value::Bool(_) => accepts("boolean"),
        Value::String(_) => accepts("string"),
        Value::Number(number) => {
            accepts("number") || ((number.is_i64() || number.is_u64()) && accepts("integer"))
        }
        Value::Array(_) | Value::Object(_) => false,
    }
}
