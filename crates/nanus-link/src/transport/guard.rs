//! The Windows link's defences that do not need Windows: the endpoint name check and the
//! same-user handshake.
//!
//! ## Why a handshake at all
//!
//! A Unix socket lives in a `0700` directory, so reaching it is already proof of being the user.
//! A named pipe has no directory: its name is global, and computed from the user's SID, which is
//! public. Another account on the machine can therefore create `\\.\pipe\nanus-<your SID>-agent`
//! before the agent does — the agent's bind is then refused, and an interface that connects is
//! talking to the squatter, credentials included. It can also open connections to the real
//! agent's pipe and hold them, because the default descriptor lets every account read it.
//!
//! The pipe's owner could be checked instead, but no call that reads it has a safe binding this
//! workspace can use, and it forbids `unsafe`. So each end proves itself with something only the
//! user can read: a random key the agent writes, after it owns the pipe name, into the user's
//! private local application data. Both ends answer a challenge with an HMAC over two fresh
//! nonces, so the key itself never crosses the pipe and a recorded answer is worthless.
//!
//! - The **client** speaks first and sends nothing else until the agent has answered with a
//!   valid proof. A squatter cannot produce one, so a client never sends it a request.
//! - The **agent** drops a client that does not prove itself within [`HANDSHAKE_TIMEOUT`], so a
//!   connection from another account is held for seconds rather than for ever.
//!
//! Compiled on every platform under test so the handshake is exercised by the ordinary suite,
//! not only by the native Windows runners.

use std::path::{Path, PathBuf};
use std::time::Duration;

use hmac::{Hmac, Mac as _};
use sha2::Sha256;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

/// Every local nanus pipe name starts with this.
pub const PIPE_PREFIX: &str = r"\\.\pipe\nanus-";

/// How long either end waits for the other to prove itself.
///
/// The agent binds immediately before it serves, so an honest agent answers in milliseconds.
/// The bound is what lets the agent drop a connection that never speaks.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// The bytes a client opens with, so an agent can tell a nanus client from anything else.
const MAGIC: &[u8; 16] = b"nanus-link-auth1";

/// The length of the key, of each nonce, and of each proof: one SHA-256 output.
const LEN: usize = 32;

/// Whether `name` is a local nanus pipe: the local prefix, then one component of ASCII letters,
/// digits, and hyphens.
///
/// Exact rather than a prefix test. Win32 normalises a `\\.\` path, so a name such as
/// `\\.\pipe\nanus-x\..\..\UNC\host\pipe\p` passes a prefix check and still resolves to a pipe on
/// another machine. A name made of one restricted component cannot leave the local namespace, and
/// it doubles as a file name for the key.
#[must_use]
pub fn is_local_pipe_name(name: &str) -> bool {
    name.strip_prefix(PIPE_PREFIX).is_some_and(|rest| {
        !rest.is_empty()
            && rest
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    })
}

/// Returns where the key for `endpoint` lives inside `dir`, or `None` for a name that is not a
/// local nanus pipe.
#[must_use]
pub fn key_path(dir: &Path, endpoint: &Path) -> Option<PathBuf> {
    let name = endpoint.to_str()?;
    if !is_local_pipe_name(name) {
        return None;
    }
    let leaf = name.strip_prefix(r"\\.\pipe\")?;
    Some(dir.join(format!("{leaf}.key")))
}

/// The secret both ends prove they hold. Its `Debug` never shows it.
pub struct Key([u8; LEN]);

impl std::fmt::Debug for Key {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Key(<redacted>)")
    }
}

impl Key {
    /// Draws a fresh key from the operating system.
    ///
    /// # Errors
    ///
    /// Returns an error when the system's random source fails.
    pub fn generate() -> std::io::Result<Self> {
        Ok(Self(random()?))
    }

    /// Writes the key to `path`, replacing any earlier one in a single rename.
    ///
    /// A reader therefore sees the old key or the new one, never half of either.
    ///
    /// # Errors
    ///
    /// Returns an error when the directory cannot be created or the file cannot be written.
    pub fn write(&self, path: &Path) -> std::io::Result<()> {
        let parent = path
            .parent()
            .ok_or_else(|| std::io::Error::other("a key path has a directory"))?;
        std::fs::create_dir_all(parent)?;
        let staging = path.with_extension(format!("key.{}", std::process::id()));
        write_private(&staging, &self.0)?;
        std::fs::rename(&staging, path).inspect_err(|_| {
            let _ignored = std::fs::remove_file(&staging);
        })
    }

    /// Reads a key written by [`Key::write`].
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be read, and `InvalidData` when it is not a key.
    pub fn read(path: &Path) -> std::io::Result<Self> {
        let bytes = std::fs::read(path)?;
        let key = <[u8; LEN]>::try_from(bytes.as_slice()).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{} is not a nanus link key", path.display()),
            )
        })?;
        Ok(Self(key))
    }

    /// The proof one `role` gives for a pair of nonces.
    fn proof(
        &self,
        role: &[u8],
        client: &[u8; LEN],
        server: &[u8; LEN],
    ) -> Result<Hmac<Sha256>, AuthError> {
        // HMAC takes a key of any length, so this refusal cannot happen; it is still an error
        // rather than a panic, and an end that cannot make a proof is one that cannot be trusted.
        let mut mac = <Hmac<Sha256>>::new_from_slice(&self.0)
            .map_err(|error| AuthError::Key(std::io::Error::other(error.to_string())))?;
        mac.update(role);
        mac.update(client);
        mac.update(server);
        Ok(mac)
    }
}

/// Why a peer was not accepted as the same user's.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    /// The stream failed or closed part way through.
    #[error("the handshake was cut short: {0}")]
    Io(#[from] std::io::Error),
    /// The peer did not finish within [`HANDSHAKE_TIMEOUT`].
    #[error("the peer did not prove itself within {}s", HANDSHAKE_TIMEOUT.as_secs())]
    TimedOut,
    /// The peer did not open with the nanus handshake.
    #[error("the peer is not a nanus link client")]
    NotNanus,
    /// The peer's proof was not made with this user's key.
    #[error("the peer does not hold this user's link key")]
    Mismatch,
    /// This end could not read its own copy of the key.
    #[error("cannot read the link key: {0}")]
    Key(std::io::Error),
}

/// The agent's half: reads the client's challenge, proves itself, and checks the client's proof.
///
/// # Errors
///
/// Returns [`AuthError`] for anything other than a client holding the same key, within `limit`.
pub async fn serve<S>(stream: &mut S, key: &Key, limit: Duration) -> Result<(), AuthError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    tokio::time::timeout(limit, async {
        let mut opening = [0u8; MAGIC.len() + LEN];
        stream.read_exact(&mut opening).await?;
        let (magic, nonce) = opening.split_at(MAGIC.len());
        if magic != MAGIC {
            return Err(AuthError::NotNanus);
        }
        let client = <[u8; LEN]>::try_from(nonce).map_err(|_| AuthError::NotNanus)?;
        let server = random()?;
        let proof = key
            .proof(b"server", &client, &server)?
            .finalize()
            .into_bytes();
        let mut answer = [0u8; LEN + LEN];
        answer[..LEN].copy_from_slice(&server);
        answer[LEN..].copy_from_slice(&proof);
        stream.write_all(&answer).await?;
        stream.flush().await?;
        let mut theirs = [0u8; LEN];
        stream.read_exact(&mut theirs).await?;
        key.proof(b"client", &client, &server)?
            .verify_slice(&theirs)
            .map_err(|_| AuthError::Mismatch)
    })
    .await
    .map_err(|_elapsed| AuthError::TimedOut)?
}

/// The client's half: challenges the agent, checks its proof, and only then proves itself.
///
/// The key is loaded *after* the agent answers. An agent writes its key after it owns the pipe
/// and before it serves, so a client that connected in between would otherwise read the previous
/// agent's key, or none.
///
/// # Errors
///
/// Returns [`AuthError`] for anything other than an agent holding the same key, within `limit`.
pub async fn connect<S, K>(stream: &mut S, load: K, limit: Duration) -> Result<(), AuthError>
where
    S: AsyncRead + AsyncWrite + Unpin,
    K: FnOnce() -> std::io::Result<Key>,
{
    tokio::time::timeout(limit, async {
        let client = random()?;
        let mut opening = [0u8; MAGIC.len() + LEN];
        opening[..MAGIC.len()].copy_from_slice(MAGIC);
        opening[MAGIC.len()..].copy_from_slice(&client);
        stream.write_all(&opening).await?;
        stream.flush().await?;
        let mut answer = [0u8; LEN + LEN];
        stream.read_exact(&mut answer).await?;
        let (nonce, proof) = answer.split_at(LEN);
        let server = <[u8; LEN]>::try_from(nonce).map_err(|_| AuthError::Mismatch)?;
        let key = load().map_err(AuthError::Key)?;
        key.proof(b"server", &client, &server)?
            .verify_slice(proof)
            .map_err(|_| AuthError::Mismatch)?;
        let mine = key
            .proof(b"client", &client, &server)?
            .finalize()
            .into_bytes();
        stream.write_all(&mine).await?;
        stream.flush().await?;
        Ok(())
    })
    .await
    .map_err(|_elapsed| AuthError::TimedOut)?
}

/// Fills a nonce or a key from the operating system's random source.
fn random() -> std::io::Result<[u8; LEN]> {
    let mut bytes = [0u8; LEN];
    getrandom::fill(&mut bytes).map_err(|error| std::io::Error::other(error.to_string()))?;
    Ok(bytes)
}

/// Creates `path` readable by this user alone where the platform has modes; on Windows the
/// directory's inherited descriptor, private to the user, is what protects it.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys() -> (Key, Key) {
        let key = Key::generate().unwrap();
        let copy = Key(key.0);
        (key, copy)
    }

    #[test]
    fn a_local_pipe_name_is_accepted() {
        assert!(is_local_pipe_name(
            r"\\.\pipe\nanus-S-1-5-21-1-2-3-1001-agent"
        ));
        assert!(is_local_pipe_name(
            r"\\.\pipe\nanus-S-1-5-21-1-attach-41-0b6f1e2a-9a8e-4c55-8f2f-111111111111"
        ));
    }

    #[test]
    fn a_name_that_could_leave_the_local_namespace_is_refused() {
        for name in [
            r"\\.\pipe\nanus-x\..\..\UNC\host\pipe\p",
            r"\\.\pipe\nanus-x\..\other",
            r"\\host\pipe\nanus-agent",
            r"\\?\pipe\nanus-agent",
            r"\\.\pipe\nanus-",
            r"\\.\pipe\nanus-a/b",
            r"\\.\pipe\nanus-a.b",
            r"\\.\pipe\other-agent",
            "",
        ] {
            assert!(!is_local_pipe_name(name), "{name}");
        }
    }

    #[test]
    fn a_key_lives_beside_its_pipe_name_and_nowhere_for_another_name() {
        let dir = Path::new("/keys");
        assert_eq!(
            key_path(dir, Path::new(r"\\.\pipe\nanus-S-1-5-agent")),
            Some(PathBuf::from("/keys/nanus-S-1-5-agent.key"))
        );
        assert_eq!(key_path(dir, Path::new(r"\\.\pipe\nanus-x\..\y")), None);
    }

    #[test]
    fn a_written_key_reads_back_and_a_short_file_is_not_a_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run").join("nanus-agent.key");
        let key = Key::generate().unwrap();
        key.write(&path).unwrap();
        assert_eq!(Key::read(&path).unwrap().0, key.0);
        // A second write replaces the first rather than failing on it.
        let next = Key::generate().unwrap();
        next.write(&path).unwrap();
        assert_eq!(Key::read(&path).unwrap().0, next.0);
        std::fs::write(&path, b"short").unwrap();
        let error = Key::read(&path).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[cfg(unix)]
    #[test]
    fn a_written_key_is_private_to_its_user() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nanus-agent.key");
        Key::generate().unwrap().write(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn a_key_never_renders_itself() {
        let key = Key([7u8; LEN]);
        assert_eq!(format!("{key:?}"), "Key(<redacted>)");
    }

    #[tokio::test]
    async fn two_ends_holding_one_key_accept_each_other_and_the_stream_stays_usable() {
        let (server_key, client_key) = keys();
        let (mut agent, mut client) = tokio::io::duplex(1024);
        let (served, connected) = tokio::join!(
            serve(&mut agent, &server_key, HANDSHAKE_TIMEOUT),
            connect(&mut client, || Ok(client_key), HANDSHAKE_TIMEOUT),
        );
        served.unwrap();
        connected.unwrap();
        // Nothing of the handshake is left in either direction for the protocol to misread.
        agent.write_all(b"ready\n").await.unwrap();
        let mut line = [0u8; 6];
        client.read_exact(&mut line).await.unwrap();
        assert_eq!(&line, b"ready\n");
    }

    #[tokio::test]
    async fn a_client_refuses_an_agent_without_the_key_before_proving_itself() {
        let (agent_key, _) = keys();
        let (client_key, _) = keys();
        let (mut agent, mut client) = tokio::io::duplex(1024);
        let squatter = async {
            let mut opening = [0u8; MAGIC.len() + LEN];
            agent.read_exact(&mut opening).await.unwrap();
            let client_nonce = <[u8; LEN]>::try_from(&opening[MAGIC.len()..]).unwrap();
            let server = [1u8; LEN];
            let proof = agent_key.proof(b"server", &client_nonce, &server).unwrap();
            let mut answer = [0u8; LEN + LEN];
            answer[..LEN].copy_from_slice(&server);
            answer[LEN..].copy_from_slice(&proof.finalize().into_bytes());
            agent.write_all(&answer).await.unwrap();
            // Whatever the client sends now, a squatter must not get a proof made with its key.
            let mut rest = Vec::new();
            agent.read_to_end(&mut rest).await.unwrap();
            rest
        };
        let connecting = async {
            let outcome = connect(&mut client, || Ok(client_key), HANDSHAKE_TIMEOUT).await;
            drop(client);
            outcome
        };
        let (sent, outcome) = tokio::join!(squatter, connecting);
        assert!(matches!(outcome, Err(AuthError::Mismatch)), "{outcome:?}");
        assert!(sent.is_empty(), "the client sent {} bytes", sent.len());
    }

    #[tokio::test]
    async fn an_agent_refuses_a_client_without_the_key() {
        let (agent_key, _) = keys();
        let (other_key, _) = keys();
        let (mut agent, mut client) = tokio::io::duplex(1024);
        let (served, _connected) = tokio::join!(
            serve(&mut agent, &agent_key, HANDSHAKE_TIMEOUT),
            // The client cannot verify this agent either, so it stops after the challenge; the
            // agent is then left waiting for a proof that never comes and closing is the answer.
            async {
                let outcome = connect(&mut client, || Ok(other_key), HANDSHAKE_TIMEOUT).await;
                drop(client);
                outcome
            },
        );
        assert!(matches!(served, Err(AuthError::Io(_))), "{served:?}");
    }

    #[tokio::test]
    async fn an_agent_refuses_a_wrong_proof_from_a_client_that_saw_its_answer() {
        let (agent_key, _) = keys();
        let (mut agent, mut client) = tokio::io::duplex(1024);
        let forger = async {
            let mut opening = [0u8; MAGIC.len() + LEN];
            opening[..MAGIC.len()].copy_from_slice(MAGIC);
            client.write_all(&opening).await.unwrap();
            let mut answer = [0u8; LEN + LEN];
            client.read_exact(&mut answer).await.unwrap();
            client.write_all(&[0u8; LEN]).await.unwrap();
        };
        let (served, ()) = tokio::join!(serve(&mut agent, &agent_key, HANDSHAKE_TIMEOUT), forger);
        assert!(matches!(served, Err(AuthError::Mismatch)), "{served:?}");
    }

    #[tokio::test]
    async fn an_agent_refuses_something_that_is_not_a_nanus_client() {
        let (agent_key, _) = keys();
        let (mut agent, mut client) = tokio::io::duplex(1024);
        let stranger = async {
            // Longer than the opening, so the agent judges it rather than waiting for more.
            client.write_all(&[b'x'; 64]).await.unwrap();
        };
        let (served, ()) = tokio::join!(serve(&mut agent, &agent_key, HANDSHAKE_TIMEOUT), stranger);
        assert!(matches!(served, Err(AuthError::NotNanus)), "{served:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn an_agent_drops_a_client_that_never_speaks() {
        let (agent_key, _) = keys();
        let (mut agent, _silent) = tokio::io::duplex(1024);
        let served = serve(&mut agent, &agent_key, HANDSHAKE_TIMEOUT).await;
        assert!(matches!(served, Err(AuthError::TimedOut)), "{served:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_client_gives_up_on_an_agent_that_never_answers() {
        let (client_key, _) = keys();
        let (_silent, mut client) = tokio::io::duplex(1024);
        let outcome = connect(&mut client, || Ok(client_key), HANDSHAKE_TIMEOUT).await;
        assert!(matches!(outcome, Err(AuthError::TimedOut)), "{outcome:?}");
    }

    #[tokio::test]
    async fn a_client_without_its_own_key_says_so_rather_than_blaming_the_agent() {
        let (agent_key, _) = keys();
        let (mut agent, mut client) = tokio::io::duplex(1024);
        let (_served, outcome) = tokio::join!(
            serve(&mut agent, &agent_key, Duration::from_millis(200)),
            async {
                let missing = || Err(std::io::Error::from(std::io::ErrorKind::NotFound));
                let outcome = connect(&mut client, missing, HANDSHAKE_TIMEOUT).await;
                drop(client);
                outcome
            },
        );
        assert!(matches!(outcome, Err(AuthError::Key(_))), "{outcome:?}");
    }
}
