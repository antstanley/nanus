//! The clock port.
//!
//! A single method, and it is a port rather than a call to `SystemTime::now()`
//! for one reason: the harness needs a reproducible time in tests and in
//! replay. A turn machine, a session header, and a timeout policy all read the
//! clock, and a test that cannot pin it cannot assert anything about them.
//!
//! The method is synchronous. There is nothing to await in reading a clock, and
//! an `async fn now_ms` would force every caller into a boxed future for no
//! benefit.

use std::rc::Rc;

/// A shared, key-addressable clock.
pub type ClockHandle = Rc<Box<dyn ClockPort>>;

/// The clock port.
pub trait ClockPort {
    /// Returns milliseconds since the Unix epoch.
    ///
    /// A clock that runs backwards would make a session log non-monotonic, so an
    /// implementation is expected to clamp rather than report a decreasing
    /// value.
    fn now_ms(&self) -> u64;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A clock that never moves, so a test can assert on exact values.
    struct Fixed(u64);

    impl ClockPort for Fixed {
        fn now_ms(&self) -> u64 {
            self.0
        }
    }

    #[test]
    fn a_clock_handle_is_shared_and_wraps_the_trait_object() {
        // The `Rc<Box<dyn _>>` shape is what the kernel's registry can recover
        // from `dyn Any`, so it is asserted here as well as documented.
        let handle: ClockHandle = Rc::new(Box::new(Fixed(1_700_000_000_000)));
        assert_eq!(handle.now_ms(), 1_700_000_000_000);
        let second = Rc::clone(&handle);
        assert_eq!(Rc::strong_count(&handle), 2);
        assert_eq!(second.now_ms(), handle.now_ms());
    }
}
