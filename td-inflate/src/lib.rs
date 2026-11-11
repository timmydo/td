//! Shared bounded gzip, zlib and raw DEFLATE decoding.
#[path = "../../engine/src/crc32.rs"]
mod crc32;
mod gzip;
pub use gzip::*;
