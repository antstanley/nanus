//! The client half of the link: an interface talking to an agent.
//!
//! ## Why the handshake is read in `connect`
//!
//! A socket that accepts a connection and then says nothing is indistinguishable from a
//! hung agent. Reading the `Ready` frame before returning means a client that has a
//! [`Client`] has proof of an agent — its workspace, its model, and the version it speaks —
//! and a socket that was something else entirely has already failed.
//!
//! ## Why attaching is a request
//!
//! A connection used to *be* a conversation: connect, and you had a session. That is a
//! pleasant default and it made the lifetimes work, but it left no way to say which
//! conversation you wanted. So a client sends [`Request::New`] or [`Request::Attach`] and is
//! told what it got in a [`Frame::Attached`]; a connection may re-attach, and the session it
//! leaves keeps running without it because the *agent* holds the session, not the
//! connection. [`crate::protocol`] argues the trade in full.

use std::path::Path;

use tokio::io::{AsyncWriteExt as _, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

use crate::error::{LinkError, LinkResult};
use crate::protocol::{
    AgentInfo, ApprovalState, Frame, PROTOCOL_VERSION, Request, SessionInfo, encode,
};
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
        // Refused here, before the connection is usable, because every frame after this one
        // is read with this build's vocabulary: a version mismatch is a sentence now rather
        // than a decode error about a field halfway through a turn.
        if info.version != PROTOCOL_VERSION {
            return Err(LinkError::Version {
                agent: info.version,
                client: PROTOCOL_VERSION,
            });
        }
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

    /// Asks the agent to start a session and attach this client to it.
    ///
    /// # Errors
    ///
    /// Returns [`LinkError::Agent`] when the agent refuses — a name that is already taken
    /// is the ordinary reason — and [`LinkError::Protocol`] when the reply is not an
    /// attachment.
    pub async fn start(&mut self, name: Option<String>) -> LinkResult<SessionInfo> {
        self.send(&Request::New { name }).await?;
        self.attached().await
    }

    /// Asks the agent to attach this client to an existing session.
    ///
    /// The reference is a name or an id; the agent prefers a session it is already
    /// holding to one on disk.
    ///
    /// # Errors
    ///
    /// Returns [`LinkError::Agent`] when nothing answers to the reference, and
    /// [`LinkError::Protocol`] when the reply is not an attachment.
    pub async fn attach(&mut self, session: &str) -> LinkResult<SessionInfo> {
        self.send(&Request::Attach {
            session: session.to_owned(),
        })
        .await?;
        self.attached().await
    }

    /// Asks the agent which sessions it is holding open.
    ///
    /// # Errors
    ///
    /// Returns [`LinkError::Protocol`] when the reply is not a listing and
    /// [`LinkError::Closed`] when the agent hung up instead of answering.
    pub async fn sessions(&mut self) -> LinkResult<Vec<SessionInfo>> {
        self.send(&Request::Sessions).await?;
        // Frames that are not the reply are discarded rather than reported: an attached
        // connection is pushed frames the agent decides to send — the approval state, a
        // prompt somebody else typed — and a client asking a question of its own should not
        // fail because one of them arrived first. This is the same rule `attached` follows.
        loop {
            match self.next().await? {
                Some(Frame::Sessions { held }) => return Ok(held),
                Some(Frame::Failed { message }) => return Err(LinkError::agent(message)),
                Some(other) => {
                    tracing::debug!(frame = ?other, "discarding a frame that is not the listing");
                }
                None => return Err(LinkError::Closed),
            }
        }
    }

    /// Answers an approval question.
    ///
    /// `always` records the tool for the session rather than granting the one call, which is
    /// the option an interface offers as "always allow".
    ///
    /// # Errors
    ///
    /// Returns [`LinkError::Io`] when the write fails.
    pub async fn approve(&mut self, call_id: &str, allow: bool, always: bool) -> LinkResult<()> {
        self.send(&Request::Approve {
            call_id: call_id.to_owned(),
            allow,
            always,
        })
        .await
    }

    /// Replaces the agent's approval state.
    ///
    /// # Errors
    ///
    /// Returns [`LinkError::Io`] when the write fails.
    pub async fn set_approval(&mut self, state: ApprovalState) -> LinkResult<()> {
        self.send(&Request::SetApproval { state }).await
    }

    /// Reads the reply to a `New` or an `Attach`.
    ///
    /// Frames from the session this client is *leaving* are discarded rather than
    /// reported as a protocol failure. A client that attaches again on a live connection
    /// is walking away from a conversation that may still be talking, and those frames
    /// belong to the session it has just left — the reply it is waiting for is the only
    /// thing it can act on. A freshly opened connection cannot receive them at all, so
    /// the discard never happens in the ordinary path.
    async fn attached(&mut self) -> LinkResult<SessionInfo> {
        loop {
            match self.next().await? {
                Some(Frame::Attached(info)) => return Ok(info),
                // A refusal is the agent answering the question rather than a broken link,
                // so it keeps its own message instead of being reported as a protocol
                // failure.
                Some(Frame::Failed { message }) => return Err(LinkError::agent(message)),
                Some(other) => {
                    tracing::debug!(frame = ?other, "discarding a frame from the session being left");
                }
                None => return Err(LinkError::Closed),
            }
        }
    }

    /// Splits the connection into a reader and a sender.
    ///
    /// A client that both watches a session and sends to it has to do both at once — a
    /// prompt sent while frames are still arriving must not stop the frames — and a
    /// single value cannot be borrowed mutably by two futures in one `select!`. Splitting
    /// is what makes the two directions independent, which is also how they are.
    #[must_use]
    pub fn split(self) -> (ClientReader, ClientSender) {
        (
            ClientReader {
                reader: self.reader,
            },
            ClientSender {
                writer: self.writer,
            },
        )
    }

    /// Asks the agent to describe itself.
    ///
    /// # Errors
    ///
    /// Returns an error when the request cannot be sent or the reply is not the
    /// description that was asked for.
    pub async fn ask_status(&mut self) -> LinkResult<AgentInfo> {
        self.send(&Request::Status).await?;
        // As `sessions`: a pushed frame is not a reason to fail a question this client asked.
        loop {
            match self.next().await? {
                Some(Frame::Status(info)) => return Ok(info),
                Some(Frame::Failed { message }) => return Err(LinkError::agent(message)),
                Some(other) => {
                    tracing::debug!(frame = ?other, "discarding a frame that is not the status");
                }
                None => return Err(LinkError::Closed),
            }
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

/// The reading half of a connection.
pub struct ClientReader {
    reader: BufReader<OwnedReadHalf>,
}

impl ClientReader {
    /// Reads the next frame, or `None` when the agent closed the connection.
    ///
    /// # Errors
    ///
    /// Returns [`LinkError::Io`] when the read fails and [`LinkError::Protocol`] when
    /// the line is not a frame.
    pub async fn next(&mut self) -> LinkResult<Option<Frame>> {
        read_message::<Frame, _>(&mut self.reader).await
    }
}

/// The writing half of a connection.
pub struct ClientSender {
    writer: OwnedWriteHalf,
}

impl ClientSender {
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
}

impl core::fmt::Debug for Client {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Client")
            .field("workspace", &self.info.workspace)
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
    async fn a_split_connection_reads_and_writes_independently() {
        // What the split is for: a client watching a session can send while frames are
        // still arriving, which a single borrow of one value cannot express.
        let (agent, client) = pair();
        let mut agent = agent;
        let mut line = encode(&Frame::Ready(AgentInfo {
            workspace: "/work".to_owned(),
            model: "scripted".to_owned(),
            models: Vec::new(),
            effort: None,
            model_efforts: Vec::new(),
            provider: String::new(),
            plan: String::new(),
            providers: Vec::new(),
            tools: 0,
            version: PROTOCOL_VERSION,
        }))
        .unwrap_or_else(|error| panic!("{error}"));
        line.push('\n');
        let written = agent.write_all(line.as_bytes()).await;
        assert!(written.is_ok(), "the peer writes");

        let opened = Client::open(client).await;
        assert!(opened.is_ok(), "{opened:?}");
        let Ok(client) = opened else { return };
        let (mut reader, mut sender) = client.split();

        let sent = sender
            .send(&Request::Prompt {
                text: "hello".to_owned(),
            })
            .await;
        assert!(sent.is_ok(), "{sent:?}");
        // The peer reads the request it was sent and answers.
        let mut echoed = String::new();
        let read = {
            use tokio::io::AsyncBufReadExt as _;
            let mut buffered = BufReader::new(&mut agent);
            buffered.read_line(&mut echoed).await
        };
        assert!(read.is_ok(), "the peer reads");
        assert!(echoed.contains("hello"), "{echoed}");
        // And the reader is still usable, which is the point. The peer is closed first,
        // so the next read is the end of the stream rather than a wait for a frame that
        // is never coming.
        drop(agent);
        assert_eq!(reader.next().await.ok().flatten(), None);
    }

    #[tokio::test]
    async fn a_frame_from_the_session_being_left_is_discarded() {
        // A client that attaches again on a live connection is walking away from a
        // conversation that may still be talking. Those frames are not a protocol error,
        // and the reply the client is waiting for is the only thing it can act on.
        let (agent, client) = pair();
        let mut agent = agent;
        let mut script = String::new();
        for frame in [
            Frame::Ready(AgentInfo {
                workspace: "/work".to_owned(),
                model: "scripted".to_owned(),
                models: Vec::new(),
                effort: None,
                model_efforts: Vec::new(),
                provider: String::new(),
                plan: String::new(),
                providers: Vec::new(),
                tools: 0,
                version: PROTOCOL_VERSION,
            }),
            Frame::Text {
                delta: "from the session being left".to_owned(),
            },
            Frame::Attached(SessionInfo {
                session: "01a09558".to_owned(),
                name: None,
                title: None,
                events: 0,
                busy: false,
                viewers: 1,
            }),
        ] {
            let mut line = encode(&frame).unwrap_or_else(|error| panic!("{error}"));
            line.push('\n');
            script.push_str(&line);
        }
        let written = agent.write_all(script.as_bytes()).await;
        assert!(written.is_ok(), "the peer writes");

        let opened = Client::open(client).await;
        assert!(opened.is_ok(), "{opened:?}");
        let Ok(mut client) = opened else { return };
        let attached = client.start(None).await;
        assert_eq!(
            attached.ok().map(|info| info.session),
            Some("01a09558".to_owned())
        );
    }

    #[tokio::test]
    async fn a_refused_attachment_keeps_the_agents_message() {
        // The agent answering "no" is not a broken link, and a caller has to be able to
        // tell the two apart: one is a name somebody else has, the other is a bug.
        let (agent, client) = pair();
        let mut agent = agent;
        let mut script = String::new();
        for frame in [
            Frame::Ready(AgentInfo {
                workspace: "/work".to_owned(),
                model: "scripted".to_owned(),
                models: Vec::new(),
                effort: None,
                model_efforts: Vec::new(),
                provider: String::new(),
                plan: String::new(),
                providers: Vec::new(),
                tools: 0,
                version: PROTOCOL_VERSION,
            }),
            Frame::Failed {
                message: "the name \"taken\" already belongs to session 01a0".to_owned(),
            },
        ] {
            let mut line = encode(&frame).unwrap_or_else(|error| panic!("{error}"));
            line.push('\n');
            script.push_str(&line);
        }
        let written = agent.write_all(script.as_bytes()).await;
        assert!(written.is_ok(), "the peer writes");

        let opened = Client::open(client).await;
        assert!(opened.is_ok(), "{opened:?}");
        let Ok(mut client) = opened else { return };
        let attached = client.start(Some("taken".to_owned())).await;
        match attached {
            Err(LinkError::Agent(message)) => assert!(message.contains("taken"), "{message}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_peer_that_hangs_up_before_its_handshake_is_a_closed_error() {
        let (agent, client) = pair();
        drop(agent);
        let opened = Client::open(client).await;
        assert!(matches!(opened, Err(LinkError::Closed)), "{opened:?}");
    }

    /// A handshake from another version is refused with a sentence naming both.
    ///
    /// This is the failure the version field exists to turn from a decode error mid-turn
    /// into a diagnosis. The two halves ship together, but a stale interface binary beside a
    /// rebuilt core is exactly the case a person cannot see from the outside.
    #[tokio::test]
    async fn a_handshake_from_another_version_is_refused_by_name() {
        let (agent, client) = pair();
        let mut agent = agent;
        let mut line = encode(&Frame::Ready(AgentInfo {
            workspace: "/work".to_owned(),
            model: "scripted".to_owned(),
            models: Vec::new(),
            effort: None,
            model_efforts: Vec::new(),
            provider: String::new(),
            plan: String::new(),
            providers: Vec::new(),
            tools: 0,
            version: PROTOCOL_VERSION.saturating_add(1),
        }))
        .unwrap_or_else(|error| panic!("{error}"));
        line.push('\n');
        let written = agent.write_all(line.as_bytes()).await;
        assert!(written.is_ok(), "the peer writes");

        let opened = Client::open(client).await;
        match opened {
            Err(LinkError::Version { agent, client }) => {
                assert_eq!(agent, PROTOCOL_VERSION.saturating_add(1));
                assert_eq!(client, PROTOCOL_VERSION);
            }
            other => panic!("expected a version refusal, got {other:?}"),
        }
    }

    /// And a build too old to send a version is refused too, rather than assumed to speak
    /// this one: silence is not agreement.
    #[tokio::test]
    async fn an_unversioned_handshake_is_refused() {
        let (agent, client) = pair();
        let mut agent = agent;
        // No `version` field, as a build that predates it writes.
        let mut line =
            String::from(r#"{"frame":"ready","workspace":"/work","model":"m","tools":0}"#);
        line.push('\n');
        let written = agent.write_all(line.as_bytes()).await;
        assert!(written.is_ok(), "the peer writes");

        let opened = Client::open(client).await;
        assert!(
            matches!(opened, Err(LinkError::Version { agent: 0, .. })),
            "an unversioned handshake is version zero: {opened:?}"
        );
    }

    #[tokio::test]
    async fn a_handshake_is_kept_as_the_agents_description() {
        let (agent, client) = pair();
        let info = AgentInfo {
            workspace: "/work".to_owned(),
            model: "deepseek-flash".to_owned(),
            models: vec!["deepseek-flash".to_owned(), "deepseek-v4-pro".to_owned()],
            effort: Some(crate::protocol::EffortState::Medium),
            model_efforts: Vec::new(),
            provider: String::new(),
            plan: String::new(),
            providers: Vec::new(),
            tools: 7,
            version: PROTOCOL_VERSION,
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
