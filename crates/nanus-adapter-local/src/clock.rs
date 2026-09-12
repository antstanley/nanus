//! The system clock adapter: an implementation of [`nanus_ports::ClockPort`].

use core::cell::Cell;
use std::time::{SystemTime, UNIX_EPOCH};

use nanus_ports::ClockPort;

/// A clock backed by the operating system's wall clock.
///
/// Wall-clock time is not monotonic — an NTP correction or a manual change can
/// move it backwards — and a session log whose timestamps went backwards would
/// sort wrongly. The adapter therefore **clamps**: it never reports a value below
/// the previous one, which is what the port's documentation asks of an
/// implementation.
///
/// Duration measurement is a different job and does not belong here: the shell
/// adapter uses `Instant` for that, because elapsed time must not be affected by
/// a wall-clock correction at all.
#[derive(Debug, Default)]
pub struct SystemClock {
    /// The highest value ever reported, used to clamp a backwards clock.
    last_ms: Cell<u64>,
}

impl SystemClock {
    /// Creates a clock reading the system wall clock.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            last_ms: Cell::new(0),
        }
    }

    /// Shares this clock as the handle a kernel plugin publishes.
    #[must_use]
    pub fn handle(self) -> nanus_ports::ClockHandle {
        std::rc::Rc::new(Box::new(self))
    }

    /// Reads the wall clock, or `0` on a machine set before the epoch.
    fn wall_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| {
                u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
            })
    }
}

impl ClockPort for SystemClock {
    fn now_ms(&self) -> u64 {
        let now = Self::wall_ms();
        let last = self.last_ms.get();
        let clamped = if now < last { last } else { now };
        self.last_ms.set(clamped);
        // Postcondition: a clamped reading is never below the one before it.
        assert!(clamped >= last, "the clock never runs backwards");
        clamped
    }
}
