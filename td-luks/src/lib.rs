//! td-luks: td's LUKS2 volume tool (DESIGN.md). Its library holds the
//! bounded LUKS2 header reader at the format level; td-protector layers
//! td's token semantics on it. Pure `std`; it has no `unsafe`.
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

pub mod header;
