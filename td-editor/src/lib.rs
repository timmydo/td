#![deny(unsafe_code)]

//! Safe editor state and explicit adapters. File and window adapters own I/O;
//! the document core has no environment, clock or filesystem access.

pub mod clipboard;
mod command;
pub mod control;
mod control_frame;
mod control_jobs;
mod dialog;
mod directory;
pub use dialog::{Discard, Reload};
pub mod files;
pub mod fill;
// The compositor's font and wire sources reach this crate through td-ui,
// which mounts them by repository path; nothing here mounts a source.
pub use td_ui::font;
pub mod keys;
pub mod layout;
pub mod model;
mod menu;
mod number;
mod path_completion;
pub mod render;
mod replace;
pub mod replay;
mod search;
mod session;
#[cfg(feature = "test-file-barrier")]
mod test_file_barrier;
pub mod spelling;
mod sys;
pub mod text;
pub mod transfer;
pub mod ui;
pub mod wayland;
pub(crate) use td_ui::wire;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidText,
    InvalidPosition,
    InvalidArgument,
    Limit,
    MissingTab,
    StaleRevision,
    Dirty,
    Exhausted,
    Protocol,
    Unavailable,
}

impl Error {
    pub fn code(self) -> &'static str {
        match self {
            Self::InvalidText => "invalid-text",
            Self::InvalidPosition => "invalid-position",
            Self::InvalidArgument => "invalid-argument",
            Self::Limit => "limit",
            Self::MissingTab => "missing-tab",
            Self::StaleRevision => "stale-revision",
            Self::Dirty => "dirty",
            Self::Exhausted => "exhausted",
            Self::Protocol => "protocol",
            Self::Unavailable => "unavailable",
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code())
    }
}

impl std::error::Error for Error {}

/// The toolkit's raster refusals are the editor's own: an argument outside
/// the contract, or a size past a ceiling.
impl From<td_ui::raster::Error> for Error {
    fn from(error: td_ui::raster::Error) -> Self {
        match error {
            td_ui::raster::Error::InvalidArgument => Self::InvalidArgument,
            td_ui::raster::Error::Limit => Self::Limit,
        }
    }
}

/// The toolkit's transport refusals are the editor's own two wire errors,
/// so a control parser can `?` through the shared envelope and codecs.
impl From<td_ui::control::Error> for Error {
    fn from(error: td_ui::control::Error) -> Self {
        match error {
            td_ui::control::Error::Protocol => Self::Protocol,
            td_ui::control::Error::Limit => Self::Limit,
        }
    }
}

/// The editor's codes travel in the toolkit's refusal line unchanged.
impl td_ui::control::ErrorCode for Error {
    fn code(&self) -> &'static str {
        Error::code(*self)
    }
}

pub type Result<T> = std::result::Result<T, Error>;
