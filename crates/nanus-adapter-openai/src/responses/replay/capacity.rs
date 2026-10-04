//! Count the prospective envelope by borrowing original items, before any item clone.
use std::collections::BTreeMap;

use nanus_ports::{LlmError, LlmResult};
use serde::{Serialize, Serializer, ser::SerializeSeq as _};
use serde_json::Value;

#[derive(Serialize)]
struct Envelope<'a> {
    protocol: &'static str,
    prefix_digest: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    context_receipt: Option<&'a nanus_domain::message::ReplayContext>,
    blocks: Blocks<'a>,
}
struct Blocks<'a> {
    slots: &'a BTreeMap<usize, super::Slot>,
    index: usize,
    item: &'a Value,
}
impl Serialize for Blocks<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let count = self
            .slots
            .values()
            .filter(|slot| slot.done.is_some())
            .count()
            .saturating_add(1);
        let mut sequence = serializer.serialize_seq(Some(count))?;
        for (index, slot) in self.slots {
            if *index == self.index {
                sequence.serialize_element(self.item)?;
            } else if let Some(item) = &slot.done {
                sequence.serialize_element(item)?;
            }
        }
        sequence.end()
    }
}

pub fn check(
    prefix: &str,
    context: Option<&nanus_domain::message::ReplayContext>,
    slots: &BTreeMap<usize, super::Slot>,
    index: usize,
    item: &Value,
    event_limit: usize,
) -> LlmResult<()> {
    let limit = event_limit.min(nanus_domain::content::RECORD_BYTES_MAX);
    let envelope = Envelope {
        protocol: "openai.responses",
        prefix_digest: prefix,
        context_receipt: context,
        blocks: Blocks { slots, index, item },
    };
    nanus_domain::content::serialized_size(&envelope, limit)
        .map(|_| ())
        .map_err(|_| LlmError::ResponseLimit {
            resource: "assistant replay bytes",
            limit,
        })
}
