//! Explicit owner-selected removal of unchanged, historically imported mail.
use super::*;

#[derive(Clone)]
pub struct Baseline {
    pub epoch: StoreEpoch,
    pub account: AccountId,
    pub key: Key,
    pub local: [u8; 16],
    pub historical_blob: [u8; 16],
    pub digest: [u8; 32],
    pub changed: u64,
}
pub enum Plan {
    Unmapped,
    Absent,
    Conflict,
    Delete(Baseline),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Applied {
    Deleted,
    Absent,
}

pub fn plan(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    authority: &Authority,
    previous: &Email,
) -> Result<Plan, Error> {
    if request.mode != Mode::Preparation {
        return Err(Error::Owner);
    }
    let authority = authority.clone();
    let previous = previous.clone();
    postgres_probe::with_reconcile_client(request, trust, deadline, move |client| {
        Box::pin(async move {
            let old = begin(client, &authority).await?;
            let Some(receipt) = receipt(client, &authority, &previous.key, old).await? else {
                return Ok(Plan::Unmapped);
            };
            if !exclusive(client, &authority, &previous.key, &receipt.local).await? {
                return Ok(Plan::Conflict);
            }
            let current = match local(client, &authority, &previous.key, &receipt, old).await {
                Ok(Some(value)) => value,
                Ok(None) => return Ok(Plan::Absent),
                Err(Error::Conflict) => return Ok(Plan::Conflict),
                Err(error) => return Err(error),
            };
            if current.digest != receipt.digest || previous.digest != receipt.digest {
                return Ok(Plan::Conflict);
            }
            Ok(Plan::Delete(Baseline {
                epoch: authority.epoch,
                account: authority.account,
                key: previous.key.clone(),
                local: receipt.local,
                historical_blob: receipt.blob,
                digest: receipt.digest,
                changed: receipt.changed,
            }))
        })
    })
    .map_err(Error::Connection)?
}

/// Caller owns explicit source-absence evidence and operator selection.
pub fn apply(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    authority: &Authority,
    previous: &Email,
    expected: &Baseline,
) -> Result<Applied, Error> {
    if request.mode != Mode::Preparation {
        return Err(Error::Owner);
    }
    if expected.epoch != authority.epoch
        || expected.account != authority.account
        || expected.key != previous.key
        || expected.digest != previous.digest
    {
        return Err(Error::Invalid);
    }
    let authority = authority.clone();
    let previous = previous.clone();
    let expected = expected.clone();
    let attempted = Arc::new(AtomicBool::new(false));
    let marker = attempted.clone();
    let result = postgres_probe::with_reconcile_client(request, trust, deadline, move |client| {
        Box::pin(async move {
            let old = begin(client, &authority).await?;
            let receipt = receipt(client, &authority, &previous.key, old)
                .await?
                .ok_or(Error::Conflict)?;
            if receipt.local != expected.local
                || receipt.blob != expected.historical_blob
                || receipt.digest != expected.digest
                || receipt.changed != expected.changed
            {
                return Err(Error::Conflict);
            }
            if !exclusive(client, &authority, &previous.key, &receipt.local).await? {
                return Err(Error::Conflict);
            }
            let Some(current) = local(client, &authority, &previous.key, &receipt, old).await?
            else {
                return Ok(Applied::Absent);
            };
            if current.digest != receipt.digest {
                return Err(Error::Conflict);
            }
            let (_, results) = crate::postgres_email_set::deletion::apply_ids(
                client,
                authority.account,
                &[crate::ids::EmailId::from_bytes(receipt.local)],
                old,
            )
            .await
            .map_err(|error| match error {
                crate::postgres_email_set::Error::Capacity => Error::Capacity,
                crate::postgres_email_set::Error::Integrity => Error::Integrity,
                _ => Error::Database,
            })?;
            if results.as_slice() != [crate::postgres_email_set::UpdateResult::Destroyed] {
                return Err(Error::Integrity);
            }
            // Retain the receipt: retries see absence, and import cannot resurrect it.
            commit(client, &marker).await?;
            Ok(Applied::Deleted)
        })
    });
    settled(result, &attempted)
}

async fn exclusive(
    client: &Client,
    authority: &Authority,
    key: &Key,
    local: &[u8; 16],
) -> Result<bool, Error> {
    client.query_one("SELECT NOT EXISTS(SELECT 1 FROM td_mta.imports WHERE account=$1 AND source_kind=3 AND local_object=$2 AND (source_instance<>$3 OR source_account<>$4 OR source_object<>$5))",&[&authority.account.as_bytes().as_slice(),&local.as_slice(),&key.instance.as_bytes().as_slice(),&key.account.as_slice(),&key.object.as_slice()]).await.map_err(|_|Error::Database)?.try_get(0).map_err(|_|Error::Integrity)
}
