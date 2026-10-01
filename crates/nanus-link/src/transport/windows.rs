//! Local-only named pipes. The pending instance keeps the name owned between accepts, and the
//! handshake in [`super::guard`] keeps another account from posing as either end.

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
};

use super::guard::{self, Key, PIPE_PREFIX, is_local_pipe_name};
use super::{Accepted, Listener, Stream, refused_connect};
use crate::{LinkError, LinkResult};

/// A connected pipe whose client/server distinction stays inside the transport.
#[derive(Debug)]
pub struct PipeStream {
    end: PipeEnd,
}

#[derive(Debug)]
enum PipeEnd {
    Client(NamedPipeClient),
    Server(NamedPipeServer),
}

impl AsyncRead for PipeStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match &mut self.get_mut().end {
            PipeEnd::Client(pipe) => Pin::new(pipe).poll_read(cx, buf),
            PipeEnd::Server(pipe) => Pin::new(pipe).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for PipeStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match &mut self.get_mut().end {
            PipeEnd::Client(pipe) => Pin::new(pipe).poll_write(cx, buf),
            PipeEnd::Server(pipe) => Pin::new(pipe).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match &mut self.get_mut().end {
            PipeEnd::Client(pipe) => Pin::new(pipe).poll_flush(cx),
            PipeEnd::Server(pipe) => Pin::new(pipe).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match &mut self.get_mut().end {
            PipeEnd::Client(pipe) => Pin::new(pipe).poll_shutdown(cx),
            PipeEnd::Server(pipe) => Pin::new(pipe).poll_shutdown(cx),
        }
    }
}

/// A pipe listener owning the next instance before dispatching an accepted one.
///
/// It also owns the link key: written when the name is first owned, removed when the listener
/// goes. See [`super::guard`] for why a pipe needs one.
#[derive(Debug)]
pub struct PipeListener {
    pub(super) endpoint: PathBuf,
    /// The instance the next client connects to. Empty only after creating it failed, in which
    /// case the next accept tries again rather than the whole listener failing.
    pending: Option<NamedPipeServer>,
    key: Arc<Key>,
    key_path: PathBuf,
}

/// Binds the first instance, refusing an existing owner or a remote endpoint, then publishes the
/// key a client checks this agent against.
///
/// # Errors
///
/// Returns an error when the name is not a local nanus pipe, is already owned, or the key cannot
/// be written.
pub async fn bind(endpoint: &Path) -> LinkResult<Listener> {
    validate(endpoint)?;
    let key_path = key_path(endpoint)?;
    // The name first: `first_pipe_instance` is what proves this process owns it. Writing the key
    // before that would replace the key of an agent that is already running here.
    let pending = create(endpoint, true)?;
    let key = Key::generate()?;
    key.write(&key_path)?;
    tracing::debug!(key = %key_path.display(), "the link key is published");
    Ok(Listener::Pipe(PipeListener {
        endpoint: endpoint.to_path_buf(),
        pending: Some(pending),
        key: Arc::new(key),
        key_path,
    }))
}

impl PipeListener {
    pub(super) async fn accept(&mut self) -> LinkResult<Accepted> {
        if self.pending.is_none() {
            self.pending = Some(create(&self.endpoint, false)?);
        }
        let pending = self
            .pending
            .as_mut()
            .ok_or_else(|| std::io::Error::other("no pipe instance is pending"))?;
        if let Err(error) = pending.connect().await {
            // An instance whose connect failed is in no state to be reused.
            self.pending = None;
            return Err(error.into());
        }
        // No await between creating the replacement and dispatching the connected instance.
        // Cancellation during connect retains the pending instance for the next accept.
        let connected = match create(&self.endpoint, false) {
            Ok(next) => self.pending.replace(next),
            Err(error) => {
                // The client that connected is still served; only the *next* instance is
                // missing, and the next accept creates it.
                tracing::warn!(%error, "the next pipe instance could not be created yet");
                self.pending.take()
            }
        };
        let stream = connected
            .ok_or_else(|| std::io::Error::other("the connected pipe instance was lost"))?;
        Ok(Accepted {
            stream: Stream::Pipe(PipeStream {
                end: PipeEnd::Server(stream),
            }),
            key: Arc::clone(&self.key),
        })
    }
}

impl Drop for PipeListener {
    fn drop(&mut self) {
        // Removed before `pending` closes: while any instance of the name is open no successor
        // can own it, so this cannot remove a key a successor has already written.
        if let Err(error) = std::fs::remove_file(&self.key_path) {
            tracing::debug!(%error, key = %self.key_path.display(), "the link key was not removed");
        }
    }
}

fn validate(endpoint: &Path) -> std::io::Result<()> {
    if endpoint.to_str().is_some_and(is_local_pipe_name) {
        return Ok(());
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!("a local nanus pipe name is required, like {PIPE_PREFIX}<letters, digits, ->"),
    ))
}

/// Where the key for `endpoint` lives: the user's private local application data, which no other
/// account can read. Not the nanus home, because the pipe name does not depend on the home either:
/// two homes reach one agent, so they must find one key.
fn key_path(endpoint: &Path) -> std::io::Result<PathBuf> {
    use etcetera::BaseStrategy as _;
    let strategy = etcetera::choose_base_strategy()
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    let dir = strategy.cache_dir().join("nanus").join("run");
    guard::key_path(&dir, endpoint).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "a local nanus pipe name is required",
        )
    })
}

fn create(endpoint: &Path, first: bool) -> std::io::Result<NamedPipeServer> {
    ServerOptions::new()
        .first_pipe_instance(first)
        .reject_remote_clients(true)
        .create(endpoint)
}

/// Opens the pipe, then requires the agent to prove it is this user's before anything is sent.
pub(super) async fn connect(endpoint: &Path) -> LinkResult<Stream> {
    validate(endpoint)?;
    let mut pipe = open(endpoint)
        .await
        .map_err(|source| refused_connect(endpoint, source))?;
    let load = || key_path(endpoint).and_then(|path| Key::read(&path));
    guard::connect(&mut pipe, load, guard::HANDSHAKE_TIMEOUT)
        .await
        .map_err(|error| LinkError::Unverified {
            path: endpoint.to_path_buf(),
            reason: error.to_string(),
        })?;
    Ok(Stream::Pipe(PipeStream {
        end: PipeEnd::Client(pipe),
    }))
}

async fn open(endpoint: &Path) -> std::io::Result<NamedPipeClient> {
    const ERROR_PIPE_BUSY: i32 = 231;
    // Busy is transient while all instances are connected. Bound the retry so readiness
    // probes and an interface aimed at an unresponsive agent can still report a failure.
    for _ in 0..200 {
        match ClientOptions::new().open(endpoint) {
            Ok(pipe) => return Ok(pipe),
            Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "the local agent pipe stayed busy",
    ))
}
