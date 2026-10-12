//! Bounded upload transfer on a dedicated worker; publication stays in the store.
use crate::{
    admission::timers::{self, TimeoutPlan},
    bounded::TextBuffer,
    http_request::{BodyReader, Framing, QUANTUM},
    ids::{AccountId, DeviceId},
    ports::{Clock, CommitFailure, Deadline, Error, FlushProgress, IoProgress, Tick, Transport},
    smtp_receiving::Control,
    store_fs::{
        BlobAuthorization, IngressSpool, StoreCoordinator, UploadAttempt, UploadAuthorization,
        UploadError, UploadRequest,
    },
    wire,
};
use std::time::Duration;

pub struct Request<'a> {
    pub account: AccountId,
    pub device: DeviceId,
    pub framing: Framing,
    pub content_type: &'a str,
    pub prefix: &'a [u8],
    pub deadline: Deadline,
    pub body_deadline: Deadline,
    pub expect_continue: bool,
}
pub struct Response {
    bytes: [u8; 4096],
    length: usize,
    pub stop: bool,
}
impl Response {
    #[cfg(test)]
    pub(crate) fn stopped_for_test() -> Self {
        let mut bytes = [0; 4096];
        if let Some(prefix) = bytes.get_mut(..2) {
            prefix.copy_from_slice(b"{}");
        }
        Self {
            bytes,
            length: 2,
            stop: true,
        }
    }
    pub fn bytes(&self) -> Result<&[u8], Error> {
        self.bytes.get(..self.length).ok_or(Error::Invalid)
    }
}
/// Called only by the dedicated upload worker after HTTPS authentication.
pub trait Backend: Sync {
    fn account(&self) -> AccountId;
    fn receive(
        &self,
        request: Request<'_>,
        transport: &mut dyn Transport,
        control: &Control,
    ) -> Result<Response, Error>;
    fn download(
        &self,
        _request: crate::https_download::Request<'_>,
        _transport: &mut dyn Transport,
        _control: &Control,
    ) -> Result<(), crate::https_download::Failure> {
        Err(crate::https_download::Failure::Before(Error::NotFound))
    }
}
pub struct Service<'s, 'r, 'l, P> {
    pub coordinator: &'s StoreCoordinator<'r, 'l, P>,
    pub spool: &'s IngressSpool<'r>,
    pub account: AccountId,
    pub maximum: u64,
    pub clock: &'s dyn Clock,
    pub timeouts: &'s TimeoutPlan,
}
impl<P: UploadAuthorization + BlobAuthorization + Sync> Backend for Service<'_, '_, '_, P> {
    fn account(&self) -> AccountId {
        self.account
    }
    fn download(
        &self,
        request: crate::https_download::Request<'_>,
        transport: &mut dyn Transport,
        control: &Control,
    ) -> Result<(), crate::https_download::Failure> {
        if request.target.account() != self.account {
            return Err(crate::https_download::Failure::Before(Error::NotFound));
        }
        crate::https_download::send(
            self.coordinator,
            self.maximum,
            request,
            transport,
            control,
            self.clock,
            self.timeouts,
        )
    }
    fn receive(
        &self,
        request: Request<'_>,
        transport: &mut dyn Transport,
        control: &Control,
    ) -> Result<Response, Error> {
        if request.account != self.account {
            return Err(Error::Forbidden);
        }
        if request.content_type.len() > 1024 {
            return Err(Error::Capacity);
        }
        if request.content_type.is_empty()
            || !request
                .content_type
                .bytes()
                .all(|b| b == b'\t' || (32..=126).contains(&b))
        {
            return Err(Error::Invalid);
        }
        let maximum = match request.framing {
            Framing::Length(n) if n <= self.maximum => n,
            Framing::Length(_) => return Err(Error::Capacity),
            Framing::Chunked => self.maximum,
        };
        let wire_max = maximum
            .checked_mul(8)
            .and_then(|n| n.checked_add(65536))
            .ok_or(Error::Capacity)?;
        let mut body = BodyReader::new(request.framing, maximum, wire_max).map_err(body_error)?;
        let mut entropy = td_crypto::SystemEntropy::try_new()?;
        let admission = UploadRequest {
            account: request.account,
            device: request.device,
            maximum,
            deadline: request.deadline,
        };
        let mut upload = match request.framing {
            Framing::Length(_) => self.coordinator.reserve_reclaiming(
                &td_crypto::Provider,
                &mut entropy,
                self.spool,
                admission,
            ),
            Framing::Chunked => {
                self.coordinator
                    .reserve(&td_crypto::Provider, &mut entropy, self.spool, admission)
            }
        }
        .map_err(upload_error)?;
        let body_deadline = request.body_deadline.min(request.deadline);
        let mut last = self.clock.sample()?.monotonic;
        if request.expect_continue {
            let interim_deadline =
                timers::deadline(last, self.timeouts.limits().response_idle_seconds)
                    .map_err(|_| Error::Invalid)?
                    .min(body_deadline);
            let mut sent = 0;
            let interim = b"HTTP/1.1 100 Continue\r\n\r\n";
            loop {
                if control.stop_requested()
                    || interim_deadline.expired(observe(self.clock, &mut last)?)
                {
                    return Err(Error::Deadline);
                }
                if sent < interim.len() {
                    match transport.write(interim.get(sent..).ok_or(Error::Invalid)?)? {
                        IoProgress::Bytes(n) if n != 0 && n <= interim.len() - sent => sent += n,
                        IoProgress::Pending => std::thread::sleep(Duration::from_millis(5)),
                        _ => return Err(Error::Invalid),
                    }
                } else if transport.flush()? == FlushProgress::Complete {
                    break;
                } else {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }
        let mut input = [0; QUANTUM];
        let mut output = [0; QUANTUM];
        let mut used = request.prefix.len();
        input
            .get_mut(..used)
            .ok_or(Error::Invalid)?
            .copy_from_slice(request.prefix);
        let current = observe(self.clock, &mut last)?;
        let mut idle = timers::deadline(current, self.timeouts.limits().body_idle_seconds)
            .map_err(|_| Error::Invalid)?
            .min(body_deadline);
        loop {
            let now = observe(self.clock, &mut last)?;
            if control.stop_requested() || idle.expired(now) || body_deadline.expired(now) {
                return Err(Error::Deadline);
            }
            let progress = body
                .feed(input.get(..used).ok_or(Error::Invalid)?, &mut output)
                .map_err(body_error)?;
            if progress.written != 0 {
                upload
                    .write(output.get(..progress.written).ok_or(Error::Invalid)?)
                    .map_err(upload_error)?;
                output
                    .get_mut(..progress.written)
                    .ok_or(Error::Invalid)?
                    .fill(0);
            }
            if progress.complete {
                break;
            }
            if progress.consumed != used {
                return Err(Error::WriterStopped);
            }
            input.fill(0);
            used = match transport.read(&mut input)? {
                IoProgress::Bytes(n) if n != 0 && n <= input.len() => {
                    idle = timers::deadline(
                        observe(self.clock, &mut last)?,
                        self.timeouts.limits().body_idle_seconds,
                    )
                    .map_err(|_| Error::Invalid)?
                    .min(body_deadline);
                    n
                }
                IoProgress::Pending => {
                    std::thread::sleep(Duration::from_millis(5));
                    0
                }
                _ => return Err(Error::Invalid),
            };
        }
        if control.stop_requested() {
            return Err(Error::Deadline);
        }
        upload.prepare().map_err(upload_error)?;
        if control.stop_requested() {
            return Err(Error::Deadline);
        }
        let mut attempt = upload.commit().map_err(upload_error)?;
        if matches!(attempt, UploadAttempt::SequenceConflict) {
            upload.replan().map_err(upload_error)?;
            attempt = upload.commit().map_err(upload_error)?;
        }
        let UploadAttempt::Complete(completion) = attempt else {
            return Err(Error::Conflict);
        };
        let stop = completion_stop(&completion)?;
        let mut response = Response {
            bytes: [0; 4096],
            length: 0,
            stop,
        };
        let mut account = [0; wire::IdKind::WIRE_BYTES];
        let mut blob = [0; wire::IdKind::WIRE_BYTES];
        let account =
            wire::encode_id(request.account, &mut account).map_err(|_| Error::WriterStopped)?;
        let blob = wire::encode_id(completion.id, &mut blob).map_err(|_| Error::WriterStopped)?;
        let mut out = TextBuffer::new(&mut response.bytes);
        out.format(format_args!(
            "{{\"accountId\":\"{account}\",\"blobId\":\"{blob}\",\"size\":{},\"type\":",
            completion.length
        ))
        .map_err(|_| Error::WriterStopped)?;
        crate::jmap_session::quoted(&mut out, request.content_type)
            .map_err(|_| Error::WriterStopped)?;
        out.append("}").map_err(|_| Error::WriterStopped)?;
        response.length = out.len();
        Ok(response)
    }
}
fn body_error(error: crate::http_request::Error) -> Error {
    match error {
        crate::http_request::Error::Limit => Error::Capacity,
        _ => Error::Invalid,
    }
}
fn upload_error(error: UploadError) -> Error {
    match error {
        UploadError::Store(Error::Capacity | Error::Invalid) => Error::Busy,
        UploadError::Store(error) => error,
        UploadError::Ledger(error) => match error {
            crate::admission::logical::Error::Quota(_) => Error::Quota,
            crate::admission::logical::Error::Expired => Error::Deadline,
            crate::admission::logical::Error::Busy | crate::admission::logical::Error::Full => {
                Error::Busy
            }
            _ => Error::WriterStopped,
        },
        UploadError::Format(_) => Error::WriterStopped,
    }
}

fn observe(clock: &dyn Clock, last: &mut Tick) -> Result<Tick, Error> {
    let now = clock.sample()?.monotonic;
    if now < *last {
        return Err(Error::WriterStopped);
    }
    *last = now;
    Ok(now)
}

fn completion_stop(completion: &crate::store_fs::UploadCompletion) -> Result<bool, Error> {
    let stop = completion.admission_stopped || completion.cleanup_error.is_some();
    match completion.outcome {
        Ok(_) => Ok(stop),
        Err(CommitFailure::Rejected(error)) if !stop => {
            Err(upload_error(UploadError::Store(error)))
        }
        Err(_) => Err(Error::WriterStopped),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejected_and_indeterminate_completion_cannot_hide_retirement() {
        for (outcome, stopped, cleanup, expected) in [
            (
                Ok(crate::format::Sequence::from_u64(1)),
                true,
                None,
                Ok(true),
            ),
            (
                Err(CommitFailure::Rejected(Error::Forbidden)),
                false,
                None,
                Err(Error::Forbidden),
            ),
            (
                Err(CommitFailure::Rejected(Error::Forbidden)),
                true,
                None,
                Err(Error::WriterStopped),
            ),
            (
                Err(CommitFailure::Rejected(Error::Forbidden)),
                false,
                Some(Error::Busy),
                Err(Error::WriterStopped),
            ),
            (
                Err(CommitFailure::Rejected(Error::Capacity)),
                false,
                None,
                Err(Error::Busy),
            ),
            (
                Err(CommitFailure::Rejected(Error::Invalid)),
                false,
                None,
                Err(Error::Busy),
            ),
            (
                Err(CommitFailure::Indeterminate(Error::Deadline)),
                false,
                None,
                Err(Error::WriterStopped),
            ),
        ] {
            let completion = crate::store_fs::UploadCompletion {
                id: crate::ids::BlobId::from_bytes([1; 16]),
                length: 0,
                digest: [0; 32],
                expires_at: 0,
                outcome,
                admission_stopped: stopped,
                cleanup_error: cleanup,
            };
            assert_eq!(completion_stop(&completion), expected);
        }
    }
}
