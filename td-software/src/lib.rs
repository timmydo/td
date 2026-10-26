//! td-software: the image's applications and their Flathub caches
//! (`DESIGN.md`). The tables, the control socket's answers and the
//! window's state are library modules so the tests drive them; the binary
//! adds the window, the worker and the files.
#![forbid(unsafe_code)]

pub mod app;
pub mod registry;
pub mod snapshot;
pub mod worker;

// td-app's status record and the image's Flathub declarations, one
// reviewed copy with td-app's, td-jail's and td-authd's.
#[path = "../../engine/src/flathub_app.rs"]
#[allow(
    dead_code,
    reason = "td-app's and td-jail's halves of the cache contract"
)]
mod flathub_app;
