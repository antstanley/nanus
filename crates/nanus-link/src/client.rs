//! The client half of the link: an interface talking to an agent.
//!
//! ## Why a connection is a conversation
//!
//! The agent keeps one session per connection, and that is not an implementation detail
//! — it is the lifecycle. An interface that opens a connection owns a conversation; when
//! it exits, the connection closes and the conversation is over. Multi-turn work is
//! therefore a matter of sending another [`Request::Prompt`] on the same client, and a
//! client that reconnects starts a new session because it asked for one.
//!
//! ## Why the handshake is read in `connect`
//!
//! A socket that accepts a connection and then says nothing is indistinguishable from a
//! hung agent. Reading the `Ready` frame before returning means a client that has a
//! [`Client`] has proof of an agent: the session id, the workspace, and the model are in
//! hand, and a socket that was something else entirely has already failed.

use std::path::Path;

use tokio::io::{AsyncWriteExt as _, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

use crate::error::{LinkError, LinkResult};
use crate::protocol::{AgentInfo, Frame, Request, encode};
use crate::wire::read_message;

/// A connection to an agent.
pub struct Client {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    info: AgentInfo,
}

impl Client {
    /// Connects to the agent listening at `path` and reads its handshake.
    ///
    /// # Errors
    ///
    /// Returns [`LinkError::Connect`] when nothing is listening, [`LinkError::Closed`]
    /// when the peer hung up before its handshake, and [`LinkError::Protocol`] when the
    /// handshake is not one.
    pub async fn connect(path: &Path) -> LinkResult<Self> {
        let stream = UnixStream::connect(path)
            .await
            .map_err(|source| LinkError::Connect {
                path: path.to_path_buf(),
                source,
            })?;
        Self::open(stream).await
    }

    /// Reads the handshake from an already-connected stream.
    ///
    /// Separate from [`Client::connect`] so that a test can hand it one end of a
    /// connected pair, which is a link with no filesystem and no path length to worry
    /// about.
    ///
    /// # Errors
    ///
    /// As [`Client::connect`], minus the connection itself.
    pub async fn open(stream: UnixStream) -> LinkResult<Self> {
        let (read_half, writer) = stream.into_split();
        let mut reader = BufReader::new(read_half);
        let Some(frame) = read_message::<Frame, _>(&mut reader).await? else {
            return Err(LinkError::Closed);
        };
        let Frame::Ready(info) = frame else {
            return Err(LinkError::protocol(format!(
                "the agent opened with {frame:?} rather than its handshake"
            )));
        };
        Ok(Self {
            reader,
            writer,
            info,
        })
    }

    /// Returns what the agent said about itself.
    #[must_use]
    pub const fn info(&self) -> &AgentInfo {
        &self.info
    }

    /// Sends one request.
    ///
    /// # Errors
    ///
    /// Returns [`LinkError::Io`] when the write fails.
    pub async fn send(&mut self, request: &Request) -> LinkResult<()> {
        let mut line = encode(request)?;
        line.push('\n');
        self.writer.write_all(line.as_bytes()).await?;
        self.writer.flush().await?;
        Ok(())
    }

    /// Reads the next frame, or `None` when the agent closed the connection.
    ///
    /// # Errors
    ///
    /// Returns [`LinkError::Io`] when the read fails and [`LinkError::Protocol`] when
    /// the line is not a frame.
    pub async fn next(&mut self) -> LinkResult<Option<Frame>> {
        read_message::<Frame, _>(&mut self.reader).await
    }

    /// Asks the agent to describe itself.
    ///
    /// # Errors
    ///
    /// Returns an error when the request cannot be sent or the reply is not the
    /// description that was asked for.
    pub async fn ask_status(&mut self) -> LinkResult<AgentInfo> {
        self.send(&Request::Status).await?;
        match self.next().await? {
            Some(Frame::Status(info)) => Ok(info),
            Some(other) => Err(LinkError::protocol(format!(
                "expected a status reply, got {other:?}"
            ))),
            None => Err(LinkError::Closed),
        }
    }

    /// Asks the agent to stop serving.
    ///
    /// The reply is deliberately not awaited. The agent acknowledges with a `Bye` and
    /// then stops, and a client that waited for it would hang whenever the agent stopped
    /// before flushing — which is exactly the failure a `stop` command must not have.
    ///
    /// # Errors
    ///
    /// Returns [`LinkError::Io`] when the request cannot be sent.
    pub async fn request_shutdown(&mut self) -> LinkResult<()> {
        self.send(&Request::Shutdown).await
    }
}

impl core::fmt::Debug for Client {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Client")
            .field("session", &self.info.session)
            .field("model", &self.info.model)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use tokio::net::UnixStream;

    use super::*;

    fn pair() -> (UnixStream, UnixStream) {
        UnixStream::pair().unwrap_or_else(|error| panic!("a socket pair: {error}"))
    }

    #[tokio::test]
    async fn a_handshake_that_is_not_a_handshake_is_refused() {
        let (agent, client) = pair();
        let mut agent = agent;
        let written = {
            let mut line = encode(&Frame::Bye).unwrap_or_else(|error| panic!("{error}"));
            line.push('\n');
            agent.write_all(line.as_bytes()).await
        };
        assert!(written.is_ok(), "the peer writes");
        drop(agent);

        let opened = Client::open(client).await;
        assert!(matches!(opened, Err(LinkError::Protocol(_))), "{opened:?}");
    }

    #[tokio::test]
    async fn a_peer_that_hangs_up_before_its_handshake_is_a_closed_error() {
        let (agent, client) = pair();
        drop(agent);
        let opened = Client::open(client).await;
        assert!(matches!(opened, Err(LinkError::Closed)), "{opened:?}");
    }

    #[tokio::test]
    async fn a_handshake_is_kept_as_the_agents_description() {
        let (agent, client) = pair();
        let info = AgentInfo {
            session: "01a09558".to_owned(),
            workspace: "/work".to_owned(),
            model: "deepseek-flash".to_owned(),
            tools: 7,
        };
        let expected = info.clone();
        let mut agent = agent;
        let mut line = encode(&Frame::Ready(info)).unwrap_or_else(|error| panic!("{error}"));
        line.push('\n');
        let written = agent.write_all(line.as_bytes()).await;
        assert!(written.is_ok(), "the peer writes");

        let opened = Client::open(client).await;
        assert!(opened.is_ok(), "{opened:?}");
        let Ok(client) = opened else { return };
        assert_eq!(client.info(), &expected);
    }
}
