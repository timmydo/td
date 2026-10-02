//! Service foundations only: no listeners, protocol handlers or capabilities.
#![forbid(unsafe_code)]

pub mod admission;
pub mod bounded;
pub mod change_cursor;
pub mod clock;
pub mod config;
pub mod format;
pub mod frame_changes;
pub mod gateway_policy;
pub mod generations;
pub mod ids;
pub mod limits;
pub mod mailbox_parents;
pub mod merge;
pub mod observability;
pub mod overlay;
pub mod ownership;
pub mod ports;
pub mod smtp_wire;
pub mod store_fs;
pub mod store_paths;
pub mod sync;
pub mod tls_admission;
pub mod tls_io;
pub mod tls_policy;
pub mod transport;
pub mod wire;

/// Operator configuration schema, independent of the future storage format.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConfigVersion(u16);

impl ConfigVersion {
    pub const CURRENT: Self = Self(1);

    pub fn parse(value: u16) -> Result<Self, UnsupportedConfigVersion> {
        if value == Self::CURRENT.0 {
            Ok(Self::CURRENT)
        } else {
            Err(UnsupportedConfigVersion(value))
        }
    }

    pub const fn number(self) -> u16 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UnsupportedConfigVersion(pub u16);

impl std::fmt::Display for UnsupportedConfigVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unsupported configuration version {}", self.0)
    }
}

impl std::error::Error for UnsupportedConfigVersion {}

#[cfg(test)]
mod tls_memory_process_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_version_does_not_accept_future_or_unversioned_input() {
        assert_eq!(ConfigVersion::parse(1), Ok(ConfigVersion::CURRENT));
        assert_eq!(ConfigVersion::CURRENT.number(), 1);
        assert_eq!(ConfigVersion::parse(0), Err(UnsupportedConfigVersion(0)));
        assert_eq!(
            ConfigVersion::parse(u16::MAX),
            Err(UnsupportedConfigVersion(u16::MAX))
        );
    }
}
