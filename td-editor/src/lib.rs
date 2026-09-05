#![deny(unsafe_code)]

//! Safe editor state and explicit adapters. Only the window adapter accesses
//! the environment, clock, filesystem and Wayland connection.

pub mod fill;
#[path = "../../td-compositor/src/font.rs"]
pub mod font;
#[path = "../../td-compositor/src/font_data.rs"]
mod font_data;
pub mod keys;
pub mod layout;
pub mod model;
pub mod render;
pub mod replay;
mod sys;
pub mod text;
pub mod ui;
pub mod wayland;
#[allow(dead_code, clippy::new_without_default)]
#[path = "../../td-compositor/src/wire.rs"]
mod wire;
pub mod xkb;
mod xkb_syntax;

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

pub type Result<T> = std::result::Result<T, Error>;
