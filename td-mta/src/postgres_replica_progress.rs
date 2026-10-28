//! Historical copy progress; returned values grant no publication authority.
use crate::{
    format::Sequence,
    ids::{AccountId, EmailId, StoreEpoch},
    postgres_maintenance,
    postgres_probe::{self, Mode, Request},
    postgres_read::ReplicaInventoryCursor,
    postgres_replica_source::{self, Source},
    postgres_schema,
    replica_pending::{Pending, Ticket},
    replica_seed::Limits,
    replica_source::{Changes, Inventory},
    sync::{DataState, DataType},
};
use std::time::Instant;
use td_crypto::TrustStore;
use tokio_postgres::{types::Type, Row};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Connection(postgres_probe::Error),
    Schema(postgres_schema::Error),
    Owner,
    Epoch,
    Source,
    Missing,
    Integrity,
    Capacity,
    Database,
}

/// A consistent historical observation, never a writer lease or completeness proof.
pub struct Progress {
    pub source: Source,
    pub account: AccountId,
    pub seed_digest: [u8; 32],
    pub limits: Limits,
    pub generation: u64,
    pub email_cursor: DataState,
    pub mailbox_checkpoint: DataState,
    pub inventory_base: DataState,
    pub inventory_cursor: Option<ReplicaInventoryCursor>,
    pub inventory_complete: bool,
    pub email_count: u64,
    pub receipt_count: u64,
    pub pending: Option<Pending>,
}

pub fn inspect(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    epoch: StoreEpoch,
    account: AccountId,
) -> Result<Progress, Error> {
    if request.mode != Mode::Preparation {
        return Err(Error::Owner);
    }
    let result = postgres_probe::with_reconcile_client(request, trust, deadline, move |client| {
        Box::pin(async move {
            let receipt = postgres_schema::lock_and_inspect(client).await.map_err(Error::Schema)?;
            if receipt.store_epoch != epoch {
                return Err(Error::Epoch);
            }
            client.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY; SET LOCAL idle_in_transaction_session_timeout=10000").await.map_err(|_| Error::Database)?;
            postgres_maintenance::owner(client).await.map_err(|_| Error::Owner)?;
            let source = postgres_replica_source::read(client).await.map_err(|_|Error::Source)?.ok_or(Error::Source)?;
            if source.epoch() == epoch { return Err(Error::Integrity); }
            let row = client.query_typed_opt("SELECT a.seed_digest,a.body_limit,a.blob_limit,a.email_limit,a.receipt_limit,p.generation,p.email_cursor,p.mailbox_checkpoint,p.inventory_base,p.inventory_cursor,p.inventory_complete,p.email_count,p.receipt_count FROM td_mta.replica_accounts a JOIN td_mta.replica_progress p USING(account) WHERE a.account=$1", &[(&account.as_bytes().as_slice(),Type::BYTEA)]).await.map_err(|_|Error::Database)?.ok_or(Error::Missing)?;
            let state = |index, kind| -> Result<DataState, Error> { Ok(DataState{epoch:source.epoch(),account,kind,sequence:Sequence::from_u64(sequence(&row,index)?)}) };
            let seed_digest = bytes(&row,0)?;
            let limits = Limits{body_bytes:count(&row,1)?,blobs:count(&row,2)?,emails:count(&row,3)?,receipts:count(&row,4)?};
            limits.validate().map_err(|_|Error::Integrity)?;
            let generation = sequence(&row,5)?;
            if generation == 0 { return Err(Error::Integrity); }
            let email_cursor = state(6,DataType::Email)?;
            let mailbox_checkpoint = state(7,DataType::Mailbox)?;
            let inventory_base = state(8,DataType::Email)?;
            let inventory_cursor = cursor(&row,9)?;
            let inventory_complete = row.try_get(10).map_err(|_|Error::Integrity)?;
            let email_count=count(&row,11)?;
            let receipt_count=count(&row,12)?;
            if email_count>limits.emails || receipt_count>limits.receipts || email_cursor.sequence<inventory_base.sequence || inventory_cursor.is_some_and(|c| c.base != inventory_base) {
                return Err(Error::Integrity);
            }
            let pending = client.query_typed_opt("SELECT generation,page,kind,observed,ids,done,next_cursor,next_state,more FROM td_mta.replica_pending WHERE account=$1", &[(&account.as_bytes().as_slice(),Type::BYTEA)]).await.map_err(|_|Error::Database)?;
            let pending = pending.as_ref().map(|row| load_pending(row,generation,email_cursor,inventory_base,inventory_cursor,inventory_complete)).transpose()?;
            if !inventory_complete && pending.is_none() { return Err(Error::Integrity); }
            client.batch_execute("ROLLBACK").await.map_err(|_|Error::Database)?;
            Ok(Progress {source,account,seed_digest,limits,generation,email_cursor,mailbox_checkpoint,inventory_base,inventory_cursor,inventory_complete,email_count,receipt_count,pending})
        })
    }).map_err(Error::Connection)?;
    if Instant::now() >= deadline {
        return Err(Error::Connection(postgres_probe::Error::Deadline));
    }
    result
}
fn load_pending(
    row: &Row,
    generation: u64,
    email_cursor: DataState,
    base: DataState,
    previous: Option<ReplicaInventoryCursor>,
    complete: bool,
) -> Result<Pending, Error> {
    let ticket = Ticket {
        generation: sequence(row, 0)?,
        page: sequence(row, 1)?,
    };
    if ticket.generation != generation {
        return Err(Error::Integrity);
    }
    let kind: i32 = row.try_get(2).map_err(|_| Error::Integrity)?;
    let observed = DataState {
        sequence: Sequence::from_u64(sequence(row, 3)?),
        ..email_cursor
    };
    let raw: &[u8] = row.try_get(4).map_err(|_| Error::Integrity)?;
    let (chunks, remainder) = raw.as_chunks::<16>();
    if !remainder.is_empty() || chunks.len() > crate::replica_pending::MAX_IDS {
        return Err(Error::Integrity);
    }
    let mut ids = Vec::new();
    ids.try_reserve_exact(chunks.len())
        .map_err(|_| Error::Capacity)?;
    for id in chunks {
        ids.push(EmailId::from_bytes(*id));
    }
    if ids.windows(2).any(|pair| pair.first() >= pair.get(1)) {
        return Err(Error::Integrity);
    }
    let done: &[u8] = row.try_get(5).map_err(|_| Error::Integrity)?;
    let next = cursor(row, 6)?;
    let next_state: Option<&[u8]> = row.try_get(7).map_err(|_| Error::Integrity)?;
    let more: bool = row.try_get(8).map_err(|_| Error::Integrity)?;
    if previous.is_some_and(|old| next.is_some_and(|new| old.upper != new.upper)) {
        return Err(Error::Integrity);
    }
    let mut pending = match kind {
        1 => {
            if complete
                || (previous.is_none() && observed != base)
                || more
                || next_state.is_some()
                || email_cursor != base
                || previous.is_some_and(|c| ids.iter().any(|id| *id <= c.after || *id > c.upper))
            {
                return Err(Error::Integrity);
            }
            Pending::inventory(
                ticket,
                &Inventory {
                    previous,
                    base,
                    observed,
                    ids,
                    next,
                },
            )
        }
        2 => {
            if !complete || next.is_some() {
                return Err(Error::Integrity);
            }
            let new = DataState {
                sequence: Sequence::from_u64(u64::from_be_bytes(
                    next_state
                        .ok_or(Error::Integrity)?
                        .try_into()
                        .map_err(|_| Error::Integrity)?,
                )),
                ..email_cursor
            };
            if new != observed {
                return Err(Error::Integrity);
            }
            Pending::changes(
                ticket,
                &Changes {
                    old: email_cursor,
                    new,
                    more,
                    created: vec![],
                    updated: ids,
                    destroyed: vec![],
                },
            )
        }
        _ => return Err(Error::Integrity),
    }
    .map_err(|_| Error::Integrity)?;
    pending.restore_done(done).map_err(|_| Error::Integrity)?;
    Ok(pending)
}
fn bytes<const N: usize>(row: &Row, index: usize) -> Result<[u8; N], Error> {
    let raw: &[u8] = row.try_get(index).map_err(|_| Error::Integrity)?;
    raw.try_into().map_err(|_| Error::Integrity)
}
fn sequence(row: &Row, index: usize) -> Result<u64, Error> {
    Ok(u64::from_be_bytes(bytes(row, index)?))
}
fn count(row: &Row, index: usize) -> Result<u64, Error> {
    u64::try_from(row.try_get::<_, i64>(index).map_err(|_| Error::Integrity)?)
        .map_err(|_| Error::Integrity)
}
fn cursor(row: &Row, index: usize) -> Result<Option<ReplicaInventoryCursor>, Error> {
    let raw: Option<&str> = row.try_get(index).map_err(|_| Error::Integrity)?;
    raw.map(ReplicaInventoryCursor::decode)
        .transpose()
        .map_err(|_| Error::Integrity)
}
