//! Email deletion with shared-body retention and transactional quota release.
use super::*;

#[derive(Clone)]
pub struct Deletion(Change);

pub fn prepare(
    binding: &Binding,
    device: DeviceId,
    emails: &[EmailId],
    expected: Option<u64>,
) -> Result<Deletion, Error> {
    if emails.len() > MAX_UPDATES {
        return Err(Error::Capacity);
    }
    let mut updates = Vec::new();
    updates
        .try_reserve_exact(emails.len())
        .map_err(|_| Error::Capacity)?;
    for email in emails {
        updates.push(Update {
            email: *email,
            mailbox: None,
            seen: None,
            flagged: None,
        });
    }
    prepare_batch_kind(binding, device, &updates, expected, true).map(Deletion)
}
impl Deletion {
    pub fn operation(&self) -> EmailOperationId {
        self.0.operation()
    }
}
pub fn inspect(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    deletion: &Deletion,
) -> Result<Option<BatchOutcome>, Error> {
    inspect_record(request, trust, deadline, &deletion.0)
}
pub fn publish(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    deletion: &Deletion,
    password: &[u8],
) -> Result<BatchOutcome, Error> {
    publish_record(request, trust, deadline, &deletion.0, password)
}

pub(super) async fn apply(
    client: &Client,
    change: &Change,
    old: u64,
) -> Result<(u64, Vec<UpdateResult>), Error> {
    let account = change.binding.account();
    let account = account.as_bytes().as_slice();
    let before = old.to_be_bytes();
    let mut requested = Vec::new();
    requested
        .try_reserve_exact(change.updates.len())
        .map_err(|_| Error::Capacity)?;
    for update in &change.updates {
        requested.push(update.email.as_bytes().as_slice());
    }
    let rows = client.query("SELECT q.ordinality,e.id,e.changed,e.blob_id,b.changed,e.thread_id,t.changed,k.n,k.valid,a.n,a.valid FROM unnest($2::bytea[]) WITH ORDINALITY q(id,ordinality) LEFT JOIN td_mta.emails e ON e.account=$1 AND e.id=q.id LEFT JOIN td_mta.blobs b ON b.account=e.account AND b.id=e.blob_id LEFT JOIN td_mta.threads t ON t.account=e.account AND t.id=e.thread_id CROSS JOIN LATERAL (SELECT count(*) n,coalesce(bool_and(changed<=$3),true) valid FROM (SELECT changed FROM td_mta.keywords WHERE account=e.account AND email_id=e.id LIMIT 257) v) k CROSS JOIN LATERAL (SELECT count(*) n,coalesce(bool_and(changed<=$3),true) valid FROM (SELECT changed FROM td_mta.thread_anchors WHERE account=e.account AND email_id=e.id LIMIT 2) v) a ORDER BY q.ordinality", &[&account,&requested,&before.as_slice()]).await.map_err(|_| Error::Database)?;
    if rows.len() != requested.len() {
        return Err(Error::Integrity);
    }
    let mut results = Vec::new();
    let mut deleted = Vec::new();
    let mut blobs = Vec::new();
    let mut threads = Vec::new();
    for values in [&mut deleted, &mut blobs, &mut threads] {
        values
            .try_reserve_exact(rows.len())
            .map_err(|_| Error::Capacity)?;
    }
    results
        .try_reserve_exact(rows.len())
        .map_err(|_| Error::Capacity)?;
    for (index, row) in rows.iter().enumerate() {
        let ordinal: i64 = row.try_get(0).map_err(|_| Error::Integrity)?;
        if usize::try_from(ordinal).ok() != Some(index + 1) {
            return Err(Error::Integrity);
        }
        let email: Option<&[u8]> = row.try_get(1).map_err(|_| Error::Integrity)?;
        let Some(email) = email else {
            results.push(UpdateResult::NotFound);
            continue;
        };
        for column in [2, 4, 6] {
            if sequence(row.try_get(column).map_err(|_| Error::Integrity)?)? > old {
                return Err(Error::Integrity);
            }
        }
        let keyword_count: i64 = row.try_get(7).map_err(|_| Error::Integrity)?;
        let keywords_valid: bool = row.try_get(8).map_err(|_| Error::Integrity)?;
        let anchors_count: i64 = row.try_get(9).map_err(|_| Error::Integrity)?;
        let anchors_valid: bool = row.try_get(10).map_err(|_| Error::Integrity)?;
        if !keywords_valid
            || !anchors_valid
            || !(0..=1).contains(&anchors_count)
            || keyword_count < 0
        {
            return Err(Error::Integrity);
        }
        if keyword_count > 256 {
            return Err(Error::Capacity);
        }
        let blob: &[u8] = row.try_get(3).map_err(|_| Error::Integrity)?;
        let thread: &[u8] = row.try_get(5).map_err(|_| Error::Integrity)?;
        if [email, blob, thread].iter().any(|id| id.len() != 16) {
            return Err(Error::Integrity);
        }
        deleted.push(email);
        if !blobs.contains(&blob) {
            blobs.push(blob);
        }
        if !threads.contains(&thread) {
            threads.push(thread);
        }
        results.push(UpdateResult::Destroyed);
    }
    if deleted.is_empty() {
        return Ok((old, results));
    }
    let next = crate::postgres_sequence::next(client, account, old)
        .await
        .map_err(|error| match error {
            crate::postgres_sequence::Error::Capacity => Error::Capacity,
            crate::postgres_sequence::Error::Integrity => Error::Integrity,
            crate::postgres_sequence::Error::Database => Error::Database,
        })?;
    let changed = next.to_be_bytes();
    let mailbox_rows = client.query("SELECT m.mailbox_id,b.changed,bool_and(m.changed<=$3) FROM td_mta.memberships m LEFT JOIN td_mta.mailboxes b ON b.account=m.account AND b.id=m.mailbox_id WHERE m.account=$1 AND m.email_id=ANY($2::bytea[]) GROUP BY m.mailbox_id,b.changed ORDER BY m.mailbox_id LIMIT 257", &[&account,&deleted,&before.as_slice()]).await.map_err(|_| Error::Database)?;
    if mailbox_rows.len() > crate::postgres_mailboxes::MAX_MAILBOXES {
        return Err(Error::Capacity);
    }
    let mut mailboxes = Vec::new();
    mailboxes
        .try_reserve_exact(mailbox_rows.len())
        .map_err(|_| Error::Capacity)?;
    for row in &mailbox_rows {
        let valid: bool = row.try_get(2).map_err(|_| Error::Integrity)?;
        if !valid || sequence(row.try_get(1).map_err(|_| Error::Integrity)?)? > old {
            return Err(Error::Integrity);
        }
        let id: &[u8] = row.try_get(0).map_err(|_| Error::Integrity)?;
        if id.len() != 16 {
            return Err(Error::Integrity);
        }
        mailboxes.push(id);
    }
    let thread_rows = client.query("SELECT q.id,t.changed,v.survives,v.valid FROM unnest($2::bytea[]) WITH ORDINALITY q(id,ordinality) LEFT JOIN td_mta.threads t ON t.account=$1 AND t.id=q.id CROSS JOIN LATERAL (SELECT coalesce(bool_or(NOT(id=ANY($3::bytea[]))),false) survives,coalesce(bool_and(changed<=$4),true) valid FROM (SELECT id,changed FROM td_mta.emails WHERE account=$1 AND thread_id=q.id LIMIT 257) owners) v ORDER BY q.ordinality", &[&account,&threads,&deleted,&before.as_slice()]).await.map_err(|_| Error::Database)?;
    if thread_rows.len() != threads.len() {
        return Err(Error::Integrity);
    }
    let mut survives = Vec::new();
    survives
        .try_reserve_exact(threads.len())
        .map_err(|_| Error::Capacity)?;
    for (row, id) in thread_rows.iter().zip(&threads) {
        let found: &[u8] = row.try_get(0).map_err(|_| Error::Integrity)?;
        let valid: bool = row.try_get(3).map_err(|_| Error::Integrity)?;
        if found != *id || !valid || sequence(row.try_get(1).map_err(|_| Error::Integrity)?)? > old
        {
            return Err(Error::Integrity);
        }
        survives.push(row.try_get::<_, bool>(2).map_err(|_| Error::Integrity)?);
    }
    let valid: bool = client.query_one("SELECT NOT EXISTS(SELECT 1 FROM td_mta.emails WHERE account=$1 AND blob_id=ANY($2::bytea[]) AND changed>$3)", &[&account,&blobs,&before.as_slice()]).await.map_err(|_| Error::Database)?.try_get(0).map_err(|_| Error::Integrity)?;
    if !valid {
        return Err(Error::Integrity);
    }
    // One round trip keeps the maximum batch inside the connection frame cap.
    let written = client.execute("WITH removed_memberships AS (DELETE FROM td_mta.memberships WHERE account=$1 AND email_id=ANY($2::bytea[])), removed_keywords AS (DELETE FROM td_mta.keywords WHERE account=$1 AND email_id=ANY($2::bytea[])), removed_anchors AS (DELETE FROM td_mta.thread_anchors WHERE account=$1 AND email_id=ANY($2::bytea[])) DELETE FROM td_mta.emails WHERE account=$1 AND id=ANY($2::bytea[])", &[&account, &deleted]).await.map_err(|_| Error::Database)?;
    if written != deleted.len() as u64 {
        return Err(Error::Integrity);
    }
    let released = client.query_one("WITH retired AS (DELETE FROM td_mta.blobs b WHERE b.account=$1 AND b.id=ANY($2::bytea[]) AND NOT EXISTS(SELECT 1 FROM td_mta.emails e WHERE e.account=b.account AND e.blob_id=b.id) AND NOT EXISTS(SELECT 1 FROM td_mta.leases l WHERE l.account=b.account AND l.blob_id=b.id) AND NOT EXISTS(SELECT 1 FROM td_mta.submissions s WHERE s.account=b.account AND s.transmitted_blob_id=b.id) RETURNING length) SELECT coalesce(sum(length),0)::bigint,count(*) FROM retired", &[&account,&blobs]).await.map_err(|_| Error::Database)?;
    let bytes: i64 = released.try_get(0).map_err(|_| Error::Integrity)?;
    let count: i64 = released.try_get(1).map_err(|_| Error::Integrity)?;
    if bytes < 0 || count < 0 || count > blobs.len() as i64 {
        return Err(Error::Integrity);
    }
    if client.execute("UPDATE td_mta.account_usage SET body_bytes=body_bytes-$2,blob_count=blob_count-$3 WHERE account=$1 AND body_bytes>=$2 AND blob_count>=$3 AND body_bytes-$2>=upload_bytes AND body_bytes-$2>=queue_bytes", &[&account,&bytes,&count]).await.map_err(|_| Error::Database)? != 1 { return Err(Error::Integrity); }
    let kept = survives.iter().filter(|value| **value).count() as u64;
    if client.execute("UPDATE td_mta.threads t SET changed=$4 FROM unnest($2::bytea[],$3::bool[]) q(id,survives) WHERE t.account=$1 AND t.id=q.id AND q.survives", &[&account,&threads,&survives,&changed.as_slice()]).await.map_err(|_| Error::Database)? != kept { return Err(Error::Integrity); }
    if client.execute("DELETE FROM td_mta.threads t USING unnest($2::bytea[],$3::bool[]) q(id,survives) WHERE t.account=$1 AND t.id=q.id AND NOT q.survives", &[&account,&threads,&survives]).await.map_err(|_| Error::Database)? != threads.len() as u64-kept { return Err(Error::Integrity); }
    let written = client.execute("INSERT INTO td_mta.changes(account,sequence,operation,kind,action,object) SELECT $1,$3,ordinality-1,3,3,id FROM unnest($2::bytea[]) WITH ORDINALITY q(id,ordinality)", &[&account,&deleted,&changed.as_slice()]).await.map_err(|_| Error::Database)?;
    if written != deleted.len() as u64 {
        return Err(Error::Integrity);
    }
    let offset = i64::try_from(deleted.len()).map_err(|_| Error::Capacity)?;
    let written = client.execute("WITH changed AS (UPDATE td_mta.mailboxes SET changed=$3 WHERE account=$1 AND id=ANY($2::bytea[]) RETURNING id) INSERT INTO td_mta.changes(account,sequence,operation,kind,action,object) SELECT $1,$3,row_number() OVER (ORDER BY id)+$4-1,1,2,id FROM changed", &[&account,&mailboxes,&changed.as_slice(),&offset]).await.map_err(|_| Error::Database)?;
    if written != mailboxes.len() as u64 {
        return Err(Error::Integrity);
    }
    let offset = i64::try_from(deleted.len() + mailboxes.len()).map_err(|_| Error::Capacity)?;
    let written = client.execute("INSERT INTO td_mta.changes(account,sequence,operation,kind,action,object) SELECT $1,$4,ordinality+$5-1,2,CASE WHEN survives THEN 2 ELSE 3 END,id FROM unnest($2::bytea[],$3::bool[]) WITH ORDINALITY q(id,survives,ordinality)", &[&account,&threads,&survives,&changed.as_slice(),&offset]).await.map_err(|_| Error::Database)?;
    if written != threads.len() as u64 {
        return Err(Error::Integrity);
    }
    if client
        .execute(
            "UPDATE td_mta.accounts SET sequence=$2 WHERE id=$1",
            &[&account, &changed.as_slice()],
        )
        .await
        .map_err(|_| Error::Database)?
        != 1
    {
        return Err(Error::Integrity);
    }
    Ok((next, results))
}
