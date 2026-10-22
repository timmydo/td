use super::*;
use crate::import_snapshot::Mailbox;
use std::sync::{atomic::AtomicBool, Arc};

pub fn publish(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    authority: &Authority,
    value: &Mailbox,
) -> Result<Outcome, Error> {
    if let Some(outcome) = inspect(
        request,
        trust,
        deadline,
        authority,
        &value.key,
        &value.digest,
    )? {
        return Ok(outcome);
    }
    validate_name(&value.name, deadline)?;
    let value = value.clone();
    let authority = authority.clone();
    let attempted = Arc::new(AtomicBool::new(false));
    let marker = attempted.clone();
    let result = postgres_probe::with_mutation_client(request, trust, deadline, move |client| {
        Box::pin(async move {
            let old = begin(client, &authority).await?;
            if let Some(outcome) =
                mapped(client, &authority, &value.key, &value.digest, old).await?
            {
                return Ok(outcome);
            }
            let parent = match &value.parent {
                Some(source) => Some(parent(client, &authority, &value.key, source, old).await?),
                None => None,
            };
            let account = authority.account.as_bytes().as_slice();
            let rows=client.query("SELECT id,name,parent_id,role,sort_order,subscribed,changed FROM td_mta.mailboxes WHERE account=$1 ORDER BY id LIMIT 257",&[&account]).await.map_err(|_|Error::Database)?;
            if rows.len() > crate::postgres_mailboxes::MAX_MAILBOXES {
                return Err(Error::Capacity);
            }
            let mut adopt = None;
            for row in &rows {
                let id: &[u8] = row.try_get(0).map_err(|_| Error::Integrity)?;
                let name: &str = row.try_get(1).map_err(|_| Error::Integrity)?;
                let existing_parent: Option<&[u8]> =
                    row.try_get(2).map_err(|_| Error::Integrity)?;
                let role: Option<&str> = row.try_get(3).map_err(|_| Error::Integrity)?;
                let order: i64 = row.try_get(4).map_err(|_| Error::Integrity)?;
                let subscribed: i64 = row.try_get(5).map_err(|_| Error::Integrity)?;
                if sequence(row.try_get(6).map_err(|_| Error::Integrity)?)? > old
                    || id.len() != 16
                    || !matches!(subscribed, 0 | 1)
                {
                    return Err(Error::Integrity);
                }
                let same_parent = existing_parent == parent.as_ref().map(|p| p.as_slice());
                let role_conflict = role.is_some() && role == value.role.as_deref();
                let name_conflict = same_parent && name == value.name;
                if role_conflict || name_conflict {
                    if role == Some("inbox")
                        && value.role.as_deref() == Some("inbox")
                        && same_parent
                        && name == value.name
                        && order == i64::from(value.sort_order)
                        && subscribed == i64::from(value.subscribed)
                        && adopt.is_none()
                    {
                        adopt = Some(<[u8; 16]>::try_from(id).map_err(|_| Error::Integrity)?);
                    } else {
                        return Err(Error::Conflict);
                    }
                }
            }
            if adopt.is_none() && rows.len() >= crate::postgres_mailboxes::MAX_MAILBOXES {
                return Err(Error::Capacity);
            }
            let id = match adopt {
                Some(id) => id,
                None => fresh()?,
            };
            if adopt.is_none() {
                unused(client, authority.account, 1, &id).await?;
            }
            // One existing local folder cannot impersonate two source folders.
            let claimed:bool=client.query_one("SELECT EXISTS(SELECT 1 FROM td_mta.imports WHERE account=$1 AND source_instance=$2 AND source_kind=1 AND source_account=$3 AND local_object=$4)",&[&account,&value.key.instance.as_bytes().as_slice(),&value.key.account.as_slice(),&id.as_slice()]).await.map_err(|_|Error::Database)?.try_get(0).map_err(|_|Error::Integrity)?;
            if claimed {
                return Err(Error::Conflict);
            }
            let next = next(client, &authority, old).await?;
            let changed = next.to_be_bytes();
            let count=if adopt.is_some(){client.execute("UPDATE td_mta.mailboxes SET changed=$3 WHERE account=$1 AND id=$2",&[&account,&id.as_slice(),&changed.as_slice()]).await}else{
            client.execute("INSERT INTO td_mta.mailboxes(account,id,name,parent_id,role,sort_order,subscribed,changed) VALUES($1,$2,$3,$4,$5,$6,$7,$8)",&[&account,&id.as_slice(),&value.name,&parent.as_ref().map(|p|p.as_slice()),&value.role,&i64::from(value.sort_order),&i64::from(value.subscribed),&changed.as_slice()]).await
        }.map_err(|_|Error::Database)?;
            if count != 1 {
                return Err(Error::Integrity);
            }
            history(
                client,
                authority.account,
                next,
                0,
                1,
                if adopt.is_some() { 2 } else { 1 },
                &id,
            )
            .await?;
            record(
                client,
                &authority,
                &value.key,
                &value.digest,
                &id,
                None,
                next,
            )
            .await?;
            commit(client, &marker).await?;
            Ok(Outcome {
                local_object: id,
                created: adopt.is_none(),
                present: true,
            })
        })
    });
    settled(result, &attempted)
}
fn validate_name(name: &str, deadline: Instant) -> Result<(), Error> {
    use crate::{
        admission::work::{Charge, Meter},
        clock::RuntimeClock,
        ports::{Clock, Deadline},
    };
    let clock = RuntimeClock::new();
    let left = deadline
        .checked_duration_since(Instant::now())
        .ok_or(Error::Connection(postgres_probe::Error::Deadline))?;
    let cap = Deadline::after(
        clock.sample().map_err(|_| Error::Invalid)?.monotonic,
        u64::try_from(left.as_millis()).map_err(|_| Error::Capacity)?,
    )
    .map_err(|_| Error::Invalid)?;
    let mut meter = Meter::new(
        cap,
        Charge {
            io_bytes: 1024 * 1024,
            output_bytes: 1024 * 1024,
            records: 65536,
            unlinks: 0,
        },
    );
    let mut budget = td_mime::nfc::HeaderBudget::new();
    let mut scratch = td_mime::nfc::Scratch::new();
    if !crate::mailbox_name::valid(name, &mut meter, &mut budget, &mut scratch, || {
        Ok(clock.sample()?.monotonic)
    })
    .map_err(|_| Error::Invalid)?
    {
        return Err(Error::Invalid);
    }
    Ok(())
}
