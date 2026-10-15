//! Bounded, side-effect-free decoding of protected PostgreSQL bootstrap input.
use std::{net::IpAddr, path::Path};

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
    host: String,
    port: u16,
    user: String,
    password: Vec<u8>,
    database: String,
    root_certificate: Option<String>,
    profile: String,
}

impl Bootstrap {
    /// Accept one PostgreSQL URI with explicit credentials and database.
    /// TLS verification is mandatory even when sslmode is omitted.
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
        let (host, port) = match endpoint.split_once(':') {
            Some((host, port)) => {
                if port.is_empty() || port.len() > 5 || !port.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(Error::Endpoint);
                }
                let port = port.parse::<u16>().map_err(|_| Error::Endpoint)?;
                if port == 0 {
                    return Err(Error::Endpoint);
                }
                (host, port)
            }
            None => (endpoint, 5432),
        };
        // The TLS facade authenticates a DNS name, never an IP literal or
        // a URI-encoded host/socket path. Resolve this name again on reconnect.
        if !valid_host(host) {
            return Err(Error::Endpoint);
        }
        let mut root_certificate = None;
        let mut mode_seen = false;
        if let Some(query) = query {
            for option in query.split('&') {
                let (name, value) = option.split_once('=').ok_or(Error::TlsPolicy)?;
                match name {
                    "sslmode" if !mode_seen && value == "verify-full" => mode_seen = true,
                    "sslrootcert" if root_certificate.is_none() => {
                        let path = text(value, 4096).map_err(|_| Error::TlsPolicy)?;
                        if !Path::new(&path).is_absolute() {
                            return Err(Error::TlsPolicy);
                        }
                        root_certificate = Some(path);
                    }
                    _ => return Err(Error::TlsPolicy),
                }
            }
        }
        Ok(Self {
            host: host.to_ascii_lowercase(),
            port,
            user,
            password,
            database,
            root_certificate,
            profile: profile.to_owned(),
        })
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn profile(&self) -> &str {
        &self.profile
    }

    /// None selects the public trust store; a supplied file replaces it.
    /// The caller must read it under the protected bootstrap-file policy.
    pub fn root_certificate(&self) -> Option<&Path> {
        self.root_certificate.as_deref().map(Path::new)
    }

    /// The caller supplies one address resolved from host under its deadline.
    pub fn request(&self, address: IpAddr) -> Request<'_> {
        Request {
            address: (address, self.port).into(),
            server_name: &self.host,
            mode: crate::postgres_probe::Mode::Preparation,
            user: &self.user,
            database: &self.database,
            password: &self.password,
        }
    }
}

pub(crate) fn valid_host(host: &str) -> bool {
    !(host.is_empty()
        || host.len() > 253
        || host.parse::<IpAddr>().is_ok()
        || host
            .rsplit('.')
            .next()
            .is_some_and(|label| label.bytes().all(|b| b.is_ascii_digit()))
        || host.split('.').all(numeric_label)
        || !host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                && label
                    .bytes()
                    .next()
                    .is_some_and(|b| b.is_ascii_alphanumeric())
                && label
                    .bytes()
                    .last()
                    .is_some_and(|b| b.is_ascii_alphanumeric())
        }))
}

// Refuse decimal/octal/hex IPv4 shorthand independently of resolver behavior.
fn numeric_label(label: &str) -> bool {
    if label.bytes().all(|b| b.is_ascii_digit()) {
        return true;
    }
    label
        .strip_prefix("0x")
        .or_else(|| label.strip_prefix("0X"))
        .is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_hexdigit()))
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

    const URI: &str = "postgresql://alice:secret@db.example/mail";

    #[test]
    fn cloud_and_local_names_bind_credentials_profile_and_verified_name() {
        for host in [
            "db.postgres.database.azure.com",
            "db.us-west-2.rds.amazonaws.com",
            "localhost",
        ] {
            let spec = format!("postgresql://alice%40tenant:p%40ss%3A%2F%25@{host}:6543/mail%20box?sslmode=verify-full&sslrootcert=/private/root.pem");
            let bootstrap = Bootstrap::parse(&spec, "desktop-1").unwrap();
            assert_eq!(bootstrap.host(), host);
            assert_eq!(bootstrap.port(), 6543);
            assert_eq!(bootstrap.profile(), "desktop-1");
            assert_eq!(
                bootstrap.root_certificate(),
                Some(Path::new("/private/root.pem"))
            );
            let request = bootstrap.request(IpAddr::from([127, 0, 0, 1]));
            assert_eq!(request.user, "alice@tenant");
            assert_eq!(request.password, b"p@ss:/%");
            assert_eq!(request.database, "mail box");
            assert_eq!(request.server_name, host);
            assert_eq!(request.address.port(), 6543);
        }
        let bootstrap = Bootstrap::parse(URI, "cloud").unwrap();
        assert_eq!(bootstrap.port(), 5432);
        assert_eq!(bootstrap.root_certificate(), None);
    }

    #[test]
    fn options_cannot_weaken_tls_change_endpoint_or_inject_driver_options() {
        for query in [
            "",
            "sslmode=disable",
            "sslmode=require",
            "sslmode=verify-ca",
            "sslmode=verify-full&sslmode=verify-full",
            "host=other",
            "options=-c",
            "connect_timeout=0",
            "sslrootcert=relative",
            "sslrootcert=%2Fa&sslrootcert=%2Fb",
            "sslrootcert=%2Fa%00",
            "sslrootcert=%2Fa%0A",
            "sslrootcert=%2Fa&",
        ] {
            assert!(Bootstrap::parse(&format!("{URI}?{query}"), "cloud").is_err());
        }
    }

    #[test]
    fn malformed_credentials_endpoints_and_profiles_are_refused() {
        for uri in [
            "",
            "host=localhost",
            "postgresql://alice@localhost/mail",
            "postgresql://:pw@localhost/mail",
            "postgresql://alice:@localhost/mail",
            "postgresql://alice:%00@localhost/mail",
            "postgresql://alice:%GG@localhost/mail",
            "postgresql://alice:%@localhost/mail",
            "postgresql://alice:pw@127.0.0.1/mail",
            "postgresql://alice:pw@127.1/mail",
            "postgresql://alice:pw@2130706433/mail",
            "postgresql://alice:pw@1.0x1/mail",
            "postgresql://alice:pw@0X7F000001/mail",
            "postgresql://alice:pw@name.123/mail",
            "postgresql://alice:pw@[::1]/mail",
            "postgresql://alice:pw@a,b/mail",
            "postgresql://alice:pw@-host/mail",
            "postgresql://alice:pw@host:/mail",
            "postgresql://alice:pw@host:0/mail",
            "postgresql://alice:pw@host:65536/mail",
            "postgresql://alice:pw@host/mail/extra",
            "postgresql://alice:pw@host/",
            "postgresql://alice:pw@host/mail#secret",
        ] {
            assert!(Bootstrap::parse(uri, "cloud").is_err());
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
            &format!("postgresql://alice:{}@host/mail", "a".repeat(4097)),
            "cloud"
        )
        .is_err());
        assert!(Bootstrap::parse(
            &format!("postgresql://{}:pw@host/mail", "a".repeat(257)),
            "cloud"
        )
        .is_err());
        assert!(Bootstrap::parse(&"a".repeat(MAX_SPECIFICATION + 1), "cloud").is_err());
    }
}
