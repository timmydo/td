//! Service foundations and direct SMTP receiving; no automatic listener activation.
#![forbid(unsafe_code)]

pub mod admission;
pub use td_mime::body_charset;
pub mod blob_body_json;
pub mod blob_body_lists;
pub mod blob_body_value;
pub mod blob_body_values;
pub mod blob_delimiters;
pub mod blob_entity;
pub mod blob_part;
pub mod blob_part_json;
pub mod blob_part_locators;
pub mod blob_preview;
pub mod blob_preview_selection;
pub mod blob_selected_metadata;
pub mod blob_selected_preview;
pub mod blob_structure;
pub mod blob_value_parts;
pub mod body_properties;
pub mod body_property;
pub mod body_value;
pub mod body_value_options;
pub mod bounded;
pub mod clock;
pub mod compose_text;
pub mod config;
pub mod dns_lookup;
pub mod dns_owner;
pub mod dns_reply;
pub mod dns_transport;
pub mod dns_wire;
pub mod draft_input;
pub mod email_preview;
pub mod email_properties;
pub mod email_property;
pub mod email_response;
pub mod postgres_queue_candidates;
pub mod postgres_queue_expiry;
pub mod postgres_submission_cancel;
pub mod queue_clock;
pub use td_mime::encoded_word;
pub mod external_sort;
pub mod filter_logic;
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
pub mod html_text;
pub mod http_auth;
pub mod http_request;
pub mod https;
pub mod https_api;
pub mod https_download;
mod https_exchange;
pub mod https_upload;
pub mod identity_catalog;
pub mod ids;
pub use td_mime::json_string;
pub mod jmap_changes;
pub mod jmap_date;
pub mod jmap_events;
mod jmap_number;
pub mod jmap_request;
pub mod jmap_session;
pub mod limits;
pub mod mailbox_filter;
pub mod mailbox_order;
pub mod mailbox_parents;
pub mod mailbox_response;
pub mod mailbox_sweep;
pub mod mailbox_tree;
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
pub mod operator_files;
pub mod outbound_dial;
pub mod ownership;
pub mod ports;
pub mod postgres_account_audit;
pub mod postgres_accounts;
pub mod postgres_attempt_phases;
pub mod postgres_attempt_recovery;
pub mod postgres_attempts;
pub mod postgres_blob_audit;
pub mod postgres_body;
pub mod postgres_bootstrap;
pub mod postgres_dispatch;
pub mod postgres_failure_notice;
mod postgres_final_capacity;
pub mod postgres_http_api;
pub mod postgres_http_auth;
pub mod postgres_http_cancellations;
pub mod postgres_http_drafts;
pub mod postgres_http_mailboxes;
pub mod postgres_http_submissions;
pub mod postgres_http_updates;
mod postgres_http_upload_receive;
pub mod postgres_http_uploads;
pub mod postgres_input;
pub mod postgres_mailboxes;
pub mod postgres_maintenance;
pub mod postgres_materials;
pub mod postgres_metadata_audit;
pub mod postgres_probe;
pub mod postgres_profile_materials;
pub mod postgres_read;
pub mod postgres_recovery;
pub mod postgres_relay_attempt;
pub mod postgres_relay_body;
pub mod postgres_relay_connect;
pub mod postgres_relay_data;
pub mod postgres_relay_finish;
pub mod postgres_relay_mail;
pub mod postgres_relay_rcpt;
pub mod postgres_relay_worker;
pub mod postgres_resolver;
pub mod postgres_route_health;
pub mod postgres_route_owners;
pub mod postgres_route_repair;
pub mod postgres_route_success;
pub mod postgres_routes;
pub mod postgres_runtime_access;
pub mod postgres_runtime_profile;
pub mod postgres_schema;
pub mod postgres_selection;
mod postgres_sequence;
mod postgres_snapshot;
pub mod postgres_submission_input;
pub mod postgres_submissions;
pub mod preview_json;
pub mod property_selection;
pub mod recipient_sweep;
pub mod reference_sweep;
pub mod root_headers;
pub mod row_references;
pub mod smtp_network;
pub mod smtp_receiving;
pub mod smtp_session;
pub mod smtp_starttls;
pub mod smtp_storage;
pub mod smtp_wire;
pub mod store_fs;
pub mod store_paths;
pub mod sync;
pub mod tls_admission;
pub mod tls_io;
pub mod tls_policy;
pub mod transmitted_copy;
pub mod transmitted_headers;
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

pub mod submission_envelope;
pub mod submission_request;

mod device_credentials;

pub mod import_snapshot;
mod postgres_change_row;
pub mod postgres_changes;
pub mod postgres_configuration;
pub mod postgres_configuration_store;
pub mod postgres_delivery_body;
pub mod postgres_devices;
pub mod postgres_drafts;
pub mod postgres_email_set;
pub mod postgres_history_audit;
pub mod postgres_imports;
mod postgres_publication;
pub mod postgres_receiving;
pub mod postgres_smtp_receiving;
pub mod postgres_smtp_storage;
pub mod postgres_uploads;
pub mod postgres_usage_audit;

mod mailbox_name;
pub mod postgres_mailbox_set;
mod receiving_headers;

pub mod smtp_data_encode;
mod smtp_ehlo;
pub mod smtp_relay_auth;
pub mod smtp_relay_body;
pub mod smtp_relay_command;
pub mod smtp_relay_ehlo;

pub mod application_log;
pub mod postgres_logs;

pub mod acme;

pub mod postgres_acme;

pub mod account_configuration;
pub mod postgres_account_admin;

pub mod outbound_http;

pub mod migration;
