//! The shipped safe wrapper, exercised on a native Windows host.

pub use nanus_sys_windows::{SidError, current_user_sid};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_current_user_has_a_well_formed_sid() {
        let sid = current_user_sid().unwrap_or_else(|error| panic!("{error:?}"));
        assert!(sid.starts_with("S-1-"), "{sid}");
        // The only characters a pipe name built from it can be handed.
        assert!(
            sid.chars()
                .all(|c| c == 'S' || c == '-' || c.is_ascii_digit()),
            "{sid}"
        );
        // `S-1-5-18` is the shortest SID a real account has.
        assert!(sid.len() >= "S-1-5-18".len(), "{sid}");
    }

    #[test]
    fn the_sid_is_the_same_every_time_it_is_asked_for() {
        // A client and a server compute the pipe name independently and never exchange it,
        // so a value that varied between calls would be two endpoints that never meet.
        let first = current_user_sid().unwrap_or_else(|error| panic!("{error:?}"));
        let second = current_user_sid().unwrap_or_else(|error| panic!("{error:?}"));
        assert_eq!(first, second);
    }

    #[test]
    fn a_different_sid_is_not_this_one() {
        // The negative direction: LocalSystem is `S-1-5-18`, and an agent started by a
        // person must not be named as if it were the system's.
        let sid = current_user_sid().unwrap_or_else(|error| panic!("{error:?}"));
        assert_ne!(sid, "S-1-5-18", "the probe ran as LocalSystem");
    }
}
