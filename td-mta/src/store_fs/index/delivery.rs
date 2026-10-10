//! One receiving envelope becomes one durable Inbox email.
use super::*;
use crate::{
    admission::work::{Charge as WorkCharge, Meter},
    format::row::{EmailOrigin, EmailRow, ReceiptRecipients, ReceiptTls, SmtpReceipt},
    ids::{EmailId, MailboxId, ThreadId},
    smtp_session::Session,
};
use std::{fmt::Write as _, net::IpAddr};
#[path = "delivery_headers.rs"]
mod headers;

/// Passive facts supplied by the trusted connection owner, never a credential.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeliveryPeer<'a> {
    pub peer: IpAddr,
    pub tls: ReceiptTls,
    pub gateway: Option<&'a str>,
}
/// A trusted connection-specific receiving policy adapter. Bind actual socket
/// and TLS evidence, enforce gateway admission, and recheck current policy.
/// Its guard excludes relevant revocation through the synchronous commit;
/// pinned recipient routing alone does not authorize a revoked gateway.
/// Acquisition must refuse within deadline and must not call the coordinator.
/// Adapter destruction must be bounded; it runs on the storage worker during
/// reservation refusal or job retirement, after local spool/lease cleanup.
pub trait DeliveryAuthorization {
    type Guard<'a>: DeliveryGuard
    where
        Self: 'a;
    fn authorize(
        &self,
        account: AccountId,
        deadline: Deadline,
    ) -> Result<Self::Guard<'_>, ports::Error>;
}
impl<A: DeliveryAuthorization + ?Sized> DeliveryAuthorization for &A {
    type Guard<'a>
        = A::Guard<'a>
    where
        Self: 'a;
    fn authorize(
        &self,
        account: AccountId,
        deadline: Deadline,
    ) -> Result<Self::Guard<'_>, ports::Error> {
        (**self).authorize(account, deadline)
    }
}

pub trait DeliveryGuard {
    fn access(&self) -> Access;
    fn peer(&self) -> DeliveryPeer<'_>;
}
#[derive(Debug)]
pub enum DeliveryError {
    /// Publication did not start or consume work; retry under the same deadline.
    CoordinationBusy,
    HeaderLimit,
    Storage(UploadError),
}
impl From<ports::Error> for DeliveryError {
    fn from(error: ports::Error) -> Self {
        Self::Storage(error.into())
    }
}
impl From<UploadError> for DeliveryError {
    fn from(error: UploadError) -> Self {
        Self::Storage(error)
    }
}
impl From<logical::Error> for DeliveryError {
    fn from(error: logical::Error) -> Self {
        Self::Storage(error.into())
    }
}
impl From<format::Error> for DeliveryError {
    fn from(error: format::Error) -> Self {
        Self::Storage(error.into())
    }
}
impl std::fmt::Display for DeliveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CoordinationBusy => f.write_str("delivery coordination is busy"),
            Self::HeaderLimit => f.write_str("message root headers exceed limit"),
            Self::Storage(e) => e.fmt(f),
        }
    }
}
impl std::error::Error for DeliveryError {}

/// Format-derived maximum, including the longest envelope, IP literal, ID and
/// four-digit UTC date. Use this for Settings::trace_bytes, never a new knob.
pub fn smtp_trace_allowance(hostname: &str) -> Result<usize, ports::Error> {
    crate::config::values::certificate_name(hostname).map_err(|_| ports::Error::Invalid)?;
    // Each variable has a protocol bound; 128 covers every fixed delimiter,
    // the longest protocol/TLS comment and UTC date; two more bytes protect
    // an orphan continuation with an explicit header/body separator.
    hostname
        .len()
        .checked_add(crate::format::row::MAX_EHLO)
        .and_then(|n| n.checked_add(crate::format::row::MAX_ADDRESS))
        .and_then(|n| n.checked_add(52 + 32 + 128 + 2))
        .ok_or(ports::Error::Capacity)
}
struct Receipt {
    account: AccountId,
    peer: IpAddr,
    tls: ReceiptTls,
    gateway: Option<String>,
    hello: String,
    reverse: String,
    recipients: Vec<u8>,
}
impl Receipt {
    fn peer(&self) -> DeliveryPeer<'_> {
        DeliveryPeer {
            peer: self.peer,
            tls: self.tls,
            gateway: self.gateway.as_deref(),
        }
    }
    fn row(&self) -> Result<SmtpReceipt<'_>, format::Error> {
        Ok(SmtpReceipt {
            peer: self.peer,
            tls: self.tls,
            gateway: self.gateway.as_deref(),
            ehlo: &self.hello,
            reverse_path: &self.reverse,
            recipients: ReceiptRecipients::decode(&self.recipients)?,
        })
    }
}
fn text_copy(text: &str) -> Result<String, ports::Error> {
    let mut result = String::new();
    result
        .try_reserve_exact(text.len())
        .map_err(|_| ports::Error::Capacity)?;
    result.push_str(text);
    Ok(result)
}
fn buffer(length: usize) -> Result<Vec<u8>, ports::Error> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length)
        .map_err(|_| ports::Error::Capacity)?;
    bytes.resize(length, 0);
    Ok(bytes)
}
fn validate_guard(guard: &impl DeliveryGuard, account: AccountId) -> Result<(), ports::Error> {
    let access = guard.access();
    if access.account != account || access.principal != Principal::Smtp {
        return Err(ports::Error::Forbidden);
    }
    Ok(())
}

/// Configured root-header ceiling and the original receiving deadline.
#[derive(Clone, Copy, Debug)]
pub struct DeliveryRequest {
    pub header_bytes: usize,
    pub deadline: Deadline,
}

/// Carries its connection authorization adapter between storage worker turns.
/// Owning the adapter does not cache permission: publication authorizes again.
/// Retire terminal failures with discard or Drop on a storage worker.
/// CoordinationBusy retains the job under its unchanged finalization deadline.
pub struct Delivery<'c, 's, 'r, 'a, C: Crypto, P, A: DeliveryAuthorization> {
    coordinator: &'c StoreCoordinator<'r, 'a, P>,
    authorization: A,
    crypto: &'c C,
    stage: Stage<'s, 'r, C::Sha256>,
    lease: Option<LeaseId>,
    receipt: Receipt,
    blob: BlobId,
    email: EmailId,
    fresh_thread: ThreadId,
    epoch: StoreEpoch,
    received_at: i64,
    trace: String,
    maximum: u64,
    incoming_maximum: u64,
    incoming: u64,
    length: u64,
    deadline: Deadline,
    last: Tick,
    meter: Meter,
    headers: headers::Headers,
    failed: bool,
}
impl<'r, 'a, P> StoreCoordinator<'r, 'a, P> {
    /// Cold receiving startup check; never call from the network event loop.
    pub fn receiving_ready(
        &self,
        account: AccountId,
        deadline: Deadline,
    ) -> Result<(), ports::Error> {
        let state = self.try_state()?;
        if state.stopped {
            return Err(ports::Error::WriterStopped);
        }
        let mut view = self.store.view(account, deadline)?;
        inbox(&mut view)?;
        Ok(())
    }

    /// Reserve only after this validated session requests DATA admission. The
    /// trusted driver supplies its connection policy and configured header bound.
    /// Transfer the adapter into the job, or pass a reference for an outer owner.
    /// Run this call on a storage worker; refusal drops the supplied adapter
    /// after local cleanup. An admitted job retains it until discard or Drop.
    pub fn reserve_delivery<'c, 's, C: Crypto, A: DeliveryAuthorization>(
        &'c self,
        crypto: &'c C,
        entropy: &mut impl Entropy,
        spool: &'s IngressSpool<'r>,
        authorization: A,
        session: &Session<'_>,
        request: DeliveryRequest,
    ) -> Result<Delivery<'c, 's, 'r, 'a, C, P, A>, DeliveryError> {
        let DeliveryRequest {
            header_bytes,
            deadline,
        } = request;
        let mut state_guard = self.try_state()?;
        let state = &mut *state_guard;
        if state.stopped {
            return Err(ports::Error::WriterStopped.into());
        }
        if state.ledger.available_cells() < 2 {
            return Err(UploadError::Ledger(logical::Error::Full).into());
        }
        let (settings, envelope, extended) = session.delivery_context()?;
        let allowance = smtp_trace_allowance(settings.hostname)?;
        if settings.trace_bytes < allowance
            || settings.message_bytes as u64 > spool.capacity().bytes_each
            || header_bytes == 0
            || header_bytes > crate::limits::MIB
            || header_bytes > settings.message_bytes
        {
            return Err(ports::Error::Invalid.into());
        }
        let mut last = self.store.clock.sample()?.monotonic;
        let time = check_time(&self.store, deadline, &mut last)?;
        let guard = authorization.authorize(envelope.account, deadline)?;
        validate_guard(&guard, envelope.account)?;
        let peer = guard.peer();
        if peer.gateway.is_some_and(|g| {
            g.is_empty()
                || g.len() > crate::format::row::MAX_GATEWAY
                || g.chars().any(char::is_control)
        }) {
            return Err(ports::Error::Invalid.into());
        }
        let mut recipient_storage = buffer(crate::format::row::MAX_RECEIPT_BYTES)?;
        let mut recipients = Vec::new();
        recipients
            .try_reserve_exact(envelope.recipients.len())
            .map_err(|_| ports::Error::Capacity)?;
        recipients.extend(envelope.recipients.iter().map(String::as_str));
        let encoded = ReceiptRecipients::encode(&recipients, &mut recipient_storage)?
            .encoded()
            .len();
        recipient_storage.truncate(encoded);
        let receipt = Receipt {
            account: envelope.account,
            peer: peer.peer,
            tls: peer.tls,
            gateway: peer.gateway.map(text_copy).transpose()?,
            hello: text_copy(&envelope.ehlo)?,
            reverse: text_copy(&envelope.reverse_path)?,
            recipients: recipient_storage,
        };
        drop(guard);
        let mut view = self.store.view(envelope.account, deadline)?;
        inbox(&mut view)?;
        let epoch = view.identity().epoch;
        drop(view);
        let mut bytes = [0; 16];
        entropy.fill(&mut bytes).map_err(ports::Error::from)?;
        let blob = BlobId::from_bytes(bytes);
        entropy.fill(&mut bytes).map_err(ports::Error::from)?;
        let email = EmailId::from_bytes(bytes);
        entropy.fill(&mut bytes).map_err(ports::Error::from)?;
        let fresh_thread = ThreadId::from_bytes(bytes);
        let trace = trace(
            &receipt,
            settings.hostname,
            extended,
            email,
            time.utc_ms,
            allowance,
        )?;
        if trace.len() > header_bytes {
            return Err(ports::Error::Invalid.into());
        }
        let headers = headers::Headers::new(header_bytes)?;
        let maximum = settings.message_bytes as u64;
        let incoming_maximum = session.incoming_limit() as u64;
        let meter = Meter::new(
            deadline,
            WorkCharge {
                io_bytes: self.work.foreground_io_bytes,
                records: self.work.foreground_records,
                output_bytes: maximum,
                unlinks: 0,
            },
        );
        let now = check_time(&self.store, deadline, &mut last)?;
        let lease = state.ledger.reserve(
            &[[
                Charge {
                    kind: Kind::BodyBytes,
                    amount: maximum,
                },
                Charge {
                    kind: Kind::BlobCount,
                    amount: 1,
                },
                Charge::ZERO,
                Charge::ZERO,
            ]],
            deadline,
            now.monotonic,
        )?;
        drop(state_guard);
        let writer = match spool.begin(crypto, envelope.account, blob, deadline) {
            Ok(writer) => writer,
            Err(error) => {
                let _ = self.cancel_job(Some(lease));
                return Err(error.into());
            }
        };
        Ok(Delivery {
            coordinator: self,
            authorization,
            crypto,
            stage: Stage::Writing(writer),
            lease: Some(lease),
            receipt,
            blob,
            email,
            fresh_thread,
            epoch,
            received_at: time.utc_ms,
            trace,
            maximum,
            incoming_maximum,
            incoming: 0,
            length: 0,
            deadline,
            last,
            meter,
            headers,
            failed: false,
        })
    }
}

fn trace(
    receipt: &Receipt,
    hostname: &str,
    extended: bool,
    email: EmailId,
    utc_ms: i64,
    maximum: usize,
) -> Result<String, ports::Error> {
    let date = td_civil::unix_to_civil_utc_checked(utc_ms.div_euclid(1000))
        .ok_or(ports::Error::Invalid)?;
    if !(1900..=9999).contains(&date.year) {
        return Err(ports::Error::Invalid);
    }
    const MONTHS: &[&str] = &[
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let month = MONTHS
        .get(
            usize::from(date.month)
                .checked_sub(1)
                .ok_or(ports::Error::Invalid)?,
        )
        .ok_or(ports::Error::Invalid)?;
    let protocol = match (extended, receipt.tls) {
        (true, ReceiptTls::Plain) => "ESMTP",
        (true, _) => "ESMTPS",
        (false, _) => "SMTP",
    };
    let tls = match receipt.tls {
        ReceiptTls::Plain => "",
        ReceiptTls::Tls12 => " (TLSv1.2)",
        ReceiptTls::Tls13 => " (TLSv1.3)",
    };
    let mut output = String::new();
    output
        .try_reserve_exact(maximum)
        .map_err(|_| ports::Error::Capacity)?;
    write!(
        output,
        "Return-Path: <{}>\r\nReceived: from {} (",
        receipt.reverse, receipt.hello
    )
    .map_err(|_| ports::Error::Capacity)?;
    match receipt.peer {
        IpAddr::V4(ip) => write!(output, "[{ip}]"),
        IpAddr::V6(ip) => write!(output, "[IPv6:{ip}]"),
    }
    .map_err(|_| ports::Error::Capacity)?;
    write!(output, ")\r\n\tby {hostname} with {protocol}{tls} id {email};\r\n\t{} {month} {} {:02}:{:02}:{:02} +0000\r\n",
        date.day, date.year, date.hour, date.minute, date.second).map_err(|_| ports::Error::Capacity)?;
    if output.len() > maximum {
        return Err(ports::Error::Capacity);
    }
    Ok(output)
}

impl<C: Crypto, P, A: DeliveryAuthorization> Delivery<'_, '_, '_, '_, C, P, A> {
    /// Accept one decoded DATA line from the session, including its CRLF.
    pub fn write(&mut self, bytes: &[u8]) -> Result<(), DeliveryError> {
        if self.failed {
            return Err(ports::Error::Invalid.into());
        }
        let result = self.write_inner(bytes);
        self.failed |= result.is_err();
        result
    }
    fn write_inner(&mut self, bytes: &[u8]) -> Result<(), DeliveryError> {
        check_time(&self.coordinator.store, self.deadline, &mut self.last)?;
        if bytes.len() > 1000 || !bytes.ends_with(b"\r\n") {
            return Err(ports::Error::Invalid.into());
        }
        let next = self
            .incoming
            .checked_add(bytes.len() as u64)
            .ok_or(ports::Error::Capacity)?;
        if next > self.incoming_maximum {
            return Err(ports::Error::Quota.into());
        }
        let Stage::Writing(writer) = &mut self.stage else {
            return Err(ports::Error::Invalid.into());
        };
        if self.headers.complete() {
            self.meter
                .charge(
                    self.last,
                    WorkCharge {
                        io_bytes: bytes.len() as u64,
                        output_bytes: bytes.len() as u64,
                        records: 0,
                        unlinks: 0,
                    },
                )
                .map_err(|_| ports::Error::Capacity)?;
            store_bytes(writer, &mut self.length, self.maximum, bytes)?;
        } else {
            let mut work = headers::Work {
                clock: self.coordinator.store.clock.as_ref(),
                deadline: self.deadline,
                meter: &mut self.meter,
                last: &mut self.last,
            };
            if self.headers.push(bytes, false, &mut work)? {
                self.headers.write_filtered(
                    self.trace.as_bytes(),
                    |bytes| store_bytes(writer, &mut self.length, self.maximum, bytes),
                    &mut work,
                )?;
            }
        }
        self.incoming = next;
        Ok(())
    }
    /// Finish only after the session's terminating dot. This is preparation, not
    /// acceptance. Header-only and empty messages take their EOF decision here.
    /// Capture the queued finalization deadline once; it cannot extend the
    /// original receiving lease. Publication retries retain this same cap.
    pub fn prepare(&mut self, deadline: Deadline) -> Result<(), DeliveryError> {
        if self.failed {
            return Err(ports::Error::Invalid.into());
        }
        self.deadline = self.deadline.min(deadline);
        let result = self.prepare_inner();
        self.failed |= result.is_err();
        result
    }
    fn prepare_inner(&mut self) -> Result<(), DeliveryError> {
        check_time(&self.coordinator.store, self.deadline, &mut self.last)?;
        let Stage::Writing(writer) = &mut self.stage else {
            return Err(ports::Error::Invalid.into());
        };
        if !self.headers.complete() {
            let mut work = headers::Work {
                clock: self.coordinator.store.clock.as_ref(),
                deadline: self.deadline,
                meter: &mut self.meter,
                last: &mut self.last,
            };
            if !self.headers.push(&[], true, &mut work)? {
                return Err(ports::Error::Invalid.into());
            }
            self.headers.write_filtered(
                self.trace.as_bytes(),
                |bytes| store_bytes(writer, &mut self.length, self.maximum, bytes),
                &mut work,
            )?;
        }
        let Stage::Writing(writer) = std::mem::replace(&mut self.stage, Stage::Finished) else {
            return Err(ports::Error::Invalid.into());
        };
        self.stage = Stage::Prepared(writer.finish()?);
        check_time(&self.coordinator.store, self.deadline, &mut self.last)?;
        Ok(())
    }
    pub fn discard(mut self) -> Result<(), DeliveryError> {
        let cleanup = discard_stage(std::mem::replace(&mut self.stage, Stage::Finished));
        self.coordinator.cancel_job(self.lease.take())?;
        cleanup.map_err(Into::into)
    }
}

fn store_bytes<D: Digest>(
    writer: &mut SpoolWriter<'_, '_, D>,
    length: &mut u64,
    maximum: u64,
    bytes: &[u8],
) -> Result<(), ports::Error> {
    let next = length
        .checked_add(bytes.len() as u64)
        .ok_or(ports::Error::Capacity)?;
    if next > maximum {
        return Err(ports::Error::Quota);
    }
    writer.write(bytes)?;
    *length = next;
    Ok(())
}
impl<C: Crypto, P, A: DeliveryAuthorization> Drop for Delivery<'_, '_, '_, '_, C, P, A> {
    fn drop(&mut self) {
        let _ = discard_stage(std::mem::replace(&mut self.stage, Stage::Finished));
        let _ = self.coordinator.cancel_job(self.lease.take());
    }
}

#[derive(Debug)]
#[must_use = "a known durable result survives accounting or cleanup failure"]
pub struct DeliveryCompletion {
    pub email: EmailId,
    pub blob: BlobId,
    pub thread: ThreadId,
    pub inbox: MailboxId,
    pub outcome: Result<ports::Commit, CommitError>,
    pub admission_stopped: bool,
    pub cleanup_error: Option<ports::Error>,
}
fn inbox(view: &mut IndexReadView<'_, '_>) -> Result<MailboxId, ports::Error> {
    let identity = view.identity();
    view.read_snapshot(|native| {
        let id = native.run(|db| {
            let mut statement = db.prepare("SELECT id FROM mailboxes WHERE account=?1 AND role='inbox' ORDER BY id LIMIT 2").map_err(sql)?;
            let mut rows = statement.query(params![identity.account.as_bytes().as_slice()]).map_err(sql)?;
            let row = rows.next().map_err(sql)?.ok_or(ports::Error::NotFound)?;
            let id = MailboxId::from_bytes(fixed_blob(row, 0)?);
            if rows.next().map_err(sql)?.is_some() { return Err(ports::Error::Conflict); }
            Ok(id)
        })?;
        let mut value = [0; 2048];
        if !matches!(get(native, identity, Key::Mailbox(id), &mut value)?,
            Some((Row::Mailbox(row), _)) if row.role == Some("inbox")) {
            return Err(ports::Error::Corrupt);
        }
        Ok(id)
    })
}

fn resolve_thread(
    view: &mut IndexReadView<'_, '_>,
    headers: &headers::Headers,
    scratch: &mut [u8],
    own: &mut [u8],
    work: &mut headers::Work<'_>,
) -> Result<(Option<ThreadId>, usize), ports::Error> {
    let mut bytes = [0; format::key::MAX_ANCHOR_BYTES];
    let mut selected = None;
    for ring in [&headers.identifiers.references, &headers.identifiers.reply] {
        for n in 0..32 {
            let Some(item) = ring.newest(n) else {
                break;
            };
            if let Some(id) = headers::candidate(&headers.source, item, &mut bytes, work)? {
                if let Some((_, thread)) = view.thread_anchor(id, scratch)? {
                    selected = Some(thread);
                    break;
                }
            }
        }
        if selected.is_some() {
            break;
        }
    }
    let length = if let Some(item) = headers.identifiers.own {
        if let Some(id) = headers::candidate(&headers.source, item, own, work)? {
            if selected.is_none() {
                selected = view.thread_anchor(id, scratch)?.map(|(_, thread)| thread);
            }
            id.len()
        } else {
            0
        }
    } else {
        0
    };
    Ok((selected, length))
}

impl<C: Crypto, P, A: DeliveryAuthorization> Delivery<'_, '_, '_, '_, C, P, A> {
    pub fn commit(&mut self) -> Result<DeliveryCompletion, DeliveryError> {
        if self.failed || !matches!(self.stage, Stage::Prepared(_)) {
            self.failed = true;
            return Err(ports::Error::Invalid.into());
        }
        if let Err(error) = check_time(&self.coordinator.store, self.deadline, &mut self.last) {
            self.failed = true;
            return Err(error.into());
        }
        let coordinator = self.coordinator;
        let mut state = coordinator.try_state().map_err(|error| match error {
            ports::Error::Busy => DeliveryError::CoordinationBusy,
            error => error.into(),
        })?;
        let mut result = self.commit_inner(&mut state);
        self.failed |= result.is_err();
        drop(state);
        if let Ok(completion) = &mut result {
            completion.cleanup_error =
                discard_stage(std::mem::replace(&mut self.stage, Stage::Finished)).err();
        }
        result
    }
    fn commit_inner(
        &mut self,
        state: &mut AdmissionState<'_>,
    ) -> Result<DeliveryCompletion, DeliveryError> {
        if state.stopped {
            return Err(ports::Error::WriterStopped.into());
        }
        let Stage::Prepared(input) = &mut self.stage else {
            return Err(ports::Error::Invalid.into());
        };
        if input.account() != self.receipt.account
            || input.id() != self.blob
            || input.len() != self.length
            || self.epoch != self.coordinator.store.epoch()
        {
            return Err(ports::Error::Invalid.into());
        }
        check_time(&self.coordinator.store, self.deadline, &mut self.last)?;
        let guard = self
            .authorization
            .authorize(self.receipt.account, self.deadline)?;
        validate_guard(&guard, self.receipt.account)?;
        if guard.peer() != self.receipt.peer() {
            return Err(ports::Error::Forbidden.into());
        }
        let mut scratch = buffer(format::MAX_VALUE_BYTES)?;
        let mut view = self
            .coordinator
            .store
            .view(self.receipt.account, self.deadline)?;
        let identity = view.identity();
        if identity.epoch != self.epoch {
            return Err(ports::Error::Conflict.into());
        }
        let inbox = inbox(&mut view)?;
        let mut own = [0; format::key::MAX_ANCHOR_BYTES];
        let mut work = headers::Work {
            clock: self.coordinator.store.clock.as_ref(),
            deadline: self.deadline,
            meter: &mut self.meter,
            last: &mut self.last,
        };
        let (selected, own_length) =
            resolve_thread(&mut view, &self.headers, &mut scratch, &mut own, &mut work)?;
        let thread = selected.unwrap_or(self.fresh_thread);
        drop(view);
        let time = check_time(&self.coordinator.store, self.deadline, &mut self.last)?;
        let mut blob_bytes = [0; 48];
        let blob_length = Row::Blob(input.row(self.received_at)).encode(&mut blob_bytes)?;
        let email_length = Row::Email(EmailRow {
            blob: self.blob,
            thread,
            received_at: self.received_at,
            origin: EmailOrigin::Smtp(self.receipt.row()?),
        })
        .encode(&mut scratch)?;
        let mut membership = [0; 32];
        let membership_length = Key::Membership(self.email, inbox).encode(&mut membership)?;
        let mut anchor = [0; 1024];
        let anchor_length = if own_length > 0 {
            let id = std::str::from_utf8(own.get(..own_length).ok_or(ports::Error::Invalid)?)
                .map_err(|_| ports::Error::Invalid)?;
            Key::ThreadAnchor(id, self.email).encode(&mut anchor)?
        } else {
            0
        };
        let mut operations = Vec::new();
        operations
            .try_reserve_exact(9)
            .map_err(|_| ports::Error::Capacity)?;
        operations.push(Operation::put(
            Table::Blobs,
            self.blob.as_bytes(),
            blob_bytes.get(..blob_length).ok_or(ports::Error::Invalid)?,
        )?);
        if selected.is_none() {
            operations.push(Operation::put(Table::Threads, thread.as_bytes(), &[])?);
        }
        operations.push(Operation::put(
            Table::Emails,
            self.email.as_bytes(),
            scratch.get(..email_length).ok_or(ports::Error::Invalid)?,
        )?);
        operations.push(Operation::put(
            Table::Memberships,
            membership
                .get(..membership_length)
                .ok_or(ports::Error::Invalid)?,
            &[],
        )?);
        if anchor_length > 0 {
            operations.push(Operation::put(
                Table::ThreadAnchors,
                anchor.get(..anchor_length).ok_or(ports::Error::Invalid)?,
                &[],
            )?);
        }
        operations.push(Operation::change(
            ObjectType::Email,
            ChangeAction::Created,
            self.email.as_bytes(),
        ));
        operations.push(Operation::change(
            ObjectType::Thread,
            if selected.is_some() {
                ChangeAction::Updated
            } else {
                ChangeAction::Created
            },
            thread.as_bytes(),
        ));
        operations.push(Operation::change(
            ObjectType::Mailbox,
            ChangeAction::Updated,
            inbox.as_bytes(),
        ));
        let planned = [self.length, 1, 0, 0];
        let lease = self.lease.ok_or(ports::Error::Invalid)?;
        let mut effects = begin_publication(
            &mut state.ledger,
            &mut state.stopped,
            &mut state.recoverable_files,
            lease,
            planned,
            self.deadline,
            time.monotonic,
        )?;
        let completion = self.coordinator.store.commit_with_files(
            self.crypto,
            CommitRequest {
                account: self.receipt.account,
                epoch: self.epoch,
                expected: identity.committed_sequence,
                utc_ms: time.utc_ms,
                deadline: self.deadline,
            },
            &operations,
            &mut [BlobSource {
                id: self.blob,
                source: input,
            }],
        );
        drop(guard);
        let outcome = completion.outcome();
        let recoverable = settle_publication(
            &mut state.ledger,
            &mut state.stopped,
            &mut effects,
            planned,
            &completion,
        );
        let logical_canceled = state.ledger.cancel(lease).is_ok();
        if !logical_canceled {
            state.stopped = true;
        }
        state.recoverable_files = recoverable && logical_canceled;
        self.lease = None;
        Ok(DeliveryCompletion {
            email: self.email,
            blob: self.blob,
            thread,
            inbox,
            outcome: outcome.map(|sequence| ports::Commit {
                account: self.receipt.account,
                epoch: self.epoch,
                sequence,
            }),
            admission_stopped: state.stopped,
            cleanup_error: None,
        })
    }
}

#[cfg(test)]
#[path = "delivery_tests.rs"]
mod tests;
