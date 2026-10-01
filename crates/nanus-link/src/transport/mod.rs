//! Platform-selected local streams and listeners. The protocol never sees an OS handle.

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::{LinkError, LinkResult};

#[cfg(any(windows, test))]
pub mod guard;
#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::bind;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::bind;

/// A local endpoint: a filesystem path on Unix, a pipe name on Windows.
pub type Endpoint = PathBuf;

/// A connected local stream, with one variant for the current platform.
#[derive(Debug)]
pub enum Stream {
    /// A Unix domain socket.
    #[cfg(unix)]
    Unix(tokio::net::UnixStream),
    /// Either end of a Windows named pipe.
    #[cfg(windows)]
    Pipe(windows::PipeStream),
}

/// The independently owned reading half of a stream.
#[cfg(unix)]
pub type OwnedReadHalf = tokio::net::unix::OwnedReadHalf;
/// The independently owned reading half of a Windows stream.
#[cfg(windows)]
pub type OwnedReadHalf = tokio::io::ReadHalf<Stream>;
/// The independently owned writing half of a stream.
#[cfg(unix)]
pub type OwnedWriteHalf = tokio::net::unix::OwnedWriteHalf;
/// The independently owned writing half of a Windows stream.
#[cfg(windows)]
pub type OwnedWriteHalf = tokio::io::WriteHalf<Stream>;

impl Stream {
    /// Connects to a local endpoint and, where the transport needs it, proves the agent there is
    /// this user's before returning.
    ///
    /// # Errors
    ///
    /// Returns [`LinkError::Connect`] when nothing is listening, [`LinkError::Inaccessible`] when
    /// something is but this process may not open it, and [`LinkError::Unverified`] when the
    /// agent at a Windows pipe cannot prove it belongs to this user.
    pub async fn connect(endpoint: &Path) -> LinkResult<Self> {
        #[cfg(unix)]
        {
            tokio::net::UnixStream::connect(endpoint)
                .await
                .map(Self::Unix)
                .map_err(|source| refused_connect(endpoint, source))
        }
        #[cfg(windows)]
        {
            windows::connect(endpoint).await
        }
    }

    /// Splits a stream into halves which can be moved to separate tasks.
    pub fn into_split(self) -> (OwnedReadHalf, OwnedWriteHalf) {
        #[cfg(unix)]
        {
            let Self::Unix(stream) = self;
            stream.into_split()
        }
        #[cfg(windows)]
        {
            tokio::io::split(self)
        }
    }
}

/// Classifies a failure to open an endpoint.
///
/// "Permission denied" is not "nothing is listening": something *is* there, and it belongs to an
/// account — or an elevation — this process cannot reach. Folding the two together made a start
/// spawn a second agent that could only fail to bind.
fn refused_connect(endpoint: &Path, source: std::io::Error) -> LinkError {
    let path = endpoint.to_path_buf();
    if source.kind() == std::io::ErrorKind::PermissionDenied {
        LinkError::Inaccessible { path, source }
    } else {
        LinkError::Connect { path, source }
    }
}

/// A connection whose peer has not yet been accepted as this user's.
///
/// What [`Listener::accept`] returns, so that a connection cannot be served without being
/// verified: the only way to the [`Stream`] is [`Accepted::verify`]. Verifying is a separate step
/// so a server can run it in the connection's own task — a peer that never speaks must hold up
/// its own connection, not every accept after it.
#[derive(Debug)]
#[must_use = "an accepted connection is served only after it is verified"]
pub struct Accepted {
    stream: Stream,
    #[cfg(windows)]
    key: std::sync::Arc<guard::Key>,
}

impl Accepted {
    /// Proves this agent to the peer and requires the peer to prove itself.
    ///
    /// On Unix there is nothing to prove: the socket's directory is the user's alone, so a peer
    /// that reached it already is the user.
    ///
    /// # Errors
    ///
    /// Returns [`LinkError::Io`] when the peer does not hold this user's link key within
    /// the handshake's timeout.
    // Unix has nothing to await; the signature is the Windows one, which does.
    #[cfg_attr(unix, allow(clippy::unused_async, clippy::unused_async_trait_impl))]
    pub async fn verify(self) -> LinkResult<Stream> {
        #[cfg(unix)]
        {
            Ok(self.stream)
        }
        #[cfg(windows)]
        {
            let Self { mut stream, key } = self;
            guard::serve(&mut stream, &key, guard::HANDSHAKE_TIMEOUT)
                .await
                .map_err(|error| {
                    std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        format!("a client was not verified as this user: {error}"),
                    )
                })?;
            Ok(stream)
        }
    }
}

#[cfg(unix)]
impl From<tokio::net::UnixStream> for Stream {
    fn from(stream: tokio::net::UnixStream) -> Self {
        Self::Unix(stream)
    }
}

impl AsyncRead for Stream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            #[cfg(unix)]
            Self::Unix(stream) => Pin::new(stream).poll_read(cx, buf),
            #[cfg(windows)]
            Self::Pipe(stream) => Pin::new(stream).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Stream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            #[cfg(unix)]
            Self::Unix(stream) => Pin::new(stream).poll_write(cx, buf),
            #[cfg(windows)]
            Self::Pipe(stream) => Pin::new(stream).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            #[cfg(unix)]
            Self::Unix(stream) => Pin::new(stream).poll_flush(cx),
            #[cfg(windows)]
            Self::Pipe(stream) => Pin::new(stream).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            #[cfg(unix)]
            Self::Unix(stream) => Pin::new(stream).poll_shutdown(cx),
            #[cfg(windows)]
            Self::Pipe(stream) => Pin::new(stream).poll_shutdown(cx),
        }
    }
}

/// A listener for this platform's local transport.
#[derive(Debug)]
pub enum Listener {
    /// A Unix domain socket listener.
    #[cfg(unix)]
    Unix(tokio::net::UnixListener),
    /// A Windows named pipe listener retaining the next instance.
    #[cfg(windows)]
    Pipe(windows::PipeListener),
}

impl Listener {
    /// Accepts a connection, to be [verified](Accepted::verify) before it is served. Cancelling
    /// an accept leaves the listener usable.
    ///
    /// # Errors
    ///
    /// Returns [`LinkError::Io`] when the operating system refuses the accept. The listener is
    /// still usable afterwards, so a server should treat this as one lost connection.
    pub async fn accept(&mut self) -> LinkResult<Accepted> {
        match self {
            #[cfg(unix)]
            Self::Unix(listener) => Ok(Accepted {
                stream: listener.accept().await?.0.into(),
            }),
            #[cfg(windows)]
            Self::Pipe(listener) => listener.accept().await,
        }
    }

    /// Returns the bound endpoint, when the OS supplies a name.
    pub fn endpoint(&self) -> Option<Endpoint> {
        match self {
            #[cfg(unix)]
            Self::Unix(listener) => listener
                .local_addr()
                .ok()
                .and_then(|address| address.as_pathname().map(Path::to_path_buf)),
            #[cfg(windows)]
            Self::Pipe(listener) => Some(listener.endpoint.clone()),
        }
    }
}

#[cfg(unix)]
impl From<tokio::net::UnixListener> for Listener {
    fn from(listener: tokio::net::UnixListener) -> Self {
        Self::Unix(listener)
    }
}
