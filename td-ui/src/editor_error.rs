//! The text editor's refusals, shared by its core and every adapter that
//! embeds it, with the codes its control transport carries.

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
impl From<crate::raster::Error> for Error {
    fn from(error: crate::raster::Error) -> Self {
        match error {
            crate::raster::Error::InvalidArgument => Self::InvalidArgument,
            crate::raster::Error::Limit => Self::Limit,
        }
    }
}

/// The toolkit's transport refusals are the editor's own two wire errors,
/// so a control parser can `?` through the shared envelope and codecs.
impl From<crate::control::Error> for Error {
    fn from(error: crate::control::Error) -> Self {
        match error {
            crate::control::Error::Protocol => Self::Protocol,
            crate::control::Error::Limit => Self::Limit,
        }
    }
}

/// The editor's codes travel in the toolkit's refusal line unchanged.
impl crate::control::ErrorCode for Error {
    fn code(&self) -> &'static str {
        Error::code(*self)
    }
}

pub type Result<T> = std::result::Result<T, Error>;
