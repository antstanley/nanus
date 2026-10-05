//! `context_recall`: reading a session's own earlier evidence back.
//!
//! Recall is bound to one immutable snapshot — the session as the runner holds it at a step
//! boundary, up to a frontier — and to the session's own sources: the neutral text of its events
//! and the archived objects its receipts publish. It never reads the workspace, another session,
//! a serialized envelope or opaque provider replay, and a missing or corrupt object is reported
//! as exactly that rather than replaced by whatever the file looks like now.
//!
//! Work is bounded by source bytes as well as by results: one call examines at most 256 KiB, a
//! single enormous line cannot force an unbounded allocation, and reaching a bound returns
//! `coverage = partial` with a sealed cursor even when nothing matched. A cursor binds the
//! query, the frontier and the position; it is sealed with a key only this process holds, so a
//! cursor from before a restart reports `cursor_expired` instead of resuming somewhere else.

use base64::Engine as _;
use nanus_domain::context::managed::proposal::cursor;
use nanus_domain::context::managed::{
    ArtifactReceipt, CaptureStatus, ContextFrontier, ContextRecallInput, Coverage, Digest,
    ErrorCode, RecallAction, RecallEncoding, RecallHit, RecallResult, RecallStatus, RecallTarget,
    SourceField, SourceKind, SourceRef, limits, notes,
};
use nanus_domain::{Session, SessionEvent};
use nanus_ports::{ArtifactError, ArtifactStore};

/// Characters of context on each side of a hit's excerpt.
const EXCERPT_CONTEXT_BYTES: usize = 120;

/// What one recall may read.
pub struct Scope<'a> {
    /// The bound session.
    pub session: &'a Session,
    /// The snapshot: nothing at or past its count is read.
    pub frontier: ContextFrontier,
    /// Events below this count are checkpointed.
    pub durable: u64,
    /// The session's archive.
    pub archive: Option<&'a dyn ArtifactStore>,
    /// The process-held cursor key.
    pub key: &'a [u8],
}

/// Runs one recall call.
pub async fn recall(scope: &Scope<'_>, input: &ContextRecallInput) -> RecallResult {
    match input.action {
        RecallAction::Search => search(scope, input).await,
        RecallAction::Read => match &input.target {
            Some(target) => read(scope, target, input).await,
            None => refused(scope, input.encoding, ErrorCode::InvalidReference),
        },
    }
}

/// A result carrying only a failure.
pub fn refused(scope: &Scope<'_>, encoding: RecallEncoding, code: ErrorCode) -> RecallResult {
    let status = match code {
        ErrorCode::SourceUnavailable => RecallStatus::Unavailable,
        ErrorCode::SourceCorrupt => RecallStatus::Corrupt,
        _ => RecallStatus::Refused,
    };
    RecallResult {
        status,
        frontier: scope.frontier.clone(),
        hits: Vec::new(),
        data: None,
        encoding,
        actual_offset: None,
        next_offset: None,
        invalid_bytes_replaced: false,
        coverage: Coverage::Unavailable,
        next_cursor: None,
        error_code: Some(code),
        source: None,
        durable: false,
    }
}

/// One searchable source of the snapshot, in a fixed order.
enum Source<'a> {
    Event {
        seq: u64,
        field: SourceField,
        text: &'a str,
    },
    Artifact {
        seq: u64,
        receipt: &'a ArtifactReceipt,
    },
}

/// Every searchable source below the frontier: event text first, then published artifacts.
fn sources<'a>(session: &'a Session, count: u64) -> Vec<Source<'a>> {
    let mut events = Vec::new();
    let mut artifacts = Vec::new();
    let limit = usize::try_from(count).unwrap_or(usize::MAX);
    for (index, event) in session.log().events().iter().take(limit).enumerate() {
        let seq = u64::try_from(index).unwrap_or(u64::MAX);
        let mut push = |field, text: Option<&'a str>| {
            if let Some(text) = text.filter(|text| !text.is_empty()) {
                events.push(Source::Event { seq, field, text });
            }
        };
        match event {
            SessionEvent::UserMessage { text } => push(SourceField::UserText, Some(text)),
            SessionEvent::AssistantMessage {
                text, reasoning, ..
            } => {
                push(SourceField::AssistantText, text.as_deref());
                push(SourceField::AssistantReasoning, reasoning.as_deref());
            }
            SessionEvent::ToolResult { content, .. } => push(SourceField::ToolText, Some(content)),
            SessionEvent::ArtifactPublished { payload }
                if payload.status != CaptureStatus::Unavailable =>
            {
                artifacts.push(Source::Artifact {
                    seq,
                    receipt: payload,
                });
            }
            _ => {}
        }
    }
    events.extend(artifacts);
    events
}

/// The search cursor's sealed payload.
struct Position {
    source: usize,
    byte: u64,
}

fn query_tag(query: &str) -> String {
    Digest::of(query.as_bytes())
        .as_str()
        .get(..16)
        .unwrap_or_default()
        .to_owned()
}

fn seal(scope: &Scope<'_>, query: &str, position: &Position) -> String {
    let payload = format!(
        "s1:{}:{}:{}:{}:{}",
        scope.frontier.event_count,
        scope
            .frontier
            .prefix_sha256
            .as_str()
            .get(..16)
            .unwrap_or_default(),
        query_tag(query),
        position.source,
        position.byte
    );
    cursor::seal(scope.key, &payload)
}

/// Opens a cursor, returning its position and the frontier count it was issued at.
fn open(scope: &Scope<'_>, query: &str, token: &str) -> Option<(Position, u64)> {
    let payload = cursor::open(scope.key, token)?;
    let parts: Vec<&str> = payload.split(':').collect();
    let [tag, count, _prefix, query_part, source, byte] = parts.as_slice() else {
        return None;
    };
    if *tag != "s1" || *query_part != query_tag(query) {
        return None;
    }
    let count: u64 = count.parse().ok()?;
    if count > scope.frontier.event_count {
        return None;
    }
    let position = Position {
        source: source.parse().ok()?,
        byte: byte.parse().ok()?,
    };
    Some((position, count))
}

/// Finds every start of `needle` in `haystack`.
fn find_all(haystack: &[u8], needle: &[u8]) -> Vec<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return Vec::new();
    }
    haystack
        .windows(needle.len())
        .enumerate()
        .filter(|(_, window)| *window == needle)
        .map(|(index, _)| index)
        .collect()
}

/// The search's running state.
struct Hunt {
    hits: Vec<RecallHit>,
    examined: usize,
    next: Option<Position>,
}

async fn search(scope: &Scope<'_>, input: &ContextRecallInput) -> RecallResult {
    let query = input.query.as_deref().unwrap_or_default();
    let (start, count) = match input.cursor.as_deref() {
        None => (Position { source: 0, byte: 0 }, scope.frontier.event_count),
        Some(token) => match open(scope, query, token) {
            Some(opened) => opened,
            None => return refused(scope, RecallEncoding::Text, ErrorCode::CursorExpired),
        },
    };
    let all = sources(scope.session, count);
    let limit = usize::try_from(input.limit).unwrap_or(limits::RECALL_HITS_DEFAULT);
    let mut hunt = Hunt {
        hits: Vec::new(),
        examined: 0,
        next: None,
    };
    let mut failure = None;
    for (index, source) in all.iter().enumerate().skip(start.source) {
        let from = if index == start.source { start.byte } else { 0 };
        if let Err(code) = scan(scope, (source, index), from, (query, limit), &mut hunt).await {
            failure.get_or_insert(code);
        }
        if hunt.next.is_some() {
            break;
        }
    }
    finish_search(scope, query, hunt, failure, input.max_bytes)
}

/// Scans one source from `from`, stopping at the work or hit bound.
async fn scan(
    scope: &Scope<'_>,
    source: (&Source<'_>, usize),
    from: u64,
    wanted: (&str, usize),
    hunt: &mut Hunt,
) -> Result<(), ErrorCode> {
    let (source, index) = source;
    let (query, limit) = wanted;
    let needle = query.as_bytes();
    let budget = limits::RECALL_WORK_BYTES.saturating_sub(hunt.examined);
    let start = usize::try_from(from).unwrap_or(usize::MAX);
    let (bytes, total): (std::borrow::Cow<'_, [u8]>, usize) = match source {
        Source::Event { text, .. } => {
            let end = start.saturating_add(budget).min(text.len());
            let slice = text.as_bytes().get(start..end).unwrap_or_default();
            (std::borrow::Cow::Borrowed(slice), text.len())
        }
        Source::Artifact { receipt, .. } => {
            let total = usize::try_from(receipt.retained_bytes).unwrap_or(usize::MAX);
            let want = budget
                .min(limits::RECALL_CHUNK_BYTES)
                .min(total.saturating_sub(start));
            let bytes = read_artifact(scope, receipt, from, want).await?;
            (std::borrow::Cow::Owned(bytes), total)
        }
    };
    hunt.examined = hunt.examined.saturating_add(bytes.len());
    for offset in find_all(&bytes, needle) {
        if hunt.hits.len() >= limit {
            let byte = from.saturating_add(u64::try_from(offset).unwrap_or(u64::MAX));
            hunt.next = Some(Position {
                source: index,
                byte,
            });
            return Ok(());
        }
        hunt.hits
            .push(hit(scope, source, &bytes, from, offset, needle.len()));
    }
    let end = start.saturating_add(bytes.len());
    if end < total {
        // Stopped by the work bound: resume so a match straddling the cut is found next time,
        // and never at or before where this scan started.
        let overlap = end
            .saturating_sub(needle.len().saturating_sub(1))
            .max(start.saturating_add(1));
        hunt.next = Some(Position {
            source: index,
            byte: u64::try_from(overlap).unwrap_or(u64::MAX),
        });
    }
    Ok(())
}

/// Builds one hit from a match at `offset` within `bytes`, which begin at `from`.
fn hit(
    scope: &Scope<'_>,
    source: &Source<'_>,
    bytes: &[u8],
    from: u64,
    offset: usize,
    length: usize,
) -> RecallHit {
    let low = offset.saturating_sub(EXCERPT_CONTEXT_BYTES);
    let high = offset
        .saturating_add(length)
        .saturating_add(EXCERPT_CONTEXT_BYTES)
        .min(bytes.len());
    let excerpt: String = String::from_utf8_lossy(bytes.get(low..high).unwrap_or_default())
        .chars()
        .take(limits::RECALL_EXCERPT_CHARS_MAX)
        .collect();
    let position = from.saturating_add(u64::try_from(offset).unwrap_or(u64::MAX));
    let length = u64::try_from(length).unwrap_or(u64::MAX);
    let (source, seq, tool) = match source {
        Source::Event { seq, field, text } => (
            SourceRef {
                kind: SourceKind::Event,
                event_seq: Some(*seq),
                block_index: None,
                artifact_id: None,
                offset: position,
                length,
                source_digest: Digest::of(text.as_bytes()),
                field: *field,
            },
            *seq,
            tool_facts(scope.session, *seq),
        ),
        Source::Artifact { seq, receipt } => (
            SourceRef {
                kind: SourceKind::Artifact,
                event_seq: None,
                block_index: None,
                artifact_id: receipt.artifact_id.clone(),
                offset: position,
                length,
                source_digest: receipt
                    .retained_sha256
                    .clone()
                    .unwrap_or_else(Digest::empty),
                field: SourceField::Artifact,
            },
            *seq,
            (None, Some(receipt.call_id.clone()), None),
        ),
    };
    RecallHit {
        source,
        tool_name: tool.0,
        call_id: tool.1,
        is_error: tool.2,
        excerpt,
        durable: seq < scope.durable,
    }
}

/// The tool name, call id and error flag of a tool-result event.
fn tool_facts(session: &Session, seq: u64) -> (Option<String>, Option<String>, Option<bool>) {
    let events = session.log().events();
    let Some(SessionEvent::ToolResult {
        call_id, is_error, ..
    }) = usize::try_from(seq)
        .ok()
        .and_then(|index| events.get(index))
    else {
        return (None, None, None);
    };
    let name = events.iter().find_map(|event| match event {
        SessionEvent::ToolCall {
            call_id: id, name, ..
        } if id == call_id => Some(name.as_str().to_owned()),
        _ => None,
    });
    (name, Some(call_id.as_str().to_owned()), Some(*is_error))
}

/// Assembles a search result inside the encoded output bound.
fn finish_search(
    scope: &Scope<'_>,
    query: &str,
    mut hunt: Hunt,
    failure: Option<ErrorCode>,
    max_bytes: u32,
) -> RecallResult {
    let bound = usize::try_from(max_bytes)
        .unwrap_or(limits::RECALL_BYTES_MAX)
        .min(limits::RECALL_BYTES_MAX);
    loop {
        let result = RecallResult {
            status: RecallStatus::Ok,
            frontier: scope.frontier.clone(),
            hits: hunt.hits.clone(),
            data: None,
            encoding: RecallEncoding::Text,
            actual_offset: None,
            next_offset: None,
            invalid_bytes_replaced: false,
            coverage: if hunt.next.is_some() || failure.is_some() {
                Coverage::Partial
            } else {
                Coverage::Complete
            },
            next_cursor: hunt
                .next
                .as_ref()
                .map(|position| seal(scope, query, position)),
            error_code: failure,
            source: None,
            durable: hunt.hits.iter().all(|hit| hit.durable),
        };
        let fits = nanus_domain::content::serialized_size(&result, bound).is_ok();
        if fits || hunt.hits.is_empty() {
            return result;
        }
        // Drop the last hit and resume from it, so nothing is skipped by the output bound.
        if let Some(dropped) = hunt.hits.pop() {
            hunt.next = Some(resume_at(scope, &dropped));
        }
    }
}

/// The cursor position that re-finds a dropped hit first.
fn resume_at(scope: &Scope<'_>, dropped: &RecallHit) -> Position {
    let all = sources(scope.session, scope.frontier.event_count);
    let source = all
        .iter()
        .position(|source| match (source, &dropped.source.kind) {
            (Source::Event { seq, field, .. }, SourceKind::Event) => {
                Some(*seq) == dropped.source.event_seq && *field == dropped.source.field
            }
            (Source::Artifact { receipt, .. }, SourceKind::Artifact) => {
                receipt.artifact_id == dropped.source.artifact_id
            }
            _ => false,
        })
        .unwrap_or(0);
    Position {
        source,
        byte: dropped.source.offset,
    }
}

/// Reads a verified range of a published artifact.
async fn read_artifact(
    scope: &Scope<'_>,
    receipt: &ArtifactReceipt,
    offset: u64,
    length: usize,
) -> Result<Vec<u8>, ErrorCode> {
    let archive = scope.archive.ok_or(ErrorCode::SourceUnavailable)?;
    let length = u64::try_from(length).map_err(|_| ErrorCode::SourceUnavailable)?;
    archive
        .read_range(scope.session.id(), receipt, offset, length)
        .await
        .map_err(|error| match error {
            ArtifactError::Corrupt => ErrorCode::SourceCorrupt,
            ArtifactError::Unavailable => ErrorCode::SourceUnavailable,
            ArtifactError::Denied => ErrorCode::PolicyDenied,
        })
}

/// What a read resolves its target to.
struct Resolved {
    bytes: Vec<u8>,
    total: u64,
    source: SourceRef,
    seq: u64,
}

async fn read(
    scope: &Scope<'_>,
    target: &RecallTarget,
    input: &ContextRecallInput,
) -> RecallResult {
    let bound = usize::try_from(input.max_bytes)
        .unwrap_or(limits::RECALL_BYTES_MAX)
        .min(limits::RECALL_BYTES_MAX);
    let window = u64::try_from(bound).unwrap_or(u64::MAX).min(target.length);
    let resolved = match resolve(scope, target, window).await {
        Ok(resolved) => resolved,
        Err(code) => return refused(scope, input.encoding, code),
    };
    let mut keep = resolved.bytes.len();
    loop {
        let result = render_read(scope, &resolved, keep, input.encoding);
        let data_fits = result
            .data
            .as_ref()
            .is_none_or(|data| nanus_domain::content::serialized_size(data, bound).is_ok());
        let whole_fits =
            nanus_domain::content::serialized_size(&result, limits::RECALL_BYTES_MAX).is_ok();
        if (data_fits && whole_fits) || keep == 0 {
            return result;
        }
        keep = keep.saturating_mul(3).checked_div(4).unwrap_or(0);
    }
}

/// Resolves a read target to the bytes of its range, at most `window` of them.
async fn resolve(
    scope: &Scope<'_>,
    target: &RecallTarget,
    window: u64,
) -> Result<Resolved, ErrorCode> {
    match target.kind {
        SourceKind::Event => {
            let seq = target.event_seq.ok_or(ErrorCode::InvalidReference)?;
            if seq >= scope.frontier.event_count {
                return Err(ErrorCode::SourceUnavailable);
            }
            let text =
                notes::event_text(scope.session.log(), seq, target.field, target.block_index)
                    .ok_or(ErrorCode::SourceUnavailable)?;
            let total = u64::try_from(text.len()).map_err(|_| ErrorCode::SourceUnavailable)?;
            let start = target.offset.min(total);
            let end = start.saturating_add(window).min(total);
            let range =
                usize::try_from(start).unwrap_or(usize::MAX)..usize::try_from(end).unwrap_or(0);
            Ok(Resolved {
                bytes: text.as_bytes().get(range).unwrap_or_default().to_vec(),
                total,
                seq,
                source: SourceRef {
                    kind: SourceKind::Event,
                    event_seq: Some(seq),
                    block_index: target.block_index,
                    artifact_id: None,
                    offset: start,
                    length: 0,
                    source_digest: Digest::of(text.as_bytes()),
                    field: target.field,
                },
            })
        }
        SourceKind::Artifact => resolve_artifact(scope, target, window).await,
    }
}

async fn resolve_artifact(
    scope: &Scope<'_>,
    target: &RecallTarget,
    window: u64,
) -> Result<Resolved, ErrorCode> {
    let id = target
        .artifact_id
        .as_ref()
        .ok_or(ErrorCode::InvalidReference)?;
    let count = usize::try_from(scope.frontier.event_count).unwrap_or(usize::MAX);
    let (seq, receipt) = scope
        .session
        .log()
        .events()
        .iter()
        .take(count)
        .enumerate()
        .find_map(|(index, event)| match event {
            SessionEvent::ArtifactPublished { payload }
                if payload.artifact_id.as_ref() == Some(id) =>
            {
                Some((u64::try_from(index).unwrap_or(u64::MAX), payload))
            }
            _ => None,
        })
        .ok_or(ErrorCode::SourceUnavailable)?;
    if receipt.status == CaptureStatus::Unavailable {
        return Err(ErrorCode::SourceUnavailable);
    }
    let total = receipt.retained_bytes;
    let start = target.offset.min(total);
    let length = window.min(total.saturating_sub(start));
    let bytes = read_artifact(scope, receipt, start, usize::try_from(length).unwrap_or(0)).await?;
    Ok(Resolved {
        bytes,
        total,
        seq,
        source: SourceRef {
            kind: SourceKind::Artifact,
            event_seq: None,
            block_index: None,
            artifact_id: Some(id.clone()),
            offset: start,
            length: 0,
            source_digest: receipt
                .retained_sha256
                .clone()
                .unwrap_or_else(Digest::empty),
            field: SourceField::Artifact,
        },
    })
}

/// Renders the first `keep` resolved bytes in the requested encoding.
fn render_read(
    scope: &Scope<'_>,
    resolved: &Resolved,
    keep: usize,
    encoding: RecallEncoding,
) -> RecallResult {
    let bytes = resolved.bytes.get(..keep).unwrap_or_default();
    let (data, replaced) = match encoding {
        RecallEncoding::Text => {
            let text = String::from_utf8_lossy(bytes);
            let replaced = matches!(text, std::borrow::Cow::Owned(_));
            (text.into_owned(), replaced)
        }
        RecallEncoding::Base64 => (
            base64::engine::general_purpose::STANDARD.encode(bytes),
            false,
        ),
    };
    let returned = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    let end = resolved.source.offset.saturating_add(returned);
    let mut source = resolved.source.clone();
    source.length = returned;
    RecallResult {
        status: RecallStatus::Ok,
        frontier: scope.frontier.clone(),
        hits: Vec::new(),
        data: Some(data),
        encoding,
        actual_offset: Some(resolved.source.offset),
        next_offset: (end < resolved.total).then_some(end),
        invalid_bytes_replaced: replaced,
        coverage: if end < resolved.total {
            Coverage::Partial
        } else {
            Coverage::Complete
        },
        next_cursor: None,
        error_code: None,
        source: Some(source),
        durable: resolved.seq < scope.durable,
    }
}

#[cfg(test)]
#[path = "recall/tests.rs"]
mod tests;
