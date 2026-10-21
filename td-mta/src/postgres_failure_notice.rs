//! Local Inbox notices, serialized with the submission's durable marker.
use crate::{
    admission::work::{Charge, Meter},
    clock::RuntimeClock,
    compose_text::{self, Address, Message},
    ids::{BlobId, EmailId, SubmissionId, ThreadId},
    ports::{self, Clock, Deadline},
    postgres_probe::{self, Request},
    postgres_publication as publication,
    postgres_route_owners::{self as owners, Claim},
};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Instant,
};
use td_crypto::{Crypto, Digest, Entropy, Provider, SystemEntropy, TrustStore};
use tokio_postgres::{Client, Row};

const MAX_TEXT: usize = 10 * 1024 * 1024;
const MAX_MESSAGE: usize = 16 * 1024 * 1024;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Connection(postgres_probe::Error),
    Owner(owners::Error),
    Work(ports::Error),
    Policy,
    Integrity,
    Capacity,
    Quota,
    Database,
    Crypto,
    Indeterminate,
}
fn field<'a, T: tokio_postgres::types::FromSql<'a>>(
    row: &'a Row,
    column: usize,
) -> Result<T, Error> {
    row.try_get(column).map_err(|_| Error::Integrity)
}
fn number(bytes: &[u8]) -> Result<u64, Error> {
    Ok(u64::from_be_bytes(
        bytes.try_into().map_err(|_| Error::Integrity)?,
    ))
}
fn publication_error(error: publication::Error) -> Error {
    match error {
        publication::Error::Quota => Error::Quota,
        publication::Error::Capacity => Error::Capacity,
        publication::Error::Database => Error::Database,
        publication::Error::Deadline => Error::Work(ports::Error::Deadline),
        _ => Error::Integrity,
    }
}
fn append(text: &mut String, value: &str) -> Result<(), Error> {
    if text
        .len()
        .checked_add(value.len())
        .is_none_or(|n| n > MAX_TEXT)
    {
        return Err(Error::Capacity);
    }
    text.try_reserve(value.len()).map_err(|_| Error::Capacity)?;
    text.push_str(value);
    Ok(())
}
fn detail(text: &mut String, label: &str, value: &str, maximum: usize) -> Result<(), Error> {
    if value.len() > maximum {
        return Err(Error::Integrity);
    }
    append(text, label)?;
    // Peer text is body data, never a header or terminal control sequence.
    for c in value.chars() {
        let mut bytes = [0; 4];
        append(
            text,
            if c.is_control() {
                "?"
            } else {
                c.encode_utf8(&mut bytes)
            },
        )?;
    }
    append(text, "\n")
}
fn recipient(text: &mut String, row: &Row, ordinal: usize) -> Result<(), Error> {
    if field::<i64>(row, 0)? != ordinal as i64 || field::<Option<i64>>(row, 4)?.is_some() {
        return Err(Error::Integrity);
    }
    render_recipient(
        text,
        Recipient {
            address: field(row, 1)?,
            state: field(row, 2)?,
            uncertain: field(row, 3)?,
            rcpt: field(row, 5)?,
            data: field(row, 6)?,
            reason: field(row, 7)?,
            diagnostic: field(row, 8)?,
        },
    )
}
struct Recipient<'a> {
    address: &'a str,
    state: i64,
    uncertain: i64,
    rcpt: Option<&'a str>,
    data: Option<&'a str>,
    reason: i64,
    diagnostic: &'a str,
}
fn render_recipient(text: &mut String, row: Recipient<'_>) -> Result<(), Error> {
    let status = match (row.state, row.uncertain) {
        (4, 0 | 1) => "Accepted by the relay; final mailbox delivery is not confirmed.\n",
        (5, 0) => "Failed; no further delivery attempts are scheduled.\n",
        (6, 0) => "Canceled.\n",
        (7, 1) => "Delivery outcome unknown; the recipient may have received this message.\n",
        _ => return Err(Error::Integrity),
    };
    detail(text, "\nRecipient: ", row.address, 254)?;
    append(text, status)?;
    let reason = match row.reason {
        0 => "None",
        1 => "Temporary SMTP refusal",
        2 => "Permanent SMTP refusal",
        3 => "Network failure",
        4 => "TLS failure",
        5 => "Authentication failure",
        6 => "Delivery deadline expired",
        7 => "Canceled",
        8 => "Protocol failure",
        9 => "Uncertain delivery",
        10 => "Restored backup; delivery uncertain; resend disabled",
        _ => return Err(Error::Integrity),
    };
    detail(text, "Reason: ", reason, 64)?;
    for (reply, label) in [(row.rcpt, "RCPT reply: "), (row.data, "Message reply: ")] {
        if let Some(reply) = reply {
            detail(text, label, reply, 4096)?;
        }
    }
    detail(text, "Diagnostic: ", row.diagnostic, 512)
}

/// Publish at most one notice. A retry takes the same account lock before
/// selecting Pending again: Stored is the receipt, even if its Email was deleted.
/// A lost COMMIT response needs no replay token or generated-ID retention because
/// selecting work and changing its marker occur in the same locked transaction.
pub fn publish_next(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    claim: &Claim,
    generation: u64,
) -> Result<bool, Error> {
    let clock = RuntimeClock::new();
    let left = deadline
        .checked_duration_since(Instant::now())
        .ok_or(Error::Work(ports::Error::Deadline))?;
    if left.is_zero() || left > postgres_probe::MAX_DURATION {
        return Err(Error::Work(ports::Error::Deadline));
    }
    let until = Deadline::after(
        clock.sample().map_err(Error::Work)?.monotonic,
        left.as_millis() as u64,
    )
    .map_err(Error::Work)?;
    let mut meter = Meter::new(
        until,
        Charge {
            io_bytes: 128 * 1024 * 1024,
            output_bytes: 64 * 1024 * 1024,
            records: 2_000_000,
            ..Charge::default()
        },
    );
    let claim = claim.clone();
    let attempted = Arc::new(AtomicBool::new(false));
    let marker = attempted.clone();
    let result = postgres_probe::with_notice_client(request, trust, deadline, move |client| {
        Box::pin(async move {
            if !owners::begin(client, &claim, true, true)
                .await
                .map_err(Error::Owner)?
            {
                return Err(Error::Policy);
            }
            let now = owners::check_locked(client, &claim, generation)
                .await
                .map_err(Error::Owner)?;
            let binding = claim.binding();
            let account_id = binding.account();
            let account = account_id.as_bytes().as_slice();
            let rows = client.query("SELECT id,email_id,recipient_count FROM td_mta.submissions WHERE account=$1 AND notification=1 AND completed_at IS NOT NULL ORDER BY id LIMIT 1", &[&account]).await.map_err(|_| Error::Database)?;
            let Some(row) = rows.first() else {
                return Ok(false);
            };
            let submission = SubmissionId::from_bytes(
                field::<&[u8]>(row, 0)?
                    .try_into()
                    .map_err(|_| Error::Integrity)?,
            );
            let original = EmailId::from_bytes(
                field::<&[u8]>(row, 1)?
                    .try_into()
                    .map_err(|_| Error::Integrity)?,
            );
            let count: i64 = field(row, 2)?;
            if !(1..=1000).contains(&count) {
                return Err(Error::Integrity);
            }
            let state = client
                .query_one(
                    "SELECT sequence FROM td_mta.accounts WHERE id=$1",
                    &[&account],
                )
                .await
                .map_err(|_| Error::Database)?;
            let old = number(field(&state, 0)?)?;
            let next = crate::postgres_sequence::next(client, account, old)
                .await
                .map_err(|error| match error {
                    crate::postgres_sequence::Error::Capacity => Error::Capacity,
                    crate::postgres_sequence::Error::Integrity => Error::Integrity,
                    crate::postgres_sequence::Error::Database => Error::Database,
                })?;
            let boxes = client.query("SELECT id,changed FROM td_mta.mailboxes WHERE account=$1 AND role='inbox' ORDER BY id LIMIT 2", &[&account]).await.map_err(|_| Error::Database)?;
            if boxes.len() != 1 {
                return Err(Error::Integrity);
            }
            let inbox = boxes.first().ok_or(Error::Integrity)?;
            let mailbox: &[u8] = field(inbox, 0)?;
            if mailbox.len() != 16 || number(field(inbox, 1)?)? > old {
                return Err(Error::Integrity);
            }
            let mut text = String::new();
            append(&mut text, &format!("Local delivery report\nSubmission: {submission}\nOriginal Email: {original}\n\nNo further attempts are scheduled for this submission.\nAn unknown outcome is not proof of failure; resending may deliver a duplicate.\n"))?;
            for start in (0..count).step_by(100) {
                let rows = client.query("SELECT ordinal,address,state,uncertain,next_attempt_at,rcpt_reply,data_reply,reason,diagnostic,changed FROM td_mta.recipients WHERE account=$1 AND submission_id=$2 AND ordinal >= $3 ORDER BY ordinal LIMIT 100", &[&account,&submission.as_bytes().as_slice(),&start]).await.map_err(|_| Error::Database)?;
                if rows.len() != (count - start).min(100) as usize {
                    return Err(Error::Integrity);
                }
                for (offset, row) in rows.iter().enumerate() {
                    if Instant::now() >= deadline {
                        return Err(Error::Work(ports::Error::Deadline));
                    }
                    if number(field(row, 9)?)? > old {
                        return Err(Error::Integrity);
                    }
                    meter
                        .charge(
                            clock.sample().map_err(Error::Work)?.monotonic,
                            Charge {
                                io_bytes: 10 * 1024,
                                output_bytes: 10 * 1024,
                                records: 1,
                                ..Charge::default()
                            },
                        )
                        .map_err(|stop| {
                            Error::Work(if stop == crate::admission::work::Stop::Deadline {
                                ports::Error::Deadline
                            } else {
                                ports::Error::Capacity
                            })
                        })?;
                    recipient(&mut text, row, start as usize + offset)?;
                }
            }
            let extra: bool = field(&client.query_one("SELECT EXISTS(SELECT 1 FROM td_mta.recipients WHERE account=$1 AND submission_id=$2 AND ordinal >= $3) OR EXISTS(SELECT 1 FROM td_mta.submission_attempts WHERE account=$1 AND submission_id=$2 AND active)", &[&account,&submission.as_bytes().as_slice(),&count]).await.map_err(|_| Error::Database)?,0)?;
            if extra {
                return Err(Error::Integrity);
            }
            let mut entropy = SystemEntropy::try_new().map_err(|_| Error::Crypto)?;
            let mut id = || {
                let mut bytes = [0; 16];
                entropy.fill(&mut bytes).map_err(|_| Error::Crypto)?;
                Ok::<_, Error>(bytes)
            };
            let email = EmailId::from_bytes(id()?);
            let blob = BlobId::from_bytes(id()?);
            let thread = ThreadId::from_bytes(id()?);
            let message_id = format!("notice-{email}@localhost.invalid");
            let mut bytes = Vec::new();
            compose_text::compose(
                &Message {
                    from: Address {
                        name: "Mail Delivery System",
                        email: "postmaster@localhost.invalid",
                    },
                    to: &[],
                    cc: &[],
                    bcc: &[],
                    reply_to: &[],
                    subject: "Delivery status report",
                    message_id: &message_id,
                    in_reply_to: &[],
                    references: &[],
                    sent_at_ms: now,
                    text: &text,
                },
                MAX_MESSAGE as u64,
                &clock,
                &mut meter,
                |chunk| {
                    bytes
                        .try_reserve(chunk.len())
                        .map_err(|_| ports::Error::Capacity)?;
                    bytes.extend_from_slice(chunk);
                    Ok(())
                },
            )
            .map_err(Error::Work)?;
            let mut hash = Provider.sha256().map_err(|_| Error::Crypto)?;
            hash.update(&bytes).map_err(|_| Error::Crypto)?;
            let digest = hash.finish().map_err(|_| Error::Crypto)?;
            let usage = publication::charge(
                client,
                account_id,
                binding.quotas(),
                [bytes.len() as u64, 1, 0, 0, 0],
            )
            .await
            .map_err(publication_error)?;
            let used: bool = field(&client.query_one("SELECT EXISTS(SELECT 1 FROM td_mta.emails WHERE account=$1 AND id=$2) OR EXISTS(SELECT 1 FROM td_mta.threads WHERE account=$1 AND id=$3) OR EXISTS(SELECT 1 FROM td_mta.changes WHERE account=$1 AND ((kind=3 AND object=$2) OR (kind=2 AND object=$3)))", &[&account,&email.as_bytes().as_slice(),&thread.as_bytes().as_slice()]).await.map_err(|_| Error::Database)?,0)?;
            if used {
                return Err(Error::Integrity);
            }
            publication::write_blob(
                client,
                publication::Blob {
                    account: account_id,
                    id: blob,
                    bytes: &bytes,
                    digest: &digest,
                    created_at: now,
                    changed: next,
                },
                deadline,
            )
            .await
            .map_err(publication_error)?;
            let changed = next.to_be_bytes();
            let changed = changed.as_slice();
            let email = email.as_bytes().as_slice();
            let thread = thread.as_bytes().as_slice();
            execute(
                client,
                "INSERT INTO td_mta.threads VALUES($1,$2,$3)",
                &[&account, &thread, &changed],
                1,
            )
            .await?;
            execute(client,"INSERT INTO td_mta.emails(account,id,blob_id,thread_id,received_at,origin,changed) VALUES($1,$2,$3,$4,$5,4,$6)", &[&account,&email,&blob.as_bytes().as_slice(),&thread,&now,&changed],1).await?;
            execute(
                client,
                "INSERT INTO td_mta.memberships VALUES($1,$2,$3,$4)",
                &[&account, &email, &mailbox, &changed],
                1,
            )
            .await?;
            execute(
                client,
                "INSERT INTO td_mta.thread_anchors VALUES($1,$2,$3,$4)",
                &[&account, &message_id, &email, &changed],
                1,
            )
            .await?;
            execute(
                client,
                "UPDATE td_mta.mailboxes SET changed=$3 WHERE account=$1 AND id=$2",
                &[&account, &mailbox, &changed],
                1,
            )
            .await?;
            execute(client,"UPDATE td_mta.submissions SET notification=2,notification_email_id=$3,changed=$4 WHERE account=$1 AND id=$2 AND notification=1 AND completed_at IS NOT NULL", &[&account,&submission.as_bytes().as_slice(),&email,&changed],1).await?;
            execute(client,"INSERT INTO td_mta.changes VALUES($1,$2,0,3,1,$3),($1,$2,1,2,1,$4),($1,$2,2,1,2,$5),($1,$2,3,5,2,$6)", &[&account,&changed,&email,&thread,&mailbox,&submission.as_bytes().as_slice()],4).await?;
            usage
                .publish(client, account_id)
                .await
                .map_err(publication_error)?;
            execute(
                client,
                "UPDATE td_mta.accounts SET sequence=$2 WHERE id=$1",
                &[&account, &changed],
                1,
            )
            .await?;
            owners::check_locked(client, &claim, generation)
                .await
                .map_err(Error::Owner)?;
            marker.store(true, Ordering::Relaxed);
            client
                .batch_execute("COMMIT")
                .await
                .map_err(|_| Error::Indeterminate)?;
            Ok(true)
        })
    });
    match result {
        Ok(value) => value,
        Err(_) if attempted.load(Ordering::Relaxed) => Err(Error::Indeterminate),
        Err(error) => Err(Error::Connection(error)),
    }
}
async fn execute(
    client: &Client,
    sql: &str,
    parameters: &[&(dyn tokio_postgres::types::ToSql + Sync)],
    count: u64,
) -> Result<(), Error> {
    if client
        .execute(sql, parameters)
        .await
        .map_err(|_| Error::Database)?
        != count
    {
        return Err(Error::Integrity);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    fn row() -> Recipient<'static> {
        Recipient {
            address: "user@example.test",
            state: 5,
            uncertain: 0,
            rcpt: None,
            data: Some("550 refused"),
            reason: 2,
            diagnostic: "remote refusal",
        }
    }
    #[test]
    fn restored_notice_renders_without_an_smtp_reply() {
        let mut text = String::new();
        render_recipient(
            &mut text,
            Recipient {
                state: 7,
                uncertain: 1,
                reason: 10,
                data: None,
                diagnostic: "Restored backup",
                ..row()
            },
        )
        .unwrap();
        assert!(text.contains("may have received"));
        assert!(text.contains("resend disabled"));
        assert!(!text.contains("Failed;"));
    }
    #[test]
    fn reports_unknown_without_claiming_failure_and_keeps_peer_text_in_body() {
        let mut text = String::new();
        render_recipient(
            &mut text,
            Recipient {
                state: 7,
                uncertain: 1,
                reason: 6,
                data: Some("550 café\r\nBcc: other@example.test\u{1b}[31m"),
                ..row()
            },
        )
        .unwrap();
        assert!(text.contains("may have received"));
        assert!(text.contains("Delivery deadline expired"));
        assert!(text.contains("550 café??Bcc: other@example.test?[31m"));
        assert!(!text.contains("Failed;"));
        for (state, uncertain) in [(1, 0), (2, 0), (3, 0), (5, 1), (6, 1), (7, 0), (8, 0)] {
            assert_eq!(
                render_recipient(
                    &mut String::new(),
                    Recipient {
                        state,
                        uncertain,
                        ..row()
                    }
                ),
                Err(Error::Integrity)
            );
        }
    }
    #[test]
    fn all_thousand_maximum_recipient_details_fit_without_truncation() {
        let reply = "x".repeat(4096);
        let address = "a".repeat(254);
        let diagnostic = "d".repeat(512);
        let mut text = String::new();
        for _ in 0..1000 {
            render_recipient(
                &mut text,
                Recipient {
                    address: &address,
                    rcpt: Some(&reply),
                    data: Some(&reply),
                    diagnostic: &diagnostic,
                    ..row()
                },
            )
            .unwrap();
        }
        assert!(text.len() < MAX_TEXT);
        assert!(text.len().div_ceil(3) * 4 + text.len().div_ceil(57) * 2 + 4096 < MAX_MESSAGE);
        assert_eq!(text.matches("Recipient: ").count(), 1000);
        assert_eq!(text.matches(&reply).count(), 2000);
        let too_long = "x".repeat(4097);
        assert_eq!(
            render_recipient(
                &mut String::new(),
                Recipient {
                    data: Some(&too_long),
                    ..row()
                }
            ),
            Err(Error::Integrity)
        );
    }
}
