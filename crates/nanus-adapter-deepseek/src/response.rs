//! HTTP response ownership and opt-in bounds before JSON decoding.
use crate::{DeepSeekError, EventStream, wire};
use futures::StreamExt as _;
use nanus_ports::{LlmEvent, LlmResult, ResponseFrames, ResponseLimits};

const BODY_SNIPPET_MAX: usize = 2_000;

/// Decodes an HTTP body with the caller-selected policy.
pub fn decode(
    response: reqwest::Response,
    host: String,
    limits: Option<ResponseLimits>,
) -> EventStream {
    let status = response.status();
    if !status.is_success() {
        let stream = futures::stream::once(error_body(response, limits)).map(move |body| {
            LlmEvent::Error(match body {
                Ok(body) => DeepSeekError::status(status.as_u16(), body).to_string(),
                Err(error) => error.to_string(),
            })
        });
        return Box::pin(stream);
    }
    let mut accumulator = wire::StreamAccumulator::default();
    accumulator.set_response_limits(limits);
    let mut bytes = Some(response.bytes_stream());
    let mut decoder = ResponseFrames::new(limits);
    Box::pin(futures::stream::poll_fn(move |cx| {
        loop {
            if let Some(event) = accumulator.take_ready() {
                return core::task::Poll::Ready(Some(event));
            }
            let Some(source) = bytes.as_mut() else {
                return core::task::Poll::Ready(None);
            };
            match source.poll_next_unpin(cx) {
                core::task::Poll::Ready(Some(Ok(chunk))) => {
                    match decoder.push(&chunk) {
                        Ok(lines) => observe_lines(&mut accumulator, lines, limits),
                        Err(error) => accumulator.fail(error.to_string()),
                    }
                    let terminal = decoder.is_done() || accumulator.is_closed();
                    if accumulator.is_closed()
                        || decoder.is_done()
                        || (limits.is_some() && terminal)
                    {
                        finish(&mut accumulator, terminal, limits);
                        bytes = None;
                    }
                }
                core::task::Poll::Ready(Some(Err(error))) => {
                    accumulator.fail(DeepSeekError::transport(&error, &host).to_string());
                    bytes = None;
                }
                core::task::Poll::Ready(None) => {
                    match decoder.finish() {
                        Ok(Some(tail)) => accumulator.observe_line(&tail),
                        Ok(None) => {}
                        Err(error) => accumulator.fail(error.to_string()),
                    }
                    let terminal = decoder.is_done() || accumulator.is_closed();
                    finish(&mut accumulator, terminal, limits);
                    bytes = None;
                }
                core::task::Poll::Pending => return core::task::Poll::Pending,
            }
        }
    }))
}

fn observe_lines(
    accumulator: &mut wire::StreamAccumulator,
    lines: Vec<String>,
    limits: Option<ResponseLimits>,
) {
    for line in lines {
        accumulator.observe_line(&line);
        if limits.is_some() && accumulator.is_closed() {
            break;
        }
    }
}

fn finish(
    accumulator: &mut wire::StreamAccumulator,
    terminal: bool,
    limits: Option<ResponseLimits>,
) {
    if accumulator.is_closed() {
        return;
    }
    if limits.is_some() && !terminal {
        accumulator.fail("malformed stream: response ended without protocol termination".into());
    } else {
        accumulator.close();
    }
}

async fn error_body(
    response: reqwest::Response,
    limits: Option<ResponseLimits>,
) -> LlmResult<String> {
    let Some(limits) = limits else {
        let text = response
            .text()
            .await
            .unwrap_or_else(|error| format!("<{error}>"));
        return Ok(crate::truncate(&text, BODY_SNIPPET_MAX));
    };
    let mut source = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = source.next().await {
        let chunk = chunk.map_err(|_| nanus_ports::LlmError::MalformedStream {
            message: "could not read bounded HTTP error body".into(),
        })?;
        ResponseLimits::add(
            "HTTP error body bytes",
            body.len(),
            chunk.len(),
            limits.error_body_bytes(),
        )?;
        body.try_reserve_exact(chunk.len()).map_err(|_| {
            nanus_ports::LlmError::MalformedStream {
                message: "could not allocate bounded HTTP error body".into(),
            }
        })?;
        body.extend_from_slice(&chunk);
    }
    let text = core::str::from_utf8(&body).map_err(|_| nanus_ports::LlmError::MalformedStream {
        message: "HTTP error body is not valid UTF-8".into(),
    })?;
    Ok(crate::truncate(text, BODY_SNIPPET_MAX))
}
