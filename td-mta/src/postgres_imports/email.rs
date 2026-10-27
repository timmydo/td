use super::*;
use crate::{
    ids::{BlobId, ThreadId},
    import_snapshot::Email,
    postgres_publication as publication,
};
use std::sync::{atomic::AtomicBool, Arc};
use td_crypto::{Digest, Sha256};

pub fn publish(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    authority: &Authority,
    value: &Email,
    bytes: &[u8],
) -> Result<Outcome, Error> {
    publish_policy(request, trust, deadline, authority, value, bytes, None)
}
pub fn publish_unmapped(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    authority: &Authority,
    value: &Email,
    bytes: &[u8],
    definitions: &[crate::import_snapshot::Mailbox],
) -> Result<Outcome, Error> {
    publish_policy(
        request,
        trust,
        deadline,
        authority,
        value,
        bytes,
        Some(definitions),
    )
}
fn publish_policy(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    authority: &Authority,
    value: &Email,
    bytes: &[u8],
    definitions: Option<&[crate::import_snapshot::Mailbox]>,
) -> Result<Outcome, Error> {
    // Refuse more than the body's allowance before any work; only the write
    // connection receives more than the ordinary ten seconds.
    if deadline.saturating_duration_since(Instant::now())
        > postgres_probe::import_duration(value.length)
    {
        return Err(Error::Connection(postgres_probe::Error::Invalid));
    }
    if let Some(outcome) = inspect_policy(
        request,
        trust,
        deadline.min(Instant::now() + postgres_probe::MAX_DURATION),
        authority,
        &value.key,
        if definitions.is_some() {
            None
        } else {
            Some(&value.digest)
        },
    )? {
        return Ok(outcome);
    }
    if definitions.is_some_and(|d| d.len() > 256) {
        return Err(Error::Capacity);
    }
    let guarded = definitions.is_some();
    let definitions = definitions.map(<[_]>::to_vec);
    if bytes.len() as u64 != value.length {
        return Err(Error::Invalid);
    }
    let mut hash = Sha256::try_new().map_err(|_| Error::Crypto)?;
    hash.update(bytes).map_err(|_| Error::Crypto)?;
    if hash.finish().map_err(|_| Error::Crypto)? != value.body_digest {
        return Err(Error::Invalid);
    }
    let (anchors, own) = identifiers(bytes, deadline)?;
    let interpretation =
        crate::postgres_mime_evidence::prepare(bytes, &value.body_digest, deadline);
    let bytes = bytes.to_vec();
    let value = value.clone();
    let authority = authority.clone();
    let attempted = Arc::new(AtomicBool::new(false));
    let marker = attempted.clone();
    let result = postgres_probe::with_import_client(
        request,
        trust,
        deadline,
        value.length,
        guarded,
        move |client| {
            Box::pin(async move {
                let old = begin(client, &authority).await?;
                if let Some(outcome) = mapped_policy(
                    client,
                    &authority,
                    &value.key,
                    if guarded { None } else { Some(&value.digest) },
                    old,
                )
                .await?
                {
                    return Ok(outcome);
                }
                if let Some(definitions) = &definitions {
                    folders::check(
                        client,
                        &authority,
                        &value.key,
                        &value.mailboxes,
                        definitions,
                        None,
                        old,
                    )
                    .await?;
                }
                let sources: Vec<&[u8]> = value.mailboxes.iter().map(Vec::as_slice).collect();
                let rows=client.query("SELECT i.local_object,i.changed,m.changed FROM unnest($4::bytea[]) requested(id) LEFT JOIN td_mta.imports i ON i.account=$1 AND i.source_instance=$2 AND i.source_kind=1 AND i.source_account=$3 AND i.source_object=requested.id LEFT JOIN td_mta.mailboxes m ON m.account=i.account AND m.id=i.local_object", &[&authority.account.as_bytes().as_slice(),&value.key.instance.as_bytes().as_slice(),&value.key.account.as_slice(),&sources]).await.map_err(|_|Error::Database)?;
                if rows.len() != sources.len() {
                    return Err(Error::Integrity);
                }
                let mut mailboxes = Vec::new();
                for row in rows {
                    let id: Option<&[u8]> = row.try_get(0).map_err(|_| Error::Integrity)?;
                    let changed: Option<&[u8]> = row.try_get(2).map_err(|_| Error::Integrity)?;
                    if sequence(changed.ok_or(Error::MissingMailbox)?)? > old
                        || sequence(row.try_get(1).map_err(|_| Error::Integrity)?)? > old
                    {
                        return Err(Error::Integrity);
                    }
                    mailboxes.push(
                        <[u8; 16]>::try_from(id.ok_or(Error::MissingMailbox)?)
                            .map_err(|_| Error::Integrity)?,
                    );
                }
                mailboxes.sort();
                if mailboxes.windows(2).any(|p| p.first() == p.get(1)) {
                    return Err(Error::Conflict);
                }
                let id = fresh()?;
                let blob = BlobId::from_bytes(fresh()?);
                let new_id = ThreadId::from_bytes(fresh()?);
                unused(client, authority.account, 3, &id).await?;
                let (thread, new_thread) =
                    publication::select_thread(client, authority.account, &anchors, new_id, old)
                        .await
                        .map_err(publication_error)?;
                if new_thread {
                    unused(client, authority.account, 2, thread.as_bytes()).await?;
                }
                let next = next(client, &authority, old).await?;
                let usage = publication::charge(
                    client,
                    authority.account,
                    authority.quotas,
                    [value.length, 1, 0, 0, 0],
                )
                .await
                .map_err(publication_error)?;
                let created_at: i64 = client
                    .query_one(
                        "SELECT (extract(epoch FROM clock_timestamp())*1000)::bigint",
                        &[],
                    )
                    .await
                    .map_err(|_| Error::Database)?
                    .try_get(0)
                    .map_err(|_| Error::Integrity)?;
                publication::write_blob(
                    client,
                    publication::Blob {
                        account: authority.account,
                        id: blob,
                        bytes: &bytes,
                        digest: &value.body_digest,
                        interpretation: interpretation.as_ref(),
                        created_at,
                        changed: next,
                    },
                    deadline,
                )
                .await
                .map_err(publication_error)?;
                let account = authority.account.as_bytes().as_slice();
                let changed = next.to_be_bytes();
                let changed = changed.as_slice();
                let sql = if new_thread {
                    "INSERT INTO td_mta.threads VALUES($1,$2,$3)"
                } else {
                    "UPDATE td_mta.threads SET changed=$3 WHERE account=$1 AND id=$2"
                };
                if client
                    .execute(sql, &[&account, &thread.as_bytes().as_slice(), &changed])
                    .await
                    .map_err(|_| Error::Database)?
                    != 1
                {
                    return Err(Error::Integrity);
                }
                if client.execute("INSERT INTO td_mta.emails(account,id,blob_id,thread_id,received_at,origin,changed) VALUES($1,$2,$3,$4,$5,3,$6)",&[&account,&id.as_slice(),&blob.as_bytes().as_slice(),&thread.as_bytes().as_slice(),&value.received_at,&changed]).await.map_err(|_|Error::Database)?!=1{return Err(Error::Integrity);}
                history(client, authority.account, next, 0, 3, 1, &id).await?;
                history(
                    client,
                    authority.account,
                    next,
                    1,
                    2,
                    if new_thread { 1 } else { 2 },
                    thread.as_bytes(),
                )
                .await?;
                let mailbox_ids: Vec<&[u8]> = mailboxes.iter().map(|id| id.as_slice()).collect();
                if client.execute("INSERT INTO td_mta.memberships SELECT $1,$2,id,$4 FROM unnest($3::bytea[]) ids(id)",&[&account,&id.as_slice(),&mailbox_ids,&changed]).await.map_err(|_|Error::Database)?!=mailboxes.len() as u64{return Err(Error::Integrity);}
                if client.execute("UPDATE td_mta.mailboxes SET changed=$3 WHERE account=$1 AND id=ANY($2::bytea[])",&[&account,&mailbox_ids,&changed]).await.map_err(|_|Error::Database)?!=mailboxes.len() as u64{return Err(Error::Integrity);}
                if client.execute("INSERT INTO td_mta.changes SELECT $1,$2,ordinality+1,1,2,id FROM unnest($3::bytea[]) WITH ORDINALITY ids(id,ordinality)",&[&account,&changed,&mailbox_ids]).await.map_err(|_|Error::Database)?!=mailboxes.len() as u64{return Err(Error::Integrity);}
                let keywords: Vec<&str> = value.keywords.iter().map(String::as_str).collect();
                if client.execute("INSERT INTO td_mta.keywords SELECT $1,$2,keyword,$4 FROM unnest($3::text[]) keys(keyword)",&[&account,&id.as_slice(),&keywords,&changed]).await.map_err(|_|Error::Database)?!=keywords.len() as u64{return Err(Error::Integrity);}
                if !own.is_empty()
                    && client
                        .execute(
                            "INSERT INTO td_mta.thread_anchors VALUES($1,$2,$3,$4)",
                            &[&account, &own, &id.as_slice(), &changed],
                        )
                        .await
                        .map_err(|_| Error::Database)?
                        != 1
                {
                    return Err(Error::Integrity);
                }
                usage
                    .publish(client, authority.account)
                    .await
                    .map_err(publication_error)?;
                record(
                    client,
                    &authority,
                    &value.key,
                    &value.digest,
                    &id,
                    Some(blob.as_bytes().as_slice()),
                    next,
                )
                .await?;
                commit(client, &marker).await?;
                Ok(Outcome {
                    local_object: id,
                    created: true,
                    recorded: true,
                    present: true,
                })
            })
        },
    );
    settled(result, &attempted)
}
fn identifiers(bytes: &[u8], deadline: Instant) -> Result<(Vec<String>, String), Error> {
    use crate::{
        admission::work::{Charge, Meter},
        clock::RuntimeClock,
        ports::{Clock, Deadline},
        receiving_headers::{Headers, Work},
    };
    let clock = RuntimeClock::new();
    let mut last = clock.sample().map_err(|_| Error::Invalid)?.monotonic;
    let left = deadline
        .checked_duration_since(Instant::now())
        .ok_or(Error::Connection(postgres_probe::Error::Deadline))?;
    let cap = Deadline::after(
        last,
        u64::try_from(left.as_millis()).map_err(|_| Error::Capacity)?,
    )
    .map_err(|_| Error::Invalid)?;
    let mut meter = Meter::new(
        cap,
        Charge {
            io_bytes: 64 * 1024 * 1024,
            output_bytes: 1024 * 1024,
            records: 4 * 1024 * 1024,
            unlinks: 0,
        },
    );
    let mut work = Work {
        clock: &clock,
        deadline: cap,
        meter: &mut meter,
        last: &mut last,
    };
    let mut header = Headers::new(1024 * 1024).map_err(|_| Error::Capacity)?;
    for part in bytes.chunks(1000) {
        if header
            .push(part, false, &mut work)
            .map_err(|_| Error::Capacity)?
        {
            break;
        }
    }
    if !header.complete() {
        header
            .push(&[], true, &mut work)
            .map_err(|_| Error::Capacity)?;
    }
    header
        .thread_identifiers(&mut work)
        .map_err(|_| Error::Invalid)
}
