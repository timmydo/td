//! Local Inbox notices, serialized with the submission's durable marker.
use crate::postgres_probe::{Client, Row};
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
use td_crypto::{Crypto, Digest, Entropy, Provider, SystemEntropy};

const MAX_TEXT: usize = 10 * 1024 * 1024;
const RECIPIENT_PAGE: &str = "SELECT ordinal,address,state,uncertain,next_attempt_at,rcpt_reply,data_reply,reason,diagnostic,changed FROM td_mta.recipients WHERE account=$1 AND submission_id=$2 AND ordinal >= $3 ORDER BY ordinal LIMIT 100";
const MAX_MESSAGE: usize = 16 * 1024 * 1024;
/// A compact notice's text and message stay this small, so storing one past
/// the account's quota overshoots it by a fixed amount.
const COMPACT_TEXT: usize = 4 * 1024;
const COMPACT_MESSAGE: usize = 16 * 1024;
/// Failed or unknown recipients a compact notice names.
const COMPACT_ADDRESSES: i64 = 8;
/// Older completed submissions get a compact notice even when a full one
/// would fit, so a full report that keeps failing still ends in a notice. A
/// day outlasts a maximal burst (10,000 notices at one per five seconds) and
/// ordinary relay downtime.
const FULL_WINDOW_MS: i64 = 24 * 3_600_000;
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
fn field<'a, T: crate::postgres_probe::FromColumn<'a>>(
    row: Row<'a>,
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
    if field::<i64>(*row, 0)? != ordinal as i64 || field::<Option<i64>>(*row, 4)?.is_some() {
        return Err(Error::Integrity);
    }
    render_recipient(
        text,
        Recipient {
            address: field(*row, 1)?,
            state: field(*row, 2)?,
            uncertain: field(*row, 3)?,
            rcpt: field(*row, 5)?,
            data: field(*row, 6)?,
            reason: field(*row, 7)?,
            diagnostic: field(*row, 8)?,
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
fn reason_text(reason: i64) -> Result<&'static str, Error> {
    Ok(match reason {
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
    })
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
    detail(text, "Reason: ", reason_text(row.reason)?, 64)?;
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
    let result = postgres_probe::with_notice_client(request, deadline, move |client| {
        if !owners::begin(client, &claim, true, true).map_err(Error::Owner)? {
            return Err(Error::Policy);
        }
        let now = owners::check_locked(client, &claim, generation).map_err(Error::Owner)?;
        let binding = claim.binding();
        let account_id = binding.account();
        let account = account_id.as_bytes().as_slice();
        // Random order: one notice that keeps failing never blocks the rest.
        let rows = client.query("SELECT id,email_id,recipient_count,completed_at FROM td_mta.submissions WHERE account=$1 AND notification=1 AND completed_at IS NOT NULL ORDER BY random() LIMIT 1", &[&account]).map_err(|_| Error::Database)?;
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
        let completed_at: i64 = field(row, 3)?;
        let state = client
            .query_one(
                "SELECT sequence FROM td_mta.accounts WHERE id=$1",
                &[&account],
            )
            .map_err(|_| Error::Database)?;
        let old = number(field(state.row(), 0)?)?;
        let next =
            crate::postgres_sequence::next(client, account, old).map_err(|error| match error {
                crate::postgres_sequence::Error::Capacity => Error::Capacity,
                crate::postgres_sequence::Error::Integrity => Error::Integrity,
                crate::postgres_sequence::Error::Database => Error::Database,
            })?;
        let mailbox = inbox(client, account, old)?;
        let heading = format!("Local delivery report\nSubmission: {submission}\nOriginal Email: {original}\n\nNo further attempts are scheduled for this submission.\nAn unknown outcome is not proof of failure; resending may deliver a duplicate.\n");
        // A recent notice reports every recipient in full; an older one, or
        // one its size or the account's quota refuses, is stored compact.
        let mut text = String::new();
        let mut full = now.saturating_sub(completed_at) <= FULL_WINDOW_MS;
        if full {
            append(&mut text, &heading)?;
            let built = (|| {
                for start in (0..count).step_by(100) {
                    let rows = client
                        .query(
                            RECIPIENT_PAGE,
                            &[&account, &submission.as_bytes().as_slice(), &start],
                        )
                        .map_err(|_| Error::Database)?;
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
                        recipient(&mut text, &row, start as usize + offset)?;
                    }
                }
                Ok::<(), Error>(())
            })();
            match built {
                Ok(()) => {}
                Err(error) if fallback(error) => full = false,
                Err(error) => return Err(error),
            }
        }
        let extra: bool = field((client.query_one("SELECT EXISTS(SELECT 1 FROM td_mta.recipients WHERE account=$1 AND submission_id=$2 AND ordinal >= $3) OR EXISTS(SELECT 1 FROM td_mta.submission_attempts WHERE account=$1 AND submission_id=$2 AND active)", &[&account,&submission.as_bytes().as_slice(),&count]).map_err(|_| Error::Database)?).row(),0)?;
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
        let mut prepared = None;
        if full {
            match render(&text, &message_id, now, MAX_MESSAGE, &clock, &mut meter) {
                // Charge first: a full account must not pay for hashing and
                // MIME evidence of a report it will not store.
                Ok(bytes) => match publication::charge(
                    client,
                    account_id,
                    binding.quotas(),
                    [bytes.len() as u64, 1, 0, 0, 0],
                ) {
                    Ok(usage) => {
                        let (digest, interpretation) = seal(&bytes, deadline)?;
                        prepared = Some((bytes, digest, interpretation, usage));
                    }
                    Err(publication::Error::Quota) => {}
                    Err(error) => return Err(publication_error(error)),
                },
                Err(error) if fallback(error) => {}
                Err(error) => return Err(error),
            }
        }
        let (bytes, digest, interpretation, usage) = match prepared {
            Some(prepared) => prepared,
            None => {
                let compact = compact_text(client, account, submission, &heading, count, old)?;
                let bytes = render(
                    &compact,
                    &message_id,
                    now,
                    COMPACT_MESSAGE,
                    &clock,
                    &mut meter,
                )?;
                let (digest, interpretation) = seal(&bytes, deadline)?;
                let usage = publication::charge(
                    client,
                    account_id,
                    compact_caps(binding.quotas()),
                    [bytes.len() as u64, 1, 0, 0, 0],
                )
                .map_err(publication_error)?;
                (bytes, digest, interpretation, usage)
            }
        };
        write_notice(
            client,
            Notice {
                account: account_id,
                mailbox: &mailbox,
                email,
                blob,
                thread,
                message_id: &message_id,
                bytes: &bytes,
                digest: &digest,
                interpretation: interpretation.as_ref(),
                now,
                next,
            },
            usage,
            deadline,
        )?;
        let changed = next.to_be_bytes();
        let changed = changed.as_slice();
        let email = email.as_bytes().as_slice();
        let submission = submission.as_bytes().as_slice();
        execute(client,"UPDATE td_mta.submissions SET notification=2,notification_email_id=$3,changed=$4 WHERE account=$1 AND id=$2 AND notification=1 AND completed_at IS NOT NULL", &[&account,&submission,&email,&changed],1)?;
        execute(
            client,
            "INSERT INTO td_mta.changes VALUES($1,$2,3,5,2,$3)",
            &[&account, &changed, &submission],
            1,
        )?;
        owners::check_locked(client, &claim, generation).map_err(Error::Owner)?;
        marker.store(true, Ordering::Relaxed);
        client.commit().map_err(|_| Error::Indeterminate)?;
        Ok(true)
    });
    match result {
        Ok(value) => value,
        Err(_) if attempted.load(Ordering::Relaxed) => Err(Error::Indeterminate),
        Err(error) => Err(Error::Connection(error)),
    }
}
fn inbox(client: &Client, account: &[u8], old: u64) -> Result<[u8; 16], Error> {
    let boxes = client.query("SELECT id,changed FROM td_mta.mailboxes WHERE account=$1 AND role='inbox' ORDER BY id LIMIT 2", &[&account]).map_err(|_| Error::Database)?;
    if boxes.len() != 1 {
        return Err(Error::Integrity);
    }
    let inbox = boxes.first().ok_or(Error::Integrity)?;
    let mailbox: &[u8] = field(inbox, 0)?;
    if number(field(inbox, 1)?)? > old {
        return Err(Error::Integrity);
    }
    mailbox.try_into().map_err(|_| Error::Integrity)
}

struct Notice<'a> {
    account: crate::ids::AccountId,
    mailbox: &'a [u8; 16],
    email: EmailId,
    blob: BlobId,
    thread: ThreadId,
    message_id: &'a str,
    bytes: &'a [u8],
    digest: &'a [u8; 32],
    interpretation: Option<&'a crate::postgres_mime_evidence::Prepared>,
    now: i64,
    next: u64,
}

/// One unread Inbox Email with its blob, fresh thread, anchor, history and
/// quota charge, in the caller's transaction holding the account row lock.
fn write_notice(
    client: &Client,
    notice: Notice<'_>,
    usage: publication::Usage,
    deadline: Instant,
) -> Result<(), Error> {
    let account = notice.account.as_bytes().as_slice();
    let used: bool = field((client.query_one("SELECT EXISTS(SELECT 1 FROM td_mta.emails WHERE account=$1 AND id=$2) OR EXISTS(SELECT 1 FROM td_mta.threads WHERE account=$1 AND id=$3) OR EXISTS(SELECT 1 FROM td_mta.changes WHERE account=$1 AND ((kind=3 AND object=$2) OR (kind=2 AND object=$3)))", &[&account,&notice.email.as_bytes().as_slice(),&notice.thread.as_bytes().as_slice()]).map_err(|_| Error::Database)?).row(),0)?;
    if used {
        return Err(Error::Integrity);
    }
    publication::write_blob(
        client,
        publication::Blob {
            account: notice.account,
            id: notice.blob,
            bytes: notice.bytes,
            digest: notice.digest,
            interpretation: notice.interpretation,
            created_at: notice.now,
            changed: notice.next,
        },
        deadline,
    )
    .map_err(publication_error)?;
    let changed = notice.next.to_be_bytes();
    let changed = changed.as_slice();
    let email = notice.email.as_bytes().as_slice();
    let thread = notice.thread.as_bytes().as_slice();
    let mailbox = notice.mailbox.as_slice();
    execute(
        client,
        "INSERT INTO td_mta.threads VALUES($1,$2,$3)",
        &[&account, &thread, &changed],
        1,
    )?;
    execute(client,"INSERT INTO td_mta.emails(account,id,blob_id,thread_id,received_at,origin,changed) VALUES($1,$2,$3,$4,$5,4,$6)", &[&account,&email,&notice.blob.as_bytes().as_slice(),&thread,&notice.now,&changed],1)?;
    execute(
        client,
        "INSERT INTO td_mta.memberships VALUES($1,$2,$3,$4)",
        &[&account, &email, &mailbox, &changed],
        1,
    )?;
    execute(
        client,
        "INSERT INTO td_mta.thread_anchors VALUES($1,$2,$3,$4)",
        &[&account, &notice.message_id, &email, &changed],
        1,
    )?;
    execute(
        client,
        "UPDATE td_mta.mailboxes SET changed=$3 WHERE account=$1 AND id=$2",
        &[&account, &mailbox, &changed],
        1,
    )?;
    execute(
        client,
        "INSERT INTO td_mta.changes VALUES($1,$2,0,3,1,$3),($1,$2,1,2,1,$4),($1,$2,2,1,2,$5)",
        &[&account, &changed, &email, &thread, &mailbox],
        3,
    )?;
    usage
        .publish(client, notice.account)
        .map_err(publication_error)?;
    execute(
        client,
        "UPDATE td_mta.accounts SET sequence=$2 WHERE id=$1",
        &[&account, &changed],
        1,
    )
}

/// Facts of an incoming message an account's quota refused at DATA.
pub(crate) struct Refused<'a> {
    pub binding: &'a crate::postgres_runtime_profile::Binding,
    /// The unverified SMTP reverse path; empty for a null one.
    pub reverse_path: &'a str,
    /// This account's accepted RCPT spellings.
    pub recipients: &'a [String],
    pub length: u64,
    /// The account sequence observed under the caller's row lock.
    pub old: u64,
}
/// Recipient addresses a refused-delivery notice names.
const REFUSED_ADDRESSES: usize = 8;
const REFUSED_HEADING: &str = "Local delivery report\n\nA message to this account was not stored: the account's mail storage\nquota is full. Other local recipients of the same delivery received it,\nso the sending server was told it was delivered and will normally not\nsend it again. Delete mail to make room for new messages.\n\n";

fn refused_text(reverse_path: &str, recipients: &[String], length: u64) -> Result<String, Error> {
    let mut text = String::new();
    append(&mut text, REFUSED_HEADING)?;
    let sender = if reverse_path.is_empty() {
        "(none)"
    } else {
        reverse_path
    };
    detail(&mut text, "Sender (unverified): ", sender, 254)?;
    let mut buffer = [0; 20];
    append(&mut text, "Size: ")?;
    append(&mut text, decimal(length, &mut buffer))?;
    append(&mut text, " bytes\n")?;
    for address in recipients.iter().take(REFUSED_ADDRESSES) {
        detail(&mut text, "Recipient: ", address, 254)?;
    }
    if let Some(more) = recipients.len().checked_sub(REFUSED_ADDRESSES) {
        if more > 0 {
            append(&mut text, "Further recipients: ")?;
            append(&mut text, decimal(more as u64, &mut buffer))?;
            append(&mut text, "\n")?;
        }
    }
    if text.len() > COMPACT_TEXT {
        return Err(Error::Integrity);
    }
    Ok(text)
}

/// Store a compact notice in an account whose quota refused an incoming
/// message that another local account of the same DATA transaction received.
/// The caller holds the selection and this account's row lock, and commits.
/// The notice may pass the caps by one compact message; past that this
/// refuses with Quota.
pub(crate) fn store_refused(
    client: &Client,
    refused: Refused<'_>,
    deadline: Instant,
) -> Result<(EmailId, u64), Error> {
    let text = refused_text(refused.reverse_path, refused.recipients, refused.length)?;
    let account_id = refused.binding.account();
    let account = account_id.as_bytes().as_slice();
    let next =
        crate::postgres_sequence::next(client, account, refused.old).map_err(
            |error| match error {
                crate::postgres_sequence::Error::Capacity => Error::Capacity,
                crate::postgres_sequence::Error::Integrity => Error::Integrity,
                crate::postgres_sequence::Error::Database => Error::Database,
            },
        )?;
    let now: i64 = field(
        (client
            .query_one(
                "SELECT (extract(epoch FROM clock_timestamp())*1000)::bigint",
                &[],
            )
            .map_err(|_| Error::Database)?)
        .row(),
        0,
    )?;
    let mailbox = inbox(client, account, refused.old)?;
    let mut entropy = SystemEntropy::try_new().map_err(|_| Error::Crypto)?;
    let mut ids = [[0; 16]; 3];
    for id in &mut ids {
        entropy.fill(id).map_err(|_| Error::Crypto)?;
    }
    let [email, blob, thread] = ids;
    let (email, blob, thread) = (
        EmailId::from_bytes(email),
        BlobId::from_bytes(blob),
        ThreadId::from_bytes(thread),
    );
    let message_id = td_alloc::format(format_args!("notice-{email}@localhost.invalid"))
        .map_err(|_| Error::Capacity)?;
    let clock = RuntimeClock::new();
    let left = deadline
        .checked_duration_since(Instant::now())
        .filter(|left| !left.is_zero())
        .ok_or(Error::Work(ports::Error::Deadline))?;
    let until = Deadline::after(
        clock.sample().map_err(Error::Work)?.monotonic,
        u64::try_from(left.as_millis()).map_err(|_| Error::Capacity)?,
    )
    .map_err(Error::Work)?;
    let mut meter = Meter::new(
        until,
        Charge {
            io_bytes: 1024 * 1024,
            output_bytes: 1024 * 1024,
            records: 64 * 1024,
            ..Charge::default()
        },
    );
    let bytes = render(&text, &message_id, now, COMPACT_MESSAGE, &clock, &mut meter)?;
    let usage = publication::charge(
        client,
        account_id,
        incoming_caps(refused.binding.quotas()),
        [bytes.len() as u64, 1, 0, 0, 0],
    )
    .map_err(publication_error)?;
    let (digest, interpretation) = seal(&bytes, deadline)?;
    write_notice(
        client,
        Notice {
            account: account_id,
            mailbox: &mailbox,
            email,
            blob,
            thread,
            message_id: &message_id,
            bytes: &bytes,
            digest: &digest,
            interpretation: interpretation.as_ref(),
            now,
            next,
        },
        usage,
        deadline,
    )?;
    Ok((email, next))
}

/// A full account still stores compact submission notices, past its
/// body/blob caps by one per configured queued submission beyond the one
/// incoming notice `incoming_caps` admits (POSTGRES.md, "Compact failure
/// notices and the quota margin").
fn compact_caps(
    caps: crate::postgres_configuration::SharedQuotas,
) -> crate::postgres_configuration::SharedQuotas {
    margin(caps, caps.queue_submissions.saturating_add(1))
}
/// A notice replacing refused incoming mail passes the caps by at most one
/// compact message, so it never takes a queued submission's share.
fn incoming_caps(
    caps: crate::postgres_configuration::SharedQuotas,
) -> crate::postgres_configuration::SharedQuotas {
    margin(caps, 1)
}
fn margin(
    caps: crate::postgres_configuration::SharedQuotas,
    notices: u64,
) -> crate::postgres_configuration::SharedQuotas {
    crate::postgres_configuration::SharedQuotas {
        body_bytes: caps
            .body_bytes
            .saturating_add(notices.saturating_mul(COMPACT_MESSAGE as u64)),
        blob_count: caps.blob_count.saturating_add(notices),
        ..caps
    }
}

/// Whether the full report's size, rather than a fault, refused it.
fn fallback(error: Error) -> bool {
    matches!(
        error,
        Error::Capacity | Error::Work(ports::Error::Capacity | ports::Error::Quota)
    )
}

fn render(
    text: &str,
    message_id: &str,
    now: i64,
    maximum: usize,
    clock: &RuntimeClock,
    meter: &mut Meter,
) -> Result<Vec<u8>, Error> {
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
            message_id,
            in_reply_to: &[],
            references: &[],
            sent_at_ms: now,
            text,
        },
        maximum as u64,
        clock,
        meter,
        |chunk| {
            bytes
                .try_reserve(chunk.len())
                .map_err(|_| ports::Error::Capacity)?;
            bytes.extend_from_slice(chunk);
            Ok(())
        },
    )
    .map_err(Error::Work)?;
    Ok(bytes)
}

fn seal(
    bytes: &[u8],
    deadline: Instant,
) -> Result<([u8; 32], Option<crate::postgres_mime_evidence::Prepared>), Error> {
    let mut hash = Provider.sha256().map_err(|_| Error::Crypto)?;
    hash.update(bytes).map_err(|_| Error::Crypto)?;
    let digest = hash.finish().map_err(|_| Error::Crypto)?;
    let interpretation = crate::postgres_mime_evidence::prepare(bytes, &digest, deadline);
    Ok((digest, interpretation))
}

fn decimal(mut value: u64, buffer: &mut [u8; 20]) -> &str {
    let mut start = buffer.len();
    loop {
        start -= 1;
        if let Some(digit) = buffer.get_mut(start) {
            *digit = b'0' + (value % 10) as u8;
        }
        value /= 10;
        if value == 0 || start == 0 {
            break;
        }
    }
    buffer
        .get(start..)
        .and_then(|digits| std::str::from_utf8(digits).ok())
        .unwrap_or("0")
}

fn outcome(state: i64, uncertain: i64) -> Result<&'static str, Error> {
    Ok(match (state, uncertain) {
        (4, 0) => "Accepted by the relay: ",
        (4, 1) => "Accepted by the relay, outcome uncertain: ",
        (5, 0) => "Failed: ",
        (6, 0) => "Canceled: ",
        (7, 1) => "Delivery outcome unknown: ",
        _ => return Err(Error::Integrity),
    })
}

const COMPACT_INTRO: &str = "\nThis compact report omits per-recipient replies. The submission's\ndelivery status keeps them until the submission is retired, which may\nalready have happened.\n\n";

fn count_line(text: &mut String, state: i64, uncertain: i64, number: i64) -> Result<(), Error> {
    append(text, outcome(state, uncertain)?)?;
    let mut buffer = [0; 20];
    let number = u64::try_from(number).map_err(|_| Error::Integrity)?;
    append(text, decimal(number, &mut buffer))?;
    append(text, "\n")
}

/// Counts by outcome and the first Failed or unknown recipients: never more
/// than `COMPACT_TEXT`, however many recipients or how long their replies.
fn compact_text(
    client: &Client,
    account: &[u8],
    submission: SubmissionId,
    heading: &str,
    count: i64,
    old: u64,
) -> Result<String, Error> {
    let submission = submission.as_bytes().as_slice();
    // The checks the full report's pages make on every recipient.
    let old = old.to_be_bytes();
    let faulty: i64 = field((client.query_one("SELECT count(*) FROM td_mta.recipients WHERE account=$1 AND submission_id=$2 AND (next_attempt_at IS NOT NULL OR reason NOT BETWEEN 0 AND 10 OR changed > $3)", &[&account, &submission, &old.as_slice()]).map_err(|_| Error::Database)?).row(), 0)?;
    if faulty != 0 {
        return Err(Error::Integrity);
    }
    let mut text = String::new();
    append(&mut text, heading)?;
    append(&mut text, COMPACT_INTRO)?;
    let groups = client.query("SELECT state,uncertain,count(*) FROM td_mta.recipients WHERE account=$1 AND submission_id=$2 GROUP BY state,uncertain ORDER BY state,uncertain", &[&account, &submission]).map_err(|_| Error::Database)?;
    let mut total = 0i64;
    for row in &groups {
        let number: i64 = field(row, 2)?;
        count_line(&mut text, field(row, 0)?, field(row, 1)?, number)?;
        total = total.checked_add(number).ok_or(Error::Integrity)?;
    }
    if total != count {
        return Err(Error::Integrity);
    }
    let listed = client.query("SELECT address,state,uncertain,reason FROM td_mta.recipients WHERE account=$1 AND submission_id=$2 AND state IN (5,7) ORDER BY ordinal LIMIT $3", &[&account, &submission, &COMPACT_ADDRESSES]).map_err(|_| Error::Database)?;
    if !listed.is_empty() {
        append(&mut text, "\n")?;
    }
    for row in &listed {
        let label = outcome(field(row, 1)?, field(row, 2)?)?;
        detail(&mut text, label, field(row, 0)?, 254)?;
        detail(&mut text, "  Reason: ", reason_text(field(row, 3)?)?, 64)?;
    }
    if text.len() > COMPACT_TEXT {
        return Err(Error::Integrity);
    }
    Ok(text)
}

fn execute(
    client: &Client,
    sql: &str,
    parameters: &[&dyn crate::postgres_probe::Encode],
    count: u64,
) -> Result<(), Error> {
    if client
        .execute(sql, parameters)
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
    fn compact_worst_case_fits_its_fixed_bounds() {
        let submission = SubmissionId::from_bytes([0xff; 16]);
        let original = EmailId::from_bytes([0xff; 16]);
        let heading = format!("Local delivery report\nSubmission: {submission}\nOriginal Email: {original}\n\nNo further attempts are scheduled for this submission.\nAn unknown outcome is not proof of failure; resending may deliver a duplicate.\n");
        let mut text = String::new();
        append(&mut text, &heading).unwrap();
        append(&mut text, COMPACT_INTRO).unwrap();
        for (state, uncertain) in [(4, 0), (4, 1), (5, 0), (6, 0), (7, 1)] {
            count_line(&mut text, state, uncertain, 1000).unwrap();
        }
        let address = format!("{}\u{1b}", "a".repeat(253));
        for _ in 0..COMPACT_ADDRESSES {
            detail(&mut text, outcome(7, 1).unwrap(), &address, 254).unwrap();
            detail(&mut text, "  Reason: ", reason_text(10).unwrap(), 64).unwrap();
        }
        assert!(text.contains("resend disabled"));
        assert!(text.len() <= COMPACT_TEXT, "{}", text.len());
        assert!(text.contains("Delivery outcome unknown: 1000\n"));
        assert!(!text.contains('\u{1b}'));
        let clock = RuntimeClock::new();
        let until = Deadline::after(clock.sample().unwrap().monotonic, 10_000).unwrap();
        let mut meter = Meter::new(
            until,
            Charge {
                io_bytes: 1024 * 1024,
                output_bytes: 1024 * 1024,
                records: 1024,
                ..Charge::default()
            },
        );
        let id = format!("notice-{original}@localhost.invalid");
        let bytes = render(
            &text,
            &id,
            1_800_000_000_000,
            COMPACT_MESSAGE,
            &clock,
            &mut meter,
        )
        .unwrap();
        assert!(bytes.len() <= COMPACT_MESSAGE);
        assert_eq!(outcome(5, 1), Err(Error::Integrity));
        assert_eq!(
            count_line(&mut String::new(), 5, 0, -1),
            Err(Error::Integrity)
        );
    }
    #[test]
    fn compact_margin_is_one_message_per_queued_submission() {
        let caps = crate::postgres_configuration::SharedQuotas {
            body_bytes: 1000,
            blob_count: 10,
            upload_bytes: 7,
            queue_bytes: 8,
            queue_submissions: 3,
        };
        let raised = compact_caps(caps);
        // One share more than the queue: an incoming notice may hold it.
        assert_eq!(raised.body_bytes, 1000 + 4 * 16 * 1024);
        assert_eq!(raised.blob_count, 14);
        let incoming = incoming_caps(caps);
        assert_eq!(
            (incoming.body_bytes, incoming.blob_count),
            (1000 + 16 * 1024, 11)
        );
        assert_eq!(
            (
                raised.upload_bytes,
                raised.queue_bytes,
                raised.queue_submissions
            ),
            (7, 8, 3)
        );
        let huge = crate::postgres_configuration::SharedQuotas {
            body_bytes: u64::MAX - 1,
            queue_submissions: u64::MAX,
            ..caps
        };
        assert_eq!(compact_caps(huge).body_bytes, u64::MAX);
    }
    #[test]
    fn refused_notice_text_stays_compact_with_a_null_sender_and_many_recipients() {
        let address = format!("{}\u{1b}", "a".repeat(253));
        let recipients = vec![address; 1000];
        let text = refused_text("", &recipients, u64::MAX).unwrap();
        assert!(text.contains("Sender (unverified): (none)\n"), "{text}");
        assert!(text.contains(&format!("Size: {} bytes\n", u64::MAX)));
        assert_eq!(text.matches("Recipient: ").count(), REFUSED_ADDRESSES);
        assert!(text.contains("Further recipients: 992\n"), "{text}");
        assert!(!text.contains('\u{1b}'));
        assert!(text.len() <= COMPACT_TEXT, "{}", text.len());
        let few = refused_text("sender@example.test", recipients.get(..2).unwrap(), 7).unwrap();
        assert!(few.contains("Sender (unverified): sender@example.test\n"));
        assert!(!few.contains("Further recipients"));
        let too_long = vec!["b".repeat(255)];
        assert_eq!(refused_text("", &too_long, 1), Err(Error::Integrity));
    }
    #[test]
    fn decimal_renders_every_width() {
        for value in [0, 7, 10, 1000, u64::MAX] {
            let mut buffer = [0; 20];
            assert_eq!(decimal(value, &mut buffer), value.to_string());
        }
    }
    #[test]
    fn only_size_refusals_fall_back_to_compact() {
        assert!(fallback(Error::Capacity));
        assert!(fallback(Error::Work(ports::Error::Capacity)));
        assert!(fallback(Error::Work(ports::Error::Quota)));
        for error in [
            Error::Quota,
            Error::Integrity,
            Error::Database,
            Error::Work(ports::Error::Deadline),
        ] {
            assert!(!fallback(error));
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
