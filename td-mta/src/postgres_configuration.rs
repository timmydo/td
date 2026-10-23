//! Cold validation of complete shared/profile source candidates; no activation.
use crate::{
    clock::{RuntimeClock, TlsClockSource},
    config::{composition, load, materialize, profile::Roles, stanza, storage, stream, values},
    generations::{GenerationConstruction, GenerationSet, PreparedGeneration},
    ids::{AccountId, StoreEpoch},
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
    SharedQuotas,
    RouteConflict,
    OutboundProfile,
    Route(crate::postgres_routes::Error),
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

/// Account-wide limits derived from every validated profile. These values are
/// policy observations, not reservations or permission to publish a mutation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SharedQuotas {
    pub body_bytes: u64,
    pub blob_count: u64,
    pub upload_bytes: u64,
    pub queue_bytes: u64,
    pub queue_submissions: u64,
}
impl SharedQuotas {
    fn from_plan(plan: &crate::admission::Plan) -> Self {
        Self {
            body_bytes: plan.disk().body_bytes,
            blob_count: plan.disk().blob_count,
            upload_bytes: plan.upload_bytes(),
            queue_bytes: plan.queue_bytes(),
            queue_submissions: plan.queue_submissions(),
        }
    }
}

struct OwnedProfile {
    route: crate::postgres_routes::Route,
    name: String,
    roles: Roles,
    source: Vec<u8>,
}

/// Validated source bytes in canonical profile-name order. Contains material
/// references, not resolved passwords or private keys. Provider checks describe
/// preparation time only; startup must revalidate before service admission.
/// This cannot be forged from raw source bytes:
/// ```compile_fail,E0451
/// use td_mta::{ids::StoreEpoch, postgres_configuration::{Candidate, SharedQuotas}};
/// fn forge(epoch: StoreEpoch, quotas: SharedQuotas) -> Candidate {
///     Candidate { epoch, shared: Vec::new(), profiles: Vec::new(),
///         digest: [0; 32], quotas, submission_routes: Vec::new() }
/// }
/// ```
pub struct Candidate {
    epoch: StoreEpoch,
    shared: Vec<u8>,
    profiles: Vec<OwnedProfile>,
    digest: [u8; 32],
    quotas: SharedQuotas,
    submission_routes: Vec<(AccountId, crate::postgres_routes::Route)>,
}
impl Candidate {
    pub fn epoch(&self) -> StoreEpoch {
        self.epoch
    }
    pub fn digest(&self) -> &[u8; 32] {
        &self.digest
    }
    pub fn quotas(&self) -> SharedQuotas {
        self.quotas
    }
    pub fn shared(&self) -> &[u8] {
        &self.shared
    }
    pub fn route(&self, profile: &str) -> Option<&crate::postgres_routes::Route> {
        self.profiles
            .iter()
            .find(|entry| entry.name == profile)
            .map(|entry| &entry.route)
    }
    pub fn submission_route(
        &self,
        account: AccountId,
        profile: &str,
    ) -> Option<&crate::postgres_routes::Route> {
        self.submission_routes
            .iter()
            .find(|(id, _)| *id == account)
            .map(|(_, route)| route)
            .or_else(|| self.route(profile))
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
    let mut profiles: Vec<OwnedProfile> = Vec::new();
    profiles
        .try_reserve_exact(ordered.len())
        .map_err(|_| Error::Capacity)?;
    let mut quotas = None;
    let mut outbound_profiles = Vec::new();
    outbound_profiles
        .try_reserve_exact(crate::config::identities::MAX_ACCOUNTS)
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
        let set = GenerationSet::at_startup();
        let (resolved, policies) = resolve_profile(
            ProfileInput {
                request,
                trust,
                deadline,
                epoch,
                shared,
                profile,
                clock: clock.clone(),
            },
            storage,
            &mut pending,
            &mut scratch,
            set.reserve().map_err(|_| Error::Provider)?,
        )?;
        drop(policies);
        let current = SharedQuotas::from_plan(resolved.candidate().resources().admission());
        if quotas.is_some_and(|expected| expected != current) {
            return Err(Error::SharedQuotas);
        }
        quotas = Some(current);
        let relay = resolved
            .candidate()
            .outbound()
            .map_err(|_| Error::Profile)?
            .relay()
            .map_err(|_| Error::Profile)?;
        let route =
            crate::postgres_routes::Route::from_relay(profile.name, relay).map_err(Error::Route)?;
        if profiles.iter().any(|old| !route.compatible(&old.route)) {
            return Err(Error::RouteConflict);
        }
        if profiles.is_empty() {
            let catalog = resolved
                .candidate()
                .identities()
                .map_err(|_| Error::Profile)?;
            for index in 0..catalog.account_count() {
                let account = catalog
                    .account_at(index)
                    .map_err(|_| Error::Profile)?
                    .ok_or(Error::Profile)?
                    .id;
                if let Some(target) = catalog
                    .outbound_profile(account)
                    .map_err(|_| Error::Profile)?
                {
                    outbound_profiles.push((
                        account,
                        String::from_utf8(copy(target.as_bytes())?).map_err(|_| Error::Name)?,
                    ));
                }
            }
        }
        storage = resolved.into_storage();
        let name = String::from_utf8(copy(profile.name.as_bytes())?).map_err(|_| Error::Name)?;
        profiles.push(OwnedProfile {
            route,
            name,
            roles: profile.roles,
            source: copy(profile.source)?,
        });
    }
    drop(storage);
    let mut submission_routes = Vec::new();
    submission_routes
        .try_reserve_exact(outbound_profiles.len())
        .map_err(|_| Error::Capacity)?;
    for (account, target) in outbound_profiles {
        let profile = profiles
            .iter()
            .find(|profile| profile.name == target && profile.roles.relay())
            .ok_or(Error::OutboundProfile)?;
        submission_routes.push((account, profile.route.clone()));
    }
    let mut candidate = Candidate {
        epoch,
        shared: copy(shared)?,
        profiles,
        digest: [0; 32],
        quotas: quotas.ok_or(Error::Profile)?,
        submission_routes,
    };
    candidate.digest = digest(&candidate)?;
    if Instant::now() >= deadline {
        return Err(Error::Deadline);
    }
    Ok(candidate)
}

pub(crate) struct ProfileInput<'a, 'r> {
    pub request: &'a Request<'r>,
    pub trust: &'a TrustStore,
    pub deadline: Instant,
    pub epoch: StoreEpoch,
    pub shared: &'a [u8],
    pub profile: Profile<'a>,
    pub clock: Arc<ClockHandle>,
}

// Candidate validation and runtime preparation use exactly the same source,
// material and TLS path. The caller owns the generation reservation.
pub(crate) fn resolve_profile(
    input: ProfileInput<'_, '_>,
    storage: storage::Storage,
    pending: &mut stanza::Pending,
    scratch: &mut [u8],
    reserved: GenerationConstruction<TlsPolicies>,
) -> Result<
    (
        materialize::ResolvedProfile,
        PreparedGeneration<TlsPolicies>,
    ),
    Error,
> {
    if Instant::now() >= input.deadline {
        return Err(Error::Deadline);
    }
    let source = composition::Source::new(input.shared, input.profile.source, scratch)
        .map_err(Error::Composition)?;
    let prepared = load::read_profile(
        storage,
        pending,
        scratch,
        &mut source.reader(),
        input.profile.roles,
    )
    .map_err(|_| Error::Profile)?;
    let mut loader = Loader::new(input.request, input.trust, input.epoch, input.deadline);
    let resolved =
        materialize::read_profile_text(prepared, scratch, |reference| loader.open_input(reference))
            .map_err(|failure| match failure.error() {
                materialize::Error::Input(error) => Error::Material(*error),
                _ => Error::Text,
            })?;
    let mut material_error = None;
    let policies = TlsPolicies::prepare_profile(reserved, &resolved, input.clock, |reference| {
        loader.open_tls(reference).map_err(|error| {
            material_error = Some(error);
            crate::ports::Error::Invalid
        })
    })
    .map_err(|_| material_error.map_or(Error::Provider, Error::Material))?;
    if Instant::now() >= input.deadline {
        return Err(Error::Deadline);
    }
    Ok((resolved, policies))
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
            mode: crate::postgres_probe::Mode::Preparation,
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
