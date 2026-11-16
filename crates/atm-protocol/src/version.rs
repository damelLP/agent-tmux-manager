//! Protocol versioning for safe upgrades.

use serde::{Deserialize, Serialize};
use std::fmt;
use thiserror::Error;

/// Protocol version for client-daemon communication.
///
/// Uses semantic versioning: major.minor
/// - Major version bump: breaking changes, incompatible
/// - Minor version bump: additive changes, backward compatible
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolVersion {
    pub major: u16,
    pub minor: u16,
}

impl ProtocolVersion {
    /// Current protocol version.
    pub const CURRENT: ProtocolVersion = ProtocolVersion { major: 1, minor: 1 };

    /// Creates a new ProtocolVersion.
    pub const fn new(major: u16, minor: u16) -> Self {
        Self { major, minor }
    }

    /// Parses a version string like "1.0".
    pub fn parse(s: &str) -> Result<Self, VersionError> {
        let parts: Vec<&str> = s.split('.').collect();
        if parts.len() != 2 {
            return Err(VersionError::InvalidFormat(s.to_string()));
        }

        let major = parts
            .first()
            .ok_or_else(|| VersionError::InvalidFormat(s.to_string()))?
            .parse::<u16>()
            .map_err(|_| VersionError::InvalidFormat(s.to_string()))?;

        let minor = parts
            .get(1)
            .ok_or_else(|| VersionError::InvalidFormat(s.to_string()))?
            .parse::<u16>()
            .map_err(|_| VersionError::InvalidFormat(s.to_string()))?;

        Ok(Self { major, minor })
    }

    /// Returns true if this version is compatible with another.
    ///
    /// Compatibility rules:
    /// - Major versions must match
    /// - Any minor version is compatible within the same major version
    pub fn is_compatible_with(&self, other: &ProtocolVersion) -> bool {
        self.major == other.major
    }

    /// Returns true if this version is newer than another.
    pub fn is_newer_than(&self, other: &ProtocolVersion) -> bool {
        (self.major, self.minor) > (other.major, other.minor)
    }

    /// Returns true if this version is the current version.
    pub fn is_current(&self) -> bool {
        *self == Self::CURRENT
    }
}

impl Default for ProtocolVersion {
    fn default() -> Self {
        Self::CURRENT
    }
}

impl fmt::Display for ProtocolVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

/// Errors that can occur with version handling.
#[derive(Error, Debug, Clone)]
pub enum VersionError {
    #[error("Invalid version format: {0}")]
    InvalidFormat(String),

    #[error("Incompatible version: got {got}, expected {expected}")]
    Incompatible { got: String, expected: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_version_parse() {
        let v = ProtocolVersion::parse("1.0").unwrap();
        assert_eq!(v.major, 1);
        assert_eq!(v.minor, 0);
    }

    #[test]
    fn test_version_parse_invalid() {
        assert!(ProtocolVersion::parse("1").is_err());
        assert!(ProtocolVersion::parse("1.0.0").is_err());
        assert!(ProtocolVersion::parse("abc").is_err());
    }

    #[test]
    fn test_version_compatibility() {
        let v1_0 = ProtocolVersion::new(1, 0);
        let v1_1 = ProtocolVersion::new(1, 1);
        let v2_0 = ProtocolVersion::new(2, 0);

        assert!(v1_0.is_compatible_with(&v1_1));
        assert!(v1_1.is_compatible_with(&v1_0));
        assert!(!v1_0.is_compatible_with(&v2_0));
    }

    #[test]
    fn test_version_display() {
        let v = ProtocolVersion::new(1, 2);
        assert_eq!(format!("{v}"), "1.2");
    }
}
