//! One bounded copy step; scheduling, credentials and recovery policy are external.
use crate::{
    config::endpoint::Origin,
    ids::{AccountId, StoreEpoch},
    outbound_http::Client,
    postgres_probe::{self, Request},
    postgres_replica_apply::{self, Publisher},
    postgres_replica_source::Source,
    replica_effect::{self, Effect},
    replica_mailboxes::{self, Graph},
    replica_pending::Advance,
    replica_source::{Binding, ReadError, ReadLimits, Reader},
    replica_transition::{self, Transition},
    sync::DataState,
};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use td_crypto::TrustStore;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Invalid,
    Capacity,
    Deadline,
    Source(ReadError),
    Storage(postgres_replica_apply::Error),
    Effect(replica_effect::Error),
    Graph(replica_mailboxes::Error),
    Transition(replica_transition::Error),
}
pub struct Local<'a> {
    pub request: &'a Request<'a>,
    pub trust: &'a TrustStore,
}
pub struct Remote<'a> {
    pub client: Client<'a>,
    pub authorization: &'a str,
    pub limits: ReadLimits,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Outcome {
    pub applied: usize,
    pub changed: usize,
    pub downloaded_bytes: u64,
    pub advanced: bool,
    /// Observing an empty change page is not a copy-completeness certificate.
    pub empty_changes: Option<DataState>,
    pub mailbox_state: DataState,
    pub pending_mailbox_removals: usize,
    pub local_sequence: u64,
}
pub struct Worker<'a> {
    local: Local<'a>,
    source: Source,
    account: AccountId,
    epoch: StoreEpoch,
    publisher: Option<Publisher>,
}
impl<'a> Worker<'a> {
    pub fn new(
        source: Source,
        account: AccountId,
        epoch: StoreEpoch,
        local: Local<'a>,
    ) -> Result<Self, Error> {
        if source.epoch() == epoch {
            return Err(Error::Invalid);
        }
        Ok(Self {
            local,
            source,
            account,
            epoch,
            publisher: None,
        })
    }
    /// Call before an external wait or pause; Drop also releases the connection.
    pub fn retire(&mut self) {
        self.publisher = None;
    }
    pub fn step(&mut self, remote: Remote<'_>) -> Result<Outcome, Error> {
        let result = self.run(remote);
        if result.is_err()
            || result
                .as_ref()
                .is_ok_and(|value| value.empty_changes.is_some())
        {
            self.retire();
        }
        result
    }
    fn publisher(&mut self, deadline: Instant) -> Result<&mut Publisher, Error> {
        if self
            .publisher
            .as_ref()
            .is_none_or(|publisher| !publisher.reusable(deadline))
        {
            self.retire();
            let source = Source::new(self.source.origin(), self.source.epoch())
                .map_err(|_| Error::Invalid)?;
            self.publisher = Some(
                Publisher::open(
                    self.local.request,
                    self.local.trust,
                    job_deadline(deadline, 0)?,
                    self.epoch,
                    source,
                    self.account,
                )
                .map_err(Error::Storage)?,
            );
        }
        self.publisher.as_mut().ok_or(Error::Invalid)
    }
    fn run(&mut self, remote: Remote<'_>) -> Result<Outcome, Error> {
        let end = remote.client.deadline;
        let remaining = end
            .checked_duration_since(Instant::now())
            .ok_or(Error::Deadline)?;
        if remaining > Duration::from_secs(60) {
            return Err(Error::Invalid);
        }
        let source_origin = Origin::parse(self.source.origin()).map_err(|_| Error::Invalid)?;
        if !source_origin
            .same_origin(Origin::parse(remote.client.origin).map_err(|_| Error::Invalid)?)
        {
            return Err(Error::Invalid);
        }
        let deadline = job_deadline(end, 0)?;
        let progress = self
            .publisher(deadline)?
            .inspect(deadline)
            .map_err(Error::Storage)?;
        let mut reader = Reader::connect(
            remote.client,
            remote.authorization,
            Some(Binding {
                account: self.account,
                epoch: self.source.epoch(),
            }),
            remote.limits,
        )
        .map_err(Error::Source)?;
        let folders = reader.mailboxes().map_err(Error::Source)?;
        let mailbox_state = folders.view().map_err(Error::Source)?.observed;
        let graph = Arc::new(
            Graph::prepare(&self.source, self.account, progress.generation, folders)
                .map_err(Error::Graph)?,
        );
        let deadline = job_deadline(end, 0)?;
        let folders = self
            .publisher(deadline)?
            .mailboxes(graph, deadline)
            .map_err(Error::Storage)?;
        let mut outcome = Outcome {
            applied: 0,
            changed: 0,
            downloaded_bytes: 0,
            advanced: false,
            empty_changes: None,
            mailbox_state,
            pending_mailbox_removals: folders.pending_removals,
            local_sequence: folders.local_sequence,
        };
        let pending = progress
            .pending
            .as_ref()
            .ok_or(Error::Storage(postgres_replica_apply::Error::Integrity))?;
        if pending.complete() {
            let transition = match pending.advance() {
                Advance::Inventory(Some(cursor)) => {
                    let page = reader.inventory(Some(cursor)).map_err(Error::Source)?;
                    Transition::inventory(&self.source, self.account, pending.ticket(), &page)
                        .map_err(Error::Transition)?
                }
                Advance::Inventory(None) | Advance::Changes { .. } => {
                    let since = match pending.advance() {
                        Advance::Changes { state, .. } => state,
                        _ => progress.email_cursor,
                    };
                    let page = reader.email_changes(since).map_err(Error::Source)?;
                    if page.created.is_empty()
                        && page.updated.is_empty()
                        && page.destroyed.is_empty()
                        && !page.more
                    {
                        outcome.empty_changes = Some(page.new);
                    }
                    Transition::changes(&self.source, self.account, pending.ticket(), &page)
                        .map_err(Error::Transition)?
                }
            };
            let deadline = job_deadline(end, 0)?;
            self.publisher(deadline)?
                .advance(Arc::new(transition), deadline)
                .map_err(Error::Storage)?;
            outcome.advanced = true;
            return Ok(outcome);
        }
        let maximum = reader
            .source_limits()
            .map_err(Error::Source)?
            .metadata_ids
            .min(16);
        if maximum == 0 {
            return Err(Error::Invalid);
        }
        let mut ids = Vec::new();
        ids.try_reserve_exact(maximum)
            .map_err(|_| Error::Capacity)?;
        ids.extend(pending.unfinished().take(maximum));
        let page = reader.metadata(&ids).map_err(Error::Source)?;
        let inventory = matches!(pending.advance(), Advance::Inventory(_));
        for id in ids {
            let present = page
                .view()
                .map_err(Error::Source)?
                .emails
                .iter()
                .any(|mail| mail.id == id);
            let applied = if inventory && present {
                let body = reader.body(&page, id).map_err(Error::Source)?;
                let length = body.bytes().len() as u64;
                let deadline = job_deadline(end, length)?;
                let effect = Arc::new(
                    Effect::prepare(
                        &self.source,
                        self.account,
                        pending.ticket(),
                        &page,
                        id,
                        Some(body),
                        deadline,
                    )
                    .map_err(Error::Effect)?,
                );
                let result = self
                    .publisher(deadline)?
                    .publish(effect, deadline)
                    .map_err(Error::Storage)?;
                outcome.downloaded_bytes = outcome
                    .downloaded_bytes
                    .checked_add(length)
                    .ok_or(Error::Capacity)?;
                result
            } else {
                let deadline = job_deadline(end, 0)?;
                let effect = Arc::new(
                    Effect::prepare_existing(
                        &self.source,
                        self.account,
                        pending.ticket(),
                        &page,
                        id,
                        deadline,
                    )
                    .map_err(Error::Effect)?,
                );
                match self.publisher(deadline)?.publish(effect, deadline) {
                    Ok(result) => result,
                    Err(postgres_replica_apply::Error::MissingBody) => {
                        let body = reader.body(&page, id).map_err(Error::Source)?;
                        let length = body.bytes().len() as u64;
                        let deadline = job_deadline(end, length)?;
                        let effect = Arc::new(
                            Effect::prepare(
                                &self.source,
                                self.account,
                                pending.ticket(),
                                &page,
                                id,
                                Some(body),
                                deadline,
                            )
                            .map_err(Error::Effect)?,
                        );
                        let result = self
                            .publisher(deadline)?
                            .publish(effect, deadline)
                            .map_err(Error::Storage)?;
                        outcome.downloaded_bytes = outcome
                            .downloaded_bytes
                            .checked_add(length)
                            .ok_or(Error::Capacity)?;
                        result
                    }
                    Err(error) => return Err(Error::Storage(error)),
                }
            };
            outcome.applied += 1;
            outcome.changed += usize::from(applied.changed);
            outcome.local_sequence = applied.local_sequence;
        }
        Ok(outcome)
    }
}
fn job_deadline(end: Instant, body_length: u64) -> Result<Instant, Error> {
    let now = Instant::now();
    if now >= end {
        return Err(Error::Deadline);
    }
    Ok(now
        .checked_add(postgres_probe::import_duration(body_length))
        .ok_or(Error::Deadline)?
        .min(end))
}
