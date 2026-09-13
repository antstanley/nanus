//! The frames an interface and an agent exchange.
//!
//! ## Why a vocabulary this small
//!
//! An interface needs exactly two things from an agent: what it is, and what happened
//! while a turn ran. So the protocol has two shapes — a request the client sends and a
//! frame the agent sends back — and the frames are a one-to-one transcription of the
//! agent loop's [`nanus_bundle::Progress`] callbacks plus the handshake and the ending.
//! Nothing an interface might *want* is here; anything an interface can learn about the
//! conversation, the session log already holds.
//!
//! ## Why lines of JSON
//!
//! One frame per line, newline-terminated. A streamed delta can contain a newline, and
//! `serde_json` escapes it, so a frame never contains a raw newline and the framing
//! cannot be confused with the payload — which a length-prefixed binary frame would also
//! manage, at the cost of a decoder. The transport is a local socket and the volume is a
//! model's output, so readability wins.
//!
//! ## Why the enum is internally tagged
//!
//! `{"frame":"text","delta":"…"}` is self-describing, so an unrecognised frame is a
//! named error rather than a silent misparse, and a log of the exchange can be read by
//! a person. The cost is a few bytes per frame on a socket that is not the bottleneck.

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::error::{LinkError, LinkResult};

/// What a client asks an agent to do.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(tag = "request", rename_all = "snake_case")]
pub enum Request {
    /// Run one turn and stream its progress.
    ///
    /// The turn joins whatever session this connection is already holding, so a second
    /// prompt continues the conversation. A connection *is* a conversation: that is why
    /// the session id is not in the request and why a client that reconnects starts a
    /// new one.
    Prompt {
        /// The prompt text.
        text: String,
    },

    /// Describe the agent without changing anything.
    Status,

    /// Ask the agent to stop serving.
    Shutdown,
}

/// What an agent tells a client.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(tag = "frame", rename_all = "snake_case")]
pub enum Frame {
    /// The first frame on every connection, sent before anything is asked.
    ///
    /// A client that knows the session id and the model before it sends a prompt can
    /// show them, and a client that gets no `Ready` knows it is not talking to an agent.
    Ready(AgentInfo),

    /// More of the model's answer.
    Text {
        /// The delta, exactly as the model emitted it.
        delta: String,
    },

    /// More of the model's reasoning.
    Reasoning {
        /// The delta, exactly as the model emitted it.
        delta: String,
    },

    /// A step began.
    Step {
        /// The step number within the turn, counting from one.
        step: u32,
    },

    /// A tool is about to run.
    Tool {
        /// The tool's name.
        name: String,
    },

    /// A tool finished.
    ToolDone {
        /// The tool's name.
        name: String,
        /// Whether it reported a failure, which is a result rather than a broken link.
        error: bool,
    },

    /// Usage was reported for the request that just completed.
    Usage {
        /// Tokens the request used, which an interface accumulates rather than replaces.
        tokens: u32,
    },

    /// The turn finished.
    Done {
        /// The model's final answer for this turn.
        answer: String,
    },

    /// The turn failed.
    Failed {
        /// The rendered reason, already user-facing.
        message: String,
    },

    /// A reply to [`Request::Status`].
    Status(AgentInfo),

    /// The agent is stopping.
    Bye,
}

impl Frame {
    /// Returns `true` when the frame ends a turn.
    ///
    /// A stream that has seen one of these is waiting for the next request rather than
    /// for more of this one, which is the property a client reader loops on.
    #[must_use]
    pub const fn is_end_of_turn(&self) -> bool {
        matches!(self, Self::Done { .. } | Self::Failed { .. })
    }
}

/// What an agent says about itself.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct AgentInfo {
    /// The session this connection is recording into.
    pub session: String,
    /// The workspace the agent's tools are confined to.
    pub workspace: String,
    /// The model id the agent will call.
    pub model: String,
    /// How many tools the agent exposes.
    pub tools: usize,
}

/// Encodes a value as one line of JSON, **without** the terminating newline.
///
/// The caller writes the newline, so a writer cannot accidentally emit a frame and a
/// separator as two operations that a reader could observe separately.
///
/// # Errors
///
/// Returns [`LinkError::Protocol`] when the value cannot be encoded, which for these
/// types means a bug in the protocol rather than bad input.
pub fn encode<T: Serialize>(value: &T) -> LinkResult<String> {
    serde_json::to_string(value).map_err(|error| LinkError::protocol(error.to_string()))
}

/// Decodes one line, tolerating a trailing newline.
///
/// # Errors
///
/// Returns [`LinkError::Protocol`] when the line is not a frame or request of this
/// protocol, including when it carries an unknown tag.
pub fn decode<T: DeserializeOwned>(line: &str) -> LinkResult<T> {
    serde_json::from_str(line.trim_end_matches(['\n', '\r']))
        .map_err(|error| LinkError::protocol(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> AgentInfo {
        AgentInfo {
            session: "01a09558".to_owned(),
            workspace: "/work".to_owned(),
            model: "deepseek-flash".to_owned(),
            tools: 7,
        }
    }

    #[test]
    fn a_round_trip_preserves_every_frame() {
        let frames = [
            Frame::Ready(info()),
            Frame::Text {
                delta: "hello".to_owned(),
            },
            Frame::Reasoning {
                delta: "thinking".to_owned(),
            },
            Frame::Step { step: 2 },
            Frame::Tool {
                name: "read".to_owned(),
            },
            Frame::ToolDone {
                name: "read".to_owned(),
                error: true,
            },
            Frame::Usage { tokens: 1234 },
            Frame::Done {
                answer: "done".to_owned(),
            },
            Frame::Failed {
                message: "boom".to_owned(),
            },
            Frame::Status(info()),
            Frame::Bye,
        ];
        for frame in frames {
            let encoded = encode(&frame);
            assert!(encoded.is_ok(), "encodes: {encoded:?}");
            let Ok(encoded) = encoded else { return };
            let decoded = decode::<Frame>(&encoded);
            assert_eq!(decoded.ok(), Some(frame.clone()), "round trip of {frame:?}");
        }
    }

    #[test]
    fn a_round_trip_preserves_every_request() {
        let requests = [
            Request::Prompt {
                text: "do the thing".to_owned(),
            },
            Request::Status,
            Request::Shutdown,
        ];
        for request in requests {
            let encoded = encode(&request);
            assert!(encoded.is_ok(), "encodes: {encoded:?}");
            let Ok(encoded) = encoded else { return };
            let decoded = decode::<Request>(&encoded);
            assert_eq!(
                decoded.ok(),
                Some(request.clone()),
                "round trip of {request:?}"
            );
        }
    }

    #[test]
    fn a_delta_containing_a_newline_still_encodes_to_one_line() {
        // The whole framing argument rests on this: the payload may contain anything at
        // all, and the frame must remain one line.
        let frame = Frame::Text {
            delta: "first\nsecond\r\nthird".to_owned(),
        };
        let Ok(encoded) = encode(&frame) else {
            panic!("a text frame encodes");
        };
        assert_eq!(encoded.lines().count(), 1, "one line: {encoded}");
        assert!(!encoded.contains('\n'), "no raw newline: {encoded}");
        assert_eq!(decode::<Frame>(&encoded).ok(), Some(frame));
    }

    #[test]
    fn an_unknown_tag_is_a_named_failure_rather_than_a_silent_misparse() {
        let decoded = decode::<Frame>(r#"{"frame":"wibble"}"#);
        assert!(
            matches!(decoded, Err(LinkError::Protocol(_))),
            "{decoded:?}"
        );
        let Err(error) = decoded else { return };
        assert!(error.to_string().contains("wibble"), "{error}");
    }

    #[test]
    fn a_line_that_is_not_json_is_refused() {
        assert!(decode::<Frame>("not json at all").is_err());
        assert!(decode::<Request>("").is_err());
    }

    #[test]
    fn only_done_and_failed_end_a_turn() {
        assert!(
            Frame::Done {
                answer: String::new()
            }
            .is_end_of_turn()
        );
        assert!(
            Frame::Failed {
                message: String::new()
            }
            .is_end_of_turn()
        );
        assert!(!Frame::Bye.is_end_of_turn());
        assert!(!Frame::Step { step: 1 }.is_end_of_turn());
    }
}
