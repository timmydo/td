//! Commit the old page's cursor and exact next work atomically.
use crate::{
    postgres_probe,
    postgres_replica_apply::Error,
    postgres_replica_progress,
    postgres_replica_source::Source,
    replica_pending::{Advance, Pending, Ticket},
    replica_transition::{Anchor, Transition},
};
use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::Instant,
};
use tokio_postgres::{types::Type, Client};

pub(crate) async fn advance(
    client: &Client,
    transition: &Transition,
    deadline: Instant,
    marker: &AtomicBool,
) -> Result<Ticket, Error> {
    let account = transition.account.as_bytes().as_slice();
    if client
        .query_typed_opt(
            "SELECT id FROM td_mta.accounts WHERE id=$1 FOR UPDATE",
            &[(&account, Type::BYTEA)],
        )
        .await
        .map_err(|_| Error::Database)?
        .is_none()
    {
        return Err(Error::Missing);
    }
    let source = Source::new(transition.source.origin(), transition.source.epoch())
        .map_err(|_| Error::Source)?;
    let progress = postgres_replica_progress::read_snapshot(client, source, transition.account)
        .await
        .map_err(|_| Error::Integrity)?;
    let current = progress.pending.as_ref().ok_or(Error::Stale)?;
    let pending = &transition.pending;
    if progress.generation != transition.previous.generation {
        return Err(Error::Stale);
    }
    if current.ticket() == pending.ticket() {
        let same_anchor = match transition.anchor {
            Anchor::Inventory(cursor) => {
                !progress.inventory_complete && progress.inventory_cursor == Some(cursor)
            }
            Anchor::Changes(state) => progress.inventory_complete && progress.email_cursor == state,
        };
        if !same_anchor || !same_page(current, pending) {
            return Err(Error::Stale);
        }
        // A matching retry must never clear work completed on the new page.
    } else {
        if current.ticket() != transition.previous || !current.complete() {
            return Err(Error::Stale);
        }
        let (cursor, complete, email_cursor) = match (current.advance(), transition.anchor) {
            (Advance::Inventory(Some(expected)), Anchor::Inventory(actual))
                if expected == actual
                    && pending.observed().sequence >= current.observed().sequence =>
            {
                (Some(actual), false, progress.email_cursor)
            }
            (Advance::Inventory(None), Anchor::Changes(old)) if old == progress.email_cursor => {
                (None, true, old)
            }
            (Advance::Changes { state, .. }, Anchor::Changes(old)) if state == old => {
                (None, true, state)
            }
            _ => return Err(Error::Stale),
        };
        let mut encoded = [0; crate::postgres_read::ReplicaInventoryCursor::WIRE_BYTES];
        let cursor = cursor
            .map(|c| c.encode(&mut encoded))
            .transpose()
            .map_err(|_| Error::Integrity)?;
        if client.execute_typed("UPDATE td_mta.replica_progress SET inventory_cursor=$2,inventory_complete=$3,email_cursor=$4 WHERE account=$1",&[(&account,Type::BYTEA),(&cursor,Type::TEXT),(&complete,Type::BOOL),(&email_cursor.sequence.number().to_be_bytes().as_slice(),Type::BYTEA)]).await.map_err(|_|Error::Database)?!=1 {return Err(Error::Integrity);}
        let mut ids = Vec::new();
        ids.try_reserve_exact(pending.ids().len().checked_mul(16).ok_or(Error::Capacity)?)
            .map_err(|_| Error::Capacity)?;
        for id in pending.ids() {
            ids.extend_from_slice(id.as_bytes());
        }
        let (kind, next_cursor, next_state, more) = match pending.advance() {
            Advance::Inventory(cursor) => (1i32, cursor, None, false),
            Advance::Changes { state, more } => (
                2i32,
                None,
                Some(state.sequence.number().to_be_bytes()),
                more,
            ),
        };
        let mut next_encoded = [0; crate::postgres_read::ReplicaInventoryCursor::WIRE_BYTES];
        let next_cursor = next_cursor
            .map(|c| c.encode(&mut next_encoded))
            .transpose()
            .map_err(|_| Error::Integrity)?;
        let next_state = next_state.as_ref().map(|n| n.as_slice());
        if client.execute_typed("UPDATE td_mta.replica_pending SET generation=$2,page=$3,kind=$4,observed=$5,ids=$6,done=$7,next_cursor=$8,next_state=$9,more=$10 WHERE account=$1",&[(&account,Type::BYTEA),(&pending.ticket().generation.to_be_bytes().as_slice(),Type::BYTEA),(&pending.ticket().page.to_be_bytes().as_slice(),Type::BYTEA),(&kind,Type::INT4),(&pending.observed().sequence.number().to_be_bytes().as_slice(),Type::BYTEA),(&ids.as_slice(),Type::BYTEA),(&pending.done().as_slice(),Type::BYTEA),(&next_cursor,Type::TEXT),(&next_state,Type::BYTEA),(&more,Type::BOOL)]).await.map_err(|_|Error::Database)?!=1 {return Err(Error::Integrity);}
    }
    if Instant::now() >= deadline {
        return Err(Error::Connection(postgres_probe::Error::Deadline));
    }
    marker.store(true, Ordering::Relaxed);
    client
        .batch_execute("COMMIT")
        .await
        .map_err(|_| Error::Indeterminate)?;
    Ok(pending.ticket())
}
fn same_page(a: &Pending, b: &Pending) -> bool {
    a.ticket() == b.ticket()
        && a.observed() == b.observed()
        && a.ids() == b.ids()
        && a.advance() == b.advance()
}
