//! Identity types for kernel participants.
//!
//! Plugin and service names are part of the framework's public contract: they are
//! referenced from configuration, from log output, and from test assertions, so
//! they are validated at construction rather than trusted.

use core::fmt;

/// Maximum length of a plugin id or service name.
///
/// Names travel through configuration files and diagnostic output; an unbounded
/// name is a denial-of-service vector for anything that renders it.
pub const NAME_MAX_LEN: usize = 96;

/// Maximum length of a plugin version string.
pub const VERSION_MAX_LEN: usize = 32;

/// A validated, non-empty, lowercase-and-kebab identifier.
///
/// Used for both plugin ids and service names, because the two share the same
/// constraints: stable, printable, configuration-safe, and log-safe.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Name(&'static str);

impl Name {
    /// Builds a validated [`Name`].
    ///
    /// # Errors
    ///
    /// Returns [`crate::Error::InvalidName`] when the name is empty, longer than
    /// [`NAME_MAX_LEN`], or contains a byte outside `[a-z0-9_.-]`.
    pub fn new(raw: &'static str) -> Result<Self, crate::Error> {
        let len = raw.len();
        if len == 0 {
            return Err(crate::Error::InvalidName {
                name: raw,
                reason: "empty",
            });
        }
        if len > NAME_MAX_LEN {
            return Err(crate::Error::InvalidName {
                name: raw,
                reason: "too long",
            });
        }
        // Negative space: reject anything that would be ambiguous in a config
        // file, a file name, or a log line.
        let bad = raw
            .bytes()
            .find(|b| !matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-'));
        if bad.is_some() {
            return Err(crate::Error::InvalidName {
                name: raw,
                reason: "bad character",
            });
        }
        // Postcondition: the accepted name is non-empty and within bounds.
        assert!(!raw.is_empty());
        assert!(raw.len() <= NAME_MAX_LEN);
        Ok(Self(raw))
    }

    /// Returns the name as a string slice.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        self.0
    }

    /// Builds a name from text the crate has already validated.
    ///
    /// Exists only so a `const` can hold a name whose validity is asserted by a
    /// test rather than by `Name::new`, which is not usable in a `const` on stable
    /// Rust. Every caller inside this crate is paired with such a test.
    pub(crate) const fn new_unchecked(raw: &'static str) -> Self {
        Self(raw)
    }
}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl fmt::Debug for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Name({})", self.0)
    }
}

/// A validated version string, e.g. `0.1.0`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version(&'static str);

impl Version {
    /// Builds a validated [`Version`].
    ///
    /// # Errors
    ///
    /// Returns [`crate::Error::InvalidName`] when the version is empty, longer
    /// than [`VERSION_MAX_LEN`], or contains whitespace or control bytes.
    pub fn new(raw: &'static str) -> Result<Self, crate::Error> {
        if raw.is_empty() {
            return Err(crate::Error::InvalidName {
                name: raw,
                reason: "empty version",
            });
        }
        if raw.len() > VERSION_MAX_LEN {
            return Err(crate::Error::InvalidName {
                name: raw,
                reason: "version too long",
            });
        }
        // Negative space: no whitespace, no control bytes, no non-ASCII.
        let bad = raw
            .bytes()
            .any(|b| b.is_ascii_whitespace() || !b.is_ascii_graphic());
        if bad {
            return Err(crate::Error::InvalidName {
                name: raw,
                reason: "bad version character",
            });
        }
        Ok(Self(raw))
    }

    /// Returns the version as a string slice.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        self.0
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl fmt::Debug for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Version({})", self.0)
    }
}

/// Identity of a mounted plugin.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PluginId(Name);

impl PluginId {
    /// Builds a plugin id.
    ///
    /// # Errors
    ///
    /// Returns [`crate::Error::InvalidName`] under the same rules as [`Name::new`].
    pub fn new(raw: &'static str) -> Result<Self, crate::Error> {
        Ok(Self(Name::new(raw)?))
    }

    /// Returns the id as a string slice.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        self.0.as_str()
    }

    /// Returns the underlying validated name.
    #[must_use]
    pub const fn name(&self) -> Name {
        self.0
    }

    /// Builds an id from a name the crate has already validated.
    ///
    /// Exists so the kernel can name itself in a `const`. The precondition is
    /// enforced by [`crate::ident::tests::kernel_id_is_valid`], which builds the
    /// same id through the validating path.
    pub(crate) const fn from_validated(name: Name) -> Self {
        Self(name)
    }
}

impl fmt::Display for PluginId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl fmt::Debug for PluginId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PluginId({})", self.0)
    }
}

/// A validated service key name, as published in the service registry.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ServiceName(Name);

impl ServiceName {
    /// Wraps an already-validated [`Name`].
    #[must_use]
    pub const fn from_name(name: Name) -> Self {
        Self(name)
    }

    /// Returns the name as a string slice.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        self.0.as_str()
    }
}

impl fmt::Display for ServiceName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl fmt::Debug for ServiceName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ServiceName({})", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kernel_id_is_valid() {
        // Pairs with `Name::new_unchecked`: whatever the kernel builds unchecked
        // must also pass validation, so the two paths cannot drift.
        assert!(Name::new("kernel").is_ok());
        assert_eq!(Name::new_unchecked("kernel").as_str(), "kernel");
        assert!(PluginId::new("kernel").is_ok());
    }

    #[test]
    fn name_accepts_dotted_kebab() {
        // Positive space: the shapes the shipped plugins actually use.
        assert!(Name::new("agent-loop").is_ok());
        assert!(Name::new("nanus.tool.read").is_ok());
        assert!(Name::new("a").is_ok());
        assert!(Name::new("0").is_ok());
    }

    #[test]
    fn name_rejects_empty_and_bad_bytes() {
        // Negative space: every rejected shape must produce a typed error, not a
        // panic and not a silently accepted value.
        assert!(Name::new("").is_err());
        assert!(Name::new("Agent").is_err());
        assert!(Name::new("agent loop").is_err());
        assert!(Name::new("agent/loop").is_err());
        assert!(Name::new("agent\nloop").is_err());
        assert!(Name::new("agênt").is_err());
    }

    #[test]
    fn name_length_boundary() {
        // Validity boundary: one below, at, and one above the limit.
        let at_limit: &'static str = Box::leak("a".repeat(NAME_MAX_LEN).into_boxed_str());
        let below: &'static str = Box::leak("a".repeat(NAME_MAX_LEN - 1).into_boxed_str());
        let above: &'static str = Box::leak("a".repeat(NAME_MAX_LEN + 1).into_boxed_str());
        assert!(Name::new(below).is_ok());
        assert!(Name::new(at_limit).is_ok());
        assert!(Name::new(above).is_err());
    }

    #[test]
    fn version_rejects_whitespace() {
        assert!(Version::new("0.1.0").is_ok());
        assert!(Version::new("").is_err());
        assert!(Version::new("0.1.0 beta").is_err());
        assert!(Version::new("0.1.0\n").is_err());
    }

    #[test]
    fn plugin_id_round_trips() {
        let id = PluginId::new("tool-read");
        assert!(id.is_ok());
        let Ok(id) = id else {
            return;
        };
        assert_eq!(id.as_str(), "tool-read");
        assert_eq!(id.to_string(), "tool-read");
    }
}
