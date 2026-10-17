//! Bounded historical device metadata for explicit administration.
use super::{
    device_credentials, postgres_probe, postgres_schema, AccountId, DeviceId, Error, Request,
    StoreEpoch, TrustStore, DEVICE_CAPACITY,
};
use std::time::Instant;

/// Public registry metadata only; no credential verifier or serving authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceSummary {
    pub id: DeviceId,
    pub label: String,
    pub created_at: u64,
    pub revoked_at: Option<u64>,
}

/// Inspect one account's complete bounded registry, including revoked devices.
/// The caller supplies database administration credentials and an expected epoch.
pub fn list(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    epoch: StoreEpoch,
    account: AccountId,
) -> Result<Vec<DeviceSummary>, Error> {
    let result = postgres_probe::with_mailbox_client(request, trust, deadline, move |client| {
        Box::pin(async move {
            let schema = postgres_schema::lock_and_inspect(client).await.map_err(Error::Schema)?;
            if schema.store_epoch != epoch {
                return Err(Error::Epoch);
            }
            client.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY; SET LOCAL idle_in_transaction_session_timeout=10000")
                .await.map_err(|_| Error::Database)?;
            if client.query("SELECT id FROM td_mta.accounts WHERE id=$1", &[&account.as_bytes().as_slice()])
                .await.map_err(|_| Error::Database)?.is_empty() {
                return Err(Error::Missing);
            }
            let limit = i64::try_from(DEVICE_CAPACITY.checked_add(1).ok_or(Error::Capacity)?)
                .map_err(|_| Error::Capacity)?;
            let rows = client.query("SELECT id,label,created_at,revoked_at FROM td_mta.devices WHERE account=$1 ORDER BY id LIMIT $2",
                &[&account.as_bytes().as_slice(), &limit]).await.map_err(|_| Error::Database)?;
            if rows.len() > DEVICE_CAPACITY {
                return Err(Error::Capacity);
            }
            let mut devices = Vec::new();
            devices.try_reserve_exact(rows.len()).map_err(|_| Error::Capacity)?;
            for row in rows {
                let id: &[u8] = row.try_get(0).map_err(|_| Error::Invalid)?;
                let id = DeviceId::from_bytes(id.try_into().map_err(|_| Error::Invalid)?);
                let label: String = row.try_get(1).map_err(|_| Error::Invalid)?;
                device_credentials::valid_label(&label).map_err(|_| Error::Invalid)?;
                let created: i64 = row.try_get(2).map_err(|_| Error::Invalid)?;
                let revoked: Option<i64> = row.try_get(3).map_err(|_| Error::Invalid)?;
                devices.push(DeviceSummary {
                    id,
                    label,
                    created_at: u64::try_from(created).map_err(|_| Error::Invalid)?,
                    revoked_at: revoked.map(u64::try_from).transpose().map_err(|_| Error::Invalid)?,
                });
            }
            client.batch_execute("ROLLBACK").await.map_err(|_| Error::Database)?;
            Ok(devices)
        })
    }).map_err(Error::Connection)?;
    if Instant::now() >= deadline {
        return Err(Error::Connection(postgres_probe::Error::Deadline));
    }
    result
}
