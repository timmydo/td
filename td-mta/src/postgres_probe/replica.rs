//! Purpose-specific retained replica publication; never general serving admission.
use super::{
    connect_authenticated_until,
    retained::{Bounds, LIFETIME},
    Error, Input, Mode, Request, Worker, MAX_DURATION,
};
use crate::{
    ids::{AccountId, StoreEpoch},
    postgres_replica_apply::{self as apply, Outcome},
    postgres_replica_progress::Progress,
    postgres_replica_source::Source,
    postgres_schema,
    replica_effect::Effect,
    replica_pending::Ticket,
    replica_transition::Transition,
};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::Instant,
};
use td_crypto::TrustStore;
pub(super) const TURNS: usize = 16;
pub(crate) struct ReplicaSession {
    worker: Worker,
    bounds: Bounds,
    epoch: StoreEpoch,
    source: Source,
    account: AccountId,
    retired: bool,
    clean_idle: bool,
}
enum Job {
    Mailboxes(Arc<crate::replica_mailboxes::Graph>),
    Inspect { source: Source, account: AccountId },
    Publish(Arc<Effect>),
    Advance(Arc<Transition>),
}
enum Completed {
    Mailboxes(crate::postgres_replica_mailboxes::Outcome),
    MissingBody,
    Inspected(Box<Progress>),
    Published(Outcome),
    Advanced(Ticket),
}
impl Job {
    fn source(&self) -> &Source {
        match self {
            Self::Mailboxes(graph) => &graph.source,
            Self::Inspect { source, .. } => source,
            Self::Publish(value) => &value.source,
            Self::Advance(value) => &value.source,
        }
    }
    fn account(&self) -> AccountId {
        match self {
            Self::Mailboxes(graph) => graph.account,
            Self::Inspect { account, .. } => *account,
            Self::Publish(value) => value.account,
            Self::Advance(value) => value.account,
        }
    }
    fn body_length(&self) -> u64 {
        match self {
            Self::Mailboxes(_) => 0,
            Self::Inspect { .. } => 0,
            Self::Publish(value) => value
                .mail
                .as_ref()
                .and_then(|mail| mail.body.as_ref())
                .map_or(0, |body| body.bytes().len() as u64),
            Self::Advance(_) => 0,
        }
    }
}
impl ReplicaSession {
    pub(crate) fn open(
        request: &Request<'_>,
        trust: &TrustStore,
        deadline: Instant,
        epoch: StoreEpoch,
        source: Source,
        account: AccountId,
    ) -> Result<Self, Error> {
        if request.mode != Mode::Runtime
            || super::remaining(deadline)? > MAX_DURATION
            || epoch == source.epoch()
        {
            return Err(Error::Invalid);
        }
        let bounds = Bounds::with_turns(Instant::now(), TURNS)?;
        let mut worker = connect_authenticated_until(
            request,
            trust,
            bounds.expires,
            deadline,
            TURNS * 72 * 1024 * 1024,
            Input::retained_replica(),
            LIFETIME,
        )?;
        worker.deadline = deadline;
        let expected = Source::new(source.origin(), source.epoch()).map_err(|_| Error::Invalid)?;
        worker.run(move |client| {
            Box::pin(async move {
                let receipt = postgres_schema::lock_and_inspect(client)
                    .await
                    .map_err(|_| Error::RuntimeSchema)?;
                if receipt.store_epoch != epoch {
                    return Err(Error::RuntimeSchema);
                }
                let mut timing =
                    crate::performance::Timer::start(crate::performance::Phase::DbRuntime);
                crate::postgres_runtime_access::qualify_replica(client, &expected)
                    .await
                    .map_err(|error| match error {
                        crate::postgres_runtime_access::Error::Role => Error::RuntimeRole,
                        crate::postgres_runtime_access::Error::Maintenance => {
                            Error::RuntimeMaintenance
                        }
                        crate::postgres_runtime_access::Error::Replica => Error::RuntimeReplica,
                        _ => Error::RuntimePrivileges,
                    })?;
                timing.succeeded();
                Ok(())
            })
        })??;
        Ok(Self {
            worker,
            bounds,
            epoch,
            source,
            account,
            retired: false,
            clean_idle: true,
        })
    }
    pub(crate) fn reusable(&self, deadline: Instant) -> bool {
        !self.retired
            && self.clean_idle
            && self.worker.failed.is_none()
            && self.bounds.reusable(deadline)
    }
    pub(crate) fn mailboxes(
        &mut self,
        graph: Arc<crate::replica_mailboxes::Graph>,
        deadline: Instant,
    ) -> Result<crate::postgres_replica_mailboxes::Outcome, apply::Error> {
        match self.run(Job::Mailboxes(graph), deadline)? {
            Completed::Mailboxes(value) => Ok(value),
            _ => Err(apply::Error::Integrity),
        }
    }
    pub(crate) fn inspect(&mut self, deadline: Instant) -> Result<Progress, apply::Error> {
        let source = Source::new(self.source.origin(), self.source.epoch())
            .map_err(|_| apply::Error::Source)?;
        match self.run(
            Job::Inspect {
                source,
                account: self.account,
            },
            deadline,
        )? {
            Completed::Inspected(progress) => Ok(*progress),
            _ => Err(apply::Error::Integrity),
        }
    }
    pub(crate) fn publish(
        &mut self,
        effect: Arc<Effect>,
        deadline: Instant,
    ) -> Result<Outcome, apply::Error> {
        match self.run(Job::Publish(effect), deadline)? {
            Completed::Published(value) => Ok(value),
            Completed::MissingBody => Err(apply::Error::MissingBody),
            _ => Err(apply::Error::Integrity),
        }
    }
    pub(crate) fn advance(
        &mut self,
        transition: Arc<Transition>,
        deadline: Instant,
    ) -> Result<Ticket, apply::Error> {
        match self.run(Job::Advance(transition), deadline)? {
            Completed::Advanced(value) => Ok(value),
            _ => Err(apply::Error::Integrity),
        }
    }
    fn run(&mut self, job: Job, deadline: Instant) -> Result<Completed, apply::Error> {
        let attempted = Arc::new(AtomicBool::new(false));
        let marker = attempted.clone();
        let result = (|| {
            if !self.reusable(deadline)
                || job.account() != self.account
                || job.source() != &self.source
            {
                return Err(apply::Error::Source);
            }
            let length = job.body_length();
            self.bounds
                .admit_bounded(Instant::now(), deadline, super::import_duration(length))
                .map_err(apply::Error::Connection)?;
            self.clean_idle = false;
            self.worker.deadline = deadline;
            self.worker.check().map_err(apply::Error::Connection)?;
            self.worker
                .begin_validation_turn()
                .map_err(apply::Error::Connection)?;
            let epoch = self.epoch;
            match self.worker.run(move |client|Box::pin(async move {
                let begin = if matches!(&job, Job::Inspect { .. }) {
                    "BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY; SET LOCAL idle_in_transaction_session_timeout=10000"
                } else {
                    "BEGIN ISOLATION LEVEL READ COMMITTED; SET LOCAL synchronous_commit=on; SET LOCAL idle_in_transaction_session_timeout=30000"
                };
                client.batch_execute(begin).await.map_err(|_|apply::Error::Database)?;
                let row=client.query_typed_one("SELECT m.store_epoch,b.blocked FROM td_mta.schema_migrations m CROSS JOIN td_mta.runtime_barrier b WHERE m.version=1 AND b.singleton=true",&[]).await.map_err(|_|apply::Error::Database)?;
                let observed:&[u8]=row.try_get(0).map_err(|_|apply::Error::Integrity)?;
                let blocked:bool=row.try_get(1).map_err(|_|apply::Error::Integrity)?;
                if observed!=epoch.as_bytes().as_slice(){return Err(apply::Error::Connection(Error::RuntimeSchema));}
                if blocked{return Err(apply::Error::Connection(Error::RuntimeMaintenance));}
                match job {
                    Job::Mailboxes(graph)=>crate::postgres_replica_mailboxes::publish(client,&graph,deadline,&marker).await.map(Completed::Mailboxes),
                    Job::Inspect {source,account}=>{
                        let progress=crate::postgres_replica_progress::read_snapshot(client,source,account).await.map_err(|_|apply::Error::Integrity)?;
                        client.batch_execute("ROLLBACK").await.map_err(|_|apply::Error::Database)?;
                        Ok(Completed::Inspected(Box::new(progress)))
                    },
                    Job::Publish(effect)=>match apply::apply(client,&effect,deadline,&marker).await {
                        Ok(value)=>Ok(Completed::Published(value)),
                        Err(apply::Error::MissingBody)=>{
                            client.batch_execute("ROLLBACK").await.map_err(|_|apply::Error::Database)?;
                            Ok(Completed::MissingBody)
                        },
                        Err(error)=>Err(error),
                    },
                    Job::Advance(transition)=>crate::postgres_replica_transition::advance(client,&transition,deadline,&marker).await.map(Completed::Advanced),
                }
            })){
                Ok(result)=>result,
                Err(_) if attempted.load(Ordering::Relaxed)=>Err(apply::Error::Indeterminate),
                Err(error)=>Err(apply::Error::Connection(error)),
            }
        })();
        match result {
            Ok(outcome) => {
                let finish = self
                    .worker
                    .check_deadline()
                    .and_then(|()| self.worker.check())
                    .and_then(|()| self.worker.finish_validation_turn());
                match finish {
                    Ok(()) => self.clean_idle = true,
                    Err(error) => {
                        self.retired = true;
                        if matches!(&outcome, Completed::Inspected(_) | Completed::MissingBody) {
                            return Err(apply::Error::Connection(error));
                        }
                    }
                }
                // Preserve confirmed COMMIT even when subsequent retirement fails.
                Ok(outcome)
            }
            Err(error) => {
                self.retired = true;
                Err(error)
            }
        }
    }
}
impl Drop for ReplicaSession {
    fn drop(&mut self) {
        if self.bounds.owner != thread::current().id() {
            self.worker.failed = Some(Error::Database);
        }
    }
}
