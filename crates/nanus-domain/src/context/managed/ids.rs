//! Identifiers, digests and the closed error vocabulary of managed context.
//!
//! Every identifier here is validated when it is constructed *and* when it is read back,
//! because each of them crosses a boundary the model can write to: a fragment id arrives in a
//! tool argument, an artifact id in a recall target, a digest in a note's source reference. A
//! value that parses is therefore a value of the documented shape, and nothing downstream has
//! to re-check the pattern.

use core::fmt;

use serde::{Deserialize, Serialize};

/// A lowercase hex BLAKE3 digest (the default 32-byte output).
///
/// A digest here detects inconsistency and stale input. It is not an authenticity claim: the
/// same operating-system user that runs the harness can rewrite a file and its digest together.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Digest(String);

impl Digest {
    /// Hashes `bytes`.
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        let mut hasher = Hasher::new();
        hasher.update(bytes);
        hasher.finish()
    }

    /// The digest of no bytes at all.
    #[must_use]
    pub fn empty() -> Self {
        Self::of(&[])
    }

    /// Parses a digest, refusing anything but 64 lowercase hex digits.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        let valid = raw.len() == 64
            && raw
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
        valid.then(|| Self(raw.to_owned()))
    }

    /// Returns the hex text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Digest({})", self.0)
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for Digest {
    type Error = String;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::parse(&raw).ok_or_else(|| String::from("a digest is 64 lowercase hex digits"))
    }
}

impl From<Digest> for String {
    fn from(digest: Digest) -> Self {
        digest.0
    }
}

/// An incremental BLAKE3, for digests over bytes that arrive in pieces.
///
/// It is `Clone`, and a clone continues from the same state: a digest over a growing prefix can be
/// carried forward and extended instead of recomputed from the first byte.
#[derive(Clone, Default)]
pub struct Hasher(blake3::Hasher);

impl fmt::Debug for Hasher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Hasher")
    }
}

impl Hasher {
    /// Starts an empty digest.
    #[must_use]
    pub fn new() -> Self {
        Self(blake3::Hasher::new())
    }

    /// Feeds more bytes.
    pub fn update(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }

    /// Finishes the digest.
    #[must_use]
    pub fn finish(self) -> Digest {
        self.digest()
    }

    /// The digest of everything fed so far, leaving the state to be fed more.
    #[must_use]
    pub fn digest(&self) -> Digest {
        let hex = self.0.finalize().to_hex();
        assert_eq!(hex.len(), 64, "a BLAKE3 digest is 64 hex digits");
        Digest(hex.as_str().to_owned())
    }
}

/// The id of one fragment: `f:<sequence of its assistant event>`.
///
/// A fragment is a derived view of the log, so its id is the position of the event it is
/// derived from rather than a counter of its own: two derivations of one log always agree.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct FragmentId(u64);

impl FragmentId {
    /// Builds the id of the fragment derived from the assistant event at `seq`.
    #[must_use]
    pub const fn new(seq: u64) -> Self {
        Self(seq)
    }

    /// Returns the sequence of the assistant event the fragment is derived from.
    #[must_use]
    pub const fn seq(self) -> u64 {
        self.0
    }

    /// Parses `f:<n>`, refusing leading zeros, signs and values past `u64`.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        let digits = raw.strip_prefix("f:")?;
        let canonical = !digits.is_empty()
            && digits.len() <= 20
            && digits.bytes().all(|byte| byte.is_ascii_digit())
            && (digits == "0" || !digits.starts_with('0'));
        if !canonical {
            return None;
        }
        digits.parse::<u64>().ok().map(Self)
    }
}

impl fmt::Debug for FragmentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "f:{}", self.0)
    }
}

impl fmt::Display for FragmentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "f:{}", self.0)
    }
}

impl TryFrom<String> for FragmentId {
    type Error = String;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::parse(&raw).ok_or_else(|| String::from("a fragment id is `f:<sequence>`"))
    }
}

impl From<FragmentId> for String {
    fn from(id: FragmentId) -> Self {
        id.to_string()
    }
}

/// The opaque id of one archived artifact: `a:<uuid>`.
///
/// Host-generated and never a path: the store maps it to a file under the session's own
/// directory, and model input can only name one it was shown.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ArtifactId(String);

impl ArtifactId {
    /// Parses `a:` followed by a lowercase hyphenated UUID.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        let uuid = raw.strip_prefix("a:")?;
        let groups: Vec<&str> = uuid.split('-').collect();
        let lengths = [8_usize, 4, 4, 4, 12];
        let valid = groups.len() == lengths.len()
            && groups.iter().zip(lengths).all(|(group, length)| {
                group.len() == length
                    && group
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            });
        valid.then(|| Self(raw.to_owned()))
    }

    /// Returns the id text, `a:` prefix included.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the UUID part, which is what a store names the object by.
    #[must_use]
    pub fn uuid(&self) -> &str {
        self.0.get(2..).unwrap_or_default()
    }
}

impl fmt::Debug for ArtifactId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Display for ArtifactId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for ArtifactId {
    type Error = String;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::parse(&raw).ok_or_else(|| String::from("an artifact id is `a:<uuid>`"))
    }
}

impl From<ArtifactId> for String {
    fn from(id: ArtifactId) -> Self {
        id.0
    }
}

/// The stable id of one working note: `n:` and one to 48 of `[A-Za-z0-9_-]`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct NoteId(String);

impl NoteId {
    /// Parses a note id.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        let body = raw.strip_prefix("n:")?;
        let valid = (1..=48).contains(&body.len())
            && body
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-');
        valid.then(|| Self(raw.to_owned()))
    }

    /// Returns the id text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for NoteId {
    type Error = String;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::parse(&raw).ok_or_else(|| String::from("a note id is `n:[A-Za-z0-9_-]{1,48}`"))
    }
}

impl From<NoteId> for String {
    fn from(id: NoteId) -> Self {
        id.0
    }
}

/// The stable failure codes of managed context.
///
/// Closed and fixed-schema on purpose: a failure record carries a code and bounded counts,
/// never an excerpt of the source or a vendor's payload, so a diagnostic cannot become a
/// channel that carries content past the protections around it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The mode, provider or host cannot support managed context.
    UnsupportedMode,
    /// The host policy refused the operation.
    PolicyDenied,
    /// The proposal's base no longer matches the accepted state.
    StaleBase,
    /// A fragment id names no eligible fragment.
    InvalidFragment,
    /// A fragment id names a protected fragment.
    ProtectedFragment,
    /// A note's source reference does not resolve.
    InvalidReference,
    /// The candidate request does not fit.
    CandidateTooLarge,
    /// The protected floor alone does not fit.
    ProtectedFloorTooLarge,
    /// The selected provider path cannot carry the projection.
    ProtocolIncompatible,
    /// A record or archive limit refused the change.
    StorageCapacity,
    /// The checkpoint was not written; the previous file is intact.
    CheckpointNotCommitted,
    /// Whether the checkpoint was written is not known.
    CheckpointUnknown,
    /// A recall cursor cannot be verified any more.
    CursorExpired,
    /// The source is not available.
    SourceUnavailable,
    /// The source did not verify.
    SourceCorrupt,
    /// The capture is partial.
    CapturePartial,
    /// The capture is unavailable.
    CaptureUnavailable,
    /// The operation was cancelled.
    Cancelled,
    /// A mutating proposal shared its batch with another call.
    MixedMutationBatch,
}

impl ErrorCode {
    /// Returns the code as it is written.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UnsupportedMode => "unsupported_mode",
            Self::PolicyDenied => "policy_denied",
            Self::StaleBase => "stale_base",
            Self::InvalidFragment => "invalid_fragment",
            Self::ProtectedFragment => "protected_fragment",
            Self::InvalidReference => "invalid_reference",
            Self::CandidateTooLarge => "candidate_too_large",
            Self::ProtectedFloorTooLarge => "protected_floor_too_large",
            Self::ProtocolIncompatible => "protocol_incompatible",
            Self::StorageCapacity => "storage_capacity",
            Self::CheckpointNotCommitted => "checkpoint_not_committed",
            Self::CheckpointUnknown => "checkpoint_unknown",
            Self::CursorExpired => "cursor_expired",
            Self::SourceUnavailable => "source_unavailable",
            Self::SourceCorrupt => "source_corrupt",
            Self::CapturePartial => "capture_partial",
            Self::CaptureUnavailable => "capture_unavailable",
            Self::Cancelled => "cancelled",
            Self::MixedMutationBatch => "mixed_mutation_batch",
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_digest_is_lowercase_hex_and_the_empty_digest_is_the_known_value() {
        assert_eq!(
            Digest::empty().as_str(),
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
        );
        assert!(Digest::parse(Digest::empty().as_str()).is_some());
        assert!(Digest::parse(&Digest::empty().as_str().to_uppercase()).is_none());
        assert!(Digest::parse("abc").is_none());
        let mut pieces = Hasher::new();
        pieces.update(b"ab");
        pieces.update(b"c");
        assert_eq!(pieces.finish(), Digest::of(b"abc"));
    }

    #[test]
    fn a_digest_taken_midway_does_not_end_the_hasher() {
        let mut running = Hasher::new();
        running.update(b"ab");
        assert_eq!(running.digest(), Digest::of(b"ab"));
        running.update(b"c");
        assert_eq!(running.digest(), Digest::of(b"abc"));
        assert_ne!(running.digest(), Digest::of(b"ab"));
    }

    #[test]
    fn a_fragment_id_is_canonical_and_bounded() {
        assert_eq!(FragmentId::parse("f:0"), Some(FragmentId::new(0)));
        assert_eq!(FragmentId::parse("f:42"), Some(FragmentId::new(42)));
        assert_eq!(
            FragmentId::parse("f:18446744073709551615"),
            Some(FragmentId::new(u64::MAX))
        );
        for bad in [
            "f:",
            "f:01",
            "f:-1",
            "f:+1",
            "g:1",
            "f:18446744073709551616",
            "f:1 ",
        ] {
            assert!(FragmentId::parse(bad).is_none(), "{bad}");
        }
        let encoded = serde_json::to_string(&FragmentId::new(7)).unwrap_or_default();
        assert_eq!(encoded, "\"f:7\"");
        assert!(serde_json::from_str::<FragmentId>("\"f:007\"").is_err());
    }

    #[test]
    fn an_artifact_id_is_a_uuid_and_never_a_path() {
        let good = "a:0190a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b";
        assert_eq!(ArtifactId::parse(good).map(|id| id.uuid().len()), Some(36));
        for bad in [
            "a:../../etc/passwd",
            "a:0190A1B2-c3d4-7e5f-8a9b-0c1d2e3f4a5b",
            "0190a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b",
            "a:0190a1b2c3d47e5f8a9b0c1d2e3f4a5b",
        ] {
            assert!(ArtifactId::parse(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn a_note_id_is_short_and_plain() {
        assert!(NoteId::parse("n:tests-pass_1").is_some());
        assert!(NoteId::parse("n:").is_none());
        assert!(NoteId::parse(&format!("n:{}", "x".repeat(49))).is_none());
        assert!(NoteId::parse("n:a/b").is_none());
    }

    #[test]
    fn every_error_code_writes_the_name_it_displays() {
        for code in [
            ErrorCode::StaleBase,
            ErrorCode::MixedMutationBatch,
            ErrorCode::Cancelled,
        ] {
            let encoded = serde_json::to_string(&code).unwrap_or_default();
            assert_eq!(encoded, format!("\"{code}\""));
        }
    }
}
