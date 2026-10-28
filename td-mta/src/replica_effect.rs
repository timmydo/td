//! Immutable source observations prepared before a local publication transaction.
use crate::{
    config::endpoint::Origin,
    ids::{AccountId, BlobId, EmailId, MailboxId, ThreadId},
    postgres_mime_evidence,
    postgres_replica_source::Source,
    replica_pending::Ticket,
    replica_source::{MetadataPage, VerifiedBody},
    sync::{DataState, DataType},
};
use std::time::Instant;
use td_crypto::{Digest, Sha256};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Source,
    Invalid,
    Capacity,
    Integrity,
    Deadline,
    Crypto,
}

pub struct Effect {
    pub(crate) source: Source,
    pub(crate) account: AccountId,
    pub(crate) id: EmailId,
    pub(crate) ticket: Ticket,
    pub(crate) observed: DataState,
    pub(crate) digest: [u8; 32],
    pub(crate) mail: Option<Mail>,
}
pub(crate) struct Mail {
    pub(crate) blob: BlobId,
    pub(crate) thread: ThreadId,
    pub(crate) received_at: i64,
    pub(crate) mailboxes: Vec<MailboxId>,
    pub(crate) keywords: Vec<String>,
    pub(crate) body: Option<VerifiedBody>,
    pub(crate) length: u64,
    pub(crate) digest: [u8; 32],
    pub(crate) interpretation: Option<postgres_mime_evidence::Prepared>,
}
enum Intake {
    Verified(VerifiedBody),
    Absent,
    Existing,
}
impl Effect {
    /// The metadata page binds the source observation; verified bytes still
    /// grant no local authority. Publication rechecks the current locked ticket.
    pub fn prepare(
        source: &Source,
        account: AccountId,
        ticket: Ticket,
        page: &MetadataPage,
        id: EmailId,
        body: Option<VerifiedBody>,
        deadline: Instant,
    ) -> Result<Self, Error> {
        Self::prepare_inner(
            source,
            account,
            ticket,
            page,
            id,
            body.map_or(Intake::Absent, Intake::Verified),
            deadline,
        )
    }
    /// Reuse requires a matching, previously verified local Blob at publication.
    /// Missing local bytes request a verified download; no placeholder is written.
    pub fn prepare_existing(
        source: &Source,
        account: AccountId,
        ticket: Ticket,
        page: &MetadataPage,
        id: EmailId,
        deadline: Instant,
    ) -> Result<Self, Error> {
        Self::prepare_inner(
            source,
            account,
            ticket,
            page,
            id,
            Intake::Existing,
            deadline,
        )
    }
    fn prepare_inner(
        source: &Source,
        account: AccountId,
        ticket: Ticket,
        page: &MetadataPage,
        id: EmailId,
        intake: Intake,
        deadline: Instant,
    ) -> Result<Self, Error> {
        if Instant::now() >= deadline {
            return Err(Error::Deadline);
        }
        if ticket.generation == 0 || ticket.page == 0 {
            return Err(Error::Invalid);
        }
        let origin = Origin::parse(source.origin()).map_err(|_| Error::Source)?;
        if !origin.same_origin(Origin::parse(page.source_origin()).map_err(|_| Error::Source)?) {
            return Err(Error::Source);
        }
        let metadata = page.view().map_err(|_| Error::Invalid)?;
        let observed = metadata.observed;
        if observed.account != account
            || observed.epoch != source.epoch()
            || observed.kind != DataType::Email
        {
            return Err(Error::Source);
        }
        let mut hash = Sha256::try_new().map_err(|_| Error::Crypto)?;
        field(&mut hash, b"td-mta-replica-email-v1")?;
        field(&mut hash, source.epoch().as_bytes())?;
        field(&mut hash, account.as_bytes())?;
        field(&mut hash, id.as_bytes())?;
        let mail = if let Some(email) = metadata.emails.into_iter().find(|email| email.id == id) {
            let body = match intake {
                Intake::Verified(body) => {
                    if body.blob() != email.blob
                        || body.bytes().len() as u64 != email.size
                        || body.digest() != email.digest
                    {
                        return Err(Error::Integrity);
                    }
                    Some(body)
                }
                Intake::Existing => None,
                Intake::Absent => return Err(Error::Invalid),
            };
            field(&mut hash, &[1])?;
            field(&mut hash, email.blob.as_bytes())?;
            field(&mut hash, email.thread.as_bytes())?;
            field(&mut hash, &email.received_at.to_be_bytes())?;
            field(&mut hash, &email.size.to_be_bytes())?;
            field(&mut hash, &email.digest)?;
            field(&mut hash, &(email.mailboxes.len() as u64).to_be_bytes())?;
            for mailbox in &email.mailboxes {
                field(&mut hash, mailbox.as_bytes())?;
            }
            field(&mut hash, &(email.keywords.len() as u64).to_be_bytes())?;
            let mut keywords = Vec::new();
            keywords
                .try_reserve_exact(email.keywords.len())
                .map_err(|_| Error::Capacity)?;
            for value in email.keywords {
                field(&mut hash, value.as_bytes())?;
                let mut owned = String::new();
                owned
                    .try_reserve_exact(value.len())
                    .map_err(|_| Error::Capacity)?;
                owned.push_str(value);
                keywords.push(owned);
            }
            let interpretation = body.as_ref().and_then(|body| {
                postgres_mime_evidence::prepare(body.bytes(), &email.digest, deadline)
            });
            Some(Mail {
                blob: email.blob,
                thread: email.thread,
                received_at: email.received_at,
                mailboxes: email.mailboxes,
                keywords,
                body,
                length: email.size,
                digest: email.digest,
                interpretation,
            })
        } else {
            if matches!(intake, Intake::Verified(_)) || !metadata.missing.contains(&id) {
                return Err(Error::Invalid);
            }
            field(&mut hash, &[0])?;
            None
        };
        let digest = hash.finish().map_err(|_| Error::Crypto)?;
        if Instant::now() >= deadline {
            return Err(Error::Deadline);
        }
        let source = Source::new(source.origin(), source.epoch()).map_err(|_| Error::Source)?;
        Ok(Self {
            source,
            account,
            id,
            ticket,
            observed,
            digest,
            mail,
        })
    }
}
fn field(hash: &mut Sha256, bytes: &[u8]) -> Result<(), Error> {
    hash.update(&(bytes.len() as u64).to_be_bytes())
        .map_err(|_| Error::Crypto)?;
    hash.update(bytes).map_err(|_| Error::Crypto)
}
