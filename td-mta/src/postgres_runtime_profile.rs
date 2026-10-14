//! Cold selected-profile preparation. No listener or runtime admission is opened.
use crate::{
    clock::{RuntimeClock, TlsClockSource},
    config::{materialize::ResolvedProfile, profile::Roles, stanza, storage, stream, values},
    generations::{GenerationConstruction, PreparedGeneration},
    ids::{AccountId, StoreEpoch},
    postgres_accounts,
    postgres_configuration::{self as configuration, ProfileInput},
    postgres_probe::{self, Request},
    postgres_selection::{self as selection, Selection},
    tls_policy::TlsPolicies,
};
use std::{sync::Arc, time::Instant};
use td_crypto::{ClockHandle, TrustStore};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Deadline,
    Name,
    Unselected,
    MissingProfile,
    Capacity,
    Account,
    Selection(selection::Error),
    Configuration(configuration::Error),
    Bootstrap(postgres_accounts::Error),
    Superseded,
}

/// Identity of prepared bytes, not a live policy or device authorization.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Binding {
    epoch: StoreEpoch,
    selection: Selection,
    account: AccountId,
    profile: String,
    roles: Roles,
}
impl Binding {
    pub fn epoch(&self) -> StoreEpoch {
        self.epoch
    }
    pub fn selection(&self) -> Selection {
        self.selection
    }
    pub fn account(&self) -> AccountId {
        self.account
    }
    pub fn profile(&self) -> &str {
        &self.profile
    }
    pub fn roles(&self) -> Roles {
        self.roles
    }
}

/// Resolved secret text and a TLS generation reserved by the caller. Keep it
/// on the cold control worker until the runtime can admit all selected roles.
pub struct Prepared {
    binding: Binding,
    configuration: ResolvedProfile,
    policies: PreparedGeneration<TlsPolicies>,
}
impl Prepared {
    pub fn binding(&self) -> &Binding {
        &self.binding
    }
    pub fn configuration(&self) -> &ResolvedProfile {
        &self.configuration
    }
    pub fn into_parts(self) -> (Binding, ResolvedProfile, PreparedGeneration<TlsPolicies>) {
        (self.binding, self.configuration, self.policies)
    }
}

/// Resolve the selected database profile and account without local material
/// fallback. The caller's TLS reservation remains in its original generation
/// domain. A final primary observation rejects an already superseded selection;
/// the result still requires fresh policy fencing before runtime publication.
pub fn load(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    epoch: StoreEpoch,
    profile: &str,
    reserved: GenerationConstruction<TlsPolicies>,
) -> Result<Prepared, Error> {
    if deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero() && *remaining <= postgres_probe::MAX_DURATION)
        .is_none()
    {
        return Err(Error::Deadline);
    }
    values::profile_name(profile).map_err(|_| Error::Name)?;
    let (selected, candidate) = selection::read(request, trust, deadline, epoch)
        .map_err(Error::Selection)?
        .ok_or(Error::Unselected)?;
    let chosen = (0..candidate.profile_count())
        .filter_map(|index| candidate.profile(index))
        .find(|entry| entry.name == profile)
        .ok_or(Error::MissingProfile)?;
    let mut name = String::new();
    name.try_reserve_exact(profile.len())
        .map_err(|_| Error::Capacity)?;
    name.push_str(profile);
    let mut scratch = Vec::new();
    scratch
        .try_reserve_exact(stream::SCRATCH_BYTES)
        .map_err(|_| Error::Capacity)?;
    scratch.resize(stream::SCRATCH_BYTES, 0);
    let storage = storage::Storage::try_new().map_err(|_| Error::Capacity)?;
    let mut pending = stanza::Pending::new();
    let clock = Arc::new(ClockHandle::new(TlsClockSource::new(Arc::new(
        RuntimeClock::new(),
    ))));
    let (configuration, policies) = configuration::resolve_profile(
        ProfileInput {
            request,
            trust,
            deadline,
            epoch,
            shared: candidate.shared(),
            profile: chosen,
            clock,
        },
        storage,
        &mut pending,
        &mut scratch,
        reserved,
    )
    .map_err(Error::Configuration)?;
    let account = configuration
        .candidate()
        .identities()
        .map_err(|_| Error::Account)?
        .account()
        .map_err(|_| Error::Account)?
        .id;
    postgres_accounts::inspect(request, trust, deadline, epoch, account)
        .map_err(Error::Bootstrap)?;
    let roles = configuration.roles();
    drop(candidate);
    if selection::observe(request, trust, deadline, epoch).map_err(Error::Selection)?
        != Some(selected)
    {
        return Err(Error::Superseded);
    }
    if Instant::now() >= deadline {
        return Err(Error::Deadline);
    }
    Ok(Prepared {
        binding: Binding {
            epoch,
            selection: selected,
            account,
            profile: name,
            roles,
        },
        configuration,
        policies,
    })
}
