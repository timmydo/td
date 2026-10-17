//! Authenticated change pages from one bounded primary snapshot.
use crate::{
    device_credentials::Presented,
    format::ObjectType,
    jmap_changes::{self, History, Page},
    ports::{self, ChangeCursor, ChangeRecord, ChangeStep, ViewIdentity},
    postgres_devices,
    postgres_probe::{self, Request, Worker},
    postgres_runtime_profile::Binding,
    postgres_schema, postgres_snapshot,
    sync::{DataState, DataType},
};
use std::{collections::VecDeque, io::ErrorKind, time::Instant};
use td_crypto::TrustStore;

// Leave room below the finite wire-frame ceiling for setup and batch replies.
const MAX_HISTORY_POLLS: usize = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Connection(postgres_probe::Error),
    Schema(postgres_schema::Error),
    Credential(postgres_devices::Error),
    Epoch,
    Policy,
    Unauthenticated,
    Integrity,
    Database,
    Page(jmap_changes::Error),
}

/// Return a detached page only after the existing transaction-aligned merger
/// completes. A state token is not authorization; authenticate each request.
pub fn read(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    binding: &Binding,
    password: &[u8],
    since: DataState,
    maximum: usize,
) -> Result<Page, Error> {
    if !binding.roles().jmap() {
        return Err(Error::Policy);
    }
    if maximum == 0 || maximum > jmap_changes::MAX_CHANGES {
        return Err(Error::Page(jmap_changes::Error::Invalid));
    }
    let presented = Presented::parse(password).ok_or(Error::Unauthenticated)?;
    let binding = binding.clone();
    postgres_probe::with_body_worker(request, trust, deadline, |worker| {
        let identity = worker
            .run(|client| {
                Box::pin(async move {
                    postgres_snapshot::begin(client, &binding, presented)
                        .await
                        .map_err(snapshot_error)
                })
            })
            .map_err(Error::Connection)??;
        read_snapshot(worker, identity, since.kind, since, maximum, &mut || Ok(()))
            .map_err(Error::Page)
    })
    .map_err(Error::Connection)?
}

/// Consume history without opening a second transaction or refreshing authority.
pub(crate) fn read_snapshot(
    worker: &mut Worker,
    identity: ViewIdentity,
    kind: DataType,
    since: DataState,
    maximum: usize,
    before_poll: &mut dyn FnMut() -> Result<(), ports::Error>,
) -> Result<Page, jmap_changes::Error> {
    let mut view = View {
        worker,
        identity,
        kind: kind.object_type(),
        batch: VecDeque::new(),
        complete: false,
    };
    let mut cursor = jmap_changes::Cursor::new(&mut view, kind, since, maximum)?;
    for _ in 0..MAX_HISTORY_POLLS {
        before_poll()?;
        if cursor.poll()? {
            return cursor.finish();
        }
    }
    Err(jmap_changes::Error::CannotCalculateChanges)
}

struct View<'a> {
    worker: &'a mut Worker,
    identity: ViewIdentity,
    kind: ObjectType,
    batch: VecDeque<ChangeRecord>,
    complete: bool,
}
impl History for View<'_> {
    fn history_identity(&self) -> ViewIdentity {
        self.identity
    }
    fn check_history(&mut self) -> Result<(), ports::Error> {
        self.worker.check().map_err(connection_error)
    }
    fn read_change(
        &mut self,
        after: ChangeCursor,
        kind: ObjectType,
    ) -> Result<ChangeStep, ports::Error> {
        self.check_history()?;
        if kind != self.kind {
            return Err(ports::Error::Invalid);
        }
        if self.batch.is_empty() && !self.complete {
            let account = self.identity.account;
            let maximum = self.identity.committed_sequence;
            let batch=self.worker.run(|client|Box::pin(async move {
                let rows=client.query("SELECT sequence,operation,action,object,kind FROM td_mta.changes WHERE account=$1 AND kind=$2 AND (sequence,operation)>($3,$4) ORDER BY sequence,operation LIMIT 128", &[&account.as_bytes().as_slice(),&i64::from(kind.tag()),&after.sequence.number().to_be_bytes().as_slice(),&i64::from(after.operation)]).await.map_err(|_|database_error())?;
                let mut batch=VecDeque::new();
                batch.try_reserve_exact(rows.len()).map_err(|_|ports::Error::Capacity)?;
                let mut previous=after;
                for row in rows {
                    let record=crate::postgres_change_row::read(&row)?;
                    let cursor=record.cursor;
                    if record.change.kind!=kind || cursor.sequence>maximum || (cursor.sequence,cursor.operation)<=(previous.sequence,previous.operation) { return Err(ports::Error::Corrupt); }
                    batch.push_back(record);
                    previous=cursor;
                }
                Ok(batch)
            })).map_err(connection_error)??;
            self.complete = batch.len() < 128;
            self.batch = batch;
        }
        self.check_history()?;
        Ok(match self.batch.pop_front() {
            Some(record) => ChangeStep::Record(record),
            None => ChangeStep::Complete,
        })
    }
}
fn database_error() -> ports::Error {
    ports::Error::Io {
        kind: ErrorKind::ConnectionAborted,
        os_code: None,
    }
}
fn connection_error(error: postgres_probe::Error) -> ports::Error {
    if error == postgres_probe::Error::Deadline {
        ports::Error::Deadline
    } else {
        database_error()
    }
}

fn snapshot_error(error: postgres_snapshot::Error) -> Error {
    match error {
        postgres_snapshot::Error::Schema(value) => Error::Schema(value),
        postgres_snapshot::Error::Credential(value) => Error::Credential(value),
        postgres_snapshot::Error::Epoch => Error::Epoch,
        postgres_snapshot::Error::Policy => Error::Policy,
        postgres_snapshot::Error::Unauthenticated => Error::Unauthenticated,
        postgres_snapshot::Error::Integrity => Error::Integrity,
        postgres_snapshot::Error::Database => Error::Database,
    }
}
