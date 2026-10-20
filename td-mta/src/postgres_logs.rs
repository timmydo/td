//! Lossy bounded diagnostics. Neither a mail receipt nor an acceptance condition.
use crate::{
    bounded::TextBuffer,
    ids::StoreEpoch,
    observability::Event,
    postgres_probe::{self, Mode, Request},
    postgres_schema,
};
use std::time::Instant;
use td_crypto::TrustStore;

pub const RETENTION_MS: i64 = 30 * 24 * 60 * 60 * 1000;
pub const MAX_ROWS: i64 = 65_536;
pub const MAX_RELATION_BYTES: i64 = 96 * 1024 * 1024;
pub const BATCH: usize = 16;
pub const PAGE: usize = 100;
const PER_MINUTE: i32 = 600;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Connection(postgres_probe::Error),
    Connect(postgres_probe::Error),
    Resolution,
    Schema(postgres_schema::Error),
    Epoch,
    Invalid,
    Database,
    Integrity,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Written {
    pub stored: usize,
    pub suppressed: usize,
    pub paused: bool,
}
pub struct Record {
    pub id: i64,
    pub recorded_ms: i64,
    pub profile: String,
    pub event: String,
}
pub struct Page {
    pub records: Vec<Record>,
    pub suppressed: i64,
    pub evicted: i64,
    pub relation_bytes: i64,
}
fn profile_valid(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-'))
}

/// Same-session schema/epoch checks, a short lock wait, one bounded transaction.
/// Uncertain commits are abandoned, never replayed: diagnostics may be lost.
/// Empty batches perform incremental age retention even on quiet services.
pub fn append(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    epoch: StoreEpoch,
    profile: &str,
    events: &[Event],
) -> Result<Written, Error> {
    if request.mode != Mode::Runtime || !profile_valid(profile) || events.len() > BATCH {
        return Err(Error::Invalid);
    }
    let mut records = Vec::with_capacity(events.len());
    for event in events {
        let mut cell = [0; crate::observability::MAX_EVENT_BYTES];
        let mut output = TextBuffer::new(&mut cell);
        event.encode(&mut output).map_err(|_| Error::Invalid)?;
        records.push(output.as_str().map_err(|_| Error::Invalid)?.to_owned());
    }
    let profile = profile.to_owned();
    let entered = std::cell::Cell::new(false);
    let result = postgres_probe::with_mailbox_client(request, trust, deadline, |client| {
        entered.set(true);
        Box::pin(write(client, epoch, profile, records))
    });
    match result {
        Ok(result) => result,
        Err(error) if !entered.get() => Err(Error::Connect(error)),
        Err(error) => Err(Error::Connection(error)),
    }
}

/// Newest first, exclusive numeric cursor. Expired rows are hidden even if
/// background reclamation has not visited them yet. No peer text is accepted.
pub fn inspect(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    epoch: StoreEpoch,
    before: Option<i64>,
    limit: usize,
) -> Result<Page, Error> {
    if before.is_some_and(|v| v <= 0) || limit == 0 || limit > PAGE {
        return Err(Error::Invalid);
    }
    let limit = i64::try_from(limit).map_err(|_| Error::Invalid)?;
    let before = before.unwrap_or(i64::MAX);
    postgres_probe::with_mailbox_client(request, trust, deadline, move |client| {
        Box::pin(read(client, epoch, before, limit))
    })
    .map_err(Error::Connection)?
}

async fn write(
    client: &tokio_postgres::Client,
    epoch: StoreEpoch,
    profile: String,
    mut records: Vec<String>,
) -> Result<Written, Error> {
    let receipt = postgres_schema::lock_and_inspect(client)
        .await
        .map_err(Error::Schema)?;
    if receipt.store_epoch != epoch {
        return Err(Error::Epoch);
    }
    client.batch_execute("BEGIN; SET LOCAL lock_timeout=50; SET LOCAL statement_timeout=1000; SET LOCAL synchronous_commit=off").await.map_err(|_|Error::Database)?;
    let row=client.query_one("SELECT next_id,window_ms,used,suppressed,evicted,(pg_total_relation_size('td_mta.application_logs')+pg_total_relation_size('td_mta.application_log_state')),(extract(epoch FROM clock_timestamp())*1000)::bigint FROM td_mta.application_log_state WHERE singleton FOR UPDATE",&[]).await.map_err(|_|Error::Database)?;
    let next: i64 = row.try_get(0).map_err(|_| Error::Integrity)?;
    let window: i64 = row.try_get(1).map_err(|_| Error::Integrity)?;
    let used: i32 = row.try_get(2).map_err(|_| Error::Integrity)?;
    let suppressed: i64 = row.try_get(3).map_err(|_| Error::Integrity)?;
    let evicted: i64 = row.try_get(4).map_err(|_| Error::Integrity)?;
    let relation_bytes: i64 = row.try_get(5).map_err(|_| Error::Integrity)?;
    let now: i64 = row.try_get(6).map_err(|_| Error::Integrity)?;
    if next <= 0
        || window < 0
        || !(0..=PER_MINUTE).contains(&used)
        || suppressed < 0
        || evicted < 0
        || relation_bytes < 0
        || now < 0
    {
        return Err(Error::Integrity);
    }
    let now = now.max(window);
    let (window, used) = if now.saturating_sub(window) >= 60_000 {
        (now, 0)
    } else {
        (window, used)
    };
    let count = if relation_bytes >= MAX_RELATION_BYTES {
        0
    } else {
        records
            .len()
            .min(usize::try_from(PER_MINUTE - used).map_err(|_| Error::Integrity)?)
    };
    let lost = records.len() - count;
    records.truncate(count);
    let following = next
        .checked_add(i64::try_from(count).map_err(|_| Error::Invalid)?)
        .ok_or(Error::Integrity)?;
    let used = used
        .checked_add(i32::try_from(count).map_err(|_| Error::Invalid)?)
        .ok_or(Error::Integrity)?;
    let suppressed = suppressed.saturating_add(i64::try_from(lost).map_err(|_| Error::Invalid)?);
    let cutoff = now.saturating_sub(RETENTION_MS);
    client.execute("DELETE FROM td_mta.application_logs WHERE slot IN (SELECT slot FROM td_mta.application_logs WHERE recorded_ms<$1 ORDER BY recorded_ms,slot LIMIT 1024)",&[&cutoff]).await.map_err(|_|Error::Database)?;
    let mut evicted = evicted;
    if count > 0 {
        let replaced:i64=client.query_one("SELECT count(*) FROM td_mta.application_logs WHERE slot IN (SELECT (($1::bigint+n)%65536)::integer FROM generate_series(0,$2::bigint-1) n)",&[&next,&(count as i64)]).await.map_err(|_|Error::Database)?.try_get(0).map_err(|_|Error::Integrity)?;
        evicted = evicted.saturating_add(replaced);
        client.execute("INSERT INTO td_mta.application_logs(slot,id,recorded_ms,profile,event) SELECT (($1::bigint+ordinality-1)%65536)::integer,$1::bigint+ordinality-1,$2::bigint,$3::text,event FROM unnest($4::text[]) WITH ORDINALITY AS v(event,ordinality) ON CONFLICT(slot) DO UPDATE SET id=EXCLUDED.id,recorded_ms=EXCLUDED.recorded_ms,profile=EXCLUDED.profile,event=EXCLUDED.event",&[&next,&now,&profile,&records]).await.map_err(|_|Error::Database)?;
    }
    if relation_bytes >= MAX_RELATION_BYTES {
        client
            .batch_execute("COMMIT")
            .await
            .map_err(|_| Error::Database)?;
        return Ok(Written {
            stored: 0,
            suppressed: lost,
            paused: true,
        });
    }
    let changed=client.execute("UPDATE td_mta.application_log_state SET next_id=$1,window_ms=$2,used=$3,suppressed=$4,evicted=$5 WHERE singleton",&[&following,&window,&used,&suppressed,&evicted]).await.map_err(|_|Error::Database)?;
    if changed != 1 {
        return Err(Error::Integrity);
    }
    client
        .batch_execute("COMMIT")
        .await
        .map_err(|_| Error::Database)?;
    Ok(Written {
        stored: count,
        suppressed: lost,
        paused: false,
    })
}

async fn read(
    client: &tokio_postgres::Client,
    epoch: StoreEpoch,
    before: i64,
    limit: i64,
) -> Result<Page, Error> {
    let receipt = postgres_schema::lock_and_inspect(client)
        .await
        .map_err(Error::Schema)?;
    if receipt.store_epoch != epoch {
        return Err(Error::Epoch);
    }
    client
        .batch_execute(
            "BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY; SET LOCAL statement_timeout=1000",
        )
        .await
        .map_err(|_| Error::Database)?;
    let row=client.query_one("SELECT suppressed,evicted,(pg_total_relation_size('td_mta.application_logs')+pg_total_relation_size('td_mta.application_log_state')) FROM td_mta.application_log_state WHERE singleton",&[]).await.map_err(|_|Error::Database)?;
    let suppressed: i64 = row.try_get(0).map_err(|_| Error::Integrity)?;
    let evicted: i64 = row.try_get(1).map_err(|_| Error::Integrity)?;
    let relation_bytes: i64 = row.try_get(2).map_err(|_| Error::Integrity)?;
    let rows=client.query("SELECT id,recorded_ms,profile,event FROM td_mta.application_logs WHERE id<$1 AND recorded_ms>=(extract(epoch FROM clock_timestamp())*1000)::bigint-$2 ORDER BY id DESC LIMIT $3",&[&before,&RETENTION_MS,&limit]).await.map_err(|_|Error::Database)?;
    let mut records = Vec::with_capacity(rows.len());
    for row in rows {
        let id: i64 = row.try_get(0).map_err(|_| Error::Integrity)?;
        let recorded_ms: i64 = row.try_get(1).map_err(|_| Error::Integrity)?;
        let profile: String = row.try_get(2).map_err(|_| Error::Integrity)?;
        let event: String = row.try_get(3).map_err(|_| Error::Integrity)?;
        if id <= 0
            || recorded_ms < 0
            || !profile_valid(&profile)
            || event.len() > crate::observability::MAX_EVENT_BYTES
        {
            return Err(Error::Integrity);
        }
        records.push(Record {
            id,
            recorded_ms,
            profile,
            event,
        });
    }
    client
        .batch_execute("ROLLBACK")
        .await
        .map_err(|_| Error::Database)?;
    Ok(Page {
        records,
        suppressed,
        evicted,
        relation_bytes,
    })
}

/// Resolve on every reconnect. Only a transport failure before the operation
/// was entered can advance to another address; never replay an uncertain batch.
pub fn append_resolved(
    loaded: &crate::postgres_input::Loaded,
    executable: &std::path::Path,
    deadline: Instant,
    epoch: StoreEpoch,
    profile: &str,
    events: &[Event],
) -> Result<Written, Error> {
    let addresses = crate::postgres_resolver::resolve(executable, &loaded.bootstrap, deadline)
        .map_err(|_| Error::Resolution)?;
    for address in addresses {
        let mut request = loaded.bootstrap.request(address);
        request.mode = Mode::Runtime;
        match append(&request, &loaded.trust, deadline, epoch, profile, events) {
            Err(Error::Connect(postgres_probe::Error::Transport)) => continue,
            result => return result,
        }
    }
    Err(Error::Connect(postgres_probe::Error::Transport))
}
