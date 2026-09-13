//! The JSONL session store: an implementation of [`nanus_ports::StorePort`].
//!
//! ## Layout
//!
//! ```text
//! <home>/sessions/<encoded-session-id>/session.jsonl
//! <home>/sessions/<encoded-session-id>/name           (optional)
//! ```
//!
//! `<home>` is `$NANUS_HOME` when set, and `<config dir>/nanus` otherwise.
//!
//! ## Why a name is a file beside the session rather than a field in it
//!
//! A name is an alias for a store key, and the domain says the store decides what
//! a key looks like. Keeping it in the session directory means naming never
//! rewrites a log, a renamed session keeps its identity, deleting a session takes
//! its name with it, and there is no shared table for two writers to lose. The
//! cost is that resolving a name reads a directory, which is the same walk
//! [`StorePort::list`] already does.
//!
//! ## Why the directory name is encoded
//!
//! A session id is store-opaque text (the domain does not validate it), so an id
//! taken from a file could contain `/`, `..`, or a NUL. Every byte outside
//! `[A-Za-z0-9._-]` is percent-hex-escaped, and `.`/`..` are rejected outright, so
//! an id can never name a path with more than one component.
//!
//! ## Why writes are atomic
//!
//! A save writes a sibling temp file, fsyncs it, and renames it over the real
//! name. A reader therefore sees either the previous complete log or the new
//! complete log, never a half-written one.
//!
//! ## What listing costs
//!
//! [`nanus_ports::SessionSummary`] carries an event count and a title, which are
//! *not* in the session header, so a listing must read each log. The header is
//! still parsed strictly and the body only leniently: a session whose body is
//! damaged still lists with correct identity fields, and one unreadable session
//! never hides the others.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use etcetera::BaseStrategy as _;
use nanus_domain::{SESSION_FORMAT_TAG, SESSION_FORMAT_VERSION, Session, SessionError, SessionId};
use nanus_ports::{LocalBoxFuture, SessionSummary, StoreError, StorePort, StoreResult};
use serde_json::Value;
use tokio::fs;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

/// The environment variable that overrides the nanus home.
pub const HOME_ENV: &str = "NANUS_HOME";

/// The directory, under the home, that holds every session.
const SESSIONS_DIR: &str = "sessions";

/// The file name of one session's log.
const SESSION_FILE: &str = "session.jsonl";

/// The file, beside a session's log, that holds the name a user gave it.
///
/// A fixed file name inside the session's own directory, so a name is *content*
/// and never a path component: no name can escape the directory it lives in.
const NAME_FILE: &str = "name";

/// The longest session name accepted.
///
/// Not a security boundary — a name is content, not a path — but a bound, because
/// a name is typed by a person and printed in a listing.
const MAX_NAME_CHARS: usize = 128;

/// How many characters of the first human turn become a session title.
const TITLE_MAX_CHARS: usize = 72;

/// Generates a new time-ordered session id.
///
/// UUID v7 rather than v4 because v7 embeds a millisecond timestamp, so ids sort
/// by creation order; and rather than a ULID because `Ulid::generate` explicitly
/// does not guarantee monotonic order within a millisecond.
#[must_use]
pub fn new_session_id() -> SessionId {
    let id = uuid::Uuid::now_v7().to_string();
    assert!(!id.is_empty(), "a generated uuid has a textual form");
    SessionId::new(id)
}

/// Resolves the nanus home directory.
///
/// # Errors
///
/// Returns [`StoreError::HomeUnavailable`] when the platform lookup fails.
pub fn resolve_home(explicit: Option<&Path>) -> StoreResult<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path.to_path_buf());
    }
    if let Some(raw) = std::env::var_os(HOME_ENV)
        && !raw.is_empty()
    {
        return Ok(PathBuf::from(raw));
    }
    let strategy =
        etcetera::choose_base_strategy().map_err(|error| StoreError::HomeUnavailable {
            message: error.to_string(),
        })?;
    Ok(strategy.config_dir().join("nanus"))
}

/// A session store rooted at a nanus home.
#[derive(Debug, Clone)]
pub struct JsonlStore {
    /// The nanus home every session lives under.
    home: PathBuf,
}

impl JsonlStore {
    /// Opens a store at `home`, creating `<home>/sessions`.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Io`] when the directory cannot be created.
    pub async fn new(home: impl Into<PathBuf>) -> StoreResult<Self> {
        let home = home.into();
        assert!(!home.as_os_str().is_empty(), "a store home is named");
        let store = Self { home };
        let root = store.sessions_root();
        fs::create_dir_all(&root)
            .await
            .map_err(|source| io_error(&root, &source))?;
        Ok(store)
    }

    /// Opens the store named by `$NANUS_HOME`, or the platform default.
    ///
    /// # Errors
    ///
    /// As [`JsonlStore::new`], plus [`StoreError::HomeUnavailable`].
    pub async fn from_env() -> StoreResult<Self> {
        Self::new(resolve_home(None)?).await
    }

    /// Shares this store as the handle a kernel plugin publishes.
    #[must_use]
    pub fn handle(self) -> nanus_ports::StoreHandle {
        std::rc::Rc::new(Box::new(self))
    }

    /// Returns the nanus home.
    #[must_use]
    pub fn home_path(&self) -> &Path {
        &self.home
    }

    /// Returns the directory that holds every session.
    #[must_use]
    pub fn sessions_root(&self) -> PathBuf {
        self.home.join(SESSIONS_DIR)
    }

    /// Returns the directory for one session, rejecting an id that cannot name one.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Io`] for an id that is empty, is `.`/`..`, or does
    /// not encode to a single path component.
    pub fn session_dir(&self, id: &SessionId) -> StoreResult<PathBuf> {
        let encoded = encode_id(id)?;
        let root = self.sessions_root();
        let dir = root.join(encoded);
        // Postcondition: the encoded id added exactly one component and stayed put.
        if dir.parent() != Some(root.as_path()) {
            return Err(StoreError::Io {
                path: dir,
                message: format!("the id {:?} does not name a single directory", id.as_str()),
            });
        }
        Ok(dir)
    }

    /// Returns the log path for one session.
    ///
    /// # Errors
    ///
    /// As [`JsonlStore::session_dir`].
    pub fn session_file(&self, id: &SessionId) -> StoreResult<PathBuf> {
        Ok(self.session_dir(id)?.join(SESSION_FILE))
    }

    /// Returns the path of the file that holds one session's name.
    ///
    /// # Errors
    ///
    /// As [`JsonlStore::session_dir`].
    pub fn name_file(&self, id: &SessionId) -> StoreResult<PathBuf> {
        Ok(self.session_dir(id)?.join(NAME_FILE))
    }

    /// Writes `session` atomically, replacing any existing log for its id.
    async fn save_blocking(&self, session: &Session) -> StoreResult<()> {
        let dir = self.session_dir(session.id())?;
        fs::create_dir_all(&dir)
            .await
            .map_err(|source| io_error(&dir, &source))?;
        let path = dir.join(SESSION_FILE);
        let body = session.to_jsonl();
        assert!(!body.is_empty(), "an encoded session is never empty");
        write_atomic(&path, &body).await
    }

    /// Reads one session, rejecting a damaged file.
    async fn load_blocking(&self, id: &SessionId) -> StoreResult<Session> {
        let path = self.session_file(id)?;
        let raw = match fs::read_to_string(&path).await {
            Ok(raw) => raw,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                return Err(not_found(id));
            }
            Err(source) => return Err(io_error(&path, &source)),
        };
        Session::from_jsonl(&raw).map_err(|error| corrupt(id, &error))
    }

    /// Removes a session directory.
    ///
    /// A session directory that is a symlink is unlinked, never followed, so a
    /// link planted under the home cannot make this delete an outside tree.
    async fn delete_blocking(&self, id: &SessionId) -> StoreResult<()> {
        let dir = self.session_dir(id)?;
        let metadata = match fs::symlink_metadata(&dir).await {
            Ok(metadata) => metadata,
            // The port is explicit: deleting something absent is not an error,
            // because the caller asked for it to be gone and it is.
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(source) => return Err(io_error(&dir, &source)),
        };
        if metadata.file_type().is_symlink() {
            return fs::remove_file(&dir)
                .await
                .map_err(|source| io_error(&dir, &source));
        }
        fs::remove_dir_all(&dir)
            .await
            .map_err(|source| io_error(&dir, &source))
    }

    /// Records `name` for `id`, refusing a name another session already holds.
    async fn name_blocking(&self, id: &SessionId, name: &str) -> StoreResult<()> {
        validate_name(name)?;
        // The session has to exist before it can be named. An alias for a session
        // that is not there is a promise this store cannot keep, and the caller
        // that made it would rather hear about it now.
        let log = self.session_file(id)?;
        if !fs::try_exists(&log).await.unwrap_or(false) {
            return Err(not_found(id));
        }
        for (other, held) in self.session_names().await? {
            if other != *id && held == name {
                return Err(StoreError::NameTaken {
                    name: name.to_owned(),
                    id: other.as_str().to_owned(),
                });
            }
        }
        // Written atomically and last, so a failure leaves the previous name — or
        // no name — rather than half of one.
        write_atomic(&self.name_file(id)?, &format!("{name}\n")).await
    }

    /// Resolves a name to the session that answers to it.
    async fn resolve_blocking(&self, name: &str) -> StoreResult<Option<SessionId>> {
        if validate_name(name).is_err() {
            // A name that could never have been recorded is held by nobody, so the
            // answer is "no session" rather than an error: the question has a true
            // answer, and a caller checking whether a name is free wants it.
            return Ok(None);
        }
        // Lowest id wins when two directories somehow hold one name. The store
        // never writes two, and a store edited by hand should still answer the
        // same way twice.
        Ok(self
            .session_names()
            .await?
            .into_iter()
            .filter(|(_, held)| held == name)
            .map(|(id, _)| id)
            .min())
    }

    /// Returns the name one session answers to.
    async fn name_of_blocking(&self, id: &SessionId) -> StoreResult<Option<String>> {
        Ok(self.session_names().await?.remove(id))
    }

    /// Reads every session's name, in one walk of the store.
    async fn session_names(&self) -> StoreResult<BTreeMap<SessionId, String>> {
        let root = self.sessions_root();
        let mut entries = match fs::read_dir(&root).await {
            Ok(entries) => entries,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                return Ok(BTreeMap::new());
            }
            Err(source) => return Err(io_error(&root, &source)),
        };
        let mut names = BTreeMap::new();
        loop {
            let entry = match entries.next_entry().await {
                Ok(Some(entry)) => entry,
                Ok(None) => break,
                Err(source) => {
                    tracing::warn!(%source, "stopped reading session names after a read error");
                    break;
                }
            };
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            let Some(encoded) = dir.file_name().and_then(OsStr::to_str) else {
                continue;
            };
            let Some(id) = decode_id(encoded) else {
                continue;
            };
            if let Some(name) = read_name(&dir).await {
                names.insert(id, name);
            }
        }
        Ok(names)
    }

    /// Lists every session, newest first.
    ///
    /// A session whose header is unreadable is skipped with a warning rather than
    /// failing the listing: one bad directory must not hide every good one.
    async fn list_blocking(&self) -> StoreResult<Vec<SessionSummary>> {
        let root = self.sessions_root();
        let mut entries = match fs::read_dir(&root).await {
            Ok(entries) => entries,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => return Err(io_error(&root, &source)),
        };
        let mut summaries: Vec<SessionSummary> = Vec::new();
        loop {
            let entry = match entries.next_entry().await {
                Ok(Some(entry)) => entry,
                Ok(None) => break,
                Err(source) => {
                    tracing::warn!(%source, "stopped listing sessions after a read error");
                    break;
                }
            };
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            let Some(encoded) = dir.file_name().and_then(OsStr::to_str) else {
                continue;
            };
            let Some(id) = decode_id(encoded) else {
                tracing::warn!(dir = %dir.display(), "skipping a session with an undecodable name");
                continue;
            };
            match summarize(&dir, &id).await {
                Ok(summary) => summaries.push(summary),
                Err(error) => tracing::warn!(%error, "skipping an unreadable session"),
            }
        }
        summaries.sort_by(|left, right| {
            right
                .created_at_ms
                .cmp(&left.created_at_ms)
                .then_with(|| right.id.cmp(&left.id))
        });
        Ok(summaries)
    }
}

impl StorePort for JsonlStore {
    fn save<'a>(&'a self, session: &'a Session) -> LocalBoxFuture<'a, StoreResult<()>> {
        Box::pin(async move { self.save_blocking(session).await })
    }

    fn load<'a>(&'a self, id: &'a SessionId) -> LocalBoxFuture<'a, StoreResult<Session>> {
        Box::pin(async move { self.load_blocking(id).await })
    }

    fn list(&self) -> LocalBoxFuture<'_, StoreResult<Vec<SessionSummary>>> {
        Box::pin(async move { self.list_blocking().await })
    }

    fn delete<'a>(&'a self, id: &'a SessionId) -> LocalBoxFuture<'a, StoreResult<()>> {
        Box::pin(async move { self.delete_blocking(id).await })
    }

    fn name<'a>(&'a self, id: &'a SessionId, name: &'a str) -> LocalBoxFuture<'a, StoreResult<()>> {
        Box::pin(async move { self.name_blocking(id, name).await })
    }

    fn resolve<'a>(&'a self, name: &'a str) -> LocalBoxFuture<'a, StoreResult<Option<SessionId>>> {
        Box::pin(async move { self.resolve_blocking(name).await })
    }

    fn name_of<'a>(&'a self, id: &'a SessionId) -> LocalBoxFuture<'a, StoreResult<Option<String>>> {
        Box::pin(async move { self.name_of_blocking(id).await })
    }

    fn home(&self) -> LocalBoxFuture<'_, StoreResult<PathBuf>> {
        Box::pin(async move {
            fs::create_dir_all(&self.home)
                .await
                .map_err(|source| io_error(&self.home, &source))?;
            Ok(self.home.clone())
        })
    }
}

/// Builds the port's not-found error for `id`.
fn not_found(id: &SessionId) -> StoreError {
    StoreError::NotFound {
        id: id.as_str().to_owned(),
    }
}

/// Translates a domain session error into the port's corrupt error.
///
/// The port has a single `Corrupt` variant, so the *kind* of damage is carried in
/// the message: a truncated tail, a version from the future, and a hole in the
/// sequence are all corrupt, and a caller reading the message can tell which.
fn corrupt(id: &SessionId, error: &SessionError) -> StoreError {
    let detail = match error {
        SessionError::MissingHeader => String::from("the log has no header line"),
        SessionError::BadHeader { line, reason } => {
            format!("the header on line {line} is invalid: {reason}")
        }
        SessionError::UnsupportedVersion { found, expected } => format!(
            "format version {found} is newer than this build's {expected}; refusing to misread it"
        ),
        SessionError::MalformedEvent { line, detail } => {
            format!("line {line} is malformed, which is what a truncated tail looks like: {detail}")
        }
        SessionError::NonContiguousSequence {
            line,
            expected,
            found,
        } => format!("line {line} has sequence {found} where {expected} was expected"),
        other => other.to_string(),
    };
    StoreError::Corrupt {
        id: id.as_str().to_owned(),
        message: detail,
    }
}

/// Reads a session file leniently, producing the summary a picker renders.
///
/// The header is parsed strictly — a bad header is an error — while the body is
/// parsed only for the two fields the summary needs, and a damaged body degrades
/// those fields rather than failing the listing.
async fn summarize(dir: &Path, id: &SessionId) -> StoreResult<SessionSummary> {
    let path = dir.join(SESSION_FILE);
    let metadata = fs::metadata(&path)
        .await
        .map_err(|source| io_error(&path, &source))?;
    let last_event_at_ms = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        });
    let file = fs::File::open(&path)
        .await
        .map_err(|source| io_error(&path, &source))?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let mut number: u64 = 0;
    let mut header: Option<Value> = None;
    let mut event_count: u64 = 0;
    let mut title: Option<String> = None;
    loop {
        line.clear();
        let read = reader
            .read_line(&mut line)
            .await
            .map_err(|source| io_error(&path, &source))?;
        if read == 0 {
            break;
        }
        number = number.saturating_add(1);
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if header.is_none() {
            header = Some(parse_header(trimmed, id, number)?);
            continue;
        }
        event_count = event_count.saturating_add(1);
        if title.is_none()
            && let Some(found) = title_of(trimmed)
        {
            title = Some(found);
        }
    }
    let header = header.ok_or_else(|| StoreError::Corrupt {
        id: id.as_str().to_owned(),
        message: String::from("the log has no header line"),
    })?;
    let created_at_ms = header
        .get("created_at_ms")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cwd = header
        .get("cwd")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    Ok(SessionSummary {
        id: id.clone(),
        created_at_ms,
        cwd,
        last_event_at_ms,
        event_count,
        title,
        name: read_name(dir).await,
    })
}

/// Reads one session's name, if it has one.
///
/// Best-effort by design: an unreadable or unusable name file costs a naming
/// convenience and never the session, so the failure is reported and the session
/// is listed as unnamed rather than hidden.
async fn read_name(dir: &Path) -> Option<String> {
    let path = dir.join(NAME_FILE);
    match fs::read_to_string(&path).await {
        Ok(raw) => {
            let trimmed = raw.trim_end_matches(['\n', '\r']);
            if validate_name(trimmed).is_ok() {
                return Some(trimmed.to_owned());
            }
            tracing::warn!(path = %path.display(), "ignoring a session name that is not usable");
            None
        }
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => None,
        Err(source) => {
            tracing::warn!(%source, path = %path.display(), "could not read a session name");
            None
        }
    }
}

/// Checks that `name` can be an alias for a session.
fn validate_name(name: &str) -> StoreResult<()> {
    let reject = |reason: &str| {
        Err(StoreError::InvalidName {
            name: name.to_owned(),
            reason: reason.to_owned(),
        })
    };
    if name.trim().is_empty() {
        return reject("it is empty");
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return reject("it is longer than 128 characters");
    }
    if name.chars().any(char::is_control) {
        return reject("it contains a control character");
    }
    Ok(())
}

/// Parses and checks one header line.
fn parse_header(line: &str, id: &SessionId, number: u64) -> StoreResult<Value> {
    let header: Value = serde_json::from_str(line).map_err(|error| StoreError::Corrupt {
        id: id.as_str().to_owned(),
        message: format!("the header on line {number} is not JSON: {error}"),
    })?;
    let format = header
        .get("format")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if format != SESSION_FORMAT_TAG {
        return Err(StoreError::Corrupt {
            id: id.as_str().to_owned(),
            message: format!("expected format {SESSION_FORMAT_TAG:?}, found {format:?}"),
        });
    }
    let version = header.get("version").and_then(Value::as_u64).unwrap_or(0);
    let version = u32::try_from(version).unwrap_or(u32::MAX);
    if version != SESSION_FORMAT_VERSION {
        return Err(StoreError::Corrupt {
            id: id.as_str().to_owned(),
            message: format!(
                "format version {version} is newer than this build's {SESSION_FORMAT_VERSION}"
            ),
        });
    }
    Ok(header)
}

/// Derives a title from one body line, when it is the first human turn.
fn title_of(line: &str) -> Option<String> {
    let parsed: Value = serde_json::from_str(line).ok()?;
    let event = parsed.get("event")?;
    if event.get("type").and_then(Value::as_str) != Some("user_message") {
        return None;
    }
    let text = event.get("text").and_then(Value::as_str)?;
    let first = text.lines().next().unwrap_or_default();
    if first.is_empty() {
        return None;
    }
    let mut title: String = first.chars().take(TITLE_MAX_CHARS).collect();
    if first.chars().count() > TITLE_MAX_CHARS {
        title.push('…');
    }
    Some(title)
}

/// Whether `byte` may stay literal in a directory name.
fn is_literal(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')
}

/// Returns the lowercase hex digit for a nibble.
fn hex_digit(nibble: u8) -> char {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let index = usize::from(nibble & 0x0f);
    char::from(*DIGITS.get(index).unwrap_or(&b'0'))
}

/// Returns the value of a lowercase hex digit.
fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte.saturating_sub(b'0')),
        b'a'..=b'f' => Some(byte.saturating_sub(b'a').saturating_add(10)),
        _ => None,
    }
}

/// Encodes a session id as exactly one non-traversing path component.
fn encode_id(id: &SessionId) -> StoreResult<String> {
    let raw = id.as_str();
    if raw.is_empty() {
        return Err(invalid_id(raw, "the id is empty"));
    }
    if raw == "." || raw == ".." {
        return Err(invalid_id(raw, "the id is a relative path component"));
    }
    let mut out = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        if is_literal(byte) {
            out.push(char::from(byte));
        } else {
            out.push('%');
            out.push(hex_digit(byte.checked_shr(4).unwrap_or(0)));
            out.push(hex_digit(byte & 0x0f));
        }
    }
    // Postcondition: the encoding is a single component that cannot climb.
    assert!(
        !out.contains('/') && out != "." && out != "..",
        "an encoded id is one safe component"
    );
    Ok(out)
}

/// Decodes a directory name back into a session id.
fn decode_id(encoded: &str) -> Option<SessionId> {
    if encoded.is_empty() || encoded == "." || encoded == ".." {
        return None;
    }
    let mut bytes: Vec<u8> = Vec::with_capacity(encoded.len());
    let mut iter = encoded.bytes();
    while let Some(byte) = iter.next() {
        if byte != b'%' {
            bytes.push(byte);
            continue;
        }
        let high = hex_value(iter.next()?)?;
        let low = hex_value(iter.next()?)?;
        bytes.push(high.checked_shl(4)?.checked_add(low)?);
    }
    let decoded = String::from_utf8(bytes).ok()?;
    assert!(
        encode_id(&SessionId::new(decoded.clone())).is_ok(),
        "a decoded id re-encodes"
    );
    Some(SessionId::new(decoded))
}

/// Builds the port's I/O error for `path`.
fn io_error(path: &Path, source: &std::io::Error) -> StoreError {
    StoreError::Io {
        path: path.to_path_buf(),
        message: source.to_string(),
    }
}

/// Builds the port's error for an id that cannot name a directory.
fn invalid_id(raw: &str, reason: &str) -> StoreError {
    StoreError::Io {
        path: PathBuf::from(raw),
        message: format!("the session id is unusable: {reason}"),
    }
}

/// Writes `body` to `path` atomically: temp file, fsync, rename.
async fn write_atomic(path: &Path, body: &str) -> StoreResult<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let temp = temp_path(path);
    {
        let mut file = fs::File::create(&temp)
            .await
            .map_err(|source| io_error(&temp, &source))?;
        file.write_all(body.as_bytes())
            .await
            .map_err(|source| io_error(&temp, &source))?;
        // fsync before the rename is what makes the rename a commit rather than a
        // hope: without it a crash can leave a zero-length file under the real name.
        file.sync_all()
            .await
            .map_err(|source| io_error(&temp, &source))?;
    }
    if let Err(source) = fs::rename(&temp, path).await {
        if fs::remove_file(&temp).await.is_ok() {
            tracing::debug!(temp = %temp.display(), "discarded the temporary file");
        }
        return Err(io_error(parent, &source));
    }
    Ok(())
}

/// Builds a unique sibling temp path for `path`.
fn temp_path(path: &Path) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let name = path.file_name().and_then(OsStr::to_str).map_or_else(
        || format!(".nanus.{pid}.{seq}.tmp"),
        |name| format!(".{name}.{pid}.{seq}.tmp"),
    );
    path.with_file_name(name)
}
