//! Service foundations only: no listeners, protocol handlers or capabilities.
#![deny(unsafe_code)]

#[forbid(unsafe_code)]
pub mod admission;
#[forbid(unsafe_code)]
pub mod bounded;
#[forbid(unsafe_code)]
pub mod clock;
#[forbid(unsafe_code)]
pub mod config;
#[forbid(unsafe_code)]
pub mod format;
#[forbid(unsafe_code)]
pub mod gateway_policy;
#[forbid(unsafe_code)]
pub mod generations;
#[forbid(unsafe_code)]
pub mod ids;
#[forbid(unsafe_code)]
pub mod limits;
#[forbid(unsafe_code)]
pub mod observability;
#[forbid(unsafe_code)]
pub mod ownership;
#[forbid(unsafe_code)]
pub mod ports;
#[forbid(unsafe_code)]
pub mod smtp_wire;
#[forbid(unsafe_code)]
pub mod store_fs;
mod store_fs_sys;
#[forbid(unsafe_code)]
pub mod store_paths;
#[forbid(unsafe_code)]
pub mod sync;
#[forbid(unsafe_code)]
pub mod tls_admission;
#[forbid(unsafe_code)]
pub mod tls_io;
#[forbid(unsafe_code)]
pub mod tls_policy;
#[forbid(unsafe_code)]
pub mod transport;
#[forbid(unsafe_code)]
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
#[forbid(unsafe_code)]
mod tls_memory_process_tests;

#[cfg(test)]
#[forbid(unsafe_code)]
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
