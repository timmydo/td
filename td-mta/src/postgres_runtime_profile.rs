//! Cold selected-profile preparation. No listener or runtime admission is opened.
use crate::{
    clock::{RuntimeClock, TlsClockSource},
    config::{materialize::ResolvedProfile, profile::Roles, stanza, storage, stream, values},
    generations::{
        GenerationConstruction, GenerationLease, GenerationSet, PreparedGeneration,
        RetiredGeneration,
    },
    ids::{AccountId, IdentityId, StoreEpoch},
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
    IdentityCatalog(crate::identity_catalog::PreparationError),
    Selection(selection::Error),
    Configuration(configuration::Error),
    Bootstrap(postgres_accounts::Error),
    Superseded,
    TlsPolicy(crate::ports::Error),
}

/// Identity of prepared bytes, not a live policy or device authorization.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Binding {
    epoch: StoreEpoch,
    selection: Selection,
    account: AccountId,
    profile: String,
    roles: Roles,
    route: crate::postgres_routes::Route,
    submission_route: crate::postgres_routes::Route,
    relay_policy: Option<crate::ports::TlsPolicyId>,
    quotas: configuration::SharedQuotas,
    upload_maximum: usize,
    upload_expiry_seconds: usize,
    work: crate::admission::WorkLimits,
    header_maximum: usize,
    recipient_maximum: usize,
    identities: Arc<Vec<SendingIdentity>>,
    identity_catalog: Option<Arc<crate::identity_catalog::Catalog>>,
}

#[derive(Clone, Eq, PartialEq)]
struct SendingIdentity {
    id: IdentityId,
    email: String,
}
impl std::fmt::Debug for SendingIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SendingIdentity(<redacted>)")
    }
}
impl Binding {
    pub(crate) fn relay_policy(&self) -> Option<crate::ports::TlsPolicyId> {
        self.relay_policy
    }

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
    pub fn upload_maximum(&self) -> usize {
        self.upload_maximum
    }
    pub fn upload_expiry_seconds(&self) -> usize {
        self.upload_expiry_seconds
    }
    pub fn quotas(&self) -> configuration::SharedQuotas {
        self.quotas
    }
    pub fn work(&self) -> crate::admission::WorkLimits {
        self.work
    }
    pub fn header_maximum(&self) -> usize {
        self.header_maximum
    }
    pub fn recipient_maximum(&self) -> usize {
        self.recipient_maximum
    }
    /// Configured mailbox evidence only; publication must fence selection and
    /// device authority again before taking responsibility for delivery.
    pub fn identity_email(&self, id: IdentityId) -> Option<&str> {
        self.identities
            .iter()
            .find(|identity| identity.id == id)
            .map(|identity| identity.email.as_str())
    }
    /// Visible configuration only; serving requires a fresh authenticated snapshot.
    pub fn identity_catalog(&self) -> Option<Arc<crate::identity_catalog::Catalog>> {
        self.identity_catalog.clone()
    }
    pub fn route(&self) -> &crate::postgres_routes::Route {
        &self.route
    }
    /// Route for new authenticated submissions, independent of this worker's relay.
    pub fn submission_route(&self) -> &crate::postgres_routes::Route {
        &self.submission_route
    }
    /// Passive eligibility only; dispatch must recheck policy and ownership.
    pub fn serves_route(&self, route: &crate::postgres_routes::Route) -> bool {
        self.roles.relay()
            && self.route.name() == route.name()
            && self.route.destination() == route.destination()
    }
    pub fn roles(&self) -> Roles {
        self.roles
    }
}

/// Resolved secret text and a TLS generation reserved by the caller. Keep it
/// on the cold control worker until the runtime can admit all selected roles.
pub struct Prepared {
    binding: Binding,
    additional: Vec<Binding>,
    configuration: ResolvedProfile,
    policies: PreparedGeneration<TlsPolicies>,
}
/// Same-load configuration and TLS policy retained for configured relay setup.
/// Constructed only by cold publication of the original prepared profile.
pub struct PublishedProfile {
    binding: Binding,
    additional: Vec<Binding>,
    pub(crate) configuration: ResolvedProfile,
    pub(crate) policies: GenerationLease<TlsPolicies>,
}
impl PublishedProfile {
    pub fn configuration(&self) -> &ResolvedProfile {
        &self.configuration
    }
    /// First configured account; request dispatch must select by authenticated account.
    pub fn binding(&self) -> &Binding {
        &self.binding
    }
    pub fn bindings(&self) -> impl Iterator<Item = &Binding> {
        std::iter::once(&self.binding).chain(self.additional.iter())
    }
    pub fn binding_for(&self, account: AccountId) -> Option<&Binding> {
        self.bindings().find(|binding| binding.account() == account)
    }
}
/// Publication changed nothing; the original cold preparation is recoverable.
#[must_use = "retain or explicitly discard the cold preparation"]
pub struct PublishRefusal {
    pub error: crate::ports::Error,
    prepared: Prepared,
}
impl std::fmt::Debug for PublishRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PublishRefusal")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}
impl PublishRefusal {
    pub fn into_prepared(self) -> Prepared {
        self.prepared
    }
}
impl Prepared {
    /// Run on the cold control worker. Send the explicit retired owner for cold
    /// disposal; this publishes no listener, worker or database selection.
    #[allow(
        clippy::result_large_err,
        reason = "Cold refusal retains original preparation without another allocation."
    )]
    pub fn publish(
        self,
        domain: &mut GenerationSet<TlsPolicies>,
    ) -> Result<(PublishedProfile, RetiredGeneration<TlsPolicies>), PublishRefusal> {
        let Self {
            binding,
            additional,
            configuration,
            policies,
        } = self;
        match domain.publish_retained(policies) {
            Ok((policies, retired)) => Ok((
                PublishedProfile {
                    binding,
                    additional,
                    configuration,
                    policies,
                },
                retired,
            )),
            Err(refusal) => Err(PublishRefusal {
                error: refusal.error(),
                prepared: Prepared {
                    binding,
                    additional,
                    configuration,
                    policies: refusal.into_prepared(),
                },
            }),
        }
    }
    /// First configured account; request dispatch must select by authenticated account.
    pub fn binding(&self) -> &Binding {
        &self.binding
    }
    pub fn bindings(&self) -> impl Iterator<Item = &Binding> {
        std::iter::once(&self.binding).chain(self.additional.iter())
    }
    pub fn binding_for(&self, account: AccountId) -> Option<&Binding> {
        self.bindings().find(|binding| binding.account() == account)
    }
    pub fn configuration(&self) -> &ResolvedProfile {
        &self.configuration
    }
    pub fn into_parts(
        self,
    ) -> Result<(Binding, ResolvedProfile, PreparedGeneration<TlsPolicies>), Error> {
        if !self.additional.is_empty() {
            return Err(Error::Account);
        }
        Ok((self.binding, self.configuration, self.policies))
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
    let catalog = configuration
        .candidate()
        .identities()
        .map_err(|_| Error::Account)?;
    let roles = configuration.roles();
    let relay_policy = if roles.relay() {
        Some(TlsPolicies::prepared_relay(&policies).map_err(Error::TlsPolicy)?)
    } else {
        None
    };
    let route = candidate
        .route(profile)
        .ok_or(Error::MissingProfile)?
        .clone();
    let quotas = candidate.quotas();
    let limits = configuration.candidate().resources().resources().limits();
    let header_maximum = limits.header_bytes;
    let recipient_maximum = limits.smtp_recipients;
    let upload_maximum = limits.message_bytes;
    let upload_expiry_seconds = limits.upload_expiry_seconds;
    let work = *configuration.candidate().resources().admission().work();
    let mut bindings = Vec::new();
    bindings
        .try_reserve_exact(catalog.account_count())
        .map_err(|_| Error::Capacity)?;
    for account_index in 0..catalog.account_count() {
        let account = catalog
            .account_at(account_index)
            .map_err(|_| Error::Account)?
            .ok_or(Error::Account)?
            .id;
        let mut identities = Vec::new();
        identities
            .try_reserve_exact(catalog.len())
            .map_err(|_| Error::Capacity)?;
        for index in 0..catalog.len() {
            let identity = catalog
                .identity(index)
                .map_err(|_| Error::Account)?
                .ok_or(Error::Account)?;
            if identity.account != account {
                continue;
            }
            let mut email = String::new();
            email
                .try_reserve_exact(identity.email.len())
                .map_err(|_| Error::Capacity)?;
            email.push_str(identity.email);
            identities.push(SendingIdentity {
                id: identity.id,
                email,
            });
        }
        postgres_accounts::inspect(request, trust, deadline, epoch, account)
            .map_err(Error::Bootstrap)?;
        let identity_catalog = if roles.jmap() {
            Some(Arc::new(
                crate::identity_catalog::Catalog::prepare_for_account(
                    configuration.resolved_text(),
                    account,
                    &td_crypto::Provider,
                )
                .map_err(Error::IdentityCatalog)?,
            ))
        } else {
            None
        };
        bindings.push(Binding {
            epoch,
            selection: selected,
            account,
            profile: name.clone(),
            roles,
            route: route.clone(),
            submission_route: candidate
                .submission_route(account, profile)
                .ok_or(Error::MissingProfile)?
                .clone(),
            relay_policy,
            quotas,
            upload_maximum,
            upload_expiry_seconds,
            work,
            header_maximum,
            recipient_maximum,
            identities: Arc::new(identities),
            identity_catalog,
        });
    }
    let mut bindings = bindings.into_iter();
    let binding = bindings.next().ok_or(Error::Account)?;
    let mut additional = Vec::new();
    additional
        .try_reserve_exact(bindings.len())
        .map_err(|_| Error::Capacity)?;
    additional.extend(bindings);
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
        binding,
        additional,
        configuration,
        policies,
    })
}
