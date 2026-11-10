//! How long a model request may wait, shared by the adapters that make one.
//!
//! A provider that accepts a connection and then says nothing is indistinguishable, to a
//! client with no deadline, from one that is still thinking: a headless run waits on it for
//! ever, and a detached agent's turn never ends. Two waits are bounded, and they are different
//! waits. Reaching the host is quick or it is not going to happen. Hearing from it *again* is
//! the one that needs room: a model reasoning at a high effort can be silent for minutes
//! between two frames of one healthy response, so the bound is on the silence, never on the
//! response as a whole — a long answer that keeps arriving is not late.

use core::time::Duration;

/// How long a connection may take to be made, by default.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a response may go without a byte, by default.
///
/// Five minutes: longer than the longest silence a reasoning model has been seen to keep
/// inside one response, and short enough that a turn nobody is watching still ends.
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// The two waits a model request bounds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HttpTimeouts {
    connect: Duration,
    idle: Duration,
}

impl Default for HttpTimeouts {
    fn default() -> Self {
        Self {
            connect: DEFAULT_CONNECT_TIMEOUT,
            idle: DEFAULT_IDLE_TIMEOUT,
        }
    }
}

impl HttpTimeouts {
    /// Bounds a connection by `connect` and a silence by `idle`.
    ///
    /// A zero wait is one that has already passed, so every request would fail before it was
    /// made; a host that wants no bound has no way to say so here, on purpose.
    #[must_use]
    pub fn new(connect: Duration, idle: Duration) -> Self {
        assert!(!connect.is_zero(), "a connection is given time to be made");
        assert!(!idle.is_zero(), "a response is given time to arrive");
        Self { connect, idle }
    }

    /// How long a connection may take to be made.
    #[must_use]
    pub const fn connect(self) -> Duration {
        self.connect
    }

    /// How long a response may go without a byte — before its head, and between any two reads
    /// of its body.
    #[must_use]
    pub const fn idle(self) -> Duration {
        self.idle
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_bound_both_waits_and_leave_room_for_a_silent_model() {
        let timeouts = HttpTimeouts::default();
        assert_eq!(timeouts.connect(), DEFAULT_CONNECT_TIMEOUT);
        assert_eq!(timeouts.idle(), DEFAULT_IDLE_TIMEOUT);
        assert!(timeouts.idle() > timeouts.connect());
    }

    #[test]
    fn chosen_waits_are_kept_as_given() {
        let timeouts = HttpTimeouts::new(Duration::from_millis(5), Duration::from_millis(7));
        assert_eq!(timeouts.connect(), Duration::from_millis(5));
        assert_eq!(timeouts.idle(), Duration::from_millis(7));
    }

    #[test]
    #[should_panic(expected = "a response is given time to arrive")]
    fn a_wait_of_zero_is_refused() {
        let _unusable = HttpTimeouts::new(Duration::from_secs(1), Duration::ZERO);
    }
}
