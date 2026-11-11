//! Shared bounded gzip, zlib and raw DEFLATE decoding.
pub use td_alloc as allocation;
#[path = "../../engine/src/crc32.rs"]
mod crc32;
mod gzip;
pub use gzip::*;
