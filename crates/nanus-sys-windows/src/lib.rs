//! Narrow Windows capabilities, empty on other platforms. No raw handles leave this crate.
#![forbid(unsafe_code)]

#[cfg(windows)]
mod sid;
#[cfg(windows)]
pub use sid::{SidError, current_user_sid};

#[cfg(windows)]
mod job;
#[cfg(windows)]
pub use job::{Job, JobError, is_process_alive};
