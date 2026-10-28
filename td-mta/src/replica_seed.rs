//! Immutable account seed prepared before owner-side database publication.
use crate::{
    config::endpoint::Origin,
    ids::{AccountId, MailboxId},
    postgres_replica_source::Source,
    replica_pending::{Pending, Ticket},
    replica_source::{Inventory, MailboxPage},
    sync::{DataState, DataType},
};
use td_crypto::{Digest, Sha256};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Invalid,
    Source,
    Capacity,
    Crypto,
    MissingInbox,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Limits {
    pub body_bytes: u64,
    pub blobs: u64,
    pub emails: u64,
    pub receipts: u64,
}
impl Limits {
    pub fn validate(self) -> Result<(), Error> {
        for value in [self.body_bytes, self.blobs, self.emails, self.receipts] {
            if value == 0 || value > i64::MAX as u64 {
                return Err(Error::Invalid);
            }
        }
        Ok(())
    }
}

/// Owns the complete graph and initial inventory. A matching seed can settle
/// an uncertain bootstrap; it cannot reset a copy that already made progress.
pub struct Seed {
    pub(crate) source: Source,
    pub(crate) account: AccountId,
    pub(crate) inbox: MailboxId,
    pub(crate) mailboxes: MailboxPage,
    pub(crate) base: DataState,
    pub(crate) pending: Pending,
    pub(crate) limits: Limits,
    pub(crate) digest: [u8; 32],
}
impl Seed {
    pub fn prepare(
        source: Source,
        inventory: Inventory,
        mailboxes: MailboxPage,
        limits: Limits,
    ) -> Result<Self, Error> {
        limits.validate()?;
        if inventory.previous.is_some()
            || inventory.base != inventory.observed
            || inventory.base.epoch != source.epoch()
        {
            return Err(Error::Source);
        }
        let origin = Origin::parse(source.origin()).map_err(|_| Error::Invalid)?;
        if !origin
            .same_origin(Origin::parse(mailboxes.source_origin()).map_err(|_| Error::Invalid)?)
        {
            return Err(Error::Source);
        }
        let graph = mailboxes.view().map_err(|_| Error::Invalid)?;
        let observed = graph.observed;
        if observed.kind != DataType::Mailbox
            || observed.account != inventory.base.account
            || observed.epoch != source.epoch()
            || observed.sequence < inventory.base.sequence
        {
            return Err(Error::Source);
        }
        if graph.list.len() as u64 > limits.receipts {
            return Err(Error::Capacity);
        }
        let inbox = graph
            .list
            .iter()
            .find(|mailbox| mailbox.row.role == Some("inbox"))
            .map(|mailbox| mailbox.id)
            .ok_or(Error::MissingInbox)?;
        let pending = Pending::inventory(
            Ticket {
                generation: 1,
                page: 1,
            },
            &inventory,
        )
        .map_err(|_| Error::Invalid)?;
        let mut hash = Sha256::try_new().map_err(|_| Error::Crypto)?;
        field(&mut hash, b"td-mta-replica-seed-v1")?;
        field(&mut hash, source.origin().as_bytes())?;
        field(&mut hash, source.epoch().as_bytes())?;
        field(&mut hash, inventory.base.account.as_bytes())?;
        field(&mut hash, &inventory.base.sequence.number().to_be_bytes())?;
        field(&mut hash, &observed.sequence.number().to_be_bytes())?;
        for value in [
            limits.body_bytes,
            limits.blobs,
            limits.emails,
            limits.receipts,
        ] {
            field(&mut hash, &value.to_be_bytes())?;
        }
        field(&mut hash, &(inventory.ids.len() as u64).to_be_bytes())?;
        for id in &inventory.ids {
            field(&mut hash, id.as_bytes())?;
        }
        match inventory.next {
            Some(next) => {
                field(&mut hash, &[1])?;
                field(&mut hash, next.after.as_bytes())?;
                field(&mut hash, next.upper.as_bytes())?;
            }
            None => field(&mut hash, &[0])?,
        }
        field(&mut hash, &(graph.list.len() as u64).to_be_bytes())?;
        for mailbox in &graph.list {
            field(&mut hash, mailbox.id.as_bytes())?;
            field(&mut hash, mailbox.row.name.as_bytes())?;
            match mailbox.row.parent {
                Some(parent) => {
                    field(&mut hash, &[1])?;
                    field(&mut hash, parent.as_bytes())?;
                }
                None => field(&mut hash, &[0])?,
            }
            match mailbox.row.role {
                Some(role) => {
                    field(&mut hash, &[1])?;
                    field(&mut hash, role.as_bytes())?;
                }
                None => field(&mut hash, &[0])?,
            }
            field(&mut hash, &mailbox.row.sort_order.to_be_bytes())?;
            field(&mut hash, &[u8::from(mailbox.row.subscribed)])?;
        }
        let digest = hash.finish().map_err(|_| Error::Crypto)?;
        drop(graph);
        Ok(Self {
            source,
            account: inventory.base.account,
            inbox,
            mailboxes,
            base: inventory.base,
            pending,
            limits,
            digest,
        })
    }
    pub fn account(&self) -> AccountId {
        self.account
    }
    pub fn digest(&self) -> [u8; 32] {
        self.digest
    }
}
fn field(hash: &mut Sha256, bytes: &[u8]) -> Result<(), Error> {
    hash.update(&(bytes.len() as u64).to_be_bytes())
        .map_err(|_| Error::Crypto)?;
    hash.update(bytes).map_err(|_| Error::Crypto)
}
