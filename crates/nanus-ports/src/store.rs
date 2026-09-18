//! The session store port.
//!
//! Storage is the one capability the harness cannot lose: a session that cannot
//! be written is a conversation that cannot be resumed, and the domain's log is
//! the only copy. The port therefore treats a *listing* as a first-class
//! operation rather than deriving it from loads, so a picker can render a
//! hundred sessions without reading a hundred files.
//!
//! [`StorePort::home`] is part of the port rather than a free function because
//! "where the harness keeps its state" is a platform decision — an XDG data
//! directory on Linux, an application-support directory on macOS — and the
//! adapters that know the answer must not be duplicated in every caller.

use std::path::PathBuf;

use nanus_domain::{Session, SessionId};
use serde::{Deserialize, Serialize};

use crate::LocalBoxFuture;

/// A shared, key-addressable session store.
pub type StoreHandle = std::rc::Rc<Box<dyn StorePort>>;

/// The session store port.
pub trait StorePort {
    /// Writes a session, replacing any stored copy.
    fn save<'a>(&'a self, session: &'a Session) -> LocalBoxFuture<'a, StoreResult<()>>;

    /// Reads a session by id.
    fn load<'a>(&'a self, id: &'a SessionId) -> LocalBoxFuture<'a, StoreResult<Session>>;

    /// Lists every stored session, newest first.
    ///
    /// The summaries are derived from the files, so listing a directory of
    /// sessions does not require loading any of them.
    fn list(&self) -> LocalBoxFuture<'_, StoreResult<Vec<SessionSummary>>>;

    /// Removes a session.
    ///
    /// Deleting a session that is not there is not an error: the caller asked
    /// for it to be gone, and it is. Any name it was known by goes with it, so a
    /// later session cannot inherit an alias for something that no longer exists.
    fn delete<'a>(&'a self, id: &'a SessionId) -> LocalBoxFuture<'a, StoreResult<()>>;

    /// Records `name` as another way to reach an existing session.
    ///
    /// A name is an alias for a store key, not part of the session: the domain's
    /// id is opaque and the store decides what a key looks like, and a
    /// human-typable key is the same kind of decision. So naming a session does
    /// not rewrite it, and a session that is renamed keeps its identity.
    ///
    /// Setting the name a session already has is not an error. Setting a name
    /// another session already holds is: silently moving an alias would make a
    /// script resume someone else's conversation.
    fn name<'a>(&'a self, id: &'a SessionId, name: &'a str) -> LocalBoxFuture<'a, StoreResult<()>>;

    /// Resolves a name to the session it aliases.
    ///
    /// `None` is not an error: a name that nobody has taken is the ordinary
    /// state of a name a user is about to choose.
    fn resolve<'a>(&'a self, name: &'a str) -> LocalBoxFuture<'a, StoreResult<Option<SessionId>>>;

    /// Returns the name a session answers to.
    ///
    /// The other direction of [`StorePort::resolve`], because both directions are
    /// asked: a session is *resumed* by name, and an agent that has just loaded a
    /// session by id needs to know what a person calls it.
    fn name_of<'a>(&'a self, id: &'a SessionId) -> LocalBoxFuture<'a, StoreResult<Option<String>>>;

    /// Returns the harness's home directory, creating it if needed.
    fn home(&self) -> LocalBoxFuture<'_, StoreResult<PathBuf>>;

    /// Claims `id` for this process to write.
    ///
    /// A session is one conversation with one log, and a store cannot merge two writers: the
    /// second save replaces the first, so a turn is lost by whichever agent wrote first. The
    /// claim is what turns that from a silent overwrite into a sentence naming the holder.
    ///
    /// A claim is held for as long as the holder has the session open — a turn's duration is
    /// not enough, because the loser's next turn would overwrite the winner's whole log from
    /// its own stale copy. *Attaching* to a session is not a claim: many clients watch one
    /// session, and one of them is what holds it.
    ///
    /// A claim is *advisory* in the sense that any process can ignore it by writing the log
    /// directly. What it defends against is another `nanus` — the ordinary way two writers
    /// meet — not a deliberate one.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Locked`] when another live process holds it. A claim whose holder
    /// is gone is taken over rather than honoured: a lock a crashed process left behind is not
    /// an owner, and honouring it would wedge a session for good.
    fn lock<'a>(&'a self, id: &'a SessionId, owner: &'a str)
    -> LocalBoxFuture<'a, StoreResult<()>>;

    /// Releases the claim this process holds on `id`.
    ///
    /// Synchronous, unlike everything else here, because its caller is a `Drop`: a claim that
    /// outlived the thing that held it would refuse the next writer for as long as this process
    /// runs, and a release that cannot happen at the instant the holder goes is a lock that
    /// leaks. Removing one small file is not worth a future to await.
    ///
    /// Releasing a claim this process does not hold does nothing. A claim taken over from a
    /// crashed process belongs to whoever took it over, and one held by another live process is
    /// not ours to remove.
    fn release_lock(&self, id: &SessionId);
}

/// The result type of every store operation.
pub type StoreResult<T> = Result<T, StoreError>;

/// What a listing shows about one session.
///
/// This is a projection, not a session: it carries exactly what a picker
/// renders, so listing never loads a log.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    /// The session's store key.
    pub id: SessionId,
    /// When it was created, in milliseconds since the Unix epoch.
    pub created_at_ms: u64,
    /// The working directory it ran in.
    pub cwd: String,
    /// When its last event was recorded, in milliseconds since the Unix epoch.
    ///
    /// Supplied by the store from the file's modification time, because the
    /// domain's event log deliberately carries no per-event clock: a timestamp
    /// on every event would be a second source of truth for ordering.
    pub last_event_at_ms: u64,
    /// How many events it holds.
    pub event_count: u64,
    /// A title derived from its first human turn.
    pub title: Option<String>,
    /// The name a user gave it, if any.
    ///
    /// Distinct from the title, which is derived from what was said: a title is
    /// what a session is *about*, and a name is what a person calls it.
    pub name: Option<String>,
}

/// Why a store operation failed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum StoreError {
    /// No session is stored under the id.
    #[error("no session {id}")]
    NotFound {
        /// The id that was requested.
        id: String,
    },

    /// A session with that id is already stored.
    #[error("session {id} already exists")]
    AlreadyExists {
        /// The contested id.
        id: String,
    },

    /// The stored bytes are not a session this crate can read.
    #[error("session {id} could not be read: {message}")]
    Corrupt {
        /// The id whose contents failed to decode.
        id: String,
        /// The rendered failure.
        message: String,
    },

    /// A name that is not usable as an alias.
    #[error("{name:?} is not a usable session name: {reason}")]
    InvalidName {
        /// The name that was rejected.
        name: String,
        /// Why it was rejected.
        reason: String,
    },

    /// Another session already answers to that name.
    #[error("the name {name:?} already belongs to session {id}")]
    NameTaken {
        /// The contested name.
        name: String,
        /// The session that already holds it.
        id: String,
    },

    /// Another live process holds the session for writing.
    ///
    /// Deliberately not merged with [`StoreError::Io`]: this is a *decision* the store made,
    /// and the caller's answer to it is to wait, to stop the other agent, or to attach to it —
    /// not to retry the same write.
    #[error("session {id} is being written by {owner} (pid {pid})")]
    Locked {
        /// The contested session.
        id: String,
        /// What the holder calls itself.
        owner: String,
        /// The holding process.
        pid: u32,
    },

    /// The harness home could not be determined or created.
    #[error("the harness home is unavailable: {message}")]
    HomeUnavailable {
        /// The rendered failure.
        message: String,
    },

    /// Any other input/output failure.
    #[error("I/O failure for {path}: {message}")]
    Io {
        /// The path involved.
        path: PathBuf,
        /// The rendered failure.
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_summary_is_a_projection_and_not_a_session() {
        let summary = SessionSummary {
            id: SessionId::new("s-1"),
            created_at_ms: 10,
            cwd: "/work".to_owned(),
            last_event_at_ms: 20,
            event_count: 7,
            title: Some("first turn".to_owned()),
            name: Some("glob-bug".to_owned()),
        };
        let encoded = serde_json::to_value(&summary);
        assert!(encoded.is_ok());
        let Ok(encoded) = encoded else { return };
        let Some(object) = encoded.as_object() else {
            panic!("a summary is a JSON object");
        };
        // No event log is present, which is what makes listing cheap.
        assert!(!object.contains_key("log"));
        assert!(!object.contains_key("events"));
        assert_eq!(object.get("event_count"), Some(&serde_json::json!(7)));
        assert_eq!(object.get("id"), Some(&serde_json::json!("s-1")));
    }

    #[test]
    fn a_summary_round_trips() {
        let summary = SessionSummary {
            id: SessionId::new("s-2"),
            created_at_ms: 1,
            cwd: "/w".to_owned(),
            last_event_at_ms: 2,
            event_count: 0,
            title: None,
            name: Some("nightly".to_owned()),
        };
        let encoded = serde_json::to_string(&summary).unwrap_or_default();
        let decoded: Result<SessionSummary, _> = serde_json::from_str(&encoded);
        assert!(decoded.is_ok());
        assert_eq!(decoded.ok(), Some(summary));
    }

    #[test]
    fn a_store_failure_names_what_it_could_not_do() {
        let missing = StoreError::NotFound {
            id: "s-9".to_owned(),
        };
        assert!(missing.to_string().contains("s-9"));
        let corrupt = StoreError::Corrupt {
            id: "s-9".to_owned(),
            message: "unexpected end of input".to_owned(),
        };
        assert!(corrupt.to_string().contains("unexpected end of input"));
    }
}
