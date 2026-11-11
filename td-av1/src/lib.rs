#![forbid(unsafe_code)]
//! Shared AV1 arithmetic used by td's picture codecs.
//! These primitives operate on caller-validated geometry, not bitstreams.

pub mod cdef;
pub mod cdf;
pub mod deblock;
pub mod transform;

#[cfg(test)]
mod tests {
    #[test]
    fn arithmetic_modules_have_no_ambient_io_or_threads() {
        for source in [
            include_str!("cdef.rs"),
            include_str!("cdf.rs"),
            include_str!("deblock.rs"),
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
