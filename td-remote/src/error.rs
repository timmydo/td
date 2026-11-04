//! Why a link could not start or carry on. Each is a sentence for a person:
//! a window shows it as a notice.

use std::fmt;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Error {
    /// A read or write failed.
    Io(String),
    /// The other side went away; what ssh last said, when anything.
    Closed(String),
    /// The other side is not the program or version this one speaks.
    Refused(String),
    /// Something other than the protocol arrived before it began, as a shell
    /// startup file's banner does.
    Garbage(String),
    /// The protocol was broken after it began.
    Protocol(String),
    /// A message past this link's bound: refused before any of it was sent,
    /// or ending the link when it was received.
    TooLong(usize),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(detail) => write!(f, "{detail}"),
            Self::Closed(detail) if detail.is_empty() => f.write_str("the connection closed"),
            Self::Closed(detail) => write!(f, "the connection closed: {detail}"),
            Self::Refused(detail) => write!(f, "{detail}"),
            Self::Garbage(seen) if seen.is_empty() => f.write_str(
                "the remote command printed blank lines before the protocol began; \
                 a shell startup file must not write to standard output",
            ),
            Self::Garbage(seen) => write!(
                f,
                "the remote command printed {seen:?} before the protocol began; \
                 a shell startup file must not write to standard output"
            ),
            Self::Protocol(detail) => write!(f, "protocol error: {detail}"),
            Self::TooLong(size) => write!(f, "a message of {size} bytes is past the bound"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error.to_string())
    }
}
