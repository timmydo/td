#![forbid(unsafe_code)]
//! Shared AV1 arithmetic used by td's picture codecs.
//! The bounded decoder admits hostile still-picture streams; the encoder
//! and arithmetic operate on caller-validated geometry.

pub mod cdef;
pub mod cdf;
pub mod deblock;
pub mod intra;
pub mod transform;

pub use intra::decode::{
    decode, header, DecodeError as Error, DecodeHeader as Header, Decoded as Yuv8,
    MAX_IMAGE_PIXELS, MAX_INPUT_BYTES, MAX_OBUS, MAX_SYMBOLS,
};
pub const MAX_AXIS: usize = intra::MAX_AXIS;

#[cfg(test)]
mod tests {
    #[test]
    fn arithmetic_modules_have_no_ambient_io_or_threads() {
        for source in [
            include_str!("cdef.rs"),
            include_str!("cdf.rs"),
            include_str!("deblock.rs"),
            include_str!("intra.rs"),
            include_str!("intra/decode.rs"),
            include_str!("transform.rs"),
        ] {
            for name in [
                "std::fs",
                "std::env",
                "std::time",
                "std::net",
                "std::process",
                "std::thread",
                "unsafe",
            ] {
                assert!(!source.contains(name), "shared arithmetic names {name}");
            }
        }
        assert!(!include_str!("../Cargo.toml").contains("[dependencies]"));
    }
}
