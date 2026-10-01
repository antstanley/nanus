//! The call chain, and the tests that hold it to a real host.

use winsafe::{self as w, co};

/// Why the current user's SID could not be read.
#[derive(Debug, PartialEq, Eq)]
pub enum SidError {
    /// A Win32 call failed; carries the system's message.
    Os(String),
    /// The token answered with something other than the user it was asked for.
    NotAUser,
    /// The token's user has no SID.
    NoSid,
}

/// Returns the current process's user SID in its string form, such as `S-1-5-21-…-1001`.
///
/// # Errors
///
/// Returns [`SidError`] when the token cannot be opened or read, or has no SID.
pub fn current_user_sid() -> Result<String, SidError> {
    let token = w::HPROCESS::GetCurrentProcess()
        .OpenProcessToken(co::TOKEN::QUERY)
        .map_err(|error| SidError::Os(error.to_string()))?;
    let info = token
        .GetTokenInformation(co::TOKEN_INFORMATION_CLASS::User)
        .map_err(|error| SidError::Os(error.to_string()))?;
    let w::TokenInfo::User(user) = info else {
        return Err(SidError::NotAUser);
    };
    user.User
        .Sid()
        .map(|sid| sid.to_string())
        .ok_or(SidError::NoSid)
}

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
