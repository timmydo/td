//! Shared selected-profile admission for finite primary read snapshots.
use crate::{
    device_credentials::Presented, format::Sequence, ports::ViewIdentity, postgres_devices,
    postgres_runtime_profile::Binding, postgres_schema,
};
use tokio_postgres::Client;

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
pub(crate) async fn begin(
    client: &Client,
    binding: &Binding,
    presented: Presented,
) -> Result<ViewIdentity, Error> {
    if !binding.roles().jmap() {
        return Err(Error::Policy);
    }
    let schema = postgres_schema::lock_and_inspect(client)
        .await
        .map_err(Error::Schema)?;
    if schema.store_epoch != binding.epoch() {
        return Err(Error::Epoch);
    }
    client.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY; SET LOCAL idle_in_transaction_session_timeout=10000").await.map_err(|_|Error::Database)?;
    let selected = client
        .query_one(
            "SELECT generation,version_id FROM td_mta.config_selection WHERE singleton=true",
            &[],
        )
        .await
        .map_err(|_| Error::Database)?;
    let generation = sequence(selected.try_get(0).map_err(|_| Error::Integrity)?)?;
    let version: Option<&[u8]> = selected.try_get(1).map_err(|_| Error::Integrity)?;
    if generation.number() != binding.selection().generation
        || version != Some(binding.selection().version.as_bytes().as_slice())
    {
        return Err(Error::Policy);
    }
    postgres_devices::authenticate(client, binding.account(), presented)
        .await
        .map_err(Error::Credential)?
        .ok_or(Error::Unauthenticated)?;
    let row = client
        .query_one(
            "SELECT sequence,floor FROM td_mta.accounts WHERE id=$1",
            &[&binding.account().as_bytes().as_slice()],
        )
        .await
        .map_err(|_| Error::Database)?;
    let committed_sequence = sequence(row.try_get(0).map_err(|_| Error::Integrity)?)?;
    let history_floor = sequence(row.try_get(1).map_err(|_| Error::Integrity)?)?;
    if history_floor > committed_sequence {
        return Err(Error::Integrity);
    }
    Ok(ViewIdentity {
        account: binding.account(),
        epoch: binding.epoch(),
        committed_sequence,
        history_floor,
    })
}
fn sequence(bytes: &[u8]) -> Result<Sequence, Error> {
    Ok(Sequence::from_u64(u64::from_be_bytes(
        bytes.try_into().map_err(|_| Error::Integrity)?,
    )))
}
