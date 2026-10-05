//! Working notes: their bounds, their digest, and how their references are resolved.
//!
//! A note is model-authored data. Nothing here makes it more than that: a note that cites a
//! source has only *named* evidence, and the reference is resolved against the session the
//! host bound — never against a path or another session the model chose.

use serde::Serialize;

use super::ids::{Digest, ErrorCode};
use super::limits;
use super::records::{SourceField, SourceKind, SourceRef, WorkingNote};
use crate::session::{SessionEvent, SessionLog};

/// Checks every bound a note array has, measured on the encoded bytes.
///
/// # Errors
///
/// Returns [`ErrorCode::InvalidReference`] for a malformed note and
/// [`ErrorCode::StorageCapacity`] for an array past its byte bound.
pub fn validate_notes(notes: &[WorkingNote]) -> Result<(), ErrorCode> {
    if notes.len() > limits::NOTES_MAX {
        return Err(ErrorCode::StorageCapacity);
    }
    for (index, note) in notes.iter().enumerate() {
        let chars = note.claim.chars().count();
        let sources = note.sources.len();
        if chars == 0
            || chars > limits::NOTE_CHARS_MAX
            || sources == 0
            || sources > limits::NOTE_SOURCES_MAX
        {
            return Err(ErrorCode::InvalidReference);
        }
        if notes[..index].iter().any(|earlier| earlier.id == note.id) {
            return Err(ErrorCode::InvalidReference);
        }
        for source in &note.sources {
            source.validate_shape()?;
        }
    }
    crate::content::serialized_size(notes, limits::NOTES_BYTES_MAX)
        .map(|_| ())
        .map_err(|_| ErrorCode::StorageCapacity)
}

/// The canonical form a notes digest is taken over.
#[derive(Serialize)]
struct Canonical<'a> {
    policy_version: u32,
    renderer_version: u32,
    notes: &'a [WorkingNote],
}

/// Returns the digest of a note array under this policy and renderer.
///
/// Only the notes and the two versions: goals, counters and archive availability change without
/// the notes changing, and a digest that moved with them would call unchanged notes new.
#[must_use]
pub fn notes_digest(notes: &[WorkingNote]) -> Digest {
    let canonical = Canonical {
        policy_version: limits::POLICY_VERSION,
        renderer_version: limits::RENDERER_VERSION,
        notes,
    };
    Digest::of(
        serde_json::to_string(&canonical)
            .unwrap_or_default()
            .as_bytes(),
    )
}

/// Returns the exact text an event source addresses, or `None` for a selector that names none.
///
/// Only neutral text: never a serialized envelope, an argument object or opaque replay.
#[must_use]
pub fn event_text(
    log: &SessionLog,
    seq: u64,
    field: SourceField,
    block: Option<u8>,
) -> Option<&str> {
    let event = log.events().get(usize::try_from(seq).ok()?)?;
    match (event, field) {
        (SessionEvent::UserMessage { text }, SourceField::UserText) => Some(text),
        (SessionEvent::AssistantMessage { text, .. }, SourceField::AssistantText) => {
            text.as_deref()
        }
        (SessionEvent::AssistantMessage { reasoning, .. }, SourceField::AssistantReasoning) => {
            reasoning.as_deref()
        }
        (SessionEvent::ToolResult { content, .. }, SourceField::ToolText) => Some(content),
        (
            SessionEvent::ToolResult {
                content_blocks: Some(blocks),
                ..
            },
            SourceField::ToolBlock,
        ) => match blocks.get(usize::from(block?))? {
            crate::ContentBlock::Text(text) => Some(text),
            crate::ContentBlock::Image { .. } => None,
        },
        _ => None,
    }
}

/// A published artifact, as far as a reference needs to know it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactFacts {
    /// Bytes the object retains.
    pub retained_bytes: u64,
    /// SHA-256 of those bytes.
    pub retained_sha256: Digest,
}

/// Resolves one reference against the bound session, below `below` events.
///
/// `artifact` looks a published artifact up by id; the host supplies it from the session's own
/// publication records, so a foreign or unpublished id resolves to nothing.
///
/// # Errors
///
/// Returns [`ErrorCode::InvalidReference`] when the reference does not resolve exactly.
pub fn resolve_source(
    log: &SessionLog,
    below: u64,
    source: &SourceRef,
    artifact: impl Fn(&super::ids::ArtifactId) -> Option<ArtifactFacts>,
) -> Result<(), ErrorCode> {
    source.validate_shape()?;
    let end = source
        .offset
        .checked_add(source.length)
        .ok_or(ErrorCode::InvalidReference)?;
    let (size, digest) = match source.kind {
        SourceKind::Event => {
            let seq = source.event_seq.ok_or(ErrorCode::InvalidReference)?;
            if seq >= below {
                return Err(ErrorCode::InvalidReference);
            }
            let text = event_text(log, seq, source.field, source.block_index)
                .ok_or(ErrorCode::InvalidReference)?;
            let size = u64::try_from(text.len()).map_err(|_| ErrorCode::InvalidReference)?;
            (size, Digest::of(text.as_bytes()))
        }
        SourceKind::Artifact => {
            let id = source
                .artifact_id
                .as_ref()
                .ok_or(ErrorCode::InvalidReference)?;
            let facts = artifact(id).ok_or(ErrorCode::InvalidReference)?;
            (facts.retained_bytes, facts.retained_sha256)
        }
    };
    if end > size || digest != source.source_digest {
        return Err(ErrorCode::InvalidReference);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::managed::ids::NoteId;
    use crate::context::managed::records::NoteCategory;

    fn log() -> SessionLog {
        let mut log = SessionLog::new();
        log.append(SessionEvent::UserMessage {
            text: "keep the API stable".to_owned(),
        });
        log
    }

    fn note(id: &str, claim: &str, source: SourceRef) -> WorkingNote {
        WorkingNote {
            id: NoteId::parse(id).unwrap_or_else(|| unreachable!("valid id")),
            claim: claim.to_owned(),
            category: NoteCategory::Observed,
            sources: vec![source],
        }
    }

    fn user_source(offset: u64, length: u64) -> SourceRef {
        SourceRef {
            kind: SourceKind::Event,
            event_seq: Some(0),
            block_index: None,
            artifact_id: None,
            offset,
            length,
            source_digest: Digest::of(b"keep the API stable"),
            field: SourceField::UserText,
        }
    }

    #[test]
    fn a_reference_resolves_only_to_the_exact_source_it_names() {
        let log = log();
        let none = |_: &super::super::ids::ArtifactId| None;
        assert!(resolve_source(&log, 1, &user_source(0, 19), none).is_ok());
        assert_eq!(
            resolve_source(&log, 1, &user_source(0, 20), none),
            Err(ErrorCode::InvalidReference),
            "a range past the end"
        );
        assert_eq!(
            resolve_source(&log, 0, &user_source(0, 4), none),
            Err(ErrorCode::InvalidReference),
            "an event outside the frontier"
        );
        let mut wrong = user_source(0, 4);
        wrong.source_digest = Digest::of(b"something else");
        assert_eq!(
            resolve_source(&log, 1, &wrong, none),
            Err(ErrorCode::InvalidReference)
        );
        let mut field = user_source(0, 4);
        field.field = SourceField::ToolText;
        assert_eq!(
            resolve_source(&log, 1, &field, none),
            Err(ErrorCode::InvalidReference)
        );
    }

    #[test]
    fn notes_are_bounded_by_count_characters_and_encoded_bytes() {
        let one = note("n:a", "the API must stay stable", user_source(0, 4));
        assert!(validate_notes(std::slice::from_ref(&one)).is_ok());
        let long = note("n:b", &"é".repeat(513), user_source(0, 4));
        assert_eq!(validate_notes(&[long]), Err(ErrorCode::InvalidReference));
        let at_limit = note("n:c", &"é".repeat(512), user_source(0, 4));
        assert!(validate_notes(&[at_limit]).is_ok());
        assert_eq!(
            validate_notes(&[one.clone(), one]),
            Err(ErrorCode::InvalidReference),
            "ids are unique"
        );
        // Escaping counts: quotes encode to two bytes each, so 32 notes of 512 quotes are far
        // past the 8 KiB array bound even though each claim is within its character bound.
        let quoted: Vec<WorkingNote> = (0..20)
            .map(|index| note(&format!("n:q{index}"), &"\"".repeat(400), user_source(0, 4)))
            .collect();
        assert_eq!(validate_notes(&quoted), Err(ErrorCode::StorageCapacity));
    }

    #[test]
    fn the_notes_digest_covers_the_notes_and_nothing_else() {
        let one = note("n:a", "claim", user_source(0, 4));
        let two = note("n:a", "another claim", user_source(0, 4));
        let same = note("n:a", "claim", user_source(0, 4));
        assert_eq!(
            notes_digest(std::slice::from_ref(&one)),
            notes_digest(&[same])
        );
        assert_ne!(notes_digest(&[one]), notes_digest(&[two]));
        assert_ne!(notes_digest(&[]), Digest::empty());
    }
}
