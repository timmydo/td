//! Email metadata, values and verified bodies under one authenticated snapshot.
use crate::{
    device_credentials::Presented,
    format::key::Key,
    ids::{EmailId, MailboxId},
    ports::{self, BlobReader, ViewIdentity},
    postgres_body::{self, BlobMetadata, Metadata, ReadRequest},
    postgres_devices,
    postgres_probe::{self, Request, Worker},
    postgres_runtime_profile::Binding,
    postgres_schema, postgres_snapshot,
};
use std::time::Instant;
use td_crypto::TrustStore;

pub const MAX_VALUES: usize = 256;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Connection(postgres_probe::Error),
    Schema(postgres_schema::Error),
    Credential(postgres_devices::Error),
    Epoch,
    Policy,
    Unauthenticated,
    Capacity,
    Integrity,
    Database,
    Invalid,
    Body(postgres_body::Error),
}
#[derive(Debug)]
pub struct Values<T> {
    pub identity: ViewIdentity,
    pub email: EmailId,
    pub values: Vec<T>,
    pub has_more: bool,
}

/// The mailbox filter shapes already supported by td-mail's Email/query.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EmailScope {
    All,
    Mailbox(MailboxId),
    Empty,
}
#[derive(Clone, Copy, Debug)]
pub struct EmailQuery {
    pub scope: EmailScope,
    pub ascending: bool,
    pub position: u64,
    pub limit: usize,
    pub calculate_total: bool,
}
#[derive(Debug)]
pub struct EmailPage {
    pub state: crate::sync::QueryState,
    pub position: u64,
    pub ids: Vec<EmailId>,
    pub total: Option<u64>,
    pub has_more: bool,
}
pub const MAX_EMAIL_PAGE: usize = 1024;

/// Every result belongs to the original snapshot. Neither this handle nor its
/// connection can escape the callback. Failed operations retire the whole view.
pub struct Snapshot<'a> {
    worker: &'a mut Worker,
    identity: ViewIdentity,
    deadline: Instant,
    failed: Option<Error>,
}

pub fn with_snapshot<T>(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    binding: &Binding,
    password: &[u8],
    read: impl FnOnce(&mut Snapshot<'_>) -> Result<T, Error>,
) -> Result<T, Error> {
    if !binding.roles().jmap() {
        return Err(Error::Policy);
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
        let mut view = Snapshot {
            worker,
            identity,
            deadline,
            failed: None,
        };
        view.check()?;
        let result = read(&mut view);
        view.check()?;
        result
    })
    .map_err(Error::Connection)?
}
impl Snapshot<'_> {
    pub fn identity(&self) -> ViewIdentity {
        self.identity
    }
    pub fn check(&mut self) -> Result<(), Error> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if let Err(error) = self.worker.check().map_err(Error::Connection) {
            self.failed = Some(error);
            return Err(error);
        }
        Ok(())
    }
    fn retain<T>(&mut self, result: Result<T, Error>) -> Result<T, Error> {
        if let Err(error) = result {
            self.failed = Some(error);
        }
        self.check()?;
        result
    }
    /// Metadata alone does not authorize body output; with_body verifies the
    /// exact same snapshot's chunks before calling its consumer.
    pub fn email(&mut self, email: EmailId) -> Result<Option<Metadata>, Error> {
        self.check()?;
        let identity = self.identity;
        let selected = ReadRequest {
            epoch: identity.epoch,
            account: identity.account,
            email,
            maximum: postgres_body::MAX_BODY_BYTES,
        };
        let result = self
            .worker
            .run(|client| {
                Box::pin(async move {
                    match postgres_body::email_metadata(client, selected).await {
                        Ok(metadata) if metadata.identity == identity => Ok(Some(metadata)),
                        Ok(_) => Err(Error::Integrity),
                        Err(postgres_body::Error::Missing) => Ok(None),
                        Err(error) => Err(Error::Body(error)),
                    }
                })
            })
            .map_err(Error::Connection)
            .and_then(|result| result);
        self.retain(result)
    }
    pub fn with_body<T>(
        &mut self,
        email: EmailId,
        read: impl FnOnce(Metadata, &mut dyn BlobReader) -> Result<T, ports::Error>,
    ) -> Result<T, Error> {
        let metadata = self
            .email(email)?
            .ok_or(Error::Body(postgres_body::Error::Missing));
        let metadata = self.retain(metadata)?;
        let result = postgres_body::with_verified_body(
            self.worker,
            BlobMetadata {
                identity: metadata.identity,
                blob: metadata.blob,
                body: metadata.body,
            },
            self.deadline,
            |body| read(metadata, body),
        )
        .map_err(Error::Body);
        self.retain(result)
    }
    /// Empty values do not prove that the Email exists; call email separately.
    pub fn mailboxes(
        &mut self,
        email: EmailId,
        after: Option<MailboxId>,
        maximum: usize,
    ) -> Result<Values<MailboxId>, Error> {
        self.check()?;
        let result = self.value_limit(maximum);
        let limit = self.retain(result)?;
        let identity = self.identity;
        let after = after.map(|value| *value.as_bytes());
        let result=self.worker.run(|client|Box::pin(async move {
            let rows=client.query("SELECT m.mailbox_id,m.changed,b.changed FROM td_mta.memberships m LEFT JOIN td_mta.mailboxes b ON b.account=m.account AND b.id=m.mailbox_id WHERE m.account=$1 AND m.email_id=$2 AND ($3::bytea IS NULL OR m.mailbox_id>$3) ORDER BY m.mailbox_id LIMIT $4", &[&identity.account.as_bytes().as_slice(),&email.as_bytes().as_slice(),&after.as_ref().map(|bytes|bytes.as_slice()),&limit]).await.map_err(|_|Error::Database)?;
            let mut values=Vec::new();
            values.try_reserve_exact(maximum).map_err(|_|Error::Capacity)?;
            let has_more=rows.len()>maximum;
            let mut previous=after;
            for row in rows {
                for column in [1,2] { check_changed(row.try_get(column).map_err(|_|Error::Integrity)?,identity)?; }
                let id:&[u8]=row.try_get(0).map_err(|_|Error::Integrity)?;
                let id:[u8;16]=id.try_into().map_err(|_|Error::Integrity)?;
                if previous.is_some_and(|previous|previous>=id) { return Err(Error::Integrity); }
                previous=Some(id);
                if values.len()<maximum { values.push(MailboxId::from_bytes(id)); }
            }
            Ok(Values { identity,email,values,has_more })
        })).map_err(Error::Connection).and_then(|result|result);
        self.retain(result)
    }
    pub fn keywords(
        &mut self,
        email: EmailId,
        after: Option<&str>,
        maximum: usize,
    ) -> Result<Values<String>, Error> {
        self.check()?;
        let result = self.value_limit(maximum);
        let limit = self.retain(result)?;
        if let Some(after) = after {
            let valid = Key::Keyword(email, after)
                .validate_local()
                .map_err(|_| Error::Invalid);
            self.retain(valid)?;
        }
        let after = after.map(copy_text).transpose();
        let after = self.retain(after)?;
        let identity = self.identity;
        let result=self.worker.run(|client|Box::pin(async move {
            let rows=client.query("SELECT keyword,changed FROM td_mta.keywords WHERE account=$1 AND email_id=$2 AND ($3::text IS NULL OR keyword>$3 COLLATE \"C\") ORDER BY keyword LIMIT $4", &[&identity.account.as_bytes().as_slice(),&email.as_bytes().as_slice(),&after,&limit]).await.map_err(|_|Error::Database)?;
            let mut values=Vec::new();
            values.try_reserve_exact(maximum).map_err(|_|Error::Capacity)?;
            let has_more=rows.len()>maximum;
            let mut previous=after.as_deref();
            for row in &rows {
                check_changed(row.try_get(1).map_err(|_|Error::Integrity)?,identity)?;
                let text:&str=row.try_get(0).map_err(|_|Error::Integrity)?;
                Key::Keyword(email,text).validate_local().map_err(|_|Error::Integrity)?;
                if previous.is_some_and(|previous|previous>=text) { return Err(Error::Integrity); }
                if values.len()<maximum { values.push(copy_text(text)?); }
                previous=Some(text);
            }
            Ok(Values { identity,email,values,has_more })
        })).map_err(Error::Connection).and_then(|result|result);
        self.retain(result)
    }
    /// Order received dates in the requested direction and equal dates by ID
    /// ascending, matching the existing td-mail query interpretation.
    pub fn query_emails(&mut self, query: EmailQuery) -> Result<EmailPage, Error> {
        self.check()?;
        if query.limit > MAX_EMAIL_PAGE {
            return self.retain(Err(Error::Capacity));
        }
        let identity = self.identity;
        let result = self.worker.run(|client| Box::pin(async move {
            let mailbox = match query.scope {
                EmailScope::Mailbox(id) => Some(*id.as_bytes()),
                _ => None,
            };
            let mut page = EmailPage {
                state: crate::sync::QueryState::current(identity, crate::sync::QueryType::Email),
                position: query.position,
                ids: Vec::new(),
                total: query.calculate_total.then_some(0),
                has_more: false,
            };
            if query.scope == EmailScope::Empty { return Ok(page); }
            let account = identity.account.as_bytes().as_slice();
            let mailbox = mailbox.as_ref().map(|id|id.as_slice());
            if query.calculate_total {
                let row=client.query_one("SELECT count(*) FROM td_mta.emails e WHERE e.account=$1 AND ($2::bytea IS NULL OR EXISTS (SELECT 1 FROM td_mta.memberships m WHERE m.account=e.account AND m.email_id=e.id AND m.mailbox_id=$2))", &[&account,&mailbox]).await.map_err(|_|Error::Database)?;
                let total:i64=row.try_get(0).map_err(|_|Error::Integrity)?;
                page.total=Some(u64::try_from(total).map_err(|_|Error::Integrity)?);
            }
            let Ok(position)=i64::try_from(query.position) else { return Ok(page); };
            let limit=i64::try_from(query.limit+1).map_err(|_|Error::Capacity)?;
            let sql=if query.ascending {
                "SELECT e.id,e.received_at,e.changed,m.changed FROM td_mta.emails e LEFT JOIN td_mta.memberships m ON m.account=e.account AND m.email_id=e.id AND m.mailbox_id=$2 WHERE e.account=$1 AND ($2::bytea IS NULL OR m.email_id IS NOT NULL) ORDER BY e.received_at ASC,e.id ASC LIMIT $3 OFFSET $4"
            } else {
                "SELECT e.id,e.received_at,e.changed,m.changed FROM td_mta.emails e LEFT JOIN td_mta.memberships m ON m.account=e.account AND m.email_id=e.id AND m.mailbox_id=$2 WHERE e.account=$1 AND ($2::bytea IS NULL OR m.email_id IS NOT NULL) ORDER BY e.received_at DESC,e.id ASC LIMIT $3 OFFSET $4"
            };
            let rows=client.query(sql,&[&account,&mailbox,&limit,&position]).await.map_err(|_|Error::Database)?;
            page.ids.try_reserve_exact(query.limit).map_err(|_|Error::Capacity)?;
            page.has_more=rows.len()>query.limit;
            let mut previous:Option<(i64,[u8;16])>=None;
            for row in rows {
                let id:&[u8]=row.try_get(0).map_err(|_|Error::Integrity)?;
                let id:[u8;16]=id.try_into().map_err(|_|Error::Integrity)?;
                let date:i64=row.try_get(1).map_err(|_|Error::Integrity)?;
                check_changed(row.try_get(2).map_err(|_|Error::Integrity)?,identity)?;
                let membership:Option<&[u8]>=row.try_get(3).map_err(|_|Error::Integrity)?;
                match (mailbox,membership) {
                    (Some(_),Some(changed))=>check_changed(changed,identity)?,
                    (None,None)=>(),
                    _=>return Err(Error::Integrity),
                }
                if let Some((old_date,old_id))=previous {
                    let order=old_date.cmp(&date);
                    let order=if query.ascending {order} else {order.reverse()};
                    if !order.then_with(||old_id.cmp(&id)).is_lt() { return Err(Error::Integrity); }
                }
                previous=Some((date,id));
                if page.ids.len()<query.limit { page.ids.push(EmailId::from_bytes(id)); }
            }
            Ok(page)
        })).map_err(Error::Connection).and_then(|result|result);
        self.retain(result)
    }
    fn value_limit(&self, maximum: usize) -> Result<i64, Error> {
        if maximum == 0 {
            return Err(Error::Invalid);
        }
        if maximum > MAX_VALUES {
            return Err(Error::Capacity);
        }
        i64::try_from(maximum + 1).map_err(|_| Error::Capacity)
    }
}
fn copy_text(text: &str) -> Result<String, Error> {
    let mut owned = String::new();
    owned
        .try_reserve_exact(text.len())
        .map_err(|_| Error::Capacity)?;
    owned.push_str(text);
    Ok(owned)
}
fn check_changed(bytes: &[u8], identity: ViewIdentity) -> Result<(), Error> {
    let sequence = u64::from_be_bytes(bytes.try_into().map_err(|_| Error::Integrity)?);
    if sequence > identity.committed_sequence.number() {
        return Err(Error::Integrity);
    }
    Ok(())
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
