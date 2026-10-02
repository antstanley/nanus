//! # nanus-tool-video
//!
//! The optional `read_video` tool. FFprobe inspects a local video and FFmpeg samples up to
//! four timestamped JPEGs from a window of at most sixty seconds. What happens next depends on
//! who is asking:
//!
//! - a model with **verified image input** receives the stills themselves;
//! - any other model receives a text interpretation from a verified vision model **on the same
//!   provider, plan and credential**.
//!
//! The main conversation model never changes, and the video never leaves the machine: only
//! stills do, and only to the provider already in use.
//!
//! This crate is not part of the stock seven tools. A host installs it explicitly by
//! registering [`read_video_tool`] beside them; `nanus-bundle` does so when the configuration
//! asks for it. The crate depends on the domain and ports only, so the minimal runner never
//! pulls in a decoder, and the extension never names a provider.
//!
//! ## What is bounded
//!
//! | Bound | Value |
//! |---|---|
//! | Source copy | 128 MiB, one hour, 8192 px per edge, 33,554,432 px |
//! | Window / frames | 60 s / 4 JPEGs of at most 512 KiB, long edge at most 1024 px |
//! | Decoder | 30 s per process, output capped, cleared environment, no shell |
//! | Whole call | 180 s |
//! | Result text | 32 KiB: manifest, labels and answer together |
//!
//! ## What is not claimed
//!
//! Audio is omitted, and the tool sees sampled instants: the manifest says so in every result.
//! An aggregate admission ledger across a batch of tool results is not implemented; the
//! per-image and per-request caps the runner already enforces apply, and a request that cannot
//! fit is refused by them.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
// FFmpeg and FFprobe are product names that appear in most doc comments; backticking each
// would make the prose unreadable and says nothing about code.
#![allow(clippy::doc_markdown)]

pub mod analyzer;
pub mod args;
mod error;
pub mod ffmpeg;
mod media;
pub mod source;
mod tool;

pub use analyzer::{AnalysisBudget, LlmAnalyzer};
pub use args::{RequestedMode, VideoReadArguments};
pub use error::VideoError;
pub use ffmpeg::FfmpegDecoder;
pub use media::{
    Analysis, AnalysisRequest, AnalysisUsage, MediaInfo, Provenance, Sample, SampledFrame,
    Snapshot, VideoAnalyzer, VideoDecoder, VideoRouting, VideoSource, Window,
};
pub use source::FsSource;
pub use tool::{VideoServices, read_video_tool};
