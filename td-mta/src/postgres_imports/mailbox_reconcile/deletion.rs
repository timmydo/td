//! Owner-selected removal of unchanged, empty, roleless imported leaves.
use super::*;
#[derive(Clone)]
pub struct Baseline {
    pub epoch: StoreEpoch,
    pub account: AccountId,
    pub key: Key,
    pub local: [u8; 16],
    pub digest: [u8; 32],
    pub changed: u64,
}
pub enum Plan {
    Unmapped,
    Absent,
    Conflict,
    Role,
    HasChild,
    HasEmail,
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
    previous: &Mailbox,
) -> Result<Plan, Error> {
    if request.mode != Mode::Preparation {
        return Err(Error::Owner);
    }
    let authority = authority.clone();
    let previous = previous.clone();
    postgres_probe::with_reconcile_client(request,trust,deadline,move |client| Box::pin(async move {
        let old=begin(client,&authority).await?;
        let Some(receipt)=receipt(client,&authority,&previous.key,old).await? else {return Ok(Plan::Unmapped)};
        let current=match local(client,&authority,&previous.key,&receipt,old).await {
            Ok(Some(v))=>v, Ok(None)=>return Ok(Plan::Absent), Err(Error::Conflict)=>return Ok(Plan::Conflict),Err(e)=>return Err(e),
        };
        if current.digest!=receipt.digest || previous.digest!=receipt.digest {return Ok(Plan::Conflict);}
        if current.role.is_some() {return Ok(Plan::Role);}
        let row=client.query_one("SELECT EXISTS(SELECT 1 FROM td_mta.mailboxes WHERE account=$1 AND parent_id=$2),EXISTS(SELECT 1 FROM td_mta.memberships WHERE account=$1 AND mailbox_id=$2)",&[&authority.account.as_bytes().as_slice(),&receipt.local.as_slice()]).await.map_err(|_|Error::Database)?;
        if row.try_get::<_,bool>(0).map_err(|_|Error::Integrity)? {return Ok(Plan::HasChild);}
        if row.try_get::<_,bool>(1).map_err(|_|Error::Integrity)? {return Ok(Plan::HasEmail);}
        Ok(Plan::Delete(Baseline{epoch:authority.epoch,account:authority.account,key:previous.key.clone(),local:receipt.local,digest:receipt.digest,changed:receipt.changed}))
    })).map_err(Error::Connection)?
}
pub fn apply(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    authority: &Authority,
    previous: &Mailbox,
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
                || receipt.digest != expected.digest
                || receipt.changed != expected.changed
            {
                return Err(Error::Conflict);
            }
            let Some(current) = local(client, &authority, &previous.key, &receipt, old).await?
            else {
                return Ok(Applied::Absent);
            };
            if current.digest != receipt.digest || current.role.is_some() {
                return Err(Error::Conflict);
            }
            let id = MailboxId::from_bytes(receipt.local);
            let (_, results) = crate::postgres_mailbox_set::apply_delete(
                client,
                authority.account.as_bytes().as_slice(),
                Some(id),
                old,
            )
            .await
            .map_err(|error| match error {
                crate::postgres_mailbox_set::Error::Capacity => Error::Capacity,
                crate::postgres_mailbox_set::Error::Integrity => Error::Integrity,
                _ => Error::Database,
            })?;
            if results.as_slice() != [crate::postgres_mailbox_set::ResultEntry::Destroyed(id)] {
                return Err(Error::Conflict);
            }
            commit(client, &marker).await?;
            Ok(Applied::Deleted)
        })
    });
    settled(result, &attempted)
}
