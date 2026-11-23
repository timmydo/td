//! Bounded, side-effect-free decoding of protected PostgreSQL bootstrap input.
use std::path::Path;

use crate::postgres_probe::Request;

const MAX_SPECIFICATION: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Specification,
    Credentials,
    Endpoint,
    TlsPolicy,
    Profile,
}

/// Intentionally has no Debug implementation: the URI contains credentials.
/// Parsing neither reads files/environment nor connects to a database.
pub struct Bootstrap {
    socket: td_fs::SocketPath,
    user: String,
    password: Vec<u8>,
    database: String,
    profile: String,
}

impl Bootstrap {
    /// Accept one PostgreSQL URI with explicit credentials and database. An
    /// empty authority host plus an absolute `host` query names the server's
    /// Unix socket; there is no network form and no TLS.
    pub fn parse(specification: &str, profile: &str) -> Result<Self, Error> {
        if profile.is_empty()
            || profile.len() > 64
            || !profile
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
            || !profile
                .bytes()
                .next()
                .is_some_and(|b| b.is_ascii_alphanumeric())
        {
            return Err(Error::Profile);
        }
        if specification.len() > MAX_SPECIFICATION
            || !specification.bytes().all(|b| (b'!'..=b'~').contains(&b))
            || specification.contains('#')
        {
            return Err(Error::Specification);
        }
        let rest = specification
            .strip_prefix("postgresql://")
            .or_else(|| specification.strip_prefix("postgres://"))
            .ok_or(Error::Specification)?;
        let (location, query) = match rest.split_once('?') {
            Some((location, query)) => (location, Some(query)),
            None => (rest, None),
        };
        let (authority, database) = location.split_once('/').ok_or(Error::Specification)?;
        if database.contains('/') {
            return Err(Error::Specification);
        }
        let database = text(database, 256).map_err(|_| Error::Credentials)?;
        let (credentials, endpoint) = authority.split_once('@').ok_or(Error::Credentials)?;
        let (user, password) = credentials.split_once(':').ok_or(Error::Credentials)?;
        let user = text(user, 256).map_err(|_| Error::Credentials)?;
        let password = decode(password, 4096).map_err(|_| Error::Credentials)?;
        // The client does no SASLprep, so it logs in with ASCII only.
        if !password.is_ascii() {
            return Err(Error::Credentials);
        }
        // The authority names no host: a network endpoint is refused.
        let port = match endpoint.strip_prefix(':') {
            Some(port) => {
                if port.is_empty() || port.len() > 5 || !port.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(Error::Endpoint);
                }
                let port = port.parse::<u16>().map_err(|_| Error::Endpoint)?;
                if port == 0 {
                    return Err(Error::Endpoint);
                }
                port
            }
            None if endpoint.is_empty() => 5432,
            None => return Err(Error::Endpoint),
        };
        let mut disabled = false;
        let mut directory = None;
        if let Some(query) = query {
            for option in query.split('&') {
                let (name, value) = option.split_once('=').ok_or(Error::TlsPolicy)?;
                match name {
                    "sslmode" if !disabled && value == "disable" => disabled = true,
                    "host" if directory.is_none() => {
                        let path = text(value, 107).map_err(|_| Error::Endpoint)?;
                        if !Path::new(&path).is_absolute() {
                            return Err(Error::Endpoint);
                        }
                        directory = Some(path);
                    }
                    _ => return Err(Error::TlsPolicy),
                }
            }
        }
        let directory = directory.ok_or(Error::Endpoint)?;
        let path = Path::new(&directory).join(format!(".s.PGSQL.{port}"));
        let socket = td_fs::SocketPath::new(&path).map_err(|_| Error::Endpoint)?;
        Ok(Self {
            socket,
            user,
            password,
            database,
            profile: profile.to_owned(),
        })
    }

    pub fn socket(&self) -> &td_fs::SocketPath {
        &self.socket
    }

    pub fn profile(&self) -> &str {
        &self.profile
    }

    /// A preparation-mode request for the bootstrap's socket and login.
    pub fn request(&self) -> Request<'_> {
        Request {
            socket: self.socket,
            mode: crate::postgres_probe::Mode::Preparation,
            user: &self.user,
            database: &self.database,
            password: &self.password,
        }
    }
}

fn text(value: &str, maximum: usize) -> Result<String, ()> {
    let bytes = decode(value, maximum)?;
    let text = String::from_utf8(bytes).map_err(|_| ())?;
    if text.chars().any(char::is_control) {
        return Err(());
    }
    Ok(text)
}

fn decode(value: &str, maximum: usize) -> Result<Vec<u8>, ()> {
    let mut decoded = Vec::with_capacity(value.len().min(maximum));
    let mut bytes = value.bytes();
    while let Some(byte) = bytes.next() {
        let byte = if byte == b'%' {
            let high = hex(bytes.next().ok_or(())?).ok_or(())?;
            let low = hex(bytes.next().ok_or(())?).ok_or(())?;
            high * 16 + low
        } else {
            byte
        };
        if byte == 0 || decoded.len() == maximum {
            return Err(());
        }
        decoded.push(byte);
    }
    if decoded.is_empty() {
        return Err(());
    }
    Ok(decoded)
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    const URI: &str = "postgresql://alice:secret@/mail?host=/run/postgresql";

    #[test]
    fn unix_endpoint_binds_socket_credentials_and_profile() {
        let bootstrap = Bootstrap::parse(
            "postgresql://alice%40tenant:p%40ss%3A%2F%25@:35434/mail%20box?host=%2Fhome%2Falice%2Fsocket&sslmode=disable",
            "replica-1",
        )
        .unwrap();
        let socket =
            td_fs::SocketPath::new(Path::new("/home/alice/socket/.s.PGSQL.35434")).unwrap();
        assert_eq!(bootstrap.socket(), &socket);
        assert_eq!(bootstrap.profile(), "replica-1");
        let request = bootstrap.request();
        assert_eq!(request.socket, socket);
        assert_eq!(request.user, "alice@tenant");
        assert_eq!(request.password, b"p@ss:/%");
        assert_eq!(request.database, "mail box");
        assert_eq!(request.mode, crate::postgres_probe::Mode::Preparation);
        let bootstrap = Bootstrap::parse(URI, "cloud").unwrap();
        assert_eq!(
            bootstrap.socket(),
            &td_fs::SocketPath::new(Path::new("/run/postgresql/.s.PGSQL.5432")).unwrap()
        );
    }

    #[test]
    fn the_network_form_is_refused_with_its_fixed_categories() {
        for (uri, error) in [
            ("postgresql://alice:secret@/mail", Error::Endpoint),
            ("postgresql://alice:secret@:5432/mail", Error::Endpoint),
            ("postgresql://alice:secret@localhost/mail", Error::Endpoint),
            (
                "postgresql://alice:secret@db.example:6543/mail",
                Error::Endpoint,
            ),
            ("postgresql://alice:secret@127.0.0.1/mail", Error::Endpoint),
            ("postgresql://alice:secret@[::1]/mail", Error::Endpoint),
            (
                "postgresql://alice:secret@localhost/mail?host=/home/alice/socket",
                Error::Endpoint,
            ),
            (
                "postgresql://alice:secret@localhost/mail?sslmode=verify-full&sslrootcert=/ca.pem",
                Error::Endpoint,
            ),
            (
                "postgresql://alice:secret@/mail?host=/home/alice/socket&sslmode=verify-full",
                Error::TlsPolicy,
            ),
            (
                "postgresql://alice:secret@/mail?host=/home/alice/socket&sslrootcert=/ca.pem",
                Error::TlsPolicy,
            ),
            (
                "postgresql://alice:secret@/mail?sslrootcert=/ca.pem",
                Error::TlsPolicy,
            ),
        ] {
            assert_eq!(Bootstrap::parse(uri, "replica").err(), Some(error), "{uri}");
        }
    }

    #[test]
    fn options_cannot_change_the_socket_or_inject_driver_options() {
        for query in [
            "",
            "sslmode=require",
            "sslmode=verify-ca",
            "sslmode=verify-full",
            "sslmode=disable&sslmode=disable",
            "host=/other",
            "host=relative",
            "options=-c",
            "connect_timeout=0",
            "sslrootcert=%2Fa",
            "sslcertmode=disable",
        ] {
            assert!(
                Bootstrap::parse(&format!("{URI}&{query}"), "cloud").is_err(),
                "{query}"
            );
        }
        for uri in [
            "postgresql://alice:secret@/mail?host=relative",
            "postgresql://alice:secret@/mail?host=/home/alice/../socket",
            "postgresql://alice:secret@/mail?host=/home/alice/./socket",
            "postgresql://alice:secret@/mail?host=/home/alice//socket",
            "postgresql://alice:secret@/mail?host=/home/alice/socket%00",
            "postgresql://alice:secret@/mail?host=/home/alice/socket&host=/other",
        ] {
            assert!(Bootstrap::parse(uri, "replica").is_err(), "{uri}");
        }
        let long = format!("postgresql://alice:secret@/mail?host=/{}", "a".repeat(100));
        assert_eq!(
            Bootstrap::parse(&long, "replica").err(),
            Some(Error::Endpoint)
        );
    }

    #[test]
    fn malformed_credentials_ports_and_profiles_are_refused() {
        for uri in [
            "",
            "host=/run/postgresql",
            "postgresql://alice@/mail?host=/run/postgresql",
            "postgresql://:pw@/mail?host=/run/postgresql",
            "postgresql://alice:@/mail?host=/run/postgresql",
            "postgresql://alice:%00@/mail?host=/run/postgresql",
            "postgresql://alice:p%C3%A9@/mail?host=/run/postgresql",
            "postgresql://alice:%80@/mail?host=/run/postgresql",
            "postgresql://alice:%GG@/mail?host=/run/postgresql",
            "postgresql://alice:%@/mail?host=/run/postgresql",
            "postgresql://alice:pw@:/mail?host=/run/postgresql",
            "postgresql://alice:pw@:0/mail?host=/run/postgresql",
            "postgresql://alice:pw@:65536/mail?host=/run/postgresql",
            "postgresql://alice:pw@:x/mail?host=/run/postgresql",
            "postgresql://alice:pw@/mail/extra?host=/run/postgresql",
            "postgresql://alice:pw@/?host=/run/postgresql",
            "postgresql://alice:pw@/mail?host=/run/postgresql#secret",
        ] {
            assert!(Bootstrap::parse(uri, "cloud").is_err(), "{uri}");
        }
        for profile in [
            "",
            ".hidden",
            "../cloud",
            "cloud/other",
            "cloud\n",
            "cloud space",
        ] {
            assert!(matches!(
                Bootstrap::parse(URI, profile),
                Err(Error::Profile)
            ));
        }
        assert!(Bootstrap::parse(URI, &"a".repeat(65)).is_err());
        assert!(Bootstrap::parse(
            &format!(
                "postgresql://alice:{}@/mail?host=/run/postgresql",
                "a".repeat(4097)
            ),
            "cloud"
        )
        .is_err());
        assert!(Bootstrap::parse(
            &format!(
                "postgresql://{}:pw@/mail?host=/run/postgresql",
                "a".repeat(257)
            ),
            "cloud"
        )
        .is_err());
        assert!(Bootstrap::parse(&"a".repeat(MAX_SPECIFICATION + 1), "cloud").is_err());
    }
}
