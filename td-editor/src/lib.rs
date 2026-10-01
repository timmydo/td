#![deny(unsafe_code)]

//! The editor window and its adapters over td-ui's shared editor core. File
//! and window adapters own I/O; the core has no environment, clock or
//! filesystem access.

// The shared core's refusals are the window's own.
pub use td_ui::editor_error::{Error, Result};

mod command;
pub mod control;
mod control_frame;
mod control_jobs;
mod directory;
pub mod files;
// The compositor's font and wire sources reach this crate through td-ui,
// which mounts them by repository path; nothing here mounts a source.
pub use td_ui::font;
mod menu;
mod number;
mod path_completion;
pub mod preview;
mod replace;
pub mod replay;
mod search;
mod session;
pub mod spelling;
mod sys;
#[cfg(feature = "test-file-barrier")]
mod test_file_barrier;
pub mod transfer;
pub mod wayland;
pub(crate) use td_ui::wire;
