//! Closed relational SQL adapter. Row codecs are transient caller-buffer views.
use super::{sequence, sql};
use crate::format::row::EmailOrigin;
use crate::{
    format::{self, key::Key, row::Row, Sequence, Table},
    ids::AccountId,
    ports,
};
use rusqlite::{params, Connection, Statement};

#[path = "relational/read.rs"]
mod read;
#[path = "relational/schema.rs"]
mod schema;

pub(super) const SCHEMA: &str = schema::SCHEMA;

fn format_error(error: format::Error) -> ports::Error {
    match error {
        format::Error::OutputFull => ports::Error::Capacity,
        _ => ports::Error::Corrupt,
    }
}
fn bind_key(
    statement: &mut Statement<'_>,
    account: AccountId,
    key: Key<'_>,
) -> Result<(), ports::Error> {
    statement
        .raw_bind_parameter(1, account.as_bytes().as_slice())
        .map_err(sql)?;
    match key {
        Key::Blob(id) | Key::Lease(id) => statement
            .raw_bind_parameter(2, id.as_bytes().as_slice())
            .map_err(sql)?,
        Key::Mailbox(id) => statement
            .raw_bind_parameter(2, id.as_bytes().as_slice())
            .map_err(sql)?,
        Key::Email(id) => statement
            .raw_bind_parameter(2, id.as_bytes().as_slice())
            .map_err(sql)?,
        Key::Thread(id) => statement
            .raw_bind_parameter(2, id.as_bytes().as_slice())
            .map_err(sql)?,
        Key::Submission(id) => statement
            .raw_bind_parameter(2, id.as_bytes().as_slice())
            .map_err(sql)?,
        Key::Membership(email, mailbox) => {
            statement
                .raw_bind_parameter(2, email.as_bytes().as_slice())
                .map_err(sql)?;
            statement
                .raw_bind_parameter(3, mailbox.as_bytes().as_slice())
                .map_err(sql)?;
        }
        Key::Keyword(email, keyword) => {
            statement
                .raw_bind_parameter(2, email.as_bytes().as_slice())
                .map_err(sql)?;
            statement.raw_bind_parameter(3, keyword).map_err(sql)?;
        }
        Key::ThreadAnchor(message, email) => {
            statement.raw_bind_parameter(2, message).map_err(sql)?;
            statement
                .raw_bind_parameter(3, email.as_bytes().as_slice())
                .map_err(sql)?;
        }
        Key::Recipient(submission, ordinal) => {
            statement
                .raw_bind_parameter(2, submission.as_bytes().as_slice())
                .map_err(sql)?;
            statement.raw_bind_parameter(3, ordinal).map_err(sql)?;
        }
        Key::Import {
            instance,
            kind,
            account,
            object,
        } => {
            statement
                .raw_bind_parameter(2, instance.as_bytes().as_slice())
                .map_err(sql)?;
            statement.raw_bind_parameter(3, kind.tag()).map_err(sql)?;
            statement.raw_bind_parameter(4, account).map_err(sql)?;
            statement.raw_bind_parameter(5, object).map_err(sql)?;
        }
    }
    Ok(())
}
pub(super) fn delete(
    db: &Connection,
    account: AccountId,
    key: Key<'_>,
) -> Result<(), ports::Error> {
    key.validate_local().map_err(|_| ports::Error::Invalid)?;
    let mut statement = db
        .prepare(schema::queries(key.table()).delete)
        .map_err(sql)?;
    bind_key(&mut statement, account, key)?;
    statement.raw_execute().map_err(sql)?;
    Ok(())
}
pub(super) fn get(
    db: &Connection,
    account: AccountId,
    key: Key<'_>,
    value: &mut [u8],
) -> Result<Option<(usize, Sequence)>, ports::Error> {
    key.validate_local().map_err(|_| ports::Error::Invalid)?;
    let mut statement = db.prepare(schema::queries(key.table()).get).map_err(sql)?;
    bind_key(&mut statement, account, key)?;
    let mut rows = statement.raw_query();
    let Some(row) = rows.next().map_err(sql)? else {
        return Ok(None);
    };
    let (length, changed) = read::value(db, account, key.table(), row, value)?;
    Row::decode(
        key.table(),
        value.get(..length).ok_or(ports::Error::Corrupt)?,
    )
    .and_then(|row| row.validate_key(key))
    .map_err(format_error)?;
    Ok(Some((length, changed)))
}
pub(super) fn next(
    db: &Connection,
    account: AccountId,
    table: Table,
    after: Option<&[u8]>,
    key: &mut [u8],
    value: &mut [u8],
) -> Result<Option<(usize, usize, Sequence)>, ports::Error> {
    let queries = schema::queries(table);
    let mut statement = db
        .prepare(if after.is_some() {
            queries.next
        } else {
            queries.first
        })
        .map_err(sql)?;
    if let Some(after) = after {
        let after = Key::decode(table, after).map_err(|_| ports::Error::Invalid)?;
        after.validate_local().map_err(|_| ports::Error::Invalid)?;
        bind_key(&mut statement, account, after)?;
    } else {
        statement
            .raw_bind_parameter(1, account.as_bytes().as_slice())
            .map_err(sql)?;
    }
    let mut rows = statement.raw_query();
    let Some(row) = rows.next().map_err(sql)? else {
        return Ok(None);
    };
    let length = read::key(table, row, key)?;
    let (value_length, changed) = read::value(db, account, table, row, value)?;
    format::row::decode_record(
        table,
        key.get(..length).ok_or(ports::Error::Corrupt)?,
        value.get(..value_length).ok_or(ports::Error::Corrupt)?,
    )
    .map_err(format_error)?;
    Ok(Some((length, value_length, changed)))
}

pub(super) fn put(
    db: &Connection,
    account: AccountId,
    key: Key<'_>,
    row: Row<'_>,
    changed: Sequence,
) -> Result<(), ports::Error> {
    row.validate_key(key).map_err(|_| ports::Error::Invalid)?;
    let a = account.as_bytes().as_slice();
    let sequence = changed.number().to_be_bytes();
    let c = sequence.as_slice();
    match (key, row) {
        (Key::Blob(id), Row::Blob(v)) => {
            db.execute(
                schema::PUT_BLOBS,
                params![
                    a,
                    id.as_bytes().as_slice(),
                    v.kind.tag(),
                    i64::try_from(v.length).map_err(|_| ports::Error::Capacity)?,
                    v.digest.as_slice(),
                    v.created_at,
                    c
                ],
            )
            .map_err(sql)?;
        }
        (Key::Mailbox(id), Row::Mailbox(v)) => {
            db.execute(
                schema::PUT_MAILBOXES,
                params![
                    a,
                    id.as_bytes().as_slice(),
                    v.name,
                    v.parent.as_ref().map(|v| v.as_bytes().as_slice()),
                    v.role,
                    v.sort_order,
                    v.subscribed,
                    c
                ],
            )
            .map_err(sql)?;
        }
        (Key::Email(id), Row::Email(v)) => {
            let (origin, receipt) = match v.origin {
                EmailOrigin::Smtp(r) => (1, Some(r)),
                EmailOrigin::Jmap => (2, None),
                EmailOrigin::Import => (3, None),
                EmailOrigin::FailureNotice => (4, None),
            };
            let v4 = receipt.and_then(|r| {
                if let std::net::IpAddr::V4(ip) = r.peer {
                    Some(ip.octets())
                } else {
                    None
                }
            });
            let v6 = receipt.and_then(|r| {
                if let std::net::IpAddr::V6(ip) = r.peer {
                    Some(ip.octets())
                } else {
                    None
                }
            });
            let peer = v4
                .as_ref()
                .map(|v| v.as_slice())
                .or_else(|| v6.as_ref().map(|v| v.as_slice()));
            let family = receipt.map(|r| if r.peer.is_ipv4() { 4 } else { 6 });
            db.execute(
                schema::PUT_EMAILS,
                params![
                    a,
                    id.as_bytes().as_slice(),
                    v.blob.as_bytes().as_slice(),
                    v.thread.as_bytes().as_slice(),
                    v.received_at,
                    origin,
                    family,
                    peer,
                    receipt.and_then(|r| r.gateway),
                    receipt.map(|r| r.tls.tag()),
                    receipt.map(|r| r.ehlo),
                    receipt.map(|r| r.reverse_path),
                    receipt.map(|r| r.recipients.count()),
                    c
                ],
            )
            .map_err(sql)?;
            db.execute(
                "DELETE FROM smtp_receipt_recipients WHERE account=?1 AND email_id=?2",
                params![a, id.as_bytes().as_slice()],
            )
            .map_err(sql)?;
            if let Some(receipt) = receipt {
                let mut insert = db.prepare(schema::INSERT_RECEIPT).map_err(sql)?;
                for (ordinal, address) in receipt
                    .recipients
                    .iter()
                    .map_err(|_| ports::Error::Invalid)?
                    .enumerate()
                {
                    let address = address.map_err(|_| ports::Error::Invalid)?;
                    insert
                        .execute(params![
                            a,
                            id.as_bytes().as_slice(),
                            i64::try_from(ordinal).map_err(|_| ports::Error::Capacity)?,
                            address
                        ])
                        .map_err(sql)?;
                }
            }
        }
        (Key::Membership(email, mailbox), Row::Membership) => {
            db.execute(
                schema::PUT_MEMBERSHIPS,
                params![
                    a,
                    email.as_bytes().as_slice(),
                    mailbox.as_bytes().as_slice(),
                    c
                ],
            )
            .map_err(sql)?;
        }
        (Key::Keyword(email, keyword), Row::Keyword) => {
            db.execute(
                schema::PUT_KEYWORDS,
                params![a, email.as_bytes().as_slice(), keyword, c],
            )
            .map_err(sql)?;
        }
        (Key::Thread(id), Row::Thread) => {
            db.execute(schema::PUT_THREADS, params![a, id.as_bytes().as_slice(), c])
                .map_err(sql)?;
        }
        (Key::ThreadAnchor(message, email), Row::ThreadAnchor) => {
            db.execute(
                schema::PUT_THREAD_ANCHORS,
                params![a, message, email.as_bytes().as_slice(), c],
            )
            .map_err(sql)?;
        }
        (Key::Submission(id), Row::Submission(v)) => {
            db.execute(
                schema::PUT_SUBMISSIONS,
                params![
                    a,
                    id.as_bytes().as_slice(),
                    v.email.as_bytes().as_slice(),
                    v.thread.as_bytes().as_slice(),
                    v.identity.as_bytes().as_slice(),
                    v.transmitted_blob.as_bytes().as_slice(),
                    v.reverse_path,
                    v.send_at,
                    v.expires_at,
                    v.recipient_count,
                    v.completed_at,
                    v.notification.tag(),
                    v.notification_email
                        .as_ref()
                        .map(|v| v.as_bytes().as_slice()),
                    c
                ],
            )
            .map_err(sql)?;
        }
        (Key::Recipient(id, ordinal), Row::Recipient(v)) => {
            db.execute(
                schema::PUT_RECIPIENTS,
                params![
                    a,
                    id.as_bytes().as_slice(),
                    ordinal,
                    v.address,
                    v.state.tag(),
                    v.uncertain,
                    v.attempt.as_ref().map(|v| v.as_bytes().as_slice()),
                    v.attempt_count,
                    v.last_attempt_at,
                    v.phase.tag(),
                    v.next_attempt_at,
                    v.rcpt_reply,
                    v.data_reply,
                    v.reason.tag(),
                    v.diagnostic,
                    c
                ],
            )
            .map_err(sql)?;
        }
        (Key::Lease(id), Row::Lease(v)) => {
            if v.account != account {
                return Err(ports::Error::Invalid);
            }
            db.execute(
                schema::PUT_LEASES,
                params![
                    a,
                    id.as_bytes().as_slice(),
                    v.device.as_bytes().as_slice(),
                    v.expires_at,
                    v.uses.tag(),
                    c
                ],
            )
            .map_err(sql)?;
        }
        (
            Key::Import {
                instance,
                kind,
                account: source_account,
                object,
            },
            Row::Import(v),
        ) => {
            db.execute(
                schema::PUT_IMPORTS,
                params![
                    a,
                    instance.as_bytes().as_slice(),
                    kind.tag(),
                    source_account,
                    object,
                    v.local_object.as_slice(),
                    v.historical_blob.as_ref().map(|v| v.as_bytes().as_slice()),
                    v.source_digest.as_slice(),
                    c
                ],
            )
            .map_err(sql)?;
        }
        _ => return Err(ports::Error::Invalid),
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::{
        format::{key::SourceKind, row::*},
        ids::*,
    };
    const ACCOUNT: AccountId = AccountId::from_bytes([1; 16]);
    const EMAIL: EmailId = EmailId::from_bytes([2; 16]);
    const MAILBOX: MailboxId = MailboxId::from_bytes([3; 16]);
    const THREAD: ThreadId = ThreadId::from_bytes([4; 16]);
    const MESSAGE: BlobId = BlobId::from_bytes([5; 16]);
    const UPLOAD: BlobId = BlobId::from_bytes([6; 16]);
    const SUBMISSION: SubmissionId = SubmissionId::from_bytes([7; 16]);
    fn database() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("PRAGMA foreign_keys=ON; PRAGMA trusted_schema=OFF;")
            .unwrap();
        for statement in SCHEMA.split(';').map(str::trim).filter(|s| !s.is_empty()) {
            assert!(statement.len() <= 8192);
            db.execute_batch(statement).unwrap();
        }
        db.execute(
            "INSERT INTO accounts(id,sequence,floor) VALUES(?1,?2,?2)",
            params![ACCOUNT.as_bytes().as_slice(), 0u64.to_be_bytes().as_slice()],
        )
        .unwrap();
        for id in [MESSAGE, UPLOAD] {
            db.execute(
                "INSERT INTO blob_ids VALUES(?1,?2)",
                params![ACCOUNT.as_bytes().as_slice(), id.as_bytes().as_slice()],
            )
            .unwrap();
        }
        db
    }
    fn blob(kind: BlobKind) -> Row<'static> {
        Row::Blob(BlobRow {
            kind,
            length: 3,
            digest: [9; 32],
            created_at: -11,
        })
    }
    fn mailbox() -> Row<'static> {
        Row::Mailbox(MailboxRow {
            name: "Réçus",
            parent: None,
            role: Some("inbox"),
            sort_order: 42,
            subscribed: true,
        })
    }
    fn email(origin: EmailOrigin<'_>) -> Row<'_> {
        Row::Email(EmailRow {
            blob: MESSAGE,
            thread: THREAD,
            received_at: -12,
            origin,
        })
    }
    fn populate(db: &Connection) {
        let changed = Sequence::from_u64(1);
        db.execute_batch("BEGIN").unwrap();
        for (key, row) in [
            (Key::Blob(MESSAGE), blob(BlobKind::Message)),
            (Key::Blob(UPLOAD), blob(BlobKind::Upload)),
            (Key::Thread(THREAD), Row::Thread),
            (Key::Mailbox(MAILBOX), mailbox()),
            (Key::Email(EMAIL), email(EmailOrigin::Jmap)),
        ] {
            put(db, ACCOUNT, key, row, changed).unwrap();
        }
        db.execute_batch("COMMIT").unwrap();
    }
    fn assert_roundtrip(db: &Connection, key: Key<'_>, row: Row<'_>, changed: Sequence) {
        let mut value = vec![0; 65536];
        let (length, sequence) = get(db, ACCOUNT, key, &mut value).unwrap().unwrap();
        assert_eq!(sequence, changed);
        assert_eq!(Row::decode(key.table(), &value[..length]).unwrap(), row);
        let mut encoded = vec![0; 65536];
        let expected = row.encode(&mut encoded).unwrap();
        assert_eq!(&value[..length], &encoded[..expected]);
    }
    #[test]
    fn every_domain_field_roundtrips_through_explicit_sql_columns() {
        let db = database();
        populate(&db);
        let mut recipients = [0; 1024];
        let receipt = ReceiptRecipients::encode(
            &[
                "same@example.test",
                "same@example.test",
                "other@example.test",
            ],
            &mut recipients,
        )
        .unwrap();
        let origin = EmailOrigin::Smtp(SmtpReceipt {
            peer: "::ffff:192.0.2.1".parse().unwrap(),
            gateway: Some("gateway-1"),
            tls: ReceiptTls::Tls13,
            ehlo: "host.test",
            reverse_path: "",
            recipients: receipt,
        });
        let rows = [
            (Key::Blob(MESSAGE), blob(BlobKind::Message)),
            (Key::Mailbox(MAILBOX), mailbox()),
            (Key::Email(EMAIL), email(origin)),
            (Key::Membership(EMAIL, MAILBOX), Row::Membership),
            (Key::Keyword(EMAIL, "$seen"), Row::Keyword),
            (Key::Thread(THREAD), Row::Thread),
            (
                Key::ThreadAnchor("é@message.test", EMAIL),
                Row::ThreadAnchor,
            ),
            (
                Key::Submission(SUBMISSION),
                Row::Submission(SubmissionRow {
                    email: EmailId::from_bytes([99; 16]),
                    thread: ThreadId::from_bytes([98; 16]),
                    identity: IdentityId::from_bytes([97; 16]),
                    transmitted_blob: MESSAGE,
                    reverse_path: "sender@test.example",
                    send_at: 10,
                    expires_at: 20,
                    recipient_count: 1,
                    completed_at: Some(-30),
                    notification: NotificationState::Stored,
                    notification_email: Some(EmailId::from_bytes([96; 16])),
                }),
            ),
            (
                Key::Recipient(SUBMISSION, 0),
                Row::Recipient(RecipientRow {
                    address: "target@test.example",
                    state: RecipientState::OutcomeUnknown,
                    uncertain: true,
                    attempt: Some(AttemptId::from_bytes([8; 16])),
                    attempt_count: u32::MAX,
                    last_attempt_at: Some(-5),
                    phase: AttemptPhase::AcceptancePossible,
                    next_attempt_at: Some(10),
                    rcpt_reply: Some("250 accepted"),
                    data_reply: Some("451 retry"),
                    reason: FailureReason::Uncertain,
                    diagnostic: "connection lost",
                }),
            ),
            (
                Key::Lease(UPLOAD),
                Row::Lease(LeaseRow {
                    account: ACCOUNT,
                    device: DeviceId::from_bytes([10; 16]),
                    expires_at: -1,
                    uses: LeaseUse::Both,
                }),
            ),
            (
                Key::Import {
                    instance: InstanceId::from_bytes([11; 16]),
                    kind: SourceKind::Email,
                    account: b"remote\0account",
                    object: b"remote\0object",
                },
                Row::Import(ImportRow {
                    local_object: [12; 16],
                    historical_blob: Some(BlobId::from_bytes([13; 16])),
                    source_digest: [14; 32],
                }),
            ),
        ];
        let changed = Sequence::from_u64(u64::MAX);
        db.execute_batch("BEGIN").unwrap();
        for (key, row) in rows {
            put(&db, ACCOUNT, key, row, changed).unwrap();
            let stored_change = if matches!(key, Key::Blob(_)) {
                Sequence::from_u64(1)
            } else {
                changed
            };
            assert_roundtrip(&db, key, row, stored_change);
        }
        db.execute_batch("COMMIT").unwrap();
        for table in [
            Table::Blobs,
            Table::Mailboxes,
            Table::Emails,
            Table::Memberships,
            Table::Keywords,
            Table::Threads,
            Table::ThreadAnchors,
            Table::Submissions,
            Table::Recipients,
            Table::Leases,
            Table::Imports,
        ] {
            let mut key = [0; 1024];
            let mut value = vec![0; 65536];
            let (kl, vl, _) = next(&db, ACCOUNT, table, None, &mut key, &mut value)
                .unwrap()
                .unwrap();
            format::row::decode_record(table, &key[..kl], &value[..vl]).unwrap();
        }
        let count: i64 = db
            .query_row(
                "SELECT count(*) FROM smtp_receipt_recipients WHERE address='same@example.test'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 2);
        for origin in [
            EmailOrigin::Jmap,
            EmailOrigin::Import,
            EmailOrigin::FailureNotice,
        ] {
            put(&db, ACCOUNT, Key::Email(EMAIL), email(origin), changed).unwrap();
            assert_roundtrip(&db, Key::Email(EMAIL), email(origin), changed);
        }
        assert_eq!(
            db.query_row("SELECT count(*) FROM smtp_receipt_recipients", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        db.execute(
            "INSERT INTO blob_chunks(account,blob,ordinal,body) VALUES(?1,?2,0,x'616263')",
            params![ACCOUNT.as_bytes().as_slice(), MESSAGE.as_bytes().as_slice()],
        )
        .unwrap();
        put(
            &db,
            ACCOUNT,
            Key::Blob(MESSAGE),
            blob(BlobKind::Message),
            changed,
        )
        .unwrap();
        assert_eq!(
            db.query_row(
                "SELECT body FROM blob_chunks WHERE account=?1 AND blob=?2 AND ordinal=0",
                params![ACCOUNT.as_bytes().as_slice(), MESSAGE.as_bytes().as_slice()],
                |r| r.get::<_, Vec<u8>>(0)
            )
            .unwrap(),
            b"abc"
        );
    }
    #[test]
    fn length_prefixed_keys_enumerate_in_logical_field_order() {
        let db = database();
        populate(&db);
        let changed = Sequence::from_u64(2);
        let mut anchors = Vec::new();
        let mut imports = Vec::new();
        for length in [1, 2, 255, 256, 257, 511, 512, 998] {
            let text = "x".repeat(length);
            let key = Key::ThreadAnchor(&text, EMAIL);
            put(&db, ACCOUNT, key, Row::ThreadAnchor, changed).unwrap();
            let mut bytes = [0; 1024];
            let used = key.encode(&mut bytes).unwrap();
            anchors.push(bytes[..used].to_vec());
            for kind in [SourceKind::Email, SourceKind::Mailbox] {
                let key = Key::Import {
                    instance: InstanceId::from_bytes([11; 16]),
                    kind,
                    account: text.as_bytes(),
                    object: b"o",
                };
                let row = Row::Import(ImportRow {
                    local_object: [12; 16],
                    historical_blob: if kind == SourceKind::Email {
                        Some(MESSAGE)
                    } else {
                        None
                    },
                    source_digest: [13; 32],
                });
                put(&db, ACCOUNT, key, row, changed).unwrap();
                let used = key.encode(&mut bytes).unwrap();
                imports.push(bytes[..used].to_vec());
            }
        }
        for (table, mut expected) in [(Table::ThreadAnchors, anchors), (Table::Imports, imports)] {
            expected.sort_by(|a, b| {
                Key::decode(table, a)
                    .unwrap()
                    .compare(Key::decode(table, b).unwrap())
            });
            let mut previous: Option<Vec<u8>> = None;
            for expected in expected {
                let mut key = [0; 1024];
                let mut value = [0; 128];
                let (length, _, _) = next(
                    &db,
                    ACCOUNT,
                    table,
                    previous.as_deref(),
                    &mut key,
                    &mut value,
                )
                .unwrap()
                .unwrap();
                assert_eq!(&key[..length], expected);
                previous = Some(expected);
            }
            assert!(next(
                &db,
                ACCOUNT,
                table,
                previous.as_deref(),
                &mut [0; 1024],
                &mut [0; 128]
            )
            .unwrap()
            .is_none());
        }
    }
    #[test]
    fn imports_seek_the_complete_native_tuple() {
        let db = database();
        let query = format!(
            "EXPLAIN QUERY PLAN {}",
            schema::queries(Table::Imports).next
        );
        let mut statement = db.prepare(&query).unwrap();
        bind_key(
            &mut statement,
            ACCOUNT,
            Key::Import {
                instance: InstanceId::from_bytes([11; 16]),
                kind: SourceKind::Email,
                account: b"source",
                object: b"object",
            },
        )
        .unwrap();
        let mut rows = statement.raw_query();
        let mut complete_seek = false;
        while let Some(row) = rows.next().unwrap() {
            let detail: String = row.get(3).unwrap();
            assert!(!detail.contains("TEMP B-TREE"));
            complete_seek |= detail.contains("PRIMARY KEY")
                && detail.contains(
                    "(source_instance,source_kind,source_account,source_object)>(?,?,?,?)",
                );
        }
        assert!(complete_seek, "import cursor must seek the entire tuple");
    }
    #[test]
    fn same_length_anchors_seek_the_complete_index_key() {
        use std::sync::{
            atomic::{AtomicU64, Ordering},
            Arc,
        };
        let db = database();
        populate(&db);
        let changed = Sequence::from_u64(2);
        db.execute_batch("BEGIN").unwrap();
        for ordinal in 0..3000u32 {
            let mut id = [0; 16];
            id[..4].copy_from_slice(&ordinal.to_be_bytes());
            let id = EmailId::from_bytes(id);
            put(
                &db,
                ACCOUNT,
                Key::Email(id),
                email(EmailOrigin::Jmap),
                changed,
            )
            .unwrap();
            let anchor = format!("{ordinal:08}@example.test");
            put(
                &db,
                ACCOUNT,
                Key::ThreadAnchor(&anchor, id),
                Row::ThreadAnchor,
                changed,
            )
            .unwrap();
        }
        db.execute_batch("COMMIT").unwrap();
        let plan_sql = format!(
            "EXPLAIN QUERY PLAN {}",
            schema::queries(Table::ThreadAnchors).next
        );
        let mut statement = db.prepare(&plan_sql).unwrap();
        bind_key(
            &mut statement,
            ACCOUNT,
            Key::ThreadAnchor("00001500@example.test", EMAIL),
        )
        .unwrap();
        let mut rows = statement.raw_query();
        let mut complete_seek = false;
        while let Some(row) = rows.next().unwrap() {
            let detail: String = row.get(3).unwrap();
            complete_seek |=
                detail.contains("PRIMARY KEY") && detail.contains("(message_id,email_id)>(?,?)");
        }
        assert!(complete_seek, "anchor cursor must seek the entire tuple");
        drop(rows);
        drop(statement);
        let steps = Arc::new(AtomicU64::new(0));
        let counted = Arc::clone(&steps);
        db.progress_handler(
            1,
            Some(move || counted.fetch_add(1, Ordering::Relaxed) >= 600_000),
        )
        .unwrap();
        let mut previous: Option<Vec<u8>> = None;
        let mut count = 0;
        loop {
            let mut key = [0; 1024];
            let mut value = [0; 1];
            let Some((length, _, _)) = next(
                &db,
                ACCOUNT,
                Table::ThreadAnchors,
                previous.as_deref(),
                &mut key,
                &mut value,
            )
            .unwrap() else {
                break;
            };
            if let Some(previous) = previous.as_ref() {
                assert!(Key::decode(Table::ThreadAnchors, previous)
                    .unwrap()
                    .compare(Key::decode(Table::ThreadAnchors, &key[..length]).unwrap())
                    .is_lt());
            }
            previous = Some(key[..length].to_vec());
            count += 1;
        }
        db.progress_handler(0, None::<fn() -> bool>).unwrap();
        assert_eq!(count, 3000);
        assert!(steps.load(Ordering::Relaxed) < 600_000);
    }
    #[test]
    fn chunks_are_bounded_account_scoped_and_deleted_with_the_blob() {
        let db = database();
        populate(&db);
        let account = ACCOUNT.as_bytes().as_slice();
        let blob = UPLOAD.as_bytes().as_slice();
        for (ordinal, body) in [(0, b"abc".as_slice()), (511, b"x".as_slice())] {
            db.execute(
                "INSERT INTO blob_chunks VALUES(?1,?2,?3,?4)",
                params![account, blob, ordinal, body],
            )
            .unwrap();
        }
        assert!(db
            .execute(
                "INSERT INTO blob_chunks VALUES(?1,?2,512,x'00')",
                params![account, blob]
            )
            .is_err());
        assert!(db
            .execute(
                "INSERT INTO blob_chunks VALUES(?1,?2,1,x'')",
                params![account, blob]
            )
            .is_err());
        assert!(db
            .execute(
                "INSERT INTO blob_chunks VALUES(?1,?2,1,zeroblob(65537))",
                params![account, blob]
            )
            .is_err());
        assert!(db
            .execute(
                "INSERT INTO blob_chunks VALUES(?1,?2,0,x'00')",
                params![[42u8; 16].as_slice(), blob]
            )
            .is_err());
        db.execute_batch("BEGIN").unwrap();
        delete(&db, ACCOUNT, Key::Blob(UPLOAD)).unwrap();
        assert_eq!(
            db.query_row("SELECT count(*) FROM blob_chunks", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        db.execute_batch("ROLLBACK").unwrap();
        assert_eq!(
            db.query_row("SELECT count(*) FROM blob_chunks", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            2
        );
        delete(&db, ACCOUNT, Key::Blob(UPLOAD)).unwrap();
        assert_eq!(
            db.query_row("SELECT count(*) FROM blob_chunks", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
    #[test]
    fn native_references_protect_both_targets_and_expired_lease_bytes() {
        let db = database();
        populate(&db);
        let changed = Sequence::from_u64(2);
        put(
            &db,
            ACCOUNT,
            Key::Membership(EMAIL, MAILBOX),
            Row::Membership,
            changed,
        )
        .unwrap();
        put(
            &db,
            ACCOUNT,
            Key::Lease(UPLOAD),
            Row::Lease(LeaseRow {
                account: ACCOUNT,
                device: DeviceId::from_bytes([10; 16]),
                expires_at: -1,
                uses: LeaseUse::Import,
            }),
            changed,
        )
        .unwrap();
        for key in [
            Key::Email(EMAIL),
            Key::Mailbox(MAILBOX),
            Key::Thread(THREAD),
            Key::Blob(MESSAGE),
            Key::Blob(UPLOAD),
        ] {
            db.execute_batch("BEGIN").unwrap();
            delete(&db, ACCOUNT, key).unwrap();
            assert!(db.execute_batch("COMMIT").is_err());
            db.execute_batch("ROLLBACK").unwrap();
        }
        db.execute_batch("BEGIN").unwrap();
        delete(&db, ACCOUNT, Key::Blob(UPLOAD)).unwrap();
        delete(&db, ACCOUNT, Key::Lease(UPLOAD)).unwrap();
        db.execute_batch("COMMIT").unwrap();
        db.execute_batch("BEGIN").unwrap();
        put(
            &db,
            ACCOUNT,
            Key::Email(EMAIL),
            Row::Email(EmailRow {
                blob: BlobId::from_bytes([66; 16]),
                thread: THREAD,
                received_at: 0,
                origin: EmailOrigin::Jmap,
            }),
            changed,
        )
        .unwrap();
        assert!(db.execute_batch("COMMIT").is_err());
        db.execute_batch("ROLLBACK").unwrap();
        assert!(get(
            &db,
            AccountId::from_bytes([42; 16]),
            Key::Email(EMAIL),
            &mut [0; 128]
        )
        .unwrap()
        .is_none());
    }
}
