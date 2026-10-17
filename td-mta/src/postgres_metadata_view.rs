//! Private canonical metadata reads within one owner-side snapshot.
use crate::{
    format::{
        key::{Key, SourceKind},
        row::*,
        ObjectType, Sequence, Table,
    },
    ids::*,
    ports::{ChangeCursor, ChangeStep, Error, ReadView, Record, ViewIdentity},
    postgres_probe::{self, Worker},
};
use std::sync::Mutex;
use tokio_postgres::{
    types::{FromSql, ToSql},
    Row as SqlRow,
};

pub(super) struct View<'a> {
    pub identity: ViewIdentity,
    // The view never leaves the worker thread. Mutex supplies Sync without
    // sharing the connection or changing ReadView's contract.
    pub worker: Mutex<&'a mut Worker>,
}

fn field<'a, T: FromSql<'a>>(row: &'a SqlRow, name: &str) -> Result<T, Error> {
    row.try_get(name).map_err(|_| Error::Corrupt)
}
fn fixed<const N: usize>(row: &SqlRow, name: &str) -> Result<[u8; N], Error> {
    field::<&[u8]>(row, name)?
        .try_into()
        .map_err(|_| Error::Corrupt)
}
fn optional_id(row: &SqlRow, name: &str) -> Result<Option<[u8; 16]>, Error> {
    field::<Option<&[u8]>>(row, name)?
        .map(|b| b.try_into().map_err(|_| Error::Corrupt))
        .transpose()
}
fn number<T: TryFrom<i64>>(row: &SqlRow, name: &str) -> Result<T, Error> {
    field::<i64>(row, name)?
        .try_into()
        .map_err(|_| Error::Corrupt)
}
fn boolean(row: &SqlRow, name: &str) -> Result<bool, Error> {
    match field::<i64>(row, name)? {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(Error::Corrupt),
    }
}
fn connection(error: postgres_probe::Error) -> Error {
    match error {
        postgres_probe::Error::Deadline => Error::Deadline,
        _ => Error::Corrupt,
    }
}

fn table(table: Table) -> (&'static str, &'static str) {
    match table {
        Table::Blobs => ("blobs", "id"),
        Table::Mailboxes => ("mailboxes", "id"),
        Table::Emails => ("emails", "id"),
        Table::Memberships => ("memberships", "email_id,mailbox_id"),
        Table::Keywords => ("keywords", "email_id,keyword"),
        Table::Threads => ("threads", "id"),
        Table::ThreadAnchors => ("thread_anchors", "message_id,email_id"),
        Table::Submissions => ("submissions", "id"),
        Table::Recipients => ("recipients", "submission_id,ordinal"),
        Table::Leases => ("leases", "blob_id"),
        Table::Imports => (
            "imports",
            "source_instance,source_kind,source_account,source_object",
        ),
    }
}
type Parameter = Box<dyn ToSql + Sync + Send>;
fn parameters(key: Key<'_>) -> Vec<Parameter> {
    match key {
        Key::Blob(id) | Key::Lease(id) => vec![Box::new(id.as_bytes().to_vec())],
        Key::Mailbox(id) => vec![Box::new(id.as_bytes().to_vec())],
        Key::Email(id) => vec![Box::new(id.as_bytes().to_vec())],
        Key::Thread(id) => vec![Box::new(id.as_bytes().to_vec())],
        Key::Submission(id) => vec![Box::new(id.as_bytes().to_vec())],
        Key::Membership(a, b) => vec![
            Box::new(a.as_bytes().to_vec()),
            Box::new(b.as_bytes().to_vec()),
        ],
        Key::Keyword(id, text) => vec![Box::new(id.as_bytes().to_vec()), Box::new(text.to_owned())],
        Key::ThreadAnchor(text, id) => {
            vec![Box::new(text.to_owned()), Box::new(id.as_bytes().to_vec())]
        }
        Key::Recipient(id, ordinal) => vec![
            Box::new(id.as_bytes().to_vec()),
            Box::new(i64::from(ordinal)),
        ],
        Key::Import {
            instance,
            kind,
            account,
            object,
        } => vec![
            Box::new(instance.as_bytes().to_vec()),
            Box::new(i64::from(kind.tag())),
            Box::new(account.to_vec()),
            Box::new(object.to_vec()),
        ],
    }
}
fn key(row: &SqlRow, table: Table) -> Result<Key<'_>, Error> {
    Ok(match table {
        Table::Blobs => Key::Blob(BlobId::from_bytes(fixed(row, "id")?)),
        Table::Mailboxes => Key::Mailbox(MailboxId::from_bytes(fixed(row, "id")?)),
        Table::Emails => Key::Email(EmailId::from_bytes(fixed(row, "id")?)),
        Table::Threads => Key::Thread(ThreadId::from_bytes(fixed(row, "id")?)),
        Table::Submissions => Key::Submission(SubmissionId::from_bytes(fixed(row, "id")?)),
        Table::Leases => Key::Lease(BlobId::from_bytes(fixed(row, "blob_id")?)),
        Table::Memberships => Key::Membership(
            EmailId::from_bytes(fixed(row, "email_id")?),
            MailboxId::from_bytes(fixed(row, "mailbox_id")?),
        ),
        Table::Keywords => Key::Keyword(
            EmailId::from_bytes(fixed(row, "email_id")?),
            field(row, "keyword")?,
        ),
        Table::ThreadAnchors => Key::ThreadAnchor(
            field(row, "message_id")?,
            EmailId::from_bytes(fixed(row, "email_id")?),
        ),
        Table::Recipients => Key::Recipient(
            SubmissionId::from_bytes(fixed(row, "submission_id")?),
            number(row, "ordinal")?,
        ),
        Table::Imports => Key::Import {
            instance: InstanceId::from_bytes(fixed(row, "source_instance")?),
            kind: SourceKind::from_tag(number(row, "source_kind")?).map_err(|_| Error::Corrupt)?,
            account: field(row, "source_account")?,
            object: field(row, "source_object")?,
        },
    })
}

impl View<'_> {
    fn query(
        &mut self,
        selected: Table,
        cursor: Option<Key<'_>>,
        exact: bool,
    ) -> Result<Option<(SqlRow, Vec<SqlRow>)>, Error> {
        let (name, columns) = table(selected);
        let account = self.identity.account.as_bytes().to_vec();
        let mut values: Vec<Parameter> = vec![Box::new(account)];
        let mut statement = format!("SELECT * FROM td_mta.{name} WHERE account=$1");
        if let Some(cursor) = cursor {
            cursor.validate_local().map_err(|_| Error::Invalid)?;
            if cursor.table() != selected {
                return Err(Error::Invalid);
            }
            let keys = parameters(cursor);
            let placeholders = (2..keys.len() + 2)
                .map(|n| format!("${n}"))
                .collect::<Vec<_>>()
                .join(",");
            statement.push_str(&format!(
                " AND ({columns}){}({placeholders})",
                if exact { "=" } else { ">" }
            ));
            values.extend(keys);
        }
        statement.push_str(&format!(" ORDER BY {columns} LIMIT 1"));
        let account = self.identity.account;
        self.worker.get_mut().map_err(|_|Error::Corrupt)?.run(move |client| Box::pin(async move {
            let params:Vec<&(dyn ToSql+Sync)>=values.iter().map(|p|p.as_ref() as &(dyn ToSql+Sync)).collect();
            let rows=client.query(&statement,&params).await.map_err(|_|Error::Corrupt)?;
            let Some(row)=rows.into_iter().next() else {return Ok(None)};
            let receipts=if selected==Table::Emails {
                let id:Vec<u8>=field(&row,"id")?;
                client.query("SELECT ordinal,address FROM td_mta.smtp_receipt_recipients WHERE account=$1 AND email_id=$2 ORDER BY ordinal LIMIT 1001",&[&account.as_bytes().as_slice(),&id]).await.map_err(|_|Error::Corrupt)?
            } else {Vec::new()};
            Ok(Some((row,receipts)))
        })).map_err(connection)?
    }
    fn decode<'a>(
        &self,
        sql: &SqlRow,
        receipts: &[SqlRow],
        selected: Table,
        output: &'a mut [u8],
    ) -> Result<(Row<'a>, Sequence), Error> {
        let changed = Sequence::from_u64(u64::from_be_bytes(fixed(sql, "changed")?));
        if changed > self.identity.committed_sequence {
            return Err(Error::Corrupt);
        }
        let mut receipt_bytes = [0; MAX_RECEIPT_BYTES];
        let row = match selected {
            Table::Blobs => Row::Blob(BlobRow {
                length: number(sql, "length")?,
                digest: fixed(sql, "digest")?,
                created_at: field(sql, "created_at")?,
            }),
            Table::Mailboxes => Row::Mailbox(MailboxRow {
                name: field(sql, "name")?,
                parent: optional_id(sql, "parent_id")?.map(MailboxId::from_bytes),
                role: field(sql, "role")?,
                sort_order: number(sql, "sort_order")?,
                subscribed: boolean(sql, "subscribed")?,
            }),
            Table::Emails => {
                let origin = match field::<i64>(sql, "origin")? {
                    1 => {
                        if number::<usize>(sql, "receipt_count")? != receipts.len() {
                            return Err(Error::Corrupt);
                        }
                        let mut addresses = Vec::with_capacity(receipts.len());
                        for (ordinal, recipient) in receipts.iter().enumerate() {
                            if number::<usize>(recipient, "ordinal")? != ordinal {
                                return Err(Error::Corrupt);
                            }
                            addresses.push(field::<&str>(recipient, "address")?);
                        }
                        let peer = match field::<i64>(sql, "peer_family")? {
                            4 => std::net::IpAddr::from(fixed::<4>(sql, "peer_octets")?),
                            6 => std::net::IpAddr::from(fixed::<16>(sql, "peer_octets")?),
                            _ => return Err(Error::Corrupt),
                        };
                        EmailOrigin::Smtp(SmtpReceipt {
                            peer,
                            gateway: field(sql, "gateway")?,
                            tls: ReceiptTls::from_tag(number(sql, "tls")?)
                                .map_err(|_| Error::Corrupt)?,
                            ehlo: field(sql, "ehlo")?,
                            reverse_path: field(sql, "reverse_path")?,
                            recipients: ReceiptRecipients::encode(&addresses, &mut receipt_bytes)
                                .map_err(|_| Error::Corrupt)?,
                        })
                    }
                    2 => EmailOrigin::Jmap,
                    3 => EmailOrigin::Import,
                    4 => EmailOrigin::FailureNotice,
                    _ => return Err(Error::Corrupt),
                };
                if !matches!(origin, EmailOrigin::Smtp(_)) && !receipts.is_empty() {
                    return Err(Error::Corrupt);
                }
                Row::Email(EmailRow {
                    blob: BlobId::from_bytes(fixed(sql, "blob_id")?),
                    thread: ThreadId::from_bytes(fixed(sql, "thread_id")?),
                    received_at: field(sql, "received_at")?,
                    origin,
                })
            }
            Table::Memberships => Row::Membership,
            Table::Keywords => Row::Keyword,
            Table::Threads => Row::Thread,
            Table::ThreadAnchors => Row::ThreadAnchor,
            Table::Submissions => Row::Submission(SubmissionRow {
                email: EmailId::from_bytes(fixed(sql, "email_id")?),
                thread: ThreadId::from_bytes(fixed(sql, "thread_id")?),
                identity: IdentityId::from_bytes(fixed(sql, "identity_id")?),
                transmitted_blob: BlobId::from_bytes(fixed(sql, "transmitted_blob_id")?),
                reverse_path: field(sql, "reverse_path")?,
                send_at: field(sql, "send_at")?,
                expires_at: field(sql, "expires_at")?,
                recipient_count: number(sql, "recipient_count")?,
                completed_at: field(sql, "completed_at")?,
                notification: NotificationState::from_tag(number(sql, "notification")?)
                    .map_err(|_| Error::Corrupt)?,
                notification_email: optional_id(sql, "notification_email_id")?
                    .map(EmailId::from_bytes),
            }),
            Table::Recipients => Row::Recipient(RecipientRow {
                address: field(sql, "address")?,
                state: RecipientState::from_tag(number(sql, "state")?)
                    .map_err(|_| Error::Corrupt)?,
                uncertain: boolean(sql, "uncertain")?,
                attempt: optional_id(sql, "attempt_id")?.map(AttemptId::from_bytes),
                attempt_count: number(sql, "attempt_count")?,
                last_attempt_at: field(sql, "last_attempt_at")?,
                phase: AttemptPhase::from_tag(number(sql, "phase")?).map_err(|_| Error::Corrupt)?,
                next_attempt_at: field(sql, "next_attempt_at")?,
                rcpt_reply: field(sql, "rcpt_reply")?,
                data_reply: field(sql, "data_reply")?,
                reason: FailureReason::from_tag(number(sql, "reason")?)
                    .map_err(|_| Error::Corrupt)?,
                diagnostic: field(sql, "diagnostic")?,
            }),
            Table::Leases => Row::Lease(LeaseRow {
                account: self.identity.account,
                device: DeviceId::from_bytes(fixed(sql, "device_id")?),
                expires_at: field(sql, "expires_at")?,
                uses: LeaseUse::from_tag(number(sql, "uses")?).map_err(|_| Error::Corrupt)?,
            }),
            Table::Imports => Row::Import(ImportRow {
                local_object: fixed(sql, "local_object")?,
                historical_blob: optional_id(sql, "historical_blob")?.map(BlobId::from_bytes),
                source_digest: fixed(sql, "source_digest")?,
            }),
        };
        row.validate_key(key(sql, selected)?)
            .map_err(|_| Error::Corrupt)?;
        let length = row.encode(output).map_err(|e| {
            if e == crate::format::Error::OutputFull {
                Error::Capacity
            } else {
                Error::Corrupt
            }
        })?;
        Ok((
            Row::decode(selected, output.get(..length).ok_or(Error::Capacity)?)
                .map_err(|_| Error::Corrupt)?,
            changed,
        ))
    }
}
impl ReadView for View<'_> {
    fn identity(&self) -> ViewIdentity {
        self.identity
    }
    fn next_change(&mut self, _: ChangeCursor, _: ObjectType) -> Result<ChangeStep, Error> {
        Err(Error::Invalid)
    }
    fn get<'a>(
        &mut self,
        requested: Key<'_>,
        value: &'a mut [u8],
    ) -> Result<Option<(Row<'a>, Sequence)>, Error> {
        let Some((sql, receipts)) = self.query(requested.table(), Some(requested), true)? else {
            return Ok(None);
        };
        if key(&sql, requested.table())?.compare(requested) != std::cmp::Ordering::Equal {
            return Err(Error::Corrupt);
        }
        self.decode(&sql, &receipts, requested.table(), value)
            .map(Some)
    }
    fn next<'a>(
        &mut self,
        selected: Table,
        after: Option<&[u8]>,
        output_key: &'a mut [u8],
        value: &'a mut [u8],
    ) -> Result<Option<Record<'a>>, Error> {
        let cursor = after
            .map(|bytes| Key::decode(selected, bytes).map_err(|_| Error::Invalid))
            .transpose()?;
        let Some((sql, receipts)) = self.query(selected, cursor, false)? else {
            return Ok(None);
        };
        let decoded = key(&sql, selected)?;
        if cursor.is_some_and(|cursor| !decoded.compare(cursor).is_gt()) {
            return Err(Error::Corrupt);
        }
        let length = decoded.encode(output_key).map_err(|_| Error::Capacity)?;
        let key = Key::decode(selected, output_key.get(..length).ok_or(Error::Capacity)?)
            .map_err(|_| Error::Corrupt)?;
        let (row, last_change) = self.decode(&sql, &receipts, selected, value)?;
        Ok(Some(Record {
            key,
            row,
            last_change,
        }))
    }
}
