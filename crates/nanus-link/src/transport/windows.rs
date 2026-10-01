//! Local-only named pipes. The pending instance keeps the name owned between accepts.

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
};

use super::{Listener, Stream};
use crate::LinkResult;

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
#[derive(Debug)]
pub struct PipeListener {
    pub(super) endpoint: PathBuf,
    pending: NamedPipeServer,
}

/// Binds the first instance, refusing an existing owner or a remote endpoint.
pub async fn bind(endpoint: &Path) -> LinkResult<Listener> {
    validate(endpoint)?;
    let pending = create(endpoint, true)?;
    Ok(Listener::Pipe(PipeListener {
        endpoint: endpoint.to_path_buf(),
        pending,
    }))
}

impl PipeListener {
    pub(super) async fn accept(&mut self) -> LinkResult<Stream> {
        self.pending.connect().await?;
        // No await between creating the replacement and dispatching the connected instance.
        // Cancellation during connect retains the pending instance for the next accept.
        let next = create(&self.endpoint, false)?;
        let connected = std::mem::replace(&mut self.pending, next);
        Ok(Stream::Pipe(PipeStream {
            end: PipeEnd::Server(connected),
        }))
    }
}

fn validate(endpoint: &Path) -> std::io::Result<()> {
    let name = endpoint.to_str().unwrap_or_default();
    if !name.starts_with(r"\\.\pipe\nanus-") || name.contains('/') {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "a local nanus pipe name is required",
        ));
    }
    Ok(())
}

fn create(endpoint: &Path, first: bool) -> std::io::Result<NamedPipeServer> {
    ServerOptions::new()
        .first_pipe_instance(first)
        .reject_remote_clients(true)
        .create(endpoint)
}

pub(super) async fn connect(endpoint: &Path) -> std::io::Result<Stream> {
    validate(endpoint)?;
    // Busy is transient while all instances are connected. Bound the retry so readiness
    // probes and an interface aimed at an unresponsive agent can still report a failure.
    for _ in 0..200 {
        match ClientOptions::new().open(endpoint) {
            Ok(pipe) => {
                return Ok(Stream::Pipe(PipeStream {
                    end: PipeEnd::Client(pipe),
                }));
            }
            Err(error) if error.raw_os_error() == Some(231) => {
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
