//! Cold selected-profile input resolution. No generation or serving authority.
use crate::{
    config::inputs::{self, Target},
    ids::StoreEpoch,
    postgres_materials::{self, Kind, Material, Version},
    postgres_probe::Request,
    tls_policy::{MaterialKind, MaterialRequest},
};
use std::{io, time::Instant};
use td_crypto::TrustStore;

const PREFIX: &str = "/postgres-material/";
const MAX_REQUESTS: usize = 256;
const MAX_RESERVED_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Reference,
    ManagedCertificate,
    Capacity,
    Deadline,
    Failed,
    Database(postgres_materials::Error),
}

/// The legacy selected-profile syntax still calls these fields paths. This
/// adapter accepts only a canonical database version, never a filesystem path.
fn version(reference: &str) -> Result<Version, Error> {
    reference
        .strip_prefix(PREFIX)
        .and_then(|id| Version::parse(id).ok())
        .ok_or(Error::Reference)
}

fn input_kind(target: Target) -> Kind {
    match target {
        Target::TextSignature(_) => Kind::TextSignature,
        Target::HtmlSignature(_) => Kind::HtmlSignature,
        Target::RelayPassword => Kind::RelayPassword,
        Target::CertificateChain(_) => Kind::CertificateChain,
        Target::CertificateKey(_) => Kind::PrivateKey,
        Target::RelayCa | Target::AcmeCa | Target::GatewayCa(_) => Kind::TrustBundle,
    }
}
fn tls_kind(kind: MaterialKind) -> Kind {
    match kind {
        MaterialKind::Chain => Kind::CertificateChain,
        MaterialKind::Key => Kind::PrivateKey,
        MaterialKind::GatewayCa | MaterialKind::RelayCa | MaterialKind::AcmeCa => Kind::TrustBundle,
    }
}

/// Use one loader for all text and TLS inputs of one selected profile. All
/// requests share the supplied absolute deadline and conservative byte budget.
/// Each lookup verifies its exact immutable version, kind, digest and epoch.
/// This does not validate a complete configuration generation or activate it.
pub struct Loader<'a, 'r> {
    request: &'a Request<'r>,
    trust: &'a TrustStore,
    epoch: StoreEpoch,
    deadline: Instant,
    requests: usize,
    reserved: usize,
    failed: bool,
    managed: Vec<(String, crate::postgres_acme::Active)>,
}
impl<'a, 'r> Loader<'a, 'r> {
    pub fn new(
        request: &'a Request<'r>,
        trust: &'a TrustStore,
        epoch: StoreEpoch,
        deadline: Instant,
    ) -> Self {
        Self {
            request,
            trust,
            epoch,
            deadline,
            requests: 0,
            reserved: 0,
            failed: false,
            managed: Vec::new(),
        }
    }

    pub fn open_input(&mut self, reference: inputs::Reference<'_>) -> Result<Reader, Error> {
        self.open(Some(reference.path()), input_kind(reference.target()))
    }

    /// Pin one complete managed pair per profile across chain/key requests.
    pub fn open_tls(&mut self, reference: MaterialRequest<'_>) -> Result<Reader, Error> {
        if reference.path().is_some() {
            return self.open(reference.path(), tls_kind(reference.kind()));
        }
        if self.failed || Instant::now() >= self.deadline {
            return Err(Error::Failed);
        }
        self.failed = true;
        self.requests = self.requests.checked_add(1).ok_or(Error::Capacity)?;
        self.reserved = self
            .reserved
            .checked_add(tls_kind(reference.kind()).max_bytes())
            .ok_or(Error::Capacity)?;
        if self.requests > MAX_REQUESTS || self.reserved > MAX_RESERVED_BYTES {
            return Err(Error::Capacity);
        }
        let profile = reference.profile().ok_or(Error::ManagedCertificate)?;
        if !self.managed.iter().any(|(name, _)| name == profile) {
            if self.managed.len() >= crate::config::certificate::MAX_PROFILES {
                return Err(Error::Capacity);
            }
            let active = crate::postgres_acme::active(
                self.request,
                self.trust,
                self.deadline,
                self.epoch,
                profile,
            )
            .map_err(|_| Error::ManagedCertificate)?;
            self.managed.push((profile.to_owned(), active));
        }
        let (_, active) = self
            .managed
            .iter()
            .find(|(name, _)| name == profile)
            .ok_or(Error::ManagedCertificate)?;
        if reference.managed_settings() != Some(active.settings) {
            return Err(Error::ManagedCertificate);
        }
        let bytes = match reference.kind() {
            MaterialKind::Chain => active.chain.clone(),
            MaterialKind::Key => active.key.clone(),
            _ => return Err(Error::ManagedCertificate),
        };
        self.failed = false;
        Ok(Reader {
            material: None,
            managed: bytes,
            position: 0,
        })
    }

    fn reserve(&mut self, reference: Option<&str>, kind: Kind) -> Result<Version, Error> {
        if self.failed {
            return Err(Error::Failed);
        }
        self.failed = true;
        if Instant::now() >= self.deadline {
            return Err(Error::Deadline);
        }
        let id = version(reference.ok_or(Error::ManagedCertificate)?)?;
        self.requests = self.requests.checked_add(1).ok_or(Error::Capacity)?;
        self.reserved = self
            .reserved
            .checked_add(kind.max_bytes())
            .ok_or(Error::Capacity)?;
        if self.requests > MAX_REQUESTS || self.reserved > MAX_RESERVED_BYTES {
            return Err(Error::Capacity);
        }
        Ok(id)
    }

    fn open(&mut self, reference: Option<&str>, kind: Kind) -> Result<Reader, Error> {
        let id = self.reserve(reference, kind)?;
        let material = postgres_materials::read(
            self.request,
            self.trust,
            self.deadline,
            self.epoch,
            id,
            kind,
        )
        .map_err(Error::Database)?;
        if Instant::now() >= self.deadline {
            return Err(Error::Deadline);
        }
        self.failed = false;
        Ok(Reader {
            material: Some(material),
            managed: Vec::new(),
            position: 0,
        })
    }
}

/// Owned verified bytes, with no path opener, Debug output or text conversion.
/// Content decoding and provider validation remain the existing consumers'.
pub struct Reader {
    material: Option<Material>,
    managed: Vec<u8>,
    position: usize,
}
impl Drop for Reader {
    fn drop(&mut self) {
        self.managed.fill(0);
        std::hint::black_box(&mut self.managed);
    }
}
impl io::Read for Reader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let bytes = self
            .material
            .as_ref()
            .map(Material::bytes)
            .unwrap_or(&self.managed);
        let remaining = bytes
            .get(self.position..)
            .ok_or(io::ErrorKind::InvalidData)?;
        let count = remaining.len().min(output.len());
        output
            .get_mut(..count)
            .ok_or(io::ErrorKind::InvalidData)?
            .copy_from_slice(remaining.get(..count).ok_or(io::ErrorKind::InvalidData)?);
        self.position = self
            .position
            .checked_add(count)
            .ok_or(io::ErrorKind::InvalidData)?;
        Ok(count)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn bounds_and_refusals_poison_the_loader_without_database_work() {
        let request = Request {
            address: crate::postgres_probe::Endpoint::Tcp("127.0.0.1:1".parse().unwrap()),
            server_name: "localhost",
            mode: crate::postgres_probe::Mode::Preparation,
            user: "fixture",
            database: "fixture",
            password: b"unused",
        };
        let trust = TrustStore::public_roots().unwrap();
        let reference = format!("{PREFIX}{}", Version::from_bytes([1; 16]));
        let new = || {
            Loader::new(
                &request,
                &trust,
                StoreEpoch::from_bytes([1; 16]),
                Instant::now() + std::time::Duration::from_secs(10),
            )
        };
        let mut loader = new();
        assert_eq!(
            loader.reserve(None, Kind::PrivateKey),
            Err(Error::ManagedCertificate)
        );
        assert_eq!(
            loader.reserve(Some(&reference), Kind::PrivateKey),
            Err(Error::Failed)
        );
        let mut loader = new();
        assert_eq!(
            loader.reserve(Some("/private"), Kind::PrivateKey),
            Err(Error::Reference)
        );
        assert_eq!(
            loader.reserve(Some(&reference), Kind::PrivateKey),
            Err(Error::Failed)
        );
        let mut loader = new();
        loader.deadline = Instant::now();
        assert_eq!(
            loader.reserve(Some(&reference), Kind::PrivateKey),
            Err(Error::Deadline)
        );
        for kind in [Kind::RelayPassword, Kind::TrustBundle] {
            let mut loader = new();
            for _ in 0..MAX_REQUESTS.min(MAX_RESERVED_BYTES / kind.max_bytes()) {
                loader.reserve(Some(&reference), kind).unwrap();
                // Simulate successful completion; the next reservation retains
                // both cumulative budgets and the original deadline.
                loader.failed = false;
            }
            assert_eq!(loader.reserve(Some(&reference), kind), Err(Error::Capacity));
            assert_eq!(loader.reserve(Some(&reference), kind), Err(Error::Failed));
        }
    }

    #[test]
    fn references_are_exact_versions_without_path_or_url_interpretation() {
        let id = Version::from_bytes([0xab; 16]);
        assert_eq!(version(&format!("{PREFIX}{id}")), Ok(id));
        for reference in [
            "/etc/private-key",
            "postgres-material/abababababababababababababababab",
            "/postgres-material/ABABABABABABABABABABABABABABABAB",
            "/postgres-material/abababababababababababababababab/",
            "/postgres-material/../abababababababababababababababab",
            "/postgres-material/%61bababababababababababababababab",
            "/postgres-material/abababababababababababababababab?key",
            "/postgres-material/",
        ] {
            assert_eq!(version(reference), Err(Error::Reference));
        }
    }
}
