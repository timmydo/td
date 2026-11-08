//! Message numbers (RFC 4250 section 4.1) and disconnect reasons.

pub const DISCONNECT: u8 = 1;
pub const IGNORE: u8 = 2;
pub const UNIMPLEMENTED: u8 = 3;
pub const DEBUG: u8 = 4;
pub const SERVICE_REQUEST: u8 = 5;
pub const SERVICE_ACCEPT: u8 = 6;
pub const EXT_INFO: u8 = 7;
pub const KEXINIT: u8 = 20;
pub const NEWKEYS: u8 = 21;
pub const KEX_ECDH_INIT: u8 = 30;
pub const KEX_ECDH_REPLY: u8 = 31;
pub const USERAUTH_REQUEST: u8 = 50;
pub const USERAUTH_FAILURE: u8 = 51;
pub const USERAUTH_SUCCESS: u8 = 52;
pub const USERAUTH_BANNER: u8 = 53;
pub const USERAUTH_PK_OK: u8 = 60;
pub const GLOBAL_REQUEST: u8 = 80;
pub const REQUEST_SUCCESS: u8 = 81;
pub const REQUEST_FAILURE: u8 = 82;
pub const CHANNEL_OPEN: u8 = 90;
pub const CHANNEL_OPEN_CONFIRMATION: u8 = 91;
pub const CHANNEL_OPEN_FAILURE: u8 = 92;
pub const CHANNEL_WINDOW_ADJUST: u8 = 93;
pub const CHANNEL_DATA: u8 = 94;
pub const CHANNEL_EXTENDED_DATA: u8 = 95;
pub const CHANNEL_EOF: u8 = 96;
pub const CHANNEL_CLOSE: u8 = 97;
pub const CHANNEL_REQUEST: u8 = 98;
pub const CHANNEL_SUCCESS: u8 = 99;
pub const CHANNEL_FAILURE: u8 = 100;

pub const DISCONNECT_PROTOCOL_ERROR: u32 = 2;
pub const DISCONNECT_KEY_EXCHANGE_FAILED: u32 = 3;
pub const DISCONNECT_BY_APPLICATION: u32 = 11;
pub const DISCONNECT_NO_MORE_AUTH_METHODS: u32 = 14;

pub const OPEN_ADMINISTRATIVELY_PROHIBITED: u32 = 1;
pub const OPEN_UNKNOWN_CHANNEL_TYPE: u32 = 3;
pub const OPEN_RESOURCE_SHORTAGE: u32 = 4;

pub const EXTENDED_DATA_STDERR: u32 = 1;

/// What may arrive in the middle of a peer's key exchange besides its own
/// messages: RFC 4253 section 7.1's transport-generic ones, which OpenSSH also
/// sends then. Not `EXT_INFO`, which RFC 8308 places only after a `NEWKEYS`.
pub fn is_generic_transport(number: u8) -> bool {
    matches!(number, IGNORE | UNIMPLEMENTED | DEBUG)
}

/// Message numbers strict KEX admits before the first `NEWKEYS` completes.
pub fn is_kex(number: u8) -> bool {
    matches!(number, KEXINIT | NEWKEYS | KEX_ECDH_INIT | KEX_ECDH_REPLY)
}
