#![forbid(unsafe_code)]
#![cfg_attr(
    test,
    allow(
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::unwrap_used
    )
)]

//! The td-portal library surface: the file chooser and the private-Wayland
//! dialog that presents it, shared with the crate's integration tests. The
//! binary in `main.rs` is the D-Bus portal service that drives them.
//!
//! Only `dialog` reaches the shared toolkit's Wayland transport and turn
//! loop; `file_chooser` renders over the raster and chrome bands and never
//! touches the transport. The confinement tests in `main.rs` pin that split.

// The shared compositor keyboard module names `crate::scene::SurfaceKey`;
// supply the one type it needs so the mount compiles here.
mod scene {
    #[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
    pub struct SurfaceKey {
        pub client: u64,
        pub object: u32,
    }
}

#[path = "../../td-compositor/src/keyboard.rs"]
#[allow(
    dead_code,
    reason = "the shared keyboard profile is broader than one chooser"
)]
pub mod keyboard;
#[path = "../../td-compositor/src/filter.rs"]
pub mod list_filter;

pub mod dialog;
pub mod file_chooser;
