// Test-only compilation of the single shared MIME sources and mail adapters.
#[allow(unused)]
#[path = "../../td-mime/src/attribute.rs"]
pub mod attribute;
#[allow(unused)]
#[path = "../../td-mime/src/base64.rs"]
pub mod base64;
#[allow(unused)]
#[path = "../../td-mime/src/body_charset.rs"]
pub mod body_charset;
#[allow(unused)]
#[path = "../../td-mime/src/body_lists.rs"]
pub mod body_lists;
#[allow(unused)]
#[path = "../../td-mime/src/charset.rs"]
pub mod charset;
#[allow(unused)]
pub mod clock;
#[allow(unused)]
#[path = "../../td-mime/src/content_id.rs"]
pub mod content_id;
#[allow(unused)]
#[path = "../../td-mime/src/decode_work.rs"]
mod decode_work;
#[allow(unused)]
#[path = "../../td-mime/src/delimiter.rs"]
pub mod delimiter;
#[allow(unused)]
#[path = "../../td-mime/src/encoded_word.rs"]
pub mod encoded_word;
#[allow(unused)]
#[path = "../../td-mime/src/fields.rs"]
pub mod fields;
#[allow(unused)]
#[path = "../../td-mime/src/filename.rs"]
pub mod filename;
#[allow(unused)]
#[path = "../../td-mime/src/header_addr_spec.rs"]
pub mod header_addr_spec;
#[allow(unused)]
#[path = "../../td-mime/src/header_address_items.rs"]
pub mod header_address_items;
#[allow(unused)]
#[path = "../../td-mime/src/header_address_text.rs"]
pub mod header_address_text;
#[allow(unused)]
#[path = "../../td-mime/src/header_addresses.rs"]
pub mod header_addresses;
#[allow(unused)]
#[path = "../../td-mime/src/header_cfws.rs"]
pub mod header_cfws;
#[allow(unused)]
#[path = "../../td-mime/src/header_comment.rs"]
pub mod header_comment;
#[allow(unused)]
#[path = "../../td-mime/src/header_date.rs"]
pub mod header_date;
#[allow(unused)]
#[path = "../../td-mime/src/header_delimited.rs"]
pub mod header_delimited;
#[allow(unused)]
#[path = "../../td-mime/src/header_mailbox.rs"]
pub mod header_mailbox;
#[allow(unused)]
#[path = "../../td-mime/src/header_message_ids.rs"]
pub mod header_message_ids;
#[allow(unused)]
#[path = "../../td-mime/src/header_name.rs"]
pub mod header_name;
#[allow(unused)]
#[path = "../../td-mime/src/header_phrase.rs"]
pub mod header_phrase;
#[allow(unused)]
#[path = "../../td-mime/src/header_property.rs"]
pub mod header_property;
#[allow(unused)]
#[path = "../../td-mime/src/header_raw.rs"]
pub mod header_raw;
#[allow(unused)]
#[path = "../../td-mime/src/header_select.rs"]
pub mod header_select;
#[allow(unused)]
#[path = "../../td-mime/src/header_text.rs"]
pub mod header_text;
#[allow(unused)]
#[path = "../../td-mime/src/header_urls.rs"]
pub mod header_urls;
#[allow(unused)]
#[path = "../../td-mime/src/header_value.rs"]
pub mod header_value;
#[allow(unused)]
#[path = "../../td-mime/src/header_work.rs"]
mod header_work;
#[allow(unused)]
#[path = "../../td-mime/src/headers.rs"]
pub mod headers;
#[allow(unused)]
#[path = "../../td-mime/src/json_string.rs"]
pub mod json_string;
#[allow(unused)]
#[path = "../../td-mime/src/label_fields.rs"]
pub mod label_fields;
#[allow(unused)]
#[path = "../../td-mime/src/language.rs"]
pub mod language;
#[allow(unused)]
#[path = "../../td-mime/src/location_field.rs"]
pub mod location_field;
#[allow(unused)]
#[path = "../../td-mime/src/location_fields.rs"]
pub mod location_fields;
#[allow(unused)]
#[path = "../../td-mime/src/location_literal.rs"]
pub mod location_literal;
#[allow(unused)]
#[path = "../../td-mime/src/location_literal_field.rs"]
pub mod location_literal_field;
#[allow(unused)]
#[path = "../../td-mime/src/location_selection.rs"]
pub mod location_selection;
#[allow(unused)]
#[path = "../../td-mime/src/location_word.rs"]
pub mod location_word;
#[allow(unused)]
#[path = "../../td-mime/src/metadata.rs"]
pub mod metadata;
#[allow(unused)]
pub mod mime_input;
#[allow(unused)]
pub mod mime_text;
#[allow(unused)]
pub mod mime_traversal;
#[allow(unused)]
#[path = "../../td-mime/src/nfc.rs"]
pub mod nfc;
#[allow(unused)]
#[path = "../../td-mime/src/parameter.rs"]
pub mod parameter;
#[allow(unused)]
#[path = "../../td-mime/src/parameter_value.rs"]
pub mod parameter_value;
#[allow(unused)]
#[path = "../../td-mime/src/part_headers.rs"]
pub mod part_headers;
#[allow(unused)]
#[path = "../../td-mime/src/quoted_printable.rs"]
pub mod quoted_printable;
#[allow(unused)]
#[path = "../../td-mime/src/structure.rs"]
pub mod structure;
#[allow(unused)]
#[path = "../../td-mime/src/unfold.rs"]
pub mod unfold;
#[allow(unused)]
#[path = "../../td-mime/src/unicode.rs"]
pub mod unicode;
#[allow(unused)]
use attribute as mime_attribute;
#[allow(unused)]
use base64 as mime_base64;
#[allow(unused)]
use body_lists as mime_body_lists;
#[allow(unused)]
use charset as mime_charset;
#[allow(unused)]
use content_id as mime_content_id;
#[allow(unused)]
use delimiter as mime_delimiter;
#[allow(unused)]
use fields as mime_fields;
#[allow(unused)]
use filename as mime_filename;
#[allow(unused)]
use headers as mime_headers;
#[allow(unused)]
use label_fields as mime_label_fields;
#[allow(unused)]
use language as mime_language;
#[allow(unused)]
use location_field as mime_location_field;
#[allow(unused)]
use location_fields as mime_location_fields;
#[allow(unused)]
use location_literal as mime_location_literal;
#[allow(unused)]
use location_literal_field as mime_location_literal_field;
#[allow(unused)]
use location_selection as mime_location_selection;
#[allow(unused)]
use location_word as mime_location_word;
#[allow(unused)]
use metadata as mime_metadata;
#[allow(unused)]
use parameter as mime_parameter;
#[allow(unused)]
use parameter_value as mime_value;
#[allow(unused)]
use part_headers as mime_part_headers;
#[allow(unused)]
use quoted_printable as mime_qp;
use td_mime::{buffer, time, work};
#[allow(unused)]
use unfold as mime_unfold;
