//! Resumable owner-side account edits through ordinary configuration publication.
use crate::{
    account_configuration::{self as editor, Edit},
    ids::{AccountId, MailboxId, StoreEpoch},
    postgres_accounts::{self as accounts, Creation},
    postgres_configuration::{self as configuration, Candidate, Profile},
    postgres_configuration_store as versions,
    postgres_probe::Request,
    postgres_selection::{self as selection, Change, Selection},
};
use std::time::Instant;
use td_crypto::{Crypto, Digest, Provider, TrustStore};

const MAGIC: &[u8] = b"td-mta account operation v1\0";
pub const MAX_OPERATION_BYTES: usize = configuration::MAX_SOURCE_BYTES + 2048;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Edit(editor::Error),
    Configuration(configuration::Error),
    Version(versions::Error),
    Selection(selection::Error),
    Account(accounts::Error),
    MissingSelection,
    Capacity,
    Integrity,
}

/// Contains source references, not resolved material secrets. Persist the whole
/// record privately before apply; retries consume the same bytes.
pub struct Operation {
    account: AccountId,
    creation: Option<Creation>,
    change: Change,
    source: Vec<u8>,
}
impl Operation {
    pub fn account(&self) -> AccountId {
        self.account
    }
    pub fn version(&self) -> versions::Version {
        self.change.version()
    }
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        let mut out = Vec::new();
        out.try_reserve_exact(MAX_OPERATION_BYTES)
            .map_err(|_| Error::Capacity)?;
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(self.account.as_bytes());
        out.push(u8::from(self.creation.is_some()));
        out.extend_from_slice(
            self.creation
                .map(|c| *c.inbox().as_bytes())
                .unwrap_or([0; 16])
                .as_slice(),
        );
        out.extend_from_slice(&self.change.encode());
        out.extend_from_slice(&self.source);
        let mut hash = Provider.sha256().map_err(|_| Error::Integrity)?;
        hash.update(&out).map_err(|_| Error::Integrity)?;
        out.extend_from_slice(&hash.finish().map_err(|_| Error::Integrity)?);
        if out.len() > MAX_OPERATION_BYTES {
            return Err(Error::Capacity);
        }
        Ok(out)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() > MAX_OPERATION_BYTES {
            return Err(Error::Capacity);
        }
        let split = bytes.len().checked_sub(32).ok_or(Error::Integrity)?;
        let (mut input, digest) = bytes.split_at_checked(split).ok_or(Error::Integrity)?;
        let mut hash = Provider.sha256().map_err(|_| Error::Integrity)?;
        hash.update(input).map_err(|_| Error::Integrity)?;
        if hash.finish().map_err(|_| Error::Integrity)?.as_slice() != digest {
            return Err(Error::Integrity);
        }
        if take(&mut input, MAGIC.len())? != MAGIC {
            return Err(Error::Integrity);
        }
        let account = AccountId::from_bytes(
            take(&mut input, 16)?
                .try_into()
                .map_err(|_| Error::Integrity)?,
        );
        let flag = take(&mut input, 1)?
            .first()
            .copied()
            .ok_or(Error::Integrity)?;
        let inbox = <[u8; 16]>::try_from(take(&mut input, 16)?).map_err(|_| Error::Integrity)?;
        let change = Change::decode(take(&mut input, selection::RETAINED_BYTES)?)
            .map_err(Error::Selection)?;
        let creation = match flag {
            0 if inbox == [0; 16] => None,
            1 => Some(Creation::retained(
                change.epoch(),
                account,
                MailboxId::from_bytes(inbox),
            )),
            _ => return Err(Error::Integrity),
        };
        versions::decode(input, change.epoch()).map_err(Error::Version)?;
        let mut source = Vec::new();
        source
            .try_reserve_exact(input.len())
            .map_err(|_| Error::Capacity)?;
        source.extend_from_slice(input);
        Ok(Self {
            account,
            creation,
            change,
            source,
        })
    }
}
fn take<'a>(bytes: &mut &'a [u8], n: usize) -> Result<&'a [u8], Error> {
    let (part, rest) = bytes.split_at_checked(n).ok_or(Error::Integrity)?;
    *bytes = rest;
    Ok(part)
}

/// No database writes. All IDs and the CAS request are fixed before returning.
pub fn prepare(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    epoch: StoreEpoch,
    edit: &Edit<'_>,
) -> Result<Operation, Error> {
    let (selected, original) = selection::read(request, trust, deadline, epoch)
        .map_err(Error::Selection)?
        .ok_or(Error::MissingSelection)?;
    let ids = editor::accounts(original.shared()).map_err(Error::Edit)?;
    let shared = editor::edit(original.shared(), edit).map_err(Error::Edit)?;
    let mut sources = Vec::new();
    sources
        .try_reserve_exact(original.profile_count())
        .map_err(|_| Error::Capacity)?;
    for index in 0..original.profile_count() {
        let profile = original.profile(index).ok_or(Error::Integrity)?;
        let source = if matches!(edit, Edit::Create { .. }) && ids.len() == 1 {
            editor::explicit_postmaster(profile.source, *ids.first().ok_or(Error::Integrity)?)
                .map_err(Error::Edit)?
        } else {
            let mut bytes = Vec::new();
            bytes
                .try_reserve_exact(profile.source.len())
                .map_err(|_| Error::Capacity)?;
            bytes.extend_from_slice(profile.source);
            bytes
        };
        sources.push(source);
    }
    let mut profiles = Vec::new();
    profiles
        .try_reserve_exact(sources.len())
        .map_err(|_| Error::Capacity)?;
    for (index, source) in sources.iter().enumerate() {
        let original = original.profile(index).ok_or(Error::Integrity)?;
        profiles.push(Profile {
            name: original.name,
            roles: original.roles,
            source,
        });
    }
    let candidate = configuration::prepare(request, trust, deadline, epoch, &shared, &profiles)
        .map_err(Error::Configuration)?;
    let version = versions::fresh_version().map_err(Error::Version)?;
    let change = selection::prepare(epoch, selected.generation, version, &candidate)
        .map_err(Error::Selection)?;
    let creation = if matches!(edit, Edit::Create { .. }) {
        Some(accounts::prepare(epoch, edit.account()).map_err(Error::Account)?)
    } else {
        None
    };
    let source = source_bytes(&candidate)?;
    Ok(Operation {
        account: edit.account(),
        creation,
        change,
        source,
    })
}
fn source_bytes(candidate: &Candidate) -> Result<Vec<u8>, Error> {
    let mut source = Vec::new();
    source
        .try_reserve_exact(MAX_OPERATION_BYTES)
        .map_err(|_| Error::Capacity)?;
    configuration::visit_bytes(candidate, |part| {
        if source
            .len()
            .checked_add(part.len())
            .filter(|n| *n <= MAX_OPERATION_BYTES)
            .is_none()
        {
            return Err(configuration::Error::Capacity);
        }
        source.extend_from_slice(part);
        Ok(())
    })
    .map_err(Error::Configuration)?;
    Ok(source)
}

/// A receipt is historical: success does not imply this version is still current
/// or that any service has restarted. Absence never permits new operation IDs.
pub fn inspect(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    operation: &Operation,
) -> Result<Option<Selection>, Error> {
    selection::inspect(request, trust, deadline, operation.change).map_err(Error::Selection)
}

pub fn apply(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    operation: &Operation,
) -> Result<Selection, Error> {
    if let Some(receipt) = inspect(request, trust, deadline, operation)? {
        return Ok(receipt);
    }
    let (shared, profiles) =
        versions::decode(&operation.source, operation.change.epoch()).map_err(Error::Version)?;
    let candidate = configuration::prepare(
        request,
        trust,
        deadline,
        operation.change.epoch(),
        shared,
        &profiles,
    )
    .map_err(Error::Configuration)?;
    if !operation.change.matches_candidate(&candidate)
        || !editor::accounts(shared)
            .map_err(Error::Edit)?
            .contains(&operation.account)
    {
        return Err(Error::Integrity);
    }
    if let Some(creation) = operation.creation {
        accounts::publish(request, trust, deadline, creation).map_err(Error::Account)?;
    }
    versions::publish(
        request,
        trust,
        deadline,
        operation.change.version(),
        &candidate,
    )
    .map_err(Error::Version)?;
    selection::publish(request, trust, deadline, operation.change).map_err(Error::Selection)
}
