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
    Inventory,
}
impl Error {
    pub const fn code(self) -> &'static str {
        match self {
            Self::Root => "postgres-bootstrap-root",
            Self::Input(error) => error.code(),
            Self::Read => "postgres-bootstrap-read",
            Self::Capacity => "postgres-bootstrap-capacity",
            Self::Specification(_) => "postgres-bootstrap-specification",
            Self::Trust => "postgres-bootstrap-trust",
            Self::Inventory => "postgres-configuration-inventory",
        }
    }
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
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
        Self::open_inputs(connection_file, profile).map(|(loaded, _)| loaded)
    }
    fn open_inputs(connection_file: &str, profile: &str) -> Result<(Self, Inputs), Error> {
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
        Ok((Self { bootstrap, trust }, inputs))
    }
}

/// Protected raw configuration input. Secret bytes have no Debug output.
pub struct MaterialInput {
    connection: Loaded,
    bytes: Vec<u8>,
}
impl MaterialInput {
    /// Bind connection, explicit trust and material to one owner/inode inventory.
    /// Content validation and generation activation are separate operations.
    pub fn open(
        connection_file: &str,
        material_file: &str,
        kind: crate::postgres_materials::Kind,
    ) -> Result<Self, Error> {
        use crate::postgres_materials::Kind;
        let (connection, mut inputs) = Loaded::open_inputs(connection_file, "material")?;
        let role = match kind {
            Kind::RelayPassword | Kind::PrivateKey => Role::Secret,
            _ => Role::Public,
        };
        let file = inputs.open(material_file, role).map_err(Error::Input)?;
        let bytes = read(file, kind.max_bytes())?;
        Ok(Self { connection, bytes })
    }
    pub fn connection(&self) -> &Loaded {
        &self.connection
    }
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// One explicit owner-side profile source; roles are structural declarations.
pub struct SourceFile<'a> {
    pub name: &'a str,
    pub roles: crate::config::profile::Roles,
    pub path: &'a str,
}
struct ProfileBytes {
    name: String,
    roles: crate::config::profile::Roles,
    bytes: Vec<u8>,
}
/// Bounded protected sources, not a validated or published configuration.
pub struct ConfigurationInput {
    connection: Loaded,
    shared: Vec<u8>,
    profiles: Vec<ProfileBytes>,
}
impl ConfigurationInput {
    pub fn open(
        connection_file: &str,
        shared_file: &str,
        sources: &[SourceFile<'_>],
    ) -> Result<Self, Error> {
        use crate::{
            config::values,
            postgres_configuration::{MAX_PROFILES, MAX_SOURCE_BYTES},
        };
        if sources.is_empty() || sources.len() > MAX_PROFILES {
            return Err(Error::Inventory);
        }
        let mut remaining = MAX_SOURCE_BYTES;
        for (index, source) in sources.iter().enumerate() {
            values::profile_name(source.name).map_err(|_| Error::Inventory)?;
            if sources
                .iter()
                .take(index)
                .any(|other| other.name == source.name)
            {
                return Err(Error::Inventory);
            }
            remaining = remaining
                .checked_sub(source.name.len() + 1)
                .ok_or(Error::Capacity)?;
        }
        let (connection, mut inputs) = Loaded::open_inputs(connection_file, "configuration")?;
        let shared = read_source(
            inputs
                .open(shared_file, Role::Public)
                .map_err(Error::Input)?,
            remaining,
        )?;
        remaining = remaining.checked_sub(shared.len()).ok_or(Error::Capacity)?;
        let mut profiles = Vec::new();
        profiles
            .try_reserve_exact(sources.len())
            .map_err(|_| Error::Capacity)?;
        for source in sources {
            let bytes = read_source(
                inputs
                    .open(source.path, Role::Public)
                    .map_err(Error::Input)?,
                remaining,
            )?;
            remaining = remaining.checked_sub(bytes.len()).ok_or(Error::Capacity)?;
            let mut name = String::new();
            name.try_reserve_exact(source.name.len())
                .map_err(|_| Error::Capacity)?;
            name.push_str(source.name);
            profiles.push(ProfileBytes {
                name,
                roles: source.roles,
                bytes,
            });
        }
        Ok(Self {
            connection,
            shared,
            profiles,
        })
    }
    pub fn connection(&self) -> &Loaded {
        &self.connection
    }
    pub fn shared(&self) -> &[u8] {
        &self.shared
    }
    pub fn profile_count(&self) -> usize {
        self.profiles.len()
    }
    pub fn profile(&self, index: usize) -> Option<crate::postgres_configuration::Profile<'_>> {
        self.profiles
            .get(index)
            .map(|profile| crate::postgres_configuration::Profile {
                name: &profile.name,
                roles: profile.roles,
                source: &profile.bytes,
            })
    }
}

fn read_source(input: impl Read, maximum: usize) -> Result<Vec<u8>, Error> {
    let bytes = read(input, maximum)?;
    // Retained profiles share the source budget, not a maximum-sized buffer
    // each. Compact fallibly while the bounded input buffer is still private.
    let mut retained = Vec::new();
    retained
        .try_reserve_exact(bytes.len())
        .map_err(|_| Error::Capacity)?;
    retained.extend_from_slice(&bytes);
    Ok(retained)
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
    fn small_sources_do_not_retain_the_unconsumed_source_allowance() {
        let maximum = crate::postgres_configuration::MAX_SOURCE_BYTES;
        let small = read_source(&b"version = 1"[..], maximum).unwrap();
        assert_eq!(small, b"version = 1");
        assert!(small.capacity() < maximum / 2);
        assert_eq!(read_source(&b""[..], maximum).unwrap().capacity(), 0);
        assert_eq!(read_source(&b"abcd"[..], 3), Err(Error::Capacity));
    }

    #[test]
    fn configuration_inventory_refuses_before_opening_paths() {
        use crate::config::profile::Roles;
        let source = || SourceFile {
            name: "desktop",
            roles: Roles::from_bits(2).unwrap(),
            path: "/missing/profile",
        };
        assert_eq!(
            ConfigurationInput::open("/missing/connection", "/missing/shared", &[]).err(),
            Some(Error::Inventory)
        );
        assert_eq!(
            ConfigurationInput::open(
                "/missing/connection",
                "/missing/shared",
                &[source(), source()]
            )
            .err(),
            Some(Error::Inventory)
        );
        let mut bad = source();
        bad.name = "Desktop";
        assert_eq!(
            ConfigurationInput::open("/missing/connection", "/missing/shared", &[bad]).err(),
            Some(Error::Inventory)
        );
        let sources: Vec<_> = (0..17).map(|_| source()).collect();
        assert_eq!(
            ConfigurationInput::open("/missing/connection", "/missing/shared", &sources).err(),
            Some(Error::Inventory)
        );
    }

    #[test]
    fn file_syntax_accepts_only_one_optional_line_ending() {
        for suffix in ["", "\n", "\r\n"] {
            let value = format!("{URI}{suffix}");
            let loaded = decode(value.as_bytes(), "desktop").unwrap();
            assert_eq!(loaded.host(), Some("localhost"));
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
