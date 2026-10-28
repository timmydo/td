//! Owner publication of a preserved source account and its initial copy page.
use crate::{
    ids::StoreEpoch,
    postgres_maintenance,
    postgres_probe::{self, Mode, Request},
    postgres_replica_source, postgres_schema,
    replica_pending::Advance,
    replica_seed::Seed,
};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Instant,
};
use td_crypto::TrustStore;
use tokio_postgres::{types::Type, Client};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Connection(postgres_probe::Error),
    Schema(postgres_schema::Error),
    Owner,
    Epoch,
    Source,
    Exists,
    Conflict,
    Integrity,
    Capacity,
    Database,
    Indeterminate,
}

/// Retain the same immutable seed until an uncertain creation is resolved.
/// A matching existing receipt never overwrites later mail or copy progress.
pub fn initialize(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    epoch: StoreEpoch,
    seed: Arc<Seed>,
) -> Result<(), Error> {
    if request.mode != Mode::Preparation {
        return Err(Error::Owner);
    }
    if epoch == seed.source.epoch() {
        return Err(Error::Epoch);
    }
    let attempted = Arc::new(AtomicBool::new(false));
    let marker = attempted.clone();
    let result = postgres_probe::with_reconcile_client(request, trust, deadline, move |client| {
        Box::pin(async move {
            let receipt = postgres_schema::lock_and_inspect(client)
                .await
                .map_err(Error::Schema)?;
            if receipt.store_epoch != epoch {
                return Err(Error::Epoch);
            }
            client.batch_execute("BEGIN ISOLATION LEVEL READ COMMITTED; SET LOCAL synchronous_commit=on; SET LOCAL idle_in_transaction_session_timeout=10000; SET LOCAL lock_timeout=1000").await.map_err(|_|Error::Database)?;
            postgres_maintenance::owner(client)
                .await
                .map_err(|_| Error::Owner)?;
            let source = postgres_replica_source::read(client)
                .await
                .map_err(|_| Error::Source)?
                .ok_or(Error::Source)?;
            if source != seed.source {
                return Err(Error::Source);
            }
            let account = seed.account.as_bytes().as_slice();
            let one = 1u64.to_be_bytes();
            let zero = 0u64.to_be_bytes();
            let written=client.execute_typed("INSERT INTO td_mta.accounts(id,sequence,floor) VALUES($1,$2,$3) ON CONFLICT DO NOTHING", &[(&account,Type::BYTEA),(&one.as_slice(),Type::BYTEA),(&zero.as_slice(),Type::BYTEA)]).await.map_err(|_|Error::Database)?;
            if written == 0 {
                let row = client
                    .query_typed_opt(
                        "SELECT seed_digest FROM td_mta.replica_accounts WHERE account=$1",
                        &[(&account, Type::BYTEA)],
                    )
                    .await
                    .map_err(|_| Error::Database)?
                    .ok_or(Error::Exists)?;
                let digest: &[u8] = row.try_get(0).map_err(|_| Error::Integrity)?;
                if digest != seed.digest.as_slice() {
                    return Err(Error::Conflict);
                }
                client
                    .batch_execute("ROLLBACK")
                    .await
                    .map_err(|_| Error::Database)?;
                return Ok(());
            }
            if written != 1 {
                return Err(Error::Integrity);
            }
            let graph = seed.mailboxes.view().map_err(|_| Error::Integrity)?;
            let mailbox_state = graph.observed.sequence.number().to_be_bytes();
            let base = seed.base.sequence.number().to_be_bytes();
            let body = i64::try_from(seed.limits.body_bytes).map_err(|_| Error::Capacity)?;
            let blobs = i64::try_from(seed.limits.blobs).map_err(|_| Error::Capacity)?;
            let emails = i64::try_from(seed.limits.emails).map_err(|_| Error::Capacity)?;
            let receipts = i64::try_from(seed.limits.receipts).map_err(|_| Error::Capacity)?;
            let count = i64::try_from(graph.list.len()).map_err(|_| Error::Capacity)?;
            if client.execute_typed("INSERT INTO td_mta.replica_accounts(account,source,seed_digest,body_limit,blob_limit,email_limit,receipt_limit) VALUES($1,true,$2,$3,$4,$5,$6)", &[(&account,Type::BYTEA),(&seed.digest.as_slice(),Type::BYTEA),(&body,Type::INT8),(&blobs,Type::INT8),(&emails,Type::INT8),(&receipts,Type::INT8)]).await.map_err(|_|Error::Database)?!=1 {return Err(Error::Integrity);}
            if client.execute_typed("INSERT INTO td_mta.replica_progress(account,generation,email_cursor,mailbox_checkpoint,inventory_base,inventory_cursor,inventory_complete,email_count,receipt_count) VALUES($1,$2,$3,$4,$3,NULL,false,0,$5)", &[(&account,Type::BYTEA),(&one.as_slice(),Type::BYTEA),(&base.as_slice(),Type::BYTEA),(&mailbox_state.as_slice(),Type::BYTEA),(&count,Type::INT8)]).await.map_err(|_|Error::Database)?!=1 {return Err(Error::Integrity);}
            if client
                .execute_typed(
                    "INSERT INTO td_mta.account_usage VALUES($1,0,0,0,0,0)",
                    &[(&account, Type::BYTEA)],
                )
                .await
                .map_err(|_| Error::Database)?
                != 1
            {
                return Err(Error::Integrity);
            }
            let mut ids = Vec::new();
            let mut names = Vec::new();
            let mut parents = Vec::new();
            let mut roles = Vec::new();
            let mut sorts = Vec::new();
            let mut subscriptions = Vec::new();
            let n = graph.list.len();
            ids.try_reserve_exact(n).map_err(|_| Error::Capacity)?;
            names.try_reserve_exact(n).map_err(|_| Error::Capacity)?;
            parents.try_reserve_exact(n).map_err(|_| Error::Capacity)?;
            roles.try_reserve_exact(n).map_err(|_| Error::Capacity)?;
            sorts.try_reserve_exact(n).map_err(|_| Error::Capacity)?;
            subscriptions
                .try_reserve_exact(n)
                .map_err(|_| Error::Capacity)?;
            for mailbox in &graph.list {
                ids.push(mailbox.id.as_bytes().as_slice());
                names.push(mailbox.row.name);
                parents.push(
                    mailbox
                        .row
                        .parent
                        .as_ref()
                        .map(|id| id.as_bytes().as_slice()),
                );
                roles.push(mailbox.row.role);
                sorts.push(i64::from(mailbox.row.sort_order));
                subscriptions.push(i64::from(mailbox.row.subscribed));
            }
            if client.execute_typed("INSERT INTO td_mta.mailboxes(account,id,name,parent_id,role,sort_order,subscribed,changed) SELECT $1,id,name,parent_id,role,sort_order,subscribed,$2 FROM unnest($3::bytea[],$4::text[],$5::bytea[],$6::text[],$7::bigint[],$8::bigint[]) q(id,name,parent_id,role,sort_order,subscribed)",&[(&account,Type::BYTEA),(&one.as_slice(),Type::BYTEA),(&ids,Type::BYTEA_ARRAY),(&names,Type::TEXT_ARRAY),(&parents,Type::BYTEA_ARRAY),(&roles,Type::TEXT_ARRAY),(&sorts,Type::INT8_ARRAY),(&subscriptions,Type::INT8_ARRAY)]).await.map_err(|_|Error::Database)?!=n as u64 {return Err(Error::Integrity);}
            if client.execute_typed("INSERT INTO td_mta.changes(account,sequence,operation,kind,action,object) SELECT $1,$2,ordinality,1,1,id FROM unnest($3::bytea[]) WITH ORDINALITY q(id,ordinality)",&[(&account,Type::BYTEA),(&one.as_slice(),Type::BYTEA),(&ids,Type::BYTEA_ARRAY)]).await.map_err(|_|Error::Database)?!=n as u64 {return Err(Error::Integrity);}
            if client.execute_typed("INSERT INTO td_mta.replica_mailbox_receipts(account,id,observed,present) SELECT $1,id,$2,true FROM unnest($3::bytea[]) q(id)",&[(&account,Type::BYTEA),(&mailbox_state.as_slice(),Type::BYTEA),(&ids,Type::BYTEA_ARRAY)]).await.map_err(|_|Error::Database)?!=n as u64 {return Err(Error::Integrity);}
            if client
                .execute_typed(
                    "INSERT INTO td_mta.account_bootstraps(account,inbox) VALUES($1,$2)",
                    &[
                        (&account, Type::BYTEA),
                        (&seed.inbox.as_bytes().as_slice(), Type::BYTEA),
                    ],
                )
                .await
                .map_err(|_| Error::Database)?
                != 1
            {
                return Err(Error::Integrity);
            }
            initial_page(client, &seed).await?;
            marker.store(true, Ordering::Relaxed);
            client
                .batch_execute("COMMIT")
                .await
                .map_err(|_| Error::Indeterminate)?;
            Ok(())
        })
    });
    match result {
        Ok(Ok(())) if !attempted.load(Ordering::Relaxed) && Instant::now() >= deadline => {
            Err(Error::Connection(postgres_probe::Error::Deadline))
        }
        Ok(result) => result,
        Err(_) if attempted.load(Ordering::Relaxed) => Err(Error::Indeterminate),
        Err(error) => Err(Error::Connection(error)),
    }
}

async fn initial_page(client: &Client, seed: &Seed) -> Result<(), Error> {
    let Advance::Inventory(next) = seed.pending.advance() else {
        return Err(Error::Integrity);
    };
    let mut encoded = [0; crate::postgres_read::ReplicaInventoryCursor::WIRE_BYTES];
    let cursor = next
        .map(|cursor| cursor.encode(&mut encoded))
        .transpose()
        .map_err(|_| Error::Integrity)?;
    let mut ids = Vec::new();
    ids.try_reserve_exact(
        seed.pending
            .ids()
            .len()
            .checked_mul(16)
            .ok_or(Error::Capacity)?,
    )
    .map_err(|_| Error::Capacity)?;
    for id in seed.pending.ids() {
        ids.extend_from_slice(id.as_bytes());
    }
    let one = 1u64.to_be_bytes();
    if client.execute_typed("INSERT INTO td_mta.replica_pending(account,generation,page,kind,observed,ids,done,next_cursor,next_state,more) VALUES($1,$2,$2,1,$3,$4,$5,$6,NULL,false)",&[(&seed.account.as_bytes().as_slice(),Type::BYTEA),(&one.as_slice(),Type::BYTEA),(&seed.pending.observed().sequence.number().to_be_bytes().as_slice(),Type::BYTEA),(&ids,Type::BYTEA),(&seed.pending.done().as_slice(),Type::BYTEA),(&cursor,Type::TEXT)]).await.map_err(|_|Error::Database)?!=1 {return Err(Error::Integrity);}
    Ok(())
}
