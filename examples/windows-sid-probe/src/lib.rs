//! Reads the current user's SID through `winsafe`, with no `unsafe` of our own.
//!
//! Empty on every platform but Windows, so the probe builds everywhere and means something
//! only where the SID exists.

#[cfg(windows)]
mod sid;

#[cfg(windows)]
pub use sid::{SidError, current_user_sid};
