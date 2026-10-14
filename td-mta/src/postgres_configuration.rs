//! Cold validation of complete shared/profile source candidates; no activation.
use crate::{
    clock::{RuntimeClock, TlsClockSource},
    config::{composition, load, materialize, profile::Roles, stanza, storage, stream, values},
    generations::GenerationSet,
    ids::StoreEpoch,
    postgres_probe::Request,
    postgres_profile_materials::{self, Loader},
    tls_policy::TlsPolicies,
};
use std::{sync::Arc, time::Instant};
use td_crypto::{ClockHandle, Crypto, Digest, Provider, TrustStore};

pub const MAX_PROFILES: usize = 16;
pub const MAX_SOURCE_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Capacity,
    Name,
    DuplicateProfile,
    Composition(composition::Error),
    Profile,
    Text,
    Material(postgres_profile_materials::Error),
    Provider,
    Deadline,
    Crypto,
}

/// Borrowed source, not validated configuration or publication authority.
#[derive(Clone, Copy)]
pub struct Profile<'a> {
    pub name: &'a str,
    pub roles: Roles,
    pub source: &'a [u8],
}

struct OwnedProfile {
    name: String,
    roles: Roles,
    source: Vec<u8>,
}

/// Validated source bytes in canonical profile-name order. Contains material
/// references, not resolved passwords or private keys. Provider checks describe
/// preparation time only; startup must revalidate before service admission.
/// This cannot be forged from raw source bytes:
/// ```compile_fail,E0451
/// use td_mta::{ids::StoreEpoch, postgres_configuration::Candidate};
/// fn forge(epoch: StoreEpoch) -> Candidate {
///     Candidate { epoch, shared: Vec::new(), profiles: Vec::new(), digest: [0; 32] }
/// }
/// ```
pub struct Candidate {
    epoch: StoreEpoch,
    shared: Vec<u8>,
    profiles: Vec<OwnedProfile>,
    digest: [u8; 32],
}
impl Candidate {
    pub fn epoch(&self) -> StoreEpoch {
        self.epoch
    }
    pub fn digest(&self) -> &[u8; 32] {
        &self.digest
    }
    pub fn shared(&self) -> &[u8] {
        &self.shared
    }
    pub fn profile_count(&self) -> usize {
        self.profiles.len()
    }
    pub fn profile(&self, index: usize) -> Option<Profile<'_>> {
        self.profiles.get(index).map(|profile| Profile {
            name: &profile.name,
            roles: profile.roles,
            source: &profile.source,
        })
    }
}

fn copy(bytes: &[u8]) -> Result<Vec<u8>, Error> {
    let mut owned = Vec::new();
    owned
        .try_reserve_exact(bytes.len())
        .map_err(|_| Error::Capacity)?;
    owned.extend_from_slice(bytes);
    Ok(owned)
}

/// Validate every profile against the same shared policy and expected store
/// epoch. All database material reads share this absolute deadline (within the
/// existing ten-second cold-worker ceiling). No candidate escapes on failure.
/// No source/configuration row is written or activated by this operation.
pub fn prepare(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    epoch: StoreEpoch,
    shared: &[u8],
    profiles: &[Profile<'_>],
) -> Result<Candidate, Error> {
    if deadline
        .checked_duration_since(Instant::now())
        .filter(|allowance| {
            !allowance.is_zero() && *allowance <= crate::postgres_probe::MAX_DURATION
        })
        .is_none()
    {
        return Err(Error::Deadline);
    }
    if profiles.is_empty() || profiles.len() > MAX_PROFILES {
        return Err(Error::Capacity);
    }
    let mut total = shared.len();
    let mut ordered = Vec::new();
    ordered
        .try_reserve_exact(profiles.len())
        .map_err(|_| Error::Capacity)?;
    for profile in profiles {
        values::profile_name(profile.name).map_err(|_| Error::Name)?;
        total = total
            .checked_add(profile.source.len())
            .and_then(|n| n.checked_add(profile.name.len() + 1))
            .ok_or(Error::Capacity)?;
        if total > MAX_SOURCE_BYTES {
            return Err(Error::Capacity);
        }
        ordered.push(*profile);
    }
    ordered.sort_unstable_by(|left, right| left.name.cmp(right.name));
    for pair in ordered.windows(2) {
        if let [left, right] = pair {
            if left.name == right.name {
                return Err(Error::DuplicateProfile);
            }
        }
    }
    let mut candidate = Candidate {
        epoch,
        shared: copy(shared)?,
        profiles: Vec::new(),
        digest: [0; 32],
    };
    candidate
        .profiles
        .try_reserve_exact(ordered.len())
        .map_err(|_| Error::Capacity)?;
    let mut scratch = Vec::new();
    scratch
        .try_reserve_exact(stream::SCRATCH_BYTES)
        .map_err(|_| Error::Capacity)?;
    scratch.resize(stream::SCRATCH_BYTES, 0);
    let mut storage = storage::Storage::try_new().map_err(|_| Error::Capacity)?;
    let mut pending = stanza::Pending::new();
    let clock = Arc::new(ClockHandle::new(TlsClockSource::new(Arc::new(
        RuntimeClock::new(),
    ))));
    for profile in ordered {
        if Instant::now() >= deadline {
            return Err(Error::Deadline);
        }
        let source = composition::Source::new(shared, profile.source, &mut scratch)
            .map_err(Error::Composition)?;
        let prepared = load::read_profile(
            storage,
            &mut pending,
            &mut scratch,
            &mut source.reader(),
            profile.roles,
        )
        .map_err(|_| Error::Profile)?;
        let mut loader = Loader::new(request, trust, epoch, deadline);
        let resolved = materialize::read_profile_text(prepared, &mut scratch, |reference| {
            loader.open_input(reference)
        })
        .map_err(|failure| match failure.error() {
            materialize::Error::Input(error) => Error::Material(*error),
            _ => Error::Text,
        })?;
        let set = GenerationSet::at_startup();
        let mut material_error = None;
        let policies = TlsPolicies::prepare_profile(
            set.reserve().map_err(|_| Error::Provider)?,
            &resolved,
            clock.clone(),
            |reference| {
                loader.open_tls(reference).map_err(|error| {
                    material_error = Some(error);
                    crate::ports::Error::Invalid
                })
            },
        )
        .map_err(|_| material_error.map_or(Error::Provider, Error::Material))?;
        drop(policies);
        storage = resolved.into_storage();
        let name = String::from_utf8(copy(profile.name.as_bytes())?).map_err(|_| Error::Name)?;
        candidate.profiles.push(OwnedProfile {
            name,
            roles: profile.roles,
            source: copy(profile.source)?,
        });
    }
    drop(storage);
    candidate.digest = digest(&candidate)?;
    if Instant::now() >= deadline {
        return Err(Error::Deadline);
    }
    Ok(candidate)
}

pub(crate) const DIGEST_DOMAIN: &[u8] = b"td-mta PostgreSQL configuration sources v1\0";

pub(crate) fn visit_bytes(
    candidate: &Candidate,
    mut write: impl FnMut(&[u8]) -> Result<(), Error>,
) -> Result<(), Error> {
    write(DIGEST_DOMAIN)?;
    write(candidate.epoch.as_bytes())?;
    let mut field = |bytes: &[u8]| -> Result<(), Error> {
        let length = u64::try_from(bytes.len()).map_err(|_| Error::Capacity)?;
        write(&length.to_be_bytes())?;
        write(bytes)
    };
    field(&candidate.shared)?;
    field(
        &u64::try_from(candidate.profiles.len())
            .map_err(|_| Error::Capacity)?
            .to_be_bytes(),
    )?;
    for profile in &candidate.profiles {
        field(profile.name.as_bytes())?;
        field(&[profile.roles.bits()])?;
        field(&profile.source)?;
    }
    Ok(())
}

fn digest(candidate: &Candidate) -> Result<[u8; 32], Error> {
    let mut hash = Provider.sha256().map_err(|_| Error::Crypto)?;
    visit_bytes(candidate, |bytes| {
        hash.update(bytes).map_err(|_| Error::Crypto)
    })?;
    hash.finish().map_err(|_| Error::Crypto)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    #[test]
    fn invalid_profile_inventory_refuses_before_database_work() {
        use super::*;
        let request = Request {
            address: "127.0.0.1:1".parse().unwrap(),
            server_name: "localhost",
            user: "fixture",
            database: "fixture",
            password: b"unused",
        };
        let trust = TrustStore::public_roots().unwrap();
        let epoch = StoreEpoch::from_bytes([1; 16]);
        for deadline in [
            Instant::now(),
            Instant::now() + std::time::Duration::from_secs(11),
        ] {
            assert_eq!(
                prepare(&request, &trust, deadline, epoch, b"", &[]).err(),
                Some(Error::Deadline)
            );
        }
        let roles = Roles::from_bits(2).unwrap();
        let valid = Profile {
            name: "desktop",
            roles,
            source: b"[server]",
        };
        let run = |profiles: &[Profile<'_>]| {
            prepare(
                &request,
                &trust,
                Instant::now() + std::time::Duration::from_secs(10),
                epoch,
                b"version = 1",
                profiles,
            )
            .err()
        };
        assert_eq!(run(&[]), Some(Error::Capacity));
        assert_eq!(run(&[valid; MAX_PROFILES + 1]), Some(Error::Capacity));
        assert_eq!(run(&[valid, valid]), Some(Error::DuplicateProfile));
        for name in ["", "Desktop", "bad/name", &"a".repeat(65)] {
            assert_eq!(run(&[Profile { name, ..valid }]), Some(Error::Name));
        }
        let bytes = vec![b' '; MAX_SOURCE_BYTES];
        assert_eq!(
            run(&[Profile {
                source: &bytes,
                ..valid
            }]),
            Some(Error::Capacity)
        );
    }
}
