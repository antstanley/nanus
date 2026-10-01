//! Platform-selected local streams and listeners. The protocol never sees an OS handle.

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::LinkResult;

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
    /// Connects to a local endpoint.
    pub async fn connect(endpoint: &Path) -> std::io::Result<Self> {
        #[cfg(unix)]
        {
            Ok(Self::Unix(tokio::net::UnixStream::connect(endpoint).await?))
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
    /// Accepts a connection. Cancelling an accept leaves the listener usable.
    pub async fn accept(&mut self) -> LinkResult<Stream> {
        match self {
            #[cfg(unix)]
            Self::Unix(listener) => Ok(listener.accept().await?.0.into()),
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
