//! Shared selected-profile admission for finite primary read snapshots.
use crate::postgres_probe::Client;
use crate::{
    device_credentials::Presented,
    format::Sequence,
    ids::{AccountId, StoreEpoch},
    ports::ViewIdentity,
    postgres_devices,
    postgres_runtime_profile::Binding,
    postgres_schema,
    postgres_selection::Selection,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Error {
    Schema(postgres_schema::Error),
    Credential(postgres_devices::Error),
    Epoch,
    Policy,
    Unauthenticated,
    Integrity,
    Database,
}
#[derive(Clone, Copy)]
pub(crate) struct ReadBinding {
    pub(crate) account: AccountId,
    pub(crate) epoch: StoreEpoch,
    selection: Selection,
    jmap: bool,
}
impl From<&Binding> for ReadBinding {
    fn from(binding: &Binding) -> Self {
        Self {
            account: binding.account(),
            epoch: binding.epoch(),
            selection: binding.selection(),
            jmap: binding.roles().jmap(),
        }
    }
}

pub(crate) fn begin(
    client: &crate::postgres_probe::Client,
    binding: &Binding,
    presented: Presented,
) -> Result<ViewIdentity, Error> {
    if !binding.roles().jmap() {
        return Err(Error::Policy);
    }
    let schema = client.inspect_schema().map_err(Error::Schema)?;
    if schema.store_epoch != binding.epoch() {
        return Err(Error::Epoch);
    }
    client.batch("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY; SET LOCAL idle_in_transaction_session_timeout=10000").map_err(|_|Error::Database)?;
    observe(client, binding.into(), presented)
}

// The caller owns a freshly qualified transaction and its migration lock.
pub(crate) fn observe(
    client: &Client,
    binding: ReadBinding,
    presented: Presented,
) -> Result<ViewIdentity, Error> {
    selected(client, binding)?;
    postgres_devices::authenticate(client, binding.account, presented)
        .map_err(Error::Credential)?
        .ok_or(Error::Unauthenticated)?;
    identity(client, binding)
}

pub(crate) fn observe_events(
    client: &Client,
    binding: ReadBinding,
    principal: crate::http_auth::Principal,
    credential: crate::http_auth::EventCredential<Presented>,
) -> Result<crate::http_auth::EventObservation, Error> {
    if principal.account != binding.account {
        return Err(Error::Unauthenticated);
    }
    selected(client, binding)?;
    let credential_stamp = postgres_devices::event_stamp(client, principal, credential)
        .map_err(Error::Credential)?
        .ok_or(Error::Unauthenticated)?;
    Ok(crate::http_auth::EventObservation {
        credential_stamp,
        identity: identity(client, binding)?,
    })
}

fn selected(client: &Client, binding: ReadBinding) -> Result<(), Error> {
    if !binding.jmap {
        return Err(Error::Policy);
    }
    let selected = client
        .query_one(
            "SELECT generation,version_id FROM td_mta.config_selection WHERE singleton=true",
            &[],
        )
        .map_err(|_| Error::Database)?;
    let generation = sequence(selected.try_get(0).map_err(|_| Error::Integrity)?)?;
    let version: Option<&[u8]> = selected.try_get(1).map_err(|_| Error::Integrity)?;
    if generation.number() != binding.selection.generation
        || version != Some(binding.selection.version.as_bytes().as_slice())
    {
        return Err(Error::Policy);
    }
    Ok(())
}

fn identity(client: &Client, binding: ReadBinding) -> Result<ViewIdentity, Error> {
    let row = client
        .query_one(
            "SELECT sequence,floor FROM td_mta.accounts WHERE id=$1",
            &[&binding.account.as_bytes().as_slice()],
        )
        .map_err(|_| Error::Database)?;
    let committed_sequence = sequence(row.try_get(0).map_err(|_| Error::Integrity)?)?;
    let history_floor = sequence(row.try_get(1).map_err(|_| Error::Integrity)?)?;
    if history_floor > committed_sequence {
        return Err(Error::Integrity);
    }
    Ok(ViewIdentity {
        account: binding.account,
        epoch: binding.epoch,
        committed_sequence,
        history_floor,
    })
}
fn sequence(bytes: &[u8]) -> Result<Sequence, Error> {
    Ok(Sequence::from_u64(u64::from_be_bytes(
        bytes.try_into().map_err(|_| Error::Integrity)?,
    )))
}
