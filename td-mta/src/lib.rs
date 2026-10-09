//! Service foundations only: no listeners, protocol handlers or capabilities.
#![forbid(unsafe_code)]

pub mod admission;
pub use td_mime::body_charset;
pub mod body_properties;
pub mod body_property;
pub mod body_value;
pub mod bounded;
pub mod clock;
pub mod config;
pub use td_mime::encoded_word;
pub mod format;
pub mod gateway_policy;
pub mod generations;
pub use td_mime::header_addr_spec;
pub use td_mime::header_address_items;
pub use td_mime::header_address_text;
pub use td_mime::header_addresses;
pub use td_mime::header_cfws;
pub use td_mime::header_comment;
pub use td_mime::header_date;
pub use td_mime::header_delimited;
pub use td_mime::header_mailbox;
pub use td_mime::header_message_ids;
pub use td_mime::header_name;
pub use td_mime::header_phrase;
pub use td_mime::header_property;
pub use td_mime::header_raw;
pub use td_mime::header_select;
pub use td_mime::header_text;
pub use td_mime::header_urls;
pub use td_mime::header_value;
pub mod ids;
pub use td_mime::json_string;
pub mod limits;
pub mod mailbox_parents;
pub mod mailbox_sweep;
pub mod metadata_sweep;
pub use td_mime::attribute as mime_attribute;
pub use td_mime::base64 as mime_base64;
pub use td_mime::body_lists as mime_body_lists;
pub use td_mime::charset as mime_charset;
pub use td_mime::content_id as mime_content_id;
pub use td_mime::delimiter as mime_delimiter;
pub use td_mime::fields as mime_fields;
pub use td_mime::filename as mime_filename;
pub use td_mime::headers as mime_headers;
pub mod mime_input;
pub use td_mime::label_fields as mime_label_fields;
pub use td_mime::language as mime_language;
pub use td_mime::location_field as mime_location_field;
pub use td_mime::location_fields as mime_location_fields;
pub use td_mime::location_literal as mime_location_literal;
pub use td_mime::location_literal_field as mime_location_literal_field;
pub use td_mime::location_selection as mime_location_selection;
pub use td_mime::location_word as mime_location_word;
pub use td_mime::metadata as mime_metadata;
pub use td_mime::parameter as mime_parameter;
pub use td_mime::part_headers as mime_part_headers;
pub use td_mime::quoted_printable as mime_qp;
pub mod mime_text;
pub use td_mime::structure as mime_structure;
pub mod mime_response;
pub use td_mime::nfc;
pub use td_mime::parameter_value as mime_value;
pub use td_mime::unfold as mime_unfold;
pub mod observability;
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
pub use td_mime::unicode;
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
