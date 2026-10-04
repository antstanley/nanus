//! Incremental source-prefix receipts. JSON is counted while hashing, not cloned into a buffer.
use std::io::Write;

use nanus_ports::{LlmError, LlmResult};
use serde::Serialize;
use sha2::{Digest as _, Sha256};

fn refused() -> LlmError {
    LlmError::Unsupported {
        feature: "request digest exceeds its byte envelope".into(),
    }
}

#[derive(Clone)]
struct Writer {
    hash: Sha256,
    used: usize,
    limit: usize,
}
impl Write for Writer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.used = self
            .used
            .checked_add(bytes.len())
            .filter(|v| *v <= self.limit)
            .ok_or_else(|| std::io::Error::other("digest byte envelope"))?;
        self.hash.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub fn json(value: &impl Serialize, limit: usize) -> LlmResult<String> {
    let mut writer = Writer {
        hash: Sha256::new(),
        used: 0,
        limit,
    };
    serde_json::to_writer(&mut writer, value).map_err(|_| refused())?;
    Ok(format!("{:x}", writer.hash.finalize()))
}

pub struct Source {
    writer: Writer,
    messages: usize,
}
impl Source {
    pub fn new() -> LlmResult<Self> {
        let mut writer = Writer {
            hash: Sha256::new(),
            used: 0,
            limit: nanus_domain::content::SESSION_BYTES_MAX,
        };
        writer.write_all(b"[").map_err(|_| refused())?;
        Ok(Self {
            writer,
            messages: 0,
        })
    }
    pub fn append(&mut self, message: &nanus_domain::Message) -> LlmResult<()> {
        if self.messages > 0 {
            self.writer.write_all(b",").map_err(|_| refused())?;
        }
        serde_json::to_writer(&mut self.writer, message).map_err(|_| refused())?;
        self.messages = self.messages.checked_add(1).ok_or_else(refused)?;
        Ok(())
    }
    pub fn prefix(&self) -> LlmResult<String> {
        let mut writer = self.writer.clone();
        writer.write_all(b"]").map_err(|_| refused())?;
        Ok(format!("{:x}", writer.hash.finalize()))
    }
}
