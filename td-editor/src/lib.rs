#![forbid(unsafe_code)]

//! Pure editor state. Adapters supply text, commands and save completions;
//! this library opens no files, starts no threads and reads no global state.

pub mod fill;
pub mod keys;
pub mod layout;
pub mod model;
pub mod replay;
pub mod text;

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
