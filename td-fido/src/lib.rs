//! td-fido: td's dependency-free FIDO2 client (td-fido/DESIGN.md). CTAP
//! HID framing, CBOR, the CTAP codecs, the PIN protocols with hmac-secret,
//! software P-256 and AES, and hidraw admission with its worker. Pure
//! `std`; it has no `unsafe`, and it owns no store, record or policy.
#![forbid(unsafe_code)]
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic
    )
)]

mod crypto;
mod root;
pub use crypto::hmac_sha256;
pub mod fido_aes;
pub mod fido_cbor;
pub mod fido_ctap;
pub mod fido_device;
pub mod fido_enroll;
pub mod fido_hid;
pub mod fido_p256;
pub mod fido_pin;
pub mod fido_transaction;

/// The test build's P-256, whose signing the virtual authenticator uses:
/// td-secret's tests compile `fido_p256.rs` by path under this name, as
/// production never signs.
#[cfg(test)]
use fido_p256 as p256_signer;

// The test-only authenticator side. td-secret's tests compile these two
// files by path as well, so they name only public items through `crate::`
// paths both crates resolve, and carry no tests of their own. The virtual
// authenticator's tests are a separate file this crate alone includes as
// its child, keeping their access to its private items.
#[cfg(test)]
#[allow(dead_code, reason = "td-secret's login tests configure keys")]
mod fido_virtual {
    include!("fido_virtual.rs");

    mod tests {
        include!("fido_virtual_tests.rs");
    }
}
#[cfg(test)]
mod fido_fixtures;
