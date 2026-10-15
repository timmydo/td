//! Authenticated change pages from one bounded primary snapshot.
use crate::{
    device_credentials::Presented,
    format::{ObjectType, Sequence},
    jmap_changes::{self, History, Page},
    ports::{self, Change, ChangeAction, ChangeCursor, ChangeRecord, ChangeStep, ViewIdentity},
    postgres_devices,
    postgres_probe::{self, Request, Worker},
    postgres_runtime_profile::Binding,
    postgres_schema,
    sync::DataState,
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
    postgres_probe::with_body_worker(request,trust,deadline,|worker| {
        let identity=worker.run(|client|Box::pin(async move {
            let schema=postgres_schema::lock_and_inspect(client).await.map_err(Error::Schema)?;
            if schema.store_epoch!=binding.epoch() { return Err(Error::Epoch); }
            client.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY; SET LOCAL idle_in_transaction_session_timeout=10000").await.map_err(|_|Error::Database)?;
            let selected=client.query_one("SELECT generation,version_id FROM td_mta.config_selection WHERE singleton=true",&[]).await.map_err(|_|Error::Database)?;
            let generation=decode_sequence(selected.try_get(0).map_err(|_|Error::Integrity)?).map_err(|_|Error::Integrity)?;
            let version:Option<&[u8]>=selected.try_get(1).map_err(|_|Error::Integrity)?;
            if generation.number()!=binding.selection().generation || version!=Some(binding.selection().version.as_bytes().as_slice()) { return Err(Error::Policy); }
            postgres_devices::authenticate(client,binding.account(),presented).await.map_err(Error::Credential)?.ok_or(Error::Unauthenticated)?;
            let row=client.query_one("SELECT sequence,floor FROM td_mta.accounts WHERE id=$1", &[&binding.account().as_bytes().as_slice()]).await.map_err(|_|Error::Database)?;
            let sequence=decode_sequence(row.try_get(0).map_err(|_|Error::Integrity)?).map_err(|_|Error::Integrity)?;
            let floor=decode_sequence(row.try_get(1).map_err(|_|Error::Integrity)?).map_err(|_|Error::Integrity)?;
            if floor>sequence { return Err(Error::Integrity); }
            Ok(ViewIdentity { account:binding.account(),epoch:binding.epoch(),committed_sequence:sequence,history_floor:floor })
        })).map_err(Error::Connection)??;
        let mut view=View { worker,identity,kind:since.kind.object_type(),batch:VecDeque::new(),complete:false };
        let mut cursor=jmap_changes::Cursor::new(&mut view,since.kind,since,maximum).map_err(Error::Page)?;
        for _ in 0..MAX_HISTORY_POLLS {
            if cursor.poll().map_err(Error::Page)? {
                return cursor.finish().map_err(Error::Page);
            }
        }
        // A stable work-limit failure must let the client refresh its state,
        // rather than retrying the same oversized folding history forever.
        Err(Error::Page(jmap_changes::Error::CannotCalculateChanges))
    }).map_err(Error::Connection)?
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
                let rows=client.query("SELECT sequence,operation,action,object FROM td_mta.changes WHERE account=$1 AND kind=$2 AND (sequence,operation)>($3,$4) ORDER BY sequence,operation LIMIT 128", &[&account.as_bytes().as_slice(),&i64::from(kind.tag()),&after.sequence.number().to_be_bytes().as_slice(),&i64::from(after.operation)]).await.map_err(|_|database_error())?;
                let mut batch=VecDeque::new();
                batch.try_reserve_exact(rows.len()).map_err(|_|ports::Error::Capacity)?;
                let mut previous=after;
                for row in rows {
                    let sequence=decode_sequence(row.try_get(0).map_err(|_|ports::Error::Corrupt)?)?;
                    let operation:i64=row.try_get(1).map_err(|_|ports::Error::Corrupt)?;
                    let operation=u32::try_from(operation).ok().filter(|value|*value<4096).ok_or(ports::Error::Corrupt)?;
                    let action=match row.try_get::<_,i64>(2).map_err(|_|ports::Error::Corrupt)? {
                        1=>ChangeAction::Created,2=>ChangeAction::Updated,3=>ChangeAction::Destroyed,_=>return Err(ports::Error::Corrupt),
                    };
                    let id:&[u8]=row.try_get(3).map_err(|_|ports::Error::Corrupt)?;
                    if sequence>maximum || (sequence,operation)<=(previous.sequence,previous.operation) { return Err(ports::Error::Corrupt); }
                    let cursor=ChangeCursor { sequence,operation };
                    batch.push_back(ChangeRecord { cursor,change:Change { kind,id:id.try_into().map_err(|_|ports::Error::Corrupt)?,action } });
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
fn decode_sequence(bytes: &[u8]) -> Result<Sequence, ports::Error> {
    Ok(Sequence::from_u64(u64::from_be_bytes(
        bytes.try_into().map_err(|_| ports::Error::Corrupt)?,
    )))
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
