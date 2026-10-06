//! What has been encoded of a session so far, carried forward as its log grows.
//!
//! A managed turn checkpoints several times, and every checkpoint names the digest of the whole
//! file it leaves behind: the header line and every event line. Recomputing that from the first
//! event made each checkpoint cost what the *conversation* had grown to rather than what the
//! step had added, and a turn pays it five times. The log is append-only, so an event line that
//! has been encoded is never encoded differently: this keeps the digests' running states and the
//! validated length beside the session, and a later question encodes only the events appended
//! since the last one.
//!
//! The cache is an optimisation and never an authority. It answers only for the header it was
//! started from — a different header line (the body upgrade, a new origin) starts it over — and
//! only for a count at or past where it stands; anything else is answered the slow way, from the
//! first event, exactly as before. A clone carries it, which is what lets a checkpoint candidate,
//! cloned from the session and adopted when it commits, hand its progress on to the next one.

use std::sync::{Mutex, PoisonError};

use super::{Session, SessionError, SessionEvent, SessionLineRef, SessionSeq, encode};
use crate::content::{RECORD_BYTES_MAX, SESSION_BYTES_MAX};
use crate::context::managed::ids::{Digest, Hasher};

/// The running encoding of a session's first events, shared by its clones' starting points.
#[derive(Default)]
pub(super) struct Encoding(Mutex<Progress>);

impl Clone for Encoding {
    fn clone(&self) -> Self {
        Self(Mutex::new(self.lock().clone()))
    }
}

impl PartialEq for Encoding {
    /// A cache says nothing about what a session *is*, so two sessions never differ by one.
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl core::fmt::Debug for Encoding {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Encoding")
            .field("count", &self.lock().count)
            .finish_non_exhaustive()
    }
}

impl Encoding {
    fn lock(&self) -> std::sync::MutexGuard<'_, Progress> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// How far the encoding has got, and the digests' state there.
#[derive(Clone, Default)]
struct Progress {
    /// The header line (without its LF) the states were started from; empty before the first.
    header: String,
    /// Events encoded, validated and fed so far.
    count: usize,
    /// Bytes of those event lines, each with its LF.
    body_bytes: usize,
    /// BLAKE3 of the header line, its LF, and the event lines so far.
    prefix: Hasher,
    /// BLAKE3 of the event lines so far.
    body: Hasher,
    /// The last prefix digests asked for, by count, newest last: a checkpoint's receipt names
    /// the prefix the next checkpoint must extend, and by then the cache has usually moved on.
    marks: Vec<(usize, Digest)>,
}

/// How many prefix digests the cache keeps behind where it stands.
const MARKS_MAX: usize = 4;

impl Progress {
    /// Starts over from `header`, fed as the file's first line.
    fn start(header: String) -> Self {
        let mut prefix = Hasher::new();
        prefix.update(header.as_bytes());
        prefix.update(b"\n");
        Self {
            header,
            prefix,
            ..Self::default()
        }
    }
}

impl Session {
    /// Brings the cache to `count` events and reads it there, or `None` when it already stands
    /// past `count` and cannot be wound back. Lines encoded on the way are appended to `sink`.
    ///
    /// A refusal leaves the cache at the last event that encoded, so a later question resumes
    /// rather than repeats — and repeats the refusal, because the event that caused it is still
    /// next.
    fn advance<R>(
        &self,
        count: usize,
        sink: Option<&mut String>,
        read: impl FnOnce(&Hasher, &Hasher) -> R,
    ) -> Result<Option<R>, SessionError> {
        assert!(count <= self.log.len(), "a cache never runs past the log");
        let mut progress = self.encoding.lock();
        let header = encode(&self.header());
        if progress.header != header {
            if header.len() > RECORD_BYTES_MAX {
                return Err(SessionError::BadHeader {
                    line: 1,
                    reason: String::from("serialized content exceeds byte limit"),
                });
            }
            *progress = Progress::start(header);
        }
        if count < progress.count {
            return Ok(None);
        }
        let header_bytes = progress.header.len().saturating_add(1);
        let batch = self.encode_batch(progress.count, count, progress.body_bytes, header_bytes);
        // Fed as one input rather than line by line: BLAKE3 hashes whole chunks in parallel only
        // when one update spans them, and a line is usually shorter than a chunk.
        progress.prefix.update(batch.lines.as_bytes());
        progress.body.update(batch.lines.as_bytes());
        if let Some(sink) = sink {
            sink.push_str(&batch.lines);
        }
        progress.body_bytes = batch.body_bytes;
        progress.count = batch.reached;
        if let Some(refusal) = batch.refusal {
            return Err(refusal);
        }
        // Postcondition: the cache stands exactly where it was asked to.
        assert_eq!(
            progress.count, count,
            "the cache reached the count asked for"
        );
        Ok(Some(read(&progress.prefix, &progress.body)))
    }

    /// Encodes the event lines from `from` up to `count`, stopping at the first refusal.
    fn encode_batch(
        &self,
        from: usize,
        count: usize,
        body_bytes: usize,
        header_bytes: usize,
    ) -> Batch {
        let mut batch = Batch {
            lines: String::new(),
            reached: from,
            body_bytes,
            refusal: None,
        };
        while batch.reached < count {
            let index = batch.reached;
            let encoded = self
                .log
                .events()
                .get(index)
                .ok_or_else(|| past_end(self, count))
                .and_then(|event| self.encode_line(index, event));
            let line = match encoded {
                Ok(line) => line,
                Err(refusal) => {
                    batch.refusal = Some(refusal);
                    break;
                }
            };
            let Some(total) = batch
                .body_bytes
                .checked_add(line.len())
                .and_then(|bytes| bytes.checked_add(1))
                .filter(|bytes| bytes.saturating_add(header_bytes) <= SESSION_BYTES_MAX)
            else {
                batch.refusal = Some(too_large());
                break;
            };
            batch.lines.push_str(&line);
            batch.lines.push('\n');
            batch.body_bytes = total;
            batch.reached = index.saturating_add(1);
        }
        batch
    }

    /// Validates and encodes one event line, without its LF.
    fn encode_line(&self, index: usize, event: &SessionEvent) -> Result<String, SessionError> {
        self.check_event(index, event)?;
        let seq = SessionSeq::new(u64::try_from(index).unwrap_or(u64::MAX));
        let line = encode(&SessionLineRef { seq, event });
        assert!(!line.is_empty(), "a session event encodes");
        if line.len() > RECORD_BYTES_MAX {
            return Err(SessionError::MalformedEvent {
                line: seq.value().saturating_add(2),
                detail: String::from("serialized content exceeds byte limit"),
            });
        }
        Ok(line)
    }

    /// The prefix digest from the cache, when it can answer for `count`: where it stands or
    /// ahead of it, or at a count it was asked about before and still remembers.
    pub(super) fn cached_prefix_digest(&self, count: usize) -> Option<Digest> {
        let digest = match self.advance(count, None, |prefix, _| prefix.digest()) {
            Ok(Some(digest)) => digest,
            Ok(None) => return self.marked(count),
            Err(_) => return None,
        };
        self.mark(count, &digest);
        Some(digest)
    }

    /// Remembers the prefix digest at `count`, forgetting the oldest beyond [`MARKS_MAX`].
    fn mark(&self, count: usize, digest: &Digest) {
        let mut progress = self.encoding.lock();
        progress.marks.retain(|(marked, _)| *marked != count);
        if progress.marks.len() >= MARKS_MAX {
            progress.marks.remove(0);
        }
        progress.marks.push((count, digest.clone()));
        let kept = progress.marks.len();
        drop(progress);
        assert!(kept <= MARKS_MAX, "the marks stay bounded");
    }

    /// A prefix digest the cache was asked for before, under the current header.
    fn marked(&self, count: usize) -> Option<Digest> {
        let progress = self.encoding.lock();
        if progress.header != encode(&self.header()) {
            return None;
        }
        progress
            .marks
            .iter()
            .find(|(marked, _)| *marked == count)
            .map(|(_, digest)| digest.clone())
    }

    /// The body digest from the cache, when it can answer for the whole log.
    pub(super) fn cached_body_digest(&self) -> Option<Digest> {
        self.advance(self.log.len(), None, |_, body| body.digest())
            .ok()
            .flatten()
    }

    /// Returns the event lines after the first `from`, each with its LF: exactly the bytes
    /// [`Session::try_to_jsonl`] writes after its first `from + 1` lines.
    ///
    /// What lets a store that already holds the first `from` events add the rest without
    /// re-encoding what it holds. Every line is validated as [`Session::try_to_jsonl`] validates
    /// it, and the whole session, header included, is held to the same 64 MiB bound.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::NonContiguousSequence`] for a `from` past the log's end, and the
    /// refusals [`Session::try_to_jsonl`] makes for any event after it.
    pub fn encoded_lines_from(&self, from: u64) -> Result<String, SessionError> {
        let len = self.log.len();
        let start = usize::try_from(from)
            .ok()
            .filter(|start| *start <= len)
            .ok_or_else(|| past_end(self, len.saturating_add(1)))?;
        let mut tail = String::new();
        let resumed = self.advance(start, None, |_, _| ())?.is_some()
            && self.advance(len, Some(&mut tail), |_, _| ())?.is_some();
        if !resumed {
            // The cache stands past `start`: validate the whole session and encode the tail.
            tail.clear();
            self.checked_len()?;
            for (index, event) in self.log.events().iter().enumerate().skip(start) {
                tail.push_str(&self.encode_line(index, event)?);
                tail.push('\n');
            }
        }
        Ok(tail)
    }
}

/// Event lines encoded in one advance, and where it stopped.
struct Batch {
    /// The lines, each with its LF.
    lines: String,
    /// The count reached: every line before it is in `lines` or was already encoded.
    reached: usize,
    /// Bytes of every event line up to `reached`.
    body_bytes: usize,
    /// Why it stopped short of the count asked for, if it did.
    refusal: Option<SessionError>,
}

/// The refusal for a count past the log's end.
fn past_end(session: &Session, found: usize) -> SessionError {
    SessionError::NonContiguousSequence {
        line: 1,
        expected: u64::try_from(session.log.len()).unwrap_or(u64::MAX),
        found: u64::try_from(found).unwrap_or(u64::MAX),
    }
}

/// The refusal for a session past the 64 MiB bound, worded as [`Session::try_to_jsonl`] words it.
fn too_large() -> SessionError {
    SessionError::BadHeader {
        line: 1,
        reason: "session exceeds 64 MiB".into(),
    }
}
