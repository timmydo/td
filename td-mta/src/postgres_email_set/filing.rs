//! Separate, retained success filing; never changes a published submission.
use super::*;

#[derive(Clone, Copy)]
pub(super) struct Patch {
    movement: Option<(MailboxId, MailboxId)>,
}

/// Retain this exact operation through an indeterminate filing commit.
#[derive(Clone)]
pub struct Filing(Change);

/// Clear `$draft`, optionally removing only source and adding destination.
/// A missing source membership refuses the whole patch, preserving concurrent
/// moves. Other memberships and keywords remain unchanged.
pub fn prepare(
    binding: &Binding,
    device: DeviceId,
    email: EmailId,
    movement: Option<(MailboxId, MailboxId)>,
) -> Result<Filing, Error> {
    if movement.is_some_and(|(source, destination)| source == destination) {
        return Err(Error::Invalid);
    }
    let mut change = prepare_batch(
        binding,
        device,
        &[Update {
            email,
            mailbox: movement.map(|(_, destination)| destination),
            seen: None,
            flagged: None,
        }],
        None,
    )?;
    let mut hash = Provider.sha256().map_err(|_| Error::Crypto)?;
    hash.update(b"td-mta PostgreSQL success filing v1\0")
        .map_err(|_| Error::Crypto)?;
    hash.update(&change.digest).map_err(|_| Error::Crypto)?;
    hash.update(&[u8::from(movement.is_some())])
        .map_err(|_| Error::Crypto)?;
    if let Some((source, _)) = movement {
        hash.update(source.as_bytes()).map_err(|_| Error::Crypto)?;
    }
    change.digest = hash.finish().map_err(|_| Error::Crypto)?;
    change.filing = Some(Patch { movement });
    Ok(Filing(change))
}

impl Filing {
    pub fn operation(&self) -> EmailOperationId {
        self.0.operation()
    }
}

pub fn inspect(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    filing: &Filing,
) -> Result<Option<BatchOutcome>, Error> {
    inspect_record(request, trust, deadline, &filing.0)
}

pub fn publish(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    filing: &Filing,
    password: &[u8],
) -> Result<BatchOutcome, Error> {
    publish_record(request, trust, deadline, &filing.0, password)
}

pub(super) async fn apply(
    client: &Client,
    change: &Change,
    patch: Patch,
    old: u64,
) -> Result<(u64, Vec<UpdateResult>), Error> {
    let account = change.binding.account();
    let account = account.as_bytes().as_slice();
    let update = change.updates.first().ok_or(Error::Integrity)?;
    if change.updates.len() != 1 {
        return Err(Error::Integrity);
    }
    let email = update.email.as_bytes().as_slice();
    let mut results = Vec::new();
    results.try_reserve_exact(1).map_err(|_| Error::Capacity)?;
    let row = client.query_opt("SELECT e.changed,k.changed,s.changed FROM td_mta.emails e LEFT JOIN td_mta.keywords k ON k.account=e.account AND k.email_id=e.id AND k.keyword='$draft' LEFT JOIN td_mta.keywords s ON s.account=e.account AND s.email_id=e.id AND s.keyword='$seen' WHERE e.account=$1 AND e.id=$2", &[&account,&email]).await.map_err(|_| Error::Database)?;
    let Some(row) = row else {
        results.push(UpdateResult::NotFound);
        return Ok((old, results));
    };
    let draft: Option<&[u8]> = row.try_get(1).map_err(|_| Error::Integrity)?;
    let seen: Option<&[u8]> = row.try_get(2).map_err(|_| Error::Integrity)?;
    for value in [
        Some(row.try_get(0).map_err(|_| Error::Integrity)?),
        draft,
        seen,
    ]
    .into_iter()
    .flatten()
    {
        if sequence(value)? > old {
            return Err(Error::Integrity);
        }
    }
    let mut mailboxes = Vec::new();
    mailboxes
        .try_reserve_exact(crate::postgres_mailboxes::MAX_MAILBOXES)
        .map_err(|_| Error::Capacity)?;
    let unread_changes = draft.is_some() && seen.is_none();
    let mut insert_destination = false;
    let mut source_present = false;
    let mut destination_present = false;
    if patch.movement.is_some() || unread_changes {
        let rows = client.query("SELECT m.mailbox_id,m.changed,b.changed FROM td_mta.memberships m JOIN td_mta.mailboxes b ON b.account=m.account AND b.id=m.mailbox_id WHERE m.account=$1 AND m.email_id=$2 ORDER BY m.mailbox_id LIMIT 257", &[&account,&email]).await.map_err(|_| Error::Database)?;
        if rows.len() > crate::postgres_mailboxes::MAX_MAILBOXES {
            return Err(Error::Capacity);
        }
        for row in &rows {
            let id: &[u8] = row.try_get(0).map_err(|_| Error::Integrity)?;
            if id.len() != 16
                || sequence(row.try_get(1).map_err(|_| Error::Integrity)?)? > old
                || sequence(row.try_get(2).map_err(|_| Error::Integrity)?)? > old
            {
                return Err(Error::Integrity);
            }
            let mailbox = MailboxId::from_bytes(id.try_into().map_err(|_| Error::Integrity)?);
            if unread_changes {
                mailboxes.push(mailbox);
            }
            if let Some((source, destination)) = patch.movement {
                source_present |= mailbox == source;
                destination_present |= mailbox == destination;
            }
        }
    }
    if let Some((source, destination)) = patch.movement {
        let target = client
            .query_opt(
                "SELECT changed FROM td_mta.mailboxes WHERE account=$1 AND id=$2",
                &[&account, &destination.as_bytes().as_slice()],
            )
            .await
            .map_err(|_| Error::Database)?;
        let Some(target) = target.filter(|_| source_present) else {
            results.push(UpdateResult::InvalidMailbox);
            return Ok((old, results));
        };
        if sequence(target.try_get(0).map_err(|_| Error::Integrity)?)? > old {
            return Err(Error::Integrity);
        }
        if !unread_changes {
            mailboxes.push(source);
        }
        insert_destination = !destination_present;
        if insert_destination {
            if mailboxes.len() == crate::postgres_mailboxes::MAX_MAILBOXES {
                return Err(Error::Capacity);
            }
            mailboxes.push(destination);
        }
    }
    results.push(UpdateResult::Updated);
    if draft.is_none() && mailboxes.is_empty() {
        return Ok((old, results));
    }
    let next = old.checked_add(1).ok_or(Error::Capacity)?;
    let changed = next.to_be_bytes();
    if let Some((source, destination)) = patch.movement {
        if client
            .execute(
                "DELETE FROM td_mta.memberships WHERE account=$1 AND email_id=$2 AND mailbox_id=$3",
                &[&account, &email, &source.as_bytes().as_slice()],
            )
            .await
            .map_err(|_| Error::Database)?
            != 1
        {
            return Err(Error::Integrity);
        }
        if insert_destination && client.execute("INSERT INTO td_mta.memberships(account,email_id,mailbox_id,changed) VALUES($1,$2,$3,$4)", &[&account,&email,&destination.as_bytes().as_slice(),&changed.as_slice()]).await.map_err(|_| Error::Database)? != 1 {
            return Err(Error::Integrity);
        }
    }
    if draft.is_some()
        && client
            .execute(
                "DELETE FROM td_mta.keywords WHERE account=$1 AND email_id=$2 AND keyword='$draft'",
                &[&account, &email],
            )
            .await
            .map_err(|_| Error::Database)?
            != 1
    {
        return Err(Error::Integrity);
    }
    if client
        .execute(
            "UPDATE td_mta.emails SET changed=$3 WHERE account=$1 AND id=$2",
            &[&account, &email, &changed.as_slice()],
        )
        .await
        .map_err(|_| Error::Database)?
        != 1
    {
        return Err(Error::Integrity);
    }
    client.execute("INSERT INTO td_mta.changes(account,sequence,operation,kind,action,object) VALUES($1,$2,0,3,2,$3)", &[&account,&changed.as_slice(),&email]).await.map_err(|_| Error::Database)?;
    if !mailboxes.is_empty() {
        let mut ids = Vec::new();
        ids.try_reserve_exact(mailboxes.len())
            .map_err(|_| Error::Capacity)?;
        for mailbox in &mailboxes {
            ids.push(mailbox.as_bytes().as_slice());
        }
        if client.execute("WITH changed AS (UPDATE td_mta.mailboxes SET changed=$3 WHERE account=$1 AND id=ANY($2::bytea[]) RETURNING id) INSERT INTO td_mta.changes(account,sequence,operation,kind,action,object) SELECT $1,$3,row_number() OVER (ORDER BY id),1,2,id FROM changed", &[&account,&ids,&changed.as_slice()]).await.map_err(|_| Error::Database)? != mailboxes.len() as u64 {
            return Err(Error::Integrity);
        }
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
