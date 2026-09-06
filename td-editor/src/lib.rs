#![deny(unsafe_code)]

//! Safe editor state and explicit adapters. File and window adapters own I/O;
//! the document core has no environment, clock or filesystem access.

pub mod clipboard;
mod data;
mod dialog;
pub use dialog::{Discard, Reload};
pub mod files;
pub mod fill;
#[path = "../../td-compositor/src/font.rs"]
pub mod font;
#[path = "../../td-compositor/src/font_data.rs"]
mod font_data;
pub mod keyboard;
pub mod keys;
pub mod layout;
pub mod model;
mod menu;
mod pointer;
pub mod render;
pub mod replay;
mod seat;
mod search;
mod session;
mod sys;
pub mod text;
pub mod transfer;
pub mod ui;
pub mod wayland;
#[allow(dead_code, clippy::new_without_default)]
#[path = "../../td-compositor/src/wire.rs"]
mod wire;
pub mod xkb;
mod xkb_compat;
mod xkb_keys;
mod xkb_symbols;
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
