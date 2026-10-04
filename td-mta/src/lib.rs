//! Service foundations only: no listeners, protocol handlers or capabilities.
#![forbid(unsafe_code)]

pub mod admission;
pub mod body_charset;
pub mod body_value;
pub mod bounded;
pub mod change_cursor;
pub mod clock;
pub mod config;
mod decode_work;
pub mod encoded_word;
pub mod format;
pub mod frame_changes;
pub mod gateway_policy;
pub mod generations;
pub mod header_addr_spec;
pub mod header_address_items;
pub mod header_address_text;
pub mod header_addresses;
pub mod header_cfws;
pub mod header_comment;
pub mod header_date;
pub mod header_delimited;
pub mod header_mailbox;
pub mod header_message_ids;
pub mod header_name;
pub mod header_phrase;
pub mod header_property;
pub mod header_raw;
pub mod header_select;
pub mod header_text;
pub mod header_urls;
pub mod header_value;
mod header_work;
pub mod ids;
pub mod json_string;
pub mod limits;
pub mod mailbox_parents;
pub mod mailbox_sweep;
pub mod merge;
pub mod mime_attribute;
pub mod mime_base64;
pub mod mime_body_lists;
pub mod mime_charset;
pub mod mime_content_id;
pub mod mime_delimiter;
pub mod mime_fields;
pub mod mime_filename;
pub mod mime_headers;
pub mod mime_input;
pub mod mime_language;
pub mod mime_location_literal;
pub mod mime_location_selection;
pub mod mime_location_word;
pub mod mime_metadata;
pub mod mime_parameter;
pub mod mime_part_headers;
pub mod mime_qp;
pub mod mime_text;
pub mod mime_traversal;
pub mod mime_unfold;
pub mod mime_value;
pub mod nfc;
pub mod observability;
pub mod overlay;
pub mod ownership;
pub mod ports;
pub mod recipient_sweep;
pub mod reference_sweep;
pub mod row_references;
pub mod smtp_wire;
pub mod store_fs;
pub mod store_paths;
pub mod sync;
pub mod tls_admission;
pub mod tls_io;
pub mod tls_policy;
pub mod transport;
pub mod unicode;
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
