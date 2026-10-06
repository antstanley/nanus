//! What the two context tools accept and return.
//!
//! The inputs are parsed strictly: every documented key must be present (a nullable one as
//! `null`), unknown keys are refused, and the cross-field rules the schema cannot state are
//! checked here. A call that fails any of it is an ordinary bounded tool failure — the model
//! is told what was wrong, and nothing was read or staged.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::ids::{ArtifactId, Digest, ErrorCode, FragmentId};
use super::limits;
use super::records::{
    ContextDecision, ContextFrontier, ContextMode, SourceField, SourceKind, SourceRef, WorkingNote,
    check_selector,
};

/// What a `context_manage` call asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManageAction {
    /// Read the accepted state and a page of the fragment catalog.
    Inspect,
    /// Request hide/restore deltas and a complete replacement note array.
    Propose,
}

/// The arguments of `context_manage`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextManageInput {
    /// Inspect or propose.
    pub action: ManageAction,
    /// The accepted revision the proposal was made against.
    pub base_revision: Option<u64>,
    /// The frontier the proposal was made against.
    pub base_frontier: Option<ContextFrontier>,
    /// Fragments to hide.
    pub hide: Vec<FragmentId>,
    /// Fragments to restore.
    pub restore: Vec<FragmentId>,
    /// The complete replacement note array.
    pub notes: Vec<WorkingNote>,
    /// A catalog cursor, for a later inspect page.
    pub cursor: Option<String>,
    /// The snapshot profile the proposal was made against.
    pub base_profile_digest: Option<Digest>,
}

/// The keys `context_manage` requires.
const MANAGE_KEYS: [&str; 8] = [
    "action",
    "base_revision",
    "base_frontier",
    "hide",
    "restore",
    "notes",
    "cursor",
    "base_profile_digest",
];

impl ContextManageInput {
    /// Parses and checks one call's arguments.
    ///
    /// # Errors
    ///
    /// Returns a sentence the model can act on.
    pub fn parse(arguments: &Value) -> Result<Self, String> {
        require_keys(arguments, &MANAGE_KEYS)?;
        let input: Self =
            serde_json::from_value(arguments.clone()).map_err(|error| error.to_string())?;
        input.validate()?;
        Ok(input)
    }

    /// Checks the cross-field rules.
    fn validate(&self) -> Result<(), String> {
        match self.action {
            ManageAction::Inspect => {
                if self.base_revision.is_some()
                    || self.base_frontier.is_some()
                    || self.base_profile_digest.is_some()
                    || !self.hide.is_empty()
                    || !self.restore.is_empty()
                    || !self.notes.is_empty()
                {
                    return Err("inspect takes null bases and empty hide, restore and notes".into());
                }
            }
            ManageAction::Propose => {
                if self.base_revision.is_none()
                    || self.base_frontier.is_none()
                    || self.base_profile_digest.is_none()
                    || self.cursor.is_some()
                {
                    return Err(
                        "propose takes base_revision, base_frontier and base_profile_digest \
                         from inspect, and a null cursor"
                            .into(),
                    );
                }
            }
        }
        if self.hide.len() > limits::HIDDEN_MAX || self.restore.len() > limits::HIDDEN_MAX {
            return Err(format!(
                "at most {} ids may be hidden or restored",
                limits::HIDDEN_MAX
            ));
        }
        if has_duplicate(&self.hide) || has_duplicate(&self.restore) {
            return Err("hide and restore ids must each be unique".into());
        }
        if self.hide.iter().any(|id| self.restore.contains(id)) {
            return Err("an id cannot be both hidden and restored".into());
        }
        if self.notes.len() > limits::NOTES_MAX {
            return Err(format!("at most {} notes", limits::NOTES_MAX));
        }
        check_cursor(self.cursor.as_deref())
    }
}

/// What a `context_recall` call asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecallAction {
    /// Literal, case-sensitive search over retained neutral text.
    Search,
    /// One bounded range of one source.
    Read,
}

/// How recalled bytes are returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecallEncoding {
    /// UTF-8 text, invalid bytes replaced and reported.
    Text,
    /// Base64 of the raw bytes.
    Base64,
}

/// A recall read target: one exact field of one event, or one artifact range.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecallTarget {
    /// Event or artifact.
    pub kind: SourceKind,
    /// The event's sequence.
    pub event_seq: Option<u64>,
    /// The text block's index, for `tool_block`.
    pub block_index: Option<u8>,
    /// The artifact.
    pub artifact_id: Option<ArtifactId>,
    /// The first byte.
    pub offset: u64,
    /// Bytes wanted.
    pub length: u64,
    /// The field.
    pub field: SourceField,
}

/// The arguments of `context_recall`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextRecallInput {
    /// Search or read.
    pub action: RecallAction,
    /// The literal to search for.
    pub query: Option<String>,
    /// The source to read.
    pub target: Option<RecallTarget>,
    /// A search cursor.
    pub cursor: Option<String>,
    /// Hits wanted, one to forty.
    pub limit: u32,
    /// Encoded bytes wanted, one to 8192.
    pub max_bytes: u32,
    /// Text or base64.
    pub encoding: RecallEncoding,
}

/// The keys `context_recall` requires.
const RECALL_KEYS: [&str; 7] = [
    "action",
    "query",
    "target",
    "cursor",
    "limit",
    "max_bytes",
    "encoding",
];

impl ContextRecallInput {
    /// Parses and checks one call's arguments.
    ///
    /// # Errors
    ///
    /// Returns a sentence the model can act on.
    pub fn parse(arguments: &Value) -> Result<Self, String> {
        require_keys(arguments, &RECALL_KEYS)?;
        let input: Self =
            serde_json::from_value(arguments.clone()).map_err(|error| error.to_string())?;
        input.validate()?;
        Ok(input)
    }

    fn validate(&self) -> Result<(), String> {
        let limit_ok = usize::try_from(self.limit)
            .is_ok_and(|limit| (1..=limits::RECALL_HITS_MAX).contains(&limit));
        let bytes_ok = usize::try_from(self.max_bytes)
            .is_ok_and(|bytes| (1..=limits::RECALL_BYTES_MAX).contains(&bytes));
        if !limit_ok || !bytes_ok {
            return Err("limit is 1 to 40 and max_bytes is 1 to 8192".into());
        }
        match self.action {
            RecallAction::Search => {
                let query = self.query.as_deref().unwrap_or_default();
                if query.is_empty() || query.chars().count() > limits::RECALL_QUERY_CHARS_MAX {
                    return Err("search takes a query of 1 to 256 characters".into());
                }
                if self.target.is_some() || self.encoding != RecallEncoding::Text {
                    return Err("search takes no target and text encoding".into());
                }
            }
            RecallAction::Read => {
                let Some(target) = &self.target else {
                    return Err("read takes a target".into());
                };
                if self.query.is_some() || self.cursor.is_some() {
                    return Err("read takes no query and no cursor".into());
                }
                check_selector(
                    target.kind,
                    target.field,
                    target.event_seq,
                    target.block_index,
                    target.artifact_id.as_ref(),
                )
                .map_err(|_| String::from("the target's selector does not match its kind"))?;
                if target.offset.checked_add(target.length).is_none() {
                    return Err("the target's range overflows".into());
                }
            }
        }
        check_cursor(self.cursor.as_deref())
    }
}

/// Refuses an argument object missing a required key.
fn require_keys(arguments: &Value, keys: &[&str]) -> Result<(), String> {
    let Value::Object(map) = arguments else {
        return Err("the arguments must be a JSON object".into());
    };
    let missing: Vec<&str> = keys
        .iter()
        .copied()
        .filter(|key| !map.contains_key(*key))
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "missing required keys (use null for an absent value): {}",
            missing.join(", ")
        ))
    }
}

fn check_cursor(cursor: Option<&str>) -> Result<(), String> {
    match cursor {
        Some(cursor) if cursor.is_empty() || cursor.chars().count() > limits::CURSOR_CHARS_MAX => {
            Err("a cursor is 1 to 512 characters".into())
        }
        _ => Ok(()),
    }
}

fn has_duplicate(ids: &[FragmentId]) -> bool {
    ids.iter()
        .enumerate()
        .any(|(index, id)| ids[..index].contains(id))
}

/// The context status a client, the notice and an inspect result report.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextStatus {
    /// Legacy or managed.
    pub mode: ContextMode,
    /// The accepted revision.
    pub revision: u64,
    /// The frontier the status describes.
    pub frontier: ContextFrontier,
    /// The estimated input of the last prepared request.
    pub estimate_input_tokens: Option<u64>,
    /// The estimated input of the protected floor.
    pub estimate_protected_tokens: Option<u64>,
    /// The output reservation.
    pub output_reserve_tokens: u64,
    /// Which estimator produced the estimates, and over what.
    pub estimator: String,
    /// Fragments hidden.
    pub hidden_fragments: u64,
    /// Fragments protected.
    pub protected_fragments: u64,
    /// The goal revision.
    pub goal_revision: Option<u64>,
    /// Whether goal data is in the generated memory.
    pub goal_data_available: bool,
    /// Whether recall can be used.
    pub recall_available: bool,
    /// Whether archived evidence can be used.
    pub archive_available: bool,
    /// The most recent decision.
    pub last_decision: Option<ContextDecision>,
    /// The snapshot profile's digest.
    pub profile_digest: Digest,
    /// Whether the next managed request can be prepared.
    pub managed_ready: bool,
    /// Why it cannot, when it cannot.
    pub unavailable_reason: Option<ErrorCode>,
}

/// One fragment in the inspect catalog.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FragmentDescriptor {
    /// The fragment.
    pub id: FragmentId,
    /// Its assistant event.
    pub assistant_seq: u64,
    /// Its last result event, when it has results.
    pub last_result_seq: Option<u64>,
    /// Whether it is protected.
    pub protected: bool,
    /// Whether it is hidden.
    pub hidden: bool,
    /// A bounded summary: the tool names, sizes and outcome.
    pub summary: String,
}

/// What an inspect or a proposal produced.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManageStatus {
    /// An inspect page.
    Inspected,
    /// A proposal waiting for its step to settle. Never "committed".
    Staged,
    /// Refused; nothing was staged.
    Refused,
}

/// The result of `context_manage`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManageResult {
    /// What happened.
    pub status: ManageStatus,
    /// The accepted state.
    pub context: ContextStatus,
    /// The staged or refused decision.
    pub decision: Option<ContextDecision>,
    /// A page of the catalog.
    pub fragments: Vec<FragmentDescriptor>,
    /// The next page's cursor.
    pub next_cursor: Option<String>,
    /// Why it was refused.
    pub error_code: Option<ErrorCode>,
}

/// How completely a recall covered what it was asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Coverage {
    /// Everything in scope was examined.
    Complete,
    /// A work or output bound stopped it; the cursor continues.
    Partial,
    /// The source could not be examined.
    Unavailable,
}

/// One search hit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecallHit {
    /// Where the hit is.
    pub source: SourceRef,
    /// The tool, for a tool result.
    pub tool_name: Option<String>,
    /// The call, for a tool result.
    pub call_id: Option<String>,
    /// Whether the call failed, for a tool result.
    pub is_error: Option<bool>,
    /// A bounded excerpt around the match.
    pub excerpt: String,
    /// Whether the source is checkpointed.
    pub durable: bool,
}

/// How a recall ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecallStatus {
    /// It ran.
    Ok,
    /// The source is not available.
    Unavailable,
    /// The source did not verify.
    Corrupt,
    /// The request was refused.
    Refused,
}

/// The result of `context_recall`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecallResult {
    /// How it ended.
    pub status: RecallStatus,
    /// The snapshot it read.
    pub frontier: ContextFrontier,
    /// Search hits.
    pub hits: Vec<RecallHit>,
    /// Read data.
    pub data: Option<String>,
    /// How `data` is encoded.
    pub encoding: RecallEncoding,
    /// The first byte actually returned.
    pub actual_offset: Option<u64>,
    /// The byte after the last one returned.
    pub next_offset: Option<u64>,
    /// Whether invalid UTF-8 was replaced.
    pub invalid_bytes_replaced: bool,
    /// How completely the scope was covered.
    pub coverage: Coverage,
    /// Where a search continues.
    pub next_cursor: Option<String>,
    /// Why it failed.
    pub error_code: Option<ErrorCode>,
    /// The verified source of a read.
    pub source: Option<SourceRef>,
    /// Whether the source is checkpointed.
    pub durable: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn inspect() -> Value {
        json!({"action": "inspect", "base_revision": null, "base_frontier": null, "hide": [],
            "restore": [], "notes": [], "cursor": null, "base_profile_digest": null})
    }

    #[test]
    fn inspect_takes_null_bases_and_no_edits() {
        assert!(ContextManageInput::parse(&inspect()).is_ok());
        let mut with_edit = inspect();
        with_edit["hide"] = json!(["f:1"]);
        assert!(ContextManageInput::parse(&with_edit).is_err());
        let mut missing = inspect();
        if let Value::Object(map) = &mut missing {
            map.remove("cursor");
        }
        let error = ContextManageInput::parse(&missing)
            .err()
            .unwrap_or_default();
        assert!(error.contains("cursor"), "{error}");
        let mut unknown = inspect();
        unknown["extra"] = json!(1);
        assert!(ContextManageInput::parse(&unknown).is_err());
    }

    #[test]
    fn propose_takes_its_bases_and_disjoint_unique_edits() {
        let frontier = json!({"session_id": "s", "event_count": 3,
            "prefix_sha256": Digest::empty().as_str(), "projection_revision": 0});
        let propose = json!({"action": "propose", "base_revision": 0, "base_frontier": frontier,
            "hide": ["f:1"], "restore": [], "notes": [], "cursor": null,
            "base_profile_digest": Digest::empty().as_str()});
        assert!(ContextManageInput::parse(&propose).is_ok());
        let mut both = propose.clone();
        both["restore"] = json!(["f:1"]);
        assert!(ContextManageInput::parse(&both).is_err());
        let mut twice = propose.clone();
        twice["hide"] = json!(["f:1", "f:1"]);
        assert!(ContextManageInput::parse(&twice).is_err());
        let mut no_base = propose;
        no_base["base_revision"] = Value::Null;
        assert!(ContextManageInput::parse(&no_base).is_err());
    }

    #[test]
    fn search_and_read_take_disjoint_arguments() {
        let search = json!({"action": "search", "query": "marker", "target": null,
            "cursor": null, "limit": 20, "max_bytes": 8192, "encoding": "text"});
        assert!(ContextRecallInput::parse(&search).is_ok());
        let mut empty = search.clone();
        empty["query"] = json!("");
        assert!(ContextRecallInput::parse(&empty).is_err());
        let mut over = search;
        over["limit"] = json!(41);
        assert!(ContextRecallInput::parse(&over).is_err());
        let read = json!({"action": "read", "query": null, "cursor": null, "limit": 1,
            "max_bytes": 100, "encoding": "base64", "target": {"kind": "event", "event_seq": 4,
            "block_index": null, "artifact_id": null, "offset": 0, "length": 10,
            "field": "tool_text"}});
        assert!(ContextRecallInput::parse(&read).is_ok());
        let mut path = read.clone();
        path["target"]["path"] = json!("/etc/passwd");
        assert!(
            ContextRecallInput::parse(&path).is_err(),
            "paths are never accepted"
        );
        let mut mixed = read;
        mixed["target"]["field"] = json!("artifact");
        assert!(ContextRecallInput::parse(&mixed).is_err());
    }
}
