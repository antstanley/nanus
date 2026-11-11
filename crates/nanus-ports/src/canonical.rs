//! A request's canonical encoding, as the ordered units a provider's prefix cache reads.
//!
//! Every adapter encodes a request in two steps: canonically, with no tail and no cache hints, and
//! then decorated with both. Provider caches are prefix caches, so whether one request can read
//! what the one before it wrote is a question about the canonical encodings: the earlier one is a
//! prefix of the later one, or it is not, and if not, the first byte where they part is where the
//! provider has to start reading again.
//!
//! A body is a JSON object whose keys serialise sorted, so its raw bytes put the conversation in
//! the middle, between fields that do not move. [`CanonicalEncoding`] states the order the
//! provider reads instead: one unit for everything that is not the conversation — the model, the
//! tools, a top-level system prompt, the settings — and then one unit per conversation item, in
//! order. Two encodings are compared unit by unit, and an offset is a byte count over the units'
//! concatenation. Nothing here is a tokenizer: an offset says where the bytes part, not how many
//! tokens precede it.

use serde_json::Value;

/// A request's canonical encoding: the head, then each conversation item, each serialised.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CanonicalEncoding {
    units: Vec<String>,
}

impl CanonicalEncoding {
    /// Splits a canonical body into its head and the items of its `conversation` array.
    ///
    /// The head is the body without that key. A body with no such array is one unit, the whole
    /// body, so it is still comparable rather than empty.
    #[must_use]
    pub fn from_body(body: &Value, conversation: &str) -> Self {
        let mut head = body.clone();
        let items = head
            .as_object_mut()
            .and_then(|fields| fields.remove(conversation))
            .and_then(|items| match items {
                Value::Array(items) => Some(items),
                _ => None,
            })
            .unwrap_or_default();
        let mut units = Vec::with_capacity(items.len().saturating_add(1));
        units.push(head.to_string());
        units.extend(items.iter().map(Value::to_string));
        // Postcondition: the head is always first, so even an empty conversation is comparable.
        assert!(!units.is_empty(), "an encoding has a head");
        Self { units }
    }

    /// The units, head first.
    #[must_use]
    pub fn units(&self) -> &[String] {
        &self.units
    }

    /// The number of bytes over every unit.
    #[must_use]
    pub fn byte_len(&self) -> u64 {
        self.units.iter().fold(0_u64, |total, unit| {
            total.saturating_add(u64::try_from(unit.len()).unwrap_or(u64::MAX))
        })
    }

    /// Where `later` parts from this encoding, or `None` when this one is a prefix of it.
    ///
    /// A prefix unit by unit: every unit here is equal to the unit at the same place in `later`.
    /// Otherwise the offset is the bytes of the leading units the two share, plus the bytes the
    /// first differing pair have in common. A `later` that is shorter and agrees as far as it goes
    /// parts where it ends.
    #[must_use]
    pub fn divergence(&self, later: &Self) -> Option<u64> {
        let mut offset = 0_u64;
        for (index, earlier) in self.units.iter().enumerate() {
            let Some(unit) = later.units.get(index) else {
                return Some(offset);
            };
            if unit != earlier {
                let common = earlier
                    .bytes()
                    .zip(unit.bytes())
                    .take_while(|(left, right)| left == right)
                    .count();
                return Some(offset.saturating_add(u64::try_from(common).unwrap_or(u64::MAX)));
            }
            offset = offset.saturating_add(u64::try_from(unit.len()).unwrap_or(u64::MAX));
        }
        // Postcondition: a prefix has consumed exactly this encoding's bytes.
        assert_eq!(
            offset,
            self.byte_len(),
            "a prefix covers the whole encoding"
        );
        None
    }

    /// The bytes before unit `index`: where that unit starts in the concatenation.
    #[must_use]
    pub fn offset_of(&self, index: usize) -> u64 {
        self.units.iter().take(index).fold(0_u64, |total, unit| {
            total.saturating_add(u64::try_from(unit.len()).unwrap_or(u64::MAX))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn encoding(messages: &[&str]) -> CanonicalEncoding {
        CanonicalEncoding::from_body(
            &json!({ "model": "m", "messages": messages, "tools": [] }),
            "messages",
        )
    }

    #[test]
    fn an_encoding_is_its_head_then_each_conversation_item() {
        let split = encoding(&["a", "b"]);
        assert_eq!(
            split.units(),
            [r#"{"model":"m","tools":[]}"#, r#""a""#, r#""b""#]
        );
        assert_eq!(split.byte_len(), 30);
        assert_eq!(split.offset_of(2), 27);
    }

    #[test]
    fn an_earlier_request_that_the_later_extends_is_a_prefix() {
        assert_eq!(encoding(&["a"]).divergence(&encoding(&["a", "b"])), None);
        assert_eq!(encoding(&["a"]).divergence(&encoding(&["a"])), None);
    }

    #[test]
    fn a_changed_item_parts_where_its_bytes_part() {
        let earlier = encoding(&["abc", "d"]);
        let later = encoding(&["abX", "d"]);
        // The head (24 bytes), then `"ab` in common.
        assert_eq!(earlier.divergence(&later), Some(27));
    }

    #[test]
    fn a_changed_head_parts_inside_the_head() {
        let earlier = encoding(&["a"]);
        let later = CanonicalEncoding::from_body(
            &json!({ "model": "n", "messages": ["a"], "tools": [] }),
            "messages",
        );
        assert_eq!(earlier.divergence(&later), Some(10));
    }

    #[test]
    fn a_later_request_that_is_shorter_parts_where_it_ends() {
        let earlier = encoding(&["a", "b"]);
        let later = encoding(&["a"]);
        assert_eq!(earlier.divergence(&later), Some(27));
    }

    #[test]
    fn a_body_without_the_conversation_is_one_unit() {
        let whole = CanonicalEncoding::from_body(&json!({ "model": "m" }), "input");
        assert_eq!(whole.units(), [r#"{"model":"m"}"#]);
    }
}
