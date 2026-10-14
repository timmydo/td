//! Cold protected-file loading; no DNS, database access or service admission.
use crate::{
    operator_files::{Inputs, Role},
    postgres_bootstrap::Bootstrap,
    store_fs::PrivateRoot,
};
use std::{fmt, io::Read, path::Path};
use td_crypto::TrustStore;

const MAX_SPECIFICATION_FILE: usize = 16 * 1024 + 2;
const MAX_TRUST_FILE: usize = 128 * 1024;

/// Fixed categories never carry paths, credentials or provider diagnostics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Root,
    Input(crate::operator_files::Error),
    Read,
    Capacity,
    Specification(crate::postgres_bootstrap::Error),
    Trust,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Root => "postgres-bootstrap-root",
            Self::Input(error) => error.code(),
            Self::Read => "postgres-bootstrap-read",
            Self::Capacity => "postgres-bootstrap-capacity",
            Self::Specification(_) => "postgres-bootstrap-specification",
            Self::Trust => "postgres-bootstrap-trust",
        })
    }
}
impl std::error::Error for Error {}

/// Credentials intentionally have no Debug representation.
pub struct Loaded {
    pub bootstrap: Bootstrap,
    pub trust: TrustStore,
}
impl Loaded {
    /// The connection file's parent must be a checked mode-0700 non-root
    /// directory. Deployment runs as its owner in a trusted stable namespace.
    /// No lock, local database or mail configuration is opened here.
    pub fn open(connection_file: &str, profile: &str) -> Result<Self, Error> {
        let parent = Path::new(connection_file)
            .parent()
            .and_then(Path::to_str)
            .ok_or(Error::Root)?;
        let root = PrivateRoot::open(parent).map_err(|_| Error::Root)?;
        let mut inputs = Inputs::new();
        inputs.bind(&root).map_err(Error::Input)?;
        let file = inputs
            .open(connection_file, Role::Secret)
            .map_err(Error::Input)?;
        let bytes = read(file, MAX_SPECIFICATION_FILE)?;
        let bootstrap = decode(&bytes, profile)?;
        let trust = match bootstrap.root_certificate() {
            Some(path) => {
                let path = path.to_str().ok_or(Error::Trust)?;
                let file = inputs.open(path, Role::Public).map_err(Error::Input)?;
                let bytes = read(file, MAX_TRUST_FILE)?;
                TrustStore::from_pem(&bytes).map_err(|_| Error::Trust)?
            }
            None => TrustStore::public_roots().map_err(|_| Error::Trust)?,
        };
        Ok(Self { bootstrap, trust })
    }
}

fn decode(bytes: &[u8], profile: &str) -> Result<Bootstrap, Error> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| Error::Specification(crate::postgres_bootstrap::Error::Specification))?;
    let text = text
        .strip_suffix("\r\n")
        .or_else(|| text.strip_suffix('\n'))
        .unwrap_or(text);
    Bootstrap::parse(text, profile).map_err(Error::Specification)
}

fn read(mut input: impl Read, maximum: usize) -> Result<Vec<u8>, Error> {
    let capacity = maximum.checked_add(1).ok_or(Error::Capacity)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(capacity)
        .map_err(|_| Error::Capacity)?;
    bytes.resize(capacity, 0);
    let mut used = 0;
    loop {
        let output = bytes.get_mut(used..).ok_or(Error::Capacity)?;
        let count = match input.read(output) {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            result => result.map_err(|_| Error::Read)?,
        };
        used = used.checked_add(count).ok_or(Error::Capacity)?;
        if used > maximum {
            return Err(Error::Capacity);
        }
        if count == 0 {
            bytes.truncate(used);
            return Ok(bytes);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    const URI: &str = "postgresql://user:secret@localhost/mail";

    #[test]
    fn file_syntax_accepts_only_one_optional_line_ending() {
        for suffix in ["", "\n", "\r\n"] {
            let value = format!("{URI}{suffix}");
            let loaded = decode(value.as_bytes(), "desktop").unwrap();
            assert_eq!(loaded.host(), "localhost");
            assert_eq!(loaded.profile(), "desktop");
        }
        for suffix in ["\r", "\n\n", "\r\n\r\n", " ", "\0", "\nother"] {
            assert!(decode(format!("{URI}{suffix}").as_bytes(), "desktop").is_err());
        }
        assert!(decode(format!(" {URI}").as_bytes(), "desktop").is_err());
        assert!(decode(&[0xff], "desktop").is_err());
        assert!(decode(URI.as_bytes(), "../desktop").is_err());
    }

    #[test]
    fn bounded_read_accepts_exact_limit_and_refuses_extra_or_io_failure() {
        assert_eq!(read(&b"abc"[..], 3), Ok(b"abc".to_vec()));
        assert_eq!(read(&b"abcd"[..], 3), Err(Error::Capacity));
        assert_eq!(read(&b""[..], 0), Ok(Vec::new()));
        assert_eq!(read(&b"x"[..], 0), Err(Error::Capacity));
        struct Fault;
        impl Read for Fault {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("must not escape"))
            }
        }
        assert_eq!(read(Fault, 3), Err(Error::Read));
        assert_eq!(Error::Read.to_string(), "postgres-bootstrap-read");
    }
}
