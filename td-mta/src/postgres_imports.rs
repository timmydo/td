//! Owner-side provisional import receipts; no service or SMTP activation.
use crate::{
    ids::{AccountId, StoreEpoch},
    import_snapshot::Key,
    postgres_configuration::SharedQuotas,
    postgres_probe::{self, Mode, Request},
    postgres_selection::{self, Selection},
};
use std::time::Instant;
use td_crypto::TrustStore;
use tokio_postgres::Client;
mod email;
mod folders;
mod mailbox;
pub mod reconcile;
pub use email::{publish as publish_email, publish_unmapped as publish_unmapped_email};
pub use mailbox::{publish as publish_mailbox, publish_unmapped as publish_unmapped_mailbox};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Owner,
    Epoch,
    Policy,
    Capacity,
    Quota,
    Integrity,
    Conflict,
    SourceChanged,
    MissingMailbox,
    Invalid,
    Crypto,
    Database,
    Indeterminate,
    Connection(postgres_probe::Error),
}
#[derive(Clone)]
pub struct Authority {
    epoch: StoreEpoch,
    account: AccountId,
    selection: Selection,
    quotas: SharedQuotas,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Outcome {
    pub local_object: [u8; 16],
    pub created: bool,
    /// This invocation committed a new receipt, including Inbox adoption.
    pub recorded: bool,
    pub present: bool,
}
pub fn prepare(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    epoch: StoreEpoch,
    account: AccountId,
) -> Result<Authority, Error> {
    if request.mode != Mode::Preparation {
        return Err(Error::Owner);
    }
    postgres_probe::with_client(request, trust, deadline, |client| {
        Box::pin(async move {
            let receipt = crate::postgres_schema::lock_and_inspect(client)
                .await
                .map_err(|_| Error::Database)?;
            if receipt.store_epoch != epoch {
                return Err(Error::Epoch);
            }
            crate::postgres_maintenance::owner(client)
                .await
                .map_err(|_| Error::Owner)
        })
    })
    .map_err(Error::Connection)??;
    let (selection, candidate) = postgres_selection::read(request, trust, deadline, epoch)
        .map_err(|_| Error::Policy)?
        .ok_or(Error::Policy)?;
    let accounts =
        crate::account_configuration::accounts(candidate.shared()).map_err(|_| Error::Integrity)?;
    if !accounts.contains(&account) {
        return Err(Error::Policy);
    }
    Ok(Authority {
        epoch,
        account,
        selection,
        quotas: candidate.quotas(),
    })
}
fn sequence(bytes: &[u8]) -> Result<u64, Error> {
    Ok(u64::from_be_bytes(
        bytes.try_into().map_err(|_| Error::Integrity)?,
    ))
}
async fn begin(client: &Client, authority: &Authority) -> Result<u64, Error> {
    let receipt = crate::postgres_schema::lock_and_inspect(client)
        .await
        .map_err(|_| Error::Database)?;
    if receipt.store_epoch != authority.epoch {
        return Err(Error::Epoch);
    }
    client.batch_execute("BEGIN ISOLATION LEVEL READ COMMITTED; SET LOCAL synchronous_commit=on; SET LOCAL idle_in_transaction_session_timeout=10000").await.map_err(|_|Error::Database)?;
    crate::postgres_maintenance::owner(client)
        .await
        .map_err(|_| Error::Owner)?;
    // Acquire selection before the account, as the normal publication adapters do.
    let selected=client.query_one("SELECT generation,version_id FROM td_mta.config_selection WHERE singleton=true FOR SHARE",&[]).await.map_err(|_|Error::Database)?;
    let row = client
        .query_one(
            "SELECT sequence,floor FROM td_mta.accounts WHERE id=$1 FOR UPDATE",
            &[&authority.account.as_bytes().as_slice()],
        )
        .await
        .map_err(|_| Error::Database)?;
    let old = sequence(row.try_get(0).map_err(|_| Error::Integrity)?)?;
    let floor = sequence(row.try_get(1).map_err(|_| Error::Integrity)?)?;
    if floor > old {
        return Err(Error::Integrity);
    }
    let generation = sequence(selected.try_get(0).map_err(|_| Error::Integrity)?)?;
    let version: Option<&[u8]> = selected.try_get(1).map_err(|_| Error::Integrity)?;
    if generation != authority.selection.generation
        || version != Some(authority.selection.version.as_bytes().as_slice())
        || crate::postgres_maintenance::blocked(client)
            .await
            .map_err(|_| Error::Database)?
    {
        return Err(Error::Policy);
    }
    Ok(old)
}
async fn mapped_policy(
    client: &Client,
    authority: &Authority,
    key: &Key,
    digest: Option<&[u8; 32]>,
    old: u64,
) -> Result<Option<Outcome>, Error> {
    let account = authority.account.as_bytes().as_slice();
    let rows=client.query("SELECT local_object,historical_blob,source_digest,changed FROM td_mta.imports WHERE account=$1 AND source_instance=$2 AND source_kind=$3 AND source_account=$4 AND source_object=$5",&[&account,&key.instance.as_bytes().as_slice(),&i64::from(key.kind.tag()),&key.account.as_slice(),&key.object.as_slice()]).await.map_err(|_|Error::Database)?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let stored: &[u8] = row.try_get(2).map_err(|_| Error::Integrity)?;
    if stored.len() != 32 {
        return Err(Error::Integrity);
    }
    if digest.is_some_and(|digest| stored != digest) {
        return Err(Error::SourceChanged);
    }
    if sequence(row.try_get(3).map_err(|_| Error::Integrity)?)? > old {
        return Err(Error::Integrity);
    }
    let local: &[u8] = row.try_get(0).map_err(|_| Error::Integrity)?;
    let local_object: [u8; 16] = local.try_into().map_err(|_| Error::Integrity)?;
    let blob: Option<&[u8]> = row.try_get(1).map_err(|_| Error::Integrity)?;
    use crate::format::key::SourceKind;
    if (key.kind == SourceKind::Email) != blob.is_some() || blob.is_some_and(|b| b.len() != 16) {
        return Err(Error::Integrity);
    }
    let sql = if key.kind == SourceKind::Mailbox {
        "SELECT EXISTS(SELECT 1 FROM td_mta.mailboxes WHERE account=$1 AND id=$2)"
    } else {
        "SELECT EXISTS(SELECT 1 FROM td_mta.emails WHERE account=$1 AND id=$2)"
    };
    let present = client
        .query_one(sql, &[&account, &local])
        .await
        .map_err(|_| Error::Database)?
        .try_get(0)
        .map_err(|_| Error::Integrity)?;
    Ok(Some(Outcome {
        local_object,
        created: false,
        recorded: false,
        present,
    }))
}
async fn record(
    client: &Client,
    authority: &Authority,
    key: &Key,
    digest: &[u8; 32],
    local: &[u8; 16],
    blob: Option<&[u8]>,
    next: u64,
) -> Result<(), Error> {
    if client.execute("INSERT INTO td_mta.imports(account,source_instance,source_kind,source_account,source_object,local_object,historical_blob,source_digest,changed) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9)",&[&authority.account.as_bytes().as_slice(),&key.instance.as_bytes().as_slice(),&i64::from(key.kind.tag()),&key.account.as_slice(),&key.object.as_slice(),&local.as_slice(),&blob,&digest.as_slice(),&next.to_be_bytes().as_slice()]).await.map_err(|_|Error::Database)?!=1 {return Err(Error::Integrity);}
    if client
        .execute(
            "UPDATE td_mta.accounts SET sequence=$2 WHERE id=$1",
            &[
                &authority.account.as_bytes().as_slice(),
                &next.to_be_bytes().as_slice(),
            ],
        )
        .await
        .map_err(|_| Error::Database)?
        != 1
    {
        return Err(Error::Integrity);
    }
    Ok(())
}
async fn next(client: &Client, authority: &Authority, old: u64) -> Result<u64, Error> {
    crate::postgres_sequence::next(client, authority.account.as_bytes().as_slice(), old)
        .await
        .map_err(|e| match e {
            crate::postgres_sequence::Error::Capacity => Error::Capacity,
            _ => Error::Integrity,
        })
}
fn publication_error(error: crate::postgres_publication::Error) -> Error {
    use crate::postgres_publication::Error as E;
    match error {
        E::Quota => Error::Quota,
        E::Capacity => Error::Capacity,
        E::Conflict => Error::Conflict,
        E::Integrity => Error::Integrity,
        E::Database => Error::Database,
        E::Deadline => Error::Connection(postgres_probe::Error::Deadline),
    }
}

/// Account locking makes an absent receipt conclusive after a pending writer.
pub fn inspect(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    authority: &Authority,
    key: &Key,
    digest: &[u8; 32],
) -> Result<Option<Outcome>, Error> {
    inspect_policy(request, trust, deadline, authority, key, Some(digest))
}
/// Presence of an exact source receipt is a skip, not synchronization evidence.
pub fn inspect_mapping(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    authority: &Authority,
    key: &Key,
) -> Result<Option<Outcome>, Error> {
    if !matches!(
        key.kind,
        crate::format::key::SourceKind::Mailbox | crate::format::key::SourceKind::Email
    ) {
        return Err(Error::Invalid);
    }
    inspect_policy(request, trust, deadline, authority, key, None)
}
fn inspect_policy(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    authority: &Authority,
    key: &Key,
    digest: Option<&[u8; 32]>,
) -> Result<Option<Outcome>, Error> {
    if request.mode != Mode::Preparation {
        return Err(Error::Owner);
    }
    let authority = authority.clone();
    let key = key.clone();
    let digest = digest.copied();
    postgres_probe::with_mailbox_client(request, trust, deadline, move |client| {
        Box::pin(async move {
            let receipt = crate::postgres_schema::lock_and_inspect(client)
                .await
                .map_err(|_| Error::Database)?;
            if receipt.store_epoch != authority.epoch {
                return Err(Error::Epoch);
            }
            client
                .batch_execute("BEGIN ISOLATION LEVEL READ COMMITTED")
                .await
                .map_err(|_| Error::Database)?;
            crate::postgres_maintenance::owner(client)
                .await
                .map_err(|_| Error::Owner)?;
            let row = client
                .query_one(
                    "SELECT sequence FROM td_mta.accounts WHERE id=$1 FOR SHARE",
                    &[&authority.account.as_bytes().as_slice()],
                )
                .await
                .map_err(|_| Error::Database)?;
            mapped_policy(
                client,
                &authority,
                &key,
                digest.as_ref(),
                sequence(row.try_get(0).map_err(|_| Error::Integrity)?)?,
            )
            .await
        })
    })
    .map_err(Error::Connection)?
}
async fn parent(
    client: &Client,
    authority: &Authority,
    key: &Key,
    source: &[u8],
    old: u64,
) -> Result<[u8; 16], Error> {
    let rows=client.query("SELECT i.local_object,i.changed,m.changed FROM td_mta.imports i LEFT JOIN td_mta.mailboxes m ON m.account=i.account AND m.id=i.local_object WHERE i.account=$1 AND i.source_instance=$2 AND i.source_kind=1 AND i.source_account=$3 AND i.source_object=$4",&[&authority.account.as_bytes().as_slice(),&key.instance.as_bytes().as_slice(),&key.account.as_slice(),&source]).await.map_err(|_|Error::Database)?;
    let row = rows.first().ok_or(Error::MissingMailbox)?;
    let changed: Option<&[u8]> = row.try_get(2).map_err(|_| Error::Integrity)?;
    if sequence(changed.ok_or(Error::MissingMailbox)?)? > old
        || sequence(row.try_get(1).map_err(|_| Error::Integrity)?)? > old
    {
        return Err(Error::Integrity);
    }
    let id: &[u8] = row.try_get(0).map_err(|_| Error::Integrity)?;
    id.try_into().map_err(|_| Error::Integrity)
}
async fn unused(
    client: &Client,
    account: AccountId,
    kind: i64,
    id: &[u8; 16],
) -> Result<(), Error> {
    let sql=match kind {
        1=>"SELECT EXISTS(SELECT 1 FROM td_mta.mailboxes WHERE account=$1 AND id=$2) OR EXISTS(SELECT 1 FROM td_mta.changes WHERE account=$1 AND kind=1 AND object=$2)",
        2=>"SELECT EXISTS(SELECT 1 FROM td_mta.threads WHERE account=$1 AND id=$2) OR EXISTS(SELECT 1 FROM td_mta.changes WHERE account=$1 AND kind=2 AND object=$2)",
        3=>"SELECT EXISTS(SELECT 1 FROM td_mta.emails WHERE account=$1 AND id=$2) OR EXISTS(SELECT 1 FROM td_mta.changes WHERE account=$1 AND kind=3 AND object=$2)",
        _=>return Err(Error::Invalid),
    };
    let used: bool = client
        .query_one(sql, &[&account.as_bytes().as_slice(), &id.as_slice()])
        .await
        .map_err(|_| Error::Database)?
        .try_get(0)
        .map_err(|_| Error::Integrity)?;
    if used {
        Err(Error::Conflict)
    } else {
        Ok(())
    }
}
fn fresh() -> Result<[u8; 16], Error> {
    use td_crypto::{Entropy, SystemEntropy};
    let mut id = [0; 16];
    SystemEntropy::try_new()
        .map_err(|_| Error::Crypto)?
        .fill(&mut id)
        .map_err(|_| Error::Crypto)?;
    Ok(id)
}
async fn history(
    client: &Client,
    account: AccountId,
    next: u64,
    ordinal: i64,
    kind: i64,
    action: i64,
    object: &[u8; 16],
) -> Result<(), Error> {
    if client
        .execute(
            "INSERT INTO td_mta.changes VALUES($1,$2,$3,$4,$5,$6)",
            &[
                &account.as_bytes().as_slice(),
                &next.to_be_bytes().as_slice(),
                &ordinal,
                &kind,
                &action,
                &object.as_slice(),
            ],
        )
        .await
        .map_err(|_| Error::Database)?
        != 1
    {
        return Err(Error::Integrity);
    }
    Ok(())
}
async fn commit(client: &Client, marker: &std::sync::atomic::AtomicBool) -> Result<(), Error> {
    marker.store(true, std::sync::atomic::Ordering::Relaxed);
    client
        .batch_execute("COMMIT")
        .await
        .map_err(|_| Error::Indeterminate)
}
fn settled<T>(
    result: Result<Result<T, Error>, postgres_probe::Error>,
    attempted: &std::sync::atomic::AtomicBool,
) -> Result<T, Error> {
    match result {
        Ok(result) => result,
        Err(_) if attempted.load(std::sync::atomic::Ordering::Relaxed) => Err(Error::Indeterminate),
        Err(error) => Err(Error::Connection(error)),
    }
}
fn fixed<const N: usize>(value: &[u8]) -> Result<[u8; N], Error> {
    value.try_into().map_err(|_| Error::Integrity)
}
