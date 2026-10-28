//! td-luks: td's LUKS2 volume tool (DESIGN.md). Its library holds the
//! bounded LUKS2 header reader at the format level, on which td-protector
//! layers td's token semantics, and the writer with its keyslot and token
//! operations. Pure `std`; it has no `unsafe`.
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

// Base64 for salts and digests: td's shared encoding, mounted as
// td-firstboot mounts it so the boot recipes compile no further rlib.
#[path = "../../td-encoding/src/lib.rs"]
#[allow(dead_code, reason = "td-luks uses base64 alone")]
mod encoding;

pub mod header;
pub mod keyslot;
pub mod write;
